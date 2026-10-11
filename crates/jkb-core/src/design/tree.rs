//! Designs nest (operator feedback, 2026-10-11): a design may be contained by another design of its
//! repo, by the same containment row a subtask uses, so the tree is one read of the graph and
//! nothing about it is stored twice. [`nest`] puts a design under another; [`split`] turns a
//! design's sections into child designs and carries their approvals with them.
//!
//! A child shares its root's plans: a span is staged into a step of its own design's **tree**
//! ([`root_of`]), so one plan can build a feature whose text is spread across children.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::Value;

use std::collections::BTreeMap;

use jkb_types::ItemId;

use super::{
    append_row, append_row_as, before_write, crdt, create, design_id, export, finish, invalid,
    item, load, set_metadata, span_items, view, Before, Crdt, DesignRow, SpanItem, SpanState,
    Version, KIND,
};
use crate::store::WriteMeta;
use crate::{containment, Result};
use rusqlite::Connection;
use yrs::{Map as _, Text as _};

/// The design `design` is nested under, if it is nested under one.
///
/// # Errors
/// A database error.
pub fn parent_of(conn: &Connection, design: ItemId) -> Result<Option<ItemId>> {
    let Some(parent) = containment::parent(conn, design)? else {
        return Ok(None);
    };
    let kind = item::get(conn, parent)?.map(|m| m.kind);
    Ok((kind.as_deref() == Some(KIND)).then_some(parent))
}

/// The top of `design`'s tree: itself when it is nested under no design. Containment is a tree
/// (`containment::contain` refuses a cycle), so the walk ends.
///
/// # Errors
/// A database error.
pub fn root_of(conn: &Connection, design: ItemId) -> Result<ItemId> {
    let mut at = design;
    while let Some(up) = parent_of(conn, at)? {
        at = up;
    }
    Ok(at)
}

/// The designs nested directly under `design`, in their order.
///
/// # Errors
/// A database error.
pub fn children_of(conn: &Connection, design: ItemId) -> Result<Vec<(ItemId, String)>> {
    let mut out = Vec::new();
    for child in containment::children(conn, design)? {
        if let Some(m) = item::get(conn, child)? {
            if m.kind == KIND {
                out.push((child, m.uid));
            }
        }
    }
    Ok(out)
}

/// Every design below `design`, depth first, in order: what its export inlines.
///
/// # Errors
/// A database error.
pub fn descendants(conn: &Connection, design: ItemId) -> Result<Vec<(ItemId, String)>> {
    let mut out = Vec::new();
    for (id, uid) in children_of(conn, design)? {
        out.push((id, uid));
        out.extend(descendants(conn, id)?);
    }
    Ok(out)
}

/// Nest design `uid` under design `under`, after its existing children.
///
/// Refused when the move would take a staging away or export text twice: a span of the tree the
/// design leaves would stop counting a plan step it is staged into (its plan stops listing it, and
/// it can read APPROVED — [`root_of`]); or a design below `uid` with a doc target of its own would
/// be exported twice, to its file and inlined into the new root's (review rounds 1–3).
///
/// # Errors
/// An unknown design, a parent in another repo, a parent inside `uid` (a cycle), a staged span the
/// move would strand, a moved design with its own doc target, or a database error.
pub fn nest(conn: &Connection, meta: &WriteMeta, uid: &str, under: &str) -> Result<DesignRow> {
    let child = design_id(conn, uid)?;
    let parent = design_id(conn, under)?;
    if child == parent {
        return Err(invalid(format!(
            "design {uid} cannot be nested under itself"
        )));
    }
    let (from, to) = (
        export::repo_of(conn, child)?,
        export::repo_of(conn, parent)?,
    );
    if from != to {
        return Err(invalid(format!(
            "design {uid} is in repo {} and {under} in repo {}: a design nests only under a design \
             of its own repo",
            from.as_deref().unwrap_or("(none)"),
            to.as_deref().unwrap_or("(none)")
        )));
    }
    if parent_of(conn, child)? == Some(parent) {
        return super::row(conn, uid);
    }
    let mut moved = vec![(child, uid.to_owned())];
    moved.extend(descendants(conn, child)?);
    for (design, design_uid) in &moved {
        if export::meta_of(conn, *design)?.doc_target.is_some() {
            return Err(exported_twice(design_uid));
        }
    }
    // Whether a staged span keeps its plans is measured, not predicted: every span of the tree the
    // design leaves is read before and after the move, in this transaction, and a span that stops
    // counting any step it was staged into refuses it. Predicting it from the edges took
    // two rounds to get wrong both ways (refusing a design that brings its own plan along, and
    // missing a root's span staged into a plan of the child that leaves) — review rounds 1 and 2.
    let old_root = root_of(conn, child)?;
    let mut tree = vec![(old_root, String::new())];
    tree.extend(descendants(conn, old_root)?);
    let before = staged_spans(conn, &tree)?;
    containment::contain(conn, meta, child, parent, next_position(conn, parent)?)?;
    let after = staged_spans(conn, &tree)?;
    // A span loses a staging when a step it counted before no longer counts: the same span still
    // STAGED through another step had silently left the first plan (review round 3).
    let lost: Vec<&str> = before
        .iter()
        .filter(|(span, steps)| {
            let now = after.get(*span);
            steps
                .iter()
                .any(|step| now.is_none_or(|n| !n.contains(step)))
        })
        .map(|(span, _)| span.as_str())
        .collect();
    if !lost.is_empty() {
        return Err(invalid(format!(
            "the move would take these spans out of the tree of a plan they are staged into, so \
             that plan would stop counting them: {}",
            lost.join(", ")
        )));
    }
    super::row(conn, uid)
}

/// Why a design with a doc target of its own may not sit inside another design's tree: the root's
/// export inlines it ([`export::tree_text`]), so it would be written twice. The one wording for
/// both ways in — nesting such a design, and giving a nested one a target.
pub(super) fn exported_twice(design_uid: &str) -> crate::Error {
    invalid(format!(
        "design {design_uid} would be exported twice — to its own doc target and inside its root \
         design's — so a nested design has no doc target of its own; give the root one instead"
    ))
}

/// The steps each span of `designs` counts as staged into now, for the spans with any, by uid.
fn staged_spans(
    conn: &Connection,
    designs: &[(ItemId, String)],
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut out = BTreeMap::new();
    for (design, _) in designs {
        let (doc, _) = load(conn, *design)?;
        let text = doc.text();
        for span in span_items(conn, *design)? {
            let steps = view(conn, &span, &doc, &text)?.steps;
            if !steps.is_empty() {
                out.insert(span.uid, steps);
            }
        }
    }
    Ok(out)
}

/// The position after every child `parent` holds, so a new child sorts last even after a sibling
/// was moved away (counting the children ties with a remaining one).
fn next_position(conn: &Connection, parent: ItemId) -> Result<i64> {
    let max: Option<i64> = conn
        .prepare_cached("SELECT MAX(position) FROM containment WHERE parent_item_id = ?1")?
        .query_row([parent.get()], |r| r.get(0))?;
    Ok(max.map_or(0, |m| m.saturating_add(1)))
}

/// What [`split`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    /// The design split, as it lists now.
    pub parent: DesignRow,
    /// The child designs, one per section, in order.
    pub children: Vec<DesignRow>,
    /// Spans moved into a child.
    pub moved: usize,
    /// Of those, the ones whose approval was carried over.
    pub carried: usize,
}

/// A heading line: its byte offset in the text, its level, and its title.
struct Heading {
    at: usize,
    level: usize,
    title: String,
}

/// The ATX headings of `text` outside fenced code, in order.
fn headings(text: &str) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let opens = ["```", "~~~"].into_iter().find(|f| trimmed.starts_with(f));
        match (fence, opens) {
            (Some(f), Some(o)) if f == o => fence = None,
            (None, Some(o)) => fence = Some(o),
            (None, None) => {
                let level = line.bytes().take_while(|b| *b == b'#').count();
                let rest = &line[level..];
                if (1..=6).contains(&level) && (rest.starts_with(' ') || rest.trim().is_empty()) {
                    out.push(Heading {
                        at,
                        level,
                        title: rest.trim().trim_end_matches('#').trim().to_owned(),
                    });
                }
            }
            _ => {}
        }
        at += line.len();
    }
    out
}

/// A section to split out: its byte range in the text, and its heading's title.
type Section = (usize, usize, String);

/// The sections of `text` at heading `level`: each heading of that level to the next heading of
/// that level or higher, or the end.
fn sections_of(text: &str, level: usize, uid: &str) -> Result<Vec<Section>> {
    let all = headings(text);
    let cuts: Vec<&Heading> = all.iter().filter(|h| h.level <= level).collect();
    let sections: Vec<Section> = cuts
        .iter()
        .enumerate()
        .filter(|(_, h)| h.level == level)
        .map(|(i, h)| {
            (
                h.at,
                cuts.get(i + 1).map_or(text.len(), |n| n.at),
                h.title.clone(),
            )
        })
        .collect();
    if sections.is_empty() {
        return Err(invalid(format!(
            "design {uid} has no level-{level} heading (`{} `) to split at",
            "#".repeat(level)
        )));
    }
    if let Some((_, _, t)) = sections.iter().find(|(_, _, t)| t.is_empty()) {
        return Err(invalid(format!(
            "a level-{level} heading has no title to name its design: {t:?}"
        )));
    }
    Ok(sections)
}

/// The sections' UTF-16 bounds, and which spans move into each, with their ranges relative to the
/// section. A span crossing a section's edge refuses the split.
#[allow(clippy::type_complexity)] // two parallel per-section lists, read once by `split`
fn assign(
    now: &Crdt,
    text: &str,
    sections: &[Section],
    spans: Vec<SpanItem>,
) -> Result<(Vec<(u32, u32)>, Vec<Vec<(SpanItem, u32, u32)>>)> {
    let mut bounds = Vec::new();
    for (s, e, _) in sections {
        bounds.push((
            crdt::byte_to_utf16(text, *s)?,
            crdt::byte_to_utf16(text, *e)?,
        ));
    }
    let mut moving: Vec<Vec<(SpanItem, u32, u32)>> = sections.iter().map(|_| Vec::new()).collect();
    for span in spans {
        let Some((s, e)) = now.range(&span.uid) else {
            continue;
        };
        let mut home = None;
        for (i, (from, to)) in bounds.iter().enumerate() {
            let inside = s >= *from && e <= *to;
            if s < *to && e > *from && !inside {
                return Err(invalid(format!(
                    "span {} crosses the edge of section {:?}: split never cuts a span — split \
                     the span first, or split at another level",
                    span.uid, sections[i].2
                )));
            }
            if inside && e > s {
                home = Some(i);
            }
        }
        if let Some(i) = home {
            let (from, _) = bounds[i];
            moving[i].push((span, s - from, e - from));
        }
    }
    Ok((bounds, moving))
}

/// Split design `uid` into child designs, one per heading of `level` (2: `##`), against the version
/// `base` the caller read. Each section — its heading to the next heading of that level or higher —
/// moves out of the parent into a new design of the same repo, titled by its heading and nested
/// under the parent in order; the parent keeps everything else. **One audited write**, which `jkb
/// undo` refuses by name ([`super::unrevertable`]): undone text comes back as new characters, so the
/// moved spans could not be anchored on it again.
///
/// Approvals survive it. An approval attests to words by their Yjs ids, and moved text gets new
/// ids, so each span whose approval holds at `base` is re-attested in the child to the same words:
/// same reviewer, same approver and time, recording the design and version it was carried from.
/// The span's item moves rather than being recreated, so its `stages` edges — and the STAGED or
/// IMPLEMENTED derived from them — carry over: a child stages into its root's plans
/// ([`root_of`]). A span whose approval does not hold moves as PROPOSED. A carried span that would
/// not read approved in its child refuses the whole split rather than silently demoting it.
///
/// # Errors
/// An unknown design, a `base` that is not its current version, a level outside 1–6, no heading of
/// that level, a span crossing a section's edge (the split never cuts a span), or a database error.
pub fn split(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    base: &str,
    level: usize,
) -> Result<Split> {
    if !(1..=6).contains(&level) {
        return Err(invalid(format!(
            "--level {level}: a heading level is 1 to 6"
        )));
    }
    let id = design_id(conn, uid)?;
    let base = Version::parse(base)?;
    let (now, version) = load(conn, id)?;
    if base != version {
        return Err(invalid(format!(
            "design {uid} changed after the version you read ({}) — re-read with `jkb design cat` \
             and split the version you read",
            base.token()
        )));
    }
    let repo = export::repo_of(conn, id)?
        .ok_or_else(|| invalid(format!("design {uid} is not under designs/<repo>")))?;
    let text = now.text();
    let sections = sections_of(&text, level, uid)?;
    // The export appends a design's children after its own text (`export::tree_text`), so a split
    // keeps the export unchanged only when the sections are the design's tail: contiguous, running
    // to the end, and the design has no children whose text would come before them. Anything else
    // would reorder the docs/ file, and a split cannot be undone (review round 1).
    let contiguous = sections.windows(2).all(|w| w[0].1 == w[1].0);
    if !contiguous || sections.last().map(|s| s.1) != Some(text.len()) {
        return Err(invalid(format!(
            "design {uid} has text after a level-{level} section (a higher-level heading), which \
             its export would move after the split-out sections — split at a level whose \
             sections run to the end of the design"
        )));
    }
    if !children_of(conn, id)?.is_empty() {
        return Err(invalid(format!(
            "design {uid} already has child designs, whose text its export places after its own — \
             a split would put the new sections after them; split a design before nesting under it"
        )));
    }
    let before = before_write(conn, id, &now)?;
    let (bounds, moving) = assign(&now, &text, &sections, span_items(conn, id)?)?;
    // A thread is not carried: its anchors would be left on deleted characters, detached for good
    // (a split cannot be undone). An open one on a section refuses the split; resolve it first.
    for t in super::threads::list(conn, uid)? {
        if t.status == "open"
            && t.anchored
            && bounds.iter().any(|(a, b)| t.start < *b && t.end > *a)
        {
            return Err(invalid(format!(
                "open thread {} is on text the split would move, and threads are not carried — \
                 resolve it first",
                t.uid
            )));
        }
    }

    // Take the sections out of the parent, last first so earlier offsets hold, with the anchors of
    // the spans leaving it.
    let leaving: Vec<String> = moving
        .iter()
        .flatten()
        .map(|(s, _, _)| s.uid.clone())
        .collect();
    let ((), update) = now.change(|txn, body, anchors| {
        for (from, to) in bounds.iter().rev() {
            body.remove_range(txn, *from, to - from);
        }
        for uid in &leaving {
            anchors.remove(txn, uid);
        }
        Ok(())
    })?;
    let update = update.ok_or_else(|| invalid("the split removed nothing"))?;
    let seq = append_row_as(conn, meta, id, uid, &update, true)?;

    let first = next_position(conn, id)?;
    let mut children = Vec::new();
    let mut carried = 0;
    for (i, ((s, e, title), spans)) in sections.iter().zip(moving).enumerate() {
        let section = &text[*s..*e];
        // Trailing blank lines stay behind unless a span covers them.
        let covered = spans.iter().map(|(_, _, e)| *e).max().unwrap_or(0);
        let trimmed = section.trim_end();
        let body = if crdt::utf16_len(trimmed)? >= covered {
            trimmed
        } else {
            section
        };
        let row = create(conn, meta, &repo, title, body)?;
        let child = design_id(conn, &row.uid)?;
        containment::contain(conn, meta, child, id, first + i64::try_from(i).unwrap_or(0))?;
        if !spans.is_empty() {
            carry(
                conn,
                meta,
                uid,
                &base,
                child,
                &row.uid,
                &before.held,
                spans,
                &mut carried,
            )?;
        }
        children.push(super::row(conn, &row.uid)?);
    }
    finish(conn, id, &now, &before, Some(seq), None)?;
    Ok(Split {
        parent: super::row(conn, uid)?,
        children,
        moved: leaving.len(),
        carried,
    })
}

/// Anchor `spans` in the new child design and move their items into it, carrying each approval
/// that held in the parent (`held`) by attesting the same words in the child.
#[allow(clippy::too_many_arguments)] // one call site, each argument a distinct fact of the split
fn carry(
    conn: &Connection,
    meta: &WriteMeta,
    parent_uid: &str,
    base: &Version,
    child: ItemId,
    child_uid: &str,
    held: &[String],
    spans: Vec<(SpanItem, u32, u32)>,
    carried: &mut usize,
) -> Result<()> {
    let (doc, _) = load(conn, child)?;
    let ((), update) = doc.change(|txn, body, anchors| {
        for (span, s, e) in &spans {
            Crdt::anchor(txn, body, anchors, &span.uid, *s, *e)?;
        }
        Ok(())
    })?;
    let update = update.ok_or_else(|| invalid("anchoring the moved spans wrote nothing"))?;
    let seq = append_row(conn, meta, child, child_uid, &update)?;
    let version = Version {
        seq,
        state_vector: doc.state_vector(),
    };
    let text = doc.text();
    for (position, (mut span, s, e)) in spans.into_iter().enumerate() {
        containment::contain(
            conn,
            meta,
            span.id,
            child,
            i64::try_from(position).unwrap_or(i64::MAX),
        )?;
        let keep = held.contains(&span.uid);
        let Some(obj) = span.metadata.as_object_mut() else {
            return Err(invalid(format!(
                "span {} has metadata that is not an object",
                span.uid
            )));
        };
        obj.insert("design".to_owned(), Value::String(child_uid.to_owned()));
        if keep {
            let attested = STANDARD.encode(crdt::encode_snapshot(&doc.attest(s, e)?));
            if let Some(Value::Object(approval)) = obj.get_mut("approval") {
                approval.insert("attested".to_owned(), Value::String(attested));
                approval.insert("version".to_owned(), Value::String(version.token()));
                approval.insert(
                    "carried_from".to_owned(),
                    serde_json::json!({ "design": parent_uid, "version": base.token() }),
                );
            }
        } else {
            obj.remove("approval");
        }
        span.design = child;
        if keep {
            let state = view(conn, &span, &doc, &text)?.state;
            if state == SpanState::Proposed {
                return Err(invalid(format!(
                    "span {} would read PROPOSED in its new design, so the split would lose its \
                     approval — nothing was split",
                    span.uid
                )));
            }
            *carried += 1;
        }
        set_metadata(conn, meta, span.id, &span.metadata)?;
    }
    let none = Before {
        overlaps: std::collections::BTreeSet::new(),
        held: Vec::new(),
    };
    finish(conn, child, &doc, &none, Some(seq), None)?;
    Ok(())
}
