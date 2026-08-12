//! Undo: revert a transaction by inverting its changelog entries.
//!
//! The inversions are listed once, in [`INVERSES`]: `undo` dispatches on that list and
//! `undo_last` generates its eligibility predicate from it, so a new inverse cannot reach one
//! and not the other. **Inserts** are reversed by deleting the affected row by
//! `rowid` (the common "oops, undo that" case for creates). An **item delete** is reversed by
//! restoring it from the complete snapshot `item::remove` recorded in `before` — the item row
//! plus the placements, tag applications, edges, and binding that `ON DELETE CASCADE` took
//! with it. That pairing is what lets `jkb item rm` exist at all: a delete nothing can undo
//! would break the promise that every mutation is reversible.
//!
//! An **edge weight update** is reversed by restoring the previous weight from `before`.
//! Inverting the remaining column updates (item status, priority, …) is still future work.
//! The row's `rowid` is stored as the changelog `entity_id` and `entity_type` is the table name.

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::Value;

use jkb_types::Error as TypeError;

use crate::store::WriteMeta;
use crate::{changelog, Error, Result};

/// One inversion [`undo`] knows how to perform.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Inverse {
    /// Put an item back from the snapshot `item::remove` recorded.
    ItemSnapshot,
    /// Restore an edge's previous weight.
    EdgeWeight,
    /// Restore a mount's previous configuration.
    MountConfig,
    /// Restore a containment row's previous container and position.
    ContainmentRow,
    /// Restore a sync journal row's previous state, or neutralize one whose transaction is being undone.
    SyncStateRow,
    /// Delete the inserted row by `rowid`.
    DeleteRow,
}

/// **The** list of inversions this module implements, as data: `(op, entity_type, inverse)`,
/// where a `None` entity type matches any table.
///
/// [`undo`] dispatches on it and [`undo_last`] builds its eligibility predicate from it, so
/// the two cannot disagree about what is invertible. They did once: the `update`+`mounts`
/// inverse was added to `undo` alone, and a bare `jkb undo` — which is the only form any
/// surface offers, since nothing prints txn ids — walked straight past a mount edit and
/// reverted an older transaction, deleting the mount it had been asked to restore.
const INVERSES: &[(&str, Option<&str>, Inverse)] = &[
    ("delete", Some("items"), Inverse::ItemSnapshot),
    ("update", Some("edges"), Inverse::EdgeWeight),
    ("update", Some("mounts"), Inverse::MountConfig),
    ("update", Some("containment"), Inverse::ContainmentRow),
    // A sync transaction deletes items AND advances the file's journal. With no inverse here
    // the journal survived the undo, so `last_synced_hash` still described bytes whose items
    // were gone — and the next reconcile saw "KB changed, disk did not" and exported an
    // item-less render over the file, silently emptying it. `undo_last` could select such a
    // transaction (it inserts items), so this was reachable by a bare `jkb undo` with the
    // watcher running.
    // NOT selectable — see `SELECTS_A_TRANSACTION`. This inverse exists to accompany a sync
    // transaction that also touched items; on its own the journal is bookkeeping, not work.
    ("update", Some("sync_state"), Inverse::SyncStateRow),
    // Any table on `UNDOABLE_TABLES`; an insert into anything else is an error, not a skip.
    // The wildcard entry must stay LAST: `inverse_for` is first-match-wins, so a
    // table-specific `insert` inverse placed after it would never be reached.
    ("insert", None, Inverse::DeleteRow),
];

/// Whether an entry of this kind may make its transaction the one a bare `jkb undo` picks.
///
/// Being invertible and being *the user's last change* are different questions, and conflating
/// them is a bug: several sync transactions write nothing but the journal — a refusal flagging
/// `needs_attention`, a re-settle, a legacy row being populated — and once `sync_state` joined
/// the inverse list those became "the last invertible transaction". A user running `jkb undo`
/// to take back a `task add` would silently rewind a journal flag instead, reporting "reverted
/// 1 change(s)", leaving the task untouched and the refused file invisible to `jkb doctor`.
///
/// Keyed on the same tuples as `INVERSES` so the two cannot drift into disagreeing about a table
/// neither list mentions.
fn selects_a_transaction(op: &str, table: Option<&str>) -> bool {
    !matches!((op, table), ("update", Some("sync_state")))
}

/// The inversion for a changelog entry, or `None` if this module cannot reverse it.
fn inverse_for(op: &str, table: &str) -> Option<Inverse> {
    INVERSES
        .iter()
        .find(|(o, t, _)| *o == op && t.is_none_or(|t| t == table))
        .map(|(_, _, inv)| *inv)
}

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
    // Every `jkb ingest` writes one per chunk and every `task add --under` writes one, so
    // omitting it made a bare `jkb undo` die on the most common transaction shape there is —
    // and, because a failed undo records no `undo` marker, re-select the same transaction
    // forever. `child_item_id` is an `INTEGER PRIMARY KEY`, hence the rowid, so the generic
    // delete-by-rowid inverse addresses the right row.
    "containment",
];

/// Invert one changelog entry, returning how many rows it changed.
///
/// `Ok(0)` covers the deliberate skips: an op this module has no inverse for (a claim, a
/// status update) is passed over rather than failing the undo.
///
/// # Errors
/// Returns a validation error for an insert into a table outside [`UNDOABLE_TABLES`] — a
/// half-undone transaction is worse than none — or for an unreadable snapshot.
fn invert_entry(
    conn: &Connection,
    op: &str,
    table: &str,
    entity_id: &str,
    before: Option<&str>,
) -> Result<usize> {
    let mut rows = 0;
    // Dispatch through `INVERSES` — the same list `undo_last` selects transactions with.
    let Some(inverse) = inverse_for(op, table) else {
        return Ok(0);
    };
    // A `match`, not an if-chain with a fall-through: the fall-through was
    // `DELETE FROM <table>`, so a future `Inverse` variant added to the enum and the table
    // but not to the dispatch compiled clean and deleted the row it was meant to restore
    // — the same shape as the mount bug above. Adding a variant now stops this compiling.
    let rowid = || -> Result<i64> {
        entity_id
            .parse::<i64>()
            .map_err(|_| TypeError::Validation(format!("bad entity id '{entity_id}'")).into())
    };
    let snapshot = |what: &str| -> Result<Option<Value>> {
        before
            .map(|b| {
                serde_json::from_str(b).map_err(|e| {
                    Error::from(TypeError::Validation(format!(
                        "unreadable {what} in changelog: {e}"
                    )))
                })
            })
            .transpose()
    };
    match inverse {
        // An item delete is inverted by putting the snapshot back (see `restore_item`).
        Inverse::ItemSnapshot => {
            if let Some(snap) = snapshot("item snapshot")? {
                rows += restore_item(conn, &snap)?;
            }
        }
        // An edge weight update is inverted by putting the previous weight back. Without
        // this the edge would keep whatever weight the undone transaction gave it.
        Inverse::EdgeWeight => {
            if let Some(snap) = snapshot("edge before-state")? {
                rows += conn
                    .prepare_cached("UPDATE edges SET weight = ?2 WHERE rowid = ?1")?
                    .execute(params![
                        rowid()?,
                        snap.get("weight").and_then(Value::as_f64)
                    ])?;
            }
        }
        // A mount edit is inverted by putting the previous configuration back. `jkb mount
        // create` doubles as the update command, so without this the generic insert
        // inverse would `DELETE FROM mounts` and destroy a mount that existed before the
        // transaction, leaving its `file://` bindings with nothing to sync them.
        Inverse::SyncStateRow => {
            rows += revert_sync_state(conn, entity_id, snapshot("sync journal before-state")?)?;
        }
        Inverse::MountConfig => {
            if let Some(snap) = snapshot("mount before-state")? {
                let field = |k: &str| snap.get(k).and_then(Value::as_str).map(str::to_owned);
                rows += conn
                    .prepare_cached(
                        "UPDATE mounts
                            SET backing_uri = ?2, sync_mode = ?3, serializer = ?4,
                                include_glob = ?5, exclude_glob = ?6, conflict_policy = ?7
                          WHERE namespace_id = ?1",
                    )?
                    .execute(params![
                        rowid()?,
                        field("backing_uri"),
                        field("sync_mode"),
                        field("serializer"),
                        field("include_glob"),
                        field("exclude_glob"),
                        field("conflict_policy"),
                    ])?;
            }
        }
        // Re-parenting is an update, not an insert (`containment::contain` upserts on the
        // child), so the generic inverse would delete a row that existed beforehand and
        // un-parent the item instead of putting its previous container back.
        Inverse::ContainmentRow => {
            if let Some(snap) = snapshot("containment before-state")? {
                rows += conn
                    .prepare_cached(
                        "UPDATE containment SET parent_item_id = ?2, position = ?3
                          WHERE child_item_id = ?1",
                    )?
                    .execute(params![
                        rowid()?,
                        snap.get("parent_item_id").and_then(Value::as_i64),
                        snap.get("position").and_then(Value::as_i64).unwrap_or(0),
                    ])?;
            }
        }
        Inverse::DeleteRow => {
            if !UNDOABLE_TABLES.contains(&table) {
                return Err(
                    TypeError::Validation(format!("cannot undo unknown table '{table}'")).into(),
                );
            }
            rows += conn
                .prepare_cached(&format!("DELETE FROM {table} WHERE rowid = ?1"))?
                .execute([rowid()?])?;
        }
    }
    Ok(rows)
}

/// Put one sync journal row back the way it was, or remove one the transaction created.
///
/// `entity_id` is the file **uri**, not a rowid: `sync_state` is keyed on `uri`, so the generic
/// delete-by-rowid inverse would address the wrong row entirely.
///
/// Restoring the hashes is the point. Without an inverse here, undoing a sync deleted the
/// items and left `last_synced_hash` describing bytes that no longer had any — after which the
/// next reconcile read "KB changed, disk did not" and exported an item-less render over the
/// file, silently emptying it.
fn revert_sync_state(conn: &Connection, entity_id: &str, before: Option<Value>) -> Result<usize> {
    let Some(snap) = before else {
        // NO BEFORE-STATE, AND IT IS AMBIGUOUS. `sync_state` writes are always logged with op
        // `update`, so an absent snapshot means either "this transaction created the row" or
        // "this entry was written before the snapshot existed" — and every entry any older
        // binary wrote is the second kind. Deleting on that reading destroys a journal row that
        // may long predate the transaction being undone, taking `base_blob_hash` and the
        // migrated `document` with it and making the file read as never synced.
        //
        // So clear the whole basis for the next direction decision — hash, base blob AND
        // document — rather than deleting the row.
        //
        // Clearing only `last_synced_hash` was tried and is worse than either: `base_blob_hash`
        // and `document` then still described the items this undo had just deleted, so the next
        // reconcile loaded that base, found the disk unchanged against it and the KB now empty,
        // and took the `(false, true)` EXPORT arm — writing an item-less render over the file and
        // stripping every task line from it. Undo is supposed to give work back, not delete more.
        //
        // With all three cleared, `load_base_doc` returns `None`, the base reads as empty, and
        // the disk side's items look like additions. An importing mount re-imports the file and
        // heals itself. An exporting one does **not** simply skip — that claim stood here and was
        // wrong: with no base, both sides read as changed, so the reconcile takes the three-way
        // arm rather than `export_or_skip`, and on an export-only mount that arm exports. What
        // stops it is `wholesale_loss` at the export seam, which refuses any render that
        // contributes no items to a file that declares some. Clearing these three fields is still
        // right — it is what lets an importing mount heal — but it is not by itself the guard.
        // The blob itself is never deleted (`jkb blob ls` still finds it), so this loses a
        // pointer, not content.
        return Ok(conn
            .prepare_cached(
                "UPDATE sync_state
                    SET last_synced_hash = NULL, base_blob_hash = NULL, document = NULL
                  WHERE uri = ?1",
            )?
            .execute(params![entity_id])?);
    };
    let field = |k: &str| snap.get(k).and_then(Value::as_str).map(str::to_owned);
    Ok(conn
        .prepare_cached(
            "UPDATE sync_state
                SET status = ?2, serializer = ?3, last_synced_hash = ?4, base_blob_hash = ?5,
                    document = ?6, parse_error = ?7, quarantine_blob_hash = ?8
              WHERE uri = ?1",
        )?
        .execute(params![
            entity_id,
            field("status"),
            field("serializer"),
            field("last_synced_hash"),
            field("base_blob_hash"),
            // Structure rewinds WITH the hashes. Since D45 the document lives on this row, so
            // restoring the hashes and leaving the structure forward would undo a sync into a KB
            // that disagrees with its own base — the state every export bug here grew out of.
            field("document"),
            // …and so does the explanation for a non-`ok` status, or undo restores
            // `needs_attention` with nothing saying why.
            field("parse_error"),
            field("quarantine_blob_hash"),
        ])?)
}

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
        reverted += invert_entry(conn, &op, &table, &entity_id, before.as_deref())?;
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
    // Containment, both directions. `child_item_id` is the PRIMARY KEY, so re-inserting the
    // row that named this item's container is one statement; the rows for the items it
    // contained are one each. `OR IGNORE` plus the `EXISTS` guard skips a row whose other
    // endpoint is gone, exactly as the edges above do.
    if let Some(c) = snapshot.get("contained_by").filter(|c| !c.is_null()) {
        restored += conn
            .prepare_cached(
                "INSERT OR IGNORE INTO containment (child_item_id, parent_item_id, position)
                 SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM items WHERE id = ?2)",
            )?
            .execute(params![
                id,
                c.get("parent_item_id").and_then(Value::as_i64),
                c.get("position").and_then(Value::as_i64).unwrap_or(0),
            ])?;
    }
    for c in rows("contains") {
        restored += conn
            .prepare_cached(
                "INSERT OR IGNORE INTO containment (child_item_id, parent_item_id, position)
                 SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM items WHERE id = ?1)",
            )?
            .execute(params![
                c.get("child_item_id").and_then(Value::as_i64),
                id,
                c.get("position").and_then(Value::as_i64).unwrap_or(0),
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
/// Invertible means the transaction contains an entry [`INVERSES`] covers — an `insert` into
/// an [`UNDOABLE_TABLES`] table, an item `delete` (which carries a restorable snapshot), an
/// edge weight `update`, a mount config `update`, a containment `update` — **and** no insert
/// into a table outside that list, which [`undo`] refuses rather than half-inverting.
///
/// The predicate is **generated** from those two lists rather than written out a second time.
/// Both halves have drifted before: the mount inverse reached `undo` alone, so a bare
/// `jkb undo` walked past a mount edit and reverted an older transaction; and `containment`
/// reached the schema without reaching `UNDOABLE_TABLES`, so undo died on every transaction
/// an `ingest` or a `task add --under` produced.
///
/// A transaction with none of those — the delete-only one `jkb item rm` produces, say — must
/// still count: otherwise `jkb undo` would skip straight past it and revert somebody's
/// unrelated earlier work while the deleted item stayed gone.
///
/// # Errors
/// Propagates any error from [`undo`].
pub fn undo_last(conn: &Connection, meta: &WriteMeta) -> Result<usize> {
    // Placeholders come from the const lists; every op and table name is *bound*, not
    // interpolated, so this stays a parameterized query.
    let mut clauses = Vec::with_capacity(INVERSES.len());
    let mut args: Vec<SqlValue> = Vec::new();
    for (op, table, _) in INVERSES {
        // An inverse says we CAN reverse this entry; it does not say the entry is worth
        // selecting a transaction for. A journal-only write is bookkeeping, not the user's work.
        if !selects_a_transaction(op, *table) {
            continue;
        }
        args.push(SqlValue::Text((*op).to_owned()));
        if let Some(t) = table {
            clauses.push("(c.op = ? AND c.entity_type = ?)".to_owned());
            args.push(SqlValue::Text((*t).to_owned()));
        } else {
            // An insert is only invertible for a table on `UNDOABLE_TABLES` — otherwise
            // `undo` errors. That list is therefore part of this predicate too, or the two
            // halves of "invertible" disagree again.
            let holes = vec!["?"; UNDOABLE_TABLES.len()].join(", ");
            clauses.push(format!("(c.op = ? AND c.entity_type IN ({holes}))"));
            args.extend(
                UNDOABLE_TABLES
                    .iter()
                    .map(|t| SqlValue::Text((*t).to_owned())),
            );
        }
    }
    // …and a transaction containing ANY insert into an unlisted table is skipped whole,
    // because `undo` would abort on it. The abort is deliberate — a half-undone transaction
    // is worse than none — but it rolls back without writing an `undo` marker, so selecting
    // such a transaction wedges `jkb undo` on it permanently, every run, forever. Selecting
    // an older one instead at least keeps undo working and leaves the entry in the log.
    //
    // Written as an uncorrelated `NOT IN` over transaction ids, NOT a correlated `NOT EXISTS`
    // on `c.txn_id`. The correlated form is re-evaluated per changelog **row** rather than per
    // transaction, and since no caller actually logs an unlisted insert it never
    // short-circuits — measured at ~6x on a database dominated by one large ingest (12.4s →
    // 71.8s). This subquery is materialized once.
    let unlisted = vec!["?"; UNDOABLE_TABLES.len()].join(", ");
    args.extend(
        UNDOABLE_TABLES
            .iter()
            .map(|t| SqlValue::Text((*t).to_owned())),
    );
    args.push(SqlValue::Integer(meta.txn_id));
    let sql = format!(
        "SELECT MAX(txn_id) FROM changelog c
         WHERE ({})
           AND c.txn_id NOT IN (
               SELECT txn_id FROM changelog
               WHERE op = 'insert' AND entity_type NOT IN ({unlisted})
           )
           AND c.txn_id < ?
           AND NOT EXISTS (
               SELECT 1 FROM changelog u
               WHERE u.op = 'undo' AND u.entity_id = CAST(c.txn_id AS TEXT)
           )",
        clauses.join(" OR ")
    );
    let target: Option<i64> = conn
        .prepare_cached(&sql)?
        .query_row(params_from_iter(args), |row| row.get(0))
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

    /// A bare `jkb undo` must survive the most common transaction shape in the system.
    ///
    /// Every `jkb ingest` writes a `containment` row per chunk and every `task add --under`
    /// writes one, but `containment` was not on `UNDOABLE_TABLES` — so `undo` aborted with
    /// "cannot undo unknown table", and because an aborted undo rolls back without writing an
    /// `undo` marker, the very next `jkb undo` selected the same transaction and died again,
    /// permanently.
    #[test]
    fn undo_last_handles_a_transaction_that_contains_an_item() {
        use crate::{containment, item};

        let db = Db::open_in_memory().unwrap();
        let parent = db
            .write_txn("t", |c, m| upsert(c, m, &note("parent")))
            .unwrap();
        // One transaction that both creates the child and files it under its container.
        let child = db
            .write_txn("t", move |c, m| {
                let child = upsert(c, m, &note("child"))?;
                containment::contain(c, m, child, parent, 0)?;
                Ok(child)
            })
            .unwrap();
        assert_eq!(
            db.read(move |c| containment::children(c, parent)).unwrap(),
            vec![child]
        );

        assert!(db.write_txn("t", undo_last).unwrap() > 0);
        assert!(
            db.read(|c| item::id_for_uid(c, "child")).unwrap().is_none(),
            "the child and its containment row go together"
        );
        assert!(db
            .read(move |c| containment::children(c, parent))
            .unwrap()
            .is_empty());
        // And undo can still advance: the parent's own transaction is next, not the same one.
        assert_eq!(db.write_txn("t", undo_last).unwrap(), 1);
        assert!(db
            .read(|c| item::id_for_uid(c, "parent"))
            .unwrap()
            .is_none());
    }

    /// Re-parenting is an upsert, so undoing it must put the previous container back — not
    /// delete the row and leave the item contained by nothing.
    #[test]
    fn undoing_a_re_parent_restores_the_previous_container() {
        use crate::containment;

        let db = Db::open_in_memory().unwrap();
        let (a, b, kid) = db
            .write_txn("t", |c, m| {
                let a = upsert(c, m, &note("a"))?;
                let b = upsert(c, m, &note("b"))?;
                let kid = upsert(c, m, &note("kid"))?;
                containment::contain(c, m, kid, a, 0)?;
                Ok((a, b, kid))
            })
            .unwrap();
        // A separate transaction that only moves it.
        db.write_txn("t", move |c, m| containment::contain(c, m, kid, b, 3))
            .unwrap();
        assert_eq!(
            db.read(move |c| containment::parent(c, kid)).unwrap(),
            Some(b)
        );

        db.write_txn("t", undo_last).unwrap();
        assert_eq!(
            db.read(move |c| containment::parent(c, kid)).unwrap(),
            Some(a),
            "the previous container is restored, not removed"
        );
    }

    /// Undoing an item delete must restore its **containment** rows, not only the
    /// `parent_of` edge. `SUBTASK_CLAUSE` — the anti-join that holds a parent off the ready
    /// frontier — reads `containment`, so restoring the edge alone put the parent back as a
    /// pickable task with a live open child, and a restored document's chunks were
    /// unreachable from `jkb ls <doc>`.
    #[test]
    fn undoing_a_delete_restores_containment_both_ways() {
        use crate::{containment, item};

        let db = Db::open_in_memory().unwrap();
        let (parent, kid, grandkid) = db
            .write_txn("t", |c, m| {
                let parent = upsert(c, m, &note("parent"))?;
                let kid = upsert(c, m, &note("kid"))?;
                let grandkid = upsert(c, m, &note("grandkid"))?;
                containment::contain(c, m, kid, parent, 0)?;
                containment::contain(c, m, grandkid, kid, 0)?;
                Ok((parent, kid, grandkid))
            })
            .unwrap();

        db.write_txn("t", move |c, m| item::remove(c, m, kid, true))
            .unwrap();
        db.write_txn("t", undo_last).unwrap();

        let restored = db
            .read(|c| item::id_for_uid(c, "kid"))
            .unwrap()
            .expect("the item is back");
        assert_eq!(
            db.read(move |c| containment::parent(c, restored)).unwrap(),
            Some(parent),
            "it is contained by its parent again"
        );
        assert_eq!(
            db.read(move |c| containment::children(c, restored))
                .unwrap(),
            vec![grandkid],
            "and it contains its own child again"
        );
    }

    /// The bare `jkb undo` — the only form any surface offers — must reverse a mount **edit**.
    /// `undo(txn)` grew that inverse while `undo_last`'s eligibility predicate did not, so the
    /// mount transaction was invisible to it: `MAX(txn_id)` landed on the earlier *create* and
    /// took the generic insert inverse, deleting the mount instead of restoring its config.
    #[test]
    fn undo_last_reverses_a_mount_edit_rather_than_deleting_the_mount() {
        use crate::{mount, ns};
        use jkb_types::{ConflictPolicy, SyncMode};

        let db = Db::open_in_memory().unwrap();
        let ns_id = db
            .write_txn("t", |c, _m| ns::ensure(c, "repos/x/docs"))
            .unwrap();
        db.write_txn("t", move |c, m| {
            mount::create(
                c,
                m,
                ns_id,
                "file:///tmp/x",
                SyncMode::Bidirectional,
                "tasks",
                Some("**/tasks.md"),
                None,
                ConflictPolicy::Manual,
            )
        })
        .unwrap();
        // A second `mount create` is the update command: it only changes the policy.
        db.write_txn("t", move |c, m| {
            mount::create(
                c,
                m,
                ns_id,
                "file:///tmp/x",
                SyncMode::Bidirectional,
                "tasks",
                Some("**/tasks.md"),
                None,
                ConflictPolicy::DiskWins,
            )
        })
        .unwrap();

        db.write_txn("t", undo_last).unwrap();
        let mount = db
            .read(move |c| mount::get(c, ns_id))
            .unwrap()
            .expect("the mount must survive: the edit is what was undone, not the mount");
        assert_eq!(mount.conflict_policy, ConflictPolicy::Manual.as_str());
        assert_eq!(mount.include_glob.as_deref(), Some("**/tasks.md"));
    }
}

#[cfg(test)]
mod selection_tests {
    use super::selects_a_transaction;

    /// A journal-only sync transaction must never be what a bare `jkb undo` picks.
    ///
    /// Several sync transactions write nothing but the journal — flagging `needs_attention`,
    /// re-settling a stale base, populating a legacy row. Adding `sync_state` to the inverse
    /// list made those eligible, so `jkb undo` meaning to take back a `task add` would rewind a
    /// journal flag instead, report "reverted 1 change(s)", leave the task untouched, and make
    /// the refused file invisible to `jkb doctor`.
    #[test]
    fn a_journal_only_transaction_is_not_the_last_undoable_change() {
        assert!(
            !selects_a_transaction("update", Some("sync_state")),
            "a journal-only sync transaction can be selected by a bare `jkb undo`"
        );
    }

    /// Everything else still selects — the exclusion is one tuple, not a new policy.
    #[test]
    fn real_work_still_selects_a_transaction() {
        for (op, table) in [
            ("insert", None),
            ("delete", Some("items")),
            ("update", Some("edges")),
            ("update", Some("mounts")),
            ("update", Some("containment")),
        ] {
            assert!(
                selects_a_transaction(op, table),
                "{op}/{table:?} stopped selecting a transaction"
            );
        }
    }
}
