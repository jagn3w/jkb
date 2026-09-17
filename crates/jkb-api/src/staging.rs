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

/// The most tasks one answer carries.
pub const MAX_STAGING_TASKS: usize = 10_000;

/// `task.staging`: the tasks of `repo` (`repo=`) with a land target, in id order, and whether the list
/// was cut at [`MAX_STAGING_TASKS`]. One read.
///
/// # Errors
/// [`crate::ErrorCode::Invalid`] for a malformed repo key, or a failed read.
pub fn staging(conn: &Connection, repo: &str) -> Result<(Vec<StagingTask>, bool), ApiError> {
    check_name("repo key", repo)?;
    let ids = jkb_core::location::tasks_in_repo(repo).evaluate(conn)?;
    let metas = item::get_many(conn, &ids)?;
    let mut tags = tag::applications_for(conn, &ids)?;
    let mut out = Vec::new();
    for id in ids {
        let Some(meta) = metas.get(&id) else { continue };
        let Some(land_target) = transition::land_target(conn, id)? else {
            continue;
        };
        if out.len() == MAX_STAGING_TASKS {
            return Ok((out, true));
        }
        let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (facet, value) in tags.remove(&id).unwrap_or_default() {
            grouped.entry(facet).or_default().push(value);
        }
        out.push(StagingTask {
            uid: meta.uid.clone(),
            title: item::title_of(meta),
            status: meta.status.clone().unwrap_or_default(),
            tags: grouped,
            land_target,
            open_subtasks: !task::subtasks_all_terminal(conn, id)?,
        });
    }
    Ok((out, false))
}
