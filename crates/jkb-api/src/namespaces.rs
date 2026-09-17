//! `jkb ns ls` and `jkb ns mv` through the ops (tasks S6.4 stage 5).
//!
//! **A client's move is held to the file roots.** Renaming a subtree renames the sections of every `tasks.md`
//! mounted in it and the placements every task under it renders, so the host's sync rewrites those
//! files: a client may move a subtree only when every mount it touches, and every item placed in it,
//! is inside its roots, and each task's tasks.md line still comes back from its file afterwards.

use jkb_core::query::{Query, Scope};
use jkb_core::{ns, nstype, WriteMeta};
use rusqlite::Connection;

use crate::kb::Budget;
use crate::tasks::{check_line, line_problem, ns_writable, writable_id, FileRoots};
use crate::{ApiError, ErrorCode};

/// `ns.list`: the child namespaces of `scope`, or the top-level ones, by path, within `budget`.
///
/// # Errors
/// A malformed path, or a failed read.
pub fn list(
    conn: &Connection,
    scope: Option<&str>,
    budget: &mut Budget,
) -> Result<Vec<String>, ApiError> {
    let rows = match scope {
        Some(path) => ns::children(conn, path)?,
        None => ns::roots(conn)?,
    };
    Ok(rows
        .into_iter()
        .map(|(_, p)| p)
        .take_while(|p| budget.take(p))
        .collect())
}

/// The most items a client's `ns.mv` may carry. Each is judged against the roots, and each filed
/// task's tasks.md line checked, in the one transaction.
pub const MAX_MOVED_ITEMS: usize = 1000;

/// The most namespaces a client's `ns.mv` may carry: each is a row rewritten and logged.
pub const MAX_MOVED_NAMESPACES: usize = 1000;

fn invalid(why: String) -> ApiError {
    ApiError::with_code(ErrorCode::Invalid, why)
}

/// Whether moving `path` — or moving something onto it — would move a reserved namespace (a
/// [`nstype::RESERVED_TYPES`] root, an ancestor of one, or anything under `_sys`), which readers find
/// by its fixed path.
fn touches_reserved(path: &str) -> bool {
    path == "_sys"
        || path.starts_with("_sys/")
        || nstype::RESERVED_TYPES
            .iter()
            .any(|(r, _)| *r == path || r.starts_with(&format!("{path}/")))
}

/// `ns.mv`: move the subtree at `from` to `to`, answering how many namespaces moved.
///
/// Under `roots` a move is held to them (see the module docs): refused for a reserved namespace, past
/// [`MAX_MOVED_ITEMS`] or [`MAX_MOVED_NAMESPACES`], when a mount it touches or an item in it is
/// outside the roots, or when it would leave a filed task's line unreadable. On the host the move is
/// the core's alone, as it always was.
///
/// # Errors
/// [`ErrorCode::Forbidden`] or [`ErrorCode::Invalid`] under `roots`, a move the core refuses, or a
/// failed write.
pub fn mv(
    conn: &Connection,
    meta: &WriteMeta,
    from: &str,
    to: &str,
    roots: Option<&FileRoots>,
) -> Result<usize, ApiError> {
    let Some(roots) = roots else {
        return Ok(ns::move_subtree(conn, meta, from, to)?);
    };
    let from_path = ns::normalize(from)?;
    let to_path = ns::normalize(to)?;
    if touches_reserved(&from_path) || touches_reserved(&to_path) {
        return Err(ApiError::with_code(
            ErrorCode::Forbidden,
            format!(
                "`{from_path}` -> `{to_path}` moves a reserved namespace, which jkb finds by its \
                 path; run it on the host if you mean it"
            ),
        ));
    }
    let ids = Query {
        scope: Scope::Subtree(from_path.clone()),
        limit: Some(MAX_MOVED_ITEMS + 1),
        ..Query::default()
    }
    .evaluate(conn)?;
    if ids.len() > MAX_MOVED_ITEMS {
        return Err(invalid(format!(
            "`{from_path}` holds more than {MAX_MOVED_ITEMS} items, more than a client moves at \
             once; move it on the host"
        )));
    }
    let namespaces = ns::subtree(conn, &from_path)?;
    if namespaces.len() > MAX_MOVED_NAMESPACES {
        return Err(invalid(format!(
            "`{from_path}` has more than {MAX_MOVED_NAMESPACES} namespaces below it, more than a \
             client moves at once; move it on the host"
        )));
    }
    // The mount the subtree is under, the one it would be under, and every mount inside it.
    ns_writable(conn, &from_path, Some(roots))?;
    ns_writable(conn, &to_path, Some(roots))?;
    for (_, path) in &namespaces {
        ns_writable(conn, path, Some(roots))?;
    }
    let mut uid_of = conn
        .prepare_cached("SELECT uid FROM items WHERE id = ?1")
        .map_err(jkb_core::Error::from)?;
    let mut lines = Vec::with_capacity(ids.len());
    for id in &ids {
        let uid: String = uid_of
            .query_row([id.get()], |r| r.get(0))
            .map_err(jkb_core::Error::from)?;
        writable_id(conn, *id, &uid, Some(roots))?;
        let before = line_problem(conn, &uid)?;
        lines.push((uid, before));
    }
    let moved = ns::move_subtree(conn, meta, &from_path, &to_path)?;
    for (uid, before) in lines {
        check_line(conn, &uid, before.as_deref())?;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests;
