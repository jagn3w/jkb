//! `jkb ns ls` and `jkb ns mv` through the ops (tasks S6.4 stage 5).
//!
//! **A move is held to the file roots.** Renaming a subtree renames the sections of every `tasks.md`
//! mounted in it and the placements every task under it renders, so the host's sync rewrites those
//! files: a client may move a subtree only when every mount it touches, and every item placed in it,
//! is inside its roots, and each task's tasks.md line still comes back from its file afterwards.

use jkb_core::query::{Query, Scope};
use jkb_core::{item, ns, WriteMeta};
use rusqlite::Connection;

use crate::tasks::{check_line, line_problem, ns_writable, writable_id, FileRoots};
use crate::{ApiError, ErrorCode};

/// `ns.list`: the child namespaces of `scope`, or the top-level ones, by path.
///
/// # Errors
/// A malformed path, or a failed read.
pub fn list(conn: &Connection, scope: Option<&str>) -> Result<Vec<String>, ApiError> {
    let rows = match scope {
        Some(path) => ns::children(conn, path)?,
        None => ns::roots(conn)?,
    };
    Ok(rows.into_iter().map(|(_, p)| p).collect())
}

/// The most items a client's `ns.mv` may carry. Each is judged against the roots and its line checked
/// in the one transaction.
pub const MAX_MOVED_ITEMS: usize = 10_000;

/// `ns.mv`: move the subtree at `from` to `to`, answering how many namespaces moved.
///
/// # Errors
/// [`ErrorCode::Forbidden`] under `roots` (see the module docs), [`ErrorCode::Invalid`] for a move the
/// core refuses or one carrying too many items, or a failed write.
pub fn mv(
    conn: &Connection,
    meta: &WriteMeta,
    from: &str,
    to: &str,
    roots: Option<&FileRoots>,
) -> Result<usize, ApiError> {
    let from_path = ns::normalize(from)?;
    let ids = Query {
        scope: Scope::Subtree(from_path.clone()),
        ..Query::default()
    }
    .evaluate(conn)?;
    let mut lines = Vec::new();
    if roots.is_some() {
        if ids.len() > MAX_MOVED_ITEMS {
            return Err(ApiError::with_code(
                ErrorCode::Invalid,
                format!(
                    "`{from_path}` holds {} items, more than a client moves at once \
                     ({MAX_MOVED_ITEMS}); move it on the host",
                    ids.len()
                ),
            ));
        }
        // The mount the subtree is under, the one it would be under, and every mount inside it.
        ns_writable(conn, &from_path, roots)?;
        ns_writable(conn, to, roots)?;
        for (_, path) in ns::subtree(conn, &from_path)? {
            ns_writable(conn, &path, roots)?;
        }
    }
    let metas = item::get_many(conn, &ids)?;
    for id in &ids {
        let Some(m) = metas.get(id) else { continue };
        writable_id(conn, *id, &m.uid, roots)?;
        lines.push((m.uid.clone(), line_problem(conn, &m.uid)?));
    }
    let moved = ns::move_subtree(conn, meta, &from_path, to)?;
    for (uid, before) in lines {
        check_line(conn, &uid, before.as_deref())?;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests;
