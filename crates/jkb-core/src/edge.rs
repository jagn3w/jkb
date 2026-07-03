//! Edge repository: the typed directed graph over items.
//!
//! `depends_on` edges must stay acyclic (design D5); [`link`] rejects any edge
//! that would close a cycle, checked with a reachability CTE.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use jkb_types::{EdgeId, EdgeType, Error as TypeError, ItemId};

use crate::store::WriteMeta;
use crate::{changelog, Result};

/// Create a typed edge `src -> dst`. Idempotent on `(src, dst, type)`. For
/// `depends_on`, rejects edges that would introduce a cycle.
///
/// # Errors
/// Returns a validation error if a `depends_on` edge would create a cycle, or a
/// database error if a statement fails.
pub fn link(
    conn: &Connection,
    meta: &WriteMeta,
    src: ItemId,
    dst: ItemId,
    edge_type: EdgeType,
    props: Option<&Value>,
) -> Result<EdgeId> {
    if edge_type == EdgeType::DependsOn && creates_cycle(conn, src, dst)? {
        return Err(TypeError::Validation(format!(
            "depends_on {src} -> {dst} would create a cycle"
        ))
        .into());
    }

    let props = props.map_or_else(|| "{}".to_owned(), ToString::to_string);
    let id: i64 = conn
        .prepare_cached(
            "INSERT INTO edges (src_item_id, dst_item_id, type, props)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(src_item_id, dst_item_id, type) DO UPDATE SET type = type
             RETURNING id",
        )?
        .query_row(
            params![src.get(), dst.get(), edge_type.as_str(), props],
            |row| row.get(0),
        )?;
    let after = json!({
        "src": src.get(), "dst": dst.get(), "type": edge_type.as_str(),
    });
    changelog::append(
        conn,
        meta,
        "insert",
        "edges",
        &id.to_string(),
        None,
        Some(&after),
    )?;
    Ok(EdgeId::new(id))
}

/// Would adding `src depends_on dst` create a cycle? True if `dst` already
/// (transitively) depends on `src`, or if `src == dst`.
fn creates_cycle(conn: &Connection, src: ItemId, dst: ItemId) -> Result<bool> {
    if src == dst {
        return Ok(true);
    }
    let hit: Option<i64> = conn
        .prepare_cached(
            "WITH RECURSIVE reach(id) AS (
                 SELECT dst_item_id FROM edges WHERE src_item_id = ?1 AND type = 'depends_on'
                 UNION
                 SELECT e.dst_item_id FROM edges e JOIN reach ON e.src_item_id = reach.id
                 WHERE e.type = 'depends_on'
             )
             SELECT 1 FROM reach WHERE id = ?2 LIMIT 1",
        )?
        .query_row(params![dst.get(), src.get()], |row| row.get(0))
        .optional()?;
    Ok(hit.is_some())
}

/// The direct `depends_on` targets of `item`.
///
/// # Errors
/// Returns an error if the query fails.
pub fn dependencies(conn: &Connection, item: ItemId) -> Result<Vec<ItemId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT dst_item_id FROM edges
         WHERE src_item_id = ?1 AND type = 'depends_on' ORDER BY dst_item_id",
    )?;
    let rows = stmt.query_map([item.get()], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(ItemId::new(row?));
    }
    Ok(out)
}

/// The direct `edge_type` targets of `src` (generalizes [`dependencies`] to any type),
/// ordered by destination. Lets file sync diff the current `parent_of` / `depends_on`
/// edges against those the file declares, to remove the ones no longer present.
///
/// # Errors
/// Returns an error if the query fails.
pub fn edges_from(conn: &Connection, src: ItemId, edge_type: EdgeType) -> Result<Vec<ItemId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT dst_item_id FROM edges
         WHERE src_item_id = ?1 AND type = ?2 ORDER BY dst_item_id",
    )?;
    let rows = stmt.query_map(params![src.get(), edge_type.as_str()], |r| {
        r.get::<_, i64>(0)
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(ItemId::new(row?));
    }
    Ok(out)
}

/// Remove the edge `src -> dst` of `edge_type` (idempotent — removing an absent edge
/// is a no-op). Paired with [`link`] so file sync can reconcile an item's edges to
/// exactly what the file declares.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn unlink(
    conn: &Connection,
    meta: &WriteMeta,
    src: ItemId,
    dst: ItemId,
    edge_type: EdgeType,
) -> Result<()> {
    let removed = conn
        .prepare_cached(
            "DELETE FROM edges WHERE src_item_id = ?1 AND dst_item_id = ?2 AND type = ?3",
        )?
        .execute(params![src.get(), dst.get(), edge_type.as_str()])?;
    if removed > 0 {
        changelog::append(
            conn,
            meta,
            "delete",
            "edges",
            &format!("{}->{}", src.get(), dst.get()),
            Some(&json!({ "src": src.get(), "dst": dst.get(), "type": edge_type.as_str() })),
            None,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{dependencies, edges_from, link, unlink};
    use crate::item::{upsert, NewItem};
    use crate::Db;
    use jkb_types::EdgeType;

    fn task(uid: &str) -> NewItem {
        NewItem {
            uid: uid.to_owned(),
            kind: "task".to_owned(),
            content: None,
            content_hash: None,
            mime: None,
        }
    }

    #[test]
    fn depends_on_stays_acyclic() {
        let db = Db::open_in_memory().unwrap();
        let (a, b) = db
            .write_txn("t", |conn, meta| {
                let a = upsert(conn, meta, &task("a"))?;
                let b = upsert(conn, meta, &task("b"))?;
                link(conn, meta, a, b, EdgeType::DependsOn, None)?; // a depends on b
                Ok((a, b))
            })
            .unwrap();

        // b depends_on a would close the cycle a -> b -> a.
        let cycle = db.write_txn("t", move |conn, meta| {
            link(conn, meta, b, a, EdgeType::DependsOn, None)
        });
        assert!(cycle.is_err());

        // a self-dependency is also rejected.
        let self_dep = db.write_txn("t", move |conn, meta| {
            link(conn, meta, a, a, EdgeType::DependsOn, None)
        });
        assert!(self_dep.is_err());

        let deps = db.read(move |conn| dependencies(conn, a)).unwrap();
        assert_eq!(deps, vec![b]);
    }

    #[test]
    fn edges_from_lists_and_unlink_removes_by_type() {
        let db = Db::open_in_memory().unwrap();
        let (a, b, c) = db
            .write_txn("t", |conn, meta| {
                let a = upsert(conn, meta, &task("a"))?;
                let b = upsert(conn, meta, &task("b"))?;
                let c = upsert(conn, meta, &task("c"))?;
                link(conn, meta, a, b, EdgeType::ParentOf, None)?;
                link(conn, meta, a, c, EdgeType::ParentOf, None)?;
                Ok((a, b, c))
            })
            .unwrap();

        let children = db
            .read(move |conn| edges_from(conn, a, EdgeType::ParentOf))
            .unwrap();
        assert_eq!(children, vec![b, c]);

        // Unlink one; the other remains. Removing an absent edge is a no-op.
        db.write_txn("t", move |conn, meta| {
            unlink(conn, meta, a, b, EdgeType::ParentOf)?;
            unlink(conn, meta, a, b, EdgeType::ParentOf) // idempotent
        })
        .unwrap();
        let children = db
            .read(move |conn| edges_from(conn, a, EdgeType::ParentOf))
            .unwrap();
        assert_eq!(children, vec![c]);
    }
}
