//! The machine: states, guarded transitions, and the outcome of asking for one.

use std::fmt;
use std::fmt::Write as _;

use crate::Fact;

/// A lifecycle state.
///
/// [`Self::ALL`] is required rather than optional because every check in [`crate::check`]
/// walks it: a state missing from the list is a state nothing verifies is reachable, can reach
/// a terminal, or accepts anything. Declare it beside the enum (a `db_enum!`-style macro or a
/// hand-written const), never in a second place that can drift.
pub trait State: Copy + Eq + std::hash::Hash + fmt::Debug + 'static {
    /// Every state, in declaration order.
    const ALL: &'static [Self];

    /// The stable, human-facing name (matches the stored spelling where there is one).
    fn name(self) -> &'static str;

    /// Whether the object is **at rest** here: it owes the system nothing further.
    ///
    /// At least one state must be, and every state must be able to reach one — see
    /// [`crate::Defect::Wedged`].
    ///
    /// Deliberately *rest*, not *finished*. This was `is_terminal` while there was one machine,
    /// and the second one broke it: a synced file is never finished — it settles and is then
    /// edited again — so it either had no terminal state, making the liveness checks vacuous,
    /// or had to lie about one. What the checks actually want is the state in which nothing is
    /// owed. For a task that is `done`/`cancelled`; for a synced file it is `ok`; for an
    /// investigation unit it would be any resolution other than `unresolved`.
    fn is_settled(self) -> bool;

    /// Whether this state legitimately has nothing the *system* can do: it is waiting on a
    /// person.
    ///
    /// Default `false`, and a lifecycle usually leaves it there — a task always has an operator
    /// escape (`cancel`), so a task state with no available move is a defect.
    ///
    /// A **reconciler** is different, and this is the second thing the second machine forced.
    /// A synced file in `conflict` is waiting for you to edit it; until you do, no observation
    /// moves it, and that is correct rather than stuck. Without this,
    /// [`crate::Defect::DeadEnd`] reports every such observation and the check trains its reader
    /// to ignore it.
    ///
    /// [`crate::Defect::Wedged`] still applies to these states, and it is the one that matters
    /// for them: waiting on a person is fine, having *no edge back to rest at all* is the
    /// "refused on every pass for ever" defect.
    fn awaits_input(self) -> bool {
        false
    }
}

/// The observation a guard reads, which **is** where the object's state lives.
///
/// The state is not passed beside the context, it is read out of it. That is deliberate: the
/// two were separate arguments in the first version of this crate, and the very first machine
/// built on it had a guard that branched on `ctx.status` while the machine was being asked
/// about a different state — a disagreement nothing could have detected, because there was
/// nothing to compare. One source, so there is no second one to drift.
pub trait Stateful<S: State> {
    /// Where the object is now.
    fn state(&self) -> S;
}

/// Something that can happen to the object: a request, or an observation.
pub trait Event: Copy + Eq + std::hash::Hash + fmt::Debug + 'static {
    /// Every event, in declaration order.
    const ALL: &'static [Self];

    /// The stable, human-facing name.
    fn name(self) -> &'static str;

    /// Whether this event is asked for or detected. See [`EventKind`].
    fn kind(self) -> EventKind;
}

/// Whether an event is asked for by somebody, or detected by looking at the world.
///
/// The distinction is load-bearing, not decorative (design S1.7). A [`EventKind::Reconciled`]
/// event **must** carry a guard — an unguarded reconciliation is a state change with no
/// evidence behind it — and it fires only through [`Machine::reconcile`], which refuses
/// ambiguity rather than picking the first match in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// Somebody asked for this: a command, a button, a script.
    Applied,
    /// The world moved and we noticed: a branch merged, a worktree vanished, an agent died.
    Reconciled,
}

/// A guard's answer.
pub enum Verdict<E> {
    /// The transition may proceed.
    Allow,
    /// It may not, for this reason — and here is the event that would unstick it.
    Deny(Denial<E>),
}

/// Why a transition was refused, and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial<E> {
    /// A sentence for a person, naming the fact that is missing or wrong.
    pub reason: String,
    /// The way out, if there is one — as an **event of this machine**, never free text.
    ///
    /// That restriction is the point (design S1.4). Advice written as prose cannot be checked,
    /// and this repository has three separate must-fix findings where following a refusal's own
    /// printed advice made the situation permanently worse. A remedy that names an event can be
    /// checked — [`Machine::validate_remedy`] does, and [`Machine::audit`] does it exhaustively
    /// — and a remedy that *cannot* be expressed as an event is a sign the machine is missing a
    /// transition, which is exactly the thing worth discovering.
    pub remedy: Option<Remedy<E>>,
}

impl<E> Denial<E> {
    /// A refusal with no way out from here. Use sparingly: a state that can only ever refuse is
    /// [`crate::Defect::DeadEnd`], which [`Machine::audit`] reports.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            remedy: None,
        }
    }

    /// A refusal plus the event that would unstick it, and how a person triggers it.
    pub fn with_remedy(reason: impl Into<String>, event: E, how: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            remedy: Some(Remedy {
                event,
                how: how.into(),
            }),
        }
    }
}

/// The event that would unstick a refusal, and the incantation that fires it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remedy<E> {
    /// The transition to take instead. Checked against the machine.
    pub event: E,
    /// How a person triggers it (`jkb task set <uid> --status open`).
    pub how: String,
}

/// Where a transition goes.
///
/// Most rows name a destination. Some cannot: an **operator override** ("set the status to
/// whatever I say") and an **external authority** ("the synced file declares this task done")
/// state the destination themselves, and modelling those as one row per target would be both
/// unreadable and dishonest — the point of such a transition is precisely that the table does
/// not decide.
///
/// They are still transitions, with guards, effects and a place in the diagram; what they are
/// not is a way *the lifecycle* gets somewhere. So a [`Dest::Stated`] edge is **excluded from
/// the liveness checks** ([`crate::Defect::Wedged`] and [`crate::Defect::DeadEnd`]): a state
/// whose only exit is somebody naming a different one is wedged, and counting the escape hatch
/// as an exit would make both checks vacuous for any machine that has one.
pub enum Dest<S, C> {
    /// The table decides.
    To(S),
    /// The caller decides, from the context. `None` means it named nothing usable, which is a
    /// refusal rather than a silent stay.
    Stated(fn(&C) -> Option<S>),
}

/// One row of the transition table.
///
/// Built as a `const`, so the whole table is `&'static` data that the checks in
/// [`crate::check`] can walk.
pub struct Transition<S: State, E: Event, C, X: 'static> {
    /// The state this applies in.
    pub from: S,
    /// The event it answers.
    pub event: E,
    /// Where it goes. May resolve to `from`: a self-loop is how an effect-only transition
    /// (release a claim) or an owner-dependent re-entry (re-claim) is spelled.
    pub to: Dest<S, C>,
    /// What must be true. `None` means unconditional — permitted only for
    /// [`EventKind::Applied`] events.
    pub guard: Option<fn(&C) -> Verdict<E>>,
    /// What must happen alongside the state change, as data the caller performs.
    ///
    /// `None` means "the state change is the whole effect". The plan is produced *with* the
    /// move, as one value, so a caller cannot apply the status write and forget the claim
    /// release — which is a real incident in this repository's history (design S1.3).
    pub plan: Option<fn(&C) -> Vec<X>>,
}

/// Whether the machine has an answer for `(state, event)`, structurally — guards not consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance<S> {
    /// A transition is declared, subject to its guard.
    Moves(S),
    /// A transition is declared whose destination the caller states ([`Dest::Stated`]), so
    /// where it goes is not knowable without the context.
    MovesAsStated,
    /// No transition is declared, but this state is the destination of this event elsewhere:
    /// the event has already achieved what it asks for, so it is a no-op (design S1.6).
    Idempotent,
    /// The machine has nothing to say. Answered as [`Outcome::Undefined`], never silently.
    Undefined,
}

/// What came of asking the machine for a transition.
///
/// Total: every `(state, event)` pair produces one of these. There is no arm that quietly does
/// nothing, which is how a flag comes to be silently ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<S, E, X> {
    /// It moved. Apply **all** the effects, in one transaction, or none of them.
    Moved {
        /// Where it was.
        from: S,
        /// What was asked.
        event: E,
        /// Where it now is.
        to: S,
        /// Everything that must accompany the move.
        effects: Vec<X>,
    },
    /// Already there. No writes, no refusal — asking twice is not an error (design S1.6).
    Idempotent {
        /// The unchanged state.
        state: S,
        /// What was asked.
        event: E,
    },
    /// A guard said no, with a reason and (usually) a way out.
    Refused {
        /// The unchanged state.
        state: S,
        /// What was asked.
        event: E,
        /// Why, and what to do.
        denial: Denial<E>,
    },
    /// The machine declares nothing for this pair. A named refusal, not a shrug.
    Undefined {
        /// The unchanged state.
        state: S,
        /// What was asked.
        event: E,
    },
}

impl<S: State, E: Event, X> Outcome<S, E, X> {
    /// Whether anything changed.
    pub fn moved(&self) -> bool {
        matches!(self, Self::Moved { .. })
    }

    /// The state after this outcome — unchanged unless it moved.
    pub fn state(&self) -> S {
        match self {
            Self::Moved { to, .. } => *to,
            Self::Idempotent { state, .. }
            | Self::Refused { state, .. }
            | Self::Undefined { state, .. } => *state,
        }
    }

    /// The effects to perform, empty unless it moved.
    pub fn effects(&self) -> &[X] {
        match self {
            Self::Moved { effects, .. } => effects,
            _ => &[],
        }
    }

    /// Why this was refused, as a sentence, or `None` if it was not.
    ///
    /// [`Self::Undefined`] gets one too: "this machine has no `land` from `cancelled`" is a
    /// refusal a person can act on, and phrasing it as an internal error would push callers
    /// back to writing their own state checks.
    pub fn refusal(&self) -> Option<String> {
        match self {
            Self::Refused { denial, .. } => Some(match &denial.remedy {
                Some(r) => format!("{} {}", denial.reason, r.how),
                None => denial.reason.clone(),
            }),
            Self::Undefined { state, event } => Some(format!(
                "`{}` is not something that can happen to a task that is `{}`.",
                event.name(),
                state.name()
            )),
            _ => None,
        }
    }
}

/// One reconciliation step (design S1.7).
///
/// Deliberately **one** step. The context was observed against the state the object was in, so
/// re-running guards against it after a move would be reasoning from stale facts; a caller that
/// wants a fixpoint re-observes and calls again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciliation<S, E, X> {
    /// Nothing the world says requires a move.
    Settled,
    /// Exactly one reconciliation applies.
    Fired(Outcome<S, E, X>),
    /// Two or more state-*changing* reconciliations apply at once, so nothing is done.
    ///
    /// Refusing rather than taking the first in declaration order is the same rule `undo`
    /// arrived at the hard way: when the machine cannot tell which thing happened, acting on a
    /// guess reverts something nobody asked about.
    Ambiguous(Vec<E>),
}

/// A declared lifecycle: the transition table plus where objects start.
pub struct Machine<S: State, E: Event, C: Stateful<S> + 'static, X: 'static> {
    /// The rows. Order is irrelevant to behaviour — [`crate::Defect::Nondeterministic`]
    /// forbids two rows for one `(from, event)` pair, so nothing is decided by position.
    pub transitions: &'static [Transition<S, E, C, X>],
    /// The state a freshly created object is in. Reachability is measured from here.
    pub initial: S,
}

impl<S: State, E: Event, C: Stateful<S> + 'static, X: 'static> Machine<S, E, C, X> {
    /// Whether this machine has a transition for `(state, event)`, before guards.
    ///
    /// The idempotence rule (design S1.6) lives here: **the destination of a transition accepts
    /// that transition's own event as a no-op, unless the table declares otherwise.** So
    /// `start` on an already-started task is not an error, and no verb has to remember that.
    /// A domain that wants different behaviour there declares the self-loop, and the declared
    /// row wins.
    pub fn accepts(&self, state: S, event: E) -> Acceptance<S> {
        if let Some(t) = self.row(state, event) {
            return match t.to {
                Dest::To(to) => Acceptance::Moves(to),
                Dest::Stated(_) => Acceptance::MovesAsStated,
            };
        }
        // Only a *declared* destination absorbs its own event. A stated one names no state, so
        // there is nothing for it to be already-achieved at.
        if self
            .transitions
            .iter()
            .any(|t| t.event == event && matches!(t.to, Dest::To(to) if to == state))
        {
            return Acceptance::Idempotent;
        }
        Acceptance::Undefined
    }

    /// Whether `(state, event)`'s destination is stated by the caller rather than by the table.
    ///
    /// The liveness checks ask this: an escape hatch is not an exit (see [`Dest`]).
    pub(crate) fn is_stated(&self, state: S, event: E) -> bool {
        self.row(state, event)
            .is_some_and(|t| matches!(t.to, Dest::Stated(_)))
    }

    /// The declared row for `(from, event)`, if any.
    fn row(&self, from: S, event: E) -> Option<&Transition<S, E, C, X>> {
        self.transitions
            .iter()
            .find(|t| t.from == from && t.event == event)
    }

    /// Ask for `event` in `state`, given what has been observed.
    ///
    /// Total — see [`Outcome`]. When it returns [`Outcome::Moved`], the effects are the
    /// complete set that must accompany the move; apply them in one transaction or not at all.
    ///
    /// A guard's remedy is not validated here; see [`Machine::audit`], which validates every
    /// remedy the machine can produce over a whole context matrix.
    pub fn apply(&self, ctx: &C, event: E) -> Outcome<S, E, X> {
        let state = ctx.state();
        let Some(t) = self.row(state, event) else {
            return match self.accepts(state, event) {
                Acceptance::Idempotent => Outcome::Idempotent { state, event },
                _ => Outcome::Undefined { state, event },
            };
        };
        if let Some(guard) = t.guard {
            if let Verdict::Deny(denial) = guard(ctx) {
                // The remedy is deliberately **not** asserted on here. Panicking would make the
                // reporting path untestable (an audit could never collect the defect it exists
                // to collect) and would turn a bad sentence into a crash in production.
                // [`Machine::audit`] is the enforcement, and it is exhaustive over the contexts
                // it is given, which is stronger than a check on whichever path happened to run.
                return Outcome::Refused {
                    state,
                    event,
                    denial,
                };
            }
        }
        let Some(to) = resolve(&t.to, ctx) else {
            return Outcome::Refused {
                state,
                event,
                denial: Denial::new(format!(
                    "`{}` needs a target state and none was given.",
                    event.name()
                )),
            };
        };
        Outcome::Moved {
            from: state,
            event,
            to,
            effects: t.plan.map(|p| p(ctx)).unwrap_or_default(),
        }
    }

    /// Whether a denial's remedy names an event this machine accepts from `state`.
    ///
    /// Returns the offending remedy event when it does not. Guards are not consulted: a remedy
    /// may legitimately be refused for its own reasons once tried, but a remedy the machine has
    /// no transition for at all is advice that cannot possibly work.
    pub fn validate_remedy(&self, state: S, denial: &Denial<E>) -> Option<E> {
        let remedy = denial.remedy.as_ref()?;
        match self.accepts(state, remedy.event) {
            Acceptance::Undefined => Some(remedy.event),
            _ => None,
        }
    }

    /// Take one reconciliation step: apply what the world says has already happened.
    ///
    /// The **only** way a [`EventKind::Reconciled`] event fires. Precedence, stated because it
    /// is a real decision:
    ///
    /// 1. If two or more allowed reconciliations would change the state, refuse
    ///    ([`Reconciliation::Ambiguous`]) and do nothing.
    /// 2. Otherwise a state-changing reconciliation wins over an effect-only self-loop. The
    ///    self-loop is not lost: it fires on the next step, against freshly observed facts.
    /// 3. Two allowed self-loops are ambiguous too — they are competing effects on one object.
    /// 4. A [`Dest::Stated`] reconciliation is not a candidate at all; see the filter below.
    pub fn reconcile(&self, ctx: &C) -> Reconciliation<S, E, X> {
        let state = ctx.state();
        let mut moves = Vec::new();
        let mut loops = Vec::new();
        for t in self.transitions.iter().filter(|t| {
            // A [`Dest::Stated`] reconciliation is **dictated** by its observer, not implied by
            // the world, so there is nothing for a generic driver to work out — and including
            // it would make every file-backed object's reconciliation permanently ambiguous
            // against every other observer's. Its observer calls `apply` with it directly; the
            // guard still runs.
            t.from == state
                && t.event.kind() == EventKind::Reconciled
                && matches!(t.to, Dest::To(_))
        }) {
            let allowed = t.guard.is_none_or(|g| matches!(g(ctx), Verdict::Allow));
            if !allowed {
                continue;
            }
            // A destination that resolves to nowhere is not evidence of anything; `apply` would
            // refuse it, so `reconcile` does not offer it as a candidate.
            match resolve(&t.to, ctx) {
                Some(to) if to == state => loops.push(t.event),
                Some(_) => moves.push(t.event),
                None => {}
            }
        }
        let chosen = match (moves.len(), loops.len()) {
            (0, 0) => return Reconciliation::Settled,
            (0, 1) => loops[0],
            (1, _) => moves[0],
            (0, _) => return Reconciliation::Ambiguous(loops),
            _ => return Reconciliation::Ambiguous(moves),
        };
        Reconciliation::Fired(self.apply(ctx, chosen))
    }

    /// Every event this machine could accept from `state`, structurally.
    ///
    /// Used by [`Machine::audit`]'s dead-end check and by `--help`-style surfaces that want to
    /// list what is possible from here.
    pub fn accepted_from(&self, state: S) -> Vec<E> {
        let mut out: Vec<E> = Vec::new();
        for &event in E::ALL {
            if !matches!(self.accepts(state, event), Acceptance::Undefined) {
                out.push(event);
            }
        }
        out
    }

    /// The transition table as a Graphviz digraph.
    ///
    /// The artifact whose absence is the first item of this design's problem statement: a
    /// lifecycle nobody can look at gets re-derived at every call site. Reconciliation edges are
    /// dashed, so the two kinds are distinguishable at a glance.
    pub fn dot(&self, name: &str) -> String {
        let mut out = format!("digraph {name} {{\n  rankdir=LR;\n");
        for s in S::ALL {
            let shape = if s.is_settled() {
                "doublecircle"
            } else {
                "ellipse"
            };
            let weight = if *s == self.initial {
                ", penwidth=2"
            } else {
                ""
            };
            let _ = writeln!(out, "  \"{}\" [shape={shape}{weight}];", s.name());
        }
        let mut any_stated = false;
        for t in self.transitions {
            let style = match t.event.kind() {
                EventKind::Applied => "solid",
                EventKind::Reconciled => "dashed",
            };
            let to = match t.to {
                Dest::To(to) => to.name(),
                Dest::Stated(_) => {
                    any_stated = true;
                    "*"
                }
            };
            let _ = writeln!(
                out,
                "  \"{}\" -> \"{to}\" [label=\"{}\", style={style}];",
                t.from.name(),
                t.event.name()
            );
        }
        if any_stated {
            // The escape hatch is drawn, and drawn as the thing it is: a destination somebody
            // names, not one the lifecycle offers.
            out.push_str("  \"*\" [shape=plaintext, label=\"(stated)\"];\n");
        }
        out.push_str("}\n");
        out
    }
}

/// The destination a row resolves to in this context, or `None` if the caller stated nothing.
fn resolve<S: State, C>(dest: &Dest<S, C>, ctx: &C) -> Option<S> {
    match dest {
        Dest::To(to) => Some(*to),
        Dest::Stated(f) => f(ctx),
    }
}

/// A guard helper: allow when `fact` is **proven true**, else deny with `reason`.
///
/// Written as a helper so the polarity is one word rather than a hand-rolled `if`. There is no
/// `require_not` counterpart that negates this one — a guard that needs a fact proven *false*
/// calls [`require_no`], and the two names are what make the choice visible in review.
pub fn require_yes<E>(fact: Fact, denial: impl FnOnce() -> Denial<E>) -> Verdict<E> {
    if fact.is_yes() {
        Verdict::Allow
    } else {
        Verdict::Deny(denial())
    }
}

/// A guard helper: allow when `fact` is **proven false**, else deny.
///
/// `Unknown` denies, exactly as it does in [`require_yes`]. That is the asymmetry the type
/// exists for: "the checkout is clean" must fail when the checkout could not be read.
pub fn require_no<E>(fact: Fact, denial: impl FnOnce() -> Denial<E>) -> Verdict<E> {
    if fact.is_no() {
        Verdict::Allow
    } else {
        Verdict::Deny(denial())
    }
}

/// Run guard clauses in order, returning the first denial.
///
/// A macro-free `?`-like chain for guards, so a multi-clause guard reads as a list of
/// requirements rather than a staircase of early returns.
pub fn all_of<E>(clauses: impl IntoIterator<Item = Verdict<E>>) -> Verdict<E> {
    for clause in clauses {
        if let Verdict::Deny(d) = clause {
            return Verdict::Deny(d);
        }
    }
    Verdict::Allow
}
