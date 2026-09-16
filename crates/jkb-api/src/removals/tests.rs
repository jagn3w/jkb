//! The removal and lease ops through the backend, with and without `jkb serve`'s file roots.

use std::path::PathBuf;

use serde_json::json;

use crate::tasks::FileRoots;
use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};
use jkb_core::Db;

fn call(b: &LocalBackend, r: serde_json::Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

fn served(db: &Db) -> LocalBackend {
    LocalBackend::new(db.clone())
        .with_actor("serve")
        .with_file_roots(
            FileRoots::new(vec![PathBuf::from("/home/h/repos")])
                .with_home(PathBuf::from("/home/h")),
        )
}

fn record(worktree: &str, repo_root: &str) -> serde_json::Value {
    json!({ "worktree": worktree, "repo_root": repo_root, "branch": "task/s", "uid": "task:t",
            "recorded_at": 7, "head": "abc" })
}

fn add(b: &LocalBackend, removal: &serde_json::Value) -> Result<i64, ApiError> {
    match call(b, json!({ "op": "removal.add", "removal": removal }))? {
        Response::RemovalAdded { id } => Ok(id),
        other => panic!("{other:?}"),
    }
}

fn changed(r: Result<Response, ApiError>) -> Result<bool, ApiError> {
    match r? {
        Response::Changed { changed } => Ok(changed),
        other => panic!("{other:?}"),
    }
}

fn forbidden(r: Result<impl std::fmt::Debug, ApiError>, why: &str) {
    let e = r.expect_err(why);
    assert_eq!(e.code, ErrorCode::Forbidden, "{why}: {e:?}");
}

/// A record written by a client names only `~/` paths under its roots, and reads back as written — with
/// the backend that wrote it.
#[test]
fn a_client_records_only_paths_under_its_roots() {
    let db = Db::open_in_memory().unwrap();
    let b = served(&db);
    let id = add(&b, &record("~/repos/p/.jkb/work/s", "~/repos/p")).unwrap();
    match call(&b, json!({ "op": "removal.list" })).unwrap() {
        Response::Removals { records, next } => {
            assert_eq!(next, None);
            assert_eq!(records.len(), 1);
            assert_eq!(
                (
                    records[0].id,
                    records[0].removal.worktree.as_str(),
                    records[0].written_via.as_str()
                ),
                (id, "~/repos/p/.jkb/work/s", "serve")
            );
        }
        other => panic!("{other:?}"),
    }

    for (worktree, repo_root, why) in [
        (
            "/home/h/repos/p/.jkb/work/s",
            "~/repos/p",
            "an absolute path",
        ),
        ("~/src/p/.jkb/work/s", "~/src/p", "outside the roots"),
        ("~/repos/../.ssh", "~/repos/p", "a parent component"),
        (
            "~/repos/./p/.jkb/work/s",
            "~/repos/p",
            "a current-directory component",
        ),
        ("~/repos", "~/repos", "the root itself"),
        ("~/repos/", "~/repos", "the root with a slash"),
        ("~/repos//p/.jkb/work/s", "~/repos/p", "an empty segment"),
        ("~/repos/p/.jkb/work/s/..", "~/repos/p", "a trailing parent"),
        ("~repos/p/.jkb/work/s", "~/repos/p", "not `~/`"),
    ] {
        forbidden(add(&b, &record(worktree, repo_root)), why);
    }
    let mut archived = record("~/repos/p/.jkb/work/s", "~/repos/p");
    archived["archive"] = json!("/tmp/x");
    archived["archived_at"] = json!(8);
    forbidden(add(&b, &archived), "an archive outside the roots");

    // The host's own backend is held to nothing.
    let host = LocalBackend::new(db.clone());
    add(
        &host,
        &record("/Users/u/src/p/.jkb/work/s", "/Users/u/src/p"),
    )
    .unwrap();
}

/// A client archives and drops only records under its roots, and archives only to them.
#[test]
fn a_client_acts_only_on_records_under_its_roots() {
    let db = Db::open_in_memory().unwrap();
    let b = served(&db);
    let host = LocalBackend::new(db.clone());
    let theirs = add(
        &host,
        &record("/Users/u/src/p/.jkb/work/s", "/Users/u/src/p"),
    )
    .unwrap();
    let ours = add(&b, &record("~/repos/p/.jkb/work/s", "~/repos/p")).unwrap();

    forbidden(
        changed(call(&b, json!({ "op": "removal.drop", "id": theirs }))),
        "a host record",
    );
    forbidden(
        changed(call(
            &b,
            json!({ "op": "removal.archived", "id": theirs, "archive": "~/repos/p/.jkb/archive/s", "at": 9 }),
        )),
        "a host record",
    );
    forbidden(
        changed(call(
            &b,
            json!({ "op": "removal.archived", "id": ours, "archive": "/tmp/s", "at": 9 }),
        )),
        "an archive outside the roots",
    );
    assert!(changed(call(
        &b,
        json!({ "op": "removal.archived", "id": ours, "archive": "~/repos/p/.jkb/archive/s", "at": 9 }),
    ))
    .unwrap());
    assert!(
        !changed(call(
            &b,
            json!({ "op": "removal.archived", "id": ours, "archive": "~/repos/p/.jkb/archive/t", "at": 10 }),
        ))
        .unwrap(),
        "archived once"
    );
    assert!(changed(call(&b, json!({ "op": "removal.drop", "id": ours }))).unwrap());
    assert!(!changed(call(&b, json!({ "op": "removal.drop", "id": ours }))).unwrap());
    assert!(changed(call(&host, json!({ "op": "removal.drop", "id": theirs }))).unwrap());
}

/// `removal.list` pages to the end, each record once.
#[test]
fn the_listing_pages_to_the_end() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db);
    let n = jkb_core::removal::PAGE + 3;
    for _ in 0..n {
        add(&b, &record("/r/.jkb/work/s", "/r")).unwrap();
    }
    let mut seen = Vec::new();
    let mut after: Option<i64> = None;
    loop {
        match call(&b, json!({ "op": "removal.list", "after": after })).unwrap() {
            Response::Removals { records, next } => {
                seen.extend(records.into_iter().map(|r| r.id));
                match next {
                    Some(n) => after = Some(n),
                    None => break,
                }
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(seen.len(), n);
    assert!(seen.windows(2).all(|w| w[0] < w[1]));
}

/// Only the leases jkb takes can be named; a take and a release are compare-and-sets; breaking one is
/// the host's alone.
#[test]
fn leases_are_named_compared_and_broken_only_on_the_host() {
    let db = Db::open_in_memory().unwrap();
    let b = served(&db);
    let host = LocalBackend::new(db.clone());
    let take = |b: &LocalBackend, holder: &str, displace: Option<&str>| {
        changed(call(
            b,
            json!({ "op": "lease.take", "name": "removal-sweep", "holder": holder, "displace": displace }),
        ))
        .unwrap()
    };
    for name in ["", "mine", "land:", "removal-sweep2"] {
        let e = call(&b, json!({ "op": "lease.get", "name": name })).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{name:?}");
    }
    assert!(matches!(
        call(&b, json!({ "op": "lease.get", "name": "land:proj" })).unwrap(),
        Response::Lease { lease: None }
    ));

    assert!(take(&b, "host:c 1", None));
    assert!(!take(&host, "host:h 2", None));
    match call(&host, json!({ "op": "lease.get", "name": "removal-sweep" })).unwrap() {
        Response::Lease { lease: Some(l) } => assert_eq!(l.holder, "host:c 1"),
        other => panic!("{other:?}"),
    }
    assert!(take(&host, "host:h 2", Some("host:c 1")));
    assert!(
        !changed(call(
            &b,
            json!({ "op": "lease.release", "name": "removal-sweep", "holder": "host:c 1" }),
        ))
        .unwrap(),
        "the displaced holder releases nothing"
    );

    forbidden(
        call(&b, json!({ "op": "lease.break", "name": "removal-sweep" })),
        "a client breaking a lease",
    );
    assert!(matches!(
        call(&host, json!({ "op": "lease.break", "name": "removal-sweep" })).unwrap(),
        Response::LeaseBroken { holder: Some(h) } if h == "host:h 2"
    ));
}

/// `removal.cancel` drops only pending records, only under the roots, and nothing at all while a sweep
/// holds its lease — which it neither takes nor needs.
#[test]
fn a_cancel_drops_pending_records_unless_a_sweep_runs() {
    let db = Db::open_in_memory().unwrap();
    let b = served(&db);
    let host = LocalBackend::new(db.clone());
    let pending = add(&b, &record("~/repos/p/.jkb/work/s", "~/repos/p")).unwrap();
    let archived = add(&b, &record("~/repos/p/.jkb/work/s", "~/repos/p")).unwrap();
    assert!(changed(call(
        &b,
        json!({ "op": "removal.archived", "id": archived, "archive": "~/repos/p/.jkb/archive/s", "at": 9 }),
    ))
    .unwrap());
    let theirs = add(
        &host,
        &record("/Users/u/src/p/.jkb/work/s", "/Users/u/src/p"),
    )
    .unwrap();
    let cancel = |b: &LocalBackend, ids: &[i64]| match call(
        b,
        json!({ "op": "removal.cancel", "ids": ids }),
    )? {
        Response::RemovalsCancelled { cancelled } => Ok(cancelled),
        other => panic!("{other:?}"),
    };

    let other = add(&b, &record("~/repos/p/.jkb/work/o", "~/repos/p")).unwrap();
    let c = cancel(&b, &[theirs, other]).unwrap();
    assert_eq!(
        (c.cancelled, c.skipped.as_slice()),
        (1, [theirs].as_slice()),
        "a host record is skipped and named, and the rest of the batch still applies"
    );
    assert!(changed(call(
        &host,
        json!({ "op": "lease.take", "name": "removal-sweep", "holder": "host:h 1" }),
    ))
    .unwrap());
    let c = cancel(&b, &[pending, archived]).unwrap();
    assert_eq!(
        (c.cancelled, c.sweep_holder.as_deref()),
        (0, Some("host:h"))
    );
    assert!(changed(call(
        &host,
        json!({ "op": "lease.release", "name": "removal-sweep", "holder": "host:h 1" }),
    ))
    .unwrap());
    let c = cancel(&b, &[pending, archived, 999]).unwrap();
    assert_eq!(
        (c.cancelled, c.sweep_holder),
        (1, None),
        "only the pending one"
    );
    match call(&b, json!({ "op": "removal.list" })).unwrap() {
        Response::Removals { records, .. } => {
            let ids: Vec<i64> = records.iter().map(|r| r.id).collect();
            assert_eq!(ids, [archived, theirs]);
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(
            call(&b, json!({ "op": "lease.get", "name": "removal-sweep" })).unwrap(),
            Response::Lease { lease: None }
        ),
        "a cancel takes no lease"
    );
    let e: ApiError = cancel(&b, &vec![1; super::MAX_CANCEL + 1]).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}
