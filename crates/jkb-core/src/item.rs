//! Item repository: the atomic knowledge/graph node.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::{Error as TypeError, ItemId};

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

/// Literal-substring content search (grep semantics), optionally scoped to the namespace
/// subtree rooted at `scope` (the namespace itself or any descendant, matched via any
/// placement). Matching is a plain substring test through `SQLite`'s `instr` — case-sensitive
/// unless `ignore_case`, no regex or globbing — so results are predictable for humans and
/// agents alike. Each matching item is returned once, ordered by uid.
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
    let needle = if ignore_case {
        pattern.to_lowercase()
    } else {
        pattern.to_owned()
    };
    let haystack = if ignore_case {
        "lower(i.content)"
    } else {
        "i.content"
    };
    let mut sql = format!(
        "SELECT i.id, i.uid, i.kind, i.content FROM items i
         WHERE i.content IS NOT NULL AND instr({haystack}, ?1) > 0"
    );
    let mut params: Vec<Value> = vec![Value::Text(needle)];
    if let Some(s) = scope {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM placements p JOIN namespaces n ON n.id = p.namespace_id
                          WHERE p.item_id = i.id AND (n.path = ?2 OR n.path LIKE ?3))",
        );
        params.push(Value::Text(s.to_owned()));
        params.push(Value::Text(format!("{s}/%")));
    }
    sql.push_str(" ORDER BY i.uid");
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
        out.push(row?);
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
        "SELECT id, uid, kind, content, content_hash, mime, status, priority, due,
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
            priority: r.get(7)?,
            due: r.get(8)?,
            created_at: r.get(9)?,
            updated_at: r.get(10)?,
        })
    })
    .optional()
    .map_err(Into::into)
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
