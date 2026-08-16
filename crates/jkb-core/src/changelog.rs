//! Append-only audit log. Every mutation records a before/after row grouped by
//! `txn_id`, which is the source of truth for `undo` (added in Section 4B).
//!
//! ## The table a mutation names is a **type**, not a string
//!
//! [`Entity`] is a closed set, and [`Entity::insert_inverse`] is an exhaustive match. Together
//! they make one recurring defect unrepresentable: a new table reaching the schema and a writer,
//! while [`crate::undo`]'s allowlist is never told about it. That omission is silent and it is
//! *worse than a crash* — `undo_last` skips any transaction containing an insert into an unlisted
//! table, so a bare `jkb undo` after the new command walks past it and reverts somebody's older
//! transaction instead, deleting the wrong rows and reporting success.
//!
//! It has happened repeatedly — `containment` reached the schema without reaching the allowlist,
//! and `branch_records` after it — because the enforcement was procedural: every future author of
//! a table had to remember a list in another module. Now they cannot write the string at all. The
//! new variant does not compile until `insert_inverse` says how an insert into it comes back, the
//! allowlist is *derived* from that answer rather than maintained beside it, and [`append`]
//! refuses an `insert` for a table whose answer is [`InsertInverse::Never`] — so a wrong decision
//! fails loudly at the new writer's first write rather than quietly at some later undo.

use rusqlite::{params, Connection};
use serde_json::Value;

use crate::store::WriteMeta;
use crate::Result;

/// The op recorded for a row a transaction created — the one op whose inverse is generic, and
/// therefore the one that needs the allowlist below.
pub(crate) const OP_INSERT: &str = "insert";

/// A table a mutation may be recorded against.
///
/// Closed on purpose: see the module doc. `entity_type` in the `changelog` table is this value's
/// [`Entity::as_str`], so the stored form is unchanged and older rows still parse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Entity {
    Items,
    Namespaces,
    Placements,
    Edges,
    TagDefs,
    TagApplications,
    Bindings,
    Mounts,
    Blobs,
    Ingestions,
    Containment,
    SyncState,
    BranchRecords,
    /// `undo` markers, whose `entity_id` is the reverted transaction's id.
    Changelog,
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
    /// Nothing writes an `insert` here, so there is no inverse to have. [`append`] refuses one,
    /// which is what turns a future writer's wrong assumption into an immediate, named failure
    /// instead of a bare `jkb undo` silently reverting an unrelated transaction.
    ///
    /// Both current members are keyed by something that is **not** a rowid — a file uri, a
    /// transaction id — so the generic inverse would address the wrong row entirely; their writes
    /// are logged as `update` and inverted by hand.
    Never,
}

impl Entity {
    /// Every table, so the undo allowlist can be derived rather than repeated.
    pub(crate) const ALL: &'static [Self] = &[
        Self::Items,
        Self::Namespaces,
        Self::Placements,
        Self::Edges,
        Self::TagDefs,
        Self::TagApplications,
        Self::Bindings,
        Self::Mounts,
        Self::Blobs,
        Self::Ingestions,
        Self::Containment,
        Self::SyncState,
        Self::BranchRecords,
        Self::Changelog,
    ];

    /// The table name, which is exactly what is stored in `changelog.entity_type`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Items => "items",
            Self::Namespaces => "namespaces",
            Self::Placements => "placements",
            Self::Edges => "edges",
            Self::TagDefs => "tag_defs",
            Self::TagApplications => "tag_applications",
            Self::Bindings => "bindings",
            Self::Mounts => "mounts",
            Self::Blobs => "blobs",
            Self::Ingestions => "ingestions",
            Self::Containment => "containment",
            Self::SyncState => "sync_state",
            Self::BranchRecords => "branch_records",
            Self::Changelog => "changelog",
        }
    }

    /// The entity a stored `entity_type` names, or `None` for a table this binary does not know —
    /// which is how a row written by a *newer* binary reads, and is refused rather than guessed at.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|e| e.as_str() == name)
    }

    /// How an `insert` against this table is undone.
    ///
    /// **Exhaustive, and that is the point.** A new variant does not compile until this arm exists,
    /// so "the undo allowlist was never told about the new table" stops being a thing anyone can
    /// forget. Answer [`InsertInverse::DeleteRow`] and the table joins the allowlist automatically;
    /// answer [`InsertInverse::Never`] and [`append`] refuses the first `insert` written against
    /// it, by name.
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
            | Self::Blobs
            | Self::Ingestions
            | Self::Containment
            | Self::BranchRecords => InsertInverse::DeleteRow,
            // Keyed by uri and by transaction id respectively, so a rowid delete would address
            // some other row; neither is ever logged with op `insert`.
            Self::SyncState | Self::Changelog => InsertInverse::Never,
        }
    }
}

/// Append one changelog entry for a mutation within the current transaction.
///
/// # Errors
/// Returns an error if the insert fails, or if `op` is `insert` for an [`Entity`] whose inserts
/// have no inverse — a programming error, refused here because the alternative is a silent one:
/// `undo_last` skips the whole transaction and reverts an older one in its place.
pub(crate) fn append(
    conn: &Connection,
    meta: &WriteMeta,
    op: &str,
    entity: Entity,
    entity_id: &str,
    before: Option<&Value>,
    after: Option<&Value>,
) -> Result<()> {
    if op == OP_INSERT && entity.insert_inverse() == InsertInverse::Never {
        return Err(jkb_types::Error::Validation(format!(
            "`{}` records no invertible insert, so logging one would make `jkb undo` skip this \
             transaction and revert an older one — log the write as an update with a hand-written \
             inverse, or give the table a rowid inverse in `changelog::Entity::insert_inverse`",
            entity.as_str()
        ))
        .into());
    }
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
            super::append(conn, meta, super::OP_INSERT, never, "1", None, None)
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
}
