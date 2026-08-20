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
            TaskEffect::ReclaimFrom(owner) => {
                // A compare-and-set on the owner that was judged: the probe ran outside this
                // transaction, so between reading the holder and clearing it somebody else can
                // legitimately have claimed the task. Discarding a claim nobody examined is how
                // a live session's hold gets dropped by a scan that never looked at it.
                claim::reclaim(conn, meta, task, &owner.as_str())?;
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
        "landed_elsewhere": facts.landed_elsewhere.as_str(),
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

/// The branch this task's work lands on, as most recently recorded.
///
/// The replacement for `branch_records.land_target`. It is a **label on the moment somebody
/// said so**, not a property kept in agreement with git: two tasks that were told different
/// targets are two rows with timestamps, resolved by recency and visible in `jkb task why`,
/// rather than one row silently keeping whichever wrote last.
///
/// # Errors
/// Returns a database error if the query fails.
pub fn land_target(conn: &Connection, task: ItemId) -> Result<Option<String>> {
    use jkb_fsm::Event as _;
    let abandoned = TaskEvent::Abandon.name();
    let mut out = None;
    for row in history(conn, task)?.into_iter().rev() {
        // An abandon **ends** the answer. Where work lands is a property of the session that was
        // doing it, so a task put back on the shelf lands nowhere until somebody picks it up
        // again — and a staging branch is spent when nothing is landing on it, so leaving a
        // stale target kept an abandoned task rendering as live work on a batch long after it
        // was dropped (design D36.3). Re-working the task records a fresh target and recovers.
        if row.event == abandoned {
            break;
        }
        if let Some(onto) = row.labels.onto {
            out = Some(onto);
            break;
        }
    }
    Ok(out)
}

/// The landing recorded for this task, whichever route recorded it.
///
/// **Both events, and that is the point.** `jkb task land` records `land`; the merge queue's
/// `jkb task landed` records `observed_landed`. Matching only the first meant the swarm's whole
/// flow — queue fast-forwards, records the graft, then `/review-log` runs `review record` — put
/// every group task in "has not grafted their work onto it yet", which was false, while three
/// separate places documented the opposite.
///
/// An `onto` is required: it is what a reader asks about ("did this work reach *that* branch"),
/// and an entry without one answers nothing.
///
/// # Errors
/// Returns a database error if the query fails.
pub fn landed(conn: &Connection, task: ItemId) -> Result<Option<TransitionRow>> {
    use jkb_fsm::Event as _;
    let landing = [TaskEvent::Land.name(), TaskEvent::ObservedLanded.name()];
    Ok(history(conn, task)?
        .into_iter()
        .rev()
        .find(|r| landing.contains(&r.event.as_str()) && r.labels.onto.is_some()))
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

/// Append a fact to a task's history without moving it.
///
/// For something that *happened* but is not a transition — most importantly, learning which
/// pull request carries this work. The alternative was a self-loop event per state carrying no
/// effects, which would put five rows of noise in the table and five edges in the diagram to
/// record one label; a note changes no state and has no guard, so modelling it as a transition
/// would be describing it wrongly to make it fit.
///
/// The `event` column holds `note` for these, so a reader can tell them from transitions.
///
/// # Errors
/// Returns a database error if the write fails.
pub fn note(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    facts: &TaskFacts,
    labels: &Labels,
) -> Result<()> {
    append(conn, meta, task, facts, "note", labels)
}

/// An event that **happened and did not move the task**, recorded under its own name.
///
/// The distinction from [`note`] is what a reader can do with the row. A `note` is bookkeeping —
/// a branch and target being written down, a pull request number arriving — and asserts nothing
/// about the world. This says the event was really observed; the task simply did not move,
/// because a guard about the *task* denied while the fact about the *work* stood.
///
/// The merge queue is the case: it grafts a whole branch, and a task on it that still has open
/// subtasks must not close (D34.4). Recording that as a `note` lost it — [`landed`] matches on
/// the event name, and no reader could tell that row from `jkb task start --onto` writing down
/// where work is going to land. So the task was held, the subtask finished, and nothing ever
/// re-ran the verb: `close-merged` then asked GitHub for a pull request that never existed,
/// because the queue grafts locally. Held for ever, which is the shape D48 exists to end.
///
/// # Errors
/// Returns a database error if the insert fails.
pub fn observed(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    facts: &TaskFacts,
    event: TaskEvent,
    labels: &Labels,
) -> Result<()> {
    use jkb_fsm::Event as _;
    append(conn, meta, task, facts, event.name(), labels)
}

/// One row, `from_status == to_status`, under whatever name the caller gives the event.
fn append(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    facts: &TaskFacts,
    event: &str,
    labels: &Labels,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO task_transitions
             (txn_id, item_id, at, event, from_status, to_status,
              agent_id, branch, onto, ref_commit, pr_number, evidence)
         VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?
    .execute(params![
        meta.txn_id,
        task.get(),
        event,
        facts.status.as_str(),
        facts.actor.as_ref().map(AgentId::as_str),
        labels.branch,
        labels.onto,
        labels.ref_commit,
        labels.pr_number,
        evidence(facts),
    ])?;
    Ok(())
}

/// Free every claim whose owner is **proven** gone, one lifecycle transition each.
///
/// The crash-recovery net (design D27.1/D27.2), now routed through the machine so it appears in
/// each task's history and obeys the same evidence rule as everything else.
///
/// The probe answers a [`Fact`], and only [`Fact::No`] reclaims. An owner whose liveness cannot
/// be established — an externally-minted `agent:` id, or a `claimant_id` in a shape this binary
/// cannot read — comes back in `unverifiable` instead. That is a behaviour change and a
/// deliberate one: the old predicate returned `bool` and treated an unreadable owner as
/// reclaimable, which silently frees a live agent's task. Reporting it costs one command
/// (`jkb task release <uid> --owner <owner>`, once you know that owner is gone); reclaiming it
/// wrongly costs the work.
///
/// Liveness is evaluated **inside the write transaction** against the freshly-read claim set,
/// which closes the race where a claim acquired concurrently by a live owner is reclaimed from a
/// stale snapshot. Each distinct owner is probed at most once; owners in `keep` are alive by
/// fiat and never probed, so a live coordinator passing its own id never reclaims its own work.
///
/// # Errors
/// Returns a database error if a query fails.
pub fn reclaim_dead(
    conn: &Connection,
    meta: &WriteMeta,
    keep: &[String],
    probe: impl Fn(&str) -> Fact,
) -> Result<Reclaimed> {
    let held = claim::claimed(conn)?;
    let mut alive: std::collections::HashMap<String, Fact> = std::collections::HashMap::new();
    for c in &held {
        if !alive.contains_key(&c.owner) {
            let live = if keep.iter().any(|o| o == &c.owner) {
                Fact::Yes
            } else {
                probe(&c.owner)
            };
            alive.insert(c.owner.clone(), live);
        }
    }
    let mut out = Reclaimed::default();
    for c in held {
        match alive[&c.owner] {
            Fact::No => {
                let facts = TaskFacts {
                    claimant: Some(AgentId::parse(&c.owner)),
                    owner_alive: Fact::No,
                    ..crate::task::observe(conn, c.id)?
                };
                let outcome = perform(
                    conn,
                    meta,
                    c.id,
                    &facts,
                    TaskEvent::ObservedOwnerGone,
                    &Labels::default(),
                )?;
                if outcome.moved() {
                    out.cleared.push(c);
                }
            }
            Fact::Unknown => out.unverifiable.push(c),
            Fact::Yes => {}
        }
    }
    Ok(out)
}

/// What a reclaim scan found.
#[derive(Debug, Default)]
pub struct Reclaimed {
    /// Claims whose owner was proven gone, now freed.
    pub cleared: Vec<claim::ClaimInfo>,
    /// Claims held by an owner whose liveness could not be established. **Not** freed — see
    /// [`reclaim_dead`].
    pub unverifiable: Vec<claim::ClaimInfo>,
}

#[cfg(test)]
mod tests {
    use super::{history, perform, reclaim_dead, Labels};
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

    // ---- the crash-recovery net, moved here with the reclaim (design S3.2) ----

    use crate::claim::claim;
    use crate::query::Scope;
    use crate::task::ready;

    fn make_task(db: &Db, uid: &str) -> jkb_types::ItemId {
        let uid = uid.to_owned();
        db.write_txn("t", move |conn, meta| {
            create(conn, meta, &NewTask::new(uid, "T"))
        })
        .expect("create")
    }

    fn claimant_of(db: &Db, id: jkb_types::ItemId) -> Option<String> {
        db.read(move |conn| {
            Ok(conn.query_row(
                "SELECT claimant_id FROM items WHERE id = ?1",
                [id.get()],
                |r| r.get(0),
            )?)
        })
        .expect("read")
    }

    fn status_of(db: &Db, id: jkb_types::ItemId) -> String {
        db.read(move |conn| {
            Ok(
                conn.query_row("SELECT status FROM items WHERE id = ?1", [id.get()], |r| {
                    r.get(0)
                })?,
            )
        })
        .expect("read")
    }

    #[test]
    fn reclaim_clears_dead_owners_and_keeps_live_ones() {
        let db = Db::open_in_memory().unwrap();
        let live = make_task(&db, "task:live");
        let dead = make_task(&db, "task:dead");
        db.write_txn("t", move |conn, meta| {
            claim(conn, meta, live, "host:live")?;
            claim(conn, meta, dead, "host:dead")?;
            Ok(())
        })
        .unwrap();

        // Only host:live is verified alive. The dead owner's claim is cleared; the live
        // owner's claim is preserved; status is never written by reclaim.
        let reclaimed = db
            .write_txn("t", move |conn, meta| {
                reclaim_dead(conn, meta, &["host:live".to_owned()], |_| Fact::No)
            })
            .unwrap();
        assert_eq!(reclaimed.cleared.len(), 1);
        assert_eq!(reclaimed.cleared[0].owner, "host:dead");
        assert_eq!(claimant_of(&db, live).as_deref(), Some("host:live"));
        assert_eq!(claimant_of(&db, dead), None);
        assert_eq!(status_of(&db, live), "in_progress");
        assert_eq!(status_of(&db, dead), "in_progress"); // reclaim never touched status
    }

    #[test]
    fn reclaim_keeps_owners_the_predicate_reports_alive_and_probes_each_once() {
        use std::sync::{Arc, Mutex};
        let db = Db::open_in_memory().unwrap();
        let a = make_task(&db, "task:a");
        let b = make_task(&db, "task:b");
        let c = make_task(&db, "task:c");
        // a and b share owner host:live; c is held by host:dead.
        db.write_txn("t", move |conn, meta| {
            claim(conn, meta, a, "host:live")?;
            claim(conn, meta, b, "host:live")?;
            claim(conn, meta, c, "host:dead")?;
            Ok(())
        })
        .unwrap();

        // The predicate reports host:live alive, host:dead not. Record every probe to
        // prove each *distinct* owner is checked at most once (two tasks, one owner).
        let probed = Arc::new(Mutex::new(Vec::<String>::new()));
        let probed2 = Arc::clone(&probed);
        let reclaimed = db
            .write_txn("t", move |conn, meta| {
                reclaim_dead(conn, meta, &[], |o| {
                    probed2.lock().unwrap().push(o.to_owned());
                    Fact::from(o == "host:live")
                })
            })
            .unwrap();

        assert_eq!(reclaimed.cleared.len(), 1);
        assert_eq!(reclaimed.cleared[0].owner, "host:dead");
        assert_eq!(claimant_of(&db, a).as_deref(), Some("host:live"));
        assert_eq!(claimant_of(&db, b).as_deref(), Some("host:live"));
        assert_eq!(claimant_of(&db, c), None);
        let mut seen = probed.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, vec!["host:dead".to_owned(), "host:live".to_owned()]);
    }

    /// The behaviour change S3.2 argues for: an owner whose liveness cannot be established
    /// keeps its claim, and is **reported** rather than freed. Reclaiming on an unestablished
    /// answer silently frees a live agent's task; reporting it costs one command.
    #[test]
    fn an_unverifiable_owner_keeps_its_claim_and_is_reported() {
        let db = db();
        let id = make_task(&db, "task:opaque");
        db.write_txn("t", move |conn, meta| {
            claim(conn, meta, id, "agent:01JBX7Q4")
        })
        .unwrap();

        let found = db
            .write_txn("t", |conn, meta| {
                reclaim_dead(conn, meta, &[], |_| Fact::Unknown)
            })
            .unwrap();
        assert!(found.cleared.is_empty(), "nothing was proven gone");
        assert_eq!(found.unverifiable.len(), 1);
        assert_eq!(found.unverifiable[0].owner, "agent:01JBX7Q4");
        assert_eq!(claimant_of(&db, id).as_deref(), Some("agent:01JBX7Q4"));
        // ...and the frontier still excludes it, so it is held rather than quietly handed out.
        let frontier = db.read(|conn| ready(conn, Scope::All, &[])).unwrap();
        assert!(frontier.is_empty());
    }

    /// A reclaim appends to the task's history, so `jkb task why` can say the claim was taken
    /// away and on what evidence — which the old side-door write could not.
    #[test]
    fn a_reclaim_is_recorded_in_the_history() {
        let db = db();
        let id = make_task(&db, "task:crashed");
        db.write_txn("t", move |conn, meta| {
            claim(conn, meta, id, "host:4294967290")
        })
        .unwrap();
        db.write_txn("t", |conn, meta| {
            reclaim_dead(conn, meta, &[], |_| Fact::No)
        })
        .unwrap();
        let rows = db.read(move |conn| history(conn, id)).expect("history");
        assert_eq!(
            rows.last().map(|r| r.event.as_str()),
            Some("observed_owner_gone")
        );
        assert_eq!(
            rows.last().unwrap().to_status,
            "in_progress",
            "status untouched"
        );
        assert_eq!(claimant_of(&db, id), None);
        assert_eq!(status_of(&db, id), "in_progress");
    }
}
