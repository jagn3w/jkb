use jkb_fsm::{Event as _, Fact, Outcome, Reconciliation, State as _};
use jkb_rbac::{Decision, Grants as _};
use jkb_types::TaskStatus;

use super::strategy::{preset, StrategySpec, Toggles, BASE, PRESETS};
use super::{GraphId, Phase, WorkflowEvent, WorkflowFacts};
use crate::roles::Role;

/// Every combination of the facts a guard reads, at every phase: what `audit` walks.
fn contexts() -> Vec<WorkflowFacts> {
    let mut out = Vec::new();
    let statuses = [
        TaskStatus::Open,
        TaskStatus::InProgress,
        TaskStatus::NeedsReview,
        TaskStatus::Done,
        TaskStatus::Cancelled,
    ];
    for &phase in Phase::ALL {
        for status in statuses {
            for &landed in Fact::ALL {
                for &new_round in Fact::ALL {
                    for &must_fix in Fact::ALL {
                        for &repeated in Fact::ALL {
                            out.push(WorkflowFacts {
                                phase,
                                stated: Some(Phase::Implement),
                                task_status: status,
                                landed,
                                new_round,
                                last_round_must_fix: must_fix,
                                repeated_areas: repeated,
                            });
                        }
                    }
                }
            }
        }
    }
    out
}

#[test]
fn every_graph_passes_every_check_and_audit() {
    let ctx = contexts();
    for &g in GraphId::ALL {
        let m = g.machine();
        assert_eq!(m.check(), vec![], "{} static checks", g.as_str());
        assert_eq!(m.audit(&ctx), vec![], "{} audit", g.as_str());
    }
}

#[test]
fn the_two_graphs_differ_in_exactly_where_a_design_goes() {
    let f = WorkflowFacts::at(Phase::Design);
    let to = |g: GraphId| match g.machine().apply(&f, WorkflowEvent::SubmitDesign) {
        Outcome::Moved { to, .. } => to,
        other => panic!("{other:?}"),
    };
    assert_eq!(to(GraphId::ReviewedDesign), Phase::DesignReview);
    assert_eq!(to(GraphId::Direct), Phase::Implement);
    assert!(GraphId::ReviewedDesign.reaches_unforced(Phase::DesignReview));
    assert!(
        !GraphId::Direct.reaches_unforced(Phase::DesignReview),
        "only an override reaches it"
    );
    assert!(GraphId::Direct.reaches_unforced(Phase::Landable));
}

fn review(new_round: Fact, must_fix: Fact, repeated: Fact) -> WorkflowFacts {
    WorkflowFacts {
        task_status: TaskStatus::NeedsReview,
        new_round,
        last_round_must_fix: must_fix,
        repeated_areas: repeated,
        ..WorkflowFacts::at(Phase::Review)
    }
}

fn reconciled(f: &WorkflowFacts) -> Option<(WorkflowEvent, Phase)> {
    match GraphId::ReviewedDesign.machine().reconcile(f) {
        Reconciliation::Fired(Outcome::Moved { event, to, .. }) => Some((event, to)),
        Reconciliation::Settled => None,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_review_round_decides_the_next_phase_and_only_a_clean_one_reaches_landable() {
    assert_eq!(
        reconciled(&review(Fact::Yes, Fact::No, Fact::No)),
        Some((WorkflowEvent::ReviewPassed, Phase::Landable))
    );
    assert_eq!(
        reconciled(&review(Fact::Yes, Fact::Yes, Fact::No)),
        Some((WorkflowEvent::ReviewFailed, Phase::Implement)),
        "must-fixes go back to an implementer"
    );
    assert_eq!(
        reconciled(&review(Fact::Yes, Fact::Yes, Fact::Yes)),
        Some((WorkflowEvent::ReviewRepeated, Phase::SystemicReview)),
        "repeating ones go to a systemic reviewer first"
    );
    assert_eq!(
        reconciled(&review(Fact::No, Fact::No, Fact::No)),
        None,
        "no new round, no move: an old clean round must not pass new work"
    );
    assert_eq!(
        reconciled(&review(Fact::Yes, Fact::Unknown, Fact::No)),
        None,
        "an unestablished must-fix count moves nothing"
    );
}

#[test]
fn a_done_checkbox_is_not_a_landing() {
    let f = WorkflowFacts {
        task_status: TaskStatus::Done,
        landed: Fact::No,
        ..WorkflowFacts::at(Phase::Implement)
    };
    assert_eq!(reconciled(&f), None);
    let landed = WorkflowFacts {
        landed: Fact::Yes,
        ..f
    };
    assert_eq!(
        reconciled(&landed),
        Some((WorkflowEvent::ObservedLanded, Phase::Landed))
    );
}

#[test]
fn systemic_review_goes_back_to_design_or_to_implementation_by_what_it_found() {
    let f = WorkflowFacts::at(Phase::SystemicReview);
    let m = GraphId::ReviewedDesign.machine();
    assert!(matches!(
        m.apply(&f, WorkflowEvent::SubmitSystemic),
        Outcome::Moved {
            to: Phase::Implement,
            ..
        }
    ));
    assert!(matches!(
        m.apply(&f, WorkflowEvent::SystemicRedesign),
        Outcome::Moved {
            to: Phase::Design,
            ..
        }
    ));
    assert!(WorkflowEvent::SubmitSystemic.needs_reason());
    assert!(WorkflowEvent::SystemicRedesign.needs_reason());
    assert!(!WorkflowEvent::SubmitWork.needs_reason());
}

#[test]
fn every_preset_validates_and_design_reviewed_is_the_default() {
    for p in PRESETS {
        (p.spec)()
            .validate()
            .unwrap_or_else(|e| panic!("{}: {e}", p.name));
    }
    assert_eq!(super::strategy::DEFAULT_PRESET, "design-reviewed");
    let d = preset("design-reviewed").unwrap();
    assert_eq!(d.graph, GraphId::ReviewedDesign);
    assert_eq!(d.toggles, Toggles::default());
    assert_eq!(BASE.check(), vec![], "the shared base table is sound");
}

fn allowed(spec: &StrategySpec, role: Role, event: WorkflowEvent) -> bool {
    spec.authorize(&[role], event).is_allowed()
}

#[test]
fn the_approval_toggle_decides_who_approves_a_design() {
    let reviewed = preset("design-reviewed").unwrap();
    let coordinated = preset("coordinated").unwrap();
    assert!(!allowed(
        &reviewed,
        Role::Coordinator,
        WorkflowEvent::ApproveDesign
    ));
    assert!(allowed(
        &reviewed,
        Role::Operator,
        WorkflowEvent::ApproveDesign
    ));
    assert!(allowed(
        &coordinated,
        Role::Coordinator,
        WorkflowEvent::ApproveDesign
    ));
    assert!(
        !allowed(&coordinated, Role::Designer, WorkflowEvent::ApproveDesign),
        "a designer never approves its own design"
    );
    let Decision::Deny(no) = reviewed.authorize(&[Role::Coordinator], WorkflowEvent::ApproveDesign)
    else {
        panic!()
    };
    assert_eq!(
        no.allowed_roles,
        vec!["operator"],
        "the refusal names who may"
    );
}

#[test]
fn the_land_toggle_decides_who_lands() {
    let reviewed = preset("design-reviewed").unwrap();
    let autonomous = preset("autonomous").unwrap();
    assert!(reviewed.may_land(&[Role::Operator]).is_allowed());
    assert!(!reviewed.may_land(&[Role::Coordinator]).is_allowed());
    assert!(autonomous.may_land(&[Role::Coordinator]).is_allowed());
    assert!(
        !autonomous.may_land(&[Role::Implementer]).is_allowed(),
        "no toggle can name a worker"
    );
}

#[test]
fn workers_fire_only_their_own_step_and_observations_are_anyones() {
    let spec = preset("design-reviewed").unwrap();
    assert!(allowed(&spec, Role::Designer, WorkflowEvent::SubmitDesign));
    assert!(!allowed(&spec, Role::Designer, WorkflowEvent::SubmitWork));
    assert!(allowed(&spec, Role::Implementer, WorkflowEvent::SubmitWork));
    assert!(!allowed(&spec, Role::Implementer, WorkflowEvent::Rework));
    assert!(!allowed(&spec, Role::Reviewer, WorkflowEvent::SubmitWork));
    assert!(allowed(
        &spec,
        Role::SystemicReviewer,
        WorkflowEvent::SystemicRedesign
    ));
    assert!(!allowed(&spec, Role::Coordinator, WorkflowEvent::Override));
    for &e in WorkflowEvent::ALL {
        if e.kind() == jkb_fsm::EventKind::Reconciled {
            assert!(
                allowed(&spec, Role::Reviewer, e),
                "{} is an observation",
                e.name()
            );
        }
    }
}

#[test]
fn a_toggle_outside_its_domain_or_saying_nothing_is_refused() {
    let mut spec = preset("design-reviewed").unwrap();
    spec.toggles.lands = vec![Role::Implementer];
    let e = spec.validate().unwrap_err().to_string();
    assert!(e.contains("toggle `lands` may name only"), "{e}");

    let mut direct = preset("autonomous").unwrap();
    direct.toggles.approves_design = vec![Role::Coordinator];
    let e = direct.validate().unwrap_err().to_string();
    assert!(e.contains("would say nothing"), "{e}");

    let mut rounds = preset("design-reviewed").unwrap();
    rounds.attributes.repeated_area.rounds = 1;
    assert!(rounds.validate().is_err(), "one round cannot repeat");
}

#[test]
fn a_spec_round_trips_and_an_unknown_field_is_refused_not_ignored() {
    let spec = preset("coordinated").unwrap();
    let json = spec.to_json().unwrap();
    assert_eq!(StrategySpec::from_json(&json).unwrap(), spec);
    let minimal = StrategySpec::from_json(r#"{"graph":"direct"}"#).unwrap();
    assert_eq!(minimal.toggles, Toggles::default(), "every field defaults");
    let newer =
        r#"{"graph":"direct","toggles":{"lands":["operator"],"approves_merges":["coordinator"]}}"#;
    let e = StrategySpec::from_json(newer).unwrap_err().to_string();
    assert!(e.contains("a newer jkb"), "{e}");
    assert!(StrategySpec::from_json(r#"{"graph":"sideways"}"#).is_err());
}

#[test]
fn the_effective_table_is_the_base_plus_the_toggles() {
    let coordinated = preset("coordinated").unwrap().permissions();
    assert!(coordinated.permits(Role::Coordinator, WorkflowEvent::ApproveDesign));
    assert!(coordinated.permits(Role::Coordinator, WorkflowEvent::RejectDesign));
    assert!(!BASE.permits(Role::Coordinator, WorkflowEvent::ApproveDesign));
    assert_eq!(coordinated.check(), vec![]);
}

#[test]
fn next_actor_names_a_role_for_every_live_phase() {
    let spec = preset("design-reviewed").unwrap();
    assert_eq!(spec.next_actor(Phase::DesignReview).0, Role::Operator);
    assert_eq!(spec.next_actor(Phase::Landable).0, Role::Operator);
    assert_eq!(spec.next_actor(Phase::Review).0, Role::Reviewer);
    let auto = preset("autonomous").unwrap();
    assert_eq!(auto.next_actor(Phase::Landable).0, Role::Coordinator);
    for &p in Phase::ALL {
        assert!(!spec.next_actor(p).1.is_empty(), "{}", p.as_str());
    }
}
