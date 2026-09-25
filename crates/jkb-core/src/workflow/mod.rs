//! Task workflows: the phases a piece of work passes through, who may move it, and what the path
//! forward is (design D52.5, `openspec/changes/jkb-rbac-workflows/`).
//!
//! A second axis over a task, beside its lifecycle status ([`crate::lifecycle`]): the lifecycle says
//! whether the task is claimed, in review or landed; the workflow says *which kind of work* is owed
//! next — a design, an implementation, a review round, a systemic review — and therefore which role
//! acts. `in_progress` spans design, implementation and review alike, which is why the two are not
//! one machine.
//!
//! **A strategy is a composition** ([`strategy`]): an execution **graph** (a `&'static` table below,
//! walked by `jkb-fsm`'s checks like every other machine here), permission **toggles** the operator
//! chooses, and **attributes** the graph's guards read. Named strategies are presets over that, and
//! the operator can define more without code.
//!
//! **This module is pure**, like [`crate::lifecycle`]: guards read [`WorkflowFacts`], which
//! [`store::observe`] gathers, so every rule is exercisable from a literal.

pub mod store;
pub mod strategy;

use jkb_fsm::{
    all_of, require_no, require_yes, Denial, Dest, Event, EventKind, Fact, Machine, State,
    Stateful, Transition, Verdict,
};
use jkb_types::TaskStatus;

/// Where a task's work stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// The design is being gathered and written.
    Design,
    /// The design waits for whoever the strategy says approves it.
    DesignReview,
    /// The code is being written — first, or again after a review found must-fixes.
    Implement,
    /// A review round is owed, or its result is being observed.
    Review,
    /// Review keeps finding the same areas: the systemic cause is being looked for.
    SystemicReview,
    /// The last round came back clean; the work waits for whoever the strategy says lands.
    Landable,
    /// It landed. Settled.
    Landed,
    /// It will not be done. Settled.
    Cancelled,
}

impl State for Phase {
    const ALL: &'static [Self] = &[
        Self::Design,
        Self::DesignReview,
        Self::Implement,
        Self::Review,
        Self::SystemicReview,
        Self::Landable,
        Self::Landed,
        Self::Cancelled,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Design => "design",
            Self::DesignReview => "design_review",
            Self::Implement => "implement",
            Self::Review => "review",
            Self::SystemicReview => "systemic_review",
            Self::Landable => "landable",
            Self::Landed => "landed",
            Self::Cancelled => "cancelled",
        }
    }

    fn is_settled(self) -> bool {
        matches!(self, Self::Landed | Self::Cancelled)
    }
}

impl Phase {
    /// The stored name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.name()
    }

    /// Parse a stored or typed name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.name() == name)
    }
}

/// Something that happens to a task's workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkflowEvent {
    /// The design is written.
    SubmitDesign,
    /// The design is approved.
    ApproveDesign,
    /// The design is sent back.
    RejectDesign,
    /// The implementation is ready for a review round.
    SubmitWork,
    /// The systemic review found a code-pattern fix: back to implementation.
    SubmitSystemic,
    /// The systemic review found that the design itself must change — something that changes how
    /// the operator understands the system (operator decision 6). Back to design.
    SystemicRedesign,
    /// A landable task needs more work after all.
    Rework,
    /// It will not be done.
    Cancel,
    /// Pick a landed or cancelled task back up.
    Reopen,
    /// The operator names the phase directly. [`Dest::Stated`], so excluded from liveness.
    Override,
    /// A new review round found no must-fix.
    ReviewPassed,
    /// A new review round found must-fixes in areas the previous rounds did not.
    ReviewFailed,
    /// A new review round found must-fixes in the same area as the rounds before it.
    ReviewRepeated,
    /// The task's lifecycle recorded a landing.
    ObservedLanded,
    /// The task's lifecycle says it was cancelled.
    ObservedCancelled,
}

impl Event for WorkflowEvent {
    const ALL: &'static [Self] = &[
        Self::SubmitDesign,
        Self::ApproveDesign,
        Self::RejectDesign,
        Self::SubmitWork,
        Self::SubmitSystemic,
        Self::SystemicRedesign,
        Self::Rework,
        Self::Cancel,
        Self::Reopen,
        Self::Override,
        Self::ReviewPassed,
        Self::ReviewFailed,
        Self::ReviewRepeated,
        Self::ObservedLanded,
        Self::ObservedCancelled,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::SubmitDesign => "submit_design",
            Self::ApproveDesign => "approve_design",
            Self::RejectDesign => "reject_design",
            Self::SubmitWork => "submit_work",
            Self::SubmitSystemic => "submit_systemic",
            Self::SystemicRedesign => "systemic_redesign",
            Self::Rework => "rework",
            Self::Cancel => "cancel",
            Self::Reopen => "reopen",
            Self::Override => "override",
            Self::ReviewPassed => "review_passed",
            Self::ReviewFailed => "review_failed",
            Self::ReviewRepeated => "review_repeated",
            Self::ObservedLanded => "observed_landed",
            Self::ObservedCancelled => "observed_cancelled",
        }
    }

    fn kind(self) -> EventKind {
        match self {
            Self::ReviewPassed
            | Self::ReviewFailed
            | Self::ReviewRepeated
            | Self::ObservedLanded
            | Self::ObservedCancelled => EventKind::Reconciled,
            _ => EventKind::Applied,
        }
    }
}

/// A workflow event is also a permission: who may fire it ([`strategy::Strategy::permissions`]).
impl jkb_rbac::Permission for WorkflowEvent {
    const ALL: &'static [Self] = <Self as Event>::ALL;

    fn name(self) -> &'static str {
        <Self as Event>::name(self)
    }
}

impl WorkflowEvent {
    /// The stored name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        <Self as Event>::name(self)
    }

    /// Parse a stored or typed name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        <Self as Event>::ALL
            .iter()
            .copied()
            .find(|e| e.as_str() == name)
    }

    /// Whether the event must carry a written reason. The systemic reviewer's choice between
    /// [`Self::SubmitSystemic`] and [`Self::SystemicRedesign`] decides whether the operator sees the
    /// fix, so the reason is on record for them to judge afterwards; an override is the operator's
    /// own, and says why.
    #[must_use]
    pub const fn needs_reason(self) -> bool {
        matches!(
            self,
            Self::SubmitSystemic | Self::SystemicRedesign | Self::Override
        )
    }
}

/// Everything a workflow guard may look at.
#[derive(Debug, Clone)]
pub struct WorkflowFacts {
    /// The phase — the machine's state.
    pub phase: Phase,
    /// The phase an `override` names.
    pub stated: Option<Phase>,
    /// The task's lifecycle status.
    pub task_status: TaskStatus,
    /// A landing transition speaks for the work — **not** `status = done`, which a synced checkbox
    /// can write (H4).
    pub landed: Fact,
    /// A review round was filed after the task last entered `review`.
    pub new_round: Fact,
    /// The newest round found at least one must-fix.
    pub last_round_must_fix: Fact,
    /// The newest rounds (as many as the strategy's `repeated_area` attribute says) all found
    /// must-fixes in a shared area.
    pub repeated_areas: Fact,
}

impl WorkflowFacts {
    /// Facts about a task at `phase` where nothing has been observed.
    #[must_use]
    pub const fn at(phase: Phase) -> Self {
        Self {
            phase,
            stated: None,
            task_status: TaskStatus::Open,
            landed: Fact::No,
            new_round: Fact::No,
            last_round_must_fix: Fact::No,
            repeated_areas: Fact::No,
        }
    }
}

impl Stateful<Phase> for WorkflowFacts {
    fn state(&self) -> Phase {
        self.phase
    }
}

/// A workflow machine. Workflows plan no effects of their own: what a move entails beyond the log
/// row (revoking a settled task's worker grants) is done by [`store`], once, for every move.
pub type WorkflowMachine = Machine<Phase, WorkflowEvent, WorkflowFacts, ()>;

// -------------------------------------------------------------------------------------------
// Guards
// -------------------------------------------------------------------------------------------

/// Neither landed nor cancelled: the review reconciliations must not compete with the observed
/// ends, so each asserts the task is still live.
fn live(f: &WorkflowFacts) -> Verdict<WorkflowEvent> {
    all_of([
        require_no(f.landed, || Denial::new("The work has already landed.")),
        require_no(Fact::from(f.task_status == TaskStatus::Cancelled), || {
            Denial::new("The task is cancelled.")
        }),
    ])
}

fn fresh_round(f: &WorkflowFacts) -> Verdict<WorkflowEvent> {
    require_yes(f.new_round, || {
        Denial::new(
            "No review round has been filed since the work was submitted. Run one \
             (`/jkb-review-log`), then observe again.",
        )
    })
}

fn review_passed(f: &WorkflowFacts) -> Verdict<WorkflowEvent> {
    all_of([
        live(f),
        fresh_round(f),
        require_no(f.last_round_must_fix, || {
            Denial::new("The newest round found must-fix findings.")
        }),
    ])
}

fn review_failed(f: &WorkflowFacts) -> Verdict<WorkflowEvent> {
    all_of([
        live(f),
        fresh_round(f),
        require_yes(f.last_round_must_fix, || {
            Denial::new("The newest round found no must-fix.")
        }),
        require_no(f.repeated_areas, || {
            Denial::new("The must-fixes repeat an area the rounds before found.")
        }),
    ])
}

fn review_repeated(f: &WorkflowFacts) -> Verdict<WorkflowEvent> {
    all_of([
        live(f),
        fresh_round(f),
        require_yes(f.last_round_must_fix, || {
            Denial::new("The newest round found no must-fix.")
        }),
        require_yes(f.repeated_areas, || {
            Denial::new("The must-fixes do not repeat an area.")
        }),
    ])
}

fn observed_landed(f: &WorkflowFacts) -> Verdict<WorkflowEvent> {
    require_yes(f.landed, || {
        Denial::new("No landing is recorded for this work (a `done` checkbox is not a landing).")
    })
}

fn observed_cancelled(f: &WorkflowFacts) -> Verdict<WorkflowEvent> {
    all_of([
        require_yes(Fact::from(f.task_status == TaskStatus::Cancelled), || {
            Denial::new("The task is not cancelled.")
        }),
        require_no(f.landed, || Denial::new("The work landed.")),
    ])
}

fn stated(f: &WorkflowFacts) -> Option<Phase> {
    f.stated
}

// -------------------------------------------------------------------------------------------
// Graphs
// -------------------------------------------------------------------------------------------

type Row = Transition<Phase, WorkflowEvent, WorkflowFacts, ()>;

const fn row(from: Phase, event: WorkflowEvent, to: Phase) -> Row {
    Transition {
        from,
        event,
        to: Dest::To(to),
        guard: None,
        plan: None,
    }
}

const fn guarded(
    from: Phase,
    event: WorkflowEvent,
    to: Phase,
    guard: fn(&WorkflowFacts) -> Verdict<WorkflowEvent>,
) -> Row {
    Transition {
        from,
        event,
        to: Dest::To(to),
        guard: Some(guard),
        plan: None,
    }
}

const fn override_from(from: Phase) -> Row {
    Transition {
        from,
        event: WorkflowEvent::Override,
        to: Dest::Stated(stated),
        guard: None,
        plan: None,
    }
}

/// A graph's table: its own `submit_design` row, then every row every graph shares. A macro rather
/// than two copies, so the shared rows cannot drift between graphs — the twinned-rule defect this
/// repository keeps paying for.
macro_rules! graph {
    ($submit_design:expr) => {
        &[
            $submit_design,
            row(
                Phase::DesignReview,
                WorkflowEvent::ApproveDesign,
                Phase::Implement,
            ),
            row(
                Phase::DesignReview,
                WorkflowEvent::RejectDesign,
                Phase::Design,
            ),
            row(Phase::Implement, WorkflowEvent::SubmitWork, Phase::Review),
            guarded(
                Phase::Review,
                WorkflowEvent::ReviewPassed,
                Phase::Landable,
                review_passed,
            ),
            guarded(
                Phase::Review,
                WorkflowEvent::ReviewFailed,
                Phase::Implement,
                review_failed,
            ),
            guarded(
                Phase::Review,
                WorkflowEvent::ReviewRepeated,
                Phase::SystemicReview,
                review_repeated,
            ),
            row(
                Phase::SystemicReview,
                WorkflowEvent::SubmitSystemic,
                Phase::Implement,
            ),
            row(
                Phase::SystemicReview,
                WorkflowEvent::SystemicRedesign,
                Phase::Design,
            ),
            row(Phase::Landable, WorkflowEvent::Rework, Phase::Implement),
            // Every live phase can observe its end, be cancelled, and be overridden.
            guarded(
                Phase::Design,
                WorkflowEvent::ObservedLanded,
                Phase::Landed,
                observed_landed,
            ),
            guarded(
                Phase::DesignReview,
                WorkflowEvent::ObservedLanded,
                Phase::Landed,
                observed_landed,
            ),
            guarded(
                Phase::Implement,
                WorkflowEvent::ObservedLanded,
                Phase::Landed,
                observed_landed,
            ),
            guarded(
                Phase::Review,
                WorkflowEvent::ObservedLanded,
                Phase::Landed,
                observed_landed,
            ),
            guarded(
                Phase::SystemicReview,
                WorkflowEvent::ObservedLanded,
                Phase::Landed,
                observed_landed,
            ),
            guarded(
                Phase::Landable,
                WorkflowEvent::ObservedLanded,
                Phase::Landed,
                observed_landed,
            ),
            guarded(
                Phase::Design,
                WorkflowEvent::ObservedCancelled,
                Phase::Cancelled,
                observed_cancelled,
            ),
            guarded(
                Phase::DesignReview,
                WorkflowEvent::ObservedCancelled,
                Phase::Cancelled,
                observed_cancelled,
            ),
            guarded(
                Phase::Implement,
                WorkflowEvent::ObservedCancelled,
                Phase::Cancelled,
                observed_cancelled,
            ),
            guarded(
                Phase::Review,
                WorkflowEvent::ObservedCancelled,
                Phase::Cancelled,
                observed_cancelled,
            ),
            guarded(
                Phase::SystemicReview,
                WorkflowEvent::ObservedCancelled,
                Phase::Cancelled,
                observed_cancelled,
            ),
            guarded(
                Phase::Landable,
                WorkflowEvent::ObservedCancelled,
                Phase::Cancelled,
                observed_cancelled,
            ),
            row(Phase::Design, WorkflowEvent::Cancel, Phase::Cancelled),
            row(Phase::DesignReview, WorkflowEvent::Cancel, Phase::Cancelled),
            row(Phase::Implement, WorkflowEvent::Cancel, Phase::Cancelled),
            row(Phase::Review, WorkflowEvent::Cancel, Phase::Cancelled),
            row(
                Phase::SystemicReview,
                WorkflowEvent::Cancel,
                Phase::Cancelled,
            ),
            row(Phase::Landable, WorkflowEvent::Cancel, Phase::Cancelled),
            row(Phase::Landed, WorkflowEvent::Reopen, Phase::Implement),
            row(Phase::Cancelled, WorkflowEvent::Reopen, Phase::Implement),
            override_from(Phase::Design),
            override_from(Phase::DesignReview),
            override_from(Phase::Implement),
            override_from(Phase::Review),
            override_from(Phase::SystemicReview),
            override_from(Phase::Landable),
            override_from(Phase::Landed),
            override_from(Phase::Cancelled),
        ]
    };
}

/// `reviewed-design`: a written design waits in `design_review` for whoever the strategy's
/// `approves_design` toggle names.
static REVIEWED_DESIGN: &[Row] = graph!(row(
    Phase::Design,
    WorkflowEvent::SubmitDesign,
    Phase::DesignReview
));

/// `direct`: a written design goes straight to implementation. `design_review` is reachable only by
/// `override`, and `approve_design` leaves it.
static DIRECT: &[Row] = graph!(row(
    Phase::Design,
    WorkflowEvent::SubmitDesign,
    Phase::Implement
));

/// Which execution graph a strategy runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GraphId {
    /// A design is reviewed before implementation starts.
    ReviewedDesign,
    /// A design goes straight to implementation.
    Direct,
}

impl GraphId {
    /// Every graph, for the checks and for listing.
    pub const ALL: &'static [Self] = &[Self::ReviewedDesign, Self::Direct];

    /// The stored and typed name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReviewedDesign => "reviewed-design",
            Self::Direct => "direct",
        }
    }

    /// Parse a name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|g| g.as_str() == name)
    }

    /// One line on what it is.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::ReviewedDesign => {
                "design → design review → implement ⇄ review (⇄ systemic review) → landable"
            }
            Self::Direct => "design → implement ⇄ review (⇄ systemic review) → landable",
        }
    }

    /// The graph as a checkable machine.
    #[must_use]
    pub fn machine(self) -> WorkflowMachine {
        Machine {
            transitions: match self {
                Self::ReviewedDesign => REVIEWED_DESIGN,
                Self::Direct => DIRECT,
            },
            initial: Phase::Design,
        }
    }

    /// Whether this graph reaches `phase` other than by an operator override — what a toggle that
    /// only acts there needs, or it says nothing.
    #[must_use]
    pub fn reaches_unforced(self, phase: Phase) -> bool {
        let m = self.machine();
        let mut seen = vec![m.initial];
        let mut i = 0;
        while i < seen.len() {
            let from = seen[i];
            for t in m.transitions {
                if t.from == from {
                    if let Dest::To(to) = t.to {
                        if !seen.contains(&to) {
                            seen.push(to);
                        }
                    }
                }
            }
            i += 1;
        }
        seen.contains(&phase)
    }
}

#[cfg(test)]
mod tests;
