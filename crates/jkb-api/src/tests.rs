use serde_json::json;

use super::{ApiError, Backend, ErrorCode, LocalBackend, Request, Response, SpecInput};
use jkb_core::Db;

fn backend() -> LocalBackend {
    LocalBackend::new(Db::open_in_memory().unwrap())
}

fn call(b: &LocalBackend, r: serde_json::Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

#[test]
fn a_request_is_an_op_tagged_object_and_round_trips() {
    let r = Request::MqSend {
        topic: "claude/notify".to_owned(),
        key: "host/mac/session/1".to_owned(),
        kind: "notify.post".to_owned(),
        payload: json!({ "title": "t" }),
        ttl_ms: Some(1000),
        producer: "hook".to_owned(),
    };
    let wire = serde_json::to_value(&r).unwrap();
    assert_eq!(wire["op"], "mq.send");
    assert_eq!(serde_json::from_value::<Request>(wire).unwrap(), r);
    assert_eq!(
        serde_json::to_value(Request::MqInspect {}).unwrap(),
        json!({ "op": "mq.inspect" })
    );
}

#[test]
fn an_unknown_op_or_field_is_refused_rather_than_ignored() {
    // Version skew is normal between a container's jkb and the host's: a field this binary does not
    // know must not be silently dropped, or a request would be served as something it did not say.
    assert!(serde_json::from_value::<Request>(json!({ "op": "mq.drop_everything" })).is_err());
    assert!(serde_json::from_value::<Request>(json!({
        "op": "mq.poll", "topic": "t", "group": "g", "max": 1, "filter": "repo/jkb"
    }))
    .is_err());
    assert!(serde_json::from_value::<Request>(json!({
        "op": "mq.topic_create", "topic": "t", "spec": { "max_bytes": 5, "retention": "7d" }
    }))
    .is_err());
    // A field-less op too: as a unit variant this parsed and answered for every topic.
    assert!(
        serde_json::from_value::<Request>(json!({ "op": "mq.inspect", "topic": "x" })).is_err()
    );
    assert!(serde_json::from_value::<Request>(json!({ "op": "mq.inspect" })).is_ok());
}

#[test]
#[allow(clippy::too_many_lines)] // one sample per op
fn every_op_names_its_own_wire_tag_and_is_advertised() {
    let samples = [
        Request::MqTopicCreate {
            topic: "t".into(),
            spec: SpecInput::default(),
        },
        Request::MqSend {
            topic: "t".into(),
            key: "k".into(),
            kind: "k".into(),
            payload: json!(1),
            ttl_ms: None,
            producer: "p".into(),
        },
        Request::MqGroupCreate {
            topic: "t".into(),
            group: "g".into(),
            from_start: false,
        },
        Request::MqPoll {
            topic: "t".into(),
            group: "g".into(),
            max: 1,
            after: None,
        },
        Request::MqAck {
            topic: "t".into(),
            group: "g".into(),
            seq: 1,
        },
        Request::MqCompact { force: false },
        Request::MqInspect {},
        Request::MqTail {
            topic: "t".into(),
            limit: 1,
        },
        Request::NotifyEvent {
            session: "s".into(),
            event: super::HookEvent::Needed,
            tool: String::new(),
            message: String::new(),
            cwd: String::new(),
            owner: String::new(),
            instance: String::new(),
        },
        Request::NotifyOpenSessions {},
        Request::NotifyGone {
            session: "s".into(),
            owner: "1".into(),
            instance: "h".into(),
        },
        Request::KbAmbient {
            cwd: "/".into(),
            home: String::new(),
        },
        Request::KbQuery {
            dsl: String::new(),
            default_scope: None,
            limit: None,
            count: false,
        },
        Request::KbLs {
            path: None,
            all: false,
            recursive: false,
        },
        Request::KbTree {
            path: None,
            all: false,
            depth: None,
        },
        Request::KbCat { uid: "u".into() },
        Request::KbGrep {
            pattern: "p".into(),
            scope: None,
            ignore_case: false,
        },
        Request::KbSearch {
            dsl: "x".into(),
            default_scope: None,
            route: super::kb::SearchRoute::Fts,
            limit: 1,
            context: None,
        },
        Request::TaskReady {
            dsl: String::new(),
            default_scope: None,
            limit: None,
        },
        Request::TaskShow { uid: "u".into() },
        Request::TaskSubtasks {
            uid: "u".into(),
            all: false,
        },
    ];
    // OPS against the tags serde actually accepts — read from its unknown-variant error, which lists
    // them all. Without this, a new variant named in `op()` but in neither OPS nor the samples below
    // left every assertion here green: the counts still matched.
    let err = serde_json::from_value::<Request>(json!({ "op": "no.such_op" }))
        .unwrap_err()
        .to_string();
    let accepted: Vec<&str> = err
        .split_once("expected one of ")
        .unwrap_or_else(|| panic!("serde's message changed shape: {err}"))
        .1
        .split(", ")
        .map(|s| s.trim_matches('`'))
        .collect();
    assert_eq!(
        accepted,
        Request::OPS,
        "OPS is every op the wire accepts, in order"
    );
    assert_eq!(
        samples.len(),
        Request::OPS.len(),
        "one sample per advertised op"
    );
    for r in &samples {
        assert_eq!(serde_json::to_value(r).unwrap()["op"], r.op());
        assert!(
            Request::OPS.contains(&r.op()),
            "{} is not advertised",
            r.op()
        );
    }
}

#[test]
fn an_error_code_from_a_newer_peer_decodes_as_unknown() {
    let e: ApiError =
        serde_json::from_value(json!({ "code": "rate_limited", "message": "slow down" })).unwrap();
    assert_eq!(e.code, ErrorCode::Unknown);
}

#[test]
fn a_corrupt_payload_is_reported_with_its_seq() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    call(&b, json!({ "op": "mq.topic_create", "topic": "t" })).unwrap();
    call(
        &b,
        json!({ "op": "mq.group_create", "topic": "t", "group": "g", "from_start": true }),
    )
    .unwrap();
    let Response::Sent { seq } = call(
        &b,
        json!({ "op": "mq.send", "topic": "t", "key": "k", "kind": "a", "payload": 1, "producer": "p" }),
    )
    .unwrap() else {
        panic!("expected Sent")
    };
    db.write_txn("t", move |c, _| {
        c.execute("UPDATE mq_messages SET payload = 'x' WHERE seq = ?1", [seq])?;
        Ok(())
    })
    .unwrap();
    let err = call(
        &b,
        json!({ "op": "mq.poll", "topic": "t", "group": "g", "max": 5 }),
    )
    .unwrap_err();
    assert_eq!((err.code, err.seq), (ErrorCode::CorruptPayload, Some(seq)));
    assert_eq!(serde_json::to_value(&err).unwrap()["seq"], seq);
}

#[test]
fn a_locked_database_is_busy_not_internal() {
    let err = ApiError::from(jkb_core::Error::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(5), // SQLITE_BUSY
        Some("database is locked".to_owned()),
    )));
    assert_eq!(err.code, ErrorCode::Busy);
}

#[test]
fn the_local_backend_serves_the_queue_end_to_end() {
    let b = backend();
    assert_eq!(
        call(
            &b,
            json!({ "op": "mq.topic_create", "topic": "t", "spec": { "max_messages": 50 } })
        )
        .unwrap(),
        Response::Created { created: true }
    );
    assert_eq!(
        call(
            &b,
            json!({ "op": "mq.group_create", "topic": "t", "group": "g", "from_start": true })
        )
        .unwrap(),
        Response::Created { created: true }
    );
    let Response::Sent { seq } = call(
        &b,
        json!({ "op": "mq.send", "topic": "t", "key": "k", "kind": "a.b", "payload": [1, 2], "producer": "p" }),
    )
    .unwrap() else {
        panic!("expected Sent")
    };
    let Response::Messages { messages } = call(
        &b,
        json!({ "op": "mq.poll", "topic": "t", "group": "g", "max": 10 }),
    )
    .unwrap() else {
        panic!("expected Messages")
    };
    assert_eq!(messages.len(), 1);
    assert_eq!(
        (messages[0].seq, &messages[0].payload),
        (seq, &json!([1, 2]))
    );
    assert_eq!(
        call(
            &b,
            json!({ "op": "mq.ack", "topic": "t", "group": "g", "seq": seq })
        )
        .unwrap(),
        Response::Position { position: seq }
    );
    let Response::Topics { topics } = call(&b, json!({ "op": "mq.inspect" })).unwrap() else {
        panic!("expected Topics")
    };
    assert_eq!(
        (topics[0].max_messages, topics[0].groups[0].backlog),
        (50, 0)
    );
    let Response::Messages { messages } =
        call(&b, json!({ "op": "mq.tail", "topic": "t", "limit": 5 })).unwrap()
    else {
        panic!("expected Messages")
    };
    assert_eq!(messages.len(), 1);
    assert!(matches!(
        call(&b, json!({ "op": "mq.compact", "force": true })).unwrap(),
        Response::Compacted {
            topics_compacted: 1,
            ..
        }
    ));
}

#[test]
fn queue_refusals_carry_a_stable_code() {
    let b = backend();
    let err = call(
        &b,
        json!({ "op": "mq.send", "topic": "nope", "key": "k", "kind": "a", "payload": null, "producer": "p" }),
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoSuchTopic);
    call(
        &b,
        json!({ "op": "mq.topic_create", "topic": "t", "spec": { "max_messages": 1 } }),
    )
    .unwrap();
    call(&b, json!({ "op": "mq.send", "topic": "t", "key": "k", "kind": "a", "payload": 1, "producer": "p" }))
        .unwrap();
    let full = call(
        &b,
        json!({ "op": "mq.send", "topic": "t", "key": "k", "kind": "a", "payload": 2, "producer": "p" }),
    )
    .unwrap_err();
    assert_eq!(full.code, ErrorCode::QueueFull);
    let err = call(
        &b,
        json!({ "op": "mq.poll", "topic": "t", "group": "ghost", "max": 1 }),
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoSuchGroup);
    // The wire form of an error is stable too.
    assert_eq!(serde_json::to_value(&full).unwrap()["code"], "queue_full");
}

#[test]
fn an_omitted_spec_field_takes_the_default() {
    let spec = SpecInput {
        max_messages: Some(3),
        ..SpecInput::default()
    }
    .resolve();
    assert_eq!(spec.max_messages, 3);
    assert_eq!(spec.max_bytes, jkb_core::mq::DEFAULT_MAX_BYTES);
    assert_eq!(spec.group_idle_ms, jkb_core::mq::DEFAULT_GROUP_IDLE_MS);
}

#[test]
fn an_idle_poll_is_answered_without_a_write() {
    // Observed through last_poll_at: the (writing) core poll refreshes it every time, so an unchanged
    // value proves the backend answered the second poll from a read.
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    call(&b, json!({ "op": "mq.topic_create", "topic": "t" })).unwrap();
    call(
        &b,
        json!({ "op": "mq.group_create", "topic": "t", "group": "g" }),
    )
    .unwrap();
    let last_poll = || {
        db.read(|c| {
            Ok(c.query_row("SELECT last_poll_at FROM mq_groups", [], |r| {
                r.get::<_, Option<i64>>(0)
            })?)
        })
        .unwrap()
    };
    call(
        &b,
        json!({ "op": "mq.poll", "topic": "t", "group": "g", "max": 5 }),
    )
    .unwrap();
    let first = last_poll();
    assert!(first.is_some(), "the first poll records itself");
    std::thread::sleep(std::time::Duration::from_millis(5));
    call(
        &b,
        json!({ "op": "mq.poll", "topic": "t", "group": "g", "max": 5 }),
    )
    .unwrap();
    assert_eq!(last_poll(), first, "an idle poll took the write lock");
}

/// The notification ops, served in-process: the wire names of events and effects, the record the
/// sweep reads, and the refusals a container's hook can meet.
#[test]
fn notify_ops_run_the_machine_and_report_what_it_did() {
    let b = backend();
    call(
        &b,
        json!({ "op": "mq.topic_create", "topic": "claude/notify" }),
    )
    .unwrap();
    call(
        &b,
        json!({ "op": "mq.group_create", "topic": "claude/notify", "group": "g" }),
    )
    .unwrap();

    let posted = call(
        &b,
        json!({
            "op": "notify.event", "session": "s1", "event": "needed",
            "message": "Claude needs your permission to use Bash", "cwd": "/w/wt",
            "owner": "4242", "instance": "host"
        }),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&posted).unwrap(),
        json!({
            "result": "notified", "state": "awaiting_tool", "moved": true,
            "effects": ["post", "remember"], "sent": 1
        })
    );

    let Response::Sessions { sessions } =
        call(&b, json!({ "op": "notify.open_sessions" })).unwrap()
    else {
        panic!("expected sessions")
    };
    assert_eq!((sessions.len(), sessions[0].owner.as_str()), (1, "4242"));

    // A mismatched tool does not move it; the refusal says why.
    let Response::Notified { moved, refusal, .. } = call(
        &b,
        json!({ "op": "notify.event", "session": "s1", "event": "tool_finished", "tool": "Read" }),
    )
    .unwrap() else {
        panic!("expected notified")
    };
    assert!(!moved);
    assert!(refusal.is_some());

    let Response::Notified { effects, .. } = call(
        &b,
        json!({ "op": "notify.gone", "session": "s1", "owner": "4242", "instance": "host" }),
    )
    .unwrap() else {
        panic!("expected notified")
    };
    assert_eq!(effects, ["withdraw", "forget"]);

    // `session_gone` is not a hook event on the wire, and a malformed session is `invalid`.
    assert!(serde_json::from_value::<Request>(
        json!({ "op": "notify.event", "session": "s1", "event": "session_gone" })
    )
    .is_err());
    let err = call(
        &b,
        json!({ "op": "notify.event", "session": "../x", "event": "needed" }),
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
}

#[test]
fn only_an_answer_that_sent_something_wakes_subscribers() {
    let b = backend();
    call(
        &b,
        json!({ "op": "mq.topic_create", "topic": "claude/notify" }),
    )
    .unwrap();
    let absent = call(
        &b,
        json!({ "op": "notify.event", "session": "s", "event": "tool_finished", "tool": "Bash" }),
    )
    .unwrap();
    assert!(
        !absent.announces_a_send(),
        "the commonest event there is — a tool finishing with nothing on screen — sends nothing: {absent:?}"
    );
    call(
        &b,
        json!({ "op": "mq.group_create", "topic": "claude/notify", "group": "g" }),
    )
    .unwrap();
    let swept = call(
        &b,
        json!({ "op": "notify.event", "session": "s", "event": "turn_ended" }),
    )
    .unwrap();
    assert!(swept.announces_a_send(), "{swept:?}");
    let sent = call(
        &b,
        json!({ "op": "mq.send", "topic": "claude/notify", "key": "k", "kind": "k", "payload": 1, "producer": "p" }),
    )
    .unwrap();
    assert!(sent.announces_a_send());
    assert!(!call(&b, json!({ "op": "notify.open_sessions" }))
        .unwrap()
        .announces_a_send());
    assert!(!call(&b, json!({ "op": "mq.inspect" }))
        .unwrap()
        .announces_a_send());
}

// ---- the agent read set (tasks S6.1) ----

/// A database with a mount at `/Users/u/repos/jkb` → `repos/jkb`, a document under it holding two
/// lines that say "needle", and a task `task:parent` under `tasks/repos/jkb` with one open subtask.
fn read_fixture() -> LocalBackend {
    use jkb_core::{item, mount, ns, placement, task};
    use jkb_types::{ConflictPolicy, PlacementRole, SyncMode};
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        let repo = ns::ensure(c, "repos/jkb")?;
        mount::create(
            c,
            m,
            repo,
            "file:///Users/u/repos/jkb",
            SyncMode::Bidirectional,
            "document",
            None,
            None,
            ConflictPolicy::Manual,
        )?;
        let doc = item::upsert(
            c,
            m,
            &item::NewItem {
                uid: "doc:a".into(),
                kind: "document".into(),
                content: Some("title\nthe needle here\nnothing\nNEEDLE again".into()),
                content_hash: None,
                mime: None,
            },
        )?;
        placement::place(c, m, doc, repo, PlacementRole::Primary, 0)?;
        let mut parent = task::NewTask::new("task:parent", "Parent task\nbody");
        parent.home = "tasks/repos/jkb".into();
        let parent = task::create(c, m, &parent)?;
        let mut child = task::NewTask::new("task:child", "Child task");
        child.home = "tasks/repos/jkb".into();
        let child = task::create(c, m, &child)?;
        task::add_subtask(c, m, parent, child)
    })
    .unwrap();
    LocalBackend::new(db)
}

#[test]
fn a_container_directory_finds_the_mount_the_host_recorded() {
    let b = read_fixture();
    let found =
        b.db.read(|c| {
            super::kb::ambient(
                c,
                "/home/vscode/repos/jkb/crates",
                "/home/vscode",
                Some(std::path::Path::new("/Users/u")),
            )
        })
        .unwrap();
    assert_eq!(found.as_deref(), Some("repos/jkb"));
    // Through the op, in one process: the client's home is this process's, so nothing is re-rooted
    // and the host's own path is what matches.
    let home = std::env::var("HOME").unwrap_or_default();
    let r = call(
        &b,
        json!({ "op": "kb.ambient", "cwd": "/Users/u/repos/jkb", "home": home }),
    )
    .unwrap();
    assert_eq!(
        r,
        Response::Ambient {
            namespace: Some("repos/jkb".into())
        }
    );
}

#[test]
fn a_search_that_would_embed_is_refused_by_a_backend_with_no_embedder() {
    let b = read_fixture();
    for route in ["vector", "hybrid"] {
        let e = call(
            &b,
            json!({ "op": "kb.search", "dsl": "needle", "route": route, "limit": 5 }),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Unsupported, "{route}: {e:?}");
    }
    let e = call(
        &b,
        json!({ "op": "kb.search", "dsl": "needle", "route": "hybrid", "limit": usize::MAX }),
    )
    .unwrap_err();
    assert_eq!(
        e.code,
        ErrorCode::Invalid,
        "a limit the fusion would overflow on: {e:?}"
    );
    let Response::SearchHits { hits } = call(
        &b,
        json!({ "op": "kb.search", "dsl": "needle", "route": "fts", "limit": 5 }),
    )
    .unwrap() else {
        panic!("a search answers with hits")
    };
    assert_eq!(
        hits.iter()
            .filter_map(|h| h.row.as_ref().map(|r| r.uid.as_str()))
            .collect::<Vec<_>>(),
        ["doc:a"],
        "FTS needs no embedder"
    );
}

#[test]
fn grep_answers_with_the_matching_lines_and_cat_with_the_body() {
    let b = read_fixture();
    let Response::GrepHits { hits } = call(
        &b,
        json!({ "op": "kb.grep", "pattern": "needle", "scope": "repos/jkb", "ignore_case": true }),
    )
    .unwrap() else {
        panic!("grep answers with hits")
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0]
            .lines
            .iter()
            .map(|l| (l.line, l.text.as_str()))
            .collect::<Vec<_>>(),
        [(2, "the needle here"), (4, "NEEDLE again")]
    );
    let Response::GrepHits { hits } = call(
        &b,
        json!({ "op": "kb.grep", "pattern": "needle", "scope": "tasks" }),
    )
    .unwrap() else {
        panic!("grep answers with hits")
    };
    assert!(hits.is_empty(), "the scope is honoured");

    assert_eq!(
        call(&b, json!({ "op": "kb.cat", "uid": "doc:a" })).unwrap(),
        Response::Content {
            content: "title\nthe needle here\nnothing\nNEEDLE again".into()
        }
    );
    let e = call(&b, json!({ "op": "kb.cat", "uid": "doc:nope" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(e.message.contains("no item with uid `doc:nope`"), "{e:?}");
}

#[test]
fn a_query_scopes_by_default_only_when_it_names_no_scope_and_counts_past_its_limit() {
    let b = read_fixture();
    let uids = |r: Response| match r {
        Response::Items { items } => items.into_iter().map(|i| i.uid).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        uids(
            call(
                &b,
                json!({ "op": "kb.query", "dsl": "", "default_scope": "repos/jkb" })
            )
            .unwrap()
        ),
        ["doc:a"]
    );
    let mut named = uids(
        call(
            &b,
            json!({ "op": "kb.query", "dsl": "ns:tasks/**", "default_scope": "repos/jkb" }),
        )
        .unwrap(),
    );
    named.sort();
    assert_eq!(named, ["task:child", "task:parent"], "a named scope wins");
    assert_eq!(
        call(
            &b,
            json!({ "op": "kb.query", "dsl": "kind:task", "limit": 1, "count": true }),
        )
        .unwrap(),
        Response::Count { count: 2 }
    );
    let e = call(&b, json!({ "op": "kb.query", "dsl": "ns:" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "a malformed query: {e:?}");
}

#[test]
fn a_task_shows_with_its_subtasks_and_holds_the_parent_off_the_frontier() {
    let b = read_fixture();
    let Response::Task { task } = call(&b, json!({ "op": "task.show", "uid": "parent" })).unwrap()
    else {
        panic!("task.show answers with a task")
    };
    assert_eq!(task.item.uid, "task:parent", "a bare slug resolves");
    assert_eq!(task.item.namespace.as_deref(), Some("tasks/repos/jkb"));
    assert_eq!(
        task.subtasks
            .iter()
            .map(|s| s.uid.as_str())
            .collect::<Vec<_>>(),
        ["task:child"]
    );
    let e = call(&b, json!({ "op": "task.show", "uid": "nope" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);

    let Response::Items { items } = call(
        &b,
        json!({ "op": "task.ready", "dsl": "", "default_scope": "tasks/repos/jkb" }),
    )
    .unwrap() else {
        panic!("task.ready answers with items")
    };
    assert_eq!(
        items.iter().map(|i| i.uid.as_str()).collect::<Vec<_>>(),
        ["task:child"]
    );

    let Response::Children { children } =
        call(&b, json!({ "op": "task.subtasks", "uid": "parent" })).unwrap()
    else {
        panic!("task.subtasks answers with children")
    };
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].reference, "task:child");
}

#[test]
fn ls_and_tree_list_the_same_children() {
    let b = read_fixture();
    let Response::Listing { rows } = call(
        &b,
        json!({ "op": "kb.ls", "path": "repos", "recursive": true }),
    )
    .unwrap() else {
        panic!("kb.ls answers with a listing")
    };
    assert_eq!(
        rows.iter()
            .map(|r| (r.parent.as_deref(), r.child.reference.as_str()))
            .collect::<Vec<_>>(),
        [(Some("repos"), "repos/jkb"), (Some("repos/jkb"), "doc:a")]
    );
    let Response::Tree { nodes } = call(
        &b,
        json!({ "op": "kb.tree", "path": "tasks/repos", "depth": 4 }),
    )
    .unwrap() else {
        panic!("kb.tree answers with nodes")
    };
    let jkb = &nodes[0];
    assert_eq!(jkb.child.reference, "tasks/repos/jkb");
    let parent = &jkb.children[0];
    assert_eq!(
        (parent.child.subtask_count, parent.child.open_subtask_count),
        (Some(1), Some(1))
    );
    assert_eq!(
        parent.children[0].child.reference, "task:child",
        "a tree descends into a container, not only a namespace"
    );
    assert!(
        serde_json::from_value::<Request>(json!({ "op": "kb.ls", "path": "x", "depth": 1 }))
            .is_err(),
        "an unknown field on a read is refused like any other"
    );
}

#[test]
fn a_search_score_crosses_the_wire_exactly() {
    // Measured: without serde_json's `float_roundtrip`, 59,200 of 400,000 random finite f64s parsed
    // back one ulp off — this one among them — so a score the daemon sent printed differently from
    // the host's. The workspace enables the feature; this pins that it stays enabled.
    let score = 1.071_566_039_146_582_6e-75_f64;
    let hit = super::kb::SearchHit {
        item: 1,
        row: None,
        route: "fts".to_owned(),
        score,
        distance: Some(0.123_456_79),
        namespace: None,
        source_document: None,
        context: Vec::new(),
    };
    let wire = serde_json::to_string(&Response::SearchHits {
        hits: vec![hit.clone()],
    })
    .unwrap();
    let Response::SearchHits { hits } = serde_json::from_str(&wire).unwrap() else {
        panic!("round-trips as search hits")
    };
    assert_eq!(hits[0].score.to_bits(), score.to_bits(), "{wire}");
    assert_eq!(hits[0], hit);
}

#[test]
fn a_tree_over_a_self_listing_node_stops_at_the_cap() {
    use jkb_core::{item, ns, placement};
    use jkb_types::PlacementRole;
    // A document whose uid is the path of the namespace it is placed in, with a chunk so it expands:
    // listing it by reference lists that namespace again.
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        let loop_ns = ns::ensure(c, "loop")?;
        let new = |uid: &str, kind: &str| item::NewItem {
            uid: uid.into(),
            kind: kind.into(),
            content: Some(uid.into()),
            content_hash: None,
            mime: None,
        };
        let doc = item::upsert(c, m, &new("loop", "document"))?;
        placement::place(c, m, doc, loop_ns, PlacementRole::Primary, 0)?;
        let chunk = item::upsert(c, m, &new("loop#0", "chunk"))?;
        jkb_core::edge::link(c, m, chunk, doc, jkb_types::EdgeType::DerivedFrom, None)
    })
    .unwrap();
    let b = LocalBackend::new(db);
    let Response::Tree { nodes } = call(&b, json!({ "op": "kb.tree", "path": "loop" })).unwrap()
    else {
        panic!("kb.tree answers with nodes")
    };
    let mut depth = 0;
    let mut level = &nodes;
    while let Some(first) = level.first() {
        depth += 1;
        level = &first.children;
    }
    assert_eq!(
        depth,
        super::kb::MAX_TREE_DEPTH + 1,
        "the cap, not the stack, ends it"
    );
}
