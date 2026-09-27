//! Pull request facts and the merged close, through the backend.

use serde_json::json;

use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};
use jkb_core::Db;

fn call(b: &LocalBackend, r: serde_json::Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

fn facts(b: &LocalBackend, uid: &str) -> super::PrFacts {
    match call(b, json!({ "op": "task.pr_facts", "uid": uid })).unwrap() {
        Response::PrFacts { facts } => facts,
        other => panic!("{other:?}"),
    }
}

fn status(b: &LocalBackend, uid: &str) -> String {
    match call(b, json!({ "op": "task.show", "uid": uid })).unwrap() {
        Response::Task { task, .. } => task.item.status.unwrap_or_default(),
        other => panic!("{other:?}"),
    }
}

/// A recorded number is read back beside the branch; a merge closes the task only when the client
/// established it, and a dry run writes nothing.
#[test]
fn a_task_closes_on_a_merge_its_client_established() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    let uid = match call(
        &b,
        json!({ "op": "task.add", "text": "the work", "managed": true }),
    )
    .unwrap()
    {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    call(
        &b,
        json!({ "op": "task.start", "uid": uid, "take": { "owner": "box:1" },
                "place": { "branch": "feat", "repo": "proj" } }),
    )
    .unwrap();
    let before = facts(&b, &uid);
    assert_eq!((before.pr, before.branch.as_deref()), (None, Some("feat")));
    assert!(before.writable);
    assert!(!before.live_landing);

    call(
        &b,
        json!({ "op": "task.pr_record", "uid": uid, "number": 31 }),
    )
    .unwrap();
    assert_eq!(facts(&b, &uid).pr, Some(31));
    let e = call(
        &b,
        json!({ "op": "task.pr_record", "uid": uid, "number": 0 }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);

    match call(&b, json!({ "op": "task.open_in_repo", "repo": "proj" })).unwrap() {
        Response::Uids { uids, .. } => assert_eq!(uids, vec![uid.clone()]),
        other => panic!("{other:?}"),
    }

    let close = |merged: &str, dry_run: bool| match call(
        &b,
        json!({ "op": "task.close_merged", "uid": uid, "merged": merged, "pr": 31,
                "dry_run": dry_run,
                "observed": { "live_landing": false, "pr": 31 } }),
    )
    .unwrap()
    {
        Response::Closed { refusal } => refusal,
        other => panic!("{other:?}"),
    };
    assert!(
        close("unknown", false).is_some(),
        "an unestablished merge holds"
    );
    assert_eq!(status(&b, &uid), "in_progress");
    assert_eq!(close("yes", true), None, "the dry run would close it");
    assert_eq!(status(&b, &uid), "in_progress", "and wrote nothing");
    // A history that moved since the client read it holds the task.
    let e = match call(
        &b,
        json!({ "op": "task.close_merged", "uid": uid, "merged": "yes", "pr": 31,
                "observed": { "live_landing": true } }),
    )
    .unwrap()
    {
        Response::Closed { refusal } => refusal,
        other => panic!("{other:?}"),
    };
    assert!(e.is_some_and(|r| r.contains("history changed")));
    assert_eq!(status(&b, &uid), "in_progress");
    let token = match call(
        &b,
        json!({ "op": "role.grant", "role": "implementer", "task": uid, "agent": "w" }),
    )
    .unwrap()
    {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    assert_eq!(close("yes", false), None);
    assert_eq!(status(&b, &uid), "done");
    // A pull request merging is a landing: the workflow parks, and the task's workers are done
    // (review round 5 — a PR close has no destination, and parked nothing).
    match call(&b, json!({ "op": "workflow.show", "uid": uid })).unwrap() {
        Response::Workflow { workflow } => assert_eq!(workflow.phase, "landed"),
        other => panic!("{other:?}"),
    }
    // And the task's facts say what landed it, which has no destination (review round 10).
    match call(&b, json!({ "op": "task.facts", "uid": uid })).unwrap() {
        Response::TaskState { state } => assert_eq!(
            state.landed.as_deref(),
            Some(&crate::sessions::LiveLanding::Merged { pr: 31 })
        ),
        other => panic!("{other:?}"),
    }
    let worker = LocalBackend::new(db).with_caller(crate::rbac::Caller::Token(token));
    let e = call(&worker, json!({ "op": "role.whoami" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthorized, "{e:?}");
    match call(&b, json!({ "op": "task.open_in_repo", "repo": "proj" })).unwrap() {
        Response::Uids { uids, .. } => assert!(uids.is_empty()),
        other => panic!("{other:?}"),
    }
}

/// The close is held when the task was put back to work, or its pull request renamed, after the client
/// read its history — and goes ahead on the history as it now is.
#[test]
fn a_close_is_held_when_the_history_moved() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = match call(
        &b,
        json!({ "op": "task.add", "text": "w", "managed": true }),
    )
    .unwrap()
    {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    for status in ["in_progress", "needs_review", "in_progress"] {
        call(
            &b,
            json!({ "op": "task.set", "uid": uid, "status": status }),
        )
        .unwrap();
    }
    call(
        &b,
        json!({ "op": "task.pr_record", "uid": uid, "number": 45 }),
    )
    .unwrap();
    let now = facts(&b, &uid);
    assert!(now.resumed_at.is_some());
    let close = |observed: serde_json::Value| match call(
        &b,
        json!({ "op": "task.close_merged", "uid": uid, "merged": "yes", "pr": 45,
                "dry_run": true, "observed": observed }),
    )
    .unwrap()
    {
        Response::Closed { refusal } => refusal,
        other => panic!("{other:?}"),
    };
    let stale_resume = close(json!({ "live_landing": false, "pr": 45 }));
    assert!(stale_resume.is_some_and(|r| r.contains("history changed")));
    let stale_pr = close(json!({ "live_landing": false, "pr": 31, "resumed_at": now.resumed_at }));
    assert!(stale_pr.is_some_and(|r| r.contains("history changed")));
    assert_eq!(
        close(json!({ "live_landing": false, "pr": 45, "resumed_at": now.resumed_at })),
        None
    );
}

fn landed(b: &LocalBackend, uid: &str) -> Option<crate::sessions::LiveLanding> {
    match call(b, json!({ "op": "task.facts", "uid": uid })).unwrap() {
        Response::TaskState { state } => state.landed.map(|l| *l),
        other => panic!("{other:?}"),
    }
}

/// The landing that speaks now is the newest one still standing: a PR merge after a held graft is
/// what closed the task, and nothing speaks once the task is put back to work — ticking it done
/// again is not a landing (review round 11).
#[test]
fn the_landing_that_speaks_is_the_newest_one_still_standing() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let add = |text: &str, under: Option<&str>| match call(
        &b,
        json!({ "op": "task.add", "text": text, "managed": true, "under": under }),
    )
    .unwrap()
    {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    let uid = add("the work", None);
    let child = add("a subtask", Some(&uid));
    call(
        &b,
        json!({ "op": "task.start", "uid": uid, "take": { "owner": "box:1" },
                "place": { "branch": "feat", "repo": "proj" } }),
    )
    .unwrap();
    // Grafted while a subtask is open: held, but the row is live.
    match call(
        &b,
        json!({ "op": "task.landed", "uid": uid,
                "landed": { "branch": "feat", "onto": "staging" } }),
    )
    .unwrap()
    {
        Response::Landing { landing } => assert!(landing.refusal.is_some(), "{landing:?}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        landed(&b, &uid),
        Some(crate::sessions::LiveLanding::Grafted {
            branch: "feat".into(),
            onto: "staging".into()
        })
    );
    call(
        &b,
        json!({ "op": "task.set", "uid": child, "status": "done" }),
    )
    .unwrap();
    call(
        &b,
        json!({ "op": "task.pr_record", "uid": uid, "number": 31 }),
    )
    .unwrap();
    let live = facts(&b, &uid).live_landing;
    match call(
        &b,
        json!({ "op": "task.close_merged", "uid": uid, "merged": "yes", "pr": 31,
                "observed": { "live_landing": live, "pr": 31 } }),
    )
    .unwrap()
    {
        Response::Closed { refusal } => assert_eq!(refusal, None),
        other => panic!("{other:?}"),
    }
    assert_eq!(status(&b, &uid), "done");
    assert_eq!(
        landed(&b, &uid),
        Some(crate::sessions::LiveLanding::Merged { pr: 31 }),
        "the merge that closed it, not the held graft before it"
    );
    // Put back to work, then ticked done: neither landing speaks.
    for s in ["open", "done"] {
        call(&b, json!({ "op": "task.set", "uid": uid, "status": s })).unwrap();
    }
    assert_eq!(landed(&b, &uid), None);
}
