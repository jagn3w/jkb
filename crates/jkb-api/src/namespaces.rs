//! `jkb ns ls` and `jkb ns mv` through the ops (tasks S6.4 stage 5).
//!
//! **A client's move is held to the file roots.** Renaming a subtree renames the sections of every `tasks.md`
//! mounted in it and the placements every task under it renders, so the host's sync rewrites those
//! files: a client may move a subtree only when every mount it touches, and every item placed in it,
//! is inside its roots, and each task's tasks.md line still comes back from its file afterwards.

use jkb_core::query::{Query, Scope};
use jkb_core::{ns, WriteMeta};
use rusqlite::Connection;

use crate::kb::Budget;
use crate::tasks::{ns_writable, writable_id, FileRoots};
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

/// The most tasks filed in a tasks.md a client's `ns.mv` may carry: each one's line is checked, from
/// its file rendered before the move and after.
pub const MAX_MOVED_FILED: usize = 64;

fn sync_problems(
    conn: &Connection,
    ids: &[jkb_types::ItemId],
) -> Result<Vec<Option<String>>, ApiError> {
    jkb_sync::filed_task_problems(conn, ids).map_err(|e| match e {
        jkb_sync::Error::Core(e) => ApiError::from(e),
        other => ApiError::with_code(ErrorCode::Internal, other.to_string()),
    })
}

fn invalid(why: String) -> ApiError {
    ApiError::with_code(ErrorCode::Invalid, why)
}

/// `ns.mv`: move the subtree at `from` to `to`, answering how many namespaces moved.
///
/// Under `roots` a move is held to them (see the module docs): refused for a reserved namespace, past
/// [`MAX_MOVED_ITEMS`], [`MAX_MOVED_NAMESPACES`] or [`MAX_MOVED_FILED`], when a mount it touches or an item in it is
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
    if ns::is_fixed(&from_path) || ns::is_fixed(&to_path) {
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
    let mut filed = Vec::new();
    for id in &ids {
        let uid: String = uid_of
            .query_row([id.get()], |r| r.get(0))
            .map_err(jkb_core::Error::from)?;
        writable_id(conn, *id, &uid, Some(roots))?;
        if jkb_core::binding::serializer_for(conn, *id)?.as_deref() == Some("tasks") {
            filed.push((*id, uid));
        }
    }
    if filed.len() > MAX_MOVED_FILED {
        return Err(invalid(format!(
            "`{from_path}` holds more than {MAX_MOVED_FILED} tasks filed in a tasks.md, each of \
             whose lines a client's move checks; move it on the host"
        )));
    }
    // Each file rendered once before the move and once after, not once per task.
    let filed_ids: Vec<_> = filed.iter().map(|(id, _)| *id).collect();
    let before = sync_problems(conn, &filed_ids)?;
    let moved = ns::move_subtree(conn, meta, &from_path, &to_path)?;
    let after = sync_problems(conn, &filed_ids)?;
    for (((_, uid), was), now) in filed.iter().zip(before).zip(after) {
        if let (None, Some(problem)) = (was, now) {
            return Err(invalid(format!(
                "{uid} is written into a tasks.md, and its line would not come back from the file \
                 as written: {problem}"
            )));
        }
    }
    Ok(moved)
}

#[cfg(test)]
mod tests;
