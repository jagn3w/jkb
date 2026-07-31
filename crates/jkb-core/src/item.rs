//! Item repository: the atomic knowledge/graph node.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::{Error as TypeError, ItemId, Resolution};

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
    changelog::append(
        conn,
        meta,
        "insert",
        "items",
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

/// Escape `%`, `_`, and `\` for use inside a `LIKE … ESCAPE '\'` pattern, so a namespace
/// path (which can contain `_`, e.g. the reserved `_sys` root) is matched literally rather
/// than as wildcards.
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
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
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        // For `-i`, keep only rows that actually contain the needle under a Unicode fold —
        // the SQL query pre-filtered by scope only, not by text.
        if ignore_case && !row.content.to_lowercase().contains(&needle) {
            continue;
        }
        out.push(row);
    }
    Ok(out)
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
        "update",
        "items",
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

    Ok(json!({
        "item": row,
        "placements": placements,
        "tags": tags,
        "edges": edges,
        "binding": binding,
    }))
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
        "delete",
        "items",
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
    let before: Option<String> = conn
        .prepare_cached("SELECT content FROM items WHERE id = ?1")?
        .query_row([item.get()], |row| row.get::<_, Option<String>>(0))
        .optional()?
        .ok_or_else(|| Error::Types(TypeError::NotFound(format!("item {item}"))))?;
    conn.prepare_cached(
        "UPDATE items SET content = ?2, content_hash = ?3,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?1",
    )?
    .execute(params![item.get(), content, content_hash])?;
    changelog::append(
        conn,
        meta,
        "update",
        "items",
        &item.get().to_string(),
        Some(&json!({ "content_len": before.map(|c| c.len()) })),
        Some(&json!({ "content_len": content.len() })),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{grep, upsert, NewItem};
    use crate::{ns, placement, Db};
    use jkb_types::PlacementRole;
    use proptest::prelude::*;

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
