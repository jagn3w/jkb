use jkb_fsm::Fact;
use jkb_types::{ItemId, TaskStatus};

use super::{
    current, define, fire, observe, observe_facts, repeated, rounds, set_strategy, Actor, Moved,
    Round, FACET_AREA,
};
use crate::roles::{self, Minter, Role};
use crate::task::{create, set_status, NewTask};
use crate::workflow::strategy::{preset, AreaScope};
use crate::workflow::{Phase, WorkflowEvent};
use crate::{tag, Db};

fn a_task(db: &Db) -> ItemId {
    db.write_txn("test", |c, m| create(c, m, &NewTask::new("task:w", "W")))
        .unwrap()
}

fn as_role(role: Role) -> Actor {
    Actor {
        roles: vec![role],
        principal: format!("test-{}", role.as_str()),
    }
}

fn do_fire(db: &Db, id: ItemId, event: WorkflowEvent, role: Role, reason: Option<&str>) -> Moved {
    let reason = reason.map(str::to_owned);
    db.write_txn("test", move |c, m| {
        fire(c, m, id, event, &as_role(role), reason.as_deref(), None)
    })
    .unwrap()
}

fn do_observe(db: &Db, id: ItemId) -> Moved {
    db.write_txn("test", move |c, m| {
        observe(c, m, id, &as_role(Role::Reviewer))
    })
    .unwrap()
}

fn phase(db: &Db, id: ItemId) -> Phase {
    db.read(move |c| current(c, id)).unwrap().phase
}

/// File a review round: findings under `ns`, one per `(priority, area)`, recorded on the task.
fn file_round(db: &Db, id: ItemId, n: usize, findings: &[(i64, &str)]) {
    let findings: Vec<(i64, String)> = findings
        .iter()
        .map(|(p, a)| (*p, (*a).to_owned()))
        .collect();
    db.write_txn("test", move |c, m| {
        let ns = format!("reviews/r{n}");
        for (i, (priority, area)) in findings.iter().enumerate() {
            let mut spec = NewTask::new(format!("task:f{n}-{i}"), format!("finding {i}"));
            spec.home = format!("{ns}/must-fix");
            spec.priority = Some(*priority);
            let f = create(c, m, &spec)?;
            if !area.is_empty() {
                tag::apply(c, m, f, FACET_AREA, area)?;
            }
        }
        crate::reviews::record(
            c,
            m,
            id,
            &ns,
            "abc",
            "test",
            crate::reviews::RoundSource::AnyNamespace,
        )
    })
    .unwrap();
}

#[test]
fn a_task_walks_the_default_strategy_and_each_role_moves_only_its_step() {
    let db = Db::open_in_memory().unwrap();
    let id = a_task(&db);
    let cur = db.read(move |c| current(c, id)).unwrap();
    assert_eq!(cur.phase, Phase::Design);
    assert_eq!(cur.spec, preset("design-reviewed").unwrap());
    assert_eq!(cur.source, "default:design-reviewed");

    assert!(matches!(
        do_fire(&db, id, WorkflowEvent::SubmitWork, Role::Implementer, None),
        Moved::Refused(_)
    ));
    assert!(matches!(
        do_fire(&db, id, WorkflowEvent::SubmitDesign, Role::Designer, None),
        Moved::To {
            to: Phase::DesignReview,
            ..
        }
    ));
    // The coordinator may not approve under the default; the operator may.
    let Moved::Refused(why) = do_fire(
        &db,
        id,
        WorkflowEvent::ApproveDesign,
        Role::Coordinator,
        None,
    ) else {
        panic!()
    };
    assert!(why.contains("operator"), "{why}");
    assert_eq!(phase(&db, id), Phase::DesignReview);
    do_fire(&db, id, WorkflowEvent::ApproveDesign, Role::Operator, None);
    assert_eq!(phase(&db, id), Phase::Implement);

    // Round 1: a must-fix in a.rs. Nothing moves until a round is filed after submission.
    do_fire(&db, id, WorkflowEvent::SubmitWork, Role::Implementer, None);
    assert_eq!(do_observe(&db, id), Moved::AlreadyThere(Phase::Review));
    file_round(&db, id, 1, &[(1, "src/a.rs"), (3, "")]);
    assert!(matches!(
        do_observe(&db, id),
        Moved::To {
            event: WorkflowEvent::ReviewFailed,
            to: Phase::Implement,
            ..
        }
    ));

    // Round 2: a must-fix in a.rs again — repeated, so a systemic reviewer first.
    do_fire(&db, id, WorkflowEvent::SubmitWork, Role::Implementer, None);
    file_round(&db, id, 2, &[(1, "src/a.rs")]);
    assert!(matches!(
        do_observe(&db, id),
        Moved::To {
            event: WorkflowEvent::ReviewRepeated,
            to: Phase::SystemicReview,
            ..
        }
    ));
    let e = db
        .write_txn("test", move |c, m| {
            fire(
                c,
                m,
                id,
                WorkflowEvent::SubmitSystemic,
                &as_role(Role::SystemicReviewer),
                None,
                None,
            )
        })
        .unwrap_err()
        .to_string();
    assert!(e.contains("needs a written reason"), "{e}");
    do_fire(
        &db,
        id,
        WorkflowEvent::SubmitSystemic,
        Role::SystemicReviewer,
        Some("one parser owns the escaping now"),
    );
    assert_eq!(phase(&db, id), Phase::Implement);

    // Round 3: clean — landable.
    do_fire(&db, id, WorkflowEvent::SubmitWork, Role::Implementer, None);
    file_round(&db, id, 3, &[(3, "src/b.rs")]);
    assert!(matches!(
        do_observe(&db, id),
        Moved::To {
            event: WorkflowEvent::ReviewPassed,
            to: Phase::Landable,
            ..
        }
    ));

    let hist = db.read(move |c| current(c, id)).unwrap().history;
    let systemic = hist.iter().find(|r| r.event == "submit_systemic").unwrap();
    assert_eq!(
        systemic.reason.as_deref(),
        Some("one parser owns the escaping now")
    );
    assert_eq!(systemic.actor, "test-systemic_reviewer");
}

#[test]
fn rounds_are_ordered_by_filing_and_count_must_fixes_at_any_status() {
    let db = Db::open_in_memory().unwrap();
    let id = a_task(&db);
    file_round(&db, id, 2, &[(1, "x.rs")]);
    file_round(&db, id, 1, &[(2, "y.rs")]);
    // Fix round 2's must-fix: the round still found one.
    db.write_txn("test", |c, m| {
        let f = crate::item::id_for_uid(c, "task:f2-0")?.unwrap();
        set_status(c, m, f, TaskStatus::Done)
    })
    .unwrap();
    let rs = db.read(move |c| rounds(c, id)).unwrap();
    assert_eq!(
        rs.iter().map(|r| r.ns.as_str()).collect::<Vec<_>>(),
        vec!["reviews/r2", "reviews/r1"],
        "filed order, not name order"
    );
    assert_eq!(
        rs[0].must_fix, 1,
        "a fixed must-fix still counts for its round"
    );
    assert_eq!(rs[1].must_fix, 0);
}

#[test]
fn repetition_needs_every_recent_round_to_share_an_area() {
    let r = |ns: &str, filed, must_fix, areas: &[&str]| Round {
        ns: ns.into(),
        filed,
        must_fix,
        areas: areas.iter().map(|a| (*a).to_owned()).collect(),
    };
    let rs = [
        r("a", 1, 1, &["src/x/a.rs"]),
        r("b", 2, 2, &["src/x/b.rs", "src/y.rs"]),
    ];
    assert!(!repeated(&rs, 2, AreaScope::File));
    assert!(repeated(&rs, 2, AreaScope::Directory), "src/x in both");
    assert!(
        !repeated(&rs, 3, AreaScope::Directory),
        "only two rounds exist"
    );
    let clean_between = [rs[0].clone(), r("c", 3, 0, &[])];
    assert!(!repeated(&clean_between, 2, AreaScope::File));
}

#[test]
fn a_title_names_the_area_for_findings_filed_before_the_area_facet() {
    let db = Db::open_in_memory().unwrap();
    let id = a_task(&db);
    db.write_txn("test", move |c, m| {
        let mut spec = NewTask::new("task:old", "parser drops quotes — src/parse.rs:42");
        spec.home = "reviews/old/must-fix".into();
        spec.priority = Some(1);
        create(c, m, &spec)?;
        crate::reviews::record(
            c,
            m,
            id,
            "reviews/old",
            "abc",
            "test",
            crate::reviews::RoundSource::AnyNamespace,
        )
    })
    .unwrap();
    let rs = db.read(move |c| rounds(c, id)).unwrap();
    assert_eq!(rs[0].areas, vec!["src/parse.rs".to_owned()]);
}

#[test]
fn only_the_operator_sets_a_strategy_and_a_task_keeps_the_version_it_pinned() {
    let db = Db::open_in_memory().unwrap();
    let id = a_task(&db);
    let e = db
        .write_txn("test", move |c, m| {
            set_strategy(c, m, id, "autonomous", &as_role(Role::Coordinator))
        })
        .unwrap_err()
        .to_string();
    assert!(e.contains("only the operator"), "{e}");

    db.write_txn("test", move |c, m| {
        let mut spec = preset("coordinated").unwrap();
        spec.attributes.repeated_area.rounds = 3;
        define(c, m, "mine", &spec)?;
        set_strategy(c, m, id, "mine", &Actor::operator())?;
        // Redefined after the task pinned it.
        spec.attributes.repeated_area.rounds = 5;
        define(c, m, "mine", &spec)
    })
    .unwrap();
    let cur = db.read(move |c| current(c, id)).unwrap();
    assert_eq!(
        cur.spec.attributes.repeated_area.rounds, 3,
        "pinned, not live"
    );
    assert_eq!(cur.source, "mine@1");
    assert_eq!(
        cur.phase,
        Phase::Design,
        "setting a strategy does not move the phase"
    );

    let e = db
        .write_txn("test", |c, m| {
            define(c, m, "autonomous", &preset("autonomous").unwrap())
        })
        .unwrap_err()
        .to_string();
    assert!(e.contains("is a preset"), "{e}");
}

#[test]
fn a_default_definition_replaces_the_default_preset() {
    let db = Db::open_in_memory().unwrap();
    let id = a_task(&db);
    db.write_txn("test", |c, m| {
        define(c, m, "default", &preset("autonomous").unwrap())
    })
    .unwrap();
    let cur = db.read(move |c| current(c, id)).unwrap();
    assert_eq!(cur.source, "default:default@1");
    assert_eq!(cur.spec, preset("autonomous").unwrap());
}

#[test]
fn a_cancelled_task_settles_its_workflow_and_revokes_its_workers() {
    let db = Db::open_in_memory().unwrap();
    let id = a_task(&db);
    let (worker, token) = db
        .write_txn("test", move |c, m| {
            roles::mint(
                c,
                m,
                Minter::Operator,
                Role::Implementer,
                "impl-1",
                Some(id),
            )
        })
        .unwrap();
    db.write_txn("test", move |c, m| {
        set_status(c, m, id, TaskStatus::Cancelled)
    })
    .unwrap();
    let facts = db
        .read(move |c| {
            let cur = current(c, id)?;
            observe_facts(c, id, &cur)
        })
        .unwrap();
    assert_eq!(facts.landed, Fact::No);
    assert!(matches!(
        do_observe(&db, id),
        Moved::To {
            event: WorkflowEvent::ObservedCancelled,
            to: Phase::Cancelled,
            revoked: 1,
            ..
        }
    ));
    assert_eq!(
        db.read(move |c| roles::resolve(c, &token)).unwrap(),
        None,
        "grant {} no longer resolves",
        worker.id
    );
}

/// Record `ns` (already holding findings) against `id` as `source` allows.
fn record(
    db: &Db,
    id: ItemId,
    ns: &str,
    source: crate::reviews::RoundSource<'static>,
) -> crate::Result<()> {
    let ns = ns.to_owned();
    db.write_txn("test", move |c, m| {
        crate::reviews::record(c, m, id, &ns, "abc", "test", source)
    })
}

#[test]
fn a_round_is_what_it_was_when_recorded() {
    let db = Db::open_in_memory().unwrap();
    let id = a_task(&db);
    file_round(&db, id, 1, &[(3, "a.rs")]);
    file_round(&db, id, 2, &[(1, "b.rs")]);
    // The implementer under review lowers round 2's must-fix, and files a line into round 1 so
    // that it would sort newest by item id.
    db.write_txn("test", |c, m| {
        let f = crate::item::id_for_uid(c, "task:f2-0")?.unwrap();
        crate::task::set_priority(c, m, f, Some(3))?;
        let mut spec = NewTask::new("task:late", "late");
        spec.home = "reviews/r1/must-fix".into();
        spec.priority = Some(3);
        create(c, m, &spec)?;
        Ok(())
    })
    .unwrap();
    let rs = db.read(move |c| rounds(c, id)).unwrap();
    assert_eq!(
        rs.iter().map(|r| r.ns.as_str()).collect::<Vec<_>>(),
        vec!["reviews/r1", "reviews/r2"],
        "recording order"
    );
    assert_eq!(rs[1].must_fix, 1, "the snapshot, not the lowered priority");
    assert_eq!(rs[1].areas, vec!["b.rs".to_owned()]);
    let must = db
        .read(|c| crate::reviews::must_fix_findings(c, &["reviews/r2".to_owned()]))
        .unwrap();
    assert_eq!(must.len(), 1);
}

#[test]
fn only_a_filing_is_a_round_to_a_non_operator_and_scope_follows_the_snapshot() {
    let db = Db::open_in_memory().unwrap();
    let id = a_task(&db);
    let other = db
        .write_txn("test", |c, m| {
            let mut spec = NewTask::new("task:other", "O");
            spec.home = "tasks/elsewhere".into();
            create(c, m, &spec)
        })
        .unwrap();
    let e = record(&db, id, "tasks", crate::reviews::RoundSource::Filed("w")).unwrap_err();
    assert!(e.to_string().contains("not a namespace"), "{e}");
    assert!(!db.read(move |c| roles::in_scope(c, id, other)).unwrap());
    // A filing: exactly what was filed is the round, and so the scope.
    let f = db
        .write_txn("test", |c, m| {
            let mut spec = NewTask::new("task:filed", "finding");
            spec.home = "reviews/f/must-fix".into();
            spec.priority = Some(1);
            let f = create(c, m, &spec)?;
            crate::reviews::record_filing(c, "reviews/f", &[f], "w")?;
            Ok(f)
        })
        .unwrap();
    // Someone else's filing is not this caller's to record.
    let e = record(
        &db,
        id,
        "reviews/f",
        crate::reviews::RoundSource::Filed("other"),
    )
    .unwrap_err();
    assert!(e.to_string().contains("this caller filed"), "{e}");
    record(
        &db,
        id,
        "reviews/f",
        crate::reviews::RoundSource::Filed("w"),
    )
    .unwrap();
    assert!(db.read(move |c| roles::in_scope(c, id, f)).unwrap());
    // Something placed under the round's namespace afterwards is not one of its findings.
    let late = db
        .write_txn("test", |c, m| {
            let mut spec = NewTask::new("task:late2", "late");
            spec.home = "reviews/f/must-fix".into();
            create(c, m, &spec)
        })
        .unwrap();
    assert!(!db.read(move |c| roles::in_scope(c, id, late)).unwrap());
}

#[test]
fn redefining_default_does_not_move_a_task_that_has_started() {
    let db = Db::open_in_memory().unwrap();
    let id = a_task(&db);
    let fresh = db
        .write_txn("test", |c, m| {
            create(c, m, &NewTask::new("task:fresh", "F"))
        })
        .unwrap();
    assert!(matches!(
        do_fire(&db, id, WorkflowEvent::SubmitDesign, Role::Designer, None),
        Moved::To { .. }
    ));
    db.write_txn("test", |c, m| {
        define(c, m, "default", &preset("autonomous").unwrap())
    })
    .unwrap();
    let cur = db.read(move |c| current(c, id)).unwrap();
    assert_eq!(
        cur.spec,
        preset("design-reviewed").unwrap(),
        "pinned at its first move"
    );
    assert_eq!(cur.source, "default:design-reviewed");
    let cur = db.read(move |c| current(c, fresh)).unwrap();
    assert_eq!(
        cur.spec,
        preset("autonomous").unwrap(),
        "a task not started takes the new default"
    );
}

#[test]
fn an_unreadable_default_is_refused_not_replaced_by_the_preset() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("test", |c, _| {
        c.execute(
            "INSERT INTO workflow_strategies (name, version, spec, defined_at)
             VALUES ('default', 1, '{\"graph\":\"direct\",\"from_the_future\":1}', 'now')",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    assert!(db.read(super::default_strategy).is_err());
}

#[test]
fn reopen_follows_the_task_s_lifecycle_and_is_the_operator_s() {
    let db = Db::open_in_memory().unwrap();
    let id = a_task(&db);
    db.write_txn("test", move |c, m| {
        set_status(c, m, id, TaskStatus::Cancelled)
    })
    .unwrap();
    assert!(matches!(
        do_observe(&db, id),
        Moved::To {
            to: Phase::Cancelled,
            ..
        }
    ));
    // Reopening the workflow alone would be undone by the next observe.
    let refused = do_fire(&db, id, WorkflowEvent::Reopen, Role::Operator, None);
    assert!(
        matches!(&refused, Moved::Refused(why) if why.contains("--status open")),
        "{refused:?}"
    );
    // The lifecycle is reopened — by anyone, a synced checkbox included — and the workflow stays
    // parked: observing does not follow it, and only the operator's `reopen` does.
    db.write_txn("test", move |c, m| set_status(c, m, id, TaskStatus::Open))
        .unwrap();
    assert!(matches!(
        do_observe(&db, id),
        Moved::AlreadyThere(Phase::Cancelled)
    ));
    assert!(
        matches!(
            do_fire(&db, id, WorkflowEvent::Reopen, Role::Coordinator, None),
            Moved::Refused(_)
        ),
        "the coordinator does not reopen"
    );
    assert!(matches!(
        do_fire(&db, id, WorkflowEvent::Reopen, Role::Operator, None),
        Moved::To {
            to: Phase::Implement,
            ..
        }
    ));
    assert!(matches!(
        do_fire(&db, id, WorkflowEvent::Reopen, Role::Operator, None),
        Moved::AlreadyThere(Phase::Implement)
    ));
}

#[test]
fn a_deleted_task_keeps_its_workflow_history() {
    let db = Db::open_in_memory().unwrap();
    let id = a_task(&db);
    do_fire(&db, id, WorkflowEvent::SubmitDesign, Role::Designer, None);
    db.write_txn("test", move |c, m| crate::item::remove(c, m, id, true))
        .unwrap();
    assert!(!db.read(move |c| super::history(c, id)).unwrap().is_empty());
}
