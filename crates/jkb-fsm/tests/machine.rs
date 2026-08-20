//! The library's own proof: one small lifecycle, plus a deliberately broken variant per
//! [`Defect`], so every check is shown to fire rather than merely to exist.
//!
//! A check nothing has ever seen fail is indistinguishable from a check that cannot fail — a
//! lesson this repository learned from seven consecutive guards that were satisfied by the very
//! recovery step they recommended.

use jkb_fsm::{
    all_of, require_no, require_yes, Acceptance, Defect, Denial, Dest, Event, EventKind, Fact,
    Machine, Outcome, Reconciliation, State, Stateful, Transition, Verdict,
};

// ---------------------------------------------------------------------------------------------
// A parcel: ordered -> shipped -> delivered, or lost. Small enough to read, rich enough to have
// a guard, a reconciliation, an effect-only self-loop and a remedy.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Parcel {
    Ordered,
    Shipped,
    Delivered,
    Lost,
}

impl State for Parcel {
    const ALL: &'static [Self] = &[Self::Ordered, Self::Shipped, Self::Delivered, Self::Lost];

    fn name(self) -> &'static str {
        match self {
            Self::Ordered => "ordered",
            Self::Shipped => "shipped",
            Self::Delivered => "delivered",
            Self::Lost => "lost",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Delivered | Self::Lost)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Move {
    Ship,
    Deliver,
    WriteOff,
    /// Reconciled: the carrier says it arrived.
    ObservedDelivered,
    /// Reconciled, effect-only: the courier holding it quit.
    ObservedCourierGone,
}

impl Event for Move {
    const ALL: &'static [Self] = &[
        Self::Ship,
        Self::Deliver,
        Self::WriteOff,
        Self::ObservedDelivered,
        Self::ObservedCourierGone,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Ship => "ship",
            Self::Deliver => "deliver",
            Self::WriteOff => "write_off",
            Self::ObservedDelivered => "observed_delivered",
            Self::ObservedCourierGone => "observed_courier_gone",
        }
    }

    fn kind(self) -> EventKind {
        match self {
            Self::ObservedDelivered | Self::ObservedCourierGone => EventKind::Reconciled,
            _ => EventKind::Applied,
        }
    }
}

// Not `Copy`: a guard takes `&C`, and a `Copy` context small enough to pass by value trips
// `trivially_copy_pass_by_ref` on every guard. Real contexts (the task machine's) are not
// trivially copyable, so this keeps the toy honest about the shape callers will write.
#[derive(Debug, Clone)]
struct Facts {
    at: Parcel,
    paid: Fact,
    signed_for: Fact,
    courier_alive: Fact,
}

impl Default for Facts {
    fn default() -> Self {
        Self {
            at: Parcel::Ordered,
            paid: Fact::Unknown,
            signed_for: Fact::Unknown,
            courier_alive: Fact::Unknown,
        }
    }
}

impl Stateful<Parcel> for Facts {
    fn state(&self) -> Parcel {
        self.at
    }
}

/// The same observation, read at another point in the lifecycle.
fn at(state: Parcel, facts: &Facts) -> Facts {
    Facts {
        at: state,
        ..facts.clone()
    }
}

/// One observation, at every state — so an audit covers them all and reports no
/// [`Defect::UncoveredState`].
fn every_state_of(facts: &Facts) -> Vec<Facts> {
    <Parcel as State>::ALL
        .iter()
        .map(|s| at(*s, facts))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fx {
    DropCourier,
}

fn ship_guard(f: &Facts) -> Verdict<Move> {
    require_yes(f.paid, || {
        Denial::with_remedy(
            "it is not paid for.",
            Move::WriteOff,
            "Write it off with `parcel write-off`.",
        )
    })
}

/// Delivery needs a signature that is **proven** present, so an unreadable manifest refuses.
fn deliver_guard(f: &Facts) -> Verdict<Move> {
    all_of([
        require_yes(f.signed_for, || {
            Denial::with_remedy(
                "nobody signed for it.",
                Move::WriteOff,
                "Write it off with `parcel write-off`.",
            )
        }),
        require_no(f.courier_alive, || {
            Denial::new("the courier is still holding it.")
        }),
    ])
}

const ROWS: &[Transition<Parcel, Move, Facts, Fx>] = &[
    Transition {
        from: Parcel::Ordered,
        event: Move::Ship,
        to: Dest::To(Parcel::Shipped),
        guard: Some(ship_guard),
        plan: None,
    },
    Transition {
        from: Parcel::Ordered,
        event: Move::WriteOff,
        to: Dest::To(Parcel::Lost),
        guard: None,
        plan: None,
    },
    Transition {
        from: Parcel::Shipped,
        event: Move::WriteOff,
        to: Dest::To(Parcel::Lost),
        guard: None,
        plan: None,
    },
    Transition {
        from: Parcel::Shipped,
        event: Move::Deliver,
        to: Dest::To(Parcel::Delivered),
        guard: Some(deliver_guard),
        plan: None,
    },
    Transition {
        from: Parcel::Shipped,
        event: Move::ObservedDelivered,
        to: Dest::To(Parcel::Delivered),
        guard: Some(|f: &Facts| require_yes(f.signed_for, || Denial::new("no signature on file."))),
        plan: None,
    },
    Transition {
        from: Parcel::Shipped,
        event: Move::ObservedCourierGone,
        to: Dest::To(Parcel::Shipped),
        guard: Some(|f: &Facts| {
            require_no(f.courier_alive, || {
                Denial::new("the courier is still there.")
            })
        }),
        plan: Some(|_| vec![Fx::DropCourier]),
    },
];

fn parcel() -> Machine<Parcel, Move, Facts, Fx> {
    Machine {
        transitions: ROWS,
        initial: Parcel::Ordered,
    }
}

/// Every combination of the three facts. Small enough to be exhaustive, which is the point:
/// a hand-picked set of contexts audits the cases the author already thought of.
fn every_context() -> Vec<Facts> {
    let mut out = Vec::new();
    for &at in <Parcel as State>::ALL {
        for &paid in Fact::ALL {
            for &signed_for in Fact::ALL {
                for &courier_alive in Fact::ALL {
                    out.push(Facts {
                        at,
                        paid,
                        signed_for,
                        courier_alive,
                    });
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The good machine
// ---------------------------------------------------------------------------------------------

#[test]
fn a_well_formed_machine_has_no_defects() {
    let m = parcel();
    let statics = m.check();
    assert!(statics.is_empty(), "{:?}", render(&statics));
    let dynamics = m.audit(&every_context());
    assert!(dynamics.is_empty(), "{:?}", render(&dynamics));
}

/// Asking for what already happened is a no-op, and no verb had to remember that (design S1.6).
#[test]
fn the_destination_of_a_transition_absorbs_its_own_event() {
    let m = parcel();
    let facts = Facts::default();
    assert!(matches!(
        m.apply(&at(Parcel::Shipped, &facts), Move::Ship),
        Outcome::Idempotent { .. }
    ));
    assert_eq!(
        m.accepts(Parcel::Shipped, Move::Ship),
        Acceptance::Idempotent
    );
    // ...and it does not invent one for an event that never lands here.
    assert_eq!(
        m.accepts(Parcel::Ordered, Move::Deliver),
        Acceptance::Undefined
    );
    assert!(matches!(
        m.apply(&at(Parcel::Ordered, &facts), Move::Deliver),
        Outcome::Undefined { .. }
    ));
}

/// An undeclared pair is a refusal a person can read, not a silent nothing.
#[test]
fn an_undefined_pair_still_explains_itself() {
    let m = parcel();
    let out = m.apply(&at(Parcel::Delivered, &Facts::default()), Move::Ship);
    assert_eq!(
        out.refusal().as_deref(),
        Some("`ship` is not something that can happen to a task that is `delivered`.")
    );
    assert!(!out.moved());
    assert_eq!(out.state(), Parcel::Delivered);
}

/// The whole reason `Fact` exists: an unobservable fact refuses in **both** polarities.
#[test]
fn an_unknown_fact_licenses_nothing_in_either_direction() {
    let m = parcel();
    // `paid` is required proven-true: unknown refuses.
    let unpaid = Facts {
        paid: Fact::Unknown,
        ..Facts::default()
    };
    assert!(m
        .apply(&at(Parcel::Ordered, &unpaid), Move::Ship)
        .refusal()
        .is_some());
    // `courier_alive` is required proven-false: unknown refuses that too, rather than reading
    // as "not alive" and letting the delivery through on an unread manifest.
    let hazy = Facts {
        signed_for: Fact::Yes,
        courier_alive: Fact::Unknown,
        ..Facts::default()
    };
    assert!(m
        .apply(&at(Parcel::Shipped, &hazy), Move::Deliver)
        .refusal()
        .is_some());
}

/// A move carries its effects as one value, so a caller cannot perform half of it.
#[test]
fn a_move_carries_its_whole_plan() {
    let m = parcel();
    let gone = Facts {
        courier_alive: Fact::No,
        ..Facts::default()
    };
    let out = m.apply(&at(Parcel::Shipped, &gone), Move::ObservedCourierGone);
    assert_eq!(out.effects(), [Fx::DropCourier]);
    assert_eq!(out.state(), Parcel::Shipped, "an effect-only self-loop");
}

#[test]
fn reconcile_fires_only_what_the_evidence_supports() {
    let m = parcel();
    // Nothing observed: nothing to do.
    assert!(matches!(
        m.reconcile(&at(Parcel::Shipped, &Facts::default())),
        Reconciliation::Settled
    ));
    // A signature: the delivery is detected.
    let signed = Facts {
        signed_for: Fact::Yes,
        courier_alive: Fact::Yes,
        ..Facts::default()
    };
    let Reconciliation::Fired(out) = m.reconcile(&at(Parcel::Shipped, &signed)) else {
        panic!("expected a move");
    };
    assert_eq!(out.state(), Parcel::Delivered);
    // A state-changing reconciliation outranks an effect-only self-loop, which is not lost —
    // the caller re-observes and it fires on the next step.
    let both = Facts {
        signed_for: Fact::Yes,
        courier_alive: Fact::No,
        ..Facts::default()
    };
    let Reconciliation::Fired(out) = m.reconcile(&at(Parcel::Shipped, &both)) else {
        panic!("expected the move to win");
    };
    assert_eq!(out.state(), Parcel::Delivered);
}

#[test]
fn the_table_renders_as_a_diagram() {
    let dot = parcel().dot("parcel");
    assert!(dot.contains("\"ordered\" -> \"shipped\" [label=\"ship\", style=solid];"));
    // Reconciliation edges are visually distinct, because the two kinds mean different things.
    assert!(dot.contains("style=dashed"));
    assert!(dot.contains("\"delivered\" [shape=doublecircle"));
}

// ---------------------------------------------------------------------------------------------
// One broken machine per defect. Each shares the parcel vocabulary and swaps the table.
// ---------------------------------------------------------------------------------------------

fn render<S: State, E: Event>(defects: &[Defect<S, E>]) -> Vec<String> {
    defects.iter().map(ToString::to_string).collect()
}

fn with(rows: &'static [Transition<Parcel, Move, Facts, Fx>]) -> Machine<Parcel, Move, Facts, Fx> {
    Machine {
        transitions: rows,
        initial: Parcel::Ordered,
    }
}

const NONDETERMINISTIC: &[Transition<Parcel, Move, Facts, Fx>] = &[
    Transition {
        from: Parcel::Ordered,
        event: Move::Ship,
        to: Dest::To(Parcel::Shipped),
        guard: None,
        plan: None,
    },
    Transition {
        from: Parcel::Ordered,
        event: Move::Ship,
        to: Dest::To(Parcel::Lost),
        guard: None,
        plan: None,
    },
];

#[test]
fn two_rows_for_one_pair_is_a_defect() {
    let defects = with(NONDETERMINISTIC).check();
    assert!(
        defects.contains(&Defect::Nondeterministic {
            from: Parcel::Ordered,
            event: Move::Ship,
        }),
        "{:?}",
        render(&defects)
    );
}

const WEDGED: &[Transition<Parcel, Move, Facts, Fx>] = &[
    Transition {
        from: Parcel::Ordered,
        event: Move::Ship,
        to: Dest::To(Parcel::Shipped),
        guard: None,
        plan: None,
    },
    // `shipped` goes nowhere: whatever reaches it never finishes. `delivered` and `lost` are
    // then unreachable, which is the same table read from the other end.
];

#[test]
fn a_state_that_cannot_finish_is_a_defect() {
    let defects = with(WEDGED).check();
    assert!(
        defects.contains(&Defect::Wedged {
            state: Parcel::Shipped
        }),
        "{:?}",
        render(&defects)
    );
    assert!(defects.contains(&Defect::UnreachableState {
        state: Parcel::Delivered
    }));
    assert!(defects.contains(&Defect::UnusedEvent {
        event: Move::WriteOff
    }));
}

const UNGUARDED_RECONCILIATION: &[Transition<Parcel, Move, Facts, Fx>] = &[
    Transition {
        from: Parcel::Ordered,
        event: Move::WriteOff,
        to: Dest::To(Parcel::Lost),
        guard: None,
        plan: None,
    },
    Transition {
        from: Parcel::Ordered,
        event: Move::ObservedDelivered,
        to: Dest::To(Parcel::Delivered),
        guard: None,
        plan: None,
    },
];

#[test]
fn a_reconciliation_with_no_evidence_is_a_defect() {
    let defects = with(UNGUARDED_RECONCILIATION).check();
    assert!(
        defects.contains(&Defect::UnguardedReconciliation {
            from: Parcel::Ordered,
            event: Move::ObservedDelivered,
        }),
        "{:?}",
        render(&defects)
    );
}

/// The `write_off` remedy is offered from a state this table has no `write_off` transition for,
/// so following the machine's own advice could not work. Three must-fix findings in this
/// repository's history are exactly this, and none was catchable because the advice was prose.
const BAD_REMEDY: &[Transition<Parcel, Move, Facts, Fx>] = &[
    Transition {
        from: Parcel::Ordered,
        event: Move::Ship,
        to: Dest::To(Parcel::Shipped),
        guard: Some(ship_guard),
        plan: None,
    },
    Transition {
        from: Parcel::Shipped,
        event: Move::WriteOff,
        to: Dest::To(Parcel::Lost),
        guard: None,
        plan: None,
    },
];

#[test]
fn a_remedy_that_leads_nowhere_is_a_defect() {
    let m = with(BAD_REMEDY);
    let unpaid = Facts {
        paid: Fact::No,
        ..Facts::default()
    };
    let defects = m.audit(&every_state_of(&unpaid));
    assert!(
        defects.contains(&Defect::UnreachableRemedy {
            state: Parcel::Ordered,
            event: Move::Ship,
            remedy: Move::WriteOff,
        }),
        "{:?}",
        render(&defects)
    );
}

/// `shipped` can be left only by delivering, and delivering needs a signature — so an
/// unsigned parcel is held for ever. Static liveness cannot see this: the edge exists.
const DEAD_END: &[Transition<Parcel, Move, Facts, Fx>] = &[
    Transition {
        from: Parcel::Ordered,
        event: Move::Ship,
        to: Dest::To(Parcel::Shipped),
        guard: None,
        plan: None,
    },
    Transition {
        from: Parcel::Ordered,
        event: Move::WriteOff,
        to: Dest::To(Parcel::Lost),
        guard: None,
        plan: None,
    },
    Transition {
        from: Parcel::Shipped,
        event: Move::Deliver,
        to: Dest::To(Parcel::Delivered),
        guard: Some(|f: &Facts| require_yes(f.signed_for, || Denial::new("nobody signed for it."))),
        plan: None,
    },
];

#[test]
fn a_state_that_some_observation_can_never_leave_is_a_defect() {
    let m = with(DEAD_END);
    // The static checks see nothing wrong with the *shape*: every state is reachable and can
    // reach a terminal on paper. (This cut-down table wires no reconciliations, so it reports
    // those as unused; what matters is that neither liveness defect is among them.)
    let statics = m.check();
    assert!(
        !statics
            .iter()
            .any(|d| matches!(d, Defect::Wedged { .. } | Defect::UnreachableState { .. })),
        "{:?}",
        render(&statics)
    );
    let unsigned = Facts {
        signed_for: Fact::No,
        ..Facts::default()
    };
    let defects = m.audit(&every_state_of(&unsigned));
    assert!(
        defects.iter().any(|d| matches!(
            d,
            Defect::DeadEnd {
                state: Parcel::Shipped,
                ..
            }
        )),
        "{:?}",
        render(&defects)
    );
}

const AMBIGUOUS: &[Transition<Parcel, Move, Facts, Fx>] = &[
    Transition {
        from: Parcel::Ordered,
        event: Move::WriteOff,
        to: Dest::To(Parcel::Lost),
        guard: None,
        plan: None,
    },
    Transition {
        from: Parcel::Ordered,
        event: Move::ObservedDelivered,
        to: Dest::To(Parcel::Delivered),
        guard: Some(|_: &Facts| Verdict::Allow),
        plan: None,
    },
    // A second reconciliation out of `ordered`, allowed under the same observation.
    Transition {
        from: Parcel::Ordered,
        event: Move::ObservedCourierGone,
        to: Dest::To(Parcel::Lost),
        guard: Some(|_: &Facts| Verdict::Allow),
        plan: None,
    },
];

#[test]
fn two_reconciliations_firing_at_once_are_refused_and_reported() {
    let m = with(AMBIGUOUS);
    let Reconciliation::Ambiguous(events) = m.reconcile(&at(Parcel::Ordered, &Facts::default()))
    else {
        panic!("expected ambiguity, not a guess");
    };
    assert_eq!(events.len(), 2);
    let defects = m.audit(&every_state_of(&Facts::default()));
    assert!(
        defects
            .iter()
            .any(|d| matches!(d, Defect::AmbiguousReconciliation { .. })),
        "{:?}",
        render(&defects)
    );
}

#[test]
fn a_machine_with_no_terminal_state_says_so() {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    enum Spin {
        A,
        B,
    }
    impl State for Spin {
        const ALL: &'static [Self] = &[Self::A, Self::B];
        fn name(self) -> &'static str {
            match self {
                Self::A => "a",
                Self::B => "b",
            }
        }
        fn is_terminal(self) -> bool {
            false
        }
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    enum Tick {
        Go,
    }
    impl Event for Tick {
        const ALL: &'static [Self] = &[Self::Go];
        fn name(self) -> &'static str {
            "go"
        }
        fn kind(self) -> EventKind {
            EventKind::Applied
        }
    }
    struct Where(Spin);
    impl Stateful<Spin> for Where {
        fn state(&self) -> Spin {
            self.0
        }
    }
    static ROWS: &[Transition<Spin, Tick, Where, ()>] = &[
        Transition {
            from: Spin::A,
            event: Tick::Go,
            to: Dest::To(Spin::B),
            guard: None,
            plan: None,
        },
        Transition {
            from: Spin::B,
            event: Tick::Go,
            to: Dest::To(Spin::A),
            guard: None,
            plan: None,
        },
    ];
    let m = Machine {
        transitions: ROWS,
        initial: Spin::A,
    };
    assert!(m.check().contains(&Defect::NoTerminalState));
}
