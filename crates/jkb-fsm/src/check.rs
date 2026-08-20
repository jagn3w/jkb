//! The checks (design S1.5): what a declared table lets you verify that a hand-rolled `match`
//! does not.
//!
//! [`Machine::check`] is static — it walks the table and needs no context. [`Machine::audit`]
//! is dynamic — it walks every `(state, event)` pair against a caller-supplied set of observed
//! contexts, which is how the guard-dependent properties (a refusal whose remedy leads nowhere,
//! a state that under some observation accepts nothing) become checkable at all.
//!
//! Both return **every** defect they find rather than the first, because a table is edited as a
//! whole and reporting one row at a time turns one review into five.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;

use crate::machine::{
    Acceptance, Dest, Event, EventKind, Machine, Outcome, Reconciliation, State, Stateful,
};

/// Which way an adjacency walk follows the table: *what can this state reach*, or *what can
/// reach this state*.
#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Backward,
}

/// Something wrong with a machine, or with the advice it gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Defect<S, E> {
    /// Two rows declare `(from, event)`. The table would otherwise be decided by position,
    /// which makes a table's order load-bearing without saying so.
    Nondeterministic {
        /// The state both rows apply in.
        from: S,
        /// The event both rows answer.
        event: E,
    },
    /// No path from the initial state reaches this one. Dead code that reviewers nonetheless
    /// reason about, and a state nothing can ever be observed in.
    UnreachableState {
        /// The state nothing reaches.
        state: S,
    },
    /// No path from this state reaches a state where nothing is owed. Whatever enters it never
    /// gets back to rest: the static half of *"held for ever"* — a task that can never close, a
    /// synced file that is refused on every pass.
    Wedged {
        /// The state with no way to finish.
        state: S,
    },
    /// The machine declares no state at rest at all, so [`Self::Wedged`] would be vacuous.
    NoSettledState,
    /// A [`EventKind::Reconciled`] transition with no guard: a state change asserted from
    /// nothing observed. Reconciliation paths are supposed to be the *explicit* ones.
    UnguardedReconciliation {
        /// The state it fires in.
        from: S,
        /// The unguarded reconciliation.
        event: E,
    },
    /// An event that appears in no row. Either it is unwired, or the table is missing it —
    /// both worth knowing, and neither visible from a call site.
    UnusedEvent {
        /// The event nothing declares.
        event: E,
    },
    /// A guard refused and offered a remedy this machine does not accept from that state, so
    /// following the machine's own advice cannot work.
    UnreachableRemedy {
        /// Where the refusal happened.
        state: S,
        /// What was refused.
        event: E,
        /// The remedy that leads nowhere.
        remedy: E,
    },
    /// Under this observation, a state can be moved out of by nothing at all.
    ///
    /// The per-context statement of *"held for ever"*, which static liveness cannot see because
    /// every path out is guarded. Skipped for states that are at rest, and for those that
    /// declare [`State::awaits_input`] — a state waiting on a person has nothing the system can
    /// do by definition, and reporting that would drown the ones that are genuinely stuck.
    DeadEnd {
        /// The state with nothing available.
        state: S,
        /// Which of the audited contexts produced it.
        context: usize,
    },
    /// No audited context put the object in this state, so nothing here was checked. An audit
    /// that silently skipped a state reads exactly like one that cleared it.
    UncoveredState {
        /// The state no context covered.
        state: S,
    },
    /// An [`EventKind::Applied`] event leads to a state that does not accept it, so **running
    /// the verb a second time is a refusal rather than a no-op**.
    ///
    /// This is design S1.6 — *apply the plan last, so a failure leaves the object where it was
    /// and the verb is re-runnable* — stated as a property of the table. The second half of that
    /// sentence is what makes the first half worth anything: a verb you cannot safely re-run is
    /// one whose failure you have to diagnose before retrying, which is the thing the ordering
    /// rule exists to avoid.
    ///
    /// It has to be checked rather than remembered because [`Machine::accepts`] only absorbs an
    /// event implicitly when nothing would be lost by skipping it — a row carrying a guard or a
    /// plan is never absorbed, since the object may have arrived by some other route with that
    /// plan still owed. That rule is right, and it silently makes every plan-carrying
    /// destination refuse its own event. The domain has to say which it wants, one row at a
    /// time; this is what asks it.
    ///
    /// Restricted to [`EventKind::Applied`] on purpose. A reconciliation is an *observation*,
    /// not a verb somebody re-runs, and what happens when the same condition is seen twice is
    /// its guards' business — see [`Machine::reconcile`].
    ///
    /// **It asks for a decision, not for a particular one.** A domain that really does want the
    /// second run to fail says so with a self-loop whose guard always denies: the pair is then
    /// declared, this stops firing, and the refusal carries a sentence and a remedy instead of
    /// being the silent absence of a row. That is strictly better than what an undeclared pair
    /// gives you, which is [`Outcome::Undefined`] and a message assembled from two names.
    Unrepeatable {
        /// The destination that will not accept the event that leads to it.
        state: S,
        /// The event that gets there and then cannot be repeated.
        event: E,
    },
    /// Under this observation, two reconciliations fire at once, so
    /// [`Machine::reconcile`] can only refuse. Reported rather than tolerated: a domain either
    /// proves the pair is impossible, or narrows a guard.
    AmbiguousReconciliation {
        /// The state they fire in.
        state: S,
        /// Which of the audited contexts produced it.
        context: usize,
        /// The competing events.
        events: Vec<E>,
    },
}

impl<S: State, E: Event> fmt::Display for Defect<S, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nondeterministic { from, event } => write!(
                f,
                "two transitions declare ({}, {})",
                from.name(),
                event.name()
            ),
            Self::UnreachableState { state } => {
                write!(f, "state `{}` is reachable from nothing", state.name())
            }
            Self::Wedged { state } => write!(
                f,
                "state `{}` cannot reach a state where nothing is owed — whatever enters it \
                 never gets back to rest",
                state.name()
            ),
            Self::NoSettledState => write!(f, "no state is at rest"),
            Self::UnguardedReconciliation { from, event } => write!(
                f,
                "reconciliation `{}` in `{}` has no guard — it would assert a state change from \
                 nothing observed",
                event.name(),
                from.name()
            ),
            Self::UnusedEvent { event } => {
                write!(f, "event `{}` appears in no transition", event.name())
            }
            Self::Unrepeatable { state, event } => write!(
                f,
                "`{}` leads to `{}`, which does not accept it — so running it twice refuses \
                 instead of doing nothing. Declare the self-loop with whatever it owes there \
                 (often nothing).",
                event.name(),
                state.name()
            ),
            Self::UnreachableRemedy {
                state,
                event,
                remedy,
            } => write!(
                f,
                "refusing `{}` in `{}` offers remedy `{}`, which this machine does not accept \
                 there",
                event.name(),
                state.name(),
                remedy.name()
            ),
            Self::UncoveredState { state } => write!(
                f,
                "no audited context put the object in state `{}`",
                state.name()
            ),
            Self::DeadEnd { state, context } => write!(
                f,
                "in context #{context}, state `{}` accepts no event that moves it",
                state.name()
            ),
            Self::AmbiguousReconciliation {
                state,
                context,
                events,
            } => write!(
                f,
                "in context #{context}, state `{}` has {} reconciliations firing at once ({})",
                state.name(),
                events.len(),
                events
                    .iter()
                    .map(|e| e.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl<S: State, E: Event, C: Stateful<S> + 'static, X: 'static> Machine<S, E, C, X> {
    /// Walk the table and report every structural defect. Cheap; call it from a test.
    ///
    /// What it checks, and why each one exists, is in [`Defect`]. What it deliberately does
    /// **not** check is totality: [`Machine::apply`] answers for every pair by construction,
    /// because an undeclared pair is [`Outcome::Undefined`] rather than a fall-through.
    pub fn check(&self) -> Vec<Defect<S, E>> {
        let mut defects = Vec::new();

        let mut seen: HashSet<(S, E)> = HashSet::new();
        for t in self.transitions {
            if !seen.insert((t.from, t.event)) {
                defects.push(Defect::Nondeterministic {
                    from: t.from,
                    event: t.event,
                });
            }
            if t.event.kind() == EventKind::Reconciled && t.guard.is_none() {
                defects.push(Defect::UnguardedReconciliation {
                    from: t.from,
                    event: t.event,
                });
            }
        }

        for &event in E::ALL {
            if !self.transitions.iter().any(|t| t.event == event) {
                defects.push(Defect::UnusedEvent { event });
            }
        }

        // Re-runnability, one destination at a time. Deduplicated, because several rows
        // legitimately share a destination (three states `land` to `done`) and the defect is a
        // property of the *pair*, not of each row that produces it.
        let mut asked: HashSet<(S, E)> = HashSet::new();
        for t in self.transitions {
            let Dest::To(dest) = t.to else { continue };
            if t.event.kind() != EventKind::Applied || !asked.insert((dest, t.event)) {
                continue;
            }
            if matches!(self.accepts(dest, t.event), Acceptance::Undefined) {
                defects.push(Defect::Unrepeatable {
                    state: dest,
                    event: t.event,
                });
            }
        }

        // Reachability from the initial state, and the ability to reach a terminal from each
        // state. Both are plain graph walks over the table; guards are ignored, which is the
        // right reading — a guard makes a path conditional, while a missing edge makes it
        // impossible, and only the second is a defect of the machine itself.
        let forward = self.adjacency(Direction::Forward);
        let backward = self.adjacency(Direction::Backward);

        let reachable = reach(&forward, std::iter::once(self.initial));
        let by_override = self.stated_reaches_everything();
        for &state in S::ALL {
            if !by_override && !reachable.contains(&state.name()) {
                defects.push(Defect::UnreachableState { state });
            }
        }

        let settled: Vec<S> = S::ALL.iter().copied().filter(|s| s.is_settled()).collect();
        if settled.is_empty() {
            defects.push(Defect::NoSettledState);
        } else {
            let can_finish = reach(&backward, settled.iter().copied());
            for &state in S::ALL {
                if !state.is_settled() && !can_finish.contains(&state.name()) {
                    defects.push(Defect::Wedged { state });
                }
            }
        }

        defects
    }

    /// The table as a name-keyed adjacency map, in whichever direction `edge` picks out.
    /// A [`Dest::Stated`] row contributes no edge: it is an escape hatch, not a way the
    /// lifecycle gets anywhere, and counting it would make [`Defect::Wedged`] vacuous for every
    /// machine that has an operator override (see [`Dest`]).
    ///
    /// **Reachability is the exception, and asks the opposite question.** *Can the object be
    /// here* is answered yes by an override, whatever the lifecycle does; *can the lifecycle get
    /// it out of here* is not. Collapsing the two reported the investigation machine's
    /// `abandoned` — which only a person ever sets — as unreachable dead code, when it is a
    /// state units are in every day.
    fn adjacency(&self, direction: Direction) -> HashMap<&'static str, Vec<&'static str>> {
        let mut map: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
        for t in self.transitions {
            let Dest::To(to) = t.to else { continue };
            let (a, b) = match direction {
                Direction::Forward => (t.from, to),
                Direction::Backward => (to, t.from),
            };
            map.entry(a.name()).or_default().push(b.name());
        }
        map
    }

    /// Every state an operator could name outright — the destinations a [`Dest::Stated`] row can
    /// resolve to, which is *any* state, since the caller chooses.
    fn stated_reaches_everything(&self) -> bool {
        self.transitions
            .iter()
            .any(|t| matches!(t.to, Dest::Stated(_)))
    }

    /// Walk every `(state, event)` pair against each observed context, and report the defects
    /// only a guard's actual answer can reveal.
    ///
    /// The caller supplies the contexts, and is responsible for them being **observations that
    /// can really occur** — a cross-product of every field will contain combinations the world
    /// cannot produce, and a dead end reported for one of those is noise. The task machine
    /// generates its matrix from the facts its guards read, which is exhaustive over what the
    /// guards can distinguish while staying inside what can be observed.
    ///
    /// [`Defect::DeadEnd`] is the one worth stating plainly: for every non-terminal state and
    /// every context, **something must be able to move the object**. A lifecycle where some
    /// observation leaves a task refusing everything is the failure that appears in fourteen
    /// must-fix findings in this repository's history, spelled *permanently*, *forever* and
    /// *unrepairable*.
    pub fn audit(&self, contexts: &[C]) -> Vec<Defect<S, E>> {
        let mut defects = Vec::new();
        let mut covered: BTreeSet<&'static str> = BTreeSet::new();
        for (i, ctx) in contexts.iter().enumerate() {
            let state = ctx.state();
            covered.insert(state.name());
            let mut can_move = false;
            for &event in E::ALL {
                if matches!(self.accepts(state, event), Acceptance::Undefined) {
                    continue;
                }
                match self.apply(ctx, event) {
                    // Same exclusion as the static liveness walk: an operator naming a
                    // different state is not the lifecycle offering a way out.
                    Outcome::Moved { from, to, .. } => {
                        can_move |= from != to && !self.is_stated(state, event);
                    }
                    Outcome::Refused { denial, .. } => {
                        if let Some(remedy) = self.validate_remedy(state, &denial) {
                            defects.push(Defect::UnreachableRemedy {
                                state,
                                event,
                                remedy,
                            });
                        }
                    }
                    Outcome::Idempotent { .. } | Outcome::Undefined { .. } => {}
                }
            }
            if !state.is_settled() && !state.awaits_input() && !can_move {
                defects.push(Defect::DeadEnd { state, context: i });
            }
            if let Reconciliation::Ambiguous(events) = self.reconcile(ctx) {
                defects.push(Defect::AmbiguousReconciliation {
                    state,
                    context: i,
                    events,
                });
            }
        }
        // A state no context put the object in was audited by nothing, and an audit that
        // silently skipped a state reads exactly like one that cleared it.
        for &state in S::ALL {
            if !covered.contains(state.name()) {
                defects.push(Defect::UncoveredState { state });
            }
        }
        defects
    }
}

/// Every node reachable from `seeds` in `graph`, by name.
///
/// Names rather than `S` values because the adjacency map is keyed by [`State::name`] — the one
/// identity a state has that is `Ord` and `'static` without asking the domain for trait bounds
/// the table itself does not need.
fn reach<S: State>(
    graph: &HashMap<&'static str, Vec<&'static str>>,
    seeds: impl IntoIterator<Item = S>,
) -> BTreeSet<&'static str> {
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    let mut stack: Vec<&'static str> = Vec::new();
    for seed in seeds {
        if seen.insert(seed.name()) {
            stack.push(seed.name());
        }
    }
    while let Some(node) = stack.pop() {
        for next in graph.get(node).into_iter().flatten() {
            if seen.insert(next) {
                stack.push(next);
            }
        }
    }
    seen
}
