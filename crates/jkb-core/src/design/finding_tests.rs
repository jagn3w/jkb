//! Review findings filed beside the reviewed task, under its plan step: a step's count leaves a
//! finished finding out ([`plan::step_tasks`]), and a task reaching a terminal status shelves its
//! rounds' still-open findings into the repo's Open work › Inbox
//! ([`crate::reviews::shelve_open_findings`], from [`crate::transition::perform`]).

use jkb_fsm::Fact;
use jkb_types::{AgentId, ItemId, TaskStatus};

use super::plan;
use crate::lifecycle::{TaskEvent, TaskFacts};
use crate::task::{self, NewTask, Under};
use crate::transition::{self, Labels};
use crate::{containment, item, ns, reviews, tag, Db};

/// A database that knows repo `app`, as one mounting it would.
fn db() -> Db {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| ns::ensure(c, m, "repos/app").map(|_| ()))
        .unwrap();
    db
}

/// A design in repo `app` with a one-step plan: `(plan uid, step id)`.
fn planned(db: &Db, title: &str) -> (String, ItemId) {
    let title = title.to_owned();
    db.write_txn("t", move |c, m| {
        let d = super::create(c, m, "app", &title, "Build it.")?.uid;
        let p = plan::create(c, m, &d, &title, &["one".to_owned()])?;
        let step = item::id_for_uid(c, &p.steps[0].uid)?.unwrap();
        Ok((p.uid, step))
    })
    .unwrap()
}

/// A task under `parent`.
fn task_under(db: &Db, uid: &str, parent: ItemId) -> ItemId {
    let uid = uid.to_owned();
    db.write_txn("t", move |c, m| {
        task::create(c, m, &NewTask::new(uid, "work", Under::Parent(parent)))
    })
    .unwrap()
}

/// Findings filed into review namespace `ns` under `parent`, each at `(priority, status)`, recorded
/// as `task`'s round when `task` is given.
fn findings(
    db: &Db,
    ns: &str,
    parent: ItemId,
    each: &[(i64, TaskStatus)],
    task: Option<ItemId>,
) -> Vec<ItemId> {
    let ns = ns.to_owned();
    let each = each.to_vec();
    db.write_txn("t", move |c, m| {
        let mut ids = Vec::new();
        for (i, (priority, status)) in each.iter().enumerate() {
            let mut spec = NewTask::new(
                format!("task:finding-{}-{i}", ns.replace('/', "-")),
                format!("finding {i}"),
                Under::Parent(parent),
            );
            spec.priority = Some(*priority);
            spec.home = format!("{ns}/nit");
            let id = task::create(c, m, &spec)?;
            let (facet, value) = reviews::FINDING_TAG;
            tag::apply(c, m, id, facet, value)?;
            if *status != TaskStatus::Open {
                task::set_status(c, m, id, *status)?;
            }
            ids.push(id);
        }
        reviews::record_filing(c, &ns, &ids, "reviewer")?;
        if let Some(task) = task {
            reviews::record(
                c,
                m,
                task,
                &ns,
                "abc123",
                "reviewer",
                reviews::RoundSource::Filed("reviewer"),
            )?;
        }
        Ok(ids)
    })
    .unwrap()
}

fn counts(db: &Db, step: ItemId) -> (bool, bool) {
    let n = db.read(move |c| plan::step_tasks(c, step)).unwrap();
    (n.implemented(), n.finished())
}

fn set_status(db: &Db, id: ItemId, status: TaskStatus) {
    db.write_txn("t", move |c, m| task::set_status(c, m, id, status))
        .unwrap();
}

fn parent(db: &Db, id: ItemId) -> Option<ItemId> {
    db.read(move |c| containment::parent(c, id)).unwrap()
}

fn inbox(db: &Db) -> ItemId {
    db.read(|c| item::id_for_uid(c, &plan::triage_step_uid("app")))
        .unwrap()
        .expect("the Inbox exists")
}

fn archived(db: &Db, plan_uid: &str) -> bool {
    let plan_uid = plan_uid.to_owned();
    db.read(move |c| plan::show(c, &plan_uid)).unwrap().archived
}

/// Test 6: a finished finding is neutral in a step's count, an open one keeps the step open, a step
/// of findings alone is never vacuously implemented, and a hand-applied `finding=review` tag is not
/// trusted.
#[test]
fn a_finished_finding_is_neutral_in_a_steps_count() {
    let db = db();
    let (_, step) = planned(&db, "Counted");
    let t = task_under(&db, "task:t", step);
    set_status(&db, t, TaskStatus::Done);
    findings(
        &db,
        "repos/app/codereviews/r1",
        step,
        &[(3, TaskStatus::Cancelled), (3, TaskStatus::Done)],
        None,
    );
    assert_eq!(
        counts(&db, step),
        (true, true),
        "done task + finished findings"
    );
    findings(
        &db,
        "repos/app/codereviews/r2",
        step,
        &[(3, TaskStatus::Open)],
        None,
    );
    assert_eq!(
        counts(&db, step),
        (false, false),
        "an open finding holds the step"
    );

    let (_, only) = planned(&db, "Only findings");
    findings(
        &db,
        "repos/app/codereviews/r3",
        only,
        &[(3, TaskStatus::Done)],
        None,
    );
    assert_eq!(counts(&db, only), (false, false), "no vacuous truth");

    // Tagged by hand, never filed: an ordinary cancelled task, which implemented nothing.
    let (_, tagged) = planned(&db, "Tagged");
    let done = task_under(&db, "task:done", tagged);
    set_status(&db, done, TaskStatus::Done);
    let fake = task_under(&db, "task:fake", tagged);
    db.write_txn("t", move |c, m| {
        let (facet, value) = reviews::FINDING_TAG;
        tag::apply(c, m, fake, facet, value)?;
        task::set_status(c, m, fake, TaskStatus::Cancelled)
    })
    .unwrap();
    assert_eq!(counts(&db, tagged), (false, true), "the tag is not trusted");
}

/// The facts a gated landing of an in-progress task observes.
fn landing_facts() -> TaskFacts {
    let owner = AgentId::agent("a");
    TaskFacts {
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
    }
}

/// `event` on `task` with `facts`, in a transaction of its own: its id.
fn perform(db: &Db, task: ItemId, facts: TaskFacts, event: TaskEvent) -> i64 {
    db.write_txn("t", move |c, m| {
        let out = transition::perform(c, m, task, &facts, event, &Labels::default())?;
        assert!(out.refusal().is_none(), "{:?}", out.refusal());
        Ok(m.txn_id)
    })
    .unwrap()
}

fn status(db: &Db, id: ItemId) -> Option<String> {
    db.read(move |c| Ok(item::get(c, id)?.and_then(|m| m.status)))
        .unwrap()
}

/// Test 7: landing a task moves its recorded round's open nit and concern to the repo's Inbox and
/// leaves the finished findings and a hand-moved one where they are; the plan then archives, and an
/// undo of the landing puts everything back.
#[test]
fn landing_shelves_the_rounds_open_findings_and_undo_restores_them() {
    let db = db();
    let (plan_uid, step) = planned(&db, "Shelved");
    let t = task_under(&db, "task:t", step);
    let (_, elsewhere) = planned(&db, "Elsewhere");
    let other = task_under(&db, "task:other", elsewhere);
    let f = findings(
        &db,
        "repos/app/codereviews/r1",
        step,
        &[
            (3, TaskStatus::Open),      // nit
            (2, TaskStatus::Open),      // concern
            (1, TaskStatus::Done),      // must-fix, fixed
            (2, TaskStatus::Cancelled), // refuted
            (3, TaskStatus::Open),      // moved by hand, below
        ],
        Some(t),
    );
    let by_hand = f[4];
    db.write_txn("t", move |c, m| task::move_under(c, m, by_hand, other))
        .unwrap();
    set_status(&db, t, TaskStatus::InProgress);
    let before = status(&db, t);
    assert!(!archived(&db, &plan_uid));

    let txn = perform(&db, t, landing_facts(), TaskEvent::Land);
    assert_eq!(status(&db, t).as_deref(), Some("done"));
    let inbox = inbox(&db);
    assert_eq!(parent(&db, f[0]), Some(inbox), "the nit");
    assert_eq!(parent(&db, f[1]), Some(inbox), "the concern");
    assert_eq!(parent(&db, f[2]), Some(step), "a finished finding stays");
    assert_eq!(parent(&db, f[3]), Some(step), "a cancelled finding stays");
    assert_eq!(parent(&db, f[4]), Some(other), "a hand-moved finding stays");
    assert!(
        archived(&db, &plan_uid),
        "nothing open is left under the step"
    );

    db.write_txn("t", move |c, m| crate::undo::undo(c, m, txn))
        .unwrap();
    assert_eq!(parent(&db, f[0]), Some(step));
    assert_eq!(parent(&db, f[1]), Some(step));
    assert_eq!(status(&db, t), before);
    assert!(!archived(&db, &plan_uid));
}

/// Test 8: an operator's done and a cancel shelve too; a non-terminal transition moves nothing; and a
/// round whose namespace names no repo leaves its findings where they were filed.
#[test]
fn every_terminal_transition_shelves_and_nothing_else_does() {
    let db = db();
    let (_, step) = planned(&db, "Paths");
    let inbox_of = |db: &Db| db.read(|c| item::id_for_uid(c, &plan::triage_step_uid("app")));

    // A non-terminal transition: request changes on a task in review.
    let reviewed = task_under(&db, "task:reviewed", step);
    let open = findings(
        &db,
        "repos/app/codereviews/rc",
        step,
        &[(3, TaskStatus::Open)],
        Some(reviewed),
    );
    set_status(&db, reviewed, TaskStatus::NeedsReview);
    let facts = TaskFacts {
        status: TaskStatus::NeedsReview,
        ..TaskFacts::default()
    };
    perform(&db, reviewed, facts, TaskEvent::RequestChanges);
    assert_eq!(status(&db, reviewed).as_deref(), Some("in_progress"));
    assert_eq!(
        parent(&db, open[0]),
        Some(step),
        "request_changes moves nothing"
    );
    assert_eq!(inbox_of(&db).unwrap(), None, "nor builds an Inbox");

    // The operator's done.
    set_status(&db, reviewed, TaskStatus::Done);
    let inbox = inbox(&db);
    assert_eq!(parent(&db, open[0]), Some(inbox), "task set --status done");

    // A cancel.
    let dropped = task_under(&db, "task:dropped", step);
    let left = findings(
        &db,
        "repos/app/codereviews/rd",
        step,
        &[(2, TaskStatus::Open)],
        Some(dropped),
    );
    let facts = TaskFacts {
        status: TaskStatus::Open,
        ..TaskFacts::default()
    };
    perform(&db, dropped, facts, TaskEvent::Cancel);
    assert_eq!(status(&db, dropped).as_deref(), Some("cancelled"));
    assert_eq!(parent(&db, left[0]), Some(inbox), "cancel");

    // A round in a namespace naming no repo: filed there by a mapping, which is the operator's call.
    let mapped = task_under(&db, "task:mapped", step);
    let stays = findings(
        &db,
        "reviews/x",
        step,
        &[(3, TaskStatus::Open)],
        Some(mapped),
    );
    set_status(&db, mapped, TaskStatus::Done);
    assert_eq!(parent(&db, stays[0]), Some(step), "no repo to shelve into");
}

/// [`plan::step_of`] is the nearest step above a task, through any subtasks, and none for a task
/// under no step.
#[test]
fn step_of_is_the_nearest_step_above_the_task() {
    let db = db();
    let (_, step) = planned(&db, "Nested");
    let t = task_under(&db, "task:t", step);
    let sub = task_under(&db, "task:sub", t);
    let loose = db
        .write_txn("t", |c, m| {
            let id = task::create(
                c,
                m,
                &NewTask::new("task:loose", "l", Under::inbox_of("app")),
            )?;
            task::make_loose(c, m, id)?;
            Ok(id)
        })
        .unwrap();
    let of = |id: ItemId| db.read(move |c| plan::step_of(c, id)).unwrap();
    assert_eq!(of(t), Some(step));
    assert_eq!(of(sub), Some(step), "never the immediate container");
    assert_eq!(of(loose), None);
}
