//! `timeline.*`: the Executions tab's lifecycle timeline, read from facts jkb already records
//! ([`jkb_core::timeline`] does every join).
//!
//! Four reads, all [`crate::rbac::OpPermission::Read`] and in the agent read set, so they are served
//! on the reader: the sessions at work (`timeline.sessions`), one session's tasks and events
//! (`timeline.session`), one task's path with its strategy's graph (`timeline.task`), and a plan's
//! progress over time (`timeline.plan`). No topic carries transitions: an app re-reads on demand.

use jkb_core::timeline::{self as core, Overview, PlanTimeline, SessionTimeline, TaskTimeline};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::workflows::{self, GraphView};
use crate::ApiError;

/// `timeline.task`'s answer: the task's timeline, and the machines of the strategy its newest
/// pinning row names (`workflow.graph` for the task) to draw its path over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    /// The timeline.
    pub timeline: TaskTimeline,
    /// Its strategy's machines.
    pub graph: GraphView,
}

/// `timeline.sessions`, judged at `now_ms`.
///
/// # Errors
/// A database error.
pub fn sessions(conn: &Connection, now_ms: i64) -> Result<Overview, ApiError> {
    Ok(core::sessions(conn, now_ms)?)
}

/// `timeline.session`, one page after `after`, judged at `now_ms`.
///
/// # Errors
/// [`crate::ErrorCode::Invalid`] for a malformed session id or cursor; a database error.
pub fn session(
    conn: &Connection,
    session: &str,
    after: Option<&str>,
    now_ms: i64,
) -> Result<SessionTimeline, ApiError> {
    Ok(core::session(conn, session, after, now_ms)?)
}

/// `timeline.task`, one page after `after`.
///
/// # Errors
/// [`crate::ErrorCode::NotFound`] for no such task, [`crate::ErrorCode::Invalid`] for a malformed
/// cursor; a stored strategy this binary cannot read; a database error.
pub fn task(conn: &Connection, uid: &str, after: Option<&str>) -> Result<TaskView, ApiError> {
    let timeline = core::task(conn, uid, after)?;
    let graph = workflows::graph(conn, Some(&timeline.uid), None)?;
    Ok(TaskView { timeline, graph })
}

/// `timeline.plan`.
///
/// # Errors
/// [`crate::ErrorCode::NotFound`] or [`crate::ErrorCode::Invalid`] for an unknown plan; a database
/// error.
pub fn plan(conn: &Connection, uid: &str) -> Result<PlanTimeline, ApiError> {
    Ok(core::plan(conn, uid)?)
}
