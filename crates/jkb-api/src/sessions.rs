//! The session verbs' database steps (tasks S6.4, `openspec/changes/jkb-message-queue/design-s6-4.md`).
//!
//! `jkb task start`/`work`/`abandon`/`sessions`/`gate` do git and filesystem work where they run and
//! their database work through these ops, so a verb run in the dev container does exactly what it
//! does on the host. Every step here is pure database work (design H4): the ops never read a
//! checkout, run a command or judge whether a process is alive — the caller does, and hands over what
//! it concluded and the owner it concluded it about, which each write re-checks (a compare-and-set)
//! in its own transaction.
//!
//! Each write goes through the task-write rules of stage 6.2 (`task_write`): under `jkb serve`'s
//! [`FileRoots`] a task filed outside the container's view is refused, and every write is held to its
//! `tasks.md` line's round trip.
//!
//! The logic was the CLI's (`swap_claim`, the start and abandon transactions, `tasks_by_branch`,
//! `stored_gate`); it lives here now so the host CLI, which serves these ops in-process, and the
//! daemon cannot differ.

use std::collections::BTreeMap;

use jkb_core::lifecycle::{self, TaskEvent};
use jkb_core::location::{set_location_facets, Location, FACET_BRANCH, FACET_REPO};
use jkb_core::{claim, item, ns, tag, task, transition, WriteMeta};
use jkb_types::AgentId;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::tasks::{check_owner, no_item, writable, FileRoots};
use crate::{ApiError, ErrorCode};

/// The longest branch, repo key or land target a session op accepts, in bytes. Each is stored as a
/// facet value and a transition label.
pub const MAX_NAME_BYTES: usize = 255;

fn check_name(what: &str, value: &str) -> Result<(), ApiError> {
    if value.is_empty() || value.len() > MAX_NAME_BYTES || value.chars().any(char::is_control) {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            format!("a {what} of 1 to {MAX_NAME_BYTES} bytes with no control characters"),
        ));
    }
    Ok(())
}

/// What a session verb needs to know about one task before it touches git.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskState {
    /// The task's full uid.
    pub uid: String,
    /// Its status (`open`, `in_progress`, …).
    pub status: String,
    /// Every facet value it carries, by facet — a multi-map, as the tags are.
    pub tags: BTreeMap<String, Vec<String>>,
    /// Who holds its claim, if anyone.
    pub claim: Option<String>,
    /// Where its work lands, from its transition history.
    pub land_target: Option<String>,
    /// Why the lifecycle would refuse to start it, if it would — for a terminal task, what
    /// `task work` refuses with.
    pub start_refusal: Option<String>,
    /// Whether its status is terminal (`done`, `cancelled`).
    pub terminal: bool,
}

/// `task.facts`: what the session verbs read about a task, in one read.
///
/// # Errors
/// [`ErrorCode::NotFound`] for no such task, or a failed read.
pub fn facts(conn: &Connection, reference: &str) -> Result<TaskState, ApiError> {
    let id = task::resolve_ref(conn, reference)?.ok_or_else(|| no_item(reference))?;
    let meta = item::get(conn, id)?.ok_or_else(|| no_item(reference))?;
    let mut tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (facet, value) in tag::applications(conn, id)? {
        tags.entry(facet).or_default().push(value);
    }
    let observed = task::observe(conn, id)?;
    let status = meta.status.clone().unwrap_or_default();
    Ok(TaskState {
        uid: meta.uid,
        terminal: jkb_types::TaskStatus::is_terminal_str(Some(status.as_str())),
        status,
        tags,
        claim: observed.claimant.as_ref().map(AgentId::as_str),
        land_target: transition::land_target(conn, id)?,
        start_refusal: lifecycle::apply(&observed, TaskEvent::Start).refusal(),
    })
}

/// One of a repo's tasks, as `task.by_branch` indexes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchTask {
    /// The task's uid.
    pub uid: String,
    /// Its status.
    pub status: String,
    /// Where its work lands.
    pub onto: Option<String>,
}

/// `task.by_branch`: the repo's tasks (`repo=<repo>`), indexed by **every** branch each records — the
/// only link from a worktree back to its task (design D36.2). One read.
///
/// # Errors
/// [`ErrorCode::Invalid`] for a malformed repo key, or a failed read.
pub fn by_branch(conn: &Connection, repo: &str) -> Result<BTreeMap<String, BranchTask>, ApiError> {
    check_name("repo key", repo)?;
    let query = jkb_core::query::Query {
        kind: Some("task".to_owned()),
        tags: vec![jkb_core::query::TagPred {
            facet: FACET_REPO.to_owned(),
            op: jkb_core::query::CmpOp::Eq,
            value: repo.to_owned(),
        }],
        ..jkb_core::query::Query::default()
    };
    let ids = query.evaluate(conn)?;
    let metas = item::get_many(conn, &ids)?;
    let tags = tag::applications_for(conn, &ids)?;
    let mut out = BTreeMap::new();
    for id in ids {
        let Some(meta) = metas.get(&id) else { continue };
        let onto = transition::land_target(conn, id)?;
        for (facet, branch) in tags.get(&id).cloned().unwrap_or_default() {
            if facet != FACET_BRANCH {
                continue;
            }
            out.insert(
                branch,
                BranchTask {
                    uid: meta.uid.clone(),
                    status: meta.status.clone().unwrap_or_default(),
                    onto: onto.clone(),
                },
            );
        }
    }
    Ok(out)
}

/// Where a task's work is: the facets `task start` and `task work` set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Place {
    /// The branch the work is on.
    pub branch: String,
    /// The repo key.
    pub repo: String,
    /// The branch it lands on, if one is recorded.
    #[serde(default)]
    pub onto: Option<String>,
}

impl Place {
    fn check(&self) -> Result<(), ApiError> {
        check_name("branch", &self.branch)?;
        check_name("repo key", &self.repo)?;
        if let Some(onto) = &self.onto {
            check_name("land target", onto)?;
        }
        Ok(())
    }
}

/// How a verb asks for a task's claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Take {
    /// The owner to hold it.
    pub owner: String,
    /// The owner the caller read and judged gone (or its own earlier self), cleared first — a
    /// compare-and-set: if the claim is no longer this, nothing is written. `None` when the caller
    /// read the task as unclaimed.
    #[serde(default)]
    pub displace: Option<String>,
}

/// Move the claim on `id` to `take.owner`, atomically against `take.displace`, through the lifecycle's
/// `start` — so starting a task is in its history, with who started it and where. `false` means the
/// claim changed hands since the caller read it, and nothing was written.
///
/// Clearing first is what lets a resumed session re-take its own claim under a new pid: the claim's
/// compare-and-set accepts only a free task or a byte-identical owner. Clearing only the *judged*
/// owner is what stops it throwing away a claim the caller never looked at.
fn swap(
    conn: &Connection,
    meta: &WriteMeta,
    id: jkb_types::ItemId,
    take: &Take,
    labels: &transition::Labels,
) -> Result<bool, ApiError> {
    if let Some(prev) = &take.displace {
        if !claim::clear_if(conn, meta, id, prev)? {
            return Ok(false);
        }
    }
    let facts = lifecycle::TaskFacts {
        actor: Some(AgentId::parse(&take.owner)),
        ..task::observe(conn, id)?
    };
    let outcome = transition::perform(conn, meta, id, &facts, TaskEvent::Start, labels)?;
    match outcome.refusal() {
        // As the core error, so the wording is the one `jkb task start` always printed.
        Some(why) => Err(jkb_core::Error::from(jkb_types::Error::Validation(why)).into()),
        // An already-started task claimed by the same owner is idempotent: asking twice is not an
        // error (D48/S1.6).
        None => Ok(true),
    }
}

/// What `task.start` was asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartAsk {
    /// The task.
    pub uid: String,
    /// The claim to take, or `None` to keep the one the caller found (a live session it is standing
    /// inside).
    #[serde(default)]
    pub take: Option<Take>,
    /// Where the work is.
    pub place: Place,
}

/// `task.start`: take the claim (unless kept), record where the work is, and note it in the task's
/// history — one transaction, as `jkb task start` always was. Returns `false`, having written nothing,
/// when the claim changed hands since the caller read it.
///
/// # Errors
/// [`ErrorCode::NotFound`]/[`ErrorCode::Forbidden`] for the task, [`ErrorCode::Invalid`] for a
/// malformed field or a task the lifecycle will not start, or a failed write.
pub fn start(
    conn: &Connection,
    meta: &WriteMeta,
    ask: &StartAsk,
    roots: Option<&FileRoots>,
) -> Result<bool, ApiError> {
    ask.place.check()?;
    let id = writable(conn, &ask.uid, roots)?;
    let labels = transition::Labels {
        branch: Some(ask.place.branch.clone()),
        onto: ask.place.onto.clone(),
        ..transition::Labels::default()
    };
    if let Some(take) = &ask.take {
        check_owner(&take.owner)?;
        if !swap(conn, meta, id, take, &labels)? {
            return Ok(false);
        }
    }
    // Through the one location-facet writer, exactly as `task.locate` does. The branch and its land
    // target are also labels on a transition, so there is no second store to keep in step.
    locate_id(conn, meta, id, &ask.place)?;
    let facts = task::observe(conn, id)?;
    transition::note(conn, meta, id, &facts, &labels)?;
    Ok(true)
}

/// What `task.take` was asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TakeAsk {
    /// The task.
    pub uid: String,
    /// The claim.
    pub take: Take,
    /// The branch the work will be on, recorded on the `start` transition.
    pub branch: String,
    /// Where it lands.
    pub onto: String,
}

/// `task.take`: `jkb task work`'s claim — the `start` transition carrying the session's branch and
/// land target, compare-and-set against the owner the caller judged. `false` when the claim changed
/// hands, with nothing written.
///
/// # Errors
/// As [`start`].
pub fn take(
    conn: &Connection,
    meta: &WriteMeta,
    ask: &TakeAsk,
    roots: Option<&FileRoots>,
) -> Result<bool, ApiError> {
    check_owner(&ask.take.owner)?;
    check_name("branch", &ask.branch)?;
    check_name("land target", &ask.onto)?;
    let id = writable(conn, &ask.uid, roots)?;
    swap(
        conn,
        meta,
        id,
        &ask.take,
        &transition::Labels {
            branch: Some(ask.branch.clone()),
            onto: Some(ask.onto.clone()),
            ..transition::Labels::default()
        },
    )
}

fn locate_id(
    conn: &Connection,
    meta: &WriteMeta,
    id: jkb_types::ItemId,
    place: &Place,
) -> Result<(), ApiError> {
    set_location_facets(
        conn,
        meta,
        id,
        &Location {
            branch: Some(&place.branch),
            repo: Some(&place.repo),
            onto: place.onto.as_deref(),
        },
    )?;
    Ok(())
}

/// `task.locate`: record where a task's work is — the facets are *set*, not added, since a second
/// value would be a contradiction rather than extra information (D36.6).
///
/// # Errors
/// As [`start`].
pub fn locate(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    place: &Place,
    roots: Option<&FileRoots>,
) -> Result<(), ApiError> {
    place.check()?;
    let id = writable(conn, uid, roots)?;
    locate_id(conn, meta, id, place)
}

/// What `task.abandon` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Abandoned {
    /// Whether the task was reopened.
    pub reopened: bool,
    /// Its status afterwards.
    pub status: String,
}

/// `task.abandon`: release the claim the caller judged and reopen the task, unless it has finished.
///
/// The claim is the one the caller **observed** before its git work (`observed`), cleared as a
/// compare-and-set: a claim taken in the meantime belongs to a worker this verb never judged, so
/// nothing is changed and that is reported. The status is re-read here, so a task that finished while
/// the caller was removing a worktree is left alone — the lifecycle has no `abandon` out of a terminal
/// status, and its refusal is the one answer.
///
/// # Errors
/// As [`start`].
pub fn abandon(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    observed: Option<&str>,
    roots: Option<&FileRoots>,
) -> Result<Abandoned, ApiError> {
    if let Some(o) = observed {
        check_owner(o)?;
    }
    let id = writable(conn, uid, roots)?;
    let current = |conn: &Connection| -> Result<String, ApiError> {
        Ok(item::get(conn, id)?
            .and_then(|m| m.status)
            .unwrap_or_default())
    };
    let unchanged = |conn: &Connection| -> Result<Abandoned, ApiError> {
        Ok(Abandoned {
            reopened: false,
            status: current(conn)?,
        })
    };
    match observed {
        // No claim was observed, but one exists now: its holder was never judged.
        None if task::observe(conn, id)?.claimant.is_some() => return unchanged(conn),
        Some(prev) if !claim::clear_if(conn, meta, id, prev)? => return unchanged(conn),
        _ => {}
    }
    let facts = lifecycle::TaskFacts {
        // Stated by the caller, which is entitled to state it: it refused a checkout it could not
        // prove clean unless the operator passed `--force`, which is the operator supplying the fact
        // (the same decision the removal record's `accept_dirty` carries).
        work_dirty: jkb_fsm::Fact::No,
        ..task::observe(conn, id)?
    };
    let outcome = transition::perform(
        conn,
        meta,
        id,
        &facts,
        TaskEvent::Abandon,
        &transition::Labels::default(),
    )?;
    if outcome.refusal().is_some() {
        return unchanged(conn);
    }
    Ok(Abandoned {
        reopened: true,
        status: current(conn)?,
    })
}

/// `repo.gate`: the gate command stored for `repo` (`repos/<repo>`'s metadata), if any.
///
/// Read-only through the daemon, deliberately (decision A): a stored gate is a shell command the host
/// runs, so a client may read it — to run it itself, where it is — but never store one. Storing stays a
/// host command.
///
/// # Errors
/// [`ErrorCode::Invalid`] for a malformed repo key, or a failed read.
pub fn gate(conn: &Connection, repo: &str) -> Result<Option<String>, ApiError> {
    check_name("repo key", repo)?;
    let Some(id) = ns::get(conn, &format!("repos/{repo}"))? else {
        return Ok(None);
    };
    Ok(ns::get_metadata(conn, id)?.and_then(|m| {
        m.get("gate")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }))
}

#[cfg(test)]
mod tests;
