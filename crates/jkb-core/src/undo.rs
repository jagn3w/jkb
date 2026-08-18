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
//! **What actually refuses is [`INVERSES`] and the pre-flight in [`undo`], not a list of flows.**
//! Naming flows here went stale within the commit that wrote it: `ns mv`, the claim, the status
//! change and the tag/placement/edge removals a file sync performs were all listed as newly
//! refused and all gained inverses in that same change. Ask the symbols instead — a flow refuses
//! exactly when one of its entries has no row in [`INVERSES`], [`blocker`] says its inverse could
//! not run, or the inverse fails when [`undo`] applies it — and note the two refusals that are
//! none of those: everything at or below the [`watermark`] predates undo history, and a
//! transaction [`already_undone`] is not undone twice.
//!
//! ## Refusal is total; the pre-flight only makes it better worded
//!
//! Two mechanisms, and only the second is the guarantee.
//!
//! [`blocker`] runs during the scan and asks what can be answered by **inspection**: is there an
//! inverse, does the before-state parse, does it name columns the table has, is the changelog key
//! a row id. Those produce a refusal that names the offending entry, so they are worth asking
//! before anything is attempted, and they are cheap. They are **not** exhaustive and cannot be —
//! an inverse that is well-formed in every way `blocker` can see still has to be accepted by the
//! database, and a UNIQUE or CHECK constraint answers that only at apply time.
//!
//! So [`undo`] wraps the apply loop: **any** error out of [`invert_entry`] becomes the same
//! [`refusal`] — the transaction named, nothing changed (the enclosing `write_txn` rolls back,
//! which is why a `&WriteMeta` is required to get here at all), and the next older work
//! transaction to try instead. That covers constraint violations, unreadable payloads, and the
//! entries the `is_work`-filtered scan never pre-flighted at all. Every previous round of this
//! family tried to *predict* one more kind of unrunnable entry; this stops predicting, and a kind
//! nobody has taught this module about is a named refusal rather than a transaction that fails
//! identically for ever.
//!
//! ## …which requires every arm to report its result honestly
//!
//! The wrapper can only see failures that are *reported*. Several arms used to answer `Ok(0)` for
//! work they had not done — an `INSERT OR IGNORE` whose UNIQUE key an unlogged writer had
//! re-taken, an `UPDATE … WHERE rowid = ?` addressing a row since deleted, an arm handed no
//! before-state at all — and a lie there is worse than a raw error, because [`undo`] writes its
//! `undo` marker on the strength of it. The user is told "reverted 0 change(s)", exit 0; then
//! clearing the obstruction and re-running meets [`already_undone`], and the before-state is gone
//! for good.
//!
//! So: **a count that could not be taken is not spelled the same as a count of none** (the rule
//! `gitrepo::ahead_count` and `gitrepo::has_own_commits` were each corrected against). [`restored`]
//! is the one place a restoring statement's row count is judged, and a restore reports zero only
//! where a named condition says the row was deliberately not put back — in this module, exactly
//! one: [`restore_children`]'s `WHERE EXISTS` guards, skipping a row whose other endpoint is gone.
//! [`Inverse::DeleteRow`] is the only arm for which zero is honest without a guard, because the
//! state an insert's inverse promises is *absence*, which a `DELETE` matching nothing already has.
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
//! back to the columns they name. That is not a convention a writer has to remember: wherever
//! [`row_shaped`] says an entry's inverse is driven by a column map, [`crate::changelog`] refuses
//! at the *write* a before-state that is absent, empty, or names anything that is not a column of
//! the table ([`check_restorable`]) — so a payload like `{"content_len": 12}`, which reads as a
//! before-state and restores nothing, fails at its own writer instead of at somebody's later
//! `jkb undo`.
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
    /// that were there before — *is* the inverse. A **subset** of the table's columns, since a
    /// write that touched one column restores one column.
    Columns,
    /// Put a deleted row back, from the columns `before` names.
    ///
    /// The mirror image of [`Inverse::Columns`], driven by the same payload — but validated more
    /// strictly: a deleted row's before-state must name **every** column of its table, because
    /// there is no surviving row for the unnamed ones to keep their values in. See
    /// [`check_restorable`]. A plain `INSERT`, so a key some unlogged writer has re-taken is a
    /// refusal rather than a swallowed conflict — see [`reinsert_row`].
    ReinsertRow,
    /// Delete the inserted row by `rowid`.
    DeleteRow,
}

/// The generic inverse driven by a before-state made of column values, if this entry has one —
/// which is the question [`check_restorable`] validates and nothing else needs to ask.
fn row_shaped(op: &str, table: &str) -> Option<Inverse> {
    inverse_for(op, table).filter(|i| matches!(i, Inverse::Columns | Inverse::ReinsertRow))
}

/// The op an item delete is logged with, and the table it names — the one pair whose before-state
/// is a *cascade snapshot* rather than a plain column map.
const DELETE: &str = "delete";

/// The column map inside a before-state.
///
/// The payload itself, everywhere except an item delete: `item::remove` wraps the row's columns
/// in a cascade snapshot under `item`, beside the placements/tags/edges the cascade took. One
/// unwrapping rule, so the validator and the restorer cannot come to disagree about which object
/// the columns are in.
fn column_map<'a>(op: &str, table: &str, before: Option<&'a Value>) -> Option<&'a Value> {
    if is_cascade_snapshot(op, table) {
        return before.and_then(|b| b.get("item"));
    }
    before
}

/// Whether this entry's before-state is a **cascade snapshot** rather than a plain column map.
///
/// The one pair, asked in one place. [`column_map`] unwraps on it and [`check_restorable`]
/// dispatches on it, and those two disagreeing about which object holds the columns is a failure
/// this module has already had once — so they read the answer from here rather than each
/// re-spelling the test.
fn is_cascade_snapshot(op: &str, table: &str) -> bool {
    op == DELETE && table == Entity::Items.as_str()
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
    // THE ONE PAIR `row_shaped` DECLINES, CHECKED ANYWAY. An item delete's before-state is a
    // whole-cascade snapshot, so it is not a column map and `row_shaped` answers `None` — which
    // left `items`, the one table carrying a bespoke inverse, as the one table nothing validated.
    // The `item` object *inside* the snapshot is an ordinary column map, put back by the same
    // `reinsert_row` every other deleter uses, so it is held to the same completeness rule.
    // Without this, `items` had **two** hand-written 15-column lists (`item::snapshot`'s SELECT
    // and this module's restore) and the next column added to the table would have been dropped
    // by both, silently, on every restore.
    if is_cascade_snapshot(op, table) {
        return check_columns(conn, op, table, column_map(op, table, before), true);
    }
    let Some(inverse) = row_shaped(op, table) else {
        return Ok(());
    };
    check_columns(conn, op, table, before, inverse == Inverse::ReinsertRow)
}

/// The shared body: `before` must be a non-empty object naming only columns of `table`, and —
/// when `whole_row` — **every** column of it.
fn check_columns(
    conn: &Connection,
    op: &str,
    table: &str,
    before: Option<&Value>,
    whole_row: bool,
) -> Result<()> {
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
    // A DELETED ROW'S BEFORE-STATE MUST BE THE WHOLE ROW. An update leaves the row in place, so
    // the columns it does not name keep their values; a delete leaves nothing, so an unnamed
    // column is a value that cannot come back.
    //
    // The rule earns its place on the columns the reinsert does NOT complain about. `reinsert_row`
    // issues a plain `INSERT` (see there), so omitting a `NOT NULL` column with no default
    // (`branch_records.created_at`) raises and leaves through `undo`'s funnel as a named refusal —
    // loud, and no worse than this check. An omitted *nullable* column is the silent case: the row
    // comes back, reported as restored, holding the column's default in place of what was there.
    //
    // Checked against the schema rather than a list, so adding a column to a table makes every
    // deleter of it fail at its next write until the column is logged. That is the property a
    // per-writer assertion cannot have: it covers columns nobody has added yet.
    if whole_row {
        let missing: Vec<&str> = columns
            .iter()
            .filter(|c| !fields.contains_key(*c))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(TypeError::Validation(format!(
                "the before-state for this `{op}` on `{table}` does not name {} — undoing a \
                 delete re-inserts the row from exactly what is logged, so an unnamed column \
                 comes back as its default, or the whole row is silently not restored at all",
                missing.join(", ")
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
    Ok(column_map(op, table, before)
        .and_then(Value::as_object)
        .map(|fields| {
            fields
                .iter()
                .map(|(column, value)| (format!("\"{column}\""), as_sql(value)))
                .collect()
        })
        .unwrap_or_default())
}

/// The row count a **restoring** statement reported, or the error that a restore which restored
/// nothing is.
///
/// A count that could not be taken must not be spelled the same as a count of none — the rule
/// `gitrepo::ahead_count` and `gitrepo::has_own_commits` were each corrected against, here in
/// undo's arms. Every arm of [`invert_entry`] returns a row count, [`undo`] adds it to the number
/// it reports as reverted, and an arm that restored nothing while returning `Ok(0)` is claiming to
/// have done work it did not do. That lie is not merely a wrong number: **`undo` writes its
/// `undo` marker only when every inverse succeeded**, so a truthful failure is a named refusal the
/// user can retry after clearing whatever is in the way, while `Ok(0)` reports success, marks the
/// transaction undone, and makes the retry refuse with "it has already been undone" — the
/// before-state gone for good.
///
/// So a restore reports zero **only** where a named condition says the row was deliberately not
/// put back; every other zero comes through here. The one such condition in this module is an
/// endpoint that no longer exists (see [`restore_children`]), and it is expressed as a `WHERE
/// EXISTS` guard on the statement rather than as an `OR IGNORE` that would also hide a conflict.
fn restored(rows: usize, nothing_happened: &str) -> Result<usize> {
    if rows == 0 {
        return Err(
            TypeError::Validation(format!("it restored nothing — {nothing_happened}")).into(),
        );
    }
    Ok(rows)
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
    let fields = empty_is_a_refusal(restorable_fields(conn, op, table, before)?, op, table)?;
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
    // NO ROW, NO RESTORE. `UPDATE … WHERE rowid = ?` matching nothing means the row this entry
    // describes is gone — deleted by a later transaction, or an `items` row whose delete is being
    // undone in the wrong order — so the before-state was written nowhere. Reported as zero it was
    // indistinguishable from a restore that happened, and the marker written on the strength of it
    // made the retry refuse.
    restored(
        conn.prepare_cached(&sql)?.execute(params_from_iter(args))?,
        &format!(
            "no row of `{table}` has rowid {row} any more, so its previous values have \
                  nowhere to go — put that row back first, or revert the change that removed it"
        ),
    )
}

/// A before-state that named no column is a refusal, not an empty restore.
///
/// [`check_restorable`] has already refused an absent or empty before-state for every op/table
/// pair [`row_shaped`] covers, so this cannot fire for one of them — and an unreachable
/// `return Ok(0)` in its place is the same lie as any other: it says "restored, zero rows".
fn empty_is_a_refusal(
    fields: Vec<(String, SqlValue)>,
    op: &str,
    table: &str,
) -> Result<Vec<(String, SqlValue)>> {
    if fields.is_empty() {
        return Err(TypeError::Validation(format!(
            "the before-state logged for this `{op}` on `{table}` names no column of it, so there \
             is nothing to restore"
        ))
        .into());
    }
    Ok(fields)
}

/// Put a deleted row back, exactly as `before` describes it.
fn reinsert_row(conn: &Connection, op: &str, table: &str, before: Option<&Value>) -> Result<usize> {
    let Some(entity) = Entity::parse(table) else {
        return Err(TypeError::Validation(format!(
            "cannot restore a row of unknown table '{table}'"
        ))
        .into());
    };
    let fields = empty_is_a_refusal(restorable_fields(conn, op, table, before)?, op, table)?;
    // A PLAIN `INSERT`. This was `INSERT OR IGNORE`, on two rationales that were both false.
    //
    // *"so a row whose foreign keys are gone is skipped rather than failing the whole undo."*
    // `SQLite`'s ON CONFLICT clauses apply to UNIQUE, NOT NULL, CHECK and PRIMARY KEY constraints
    // and **not** to FOREIGN KEY constraints, so `OR IGNORE` never skipped a dangling-endpoint row
    // — it raised, exactly as a plain `INSERT` does. Verified by execution, not by reading. What
    // does skip such a row is `restore_children`'s explicit `WHERE EXISTS` guards.
    //
    // *"so a re-run is not an error."* A completed undo is refused by `already_undone`; a
    // rolled-back one left nothing behind to collide with.
    //
    // What `OR IGNORE` did do was swallow the case this whole module exists for: an unlogged
    // writer re-taking a UNIQUE key (`ns::ensure` re-creating a deleted `namespaces.path`,
    // `apply_doc` re-placing a placement, `jkb task depend` re-linking an edge). The conflict
    // vanished, the arm answered `Ok(0)`, the CLI printed "reverted 0 change(s)" and exited 0 —
    // and then the marker was written, so clearing the conflict and re-running met "it has already
    // been undone" with the row's `metadata` — its namespace type, a file's sync structure, a
    // repo's gate — gone for good. Now the conflict leaves through `undo`'s funnel as a named
    // refusal, nothing is marked, and the retry works.
    let sql = format!(
        "INSERT INTO \"{}\" ({}) VALUES ({})",
        entity.as_str(),
        fields
            .iter()
            .map(|(c, _)| c.clone())
            .collect::<Vec<_>>()
            .join(", "),
        vec!["?"; fields.len()].join(", ")
    );
    // Not wrapped in `restored`: an unqualified `INSERT … VALUES` either inserts its one row or
    // raises, so a zero here is not a state. It is `OR IGNORE` that makes zero reachable, which is
    // the whole reason it is gone.
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
    // `tag::rename_facet` rewrites `tag_defs.facet` and every application's, one logged entry per
    // row. Before that it logged one entry keyed on the facet *name* — not a row id, describing
    // none of the applications — so `jkb tag rename` left every later bare `jkb undo` refusing
    // that transaction, with no surface printing a txn id to escape it with.
    ("update", Some("tag_defs"), Inverse::Columns),
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
        // ZERO IS THE ANSWER FOR BOOKKEEPING, AND ONLY FOR IT. The one member that gets here is
        // the `undo` marker itself: undoing an undo is not something this module does, and there
        // is no work behind the entry to leave un-restored. A *work* entry with no inverse is
        // `blocker`'s refusal, so reaching here with one would mean the scan and the dispatch
        // disagree about `INVERSES` — which is the "reverted 0 change(s)" that left the row you
        // asked about untouched, back again one layer down.
        return if is_work(op, table) {
            Err(TypeError::Validation(format!(
                "no inverse for `{op}` on `{table}`, and the pre-flight did not catch it"
            ))
            .into())
        } else {
            Ok(0)
        };
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
    // AN ABSENT BEFORE-STATE IS A REFUSAL, NOT AN EMPTY RESTORE. Four arms used to read it with
    // `if let Some(snap)`, so an entry logged without one restored nothing and answered `Ok(0)` —
    // the same lie as a swallowed conflict, and the more dangerous one on the three arms
    // `check_restorable` does not cover (`edges`, `mounts`, `containment` are not `row_shaped`,
    // so nothing validates their payloads at the write). Unreachable today, because every one of
    // those entries is written by `changelog::upsert`, which derives `update` from a before-state
    // being present — but "unreachable" is what the `OR IGNORE` conflict was assumed to be too.
    let required = |what: &str| -> Result<Value> {
        snapshot(what)?.ok_or_else(|| {
            Error::from(TypeError::Validation(format!(
                "reversing `{op}` on `{table}` needs the {what} it was logged with, and this entry \
                 has none"
            )))
        })
    };
    match inverse {
        // An item delete is inverted by putting the snapshot back (see `restore_item`).
        Inverse::ItemSnapshot => {
            rows += restore_item(conn, &required("item snapshot")?)?;
        }
        // An edge weight update is inverted by putting the previous weight back. Without
        // this the edge would keep whatever weight the undone transaction gave it.
        Inverse::EdgeWeight => {
            rows += restore_edge_weight(conn, rowid()?, &required("edge before-state")?)?;
        }
        Inverse::SyncStateRow => {
            rows += revert_sync_state(conn, entity_id, snapshot("sync journal before-state")?)?;
        }
        // A mount edit is inverted by putting the previous configuration back. `jkb mount
        // create` doubles as the update command, so without this the generic insert
        // inverse would `DELETE FROM mounts` and destroy a mount that existed before the
        // transaction, leaving its `file://` bindings with nothing to sync them.
        Inverse::MountConfig => {
            rows += restore_mount_config(conn, rowid()?, &required("mount before-state")?)?;
        }
        // Re-parenting is an update, not an insert (`containment::contain` upserts on the
        // child), so the generic inverse would delete a row that existed beforehand and
        // un-parent the item instead of putting its previous container back.
        Inverse::ContainmentRow => {
            rows +=
                restore_containment_row(conn, rowid()?, &required("containment before-state")?)?;
        }
        // Every write that changed an existing row, for the `(op, table)` pairs `INVERSES` maps
        // here: put the columns named in `before` back. Nothing is hand-written per statement,
        // so a new `UPDATE items SET …` gains an inverse by supplying its before-state — which
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
        // THE ONE ARM WHERE ZERO IS AN HONEST ANSWER, and the condition is worth naming because
        // every other arm here now refuses one. The inverse of an insert is *absence*: a `DELETE`
        // matching no row means the row this transaction created is already gone — dropped by a
        // later `jkb item rm`, or cascaded away with its parent — so the state this arm promises
        // is the state that already holds. Every restoring arm promises the opposite (a row with
        // particular values *present*), which no count of zero can be evidence of.
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

/// Put an edge's previous weight back on the row `row` identifies.
fn restore_edge_weight(conn: &Connection, row: i64, snap: &Value) -> Result<usize> {
    restored(
        conn.prepare_cached("UPDATE edges SET weight = ?2 WHERE rowid = ?1")?
            .execute(params![row, snap.get("weight").and_then(Value::as_f64)])?,
        &format!("edge {row} is gone, so its previous weight has nowhere to go"),
    )
}

/// Put a mount's previous configuration back on the namespace `row` identifies.
fn restore_mount_config(conn: &Connection, row: i64, snap: &Value) -> Result<usize> {
    let field = |k: &str| snap.get(k).and_then(Value::as_str).map(str::to_owned);
    restored(
        conn.prepare_cached(
            "UPDATE mounts
                SET backing_uri = ?2, sync_mode = ?3, serializer = ?4,
                    include_glob = ?5, exclude_glob = ?6, conflict_policy = ?7
              WHERE namespace_id = ?1",
        )?
        .execute(params![
            row,
            field("backing_uri"),
            field("sync_mode"),
            field("serializer"),
            field("include_glob"),
            field("exclude_glob"),
            field("conflict_policy"),
        ])?,
        &format!(
            "namespace {row} has no mount any more, so its previous configuration has nowhere to go"
        ),
    )
}

/// Put an item's previous container and position back on the containment row keyed by `row`.
fn restore_containment_row(conn: &Connection, row: i64, snap: &Value) -> Result<usize> {
    restored(
        conn.prepare_cached(
            "UPDATE containment SET parent_item_id = ?2, position = ?3 WHERE child_item_id = ?1",
        )?
        .execute(params![
            row,
            snap.get("parent_item_id").and_then(Value::as_i64),
            snap.get("position").and_then(Value::as_i64).unwrap_or(0),
        ])?,
        &format!(
            "item {row} is contained by nothing now, so its previous container has nowhere to go"
        ),
    )
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
        //
        // Zero rows is a lie here as much as on the restoring arm below: nothing in this codebase
        // deletes a `sync_state` row (`grep -n "DELETE FROM sync_state" crates` is empty), so a
        // uri with no row means the journal entry names a file the journal never had — and
        // clearing nothing leaves the base still describing items this undo is deleting, which is
        // precisely the item-less export the paragraph above exists to prevent.
        return restored(
            conn.prepare_cached(
                "UPDATE sync_state
                    SET last_synced_hash = NULL, base_blob_hash = NULL, document = NULL
                  WHERE uri = ?1",
            )?
            .execute(params![entity_id])?,
            &format!("the sync journal has no row for `{entity_id}`, so its base was not cleared"),
        );
    };
    let field = |k: &str| snap.get(k).and_then(Value::as_str).map(str::to_owned);
    restored(
        conn.prepare_cached(
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
        ])?,
        &format!("the sync journal has no row for `{entity_id}` to restore"),
    )
}

/// Why this entry's inverse could not be **run**, judged by inspection alone — or `None` if
/// nothing visible from here stops it.
///
/// It used to ask only whether an inverse *existed*, which is half the question: an entry written
/// under the old, unvalidated contract — `item::set_content`'s `{"content_len": 12}`,
/// `binding::mark_synced`'s absent before-state — has an inverse, passes an existence check, and
/// then dies inside [`restore_columns`]. Asking here instead turns that into a refusal that names
/// the entry and its reason, which is worth having and is why this function exists.
///
/// **It is deliberately not exhaustive, and must not be made to try.** Whether the database will
/// accept a well-formed inverse is knowable only by running it — a restored `tag_defs.facet`
/// whose old name an unlogged `tag::define_facet` has since re-taken violates a UNIQUE index, and
/// no amount of payload inspection sees that coming. What makes refusal *total* is [`undo`]'s
/// wrapper around [`invert_entry`], not this scan; this one only makes the common cases read
/// better. Adding a predicted constraint check here would be the fifth round of predicting.
fn blocker(
    conn: &Connection,
    op: &str,
    table: &str,
    entity_id: &str,
    before: Option<&str>,
) -> Option<String> {
    let Some(inverse) = inverse_for(op, table) else {
        return Some("no inverse for it".to_owned());
    };
    if Entity::parse(table).is_none() {
        return Some(format!("`{table}` is not a table this build knows"));
    }
    let snapshot = match before.map(serde_json::from_str::<Value>).transpose() {
        Ok(snapshot) => snapshot,
        Err(e) => return Some(format!("its before-state is not readable JSON: {e}")),
    };
    // Covers what `row_shaped` declines to shape as well as what it accepts — the item-snapshot
    // arm included, since `check_restorable` now dispatches on the pair rather than on the shape.
    if let Err(e) = check_restorable(conn, op, table, snapshot.as_ref()) {
        return Some(e.to_string());
    }
    // The inverses that address a row by `rowid`. `SyncStateRow`'s key is a file uri and
    // `ItemSnapshot`/`ReinsertRow` take their key from the payload, so neither is asked for one.
    if matches!(
        inverse,
        Inverse::Columns
            | Inverse::DeleteRow
            | Inverse::EdgeWeight
            | Inverse::MountConfig
            | Inverse::ContainmentRow
    ) && entity_id.parse::<i64>().is_err()
    {
        return Some(format!("its changelog key `{entity_id}` is not a row id"));
    }
    None
}

/// A refusal that a reader can act on: what is wrong, that nothing moved, and **which
/// transaction to try instead**.
///
/// The escape matters because `jkb undo <txn>` is the only way past a refusal and *nothing in the
/// CLI prints a transaction id* — `grep -n txn_id crates/jkb-cli/src` is empty — while raw
/// `sqlite3` against a jkb database is hook-blocked. Without the id, one `jkb tag rename` made
/// every later bare `jkb undo` refuse the same transaction with no way for the user to reach
/// anything older.
fn refusal(conn: &Connection, txn_id: i64, why: &str) -> Result<String> {
    let escape = match select_work_txn(conn, txn_id)? {
        Some(older) => format!(
            " The next older change is transaction {older} — `jkb undo {older}` reverts that one \
             instead."
        ),
        None => " There is no older change to revert instead.".to_owned(),
    };
    Ok(format!(
        "transaction {txn_id} cannot be undone: {why}. Nothing was changed.{escape}"
    ))
}

/// Where undo history begins — the newest transaction that existed when `V014` ran, or 0.
///
/// Everything at or below it was written under the **audit** contract, before
/// [`check_restorable`] held a writer to the *undo* contract, so its payloads were never
/// required to be able to restore anything. See `V014__undo_watermark.sql`: this is a date line
/// rather than a per-entry inference precisely because inferring whether a payload nobody
/// designed to be inverted happens to be invertible is the same mistake one level along.
fn watermark(conn: &Connection) -> Result<i64> {
    Ok(conn
        .prepare_cached("SELECT from_txn FROM undo_watermark WHERE id = 1")?
        .query_row([], |row| row.get(0))
        .optional()?
        .unwrap_or(0))
}

/// "This transaction has already been undone", as `SQL`, spelled **once**.
///
/// Two callers need it against different operands — [`select_work_txn`] correlates it with each
/// row it scans, [`already_undone`] asks it of one bound id — so the operand is the only hole,
/// and it is an identifier or a placeholder this module writes, never a value. Written out twice
/// the copies would answer differently, and the answer decides whether `DeleteRow` runs a second
/// time against row ids `SQLite` has since reissued.
fn undone_sql(operand: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM changelog u
                  WHERE u.op = 'undo' AND u.entity_id = CAST({operand} AS TEXT))"
    )
}

/// Whether `txn_id` already has an `undo` marker against it.
fn already_undone(conn: &Connection, txn_id: i64) -> Result<bool> {
    Ok(conn
        .prepare_cached(&format!("SELECT {}", undone_sql("?1")))?
        .query_row([txn_id], |row| row.get(0))?)
}

/// The newest transaction containing **work**, below `below`, above the watermark, not already
/// undone — or `None`.
///
/// One implementation, two callers: [`undo_last`]'s selection and [`refusal`]'s escape hatch.
/// They must agree, because the escape is an instruction to run the command the selection would
/// have run had the refused transaction not been in the way.
fn select_work_txn(conn: &Connection, below: i64) -> Result<Option<i64>> {
    work_txn_after(conn, below, watermark(conn)?)
}

/// The same selection with the lower bound stated rather than taken from the [`watermark`].
///
/// The only caller that passes anything else is [`undo_last`], asking one question the watermark
/// makes unanswerable otherwise: *is there nothing to undo, or is everything there is below the
/// line?* Both come back as `None` from [`select_work_txn`], and "reverted 0 change(s)" on a
/// database with thousands of transactions reads as an empty one.
fn work_txn_after(conn: &Connection, below: i64, after: i64) -> Result<Option<i64>> {
    // Bookkeeping-only transactions are skipped, and nothing else is. The clauses are generated
    // from `BOOKKEEPING`, the same list `is_work` answers from, and every op and table name is
    // *bound* rather than interpolated — so this stays a parameterized query and cannot come to
    // disagree with the predicate about which entries are the user's work.
    let mut args: Vec<SqlValue> = Vec::with_capacity(BOOKKEEPING.len() * 2 + 2);
    for (op, table) in BOOKKEEPING {
        args.push(SqlValue::Text((*op).to_owned()));
        args.push(SqlValue::Text((*table).to_owned()));
    }
    args.push(SqlValue::Integer(below));
    args.push(SqlValue::Integer(after));
    let sql = format!(
        "SELECT MAX(txn_id) FROM changelog c
          WHERE {}
            AND c.txn_id < ?
            AND c.txn_id > ?
            AND NOT {}",
        BOOKKEEPING
            .iter()
            .map(|_| "NOT (c.op = ? AND c.entity_type = ?)".to_owned())
            .collect::<Vec<_>>()
            .join(" AND "),
        undone_sql("c.txn_id")
    );
    Ok(conn
        .prepare_cached(&sql)?
        .query_row(params_from_iter(args), |row| row.get(0))
        .optional()?
        .flatten())
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
    let mark = watermark(conn)?;
    if txn_id <= mark {
        return Err(TypeError::Validation(refusal(
            conn,
            txn_id,
            &format!(
                "it predates undo history, which begins after transaction {mark}. Entries older \
                 than that were written before `jkb undo` required a before-state that could \
                 restore anything, so reversing one would restore some of it and silently drop \
                 the rest"
            ),
        )?)
        .into());
    }
    // ONCE, AND THE SELECTION ALREADY KNEW IT. `select_work_txn` has never handed back a
    // transaction that has been undone; this form did not ask, so re-running `jkb undo <txn>`
    // re-executed the whole inversion — and `DeleteRow` addresses a row id, which `SQLite`
    // reissues for every table here except `items` (`AUTOINCREMENT` since D40). The second run
    // therefore unplaced, untagged or unlinked whatever now holds that id. It became reachable
    // when refusals started printing transaction ids and recommending the explicit form.
    if already_undone(conn, txn_id)? {
        return Err(TypeError::Validation(refusal(
            conn,
            txn_id,
            "it has already been undone; reversing it a second time would address row ids that \
             belong to other rows now",
        )?)
        .into());
    }
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

    let blocked: Vec<String> = entries
        .iter()
        .filter(|(op, table, _, _)| is_work(op, table))
        .filter_map(|(op, table, entity_id, before)| {
            blocker(conn, op, table, entity_id, before.as_deref())
                .map(|why| format!("`{op}` on `{table}` ({why})"))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !blocked.is_empty() {
        return Err(TypeError::Validation(refusal(
            conn,
            txn_id,
            &format!(
                "`jkb undo` cannot reverse {}, and reversing the rest would leave the change \
                 half undone",
                blocked.join(", ")
            ),
        )?)
        .into());
    }

    // THE BACKSTOP THAT MAKES REFUSAL TOTAL. `blocker` above answers the questions that can be
    // answered by inspection; whether the database will *accept* an inverse is not one of them.
    // A well-formed `UPDATE tag_defs SET facet = 'size' WHERE rowid = ?` is refused by
    // `tag_defs.facet` when an unlogged `tag::define_facet` has re-taken the name since, and the
    // same shape exists on `namespaces.path` — an unlogged writer re-taking a UNIQUE value a
    // logged `Inverse::Columns` restore writes back. As a bare `?` that was a raw `SQLite` error
    // with no transaction id, no statement that nothing had moved and nothing to try instead;
    // and because it writes no `undo` marker the transaction is re-selected by the next
    // `jkb undo`, and the next, for ever.
    //
    // So every apply-time failure — UNIQUE, CHECK, foreign key, an unreadable payload, anything a
    // future inverse can hit — leaves through one funnel and reads as the refusal the pre-flight
    // would have produced. This covers the entries the scan never looked at as well: `blocker`
    // runs behind an `is_work` filter, so `sync_state` reaches `invert_entry` unexamined.
    // Extending `blocker` to predict constraint violations instead would mean re-implementing
    // the constraint checker, which is the prediction game the previous four rounds lost.
    let mut reverted = 0;
    for (op, table, entity_id, before) in entries {
        match invert_entry(conn, &op, &table, &entity_id, before.as_deref()) {
            Ok(rows) => reverted += rows,
            Err(e) => {
                let why = format!("reversing `{op}` on `{table}` failed: {e}");
                // …and the funnel does not depend on the connection still being usable. If the
                // failure aborted the transaction, `refusal`'s own lookup for the escape hatch
                // fails too; say what happened plainly rather than replacing the original error
                // with a second one about the query that tried to describe it.
                let message = refusal(conn, txn_id, &why).unwrap_or_else(|_| {
                    format!("transaction {txn_id} cannot be undone: {why}. Nothing was changed.")
                });
                return Err(TypeError::Validation(message).into());
            }
        }
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
/// The item is inserted **first** so the children's foreign keys resolve. An edge or containment
/// row whose other endpoint was itself deleted and not restored simply cannot come back, and
/// skipping it is better than failing the whole undo — which is what the `WHERE EXISTS` guards in
/// [`restore_children`] are for, and they are the only sanctioned way a restore here reports zero.
fn restore_item(conn: &Connection, snapshot: &Value) -> Result<usize> {
    let item = snapshot
        .get("item")
        .ok_or_else(|| TypeError::Validation("item snapshot has no `item`".to_owned()))?;
    let id = item
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| TypeError::Validation("item snapshot has no `id`".to_owned()))?;

    // Driven by the snapshot's own keys, through the same `reinsert_row` every other deleter
    // uses. It was a hand-written 15-column list, which made `items` the only table with **two**
    // of them — one here and one in `item::snapshot` — and the next column added to the schema
    // would have been dropped by both on every restore, in silence. Now the columns come from
    // the payload and `check_restorable` requires the payload to name all of them.
    let mut put_back = reinsert_row(conn, DELETE, Entity::Items.as_str(), Some(snapshot))?;

    put_back += restore_children(conn, snapshot, id)?;
    Ok(put_back)
}

/// Restore the rows that `ON DELETE CASCADE` took with an item: its placements, tag
/// applications, edges, containment and binding. Split out of `restore_item` so each table's
/// column list stays readable.
///
/// **The `WHERE EXISTS` guards are the one sanctioned way a restore in this module reports zero**,
/// and they say exactly what they skip: a row whose *other* endpoint is not there to point at.
/// Two items removed in one transaction produce two snapshots naming the same edge, and whichever
/// is restored first cannot have it — so the guard is also what stops the second restore
/// duplicating the first's row.
///
/// Every insert here was `OR IGNORE`, on the rationale that "re-running an undo must not fail on
/// rows a previous attempt already put back". A *completed* undo is refused by [`already_undone`]
/// and a rolled-back one left nothing behind, so what the clause actually did was hide a UNIQUE
/// conflict as a silent zero — and it never did the foreign-key job the edge comment credited it
/// with, because `SQLite`'s ON CONFLICT clauses do not apply to FOREIGN KEY constraints at all.
/// Plain `INSERT`s now, so a conflict leaves through [`undo`]'s funnel as a named refusal.
fn restore_children(conn: &Connection, snapshot: &Value, id: i64) -> Result<usize> {
    let rows = |key: &str| -> Vec<&Value> {
        snapshot
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    };
    let mut put_back = 0;

    for placement in rows("placements") {
        put_back += conn
            .prepare_cached(
                "INSERT INTO placements (item_id, namespace_id, role, position, metadata)
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
        put_back += conn
            .prepare_cached(
                "INSERT INTO tag_applications (item_id, facet, value, props)
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
        // The `EXISTS` guards skip an edge whose other endpoint is gone — better than failing the
        // whole undo on a foreign key. They, not the removed `OR IGNORE`, are what does that: an
        // ON CONFLICT clause never applied to a FOREIGN KEY constraint.
        put_back += conn
            .prepare_cached(
                "INSERT INTO edges
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
    // contained are one each. The `EXISTS` guard skips a row whose other endpoint is gone,
    // exactly as the edges above do.
    if let Some(c) = snapshot.get("contained_by").filter(|c| !c.is_null()) {
        put_back += conn
            .prepare_cached(
                "INSERT INTO containment (child_item_id, parent_item_id, position)
                 SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM items WHERE id = ?2)",
            )?
            .execute(params![
                id,
                c.get("parent_item_id").and_then(Value::as_i64),
                c.get("position").and_then(Value::as_i64).unwrap_or(0),
            ])?;
    }
    for c in rows("contains") {
        put_back += conn
            .prepare_cached(
                "INSERT INTO containment (child_item_id, parent_item_id, position)
                 SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM items WHERE id = ?1)",
            )?
            .execute(params![
                c.get("child_item_id").and_then(Value::as_i64),
                id,
                c.get("position").and_then(Value::as_i64).unwrap_or(0),
            ])?;
    }
    if let Some(binding) = snapshot.get("binding").filter(|b| !b.is_null()) {
        put_back += conn
            .prepare_cached(
                "INSERT INTO bindings
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
    Ok(put_back)
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
/// **The claim is about transactions that logged something, and only those.** A mutation that
/// writes no changelog entry at all is invisible here, so `jkb undo` still reaches past it and
/// reverts the change before it. `ns::ensure` (`jkb ns mk`) is the live example — it takes no
/// [`WriteMeta`] and logs nothing — and the ingest embed stage and `jkb index --sweep` share the
/// shape. That is a gap in what the *writers* record, not in this selection, and it is filed
/// rather than fixed here.
///
/// # Errors
/// Propagates any error from [`undo`], including its refusal; and says so rather than answering
/// 0 when there is work but all of it is at or below the [`watermark`].
pub fn undo_last(conn: &Connection, meta: &WriteMeta) -> Result<usize> {
    if let Some(txn_id) = select_work_txn(conn, meta.txn_id)? {
        return undo(conn, meta, txn_id);
    }
    // NOTHING TO UNDO, OR NOTHING UNDOABLE? Both are `None` above, and they are not the same
    // news: on a database whose whole history predates `V014`, "reverted 0 change(s)" is exactly
    // what an empty database prints. The difference between the two answers *is* the watermark,
    // so the same selection is re-run without it. No `mark > 0` short-circuit: with the watermark
    // at 0 the two queries are the same query, so it could never change the answer — and a
    // condition that cannot fire is a second model of the world rather than a saving.
    let mark = watermark(conn)?;
    if work_txn_after(conn, meta.txn_id, 0)?.is_some() {
        return Err(TypeError::Validation(format!(
            "nothing to undo: undo history begins after transaction {mark}, and every change on \
             this database that has not already been reverted was recorded before that. Entries \
             written then were not required to carry a before-state that could restore anything, \
             so reversing one would restore some of it and silently drop the rest"
        ))
        .into());
    }
    Ok(0)
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

    /// Undoing a **re-home** puts the item back in the namespace it came from, at its old
    /// position.
    ///
    /// `placement::set_primary` deletes the old primary and inserts the new one, so this exercises
    /// the delete inverse and the column restore in one transaction. The position matters: a
    /// placement put back in the right namespace at the wrong ordinal is a different placement,
    /// and it is what `jkb ls` orders by.
    #[test]
    fn undoing_a_re_home_restores_the_previous_placement() {
        use crate::{ns, placement};

        let db = Db::open_in_memory().unwrap();
        let id = db.write_txn("t", |c, m| upsert(c, m, &note("a"))).unwrap();
        let (from, to) = db
            .write_txn("t", |c, _| {
                Ok((ns::ensure(c, "x/from")?, ns::ensure(c, "x/to")?))
            })
            .unwrap();
        db.write_txn("t", move |c, m| placement::set_primary(c, m, id, from, 7))
            .unwrap();
        db.write_txn("t", move |c, m| placement::set_primary(c, m, id, to, 0))
            .unwrap();

        db.write_txn("t", undo_last).unwrap();
        let placed: Vec<(i64, i64)> = db
            .read(move |c| {
                let mut stmt = c.prepare(
                    "SELECT namespace_id, position FROM placements
                      WHERE item_id = ?1 AND role = 'primary'",
                )?;
                let rows = stmt.query_map([id.get()], |r| Ok((r.get(0)?, r.get(1)?)))?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .unwrap();
        assert_eq!(
            placed,
            vec![(from.get(), 7)],
            "the re-home was not undone: the item should be back in its old namespace, at the \
             position it had"
        );
    }

    /// Every row of `table`, whole, in a form two dumps can be compared with.
    ///
    /// `SELECT *`, deliberately: the comparison then covers a column added to the schema later
    /// without anybody remembering to extend it — which is the failure mode a hand-written list of
    /// expected fields has, and the one this family keeps producing.
    fn dump(db: &Db, table: &'static str) -> Vec<Vec<String>> {
        db.read(move |c| {
            let mut stmt = c.prepare(&format!("SELECT * FROM \"{table}\" ORDER BY rowid"))?;
            let width = stmt.column_count();
            let rows = stmt.query_map([], |r| {
                (0..width)
                    .map(|i| {
                        r.get::<_, rusqlite::types::Value>(i)
                            .map(|v| format!("{v:?}"))
                    })
                    .collect::<std::result::Result<Vec<_>, _>>()
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .unwrap()
    }

    /// Run `delete` and undo it, asserting `table` comes back **byte for byte**.
    ///
    /// The `assert_ne` is not decoration: without it a setup that deleted nothing would pass this
    /// trivially, which is exactly how a round-trip test goes vacuous.
    fn delete_round_trips(
        db: &Db,
        table: &'static str,
        delete: impl FnOnce(&rusqlite::Connection, &crate::WriteMeta) -> crate::Result<()>
            + Send
            + 'static,
    ) {
        let before = dump(db, table);
        db.write_txn("t", move |c, m| delete(c, m)).unwrap();
        assert_ne!(
            dump(db, table),
            before,
            "setup: nothing was actually deleted from `{table}`"
        );
        db.write_txn("t", undo_last).unwrap();
        assert_eq!(
            dump(db, table),
            before,
            "undoing a delete from `{table}` did not restore the row exactly as it was"
        );
    }

    /// **Every** deleter's before-state round-trips its whole row.
    ///
    /// One assertion shape for all five, comparing `SELECT *` before and after rather than the
    /// columns whoever wrote the test happened to think of. That matters because the gap it closes
    /// was precisely a *thought-of* column list: `tag::remove`'s `props`, `placement::unplace`'s
    /// `position` and `metadata`, `ns::remove`'s `kind`, `edge::unlink`'s `props` and
    /// `branch::forget`'s `created_at` were all added to the log this round and none of them was
    /// asserted anywhere — each could be poisoned to a wrong value with the whole workspace green.
    ///
    /// It also needs no row generators, which is what makes it proportionate: the rows are built
    /// by the ordinary repo API, so they are valid by construction, and `SELECT *` extends the
    /// comparison to any column added later. `check_restorable` covers the other half — a column
    /// *omitted* from the before-state, which this could only catch when the fixture happens to
    /// give it a non-default value.
    #[test]
    fn undoing_a_delete_restores_the_whole_row_for_every_deleter() {
        use crate::{branch, edge, ns, placement, tag};
        use jkb_types::{EdgeType, PlacementRole};
        use serde_json::json;

        let db = Db::open_in_memory().unwrap();
        let (item, other, ns_id) = db
            .write_txn("t", |c, m| {
                let item = upsert(c, m, &note("a"))?;
                let other = upsert(c, m, &note("b"))?;
                let ns_id = ns::ensure(c, "x/y")?;
                Ok((item, other, ns_id))
            })
            .unwrap();

        // Each row is given a value that is NOT the column's default, so a before-state that
        // omits or fabricates the column cannot restore it by luck.
        db.write_txn("t", move |c, m| {
            tag::apply(c, m, item, "size", "small")?;
            placement::place(c, m, item, ns_id, PlacementRole::Reference, 5)?;
            edge::link_weighted(
                c,
                m,
                item,
                other,
                EdgeType::Supports,
                Some(2.5),
                Some(&json!({ "why": "evidence" })),
            )?;
            branch::record_cut_point(
                c,
                m,
                "jkb",
                "task/x",
                &branch::Cut::Fork("a".repeat(40)),
                Some(&branch::Anchor {
                    sha: "b".repeat(40),
                    ts: 7,
                }),
                branch::Supersede::default(),
            )?;
            ns::ensure(c, "x/doomed")?;
            Ok(())
        })
        .unwrap();

        delete_round_trips(&db, "tag_applications", move |c, m| {
            tag::remove(c, m, item, "size", "small")
        });
        delete_round_trips(&db, "placements", move |c, m| {
            placement::unplace(c, m, item, ns_id).map(|_| ())
        });
        delete_round_trips(&db, "edges", move |c, m| {
            edge::unlink(c, m, item, other, EdgeType::Supports)
        });
        delete_round_trips(&db, "namespaces", |c, m| ns::remove(c, m, "x/doomed"));
        delete_round_trips(&db, "branch_records", |c, m| {
            branch::forget(c, m, "jkb", "task/x").map(|_| ())
        });
    }

    /// Undoing an edit to an item's body restores the body **and its hash**.
    ///
    /// `item::set_content` is the one column write this round changed whose before-state nothing
    /// read back: logging `{"content": "anything at all"}` passed validation, restored garbage,
    /// and left the whole workspace green. Both columns are asserted because restoring the content
    /// while leaving `content_hash` describing the replacement makes the item its own mismatch —
    /// which is worse than not undoing at all, since `content_hash` is globally unique and is what
    /// ingest dedups on.
    #[test]
    fn undoing_a_content_edit_restores_the_body_and_its_hash() {
        use crate::item;

        let db = Db::open_in_memory().unwrap();
        let id = db
            .write_txn("t", |c, m| {
                upsert(
                    c,
                    m,
                    &NewItem {
                        uid: "doc".to_owned(),
                        kind: "document".to_owned(),
                        content: Some("the original body".to_owned()),
                        content_hash: Some("b3:original".to_owned()),
                        mime: None,
                    },
                )
            })
            .unwrap();
        db.write_txn("t", move |c, m| {
            item::set_content(c, m, id, "a replacement body", Some("b3:replacement"))
        })
        .unwrap();

        db.write_txn("t", undo_last).unwrap();
        let meta = db
            .read(move |c| item::get(c, id))
            .unwrap()
            .expect("the item survives");
        assert_eq!(
            (meta.content.as_deref(), meta.content_hash.as_deref()),
            (Some("the original body"), Some("b3:original")),
            "undoing a content edit did not restore the body and the hash it was logged with"
        );
    }

    /// A namespace's `metadata` and a binding's sync stamp both come back — the two writers that
    /// recorded **no** before-state at all.
    ///
    /// Neither is decorative: a file sync writes both on every reconcile, so with either one
    /// uninvertible `jkb undo` after a sync is refused outright, and before the refusal existed it
    /// silently reverted an older transaction instead.
    #[test]
    fn undoing_a_metadata_write_and_a_sync_stamp_restores_what_each_replaced() {
        use crate::{binding, ns};
        use serde_json::json;

        let db = Db::open_in_memory().unwrap();
        let id = db.write_txn("t", |c, m| upsert(c, m, &note("a"))).unwrap();
        let nsid = db.write_txn("t", |c, _| ns::ensure(c, "x/y")).unwrap();
        db.write_txn("t", move |c, m| {
            ns::set_metadata(c, m, nsid, &json!({ "layout": "first" }))?;
            binding::set(c, m, id, "file:///tmp/a.md", None, None)?;
            binding::mark_synced(c, m, id, "hash-one")
        })
        .unwrap();
        db.write_txn("t", move |c, m| {
            ns::set_metadata(c, m, nsid, &json!({ "layout": "second" }))?;
            binding::mark_synced(c, m, id, "hash-two")
        })
        .unwrap();

        db.write_txn("t", undo_last).unwrap();
        assert_eq!(
            db.read(move |c| ns::get_metadata(c, nsid)).unwrap(),
            Some(json!({ "layout": "first" })),
            "undoing a metadata write did not restore the JSON it replaced"
        );
        assert_eq!(
            db.read(move |c| binding::get(c, id))
                .unwrap()
                .and_then(|b| b.last_synced_hash),
            Some("hash-one".to_owned()),
            "undoing a sync stamp left the binding claiming it had settled on bytes the undo threw \
             away"
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

    /// A before-state that could not restore the row fails **at the writer**, not at a later undo.
    ///
    /// Three ways to get it wrong, all silent if they reach the log. `item::set_content` logged
    /// `{"content_len": 12}` — a payload that reads like a before-state and names no column, so
    /// the inverse would have run, restored nothing and reported success. An absent one is the
    /// same thing one step further along. And a *deleted* row logged with only some of its columns
    /// is worse still, because there is no surviving row for the rest to keep their values in.
    ///
    /// This is the property that makes the generic inverses safe to apply without a hand-written
    /// statement per writer.
    #[test]
    fn a_before_state_that_could_not_restore_the_row_is_refused_at_the_writer() {
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

        // A DELETED row's before-state must be the WHOLE row, not merely column-shaped: an
        // update leaves the row in place so its unnamed columns keep their values, but a delete
        // leaves nothing behind for them to keep. Refused by naming the missing column, because
        // the reinsert is a plain `INSERT`: a missing `NOT NULL` column raises loudly, but a
        // missing *nullable* one restores the row with the column's default silently substituted
        // for the value that was there, and reports success.
        let err = db
            .write_txn("t", |c, m| {
                changelog::append(
                    c,
                    m,
                    "delete",
                    Entity::TagApplications,
                    "1",
                    Some(&serde_json::json!({
                        "item_id": 1, "facet": "size", "value": "small",
                    })),
                    None,
                )
            })
            .expect_err("a deleted row was logged without every column of its table");
        assert!(
            err.to_string().contains("does not name props"),
            "the refusal does not name the column that would not come back: {err}"
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

    /// An entry whose inverse **exists but cannot run** is a named refusal, not a mid-apply error.
    ///
    /// The scan used to ask only whether an inverse existed. A payload written under the old,
    /// audit-only contract — `item::set_content`'s `{"content_len": 12}` is the real one — has an
    /// inverse, passes an existence check, and then dies inside `restore_columns` with the
    /// writer's own validation message: no transaction id, no statement that nothing moved, and
    /// no way onward, since **nothing in the CLI prints a txn id** and raw `sqlite3` against a jkb
    /// database is hook-blocked. The entry is written straight into the log here because
    /// `changelog::write` now refuses it at the door: what is being pinned is what `undo` does
    /// with the rows already in the table.
    #[test]
    fn an_entry_whose_inverse_cannot_run_is_refused_by_name_with_a_way_onward() {
        use crate::item;

        let db = Db::open_in_memory().unwrap();
        let older = db
            .write_txn("t", |c, m| {
                upsert(c, m, &note("keep-me"))?;
                Ok(m.txn_id)
            })
            .unwrap();
        let bad = db
            .write_txn("t", |c, m| {
                upsert(c, m, &note("newer"))?;
                // The legacy shape, inserted past the writer's guard.
                c.execute(
                    "INSERT INTO changelog (txn_id, op, entity_type, entity_id, before, actor)
                     VALUES (?1, 'update', 'items', '1', '{\"content_len\": 12}', 't')",
                    [m.txn_id],
                )?;
                Ok(m.txn_id)
            })
            .unwrap();

        let err = db
            .write_txn("t", undo_last)
            .expect_err("an unrunnable inverse was applied rather than refused")
            .to_string();
        assert!(
            err.contains(&format!("transaction {bad} cannot be undone")),
            "the failure is not the pre-flight refusal — it died part-way through applying: {err}"
        );
        assert!(
            err.contains("`update` on `items`") && err.contains("content_len"),
            "the refusal does not name the entry it cannot run, or why: {err}"
        );
        // Every refusal has to be escapable, and the escape is the only one there is.
        assert!(
            err.contains(&format!("`jkb undo {older}`")),
            "the refusal names no older transaction to revert instead: {err}"
        );
        assert!(
            db.read(|c| item::id_for_uid(c, "newer")).unwrap().is_some()
                && db
                    .read(|c| item::id_for_uid(c, "keep-me"))
                    .unwrap()
                    .is_some(),
            "the refusal reverted something"
        );
        // …and the escape it named actually works.
        db.write_txn("t", move |c, m| super::undo(c, m, older))
            .unwrap();
        assert!(
            db.read(|c| item::id_for_uid(c, "keep-me"))
                .unwrap()
                .is_none(),
            "`jkb undo <older>`, the way out the refusal advertises, did nothing"
        );
    }

    /// Undo history begins at the watermark, and nothing below it is reverted.
    ///
    /// The entries below were written under the **audit** contract, before a before-state was
    /// required to be able to restore anything — so whether any one of them happens to be
    /// invertible is a guess, and guessing per entry is the same mistake that produced them.
    /// `undo_last` does not select below the line; an explicit `jkb undo <txn>` below it says so.
    #[test]
    fn nothing_at_or_below_the_watermark_is_reverted() {
        use crate::item;

        let db = Db::open_in_memory().unwrap();
        // A fresh database excludes nothing: the seed is `MAX(txn_id)` over an empty changelog.
        assert_eq!(db.read(super::watermark).unwrap(), 0);
        // …and with nothing recorded at all, nothing to undo is simply that.
        assert_eq!(db.write_txn("t", undo_last).unwrap(), 0);

        let txn = db
            .write_txn("t", |c, m| {
                upsert(c, m, &note("historic"))?;
                Ok(m.txn_id)
            })
            .unwrap();
        db.write_txn("t", move |c, _| {
            c.execute("UPDATE undo_watermark SET from_txn = ?1", [txn])?;
            Ok(())
        })
        .unwrap();

        // A BARE `jkb undo` DOES NOT REACH BELOW THE LINE — and says which of the two silences
        // this is. Answering 0 here prints "reverted 0 change(s)", which is what an empty
        // database prints, on a database that may hold thousands of transactions.
        let hidden = db
            .write_txn("t", undo_last)
            .expect_err("a bare `jkb undo` reached below the watermark")
            .to_string();
        assert!(
            hidden.contains("nothing to undo")
                && hidden.contains(&format!("undo history begins after transaction {txn}")),
            "a database whose whole history predates the watermark reads as an empty one: {hidden}"
        );
        let err = db
            .write_txn("t", move |c, m| super::undo(c, m, txn))
            .expect_err("an explicit undo below the watermark was performed")
            .to_string();
        assert!(
            err.contains("predates undo history") && err.contains(&format!("transaction {txn}")),
            "the refusal does not say the transaction predates undo history: {err}"
        );
        assert!(
            db.read(|c| item::id_for_uid(c, "historic"))
                .unwrap()
                .is_some(),
            "the pre-watermark transaction was reverted anyway"
        );
    }

    /// An inverse the **database** refuses is a named refusal, and stays one on the next attempt.
    ///
    /// The pre-flight passes this entry on every question it can ask: `("update", "tag_defs")` has
    /// an inverse, the entity resolves, the before-state `{"facet":"size"}` names a real column,
    /// the key is a row id. Then `UPDATE tag_defs SET facet = 'size' WHERE rowid = ?` meets a
    /// UNIQUE index, because `tag::define_facet` re-declared `size` in a later transaction and
    /// **logs nothing** — so no amount of reading the log foresees it. As a bare `?` that was a
    /// raw `SQLite` error, and since a hard error writes no `undo` marker the same transaction was
    /// re-selected and failed identically for ever. The same shape exists on `namespaces.path`.
    #[test]
    fn an_inverse_the_database_refuses_is_a_named_refusal_not_a_wedge() {
        use crate::tag;

        let db = Db::open_in_memory().unwrap();
        let (oldest, a) = db
            .write_txn("t", |c, m| {
                let a = upsert(c, m, &note("a"))?;
                tag::apply(c, m, a, "size", "small")?;
                Ok((m.txn_id, a))
            })
            .unwrap();
        let renamed = db
            .write_txn("t", |c, m| {
                tag::rename_facet(c, m, "size", "scale")?;
                Ok(m.txn_id)
            })
            .unwrap();
        // Re-takes the old name. `define_facet` writes no changelog entry, so the log still says
        // `size` is free.
        db.write_txn("t", |c, m| {
            let b = upsert(c, m, &note("b"))?;
            tag::apply(c, m, b, "size", "tiny")
        })
        .unwrap();

        // The newest change reverts normally; `size` survives it, unlogged.
        db.write_txn("t", undo_last).unwrap();

        let first = db
            .write_txn("t", undo_last)
            .expect_err("an inverse the database refuses was applied rather than refused")
            .to_string();
        assert!(
            first.contains(&format!("transaction {renamed} cannot be undone"))
                && first.contains("Nothing was changed."),
            "an apply-time constraint failure is a raw error, not a named refusal: {first}"
        );
        assert!(
            first.contains("`update` on `tag_defs`") && first.contains("UNIQUE"),
            "the refusal does not name the entry that failed, or why: {first}"
        );
        assert!(
            first.contains(&format!("`jkb undo {oldest}`")),
            "the refusal names no older transaction to revert instead: {first}"
        );
        assert_eq!(
            db.read(move |c| tag::applications(c, a)).unwrap(),
            vec![("scale".to_owned(), "small".to_owned())],
            "the refused undo moved something before it failed"
        );
        // …and it is a refusal every time, not something that degrades on the second run.
        let second = db
            .write_txn("t", undo_last)
            .expect_err("the second attempt did something")
            .to_string();
        assert_eq!(first, second, "the refusal is not stable across attempts");
        // The way out it advertises works.
        db.write_txn("t", move |c, m| super::undo(c, m, oldest))
            .unwrap();
        assert!(
            db.read(move |c| tag::applications(c, a))
                .unwrap()
                .is_empty(),
            "`jkb undo <older>`, the way out the refusal advertises, did nothing"
        );
    }

    /// A re-insert whose key an **unlogged** writer has re-taken refuses, and the retry works
    /// once the obstruction is cleared.
    ///
    /// The finding's exact sequence: `jkb ns mk probe/zone` → `jkb ns rm probe/zone` (logging the
    /// whole row) → `jkb ns mk probe/zone` again — `ns::ensure` takes no `WriteMeta`, logs nothing,
    /// and re-takes the UNIQUE `namespaces.path` — → `jkb undo`. The reinsert was
    /// `INSERT OR IGNORE`, so the conflict vanished, the arm answered `Ok(0)`, and the CLI printed
    /// "reverted 0 change(s)" and exited 0. **The marker was then written**, so clearing the
    /// conflicting namespace and re-running the documented recovery met "it has already been
    /// undone" and the original row's `metadata` — its type, a file's sync structure, a repo's
    /// gate — was gone for good.
    ///
    /// Both halves are asserted, and the second is the one that shows recoverability came back:
    /// a refusal that still marked the transaction would pass the first assertion alone.
    #[test]
    fn a_reinsert_whose_key_was_re_taken_refuses_and_the_retry_works_once_it_is_cleared() {
        use crate::ns;
        use serde_json::json;

        let db = Db::open_in_memory().unwrap();
        let zone = db
            .write_txn("t", |c, _| ns::ensure(c, "probe/zone"))
            .unwrap();
        // Something worth losing, on the row the delete snapshots.
        db.write_txn("t", move |c, m| {
            ns::set_metadata(c, m, zone, &json!({ "gate": "scripts/check.sh" }))
        })
        .unwrap();
        let removed = db
            .write_txn("t", |c, m| {
                ns::remove(c, m, "probe/zone")?;
                Ok(m.txn_id)
            })
            .unwrap();
        // …and the unlogged writer takes the path back. No changelog entry, so nothing about the
        // log foresees the conflict.
        db.write_txn("t", |c, _| ns::ensure(c, "probe/zone"))
            .unwrap();

        let err = db
            .write_txn("t", move |c, m| super::undo(c, m, removed))
            .expect_err("a swallowed UNIQUE conflict was reported as a successful undo")
            .to_string();
        assert!(
            err.contains(&format!("transaction {removed} cannot be undone"))
                && err.contains("Nothing was changed.")
                && err.contains("UNIQUE"),
            "the swallowed conflict is not a named refusal naming the constraint: {err}"
        );

        // The user clears what is in the way and runs the documented recovery again.
        db.write_txn("t", |c, m| ns::remove(c, m, "probe/zone"))
            .unwrap();
        db.write_txn("t", move |c, m| super::undo(c, m, removed))
            .expect("the retry was refused, so the refused undo had marked the transaction anyway");

        let restored = db
            .read(|c| ns::get(c, "probe/zone"))
            .unwrap()
            .expect("the namespace did not come back");
        assert_eq!(
            restored, zone,
            "the namespace came back under a different id, so nothing that referenced it resolves"
        );
        assert_eq!(
            db.read(move |c| ns::get_metadata(c, zone)).unwrap(),
            Some(json!({ "gate": "scripts/check.sh" })),
            "the row came back without the metadata the delete had snapshotted"
        );
    }

    /// A column restore whose target row is **gone** refuses, and the retry works once the row is
    /// back.
    ///
    /// The other half of the same lie. `UPDATE … SET … WHERE rowid = ?` matching nothing wrote the
    /// before-state nowhere, and `Ok(0)` said it had — so `jkb undo` reported success, marked the
    /// transaction undone, and the edit was unrecoverable even after the deleted item was itself
    /// restored. The order matters and is the ordinary one: you undo the delete first, *then* the
    /// edit before it.
    #[test]
    fn a_column_restore_whose_row_is_gone_refuses_and_the_retry_works_once_it_is_back() {
        use crate::item;

        let db = Db::open_in_memory().unwrap();
        let a = db
            .write_txn("t", |c, m| {
                let a = upsert(c, m, &note("a"))?;
                item::set_content(c, m, a, "the original body", Some("b3:original"))?;
                Ok(a)
            })
            .unwrap();
        let edited = db
            .write_txn("t", move |c, m| {
                item::set_content(c, m, a, "the edited body", Some("b3:edited"))?;
                Ok(m.txn_id)
            })
            .unwrap();
        let deleted = db
            .write_txn("t", move |c, m| {
                item::remove(c, m, a, true)?;
                Ok(m.txn_id)
            })
            .unwrap();

        let err = db
            .write_txn("t", move |c, m| super::undo(c, m, edited))
            .expect_err("an update whose row is gone was reported as a successful undo")
            .to_string();
        assert!(
            err.contains(&format!("transaction {edited} cannot be undone"))
                && err.contains("restored nothing"),
            "the vanished row is not a named refusal: {err}"
        );

        // Put the row back the ordinary way, then reach the edit behind it.
        db.write_txn("t", move |c, m| super::undo(c, m, deleted))
            .unwrap();
        db.write_txn("t", move |c, m| super::undo(c, m, edited))
            .expect("the retry was refused, so the refused undo had marked the transaction anyway");
        assert_eq!(
            db.read(move |c| item::get_content(c, a)).unwrap(),
            Some("the original body".to_owned()),
            "the edit was not reverted once its row was back"
        );
    }

    /// A partially-applied undo is **rolled back**, and the failing entry is not the first applied.
    ///
    /// `an_inverse_the_database_refuses_is_a_named_refusal_not_a_wedge` above cannot show this:
    /// `tag::rename_facet` logs its `tag_applications` entries *before* its `tag_defs` entry and
    /// the loop applies `ORDER BY id DESC`, so the entry that fails is the **first** one applied —
    /// nothing has been applied to roll back, and its "the refused undo moved something" assertion
    /// would hold with `write_txn`'s rollback disabled. Round 8's funnel is the first mechanism
    /// under which a partially-applied undo exists at all.
    ///
    /// So: one transaction whose highest-id entry (`item::set_content`, applied first) inverts
    /// cleanly and whose next entry (the rename's `tag_defs` row) hits the UNIQUE index. The
    /// content must be back at its **post-transaction** value afterwards.
    #[test]
    fn a_partly_applied_undo_is_rolled_back_whole() {
        use crate::{item, tag};

        let db = Db::open_in_memory().unwrap();
        let a = db
            .write_txn("t", |c, m| {
                let a = upsert(c, m, &note("a"))?;
                tag::apply(c, m, a, "size", "small")?;
                item::set_content(c, m, a, "before the change", None)?;
                Ok(a)
            })
            .unwrap();
        // ONE transaction, two invertible entries. The content edit is logged last, so `ORDER BY
        // id DESC` applies its inverse first — cleanly — and the rename's `tag_defs` row second.
        let mixed = db
            .write_txn("t", move |c, m| {
                tag::rename_facet(c, m, "size", "scale")?;
                item::set_content(c, m, a, "after the change", None)?;
                Ok(m.txn_id)
            })
            .unwrap();
        // The unlogged re-declaration that makes the rename's inverse violate the UNIQUE index.
        db.write_txn("t", |c, m| {
            let b = upsert(c, m, &note("b"))?;
            tag::apply(c, m, b, "size", "tiny")
        })
        .unwrap();

        let err = db
            .write_txn("t", move |c, m| super::undo(c, m, mixed))
            .expect_err("the mixed transaction was undone despite an inverse the database refuses")
            .to_string();
        assert!(
            err.contains("Nothing was changed."),
            "the refusal does not claim nothing moved: {err}"
        );
        assert_eq!(
            db.read(move |c| item::get_content(c, a)).unwrap(),
            Some("after the change".to_owned()),
            "the inverse that ran before the failing one was left applied, so `Nothing was \
             changed.` is false and half the transaction is undone"
        );
    }

    /// A transaction is undone **once**. Asked again, it refuses.
    ///
    /// `select_work_txn` has never handed back a transaction carrying an `undo` marker, so a bare
    /// `jkb undo` could not reach one; the explicit form did not ask at all, and refusals now
    /// print transaction ids and recommend it. Every table an insert is inverted on except
    /// `items` is a plain rowid table, so `SQLite` reissues the id — and the second run deletes
    /// whatever holds it now.
    #[test]
    fn a_transaction_is_not_undone_twice() {
        use jkb_types::PlacementRole;

        let db = Db::open_in_memory().unwrap();
        let ns = db
            .write_txn("t", |c, _| crate::ns::ensure(c, "x/y"))
            .unwrap();
        let mine = db
            .write_txn("t", |c, m| upsert(c, m, &note("mine")))
            .unwrap();
        let placed = db
            .write_txn("t", move |c, m| {
                crate::placement::place(c, m, mine, ns, PlacementRole::Primary, 0)?;
                Ok(m.txn_id)
            })
            .unwrap();
        db.write_txn("t", move |c, m| super::undo(c, m, placed))
            .unwrap();

        // Somebody else's placement takes the row id the undone one gave up.
        let yours = db
            .write_txn("t", |c, m| upsert(c, m, &note("yours")))
            .unwrap();
        db.write_txn("t", move |c, m| {
            crate::placement::place(c, m, yours, ns, PlacementRole::Primary, 0)
        })
        .unwrap();

        let err = db
            .write_txn("t", move |c, m| super::undo(c, m, placed))
            .expect_err("a transaction was undone a second time")
            .to_string();
        assert!(
            err.contains("already been undone") && err.contains(&format!("transaction {placed}")),
            "the second undo does not say the transaction was already undone: {err}"
        );
        let survivors: Vec<i64> = db
            .read(|c| {
                let mut stmt = c.prepare("SELECT item_id FROM placements")?;
                let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .unwrap();
        assert_eq!(
            survivors,
            vec![yours.get()],
            "undoing the same transaction twice unplaced a row that reused the deleted row id"
        );
    }

    /// `jkb tag rename` is undoable — the definition **and** every application come back.
    ///
    /// It logged one entry against `tag_defs` keyed on the facet *name*: not a row id, and
    /// describing none of the applications it had just rewritten. So `undo` had no inverse for it
    /// and one rename made every later bare `jkb undo` refuse that transaction for good.
    #[test]
    fn undoing_a_facet_rename_puts_the_definition_and_every_application_back() {
        use crate::tag;

        let db = Db::open_in_memory().unwrap();
        let (a, b) = db
            .write_txn("t", |c, m| {
                let a = upsert(c, m, &note("a"))?;
                let b = upsert(c, m, &note("b"))?;
                tag::apply(c, m, a, "size", "small")?;
                tag::apply(c, m, b, "size", "large")?;
                Ok((a, b))
            })
            .unwrap();
        assert_eq!(
            db.write_txn("t", |c, m| tag::rename_facet(c, m, "size", "scale"))
                .unwrap(),
            2
        );

        db.write_txn("t", undo_last).unwrap();
        assert_eq!(
            db.read(move |c| tag::applications(c, a)).unwrap(),
            vec![("size".to_owned(), "small".to_owned())],
            "the renamed application was not put back"
        );
        assert_eq!(
            db.read(move |c| tag::applications(c, b)).unwrap(),
            vec![("size".to_owned(), "large".to_owned())],
            "only some of the renamed applications were put back"
        );
        assert!(
            db.read(tag::facets)
                .unwrap()
                .iter()
                .any(|(f, _)| f == "size"),
            "the facet definition kept the new name"
        );
    }

    /// An item delete's before-state must name **every** column of `items`, exactly as every
    /// other deleter's must.
    ///
    /// Its inverse is a cascade snapshot, so `row_shaped` declines to shape it — which left the
    /// one table carrying a bespoke inverse as the one table nothing validated, and `items`
    /// carried **two** hand-written column lists as a result. The next column added to the schema
    /// would have been dropped by both, on every restore, in silence.
    #[test]
    fn an_item_snapshot_missing_a_column_is_refused_at_the_writer() {
        use crate::changelog::{self, Entity};

        let db = Db::open_in_memory().unwrap();
        let err = db
            .write_txn("t", |c, m| {
                changelog::append(
                    c,
                    m,
                    "delete",
                    Entity::Items,
                    "1",
                    Some(&serde_json::json!({ "item": { "id": 1, "uid": "x", "kind": "note" } })),
                    None,
                )
            })
            .expect_err("an item snapshot missing most of the row was accepted")
            .to_string();
        assert!(
            err.contains("does not name") && err.contains("content_hash"),
            "the refusal does not name the columns that would not come back: {err}"
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
