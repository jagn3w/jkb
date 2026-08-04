//! Containment: which item a node lives inside (design D35).
//!
//! A *pure namespace* is a node that only contains. A parent task both **is** a task and
//! **contains** its subtasks; a document **contains** its chunks. Containment is therefore a
//! behaviour a node takes on, not a node kind — and this module is the one place it is
//! recorded, whatever the two nodes are.
//!
//! ## Why its own table, and why keyed on the child
//! "X is contained by Y" is a property of **X**, not of one of X's locations. An item has
//! several placements (a home plus the `tasks/<repo>` mirror), so recording the parent on
//! the placement would store one fact N times, free to disagree. `child_item_id` is the
//! PRIMARY KEY, which makes "at most one container" structural rather than conventional.
//!
//! ## What this does NOT replace
//! The `parent_of` / `derived_from` edges survive, carrying what a containment row cannot:
//! [`crate::edge::link`]'s cycle guard, `jkb related` traversal, `derived_from` as the
//! provenance search reads for `source_document`, and the `tasks` file serializer's
//! indentation round-trip. [`contain`] writes both in one call so they cannot drift.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::ItemId;

use crate::store::WriteMeta;
use crate::{changelog, Result};

/// Record that `child` is contained by `parent`, at `position` among its siblings.
/// Idempotent: re-containing the same pair updates the position.
///
/// Prefer `crate::task::add_subtask`, which also links the `parent_of` edge (and so
/// inherits its cycle guard). Use this directly only where the relationship edge is already
/// written, as the ingest pipeline does for chunks.
///
/// # Errors
/// Returns a validation error if `child` and `parent` are the same item; otherwise an error
/// if a statement or the changelog append fails.
pub fn contain(
    conn: &Connection,
    meta: &WriteMeta,
    child: ItemId,
    parent: ItemId,
    position: i64,
) -> Result<()> {
    if child == parent {
        return Err(
            jkb_types::Error::Validation(format!("item {child} cannot contain itself")).into(),
        );
    }
    conn.prepare_cached(
        "INSERT INTO containment (child_item_id, parent_item_id, position)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(child_item_id) DO UPDATE SET
             parent_item_id = excluded.parent_item_id,
             position = excluded.position",
    )?
    .execute(params![child.get(), parent.get(), position])?;
    changelog::append(
        conn,
        meta,
        "insert",
        "containment",
        &child.get().to_string(),
        None,
        Some(&json!({
            "child_item_id": child.get(),
            "parent_item_id": parent.get(),
            "position": position,
        })),
    )?;
    Ok(())
}

/// The items contained by `parent`, in sibling order.
///
/// One query for every container: a task's subtasks and a document's chunks are the same
/// read, because containment is recorded the same way for both.
///
/// # Errors
/// Returns an error if the query fails.
pub fn children(conn: &Connection, parent: ItemId) -> Result<Vec<ItemId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT child_item_id FROM containment
          WHERE parent_item_id = ?1 ORDER BY position, child_item_id",
    )?;
    let rows = stmt.query_map([parent.get()], |r| r.get::<_, i64>(0))?;
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(ItemId::new)
        .collect())
}

/// The item containing `child`, if any.
///
/// # Errors
/// Returns an error if the query fails.
pub fn parent(conn: &Connection, child: ItemId) -> Result<Option<ItemId>> {
    Ok(conn
        .prepare_cached("SELECT parent_item_id FROM containment WHERE child_item_id = ?1")?
        .query_row([child.get()], |r| r.get::<_, i64>(0))
        .optional()?
        .map(ItemId::new))
}

/// `(total, open)` child counts for many parents at once; parents with none are absent.
/// "Open" means a non-terminal `status`, so a container can say what is outstanding.
///
/// Batched because the tree asks it for every row it lists.
///
/// # Errors
/// Returns an error if the query fails.
pub fn child_counts(
    conn: &Connection,
    parents: &[ItemId],
) -> Result<std::collections::HashMap<ItemId, (i64, i64)>> {
    let mut out = std::collections::HashMap::new();
    if parents.is_empty() {
        return Ok(out);
    }
    let placeholders = (1..=parents.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    // Placeholders are generated from a count; every value is bound.
    let sql = format!(
        "SELECT c.parent_item_id, COUNT(*),
                SUM(CASE WHEN i.status IS NOT 'done' AND i.status IS NOT 'cancelled'
                         THEN 1 ELSE 0 END)
           FROM containment c JOIN items i ON i.id = c.child_item_id
          WHERE c.parent_item_id IN ({placeholders})
          GROUP BY c.parent_item_id"
    );
    let params: Vec<rusqlite::types::Value> = parents.iter().map(|p| p.get().into()).collect();
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (parent, total, open) = row?;
        out.insert(ItemId::new(parent), (total, open));
    }
    Ok(out)
}
