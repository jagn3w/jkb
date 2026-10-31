use jkb_fsm::{Event as _, Fact, Outcome, Reconciliation, State as _};
use jkb_rbac::{Decision, Grants as _};
use jkb_types::TaskStatus;

use super::strategy::{preset, AreaScope, StrategySpec, Toggles, BASE, PRESETS, TOGGLES};
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
    assert_eq!(to(GraphId::DesignGate), Phase::DesignReview);
    assert_eq!(to(GraphId::Direct), Phase::Implement);
    assert!(GraphId::DesignGate.reaches_unforced(Phase::DesignReview));
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
    match GraphId::DesignGate.machine().reconcile(f) {
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
    let m = GraphId::DesignGate.machine();
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
    assert_eq!(d.graph, GraphId::DesignGate);
    assert_eq!(d.toggles, Toggles::default());
    assert_eq!(BASE.check(), vec![], "the shared base table is sound");
}

#[test]
fn each_preset_says_what_it_changes_from_design_reviewed() {
    let base = preset("design-reviewed").unwrap();
    assert_eq!(base.changes_from(&base), Vec::<String>::new());
    assert_eq!(
        preset("coordinated").unwrap().changes_from(&base),
        vec!["approves designs: operator, coordinator (default: operator)"]
    );
    assert_eq!(
        preset("autonomous").unwrap().changes_from(&base),
        vec![
            "graph: direct (default: design-gate)",
            "lands: operator, coordinator (default: operator)",
        ]
    );
    // Against another base, the lines name that base's values as the default.
    assert_eq!(
        base.changes_from(&preset("autonomous").unwrap()),
        vec![
            "graph: design-gate (default: direct)",
            "lands: operator (default: operator, coordinator)",
        ]
    );
}

#[test]
fn a_toggle_change_is_the_roles_it_grants_not_how_the_list_is_written() {
    let base = preset("design-reviewed").unwrap();
    let mut same = base.clone();
    // The operator always holds every toggle, so an empty list grants what `[operator]` does.
    same.toggles.lands = vec![];
    assert_eq!(same.changes_from(&base), Vec::<String>::new());
    let coordinated = preset("coordinated").unwrap();
    let mut reordered = coordinated.clone();
    reordered.toggles.approves_design = vec![Role::Coordinator, Role::Operator];
    assert_eq!(reordered.changes_from(&coordinated), Vec::<String>::new());
}

#[test]
fn a_repeated_area_change_says_its_rounds_and_scope() {
    let base = preset("design-reviewed").unwrap();
    let mut spec = base.clone();
    spec.attributes.repeated_area.rounds = 3;
    spec.attributes.repeated_area.scope = AreaScope::Directory;
    assert_eq!(
        spec.changes_from(&base),
        vec!["must-fixes repeat after 3 rounds in one directory (default: 2 in one file)"]
    );
}

/// `changes_from` walks `TOGGLES`, so every toggle is named without anyone adding it there.
#[test]
fn every_toggle_has_a_label_and_flipping_it_alone_is_one_change() {
    let base = preset("design-reviewed").unwrap();
    for t in TOGGLES {
        assert!(!t.label.trim().is_empty(), "{} has no label", t.name);
        let mut spec = base.clone();
        let flipped: Vec<Role> = if base.toggles.get(t.name).iter().eq(t.domain.iter()) {
            vec![Role::Operator]
        } else {
            t.domain.to_vec()
        };
        spec.toggles.set(t.name, flipped).unwrap();
        let lines = spec.changes_from(&base);
        assert_eq!(lines.len(), 1, "{}: {lines:?}", t.name);
        assert!(
            lines[0].starts_with(&format!("{}: ", t.label)),
            "{}: {lines:?}",
            t.name
        );
    }
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

/// `design-gate` was `reviewed-design`; the old name is read, never written.
#[test]
fn the_graph_s_former_name_is_read_and_its_new_name_written() {
    assert_eq!(GraphId::parse("design-gate"), Some(GraphId::DesignGate));
    assert_eq!(GraphId::parse("reviewed-design"), Some(GraphId::DesignGate));
    assert_eq!(GraphId::parse("direct"), Some(GraphId::Direct));
    assert_eq!(
        GraphId::parse("design-reviewed"),
        None,
        "a preset is not a graph"
    );
    let old = StrategySpec::from_json(r#"{"graph":"reviewed-design"}"#).unwrap();
    assert_eq!(old.graph, GraphId::DesignGate);
    assert!(old.to_json().unwrap().contains(r#""graph":"design-gate""#));
    for &(name, g) in GraphId::LEGACY_ALIASES {
        assert!(
            GraphId::ALL.iter().all(|c| c.as_str() != name),
            "an alias never shadows a current name"
        );
        let json = format!(r#"{{"graph":"{name}"}}"#);
        assert_eq!(
            StrategySpec::from_json(&json).unwrap().graph,
            g,
            "serde agrees with parse"
        );
    }
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

#[test]
fn the_coordinator_s_base_permits_are_exactly_these() {
    // Pinned whole, so a widening is a visible edit here rather than an unnoticed row: `reopen`
    // (picking up what the operator landed or cancelled) and `override` stay the operator's.
    assert_eq!(
        BASE.permissions_of(Role::Coordinator),
        vec![
            WorkflowEvent::SubmitDesign,
            WorkflowEvent::SubmitWork,
            WorkflowEvent::SubmitSystemic,
            WorkflowEvent::SystemicRedesign,
            WorkflowEvent::Rework,
            WorkflowEvent::Cancel,
        ]
    );
}

/// A verdict as something comparable: `None` to allow, the denial's sentence to refuse.
fn verdict(v: jkb_fsm::Verdict<WorkflowEvent>) -> Option<String> {
    match v {
        jkb_fsm::Verdict::Allow => None,
        jkb_fsm::Verdict::Deny(d) => Some(d.reason),
    }
}

#[test]
fn one_guard_per_event() {
    // `describe_guard` is keyed by event: that is only sound while every guarded row of an event
    // answers every observation the same way.
    for &g in GraphId::ALL {
        let m = g.machine();
        for &e in <WorkflowEvent as jkb_fsm::Event>::ALL {
            let guards: Vec<_> = m
                .transitions
                .iter()
                .filter(|t| t.event == e)
                .filter_map(|t| t.guard)
                .collect();
            for f in contexts() {
                let answers: Vec<_> = guards.iter().map(|guard| verdict(guard(&f))).collect();
                assert!(
                    answers.windows(2).all(|w| w[0] == w[1]),
                    "{} rows of {} disagree at {f:?}",
                    e.as_str(),
                    g.as_str()
                );
            }
        }
    }
}

#[test]
fn every_guarded_row_says_what_its_guard_checks() {
    let area = super::strategy::RepeatedArea::default();
    let mut guarded_events = Vec::new();
    for &g in GraphId::ALL {
        for t in g.machine().transitions {
            if t.guard.is_some() {
                assert!(
                    t.event.describe_guard(&area).is_some(),
                    "{} --{}--> in {} is guarded but says nothing about it",
                    t.from.as_str(),
                    t.event.as_str(),
                    g.as_str()
                );
                guarded_events.push(t.event);
            }
        }
    }
    // And no description outlives its guard.
    for &e in <WorkflowEvent as jkb_fsm::Event>::ALL {
        assert_eq!(
            e.describe_guard(&area).is_some(),
            guarded_events.contains(&e),
            "{}",
            e.as_str()
        );
    }
}

#[test]
fn the_repeated_review_guard_reads_the_strategy_s_attribute() {
    use super::strategy::{AreaScope, RepeatedArea};
    let two_files = RepeatedArea {
        scope: AreaScope::File,
        rounds: 2,
    };
    let said = WorkflowEvent::ReviewRepeated
        .describe_guard(&two_files)
        .unwrap();
    assert!(
        said.contains("the newest 2 rounds all flagged must-fixes in the same file"),
        "{said}"
    );
    let three_dirs = RepeatedArea {
        scope: AreaScope::Directory,
        rounds: 3,
    };
    let said = WorkflowEvent::ReviewFailed
        .describe_guard(&three_dirs)
        .unwrap();
    assert!(
        said.contains("but not the newest 3 rounds all flagged must-fixes in the same directory"),
        "{said}"
    );
}

#[test]
fn each_event_names_the_command_that_fires_it() {
    assert_eq!(
        WorkflowEvent::SubmitWork.command(),
        "jkb workflow fire <uid> submit_work"
    );
    assert_eq!(
        WorkflowEvent::SubmitSystemic.command(),
        "jkb workflow fire <uid> submit_systemic --reason <why>"
    );
    assert_eq!(
        WorkflowEvent::Override.command(),
        "jkb workflow fire <uid> override --to <phase> --reason <why>"
    );
    for e in [
        WorkflowEvent::ReviewPassed,
        WorkflowEvent::ReviewFailed,
        WorkflowEvent::ReviewRepeated,
        WorkflowEvent::ObservedLanded,
        WorkflowEvent::ObservedCancelled,
    ] {
        assert_eq!(e.command(), "jkb workflow observe <uid>", "{}", e.as_str());
    }
    // Every reason an event needs is in its command.
    for &e in <WorkflowEvent as jkb_fsm::Event>::ALL {
        assert_eq!(
            e.command().contains("--reason"),
            e.needs_reason(),
            "{}",
            e.as_str()
        );
    }
}

/// The compiled-in descriptions — presets, graphs, agent sets and packaged agents — are each one
/// sentence: a capital first letter, a final full stop, no line break. An operator's own text only
/// has to pass `validate_describe`.
#[test]
fn every_compiled_in_description_is_one_sentence() {
    let mut all: Vec<(String, &str)> = Vec::new();
    for p in PRESETS {
        all.push((format!("preset {}", p.name), p.describe));
    }
    for g in GraphId::ALL {
        all.push((format!("graph {}", g.as_str()), g.describe()));
    }
    for s in super::agents::agent_sets().unwrap() {
        all.push((format!("agent set {}", s.name), &s.describe));
    }
    for a in super::agents::packaged().unwrap() {
        all.push((format!("agent {}", a.name), &a.def.describe));
    }
    for (what, d) in all {
        super::validate_describe(d).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert!(
            d.chars().next().is_some_and(char::is_uppercase),
            "{what} starts lowercase: {d}"
        );
        assert!(
            d.ends_with('.'),
            "{what} does not end with a full stop: {d}"
        );
        assert!(!d.contains('\n'), "{what} spans lines: {d}");
        assert!(
            !d.contains("(the default)"),
            "{what} claims the default: {d}"
        );
    }
}

/// A graph's sentence says why; its path says what. They are different texts.
#[test]
fn a_graph_s_sentence_is_not_its_path() {
    for g in GraphId::ALL {
        assert!(g.path().contains('→'), "{}", g.as_str());
        assert!(!g.describe().contains('→'), "{}", g.as_str());
    }
}
