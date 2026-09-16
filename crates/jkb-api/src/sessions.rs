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
use jkb_core::location::{set_location_facets, valid_ref, Location, FACET_BRANCH};
use jkb_core::{claim, item, ns, tag, task, transition, WriteMeta};
use jkb_types::AgentId;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::tasks::{check_line, check_owner, line_problem, no_item, writable, FileRoots};
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
    /// Why the lifecycle refuses to start a **finished** task — what `task work` refuses with. `None`
    /// for a task that is not finished: whether an unfinished one can be started depends on who asks,
    /// which only the write decides.
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
    let terminal = jkb_types::TaskStatus::is_terminal_str(Some(status.as_str()));
    Ok(TaskState {
        uid: meta.uid,
        terminal,
        status,
        tags,
        claim: observed.claimant.as_ref().map(AgentId::as_str),
        land_target: transition::land_target(conn, id)?,
        start_refusal: if terminal {
            lifecycle::apply(&observed, TaskEvent::Start).refusal()
        } else {
            None
        },
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
/// **Every task on a branch**, in id order, not one of them: two tasks can record the same branch, and
/// keeping whichever was indexed last had `task work` judge a batch spent from a finished task while a
/// live one still landed on it (stage-2 review, round 3).
///
/// # Errors
/// [`ErrorCode::Invalid`] for a malformed repo key, or a failed read.
pub fn by_branch(
    conn: &Connection,
    repo: &str,
) -> Result<BTreeMap<String, Vec<BranchTask>>, ApiError> {
    check_name("repo key", repo)?;
    let ids = jkb_core::location::tasks_in_repo(repo).evaluate(conn)?;
    let metas = item::get_many(conn, &ids)?;
    let tags = tag::applications_for(conn, &ids)?;
    let mut out: BTreeMap<String, Vec<BranchTask>> = BTreeMap::new();
    for id in ids {
        let Some(meta) = metas.get(&id) else { continue };
        let onto = transition::land_target(conn, id)?;
        for (facet, branch) in tags.get(&id).cloned().unwrap_or_default() {
            if facet != FACET_BRANCH {
                continue;
            }
            out.entry(branch).or_default().push(BranchTask {
                uid: meta.uid.clone(),
                status: meta.status.clone().unwrap_or_default(),
                onto: onto.clone(),
            });
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
    /// Bounded, and the branch and land target are names git can take — the check every other
    /// writer of a land target makes, so a value like `--upload-pack=…` is refused here rather than
    /// stored for `staging ls` and `task land` to trip on.
    fn check(&self) -> Result<(), ApiError> {
        check_name("branch", &self.branch)?;
        valid_ref(&self.branch)?;
        check_name("repo key", &self.repo)?;
        if let Some(onto) = &self.onto {
            check_name("land target", onto)?;
            valid_ref(onto)?;
        }
        Ok(())
    }

    fn labels(&self) -> transition::Labels {
        transition::Labels {
            branch: Some(self.branch.clone()),
            onto: self.onto.clone(),
            ..transition::Labels::default()
        }
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
/// claim changed hands since the caller read it, and nothing was written: a claim that is not
/// `displace` (or, with no `displace`, any claim but `take.owner`'s own) was never judged by the
/// caller.
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
    let judged = match &take.displace {
        Some(prev) => claim::clear_if(conn, meta, id, prev)?,
        None => task::observe(conn, id)?
            .claimant
            .is_none_or(|c| c.as_str() == take.owner),
    };
    if !judged {
        return Ok(false);
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

/// What `task.start` was asked. Exactly one of `take` and `keep`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartAsk {
    /// The task.
    pub uid: String,
    /// The claim to take.
    #[serde(default)]
    pub take: Option<Take>,
    /// The claim the caller found and keeps — a live session it is standing inside. The write happens
    /// only while the task is still held by exactly this owner.
    #[serde(default)]
    pub keep: Option<String>,
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
    let labels = ask.place.labels();
    match (&ask.take, &ask.keep) {
        (Some(take), None) => {
            check_owner(&take.owner)?;
            if !swap(conn, meta, id, take, &labels)? {
                return Ok(false);
            }
        }
        (None, Some(kept)) => {
            check_owner(kept)?;
            let held = task::observe(conn, id)?.claimant.map(|c| c.as_str());
            if held.as_deref() != Some(kept.as_str()) {
                return Ok(false);
            }
        }
        _ => {
            return Err(ApiError::with_code(
                ErrorCode::Invalid,
                "a start takes a claim or keeps one: exactly one of `take` and `keep`",
            ))
        }
    }
    // Through the one location-facet writer, as `task.take` does. The branch and its land target are
    // also labels on a transition, so there is no second store to keep in step.
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
    /// Where the work will be; the land target is required.
    pub place: Place,
}

/// `task.take`: `jkb task work`'s claim — the `start` transition, compare-and-set against the owner the
/// caller judged. `false` when the claim changed hands, with nothing written.
///
/// **The start carries no branch and no land target.** Nothing is there yet; [`locate`] labels the
/// history once the worktree exists. Labelled here, a run that then failed its git work left the
/// task's history naming a branch nobody made — which `close-merged` and `task pr` then looked for,
/// holding the task for ever (stage-2 review, round 3).
///
/// **The location is judged here and written later** ([`locate`], once the worktree exists). Every
/// refusal of it — a malformed place, a line the task's `tasks.md` could not read back with it —
/// comes now, while there is nothing to undo: the facets are written for trial inside a savepoint,
/// the line is checked, and the trial is rolled back. Written for real here, a run that then failed
/// its git work left the task pointing at a branch nobody made and unlinked from the one it had
/// (stage-2 review, round 2); refused only after the worktree was made, it left a claim on a
/// checkout no verb could find its task from (round 1).
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
    ask.place.check()?;
    if ask.place.onto.is_none() {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            "a session's claim records where its work lands",
        ));
    }
    let id = writable(conn, &ask.uid, roots)?;
    trial_locate(conn, meta, id, &ask.uid, &ask.place)?;
    swap(conn, meta, id, &ask.take, &transition::Labels::default())
}

/// Write the place, check the task's `tasks.md` line would still read back, and undo the write —
/// whatever the check said. The refusal is the check's own, as a write would get it.
fn trial_locate(
    conn: &Connection,
    meta: &WriteMeta,
    id: jkb_types::ItemId,
    uid: &str,
    place: &Place,
) -> Result<(), ApiError> {
    let before = line_problem(conn, uid)?;
    conn.execute_batch("SAVEPOINT trial_locate")
        .map_err(jkb_core::Error::from)?;
    let tried =
        locate_id(conn, meta, id, place).and_then(|()| check_line(conn, uid, before.as_deref()));
    let undone = conn
        .execute_batch("ROLLBACK TO trial_locate; RELEASE trial_locate")
        .map_err(|e| ApiError::from(jkb_core::Error::from(e)));
    // The trial's own failure is the one to report: one that aborted the whole transaction (a full
    // disk) also takes the savepoint with it, and "no such savepoint" would say nothing.
    tried.and(undone)
}

/// `task.locate`: record where `owner`'s work on a task is — the facets are *set*, not added, since a
/// second value would be a contradiction (D36.6) — and, when the branch or land target differs from what
/// the history last said, a `note` carrying them, so the history names where the work is once it is
/// there. **Only while `owner` holds the claim**: `false`, with nothing written, when it does not, so a
/// run displaced by another cannot overwrite the location its successor recorded.
///
/// # Errors
/// As [`start`].
pub fn locate(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    owner: &str,
    place: &Place,
    roots: Option<&FileRoots>,
) -> Result<bool, ApiError> {
    check_owner(owner)?;
    place.check()?;
    let id = writable(conn, uid, roots)?;
    let facts = task::observe(conn, id)?;
    if facts.claimant.as_ref().map(AgentId::as_str).as_deref() != Some(owner) {
        return Ok(false);
    }
    locate_id(conn, meta, id, place)?;
    let branch_known = transition::latest_with_branch(conn, id)?
        .and_then(|r| r.labels.branch)
        .as_deref()
        == Some(place.branch.as_str());
    let onto_known = place.onto.is_none() || transition::land_target(conn, id)? == place.onto;
    if !(branch_known && onto_known) {
        transition::note(conn, meta, id, &facts, &place.labels())?;
    }
    Ok(true)
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
        // (the same decision the removal record's `accept_dirty` carries). A client of `jkb serve`
        // states it the same way; that grants nothing it lacks, since `task.set` already sets any
        // status (6.2).
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
