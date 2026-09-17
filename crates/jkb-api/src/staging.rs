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
/// `jkb staging ls` hides it by — so the answer grows with the work in flight, not with every task ever
/// landed in the repo. Only tasks with a land target are loaded.
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
    let mut targeted = Vec::new();
    for id in jkb_core::location::tasks_in_repo(repo).evaluate(conn)? {
        if let Some(target) = transition::land_target(conn, id)? {
            targeted.push((id, target));
        }
    }
    let ids: Vec<_> = targeted.iter().map(|(id, _)| *id).collect();
    let metas = item::get_many(conn, &ids)?;
    let status = |id| {
        metas
            .get(id)
            .and_then(|m: &item::ItemMeta| m.status.clone())
            .unwrap_or_default()
    };
    let mut spent: BTreeMap<&str, bool> = BTreeMap::new();
    for (id, target) in &targeted {
        let done = jkb_types::TaskStatus::is_terminal_str(Some(status(id).as_str()));
        *spent.entry(target.as_str()).or_insert(true) &= done;
    }
    let mut tags = tag::applications_for(conn, &ids)?;
    let mut out = Vec::new();
    for (id, land_target) in &targeted {
        let Some(meta) = metas.get(id) else { continue };
        if !all && spent.get(land_target.as_str()).copied().unwrap_or(false) {
            continue;
        }
        let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (facet, value) in tags.remove(id).unwrap_or_default() {
            grouped.entry(facet).or_default().push(value);
        }
        let row = StagingTask {
            uid: meta.uid.clone(),
            title: item::title_of(meta),
            status: meta.status.clone().unwrap_or_default(),
            tags: grouped,
            land_target: land_target.clone(),
            open_subtasks: !task::subtasks_all_terminal(conn, *id)?,
        };
        if !budget.take(&row) {
            break;
        }
        out.push(row);
    }
    Ok(out)
}
