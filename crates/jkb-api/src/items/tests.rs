//! Any item, the edge walk and the sync archive, through the backend.

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

fn show(b: &LocalBackend, uid: &str, preview: Option<usize>) -> super::ItemInfo {
    match call(
        b,
        json!({ "op": "item.show", "uid": uid, "preview": preview }),
    )
    .unwrap()
    {
        Response::Item { item } => *item,
        other => panic!("{other:?}"),
    }
}

/// `item.show` carries the details `stat` and `item show` print, and a preview bounded by kind or by
/// the caller.
#[test]
fn an_item_is_shown_with_a_bounded_preview() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "a task #size=s +work/x");
    let info = show(&b, &uid, None);
    assert_eq!(info.kind, "task");
    assert_eq!(info.namespace.as_deref(), Some("work/x"));
    assert_eq!(info.binding.as_deref(), Some("managed:"));
    assert_eq!(
        info.tags,
        vec![super::Tag {
            facet: "size".into(),
            value: "s".into()
        }]
    );
    assert_eq!(
        (info.preview.as_str(), info.preview_truncated),
        ("a task", false)
    );
    let cut = show(&b, &uid, Some(2));
    assert_eq!((cut.preview.as_str(), cut.preview_truncated), ("a ", true));
    assert_eq!(cut.content_chars, 6);
    let e = call(&b, json!({ "op": "item.show", "uid": "nope:x" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

/// `item.rm` removes what a client may write and refuses what it may not.
#[test]
fn an_item_is_removed_only_where_its_client_may_write() {
    let (db, inside, outside, managed) = crate::tests::mutate_fixture();
    let rooted = crate::tests::rooted(&db);
    // Filed in a tasks.md, so the synced-file guard wants `force` — but the roots refuse first.
    let e = call(
        &rooted,
        json!({ "op": "item.rm", "uid": outside, "force": true }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
    let e = call(&rooted, json!({ "op": "item.rm", "uid": inside })).unwrap_err();
    assert!(e.message.contains("synced file"), "{e:?}");
    for uid in [&inside, &managed] {
        match call(
            &rooted,
            json!({ "op": "item.rm", "uid": uid, "force": true }),
        )
        .unwrap()
        {
            Response::ItemRemoved { removed } => assert_eq!(&removed.uid, uid),
            other => panic!("{other:?}"),
        }
    }
    // A bare slug is not a uid here: `item rm` never guesses which task was meant.
    let slug = managed.trim_start_matches("task:");
    let e = call(&rooted, json!({ "op": "item.rm", "uid": slug })).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

/// `kb.related` walks the typed edges, and refuses a walk it cannot bound or an edge it does not know.
#[test]
fn related_walks_the_edges_it_is_asked_to() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    let (a, c, d) = (add(&b, "a"), add(&b, "c"), add(&b, "d"));
    call(&b, json!({ "op": "task.depend", "uid": a, "dep": c })).unwrap();
    call(&b, json!({ "op": "task.depend", "uid": c, "dep": d })).unwrap();
    let related = |r: serde_json::Value| match call(&b, r).unwrap() {
        Response::Related {
            rows, truncated, ..
        } => {
            assert!(!truncated);
            rows.into_iter()
                .map(|r| (r.uid, r.depth, r.direction))
                .collect::<Vec<_>>()
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(
        related(json!({ "op": "kb.related", "uid": a, "depth": 2, "edges": ["depends_on"] })),
        vec![
            (c.clone(), 1, "out".to_owned()),
            (d.clone(), 2, "out".to_owned())
        ]
    );
    assert_eq!(
        related(json!({ "op": "kb.related", "uid": d, "depth": 1, "direction": "in" })),
        vec![(c.clone(), 1, "in".to_owned())]
    );
    for bad in [
        json!({ "op": "kb.related", "uid": a, "depth": 1, "edges": ["loves"] }),
        json!({ "op": "kb.related", "uid": a, "depth": super::MAX_RELATED_DEPTH + 1 }),
    ] {
        assert_eq!(call(&b, bad).unwrap_err().code, ErrorCode::Invalid);
    }
    let tight = LocalBackend::new(db).with_read_budget(1);
    match call(&tight, json!({ "op": "kb.related", "uid": a, "depth": 2 })).unwrap() {
        Response::Related {
            rows, truncated, ..
        } => assert!(rows.is_empty() && truncated),
        other => panic!("{other:?}"),
    }
}

/// The archive lists and reads back text blobs by a unique prefix, and refuses what it cannot answer
/// as text.
#[test]
fn blobs_are_listed_and_read_back_as_text() {
    let db = Db::open_in_memory().unwrap();
    let text_hash = jkb_core::blob::hash_bytes(b"version one\n");
    let bin_hash = jkb_core::blob::hash_bytes(&[0xff, 0xfe]);
    let (t, bh) = (text_hash.clone(), bin_hash.clone());
    db.write_txn("t", move |c, _| {
        jkb_core::blob::store(c, &t, b"version one\n", Some("text/markdown"))?;
        jkb_core::blob::store(c, &bh, &[0xff, 0xfe], None)
    })
    .unwrap();
    let b = LocalBackend::new(db);
    match call(
        &b,
        json!({ "op": "kb.blobs", "contains": "one", "limit": 10 }),
    )
    .unwrap()
    {
        Response::Blobs { blobs, .. } => {
            assert_eq!(blobs.len(), 1);
            assert_eq!(blobs[0].hash, text_hash);
        }
        other => panic!("{other:?}"),
    }
    match call(&b, json!({ "op": "kb.blob", "prefix": &text_hash[..8] })).unwrap() {
        Response::Blob { hash, text } => {
            assert_eq!(hash, text_hash);
            assert_eq!(text, "version one\n");
        }
        other => panic!("{other:?}"),
    }
    let e = call(&b, json!({ "op": "kb.blob", "prefix": &bin_hash[..8] })).unwrap_err();
    assert!(e.message.contains("not text"), "{e:?}");
    for (prefix, code) in [
        ("abc", ErrorCode::Invalid),
        ("zzzz", ErrorCode::Invalid),
        ("0000000000", ErrorCode::NotFound),
    ] {
        let e = call(&b, json!({ "op": "kb.blob", "prefix": prefix })).unwrap_err();
        assert_eq!(e.code, code, "{prefix}: {e:?}");
    }
    for bad in [
        json!({ "op": "kb.blobs", "contains": "", "limit": 1 }),
        json!({ "op": "kb.blobs", "limit": super::MAX_BLOBS + 1 }),
    ] {
        assert_eq!(call(&b, bad).unwrap_err().code, ErrorCode::Invalid);
    }
}

/// A walk reaching more items than one answer reads is cut, and says so.
#[test]
fn a_related_walk_is_capped() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        let hub = jkb_core::task::create(c, m, &jkb_core::task::NewTask::new("task:hub", "hub"))?;
        for i in 0..=super::MAX_RELATED_NODES {
            let uid = format!("task:n{i}");
            let n = jkb_core::task::create(c, m, &jkb_core::task::NewTask::new(&uid, "n"))?;
            jkb_core::edge::link(c, m, hub, n, jkb_types::EdgeType::Informs, None)?;
        }
        Ok(())
    })
    .unwrap();
    let b = LocalBackend::new(db);
    match call(
        &b,
        json!({ "op": "kb.related", "uid": "task:hub", "depth": 1 }),
    )
    .unwrap()
    {
        Response::Related {
            rows,
            truncated,
            at_node_cap,
        } => {
            assert_eq!(rows.len(), super::MAX_RELATED_NODES);
            assert!(at_node_cap && !truncated);
        }
        other => panic!("{other:?}"),
    }
}

/// A neighbour linked by several edges is one neighbour, and the walk still finds every one.
#[test]
fn a_walk_counts_neighbours_not_edges() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        let hub = jkb_core::task::create(c, m, &jkb_core::task::NewTask::new("task:hub", "hub"))?;
        let mut ids = Vec::new();
        for i in 0..900 {
            let uid = format!("task:n{i}");
            ids.push(jkb_core::task::create(
                c,
                m,
                &jkb_core::task::NewTask::new(&uid, "n"),
            )?);
        }
        // The first 500 neighbours two edges each, so the walk's first page (1001 rows) holds only
        // 501 of them, and the rest are found only on the next.
        for n in &ids[..500] {
            for ty in [
                jkb_types::EdgeType::References,
                jkb_types::EdgeType::DerivedFrom,
            ] {
                jkb_core::edge::link(c, m, hub, *n, ty, None)?;
            }
        }
        for n in &ids[500..] {
            jkb_core::edge::link(c, m, hub, *n, jkb_types::EdgeType::References, None)?;
        }
        Ok(())
    })
    .unwrap();
    let b = LocalBackend::new(db);
    match call(
        &b,
        json!({ "op": "kb.related", "uid": "task:hub", "depth": 1 }),
    )
    .unwrap()
    {
        Response::Related {
            rows, at_node_cap, ..
        } => {
            assert_eq!(rows.len(), 900);
            assert!(!at_node_cap);
            assert!(rows.iter().all(|r| r.via == "references"));
        }
        other => panic!("{other:?}"),
    }
}

/// Past the cap, a walk over neighbours of several edges each stops at the cap and says so.
#[test]
fn a_multi_edge_walk_is_capped() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        let hub = jkb_core::task::create(c, m, &jkb_core::task::NewTask::new("task:hub", "hub"))?;
        for i in 0..=super::MAX_RELATED_NODES {
            let uid = format!("task:n{i}");
            let n = jkb_core::task::create(c, m, &jkb_core::task::NewTask::new(&uid, "n"))?;
            for ty in [
                jkb_types::EdgeType::References,
                jkb_types::EdgeType::DerivedFrom,
            ] {
                jkb_core::edge::link(c, m, hub, n, ty, None)?;
            }
        }
        Ok(())
    })
    .unwrap();
    match call(
        &LocalBackend::new(db),
        json!({ "op": "kb.related", "uid": "task:hub", "depth": 1 }),
    )
    .unwrap()
    {
        Response::Related {
            rows, at_node_cap, ..
        } => {
            assert_eq!(rows.len(), super::MAX_RELATED_NODES);
            assert!(at_node_cap);
        }
        other => panic!("{other:?}"),
    }
}

/// A blob larger than one answer is refused as text, and read whole in-process.
#[test]
fn a_large_blob_is_read_whole_only_in_process() {
    let db = Db::open_in_memory().unwrap();
    let big = vec![b'a'; usize::try_from(super::MAX_BLOB_TEXT_BYTES).unwrap() + 1];
    let hash = jkb_core::blob::hash_bytes(&big);
    let (h, bytes) = (hash.clone(), big.clone());
    db.write_txn("t", move |c, _| jkb_core::blob::store(c, &h, &bytes, None))
        .unwrap();
    let b = LocalBackend::new(db.clone());
    let e = call(&b, json!({ "op": "kb.blob", "prefix": &hash[..8] })).unwrap_err();
    assert!(e.message.contains("more than one answer carries"), "{e:?}");
    let prefix = hash[..8].to_owned();
    let (full, read) = db
        .read_with(move |c| super::blob_bytes(c, &prefix))
        .unwrap();
    assert_eq!((full, read.len()), (hash, big.len()));
}

/// Editing any item is bounded for a client of `jkb serve`; on the host only a task's body is.
#[test]
fn an_edit_is_bounded_for_a_task_and_for_any_client_item() {
    let (db, ..) = crate::tests::mutate_fixture();
    db.write_txn("t", |c, m| {
        jkb_core::item::upsert(
            c,
            m,
            &jkb_core::item::NewItem {
                uid: "note:big".into(),
                kind: "text".into(),
                content: Some("x".into()),
                content_hash: None,
                mime: None,
            },
        )
    })
    .unwrap();
    let big = "y".repeat(crate::tasks::MAX_CONTENT_BYTES + 1);
    let host = LocalBackend::new(db.clone());
    let edit = |b: &LocalBackend, uid: &str| {
        call(b, json!({ "op": "task.edit", "uid": uid, "text": big }))
    };
    edit(&host, "note:big").expect("the host edits any item unbounded");
    let e = edit(&crate::tests::rooted(&db), "note:big").unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    let task = add(&host, "a task");
    assert_eq!(edit(&host, &task).unwrap_err().code, ErrorCode::Invalid);
}

/// A file's versions are found by the path the client names, re-rooted from the client's home.
#[test]
fn a_file_s_history_is_found_from_the_client_s_path() {
    let db = Db::open_in_memory().unwrap();
    let server_home = std::env::var("HOME").unwrap_or_else(|_| "/".to_owned());
    let uri = format!("file://{server_home}/repos/p/notes.md");
    let u = uri.clone();
    db.write_txn("t", move |c, m| {
        for hash in ["h1", "h2", "h2"] {
            jkb_core::sync_state::upsert(
                c,
                m,
                &jkb_core::sync_state::SyncStateWrite {
                    uri: &u,
                    serializer: "document",
                    status: "ok",
                    last_synced_hash: Some(hash),
                    base_blob_hash: Some(hash),
                    parse_error: None,
                    quarantine_blob_hash: None,
                    document: None,
                },
            )?;
        }
        Ok(())
    })
    .unwrap();
    let b = LocalBackend::new(db);
    match call(
        &b,
        json!({ "op": "kb.history", "path": "/home/client/repos/p/notes.md",
                "home": "/home/client" }),
    )
    .unwrap()
    {
        Response::Versions {
            uri: got, versions, ..
        } => {
            assert_eq!(got, uri);
            let blobs: Vec<&str> = versions.iter().map(|v| v.blob.as_str()).collect();
            assert!(blobs == ["h2", "h1"], "newest first, once each: {blobs:?}");
        }
        other => panic!("{other:?}"),
    }
    let e = call(&b, json!({ "op": "kb.history", "path": "relative.md" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}
