//! Rendering helpers: fetch item rows for display and print them as human-readable
//! lines or `--json`.

use anyhow::Result;
use jkb_core::{tag, Db};
use jkb_types::ItemId;
use serde_json::{json, Value};

/// A denormalized item row for listing output.
pub struct DisplayItem {
    /// Row id.
    pub id: i64,
    /// Stable uid.
    pub uid: String,
    /// Item kind.
    pub kind: String,
    /// Status (tasks).
    pub status: Option<String>,
    /// Resolution — how a unit ended (investigation units); `None` = unresolved.
    pub resolution: Option<String>,
    /// Priority (tasks).
    pub priority: Option<i64>,
    /// Due date (tasks).
    pub due: Option<String>,
    /// A one-line content snippet.
    pub snippet: Option<String>,
    /// The namespace the item is placed under (primary preferred).
    pub namespace: Option<String>,
    /// Last-update timestamp (ISO), for `jkb recent` ordering.
    pub updated: Option<String>,
}

impl DisplayItem {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "uid": self.uid,
            "kind": self.kind,
            "status": self.status,
            "resolution": self.resolution,
            "priority": self.priority,
            "due": self.due,
            "namespace": self.namespace,
            "snippet": self.snippet,
            "updated": self.updated,
        })
    }

    fn to_line(&self) -> String {
        let mut parts = vec![format!("{:<24} [{}]", self.uid, self.kind)];
        if let Some(s) = &self.status {
            parts.push(format!("({s})"));
        }
        if let Some(r) = &self.resolution {
            parts.push(format!("<{r}>"));
        }
        if let Some(p) = self.priority {
            parts.push(format!("!p{p}"));
        }
        if let Some(d) = &self.due {
            parts.push(format!("@{d}"));
        }
        if let Some(ns) = &self.namespace {
            parts.push(format!("<{ns}>"));
        }
        if let Some(snip) = &self.snippet {
            if !snip.is_empty() {
                parts.push(format!("— {snip}"));
            }
        }
        parts.join(" ")
    }
}

/// The first non-blank line of `content`, trimmed and untruncated.
///
/// The **one** copy of this derivation. There were four, at three different truncation
/// lengths, over content that carries checkbox and quick-add syntax (`- [ ] … !p1 ^id`) for
/// every finding and every serializer-imported task — so the first change to it, such as
/// stripping the trailing `^id`, had to be made in four places with nothing forcing the
/// fourth, after which a staging row, a gate refusal and `jkb task show` would disagree
/// about one task's name. Truncation is deliberately left to each caller: a listing, a
/// staging row and a refusal message have genuinely different widths.
pub(crate) fn first_nonblank(content: &str) -> &str {
    content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

/// An item's display title: its first non-blank line, falling back to the uid for an item
/// with no body. Untruncated — see [`first_nonblank`].
pub(crate) fn title_of(meta: &jkb_core::item::ItemMeta) -> String {
    match meta.content.as_deref().map(first_nonblank) {
        Some(line) if !line.is_empty() => line.to_owned(),
        _ => meta.uid.clone(),
    }
}

/// A one-line snippet: the first non-empty line, trimmed to ~80 chars.
fn snippet(content: &str) -> String {
    let line = first_nonblank(content);
    let mut out: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        out.push('…');
    }
    out
}

/// Fetch display rows for `ids`, preserving their order and skipping any that no
/// longer exist.
///
/// # Errors
/// Returns an error if a read fails.
pub fn fetch_items(db: &Db, ids: &[ItemId]) -> Result<Vec<DisplayItem>> {
    let ids: Vec<i64> = ids.iter().map(|id| id.get()).collect();
    let rows = db.read(move |conn| {
        let mut out = Vec::new();
        for id in &ids {
            let row = conn
                .prepare_cached(
                    "SELECT id, uid, kind, status, resolution, priority, due, content, updated_at
                     FROM items WHERE id = ?1",
                )?
                .query_row([id], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                        r.get::<_, String>(8)?,
                    ))
                })
                .ok();
            let Some((id, uid, kind, status, resolution, priority, due, content, updated)) = row
            else {
                continue;
            };
            let namespace = conn
                .prepare_cached(
                    "SELECT n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
                     WHERE p.item_id = ?1
                     ORDER BY (p.role = 'primary') DESC, p.position LIMIT 1",
                )?
                .query_row([id], |r| r.get::<_, String>(0))
                .ok();
            out.push(DisplayItem {
                id,
                uid,
                kind,
                status,
                resolution,
                priority,
                due,
                snippet: content.as_deref().map(snippet),
                namespace,
                updated: Some(updated),
            });
        }
        Ok(out)
    })?;
    Ok(rows)
}

/// Facts about an item that are not the item's own row — a task's branch records — for the one
/// command that shows them.
///
/// Carried in rather than printed beside, because a `--json` consumer reading one document must
/// not get half the answer: `json` is merged into the item object and `lines` are printed inside
/// its header block.
#[derive(Default)]
pub struct Extra {
    /// Fields merged into the item's JSON object.
    pub json: serde_json::Map<String, Value>,
    /// Human lines printed after the item's own header fields.
    pub lines: Vec<String>,
}

/// Fetch and print a single item in full — every listing field plus the
/// untruncated `content`. Used by `jkb task show` so agents can read a task body
/// the frontier/query listings only snippet.
///
/// # Errors
/// Returns an error if the read fails or the item no longer exists.
pub fn print_item_full(db: &Db, id: ItemId, as_json: bool, extra: &Extra) -> Result<()> {
    let id = id.get();
    let row = db.read(move |conn| {
        let row = conn
            .prepare_cached(
                "SELECT id, uid, kind, status, priority, due, content
                 FROM items WHERE id = ?1",
            )?
            .query_row([id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            })
            .ok();
        let Some((id, uid, kind, status, priority, due, content)) = row else {
            return Ok(None);
        };
        let namespace = conn
            .prepare_cached(
                "SELECT n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
                 WHERE p.item_id = ?1
                 ORDER BY (p.role = 'primary') DESC, p.position LIMIT 1",
            )?
            .query_row([id], |r| r.get::<_, String>(0))
            .ok();
        // Tags carry where the work is happening (`branch=`/`onto=`) and whether it has been
        // reviewed (`reviewed=`/`review=`) — the state D38 introduced. Showing a task without
        // them means the only way to see why it will or will not land is another command.
        let tags = tag::applications(conn, ItemId::new(id))?;
        Ok(Some((
            uid, kind, status, priority, due, content, namespace, tags,
        )))
    })?;
    let Some((uid, kind, status, priority, due, content, namespace, tags)) = row else {
        anyhow::bail!("item {id} no longer exists");
    };
    let tag_pairs: Vec<String> = tags.iter().map(|(f, v)| format!("{f}={v}")).collect();
    if as_json {
        let mut v = json!({
            "id": id,
            "uid": uid,
            "kind": kind,
            "status": status,
            "priority": priority,
            "due": due,
            "namespace": namespace,
            "content": content,
            "tags": tags.iter().map(|(f, v)| json!({"facet": f, "value": v}))
                .collect::<Vec<_>>(),
        });
        // Merged into the SAME object rather than printed beside it: a caller with facts that are
        // not the item's own — a task's branch records — otherwise has to emit a second JSON
        // document, and a `--json` consumer reading one line gets half the answer.
        if let Some(map) = v.as_object_mut() {
            for (k, value) in &extra.json {
                map.insert(k.clone(), value.clone());
            }
        }
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else {
        println!("uid:       {uid}");
        println!("kind:      {kind}");
        if let Some(s) = &status {
            println!("status:    {s}");
        }
        if let Some(p) = priority {
            println!("priority:  {p}");
        }
        if let Some(d) = &due {
            println!("due:       {d}");
        }
        if let Some(ns) = &namespace {
            println!("namespace: {ns}");
        }
        if !tag_pairs.is_empty() {
            println!("tags:      {}", tag_pairs.join(", "));
        }
        for line in &extra.lines {
            println!("{line}");
        }
        println!();
        println!("{}", content.as_deref().unwrap_or("(no content)"));
    }
    Ok(())
}

/// Print items as JSON or human lines.
pub fn print_items(items: &[DisplayItem], as_json: bool) {
    if as_json {
        let arr: Vec<Value> = items.iter().map(DisplayItem::to_json).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Array(arr)).unwrap_or_default()
        );
    } else if items.is_empty() {
        println!("(no results)");
    } else {
        for item in items {
            println!("{}", item.to_line());
        }
    }
}
