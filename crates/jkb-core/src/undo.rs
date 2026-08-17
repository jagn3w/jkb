//! Undo: revert a transaction by inverting its changelog entries.
//!
//! ## A transaction is undone whole, or refused
//!
//! [`undo`] refuses a transaction containing work it cannot invert, and [`undo_last`] selects the
//! newest transaction containing **any** work rather than the newest one it happens to be able to
//! invert. Those two sentences are one rule, and the rule is the point of this module.
//!
//! What it replaces is a *retarget*: `undo_last` used to skip a transaction whose entries were not
//! in [`INVERSES`] and revert an older, unrelated one in its place — reporting success. Every
//! member of a three-pass family of defects was that retarget seen through a different hole.
//! Round 4 found `branch_records` missing from the undoable set (**which** tables); round 5 found
//! four upserts logging the op `insert` (**which** op); round 6 found that deriving the op
//! correctly produced `update` entries [`INVERSES`] could not invert (**invertibility**) — a
//! defect *caused* by the previous round's fix. Each axis was closed and the next one opened,
//! because the mechanism underneath — "cannot invert this? revert something else" — was untouched.
//! With the refusal in place a kind of entry nobody has taught this module about is a loud,
//! named refusal that writes nothing, whatever axis it arrives on.
//!
//! The cost is priced and accepted: `jkb undo` now refuses after `jkb ns mv`, `jkb tag rename`,
//! and the tag/placement/edge *removals* a file sync performs. That is strictly better than what
//! it did before, which was to revert somebody's earlier transaction instead and say it worked.
//!
//! ## The inverses
//!
//! Listed once, in [`INVERSES`]. **Inserts** are reversed by deleting the affected row by `rowid`
//! (the common "oops, undo that" case for creates). An **item delete** is reversed by restoring it
//! from the complete snapshot `item::remove` recorded in `before` — the item row plus the
//! placements, tag applications, edges, and binding that `ON DELETE CASCADE` took with it. That
//! pairing is what lets `jkb item rm` exist at all: a delete nothing can undo would break the
//! promise that every mutation is reversible.
//!
//! A **column update** ([`Inverse::Columns`]) is reversed by writing the `before` object's fields
//! back to the columns they name. That is not a convention a writer has to remember: for the
//! entities in [`COLUMN_UPDATES`], [`crate::changelog`] refuses at the *write* a before-state that
//! is absent, empty, or names anything that is not a column of the table — so a payload like
//! `{"content_len": 12}`, which reads as a before-state and restores nothing, fails at its own
//! writer instead of at somebody's later `jkb undo`.
//!
//! The row's `rowid` is stored as the changelog `entity_id` and `entity_type` is the table name.

use std::collections::BTreeSet;

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::Value;

use jkb_types::Error as TypeError;

use crate::changelog::{Entity, InsertInverse};
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
    /// Write the `before` object's fields back to the columns they name, keyed on `entity_id`.
    ///
    /// The generic answer for a write that changed an existing row, and the one that needs
    /// nothing hand-written per statement: what a writer already has to supply — the values
    /// that were there before — *is* the inverse. See [`ROW_KEYS`].
    Columns,
    /// Put a deleted row back, from the columns `before` names.
    ///
    /// The mirror image of [`Inverse::Columns`], driven by the same payload and validated by the
    /// same check. `OR IGNORE`, so re-running an undo is not an error.
    ReinsertRow,
    /// Delete the inserted row by `rowid`.
    DeleteRow,
}

/// Whether this entry's inverse is driven by a before-state made of column values — which is the
/// question [`check_restorable`] validates and nothing else needs to ask.
fn is_row_shaped(op: &str, table: &str) -> bool {
    matches!(
        inverse_for(op, table),
        Some(Inverse::Columns | Inverse::ReinsertRow)
    )
}

/// The column names `table` has, straight from the schema.
///
/// Read rather than listed: a column list kept beside the schema is exactly the drift this
/// module has spent three passes on, and `pragma_table_info` cannot be out of date.
fn table_columns(conn: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut stmt = conn.prepare_cached("SELECT name FROM pragma_table_info(?1)")?;
    let rows = stmt.query_map([table], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<BTreeSet<_>, _>>()?)
}

/// Refuse, **at the write**, a before-state that would restore nothing.
///
/// [`Inverse::Columns`] and [`Inverse::ReinsertRow`] can only put back what they are given, so a
/// payload that is absent, empty, or describes something other than the table's columns is an
/// inverse that silently does nothing — which is the shape this module keeps being bitten by.
/// `item::set_content` logged `{"content_len": 12}`, which reads like a before-state and restores
/// no content at all; `edge::unlink` logged `{"src": …, "dst": …}`, which are the *arguments* it
/// was called with rather than the columns `edges` has.
///
/// Checked here so the failure lands on the writer that got it wrong, in its own test, rather
/// than on an unrelated user's `jkb undo` much later.
///
/// # Errors
/// Returns a validation error naming the entity and the offending key.
pub(crate) fn check_restorable(
    conn: &Connection,
    op: &str,
    table: &str,
    before: Option<&Value>,
) -> Result<()> {
    if !is_row_shaped(op, table) {
        return Ok(());
    }
    let fields = before.and_then(Value::as_object).filter(|o| !o.is_empty());
    let Some(fields) = fields else {
        return Err(TypeError::Validation(format!(
            "a `{op}` on `{table}` is undone by writing its before-state back to the columns it \
             names, so it must be logged with a non-empty before-state; without one `jkb undo` \
             would restore nothing and report success"
        ))
        .into());
    };
    let columns = table_columns(conn, table)?;
    for key in fields.keys() {
        if !columns.contains(key) {
            return Err(TypeError::Validation(format!(
                "`{key}` is not a column of `{table}`, so undoing this `{op}` would not restore \
                 it — log the previous value under the column's own name"
            ))
            .into());
        }
    }
    Ok(())
}

/// A JSON before-state value as something `rusqlite` can bind.
fn as_sql(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Number(n) => n.as_i64().map_or_else(
            || n.as_f64().map_or(SqlValue::Null, SqlValue::Real),
            SqlValue::Integer,
        ),
        Value::String(s) => SqlValue::Text(s.clone()),
        // A JSON column (`items.metadata`, `placements.metadata`) round-trips as its text.
        other => SqlValue::Text(other.to_string()),
    }
}

/// The before-state's fields as `(quoted column, bound value)`, having re-checked that each names
/// a column of `table`.
///
/// Re-checked here as well as at the write because a row may have been logged by a build whose
/// idea of the schema differs; the identifiers are spliced into SQL, so "these came from
/// `pragma_table_info`" has to be true at the moment they are used, not merely when they were
/// written.
fn restorable_fields(
    conn: &Connection,
    op: &str,
    table: &str,
    before: Option<&Value>,
) -> Result<Vec<(String, SqlValue)>> {
    check_restorable(conn, op, table, before)?;
    Ok(before
        .and_then(Value::as_object)
        .map(|fields| {
            fields
                .iter()
                .map(|(column, value)| (format!("\"{column}\""), as_sql(value)))
                .collect()
        })
        .unwrap_or_default())
}

/// Put the columns named in `before` back on the row `entity_id` identifies.
fn restore_columns(
    conn: &Connection,
    op: &str,
    table: &str,
    entity_id: &str,
    before: Option<&Value>,
) -> Result<usize> {
    // The table name spliced into the SQL comes from `Entity`, never from the string the log
    // handed us — the same rule the `DeleteRow` arm follows.
    let Some(entity) = Entity::parse(table) else {
        return Err(TypeError::Validation(format!(
            "cannot restore columns of unknown table '{table}'"
        ))
        .into());
    };
    let fields = restorable_fields(conn, op, table, before)?;
    if fields.is_empty() {
        return Ok(0);
    }
    // Every key column here is an integer one; a row id that is not a number is a corrupt entry,
    // not something to coerce.
    let row: i64 = entity_id.parse().map_err(|_| {
        TypeError::Validation(format!("bad {table} row id '{entity_id}' in changelog"))
    })?;
    let mut args: Vec<SqlValue> = fields.iter().map(|(_, v)| v.clone()).collect();
    args.push(SqlValue::Integer(row));
    // ADDRESSED BY `rowid`, always. Every entity with this inverse records `entity_id` as one —
    // the same convention `InsertInverse::DeleteRow` relies on — so there is no per-table key
    // column to keep in step with the schema. A list of them was tried and removed: `SQLite` reads
    // a double-quoted identifier that resolves to no column as a **string literal**, so a wrong
    // key in the `WHERE` matched nothing in perfect silence, and the test written to check the
    // list passed with a fabricated column name in it.
    let sql = format!(
        "UPDATE \"{}\" SET {} WHERE rowid = ?",
        entity.as_str(),
        fields
            .iter()
            .map(|(c, _)| format!("{c} = ?"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(conn.prepare_cached(&sql)?.execute(params_from_iter(args))?)
}

/// Put a deleted row back, exactly as `before` describes it.
fn reinsert_row(conn: &Connection, op: &str, table: &str, before: Option<&Value>) -> Result<usize> {
    let Some(entity) = Entity::parse(table) else {
        return Err(TypeError::Validation(format!(
            "cannot restore a row of unknown table '{table}'"
        ))
        .into());
    };
    let fields = restorable_fields(conn, op, table, before)?;
    if fields.is_empty() {
        return Ok(0);
    }
    // `OR IGNORE` so a re-run is not an error, and so a row whose foreign keys are gone (the other
    // end of an edge deleted in the same transaction and not restored) is skipped rather than
    // failing the whole undo — the rule `restore_children` already follows.
    let sql = format!(
        "INSERT OR IGNORE INTO \"{}\" ({}) VALUES ({})",
        entity.as_str(),
        fields
            .iter()
            .map(|(c, _)| c.clone())
            .collect::<Vec<_>>()
            .join(", "),
        vec!["?"; fields.len()].join(", ")
    );
    Ok(conn
        .prepare_cached(&sql)?
        .execute(params_from_iter(fields.into_iter().map(|(_, v)| v)))?)
}

/// **The** list of inversions this module implements, as data: `(op, entity_type, inverse)`,
/// where a `None` entity type matches any table.
///
/// [`undo`] dispatches on it and refuses any *work* entry it does not cover, so a gap here is a
/// named refusal rather than an unrelated transaction quietly disappearing. It was the latter
/// once: the `update`+`mounts` inverse was added to `undo` alone, and a bare `jkb undo` — which
/// is the only form any surface offers, since nothing prints txn ids — walked straight past a
/// mount edit and reverted an older transaction, deleting the mount it had been asked to restore.
const INVERSES: &[(&str, Option<&str>, Inverse)] = &[
    // The hand-written ones come first: `inverse_for` is first-match-wins, and each of these is a
    // table whose before-state is NOT a plain column map (an item delete carries a whole cascade
    // snapshot) or whose row is keyed by something `entity_id` does not hold.
    ("delete", Some("items"), Inverse::ItemSnapshot),
    ("update", Some("edges"), Inverse::EdgeWeight),
    ("update", Some("mounts"), Inverse::MountConfig),
    ("update", Some("containment"), Inverse::ContainmentRow),
    // (`insert` is deliberately absent from these table-specific entries and handled by the
    // wildcard below, whose membership is DERIVED from `Entity::insert_inverse`.)
    // A sync transaction deletes items AND advances the file's journal. With no inverse here
    // the journal survived the undo, so `last_synced_hash` still described bytes whose items
    // were gone — and the next reconcile saw "KB changed, disk did not" and exported an
    // item-less render over the file, silently emptying it. `undo_last` could select such a
    // transaction (it inserts items), so this was reachable by a bare `jkb undo` with the
    // watcher running.
    // NOT work — see `is_work`. This inverse exists to accompany a sync transaction that also
    // touched items; on its own the journal is bookkeeping, not the user's change.
    ("update", Some("sync_state"), Inverse::SyncStateRow),
    // …then the generic pair, which is where a new table should go. Both are driven by the
    // before-state the writer already records, and `changelog::write` refuses one that could not
    // restore anything — so adding a row here is a claim the compiler and the writer's own first
    // write both check, rather than a promise to have written the right SQL somewhere else.
    ("update", Some("items"), Inverse::Columns),
    // The claim ops write `items` columns like any other write; naming them keeps `jkb undo`
    // after a `jkb task start` working instead of refusing on the claim that came with it.
    ("claim", Some("items"), Inverse::Columns),
    ("release", Some("items"), Inverse::Columns),
    ("reclaim", Some("items"), Inverse::Columns),
    ("update", Some("bindings"), Inverse::Columns),
    ("update", Some("placements"), Inverse::Columns),
    ("update", Some("tag_applications"), Inverse::Columns),
    ("update", Some("namespaces"), Inverse::Columns),
    ("update", Some("branch_records"), Inverse::Columns),
    ("update", Some("ingestions"), Inverse::Columns),
    ("delete", Some("placements"), Inverse::ReinsertRow),
    ("delete", Some("tag_applications"), Inverse::ReinsertRow),
    ("delete", Some("edges"), Inverse::ReinsertRow),
    ("delete", Some("namespaces"), Inverse::ReinsertRow),
    ("delete", Some("branch_records"), Inverse::ReinsertRow),
    // Any table whose inserts delete by rowid; an insert into anything else is uninvertible, not
    // deleted on a guess. The wildcard entry must stay LAST: `inverse_for` is first-match-wins, so
    // a table-specific `insert` inverse placed after it would never be reached.
    ("insert", None, Inverse::DeleteRow),
];

/// Whether an entry is the user's **work**, as opposed to bookkeeping that rides along with it.
///
/// Two consequences, and they are the whole selection policy. A transaction made only of
/// non-work entries is not what a bare `jkb undo` reaches for; and a non-work entry does not
/// make its transaction refusable, because failing to rewind a marker is not failing to give
/// work back.
///
/// The two members earn it differently. Several sync transactions write nothing but the
/// journal — a refusal flagging `needs_attention`, a re-settle, a legacy row being populated —
/// and once `sync_state` joined the inverse list those became "the last invertible
/// transaction", so `jkb undo` meaning to take back a `task add` rewound a journal flag,
/// reported "reverted 1 change(s)", left the task untouched and made the refused file invisible
/// to `jkb doctor`. An `undo` marker is the record that a transaction was reverted; undoing an
/// undo is not something this module does, and treating the marker as work would make every
/// `jkb undo` after the first refuse.
fn is_work(op: &str, table: &str) -> bool {
    !BOOKKEEPING.contains(&(op, table))
}

/// The entries [`is_work`] excludes — **the list, spelled once**, because `undo_last`'s selection
/// query needs the same pairs and a `matches!` in Rust cannot be handed to `SQLite`.
///
/// It was written out twice, and the two copies did not do the same job: mutating the predicate
/// changed nothing about which transaction a bare `jkb undo` picked, because the query had its own
/// copy. Two spellings of one rule is the shape this whole module is a correction of.
const BOOKKEEPING: &[(&str, &str)] = &[("update", "sync_state"), ("undo", "changelog")];

/// The inversion for a changelog entry, or `None` if this module cannot reverse it.
///
/// The wildcard `insert` entry is narrowed here rather than in the list: an insert is only
/// reversible by `DELETE … WHERE rowid = ?` for a table that answered
/// [`InsertInverse::DeleteRow`], which is the same derivation [`crate::changelog::write`]
/// enforces at the writer. A row naming any other table — from a hand-edited log, or a binary
/// that knows tables this one does not — is uninvertible, not deleted on a guess.
fn inverse_for(op: &str, table: &str) -> Option<Inverse> {
    let inverse = INVERSES
        .iter()
        .find(|(o, t, _)| *o == op && t.is_none_or(|t| t == table))
        .map(|(_, _, inv)| *inv)?;
    if inverse == Inverse::DeleteRow
        && Entity::parse(table).map(Entity::insert_inverse) != Some(InsertInverse::DeleteRow)
    {
        return None;
    }
    Some(inverse)
}

/// Invert one changelog entry, returning how many rows it changed.
///
/// Only called for entries [`inverse_for`] covers — [`undo`] refuses the transaction otherwise
/// — except for non-work entries, which are skipped with `Ok(0)`.
///
/// # Errors
/// Returns a validation error for a malformed `entity_id`, or for an unreadable or unusable
/// before-state.
fn invert_entry(
    conn: &Connection,
    op: &str,
    table: &str,
    entity_id: &str,
    before: Option<&str>,
) -> Result<usize> {
    let mut rows = 0;
    // Dispatch through `INVERSES` — the same list `undo` refuses a missing entry from.
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
        // Every write that changed an existing row, for the entities in `COLUMN_UPDATES`: put
        // the columns named in `before` back. Nothing is hand-written per statement, so a new
        // `UPDATE items SET …` gains an inverse by supplying its before-state — which
        // `changelog::write` requires of it anyway.
        Inverse::Columns => {
            let snap = snapshot("column before-state")?;
            rows += restore_columns(conn, op, table, entity_id, snap.as_ref())?;
        }
        // …and the mirror image for a delete: the row `before` describes, put back whole.
        Inverse::ReinsertRow => {
            let snap = snapshot("deleted row")?;
            rows += reinsert_row(conn, op, table, snap.as_ref())?;
        }
        Inverse::DeleteRow => {
            // The interpolated name comes from the **enum**, not from the string the log handed
            // us: a row written by a newer binary, or a hand-edited log, cannot name a table this
            // build will splice into SQL.
            let entity = Entity::parse(table)
                .filter(|e| e.insert_inverse() == InsertInverse::DeleteRow)
                .ok_or_else(|| {
                    TypeError::Validation(format!("cannot undo unknown table '{table}'"))
                })?;
            rows += conn
                .prepare_cached(&format!("DELETE FROM {} WHERE rowid = ?1", entity.as_str()))?
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

/// Revert transaction `txn_id` by inverting its changelog entries (most recent first).
/// Returns the number of rows changed and records an `undo` marker.
///
/// **All of it, or none of it.** A transaction containing work no [`Inverse`] covers is refused
/// before anything is written, and the refusal names every such entry rather than the first one.
/// The alternative — invert what we can and skip the rest — is what "reverted 3 change(s)" used
/// to mean when it had left the row you asked about untouched.
///
/// # Errors
/// Returns a validation error if the transaction contains work this module cannot invert, or if
/// an entry's before-state is unreadable; otherwise a database error.
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

    let uninvertible: Vec<String> = entries
        .iter()
        .filter(|(op, table, _, _)| is_work(op, table) && inverse_for(op, table).is_none())
        .map(|(op, table, _, _)| format!("`{op}` on `{table}`"))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !uninvertible.is_empty() {
        return Err(TypeError::Validation(format!(
            "transaction {txn_id} cannot be undone: `jkb undo` has no inverse for {}, and \
             reversing the rest would leave the change half undone. Nothing was changed",
            uninvertible.join(", "),
        ))
        .into());
    }

    let mut reverted = 0;
    for (op, table, entity_id, before) in entries {
        reverted += invert_entry(conn, &op, &table, &entity_id, before.as_deref())?;
    }

    changelog::append(
        conn,
        meta,
        "undo",
        Entity::Changelog,
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

/// Undo the most recent transaction containing **work** that has not already been undone, and
/// return the number of rows it changed (0 if there is nothing to undo).
///
/// The selection asks one question — *which transaction is the user's last change?* — and asks
/// it of [`is_work`] alone. It deliberately does **not** ask whether that transaction can be
/// inverted: if it cannot, [`undo`] refuses and says so. That separation is the whole fix.
///
/// It used to conflate the two, selecting the newest transaction it *could* invert, which meant
/// every gap in [`INVERSES`] was a silent retarget onto somebody's older work. The gaps were
/// found one axis at a time — a missing table (`branch_records`), a wrongly chosen op
/// (`insert` on an upsert), an op with no inverse (`update` on `items`, which the op fix itself
/// created) — and closing each one left the retarget, so the next gap did the same damage
/// through a different hole. Now a gap is a refusal.
///
/// A transaction with no invertible entry at all — the delete-only one `jkb item rm` produces,
/// say — is therefore still selected, because it is still the user's last change; reaching past
/// it to revert unrelated earlier work is exactly the behaviour being removed.
///
/// # Errors
/// Propagates any error from [`undo`], including its refusal.
pub fn undo_last(conn: &Connection, meta: &WriteMeta) -> Result<usize> {
    // Bookkeeping-only transactions are skipped, and nothing else is. The clauses are generated
    // from `BOOKKEEPING`, the same list `is_work` answers from, and every op and table name is
    // *bound* rather than interpolated — so this stays a parameterized query and cannot come to
    // disagree with the predicate about which entries are the user's work.
    let mut args: Vec<SqlValue> = Vec::with_capacity(BOOKKEEPING.len() * 2 + 1);
    for (op, table) in BOOKKEEPING {
        args.push(SqlValue::Text((*op).to_owned()));
        args.push(SqlValue::Text((*table).to_owned()));
    }
    args.push(SqlValue::Integer(meta.txn_id));
    let sql = format!(
        "SELECT MAX(txn_id) FROM changelog c
          WHERE {}
            AND c.txn_id < ?
            AND NOT EXISTS (
                SELECT 1 FROM changelog u
                WHERE u.op = 'undo' AND u.entity_id = CAST(c.txn_id AS TEXT)
            )",
        BOOKKEEPING
            .iter()
            .map(|_| "NOT (c.op = ? AND c.entity_type = ?)".to_owned())
            .collect::<Vec<_>>()
            .join(" AND ")
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

    /// Undoing a transaction that **re-saved** a view puts the previous query back.
    ///
    /// Two defects met here, one per round. `view::save` upserts, and it used to log the op
    /// `insert` unconditionally — so `undo` took the generic `DELETE … WHERE rowid = ?` and
    /// destroyed a saved query the transaction had merely edited. Deriving the op fixed that and
    /// produced an `update` nothing could invert, so `undo_last` skipped the whole transaction
    /// and reverted an older one in its place. The `Inverse::Columns` arm closes the second: the
    /// before-state `view::save` already had to record *is* the restoration.
    #[test]
    fn undoing_a_re_saved_view_restores_the_previous_query() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |c, m| {
            crate::view::save(c, m, "bugs", "kind:task status:open")
        })
        .unwrap();
        db.write_txn("t", |c, m| {
            crate::view::save(c, m, "bugs", "kind:task status:done")
        })
        .unwrap();

        // Through the bare form — the only one any surface offers, and the one that used to
        // reach past this transaction entirely.
        db.write_txn("t", undo_last).unwrap();
        assert_eq!(
            db.read(|c| crate::view::get(c, "bugs")).unwrap().as_deref(),
            Some("kind:task status:open"),
            "undoing an edit to a view did not restore the query it replaced"
        );
    }

    /// Undoing a transaction that **re-placed** an existing mirror restores its old position.
    ///
    /// `placement::place` is idempotent on `(item, namespace, role)` and the sync engine re-places
    /// every mirror on every `apply_doc`, so logging that as an insert made `jkb undo` of an
    /// ordinary file sync unplace mirrors that long predated the transaction.
    #[test]
    fn undoing_a_re_placement_restores_its_position() {
        use jkb_types::PlacementRole;
        let db = Db::open_in_memory().unwrap();
        let id = db.write_txn("t", |c, m| upsert(c, m, &note("a"))).unwrap();
        let ns = db
            .write_txn("t", |c, _| crate::ns::ensure(c, "x/y"))
            .unwrap();
        db.write_txn("t", move |c, m| {
            crate::placement::place(c, m, id, ns, PlacementRole::Primary, 0)
        })
        .unwrap();
        db.write_txn("t", move |c, m| {
            crate::placement::place(c, m, id, ns, PlacementRole::Primary, 1)
        })
        .unwrap();

        db.write_txn("t", undo_last).unwrap();
        let placed: Vec<i64> = db
            .read(move |c| {
                let mut stmt =
                    c.prepare("SELECT position FROM placements WHERE item_id = ?1 ORDER BY rowid")?;
                let rows = stmt.query_map([id.get()], |r| r.get::<_, i64>(0))?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .unwrap();
        assert_eq!(
            placed,
            vec![0],
            "the placement must survive with the position it had before the transaction"
        );
    }

    /// Undoing a transaction that **re-bound** an already-bound item puts the old uri back.
    ///
    /// Sync's re-attach path calls `binding::set` for items that were already bound; logging that
    /// as an insert had undo delete the binding row, leaving a file-backed item with no uri. With
    /// the op derived but no inverse for it, `undo_last` walked past the re-binding instead and
    /// reverted an older transaction: the wrong uri survived and the item ended up unbound anyway.
    #[test]
    fn undoing_a_re_binding_restores_the_previous_uri() {
        let db = Db::open_in_memory().unwrap();
        let id = db.write_txn("t", |c, m| upsert(c, m, &note("a"))).unwrap();
        db.write_txn("t", move |c, m| {
            crate::binding::set(c, m, id, "file:///tmp/a.md", None, None)
        })
        .unwrap();
        db.write_txn("t", move |c, m| {
            crate::binding::set(c, m, id, "file:///tmp/WRONG.md", None, None)
        })
        .unwrap();

        db.write_txn("t", undo_last).unwrap();
        assert_eq!(
            db.read(move |c| crate::binding::get(c, id))
                .unwrap()
                .map(|b| b.uri),
            Some("file:///tmp/a.md".to_owned()),
            "undoing a re-binding did not put the uri it replaced back"
        );
    }

    /// Undoing a transaction that **re-applied** an existing tag leaves the tag on the item.
    ///
    /// `tag::apply` is documented as idempotent, and its `ON CONFLICT` arm updates the row that is
    /// already there — logged as an insert, `undo` removed a tag application the transaction never
    /// created.
    #[test]
    fn undoing_a_re_applied_tag_does_not_remove_it() {
        let db = Db::open_in_memory().unwrap();
        let id = db.write_txn("t", |c, m| upsert(c, m, &note("a"))).unwrap();
        db.write_txn("t", move |c, m| {
            crate::tag::apply(c, m, id, "size", "small")
        })
        .unwrap();
        let second = db
            .write_txn("t", move |c, m| {
                crate::tag::apply(c, m, id, "size", "small")?;
                Ok(m.txn_id)
            })
            .unwrap();

        db.write_txn("t", move |c, m| super::undo(c, m, second))
            .unwrap();
        assert_eq!(
            db.read(move |c| crate::tag::applications(c, id)).unwrap(),
            vec![("size".to_owned(), "small".to_owned())],
            "undoing a re-application removed a tag the transaction had not created"
        );
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
    /// writes one, but `containment` had no inverse — so `undo` aborted with
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

    /// The other half of the same family, one table later: `jkb task start` writes a
    /// `branch_records` row, logged as an `insert`.
    ///
    /// An insert `undo` could not reverse used to make `undo_last` skip the whole transaction — so
    /// a bare `jkb undo` after `task start` did not fail, it quietly reverted the *previous*
    /// transaction and reported success, deleting the task itself. That is the failure mode worth
    /// a test: not "undo errors" but "undo silently reverts the wrong thing".
    ///
    /// Membership is derived from `Entity::insert_inverse`, so this is a regression test for the
    /// derivation rather than for a line in a list — and since round 6 the skip itself is gone, so
    /// the same omission would now cost a refusal instead of this task.
    #[test]
    fn a_transaction_that_records_a_branch_is_the_one_undo_last_reverts() {
        use crate::branch::{self, Cut, Supersede};
        use crate::item;

        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |c, m| upsert(c, m, &note("the-task")))
            .unwrap();
        db.write_txn("t", |c, m| {
            branch::record_cut_point(
                c,
                m,
                "jkb",
                "task/x",
                &Cut::Fork("a".repeat(40)),
                None,
                Supersede::default(),
            )
        })
        .unwrap();

        assert!(db.write_txn("t", undo_last).unwrap() > 0);
        assert_eq!(
            db.read(|c| branch::get(c, "jkb", "task/x")).unwrap(),
            None,
            "the branch record survived the undo that claimed to have reverted its transaction"
        );
        assert!(
            db.read(|c| item::id_for_uid(c, "the-task"))
                .unwrap()
                .is_some(),
            "undo reverted an older transaction instead, deleting a task nobody asked about"
        );
    }

    /// **The root of the three-pass family.** A transaction carrying work with no inverse is
    /// refused; it is never swapped for an older one.
    ///
    /// The transaction here does real work (it creates an item) *and* records something this
    /// module has never been taught to reverse. Before the refusal, `undo_last` skipped the whole
    /// transaction, found the older one that created `keep-me`, deleted that item and reported
    /// "reverted 1 change(s)" — with the newer item still there and nothing having been undone
    /// that anybody asked about.
    ///
    /// The unknown entry is written directly rather than through a repo, deliberately: every op
    /// any repo writes today *is* invertible, and a test keyed to whichever one happens not to be
    /// would go vacuously green the day that one gained an inverse. What is pinned is the policy
    /// — an entry kind nobody taught this module about costs a named message, never somebody
    /// else's work — and that has to survive the list growing.
    #[test]
    fn work_with_no_inverse_is_refused_rather_than_swapped_for_an_older_change() {
        use crate::changelog::{self, Entity};
        use crate::item;

        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |c, m| upsert(c, m, &note("keep-me")))
            .unwrap();
        db.write_txn("t", |c, m| {
            upsert(c, m, &note("newer"))?;
            changelog::append(c, m, "transmute", Entity::Items, "1", None, None)
        })
        .unwrap();

        let err = db
            .write_txn("t", undo_last)
            .expect_err("the un-invertible transaction was skipped and an older one reverted");
        let err = err.to_string();
        assert!(
            err.contains("`transmute` on `items`"),
            "the refusal does not name what it cannot invert: {err}"
        );

        assert!(
            db.read(|c| item::id_for_uid(c, "keep-me"))
                .unwrap()
                .is_some(),
            "an older, unrelated transaction was reverted in its place"
        );
        assert!(
            db.read(|c| item::id_for_uid(c, "newer")).unwrap().is_some(),
            "the refusal reverted half of the transaction it refused"
        );
    }

    /// `jkb ns mv` moves a whole subtree, and undoing it must put **every** row back.
    ///
    /// It logged one entry naming the root's old `path`, which describes a fraction of what
    /// changed — so an undo would have left every descendant under the new path. That is why the
    /// refusal above found it uninvertible, and why the fix was to log what actually happened
    /// rather than to add a special case.
    #[test]
    fn undoing_a_namespace_move_restores_the_whole_subtree() {
        use crate::ns;

        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |c, _| ns::ensure(c, "notes/old/deep"))
            .unwrap();
        db.write_txn("t", |c, m| ns::move_subtree(c, m, "notes/old", "notes/new"))
            .unwrap();
        assert!(db.read(|c| ns::get(c, "notes/new/deep")).unwrap().is_some());

        db.write_txn("t", undo_last).unwrap();
        assert!(
            db.read(|c| ns::get(c, "notes/old/deep")).unwrap().is_some(),
            "the descendant was left under the new path, so the move was only half undone"
        );
        assert!(
            db.read(|c| ns::get(c, "notes/new/deep")).unwrap().is_none(),
            "the descendant is still under the path the move gave it"
        );
    }

    /// Undoing a transaction that **removed** a tag application puts it back, with its props.
    ///
    /// A delete is inverted by re-inserting the row its before-state describes, which is the
    /// mirror image of the column restore — and the reason a file sync, which reconciles tags by
    /// removing the ones the file no longer declares, is undoable at all.
    #[test]
    fn undoing_a_tag_removal_puts_the_application_back() {
        let db = Db::open_in_memory().unwrap();
        let id = db.write_txn("t", |c, m| upsert(c, m, &note("a"))).unwrap();
        db.write_txn("t", move |c, m| {
            crate::tag::apply(c, m, id, "size", "small")
        })
        .unwrap();
        db.write_txn("t", move |c, m| {
            crate::tag::remove(c, m, id, "size", "small")
        })
        .unwrap();

        db.write_txn("t", undo_last).unwrap();
        assert_eq!(
            db.read(move |c| crate::tag::applications(c, id)).unwrap(),
            vec![("size".to_owned(), "small".to_owned())],
            "undoing a tag removal did not put the application back"
        );
    }

    /// …and an unlinked edge comes back **with its weight**, which is the knowledge it carried.
    #[test]
    fn undoing_an_unlink_restores_the_edge_and_its_weight() {
        use crate::edge;
        use jkb_types::EdgeType;

        let db = Db::open_in_memory().unwrap();
        let (obs, hyp) = db
            .write_txn("t", |c, m| {
                let obs = upsert(c, m, &note("observation"))?;
                let hyp = upsert(c, m, &note("hypothesis"))?;
                edge::link_weighted(c, m, obs, hyp, EdgeType::Supports, Some(2.5), None)?;
                Ok((obs, hyp))
            })
            .unwrap();
        db.write_txn("t", move |c, m| {
            edge::unlink(c, m, obs, hyp, EdgeType::Supports)
        })
        .unwrap();

        db.write_txn("t", undo_last).unwrap();
        assert_eq!(
            db.read(move |c| edge::edges_from(c, obs, EdgeType::Supports))
                .unwrap(),
            vec![hyp],
            "the unlinked edge did not come back"
        );
        let weight = db.read(move |c| edge::evidence_for(c, hyp)).unwrap();
        assert!(
            (weight - 2.5).abs() < 1e-9,
            "the edge came back without the weight it carried, got {weight}"
        );
    }

    /// Undoing a `jkb task start` puts the claim back the way it was, rather than refusing or —
    /// worse — reaching past the claim for somebody's earlier transaction.
    ///
    /// `claim`/`release`/`reclaim` are ops of their own whose before-states are ordinary `items`
    /// columns, so they are inverted by the same arm every other column write is.
    #[test]
    fn undoing_a_claim_hands_the_task_back_unclaimed() {
        use crate::{claim, item};
        use jkb_types::TaskStatus;

        let db = Db::open_in_memory().unwrap();
        let id = db
            .write_txn("t", |c, m| upsert(c, m, &note("a-task")))
            .unwrap();
        db.write_txn("t", move |c, m| {
            crate::task::set_status(c, m, id, TaskStatus::Open)
        })
        .unwrap();
        assert!(db
            .write_txn("t", move |c, m| claim::claim(c, m, id, "host:1"))
            .unwrap());

        db.write_txn("t", undo_last).unwrap();
        assert!(
            db.read(claim::claimed).unwrap().is_empty(),
            "undoing a claim left the task claimed"
        );
        assert_eq!(
            db.read(move |c| item::get(c, id))
                .unwrap()
                .and_then(|m| m.status),
            Some("open".to_owned()),
            "undoing a claim left the task in the status the claim set"
        );
    }

    /// A before-state that cannot restore anything fails **at the writer**, not at a later undo.
    ///
    /// `item::set_content` logged `{"content_len": 12}` — a payload that reads like a before-state
    /// and names no column, so the inverse would have run, restored nothing and reported success.
    /// This is the property that makes `Inverse::Columns` safe to apply generically.
    #[test]
    fn a_before_state_that_names_no_column_is_refused_at_the_writer() {
        use crate::changelog::{self, Entity};

        let db = Db::open_in_memory().unwrap();
        let err = db
            .write_txn("t", |c, m| {
                changelog::append(
                    c,
                    m,
                    "update",
                    Entity::Items,
                    "1",
                    Some(&serde_json::json!({ "content_len": 12 })),
                    None,
                )
            })
            .expect_err("a before-state naming no column was accepted");
        assert!(
            err.to_string().contains("`content_len` is not a column"),
            "the refusal does not name the offending key: {err}"
        );

        // …and an absent one is refused too: it is the same silent no-op one step further along.
        let err = db
            .write_txn("t", |c, m| {
                changelog::append(c, m, "update", Entity::Items, "1", None, None)
            })
            .expect_err("an update with no before-state at all was accepted");
        assert!(
            err.to_string().contains("non-empty before-state"),
            "the refusal does not say what is missing: {err}"
        );
    }

    /// A **`branch_records`** update comes back too — the entity whose non-insert writes are all
    /// upserts, so an undo of one has nothing to delete and everything to restore.
    ///
    /// Its own test because `record_json` deliberately emits every column rather than the ones the
    /// statement touched, and that is only load-bearing through this arm.
    #[test]
    fn undoing_a_land_target_write_restores_the_record_it_replaced() {
        use crate::branch::{self, Cut, Supersede};

        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |c, m| {
            branch::record_cut_point(
                c,
                m,
                "jkb",
                "task/x",
                &Cut::Fork("a".repeat(40)),
                None,
                Supersede::default(),
            )
        })
        .unwrap();
        db.write_txn("t", |c, m| {
            branch::set_land_target(c, m, "jkb", "task/x", Some("batch-1"))
        })
        .unwrap();
        db.write_txn("t", |c, m| {
            branch::set_land_target(c, m, "jkb", "task/x", Some("batch-2"))
        })
        .unwrap();

        db.write_txn("t", undo_last).unwrap();
        let record = db
            .read(|c| branch::get(c, "jkb", "task/x"))
            .unwrap()
            .expect("the record must survive: the edit is what was undone");
        assert_eq!(
            record.land_target.as_deref(),
            Some("batch-1"),
            "undoing a land-target edit did not restore the target it replaced"
        );
        assert_eq!(
            record.cut_point.as_deref(),
            Some("a".repeat(40).as_str()),
            "the cut point was disturbed by undoing an unrelated column"
        );
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
    use super::{is_work, undo_last};
    use crate::changelog::{self, Entity};
    use crate::item::{self, upsert, NewItem};
    use crate::Db;

    /// A bare `jkb undo` must reach past **exactly** what `is_work` calls bookkeeping, and nothing
    /// else.
    ///
    /// `undo_last`'s SQL spells the two pairs out as bound parameters, so this is what stops that
    /// query and `is_work` drifting into disagreement — and it asserts the *consequence* rather
    /// than the predicate, so it cannot be satisfied by the function under test agreeing with
    /// itself.
    ///
    /// The two members earn their place differently. Several sync transactions write nothing but
    /// the journal — flagging `needs_attention`, re-settling a stale base, populating a legacy row
    /// — and once `sync_state` gained an inverse those became "the last invertible transaction",
    /// so `jkb undo` meaning to take back a `task add` rewound a journal flag, reported "reverted
    /// 1 change(s)" and left the task untouched. An `undo` marker is the record that a transaction
    /// was reverted, and treating it as work would make every `jkb undo` after the first refuse
    /// (nothing inverts an undo), which is a stuck command rather than a wrong one.
    #[test]
    fn a_bookkeeping_only_transaction_is_not_the_last_change() {
        for (op, entity) in [("update", Entity::SyncState), ("undo", Entity::Changelog)] {
            let db = Db::open_in_memory().unwrap();
            db.write_txn("t", |c, m| {
                upsert(
                    c,
                    m,
                    &NewItem {
                        uid: "target".to_owned(),
                        kind: "note".to_owned(),
                        content: None,
                        content_hash: None,
                        mime: None,
                    },
                )
            })
            .unwrap();
            db.write_txn("t", move |c, m| {
                changelog::append(c, m, op, entity, "file:///x", None, None)
            })
            .unwrap();

            db.write_txn("t", undo_last).unwrap();
            assert!(
                db.read(|c| item::id_for_uid(c, "target"))
                    .unwrap()
                    .is_none(),
                "a transaction of nothing but {op}/{} was taken as the user's last change, so \
                 `jkb undo` rewound bookkeeping and left the real one standing",
                entity.as_str()
            );
            // Asserted *after* the consequence, so a mutation to either half fails on the
            // behaviour rather than on this line: the predicate agreeing with itself is not what
            // the test is for.
            assert!(
                !is_work(op, entity.as_str()),
                "{op}/{} is treated as the user's work",
                entity.as_str()
            );
        }
    }

    /// Everything else is work — including the kinds this module has no inverse for. That is the
    /// point: they make their transaction selectable, and `undo` then refuses it by name rather
    /// than reaching past it.
    #[test]
    fn real_work_still_selects_a_transaction() {
        for (op, table) in [
            ("insert", "items"),
            ("delete", "items"),
            ("update", "edges"),
            ("update", "mounts"),
            ("update", "containment"),
            ("update", "tag_defs"),
            ("claim", "items"),
        ] {
            assert!(is_work(op, table), "{op}/{table} stopped counting as work");
        }
    }
}
