//! Append-only audit log. Every mutation records a before/after row grouped by
//! `txn_id`, which is the source of truth for `undo` (added in Section 4B).

use rusqlite::{params, Connection};
use serde_json::Value;

use crate::store::WriteMeta;
use crate::Result;

/// Append one changelog entry for a mutation within the current transaction.
///
/// # Errors
/// Returns an error if the insert fails.
pub fn append(
    conn: &Connection,
    meta: &WriteMeta,
    op: &str,
    entity_type: &str,
    entity_id: &str,
    before: Option<&Value>,
    after: Option<&Value>,
) -> Result<()> {
    let before = before.map(ToString::to_string);
    let after = after.map(ToString::to_string);
    conn.prepare_cached(
        "INSERT INTO changelog (txn_id, op, entity_type, entity_id, before, after, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![
        meta.txn_id,
        op,
        entity_type,
        entity_id,
        before,
        after,
        meta.actor
    ])?;
    Ok(())
}
