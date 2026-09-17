//! Namespaces through the backend, and the file roots a move is held to.

use serde_json::json;

use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};

fn call(b: &LocalBackend, r: serde_json::Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

fn list(b: &LocalBackend, scope: Option<&str>) -> Vec<String> {
    match call(b, json!({ "op": "ns.list", "scope": scope })).unwrap() {
        Response::Namespaces { paths } => paths,
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
