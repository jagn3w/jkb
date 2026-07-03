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
    use super::{upsert, NewItem};
    use crate::Db;
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
