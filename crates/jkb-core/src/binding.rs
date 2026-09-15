//! Binding repository: where an item's bytes live and how they sync (design D3).
//!
//! Every item has at most one binding (the row's `item_id` is its rowid). The
//! default is `managed:`; a `file://` binding participates in sync.

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::{ItemId, SyncMode};

use crate::changelog::{Entity, Op};
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
    // The binding this replaces, read before the upsert. Sync's re-attach path calls this for
    // items that were already bound, and logging that as an insert had `jkb undo` delete the
    // binding row — leaving a file-backed item with no uri at all.
    let before = get(conn, item)?;
    conn.prepare_cached(
        "INSERT INTO bindings (item_id, uri, sync_mode, serializer) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(item_id) DO UPDATE SET
             uri = excluded.uri, sync_mode = excluded.sync_mode, serializer = excluded.serializer",
    )?
    .execute(params![item.get(), uri, mode, serializer])?;
    let after =
        json!({ "item_id": item.get(), "uri": uri, "sync_mode": mode, "serializer": serializer });
    changelog::upsert(
        conn,
        meta,
        Entity::Bindings,
        &item.get().to_string(),
        before
            .map(|b| {
                json!({
                    "item_id": item.get(),
                    "uri": b.uri,
                    "sync_mode": b.sync_mode,
                    "serializer": b.serializer,
                })
            })
            .as_ref(),
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
    let mut stmt = conn.prepare_cached(
        "SELECT uri, item_id FROM bindings WHERE uri IN (SELECT value FROM json_each(?1))",
    )?;
    let rows = stmt.query_map([crate::sql::json_strings(uris)], |row| {
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
    let file = bare_uri.strip_prefix("file://");
    let mut out = Vec::new();
    for row in rows {
        let uri = row?;
        // `LIKE '<uri>#%'` also matches a document whose own name continues past a `#` (`C#.md` for
        // the file `C`); only a uri whose fragment [`file_path`] takes off belongs to this file.
        if uri == bare_uri || file_path(&uri) == file {
            out.push(uri);
        }
    }
    Ok(out)
}

/// Record a successful sync: stamp `last_synced_hash`/`last_synced_at` for `item`.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn mark_synced(conn: &Connection, meta: &WriteMeta, item: ItemId, hash: &str) -> Result<()> {
    // Read the stamp this replaces: an update to `bindings` is undone by writing its
    // before-state back column by column, so logging none would have `jkb undo` of a sync leave
    // the binding claiming it had settled on bytes the undo has just thrown away.
    let before: Option<(Option<String>, Option<String>)> = conn
        .prepare_cached("SELECT last_synced_hash, last_synced_at FROM bindings WHERE item_id = ?1")?
        .query_row([item.get()], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let updated = conn
        .prepare_cached(
            "UPDATE bindings SET last_synced_hash = ?2,
                 last_synced_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE item_id = ?1",
        )?
        .execute(params![item.get(), hash])?;
    // No binding to stamp: there is nothing to record and nothing to undo.
    let (Some((before_hash, before_at)), 1..) = (before, updated) else {
        return Ok(());
    };
    changelog::append(
        conn,
        meta,
        Op::Update,
        Entity::Bindings,
        &item.get().to_string(),
        Some(&json!({ "last_synced_hash": before_hash, "last_synced_at": before_at })),
        Some(&json!({ "last_synced_hash": hash })),
    )?;
    Ok(())
}

/// Fetch an item's binding, if one is set.
///
/// The file a `file://` binding uri names: the path, less a trailing `#<local id>` fragment. `None` for
/// any other uri.
///
/// The one parse of a binding's fragment — the sync engine gathers a file's bindings by it
/// ([`synced_uris_for_file`], and the bound files it walks), `jkb_api::tasks::FileRoots` judges the path
/// it returns, and [`serializer_for`] finds the file's journal row and mount by it. A `#` in a filename
/// is indistinguishable from a fragment by spelling alone, so a fragment is only text a local id can
/// be: non-empty, with no `/` (a `#` in a directory name is path) and no `.` (a minted id slugs a `.`
/// to `-`, and a `^id` is letters, digits and dashes — while a document's filename after a `#` almost
/// always carries its extension). Splitting `C#.md` at the `#` had the second sync of a note of that
/// name export it to a new file `C`. What stays ambiguous is a dotless name such as `Makefile#x`.
#[must_use]
pub fn file_path(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("file://")?;
    Some(match rest.rsplit_once('#') {
        Some((path, fragment)) if !fragment.is_empty() && !fragment.contains(['/', '.']) => path,
        _ => rest,
    })
}

/// The serializer that owns an item's file binding: the binding's own override, else the serializer
/// the sync journal says last produced the file, else — for a file not synced yet — the serializer of
/// the mount whose directory covers it most closely. `None` for an item bound to no file, or to a file
/// no mount covers.
///
/// The journal comes before the mounts because it records what the engine actually used, and the
/// engine syncs a file with the serializer of the mount *doing the sync*: under a `document` mount
/// over a directory with a nested `tasks` mount, `jkb sync` of the outer one imports the nested
/// `tasks.md` as one whole-file document, which the closest mount would have misjudged as a task. The
/// journal is keyed by the bare file uri, so a document's own uri is tried as it is before the
/// fragment is taken off it.
///
/// # Errors
/// Returns an error if a read fails.
pub fn serializer_for(conn: &Connection, item: ItemId) -> Result<Option<String>> {
    let Some(bound) = get(conn, item)? else {
        return Ok(None);
    };
    let Some(path) = file_path(&bound.uri) else {
        return Ok(None);
    };
    if let Some(own) = bound.serializer {
        return Ok(Some(own));
    }
    for uri in [bound.uri.clone(), format!("file://{path}")] {
        if let Some(journal) = crate::sync_state::get(conn, &uri)? {
            return Ok(Some(journal.serializer));
        }
    }
    let mut stmt = conn.prepare_cached(
        "SELECT backing_uri, serializer FROM mounts WHERE backing_uri LIKE 'file://%'",
    )?;
    let mounts = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let path = std::path::Path::new(path);
    Ok(mounts
        .into_iter()
        .filter_map(|(uri, serializer)| {
            let dir = uri
                .strip_prefix("file://")?
                .trim_end_matches('/')
                .to_owned();
            path.starts_with(&dir).then_some((dir.len(), serializer))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, serializer)| serializer))
}

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
    /// The serializer owning a binding decides whether it is a tasks file, not a `#` in its uri: a
    /// document named `C#.md` is a whole-file note.
    #[test]
    fn a_binding_s_serializer_is_its_override_else_its_journal_s_else_its_closest_mount_s() {
        use crate::{mount, ns};
        use jkb_types::{ConflictPolicy, SyncMode};
        // A fragment is only text a local id can be, so a document's own `#` stays in its path, while
        // a minted id keeps its (lowercased, possibly non-ASCII) letters. Whether an item is in a
        // tasks file is still asked of its serializer, below, never of the spelling.
        assert_eq!(super::file_path("file:///n/C#.md"), Some("/n/C#.md"));
        assert_eq!(super::file_path("file:///n/C#"), Some("/n/C#"));
        assert_eq!(
            super::file_path("file:///n/tasks.md#fix-é-0a1b2c"),
            Some("/n/tasks.md")
        );
        assert_eq!(
            super::file_path("file:///n/tasks.md#t1"),
            Some("/n/tasks.md")
        );
        assert_eq!(
            super::file_path("file:///n/a#b/tasks.md#t1"),
            Some("/n/a#b/tasks.md")
        );
        assert_eq!(super::file_path("managed:"), None);
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |c, m| {
            for (path, dir, serializer) in [
                ("notes", "file:///n", "document"),
                ("notes/plan", "file:///n/plan", "tasks"),
            ] {
                let id = ns::ensure(c, path)?;
                mount::create(
                    c,
                    m,
                    id,
                    dir,
                    SyncMode::Bidirectional,
                    serializer,
                    None,
                    None,
                    ConflictPolicy::Manual,
                )?;
            }
            let bound = |uid: &str,
                         uri: &str,
                         serializer: Option<&str>|
             -> crate::Result<jkb_types::ItemId> {
                let item = upsert(
                    c,
                    m,
                    &NewItem {
                        uid: uid.into(),
                        kind: "document".into(),
                        content: None,
                        content_hash: None,
                        mime: None,
                    },
                )?;
                set(c, m, item, uri, None, serializer)?;
                Ok(item)
            };
            let note = bound("n1", "file:///n/C#.md", None)?;
            let task = bound("t1", "file:///n/plan/tasks.md#t1", None)?;
            let forced = bound("f1", "file:///n/plan/notes.md", Some("document"))?;
            assert_eq!(super::serializer_for(c, note)?.as_deref(), Some("document"));
            assert_eq!(super::serializer_for(c, task)?.as_deref(), Some("tasks"));
            assert_eq!(
                super::serializer_for(c, forced)?.as_deref(),
                Some("document"),
                "the override wins"
            );
            assert!(!crate::item::in_tasks_file(c, note)?);
            // The outer `document` mount synced the nested tasks.md as one whole-file note: the journal
            // says so, and it is not judged a task because a `tasks` mount covers it more closely.
            let whole = bound("w1", "file:///n/plan/tasks.md", None)?;
            assert_eq!(super::serializer_for(c, whole)?.as_deref(), Some("tasks"));
            let journal = |uri, serializer| crate::sync_state::SyncStateWrite {
                uri,
                serializer,
                status: "ok",
                last_synced_hash: None,
                base_blob_hash: None,
                parse_error: None,
                quarantine_blob_hash: None,
                document: None,
            };
            crate::sync_state::upsert(c, m, &journal("file:///n/plan/tasks.md", "document"))?;
            assert_eq!(
                super::serializer_for(c, whole)?.as_deref(),
                Some("document")
            );
            assert_eq!(
                super::serializer_for(c, task)?.as_deref(),
                Some("document"),
                "a fragment is taken off to find the file's journal row"
            );
            crate::sync_state::upsert(c, m, &journal("file:///n/C#.md", "tasks"))?;
            assert_eq!(
                super::serializer_for(c, note)?.as_deref(),
                Some("tasks"),
                "a document's own uri is looked up as it is"
            );
            Ok(())
        })
        .unwrap();
    }

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
                ("d", "file:///repo/a_b.md#.bak"),  // a document named `a_b.md#.bak`, no fragment
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
