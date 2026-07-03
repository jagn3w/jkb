//! Undo: revert a transaction by inverting its changelog entries.
//!
//! v1 inverts inserts (delete the affected row by `rowid`), which covers the
//! common "oops, undo that" case for creates. Inverting updates/deletes (restoring
//! the recorded `before`) is future work. The row's `rowid` is stored as the
//! changelog `entity_id` and `entity_type` is the table name.

use rusqlite::{Connection, OptionalExtension};

use jkb_types::Error as TypeError;

use crate::store::WriteMeta;
use crate::{changelog, Result};

/// Tables whose inserts `undo` may reverse. This allowlist guards the table-name
/// interpolation below (the name comes from our own code, never user input, but
/// we validate anyway).
const UNDOABLE_TABLES: &[&str] = &[
    "items",
    "namespaces",
    "placements",
    "edges",
    "tag_defs",
    "tag_applications",
    "bindings",
    "mounts",
    "blobs",
    "ingestions",
];

/// Revert transaction `txn_id` by deleting the rows it inserted (most recent
/// first). Returns the number of rows removed and records an `undo` marker.
///
/// # Errors
/// Returns a validation error if an entry names a table not on the allowlist, or
/// a database error if a statement fails.
pub fn undo(conn: &Connection, meta: &WriteMeta, txn_id: i64) -> Result<usize> {
    let mut stmt = conn.prepare_cached(
        "SELECT op, entity_type, entity_id FROM changelog WHERE txn_id = ?1 ORDER BY id DESC",
    )?;
    let entries = stmt
        .query_map([txn_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut reverted = 0;
    for (op, table, entity_id) in entries {
        if op != "insert" {
            continue;
        }
        if !UNDOABLE_TABLES.contains(&table.as_str()) {
            return Err(
                TypeError::Validation(format!("cannot undo unknown table '{table}'")).into(),
            );
        }
        let rowid: i64 = entity_id
            .parse()
            .map_err(|_| TypeError::Validation(format!("bad entity id '{entity_id}'")))?;
        reverted += conn
            .prepare_cached(&format!("DELETE FROM {table} WHERE rowid = ?1"))?
            .execute([rowid])?;
    }

    changelog::append(
        conn,
        meta,
        "undo",
        "changelog",
        &txn_id.to_string(),
        None,
        None,
    )?;
    Ok(reverted)
}

/// Undo the most recent insert-bearing transaction that has not already been
/// undone. Returns the number of rows removed (0 if there is nothing to undo).
///
/// # Errors
/// Propagates any error from [`undo`].
pub fn undo_last(conn: &Connection, meta: &WriteMeta) -> Result<usize> {
    let target: Option<i64> = conn
        .prepare_cached(
            "SELECT MAX(txn_id) FROM changelog c
             WHERE c.op = 'insert' AND c.txn_id < ?1
               AND NOT EXISTS (
                   SELECT 1 FROM changelog u
                   WHERE u.op = 'undo' AND u.entity_id = CAST(c.txn_id AS TEXT)
               )",
        )?
        .query_row([meta.txn_id], |row| row.get(0))
        .optional()?
        .flatten();

    match target {
        Some(txn_id) => undo(conn, meta, txn_id),
        None => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::undo_last;
    use crate::item::{upsert, NewItem};
    use crate::Db;

    fn note(uid: &str) -> NewItem {
        NewItem {
            uid: uid.to_owned(),
            kind: "note".to_owned(),
            content: Some("body".to_owned()),
            content_hash: None,
            mime: None,
        }
    }

    #[test]
    fn undo_last_reverts_the_most_recent_transaction() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |c, m| upsert(c, m, &note("a"))).unwrap();
        db.write_txn("t", |c, m| upsert(c, m, &note("b"))).unwrap();

        let reverted = db.write_txn("t", undo_last).unwrap();
        assert_eq!(reverted, 1);

        let remaining: Vec<String> = db
            .read(|c| {
                let mut stmt = c.prepare("SELECT uid FROM items ORDER BY uid")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .unwrap();
        assert_eq!(remaining, vec!["a".to_owned()]);

        // A second undo reverts the earlier transaction; a third finds nothing.
        assert_eq!(db.write_txn("t", undo_last).unwrap(), 1);
        assert_eq!(db.write_txn("t", undo_last).unwrap(), 0);
    }
}
