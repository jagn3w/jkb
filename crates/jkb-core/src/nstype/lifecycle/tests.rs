//! Both tables, checked and audited — and the differences between them asserted as differences,
//! since that is the thing this machine exists to make visible.

use jkb_fsm::{Acceptance, Fact, Outcome, Reconciliation, State};
use jkb_types::Resolution;

use super::{deriving, UnitEffect, UnitEvent, UnitFacts, UnitMachine, BASE, DEBUGGING};

/// Every observation over one unit. Four independent three-valued facts and five states, taken
/// whole — small enough that there is no reason to model or sample it.
fn every_observation() -> Vec<UnitFacts> {
    let mut out = Vec::new();
    for &resolution in <Resolution as State>::ALL {
        for &refuted in Fact::ALL {
            for &superseded in Fact::ALL {
                for &confirmed in Fact::ALL {
                    for &stale in Fact::ALL {
                        out.push(UnitFacts {
                            resolution,
                            refuted,
                            superseded,
                            confirmed,
                            stale,
                            stated: Some(Resolution::Abandoned),
                        });
                    }
                }
            }
        }
    }
    out
}

/// The family, for the one check that is about the family rather than about a machine.
const FAMILY: &[(&UnitMachine, &str)] = &[(&BASE, "base"), (&DEBUGGING, "debugging")];

/// Whether some *other* machine in the family declares `event`.
fn used_elsewhere(event: UnitEvent, mine: &str) -> bool {
    FAMILY
        .iter()
        .filter(|(_, name)| *name != mine)
        .any(|(m, _)| m.transitions.iter().any(|t| t.event == event))
}

fn well_formed(m: &UnitMachine, which: &str) {
    // `UnusedEvent` is a **per-machine** statement, and this is the first place two machines
    // share one event enum: `went_stale` belongs to `debugging` alone, so the base table
    // legitimately never declares it. The filter is narrow on purpose — an event no machine in
    // the family uses is still a defect, and still reported.
    let defects: Vec<_> = m
        .check()
        .into_iter()
        .filter(|d| !matches!(d, jkb_fsm::Defect::UnusedEvent { event } if used_elsewhere(*event, which)))
        .collect();
    let rendered: Vec<String> = defects.iter().map(ToString::to_string).collect();
    assert!(defects.is_empty(), "{which}: {rendered:#?}");
    let dynamic = m.audit(&every_observation());
    let rendered: Vec<String> = dynamic.iter().map(ToString::to_string).collect();
    assert!(dynamic.is_empty(), "{which}: {rendered:#?}");
}

#[test]
fn both_tables_are_well_formed() {
    for (m, which) in FAMILY {
        well_formed(m, which);
    }
}

/// ...and between them the family uses every event, which is what the per-machine
/// `UnusedEvent` filter above gives up and has to be reasserted here.
#[test]
fn the_family_between_them_uses_every_event() {
    for &event in <UnitEvent as jkb_fsm::Event>::ALL {
        assert!(
            FAMILY
                .iter()
                .any(|(m, _)| m.transitions.iter().any(|t| t.event == event)),
            "no table declares `{event:?}`"
        );
    }
}

/// The priority is a **guard clause**, not arm order — so contradictory evidence resolves to one
/// event, and the audit above proves the four conditions never overlap.
///
/// `default_rollup` gets the same answer by asking *refuted?* first and returning, which is
/// correct and invisible: nothing in it says a refutation outranks a confirmation, and nothing
/// would notice if the questions were reordered.
#[test]
fn a_refutation_outranks_a_confirmation_and_says_so() {
    let contradictory = UnitFacts {
        resolution: Resolution::Unresolved,
        refuted: Fact::Yes,
        superseded: Fact::No,
        confirmed: Fact::Yes,
        stale: Fact::No,
        stated: None,
    };
    let Reconciliation::Fired(out) = BASE.reconcile(&contradictory) else {
        panic!("contradictory evidence must still resolve, not stall");
    };
    assert_eq!(out.state(), Resolution::DeadEnd);

    // ...and the refusal says *why* the confirmation lost — in a sentence, and deliberately with
    // **no remedy event**. The obvious candidate is `refuted`, which is what is blocking it: a
    // remedy naming the event that makes things worse is exactly the class the remedy check
    // exists for, and `validate_remedy` would have certified this one, because `refuted` really
    // is accepted where the unit is unresolved.
    let Outcome::Refused { denial, .. } = BASE.apply(&contradictory, UnitEvent::Confirmed) else {
        panic!("a confirmation against a refutation must be refused");
    };
    assert_eq!(denial.remedy, None, "the remedy points at the blocker");
    assert!(denial.reason.contains("unlink the refuting edge"));
}

/// An observation nobody could establish moves nothing. For a rollup that is the whole safety
/// property: the alternative is writing a resolution over evidence that was not read.
#[test]
fn an_unread_unit_is_left_alone() {
    let unread = UnitFacts {
        resolution: Resolution::Success,
        ..UnitFacts::default()
    };
    assert!(matches!(BASE.reconcile(&unread), Reconciliation::Settled));
    assert!(matches!(
        DEBUGGING.reconcile(&unread),
        Reconciliation::Settled
    ));
}

// ---------------------------------------------------------------------------------------------
// The two differences — asserted as differences, in both directions
// ---------------------------------------------------------------------------------------------

/// `debugging` alone lets a settled unit go back to `unresolved`: an observation about a mutable
/// system goes stale, and a `success` resting on stale evidence is not a result any more.
#[test]
fn only_debugging_lets_a_result_go_stale() {
    let stale = UnitFacts {
        resolution: Resolution::Success,
        refuted: Fact::No,
        superseded: Fact::No,
        confirmed: Fact::No,
        stale: Fact::Yes,
        stated: None,
    };
    let Reconciliation::Fired(out) = DEBUGGING.reconcile(&stale) else {
        panic!("a stale success must return to the frontier");
    };
    assert_eq!(out.state(), Resolution::Unresolved);
    assert_eq!(
        out.effects(),
        [UnitEffect::SetResolution(Resolution::Unresolved)]
    );

    // The base table has no such transition at all — a proved lemma does not unprove itself.
    assert!(matches!(
        BASE.accepts(Resolution::Success, UnitEvent::WentStale),
        Acceptance::Undefined
    ));
    assert!(matches!(BASE.reconcile(&stale), Reconciliation::Settled));
}

/// The base table lets a tombstone be revived; `debugging`'s does not.
///
/// Both were already true and neither was written down: the base behaviour is `default_rollup`
/// having no early return for a settled unit, and `debugging`'s is one line of its rollup —
/// *"deaths and supersessions stand as-is"*. Asserting them against each other is what makes the
/// asymmetry a decision rather than an accident of two functions' shapes.
#[test]
fn a_tombstone_revives_under_the_base_table_and_not_under_debugging() {
    let revived = UnitFacts {
        resolution: Resolution::DeadEnd,
        refuted: Fact::No, // the refuting edge was withdrawn — a deliberate act
        superseded: Fact::No,
        confirmed: Fact::Yes,
        stale: Fact::No,
        stated: None,
    };
    let Reconciliation::Fired(out) = BASE.reconcile(&revived) else {
        panic!("the base table revives a dead end whose refutation was withdrawn");
    };
    assert_eq!(out.state(), Resolution::Success);

    assert!(matches!(
        DEBUGGING.accepts(Resolution::DeadEnd, UnitEvent::Confirmed),
        Acceptance::Undefined
    ));
    assert!(matches!(
        DEBUGGING.reconcile(&revived),
        Reconciliation::Settled
    ));
}

/// `abandoned` is *dropped, not disproved*, so it is fair game to pick back up — under both
/// tables, and by evidence rather than only by a person.
///
/// This is `Resolution::is_tombstone`'s doc comment, which excludes `abandoned` from the
/// anti-retread set, stated as transitions instead.
#[test]
fn an_abandoned_unit_can_be_picked_back_up_by_evidence() {
    for (m, which) in FAMILY {
        let facts = UnitFacts {
            resolution: Resolution::Abandoned,
            refuted: Fact::No,
            superseded: Fact::No,
            confirmed: Fact::Yes,
            stale: Fact::No,
            stated: None,
        };
        let Reconciliation::Fired(out) = m.reconcile(&facts) else {
            panic!("{which}: an abandoned unit is not a tombstone");
        };
        assert_eq!(out.state(), Resolution::Success, "{which}");
    }
}

/// An operator may name any resolution, from any state, under either table — and it is the only
/// way `abandoned` is ever reached.
///
/// That last part is why the reachability walk had to start counting a stated destination:
/// measured over declared edges alone, `abandoned` is unreachable, and the check reported a
/// state units are in every day as dead code.
#[test]
fn only_a_person_ever_abandons_a_unit() {
    for (m, which) in FAMILY {
        for &from in <Resolution as State>::ALL {
            let out = m.apply(
                &UnitFacts {
                    resolution: from,
                    stated: Some(Resolution::Abandoned),
                    ..UnitFacts::default()
                },
                UnitEvent::Stated,
            );
            assert_eq!(out.state(), Resolution::Abandoned, "{which} from {from:?}");
        }
        // No observation produces it.
        assert!(
            m.transitions
                .iter()
                .all(|t| !matches!(t.to, jkb_fsm::Dest::To(Resolution::Abandoned))),
            "{which}: something derives `abandoned` from evidence"
        );
    }
}

/// [`deriving`] is total, and every resolution it names is a transition the base table has from
/// somewhere — so a caller that computed a resolution another way can be checked against it.
#[test]
fn every_derived_resolution_maps_back_to_an_event() {
    for &to in <Resolution as State>::ALL {
        let event = deriving(to);
        // Across the family: `went_stale` leads to `unresolved` under `debugging` only, which is
        // exactly the per-strategy difference this machine exists to make visible.
        let declared = FAMILY.iter().any(|(m, _)| {
            m.transitions
                .iter()
                .any(|t| t.event == event && matches!(t.to, jkb_fsm::Dest::To(d) if d == to))
        });
        let stated = event == UnitEvent::Stated;
        assert!(
            declared || stated,
            "`{to:?}` maps to `{event:?}`, which never leads there"
        );
    }
}
