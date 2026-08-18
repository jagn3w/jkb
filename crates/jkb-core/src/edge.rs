//! Edge repository: the typed directed graph over items.
//!
//! `depends_on` edges must stay acyclic (design D5); [`link`] rejects any edge
//! that would close a cycle, checked with a reachability CTE.

use std::collections::HashMap;

use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection, OptionalExtension};
use serde_json::{json, Value};

use jkb_types::{EdgeId, EdgeType, Error as TypeError, ItemId};

use crate::changelog::{Entity, Op};
use crate::store::WriteMeta;
use crate::{changelog, Result};

/// Create a typed edge `src -> dst`. Idempotent on `(src, dst, type)`. For
/// `depends_on`, rejects edges that would introduce a cycle.
///
/// Leaves `weight` NULL — use [`link_weighted`] for signed evidence edges
/// (`supports`/`contradicts`).
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
    link_weighted(conn, meta, src, dst, edge_type, None, props)
}

/// Create a typed edge `src -> dst` carrying an optional `weight` — the magnitude of a
/// signed evidence edge (design Dmem.4). `None` stores NULL, which
/// [`evidence_for`] reads as 1.0, so an unweighted `supports` still counts as one vote.
///
/// Re-linking an existing edge with `Some(w)` **updates** its weight (the edge is
/// idempotent on `(src, dst, type)`, so strengthening evidence is a re-link, not a
/// duplicate); re-linking with `None` leaves any existing weight intact, so a plain
/// [`link`] never silently erases one. To clear a weight, [`unlink`] then re-link.
///
/// # Errors
/// Returns a validation error if a `depends_on` edge would create a cycle or if `weight`
/// is not finite, or a database error if a statement fails.
pub fn link_weighted(
    conn: &Connection,
    meta: &WriteMeta,
    src: ItemId,
    dst: ItemId,
    edge_type: EdgeType,
    weight: Option<f64>,
    props: Option<&Value>,
) -> Result<EdgeId> {
    if edge_type == EdgeType::DependsOn && creates_cycle(conn, src, dst)? {
        return Err(TypeError::Validation(format!(
            "depends_on {src} -> {dst} would create a cycle"
        ))
        .into());
    }
    // A NaN/infinite weight would silently poison every downstream aggregate.
    if let Some(w) = weight {
        if !w.is_finite() {
            return Err(TypeError::Validation(format!(
                "edge weight must be a finite number, got {w}"
            ))
            .into());
        }
    }

    // Whether this call creates the edge or updates an existing one decides how it is
    // journalled — and `undo` inverts an `insert` by DELETING the row. Recording an update
    // as an insert would make `jkb undo` destroy an edge that existed beforehand (and the
    // knowledge it carried) instead of putting its weight back.
    let existing: Option<(i64, Option<f64>)> = conn
        .prepare_cached(
            "SELECT id, weight FROM edges
             WHERE src_item_id = ?1 AND dst_item_id = ?2 AND type = ?3",
        )?
        .query_row(params![src.get(), dst.get(), edge_type.as_str()], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;

    let props = props.map_or_else(|| "{}".to_owned(), ToString::to_string);
    let id: i64 = conn
        .prepare_cached(
            "INSERT INTO edges (src_item_id, dst_item_id, type, props, weight)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(src_item_id, dst_item_id, type)
                 DO UPDATE SET weight = COALESCE(excluded.weight, edges.weight)
             RETURNING id",
        )?
        .query_row(
            params![src.get(), dst.get(), edge_type.as_str(), props, weight],
            |row| row.get(0),
        )?;

    let describe = |w: Option<f64>| json!({ "src": src.get(), "dst": dst.get(), "type": edge_type.as_str(), "weight": w });
    match existing {
        Some((_, before_weight)) => changelog::append(
            conn,
            meta,
            Op::Update,
            Entity::Edges,
            &id.to_string(),
            Some(&describe(before_weight)),
            Some(&describe(weight.or(before_weight))),
        )?,
        None => changelog::upsert(
            conn,
            meta,
            Entity::Edges,
            &id.to_string(),
            None,
            Some(&describe(weight)),
        )?,
    }
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

/// The direct `edge_type` targets of every source in `srcs`, keyed by source id, in one
/// query. Batches [`edges_from`] so callers reconciling many items (e.g. file sync) avoid
/// one round-trip per source. Sources with no such edges are absent from the map.
///
/// # Errors
/// Returns an error if the query fails.
pub fn edges_from_many(
    conn: &Connection,
    srcs: &[ItemId],
    edge_type: EdgeType,
) -> Result<HashMap<ItemId, Vec<ItemId>>> {
    let mut out: HashMap<ItemId, Vec<ItemId>> = HashMap::new();
    if srcs.is_empty() {
        return Ok(out);
    }
    let placeholders = vec!["?"; srcs.len()].join(", ");
    let sql = format!(
        "SELECT src_item_id, dst_item_id FROM edges
         WHERE type = ? AND src_item_id IN ({placeholders})
         ORDER BY src_item_id, dst_item_id"
    );
    let mut params: Vec<SqlValue> = Vec::with_capacity(srcs.len() + 1);
    params.push(SqlValue::Text(edge_type.as_str().to_owned()));
    params.extend(srcs.iter().map(|id| SqlValue::Integer(id.get())));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok((
            ItemId::new(r.get::<_, i64>(0)?),
            ItemId::new(r.get::<_, i64>(1)?),
        ))
    })?;
    for row in rows {
        let (src, dst) = row?;
        out.entry(src).or_default().push(dst);
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
    // The WHOLE row, under the column names `edges` actually has. It used to log
    // `{src, dst, type}` — the arguments this function was called with, which name no column and
    // omit the `props`, `weight` and `created_at` an edge carries — so `undo` had nothing to
    // rebuild the edge from and the knowledge the edge encoded was simply gone.
    let row = conn
        .prepare_cached(
            "SELECT id, src_item_id, dst_item_id, type, props, weight, created_at FROM edges
              WHERE src_item_id = ?1 AND dst_item_id = ?2 AND type = ?3",
        )?
        .query_row(params![src.get(), dst.get(), edge_type.as_str()], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "src_item_id": r.get::<_, i64>(1)?,
                "dst_item_id": r.get::<_, i64>(2)?,
                "type": r.get::<_, String>(3)?,
                "props": r.get::<_, String>(4)?,
                "weight": r.get::<_, Option<f64>>(5)?,
                "created_at": r.get::<_, String>(6)?,
            }))
        })
        .optional()?;
    let removed = conn
        .prepare_cached(
            "DELETE FROM edges WHERE src_item_id = ?1 AND dst_item_id = ?2 AND type = ?3",
        )?
        .execute(params![src.get(), dst.get(), edge_type.as_str()])?;
    if let (1.., Some(row)) = (removed, row) {
        changelog::append(
            conn,
            meta,
            Op::Delete,
            Entity::Edges,
            &format!("{}->{}", src.get(), dst.get()),
            Some(&row),
            None,
        )?;
    }
    Ok(())
}

/// The **signed-evidence aggregate** for `item` (design Dmem.5, primitive 5):
/// `Σ(supports weight) − Σ(contradicts weight)` over every edge *pointing at* `item`.
/// A NULL weight counts as 1.0, so an unweighted `supports` is one vote. Positive means
/// the balance of recorded evidence favours the unit; negative means it is in trouble.
///
/// Belief is a **query, not a stored scalar** (design Dmem.4): every contributing edge
/// stays inspectable, and conflicting evidence coexists instead of one side overwriting
/// the other.
///
/// # Errors
/// Returns an error if the query fails.
pub fn evidence_for(conn: &Connection, item: ItemId) -> Result<f64> {
    let total: Option<f64> = conn
        .prepare_cached(
            "SELECT SUM(CASE e.type WHEN 'supports' THEN 1.0 ELSE -1.0 END
                        * COALESCE(e.weight, 1.0))
             FROM edges e
             WHERE e.dst_item_id = ?1 AND e.type IN ('supports', 'contradicts')",
        )?
        .query_row([item.get()], |row| row.get(0))?;
    Ok(total.unwrap_or(0.0))
}

/// One edge contributing to a [`evidence_for`] aggregate — enough to show *why* a unit's
/// belief sits where it does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvidenceEdge {
    /// The item on the other end (the source of the evidence).
    pub src: ItemId,
    /// `supports` or `contradicts`.
    pub edge_type: EdgeType,
    /// The signed contribution (`±weight`, NULL weight read as 1.0).
    pub contribution: f64,
}

/// Every `supports`/`contradicts` edge pointing at `item`, strongest contribution first.
/// The itemized form of [`evidence_for`].
///
/// # Errors
/// Returns an error if the query fails or an unexpected edge type is stored.
pub fn evidence_edges(conn: &Connection, item: ItemId) -> Result<Vec<EvidenceEdge>> {
    // Ordered by the SIGNED contribution, not the raw magnitude: "strongest first" has to
    // mean strongest *support* first, or `jkb inv evidence` leads with the most damaging
    // item under a heading that promises the opposite.
    let mut stmt = conn.prepare_cached(
        "SELECT e.src_item_id, e.type, COALESCE(e.weight, 1.0)
         FROM edges e
         WHERE e.dst_item_id = ?1 AND e.type IN ('supports', 'contradicts')
         ORDER BY (CASE e.type WHEN 'supports' THEN 1.0 ELSE -1.0 END
                   * COALESCE(e.weight, 1.0)) DESC,
                  e.src_item_id",
    )?;
    let rows = stmt.query_map([item.get()], |r| {
        Ok((
            ItemId::new(r.get::<_, i64>(0)?),
            r.get::<_, String>(1)?,
            r.get::<_, f64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (src, type_str, weight) = row?;
        let edge_type = EdgeType::from_str_opt(&type_str).ok_or_else(|| {
            crate::Error::Types(TypeError::Validation(format!(
                "unknown edge type `{type_str}` on an evidence edge"
            )))
        })?;
        out.push(EvidenceEdge {
            src,
            edge_type,
            contribution: edge_type.evidence_sign() * weight,
        });
    }
    Ok(out)
}

/// Which way to follow edges in [`walk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Follow edges away from the start item (`src -> dst`) — "what does this point at".
    #[default]
    Out,
    /// Follow edges into the start item (`dst <- src`) — "what points at this".
    In,
    /// Follow edges in both directions (the associative neighbourhood).
    Both,
}

/// One item reached by [`walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Related {
    /// The reached item.
    pub item: ItemId,
    /// How many edges from the start item (1 = a direct neighbour).
    pub depth: usize,
    /// The edge type traversed to reach it on the shortest path found.
    pub via: EdgeType,
    /// Whether that final edge was followed outward (`start -> item`) or inward.
    pub direction: Direction,
}

/// Walk the typed edge graph outward from `start`, breadth-first, up to `depth` hops —
/// the traversal read behind `jkb related` (design Dmem.5). This is the primitive a cold
/// agent uses to reconstruct *context*: the ancestry back to a goal, the prior attempts
/// around a unit, the evidence attached to a hypothesis.
///
/// `types` restricts which edge types may be traversed; an empty slice means "any type".
/// Nodes are returned in BFS order and **de-duplicated** — each item appears once, at the
/// shortest depth it was reached (so a cycle in the non-`depends_on` edge types, which are
/// not acyclicity-guarded, terminates instead of looping). `start` itself is never
/// included.
///
/// # Errors
/// Returns an error if a query fails or an unknown edge type is stored.
pub fn walk(
    conn: &Connection,
    start: ItemId,
    types: &[EdgeType],
    depth: usize,
    direction: Direction,
) -> Result<Vec<Related>> {
    use std::collections::HashSet;

    let mut seen: HashSet<i64> = HashSet::from([start.get()]);
    let mut out: Vec<Related> = Vec::new();
    let mut frontier = vec![start];

    for hop in 1..=depth {
        let mut next = Vec::new();
        for node in frontier {
            for (neighbour, edge_type, dir) in neighbours(conn, node, types, direction)? {
                if !seen.insert(neighbour.get()) {
                    continue;
                }
                out.push(Related {
                    item: neighbour,
                    depth: hop,
                    via: edge_type,
                    direction: dir,
                });
                next.push(neighbour);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(out)
}

/// The direct neighbours of `node` in `direction`, restricted to `types` (empty = any).
fn neighbours(
    conn: &Connection,
    node: ItemId,
    types: &[EdgeType],
    direction: Direction,
) -> Result<Vec<(ItemId, EdgeType, Direction)>> {
    let type_filter = if types.is_empty() {
        String::new()
    } else {
        // Placeholders only — the type strings themselves are bound as parameters.
        format!(" AND type IN ({})", vec!["?"; types.len()].join(", "))
    };
    let mut out = Vec::new();
    let legs: &[Direction] = match direction {
        Direction::Out => &[Direction::Out],
        Direction::In => &[Direction::In],
        Direction::Both => &[Direction::Out, Direction::In],
    };
    for leg in legs {
        let (from_col, to_col) = match leg {
            Direction::In => ("dst_item_id", "src_item_id"),
            // `Both` is expanded into its two legs above and never reaches here.
            Direction::Out | Direction::Both => ("src_item_id", "dst_item_id"),
        };
        let sql = format!(
            "SELECT {to_col}, type FROM edges
             WHERE {from_col} = ?{type_filter} ORDER BY id"
        );
        let mut params: Vec<SqlValue> = vec![SqlValue::Integer(node.get())];
        params.extend(types.iter().map(|t| SqlValue::Text(t.as_str().to_owned())));
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
            Ok((ItemId::new(r.get::<_, i64>(0)?), r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (neighbour, type_str) = row?;
            let edge_type = EdgeType::from_str_opt(&type_str).ok_or_else(|| {
                crate::Error::Types(TypeError::Validation(format!(
                    "unknown edge type `{type_str}` stored on an edge"
                )))
            })?;
            out.push((neighbour, edge_type, *leg));
        }
    }
    Ok(out)
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

    #[test]
    fn signed_evidence_aggregates_weights_and_defaults_null_to_one() {
        use super::{evidence_edges, evidence_for, link_weighted};

        let db = Db::open_in_memory().unwrap();
        let hypothesis = db
            .write_txn("t", |conn, meta| {
                let h = upsert(conn, meta, &task("hypothesis"))?;
                let strong = upsert(conn, meta, &task("obs-strong"))?;
                let weak = upsert(conn, meta, &task("obs-weak"))?;
                let against = upsert(conn, meta, &task("obs-against"))?;
                let unrelated = upsert(conn, meta, &task("unrelated"))?;
                link_weighted(conn, meta, strong, h, EdgeType::Supports, Some(2.0), None)?;
                // A NULL weight counts as one vote.
                link(conn, meta, weak, h, EdgeType::Supports, None)?;
                link_weighted(
                    conn,
                    meta,
                    against,
                    h,
                    EdgeType::Contradicts,
                    Some(0.5),
                    None,
                )?;
                // A non-evidence edge must not contribute.
                link(conn, meta, unrelated, h, EdgeType::References, None)?;
                Ok(h)
            })
            .unwrap();

        let total = db.read(move |conn| evidence_for(conn, hypothesis)).unwrap();
        assert!(
            (total - 2.5).abs() < 1e-9,
            "2.0 + 1.0 (null) - 0.5 = 2.5, got {total}"
        );

        let edges = db
            .read(move |conn| evidence_edges(conn, hypothesis))
            .unwrap();
        assert_eq!(edges.len(), 3, "only the signed edges are itemized");
        assert!(
            (edges[0].contribution - 2.0).abs() < 1e-9,
            "strongest first"
        );
        // Ordered by SIGNED contribution — strongest support first, worst news last. A
        // heavy `contradicts` must not lead a list headed "strongest first".
        let contributions: Vec<f64> = edges.iter().map(|e| e.contribution).collect();
        assert!(
            contributions.windows(2).all(|w| w[0] >= w[1]),
            "must descend by signed contribution, got {contributions:?}"
        );
        assert!(edges.iter().any(|e| e.contribution < 0.0));

        // A unit with no evidence balances at zero rather than erroring.
        let bare = db
            .write_txn("t", |conn, meta| upsert(conn, meta, &task("bare")))
            .unwrap();
        assert!(db.read(move |conn| evidence_for(conn, bare)).unwrap().abs() < f64::EPSILON);
    }

    #[test]
    fn re_linking_updates_a_weight_but_a_plain_link_preserves_it() {
        use super::{evidence_for, link_weighted};

        let db = Db::open_in_memory().unwrap();
        let (obs, hyp) = db
            .write_txn("t", |conn, meta| {
                let obs = upsert(conn, meta, &task("obs"))?;
                let hyp = upsert(conn, meta, &task("hyp"))?;
                link_weighted(conn, meta, obs, hyp, EdgeType::Supports, Some(1.0), None)?;
                Ok((obs, hyp))
            })
            .unwrap();

        // Strengthening the same evidence re-links (idempotent on src/dst/type).
        db.write_txn("t", move |conn, meta| {
            link_weighted(conn, meta, obs, hyp, EdgeType::Supports, Some(3.0), None)
        })
        .unwrap();
        let total = db.read(move |conn| evidence_for(conn, hyp)).unwrap();
        assert!((total - 3.0).abs() < 1e-9, "weight updated, got {total}");

        // A plain `link` (weight None) must not erase the recorded weight.
        db.write_txn("t", move |conn, meta| {
            link(conn, meta, obs, hyp, EdgeType::Supports, None)
        })
        .unwrap();
        let total = db.read(move |conn| evidence_for(conn, hyp)).unwrap();
        assert!((total - 3.0).abs() < 1e-9, "weight preserved, got {total}");

        // A heavier `contradicts` must sort BELOW a lighter `supports`.
        let target = db
            .write_txn("t", |conn, meta| {
                let target = upsert(conn, meta, &task("ordering-target"))?;
                let against = upsert(conn, meta, &task("heavy-against"))?;
                let mild = upsert(conn, meta, &task("mild-for"))?;
                link_weighted(
                    conn,
                    meta,
                    against,
                    target,
                    EdgeType::Contradicts,
                    Some(9.0),
                    None,
                )?;
                link_weighted(
                    conn,
                    meta,
                    mild,
                    target,
                    EdgeType::Supports,
                    Some(0.5),
                    None,
                )?;
                Ok(target)
            })
            .unwrap();
        let ordered = db
            .read(move |conn| super::evidence_edges(conn, target))
            .unwrap();
        assert!(
            ordered[0].contribution > 0.0,
            "the mild support leads, not the heavy contradiction: {ordered:?}"
        );

        // A non-finite weight is rejected rather than poisoning the aggregate.
        let err = db.write_txn("t", move |conn, meta| {
            link_weighted(
                conn,
                meta,
                obs,
                hyp,
                EdgeType::Supports,
                Some(f64::NAN),
                None,
            )
        });
        assert!(err.is_err());
    }

    #[test]
    fn walk_is_breadth_first_deduped_and_type_filtered() {
        use super::{link_weighted, walk, Direction};

        let db = Db::open_in_memory().unwrap();
        // goal <-answers- root_cause <-confirms- experiment ; goal -references-> aside
        // plus a `references` cycle (aside -> goal) to prove termination.
        let (goal, root_cause, experiment, aside) = db
            .write_txn("t", |conn, meta| {
                let goal = upsert(conn, meta, &task("goal"))?;
                let root_cause = upsert(conn, meta, &task("root-cause"))?;
                let experiment = upsert(conn, meta, &task("experiment"))?;
                let aside = upsert(conn, meta, &task("aside"))?;
                link(conn, meta, root_cause, goal, EdgeType::Answers, None)?;
                link(conn, meta, experiment, root_cause, EdgeType::Confirms, None)?;
                link(conn, meta, goal, aside, EdgeType::References, None)?;
                link(conn, meta, aside, goal, EdgeType::References, None)?;
                link_weighted(
                    conn,
                    meta,
                    experiment,
                    goal,
                    EdgeType::Supports,
                    Some(1.0),
                    None,
                )?;
                Ok((goal, root_cause, experiment, aside))
            })
            .unwrap();

        // Inward from the goal: the answer at depth 1, its confirming experiment at 2.
        let reached = db
            .read(move |conn| walk(conn, goal, &[], 2, Direction::In))
            .unwrap();
        let at = |item| reached.iter().find(|r| r.item == item).map(|r| r.depth);
        assert_eq!(at(root_cause), Some(1));
        assert_eq!(at(experiment), Some(1), "also supports the goal directly");
        assert_eq!(at(aside), Some(1));
        assert!(
            !reached.iter().any(|r| r.item == goal),
            "the start item is never returned"
        );

        // Depth 1 stops at direct neighbours.
        let one = db
            .read(move |conn| walk(conn, goal, &[EdgeType::Answers], 1, Direction::In))
            .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].item, root_cause);
        assert_eq!(one[0].via, EdgeType::Answers);

        // A `references` cycle terminates (dedup by item, shortest depth kept).
        let both = db
            .read(move |conn| walk(conn, goal, &[EdgeType::References], 5, Direction::Both))
            .unwrap();
        assert_eq!(both.len(), 1, "only `aside`, reached once: {both:?}");
        assert_eq!(both[0].item, aside);
    }
}
