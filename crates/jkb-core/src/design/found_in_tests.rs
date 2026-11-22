//! The `found_in` edge (finding → the task a review of which filed it): written by the review seam
//! ([`reviews::link_findings`], from [`reviews::record`] here and from `task.review_file` in
//! `jkb-api`), and **display only** — no frontier, blocking rule, subtask fact or step count
//! follows it, and it survives every move a finding makes.

use std::collections::BTreeMap;

use jkb_fsm::Fact;
use proptest::prelude::*;
use rusqlite::params;

use jkb_types::{EdgeType, ItemId, TaskStatus};

use super::plan;
use crate::query::{self, Query};
use crate::task::{self, NewTask, Under};
use crate::{containment, edge, item, ns, reviews, tag, Db};

fn db() -> Db {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| ns::ensure(c, m, "repos/app").map(|_| ()))
        .unwrap();
    db
}

/// A design in repo `app` with a plan of `steps` steps: `(plan uid, step ids)`.
fn planned(db: &Db, title: &str, steps: usize) -> (String, Vec<ItemId>) {
    let title = title.to_owned();
    db.write_txn("t", move |c, m| {
        let d = super::create(c, m, "app", &title, "Build it.")?.uid;
        let names: Vec<String> = (0..steps).map(|i| format!("step {i}")).collect();
        let p = plan::create(c, m, &d, &title, &names)?;
        let mut ids = Vec::new();
        for s in &p.steps {
            ids.push(item::id_for_uid(c, &s.uid)?.unwrap());
        }
        Ok((p.uid, ids))
    })
    .unwrap()
}

fn task_under(db: &Db, uid: &str, parent: ItemId) -> ItemId {
    let uid = uid.to_owned();
    db.write_txn("t", move |c, m| {
        task::create(c, m, &NewTask::new(uid, "work", Under::Parent(parent)))
    })
    .unwrap()
}

/// One finding at `priority` filed into review namespace `ns` under `parent`, recorded as `task`'s
/// round when `record` — what `task.review_file` then `task.review_record` write.
fn finding(db: &Db, ns: &str, parent: ItemId, priority: i64, task: ItemId, record: bool) -> ItemId {
    let ns = ns.to_owned();
    db.write_txn("t", move |c, m| {
        let mut spec = NewTask::new(
            format!("task:finding-{}", ns.replace('/', "-")),
            format!("finding p{priority}"),
            Under::Parent(parent),
        );
        spec.priority = Some(priority);
        spec.home = format!("{ns}/must-fix");
        let id = task::create(c, m, &spec)?;
        let (facet, value) = reviews::FINDING_TAG;
        tag::apply(c, m, id, facet, value)?;
        reviews::record_filing(c, &ns, &[id], "reviewer")?;
        if record {
            reviews::record(
                c,
                m,
                task,
                &ns,
                "abc123",
                "reviewer",
                reviews::RoundSource::Filed("reviewer"),
            )?;
        } else {
            reviews::link_findings(c, m, task, &[id])?;
        }
        Ok(id)
    })
    .unwrap()
}

fn found_in(db: &Db, src: ItemId) -> Vec<ItemId> {
    db.read(move |c| edge::edges_from(c, src, EdgeType::FoundIn))
        .unwrap()
}

fn set_status(db: &Db, id: ItemId, status: TaskStatus) {
    db.write_txn("t", move |c, m| task::set_status(c, m, id, status))
        .unwrap();
}

fn ready_ids(db: &Db) -> Vec<ItemId> {
    db.read(|c| task::ready(c, Query::default()))
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect()
}

fn is_ready(db: &Db) -> Vec<ItemId> {
    db.read(|c| query::parse("is:ready")?.evaluate(c)).unwrap()
}

fn archived(db: &Db, plan_uid: &str) -> bool {
    let plan_uid = plan_uid.to_owned();
    db.read(move |c| plan::show(c, &plan_uid)).unwrap().archived
}

/// Test 2: recording links every finding of the round's snapshot to the recording task; recording
/// again writes nothing; a staging round recorded against two tasks links each finding to both.
#[test]
fn recording_links_the_rounds_findings_to_each_task_that_records_it() {
    let db = db();
    let (_, steps) = planned(&db, "Linked", 1);
    let a = task_under(&db, "task:a", steps[0]);
    let b = task_under(&db, "task:b", steps[0]);
    let ns = "repos/app/codereviews/batch";
    let f = finding(&db, ns, steps[0], 1, a, true);
    assert_eq!(found_in(&db, f), [a]);

    let journal = |db: &Db| -> i64 {
        db.read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM changelog WHERE entity_type = 'edges'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap()
    };
    let before = journal(&db);
    let record = |task: ItemId| {
        db.write_txn("t", move |c, m| {
            reviews::record(
                c,
                m,
                task,
                ns,
                "abc123",
                "reviewer",
                reviews::RoundSource::Filed("reviewer"),
            )
        })
        .unwrap();
    };
    record(a);
    assert_eq!(found_in(&db, f), [a], "recording again links nothing new");
    assert_eq!(journal(&db), before, "and journals nothing");

    record(b);
    assert_eq!(found_in(&db, f), [a, b], "the staging round's second task");
}

/// A finding that is the recording task itself (a round holding the very task that records it) is
/// never linked to itself.
#[test]
fn a_finding_is_never_linked_to_itself() {
    let db = db();
    let (_, steps) = planned(&db, "Self", 1);
    let t = task_under(&db, "task:t", steps[0]);
    db.write_txn("t", move |c, m| reviews::link_findings(c, m, t, &[t]))
        .unwrap();
    assert!(found_in(&db, t).is_empty());
}

/// Test 3, the load-bearing one: an open finding linked `found_in` its task — a must-fix (p1) and
/// a nit (p3) — beside it under the step leaves the task on the ready frontier, unblocked, with no
/// open subtasks.
#[test]
fn an_open_finding_found_in_a_task_never_holds_it() {
    for priority in [1, 3] {
        let db = db();
        let (_, steps) = planned(&db, "Held?", 1);
        let t = task_under(&db, "task:t", steps[0]);
        let f = finding(
            &db,
            &format!("repos/app/codereviews/p{priority}"),
            steps[0],
            priority,
            t,
            true,
        );
        assert_eq!(found_in(&db, f), [t], "p{priority}: the edge is there");
        assert!(ready_ids(&db).contains(&t), "p{priority}: task::ready");
        assert!(is_ready(&db).contains(&t), "p{priority}: is:ready");
        let facts = db.read(move |c| task::observe(c, t)).unwrap();
        assert_eq!(facts.open_subtasks, Fact::No, "p{priority}: open_subtasks");
        assert!(
            !db.read(move |c| task::is_blocked(c, t)).unwrap(),
            "p{priority}: is_blocked"
        );
        assert!(
            db.read(move |c| task::subtasks_all_terminal(c, t)).unwrap(),
            "p{priority}: subtasks_all_terminal"
        );
    }
}

/// Test 3, second case: a finding under a different step, linked to T. T's step archives once T is
/// done; the finding's step, which holds it open, does not.
#[test]
fn a_finding_elsewhere_holds_its_own_step_and_not_the_tasks() {
    let db = db();
    let (t_plan, t_steps) = planned(&db, "Reviewed", 1);
    let (f_plan, f_steps) = planned(&db, "Elsewhere", 1);
    let t = task_under(&db, "task:t", t_steps[0]);
    let f = finding(&db, "repos/app/codereviews/x", f_steps[0], 1, t, false);
    assert_eq!(found_in(&db, f), [t]);
    set_status(&db, t, TaskStatus::Done);
    assert!(archived(&db, &t_plan), "T's step has only T, done");
    assert!(!archived(&db, &f_plan), "F's step holds F open");
}

/// Test 4: the edge survives a `task.move` of the finding, its shelving when the task finishes, and
/// an `item rm` of the finding undone.
#[test]
fn the_edge_survives_moves_shelving_and_an_undone_removal() {
    let db = db();
    let (_, steps) = planned(&db, "Survive", 2);
    let t = task_under(&db, "task:t", steps[0]);
    let f = finding(&db, "repos/app/codereviews/s", steps[0], 3, t, true);

    let to = steps[1];
    db.write_txn("t", move |c, m| task::move_under(c, m, f, to))
        .unwrap();
    assert_eq!(found_in(&db, f), [t], "after task.move");
    let back = steps[0];
    db.write_txn("t", move |c, m| task::move_under(c, m, f, back))
        .unwrap();

    set_status(&db, t, TaskStatus::Done);
    let inbox = db
        .read(|c| item::id_for_uid(c, &plan::triage_step_uid("app")))
        .unwrap()
        .unwrap();
    assert_eq!(
        db.read(move |c| containment::parent(c, f)).unwrap(),
        Some(inbox),
        "shelved"
    );
    assert_eq!(found_in(&db, f), [t], "after shelving");

    let removed = db
        .write_txn("t", move |c, m| {
            item::remove(c, m, f, true)?;
            Ok(m.txn_id)
        })
        .unwrap();
    assert!(db
        .read(|c| Ok(c.query_row(
            "SELECT count(*) FROM edges WHERE type = 'found_in'",
            [],
            |r| r.get::<_, i64>(0)
        )?))
        .unwrap()
        .eq(&0));
    db.write_txn("t", move |c, m| crate::undo::undo(c, m, removed))
        .unwrap();
    assert_eq!(found_in(&db, f), [t], "restored with the finding");
}

/// Test 6: no investigation strategy lists `found_in`, and an investigation link or unit refuses it:
/// the edge means "a review filed this", and only the review seam writes one.
#[test]
fn no_strategy_lists_found_in_and_an_investigation_link_refuses_it() {
    for name in crate::nstype::STRATEGIES {
        let s = crate::nstype::resolve_strategy(name).unwrap();
        assert!(
            !s.edge_types().contains(&EdgeType::FoundIn),
            "{} lists found_in",
            s.name()
        );
    }
    let db = db();
    let (_, steps) = planned(&db, "Inv", 1);
    task_under(&db, "task:a", steps[0]);
    task_under(&db, "task:b", steps[0]);
    let e = db
        .write_txn("t", |c, m| {
            crate::investigation::link(c, m, "task:a", EdgeType::FoundIn, "task:b", None)
        })
        .unwrap_err();
    assert!(e.to_string().contains("only by a review"), "{e}");
    db.write_txn("t", |c, m| {
        crate::investigation::link(c, m, "task:a", EdgeType::DiscoveredFrom, "task:b", None)
    })
    .unwrap();
}

/// What the gates and counts read, from one database.
#[derive(Debug, PartialEq, Eq)]
struct Gates {
    ready: Vec<ItemId>,
    is_ready: Vec<ItemId>,
    blocked: Vec<bool>,
    open_subtasks: Vec<Fact>,
    unplanned: Vec<String>,
    steps: BTreeMap<i64, (bool, bool)>,
}

fn gates(db: &Db, tasks: &[ItemId], steps: &[ItemId]) -> Gates {
    let tasks = tasks.to_vec();
    let steps = steps.to_vec();
    let mut ready = ready_ids(db);
    ready.sort();
    let mut is_ready = is_ready(db);
    is_ready.sort();
    db.read(move |c| {
        let mut blocked = Vec::new();
        let mut open_subtasks = Vec::new();
        for &t in &tasks {
            blocked.push(task::is_blocked(c, t)?);
            open_subtasks.push(task::observe(c, t)?.open_subtasks);
        }
        let unplanned = plan::unplanned(c)?.into_iter().map(|t| t.uid).collect();
        let mut by_step = BTreeMap::new();
        for &s in &steps {
            let n = plan::step_tasks(c, s)?;
            by_step.insert(s.get(), (n.implemented(), n.finished()));
        }
        Ok(Gates {
            ready,
            is_ready,
            blocked,
            open_subtasks,
            unplanned,
            steps: by_step,
        })
    })
    .unwrap()
}

/// One random task: where it goes (a step, or a task made before it, or loose), its status, and an
/// optional dependency on an earlier task.
#[derive(Debug, Clone)]
struct Spec {
    under: usize,
    status: usize,
    dep: Option<usize>,
}

const STATUSES: [TaskStatus; 4] = [
    TaskStatus::Open,
    TaskStatus::InProgress,
    TaskStatus::Done,
    TaskStatus::Cancelled,
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Any `found_in` edges between any tasks change nothing a gate or count reads: the ready
    /// frontier (both forms), blocking, open subtasks, the unplanned set and every step's count are
    /// the same with the edges present as without.
    #[test]
    fn found_in_edges_change_no_gate_or_count(
        specs in proptest::collection::vec(
            (0usize..8, 0usize..STATUSES.len(), proptest::option::of(0usize..12))
                .prop_map(|(under, status, dep)| Spec { under, status, dep }),
            1..12,
        ),
        links in proptest::collection::vec((0usize..12, 0usize..12), 0..16),
    ) {
        let db = db();
        let (_, steps) = planned(&db, "Prop", 2);
        let mut tasks: Vec<ItemId> = Vec::new();
        // Only a task under a step may hold a subtask (`homes_tasks`); a loose one may not.
        let mut planned_tasks: Vec<ItemId> = Vec::new();
        for (i, s) in specs.iter().enumerate() {
            // 0–1: a step; 2–5: an earlier task (else step 0); 6–7: loose.
            let uid = format!("task:p{i}");
            let id = match s.under {
                0 | 1 => task_under(&db, &uid, steps[s.under]),
                2..=5 if !planned_tasks.is_empty() => {
                    task_under(&db, &uid, planned_tasks[s.under % planned_tasks.len()])
                }
                2..=5 => task_under(&db, &uid, steps[0]),
                _ => {
                    let uid = uid.clone();
                    db.write_txn("t", move |c, m| {
                        let t = task::create(c, m, &NewTask::new(uid, "loose", Under::inbox_of("app")))?;
                        task::make_loose(c, m, t)?;
                        Ok(t)
                    })
                    .unwrap()
                }
            };
            if let Some(d) = s.dep.filter(|&d| d < tasks.len()) {
                let dst = tasks[d];
                db.write_txn("t", move |c, m| edge::link(c, m, id, dst, EdgeType::DependsOn, None).map(|_| ()))
                    .unwrap();
            }
            if s.under < 6 {
                planned_tasks.push(id);
            }
            tasks.push(id);
        }
        for (i, s) in specs.iter().enumerate() {
            if STATUSES[s.status] != TaskStatus::Open {
                set_status(&db, tasks[i], STATUSES[s.status]);
            }
        }
        let before = gates(&db, &tasks, &steps);
        let n = tasks.len();
        let pairs: Vec<(ItemId, ItemId)> = links
            .iter()
            .map(|&(a, b)| (tasks[a % n], tasks[b % n]))
            .collect();
        db.write_txn("t", move |c, m| {
            for (f, t) in pairs {
                reviews::link_findings(c, m, t, &[f])?;
            }
            Ok(())
        })
        .unwrap();
        let after = gates(&db, &tasks, &steps);
        prop_assert_eq!(before, after);
        let edges: i64 = db
            .read(|c| Ok(c.query_row(
                "SELECT count(*) FROM edges WHERE type = ?1",
                params![EdgeType::FoundIn.as_str()],
                |r| r.get(0),
            )?))
            .unwrap();
        prop_assert!(links.iter().all(|&(a, b)| a % n == b % n) || edges > 0);
    }
}
