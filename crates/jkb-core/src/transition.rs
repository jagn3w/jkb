//! Performing a lifecycle transition, and remembering that it happened (design S-series).
//!
//! [`crate::lifecycle`] holds the rules and touches nothing; this module is the one place a
//! decision becomes writes. [`perform`] is the seam: it asks the machine, applies **the whole
//! plan or none of it**, and appends one row to the history. Every workflow verb goes through
//! it, which is what makes the history complete rather than best-effort.
//!
//! The history is append-only and deliberately not changelogged (see `V015`). It replaces
//! `branch_records`, whose trouble was never what it stored but that it was a *mutable
//! projection of the past* keyed by a branch name — a name git lets you delete, recreate and
//! reuse, so the row had to be kept in agreement with a moving world, and every guard around
//! that reconciliation produced its own defects. A log makes no claim about the present, so
//! there is nothing to reconcile: a name that changes hands appends a row rather than
//! corrupting one.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::{AgentId, Error as TypeError, ItemId, TaskStatus};

use crate::lifecycle::{self, TaskEffect, TaskEvent, TaskFacts};
use crate::store::WriteMeta;
use crate::{claim, Error, Result};

pub use jkb_fsm::{Fact, Outcome, Reconciliation};

/// The lifecycle outcome type, spelled once.
pub type TaskOutcome = Outcome<TaskStatus, TaskEvent, TaskEffect>;

/// Descriptive detail recorded beside a transition.
///
/// **Labels, never keys.** Nothing looks a transition up by branch name; that is the whole
/// point of moving off `branch_records`. They are here so `jkb task why` can say *what* the
/// work was, and so a person reading the history recognizes it.
#[derive(Debug, Clone, Default)]
pub struct Labels {
    /// The branch the work is on.
    pub branch: Option<String>,
    /// The branch it lands on.
    pub onto: Option<String>,
    /// A commit worth remembering — the branch's tip at a landing, say.
    pub ref_commit: Option<String>,
    /// The pull request that proved an external landing.
    pub pr_number: Option<i64>,
}

/// One entry of a task's history.
#[derive(Debug, Clone)]
pub struct TransitionRow {
    /// Row id, ascending with time.
    pub id: i64,
    /// The transaction it was applied in — line it up with the changelog, and with the `undo`
    /// marker if one was later written.
    pub txn_id: String,
    /// When.
    pub at: String,
    /// The [`TaskEvent`] name.
    pub event: String,
    /// Where it came from; `None` for a task whose history starts after this table did.
    pub from_status: Option<String>,
    /// Where it went.
    pub to_status: String,
    /// Who acted.
    pub agent_id: Option<AgentId>,
    /// The descriptive labels.
    pub labels: Labels,
    /// The facts the guard fired on, as JSON — what makes the history say *why*.
    pub evidence: Option<String>,
}

/// Ask the lifecycle for `event`, and if it moves, write the whole plan and record it.
///
/// The **one** way a workflow transition happens. Callers pass what they have observed; they do
/// not decide.
///
/// Ordering is load-bearing and is the rule that makes a failed git step survivable: a caller
/// performs every fallible external step **first**, and calls this last. A git failure then
/// leaves the task exactly where it was and the verb is simply re-runnable — which
/// [`jkb_fsm`]'s idempotence rule guarantees is a no-op once it has succeeded. The incident
/// this replaces set the status, cleared the claim, and *then* asked git to remove a worktree
/// that git refused to remove.
///
/// # Errors
/// Returns an error if a write fails. A refusal is **not** an error: it comes back as
/// [`Outcome::Refused`]/[`Outcome::Undefined`], which the caller renders.
pub fn perform(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    facts: &TaskFacts,
    event: TaskEvent,
    labels: &Labels,
) -> Result<TaskOutcome> {
    let outcome = lifecycle::apply(facts, event);
    if let Outcome::Moved {
        from, to, effects, ..
    } = &outcome
    {
        apply_effects(conn, meta, task, effects)?;
        record(conn, meta, task, event, Some(*from), *to, facts, labels)?;
    }
    Ok(outcome)
}

/// Apply a plan. All of it, in the caller's transaction, or none of it.
///
/// Not public: a plan comes from [`perform`], because a caller able to apply effects it chose
/// itself is a caller able to apply half a transition — which is the defect the plan being one
/// value exists to prevent.
fn apply_effects(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    effects: &[TaskEffect],
) -> Result<()> {
    for effect in effects {
        match effect {
            TaskEffect::SetStatus(status) => write_status(conn, meta, task, *status)?,
            TaskEffect::Claim(owner) => {
                // `claim::claim` is a compare-and-swap that sets `status = 'in_progress'` in the
                // same statement, so there is never a claimed-but-`open` window (D27.1). That is
                // why a `Start` plan is `[Claim]` alone rather than `[SetStatus, Claim]`:
                // claiming *is* starting, and spelling it twice would write the column twice and
                // invite the two to disagree.
                if !claim::claim(conn, meta, task, &owner.as_str())? {
                    return Err(Error::Types(TypeError::Validation(format!(
                        "task {task} was claimed by somebody else between the check and the write"
                    ))));
                }
            }
            TaskEffect::ReleaseClaim => {
                claim::clear(conn, meta, task)?;
            }
        }
    }
    Ok(())
}

/// Write `items.status`, changelogged.
///
/// The claim release that a terminal status entails is **not** here — it is a [`TaskEffect`],
/// so it travels with the plan. It used to be a hidden tail on this function, which meant a
/// second writer of the column silently did not do it.
fn write_status(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    status: TaskStatus,
) -> Result<()> {
    let before: Option<String> = conn
        .prepare_cached("SELECT status FROM items WHERE id = ?1")?
        .query_row([task.get()], |row| row.get::<_, Option<String>>(0))
        .optional()?
        .ok_or_else(|| Error::Types(TypeError::NotFound(format!("task {task}"))))?;
    conn.prepare_cached(
        "UPDATE items SET status = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?1",
    )?
    .execute(params![task.get(), status.as_str()])?;
    crate::changelog::append(
        conn,
        meta,
        crate::changelog::Op::Update,
        crate::changelog::Entity::Items,
        &task.get().to_string(),
        Some(&json!({ "status": before })),
        Some(&json!({ "status": status.as_str() })),
    )?;
    Ok(())
}

/// Append one row to the history.
#[allow(clippy::too_many_arguments)]
fn record(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    event: TaskEvent,
    from: Option<TaskStatus>,
    to: TaskStatus,
    facts: &TaskFacts,
    labels: &Labels,
) -> Result<()> {
    use jkb_fsm::Event as _;
    conn.prepare_cached(
        "INSERT INTO task_transitions
             (txn_id, item_id, at, event, from_status, to_status,
              agent_id, branch, onto, ref_commit, pr_number, evidence)
         VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?
    .execute(params![
        meta.txn_id,
        task.get(),
        event.name(),
        from.map(TaskStatus::as_str),
        to.as_str(),
        facts.actor.as_ref().map(AgentId::as_str),
        labels.branch,
        labels.onto,
        labels.ref_commit,
        labels.pr_number,
        evidence(facts),
    ])?;
    Ok(())
}

/// The facts a guard fired on, as JSON.
///
/// Only the three-valued observations and the identities — not the whole struct — because the
/// point is to answer *why* this moved (or did not), and a field nothing reads is noise in a
/// record a person is meant to read.
fn evidence(facts: &TaskFacts) -> String {
    json!({
        "owner_alive": facts.owner_alive.as_str(),
        "claimant": facts.claimant.as_ref().map(AgentId::as_str),
        "file_backed": facts.file_backed.as_str(),
        "session_exists": facts.session_exists.as_str(),
        "work_dirty": facts.work_dirty.as_str(),
        "has_commits": facts.has_commits.as_str(),
        "target_ready": facts.target_ready.as_str(),
        "reviewed": facts.reviewed.as_str(),
        "review_clean": facts.review_clean.as_str(),
        "review_waived": facts.review_waived.as_str(),
        "open_subtasks": facts.open_subtasks.as_str(),
        "pr_merged": facts.pr_merged.as_str(),
    })
    .to_string()
}

/// A task's history, oldest first.
///
/// # Errors
/// Returns a database error if the query fails.
pub fn history(conn: &Connection, task: ItemId) -> Result<Vec<TransitionRow>> {
    let rows = conn
        .prepare_cached(
            "SELECT id, txn_id, at, event, from_status, to_status, agent_id,
                    branch, onto, ref_commit, pr_number, evidence
               FROM task_transitions WHERE item_id = ?1 ORDER BY id",
        )?
        .query_map([task.get()], |row| {
            Ok(TransitionRow {
                id: row.get(0)?,
                txn_id: row.get(1)?,
                at: row.get(2)?,
                event: row.get(3)?,
                from_status: row.get(4)?,
                to_status: row.get(5)?,
                agent_id: row
                    .get::<_, Option<String>>(6)?
                    .as_deref()
                    .map(AgentId::parse),
                labels: Labels {
                    branch: row.get(7)?,
                    onto: row.get(8)?,
                    ref_commit: row.get(9)?,
                    pr_number: row.get(10)?,
                },
                evidence: row.get(11)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The most recent transition that recorded a branch, if any.
///
/// The replacement for "look this branch up in `branch_records`", turned the right way round: a
/// reader asks about a **task** — which it always has — and gets back what that task's work was
/// last recorded as being on. A branch name that has since changed hands cannot make an older
/// row describe newer work, because nothing is keyed by it.
///
/// # Errors
/// Returns a database error if the query fails.
pub fn latest_with_branch(conn: &Connection, task: ItemId) -> Result<Option<TransitionRow>> {
    Ok(history(conn, task)?
        .into_iter()
        .rev()
        .find(|r| r.labels.branch.is_some()))
}

/// The pull request recorded for this task, if any — the most recently recorded one.
///
/// # Errors
/// Returns a database error if the query fails.
pub fn pull_request(conn: &Connection, task: ItemId) -> Result<Option<i64>> {
    Ok(history(conn, task)?
        .into_iter()
        .rev()
        .find_map(|r| r.labels.pr_number))
}

#[cfg(test)]
mod tests {
    use super::{history, perform, Labels};
    use crate::lifecycle::{TaskEvent, TaskFacts};
    use crate::task::{create, NewTask};
    use crate::Db;
    use jkb_fsm::{Fact, Outcome};
    use jkb_types::{AgentId, TaskStatus};

    fn db() -> Db {
        Db::open_in_memory().expect("open")
    }

    fn a_task(db: &Db) -> jkb_types::ItemId {
        db.write_txn("test", |conn, meta| {
            create(conn, meta, &NewTask::new("task:t", "T"))
        })
        .expect("create")
    }

    /// `write_txn` takes a `'static` closure, so everything a transition needs is moved in.
    fn do_perform(
        db: &Db,
        id: jkb_types::ItemId,
        facts: TaskFacts,
        event: TaskEvent,
        labels: Labels,
    ) -> super::TaskOutcome {
        db.write_txn("test", move |conn, meta| {
            perform(conn, meta, id, &facts, event, &labels)
        })
        .expect("perform")
    }

    fn status_and_claim(db: &Db, id: jkb_types::ItemId) -> (String, Option<String>) {
        db.read(move |conn| {
            Ok(conn.query_row(
                "SELECT status, claimant_id FROM items WHERE id = ?1",
                [id.get()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )?)
        })
        .expect("read")
    }

    #[test]
    fn a_move_writes_its_whole_plan_and_one_history_row() {
        let db = db();
        let id = a_task(&db);
        let owner = AgentId::agent("a");
        let facts = TaskFacts {
            status: TaskStatus::Open,
            actor: Some(owner.clone()),
            ..TaskFacts::default()
        };
        let out = do_perform(&db, id, facts, TaskEvent::Start, Labels::default());
        assert!(matches!(out, Outcome::Moved { .. }));

        let (status, claimant) = status_and_claim(&db, id);
        assert_eq!(status, "in_progress");
        assert_eq!(claimant.as_deref(), Some("agent:a"));

        let rows = db.read(move |conn| history(conn, id)).expect("history");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event, "start");
        assert_eq!(rows[0].from_status.as_deref(), Some("open"));
        assert_eq!(rows[0].to_status, "in_progress");
        assert_eq!(rows[0].agent_id.as_ref(), Some(&owner));
    }

    /// A refusal is not an error, writes nothing, and appends no history: the log records what
    /// happened, and nothing happened.
    #[test]
    fn a_refusal_writes_nothing() {
        let db = db();
        let id = a_task(&db);
        let facts = TaskFacts {
            status: TaskStatus::Open,
            actor: Some(AgentId::agent("b")),
            claimant: Some(AgentId::agent("a")),
            owner_alive: Fact::Yes,
            ..TaskFacts::default()
        };
        let out = do_perform(&db, id, facts, TaskEvent::Start, Labels::default());
        assert!(out.refusal().is_some());
        let rows = db.read(move |conn| history(conn, id)).expect("history");
        assert!(rows.is_empty());
    }

    /// Landing writes the status **and** releases the claim, from one plan.
    #[test]
    fn landing_settles_status_and_claim_together() {
        let db = db();
        let id = a_task(&db);
        let owner = AgentId::agent("a");
        let start = TaskFacts {
            status: TaskStatus::Open,
            actor: Some(owner.clone()),
            ..TaskFacts::default()
        };
        do_perform(&db, id, start, TaskEvent::Start, Labels::default());

        let landing = TaskFacts {
            status: TaskStatus::InProgress,
            actor: Some(owner.clone()),
            claimant: Some(owner),
            owner_alive: Fact::Yes,
            session_exists: Fact::Yes,
            work_dirty: Fact::No,
            has_commits: Fact::Yes,
            target_ready: Fact::Yes,
            reviewed: Fact::Yes,
            review_clean: Fact::Yes,
            open_subtasks: Fact::No,
            ..TaskFacts::default()
        };
        let labels = Labels {
            branch: Some("task/t".into()),
            onto: Some("batch".into()),
            ..Labels::default()
        };
        let out = do_perform(&db, id, landing, TaskEvent::Land, labels);
        assert!(out.refusal().is_none(), "{:?}", out.refusal());

        let (status, claimant) = status_and_claim(&db, id);
        assert_eq!(status, "done");
        assert_eq!(claimant, None, "a settled task holds no claim");

        let rows = db.read(move |conn| history(conn, id)).expect("history");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].event, "land");
        assert_eq!(rows[1].labels.branch.as_deref(), Some("task/t"));
        assert_eq!(rows[1].labels.onto.as_deref(), Some("batch"));
        // The history says *why*, not only *what*.
        assert!(rows[1]
            .evidence
            .as_deref()
            .unwrap()
            .contains("\"reviewed\":\"yes\""));
    }
}
