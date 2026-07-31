//! Undo: revert a transaction by inverting its changelog entries.
//!
//! Two inversions are implemented. **Inserts** are reversed by deleting the affected row by
//! `rowid` (the common "oops, undo that" case for creates). An **item delete** is reversed by
//! restoring it from the complete snapshot `item::remove` recorded in `before` — the item row
//! plus the placements, tag applications, edges, and binding that `ON DELETE CASCADE` took
//! with it. That pairing is what lets `jkb item rm` exist at all: a delete nothing can undo
//! would break the promise that every mutation is reversible.
//!
//! An **edge weight update** is reversed by restoring the previous weight from `before`.
//! Inverting the remaining column updates (item status, priority, …) is still future work.
//! The row's `rowid` is stored as the changelog `entity_id` and `entity_type` is the table name.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

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
        "SELECT op, entity_type, entity_id, before FROM changelog
         WHERE txn_id = ?1 ORDER BY id DESC",
    )?;
    let entries = stmt
        .query_map([txn_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut reverted = 0;
    for (op, table, entity_id, before) in entries {
        // An item delete is inverted by putting the snapshot back (see `restore_item`).
        if op == "delete" && table == "items" {
            if let Some(before) = before {
                let snapshot: Value = serde_json::from_str(&before).map_err(|e| {
                    TypeError::Validation(format!("unreadable item snapshot in changelog: {e}"))
                })?;
                reverted += restore_item(conn, &snapshot)?;
            }
            continue;
        }
        // An edge weight update is inverted by putting the previous weight back. Without
        // this the edge would keep whatever weight the undone transaction gave it.
        if op == "update" && table == "edges" {
            if let Some(before) = before {
                let snapshot: Value = serde_json::from_str(&before).map_err(|e| {
                    TypeError::Validation(format!("unreadable edge before-state: {e}"))
                })?;
                let rowid: i64 = entity_id
                    .parse()
                    .map_err(|_| TypeError::Validation(format!("bad entity id '{entity_id}'")))?;
                reverted += conn
                    .prepare_cached("UPDATE edges SET weight = ?2 WHERE rowid = ?1")?
                    .execute(params![
                        rowid,
                        snapshot.get("weight").and_then(Value::as_f64)
                    ])?;
            }
            continue;
        }
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

/// Restore an item deleted by `item::remove` from its changelog snapshot: the item row
/// (with its original `id`, so every reference to it still resolves), then the placements,
/// tag applications, edges, and binding that cascaded away with it. Returns the number of
/// rows restored.
///
/// The item is inserted **first** so the children's foreign keys resolve. Edges are inserted
/// with `OR IGNORE`: if the item at the other end was itself deleted and not restored, that
/// edge simply cannot come back, and skipping it is better than failing the whole undo.
fn restore_item(conn: &Connection, snapshot: &Value) -> Result<usize> {
    let item = snapshot
        .get("item")
        .ok_or_else(|| TypeError::Validation("item snapshot has no `item`".to_owned()))?;
    let id = item
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| TypeError::Validation("item snapshot has no `id`".to_owned()))?;

    let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned);
    let mut restored = conn
        .prepare_cached(
            "INSERT OR IGNORE INTO items
                 (id, uid, kind, content, content_hash, mime, status, resolution, priority, due,
                  metadata, created_at, updated_at, claimant_id, claimed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?
        .execute(params![
            id,
            text("uid"),
            text("kind"),
            text("content"),
            text("content_hash"),
            text("mime"),
            text("status"),
            text("resolution"),
            item.get("priority").and_then(Value::as_i64),
            text("due"),
            text("metadata").unwrap_or_else(|| "{}".to_owned()),
            text("created_at"),
            text("updated_at"),
            text("claimant_id"),
            text("claimed_at"),
        ])?;

    restored += restore_children(conn, snapshot, id)?;
    Ok(restored)
}

/// Restore the rows that `ON DELETE CASCADE` took with an item: its placements, tag
/// applications, edges, and binding. Split out of `restore_item` so each table's column list
/// stays readable. All inserts are `OR IGNORE` — re-running an undo must not fail on rows a
/// previous attempt already put back.
fn restore_children(conn: &Connection, snapshot: &Value, id: i64) -> Result<usize> {
    let rows = |key: &str| -> Vec<&Value> {
        snapshot
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    };
    let mut restored = 0;

    for placement in rows("placements") {
        restored += conn
            .prepare_cached(
                "INSERT OR IGNORE INTO placements (item_id, namespace_id, role, position, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                id,
                placement.get("namespace_id").and_then(Value::as_i64),
                placement.get("role").and_then(Value::as_str),
                placement
                    .get("position")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                placement
                    .get("metadata")
                    .and_then(Value::as_str)
                    .unwrap_or("{}"),
            ])?;
    }
    for tag in rows("tags") {
        restored += conn
            .prepare_cached(
                "INSERT OR IGNORE INTO tag_applications (item_id, facet, value, props)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                id,
                tag.get("facet").and_then(Value::as_str),
                tag.get("value").and_then(Value::as_str).unwrap_or(""),
                tag.get("props").and_then(Value::as_str).unwrap_or("{}"),
            ])?;
    }
    for edge in rows("edges") {
        // The `EXISTS` guards skip an edge whose other endpoint is gone — better than
        // failing the whole undo on a foreign key.
        restored += conn
            .prepare_cached(
                "INSERT OR IGNORE INTO edges
                     (src_item_id, dst_item_id, type, props, weight, created_at)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6
                 WHERE EXISTS (SELECT 1 FROM items WHERE id = ?1)
                   AND EXISTS (SELECT 1 FROM items WHERE id = ?2)",
            )?
            .execute(params![
                edge.get("src").and_then(Value::as_i64),
                edge.get("dst").and_then(Value::as_i64),
                edge.get("type").and_then(Value::as_str),
                edge.get("props").and_then(Value::as_str).unwrap_or("{}"),
                edge.get("weight").and_then(Value::as_f64),
                edge.get("created_at").and_then(Value::as_str),
            ])?;
    }
    if let Some(binding) = snapshot.get("binding").filter(|b| !b.is_null()) {
        restored += conn
            .prepare_cached(
                "INSERT OR IGNORE INTO bindings
                     (item_id, uri, sync_mode, serializer, last_synced_hash, last_synced_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                id,
                binding.get("uri").and_then(Value::as_str),
                binding.get("sync_mode").and_then(Value::as_str),
                binding.get("serializer").and_then(Value::as_str),
                binding.get("last_synced_hash").and_then(Value::as_str),
                binding.get("last_synced_at").and_then(Value::as_str),
            ])?;
    }
    Ok(restored)
}

/// Undo the most recent **invertible** transaction that has not already been undone, and
/// return the number of rows it changed (0 if there is nothing to undo).
///
/// Invertible means the transaction contains an `insert`, an item `delete` (which carries a
/// restorable snapshot), or an edge weight `update`. A transaction with none of those — the
/// delete-only one `jkb item rm` produces, say — must still count: otherwise `jkb undo` would
/// skip straight past it and revert somebody's unrelated earlier work while the deleted item
/// stayed gone.
///
/// # Errors
/// Propagates any error from [`undo`].
pub fn undo_last(conn: &Connection, meta: &WriteMeta) -> Result<usize> {
    let target: Option<i64> = conn
        .prepare_cached(
            "SELECT MAX(txn_id) FROM changelog c
             WHERE (c.op = 'insert'
                    OR (c.op = 'delete' AND c.entity_type = 'items')
                    OR (c.op = 'update' AND c.entity_type = 'edges'))
               AND c.txn_id < ?1
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

    /// Undoing an `item::remove` must put back everything the cascade took — above all the
    /// **edges**, which carry the knowledge (what refuted what, what a unit depends on) and
    /// which no changelog entry of their own records, because a cascade bypasses the repos.
    #[test]
    fn undoing_an_item_delete_restores_its_placements_tags_and_edges() {
        use crate::{binding, edge, item, ns, placement, tag};
        use jkb_types::{EdgeType, PlacementRole};

        let db = Db::open_in_memory().unwrap();
        let (victim, neighbour) = db
            .write_txn("t", |c, m| {
                let victim = upsert(c, m, &note("doomed"))?;
                let neighbour = upsert(c, m, &note("neighbour"))?;
                let home = ns::ensure(c, "notes/home")?;
                let mirror = ns::ensure(c, "notes/mirror")?;
                placement::place(c, m, victim, home, PlacementRole::Primary, 3)?;
                placement::place(c, m, victim, mirror, PlacementRole::Reference, 0)?;
                tag::apply(c, m, victim, "size", "small")?;
                binding::set(c, m, victim, "managed:", None, None)?;
                // One edge in each direction, one of them weighted.
                edge::link_weighted(c, m, victim, neighbour, EdgeType::Supports, Some(2.5), None)?;
                edge::link(c, m, neighbour, victim, EdgeType::References, None)?;
                Ok((victim, neighbour))
            })
            .unwrap();

        let removed = db
            .write_txn("t", move |c, m| item::remove(c, m, victim, false))
            .unwrap();
        assert_eq!(removed.uid, "doomed");
        assert_eq!(removed.placements, 2);
        assert_eq!(removed.edges, 2);
        assert_eq!(removed.tags, 1);

        // Everything really is gone (the cascade fired).
        let gone = db
            .read(|c| {
                Ok((
                    item::id_for_uid(c, "doomed")?,
                    c.query_row("SELECT count(*) FROM placements", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                    c.query_row("SELECT count(*) FROM edges", [], |r| r.get::<_, i64>(0))?,
                    c.query_row("SELECT count(*) FROM tag_applications", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                ))
            })
            .unwrap();
        assert_eq!(gone, (None, 0, 0, 0));

        // Undo puts the item back with its ORIGINAL id, so every reference still resolves.
        db.write_txn("t", undo_last).unwrap();
        let restored = db
            .read(|c| item::id_for_uid(c, "doomed"))
            .unwrap()
            .expect("the item is back");
        assert_eq!(restored, victim, "the id is preserved, not reassigned");

        // …and with its placements, tag, binding, and both edges — weight included.
        let meta = db.read(move |c| item::get(c, restored)).unwrap().unwrap();
        assert_eq!(meta.kind, "note");
        assert_eq!(meta.content.as_deref(), Some("body"));
        let places = db
            .read(move |c| {
                let mut stmt = c.prepare(
                    "SELECT role, position FROM placements WHERE item_id = ?1 ORDER BY role",
                )?;
                let rows = stmt.query_map([restored.get()], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .unwrap();
        assert_eq!(
            places,
            vec![("primary".to_owned(), 3), ("reference".to_owned(), 0)],
            "both placements, with their roles and positions"
        );
        assert_eq!(
            db.read(move |c| tag::applications(c, restored)).unwrap(),
            vec![("size".to_owned(), "small".to_owned())]
        );
        assert!(db
            .read(move |c| binding::get(c, restored))
            .unwrap()
            .is_some());
        assert_eq!(
            db.read(move |c| edge::edges_from(c, restored, EdgeType::Supports))
                .unwrap(),
            vec![neighbour],
            "the outgoing edge is back"
        );
        assert_eq!(
            db.read(move |c| edge::edges_from(c, neighbour, EdgeType::References))
                .unwrap(),
            vec![restored],
            "the incoming edge is back too"
        );
        let weight = db.read(move |c| edge::evidence_for(c, neighbour)).unwrap();
        assert!(
            (weight - 2.5).abs() < 1e-9,
            "the edge weight survived the round trip, got {weight}"
        );
    }

    /// The guards: investigation memory and synced-file items are not deleted by accident.
    #[test]
    fn remove_refuses_investigation_memory_and_file_backed_items_without_force() {
        use crate::{binding, edge, item};
        use jkb_types::{EdgeType, Resolution};

        let db = Db::open_in_memory().unwrap();
        let (tombstone, killed, synced) = db
            .write_txn("t", |c, m| {
                let tombstone = upsert(c, m, &note("dead-end"))?;
                item::set_resolution(c, m, tombstone, Resolution::DeadEnd)?;

                // No resolution set, but an edge records that it was killed.
                let killed = upsert(c, m, &note("refuted"))?;
                let obstruction = upsert(c, m, &note("obstruction"))?;
                edge::link(c, m, obstruction, killed, EdgeType::Refutes, None)?;

                let synced = upsert(c, m, &note("from-a-file"))?;
                binding::set(c, m, synced, "file:///tmp/notes.md#abc", None, None)?;
                Ok((tombstone, killed, synced))
            })
            .unwrap();

        // A tombstone is the anti-retread record — refused, and the message says why.
        let err = db
            .write_txn("t", move |c, m| item::remove(c, m, tombstone, false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("dead_end` tombstone"), "{err}");
        assert!(err.contains("--force"), "{err}");

        // So is a unit an edge records as killed, even with no resolution of its own.
        let err = db
            .write_txn("t", move |c, m| item::remove(c, m, killed, false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("refutes` edge"), "{err}");

        // A synced-file item would just come back on the next sync, so deleting it is a lie.
        let err = db
            .write_txn("t", move |c, m| item::remove(c, m, synced, false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("bound to the synced file"), "{err}");
        assert!(err.contains("source file"), "{err}");

        // Nothing was deleted by the refusals.
        for uid in ["dead-end", "refuted", "from-a-file"] {
            assert!(db
                .read(move |c| item::id_for_uid(c, uid))
                .unwrap()
                .is_some());
        }

        // `--force` gets through, and the delete is still undoable.
        db.write_txn("t", move |c, m| item::remove(c, m, tombstone, true))
            .unwrap();
        assert!(db
            .read(|c| item::id_for_uid(c, "dead-end"))
            .unwrap()
            .is_none());
        db.write_txn("t", undo_last).unwrap();
        assert!(
            db.read(|c| item::id_for_uid(c, "dead-end"))
                .unwrap()
                .is_some(),
            "even a forced delete of a tombstone is recoverable"
        );
    }

    /// Re-linking an existing edge to change its weight is an UPDATE, not an insert. Undoing
    /// it must restore the old weight — not delete an edge that existed beforehand, taking
    /// the knowledge it carried with it.
    #[test]
    fn undoing_a_weight_change_restores_it_instead_of_deleting_the_edge() {
        use crate::edge;
        use jkb_types::EdgeType;

        let db = Db::open_in_memory().unwrap();
        let (obs, hyp) = db
            .write_txn("t", |c, m| {
                let obs = upsert(c, m, &note("observation"))?;
                let hyp = upsert(c, m, &note("hypothesis"))?;
                edge::link_weighted(c, m, obs, hyp, EdgeType::Supports, Some(1.0), None)?;
                Ok((obs, hyp))
            })
            .unwrap();

        // A separate transaction that only strengthens the existing edge.
        db.write_txn("t", move |c, m| {
            edge::link_weighted(c, m, obs, hyp, EdgeType::Supports, Some(5.0), None)
        })
        .unwrap();
        assert!(
            (db.read(move |c| edge::evidence_for(c, hyp)).unwrap() - 5.0).abs() < 1e-9,
            "the weight was raised"
        );

        // Undo restores the previous weight, and the edge survives.
        db.write_txn("t", undo_last).unwrap();
        assert_eq!(
            db.read(move |c| edge::edges_from(c, obs, EdgeType::Supports))
                .unwrap(),
            vec![hyp],
            "the pre-existing edge must NOT be deleted by undoing a weight change"
        );
        let restored = db.read(move |c| edge::evidence_for(c, hyp)).unwrap();
        assert!(
            (restored - 1.0).abs() < 1e-9,
            "the previous weight is restored, got {restored}"
        );
    }

    /// A delete-only transaction must be what `undo` picks: otherwise `jkb item rm` followed
    /// by `jkb undo` would silently revert somebody's earlier, unrelated work instead.
    #[test]
    fn undo_last_targets_a_delete_only_transaction() {
        use crate::item;

        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |c, m| upsert(c, m, &note("keep-me")))
            .unwrap();
        let doomed = db
            .write_txn("t", |c, m| upsert(c, m, &note("doomed")))
            .unwrap();
        // A separate transaction that only deletes.
        db.write_txn("t", move |c, m| item::remove(c, m, doomed, false))
            .unwrap();

        db.write_txn("t", undo_last).unwrap();
        assert!(
            db.read(|c| item::id_for_uid(c, "doomed"))
                .unwrap()
                .is_some(),
            "undo must reverse the delete, not skip past it"
        );
        assert!(
            db.read(|c| item::id_for_uid(c, "keep-me"))
                .unwrap()
                .is_some(),
            "and must not have reverted the unrelated earlier insert"
        );
    }
}
