//! Append-only audit log. Every mutation records a before/after row grouped by
//! `txn_id`, which is the source of truth for `undo` (added in Section 4B).
//!
//! ## The table a mutation names is a **type**, not a string
//!
//! [`Entity`] is a closed set, and [`Entity::insert_inverse`] is an exhaustive match. Together
//! they make one recurring defect unrepresentable: a new table reaching the schema and a writer,
//! while [`crate::undo`] is never told how an insert into it comes back. That omission used to be
//! silent and *worse than a crash* — `undo_last` skipped any transaction containing an insert it
//! could not invert, so a bare `jkb undo` after the new command walked past it and reverted
//! somebody's older transaction instead, deleting the wrong rows and reporting success.
//!
//! It happened repeatedly — `containment` reached the schema without reaching the list, and
//! `branch_records` after it — because the enforcement was procedural: every future author of a
//! table had to remember a list in another module. Now they cannot write the string at all. The
//! new variant does not compile until `insert_inverse` says how an insert into it comes back,
//! [`Entity::ALL`] is generated beside the variants by [`entities!`] so it cannot omit one, and
//! [`write`] refuses an `insert` for a table whose answer is [`InsertInverse::Never`] — so a wrong
//! decision fails loudly at the new writer's first write rather than quietly at some later undo.
//! `undo` itself now **refuses** anything it cannot invert rather than skipping it, so the worst a
//! gap here can still cost is a refusal; these checks exist to make that refusal land on the
//! author rather than on a user.
//!
//! ## The op is **derived**, never chosen
//!
//! Typing the entity left the other half — the op, which is what actually selects the inverse — a
//! free string each call site had to get right, and four upserts got it wrong: `view::save`,
//! `placement::place`, `binding::set` and `tag::apply` each logged `insert` for a statement whose
//! `ON CONFLICT` arm updates a row that existed before the transaction. `undo` then took
//! [`InsertInverse::DeleteRow`] and deleted it — a re-saved view, a mirror placement an ordinary
//! file sync had merely re-written — and reported success.
//!
//! So a row-writing mutation calls [`upsert`], which derives `insert` or `update` from the
//! before-state it is handed, and [`append`] **refuses** the op `insert` outright. A call site
//! cannot choose the wrong op because it no longer chooses one; what it supplies instead is the
//! fact `undo` actually needs, which is whether there was anything there before.

use rusqlite::{params, Connection};
use serde_json::Value;

use crate::store::WriteMeta;
use crate::Result;

/// The op recorded for a row a transaction created — the one op whose inverse is a bare
/// `DELETE`, and therefore the one that needs [`Entity::insert_inverse`]'s answer.
///
/// Written only by [`upsert`], from a before-state; [`append`] refuses it. See the module doc.
pub(crate) const OP_INSERT: &str = "insert";

/// The op recorded for a write that changed a row which already existed.
const OP_UPDATE: &str = "update";

/// Declare the entity set **once**, generating the variants, their stored names and
/// [`Entity::ALL`] together.
///
/// [`Entity::ALL`] is what [`Entity::parse`] iterates, and it used to be a hand-written
/// list beside the enum: a variant could exist, decide its inverse, and never reach it — the
/// silent half of the very defect the type exists to close. A runtime guard and a test were tried
/// first and the test was vacuous (it asserted the list's *length*, which fires on the correct
/// edit and stays green on the omission). Generating the list removes the question instead: there
/// is one place a variant can be written and it produces both.
macro_rules! entities {
    ($( $(#[$attr:meta])* $variant:ident => $name:literal ),+ $(,)?) => {
        /// A table a mutation may be recorded against.
        ///
        /// Closed on purpose: see the module doc. `entity_type` in the `changelog` table is this
        /// value's [`Entity::as_str`], so the stored form is unchanged and older rows still parse.
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub(crate) enum Entity {
            $( $(#[$attr])* $variant, )+
        }

        impl Entity {
            /// Every table, so a stored name can be resolved back to a variant. Generated
            /// with the variants by [`entities!`], so it cannot be missing one.
            pub(crate) const ALL: &'static [Self] = &[ $( Self::$variant, )+ ];

            /// The table name, which is exactly what is stored in `changelog.entity_type`.
            pub(crate) fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $name, )+
                }
            }
        }
    };
}

entities! {
    Items => "items",
    Namespaces => "namespaces",
    Placements => "placements",
    Edges => "edges",
    TagDefs => "tag_defs",
    TagApplications => "tag_applications",
    Bindings => "bindings",
    Mounts => "mounts",
    Blobs => "blobs",
    Ingestions => "ingestions",
    Containment => "containment",
    SyncState => "sync_state",
    BranchRecords => "branch_records",
    /// `undo` markers, whose `entity_id` is the reverted transaction's id.
    Changelog => "changelog",
}

/// How [`crate::undo`] reverses an `insert` recorded against a table.
///
/// The variants are the whole decision a new table has to make, and it is made **once**, in
/// [`Entity::insert_inverse`], rather than by remembering to append to a list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InsertInverse {
    /// `DELETE FROM <table> WHERE rowid = ?`, with `entity_id` the row's rowid. The ordinary
    /// answer, and the reason `entity_id` is a rowid nearly everywhere.
    DeleteRow,
    /// Nothing writes an `insert` here, so there is no inverse to have. [`write`] refuses one,
    /// which is what turns a future writer's wrong assumption into an immediate, named failure
    /// instead of a bare `jkb undo` silently reverting an unrelated transaction.
    ///
    /// Every current member is keyed by something that is **not** a rowid — a file uri, a
    /// transaction id, a content hash — so the generic inverse would address the wrong row
    /// entirely. What each owes instead differs, and the answer is in [`crate::undo`]'s
    /// `INVERSES` rather than restated here:
    ///
    ///  * [`Entity::SyncState`] is logged as an `update` and inverted by hand
    ///    (`Inverse::SyncStateRow`), because a sync transaction's journal row must rewind with the
    ///    items it describes.
    ///  * [`Entity::Changelog`] carries `undo` markers. `INVERSES` has no `undo` entry at all, so
    ///    it is deliberately never inverted — undoing an undo is not something this module does.
    ///  * [`Entity::Blobs`] is content-addressed and has no changelogged writer at all
    ///    (`blob::store` is a dedupe `INSERT OR IGNORE`: the same hash is always the same bytes,
    ///    so there is nothing to take back). The first author to changelog one will reach for the
    ///    hash as `entity_id`, which is what makes `DeleteRow` the wrong declared answer — it
    ///    would compile, ship, and fail at some user's `jkb undo` with "bad entity id" instead of
    ///    on that author's first write.
    Never,
}

impl Entity {
    /// The entity a stored `entity_type` names, or `None` for a table this binary does not know —
    /// which is how a row written by a *newer* binary reads, and is refused rather than guessed at.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|e| e.as_str() == name)
    }

    /// How an `insert` against this table is undone.
    ///
    /// **Exhaustive, and that is the point.** A new variant does not compile until this arm exists,
    /// so "`undo` was never told about the new table" stops being a thing anyone can forget.
    /// Answer [`InsertInverse::DeleteRow`] and an insert into the table is undone by deleting the
    /// row; answer [`InsertInverse::Never`] and [`append`] refuses the first `insert` written
    /// against it, by name.
    pub(crate) fn insert_inverse(self) -> InsertInverse {
        match self {
            Self::Items
            | Self::Namespaces
            | Self::Placements
            | Self::Edges
            | Self::TagDefs
            | Self::TagApplications
            | Self::Bindings
            | Self::Mounts
            | Self::Ingestions
            | Self::Containment
            | Self::BranchRecords => InsertInverse::DeleteRow,
            // Keyed by a uri, a transaction id and a content hash respectively, so a rowid
            // delete would address some other row; none is ever logged with op `insert`.
            Self::SyncState | Self::Changelog | Self::Blobs => InsertInverse::Never,
        }
    }
}

/// Refuse an entry `undo` could not honour, at the moment it is written.
///
/// The table's inserts may have no inverse — [`InsertInverse::Never`] says so, and logging one
/// anyway makes `jkb undo` refuse every transaction that contains one, permanently. That fails
/// here, on the author, rather than on a user with work they cannot take back.
///
/// The other way a table used to reach the log without reaching `undo` — existing as a variant but
/// missing from [`Entity::ALL`], which [`Entity::parse`] iterates — is no longer a state: both are
/// generated together by [`entities!`].
fn undoable(op: &str, entity: Entity) -> Result<()> {
    if op == OP_INSERT && entity.insert_inverse() == InsertInverse::Never {
        return Err(jkb_types::Error::Validation(format!(
            "`{}` records no invertible insert, so logging one would make `jkb undo` skip this \
             transaction and revert an older one — log the write as an update with a hand-written \
             inverse, or give the table a rowid inverse in `changelog::Entity::insert_inverse`",
            entity.as_str()
        ))
        .into());
    }
    Ok(())
}

/// Log a write that created a row **or** replaced one that already existed — the op is derived
/// from `before`, never chosen.
///
/// This is the entry point for every `INSERT … ON CONFLICT DO UPDATE`, and for a plain `INSERT`
/// too (which is simply the case where `before` is honestly `None`). `undo` picks its inverse from
/// the op, so an upsert logged as `insert` makes it `DELETE FROM <table> WHERE rowid = …` a row
/// the transaction never created: a re-saved view destroyed, a pre-existing mirror placement
/// unplaced by an undo of an ordinary file sync.
///
/// `before` is the *whole* reason this signature exists. Supplying it is not bookkeeping for the
/// reader — it is the fact that selects the inverse, so a caller that has not looked cannot get
/// the op right by accident.
///
/// # Errors
/// Returns an error if the insert fails, or if this entry could not be undone in the way its op
/// implies — see [`undoable`].
pub(crate) fn upsert(
    conn: &Connection,
    meta: &WriteMeta,
    entity: Entity,
    entity_id: &str,
    before: Option<&Value>,
    after: Option<&Value>,
) -> Result<()> {
    let op = if before.is_some() {
        OP_UPDATE
    } else {
        OP_INSERT
    };
    write(conn, meta, op, entity, entity_id, before, after)
}

/// Append one changelog entry for a mutation within the current transaction.
///
/// **`insert` is not spellable here** — it is derived by [`upsert`] from a before-state. See the
/// module doc: typing the entity while leaving the op a free string moved the defect rather than
/// closing it, since the op is the half that selects the inverse.
///
/// # Errors
/// Returns an error if the op is `insert`, if the entry could not be undone in the way its op
/// implies (see [`undoable`] and [`crate::undo::check_restorable`]), or if the statement fails.
/// Those are programming errors, refused here because the alternative is a silent one: `undo`
/// inverts the wrong way, or restores nothing and reports success.
pub(crate) fn append(
    conn: &Connection,
    meta: &WriteMeta,
    op: &str,
    entity: Entity,
    entity_id: &str,
    before: Option<&Value>,
    after: Option<&Value>,
) -> Result<()> {
    if op == OP_INSERT {
        return Err(jkb_types::Error::Validation(format!(
            "`insert` is derived from a before-state, not chosen: log this write to `{}` with \
             `changelog::upsert`, which records an `insert` only when there was no row before. \
             Choosing the op is how an upsert came to be logged as an insert, after which `jkb \
             undo` deletes a row the transaction never created",
            entity.as_str()
        ))
        .into());
    }
    write(conn, meta, op, entity, entity_id, before, after)
}

/// The one statement, shared by [`append`] and [`upsert`] — the only place a changelog row is
/// written, so [`undoable`] cannot be bypassed by adding an entry point.
fn write(
    conn: &Connection,
    meta: &WriteMeta,
    op: &str,
    entity: Entity,
    entity_id: &str,
    before: Option<&Value>,
    after: Option<&Value>,
) -> Result<()> {
    undoable(op, entity)?;
    // …and, for a write whose inverse is to put the before-state's columns back, that the
    // before-state can actually do that. See `undo::check_restorable`: the alternative is an
    // inverse that runs, restores nothing, and reports success.
    crate::undo::check_restorable(conn, op, entity.as_str(), before)?;
    let before = before.map(ToString::to_string);
    let after = after.map(ToString::to_string);
    conn.prepare_cached(
        "INSERT INTO changelog (txn_id, op, entity_type, entity_id, before, after, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![
        meta.txn_id,
        op,
        entity.as_str(),
        entity_id,
        before,
        after,
        meta.actor
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Entity, InsertInverse};

    /// Every table a writer can name round-trips through the stored string form, which is what
    /// `undo` reads back out of the log. A variant added without an `as_str` arm cannot compile;
    /// one added with a *duplicate* string would silently shadow another table's entries here.
    #[test]
    fn every_entity_round_trips_through_its_stored_name_and_no_two_share_one() {
        let mut seen = std::collections::BTreeSet::new();
        for e in Entity::ALL {
            assert_eq!(Entity::parse(e.as_str()), Some(*e));
            assert!(seen.insert(e.as_str()), "two entities named {}", e.as_str());
        }
        assert_eq!(Entity::parse("no_such_table"), None);
    }

    /// The refusal that makes a wrong answer loud. Written against the *policy* rather than
    /// against a particular table, so it keeps meaning something when the membership changes.
    #[test]
    fn logging_an_insert_for_a_table_with_no_inverse_is_refused() {
        let db = crate::Db::open_in_memory().unwrap();
        let never = Entity::ALL
            .iter()
            .find(|e| e.insert_inverse() == InsertInverse::Never)
            .copied()
            .expect("no entity declares its inserts uninvertible");
        let err = db.write_txn("t", move |conn, meta| {
            super::upsert(conn, meta, never, "1", None, None)
        });
        assert!(
            err.is_err(),
            "an insert into {} was logged, so `jkb undo` will skip its transaction in silence",
            never.as_str()
        );
        // …and the same entity logs an `update` perfectly well: the refusal is about the op, not
        // about the table being unloggable.
        db.write_txn("t", move |conn, meta| {
            super::append(conn, meta, "update", never, "1", None, None)
        })
        .unwrap();
    }

    /// The op that selects the generic inverse is **derived**, and no caller may name it.
    ///
    /// Two halves, both load-bearing. `append` refuses `insert` outright, so a call site cannot
    /// choose it for a statement whose `ON CONFLICT` arm updates a pre-existing row — the shape
    /// that had `undo` delete a re-saved view. And `upsert` derives the op from the before-state,
    /// so the only way to record an `insert` is to say there was no row before.
    #[test]
    fn an_insert_op_is_derived_from_the_before_state_and_cannot_be_chosen() {
        let db = crate::Db::open_in_memory().unwrap();
        let err = db
            .write_txn("t", |conn, meta| {
                super::append(conn, meta, super::OP_INSERT, Entity::Items, "1", None, None)
            })
            .expect_err("`append` accepted the op `insert`, so a call site can still choose it");
        assert!(
            err.to_string().contains("changelog::upsert"),
            "the refusal does not name the entry point that derives the op: {err}"
        );

        let ops = |before: Option<serde_json::Value>| {
            db.write_txn("t", move |conn, meta| {
                super::upsert(
                    conn,
                    meta,
                    Entity::Items,
                    "1",
                    before.as_ref(),
                    Some(&serde_json::json!({"uid": "x"})),
                )
            })
            .unwrap();
            db.read(|conn| {
                Ok(conn.query_row(
                    "SELECT op FROM changelog ORDER BY id DESC LIMIT 1",
                    [],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .unwrap()
        };
        assert_eq!(
            ops(None),
            "insert",
            "no before-state did not record an insert"
        );
        assert_eq!(
            ops(Some(serde_json::json!({"uid": "x"}))),
            "update",
            "a write over an existing row was recorded as an insert, so `undo` will delete it"
        );
    }
}
