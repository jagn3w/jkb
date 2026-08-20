//! The task lifecycle, declared once (design S2, `openspec/changes/jkb-state-machine/`).
//!
//! Before this, the lifecycle existed only as the intersection of about a dozen call sites —
//! `claim::claim`'s terminal pre-check, `task::set_status`, `staging::State::from_status`,
//! `staging::land_blocker`, `land_preflight`, `close-merged`, `task abandon`, `task work`,
//! `review record`, `merge-queue.sh`, the VS Code row — each deriving the part of it that its
//! own question needed. The most common defect in this repository's review history is two of
//! those answering one question differently, and the standing fix was to make the two share a
//! function. That works and does not generalize: the thirteenth site is written by whoever adds
//! the next verb, and nothing tells them the list exists.
//!
//! So the lifecycle is a `&'static` table here, and the readers ask it. What that buys is in
//! [`jkb_fsm`]: every state can reach a terminal one, no observation leaves a task refusing
//! everything, every refusal's advice is an event the machine really accepts, and asking twice
//! is a no-op rather than an error.
//!
//! **This module is pure.** No `Connection`, no `Command`, no git. Guards read [`TaskFacts`],
//! which the caller fills in — the CLI from git and the database, a test from a literal — so
//! the rules are exercisable without a repository. That is why the corresponding rules today
//! can only be tested by string-matching `assert_cmd` output against a scratch checkout.

use jkb_fsm::{
    all_of, require_no, require_yes, Denial, Dest, Event, EventKind, Fact, Machine, Outcome,
    Reconciliation, Stateful, Transition, Verdict,
};
use jkb_types::{AgentId, TaskStatus};

/// Something that happens to a task.
///
/// Split by [`EventKind`] into what somebody **asks for** and what the world **forces on us**.
/// The second kind is the one the design brief names: reconciliation paths that were previously
/// commands with no shared model, so nothing had ever asked whether two of them can fire on one
/// task at once. [`jkb_fsm::Machine::reconcile`] now refuses that rather than picking one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskEvent {
    /// Somebody picked this up: `jkb task work`, `task start`, `task claim`.
    Start,
    /// The work is on a branch and wants review.
    SubmitForReview,
    /// A review found something that must be fixed before landing.
    RequestChanges,
    /// jkb grafted the work onto its target and the gate was green — or a merged pull request
    /// proves it landed and the operator confirmed.
    Land,
    /// Put it back on the shelf: the session is dropped, the task is not.
    Abandon,
    /// It will not be done.
    Cancel,
    /// Pick a finished or dropped task back up.
    Reopen,
    /// An operator states the status directly (`jkb task set --status`).
    ///
    /// [`Dest::Stated`], and therefore **excluded from the liveness checks**: an escape hatch is
    /// not an exit. D38 is explicit that status and the land gate are not fused, so this is
    /// deliberately unguarded — what it is not is *outside* the machine, which is where it used
    /// to be. Being a row means it carries the same effects as every other transition, including
    /// the claim release that reaching a terminal status entails and that `task::set_status`
    /// previously performed by hand.
    Override,
    /// A synced file declares this task's status (the `tasks` serializer's `[ ]`/`[x]`/`[~]`/`[-]`).
    ///
    /// A reconciliation: the authority is outside jkb, and the guard is that this task really is
    /// backed by that file. Also [`Dest::Stated`] — the file names the status.
    SetFromFile,
    /// The agent holding the claim is gone. An effect-only self-loop: it releases the claim and
    /// touches nothing else, which is exactly what `claim::reclaim_dead` documents in prose.
    ObservedOwnerGone,
    /// A merged pull request proves this work reached its target.
    ///
    /// The one reconciliation that replaced an inference. It used to be
    /// `close-merged`, which asked the commit graph "does this branch add anything to trunk?"
    /// — a question that cannot distinguish *squash-landed* from *never started*, and so needed
    /// a stored cut point, an instance anchor derived from the reflog, and a supersede rule for
    /// when a branch name changed hands. A pull request number is minted by GitHub and never
    /// reused, so none of that has anything to disambiguate.
    ObservedLanded,
}

impl Event for TaskEvent {
    const ALL: &'static [Self] = &[
        Self::Start,
        Self::SubmitForReview,
        Self::RequestChanges,
        Self::Land,
        Self::Abandon,
        Self::Cancel,
        Self::Reopen,
        Self::Override,
        Self::SetFromFile,
        Self::ObservedOwnerGone,
        Self::ObservedLanded,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::SubmitForReview => "submit_for_review",
            Self::RequestChanges => "request_changes",
            Self::Land => "land",
            Self::Abandon => "abandon",
            Self::Cancel => "cancel",
            Self::Reopen => "reopen",
            Self::Override => "override",
            Self::SetFromFile => "set_from_file",
            Self::ObservedOwnerGone => "observed_owner_gone",
            Self::ObservedLanded => "observed_landed",
        }
    }

    fn kind(self) -> EventKind {
        match self {
            Self::SetFromFile | Self::ObservedOwnerGone | Self::ObservedLanded => {
                EventKind::Reconciled
            }
            _ => EventKind::Applied,
        }
    }
}

impl TaskEvent {
    /// Parse an event name, for the CLI and for reading the transition log back.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|e| e.name() == name)
    }
}

/// What a transition requires the caller to do, beyond the state change itself.
///
/// Three, and deliberately no more. The machine's authority is exactly the pair of fields that
/// `settle_landing` desynchronized — `items.status` and the claim columns — and both live on one
/// row, so one `write_txn` applies a whole plan atomically by construction.
///
/// Worktree removal and the merge graft stay **outside**: they are git operations that can fail
/// after the transaction commits, and putting them in a plan would recreate that same bug with
/// more ceremony. What the machine gives those callers instead is the ordering rule that makes
/// failure survivable — **apply the plan last**, after every fallible git step has succeeded, so
/// a git failure leaves the task where it was and the verb is simply re-runnable, which
/// [`jkb_fsm`]'s idempotence rule guarantees is a no-op once it has worked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEffect {
    /// Write `items.status`.
    SetStatus(TaskStatus),
    /// Take the claim for this owner.
    Claim(AgentId),
    /// Clear the claim, whoever holds it.
    ReleaseClaim,
}

/// Everything a guard is allowed to look at.
///
/// Every field about the world outside the database is a [`Fact`], not a `bool`, so a guard has
/// to say whether it needs the answer proven *true* or proven *false* — and an answer we could
/// not obtain refuses either way. Nine of this repository's must-fix findings are one unobtained
/// answer spelled as `false`.
#[derive(Debug, Clone, Default)]
pub struct TaskFacts {
    /// The task's current status — the machine's state.
    pub status: TaskStatus,
    /// Who is asking, when that matters (`Start` on a claimed task).
    pub actor: Option<AgentId>,
    /// Who currently holds it.
    pub claimant: Option<AgentId>,
    /// Whether that holder is still there. `Unknown` for an [`AgentId::Agent`] and for an
    /// owner id we cannot read, which is why reclaiming requires it proven *false*.
    pub owner_alive: Fact,
    /// The status the caller is asking for, for the two [`Dest::Stated`] events.
    pub stated: Option<TaskStatus>,
    /// Whether this task's content is owned by a synced file.
    pub file_backed: Fact,
    /// A `.jkb/work` checkout exists for this task.
    pub session_exists: Fact,
    /// That checkout has uncommitted changes.
    pub work_dirty: Fact,
    /// The work has commits its land target does not.
    pub has_commits: Fact,
    /// The checkout a graft would happen in is clean enough to take it.
    pub target_ready: Fact,
    /// A review was recorded against the work's current head.
    pub reviewed: Fact,
    /// That review has no open must-fix finding.
    pub review_clean: Fact,
    /// `--no-review` was recorded for this head.
    pub review_waived: Fact,
    /// The task has a non-terminal subtask (design D34.1: you work the leaves).
    pub open_subtasks: Fact,
    /// A merged pull request proves this work reached its target.
    ///
    /// `Unknown` where there is no pull request recorded, no `gh`, no network, or no GitHub
    /// remote — and `Unknown` holds, which is the whole point: it replaces an *inference* from
    /// the commit graph, which could not tell a squash-landed branch from one that never
    /// started, with a *lookup* on an id that is never reused.
    pub pr_merged: Fact,
}

/// The state lives in the observation, so nothing can pass a state that disagrees with the
/// facts gathered about it.
impl Stateful<TaskStatus> for TaskFacts {
    fn state(&self) -> TaskStatus {
        self.status
    }
}

/// The lifecycle machine.
pub type TaskMachine = Machine<TaskStatus, TaskEvent, TaskFacts, TaskEffect>;

// -------------------------------------------------------------------------------------------
// Guards. Each one is a rule that used to live in a command, named after the incident that
// taught it where that is worth recording.
// -------------------------------------------------------------------------------------------

/// Whether `actor` may take a claim currently held by `claimant`.
///
/// Free, or held by the same owner, or held by an owner **proven** gone. An owner whose liveness
/// is unestablished keeps its claim (design S3.2) — the other direction frees a live agent's
/// task, and this repository's rule is that of two ways to be wrong, the recoverable one wins.
fn claim_available(f: &TaskFacts) -> Verdict<TaskEvent> {
    match (&f.claimant, &f.actor) {
        (None, _) => Verdict::Allow,
        (Some(held), Some(actor)) if held == actor => Verdict::Allow,
        _ if f.owner_alive.is_no() => Verdict::Allow,
        (Some(held), _) => Verdict::Deny(Denial::with_remedy(
            format!(
                "It is held by `{held}`, which is {}.",
                if f.owner_alive.is_yes() {
                    "still running"
                } else {
                    "an owner whose liveness cannot be established here"
                }
            ),
            TaskEvent::ObservedOwnerGone,
            "Free it with `jkb task reclaim` once you know that owner is gone.",
        )),
    }
}

fn start_guard(f: &TaskFacts) -> Verdict<TaskEvent> {
    claim_available(f)
}

/// Landing: the rule that lived in `land_blocker` **and** `land_preflight` **and** the In Flight
/// row, which is how a task could be told two opposite things about the same blocker.
///
/// Note the polarity of each clause. `work_dirty` must be proven **false** — a checkout we could
/// not read refuses, rather than being grafted unseen. `has_commits` must be proven **true** —
/// zero is a load-bearing answer meaning *nothing to land*, and an unresolvable branch used to
/// produce it.
fn land_guard(f: &TaskFacts) -> Verdict<TaskEvent> {
    all_of([
        require_yes(f.session_exists.or(f.has_commits), || {
            // The remedy is state-dependent, and has to be: `start` is not accepted from
            // `needs_review`, so offering it there would be advice the machine itself refuses.
            // `Machine::audit` caught exactly that in the first version of this guard, which is
            // the whole argument for a remedy being an event rather than a sentence.
            if f.status == TaskStatus::NeedsReview {
                Denial::with_remedy(
                    "There is no work to land — no session checkout and no commits.",
                    TaskEvent::RequestChanges,
                    "Send it back to the implementer with `jkb task set <uid> --status \
                     in_progress`, then open a session with `jkb task work <uid>`.",
                )
            } else {
                Denial::with_remedy(
                    "There is no work to land — no session checkout and no commits.",
                    TaskEvent::Start,
                    "Open one with `jkb task work <uid>`.",
                )
            }
        }),
        require_no(f.work_dirty, || {
            Denial::new(
                "The session has uncommitted changes (or could not be read) — commit them first.",
            )
        }),
        require_yes(f.has_commits, || {
            Denial::new("It has no commits that the land target does not already have.")
        }),
        require_no(f.open_subtasks, || {
            Denial::new(
                "It still has open subtasks — you work the leaves, and the parent lands after \
                 them.",
            )
        }),
        require_yes(f.target_ready, || {
            Denial::new(
                "The checkout the graft would happen in has uncommitted changes — landing rolls \
                 it back on a red gate, which would take them with it.",
            )
        }),
        review_gate(f),
    ])
}

/// The review half of the land gate (design D38.5): reviewed, and no open must-fix.
///
/// `reviewed` must be proven **true**, which is the fix for the very first finding of the
/// corpus: a review whose findings never reached the knowledge base was indistinguishable from
/// a review with nothing to say, and landed as reviewed.
fn review_gate(f: &TaskFacts) -> Verdict<TaskEvent> {
    if f.review_waived.is_yes() {
        return Verdict::Allow;
    }
    all_of([
        require_yes(f.reviewed, || {
            Denial::with_remedy(
                "No review is recorded for this head.",
                TaskEvent::SubmitForReview,
                "Run `/review-log`, or land anyway with `jkb task land --no-review`.",
            )
        }),
        require_yes(f.review_clean, || {
            Denial::with_remedy(
                "Its review has open must-fix findings.",
                TaskEvent::RequestChanges,
                "Fix or cancel them, or land anyway with `jkb task land --no-review`.",
            )
        }),
    ])
}

/// The pull-request-proved landing, for work jkb did not graft itself.
///
/// This is the whole of what replaced the commit-graph inference. `pr_merged` must be proven
/// true, and `Unknown` — no pull request recorded, no `gh`, no network, no GitHub remote —
/// holds and says so. The inference it replaces had to distinguish *squash-landed* from *never
/// started* from a branch that adds nothing to trunk, which needed a stored cut point, an
/// instance anchor, and roughly a quarter of the review corpus's must-fix findings.
///
/// It is a separate event from [`TaskEvent::Land`] rather than a second way to satisfy that
/// event's guard, because the two ask different questions: `Land`'s guard is *may jkb perform
/// this graft* (is the checkout clean, is the target ready, did a review pass), while this is
/// *did it already land*. Folding an observation into an action's preconditions is how a
/// review gate comes to be bypassed by a merge somebody else performed.
fn landed_externally(f: &TaskFacts) -> Verdict<TaskEvent> {
    all_of([
        require_yes(f.pr_merged, || {
            Denial::new("No merged pull request proves this work landed.")
        }),
        require_no(f.open_subtasks, || {
            Denial::new("Its pull request merged, but it still has open subtasks.")
        }),
    ])
}

fn abandon_guard(f: &TaskFacts) -> Verdict<TaskEvent> {
    require_no(f.work_dirty, || {
        Denial::new(
            "The session has uncommitted changes (or could not be read) — commit or discard them \
             before abandoning it.",
        )
    })
}

fn owner_gone(f: &TaskFacts) -> Verdict<TaskEvent> {
    all_of([
        require_yes(Fact::from(f.claimant.is_some()), || {
            Denial::new("Nothing holds it.")
        }),
        require_no(f.owner_alive, || {
            Denial::new(
                "Its owner is still running, or its liveness cannot be established from here.",
            )
        }),
    ])
}

/// The synced file is only an authority over a task it actually backs.
fn file_is_authority(f: &TaskFacts) -> Verdict<TaskEvent> {
    all_of([
        require_yes(f.file_backed, || {
            Denial::new("This task is not backed by a synced file.")
        }),
        require_yes(Fact::from(f.stated.is_some()), || {
            Denial::new("The file states no status for it.")
        }),
    ])
}

/// The status the caller named. `None` refuses rather than silently staying put.
fn stated(f: &TaskFacts) -> Option<TaskStatus> {
    f.stated
}

// -------------------------------------------------------------------------------------------
// Effects
// -------------------------------------------------------------------------------------------

/// Starting is claiming.
///
/// The plan is `[Claim]` alone, not `[SetStatus, Claim]`: `claim::claim` is a compare-and-swap
/// that advances the status in the same statement, so there is never a claimed-but-`open`
/// window (D27.1), and spelling the status twice would write the column twice and invite the
/// two writes to disagree. Where no actor is named — a start with nobody to attribute it to —
/// the status moves on its own.
fn take_claim(f: &TaskFacts) -> Vec<TaskEffect> {
    match &f.actor {
        Some(actor) => vec![TaskEffect::Claim(actor.clone())],
        None => vec![TaskEffect::SetStatus(TaskStatus::InProgress)],
    }
}

/// Reaching a terminal status releases the claim.
///
/// `task::set_status` did this by hand, which is right and is exactly the kind of pairing that
/// gets lost when a second caller writes the status directly. Here it is part of the plan, so a
/// caller that applies the plan cannot apply half of it.
fn settle(status: TaskStatus) -> Vec<TaskEffect> {
    let mut fx = vec![TaskEffect::SetStatus(status)];
    if status.is_terminal() {
        fx.push(TaskEffect::ReleaseClaim);
    }
    fx
}

fn plan_land(_: &TaskFacts) -> Vec<TaskEffect> {
    settle(TaskStatus::Done)
}

fn plan_cancel(_: &TaskFacts) -> Vec<TaskEffect> {
    settle(TaskStatus::Cancelled)
}

fn plan_reopen(_: &TaskFacts) -> Vec<TaskEffect> {
    vec![TaskEffect::SetStatus(TaskStatus::Open)]
}

/// Abandoning returns the task to the shelf **and** drops the claim: leaving it held is how a
/// task comes back as `open` and immediately unavailable to everyone including its own owner.
fn plan_abandon(_: &TaskFacts) -> Vec<TaskEffect> {
    vec![
        TaskEffect::SetStatus(TaskStatus::Open),
        TaskEffect::ReleaseClaim,
    ]
}

fn plan_review(_: &TaskFacts) -> Vec<TaskEffect> {
    vec![TaskEffect::SetStatus(TaskStatus::NeedsReview)]
}

fn plan_changes(_: &TaskFacts) -> Vec<TaskEffect> {
    vec![TaskEffect::SetStatus(TaskStatus::InProgress)]
}

fn plan_release(_: &TaskFacts) -> Vec<TaskEffect> {
    vec![TaskEffect::ReleaseClaim]
}

/// A stated status carries the same obligations as a declared one — including the claim release
/// on a terminal — so `jkb task set --status done` and `jkb task land` cannot leave the row in
/// two different shapes.
fn plan_stated(f: &TaskFacts) -> Vec<TaskEffect> {
    f.stated.map(settle).unwrap_or_default()
}

// -------------------------------------------------------------------------------------------
// The table
// -------------------------------------------------------------------------------------------

const ROWS: &[Transition<TaskStatus, TaskEvent, TaskFacts, TaskEffect>] = &[
    // --- picking work up -------------------------------------------------------------------
    Transition {
        from: TaskStatus::Open,
        event: TaskEvent::Start,
        to: Dest::To(TaskStatus::InProgress),
        guard: Some(start_guard),
        plan: Some(take_claim),
    },
    // Declared so a re-`start` by a *different* agent is refused rather than silently absorbed
    // by the idempotence rule. Same agent: a no-op that re-asserts the claim.
    Transition {
        from: TaskStatus::InProgress,
        event: TaskEvent::Start,
        to: Dest::To(TaskStatus::InProgress),
        guard: Some(start_guard),
        plan: Some(take_claim),
    },
    // --- review ----------------------------------------------------------------------------
    Transition {
        from: TaskStatus::InProgress,
        event: TaskEvent::SubmitForReview,
        to: Dest::To(TaskStatus::NeedsReview),
        guard: None,
        plan: Some(plan_review),
    },
    Transition {
        from: TaskStatus::NeedsReview,
        event: TaskEvent::RequestChanges,
        to: Dest::To(TaskStatus::InProgress),
        guard: None,
        plan: Some(plan_changes),
    },
    // --- landing ---------------------------------------------------------------------------
    Transition {
        from: TaskStatus::InProgress,
        event: TaskEvent::Land,
        to: Dest::To(TaskStatus::Done),
        guard: Some(land_guard),
        plan: Some(plan_land),
    },
    Transition {
        from: TaskStatus::NeedsReview,
        event: TaskEvent::Land,
        to: Dest::To(TaskStatus::Done),
        guard: Some(land_guard),
        plan: Some(plan_land),
    },
    // --- putting it back -------------------------------------------------------------------
    Transition {
        from: TaskStatus::InProgress,
        event: TaskEvent::Abandon,
        to: Dest::To(TaskStatus::Open),
        guard: Some(abandon_guard),
        plan: Some(plan_abandon),
    },
    Transition {
        from: TaskStatus::NeedsReview,
        event: TaskEvent::Abandon,
        to: Dest::To(TaskStatus::Open),
        guard: Some(abandon_guard),
        plan: Some(plan_abandon),
    },
    // --- ending it -------------------------------------------------------------------------
    Transition {
        from: TaskStatus::Open,
        event: TaskEvent::Cancel,
        to: Dest::To(TaskStatus::Cancelled),
        guard: None,
        plan: Some(plan_cancel),
    },
    Transition {
        from: TaskStatus::InProgress,
        event: TaskEvent::Cancel,
        to: Dest::To(TaskStatus::Cancelled),
        guard: None,
        plan: Some(plan_cancel),
    },
    Transition {
        from: TaskStatus::NeedsReview,
        event: TaskEvent::Cancel,
        to: Dest::To(TaskStatus::Cancelled),
        guard: None,
        plan: Some(plan_cancel),
    },
    Transition {
        from: TaskStatus::Done,
        event: TaskEvent::Reopen,
        to: Dest::To(TaskStatus::Open),
        guard: None,
        plan: Some(plan_reopen),
    },
    Transition {
        from: TaskStatus::Cancelled,
        event: TaskEvent::Reopen,
        to: Dest::To(TaskStatus::Open),
        guard: None,
        plan: Some(plan_reopen),
    },
    // --- reconciliation --------------------------------------------------------------------
    Transition {
        from: TaskStatus::InProgress,
        event: TaskEvent::ObservedLanded,
        to: Dest::To(TaskStatus::Done),
        guard: Some(landed_externally),
        plan: Some(plan_land),
    },
    Transition {
        from: TaskStatus::NeedsReview,
        event: TaskEvent::ObservedLanded,
        to: Dest::To(TaskStatus::Done),
        guard: Some(landed_externally),
        plan: Some(plan_land),
    },
    Transition {
        from: TaskStatus::Open,
        event: TaskEvent::ObservedOwnerGone,
        to: Dest::To(TaskStatus::Open),
        guard: Some(owner_gone),
        plan: Some(plan_release),
    },
    Transition {
        from: TaskStatus::InProgress,
        event: TaskEvent::ObservedOwnerGone,
        to: Dest::To(TaskStatus::InProgress),
        guard: Some(owner_gone),
        plan: Some(plan_release),
    },
    Transition {
        from: TaskStatus::NeedsReview,
        event: TaskEvent::ObservedOwnerGone,
        to: Dest::To(TaskStatus::NeedsReview),
        guard: Some(owner_gone),
        plan: Some(plan_release),
    },
    // --- stated destinations ---------------------------------------------------------------
    Transition {
        from: TaskStatus::Open,
        event: TaskEvent::Override,
        to: Dest::Stated(stated),
        guard: None,
        plan: Some(plan_stated),
    },
    Transition {
        from: TaskStatus::InProgress,
        event: TaskEvent::Override,
        to: Dest::Stated(stated),
        guard: None,
        plan: Some(plan_stated),
    },
    Transition {
        from: TaskStatus::NeedsReview,
        event: TaskEvent::Override,
        to: Dest::Stated(stated),
        guard: None,
        plan: Some(plan_stated),
    },
    Transition {
        from: TaskStatus::Done,
        event: TaskEvent::Override,
        to: Dest::Stated(stated),
        guard: None,
        plan: Some(plan_stated),
    },
    Transition {
        from: TaskStatus::Cancelled,
        event: TaskEvent::Override,
        to: Dest::Stated(stated),
        guard: None,
        plan: Some(plan_stated),
    },
    Transition {
        from: TaskStatus::Open,
        event: TaskEvent::SetFromFile,
        to: Dest::Stated(stated),
        guard: Some(file_is_authority),
        plan: Some(plan_stated),
    },
    Transition {
        from: TaskStatus::InProgress,
        event: TaskEvent::SetFromFile,
        to: Dest::Stated(stated),
        guard: Some(file_is_authority),
        plan: Some(plan_stated),
    },
    Transition {
        from: TaskStatus::NeedsReview,
        event: TaskEvent::SetFromFile,
        to: Dest::Stated(stated),
        guard: Some(file_is_authority),
        plan: Some(plan_stated),
    },
    Transition {
        from: TaskStatus::Done,
        event: TaskEvent::SetFromFile,
        to: Dest::Stated(stated),
        guard: Some(file_is_authority),
        plan: Some(plan_stated),
    },
    Transition {
        from: TaskStatus::Cancelled,
        event: TaskEvent::SetFromFile,
        to: Dest::Stated(stated),
        guard: Some(file_is_authority),
        plan: Some(plan_stated),
    },
];

/// The task lifecycle.
#[must_use]
pub fn machine() -> TaskMachine {
    Machine {
        transitions: ROWS,
        initial: TaskStatus::Open,
    }
}

/// Ask the lifecycle for `event`, given what has been observed.
#[must_use]
pub fn apply(facts: &TaskFacts, event: TaskEvent) -> Outcome<TaskStatus, TaskEvent, TaskEffect> {
    machine().apply(facts, event)
}

/// Take one reconciliation step against what has been observed.
#[must_use]
pub fn reconcile(facts: &TaskFacts) -> Reconciliation<TaskStatus, TaskEvent, TaskEffect> {
    machine().reconcile(facts)
}

#[cfg(test)]
mod tests;
