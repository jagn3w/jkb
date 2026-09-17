//! Investigations through the backend, and the file roots their writes are held to.

use serde_json::json;

use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};
use jkb_core::Db;

fn call(b: &LocalBackend, r: serde_json::Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

fn inv(b: &LocalBackend, r: serde_json::Value) -> Result<super::InvAnswer, ApiError> {
    match call(b, r)? {
        Response::Inv { answer, .. } => Ok(answer),
        other => panic!("{other:?}"),
    }
}

fn unit_uid(a: super::InvAnswer) -> String {
    match a {
        super::InvAnswer::Unit { uid, .. } => uid,
        other => panic!("{other:?}"),
    }
}

/// An investigation is started, worked with a verb and read back through the ops.
#[test]
fn an_investigation_is_worked_and_read_through_the_ops() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let created = inv(
        &b,
        json!({ "op": "inv.write", "write": "new", "type_name": "debugging",
                "ns": "memory/p/bug", "goal_kind": "symptom", "goal": "it crashes" }),
    )
    .unwrap();
    let super::InvAnswer::Created { goal_uid, existed } = created else {
        panic!("{created:?}")
    };
    assert!(!existed);
    let again = inv(
        &b,
        json!({ "op": "inv.write", "write": "new", "type_name": "debugging",
                "ns": "memory/p/bug", "goal_kind": "symptom", "goal": "it crashes" }),
    )
    .unwrap();
    assert!(matches!(
        again,
        super::InvAnswer::Created { existed: true, .. }
    ));

    match inv(
        &b,
        json!({ "op": "inv.read", "read": "type", "ns": "memory/p/bug" }),
    )
    .unwrap()
    {
        super::InvAnswer::Type { type_name, .. } => {
            assert_eq!(type_name.as_deref(), Some("debugging"));
        }
        other => panic!("{other:?}"),
    }
    let hypothesis = unit_uid(
        inv(
            &b,
            json!({ "op": "inv.write", "write": "do", "ns": "memory/p/bug",
                    "verb": "hypothesize", "text": "a race\nin the pool", "on": goal_uid }),
        )
        .unwrap(),
    );
    match inv(
        &b,
        json!({ "op": "inv.read", "read": "frontier", "ns": "memory/p/bug" }),
    )
    .unwrap()
    {
        super::InvAnswer::Units { units, .. } => {
            let row = units
                .iter()
                .find(|u| u.uid == hypothesis)
                .expect("on the frontier");
            assert_eq!(row.snippet.as_deref(), Some("a race"));
        }
        other => panic!("{other:?}"),
    }
    match inv(&b, json!({ "op": "inv.read", "read": "ls" })).unwrap() {
        super::InvAnswer::List { rows } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].ns, "memory/p/bug");
        }
        other => panic!("{other:?}"),
    }
    inv(
        &b,
        json!({ "op": "inv.write", "write": "resolve", "uid": hypothesis, "resolution": "dead_end" }),
    )
    .unwrap();
    match inv(
        &b,
        json!({ "op": "inv.read", "read": "tombstones", "ns": "memory/p/bug" }),
    )
    .unwrap()
    {
        super::InvAnswer::Tombstones { rows } => assert_eq!(rows[0].unit.uid, hypothesis),
        other => panic!("{other:?}"),
    }
    digest_and_refusals(&b, &hypothesis);
}

fn digest_and_refusals(b: &LocalBackend, hypothesis: &str) {
    match inv(
        b,
        json!({ "op": "inv.write", "write": "digest", "ns": "memory/p/bug" }),
    )
    .unwrap()
    {
        super::InvAnswer::Digest { uid, text } => {
            assert!(uid.is_some());
            assert!(text.contains("memory/p/bug"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    let e = inv(
        b,
        json!({ "op": "inv.read", "read": "retread", "uid": hypothesis,
                "depth": crate::items::MAX_RELATED_DEPTH + 1 }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    let e = inv(
        b,
        json!({ "op": "inv.read", "read": "evidence", "uid": "nope:x" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

/// Under file roots, a write into a namespace another directory's mount exports, or onto a unit filed
/// outside the roots, is refused and writes nothing.
#[test]
fn a_rooted_investigation_write_stays_inside_its_roots() {
    let (db, inside, outside, _managed) = crate::tests::mutate_fixture();
    let rooted = crate::tests::rooted(&db);
    // A unit inside the roots, to name as the other end.
    let host = LocalBackend::new(db.clone());
    let goal = match inv(
        &host,
        json!({ "op": "inv.write", "write": "new", "type_name": "conjecture-attack",
                "ns": "memory/in-roots", "goal_kind": "conjecture", "goal": "x" }),
    )
    .unwrap()
    {
        super::InvAnswer::Created { goal_uid, .. } => goal_uid,
        other => panic!("{other:?}"),
    };
    // `docs/out` is mounted from a directory outside the roots, and `outside` is filed there. One
    // write per `InvWrite` variant, so a guard dropped from any arm fails here.
    for write in [
        json!({ "op": "inv.write", "write": "digest", "ns": "docs/out" }),
        json!({ "op": "inv.write", "write": "do", "ns": "docs/out", "verb": "hypothesize",
                "text": "t" }),
        json!({ "op": "inv.write", "write": "do", "ns": "memory/in-roots", "verb": "hypothesize",
                "text": "t", "on": outside }),
        json!({ "op": "inv.write", "write": "add", "ns": "docs/out", "kind": "node", "text": "t" }),
        json!({ "op": "inv.write", "write": "add", "ns": "memory/in-roots", "kind": "node",
                "text": "t", "edges": [{ "edge": "references", "target": outside }] }),
        json!({ "op": "inv.write", "write": "reopen", "route": outside, "mechanism": goal }),
        json!({ "op": "inv.write", "write": "reopen", "route": goal, "mechanism": outside }),
        json!({ "op": "inv.write", "write": "stale", "ns": "docs/out", "window": "w" }),
        json!({ "op": "inv.write", "write": "new", "type_name": "debugging",
                "ns": "docs/out/bug", "goal_kind": "symptom", "goal": "x" }),
        json!({ "op": "inv.write", "write": "rollup", "ns": "docs/out" }),
        json!({ "op": "inv.write", "write": "link", "src": inside, "edge": "informs",
                "dst": outside }),
        json!({ "op": "inv.write", "write": "promise", "uid": outside, "value": 0.5 }),
        json!({ "op": "inv.write", "write": "resolve", "uid": outside, "resolution": "dead_end" }),
    ] {
        let e = inv(&rooted, write.clone()).unwrap_err();
        assert_eq!(e.code, ErrorCode::Forbidden, "{write}: {e:?}");
    }
    // Too many edges or tags on one unit is refused before any is judged.
    let many: Vec<_> = (0..=crate::tasks::MAX_QUICK_ADD_MODIFIERS)
        .map(|_| json!({ "edge": "references", "target": goal }))
        .collect();
    let e = inv(
        &rooted,
        json!({ "op": "inv.write", "write": "add", "ns": "memory/in-roots", "kind": "node",
                "text": "t", "edges": many }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    // The same kind of write inside the roots is served.
    inv(
        &rooted,
        json!({ "op": "inv.write", "write": "new", "type_name": "debugging",
                "ns": "memory/in", "goal_kind": "symptom", "goal": "x" }),
    )
    .unwrap();
    // A write that names something unknown is not a request at all.
    assert!(serde_json::from_value::<crate::Request>(
        json!({ "op": "inv.write", "write": "promise", "uid": "u", "value": 1.0, "extra": 1 })
    )
    .is_err());
}
