//! `task.staging`: what `jkb staging ls` reads from the database (tasks S6.4 stage 5).
//!
//! The listing groups a repo's tasks by where their work lands and asks git the rest where the command
//! runs, so this is the whole database half in one read: every task of the repo that has a land
//! target, with what a row shows and what the land gate needs. The findings are read separately
//! (`task.review_findings`), once per distinct set of review namespaces.

use std::collections::BTreeMap;

use jkb_core::{item, tag, task, transition};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::kb::Budget;
use crate::sessions::check_name;
use crate::ApiError;

/// One task of a repo whose work lands somewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagingTask {
    /// Its uid.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// Its status.
    pub status: String,
    /// Every facet value it carries, by facet.
    pub tags: BTreeMap<String, Vec<String>>,
    /// Where its work lands, from its transition history.
    pub land_target: String,
    /// Whether any subtask is unfinished.
    pub open_subtasks: bool,
}

/// `task.staging`: the tasks of `repo` (`repo=`) with a land target, in id order, within `budget`.
///
/// **A spent batch is left out unless `all`** — one whose every task has finished, the rule
/// `jkb staging ls` hides it by — so the answer, and the bodies read for it, grow with the work in
/// flight. **Residual, stated:** whether a batch is spent needs every task's land target, which is one
/// indexed history read per task the repo has ever worked.
///
/// # Errors
/// [`crate::ErrorCode::Invalid`] for a malformed repo key, or a failed read.
pub fn staging(
    conn: &Connection,
    repo: &str,
    all: bool,
    budget: &mut Budget,
) -> Result<Vec<StagingTask>, ApiError> {
    check_name("repo key", repo)?;
    // Status first, without bodies, for every task that has a land target: what decides a batch is
    // spent. Bodies, tags and subtasks are read only for the rows kept.
    let mut status_of = conn
        .prepare_cached("SELECT status FROM items WHERE id = ?1")
        .map_err(jkb_core::Error::from)?;
    let mut targeted = Vec::new();
    for id in jkb_core::location::tasks_in_repo(repo).evaluate(conn)? {
        if let Some(target) = transition::land_target(conn, id)? {
            let status: Option<String> = status_of
                .query_row([id.get()], |r| r.get(0))
                .map_err(jkb_core::Error::from)?;
            targeted.push((id, target, status.unwrap_or_default()));
        }
    }
    let mut spent: BTreeMap<String, bool> = BTreeMap::new();
    for (_, target, status) in &targeted {
        let done = jkb_types::TaskStatus::is_terminal_str(Some(status.as_str()));
        *spent.entry(target.clone()).or_insert(true) &= done;
    }
    targeted.retain(|(_, target, _)| all || !spent.get(target).copied().unwrap_or(false));
    let ids: Vec<_> = targeted.iter().map(|(id, _, _)| *id).collect();
    let metas = item::get_many(conn, &ids)?;
    let mut tags = tag::applications_for(conn, &ids)?;
    let mut out = Vec::new();
    for (id, land_target, status) in targeted {
        let Some(meta) = metas.get(&id) else { continue };
        let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (facet, value) in tags.remove(&id).unwrap_or_default() {
            grouped.entry(facet).or_default().push(value);
        }
        let row = StagingTask {
            uid: meta.uid.clone(),
            title: item::title_of(meta),
            status,
            tags: grouped,
            land_target,
            open_subtasks: !task::subtasks_all_terminal(conn, id)?,
        };
        if !budget.take(&row) {
            break;
        }
        out.push(row);
    }
    Ok(out)
}
