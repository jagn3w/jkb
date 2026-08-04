//! Placement repository: the many-to-many link between items and namespaces.
//!
//! An item may be placed under many namespaces (design D3) — e.g. a task under
//! both its `tasks/…` home and a `repos/…` mirror.

use rusqlite::{params, Connection};
use serde_json::json;

use jkb_types::{ItemId, NamespaceId, PlacementRole};

use crate::store::WriteMeta;
use crate::{changelog, Result};

/// Place `item` under `namespace` with the given `role` and `position`. Idempotent
/// on `(item, namespace, role)`: re-placing updates the position.
///
/// This is the **writer boundary** for a typed namespace's contract (design D33.2): it is
/// the single choke point through which an item enters a namespace, so
/// [`crate::nstype::check_placement`] runs here and the guarantee holds for every writer —
/// the task repo, the sync engine, the ingest pipeline — not only for the engine that
/// happens to know the namespace is typed. Untyped namespaces are unaffected.
///
/// # Errors
/// Returns a validation error if the namespace's type does not accept the item's kind;
/// otherwise an error if a statement or the changelog append fails.
pub fn place(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    namespace: NamespaceId,
    role: PlacementRole,
    position: i64,
) -> Result<()> {
    place_under(conn, meta, item, namespace, None, role, position)
}

/// Place `item` in `namespace`, **contained by** `parent` (design D35).
///
/// A placement says where a node lives: in this namespace, inside this container. `None`
/// puts it directly in the namespace, which is what [`place`] does. Listing a container is
/// then one query over one table rather than a per-relationship read plus a filter.
///
/// `namespace` is still required and still meaningful — namespace scoping (`ns:tasks/**`)
/// resolves through it, so a contained item must remain findable by scope. Containment adds
/// *where inside* the namespace, it does not replace *which* namespace.
///
/// # Errors
/// Returns a validation error if the namespace's type does not accept the item's kind;
/// otherwise an error if a statement or the changelog append fails.
pub fn place_under(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    namespace: NamespaceId,
    parent: Option<ItemId>,
    role: PlacementRole,
    position: i64,
) -> Result<()> {
    crate::nstype::check_placement(conn, item, namespace)?;
    // A node cannot contain itself. Deeper cycles are refused by `edge::link` when the
    // relationship is recorded; this is the one case a placement can create on its own.
    if parent == Some(item) {
        return Err(
            jkb_types::Error::Validation(format!("item {item} cannot contain itself")).into(),
        );
    }
    let rowid: i64 = conn
        .prepare_cached(
            "INSERT INTO placements (item_id, namespace_id, role, position, parent_item_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(item_id, namespace_id, role) DO UPDATE SET
                 position = excluded.position,
                 parent_item_id = excluded.parent_item_id
             RETURNING rowid",
        )?
        .query_row(
            params![
                item.get(),
                namespace.get(),
                role.as_str(),
                position,
                parent.map(ItemId::get)
            ],
            |row| row.get(0),
        )?;
    let after = json!({
        "item_id": item.get(),
        "namespace_id": namespace.get(),
        "role": role.as_str(),
        "position": position,
        "parent_item_id": parent.map(ItemId::get),
    });
    changelog::append(
        conn,
        meta,
        "insert",
        "placements",
        &rowid.to_string(),
        None,
        Some(&after),
    )?;
    Ok(())
}

/// Make `namespace` the item's **sole** primary placement at `position`: any existing
/// primary under a *different* namespace is removed (each recorded in the changelog)
/// before the new primary is placed. Used by file sync when an item moves between
/// sections, so an item never ends up with two primary homes.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn set_primary(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    namespace: NamespaceId,
    position: i64,
) -> Result<()> {
    let stale: Vec<(i64, i64)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT rowid, namespace_id FROM placements
             WHERE item_id = ?1 AND role = 'primary' AND namespace_id != ?2",
        )?;
        let rows = stmt.query_map([item.get(), namespace.get()], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (rowid, old_ns) in stale {
        conn.prepare_cached("DELETE FROM placements WHERE rowid = ?1")?
            .execute([rowid])?;
        changelog::append(
            conn,
            meta,
            "delete",
            "placements",
            &rowid.to_string(),
            Some(&json!({ "item_id": item.get(), "namespace_id": old_ns, "role": "primary" })),
            None,
        )?;
    }
    place(
        conn,
        meta,
        item,
        namespace,
        PlacementRole::Primary,
        position,
    )
}

/// Remove any **reference** (mirror) placement of `item` under `namespace`, recording each
/// removal in the changelog. The primary home is left untouched — re-home with
/// [`set_primary`] instead. Idempotent: removing a mirror that isn't there is a no-op.
/// Returns the number of placements removed. This is the inverse of [`place`] with
/// [`PlacementRole::Reference`].
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn unplace(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    namespace: NamespaceId,
) -> Result<usize> {
    let rowids: Vec<i64> = {
        let mut stmt = conn.prepare_cached(
            "SELECT rowid FROM placements
             WHERE item_id = ?1 AND namespace_id = ?2 AND role = 'reference'",
        )?;
        let rows = stmt.query_map([item.get(), namespace.get()], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for rowid in &rowids {
        conn.prepare_cached("DELETE FROM placements WHERE rowid = ?1")?
            .execute([rowid])?;
        changelog::append(
            conn,
            meta,
            "delete",
            "placements",
            &rowid.to_string(),
            Some(&json!({
                "item_id": item.get(),
                "namespace_id": namespace.get(),
                "role": "reference",
            })),
            None,
        )?;
    }
    Ok(rowids.len())
}

/// The items placed **directly** in `namespace` — those not contained by another item —
/// ordered by position.
///
/// Distinct from [`items_in`], which returns everything placed in the namespace including
/// contained nodes. Both are correct for their question: a subtask *is* in `tasks/jkb`
/// (which is why `ns:tasks/**` scoping finds it), but it is *listed* under its parent, not
/// beside it.
///
/// # Errors
/// Returns an error if the query fails.
pub fn items_directly_in(conn: &Connection, namespace: NamespaceId) -> Result<Vec<ItemId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT item_id, position FROM placements
          WHERE namespace_id = ?1 AND parent_item_id IS NULL
          ORDER BY position, item_id",
    )?;
    let rows = stmt.query_map([namespace.get()], |r| r.get::<_, i64>(0))?;
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(ItemId::new)
        .collect())
}

/// The items **contained by** `parent`, in placement order — the container behaviour a
/// node takes on (design D35).
///
/// One query over one table, whatever the container is: a task's subtasks and a document's
/// chunks are the same read, because containment is recorded the same way for both.
///
/// # Errors
/// Returns an error if the query fails.
pub fn items_under(conn: &Connection, parent: ItemId) -> Result<Vec<ItemId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT item_id, position FROM placements
          WHERE parent_item_id = ?1 ORDER BY position, item_id",
    )?;
    let rows = stmt.query_map([parent.get()], |r| r.get::<_, i64>(0))?;
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(ItemId::new)
        .collect())
}

/// The items directly placed under `namespace`, optionally filtered by `role`,
/// ordered by position.
///
/// # Errors
/// Returns an error if the query fails.
pub fn items_in(
    conn: &Connection,
    namespace: NamespaceId,
    role: Option<PlacementRole>,
) -> Result<Vec<ItemId>> {
    let mut out = Vec::new();
    if let Some(role) = role {
        let mut stmt = conn.prepare_cached(
            "SELECT item_id FROM placements
             WHERE namespace_id = ?1 AND role = ?2 ORDER BY position, item_id",
        )?;
        let rows = stmt.query_map(params![namespace.get(), role.as_str()], |r| {
            r.get::<_, i64>(0)
        })?;
        for row in rows {
            out.push(ItemId::new(row?));
        }
    } else {
        let mut stmt = conn.prepare_cached(
            "SELECT item_id FROM placements WHERE namespace_id = ?1 ORDER BY position, item_id",
        )?;
        let rows = stmt.query_map([namespace.get()], |r| r.get::<_, i64>(0))?;
        for row in rows {
            out.push(ItemId::new(row?));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{items_in, place};
    use crate::item::{upsert, NewItem};
    use crate::{ns, Db};
    use jkb_types::PlacementRole;

    #[test]
    fn one_item_resolves_under_multiple_namespaces() {
        let db = Db::open_in_memory().unwrap();
        let (task_id, home, mirror) = db
            .write_txn("t", |conn, meta| {
                let home = ns::ensure(conn, "tasks/inbox")?;
                let mirror = ns::ensure(conn, "repos/monorepo/backend")?;
                let task_id = upsert(
                    conn,
                    meta,
                    &NewItem {
                        uid: "task:1".to_owned(),
                        kind: "task".to_owned(),
                        content: None,
                        content_hash: None,
                        mime: None,
                    },
                )?;
                place(conn, meta, task_id, home, PlacementRole::Primary, 0)?;
                place(conn, meta, task_id, mirror, PlacementRole::Reference, 0)?;
                Ok((task_id, home, mirror))
            })
            .unwrap();

        let in_home = db.read(move |conn| items_in(conn, home, None)).unwrap();
        let in_mirror = db.read(move |conn| items_in(conn, mirror, None)).unwrap();
        assert_eq!(in_home, vec![task_id]);
        assert_eq!(in_mirror, vec![task_id]);
    }

    #[test]
    fn set_primary_moves_the_sole_primary_and_audits() {
        use super::set_primary;
        let db = Db::open_in_memory().unwrap();
        let (item, a, b) = db
            .write_txn("t", |conn, meta| {
                let a = ns::ensure(conn, "docs/a")?;
                let b = ns::ensure(conn, "docs/b")?;
                let item = upsert(
                    conn,
                    meta,
                    &NewItem {
                        uid: "i".to_owned(),
                        kind: "task".to_owned(),
                        content: None,
                        content_hash: None,
                        mime: None,
                    },
                )?;
                set_primary(conn, meta, item, a, 0)?;
                Ok((item, a, b))
            })
            .unwrap();
        assert_eq!(
            db.read(move |conn| items_in(conn, a, None)).unwrap(),
            vec![item]
        );

        // Re-home to b: the primary under a is removed, leaving exactly one primary.
        db.write_txn("t", move |conn, meta| set_primary(conn, meta, item, b, 0))
            .unwrap();
        assert!(db
            .read(move |conn| items_in(conn, a, None))
            .unwrap()
            .is_empty());
        assert_eq!(
            db.read(move |conn| items_in(conn, b, None)).unwrap(),
            vec![item]
        );

        // The removal was recorded in the changelog (audit convention).
        let deletes: i64 = db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM changelog WHERE entity_type = 'placements' AND op = 'delete'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(deletes, 1);
    }

    #[test]
    fn unplace_removes_only_the_reference_mirror() {
        use super::{set_primary, unplace};
        let db = Db::open_in_memory().unwrap();
        let (item, home, mirror) = db
            .write_txn("t", |conn, meta| {
                let home = ns::ensure(conn, "tasks/jkb/.backlog")?;
                let mirror = ns::ensure(conn, ".backlog")?;
                let item = upsert(
                    conn,
                    meta,
                    &NewItem {
                        uid: "i".to_owned(),
                        kind: "task".to_owned(),
                        content: None,
                        content_hash: None,
                        mime: None,
                    },
                )?;
                set_primary(conn, meta, item, home, 0)?;
                place(conn, meta, item, mirror, PlacementRole::Reference, 0)?;
                Ok((item, home, mirror))
            })
            .unwrap();

        // Removing the mirror leaves the primary home intact.
        let removed = db
            .write_txn("t", move |conn, meta| unplace(conn, meta, item, mirror))
            .unwrap();
        assert_eq!(removed, 1);
        assert!(db
            .read(move |conn| items_in(conn, mirror, None))
            .unwrap()
            .is_empty());
        assert_eq!(
            db.read(move |conn| items_in(conn, home, None)).unwrap(),
            vec![item]
        );

        // Idempotent, and it never touches a primary placement.
        let again = db
            .write_txn("t", move |conn, meta| unplace(conn, meta, item, home))
            .unwrap();
        assert_eq!(again, 0);
        assert_eq!(
            db.read(move |conn| items_in(conn, home, None)).unwrap(),
            vec![item]
        );
    }
}
