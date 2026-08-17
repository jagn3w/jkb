//! Tag repository: namespaced facets applied to items.
//!
//! A facet (e.g. `read_year`, `topic`, `size`) is declared once; applications
//! attach a value to an item and may carry per-application properties.
//!
//! Every facet here holds **content**: something a person or an agent asserted about an item, and
//! which any writer may set. There is deliberately no privileged or reserved facet.
//!
//! There was one — `base`, the commit a branch was cut from — and the reservation apparatus around
//! it (a refusal in [`apply`], a privileged `apply_reserved`, an authored/unauthored split on the
//! read side, and skips in both directions of [`reconcile_tags`]) existed because a *branch* fact
//! was being kept in an item-keyed, multi-valued, untyped, openly-writable store. Six ascending
//! choke points did not close it — the fifth write route was found *after* the store-side
//! reservation was added, and the reservation's own asymmetry produced a must-fix of its own. The
//! fact moved to `branch_records`, keyed `(repo, branch)`, and the whole apparatus went with it:
//! there is nothing to route around when the value is not in the facet namespace at all.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::ItemId;

use crate::changelog::Entity;
use crate::store::WriteMeta;
use crate::{changelog, Result};

/// Declare a facet with a value kind (idempotent).
///
/// # Errors
/// Returns an error if the statement fails.
pub fn define_facet(conn: &Connection, facet: &str, value_kind: &str) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO tag_defs (facet, value_kind) VALUES (?1, ?2)
         ON CONFLICT(facet) DO NOTHING",
    )?
    .execute(params![facet, value_kind])?;
    Ok(())
}

/// Apply `facet = value` to `item` (auto-declaring the facet as `string` if new).
/// Idempotent on `(item, facet, value)`.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn apply(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    facet: &str,
    value: &str,
) -> Result<()> {
    define_facet(conn, facet, "string")?;
    // Idempotent means the second call updates a row that was already there, and logging that as
    // an insert made `jkb undo` remove a tag application the transaction had not created.
    let existing: Option<i64> = conn
        .prepare_cached(
            "SELECT rowid FROM tag_applications WHERE item_id = ?1 AND facet = ?2 AND value = ?3",
        )?
        .query_row(params![item.get(), facet, value], |row| row.get(0))
        .optional()?;
    let rowid: i64 = conn
        .prepare_cached(
            "INSERT INTO tag_applications (item_id, facet, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(item_id, facet, value) DO UPDATE SET value = value
             RETURNING rowid",
        )?
        .query_row(params![item.get(), facet, value], |row| row.get(0))?;
    let after = json!({ "item_id": item.get(), "facet": facet, "value": value });
    changelog::upsert(
        conn,
        meta,
        Entity::TagApplications,
        &rowid.to_string(),
        // The key is the whole row, so a re-application's before-state is its after-state; what
        // this records is that there *was* one.
        existing.map(|_| after.clone()).as_ref(),
        Some(&after),
    )?;
    Ok(())
}

/// Remove the application `facet = value` from `item` (idempotent — removing an
/// absent application is a no-op). Paired with [`apply`] so file sync can reconcile a
/// task's tag set to exactly what the file declares.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn remove(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    facet: &str,
    value: &str,
) -> Result<()> {
    // Read `props` before the delete: `undo` puts the application back from exactly the columns
    // recorded here, and one restored without them is a different application.
    let props: Option<String> = conn
        .prepare_cached(
            "SELECT props FROM tag_applications WHERE item_id = ?1 AND facet = ?2 AND value = ?3",
        )?
        .query_row(params![item.get(), facet, value], |r| r.get(0))
        .optional()?;
    let removed = conn
        .prepare_cached(
            "DELETE FROM tag_applications WHERE item_id = ?1 AND facet = ?2 AND value = ?3",
        )?
        .execute(params![item.get(), facet, value])?;
    if let (1.., Some(props)) = (removed, props) {
        changelog::append(
            conn,
            meta,
            "delete",
            Entity::TagApplications,
            &item.get().to_string(),
            Some(&json!({
                "item_id": item.get(), "facet": facet, "value": value, "props": props,
            })),
            None,
        )?;
    }
    Ok(())
}

/// Reconcile `item`'s tags to exactly `desired`.
///
/// The one tag seam for file sync, shared by the engine's create and update paths so a task's
/// tags are diffed against the document in exactly one place. Every facet is ordinary content
/// here — there is no reserved facet to skip in either direction, because the one fact that was
/// not content is not a facet any more (see the module docs).
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn reconcile_tags(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    desired: &[(String, String)],
) -> Result<()> {
    let want: HashSet<(&str, &str)> = desired
        .iter()
        .map(|(f, v)| (f.as_str(), v.as_str()))
        .collect();
    let current = applications(conn, item)?;
    let have: HashSet<(&str, &str)> = current
        .iter()
        .map(|(f, v)| (f.as_str(), v.as_str()))
        .collect();
    for (facet, value) in &current {
        if !want.contains(&(facet.as_str(), value.as_str())) {
            remove(conn, meta, item, facet, value)?;
        }
    }
    for (facet, value) in desired {
        if !have.contains(&(facet.as_str(), value.as_str())) {
            apply(conn, meta, item, facet, value)?;
        }
    }
    Ok(())
}

/// The `(facet, value)` applications on `item`, ordered. Lets sync diff a task's
/// current tags against the file's declared set.
///
/// # Errors
/// Returns an error if the query fails.
pub fn applications(conn: &Connection, item: ItemId) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT facet, value FROM tag_applications WHERE item_id = ?1 ORDER BY facet, value",
    )?;
    let rows = stmt.query_map([item.get()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The `(facet, value)` applications for every item in `items`, keyed by item id,
/// in one query. Batches [`applications`] so callers reconciling many items (e.g. file
/// sync) avoid one round-trip per item. Items with no tags are absent from the map.
///
/// # Errors
/// Returns an error if the query fails.
pub fn applications_for(
    conn: &Connection,
    items: &[ItemId],
) -> Result<HashMap<ItemId, Vec<(String, String)>>> {
    let mut out: HashMap<ItemId, Vec<(String, String)>> = HashMap::new();
    if items.is_empty() {
        return Ok(out);
    }
    let placeholders = vec!["?"; items.len()].join(", ");
    let sql = format!(
        "SELECT item_id, facet, value FROM tag_applications
         WHERE item_id IN ({placeholders}) ORDER BY item_id, facet, value"
    );
    let params: Vec<Value> = items.iter().map(|id| Value::Integer(id.get())).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok((
            ItemId::new(r.get::<_, i64>(0)?),
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (id, facet, value) = row?;
        out.entry(id).or_default().push((facet, value));
    }
    Ok(out)
}

/// The items tagged with `facet = value`.
///
/// # Errors
/// Returns an error if the query fails.
pub fn items_with(conn: &Connection, facet: &str, value: &str) -> Result<Vec<ItemId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT item_id FROM tag_applications WHERE facet = ?1 AND value = ?2 ORDER BY item_id",
    )?;
    let rows = stmt.query_map(params![facet, value], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(ItemId::new(row?));
    }
    Ok(out)
}

/// Rename a facet across its definition and every application. Returns the number
/// of applications updated.
///
/// # Errors
/// Returns an error if a statement fails (e.g. the new facet collides with an
/// existing application on the same item and value).
pub fn rename_facet(conn: &Connection, meta: &WriteMeta, old: &str, new: &str) -> Result<usize> {
    let updated = conn
        .prepare_cached("UPDATE tag_applications SET facet = ?1 WHERE facet = ?2")?
        .execute(params![new, old])?;
    conn.prepare_cached("UPDATE tag_defs SET facet = ?1 WHERE facet = ?2")?
        .execute(params![new, old])?;
    changelog::append(
        conn,
        meta,
        "update",
        Entity::TagDefs,
        old,
        Some(&json!({ "facet": old })),
        Some(&json!({ "facet": new })),
    )?;
    Ok(updated)
}

/// List declared facets as `(facet, value_kind)`, ordered by facet.
///
/// # Errors
/// Returns an error if the query fails.
pub fn facets(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare_cached("SELECT facet, value_kind FROM tag_defs ORDER BY facet")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{applications, apply, facets, items_with, reconcile_tags, rename_facet};
    use crate::item::{upsert, NewItem};
    use crate::Db;
    use jkb_types::ItemId;

    fn an_item(db: &Db) -> ItemId {
        db.write_txn("t", |conn, meta| {
            upsert(
                conn,
                meta,
                &NewItem {
                    uid: "task:t".to_owned(),
                    kind: "task".to_owned(),
                    content: None,
                    content_hash: None,
                    mime: None,
                },
            )
        })
        .unwrap()
    }

    fn tags_of(db: &Db, id: ItemId) -> Vec<(String, String)> {
        db.read(move |conn| applications(conn, id)).unwrap()
    }

    /// Reconciling adds and drops exactly what the document declares — this is the sync engine's
    /// whole tag contract, and every facet is subject to it.
    #[test]
    fn reconciling_still_adds_and_drops_ordinary_facets() {
        let db = Db::open_in_memory().unwrap();
        let id = an_item(&db);
        db.write_txn("t", move |conn, meta| {
            apply(conn, meta, id, "area", "sync")?;
            apply(conn, meta, id, "size", "small")
        })
        .unwrap();
        let desired = vec![
            ("area".to_owned(), "sync".to_owned()),
            ("owner".to_owned(), "me".to_owned()),
        ];
        db.write_txn("t", move |conn, meta| {
            reconcile_tags(conn, meta, id, &desired)
        })
        .unwrap();
        assert_eq!(
            tags_of(&db, id),
            vec![
                ("area".to_owned(), "sync".to_owned()),
                ("owner".to_owned(), "me".to_owned())
            ]
        );
    }

    #[test]
    fn tagged_items_are_found_by_facet_and_value() {
        let db = Db::open_in_memory().unwrap();
        let sicp = db
            .write_txn("t", |conn, meta| {
                let sicp = upsert(
                    conn,
                    meta,
                    &NewItem {
                        uid: "book:sicp".to_owned(),
                        kind: "document".to_owned(),
                        content: None,
                        content_hash: None,
                        mime: None,
                    },
                )?;
                apply(conn, meta, sicp, "read_year", "2025")?;
                apply(conn, meta, sicp, "read_year", "2025")?; // idempotent
                Ok(sicp)
            })
            .unwrap();

        let hits = db
            .read(|conn| items_with(conn, "read_year", "2025"))
            .unwrap();
        assert_eq!(hits, vec![sicp]);

        let misses = db
            .read(|conn| items_with(conn, "read_year", "2024"))
            .unwrap();
        assert!(misses.is_empty());
    }

    #[test]
    fn renaming_a_facet_moves_applications_and_defs() {
        let db = Db::open_in_memory().unwrap();
        let item = db
            .write_txn("t", |conn, meta| {
                let item = upsert(
                    conn,
                    meta,
                    &NewItem {
                        uid: "book".to_owned(),
                        kind: "document".to_owned(),
                        content: None,
                        content_hash: None,
                        mime: None,
                    },
                )?;
                apply(conn, meta, item, "year_read", "2025")?;
                Ok(item)
            })
            .unwrap();

        let updated = db
            .write_txn("t", |conn, meta| {
                rename_facet(conn, meta, "year_read", "read_year")
            })
            .unwrap();
        assert_eq!(updated, 1);

        assert_eq!(
            db.read(move |conn| items_with(conn, "read_year", "2025"))
                .unwrap(),
            vec![item]
        );
        assert!(db
            .read(|conn| items_with(conn, "year_read", "2025"))
            .unwrap()
            .is_empty());
        assert!(db
            .read(facets)
            .unwrap()
            .iter()
            .any(|(facet, _)| facet == "read_year"));
    }
}
