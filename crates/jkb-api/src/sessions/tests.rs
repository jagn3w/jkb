//! The session ops through the backend, as the CLI drives them: each write is a compare-and-set on the
//! owner the caller judged, and a client under file roots is held to them like every task write.

use serde_json::json;

use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};
use jkb_core::Db;

fn call(b: &LocalBackend, r: serde_json::Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

fn add(b: &LocalBackend, text: &str) -> String {
    match call(
        b,
        json!({ "op": "task.add", "text": text, "managed": true }),
    )
    .unwrap()
    {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    }
}

fn facts(b: &LocalBackend, uid: &str) -> super::TaskState {
    match call(b, json!({ "op": "task.facts", "uid": uid })).unwrap() {
        Response::TaskState { state } => state,
        other => panic!("{other:?}"),
    }
}

fn taken(r: Response) -> bool {
    match r {
        Response::Taken { taken } => taken,
        other => panic!("{other:?}"),
    }
}

fn start(
    b: &LocalBackend,
    uid: &str,
    owner: &str,
    displace: Option<&str>,
) -> Result<bool, ApiError> {
    call(
        b,
        json!({ "op": "task.start", "uid": uid,
                "take": { "owner": owner, "displace": displace },
                "place": { "branch": "feat", "repo": "proj", "onto": "batch" } }),
    )
    .map(taken)
}

/// `task.start` takes the claim, sets the location facets and records the branch and land target in
/// the history — and `task.facts` reads all of it back in one answer.
#[test]
fn a_start_claims_and_records_where_the_work_is() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "do the thing +tasks/x");
    let before = facts(&b, &uid);
    assert_eq!(
        (before.status.as_str(), before.claim.as_deref()),
        ("open", None)
    );
    assert_eq!(before.start_refusal, None);

    assert!(start(&b, &uid, "host:1", None).unwrap());
    let after = facts(&b, &uid);
    assert_eq!(after.claim.as_deref(), Some("host:1"));
    assert_eq!(after.status, "in_progress");
    assert_eq!(
        after.tags.get("branch").map(Vec::as_slice),
        Some(&["feat".to_owned()][..])
    );
    assert_eq!(
        after.tags.get("repo").map(Vec::as_slice),
        Some(&["proj".to_owned()][..])
    );
    assert_eq!(after.land_target.as_deref(), Some("batch"));

    let Response::BranchTasks { tasks } =
        call(&b, json!({ "op": "task.by_branch", "repo": "proj" })).unwrap()
    else {
        panic!("expected tasks")
    };
    let t = &tasks["feat"];
    assert_eq!(
        (t.uid.as_str(), t.onto.as_deref()),
        (uid.as_str(), Some("batch"))
    );
    let Response::BranchTasks { tasks } =
        call(&b, json!({ "op": "task.by_branch", "repo": "other" })).unwrap()
    else {
        panic!("expected tasks")
    };
    assert!(tasks.is_empty());
}

/// **The compare-and-set.** A takeover names the owner the caller judged; if the claim moved since,
/// nothing is written — not the claim, not the facets.
#[test]
fn a_takeover_of_an_owner_that_changed_writes_nothing() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "contested +tasks/x");
    assert!(start(&b, &uid, "host:1", None).unwrap());
    // The caller judged `host:9` gone, but the claim is `host:1`'s.
    assert!(!start(&b, &uid, "host:2", Some("host:9")).unwrap());
    let f = facts(&b, &uid);
    assert_eq!(f.claim.as_deref(), Some("host:1"));
    // A takeover of the owner actually there succeeds.
    assert!(start(&b, &uid, "host:2", Some("host:1")).unwrap());
    assert_eq!(facts(&b, &uid).claim.as_deref(), Some("host:2"));
    // Kept: no claim change, only the location.
    let kept = call(
        &b,
        json!({ "op": "task.start", "uid": uid,
                "place": { "branch": "feat2", "repo": "proj" } }),
    )
    .map(taken)
    .unwrap();
    assert!(kept);
    let f = facts(&b, &uid);
    assert_eq!(f.claim.as_deref(), Some("host:2"));
    assert_eq!(f.tags["branch"], ["feat2"]);
}

/// `task.take` is `task work`'s claim: the start transition carries the branch and land target, and a
/// same-owner retake is not an error.
#[test]
fn a_take_claims_with_the_session_labels_and_is_idempotent() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "work it +tasks/x");
    let take = |displace: Option<&str>| {
        call(
            &b,
            json!({ "op": "task.take", "uid": uid,
                    "take": { "owner": "session:1:~/w", "displace": displace },
                    "branch": "task/w", "onto": "batch" }),
        )
        .map(taken)
    };
    assert!(take(None).unwrap());
    let f = facts(&b, &uid);
    assert_eq!(
        (f.claim.as_deref(), f.land_target.as_deref()),
        (Some("session:1:~/w"), Some("batch"))
    );
    assert!(
        take(Some("session:1:~/w")).unwrap(),
        "a resume re-takes its own claim"
    );
    call(
        &b,
        json!({ "op": "task.locate", "uid": uid,
                "place": { "branch": "task/w", "repo": "proj", "onto": "batch" } }),
    )
    .unwrap();
    assert_eq!(facts(&b, &uid).tags["repo"], ["proj"]);
}

/// A terminal task cannot be started: the lifecycle's refusal is what `task.facts` reports and what a
/// write returns.
#[test]
fn a_finished_task_is_not_started() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "finished +tasks/x");
    call(
        &b,
        json!({ "op": "task.set", "uid": uid, "status": "done" }),
    )
    .unwrap();
    let f = facts(&b, &uid);
    assert!(f.terminal);
    assert!(f.start_refusal.is_some());
    let e = start(&b, &uid, "host:1", None).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    assert_eq!(facts(&b, &uid).tags.get("branch"), None, "nothing written");
}

fn abandon(b: &LocalBackend, uid: &str, observed: Option<&str>) -> super::Abandoned {
    match call(
        b,
        json!({ "op": "task.abandon", "uid": uid, "observed": observed }),
    )
    .unwrap()
    {
        Response::Abandoned { abandoned } => abandoned,
        other => panic!("{other:?}"),
    }
}

/// `task.abandon` releases the claim it was told about and reopens — and changes nothing when the
/// claim is not the one the caller judged, including a claim that appeared after it looked.
#[test]
fn an_abandon_releases_only_the_judged_claim() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "drop it +tasks/x");
    assert!(start(&b, &uid, "host:1", None).unwrap());

    let a = abandon(&b, &uid, Some("host:9"));
    assert_eq!((a.reopened, a.status.as_str()), (false, "in_progress"));
    assert_eq!(facts(&b, &uid).claim.as_deref(), Some("host:1"));

    let a = abandon(&b, &uid, None);
    assert!(!a.reopened, "a claim nobody judged stays");

    let a = abandon(&b, &uid, Some("host:1"));
    assert_eq!((a.reopened, a.status.as_str()), (true, "open"));
    let f = facts(&b, &uid);
    assert_eq!((f.claim, f.land_target), (None, None));

    // A finished task is left finished.
    assert!(start(&b, &uid, "host:1", None).unwrap());
    call(
        &b,
        json!({ "op": "task.set", "uid": uid, "status": "done" }),
    )
    .unwrap();
    let a = abandon(&b, &uid, facts(&b, &uid).claim.as_deref());
    assert_eq!((a.reopened, a.status.as_str()), (false, "done"));
}

/// The gate is read through the daemon and never written: there is no op that stores one.
#[test]
fn the_gate_is_read_only() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    let gate = |repo: &str| match call(&b, json!({ "op": "repo.gate", "repo": repo })).unwrap() {
        Response::Gate { gate } => gate,
        other => panic!("{other:?}"),
    };
    assert_eq!(gate("proj"), None);
    db.write_txn("t", |c, m| {
        let id = jkb_core::ns::ensure(c, "repos/proj")?;
        jkb_core::ns::set_metadata(c, m, id, &json!({ "gate": "make test", "type": "repo" }))
    })
    .unwrap();
    assert_eq!(gate("proj").as_deref(), Some("make test"));
    assert!(
        crate::Request::OPS
            .iter()
            .all(|op| !op.contains("gate") || *op == "repo.gate"),
        "no op stores a gate"
    );
}

/// Every name a session op stores is bounded and free of control characters, and an owner is held to
/// the claim ops' own limit.
#[test]
fn malformed_session_fields_are_refused() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "bounded +tasks/x");
    let long = "b".repeat(super::MAX_NAME_BYTES + 1);
    for place in [
        json!({ "branch": "", "repo": "r" }),
        json!({ "branch": long, "repo": "r" }),
        json!({ "branch": "b\nx", "repo": "r" }),
        json!({ "branch": "b", "repo": "r", "onto": "" }),
    ] {
        let e = call(
            &b,
            json!({ "op": "task.locate", "uid": uid, "place": place }),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{place}: {e:?}");
    }
    let e = call(&b, json!({ "op": "task.by_branch", "repo": "" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    let e = start(
        &b,
        &uid,
        &"o".repeat(crate::tasks::MAX_OWNER_BYTES + 1),
        None,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    assert!(!facts(&b, &uid).tags.contains_key("branch"));
    let e = call(&b, json!({ "op": "task.facts", "uid": "task:nope" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

/// `session.state` answers from the registry: unknown, live, ended.
#[test]
fn a_session_state_is_read_from_the_registry() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let state = || match call(&b, json!({ "op": "session.state", "session": "s1" })).unwrap() {
        Response::SessionIs { state } => state,
        other => panic!("{other:?}"),
    };
    assert_eq!(state(), "unknown");
    call(
        &b,
        json!({ "op": "session.started", "session": "s1", "source": "startup",
                "pid": "1", "instance": "h" }),
    )
    .unwrap();
    assert_eq!(state(), "live");
    call(
        &b,
        json!({ "op": "session.ended", "session": "s1", "reason": "other",
                "pid": "1", "instance": "h" }),
    )
    .unwrap();
    assert_eq!(state(), "ended");
}
