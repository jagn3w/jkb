//! Saved views: named query strings stored as items under `_sys/views` (design
//! D-shared / task 8.4). A view is an item `kind='view'`, `uid='view:<name>'`, whose
//! `content` is the DSL query string; running a view parses and evaluates it.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::{ItemId, PlacementRole};

use crate::changelog::Entity;
use crate::store::WriteMeta;
use crate::{changelog, ns, placement, query, Error, Result};

/// The namespace saved views live under.
const VIEWS_NS: &str = "_sys/views";

/// Save (or overwrite) the query `query_str` under `name`. The query is validated
/// by parsing it first, so an invalid view is never stored.
///
/// # Errors
/// Returns an error if `query_str` doesn't parse, or a statement/placement fails.
pub fn save(conn: &Connection, meta: &WriteMeta, name: &str, query_str: &str) -> Result<()> {
    // Reject an unparseable query up front.
    query::parse(query_str)?;

    let ns_id = ns::ensure(conn, VIEWS_NS)?;
    let uid = format!("view:{name}");
    // Read what is there **before** the upsert: saving over an existing view updates its row, and
    // logging that as an insert made `jkb undo` delete the view outright — reporting success while
    // destroying a saved query the transaction had merely edited.
    let before: Option<Option<String>> = conn
        .prepare_cached("SELECT content FROM items WHERE uid = ?1")?
        .query_row([&uid], |row| row.get(0))
        .optional()?;
    let id: i64 = conn
        .prepare_cached(
            "INSERT INTO items (uid, kind, content) VALUES (?1, 'view', ?2)
             ON CONFLICT(uid) DO UPDATE SET
                 content = excluded.content,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             RETURNING id",
        )?
        .query_row(params![uid, query_str], |row| row.get(0))?;

    changelog::upsert(
        conn,
        meta,
        Entity::Items,
        &id.to_string(),
        before
            .map(|content| json!({ "uid": &uid, "kind": "view", "content": content }))
            .as_ref(),
        Some(&json!({ "uid": &uid, "kind": "view", "content": query_str })),
    )?;
    placement::place(
        conn,
        meta,
        ItemId::new(id),
        ns_id,
        PlacementRole::Primary,
        0,
    )?;
    Ok(())
}

/// List saved views as `(name, query_string)` pairs, ordered by name.
///
/// # Errors
/// Returns an error if the query fails.
pub fn list(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt =
        conn.prepare_cached("SELECT uid, content FROM items WHERE kind = 'view' ORDER BY uid")?;
    let rows = stmt
        .query_map([], |row| {
            let uid: String = row.get(0)?;
            let query: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            let name = uid.strip_prefix("view:").unwrap_or(&uid).to_owned();
            Ok((name, query))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The stored query string for the view named `name`, if it exists.
///
/// # Errors
/// Returns an error if the query fails.
pub fn get(conn: &Connection, name: &str) -> Result<Option<String>> {
    let query: Option<String> = conn
        .prepare_cached("SELECT content FROM items WHERE uid = ?1")?
        .query_row([format!("view:{name}")], |row| row.get(0))
        .optional()?;
    Ok(query)
}

/// Run the view named `name`: fetch, parse, and evaluate its query.
///
/// # Errors
/// Returns [`Error::NotFound`] if no such view exists, or an error from parsing or
/// evaluation.
pub fn run(conn: &Connection, name: &str) -> Result<Vec<ItemId>> {
    let query_str = get(conn, name)?
        .ok_or_else(|| Error::Types(jkb_types::Error::NotFound(format!("view {name}"))))?;
    query::parse(&query_str)?.evaluate(conn)
}
