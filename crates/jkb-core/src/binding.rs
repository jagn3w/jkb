//! Binding repository: where an item's bytes live and how they sync (design D3).
//!
//! Every item has at most one binding (the row's `item_id` is its rowid). The
//! default is `managed:`; a `file://` binding participates in sync.

use std::collections::HashMap;

use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::{ItemId, SyncMode};

use crate::sql::like_escape;
use crate::store::WriteMeta;
use crate::{changelog, Result};

/// An item's storage binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Where the bytes live (`managed:`, `file:///…`).
    pub uri: String,
    /// Sync direction, if this is a synced binding.
    pub sync_mode: Option<String>,
    /// Per-file serializer override (`NULL` inherits the mount's).
    pub serializer: Option<String>,
    /// Hash at last successful sync (for conflict detection).
    pub last_synced_hash: Option<String>,
}

/// Set (create or replace) an item's binding.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn set(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    uri: &str,
    sync_mode: Option<SyncMode>,
    serializer: Option<&str>,
) -> Result<()> {
    let mode = sync_mode.map(SyncMode::as_str);
    conn.prepare_cached(
        "INSERT INTO bindings (item_id, uri, sync_mode, serializer) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(item_id) DO UPDATE SET
             uri = excluded.uri, sync_mode = excluded.sync_mode, serializer = excluded.serializer",
    )?
    .execute(params![item.get(), uri, mode, serializer])?;
    let after =
        json!({ "item_id": item.get(), "uri": uri, "sync_mode": mode, "serializer": serializer });
    changelog::append(
        conn,
        meta,
        "insert",
        "bindings",
        &item.get().to_string(),
        None,
        Some(&after),
    )?;
    Ok(())
}

/// Reverse lookup: the item bound to `uri`, if any. `uri` is unique per synced
/// file, so this resolves a `file://` path back to its item during sync.
///
/// # Errors
/// Returns an error if the query fails.
pub fn item_for_uri(conn: &Connection, uri: &str) -> Result<Option<ItemId>> {
    let id: Option<i64> = conn
        .prepare_cached("SELECT item_id FROM bindings WHERE uri = ?1")?
        .query_row([uri], |row| row.get(0))
        .optional()?;
    Ok(id.map(ItemId::new))
}

/// Batched reverse lookup: the `uri -> ItemId` map for every uri in `uris`, in a
/// single query. Uris with no binding are simply absent from the map. This is the
/// many-uri form of [`item_for_uri`], letting file sync resolve all of a file's
/// bindings in one round-trip instead of N.
///
/// # Errors
/// Returns an error if the query fails.
pub fn items_for_uris(conn: &Connection, uris: &[String]) -> Result<HashMap<String, ItemId>> {
    let mut out = HashMap::new();
    if uris.is_empty() {
        return Ok(out);
    }
    let placeholders = vec!["?"; uris.len()].join(", ");
    let sql = format!("SELECT uri, item_id FROM bindings WHERE uri IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(uris.iter()), |row| {
        Ok((row.get::<_, String>(0)?, ItemId::new(row.get::<_, i64>(1)?)))
    })?;
    for row in rows {
        let (uri, id) = row?;
        out.insert(uri, id);
    }
    Ok(out)
}

/// The distinct `file://` binding uris of items placed under `ns_path` or any
/// descendant namespace. Lets file sync reconcile items whose backing file was
/// created in the KB (needs export) or deleted on disk, not just files found by
/// walking the directory.
///
/// # Errors
/// Returns an error if the query fails.
pub fn synced_uris_under(conn: &Connection, ns_path: &str) -> Result<Vec<String>> {
    // Escape LIKE metacharacters in the path (`_sys`, `jkb-v1-foundation` all contain `_`),
    // else the subtree prefix would match sibling namespaces.
    let like = format!("{}/%", like_escape(ns_path));
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT b.uri FROM bindings b
         JOIN placements p ON p.item_id = b.item_id
         JOIN namespaces n ON n.id = p.namespace_id
         WHERE b.uri LIKE 'file://%' AND (n.path = ?1 OR n.path LIKE ?2 ESCAPE '\\')",
    )?;
    let rows = stmt.query_map(params![ns_path, like], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The `file://` binding uris belonging to one file: the bare `bare_uri` itself plus
/// any `bare_uri#<local_id>` fragments. A multi-item serializer binds each item it
/// parses out of a file to `file://<path>#<local_id>`; this is how the sync engine
/// gathers all of a file's items back together for reconciliation. Ordered for
/// determinism.
///
/// # Errors
/// Returns an error if the query fails.
pub fn synced_uris_for_file(conn: &Connection, bare_uri: &str) -> Result<Vec<String>> {
    // Escape LIKE metacharacters — file uris commonly contain `_` (a wildcard), so an
    // unescaped `<uri>#%` could gather a *different* file's item bindings.
    let fragment_like = format!("{}#%", like_escape(bare_uri));
    let mut stmt = conn.prepare_cached(
        "SELECT uri FROM bindings WHERE uri = ?1 OR uri LIKE ?2 ESCAPE '\\' ORDER BY uri",
    )?;
    let rows = stmt.query_map(params![bare_uri, fragment_like], |row| {
        row.get::<_, String>(0)
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Record a successful sync: stamp `last_synced_hash`/`last_synced_at` for `item`.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn mark_synced(conn: &Connection, meta: &WriteMeta, item: ItemId, hash: &str) -> Result<()> {
    conn.prepare_cached(
        "UPDATE bindings SET last_synced_hash = ?2,
             last_synced_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE item_id = ?1",
    )?
    .execute(params![item.get(), hash])?;
    changelog::append(
        conn,
        meta,
        "update",
        "bindings",
        &item.get().to_string(),
        None,
        Some(&json!({ "last_synced_hash": hash })),
    )?;
    Ok(())
}

/// Fetch an item's binding, if one is set.
///
/// # Errors
/// Returns an error if the query fails.
pub fn get(conn: &Connection, item: ItemId) -> Result<Option<Binding>> {
    let binding = conn
        .prepare_cached(
            "SELECT uri, sync_mode, serializer, last_synced_hash FROM bindings WHERE item_id = ?1",
        )?
        .query_row([item.get()], |row| {
            Ok(Binding {
                uri: row.get(0)?,
                sync_mode: row.get(1)?,
                serializer: row.get(2)?,
                last_synced_hash: row.get(3)?,
            })
        })
        .optional()?;
    Ok(binding)
}

#[cfg(test)]
mod tests {
    use super::{get, set, synced_uris_for_file};
    use crate::item::{upsert, NewItem};
    use crate::Db;
    use jkb_types::SyncMode;

    #[test]
    fn synced_uris_for_file_does_not_leak_across_underscore_siblings() {
        // Two files whose names differ only where one has `_` and the other any char — a
        // LIKE wildcard would conflate them when gathering a file's item bindings.
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            for (uid, uri) in [
                ("a", "file:///repo/a_b.md#one"),
                ("b", "file:///repo/a_b.md#two"),
                ("c", "file:///repo/axb.md#three"), // sibling: `x` where the other has `_`
            ] {
                let item = upsert(
                    conn,
                    meta,
                    &NewItem {
                        uid: uid.to_owned(),
                        kind: "task".to_owned(),
                        content: None,
                        content_hash: None,
                        mime: None,
                    },
                )?;
                set(
                    conn,
                    meta,
                    item,
                    uri,
                    Some(SyncMode::Bidirectional),
                    Some("tasks"),
                )?;
            }
            Ok(())
        })
        .unwrap();

        let uris = db
            .read(|conn| synced_uris_for_file(conn, "file:///repo/a_b.md"))
            .unwrap();
        assert_eq!(
            uris,
            vec![
                "file:///repo/a_b.md#one".to_owned(),
                "file:///repo/a_b.md#two".to_owned(),
            ],
            "the `axb.md` sibling must not be gathered via the `_` wildcard"
        );
    }

    #[test]
    fn binding_set_and_get_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let item = db
            .write_txn("t", |conn, meta| {
                let item = upsert(
                    conn,
                    meta,
                    &NewItem {
                        uid: "readme".to_owned(),
                        kind: "document".to_owned(),
                        content: None,
                        content_hash: None,
                        mime: None,
                    },
                )?;
                set(
                    conn,
                    meta,
                    item,
                    "file:///repo/README.md",
                    Some(SyncMode::Bidirectional),
                    Some("document"),
                )?;
                Ok(item)
            })
            .unwrap();

        let binding = db.read(move |conn| get(conn, item)).unwrap().unwrap();
        assert_eq!(binding.uri, "file:///repo/README.md");
        assert_eq!(binding.sync_mode.as_deref(), Some("bidirectional"));
        assert_eq!(binding.serializer.as_deref(), Some("document"));
    }
}
