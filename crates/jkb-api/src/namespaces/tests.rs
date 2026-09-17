//! Namespaces through the backend, and the file roots a move is held to.

use serde_json::json;

use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};

fn call(b: &LocalBackend, r: serde_json::Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

fn list(b: &LocalBackend, scope: Option<&str>) -> Vec<String> {
    match call(b, json!({ "op": "ns.list", "scope": scope })).unwrap() {
        Response::Namespaces { paths, .. } => paths,
        other => panic!("{other:?}"),
    }
}

/// A client moves what lies wholly inside its roots, and nothing that would have the host rewrite a
/// file outside them: a mount outside them in the subtree, or a task filed there placed under it.
#[test]
fn a_rooted_move_stays_inside_its_roots() {
    let (db, _inside, _outside, _managed) = crate::tests::mutate_fixture();
    let rooted = crate::tests::rooted(&db);
    assert!(list(&rooted, None).contains(&"docs".to_owned()));
    assert_eq!(list(&rooted, Some("docs")), vec!["docs/out".to_owned()]);

    for (from, to) in [
        ("docs", "papers"),
        ("docs/out", "repos/out"),
        ("tasks/docs", "tasks/elsewhere"),
    ] {
        let e = call(&rooted, json!({ "op": "ns.mv", "from": from, "to": to })).unwrap_err();
        assert_eq!(e.code, ErrorCode::Forbidden, "{from}: {e:?}");
    }
    assert!(list(&rooted, Some("docs")).contains(&"docs/out".to_owned()));

    // Nor onto a mount outside the roots, nor a reserved namespace.
    call(
        &rooted,
        json!({ "op": "task.add", "text": "m +scratch/a", "managed": true }),
    )
    .unwrap();
    for (from, to) in [
        ("scratch", "docs/out/scratch"),
        ("tasks", "old-tasks"),
        ("_sys/views", "junk"),
        ("scratch", "_sys/scratch"),
    ] {
        let e = call(&rooted, json!({ "op": "ns.mv", "from": from, "to": to })).unwrap_err();
        assert_eq!(e.code, ErrorCode::Forbidden, "{from} -> {to}: {e:?}");
    }
    assert!(list(&rooted, None).contains(&"scratch".to_owned()));

    match call(
        &rooted,
        json!({ "op": "ns.mv", "from": "repos/in", "to": "repos/in2" }),
    )
    .unwrap()
    {
        Response::Moved { count } => assert_eq!(count, 1),
        other => panic!("{other:?}"),
    }
    // The host is held to none of it.
    let host = LocalBackend::new(db);
    call(
        &host,
        json!({ "op": "ns.mv", "from": "docs", "to": "papers" }),
    )
    .unwrap();
    assert!(list(&host, None).contains(&"papers".to_owned()));
}

/// A client moves at most so many items at once; the host is not bounded.
#[test]
fn a_rooted_move_is_bounded() {
    let (db, ..) = crate::tests::mutate_fixture();
    db.write_txn("t", |c, m| {
        for i in 0..=super::MAX_MOVED_ITEMS {
            let mut t = jkb_core::task::NewTask::new(format!("task:b{i}"), "b");
            t.home = "bulk/x".into();
            jkb_core::task::create(c, m, &t)?;
        }
        Ok(())
    })
    .unwrap();
    let e = call(
        &crate::tests::rooted(&db),
        json!({ "op": "ns.mv", "from": "bulk", "to": "bulk2" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    call(
        &LocalBackend::new(db),
        json!({ "op": "ns.mv", "from": "bulk", "to": "bulk2" }),
    )
    .unwrap();
}
