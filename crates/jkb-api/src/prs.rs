//! `jkb task pr` and `jkb task close-merged` through the ops (tasks S6.4 stage 5).
//!
//! The pull request is looked up where the command runs (`gh`, the checkout); these ops read what the
//! history says about a task's landing and record what the client found. `task.close_merged` takes the
//! client's answer to "did it merge?" as a fact for the lifecycle's `observed_landed` guard — a caller
//! statement, as `task.landed` is — and is held to the task-write rules.

use jkb_core::lifecycle::{self, TaskEvent};
use jkb_core::{item, task, transition, WriteMeta};
use jkb_fsm::Fact;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::sessions::check_name;
use crate::tasks::{no_item, writable, FileRoots};
use crate::{ApiError, ErrorCode};

/// What a task's history says about where its work went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrFacts {
    /// The task's full uid.
    pub uid: String,
    /// The pull request most recently recorded for it.
    pub pr: Option<i64>,
    /// The branch it most recently recorded.
    pub branch: Option<String>,
    /// A landing jkb recorded still speaks for the work in flight.
    pub live_landing: bool,
    /// A recorded landing was superseded: `(onto, the event that retired it, when)`.
    pub superseded: Option<(Option<String>, String, String)>,
    /// When the task was last put back to work.
    pub resumed_at: Option<String>,
    /// Whether this client may write the task — `false` for one filed outside `jkb serve`'s file
    /// roots, which `close-merged` holds rather than failing the whole run on.
    pub writable: bool,
}

/// `task.pr_facts`.
///
/// # Errors
/// [`ErrorCode::NotFound`], or a failed read.
pub fn facts(
    conn: &Connection,
    reference: &str,
    roots: Option<&FileRoots>,
) -> Result<PrFacts, ApiError> {
    let id = task::resolve_ref(conn, reference)?.ok_or_else(|| no_item(reference))?;
    let writable = match writable(conn, reference, roots) {
        Ok(_) => true,
        Err(e) if e.code == ErrorCode::Forbidden => false,
        Err(e) => return Err(e),
    };
    let uid = item::get(conn, id)?.ok_or_else(|| no_item(reference))?.uid;
    let landing = transition::landing(conn, id)?;
    Ok(PrFacts {
        uid,
        pr: landing.pr_number(),
        branch: transition::latest_with_branch(conn, id)?.and_then(|r| r.labels.branch),
        live_landing: landing.live().is_some(),
        superseded: landing.superseded().map(|(landed, resumed)| {
            (
                landed.labels.onto.clone(),
                resumed.event.clone(),
                resumed.at.clone(),
            )
        }),
        resumed_at: landing.resumed_at().map(str::to_owned),
        writable,
    })
}

/// `task.open_in_repo`: the uids of the repo's (`repo=`) tasks that are not finished, in id order.
///
/// # Errors
/// [`ErrorCode::Invalid`] for a malformed repo key, or a failed read.
pub fn open_in_repo(conn: &Connection, repo: &str) -> Result<Vec<String>, ApiError> {
    check_name("repo key", repo)?;
    let ids = jkb_core::location::tasks_in_repo(repo).evaluate(conn)?;
    let metas = item::get_many(conn, &ids)?;
    Ok(ids
        .iter()
        .filter_map(|id| metas.get(id))
        .filter(|m| !jkb_types::TaskStatus::is_terminal_str(m.status.as_deref()))
        .map(|m| m.uid.clone())
        .collect())
}

fn check_number(number: i64) -> Result<(), ApiError> {
    if number <= 0 {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            "a pull request number is positive",
        ));
    }
    Ok(())
}

/// `task.pr_record`: note pull request `number` in the task's history, beside its latest branch.
///
/// # Errors
/// [`ErrorCode::NotFound`], [`ErrorCode::Forbidden`] under `roots`, or a failed write.
pub fn record(
    conn: &Connection,
    meta: &WriteMeta,
    reference: &str,
    number: i64,
    roots: Option<&FileRoots>,
) -> Result<(), ApiError> {
    check_number(number)?;
    let id = writable(conn, reference, roots)?;
    let branch = transition::latest_with_branch(conn, id)?.and_then(|r| r.labels.branch);
    let facts = task::observe(conn, id)?;
    let labels = transition::Labels {
        branch,
        pr_number: Some(number),
        ..transition::Labels::default()
    };
    transition::note(conn, meta, id, &facts, &labels)?;
    Ok(())
}

/// Whether the client established that the work merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Merged {
    /// Proven merged.
    Yes,
    /// Proven not merged.
    No,
    /// Not established.
    Unknown,
}

impl From<Merged> for Fact {
    fn from(m: Merged) -> Self {
        match m {
            Merged::Yes => Self::Yes,
            Merged::No => Self::No,
            Merged::Unknown => Self::Unknown,
        }
    }
}

impl From<Fact> for Merged {
    fn from(f: Fact) -> Self {
        match f {
            Fact::Yes => Self::Yes,
            Fact::No => Self::No,
            Fact::Unknown => Self::Unknown,
        }
    }
}

/// `task.close_merged`: ask the lifecycle for `observed_landed` with `merged` as the landing fact —
/// judged only, without writing, when `dry_run`. The status is read in the op's transaction, so a
/// task cancelled while the client asked `gh` is not overwritten. The reason it was held, if it was.
///
/// # Errors
/// [`ErrorCode::NotFound`], [`ErrorCode::Forbidden`] under `roots`, or a failed write.
pub fn close_merged(
    conn: &Connection,
    meta: &WriteMeta,
    reference: &str,
    merged: Merged,
    pr: Option<i64>,
    dry_run: bool,
    roots: Option<&FileRoots>,
) -> Result<Option<String>, ApiError> {
    if let Some(n) = pr {
        check_number(n)?;
    }
    let id = writable(conn, reference, roots)?;
    let facts = lifecycle::TaskFacts {
        landed_elsewhere: merged.into(),
        ..task::observe(conn, id)?
    };
    if dry_run {
        return Ok(lifecycle::apply(&facts, TaskEvent::ObservedLanded).refusal());
    }
    let outcome = transition::perform(
        conn,
        meta,
        id,
        &facts,
        TaskEvent::ObservedLanded,
        &transition::Labels {
            pr_number: pr,
            ..transition::Labels::default()
        },
    )?;
    Ok(outcome.refusal())
}

#[cfg(test)]
mod tests;
