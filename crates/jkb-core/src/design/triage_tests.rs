//! Every new task is filed under a plan step: the guard in the callee ([`plan::homes_tasks`]), the
//! default step ([`plan::step_for_home`]) and each repo's Open work › Inbox ([`plan::triage_step`]).

use proptest::prelude::*;
use rusqlite::params;

use jkb_types::{ItemId, TaskStatus};

use super::plan;
use crate::task::{self, NewTask, Under};
use crate::{containment, investigation, item, ns, Db};

fn db() -> Db {
    Db::open_in_memory().unwrap()
}

/// A design in repo `app` with a one-step plan: `(design uid, plan uid, step id)`.
fn planned(db: &Db) -> (String, String, ItemId) {
    db.write_txn("t", |c, m| {
        let d = super::create(c, m, "app", "Factory", "Build it.")?.uid;
        let p = plan::create(c, m, &d, "Ship it", &["one".to_owned()])?;
        let step = item::id_for_uid(c, &p.steps[0].uid)?.unwrap();
        Ok((d, p.uid, step))
    })
    .unwrap()
}

fn id(db: &Db, uid: &str) -> ItemId {
    let uid = uid.to_owned();
    db.read(move |c| item::id_for_uid(c, &uid))
        .unwrap()
        .unwrap()
}

fn homes(db: &Db, parent: ItemId) -> bool {
    db.read(move |c| plan::homes_tasks(c, parent)).unwrap()
}

/// A task left with no container, as a build before the guard could leave one.
fn loose(db: &Db, uid: &str) -> ItemId {
    let uid = uid.to_owned();
    db.write_txn("t", move |c, m| {
        let t = task::create(c, m, &NewTask::new(uid, "loose", Under::inbox_of("t")))?;
        task::make_loose(c, m, t)?;
        Ok(t)
    })
    .unwrap()
}

/// A new task under exactly `parent`.
fn create_under(db: &Db, uid: &str, parent: ItemId) -> crate::Result<ItemId> {
    let uid = uid.to_owned();
    db.write_txn("t", move |c, m| {
        task::create(c, m, &NewTask::new(uid, "t", Under::Parent(parent)))
    })
}

/// Contain `child` under `parent` with the raw writer: what the guards refuse, built directly.
fn contain_raw(db: &Db, child: ItemId, parent: ItemId) {
    db.write_txn("t", move |c, m| {
        containment::contain(c, m, child, parent, 0)
    })
    .unwrap();
}

fn unplanned(db: &Db) -> Vec<String> {
    db.read(plan::unplanned)
        .unwrap()
        .into_iter()
        .map(|t| t.uid)
        .collect()
}

fn changelog_rows(db: &Db) -> i64 {
    db.read(|c| Ok(c.query_row("SELECT count(*) FROM changelog", [], |r| r.get(0))?))
        .unwrap()
}

/// Test 1: a step, and a task the every-step walk reaches (through a subtask, or through a non-task
/// item under a step), holds tasks; a plan, a span, a design, a loose task and a task under a plan
/// directly do not.
#[test]
fn only_a_step_or_a_task_under_one_holds_tasks() {
    let db = db();
    let (d, p, step) = planned(&db);
    let t1 = create_under(&db, "task:t1", step).unwrap();
    let t2 = create_under(&db, "task:t2", t1).unwrap();
    let note = db
        .write_txn("t", |c, m| {
            item::upsert(
                c,
                m,
                &item::NewItem {
                    uid: "note:n".to_owned(),
                    kind: "note".to_owned(),
                    content: None,
                    content_hash: None,
                    mime: None,
                },
            )
        })
        .unwrap();
    contain_raw(&db, note, step);
    let via_note = loose(&db, "task:via-note");
    contain_raw(&db, via_note, note);
    let span = {
        let d = d.clone();
        db.write_txn("t", move |c, m| {
            let version = super::read(c, &d)?.version.token();
            let span = super::add_span(
                c,
                m,
                &d,
                &version,
                "Build it.",
                None,
                super::Reviewer::Operator,
            )?
            .span
            .unwrap();
            Ok(item::id_for_uid(c, &span)?.unwrap())
        })
        .unwrap()
    };
    let lone = loose(&db, "task:lone");
    let under_plan = loose(&db, "task:under-plan");
    let plan_id = id(&db, &p);
    contain_raw(&db, under_plan, plan_id);

    for yes in [step, t1, t2, via_note] {
        assert!(homes(&db, yes), "{yes}");
    }
    for no in [plan_id, span, id(&db, &d), lone, under_plan, note] {
        assert!(!homes(&db, no), "{no}");
    }
}

/// Test 2: a parent under no step is refused with the words every surface prints, and nothing of
/// the task is written.
#[test]
fn creating_under_an_unplanned_parent_is_refused_and_writes_nothing() {
    let db = db();
    let lone = loose(&db, "task:lone");
    let before = changelog_rows(&db);
    let e = create_under(&db, "task:new", lone).unwrap_err().to_string();
    assert!(e.contains("`--under <step>`"), "{e}");
    assert!(e.contains("jkb design plan ls"), "{e}");
    assert!(e.contains("jkb task move task:lone --under <step>"), "{e}");
    assert!(db
        .read(|c| item::id_for_uid(c, "task:new"))
        .unwrap()
        .is_none());
    assert_eq!(changelog_rows(&db), before);
}

fn default_create(db: &Db, uid: &str, home: &str, ambient: Option<&str>) -> crate::Result<ItemId> {
    let (uid, home, ambient) = (uid.to_owned(), home.to_owned(), ambient.map(str::to_owned));
    db.write_txn("t", move |c, m| {
        let mut t = NewTask::new(
            uid,
            "t",
            Under::Default {
                ambient_repo: ambient,
            },
        );
        t.home = home;
        task::create(c, m, &t)
    })
}

fn parent_uid(db: &Db, child: ItemId) -> Option<String> {
    db.read(move |c| {
        Ok(match containment::parent(c, child)? {
            Some(p) => item::get(c, p)?.map(|m| m.uid),
            None => None,
        })
    })
    .unwrap()
}

fn uid_rows(db: &Db, uid: &str) -> i64 {
    let uid = uid.to_owned();
    db.read(move |c| {
        Ok(
            c.query_row("SELECT count(*) FROM items WHERE uid = ?1", [uid], |r| {
                r.get(0)
            })?,
        )
    })
    .unwrap()
}

/// Test 3: a task with no step of its own goes to the Open work › Inbox of the repo its home names,
/// else of the ambient repo; the tree is made once and found by uid after; with no repo, refused.
#[test]
fn the_default_is_the_repos_open_work_inbox_made_once() {
    let db = db();
    let a = default_create(&db, "task:a", "tasks/repos/app/inbox", None).unwrap();
    let b = default_create(&db, "task:b", "tasks/repos/app/.backlog", None).unwrap();
    for t in [a, b] {
        assert_eq!(parent_uid(&db, t).as_deref(), Some("step:inbox@app"));
    }
    for uid in [
        "design:open-work@app",
        "plan:open-work@app",
        "step:inbox@app",
    ] {
        assert_eq!(uid_rows(&db, uid), 1, "{uid}");
    }
    let step = id(&db, "step:inbox@app");
    assert_eq!(parent_uid(&db, step).as_deref(), Some("plan:open-work@app"));
    let plan_id = id(&db, "plan:open-work@app");
    assert_eq!(
        parent_uid(&db, plan_id).as_deref(),
        Some("design:open-work@app")
    );
    // Listed with the repo's designs, titled for what it is.
    let designs = db.read(|c| super::list(c, Some("app"), false)).unwrap();
    assert!(designs
        .iter()
        .any(|d| d.uid == "design:open-work@app" && d.title == "Open work"));

    // A home naming no repo takes the ambient one.
    let c = default_create(&db, "task:c", "projects/alpha", Some("app")).unwrap();
    assert_eq!(parent_uid(&db, c).as_deref(), Some("step:inbox@app"));
    // A home naming one wins over the ambient one.
    let d = default_create(&db, "task:d", "repos/web/notes", Some("app")).unwrap();
    assert_eq!(parent_uid(&db, d).as_deref(), Some("step:inbox@web"));
    // Neither: refused, and nothing written.
    let e = default_create(&db, "task:e", "projects/alpha", None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("outside a mounted repo"), "{e}");
    assert!(e.contains("`--under <step>`"), "{e}");
    assert_eq!(uid_rows(&db, "task:e"), 0);
}

/// Test 4: a namespace mapped to a step wins over the Inbox for everything homed at or below it; a
/// mapping to a uid that is gone is refused by name; mapping keeps the namespace's other keys.
#[test]
fn a_mapped_namespace_wins_and_a_stale_mapping_is_named() {
    let db = db();
    let (_, _, step) = planned(&db);
    let step_uid = db
        .read(move |c| Ok(item::get(c, step)?.unwrap().uid))
        .unwrap();
    db.write_txn("t", |c, m| {
        let id = ns::ensure(c, m, "tasks/repos/app")?;
        ns::set_type(c, m, id, "tasks")
    })
    .unwrap();
    let s = step_uid.clone();
    let before = db
        .write_txn("t", move |c, m| {
            plan::map_namespace(c, m, "tasks/repos/app", Some(&s))
        })
        .unwrap();
    assert_eq!(before, None);
    assert_eq!(
        db.read(|c| ns::get_type(c, "tasks/repos/app"))
            .unwrap()
            .as_deref(),
        Some("tasks"),
        "the type key survives the mapping"
    );
    let t = default_create(&db, "task:mapped", "tasks/repos/app/inbox", None).unwrap();
    assert_eq!(parent_uid(&db, t).as_deref(), Some(step_uid.as_str()));
    assert_eq!(uid_rows(&db, "step:inbox@app"), 0, "no Inbox was needed");

    // Only a step can be mapped.
    let e = db
        .write_txn("t", |c, m| {
            plan::map_namespace(c, m, "tasks/repos/app", Some("task:mapped"))
        })
        .unwrap_err()
        .to_string();
    assert!(e.contains("not a plan step"), "{e}");

    // A mapping whose step is gone (written raw) is refused, naming how to clear it.
    db.write_txn("t", |c, m| {
        let id = ns::get(c, "tasks/repos/app")?.unwrap();
        let mut md = ns::get_metadata(c, id)?.unwrap();
        md["plan_step"] = serde_json::json!("step:gone");
        ns::set_metadata(c, m, id, &md)
    })
    .unwrap();
    let e = default_create(&db, "task:stale", "tasks/repos/app/inbox", None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("`step:gone`, which no longer exists"), "{e}");
    assert!(e.contains("jkb ns step tasks/repos/app --clear"), "{e}");

    // Cleared, the Inbox is the default again.
    let before = db
        .write_txn("t", |c, m| {
            plan::map_namespace(c, m, "tasks/repos/app", None)
        })
        .unwrap();
    assert_eq!(before.as_deref(), Some("step:gone"));
    let t = default_create(&db, "task:after", "tasks/repos/app/inbox", None).unwrap();
    assert_eq!(parent_uid(&db, t).as_deref(), Some("step:inbox@app"));
}

/// Test 5: every writer of a task's container refuses an unplanned parent; a loose task moves under
/// a step, which is how it is planned.
#[test]
fn every_container_writer_refuses_an_unplanned_parent() {
    let db = db();
    let (d, _, step) = planned(&db);
    let child = create_under(&db, "task:child", step).unwrap();
    let lone = loose(&db, "task:lone");
    let design = id(&db, &d);
    for parent in [lone, design] {
        let e = db
            .write_txn("t", move |c, m| task::add_subtask(c, m, parent, child))
            .unwrap_err();
        assert!(e.to_string().contains("under no plan step"), "{e}");
        let e = db
            .write_txn("t", move |c, m| task::set_container(c, m, child, parent, 0))
            .unwrap_err();
        assert!(e.to_string().contains("under no plan step"), "{e}");
        let e = db
            .write_txn("t", move |c, m| task::move_under(c, m, child, parent))
            .unwrap_err();
        assert!(e.to_string().contains("under no plan step"), "{e}");
        assert_eq!(
            db.read(move |c| containment::parent(c, child)).unwrap(),
            Some(step)
        );
    }
    // The loose task is planned by a move, after which it holds tasks.
    db.write_txn("t", move |c, m| task::move_under(c, m, lone, step))
        .unwrap();
    assert!(homes(&db, lone));
    assert!(unplanned(&db).is_empty());
}

/// Test 6: no other way to write a task row is left open — `item::upsert` and an investigation's
/// `add` refuse a task — and a fixture built through every creator leaves nothing unplanned.
#[test]
fn the_bypasses_are_closed() {
    let db = db();
    let e = db
        .write_txn("t", |c, m| {
            item::upsert(
                c,
                m,
                &item::NewItem {
                    uid: "task:raw".to_owned(),
                    kind: "task".to_owned(),
                    content: None,
                    content_hash: None,
                    mime: None,
                },
            )
        })
        .unwrap_err()
        .to_string();
    assert!(e.contains("`task::create`"), "{e}");
    let e = db
        .write_txn("t", |c, m| {
            investigation::add(
                c,
                m,
                &investigation::NewUnit::new("task", "fix it", "inv/bug"),
            )
        })
        .unwrap_err()
        .to_string();
    assert!(e.contains("jkb task add … --under <step> +inv/bug"), "{e}");

    let (_, _, step) = planned(&db);
    let a = create_under(&db, "task:a", step).unwrap();
    create_under(&db, "task:b", a).unwrap();
    default_create(&db, "task:c", "tasks/repos/app/inbox", None).unwrap();
    default_create(&db, "task:d", "notes", Some("app")).unwrap();
    let e = create_under(&db, "task:e", step).unwrap();
    db.write_txn("t", move |c, m| task::move_under(c, m, e, a))
        .unwrap();
    assert!(unplanned(&db).is_empty(), "{:?}", unplanned(&db));
}

fn concurrent_triage(dbs: [Db; 2]) {
    let handles: Vec<_> = dbs
        .into_iter()
        .map(|db| {
            std::thread::spawn(move || {
                db.write_txn("t", |c, m| plan::triage_step(c, m, "app"))
                    .unwrap()
            })
        })
        .collect();
    let ids: Vec<ItemId> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(ids[0], ids[1]);
}

/// Test 7: two creators at once — two threads on one handle, and two handles on one file, as two
/// processes would be — get the same step, and the tree exists once.
#[test]
fn two_concurrent_creators_share_one_triage_tree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jkb.db");
    let db = Db::open(&path).unwrap();
    concurrent_triage([db.clone(), db.clone()]);
    for uid in [
        "design:open-work@app",
        "plan:open-work@app",
        "step:inbox@app",
    ] {
        assert_eq!(uid_rows(&db, uid), 1, "{uid}");
    }
    drop(db);

    let path2 = dir.path().join("two.db");
    let first = Db::open(&path2).unwrap();
    let second = Db::open(&path2).unwrap();
    concurrent_triage([first.clone(), second]);
    for uid in [
        "design:open-work@app",
        "plan:open-work@app",
        "step:inbox@app",
    ] {
        assert_eq!(uid_rows(&first, uid), 1, "{uid}");
    }
}

/// Test 8: a triage uid that names the wrong kind, or sits in the wrong place, is refused rather
/// than built beside; the Open work design is never retired.
#[test]
fn a_mismatched_triage_tree_is_refused_and_open_work_is_never_retired() {
    let db = db();
    db.write_txn("t", |c, m| {
        let d = plan::triage_design_uid("app");
        super::create_as(c, m, &d, "app", "Open work", "")?;
        let design = super::design_id(c, &d)?;
        let plan_uid = plan::triage_plan_uid("app");
        let p = item::upsert(
            c,
            m,
            &item::NewItem {
                uid: plan_uid,
                kind: plan::PLAN_KIND.to_owned(),
                content: None,
                content_hash: None,
                mime: None,
            },
        )?;
        containment::contain(c, m, p, design, 0)?;
        item::upsert(
            c,
            m,
            &item::NewItem {
                uid: plan::triage_step_uid("app"),
                kind: "note".to_owned(),
                content: None,
                content_hash: None,
                mime: None,
            },
        )?;
        Ok(())
    })
    .unwrap();
    let e = db
        .write_txn("t", |c, m| plan::triage_step(c, m, "app"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("`step:inbox@app` is a note"), "{e}");

    // A real step, but under another plan.
    let db = db_with_inbox("web");
    let (d, _, _) = planned(&db);
    let other = db
        .write_txn("t", move |c, m| {
            let p = plan::create(c, m, &d, "Elsewhere", &["x".to_owned()])?;
            item::id_for_uid(c, &p.uid)
        })
        .unwrap()
        .unwrap();
    let inbox = id(&db, "step:inbox@web");
    contain_raw(&db, inbox, other);
    let e = db
        .write_txn("t", |c, m| plan::triage_step(c, m, "web"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("`step:inbox@web` is not where"), "{e}");

    let e = db
        .write_txn("t", |c, m| super::retire(c, m, "design:open-work@web"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("never retired"), "{e}");
}

fn db_with_inbox(repo: &'static str) -> Db {
    let db = db();
    db.write_txn("t", move |c, m| plan::triage_step(c, m, repo))
        .unwrap();
    db
}

/// Test 10: a loose task from before the guard still reads, is still listed as unplanned, and still
/// changes status — only its container's writes are held to the rule.
#[test]
fn a_loose_task_from_before_still_reads_and_changes_status() {
    let db = db();
    let lone = loose(&db, "task:lone");
    assert_eq!(unplanned(&db), ["task:lone"]);
    let meta = db.read(move |c| item::get(c, lone)).unwrap().unwrap();
    assert_eq!(meta.kind, "task");
    db.write_txn("t", move |c, m| {
        task::set_status(c, m, lone, TaskStatus::InProgress)
    })
    .unwrap();
}

/// The kinds a random forest is built from.
const KINDS: [&str; 5] = ["plan_step", "exec_plan", "design", "task", "note"];

/// The upward oracle: `kind` is a step, or a task with a step among its proper ancestors.
fn oracle(kinds: &[usize], parents: &[Option<usize>], i: usize) -> bool {
    match KINDS[kinds[i]] {
        "plan_step" => true,
        "task" => {
            let mut at = parents[i];
            while let Some(p) = at {
                if KINDS[kinds[p]] == "plan_step" {
                    return true;
                }
                at = parents[p];
            }
            false
        }
        _ => false,
    }
}

fn forest() -> impl Strategy<Value = (Vec<usize>, Vec<Option<usize>>)> {
    (2usize..10).prop_flat_map(|n| {
        let kinds = proptest::collection::vec(0..KINDS.len(), n);
        let parents = (0..n)
            .map(|i| {
                if i == 0 {
                    Just(None).boxed()
                } else {
                    proptest::option::of(0..i).boxed()
                }
            })
            .collect::<Vec<_>>();
        (kinds, parents)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Test 9: over random containment forests built from raw rows, a new task under P is accepted
    /// exactly when `homes_tasks(P)` — which agrees with an independent upward walk — and every task
    /// it accepts is one the unplanned read counts as planned.
    #[test]
    fn the_guard_agrees_with_the_unplanned_read((kinds, parents) in forest()) {
        let db = db();
        let ids: Vec<ItemId> = {
            let (kinds, parents) = (kinds.clone(), parents.clone());
            db.write_txn("t", move |c, m| {
                let mut ids = Vec::new();
                for (i, &k) in kinds.iter().enumerate() {
                    let status = (KINDS[k] == "task").then_some("open");
                    let id: i64 = c.query_row(
                        "INSERT INTO items (uid, kind, status) VALUES (?1, ?2, ?3) RETURNING id",
                        params![format!("x:{i}"), KINDS[k], status],
                        |r| r.get(0),
                    )?;
                    ids.push(ItemId::new(id));
                }
                for (i, p) in parents.iter().enumerate() {
                    if let Some(p) = p {
                        containment::contain(c, m, ids[i], ids[*p], 0)?;
                    }
                }
                Ok(ids)
            })
            .unwrap()
        };
        for (i, &p) in ids.iter().enumerate() {
            let expected = oracle(&kinds, &parents, i);
            prop_assert_eq!(homes(&db, p), expected, "item {} ({})", i, KINDS[kinds[i]]);
            let uid = format!("task:new-{i}");
            let made = create_under(&db, &uid, p);
            prop_assert_eq!(made.is_ok(), expected, "item {}: {:?}", i, made.err());
            if expected {
                prop_assert!(!unplanned(&db).contains(&uid));
            }
        }
    }
}
