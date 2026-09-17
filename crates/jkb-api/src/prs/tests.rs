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
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
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
        Response::Uids { uids } => assert_eq!(uids, vec![uid.clone()]),
        other => panic!("{other:?}"),
    }

    let close = |merged: &str, dry_run: bool| match call(
        &b,
        json!({ "op": "task.close_merged", "uid": uid, "merged": merged, "pr": 31,
                "dry_run": dry_run }),
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
    assert_eq!(close("yes", false), None);
    assert_eq!(status(&b, &uid), "done");
    match call(&b, json!({ "op": "task.open_in_repo", "repo": "proj" })).unwrap() {
        Response::Uids { uids } => assert!(uids.is_empty()),
        other => panic!("{other:?}"),
    }
}
