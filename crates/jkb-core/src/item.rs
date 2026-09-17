//! Item repository: the atomic knowledge/graph node.

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::{Error as TypeError, ItemId, Resolution};

use crate::changelog::{Entity, Op};
use crate::sql::like_escape;
use crate::store::WriteMeta;
use crate::{changelog, Error, Result};

/// The fields needed to create an item. `content_hash`, when present, is the
/// global dedup key (design D4).
pub struct NewItem {
    /// Stable string identity (e.g. `book:sicp`, `b3:<hash>:<idx>`).
    pub uid: String,
    /// Item kind (e.g. `document`, `chunk`, `note`, `task`).
    pub kind: String,
    /// Text content, if any.
    pub content: Option<String>,
    /// Content hash (blake3 hex); identical content dedups to one item.
    pub content_hash: Option<String>,
    /// MIME type, if known.
    pub mime: Option<String>,
}

/// Insert `item`, or return the id of the existing item with the same
/// `content_hash` (global dedup). Records a changelog entry on insert.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn upsert(conn: &Connection, meta: &WriteMeta, item: &NewItem) -> Result<ItemId> {
    if let Some(hash) = item.content_hash.as_deref() {
        let existing: Option<i64> = conn
            .prepare_cached("SELECT id FROM items WHERE content_hash = ?1")?
            .query_row([hash], |row| row.get(0))
            .optional()?;
        if let Some(id) = existing {
            return Ok(ItemId::new(id));
        }
    }

    let id: i64 = conn
        .prepare_cached(
            "INSERT INTO items (uid, kind, content, content_hash, mime)
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
        )?
        .query_row(
            params![
                item.uid,
                item.kind,
                item.content,
                item.content_hash,
                item.mime
            ],
            |row| row.get(0),
        )?;

    let after = json!({
        "uid": item.uid.clone(),
        "kind": item.kind.clone(),
        "mime": item.mime.clone(),
    });
    changelog::upsert(
        conn,
        meta,
        Entity::Items,
        &id.to_string(),
        None,
        Some(&after),
    )?;

    Ok(ItemId::new(id))
}

/// Look up an item's id by its stable `uid`, if it exists.
///
/// # Errors
/// Returns an error if the query fails.
pub fn id_for_uid(conn: &Connection, uid: &str) -> Result<Option<ItemId>> {
    let id: Option<i64> = conn
        .prepare_cached("SELECT id FROM items WHERE uid = ?1")?
        .query_row([uid], |row| row.get(0))
        .optional()?;
    Ok(id.map(ItemId::new))
}

/// Fetch an item's text `content`, if any (the item must exist).
///
/// # Errors
/// Returns an error if the query fails.
pub fn get_content(conn: &Connection, item: ItemId) -> Result<Option<String>> {
    let content: Option<Option<String>> = conn
        .prepare_cached("SELECT content FROM items WHERE id = ?1")?
        .query_row([item.get()], |row| row.get::<_, Option<String>>(0))
        .optional()?;
    Ok(content.flatten())
}

/// One item matched by [`grep`] — enough to locate it and extract the matching lines.
#[derive(Debug, Clone)]
pub struct GrepRow {
    /// The item id.
    pub id: ItemId,
    /// The stable uid.
    pub uid: String,
    /// The item kind.
    pub kind: String,
    /// The item's full text content (the match lives inside it).
    pub content: String,
}

/// Literal-substring content search (grep semantics), optionally scoped to the namespace
/// subtree rooted at `scope` (the namespace itself or any descendant, matched via any
/// placement). Matching is a plain substring test — no regex or globbing. Case-sensitive by
/// default; with `ignore_case` the fold is done in Rust (full Unicode via `to_lowercase`),
/// **not** `SQLite`'s `lower` (which folds only ASCII), so accented text matches and the
/// result agrees with a caller re-scanning the same content. Each matching item is returned
/// once, ordered by uid.
///
/// # Errors
/// Returns an error if the query fails.
pub fn grep(
    conn: &Connection,
    pattern: &str,
    scope: Option<&str>,
    ignore_case: bool,
) -> Result<Vec<GrepRow>> {
    let mut out = Vec::new();
    grep_each(conn, pattern, scope, ignore_case, |row| {
        out.push(row);
        true
    })?;
    Ok(out)
}

/// [`grep`], handing each matching item to `each` as it is read instead of collecting them, so a
/// caller that keeps only part of each (the matching lines, a count) never holds every item's content
/// at once. `each` returns whether to continue.
///
/// # Errors
/// Returns an error if the query fails.
pub fn grep_each(
    conn: &Connection,
    pattern: &str,
    scope: Option<&str>,
    ignore_case: bool,
    mut each: impl FnMut(GrepRow) -> bool,
) -> Result<()> {
    use rusqlite::types::Value;
    let mut sql = String::from(
        "SELECT i.id, i.uid, i.kind, i.content FROM items i WHERE i.content IS NOT NULL",
    );
    let mut params: Vec<Value> = Vec::new();
    // Case-sensitive matching is a literal `instr` filter pushed into SQL (fast, exact).
    // Case-insensitive is folded in Rust below — `SQLite`'s `lower` is ASCII-only, so an
    // in-SQL `-i` filter would both miss non-ASCII and disagree with a Rust re-scan.
    if !ignore_case {
        sql.push_str(" AND instr(i.content, ?) > 0");
        params.push(Value::Text(pattern.to_owned()));
    }
    if let Some(s) = scope {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM placements p JOIN namespaces n ON n.id = p.namespace_id
                          WHERE p.item_id = i.id AND (n.path = ? OR n.path LIKE ? ESCAPE '\\'))",
        );
        params.push(Value::Text(s.to_owned()));
        params.push(Value::Text(format!("{}/%", like_escape(s))));
    }
    sql.push_str(" ORDER BY i.uid");

    let needle = pattern.to_lowercase();
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        Ok(GrepRow {
            id: ItemId::new(r.get(0)?),
            uid: r.get(1)?,
            kind: r.get(2)?,
            content: r.get(3)?,
        })
    })?;
    for row in rows {
        let row = row?;
        // For `-i`, keep only rows that actually contain the needle under a Unicode fold —
        // the SQL query pre-filtered by scope only, not by text.
        if ignore_case && !row.content.to_lowercase().contains(&needle) {
            continue;
        }
        if !each(row) {
            break;
        }
    }
    Ok(())
}

/// A full item row (metadata + content) for detail views (`jkb item show`).
#[derive(Debug, Clone)]
pub struct ItemMeta {
    /// Row id.
    pub id: ItemId,
    /// Stable uid.
    pub uid: String,
    /// Item kind (`document`/`chunk`/`task`/`text`/`view`/…).
    pub kind: String,
    /// Text content, if any.
    pub content: Option<String>,
    /// Content hash (blake3 hex), if content-addressed.
    pub content_hash: Option<String>,
    /// MIME type, if known.
    pub mime: Option<String>,
    /// Task status, if a task.
    pub status: Option<String>,
    /// How the unit **ended** — the outcome axis, orthogonal to `status` (design
    /// Dmem.3). `None` (NULL) reads as [`jkb_types::Resolution::Unresolved`].
    pub resolution: Option<String>,
    /// Priority, if set.
    pub priority: Option<i64>,
    /// Due date, if set.
    pub due: Option<String>,
    /// Creation timestamp (ISO).
    pub created_at: String,
    /// Last-update timestamp (ISO).
    pub updated_at: String,
}

/// Whether `line` ends a task's body in a `tasks.md`: blank once trimmed. The one copy — the `tasks`
/// serializer closes a body on it, and an edit that would put one inside a body is refused by
/// [`edit_content`], because the text after it would come back from the file as section prose.
#[must_use]
pub fn ends_task_body(line: &str) -> bool {
    line.trim().is_empty()
}

/// Whether an item's content is written into a file by the `tasks` serializer — decided by the
/// serializer that owns its binding ([`crate::binding::serializer_for`]), not by how the uri is
/// spelled: a `#` in a document's own filename (`C#.md`) made a whole-file note read as a task.
///
/// # Errors
/// Returns an error if the read fails.
pub fn in_tasks_file(conn: &Connection, item: ItemId) -> Result<bool> {
    Ok(crate::binding::serializer_for(conn, item)?.as_deref() == Some("tasks"))
}

/// Replace an item's content with `text`, or append it, through [`set_content`] — the one rule for an
/// edit, shared by `jkb task edit` (`jkb_api::tasks::edit`) and `jkb item edit`. An item in a tasks
/// file ([`in_tasks_file`]) appends with a single newline (its body is contiguous indented lines) and
/// refuses a result `tasks_problem` names — the tasks serializer's own round trip
/// (`jkb_sync::task_content_problem`), handed in because core does not depend on it: a blank line
/// ending the body, an indented checkbox becoming a child task, a trailing `^x` or `@x` becoming an
/// identity or a due date. The *result* is judged, not the text sent. Any other item appends after a
/// blank line. With `max_bytes`, a result longer than that is refused. Answers whether the item is in
/// a tasks file.
///
/// # Errors
/// A validation error for a refused result, [`jkb_types::Error::NotFound`] via [`set_content`], or a
/// failed read or write.
pub fn edit_content(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    text: &str,
    append: bool,
    max_bytes: Option<usize>,
    tasks_problem: &dyn Fn(&str) -> Option<String>,
) -> Result<bool> {
    let tasks_file = in_tasks_file(conn, item)?;
    let content = if append {
        let separator = if tasks_file { "\n" } else { "\n\n" };
        match get_content(conn, item)? {
            Some(existing) if !existing.is_empty() => format!("{existing}{separator}{text}"),
            _ => text.to_owned(),
        }
    } else {
        text.to_owned()
    };
    // The size first: the round-trip probe parses the whole text, inside the writer's transaction.
    if let Some(max) = max_bytes {
        if content.len() > max {
            return Err(TypeError::Validation(format!(
                "an item's content of at most {max} bytes ({} after this edit)",
                content.len()
            ))
            .into());
        }
    }
    if tasks_file {
        if let Some(problem) = tasks_problem(&content) {
            return Err(TypeError::Validation(format!(
                "this task is written into a tasks.md, and its text would not come back from the file \
                 as written: {problem}"
            ))
            .into());
        }
    }
    set_content(conn, meta, item, &content, None)?;
    Ok(tasks_file)
}

/// The first non-blank line of `content`, trimmed and untruncated.
///
/// The **one** copy of this derivation. There were four, at three different truncation
/// lengths, over content that carries checkbox and quick-add syntax (`- [ ] … !p1 ^id`) for
/// every finding and every serializer-imported task — so the first change to it, such as
/// stripping the trailing `^id`, had to be made in four places with nothing forcing the
/// fourth, after which a staging row, a gate refusal and `jkb task show` would disagree
/// about one task's name. Truncation is deliberately left to each caller: a listing, a
/// staging row and a refusal message have genuinely different widths. It lives here rather
/// than in the CLI because the typed operations (`jkb-api`) name items too.
#[must_use]
pub fn first_nonblank(content: &str) -> &str {
    content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

/// The first non-blank line of `content` in at most `max` characters, a cut one ending in `…` — the
/// one-line snippet listings print. The one copy: the CLI renders with it, and the ops that send a
/// snippet cut it with it, so a line is never cut twice to two different lengths.
#[must_use]
pub fn snippet(content: &str, max: usize) -> String {
    let line = first_nonblank(content);
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let head: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// The width of a listing's snippet, in characters.
pub const SNIPPET_CHARS: usize = 100;

/// An item's display title: its first non-blank line, falling back to the uid for an item
/// with no body. Untruncated — see [`first_nonblank`].
#[must_use]
pub fn title_of(meta: &ItemMeta) -> String {
    title_from(&meta.uid, meta.content.as_deref())
}

/// [`title_of`] from an item's uid and content, for a caller holding a row other than [`ItemMeta`].
#[must_use]
pub fn title_from(uid: &str, content: Option<&str>) -> String {
    match content.map(first_nonblank) {
        Some(line) if !line.is_empty() => line.to_owned(),
        _ => uid.to_owned(),
    }
}

/// Fetch an item's full row by id, or `None` if it does not exist.
///
/// # Errors
/// Returns an error if the query fails.
pub fn get(conn: &Connection, item: ItemId) -> Result<Option<ItemMeta>> {
    conn.prepare_cached(
        "SELECT id, uid, kind, content, content_hash, mime, status, resolution, priority, due,
                created_at, updated_at
         FROM items WHERE id = ?1",
    )?
    .query_row([item.get()], |r| {
        Ok(ItemMeta {
            id: ItemId::new(r.get(0)?),
            uid: r.get(1)?,
            kind: r.get(2)?,
            content: r.get(3)?,
            content_hash: r.get(4)?,
            mime: r.get(5)?,
            status: r.get(6)?,
            resolution: r.get(7)?,
            priority: r.get(8)?,
            due: r.get(9)?,
            created_at: r.get(10)?,
            updated_at: r.get(11)?,
        })
    })
    .optional()
    .map_err(Into::into)
}

/// Every row in `items`, keyed by id, in one query.
///
/// Batches [`get`] for callers holding a candidate set. A per-item `get` is a round-trip
/// serialized on the writer thread, so a caller resolving N items pays N of them — fine for
/// three, not for a view that redraws on every database write.
///
/// # Errors
/// Returns an error if the query fails.
pub fn get_many(conn: &Connection, items: &[ItemId]) -> Result<HashMap<ItemId, ItemMeta>> {
    let mut out = HashMap::new();
    if items.is_empty() {
        return Ok(out);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT id, uid, kind, content, content_hash, mime, status, resolution, priority, due,
                created_at, updated_at
         FROM items WHERE id IN (SELECT value FROM json_each(?1))",
    )?;
    let rows = stmt.query_map([crate::sql::json_ids(items.iter().map(|i| i.get()))], |r| {
        Ok(ItemMeta {
            id: ItemId::new(r.get(0)?),
            uid: r.get(1)?,
            kind: r.get(2)?,
            content: r.get(3)?,
            content_hash: r.get(4)?,
            mime: r.get(5)?,
            status: r.get(6)?,
            resolution: r.get(7)?,
            priority: r.get(8)?,
            due: r.get(9)?,
            created_at: r.get(10)?,
            updated_at: r.get(11)?,
        })
    })?;
    for row in rows {
        let meta = row?;
        out.insert(meta.id, meta);
    }
    Ok(out)
}

/// Set `item`'s [`Resolution`] — how the unit **ended** (design Dmem.3), orthogonal to
/// its `status`. Recorded in the changelog like any other mutation.
///
/// Setting a tombstone resolution ([`Resolution::DeadEnd`]/[`Resolution::Superseded`])
/// deliberately does **not** delete anything: the unit is retained so the next agent can
/// see it was tried. Link the edge that killed it ([`jkb_types::EdgeType::Refutes`],
/// `RulesOut`, `Supersedes`) so the tombstone says *why*.
///
/// # Errors
/// Returns [`jkb_types::Error::NotFound`] if `item` does not exist; otherwise a
/// database error.
pub fn set_resolution(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    resolution: Resolution,
) -> Result<()> {
    let before: Option<String> = conn
        .prepare_cached("SELECT resolution FROM items WHERE id = ?1")?
        .query_row([item.get()], |row| row.get::<_, Option<String>>(0))
        .optional()?
        .ok_or_else(|| Error::Types(TypeError::NotFound(format!("item {item}"))))?;
    conn.prepare_cached(
        "UPDATE items SET resolution = ?2,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?1",
    )?
    .execute(params![item.get(), resolution.as_str()])?;
    changelog::append(
        conn,
        meta,
        Op::Update,
        Entity::Items,
        &item.get().to_string(),
        Some(&json!({ "resolution": before })),
        Some(&json!({ "resolution": resolution.as_str() })),
    )?;
    Ok(())
}

/// Read `item`'s [`Resolution`], defaulting a NULL column to
/// [`Resolution::Unresolved`]. Returns `None` if the item does not exist.
///
/// # Errors
/// Returns an error if the query fails, or a validation error if the stored string is
/// not a known resolution (the `V007` CHECK constraint makes that unreachable in
/// practice — it would mean the column was written outside jkb).
pub fn get_resolution(conn: &Connection, item: ItemId) -> Result<Option<Resolution>> {
    let raw: Option<Option<String>> = conn
        .prepare_cached("SELECT resolution FROM items WHERE id = ?1")?
        .query_row([item.get()], |row| row.get::<_, Option<String>>(0))
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let text = raw.unwrap_or_default();
    Resolution::from_str_opt(&text).map(Some).ok_or_else(|| {
        Error::Types(TypeError::Validation(format!(
            "item {item} has unknown resolution `{text}`"
        )))
    })
}

/// What [`remove`] deleted, so a caller can report it honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// The removed item's uid.
    pub uid: String,
    /// The removed item's kind.
    pub kind: String,
    /// How many placements went with it.
    pub placements: usize,
    /// How many edges went with it (in both directions).
    pub edges: usize,
    /// How many tag applications went with it.
    pub tags: usize,
}

/// A complete snapshot of `item` and everything that cascades with it, as the JSON the
/// changelog stores in `before`.
///
/// `items` has `ON DELETE CASCADE` children (placements, edges, tag applications, the
/// binding) and cascades do **not** pass through the repositories, so they generate no
/// changelog entries of their own. Capturing them here is the only thing that makes the
/// delete reversible — without it, `undo` would restore a naked item stripped of its
/// placements and, worse, its edges.
fn snapshot(conn: &Connection, item: ItemId) -> Result<serde_json::Value> {
    let id = item.get();
    let row = conn
        .prepare_cached(
            "SELECT uid, kind, content, content_hash, mime, status, resolution, priority, due,
                    metadata, created_at, updated_at, claimant_id, claimed_at
             FROM items WHERE id = ?1",
        )?
        .query_row([id], |r| {
            Ok(json!({
                "id": id,
                "uid": r.get::<_, String>(0)?,
                "kind": r.get::<_, String>(1)?,
                "content": r.get::<_, Option<String>>(2)?,
                "content_hash": r.get::<_, Option<String>>(3)?,
                "mime": r.get::<_, Option<String>>(4)?,
                "status": r.get::<_, Option<String>>(5)?,
                "resolution": r.get::<_, Option<String>>(6)?,
                "priority": r.get::<_, Option<i64>>(7)?,
                "due": r.get::<_, Option<String>>(8)?,
                "metadata": r.get::<_, String>(9)?,
                "created_at": r.get::<_, String>(10)?,
                "updated_at": r.get::<_, String>(11)?,
                "claimant_id": r.get::<_, Option<String>>(12)?,
                "claimed_at": r.get::<_, Option<String>>(13)?,
            }))
        })
        .optional()?
        .ok_or_else(|| Error::Types(TypeError::NotFound(format!("item {item}"))))?;

    let placements = {
        let mut stmt = conn.prepare_cached(
            "SELECT namespace_id, role, position, metadata FROM placements WHERE item_id = ?1",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok(json!({
                "namespace_id": r.get::<_, i64>(0)?,
                "role": r.get::<_, String>(1)?,
                "position": r.get::<_, i64>(2)?,
                "metadata": r.get::<_, String>(3)?,
            }))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let tags = {
        let mut stmt = conn.prepare_cached(
            "SELECT facet, value, props FROM tag_applications WHERE item_id = ?1",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok(json!({
                "facet": r.get::<_, String>(0)?,
                "value": r.get::<_, String>(1)?,
                "props": r.get::<_, String>(2)?,
            }))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let edges = {
        let mut stmt = conn.prepare_cached(
            "SELECT src_item_id, dst_item_id, type, props, weight, created_at FROM edges
             WHERE src_item_id = ?1 OR dst_item_id = ?1",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok(json!({
                "src": r.get::<_, i64>(0)?,
                "dst": r.get::<_, i64>(1)?,
                "type": r.get::<_, String>(2)?,
                "props": r.get::<_, String>(3)?,
                "weight": r.get::<_, Option<f64>>(4)?,
                "created_at": r.get::<_, String>(5)?,
            }))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let binding = conn
        .prepare_cached(
            "SELECT uri, sync_mode, serializer, last_synced_hash, last_synced_at
             FROM bindings WHERE item_id = ?1",
        )?
        .query_row([id], |r| {
            Ok(json!({
                "uri": r.get::<_, String>(0)?,
                "sync_mode": r.get::<_, Option<String>>(1)?,
                "serializer": r.get::<_, Option<String>>(2)?,
                "last_synced_hash": r.get::<_, Option<String>>(3)?,
                "last_synced_at": r.get::<_, Option<String>>(4)?,
            }))
        })
        .optional()?;

    let (contained_by, contains) = containment_snapshot(conn, id)?;

    Ok(json!({
        "item": row,
        "placements": placements,
        "tags": tags,
        "edges": edges,
        "binding": binding,
        "contained_by": contained_by,
        "contains": contains,
    }))
}

/// The containment rows a delete takes with the item, both directions: the row saying which
/// item contains this one, and the rows saying which items it contains.
///
/// Restoring only the `parent_of` edge put a parent task back with its subtask edge intact
/// but no containment row — and `SUBTASK_CLAUSE`, the anti-join that holds a parent off the
/// ready frontier, reads `containment`, not the edge. So the parent came back pickable with a
/// live open child, and a restored document's chunks were unreachable from `jkb ls <doc>`.
fn containment_snapshot(
    conn: &Connection,
    id: i64,
) -> Result<(Option<serde_json::Value>, Vec<serde_json::Value>)> {
    let contained_by = conn
        .prepare_cached(
            "SELECT parent_item_id, position FROM containment WHERE child_item_id = ?1",
        )?
        .query_row([id], |r| {
            Ok(json!({
                "parent_item_id": r.get::<_, i64>(0)?,
                "position": r.get::<_, i64>(1)?,
            }))
        })
        .optional()?;
    let contains = {
        let mut stmt = conn.prepare_cached(
            "SELECT child_item_id, position FROM containment WHERE parent_item_id = ?1",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok(json!({
                "child_item_id": r.get::<_, i64>(0)?,
                "position": r.get::<_, i64>(1)?,
            }))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok((contained_by, contains))
}

/// Whether `item` carries investigation **memory** that a delete would destroy: a tombstone
/// resolution, or an incident `refutes`/`rules_out` edge recording that something was tried
/// and killed. Returns the reason, or `None` if it holds no such memory.
fn memory_reason(conn: &Connection, item: ItemId) -> Result<Option<String>> {
    if let Some(resolution) = get_resolution(conn, item)? {
        if resolution.is_tombstone() {
            return Ok(Some(format!(
                "it is a `{}` tombstone — the record that this was tried and did not work",
                resolution.as_str()
            )));
        }
    }
    let killer: Option<String> = conn
        .prepare_cached(
            "SELECT type FROM edges
             WHERE dst_item_id = ?1 AND type IN ('refutes', 'rules_out') LIMIT 1",
        )?
        .query_row([item.get()], |r| r.get(0))
        .optional()?;
    if let Some(edge_type) = killer {
        return Ok(Some(format!(
            "a `{edge_type}` edge records what killed it — deleting it loses why"
        )));
    }
    Ok(None)
}

/// Delete `item` and everything that cascades with it (placements, edges, tag applications,
/// its binding), recording a **complete snapshot** in the changelog so [`crate::undo`] can
/// put it all back.
///
/// This is the escape hatch for detritus — an item the KB holds that nothing should reference
/// any more — not a routine verb. Two guards stand in the way unless `force` is set, because
/// each names a case where deleting is either destructive or a lie:
///
/// - **It is investigation memory.** A `dead_end`/`superseded` tombstone, or a unit with an
///   incident `refutes`/`rules_out` edge, is the anti-retread record: the whole reason it is
///   retained is so the next agent does not redo it (design Dmem.3/Dmem.8). Deleting one is
///   the single most costly thing you can do to an investigation.
/// - **It is bound to a synced file.** If the source file still declares it, the next sync
///   recreates it — so the delete looks like it worked and then quietly undoes itself. Remove
///   it from the file instead.
///
/// # Errors
/// Returns [`jkb_types::Error::NotFound`] if `item` does not exist, a validation error if a
/// guard refuses (naming the guard and that `--force` overrides it), or a database error.
pub fn remove(conn: &Connection, meta: &WriteMeta, item: ItemId, force: bool) -> Result<Removed> {
    let before = snapshot(conn, item)?;

    if !force {
        if let Some(reason) = memory_reason(conn, item)? {
            return Err(Error::Types(TypeError::Validation(format!(
                "refusing to delete {item}: {reason}. Investigation memory is meant to be \
                 retained — mark it `abandoned` with `jkb inv resolve` if it is merely stale. \
                 Pass --force if you really mean to destroy it."
            ))));
        }
        let uri = before
            .get("binding")
            .and_then(|b| b.get("uri"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if uri.starts_with("file://") {
            return Err(Error::Types(TypeError::Validation(format!(
                "refusing to delete {item}: it is bound to the synced file `{uri}`, so if that \
                 file still declares it the next sync will recreate it. Remove it from the \
                 source file instead, or pass --force if the file no longer has it."
            ))));
        }
    }

    let counts = |key: &str| {
        before
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len)
    };
    let removed = Removed {
        uid: before["item"]["uid"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        kind: before["item"]["kind"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        placements: counts("placements"),
        edges: counts("edges"),
        tags: counts("tags"),
    };

    // One DELETE; `PRAGMA foreign_keys = ON` cascades the children, which is why the snapshot
    // above had to capture them.
    conn.prepare_cached("DELETE FROM items WHERE id = ?1")?
        .execute([item.get()])?;
    changelog::append(
        conn,
        meta,
        Op::Delete,
        Entity::Items,
        &item.get().to_string(),
        Some(&before),
        None,
    )?;
    Ok(removed)
}

/// Set `item`'s resolution from a manual string, rejecting unknown values with an
/// actionable error. The string boundary for the CLI and MCP edges (mirroring
/// [`crate::task::set_status_str`]).
///
/// # Errors
/// Returns a validation error if `resolution` is not one of
/// `unresolved`/`success`/`dead_end`/`superseded`/`abandoned`, or the errors of
/// [`set_resolution`].
pub fn set_resolution_str(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    resolution: &str,
) -> Result<()> {
    let parsed = Resolution::from_str_opt(resolution).ok_or_else(|| {
        Error::Types(TypeError::Validation(format!(
            "unknown resolution `{resolution}`; expected one of {}",
            Resolution::ALL
                .iter()
                .map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    })?;
    set_resolution(conn, meta, item, parsed)
}

/// The items each of `children` was **derived from**, as `child -> sources`; children with
/// no `derived_from` edge are absent. The inverse of [`derived_kind_counts`], batched for a
/// listing that must know which of its rows are already reachable via a container.
///
/// # Errors
/// Returns an error if the query fails.
pub fn derived_from(
    conn: &Connection,
    children: &[ItemId],
) -> Result<std::collections::HashMap<ItemId, Vec<ItemId>>> {
    let mut out: std::collections::HashMap<ItemId, Vec<ItemId>> = std::collections::HashMap::new();
    if children.is_empty() {
        return Ok(out);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT src_item_id, dst_item_id FROM edges
          WHERE type = 'derived_from' AND src_item_id IN (SELECT value FROM json_each(?1))",
    )?;
    let rows = stmt.query_map(
        [crate::sql::json_ids(children.iter().map(|c| c.get()))],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;
    for row in rows {
        let (child, source) = row?;
        out.entry(ItemId::new(child))
            .or_default()
            .push(ItemId::new(source));
    }
    Ok(out)
}

/// The items **derived from** `parent` of the given `kind`, in document order.
///
/// The motivating case is a document's chunks: ingest links `chunk --derived_from-->
/// document` and records the fragment's index as the `chunk` placement's `position`, which
/// is the only faithful ordering (a uid ending `:10` sorts before `:9` lexicographically).
/// Items with no such placement sort last, by uid, so an unexpected shape still lists.
///
/// # Errors
/// Returns an error if the query fails.
pub fn derived_children(conn: &Connection, parent: ItemId, kind: &str) -> Result<Vec<ItemMeta>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.id, i.uid, i.kind, i.content, i.content_hash, i.mime, i.status,
                i.resolution, i.priority, i.due, i.created_at, i.updated_at
           FROM edges e
           JOIN items i ON i.id = e.src_item_id
           LEFT JOIN placements p ON p.item_id = i.id AND p.role = 'chunk'
          WHERE e.dst_item_id = ?1 AND e.type = 'derived_from' AND i.kind = ?2
          ORDER BY p.position IS NULL, p.position, i.uid",
    )?;
    let rows = stmt.query_map(params![parent.get(), kind], |row| {
        Ok(ItemMeta {
            id: ItemId::new(row.get(0)?),
            uid: row.get(1)?,
            kind: row.get(2)?,
            content: row.get(3)?,
            content_hash: row.get(4)?,
            mime: row.get(5)?,
            status: row.get(6)?,
            resolution: row.get(7)?,
            priority: row.get(8)?,
            due: row.get(9)?,
            created_at: row.get(10)?,
            updated_at: row.get(11)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// The path of the namespace an item is primarily placed under, if it has one.
///
/// "Primary" is the item's home as opposed to a `reference` mirror, so this answers *where
/// this item lives* rather than everywhere it is visible.
///
/// # Errors
/// Returns an error if the query fails.
pub fn primary_namespace(conn: &Connection, item: ItemId) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
              WHERE p.item_id = ?1 AND p.role = 'primary' LIMIT 1",
        )?
        .query_row([item.get()], |r| r.get(0))
        .optional()?)
}

/// How many items of `kind` were **derived from** each of `parents`, as a
/// `parent -> count` map (parents with none are absent).
///
/// The motivating case is chunks: ingest stores a document plus one item per chunk, each
/// linked `chunk --derived_from--> document`. A tree that lists those chunks alongside the
/// document buries the real content under its own index units, but the *number* of them is
/// worth showing next to the document they came from — so this counts them in one grouped
/// query rather than a lookup per document.
///
/// # Errors
/// Returns an error if the query fails.
pub fn derived_kind_counts(
    conn: &Connection,
    parents: &[ItemId],
    kind: &str,
) -> Result<std::collections::HashMap<ItemId, i64>> {
    let mut out = std::collections::HashMap::new();
    if parents.is_empty() {
        return Ok(out);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT e.dst_item_id, COUNT(*) FROM edges e
         JOIN items i ON i.id = e.src_item_id
         WHERE e.type = 'derived_from' AND i.kind = ?1
           AND e.dst_item_id IN (SELECT value FROM json_each(?2))
         GROUP BY e.dst_item_id",
    )?;
    let ids = crate::sql::json_ids(parents.iter().map(|p| p.get()));
    let rows = stmt.query_map(params![kind, ids], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (id, count) = row?;
        out.insert(ItemId::new(id), count);
    }
    Ok(out)
}

/// Replace an item's `content` (and `content_hash`), bumping `updated_at` and
/// recording the change. Used by file sync when a bound file changes on disk; the
/// FTS index follows automatically via the `V002` triggers.
///
/// # Errors
/// Returns [`jkb_types::Error::NotFound`] if the item does not exist; otherwise a
/// database error.
pub fn set_content(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    content: &str,
    content_hash: Option<&str>,
) -> Result<()> {
    // The previous body, under the column names it came from. It used to be logged as
    // `{"content_len": n}`, which reads like a before-state and restores nothing: `jkb undo`
    // of a sync import would have left the imported text in place. Both columns are needed —
    // restoring the content while leaving `content_hash` describing the new text would make the
    // item its own mismatch. `changelog::write` now refuses a payload that names anything but
    // this table's columns, so the old shape cannot come back.
    let before: Option<(Option<String>, Option<String>)> = conn
        .prepare_cached("SELECT content, content_hash FROM items WHERE id = ?1")?
        .query_row([item.get()], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let (before_content, before_hash) =
        before.ok_or_else(|| Error::Types(TypeError::NotFound(format!("item {item}"))))?;
    conn.prepare_cached(
        "UPDATE items SET content = ?2, content_hash = ?3,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?1",
    )?
    .execute(params![item.get(), content, content_hash])?;
    changelog::append(
        conn,
        meta,
        Op::Update,
        Entity::Items,
        &item.get().to_string(),
        Some(&json!({ "content": before_content, "content_hash": before_hash })),
        // The *after* side stays a summary. Only `before` is read — it is what `undo` writes back —
        // and a file sync calls this on every disk edit, so keeping the new body here too would
        // store a second copy of every document per edit for nobody's benefit.
        Some(&json!({ "content_hash": content_hash, "content_len": content.len() })),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{grep, remove, upsert, NewItem};
    use crate::{ns, placement, Db};
    use jkb_types::PlacementRole;
    use proptest::prelude::*;

    /// An id freed by a delete must never be handed to a later item (design D40).
    ///
    /// `items.id` is a rowid alias, and `SQLite` reissues the largest freed rowid — so before
    /// `AUTOINCREMENT` a new item inherited a deleted item's row in every derived index that
    /// cannot carry a foreign key. Concretely, it inherited its **embedding**: vector search
    /// returned the new item for the deleted item's text, and `index_pending` considered it
    /// already indexed, so it could never be re-embedded.
    ///
    /// This is the invariant, not the four call-site sweeps that preceded it. If the V010
    /// migration is ever reverted or a rebuild of `items` drops the `AUTOINCREMENT`, this
    /// fails immediately rather than surfacing as a wrong search result months later.
    #[test]
    fn a_deleted_items_id_is_never_reused() {
        let db = Db::open_in_memory().unwrap();
        let first = db
            .write_txn("t", |conn, meta| upsert(conn, meta, &note("gone", None)))
            .unwrap();
        db.write_txn("t", move |conn, meta| remove(conn, meta, first, true))
            .unwrap();
        let second = db
            .write_txn("t", |conn, meta| upsert(conn, meta, &note("fresh", None)))
            .unwrap();
        assert!(
            second.get() > first.get(),
            "id {} was reissued after {} was deleted — a new item would inherit its vector",
            second.get(),
            first.get()
        );
    }

    fn note(uid: &str, hash: Option<&str>) -> NewItem {
        NewItem {
            uid: uid.to_owned(),
            kind: "note".to_owned(),
            content: Some("body".to_owned()),
            content_hash: hash.map(str::to_owned),
            mime: None,
        }
    }

    #[test]
    fn grep_is_literal_case_sensitive_and_scoped() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            let a = ns::ensure(conn, "proj/a")?;
            let b = ns::ensure(conn, "proj/b")?;
            for (uid, body, at) in [
                ("n:1", "buy a 6-inch Pipe", a),
                ("n:2", "review the pipe", a),
                ("n:3", "unrelated note", b),
            ] {
                let item = upsert(
                    conn,
                    meta,
                    &NewItem {
                        uid: uid.to_owned(),
                        kind: "note".to_owned(),
                        content: Some(body.to_owned()),
                        content_hash: None,
                        mime: None,
                    },
                )?;
                placement::place(conn, meta, item, at, PlacementRole::Primary, 0)?;
            }
            Ok(())
        })
        .unwrap();

        // Case-sensitive substring: "pipe" hits n:2 only; "Pipe" hits n:1 only.
        let hits = db.read(|conn| grep(conn, "pipe", None, false)).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.uid.as_str()).collect::<Vec<_>>(),
            ["n:2"]
        );
        // Case-insensitive: both.
        let hits = db.read(|conn| grep(conn, "pipe", None, true)).unwrap();
        assert_eq!(hits.len(), 2);
        // Scoped to proj/a: n:3 is excluded even though it wouldn't match anyway; a scope
        // with no textual match returns empty.
        let hits = db
            .read(|conn| grep(conn, "note", Some("proj/b"), false))
            .unwrap();
        assert_eq!(
            hits.iter().map(|h| h.uid.as_str()).collect::<Vec<_>>(),
            ["n:3"]
        );
        let hits = db
            .read(|conn| grep(conn, "pipe", Some("proj/b"), true))
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn grep_ci_folds_unicode_and_scope_does_not_leak_across_siblings() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            // A non-ASCII case difference, and two sibling namespaces where one name is a
            // LIKE prefix of the other under a wildcard (`_` matches any char).
            for (uid, body, ns_path) in [
                ("n:accent", "the RÉSUMÉ is ready", "_sys/z"),
                ("n:bleed", "résumé elsewhere", "xsys/z"),
            ] {
                let at = ns::ensure(conn, ns_path)?;
                let item = upsert(
                    conn,
                    meta,
                    &NewItem {
                        uid: uid.to_owned(),
                        kind: "note".to_owned(),
                        content: Some(body.to_owned()),
                        content_hash: None,
                        mime: None,
                    },
                )?;
                placement::place(conn, meta, item, at, PlacementRole::Primary, 0)?;
            }
            Ok(())
        })
        .unwrap();

        // Unicode-aware `-i`: lowercase "résumé" matches the uppercase "RÉSUMÉ".
        let hits = db.read(|conn| grep(conn, "résumé", None, true)).unwrap();
        assert_eq!(
            hits.len(),
            2,
            "both accented items match under a Unicode fold"
        );

        // Scope `_sys` must NOT leak into `xsys` — `_` is escaped, not a LIKE wildcard.
        let hits = db.read(|conn| grep(conn, "é", Some("_sys"), true)).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.uid.as_str()).collect::<Vec<_>>(),
            ["n:accent"],
            "the `_sys` scope must not match the sibling `xsys`"
        );
    }

    #[test]
    fn resolution_defaults_to_unresolved_and_round_trips() {
        use super::{get, get_resolution, set_resolution, set_resolution_str};
        use jkb_types::Resolution;

        let db = Db::open_in_memory().unwrap();
        let id = db
            .write_txn("t", |conn, meta| upsert(conn, meta, &note("n:1", None)))
            .unwrap();

        // A fresh item's NULL column reads as `unresolved` — no back-fill needed.
        assert_eq!(
            db.read(move |conn| get_resolution(conn, id)).unwrap(),
            Some(Resolution::Unresolved)
        );
        assert_eq!(
            db.read(move |conn| get(conn, id))
                .unwrap()
                .unwrap()
                .resolution,
            None,
            "the stored column stays NULL until explicitly set"
        );

        // A tombstone resolution RETAINS the item (the graveyard is the memory).
        db.write_txn("t", move |conn, meta| {
            set_resolution(conn, meta, id, Resolution::DeadEnd)
        })
        .unwrap();
        let meta = db.read(move |conn| get(conn, id)).unwrap().unwrap();
        assert_eq!(meta.resolution.as_deref(), Some("dead_end"));
        assert_eq!(meta.uid, "n:1", "the dead end is retained, not deleted");

        // The string boundary rejects unknown values with the valid set named.
        let err = db
            .write_txn("t", move |conn, meta| {
                set_resolution_str(conn, meta, id, "refuted")
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("refuted"), "{err}");
        assert!(err.contains("dead_end"), "{err}");

        // Setting a resolution is changelogged like any other mutation.
        let updates: i64 = db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM changelog
                     WHERE entity_type = 'items' AND op = 'update'
                       AND after LIKE '%dead_end%'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(updates, 1);
    }

    #[test]
    fn insert_records_a_changelog_entry_with_actor() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("cli", |conn, meta| upsert(conn, meta, &note("n:1", None)))
            .unwrap();

        let (count, actor): (i64, String) = db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*), COALESCE(MAX(actor), '') FROM changelog
                     WHERE entity_type = 'items'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(actor, "cli");
    }

    proptest! {
        /// Inserting the same `content_hash` any number of times yields one item.
        #[test]
        fn same_content_hash_dedups_to_one_item(hash in "[a-f0-9]{16}", copies in 1_u8..6) {
            let db = Db::open_in_memory().unwrap();
            for i in 0..copies {
                let hash = hash.clone();
                db.write_txn("t", move |conn, meta| {
                    upsert(conn, meta, &NewItem {
                        uid: format!("u{i}"),
                        kind: "note".to_owned(),
                        content: Some("body".to_owned()),
                        content_hash: Some(hash),
                        mime: None,
                    })
                })
                .unwrap();
            }
            let n = db
                .read(|conn| Ok(conn.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))?))
                .unwrap();
            prop_assert_eq!(n, 1);
        }
    }
}
