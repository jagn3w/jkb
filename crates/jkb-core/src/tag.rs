//! Tag repository: namespaced facets applied to items.
//!
//! A facet (e.g. `read_year`, `topic`, `size`) is declared once; applications
//! attach a value to an item and may carry per-application properties.
//!
//! ## Reserved facets
//!
//! Almost every facet holds *content*: something a person or an agent asserted about an item,
//! and which any writer may set. A few hold **coordination state jkb itself maintains**, where a
//! value that did not come from the code that owns it is not merely wrong but actively harmful.
//! [`FACET_BASE`] is the one such facet today (design D46: it records the commit a branch was cut
//! from, and a fabricated value makes a task close as merged with nothing on its branch).
//!
//! Those facets are listed in `RESERVED` and the rule is enforced **here, at the store**, not at
//! the writers. That siting is the whole point. The rule previously lived in `jkb-cli`, where four
//! consecutive review passes each found a different writer that had not been taught it — and the
//! fifth found a writer in a different crate entirely (`jkb-sync`'s engine, applying facets parsed
//! out of a synced `tasks.md` line). Every route into the store now passes one of three doors:
//!
//! * [`apply`] — the general writer. **Refuses** a reserved facet.
//! * [`reconcile_authored`] — for a caller reconciling an item's tags to a set that came from a
//!   *document* (file sync). Reserved facets are invisible to it in **both** directions: it never
//!   stores one, and never removes one.
//! * [`apply_reserved`] — the privileged writer, for the module that owns the facet. In this
//!   workspace its only non-test caller is `jkb-cli`'s `base` module.
//!
//! Cross-crate visibility cannot make the third callable by exactly one module, so the guarantee
//! is honest rather than absolute: no facet can be written **by accident**, and every write of a
//! reserved one is a call to a function whose name says it is not for you.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, params_from_iter, types::Value, Connection};
use serde_json::json;

use jkb_types::ItemId;

use crate::store::WriteMeta;
use crate::{changelog, Result};

/// The facet recording the commit a branch was cut from (design D46), spelled once.
///
/// The *encoding* of the value — `<branch>:<sha>` — is not core's business and is private to
/// `jkb-cli`'s `base` module, which is the only thing that may format or split one. What lives
/// here is the narrower fact that the facet is **reserved**, because that is a property of the
/// store and has to bind writers in every crate.
pub const FACET_BASE: &str = "base";

/// Facets no general writer may set. See the module docs.
const RESERVED: &[&str] = &[FACET_BASE];

/// Whether `facet` is reserved — its value is jkb's own coordination state, not content.
#[must_use]
pub fn is_reserved(facet: &str) -> bool {
    RESERVED.contains(&facet)
}

/// The refusal, worded once so every route that hits it says the same thing.
fn reserved_refusal(facet: &str) -> crate::Error {
    jkb_types::Error::Validation(format!(
        "`{facet}` is a reserved facet: its value is coordination state jkb maintains, not \
         content, so it cannot be set through a general tag write. Use the command that owns it."
    ))
    .into()
}

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
/// **Refuses a reserved facet** (see the module docs). Loudly, rather than silently dropping it:
/// a caller reaching here with a reserved facet is passing along a value someone typed, and the
/// one thing that must not happen is for it to look accepted. The caller that legitimately has
/// user-authored tags to store *and* must not fail on one — file sync, where a hard error would
/// take a whole reconcile down — has [`reconcile_authored`] instead.
///
/// # Errors
/// Returns an error if `facet` is reserved, or if a statement or the changelog append fails.
pub fn apply(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    facet: &str,
    value: &str,
) -> Result<()> {
    if is_reserved(facet) {
        return Err(reserved_refusal(facet));
    }
    apply_reserved(conn, meta, item, facet, value)
}

/// [`apply`] **without** the reserved-facet refusal — for the module that owns the facet.
///
/// Named so that a call site reads as a deliberate exception. Its only non-test caller in this
/// workspace is `jkb-cli`'s `base` module, which owns [`FACET_BASE`]; tests use it to plant the
/// legacy and hand-typed values that predate the refusal, which is exactly what a real database
/// still holds.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn apply_reserved(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    facet: &str,
    value: &str,
) -> Result<()> {
    define_facet(conn, facet, "string")?;
    let rowid: i64 = conn
        .prepare_cached(
            "INSERT INTO tag_applications (item_id, facet, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(item_id, facet, value) DO UPDATE SET value = value
             RETURNING rowid",
        )?
        .query_row(params![item.get(), facet, value], |row| row.get(0))?;
    let after = json!({ "item_id": item.get(), "facet": facet, "value": value });
    changelog::append(
        conn,
        meta,
        "insert",
        "tag_applications",
        &rowid.to_string(),
        None,
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
    let removed = conn
        .prepare_cached(
            "DELETE FROM tag_applications WHERE item_id = ?1 AND facet = ?2 AND value = ?3",
        )?
        .execute(params![item.get(), facet, value])?;
    if removed > 0 {
        changelog::append(
            conn,
            meta,
            "delete",
            "tag_applications",
            &item.get().to_string(),
            Some(&json!({ "item_id": item.get(), "facet": facet, "value": value })),
            None,
        )?;
    }
    Ok(())
}

/// Reconcile `item`'s tags to exactly `desired` — for a caller whose `desired` set was
/// **authored in a document** rather than by jkb (file sync).
///
/// Reserved facets are invisible in **both** directions, and both halves are load-bearing:
///
/// * a reserved facet in `desired` is skipped, never stored. It got there because someone typed
///   `#base=…` into a synced `tasks.md`, or because an older export wrote one back. Skipping
///   rather than erroring is the difference between one ignored modifier and a whole reconcile
///   pass failing on one file.
/// * a reserved facet already on the item is never removed. This half is the one that would do
///   *worse* damage than the value it guards against: the item's real, measured cut point is not
///   in the document — it cannot be, since nothing writes it there — so a reconcile that treated
///   "absent from the file" as "delete it" would erase a live cut point on every sync of the
///   file the task is recorded in.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn reconcile_authored(
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
        if !is_reserved(facet) && !want.contains(&(facet.as_str(), value.as_str())) {
            remove(conn, meta, item, facet, value)?;
        }
    }
    for (facet, value) in desired {
        if !is_reserved(facet) && !have.contains(&(facet.as_str(), value.as_str())) {
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

/// [`applications_for`] with reserved facets removed — the read half of the pair whose write half
/// is [`reconcile_authored`], so a document-shaped caller cannot **see** one either.
///
/// Not a nicety, and not redundant with the serializer that declines to write one out. File sync
/// detects per-item edits by comparing an item's assembled signature against the base document
/// parsed back from the file — and a reserved facet is never in the file. Left in the assembled
/// side, it makes every task carrying one look permanently KB-edited, so the next disk edit to
/// that same line is reported as a conflict and refused. Recording a cut point would quietly make
/// a task's line uneditable.
///
/// # Errors
/// Returns an error if the query fails.
pub fn authored_applications_for(
    conn: &Connection,
    items: &[ItemId],
) -> Result<HashMap<ItemId, Vec<(String, String)>>> {
    let mut out = applications_for(conn, items)?;
    for tags in out.values_mut() {
        tags.retain(|(facet, _)| !is_reserved(facet));
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
        "tag_defs",
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
    use super::{
        applications, apply, apply_reserved, facets, items_with, reconcile_authored, rename_facet,
        FACET_BASE,
    };
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

    /// The general writer refuses a reserved facet. Enforced at the store rather than at the
    /// writers, because the writers are the problem: five have now had to be taught this rule one
    /// at a time, the last of them in another crate (design D46).
    #[test]
    fn the_general_writer_refuses_a_reserved_facet() {
        let db = Db::open_in_memory().unwrap();
        let id = an_item(&db);
        let err = db
            .write_txn("t", move |conn, meta| {
                apply(conn, meta, id, FACET_BASE, "feature:abc")
            })
            .expect_err("a reserved facet must be refused, not stored");
        assert!(
            err.to_string().contains("reserved facet"),
            "the refusal must say why: {err}"
        );
        assert!(tags_of(&db, id).is_empty());
    }

    /// A document may not author one either — and, crucially, it may not *un*-author one. The
    /// cut point is never written into the file, so "absent from the document" must not read as
    /// "delete it" or every sync of a task's own file would erase its cut point.
    #[test]
    fn a_document_can_neither_author_nor_erase_a_reserved_facet() {
        let db = Db::open_in_memory().unwrap();
        let id = an_item(&db);
        db.write_txn("t", move |conn, meta| {
            apply_reserved(conn, meta, id, FACET_BASE, "feature:abc")?;
            apply(conn, meta, id, "area", "sync")
        })
        .unwrap();

        // A document declaring a different `base=` and no `area=`.
        let desired = vec![(FACET_BASE.to_owned(), "feature:deadbeef".to_owned())];
        db.write_txn("t", move |conn, meta| {
            reconcile_authored(conn, meta, id, &desired)
        })
        .unwrap();

        assert_eq!(
            tags_of(&db, id),
            vec![(FACET_BASE.to_owned(), "feature:abc".to_owned())],
            "the document's `base=` was stored, or the item's real one was reconciled away"
        );
    }

    /// Everything unreserved still reconciles exactly — this is the sync engine's whole tag
    /// contract, and skipping reserved facets must not have made the rest lenient.
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
            reconcile_authored(conn, meta, id, &desired)
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
