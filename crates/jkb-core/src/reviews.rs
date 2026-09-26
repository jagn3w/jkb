//! A task's review record: which review rounds were recorded against it, at which HEAD, and whether a
//! landing waived the gate (design D52.7, supersedes D38.4's facets).
//!
//! **Not tags.** These three facts decide the land gate, and a tag is content any writer may set —
//! including the sync engine applying a line of a `tasks.md` that an agent in the dev container
//! edited (`#review-waived=x` waived the gate; hole H3). `tag.rs` records why the answer is not a
//! reserved facet: that apparatus was tried once, for `base`, and six choke points did not close it.
//! The fact moves out of the facet namespace instead, and the gate reads only this table, so a
//! `reviewed=` tag is ordinary content that nothing trusts.
//!
//! **Append-only, and not changelogged**, like `task_transitions`: a review happened, and an undo of
//! the transaction that recorded it does not un-happen it. The writers are `task.review_record`
//! ([`record`]), `task.review_file` ([`record_filing`]) and a landing ([`waive`], [`gate_on_host`]).
//!
//! **A round is snapshotted when it is first recorded** ([`Round`]): which findings it holds, which of
//! them were must-fix, and the file each names. Everything that asks "was the last round clean", "did
//! the same area repeat", or "is this finding one of the task's" reads the snapshot, never the
//! findings' live priority, placement or tags — those are ordinary task content the implementer under
//! review can edit.

use std::collections::{BTreeSet, HashMap};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use jkb_types::{Error as TypeError, ItemId};

use crate::query::{Query, Scope};
use crate::store::WriteMeta;
use crate::{item, tag, Error, Result};

/// The facet a finding's area (the file it is in) is recorded under.
pub const FACET_AREA: &str = "area";

/// What the land gate reads about a task's reviews.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewState {
    /// The HEAD the newest recorded review ran against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed: Option<String>,
    /// Every recorded round's findings namespace, in recording order. The gate unions them: a newer
    /// round never retires an older round's open must-fix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
    /// The HEAD a `--no-review` landing waived the gate for, newest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waived: Option<String>,
    /// The HEAD a landing ran its gate on the host for, rather than in the dev container
    /// (`--gate-on-host`, D52.12), newest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_on_host: Option<String>,
}

impl ReviewState {
    /// Nothing recorded at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reviewed.is_none()
            && self.namespaces.is_empty()
            && self.waived.is_none()
            && self.gate_on_host.is_none()
    }
}

const MAX_TEXT: usize = 1024;

fn check(what: &str, v: &str) -> Result<()> {
    if v.is_empty() || v.len() > MAX_TEXT {
        return Err(Error::Types(TypeError::Validation(format!(
            "a {what} of 1 to {MAX_TEXT} bytes"
        ))));
    }
    Ok(())
}

/// Which namespaces a caller may record as a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundSource<'a> {
    /// Only one this principal's own `task.review_file` filed ([`record_filing`]); the round is exactly
    /// what it filed. Anyone but the operator.
    Filed(&'a str),
    /// Any namespace, its round being the tasks under it — the operator's `/review-log`, whose
    /// findings reach the KB through a mount rather than a filing.
    AnyNamespace,
}

/// Record that a review of `task` ran at `sha`, filing its findings under `ns`. Recording the same
/// round twice is a no-op. The round is snapshotted the first time any task records it.
///
/// # Errors
/// [`Error::Types`] for an empty or oversized value, a namespace `source` does not allow, or a
/// database error.
pub fn record(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    ns: &str,
    sha: &str,
    actor: &str,
    source: RoundSource<'_>,
) -> Result<()> {
    check("findings namespace", ns)?;
    check("sha", sha)?;
    snapshot_round(conn, ns, actor, source)?;
    let exists: bool = conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM reviews
                            WHERE item_id = ?1 AND kind = 'recorded' AND ns = ?2 AND sha = ?3)",
        )?
        .query_row(params![task.get(), ns, sha], |r| r.get(0))?;
    if !exists {
        insert(conn, meta, task, "recorded", Some(ns), sha, actor)?;
    }
    Ok(())
}

/// Record that `task` landed at `sha` with the review gate waived (`--no-review`).
///
/// # Errors
/// [`Error::Types`] for an empty or oversized value, or a database error.
pub fn waive(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    sha: &str,
    actor: &str,
) -> Result<()> {
    check("sha", sha)?;
    insert(conn, meta, task, "waived", None, sha, actor)
}

/// Record that a landing of `task` at `sha` ran its gate on the host (`--gate-on-host`, D52.12): the
/// candidate's own, container-written code, run as the operator — by the operator's choice, visibly.
///
/// # Errors
/// [`Error::Types`] for an empty or oversized value, or a database error.
pub fn gate_on_host(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    sha: &str,
    actor: &str,
) -> Result<()> {
    check("sha", sha)?;
    insert(conn, meta, task, "gate_on_host", None, sha, actor)
}

fn insert(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    kind: &str,
    ns: Option<&str>,
    sha: &str,
    actor: &str,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO reviews (item_id, kind, ns, sha, actor, txn_id, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
    )?
    .execute(params![task.get(), kind, ns, sha, actor, meta.txn_id])?;
    Ok(())
}

/// Record that `filed_by` filed `items` into the new namespace `ns` (`task.review_file`).
///
/// # Errors
/// [`Error::Types`] for an empty or oversized namespace, or a database error.
pub fn record_filing(conn: &Connection, ns: &str, items: &[ItemId], filed_by: &str) -> Result<()> {
    check("findings namespace", ns)?;
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO review_filings (ns, item_id, filed_by) VALUES (?1, ?2, ?3)",
    )?;
    for id in items {
        stmt.execute(params![ns, id.get(), filed_by])?;
    }
    Ok(())
}

/// The review namespace that `ns` is or lies inside, if any: one filed or recorded, or any
/// `repos/<repo>/codereviews/<folder>` — where `/review-log` mounts a round, which is neither until
/// the operator records it, and which a recording then snapshots whole (review round 4: a line a
/// worker put there first became another task's finding) — and each of these through its
/// `tasks/…` mirror (D26/D32, [`crate::task::mirror_forms`]), which a recording may name as well
/// (rounds 5–6).
/// A caller held to one task places nothing there, whatever its task's own placements: a worker bound
/// to a finding lives in the round, and "beside its task" would otherwise be inside it.
///
/// # Errors
/// A database error.
pub fn review_namespace_containing(conn: &Connection, ns: &str) -> Result<Option<String>> {
    for candidate in &crate::task::mirror_forms(ns) {
        let known = conn
            .prepare_cached(
                "SELECT ns FROM (SELECT ns FROM review_rounds UNION SELECT ns FROM review_filings)
                 WHERE ns = ?1 OR substr(?1, 1, length(ns) + 1) = ns || '/'
                 LIMIT 1",
            )?
            .query_row([candidate], |r| r.get(0))
            .optional()?;
        if known.is_some() {
            return Ok(known);
        }
    }
    // By name, under any of its names, the same answer from each (round 7).
    Ok(crate::task::mirror_forms(ns).iter().find_map(|form| {
        let parts: Vec<&str> = form.split('/').take(4).collect();
        match parts.as_slice() {
            ["repos" | "tasks", repo, "codereviews", folder]
                if !repo.is_empty() && !folder.is_empty() =>
            {
                Some(parts.join("/"))
            }
            _ => None,
        }
    }))
}

/// The recorded round `ns` is, lies inside, or contains, if any. Filing there would put findings under
/// a round another recording already fixed.
///
/// # Errors
/// A database error.
pub fn round_overlapping(conn: &Connection, ns: &str) -> Result<Option<String>> {
    // Under any of its names: a round recorded through its `tasks/` mirror still holds its home
    // (review round 6).
    for candidate in &crate::task::mirror_forms(ns) {
        let found = conn
            .prepare_cached(
                "SELECT ns FROM review_rounds
                 WHERE ns = ?1 OR substr(?1, 1, length(ns) + 1) = ns || '/'
                    OR substr(ns, 1, length(?1) + 1) = ?1 || '/'
                 LIMIT 1",
            )?
            .query_row([candidate], |r| r.get(0))
            .optional()?;
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

/// The area a finding names: its `area=` facet, or — for a finding filed before that facet existed —
/// the file in its title, which `review file` wrote as `summary — file[:line]`.
fn area_of(meta: &item::ItemMeta, tags: &[(String, String)]) -> Option<String> {
    if let Some((_, v)) = tags.iter().find(|(f, _)| f == FACET_AREA) {
        return Some(v.clone());
    }
    let title = item::title_of(meta);
    let (_, tail) = title.rsplit_once(" — ")?;
    let file = match tail.rsplit_once(':') {
        Some((f, line)) if line.chars().all(|c| c.is_ascii_digit()) && !line.is_empty() => f,
        _ => tail,
    };
    (!file.is_empty() && !file.contains(' ')).then(|| file.to_owned())
}

/// Snapshot the round `ns` if no recording has yet, returning its id (its recording order).
fn snapshot_round(
    conn: &Connection,
    ns: &str,
    actor: &str,
    source: RoundSource<'_>,
) -> Result<i64> {
    let existing: Option<i64> = conn
        .prepare_cached("SELECT id FROM review_rounds WHERE ns = ?1")?
        .query_row([ns], |r| r.get(0))
        .optional()?;
    let filings: Vec<(ItemId, String)> = conn
        .prepare_cached(
            "SELECT item_id, filed_by FROM review_filings WHERE ns = ?1 ORDER BY item_id",
        )?
        .query_map([ns], |r| Ok((ItemId::new(r.get::<_, i64>(0)?), r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if let RoundSource::Filed(by) = source {
        // Checked even for a round already recorded: a caller that may record only its own filings
        // must not ride on a namespace the operator, or another worker, chose.
        if filings.is_empty() || filings.iter().any(|(_, f)| f != by) {
            return Err(Error::Types(TypeError::Validation(format!(
                "`{ns}` is not a namespace this caller filed with `jkb task review file` — only the \
                 operator records a review someone else filed, or whose findings reached the KB \
                 another way (a mounted `/review-log` folder), because the round a task records \
                 decides which tasks its workers may write. Record it on the host: `jkb task review \
                 record --branch <branch> --findings {ns}`"
            ))));
        }
    }
    let filed: Vec<ItemId> = filings.into_iter().map(|(id, _)| id).collect();
    if let Some(id) = existing {
        return Ok(id);
    }
    let ids = if filed.is_empty() {
        Query {
            kind: Some("task".to_owned()),
            scope: Scope::Subtree(ns.to_owned()),
            ..Query::default()
        }
        .evaluate(conn)?
    } else {
        filed
    };
    let id: i64 = conn
        .prepare_cached(
            "INSERT INTO review_rounds (ns, actor, recorded_at)
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) RETURNING id",
        )?
        .query_row(params![ns, actor], |r| r.get(0))?;
    let metas = item::get_many(conn, &ids)?;
    let tags = tag::applications_for(conn, &ids)?;
    let mut stmt = conn.prepare_cached(
        "INSERT INTO review_round_findings (round_id, item_id, must_fix, area)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for item_id in &ids {
        let Some(m) = metas.get(item_id) else {
            continue;
        };
        let must_fix = m.priority.is_some_and(|p| p <= 1);
        let area = area_of(m, tags.get(item_id).map_or(&[][..], Vec::as_slice));
        stmt.execute(params![id, item_id.get(), must_fix, area])?;
    }
    Ok(id)
}

/// One review round, as it stood when it was first recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Round {
    /// Its findings namespace.
    pub ns: String,
    /// Its recording order: rounds are compared by when they were recorded, never by what their
    /// findings look like now.
    pub filed: i64,
    /// How many findings it held as must-fix (priority 1 or above) when recorded, **at any status**
    /// since: fixing a finding does not make the round that found it clean.
    pub must_fix: usize,
    /// The files its must-fix findings name.
    pub areas: Vec<String>,
}

/// The rounds recorded under `namespaces`, oldest recording first. A namespace no review recorded is
/// not a round.
///
/// # Errors
/// A database error.
pub fn rounds_in(conn: &Connection, namespaces: &[String]) -> Result<Vec<Round>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare_cached(
        "SELECT r.id, f.must_fix, f.area FROM review_rounds r
         LEFT JOIN review_round_findings f ON f.round_id = r.id
         WHERE r.ns = ?1",
    )?;
    for ns in namespaces {
        let rows = stmt.query_map([ns], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<bool>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut round: Option<Round> = None;
        let mut areas = BTreeSet::new();
        for row in rows {
            let (id, must_fix, area) = row?;
            let r = round.get_or_insert_with(|| Round {
                ns: ns.clone(),
                filed: id,
                must_fix: 0,
                areas: Vec::new(),
            });
            if must_fix == Some(true) {
                r.must_fix += 1;
                areas.extend(area);
            }
        }
        if let Some(mut r) = round {
            r.areas = areas.into_iter().collect();
            out.push(r);
        }
    }
    out.sort_by_key(|r| r.filed);
    Ok(out)
}

/// The findings `task`'s recorded rounds held as must-fix when they were recorded — what the land
/// gate counts as open until each is finished, wherever it has since been moved and whatever its
/// priority now reads.
///
/// # Errors
/// A database error.
pub fn must_fix_findings(conn: &Connection, namespaces: &[String]) -> Result<Vec<ItemId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT f.item_id FROM review_rounds r JOIN review_round_findings f ON f.round_id = r.id
         WHERE r.ns = ?1 AND f.must_fix",
    )?;
    let mut out = BTreeSet::new();
    for ns in namespaces {
        for id in stmt.query_map([ns], |r| r.get::<_, i64>(0))? {
            out.insert(id?);
        }
    }
    Ok(out.into_iter().map(ItemId::new).collect())
}

/// Whether `target` is a finding of one of `task`'s recorded rounds — the findings a principal scoped
/// to `task` may work on. Read from the snapshot, so naming a namespace never widens it.
///
/// # Errors
/// A database error.
pub fn is_finding_of(conn: &Connection, task: ItemId, target: ItemId) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS (
                 SELECT 1 FROM reviews v
                 JOIN review_rounds r ON r.ns = v.ns
                 JOIN review_round_findings f ON f.round_id = r.id
                 WHERE v.item_id = ?1 AND v.kind = 'recorded' AND f.item_id = ?2)",
        )?
        .query_row(params![task.get(), target.get()], |r| r.get(0))?)
}

/// `task`'s review state.
///
/// # Errors
/// A database error.
pub fn state(conn: &Connection, task: ItemId) -> Result<ReviewState> {
    Ok(state_for(conn, &[task])?.remove(&task).unwrap_or_default())
}

/// The review state of every task in `tasks`, in one query. A task with no record is absent.
///
/// # Errors
/// A database error.
pub fn state_for(conn: &Connection, tasks: &[ItemId]) -> Result<HashMap<ItemId, ReviewState>> {
    let mut out: HashMap<ItemId, ReviewState> = HashMap::new();
    if tasks.is_empty() {
        return Ok(out);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT item_id, kind, ns, sha FROM reviews
         WHERE item_id IN (SELECT value FROM json_each(?1)) ORDER BY id",
    )?;
    let rows = stmt.query_map(
        [crate::sql::json_ids(tasks.iter().map(|id| id.get()))],
        |r| {
            Ok((
                ItemId::new(r.get::<_, i64>(0)?),
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        },
    )?;
    for row in rows {
        let (id, kind, ns, sha) = row?;
        let s = out.entry(id).or_default();
        match kind.as_str() {
            "waived" => {
                s.waived = Some(sha);
                continue;
            }
            "gate_on_host" => {
                s.gate_on_host = Some(sha);
                continue;
            }
            _ => {}
        }
        s.reviewed = Some(sha);
        if let Some(ns) = ns {
            if !s.namespaces.contains(&ns) {
                s.namespaces.push(ns);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{record, state, waive};
    use crate::task::{create, NewTask};
    use crate::Db;

    #[test]
    fn rounds_accumulate_in_order_and_the_newest_head_is_the_reviewed_one() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .write_txn("t", |c, m| create(c, m, &NewTask::new("task:r", "R")))
            .unwrap();
        db.write_txn("t", move |c, m| {
            record(
                c,
                m,
                id,
                "reviews/2",
                "bbb",
                "reviewer@a1",
                super::RoundSource::AnyNamespace,
            )?;
            record(
                c,
                m,
                id,
                "reviews/1",
                "aaa",
                "reviewer@a1",
                super::RoundSource::AnyNamespace,
            )?;
            record(
                c,
                m,
                id,
                "reviews/1",
                "aaa",
                "reviewer@a1",
                super::RoundSource::AnyNamespace,
            )?;
            waive(c, m, id, "ccc", "operator")?;
            super::gate_on_host(c, m, id, "ddd", "operator")
        })
        .unwrap();
        let s = db.read(move |c| state(c, id)).unwrap();
        assert_eq!(
            s.namespaces,
            vec!["reviews/2", "reviews/1"],
            "recording order, deduplicated"
        );
        assert_eq!(
            s.reviewed.as_deref(),
            Some("aaa"),
            "the newest record's head"
        );
        assert_eq!(s.waived.as_deref(), Some("ccc"));
        assert_eq!(s.gate_on_host.as_deref(), Some("ddd"));
        assert_eq!(s.reviewed.as_deref(), Some("aaa"), "neither is a review");
        assert!(db
            .write_txn("t", move |c, m| record(
                c,
                m,
                id,
                "",
                "x",
                "a",
                super::RoundSource::AnyNamespace
            ))
            .is_err());
    }
}
