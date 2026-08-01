//! Namespace repository: the logical folder tree.
//!
//! Paths are logical jkb addresses, normalized and validated before storage.
//! The tree is stored as an adjacency list (`parent_id`); subtree reads use a
//! recursive CTE (design D6). A closure table is a v2 optimization.

use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use unicode_normalization::UnicodeNormalization;

use jkb_types::{Error as TypeError, NamespaceId};

use crate::store::WriteMeta;
use crate::{changelog, Result};

/// Normalize and validate a logical namespace path: reject empty paths/segments,
/// `.`/`..` traversal, and control characters; apply Unicode NFC to each segment.
///
/// # Errors
/// Returns [`crate::Error::Types`] wrapping a validation error for any violation.
pub fn normalize(path: &str) -> Result<String> {
    if path.is_empty() {
        return Err(TypeError::Validation("namespace path is empty".to_owned()).into());
    }
    let mut segments = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            return Err(TypeError::Validation(format!("empty segment in '{path}'")).into());
        }
        if segment == "." || segment == ".." {
            return Err(TypeError::Validation(format!("path traversal in '{path}'")).into());
        }
        if segment.chars().any(char::is_control) {
            return Err(TypeError::Validation(format!("control character in '{path}'")).into());
        }
        segments.push(segment.nfc().collect::<String>());
    }
    Ok(segments.join("/"))
}

/// Whether `path` denotes a system namespace (`_sys` or below).
fn kind_for(path: &str) -> &'static str {
    if path == "_sys" || path.starts_with("_sys/") {
        "system"
    } else {
        "logical"
    }
}

/// Ensure the namespace at `path` and all of its ancestors exist, returning the
/// id of the leaf. Idempotent: already-present namespaces are left untouched.
///
/// A path listed in [`crate::nstype::RESERVED_TYPES`] is stamped with its type as it is
/// created (design D33.4), so a reserved root is never left untyped — whether it arrives
/// on a fresh database, via `jkb ns mk tasks`, or as a side effect of the first
/// `task add`. The stamp is seeded rather than changelogged, matching how `V001` seeds the
/// `_sys` namespaces: it is schema, not a user edit.
///
/// # Errors
/// Returns an error if the path is invalid or a statement fails.
pub fn ensure(conn: &Connection, path: &str) -> Result<NamespaceId> {
    let normalized = normalize(path)?;
    let mut parent: Option<i64> = None;
    let mut current = String::new();
    let mut id = 0_i64;
    for segment in normalized.split('/') {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(segment);
        id = conn
            .prepare_cached(
                "INSERT INTO namespaces (path, parent_id, kind) VALUES (?1, ?2, ?3)
                 ON CONFLICT(path) DO UPDATE SET path = path
                 RETURNING id",
            )?
            .query_row(params![current, parent, kind_for(&current)], |row| {
                row.get(0)
            })?;
        seed_reserved_type(conn, &current)?;
        parent = Some(id);
    }
    Ok(NamespaceId::new(id))
}

/// Stamp the declared type on a reserved namespace, if `path` is one and it is not already
/// typed. Never clobbers an existing type, so re-typing by hand survives.
fn seed_reserved_type(conn: &Connection, path: &str) -> Result<()> {
    let Some((_, type_name)) = crate::nstype::RESERVED_TYPES
        .iter()
        .find(|(reserved, _)| *reserved == path)
    else {
        return Ok(());
    };
    let sql = format!(
        "UPDATE namespaces SET metadata = json_set({METADATA_OBJ}, '$.{TYPE_KEY}', ?2)
         WHERE path = ?1 AND {} IS NULL",
        type_expr()
    );
    conn.prepare_cached(&sql)?
        .execute(params![path, type_name])?;
    Ok(())
}

/// `namespaces.metadata` as a JSON object, tolerating NULL and (only reachable if written
/// outside jkb) non-JSON — mirroring [`get_metadata`], which drops what will not parse
/// rather than failing the read. `json_extract` *raises* on invalid JSON, so every SQL
/// reader of the type key goes through this.
const METADATA_OBJ: &str = "CASE WHEN json_valid(metadata) THEN metadata ELSE '{}' END";

/// The SQL expression reading a namespace's [`TYPE_KEY`], NULL when untyped.
fn type_expr() -> String {
    format!("json_extract({METADATA_OBJ}, '$.{TYPE_KEY}')")
}

/// Look up the id of an existing namespace by path.
///
/// # Errors
/// Returns an error if the path is invalid or the query fails.
pub fn get(conn: &Connection, path: &str) -> Result<Option<NamespaceId>> {
    let normalized = normalize(path)?;
    let id: Option<i64> = conn
        .prepare_cached("SELECT id FROM namespaces WHERE path = ?1")?
        .query_row([&normalized], |row| row.get(0))
        .optional()?;
    Ok(id.map(NamespaceId::new))
}

/// Return the namespace at `path` and all of its descendants as `(id, path)`,
/// ordered by path, via a recursive CTE.
///
/// # Errors
/// Returns an error if the path is invalid or the query fails.
pub fn subtree(conn: &Connection, path: &str) -> Result<Vec<(NamespaceId, String)>> {
    let normalized = normalize(path)?;
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE sub(id, path) AS (
             SELECT id, path FROM namespaces WHERE path = ?1
             UNION ALL
             SELECT n.id, n.path FROM namespaces n JOIN sub ON n.parent_id = sub.id
         )
         SELECT id, path FROM sub ORDER BY path",
    )?;
    let rows = stmt.query_map([&normalized], |row| {
        Ok((
            NamespaceId::new(row.get::<_, i64>(0)?),
            row.get::<_, String>(1)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Count the distinct item leaves placed anywhere in the subtree rooted at `path`
/// (the namespace itself or any descendant), so the tree can indicate whether a folder
/// leads to real content. When `include_terminal` is false, `done`/`cancelled` tasks are
/// excluded (mirroring the default `jkb ls` view); items with no status always count.
///
/// # Errors
/// Returns an error if the path is invalid or the query fails.
pub fn subtree_leaf_count(conn: &Connection, path: &str, include_terminal: bool) -> Result<i64> {
    let normalized = normalize(path)?;
    // The namespace hierarchy is a strict `parent_id` tree (a namespace's parent is always
    // its path prefix), and mirrors are item→namespace placements, not namespace links — so
    // no cycle is reachable here. The `depth < 256` bound is belt-and-suspenders: even if a
    // cyclic `parent_id` were ever written, the CTE terminates instead of spinning.
    let count: i64 = conn
        .prepare_cached(
            "WITH RECURSIVE sub(id, depth) AS (
                 SELECT id, 0 FROM namespaces WHERE path = ?1
                 UNION ALL
                 SELECT n.id, sub.depth + 1 FROM namespaces n JOIN sub ON n.parent_id = sub.id
                 WHERE sub.depth < 256
             )
             SELECT COUNT(DISTINCT p.item_id) FROM placements p
             JOIN sub ON sub.id = p.namespace_id
             JOIN items i ON i.id = p.item_id
             WHERE ?2 OR i.status IS NULL OR i.status NOT IN ('done', 'cancelled')",
        )?
        .query_row(params![normalized, include_terminal], |row| row.get(0))?;
    Ok(count)
}

/// Leaf counts (see [`subtree_leaf_count`]) for **every direct child** of `parent`
/// (or every root namespace when `parent` is `None`), computed in a single grouped
/// recursive query keyed by each child's own subtree. This replaces N independent
/// descendant walks — one per child — when listing a wide namespace.
///
/// The returned map holds only children with a non-zero count; a child absent from
/// the map has zero visible leaves. `include_terminal` matches [`subtree_leaf_count`].
///
/// # Errors
/// Returns an error if the path is invalid or the query fails.
pub fn subtree_leaf_counts(
    conn: &Connection,
    parent: Option<&str>,
    include_terminal: bool,
) -> Result<std::collections::HashMap<NamespaceId, i64>> {
    // The recursive part is shared; only the anchor (direct children of `parent`, or the
    // roots) differs. Each anchored child carries its own id as `root_id` down its subtree,
    // so a single grouped COUNT(DISTINCT) yields per-child leaf totals. The `depth < 256`
    // bound mirrors `subtree_leaf_count` — belt-and-suspenders against a cyclic `parent_id`.
    const TAIL: &str = "
             UNION ALL
             SELECT sub.root_id, n.id, sub.depth + 1
             FROM namespaces n JOIN sub ON n.parent_id = sub.id
             WHERE sub.depth < 256
         )
         SELECT sub.root_id, COUNT(DISTINCT p.item_id)
         FROM sub
         JOIN placements p ON p.namespace_id = sub.id
         JOIN items i ON i.id = p.item_id
         WHERE (?1 OR i.status IS NULL OR i.status NOT IN ('done', 'cancelled'))
         GROUP BY sub.root_id";
    let mut out = std::collections::HashMap::new();
    match parent {
        None => {
            let sql = format!(
                "WITH RECURSIVE sub(root_id, id, depth) AS (
                     SELECT id, id, 0 FROM namespaces WHERE parent_id IS NULL{TAIL}"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![include_terminal], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (id, count) = row?;
                out.insert(NamespaceId::new(id), count);
            }
        }
        Some(p) => {
            let normalized = normalize(p)?;
            let sql = format!(
                "WITH RECURSIVE sub(root_id, id, depth) AS (
                     SELECT c.id, c.id, 0 FROM namespaces c
                     JOIN namespaces par ON c.parent_id = par.id
                     WHERE par.path = ?2{TAIL}"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![include_terminal, normalized], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (id, count) = row?;
                out.insert(NamespaceId::new(id), count);
            }
        }
    }
    Ok(out)
}

/// The root namespaces (those with no parent), ordered by path. Backs `jkb ns ls`
/// with no scope argument.
///
/// # Errors
/// Returns an error if the query fails.
pub fn roots(conn: &Connection) -> Result<Vec<(NamespaceId, String)>> {
    let mut stmt = conn
        .prepare_cached("SELECT id, path FROM namespaces WHERE parent_id IS NULL ORDER BY path")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            NamespaceId::new(row.get::<_, i64>(0)?),
            row.get::<_, String>(1)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The direct children of the namespace at `path` (depth 1), ordered by path.
///
/// # Errors
/// Returns an error if the path is invalid or the query fails.
pub fn children(conn: &Connection, path: &str) -> Result<Vec<(NamespaceId, String)>> {
    let normalized = normalize(path)?;
    let mut stmt = conn.prepare_cached(
        "SELECT c.id, c.path FROM namespaces c
         JOIN namespaces p ON c.parent_id = p.id
         WHERE p.path = ?1 ORDER BY c.path",
    )?;
    let rows = stmt.query_map([&normalized], |row| {
        Ok((
            NamespaceId::new(row.get::<_, i64>(0)?),
            row.get::<_, String>(1)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Store `metadata` JSON on the namespace `id`, recording the change. File sync uses
/// this to persist a section's original header line and ordinal position on the
/// namespace it maps to, so the file can be re-rendered with faithful formatting.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn set_metadata(
    conn: &Connection,
    meta: &WriteMeta,
    id: NamespaceId,
    metadata: &Value,
) -> Result<()> {
    conn.prepare_cached("UPDATE namespaces SET metadata = ?2 WHERE id = ?1")?
        .execute(params![id.get(), metadata.to_string()])?;
    changelog::append(
        conn,
        meta,
        "update",
        "namespaces",
        &id.get().to_string(),
        None,
        Some(metadata),
    )?;
    Ok(())
}

/// Read the `metadata` JSON stored on the namespace `id`, if any (and if it parses).
///
/// # Errors
/// Returns an error if the query fails.
pub fn get_metadata(conn: &Connection, id: NamespaceId) -> Result<Option<Value>> {
    let raw: Option<Option<String>> = conn
        .prepare_cached("SELECT metadata FROM namespaces WHERE id = ?1")?
        .query_row([id.get()], |row| row.get::<_, Option<String>>(0))
        .optional()?;
    Ok(raw.flatten().and_then(|s| serde_json::from_str(&s).ok()))
}

/// The `metadata` key holding a namespace's **strategy type** (design Dmem.1). The
/// value is a [`crate::nstype::NamespaceType`] name (e.g. `debugging`).
///
/// This is deliberately a metadata key rather than a new column: `namespaces.kind`
/// (`logical`/`mount`/`system`) answers *how the namespace is backed*, which is
/// orthogonal to *what protocol runs inside it*. A namespace with no `type` is untyped
/// and behaves exactly as it did before (no descriptor, no frontier extras).
pub const TYPE_KEY: &str = "type";

/// Record the type `type_name` on the namespace `id`, merging into (not replacing) its
/// existing metadata. Validating that `type_name` names a registered type is otherwise the
/// caller's job (`crate::nstype::resolve`), so a namespace can be typed before the strategy
/// that reads it is built.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn set_type(
    conn: &Connection,
    meta: &WriteMeta,
    id: NamespaceId,
    type_name: &str,
) -> Result<()> {
    let mut metadata = get_metadata(conn, id)?.unwrap_or_else(|| Value::Object(Map::new()));
    // Non-object metadata (only reachable if written outside jkb) is replaced rather than
    // silently dropping the type.
    if !metadata.is_object() {
        metadata = Value::Object(Map::new());
    }
    if let Some(map) = metadata.as_object_mut() {
        map.insert(TYPE_KEY.to_owned(), Value::String(type_name.to_owned()));
    }
    set_metadata(conn, meta, id, &metadata)
}

/// Remove the type from the namespace `id`, leaving the rest of its metadata intact. The
/// namespace reverts to untyped: it enforces no contract of its own and falls back to its
/// nearest typed ancestor's, exactly as an ordinary namespace does.
///
/// The inverse of [`set_type`], for a namespace typed by mistake — a `debugging`
/// investigation started in the wrong place, say. Items already placed are untouched; only
/// the rule governing future writes changes.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn clear_type(conn: &Connection, meta: &WriteMeta, id: NamespaceId) -> Result<()> {
    let mut metadata = get_metadata(conn, id)?.unwrap_or_else(|| Value::Object(Map::new()));
    if let Some(map) = metadata.as_object_mut() {
        map.remove(TYPE_KEY);
    }
    set_metadata(conn, meta, id, &metadata)
}

/// The type recorded on the namespace `path`, or `None` if it is untyped (or
/// does not exist).
///
/// # Errors
/// Returns an error if the path is malformed or a query fails.
pub fn get_type(conn: &Connection, path: &str) -> Result<Option<String>> {
    let Some(id) = get(conn, path)? else {
        return Ok(None);
    };
    get_type_by_id(conn, id)
}

/// The strategy type recorded on the namespace `id`, or `None` if it is untyped.
///
/// # Errors
/// Returns an error if the query fails.
pub fn get_type_by_id(conn: &Connection, id: NamespaceId) -> Result<Option<String>> {
    Ok(get_metadata(conn, id)?
        .and_then(|m| m.get(TYPE_KEY).and_then(Value::as_str).map(str::to_owned)))
}

/// The strategy type governing `path`: its own `type`, else the nearest **typed
/// ancestor**'s. An investigation types its root namespace once
/// (`memory/<repo>/<name>`), and every sub-namespace inside it inherits that type — so a
/// node filed under `memory/jkb/heisenbug/hypotheses` resolves the same strategy as the
/// root. Returns the `(namespace path, type)` pair so callers can report *where* the type
/// came from, or `None` if neither `path` nor any ancestor is typed.
///
/// # Errors
/// Returns an error if the path is malformed or a query fails.
pub fn effective_type(conn: &Connection, path: &str) -> Result<Option<(String, String)>> {
    // One query over the whole ancestor chain rather than a query per level: this runs on
    // every placement now (design D33.2), and ingest places one item per chunk.
    let ancestors = ancestor_paths(&normalize(path)?);
    let placeholders = (1..=ancestors.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    // `placeholders` is generated from a count, never from user input; the paths themselves
    // are bound parameters.
    let type_expr = type_expr();
    let sql = format!(
        "SELECT path, {type_expr} FROM namespaces
         WHERE path IN ({placeholders}) AND {type_expr} IS NOT NULL
         ORDER BY length(path) DESC LIMIT 1"
    );
    let found = conn
        .prepare_cached(&sql)?
        .query_row(params_from_iter(ancestors.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .optional()?;
    Ok(found)
}

/// `path` and every ancestor of it, deepest first (`a/b/c` → `a/b/c`, `a/b`, `a`).
fn ancestor_paths(path: &str) -> Vec<String> {
    let mut out = vec![path.to_owned()];
    let mut current = path;
    while let Some(cut) = current.rfind('/') {
        current = &current[..cut];
        out.push(current.to_owned());
    }
    out
}

/// Move the subtree rooted at `from` to `to`, rewriting paths and re-pointing the
/// moved root's parent. Namespace ids are stable, so placements and edges are
/// unaffected. Returns the number of namespaces moved.
///
/// # Errors
/// Returns a validation error if `from` is missing, `to` already exists, or the
/// move would nest `from` inside itself; otherwise a database error.
pub fn move_subtree(conn: &Connection, meta: &WriteMeta, from: &str, to: &str) -> Result<usize> {
    let from = normalize(from)?;
    let to = normalize(to)?;
    if to == from || to.starts_with(&format!("{from}/")) {
        return Err(TypeError::Validation(format!("cannot move '{from}' into itself")).into());
    }
    let Some(root_id) = get(conn, &from)?.map(NamespaceId::get) else {
        return Err(TypeError::Validation(format!("namespace '{from}' does not exist")).into());
    };
    if get(conn, &to)?.is_some() {
        return Err(TypeError::Validation(format!("target '{to}' already exists")).into());
    }

    let new_parent: Option<i64> = match to.rsplit_once('/') {
        Some((parent, _)) => Some(ensure(conn, parent)?.get()),
        None => None,
    };

    let rows = subtree(conn, &from)?;
    for (id, path) in &rows {
        let new_path = if *path == from {
            to.clone()
        } else {
            format!("{to}{}", &path[from.len()..])
        };
        conn.prepare_cached("UPDATE namespaces SET path = ?1 WHERE id = ?2")?
            .execute(params![new_path, id.get()])?;
    }
    conn.prepare_cached("UPDATE namespaces SET parent_id = ?1 WHERE id = ?2")?
        .execute(params![new_parent, root_id])?;
    changelog::append(
        conn,
        meta,
        "update",
        "namespaces",
        &root_id.to_string(),
        Some(&json!({ "path": from })),
        Some(&json!({ "path": to })),
    )?;
    Ok(rows.len())
}

/// Remove an **empty** namespace — no child namespaces and no item placements — recording
/// the deletion in the changelog. Refuses a non-empty namespace so content is never
/// silently orphaned. (A mount on the namespace is removed by cascade.)
///
/// # Errors
/// Returns a validation error if the namespace does not exist or is not empty; otherwise a
/// database error.
pub fn remove(conn: &Connection, meta: &WriteMeta, path: &str) -> Result<()> {
    let normalized = normalize(path)?;
    let Some(id) = get(conn, &normalized)? else {
        return Err(
            TypeError::Validation(format!("namespace '{normalized}' does not exist")).into(),
        );
    };
    let children: i64 = conn
        .prepare_cached("SELECT count(*) FROM namespaces WHERE parent_id = ?1")?
        .query_row([id.get()], |r| r.get(0))?;
    if children > 0 {
        return Err(TypeError::Validation(format!(
            "namespace '{normalized}' has {children} child namespace(s); remove or move them first"
        ))
        .into());
    }
    let placements: i64 = conn
        .prepare_cached("SELECT count(*) FROM placements WHERE namespace_id = ?1")?
        .query_row([id.get()], |r| r.get(0))?;
    if placements > 0 {
        return Err(TypeError::Validation(format!(
            "namespace '{normalized}' holds {placements} item placement(s); move or unplace them first"
        ))
        .into());
    }
    conn.prepare_cached("DELETE FROM namespaces WHERE id = ?1")?
        .execute([id.get()])?;
    changelog::append(
        conn,
        meta,
        "delete",
        "namespaces",
        &id.get().to_string(),
        Some(&json!({ "path": normalized })),
        None,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        children, clear_type, ensure, get, get_metadata, get_type, move_subtree, normalize, roots,
        set_metadata, set_type, subtree, subtree_leaf_count, subtree_leaf_counts,
    };
    use crate::item::{upsert, NewItem};
    use crate::{placement, Db};
    use jkb_types::PlacementRole;
    use serde_json::json;

    #[test]
    fn subtree_leaf_count_spans_descendants_dedups_and_honours_terminal() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            let deep = ensure(conn, "tasks/jkb/openspec")?;
            let leaf = ensure(conn, "tasks/jkb/openspec/changes")?;
            let new = |uid: &str| NewItem {
                uid: uid.to_owned(),
                kind: "task".to_owned(),
                content: None,
                content_hash: None,
                mime: None,
            };
            // `a` lives (via a mirror) under a descendant, and is *also* placed on an
            // ancestor in the same subtree — it must be counted once, not twice.
            let a = upsert(conn, meta, &new("task:a"))?;
            placement::place(conn, meta, a, leaf, PlacementRole::Reference, 0)?;
            placement::place(conn, meta, a, deep, PlacementRole::Reference, 0)?;
            // `b` is a second, distinct leaf under the descendant.
            let b = upsert(conn, meta, &new("task:b"))?;
            placement::place(conn, meta, b, leaf, PlacementRole::Reference, 0)?;
            // `c` is done — hidden unless terminal items are included.
            let c = upsert(conn, meta, &new("task:c"))?;
            placement::place(conn, meta, c, leaf, PlacementRole::Reference, 0)?;
            conn.execute("UPDATE items SET status = 'done' WHERE id = ?1", [c.get()])?;
            Ok(())
        })
        .unwrap();

        // Two visible distinct leaves (a counted once despite two placements; c excluded).
        let visible = db
            .read(|conn| subtree_leaf_count(conn, "tasks/jkb", false))
            .unwrap();
        assert_eq!(visible, 2);
        // Including terminals reveals the done task.
        let all = db
            .read(|conn| subtree_leaf_count(conn, "tasks/jkb", true))
            .unwrap();
        assert_eq!(all, 3);
        // A sibling branch with no placements has no leaves.
        db.write_txn("t", |conn, _m| ensure(conn, "tasks/other"))
            .unwrap();
        let none = db
            .read(|conn| subtree_leaf_count(conn, "tasks/other", true))
            .unwrap();
        assert_eq!(none, 0);
    }

    #[test]
    fn subtree_leaf_counts_matches_per_child_and_covers_roots() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            let a_leaf = ensure(conn, "tasks/jkb/a/deep")?;
            let b_leaf = ensure(conn, "tasks/other/b")?;
            ensure(conn, "tasks/empty")?; // a child with no placements at all
            let new = |uid: &str| NewItem {
                uid: uid.to_owned(),
                kind: "task".to_owned(),
                content: None,
                content_hash: None,
                mime: None,
            };
            let x = upsert(conn, meta, &new("task:x"))?;
            placement::place(conn, meta, x, a_leaf, PlacementRole::Reference, 0)?;
            let y = upsert(conn, meta, &new("task:y"))?;
            placement::place(conn, meta, y, b_leaf, PlacementRole::Reference, 0)?;
            let z = upsert(conn, meta, &new("task:z"))?;
            placement::place(conn, meta, z, b_leaf, PlacementRole::Reference, 0)?;
            conn.execute("UPDATE items SET status = 'done' WHERE id = ?1", [z.get()])?;
            Ok(())
        })
        .unwrap();

        // The batched grouped query must agree with the per-child `subtree_leaf_count` for
        // every direct child of `tasks`, for both the default and terminal-inclusive views.
        for include_terminal in [false, true] {
            let batched = db
                .read(move |conn| subtree_leaf_counts(conn, Some("tasks"), include_terminal))
                .unwrap();
            let kids = db.read(|conn| children(conn, "tasks")).unwrap();
            for (id, path) in kids {
                let p = path.clone();
                let expected = db
                    .read(move |conn| subtree_leaf_count(conn, &p, include_terminal))
                    .unwrap();
                assert_eq!(batched.get(&id).copied().unwrap_or(0), expected, "{path}");
            }
        }

        // The root-level variant (`parent = None`) counts each top-level namespace's subtree.
        let root_counts = db
            .read(|conn| subtree_leaf_counts(conn, None, true))
            .unwrap();
        let root_ids = db.read(roots).unwrap();
        for (id, path) in root_ids {
            let p = path.clone();
            let expected = db
                .read(move |conn| subtree_leaf_count(conn, &p, true))
                .unwrap();
            assert_eq!(
                root_counts.get(&id).copied().unwrap_or(0),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn normalize_rejects_bad_paths_and_keeps_case() {
        assert!(normalize("").is_err());
        assert!(normalize("a//b").is_err());
        assert!(normalize("books/../secret").is_err());
        assert!(normalize("a/./b").is_err());
        assert_eq!(normalize("Books/SICP").unwrap(), "Books/SICP");
    }

    #[test]
    fn ensure_creates_ancestors_and_subtree_lists_them() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("test", |conn, _meta| {
            ensure(conn, "books/sicp/ch1")?;
            ensure(conn, "books/dragon")?;
            Ok(())
        })
        .unwrap();

        let paths: Vec<String> = db
            .read(|conn| subtree(conn, "books"))
            .unwrap()
            .into_iter()
            .map(|(_, path)| path)
            .collect();
        assert_eq!(
            paths,
            ["books", "books/dragon", "books/sicp", "books/sicp/ch1"]
        );
    }

    #[test]
    fn moving_a_subtree_reindexes_paths_and_keeps_placements() {
        let db = Db::open_in_memory().unwrap();
        let item = db
            .write_txn("t", |conn, meta| {
                let leaf = ensure(conn, "tasks/mono/backend")?;
                let item = upsert(
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
                placement::place(conn, meta, item, leaf, PlacementRole::Primary, 0)?;
                Ok(item)
            })
            .unwrap();

        // Move tasks/mono -> tasks/monorepo (2 namespaces: the root and backend).
        let moved = db
            .write_txn("t", |conn, meta| {
                move_subtree(conn, meta, "tasks/mono", "tasks/monorepo")
            })
            .unwrap();
        assert_eq!(moved, 2);

        // The item now resolves under the new path; the old path is gone.
        let new_leaf = db
            .read(|conn| super::get(conn, "tasks/monorepo/backend"))
            .unwrap()
            .unwrap();
        let items = db
            .read(move |conn| placement::items_in(conn, new_leaf, None))
            .unwrap();
        assert_eq!(items, vec![item]);
        assert!(db
            .read(|conn| super::get(conn, "tasks/mono/backend"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn metadata_round_trips_on_a_namespace() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .write_txn("t", |conn, meta| {
                let id = ensure(conn, "docs/plan/backend")?;
                set_metadata(
                    conn,
                    meta,
                    id,
                    &json!({ "header_line": "## 1. Backend", "position": 0 }),
                )?;
                Ok(id)
            })
            .unwrap();

        let got = db
            .read(move |conn| get_metadata(conn, id))
            .unwrap()
            .unwrap();
        assert_eq!(got["header_line"], "## 1. Backend");
        assert_eq!(got["position"], 0);

        // A namespace with no metadata set reads back the `'{}'` default (no header_line).
        let bare = db
            .write_txn("t", |conn, _m| ensure(conn, "docs/plan"))
            .unwrap();
        let bare_meta = db
            .read(move |conn| get_metadata(conn, bare))
            .unwrap()
            .unwrap();
        assert!(bare_meta.get("header_line").is_none());
    }

    #[test]
    fn a_namespace_type_is_inherited_by_its_subtree_and_merges_with_metadata() {
        use super::{effective_type, get_type, set_metadata, set_type};

        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            let root = ensure(conn, "memory/jkb/heisenbug")?;
            // Pre-existing metadata must survive typing the namespace.
            set_metadata(conn, meta, root, &json!({ "position": 3 }))?;
            set_type(conn, meta, root, "debugging")?;
            ensure(conn, "memory/jkb/heisenbug/hypotheses")?;
            ensure(conn, "memory/jkb/other")?;
            Ok(())
        })
        .unwrap();

        let got = db
            .read(|conn| get_type(conn, "memory/jkb/heisenbug"))
            .unwrap();
        assert_eq!(got.as_deref(), Some("debugging"));
        let merged = db
            .read(|conn| {
                let id = super::get(conn, "memory/jkb/heisenbug")?.unwrap();
                get_metadata(conn, id)
            })
            .unwrap()
            .unwrap();
        assert_eq!(merged["position"], 3, "typing must not clobber metadata");

        // A child inherits the nearest typed ancestor's strategy…
        let (source, type_name) = db
            .read(|conn| effective_type(conn, "memory/jkb/heisenbug/hypotheses"))
            .unwrap()
            .unwrap();
        assert_eq!(source, "memory/jkb/heisenbug");
        assert_eq!(type_name, "debugging");

        // …but an unrelated sibling and its own `type` read stay untyped.
        assert!(db
            .read(|conn| get_type(conn, "memory/jkb/heisenbug/hypotheses"))
            .unwrap()
            .is_none());
        assert!(db
            .read(|conn| effective_type(conn, "memory/jkb/other"))
            .unwrap()
            .is_none());
        // A path that does not exist at all is untyped, not an error.
        assert!(db
            .read(|conn| effective_type(conn, "nowhere/at/all"))
            .unwrap()
            .is_none());
    }

    /// Clearing is the inverse of setting: a namespace typed by mistake reverts to
    /// untyped and falls back to inheriting its nearest typed ancestor's contract.
    #[test]
    fn clearing_a_type_reverts_a_namespace_to_untyped() {
        use crate::nstype::for_namespace;
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            let id = ensure(conn, "memory/oops")?;
            set_type(conn, meta, id, "debugging")
        })
        .unwrap();
        assert_eq!(
            db.read(|conn| get_type(conn, "memory/oops"))
                .unwrap()
                .as_deref(),
            Some("debugging")
        );

        db.write_txn("t", |conn, meta| {
            let id = get(conn, "memory/oops")?.expect("exists");
            clear_type(conn, meta, id)
        })
        .unwrap();
        assert!(db
            .read(|conn| get_type(conn, "memory/oops"))
            .unwrap()
            .is_none());
        assert!(db
            .read(|conn| for_namespace(conn, "memory/oops"))
            .unwrap()
            .is_none());
    }

    /// A type is **not** a location marker (design D33.5): nothing resolves "which namespace
    /// carries the `tasks` contract" to find the tasks root, so several namespaces may carry
    /// the same contract without one silently winning. `tasks/` is the tasks root because
    /// the D32 layout reserves it.
    #[test]
    fn a_contract_may_type_several_namespaces_and_locates_nothing() {
        use crate::{nstype, task};
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            for path in ["work/todo", "alpha/queue", "a/b/c/d"] {
                let id = ensure(conn, path)?;
                set_type(conn, meta, id, nstype::tasks::NAME)?;
            }
            Ok(())
        })
        .unwrap();

        // All three hold the contract; none of them is "the tasks root".
        for path in ["work/todo", "alpha/queue", "a/b/c/d"] {
            assert_eq!(
                db.read(move |conn| get_type(conn, path))
                    .unwrap()
                    .as_deref(),
                Some(nstype::tasks::NAME),
                "{path}"
            );
        }
        assert_eq!(task::DEFAULT_ROOT, "tasks");
        assert_eq!(task::DEFAULT_HOME, "tasks/inbox");
    }
}
