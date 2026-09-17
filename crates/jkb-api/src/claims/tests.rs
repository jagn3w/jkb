//! Crash recovery through the backend: the op frees only what the client proved gone and could have.

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

fn claim(b: &LocalBackend, uid: &str, owner: &str) {
    match call(b, json!({ "op": "task.claim", "uid": uid, "owner": owner })).unwrap() {
        Response::Claimed { claimed } => assert!(claimed.acquired, "{claimed:?}"),
        other => panic!("{other:?}"),
    }
}

fn held(b: &LocalBackend) -> Vec<(String, String)> {
    match call(b, json!({ "op": "task.claims" })).unwrap() {
        Response::Claims { claims, truncated } => {
            assert!(!truncated);
            claims.into_iter().map(|c| (c.uid, c.owner)).collect()
        }
        other => panic!("{other:?}"),
    }
}

fn reclaim(b: &LocalBackend, dead: &[&str]) -> super::Reclaimed {
    match call(b, json!({ "op": "task.reclaim", "dead": dead })).unwrap() {
        Response::Reclaimed { reclaimed } => reclaimed,
        other => panic!("{other:?}"),
    }
}

fn status(b: &LocalBackend, uid: &str) -> String {
    match call(b, json!({ "op": "task.show", "uid": uid })).unwrap() {
        Response::Task { task, .. } => task.item.status.unwrap_or_default(),
        other => panic!("{other:?}"),
    }
}

/// The owners named are freed through the lifecycle; an owner not named, and one nobody can prove
/// gone, keep their claims.
#[test]
fn a_reclaim_frees_exactly_the_owners_named() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let (a, c, e) = (add(&b, "a"), add(&b, "c"), add(&b, "e"));
    claim(&b, &a, "box:10");
    claim(&b, &c, "box:11");
    claim(&b, &e, "agent:ext");
    assert_eq!(held(&b).len(), 3);

    let got = reclaim(&b, &["box:10", "agent:ext", "box:99"]);
    assert_eq!(
        got.cleared,
        vec![super::Claim {
            uid: a.clone(),
            owner: "box:10".to_owned()
        }]
    );
    assert_eq!(got.refused.len(), 1, "{got:?}");
    assert_eq!(got.refused[0].owner, "agent:ext");
    // Owner-gone only releases: the status is the work's, and stays.
    assert_eq!(status(&b, &a), "in_progress");
    assert_eq!(
        held(&b),
        vec![
            (c.clone(), "box:11".to_owned()),
            (e.clone(), "agent:ext".to_owned())
        ]
    );

    // A task taken again, by an owner under another string, is not freed by the earlier probe's answer.
    claim(&b, &a, "box:12");
    assert!(reclaim(&b, &["box:10"]).cleared.is_empty());
    assert!(held(&b).contains(&(a.clone(), "box:12".to_owned())));

    for bad in [
        json!([""]),
        json!(vec!["box:1"; super::MAX_DEAD_OWNERS + 1]),
    ] {
        let err = call(&b, json!({ "op": "task.reclaim", "dead": bad })).unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid, "{err:?}");
    }
}

/// A client of the daemon cannot have probed the daemon host's processes, or a checkout outside the
/// directory it shares, and may not free a task filed outside its roots.
#[test]
fn a_rooted_reclaim_takes_only_what_its_client_could_have_proved() {
    let (db, inside, outside, managed) = crate::tests::mutate_fixture();
    let host = LocalBackend::new(db.clone());
    let here = format!("{}:5", jkb_core::host::name());
    let shared = "session:5:~/repos/proj/.jkb/work/s";
    let private = "session:5:/Users/u/elsewhere/.jkb/work/s";
    let (h, s, p) = (add(&host, "h"), add(&host, "s"), add(&host, "p"));
    claim(&host, &h, &here);
    claim(&host, &s, shared);
    claim(&host, &p, private);
    claim(&host, &inside, "container:7");
    claim(&host, &outside, "container:7");
    claim(&host, &managed, "container:7");

    let rooted = crate::tests::rooted(&db);
    let got = reclaim(&rooted, &[&here, shared, private, "container:7"]);
    let mut cleared: Vec<&str> = got.cleared.iter().map(|c| c.uid.as_str()).collect();
    cleared.sort_unstable();
    let mut want = vec![s.as_str(), inside.as_str(), managed.as_str()];
    want.sort_unstable();
    assert_eq!(cleared, want);
    let mut refused: Vec<&str> = got.refused.iter().map(|r| r.owner.as_str()).collect();
    refused.sort_unstable();
    let mut want = vec![here.as_str(), private];
    want.sort_unstable();
    assert_eq!(refused, want);
    assert_eq!(
        got.unwritable,
        vec![super::Claim {
            uid: outside.clone(),
            owner: "container:7".to_owned()
        }]
    );

    // The same owners from the host itself are its own to judge.
    let got = reclaim(&host, &[&here, private, "container:7"]);
    assert_eq!(got.cleared.len(), 3, "{got:?}");
    assert!(got.refused.is_empty());
}
