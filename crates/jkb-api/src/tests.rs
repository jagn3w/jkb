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

#[allow(clippy::too_many_lines)] // one sample per op
/// One request per op.
fn samples() -> Vec<Request> {
    vec![
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
        Request::SessionStarted {
            session: "s".into(),
            source: "startup".into(),
            pid: "1".into(),
            instance: "h".into(),
            cwd: String::new(),
        },
        Request::SessionEnded {
            session: "s".into(),
            reason: "other".into(),
            pid: "1".into(),
            instance: "h".into(),
        },
        Request::SessionGone {
            session: "s".into(),
            pid: "1".into(),
            instance: "h".into(),
        },
        Request::SessionList {
            all: false,
            after: None,
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
            order: super::kb::QueryOrder::Id,
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
            mode: super::kb::GrepMode::Lines,
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
        Request::TaskWhy { uid: "u".into() },
        Request::TaskAdd(super::tasks::AddAsk {
            text: "t".into(),
            home: None,
            under: None,
            backlog: false,
            global_backlog: false,
            sync: false,
            managed: false,
            cwd: String::new(),
            client_home: String::new(),
        }),
        Request::TaskSet {
            uid: "u".into(),
            status: None,
            priority: Some(1),
            due: None,
        },
        Request::TaskEdit {
            uid: "u".into(),
            text: "t".into(),
            append: false,
        },
        Request::TaskTag {
            uid: "u".into(),
            facet_value: "a=b".into(),
            mode: super::tasks::TagMode::Add,
        },
        Request::TaskDepend {
            uid: "u".into(),
            dep: "d".into(),
        },
        Request::TaskUndepend {
            uid: "u".into(),
            dep: "d".into(),
        },
        Request::TaskPlace {
            uid: "u".into(),
            ns: "n".into(),
            home: false,
        },
        Request::TaskUnplace {
            uid: "u".into(),
            ns: "n".into(),
        },
        Request::TaskBind {
            uid: "u".into(),
            sync: None,
        },
        Request::TaskClaim {
            uid: "u".into(),
            owner: "agent:x".into(),
        },
        Request::TaskRelease {
            uid: "u".into(),
            owner: "agent:x".into(),
        },
        Request::IngestText(super::ingest::IngestAsk {
            text: "t".into(),
            mime: "text/plain".into(),
            namespace: "inbox".into(),
            raw: None,
        }),
        Request::TaskFacts { uid: "u".into() },
        Request::TaskByBranch { repo: "r".into() },
        Request::TaskStart(super::sessions::StartAsk {
            uid: "u".into(),
            take: None,
            keep: Some("agent:x".into()),
            place: super::sessions::Place {
                branch: "b".into(),
                repo: "r".into(),
                onto: None,
            },
        }),
        Request::TaskTake(super::sessions::TakeAsk {
            uid: "u".into(),
            take: super::sessions::Take {
                owner: "agent:x".into(),
                displace: None,
            },
            place: super::sessions::Place {
                branch: "b".into(),
                repo: "r".into(),
                onto: Some("o".into()),
            },
        }),
        Request::TaskLocate {
            uid: "u".into(),
            owner: "agent:x".into(),
            place: super::sessions::Place {
                branch: "b".into(),
                repo: "r".into(),
                onto: None,
            },
        },
        Request::TaskAbandon {
            uid: "u".into(),
            observed: None,
        },
        Request::RepoGate { repo: "r".into() },
        Request::SessionState {
            session: "s".into(),
        },
        Request::RemovalAdd {
            removal: super::removals::Removal {
                worktree: "~/repos/p/.jkb/work/s".into(),
                repo_root: "~/repos/p".into(),
                branch: String::new(),
                uid: String::new(),
                delete_branch: false,
                accept_dirty: false,
                recorded_at: 0,
                head: None,
                archive: None,
                archived_at: None,
            },
        },
        Request::RemovalList { after: None },
        Request::RemovalArchived {
            id: 1,
            archive: "~/repos/p/.jkb/archive/s".into(),
            at: 0,
        },
        Request::RemovalCancel { ids: vec![1] },
        Request::RemovalDrop { id: 1 },
        Request::LeaseGet {
            name: "removal-sweep".into(),
        },
        Request::LeaseTake {
            name: "removal-sweep".into(),
            holder: "host:1 n".into(),
            displace: None,
        },
        Request::LeaseRelease {
            name: "removal-sweep".into(),
            holder: "host:1 n".into(),
        },
        Request::LeaseBreak {
            name: "removal-sweep".into(),
        },
        Request::TaskLand {
            uid: "u".into(),
            landed: super::sessions::Landed {
                branch: "b".into(),
                onto: "o".into(),
                head: None,
            },
        },
        Request::TaskLanded {
            uid: "u".into(),
            landed: super::sessions::Landed {
                branch: "b".into(),
                onto: "o".into(),
                head: None,
            },
        },
        Request::TaskReviewFindings {
            namespaces: vec!["reviews/x".into()],
        },
    ]
}

#[test]
fn every_op_names_its_own_wire_tag_and_is_advertised() {
    let samples = samples();
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
    let Response::SearchHits { hits, .. } = call(
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
    let grep = |q: serde_json::Value| match call(&b, q).unwrap() {
        Response::GrepHits { answer } => answer,
        other => panic!("grep answers with hits: {other:?}"),
    };
    let answer = grep(
        json!({ "op": "kb.grep", "pattern": "needle", "scope": "repos/jkb", "ignore_case": true }),
    );
    let hits = &answer.hits;
    assert_eq!((hits.len(), answer.count, answer.truncated), (1, 1, false));
    assert_eq!(
        hits[0]
            .lines
            .iter()
            .map(|l| (l.line, l.text.as_str()))
            .collect::<Vec<_>>(),
        [(2, "the needle here"), (4, "NEEDLE again")]
    );
    let answer = grep(json!({ "op": "kb.grep", "pattern": "needle", "scope": "tasks" }));
    assert!(
        answer.hits.is_empty() && answer.count == 0,
        "the scope is honoured"
    );
    let names = grep(json!({ "op": "kb.grep", "pattern": "needle", "mode": "names" }));
    assert_eq!(names.count, 1);
    assert!(names.hits[0].lines.is_empty(), "names carry no lines");
    let count = grep(json!({ "op": "kb.grep", "pattern": "needle", "mode": "count" }));
    assert_eq!(
        (count.count, count.hits.len()),
        (1, 0),
        "a count carries no hits"
    );
    let e = call(&b, json!({ "op": "kb.grep", "pattern": "" })).unwrap_err();
    assert_eq!(
        e.code,
        ErrorCode::Invalid,
        "an empty pattern matches every line: {e:?}"
    );

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
        Response::Items { items, .. } => items.into_iter().map(|i| i.uid).collect::<Vec<_>>(),
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
    let Response::Task { task, .. } =
        call(&b, json!({ "op": "task.show", "uid": "parent" })).unwrap()
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

    let Response::Items { items, .. } = call(
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

    let Response::Children { children, .. } =
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
    let Response::Listing { rows, .. } = call(
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
    let Response::Tree { nodes, .. } = call(
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
        truncated: false,
    })
    .unwrap();
    let Response::SearchHits { hits, .. } = serde_json::from_str(&wire).unwrap() else {
        panic!("round-trips as search hits")
    };
    assert_eq!(hits[0].score.to_bits(), score.to_bits(), "{wire}");
    assert_eq!(hits[0], hit);
}

/// Items and namespaces for the tree guards: `new(uid)` is a document with one chunk, so it expands.
fn expanding_document(
    c: &rusqlite::Connection,
    m: &jkb_core::WriteMeta,
    uid: &str,
) -> jkb_core::Result<jkb_types::ItemId> {
    use jkb_core::item;
    let new = |uid: String, kind: &str| item::NewItem {
        content: Some(uid.clone()),
        uid,
        kind: kind.into(),
        content_hash: None,
        mime: None,
    };
    let doc = item::upsert(c, m, &new(uid.to_owned(), "document"))?;
    let chunk = item::upsert(c, m, &new(format!("{uid}#0"), "chunk"))?;
    jkb_core::edge::link(c, m, chunk, doc, jkb_types::EdgeType::DerivedFrom, None)?;
    Ok(doc)
}

fn count(nodes: &[super::kb::TreeNode]) -> usize {
    nodes.iter().map(|n| 1 + count(&n.children)).sum()
}

fn tree_of(b: &LocalBackend, path: &str) -> Vec<super::kb::TreeNode> {
    match call(b, json!({ "op": "kb.tree", "path": path })).unwrap() {
        Response::Tree { nodes, .. } => nodes,
        other => panic!("kb.tree answers with nodes: {other:?}"),
    }
}

#[test]
fn a_tree_does_not_descend_into_a_node_that_lists_its_own_ancestor() {
    use jkb_core::{ns, placement};
    use jkb_types::PlacementRole;
    // Namespaces `a` and `b`, and documents with those uids placed in both: listing either document by
    // its reference lists a namespace holding both again. Measured before the fix: without the ancestor
    // check this walked to the depth cap with a fan-out of two at every level.
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        let spaces = [ns::ensure(c, "a")?, ns::ensure(c, "b")?];
        for uid in ["a", "b"] {
            let doc = expanding_document(c, m, uid)?;
            for (i, space) in spaces.iter().enumerate() {
                placement::place(
                    c,
                    m,
                    doc,
                    *space,
                    PlacementRole::Reference,
                    i64::try_from(i).unwrap(),
                )?;
            }
        }
        Ok(())
    })
    .unwrap();
    let b = LocalBackend::new(db);
    let started = std::time::Instant::now();
    let nodes = tree_of(&b, "a");
    assert!(count(&nodes) <= 6, "{} nodes: {nodes:#?}", count(&nodes));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn a_tree_stops_at_its_node_cap_and_says_so() {
    use jkb_core::ns;
    // More child namespaces than the cap, each with a namespace of its own under it.
    let db = Db::open_in_memory().unwrap();
    let width = super::kb::MAX_TREE_NODES + 50;
    db.write_txn("t", move |c, _| {
        for i in 0..width {
            ns::ensure(c, &format!("w/{i:05}/x"))?;
        }
        Ok(())
    })
    .unwrap();
    let b = LocalBackend::new(db);
    let Response::Tree {
        nodes,
        truncated,
        at_node_cap,
    } = call(&b, json!({ "op": "kb.tree", "path": "w" })).unwrap()
    else {
        panic!("kb.tree answers with nodes")
    };
    assert!(
        truncated && at_node_cap,
        "cut, and by the cap rather than the budget"
    );
    assert_eq!(
        count(&nodes),
        super::kb::MAX_TREE_NODES,
        "the cap counts every node listed, siblings included"
    );
}

#[test]
fn a_tree_at_the_depth_cap_decodes_through_the_wire() {
    use jkb_core::ns;
    // Each tree level is two levels of JSON nesting and serde_json refuses past 128; a cap above it
    // printed on the host and failed to decode through the daemon.
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, _| {
        ns::ensure(c, &vec!["d"; super::kb::MAX_TREE_DEPTH + 10].join("/"))?;
        Ok(())
    })
    .unwrap();
    let b = LocalBackend::new(db);
    let response = call(&b, json!({ "op": "kb.tree", "path": "d" })).unwrap();
    let Response::Tree { nodes, .. } = &response else {
        panic!("kb.tree answers with nodes")
    };
    let mut depth = 0;
    let mut level = nodes;
    while let Some(first) = level.first() {
        depth += 1;
        level = &first.children;
    }
    assert_eq!(depth, super::kb::MAX_TREE_DEPTH + 1, "the cap ends it");
    let wire = serde_json::to_string(&response).unwrap();
    let back: Response = serde_json::from_str(&wire).expect("a capped tree decodes");
    assert_eq!(back, response);
}

#[test]
fn a_search_asking_for_a_whole_document_of_context_is_refused() {
    let b = read_fixture();
    let e = call(
        &b,
        json!({ "op": "kb.search", "dsl": "needle", "route": "fts", "limit": 5,
                "context": super::kb::MAX_SEARCH_CONTEXT + 1 }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    assert!(call(
        &b,
        json!({ "op": "kb.search", "dsl": "needle", "route": "fts", "limit": 5,
                "context": super::kb::MAX_SEARCH_CONTEXT }),
    )
    .is_ok());
}

#[test]
fn every_listing_read_stays_within_its_budget_and_says_when_it_was_cut() {
    const BUDGET: usize = 4096;
    use jkb_core::{item, ns, placement, task};
    use jkb_types::PlacementRole;
    // Enough of everything that each read's full answer is well over the budget: many items, a
    // document of many short matching lines (the grep answer whose per-line JSON outweighs its text),
    // documents with bodies large enough that one search hit's context is over it on its own, and a
    // task with many subtasks.
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        let space = ns::ensure(c, "big")?;
        for i in 0..200 {
            let doc = item::upsert(
                c,
                m,
                &item::NewItem {
                    uid: format!("doc:{i:03}"),
                    kind: "document".into(),
                    content: Some(format!("needle {i}\n{}", "x\n".repeat(3000))),
                    content_hash: None,
                    mime: None,
                },
            )?;
            placement::place(c, m, doc, space, PlacementRole::Primary, i)?;
        }
        let mut parent = task::NewTask::new("task:big", "Big");
        parent.home = "big".into();
        let parent = task::create(c, m, &parent)?;
        for i in 0..200 {
            let mut child = task::NewTask::new(format!("task:sub-{i:03}"), format!("sub {i}"));
            child.home = "big".into();
            let child = task::create(c, m, &child)?;
            task::add_subtask(c, m, parent, child)?;
        }
        Ok(())
    })
    .unwrap();
    // A history long enough to cut: each claim and release is a transition.
    let host = LocalBackend::new(db.clone());
    for round in 0..40 {
        let owner = format!("agent:{round}");
        call(
            &host,
            json!({ "op": "task.claim", "uid": "big", "owner": owner }),
        )
        .unwrap();
        call(
            &host,
            json!({ "op": "task.set", "uid": "big", "status": "open" }),
        )
        .unwrap();
        call(
            &host,
            json!({ "op": "task.release", "uid": "big", "owner": owner }),
        )
        .unwrap();
    }
    let b = LocalBackend::new(db).with_read_budget(BUDGET);
    for request in [
        json!({ "op": "kb.query", "dsl": "" }),
        json!({ "op": "kb.query", "dsl": "", "order": "updated_desc" }),
        json!({ "op": "task.ready", "dsl": "" }),
        json!({ "op": "kb.ls", "path": "big", "recursive": true, "all": true }),
        json!({ "op": "kb.tree", "path": "big", "all": true }),
        json!({ "op": "kb.grep", "pattern": "x" }),
        json!({ "op": "kb.grep", "pattern": "needle", "mode": "names" }),
        json!({ "op": "kb.search", "dsl": "needle", "route": "fts", "limit": 100, "context": 0 }),
        json!({ "op": "task.show", "uid": "big" }),
        json!({ "op": "task.subtasks", "uid": "big" }),
        json!({ "op": "task.why", "uid": "big" }),
    ] {
        let response = call(&b, request.clone()).unwrap();
        let wire = serde_json::to_string(&response).unwrap();
        // The budget counts what the rows serialize to; the envelope around them is not charged.
        assert!(
            wire.len() <= BUDGET + 256,
            "{request}: {} bytes over a {BUDGET}-byte budget",
            wire.len()
        );
        let v: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(
            v["truncated"], true,
            "{request}: cut, and says so: {wire:.300}"
        );
        // A budget cut is not the node cap's: the CLI tells them apart, and only the budget's is
        // lifted on the host.
        assert!(
            v.get("at_node_cap").is_none(),
            "{request}: a budget cut named as the node cap: {wire:.300}"
        );
    }
    let Response::GrepHits { answer } =
        call(&b, json!({ "op": "kb.grep", "pattern": "needle" })).unwrap()
    else {
        panic!("grep answers with hits")
    };
    assert_eq!(answer.count, 200, "counting goes on past the budget");
    // Unbounded, the same reads are whole.
    let whole = LocalBackend::new(b.db.clone());
    let Response::Items { items, truncated } =
        call(&whole, json!({ "op": "kb.query", "dsl": "kind:document" })).unwrap()
    else {
        panic!("kb.query answers with items")
    };
    assert_eq!((items.len(), truncated), (200, false));
}

#[test]
fn recent_orders_and_limits_on_the_server() {
    let b = read_fixture();
    // Touch the task created last, so the newest is not also the lowest id — by id, `doc:a` is first.
    std::thread::sleep(std::time::Duration::from_millis(5));
    b.db.write_txn("t", |c, m| {
        let id = jkb_core::item::id_for_uid(c, "task:child")?.unwrap();
        jkb_core::item::set_content(c, m, id, "fresh", None)
    })
    .unwrap();
    let Response::Items { items, .. } = call(
        &b,
        json!({ "op": "kb.query", "dsl": "", "limit": 1, "order": "updated_desc" }),
    )
    .unwrap() else {
        panic!("kb.query answers with items")
    };
    assert_eq!(
        items.iter().map(|i| i.uid.as_str()).collect::<Vec<_>>(),
        ["task:child"]
    );
}

#[test]
fn a_task_s_subtasks_carry_their_titles_not_their_bodies() {
    let b = read_fixture();
    b.db.write_txn("t", |c, m| {
        let id = jkb_core::item::id_for_uid(c, "task:child")?.unwrap();
        jkb_core::item::set_content(c, m, id, "\n\nChild title\nand a long body", None)
    })
    .unwrap();
    let Response::Task { task, .. } =
        call(&b, json!({ "op": "task.show", "uid": "parent" })).unwrap()
    else {
        panic!("task.show answers with a task")
    };
    assert_eq!(task.subtasks[0].title, "Child title");
}

#[test]
fn a_long_read_on_the_reader_does_not_hold_up_a_write() {
    // `Db` runs every call on one thread. Measured before `with_reader`: a read holding it made a
    // `notify.event` wait for the whole read — past the notification hook's 1 s budget.
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("jkb.db")).unwrap();
    let b = LocalBackend::new(db.clone()).with_reader(db.reader().unwrap());
    call(
        &b,
        json!({ "op": "mq.topic_create", "topic": "claude/notify" }),
    )
    .unwrap();
    let reads = b.reads.clone();
    let (held, holding) = std::sync::mpsc::channel();
    let long_read = std::thread::spawn(move || {
        reads
            .read(move |_| {
                held.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(1500));
                Ok(())
            })
            .unwrap();
    });
    holding.recv().unwrap();
    let started = std::time::Instant::now();
    call(
        &b,
        json!({ "op": "notify.event", "session": "s", "event": "needed" }),
    )
    .unwrap();
    let took = started.elapsed();
    long_read.join().unwrap();
    assert!(
        took < std::time::Duration::from_millis(700),
        "a write waited {took:?} behind a read"
    );

    // And the reader cannot write.
    let e = b
        .reads
        .write_txn("t", |c, _| {
            c.execute("DELETE FROM notify_sessions", [])?;
            Ok(())
        })
        .unwrap_err();
    assert!(e.to_string().contains("readonly"), "{e}");
}

// Which ops read is stated here, apart from the classification under test: filtering on
// `is_agent_read` itself dropped a read misclassed as a write from the very loop meant to catch it.
const READS: &[&str] = &[
    "kb.ambient",
    "kb.query",
    "kb.ls",
    "kb.tree",
    "kb.cat",
    "kb.grep",
    "kb.search",
    "task.ready",
    "task.show",
    "task.subtasks",
    "task.why",
    "task.facts",
    "task.by_branch",
    "repo.gate",
    "task.review_findings",
];

#[test]
fn every_op_is_served_on_the_connection_its_class_names() {
    // `Request::is_agent_read` alone chooses. A read left on the writer waits behind whatever holds it; a
    // write classed as a read fails on the `query_only` reader. So: no op is refused as a write to a
    // read-only database, and with the writer's thread held every read still answers at once.
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("jkb.db")).unwrap();
    let b = LocalBackend::new(db.clone()).with_reader(db.reader().unwrap());
    // Every op, not only those classed as writes: a write misclassed as a read is exactly the one that
    // would be missed by filtering on the class under test.
    for request in samples() {
        if let Err(e) = b.call(request.clone()) {
            assert!(
                !e.message.contains("readonly"),
                "{}: a write served on the reader: {e:?}",
                request.op()
            );
        }
    }
    let writer = b.db.clone();
    let (held, holding) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        writer
            .read(move |_| {
                held.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_secs(3));
                Ok(())
            })
            .unwrap();
    });
    holding.recv().unwrap();
    for request in samples() {
        assert_eq!(
            request.is_agent_read(),
            READS.contains(&request.op()),
            "{} is classed wrongly",
            request.op()
        );
    }
    for request in samples().into_iter().filter(|r| READS.contains(&r.op())) {
        let op = request.op();
        let at = std::time::Instant::now();
        let _ = b.call(request);
        assert!(
            at.elapsed() < std::time::Duration::from_secs(1),
            "{op} waited {:?} behind the writer",
            at.elapsed()
        );
    }
    blocker.join().unwrap();

    // And the other way: with the reader held — a container's long grep — every op outside the read
    // set still answers at once. The hook's `notify.open_sessions` sweep has 1 s.
    let reader = b.reads.clone();
    let (held, holding) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        reader
            .read(move |_| {
                held.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_secs(3));
                Ok(())
            })
            .unwrap();
    });
    holding.recv().unwrap();
    for request in samples().into_iter().filter(|r| !READS.contains(&r.op())) {
        let op = request.op();
        let at = std::time::Instant::now();
        let _ = b.call(request);
        assert!(
            at.elapsed() < std::time::Duration::from_secs(1),
            "{op} waited {:?} behind the reader",
            at.elapsed()
        );
    }
    blocker.join().unwrap();
}

#[test]
fn the_ready_frontier_is_in_priority_order_and_honours_its_limit() {
    use jkb_core::task;
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        for (uid, priority) in [
            ("task:low", Some(3)),
            ("task:none", None),
            ("task:high", Some(1)),
        ] {
            let mut t = task::NewTask::new(uid, uid);
            t.priority = priority;
            t.home = "tasks/f".into();
            task::create(c, m, &t)?;
        }
        Ok(())
    })
    .unwrap();
    let b = LocalBackend::new(db);
    let uids = |limit: Option<usize>| match call(
        &b,
        json!({ "op": "task.ready", "dsl": "", "default_scope": "tasks/f", "limit": limit }),
    )
    .unwrap()
    {
        Response::Items { items, .. } => items.into_iter().map(|i| i.uid).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(uids(None), ["task:high", "task:low", "task:none"]);
    assert_eq!(uids(Some(2)), ["task:high", "task:low"]);
}

#[test]
fn a_task_larger_than_the_budget_is_shown_whole_and_not_called_cut() {
    use jkb_core::task;
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        task::create(c, m, &task::NewTask::new("task:huge", "x".repeat(10_000)))?;
        Ok(())
    })
    .unwrap();
    let b = LocalBackend::new(db).with_read_budget(256);
    let Response::Task { task, truncated } =
        call(&b, json!({ "op": "task.show", "uid": "huge" })).unwrap()
    else {
        panic!("task.show answers with a task")
    };
    assert_eq!(task.item.content.map(|c| c.len()), Some(10_000));
    assert!(!truncated, "no subtask was dropped, so nothing was cut");
}

// ---- the task-mutate set (tasks S6.2) ----

/// Two `tasks` mounts, one under the client's file root and one outside it, with a file-backed task in
/// each (added through an unrooted backend, as the host would) and a managed task.
fn mutate_fixture() -> (Db, String, String, String) {
    use jkb_core::{mount, ns};
    use jkb_types::{ConflictPolicy, SyncMode};
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        for (path, dir) in [
            ("repos/in", "file:///Users/u/repos/in"),
            ("docs/out", "file:///Users/u/Documents/out"),
        ] {
            let id = ns::ensure(c, path)?;
            mount::create(
                c,
                m,
                id,
                dir,
                SyncMode::Bidirectional,
                "tasks",
                None,
                None,
                ConflictPolicy::Manual,
            )?;
        }
        Ok(())
    })
    .unwrap();
    let host = LocalBackend::new(db.clone());
    let add = |text: &str, managed: bool| match call(
        &host,
        json!({ "op": "task.add", "text": text, "managed": managed }),
    )
    .unwrap()
    {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    let inside = add("inside +repos/in", false);
    let outside = add("outside +docs/out", false);
    let managed = add("managed +docs/out", true);
    (db, inside, outside, managed)
}

fn rooted(db: &Db) -> LocalBackend {
    LocalBackend::new(db.clone()).with_file_roots(super::tasks::FileRoots::new(vec![
        std::path::PathBuf::from("/Users/u/repos"),
    ]))
}

#[test]
fn a_rooted_backend_refuses_every_write_to_a_task_filed_outside_its_roots() {
    let (db, inside, outside, managed) = mutate_fixture();
    let b = rooted(&db);
    let writes = |uid: &str| {
        vec![
            json!({ "op": "task.set", "uid": uid, "priority": 1 }),
            json!({ "op": "task.edit", "uid": uid, "text": "x", "append": true }),
            json!({ "op": "task.tag", "uid": uid, "facet_value": "size=s", "mode": "add" }),
            json!({ "op": "task.depend", "uid": uid, "dep": inside }),
            json!({ "op": "task.undepend", "uid": uid, "dep": inside }),
            json!({ "op": "task.place", "uid": uid, "ns": "elsewhere" }),
            json!({ "op": "task.unplace", "uid": uid, "ns": "elsewhere" }),
            json!({ "op": "task.bind", "uid": uid }),
            json!({ "op": "task.claim", "uid": uid, "owner": "agent:a" }),
            json!({ "op": "task.release", "uid": uid, "owner": "agent:a" }),
        ]
    };
    for request in writes(&outside) {
        let e = call(&b, request.clone()).unwrap_err();
        assert_eq!(e.code, ErrorCode::Forbidden, "{request}: {e:?}");
        assert!(
            e.message.contains("file:///Users/u/Documents/out/tasks.md"),
            "{e:?}"
        );
    }
    // The same writes to a task filed under the root, and to a managed one, are served.
    for uid in [&inside, &managed] {
        for request in writes(uid) {
            if request["op"] == "task.depend" && *uid == inside {
                continue; // a task cannot depend on itself
            }
            call(&b, request.clone()).unwrap_or_else(|e| panic!("{request}: {e:?}"));
        }
    }
    // And the unrooted host backend is refused nothing.
    let host = LocalBackend::new(db.clone());
    call(
        &host,
        json!({ "op": "task.set", "uid": outside, "priority": 2 }),
    )
    .unwrap();
}

#[test]
fn a_rooted_backend_neither_files_a_new_task_nor_binds_one_outside_its_roots() {
    let (db, inside, _outside, managed) = mutate_fixture();
    let b = rooted(&db);
    let e = call(&b, json!({ "op": "task.add", "text": "new +docs/out" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
    let Response::Added { added } = call(
        &b,
        json!({ "op": "task.add", "text": "new +docs/out", "managed": true }),
    )
    .unwrap() else {
        panic!("added")
    };
    assert_eq!(
        added.binding, None,
        "--managed files nothing, so it is served"
    );
    let Response::Added { added } =
        call(&b, json!({ "op": "task.add", "text": "new +repos/in" })).unwrap()
    else {
        panic!("added")
    };
    assert_eq!(
        added.binding.as_deref(),
        Some("file:///Users/u/repos/in/tasks.md"),
        "filed under the root: {added:?}"
    );
    // A file binding is refused through a rooted backend whatever the file: the design refuses a row
    // choosing a host file for sync, under the root or not.
    for uri in [
        "file:///Users/u/repos/in/tasks.md#x",
        "file:///Users/u/.ssh/tasks.md#x",
    ] {
        let e = call(
            &b,
            json!({ "op": "task.bind", "uid": managed, "sync": uri }),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Forbidden, "{uri}: {e:?}");
    }
    let host = LocalBackend::new(db);
    call(
        &host,
        json!({ "op": "task.bind", "uid": managed, "sync": "file:///Users/u/repos/in/tasks.md#y" }),
    )
    .unwrap();
    let _ = inside;
}

#[test]
#[allow(clippy::too_many_lines)] // one walk through every write
fn the_task_writes_do_what_their_commands_did() {
    let (db, inside, _outside, managed) = mutate_fixture();
    let b = rooted(&db);
    call(
        &b,
        json!({ "op": "task.set", "uid": managed, "status": "needs_review", "priority": 2, "due": "2026-10-01" }),
    )
    .unwrap();
    call(
        &b,
        json!({ "op": "task.edit", "uid": managed, "text": "more", "append": true }),
    )
    .unwrap();
    call(
        &b,
        json!({ "op": "task.tag", "uid": managed, "facet_value": "branch=work", "mode": "add" }),
    )
    .unwrap();
    let e = call(
        &b,
        json!({ "op": "task.tag", "uid": managed, "facet_value": "branch=--upload-pack=x", "mode": "add" }),
    )
    .unwrap_err();
    assert_eq!(
        e.code,
        ErrorCode::Invalid,
        "a ref git reads as an option: {e:?}"
    );
    let e = call(
        &b,
        json!({ "op": "task.tag", "uid": managed, "facet_value": "onto=main", "mode": "set" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    call(
        &b,
        json!({ "op": "task.depend", "uid": managed, "dep": inside }),
    )
    .unwrap();
    let Response::Task { task, .. } =
        call(&b, json!({ "op": "task.show", "uid": managed })).unwrap()
    else {
        panic!("task")
    };
    assert_eq!(task.item.status.as_deref(), Some("needs_review"));
    assert_eq!(task.item.priority, Some(2));
    assert!(task
        .item
        .content
        .as_deref()
        .is_some_and(|c| c.ends_with("\n\nmore")));
    assert!(task
        .item
        .tags
        .iter()
        .any(|t| t.facet == "branch" && t.value == "work"));

    let Response::Claimed { claimed } = call(
        &b,
        json!({ "op": "task.claim", "uid": inside, "owner": "agent:one" }),
    )
    .unwrap() else {
        panic!("claimed")
    };
    assert!(claimed.acquired, "{claimed:?}");
    let Response::Claimed { claimed } = call(
        &b,
        json!({ "op": "task.claim", "uid": inside, "owner": "agent:two" }),
    )
    .unwrap() else {
        panic!("claimed")
    };
    assert!(
        !claimed.acquired && claimed.refusal.is_some(),
        "a held task is refused, with the reason: {claimed:?}"
    );
    assert_eq!(
        call(
            &b,
            json!({ "op": "task.release", "uid": inside, "owner": "agent:two" })
        )
        .unwrap(),
        Response::Released { released: false },
        "one owner cannot drop another's claim"
    );
    assert_eq!(
        call(
            &b,
            json!({ "op": "task.release", "uid": inside, "owner": "agent:one" })
        )
        .unwrap(),
        Response::Released { released: true }
    );
    let Response::History { entries, .. } =
        call(&b, json!({ "op": "task.why", "uid": inside })).unwrap()
    else {
        panic!("history")
    };
    assert!(
        entries.iter().any(|e| e.event == "start"),
        "the claim went through the lifecycle: {entries:?}"
    );
    assert!(
        serde_json::from_value::<Request>(
            json!({ "op": "task.add", "text": "t", "owner": "sneaky" })
        )
        .is_err(),
        "task.add refuses a field it does not know, like every op"
    );
}

#[test]
fn every_task_write_a_client_can_send_is_refused_for_a_task_filed_outside_the_roots() {
    // Driven by the one-sample-per-op list, not a list of its own: an op added later without the file
    // guard fails here, because `every_op_names_its_own_wire_tag_and_is_advertised` makes it add a
    // sample first.
    let (db, inside, outside, _managed) = mutate_fixture();
    let b = rooted(&db);
    let mut checked = 0;
    for request in samples() {
        if !request.op().starts_with("task.") || request.is_agent_read() {
            continue;
        }
        let mut wire = serde_json::to_value(&request).unwrap();
        if wire.get("uid").is_some() {
            wire["uid"] = json!(outside);
        }
        if wire.get("dep").is_some() {
            wire["dep"] = json!(inside);
        }
        if request.op() == "task.add" {
            wire["text"] = json!("new +docs/out");
        }
        let e = call(&b, wire.clone()).unwrap_err();
        assert_eq!(e.code, ErrorCode::Forbidden, "{wire}: {e:?}");
        checked += 1;
    }
    assert_eq!(checked, 17, "every task write was asked");
}

#[test]
fn a_task_whose_uid_names_a_file_outside_the_roots_is_refused_though_rebound_managed() {
    // What sync leaves when a line is taken out of its file: rebound `managed:`, its `file://` uid kept,
    // and re-attached by that uid if the line comes back.
    let (db, _inside, _outside, _managed) = mutate_fixture();
    let uid = "file:///Users/u/Documents/out/tasks.md#detached";
    db.write_txn("t", move |c, m| {
        let id = jkb_core::task::create(c, m, &jkb_core::task::NewTask::new(uid, "detached"))?;
        jkb_core::binding::set(c, m, id, jkb_core::task::MANAGED_BINDING, None, None)
    })
    .unwrap();
    let e = call(
        &rooted(&db),
        json!({ "op": "task.place", "uid": uid, "ns": "anything" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
}

#[test]
fn an_edit_is_judged_file_backed_by_the_task_s_binding_not_its_uid() {
    let (db, inside, _outside, managed) = mutate_fixture();
    let b = rooted(&db);
    assert!(
        inside.starts_with("task:"),
        "filed by task add, so a task: uid"
    );
    let e = call(
        &b,
        json!({ "op": "task.edit", "uid": inside, "text": "a\n\nb", "append": true }),
    )
    .unwrap_err();
    assert_eq!(
        e.code,
        ErrorCode::Invalid,
        "a blank line would detach on sync: {e:?}"
    );
    assert_eq!(
        call(
            &b,
            json!({ "op": "task.edit", "uid": inside, "text": "more", "append": true })
        )
        .unwrap(),
        Response::Edited { file_backed: true }
    );
    let Response::Task { task, .. } =
        call(&b, json!({ "op": "task.show", "uid": inside })).unwrap()
    else {
        panic!("task")
    };
    assert!(
        task.item
            .content
            .as_deref()
            .is_some_and(|c| c.ends_with("inside\nmore")),
        "a file-backed body appends with one newline: {:?}",
        task.item.content
    );
    assert_eq!(
        call(
            &b,
            json!({ "op": "task.edit", "uid": managed, "text": "a\n\nb" })
        )
        .unwrap(),
        Response::Edited { file_backed: false }
    );
}

#[test]
fn a_namespace_path_too_long_or_too_deep_is_refused_before_any_row_is_written() {
    let (db, _inside, _outside, managed) = mutate_fixture();
    let b = rooted(&db);
    for ns in [
        vec!["a"; jkb_core::ns::MAX_DEPTH + 1].join("/"),
        "x".repeat(jkb_core::ns::MAX_PATH_BYTES + 1),
        vec!["a"; 500_000].join("/"),
    ] {
        let started = std::time::Instant::now();
        let e = call(&b, json!({ "op": "task.place", "uid": managed, "ns": ns })).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let e = call(&b, json!({ "op": "task.add", "text": "t", "home": ns })).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    }
    let deepest = vec!["a"; jkb_core::ns::MAX_DEPTH].join("/");
    call(
        &b,
        json!({ "op": "task.place", "uid": managed, "ns": deepest }),
    )
    .expect("the deepest allowed path is served");
}

#[test]
fn a_claim_owner_is_bounded() {
    let (db, inside, _outside, _managed) = mutate_fixture();
    let b = rooted(&db);
    for owner in [String::new(), "x".repeat(super::tasks::MAX_OWNER_BYTES + 1)] {
        for op in ["task.claim", "task.release"] {
            let e = call(&b, json!({ "op": op, "uid": inside, "owner": owner })).unwrap_err();
            assert_eq!(e.code, ErrorCode::Invalid, "{op}: {e:?}");
        }
    }
}

#[test]
fn the_global_backlog_is_asked_about_only_once_everything_else_is_valid() {
    let (db, _inside, _outside, _managed) = mutate_fixture();
    let b = rooted(&db);
    let before = |b: &LocalBackend| match call(
        b,
        json!({ "op": "kb.query", "dsl": "kind:task", "count": true }),
    )
    .unwrap()
    {
        Response::Count { count } => count,
        other => panic!("{other:?}"),
    };
    let n = before(&b);
    assert_eq!(
        call(
            &b,
            json!({ "op": "task.add", "text": "later", "backlog": true, "cwd": "/nowhere" })
        )
        .unwrap(),
        Response::NeedsGlobalBacklogAssent {}
    );
    assert_eq!(before(&b), n, "nothing created before the user agreed");
    let e = call(
        &b,
        json!({ "op": "task.add", "text": "later #onto=main", "backlog": true, "cwd": "/nowhere" }),
    )
    .unwrap_err();
    assert_eq!(
        e.code,
        ErrorCode::Invalid,
        "a request that would fail anyway is refused, not asked about: {e:?}"
    );
    let e = call(
        &b,
        json!({ "op": "task.add", "text": "later", "backlog": true, "sync": true, "cwd": "/nowhere" }),
    )
    .unwrap_err();
    assert_eq!(
        e.code,
        ErrorCode::Invalid,
        "--sync with no mount is judged before the question: {e:?}"
    );
    let Response::Added { added } = call(
        &b,
        json!({ "op": "task.add", "text": "later", "backlog": true, "global_backlog": true, "cwd": "/nowhere" }),
    )
    .unwrap() else {
        panic!("added")
    };
    assert_eq!(added.home, "tasks/.backlog");
}

#[test]
fn an_edit_is_judged_by_the_body_it_leaves_and_bounded() {
    let (db, inside, _outside, managed) = mutate_fixture();
    let b = rooted(&db);
    for (text, append) in [
        ("step one\n  \nstep two", false),
        ("a\r\n\r\nb", false),
        ("\nmore", true),
        ("more\n\t\n", true),
    ] {
        let e = call(
            &b,
            json!({ "op": "task.edit", "uid": inside, "text": text, "append": append }),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{text:?}: {e:?}");
    }
    // A managed task's content is free-form.
    call(
        &b,
        json!({ "op": "task.edit", "uid": managed, "text": "a\n  \nb" }),
    )
    .unwrap();
    // Growth is bounded: an append loop stops at the cap.
    let chunk = "x".repeat(64 * 1024);
    let mut refused = None;
    for _ in 0..8 {
        if let Err(e) = call(
            &b,
            json!({ "op": "task.edit", "uid": managed, "text": chunk, "append": true }),
        ) {
            refused = Some(e);
            break;
        }
    }
    assert_eq!(
        refused.map(|e| e.code),
        Some(ErrorCode::Invalid),
        "an append past MAX_CONTENT_BYTES is refused"
    );
    let e = call(
        &b,
        json!({ "op": "task.set", "uid": managed, "due": "9".repeat(jkb_core::task::MAX_DUE_BYTES + 1) }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    let e = call(
        &b,
        json!({ "op": "task.tag", "uid": managed, "facet_value": format!("f={}", "v".repeat(jkb_core::tag::MAX_TAG_BYTES)), "mode": "add" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}

/// The round-trip probe parses the whole edit inside the single writer's transaction, so an edit is
/// held to its size before the probe, and the parse is linear in lines that mint the same id: 16k
/// identical checkbox lines took 11 s to judge.
#[test]
fn an_edit_to_a_tasks_md_task_is_sized_before_it_is_parsed_and_parses_in_linear_time() {
    let (db, inside, _outside, _managed) = mutate_fixture();
    let b = rooted(&db);
    let over = format!(
        "t{}",
        "\n- [ ] x".repeat(super::tasks::MAX_CONTENT_BYTES / 8 + 1)
    );
    let e = call(
        &b,
        json!({ "op": "task.edit", "uid": inside, "text": over }),
    )
    .unwrap_err();
    assert!(e.message.contains("bytes"), "refused on its size: {e:?}");
    let under = format!(
        "t{}",
        "\n- [ ] x".repeat(super::tasks::MAX_CONTENT_BYTES / 8 - 1)
    );
    let started = std::time::Instant::now();
    let e = call(
        &b,
        json!({ "op": "task.edit", "uid": inside, "text": under }),
    )
    .unwrap_err();
    assert!(e.message.contains("a task of its own"), "{e:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "32k duplicate lines judged in {:?}",
        started.elapsed()
    );
}

/// A tasks-file task's line must come back from the file whatever field a write changed: a due date, a
/// tag or a namespace with a space in it rendered a line the next import read back as another title,
/// clearing the field.
#[test]
fn every_task_write_holds_the_task_s_tasks_md_line_to_the_round_trip() {
    let (db, inside, _outside, _managed) = mutate_fixture();
    let host = LocalBackend::new(db.clone());
    let Response::Added { added } = call(
        &host,
        json!({ "op": "task.add", "text": "other +repos/in" }),
    )
    .unwrap() else {
        panic!("added")
    };
    let other = added.uid;
    let uid = inside.clone();
    let uri = db
        .write_txn("t", move |c, m| {
            let id = jkb_core::task::resolve_ref(c, &uid)?.expect("task");
            // Planted below the ops, as a writer that never asked the file could have left it.
            jkb_core::task::set_due(c, m, id, Some("next week"))?;
            Ok(jkb_core::binding::get(c, id)?.expect("bound").uri)
        })
        .unwrap();
    let writes = vec![
        json!({ "op": "task.set", "uid": inside, "priority": 1 }),
        json!({ "op": "task.edit", "uid": inside, "text": "more", "append": true }),
        json!({ "op": "task.tag", "uid": inside, "facet_value": "size=s", "mode": "add" }),
        json!({ "op": "task.depend", "uid": inside, "dep": other }),
        json!({ "op": "task.undepend", "uid": inside, "dep": other }),
        json!({ "op": "task.place", "uid": inside, "ns": "elsewhere" }),
        json!({ "op": "task.unplace", "uid": inside, "ns": "elsewhere" }),
        json!({ "op": "task.bind", "uid": inside, "sync": uri }),
        json!({ "op": "task.claim", "uid": inside, "owner": "agent:a" }),
        json!({ "op": "task.release", "uid": inside, "owner": "agent:a" }),
        json!({ "op": "task.start", "uid": inside, "take": { "owner": "agent:b" },
                "place": { "branch": "b1", "repo": "r", "onto": "o" } }),
        json!({ "op": "task.take", "uid": inside,
                "take": { "owner": "agent:c", "displace": "agent:b" },
                "place": { "branch": "b2", "repo": "r", "onto": "o" } }),
        json!({ "op": "task.locate", "uid": inside, "owner": "agent:c",
                "place": { "branch": "b2", "repo": "r" } }),
        json!({ "op": "task.abandon", "uid": inside, "observed": "agent:c" }),
        json!({ "op": "task.landed", "uid": inside,
                "landed": { "branch": "b2", "onto": "o", "head": "abcd" } }),
        json!({ "op": "task.land", "uid": inside,
                "landed": { "branch": "b2", "onto": "o" } }),
    ];
    // Every task write the wire accepts is here; `task.add` checks the task it makes, below.
    let mut covered: Vec<&str> = writes.iter().filter_map(|w| w["op"].as_str()).collect();
    covered.push("task.add");
    covered.sort_unstable();
    let mut task_writes: Vec<&str> = samples()
        .iter()
        .map(super::Request::op)
        .filter(|op| op.starts_with("task.") && !READS.contains(op))
        .collect();
    task_writes.sort_unstable();
    assert_eq!(covered, task_writes);
    // A line some other writer already broke does not block the task: only a write that breaks a
    // readable line is refused, or a task a host command had left unreadable could not even be released.
    let problem = db
        .read({
            let uid = inside.clone();
            move |c| {
                let id = jkb_core::task::resolve_ref(c, &uid)?.expect("task");
                Ok(jkb_sync::filed_task_problem(c, id).unwrap())
            }
        })
        .unwrap();
    assert!(
        problem.as_deref().is_some_and(|p| p.contains("due date")),
        "the planted line is unreadable: {problem:?}"
    );
    // Except onto another line: the old line's problem does not excuse the new one.
    let e = call(
        &host,
        json!({ "op": "task.bind", "uid": inside, "sync": format!("file:///Users/u/repos/in/tasks.md#{}", other.trim_start_matches("task:")) }),
    )
    .unwrap_err();
    assert!(e.message.contains("already another task's"), "{e:?}");
    for request in writes {
        call(&host, request.clone()).unwrap_or_else(|e| panic!("{request}: {e:?}"));
    }

    call(
        &host,
        json!({ "op": "task.set", "uid": inside, "due": "2026-07-15" }),
    )
    .expect("a write that takes the offending value away passes");
}

/// Each field a tasks.md line cannot carry is refused on its own, and the refusal rolls the write back.
#[test]
fn each_value_a_tasks_md_line_cannot_carry_is_refused_naming_its_field() {
    let (db, inside, _outside, _managed) = mutate_fixture();
    let host = LocalBackend::new(db.clone());
    let Response::Added { added } = call(
        &host,
        json!({ "op": "task.add", "text": "other +repos/in" }),
    )
    .unwrap() else {
        panic!("added")
    };
    let other = added.uid;
    call(
        &host,
        json!({ "op": "task.set", "uid": inside, "due": "2026-07-15" }),
    )
    .expect("a one-word due date comes back");
    for (request, field) in [
        (
            json!({ "op": "task.set", "uid": inside, "due": "2026-07-15 17:00" }),
            "due date",
        ),
        (
            json!({ "op": "task.tag", "uid": inside, "facet_value": "note=two words", "mode": "add" }),
            "tags",
        ),
        (
            json!({ "op": "task.place", "uid": inside, "ns": "has space" }),
            "placements",
        ),
        (
            json!({ "op": "task.bind", "uid": inside, "sync": "file:///Users/u/repos/in/tasks.md#Fix_Login" }),
            "identity",
        ),
        (
            json!({ "op": "task.bind", "uid": inside, "sync": format!("file:///Users/u/repos/in/tasks.md#{}", other.trim_start_matches("task:")) }),
            "already another task's",
        ),
        (
            json!({ "op": "task.add", "text": "\"first\n\nsecond\" +repos/in" }),
            "section prose",
        ),
    ] {
        let e = call(&host, request.clone()).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{request}: {e:?}");
        assert!(e.message.contains(field), "{request}: {e:?}");
    }
    let Response::Task { task, .. } =
        call(&host, json!({ "op": "task.show", "uid": inside })).unwrap()
    else {
        panic!("task")
    };
    assert_eq!(task.item.due.as_deref(), Some("2026-07-15"), "rolled back");
}

/// A repo mounted twice — as documents and as tasks — still holds its tasks' lines to the tasks file's
/// rules: the `tasks` mount owns a `#<local id>` line, whichever mount sorts first.
#[test]
fn a_directory_mounted_as_documents_too_still_judges_its_task_lines() {
    let (db, inside, _outside, _managed) = mutate_fixture();
    db.write_txn("t", |c, m| {
        let id = jkb_core::ns::ensure(c, "repos/aaa")?;
        jkb_core::mount::create(
            c,
            m,
            id,
            "file:///Users/u/repos/in",
            jkb_types::SyncMode::Bidirectional,
            "document",
            None,
            None,
            jkb_types::ConflictPolicy::Manual,
        )
    })
    .unwrap();
    let e = call(
        &LocalBackend::new(db.clone()),
        json!({ "op": "task.set", "uid": inside, "due": "2026-07-15 17:00" }),
    )
    .unwrap_err();
    assert!(e.message.contains("due date"), "{e:?}");
}

/// A title in any script files and round-trips: its minted id keeps the title's letters, and the
/// serializer reads such an id back as the task's identity.
#[test]
fn a_task_titled_in_any_script_is_filed_and_written_like_any_other() {
    let (db, _inside, _outside, _managed) = mutate_fixture();
    let b = rooted(&db);
    for title in ["Fix naïve parser", "修复解析器", "İstanbul ofisi"] {
        let Response::Added { added } = call(
            &b,
            json!({ "op": "task.add", "text": format!("{title} +repos/in") }),
        )
        .unwrap_or_else(|e| panic!("{title}: {e:?}")) else {
            panic!("added")
        };
        assert!(added.binding.is_some(), "{title} is filed: {added:?}");
        call(
            &b,
            json!({ "op": "task.claim", "uid": added.uid, "owner": "agent:a" }),
        )
        .unwrap_or_else(|e| panic!("{title}: {e:?}"));
    }
}

/// The line check is part of what `task.add` judges before it asks about the global backlog, so a line
/// the file could not carry is refused, not asked about and then refused.
#[test]
fn a_backlog_add_whose_line_would_not_come_back_is_refused_before_the_question() {
    let (db, _inside, _outside, _managed) = mutate_fixture();
    db.write_txn("t", |c, m| {
        let id = jkb_core::ns::ensure(c, "tasks/.backlog")?;
        jkb_core::mount::create(
            c,
            m,
            id,
            "file:///Users/u/repos/backlog",
            jkb_types::SyncMode::Bidirectional,
            "tasks",
            None,
            None,
            jkb_types::ConflictPolicy::Manual,
        )
    })
    .unwrap();
    let b = rooted(&db);
    let e = call(
        &b,
        json!({ "op": "task.add", "text": "\"first\n\nsecond\"", "backlog": true, "cwd": "/nowhere" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    assert_eq!(
        call(
            &b,
            json!({ "op": "task.add", "text": "later", "backlog": true, "cwd": "/nowhere" })
        )
        .unwrap(),
        Response::NeedsGlobalBacklogAssent {},
        "a line that comes back is still asked about"
    );
}

#[test]
fn a_due_date_is_bounded_on_add_and_a_tag_over_the_limit_can_still_be_removed() {
    let (db, _inside, _outside, managed) = mutate_fixture();
    let b = rooted(&db);
    let items = || {
        db.read(|c| Ok(c.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))?))
            .unwrap()
    };
    let before = items();
    let due = "9".repeat(jkb_core::task::MAX_DUE_BYTES + 1);
    let e = call(
        &b,
        json!({ "op": "task.add", "text": format!("t @{due}"), "managed": true }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    assert_eq!(items(), before, "nothing written");

    // A tag stored before the limit existed is removed like any other.
    let over = "v".repeat(jkb_core::tag::MAX_TAG_BYTES);
    let uid = managed.clone();
    let value = over.clone();
    db.write_txn("t", move |c, _| {
        let id = jkb_core::task::resolve_ref(c, &uid)?.expect("task");
        c.execute(
            "INSERT INTO tag_applications (item_id, facet, value) VALUES (?1, 'f', ?2)",
            rusqlite::params![id.get(), value],
        )?;
        Ok(())
    })
    .unwrap();
    call(
        &b,
        json!({ "op": "task.tag", "uid": managed, "facet_value": format!("f={over}"), "mode": "rm" }),
    )
    .expect("an over-limit tag is removable");
    let Response::Task { task, .. } =
        call(&b, json!({ "op": "task.show", "uid": managed })).unwrap()
    else {
        panic!("task")
    };
    assert!(!format!("{task:?}").contains(&over), "the tag is gone");
}

#[test]
fn binding_a_task_into_a_tasks_md_holds_its_text_to_the_round_trip() {
    let (db, _inside, _outside, managed) = mutate_fixture();
    let host = LocalBackend::new(db.clone());
    call(
        &host,
        json!({ "op": "task.edit", "uid": managed, "text": "managed\n\nsecond paragraph" }),
    )
    .expect("a managed task's body is free-form");
    let e = call(
        &host,
        json!({ "op": "task.bind", "uid": managed, "sync": "file:///Users/u/Documents/out/tasks.md#m" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    assert!(e.message.contains("section prose"), "{e:?}");
    let uid = managed.clone();
    let binding = db
        .read(move |c| {
            let id = jkb_core::task::resolve_ref(c, &uid)?.expect("task");
            Ok(jkb_core::binding::get(c, id)?.map(|b| b.uri))
        })
        .unwrap();
    assert_eq!(binding.as_deref(), Some("managed:"), "still unbound");
    call(
        &host,
        json!({ "op": "task.edit", "uid": managed, "text": "managed" }),
    )
    .unwrap();
    call(
        &host,
        json!({ "op": "task.bind", "uid": managed, "sync": "file:///Users/u/Documents/out/tasks.md#m" }),
    )
    .expect("a task that round-trips is bound");
}

#[test]
fn a_quick_add_line_is_bounded_in_what_it_fans_out_to() {
    let (db, _inside, _outside, _managed) = mutate_fixture();
    let b = rooted(&db);
    let line = (0..=super::tasks::MAX_QUICK_ADD_MODIFIERS)
        .map(|i| format!("#f{i}=v"))
        .collect::<Vec<_>>()
        .join(" ");
    let e = call(
        &b,
        json!({ "op": "task.add", "text": format!("t {line}"), "managed": true }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
}

#[test]
fn a_task_homed_at_the_namespace_limit_is_refused_naming_its_mirror() {
    let (db, _inside, _outside, _managed) = mutate_fixture();
    let b = rooted(&db);
    let home = vec!["d"; jkb_core::ns::MAX_DEPTH].join("/");
    let e = call(
        &b,
        json!({ "op": "task.add", "text": "t", "home": home, "managed": true }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    assert!(e.message.contains("needs the mirror"), "{e:?}");
}

#[test]
fn the_global_backlog_question_follows_every_other_refusal_and_writes_nothing() {
    let (db, _inside, _outside, _managed) = mutate_fixture();
    let b = rooted(&db);
    let count =
        |b: &LocalBackend| match call(b, json!({ "op": "kb.query", "dsl": "", "count": true }))
            .unwrap()
        {
            Response::Count { count } => count,
            other => panic!("{other:?}"),
        };
    let before = count(&b);
    let e = call(
        &b,
        json!({ "op": "task.add", "text": "later ^task:does-not-exist", "backlog": true, "cwd": "/nowhere" }),
    )
    .unwrap_err();
    assert_ne!(
        e.code,
        ErrorCode::Unknown,
        "a missing dependency is the answer: {e:?}"
    );
    assert_eq!(
        call(
            &b,
            json!({ "op": "task.add", "text": "later", "backlog": true, "cwd": "/nowhere" })
        )
        .unwrap(),
        Response::NeedsGlobalBacklogAssent {}
    );
    assert_eq!(count(&b), before, "the create ran and was rolled back");
}

#[test]
fn an_edit_or_add_filed_in_a_tasks_md_must_read_back_as_written() {
    let (db, inside, _outside, managed) = mutate_fixture();
    let b = rooted(&db);
    for text in [
        "also\n- [ ] a child",
        "Refactor ^parser",
        "Ship #size=small",
    ] {
        let e = call(
            &b,
            json!({ "op": "task.edit", "uid": inside, "text": text }),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{text:?}: {e:?}");
        call(
            &b,
            json!({ "op": "task.edit", "uid": managed, "text": text }),
        )
        .unwrap_or_else(|e| panic!("a managed task takes {text:?}: {e:?}"));
    }
    let e = call(
        &b,
        json!({ "op": "task.add", "text": "\"first\n\nsecond\" +repos/in" }),
    )
    .unwrap_err();
    assert_eq!(
        e.code,
        ErrorCode::Invalid,
        "a quoted title with a blank line: {e:?}"
    );
}

/// A client's text is captured without a model: the daemon's backend has no embedder, so the document
/// is keyword-searchable at once and left for the host's `jkb index --pending`. Addressed by its text,
/// with no blob, and the same text again is the same document.
#[test]
fn ingest_text_captures_a_client_s_text_without_calling_a_model() {
    let db = Db::open_in_memory().unwrap();
    let daemon = LocalBackend::new(db.clone()).with_actor("serve");
    let text = "Ingested from the container. ".repeat(80);
    let ask = json!({ "op": "ingest.text", "text": text, "mime": "text/markdown", "namespace": "references/notes" });
    let Response::Ingested { ingested } = call(&daemon, ask.clone()).unwrap() else {
        panic!("ingested")
    };
    assert!(!ingested.embedded);
    assert!(ingested.chunk_count > 1, "{ingested:?}");
    assert!(
        ingested
            .warnings
            .iter()
            .any(|w| w.contains("index --pending")),
        "{ingested:?}"
    );
    let Response::Ingested { ingested: again } = call(&daemon, ask).unwrap() else {
        panic!("ingested")
    };
    assert_eq!(
        again.document, ingested.document,
        "the same text is the same document"
    );
    assert!(again.already_ingested && !again.embedded, "{again:?}");
    // Asked under another namespace, it answers where the document is.
    let Response::Ingested {
        ingested: elsewhere,
    } = call(
        &daemon,
        json!({ "op": "ingest.text", "text": text, "mime": "text/markdown", "namespace": "inbox" }),
    )
    .unwrap()
    else {
        panic!("ingested")
    };
    assert_eq!(elsewhere.namespace, "references/notes");
    let (uid, blobs, actor) = db
        .read(move |c| {
            let uid: String = c.query_row(
                "SELECT uid FROM items WHERE id = ?1",
                [ingested.document],
                |r| r.get(0),
            )?;
            let blobs: i64 = c.query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))?;
            let actor: String = c.query_row(
                "SELECT actor FROM changelog WHERE entity_type = 'items' ORDER BY id LIMIT 1",
                [],
                |r| r.get(0),
            )?;
            Ok((uid, blobs, actor))
        })
        .unwrap();
    assert_eq!(uid, format!("b3:{}", jkb_ingest::text_address(&text)));
    assert_ne!(
        jkb_ingest::text_address(&text),
        jkb_ingest::blob::hash_bytes(text.as_bytes()),
        "never the address of a file whose bytes are this text"
    );
    assert_eq!(blobs, 0, "no source bytes, so no blob");
    assert_eq!(actor, "serve");
    let Response::SearchHits { hits, .. } = call(
        &daemon,
        json!({ "op": "kb.search", "dsl": "container", "route": "fts", "limit": 5 }),
    )
    .unwrap() else {
        panic!("hits")
    };
    assert!(!hits.is_empty(), "keyword-searchable at once");
}

/// The source bytes address a document only from the host's own process: they are not a field of the
/// wire, so a client cannot name the hash its text is filed under.
#[test]
fn ingest_text_takes_source_bytes_only_in_process() {
    let e = serde_json::from_value::<Request>(json!({
        "op": "ingest.text", "text": "t", "mime": "text/plain", "namespace": "inbox", "raw": [1, 2]
    }))
    .unwrap_err();
    assert!(e.to_string().contains("raw"), "{e}");
    let db = Db::open_in_memory().unwrap();
    let host = LocalBackend::new(db.clone());
    let raw = b"# Title\n\nthe markdown source".to_vec();
    let Response::Ingested { ingested } = host
        .call(Request::IngestText(super::ingest::IngestAsk {
            text: "Title the markdown source".into(),
            mime: "text/markdown".into(),
            namespace: "inbox".into(),
            raw: Some(raw.clone()),
        }))
        .unwrap()
    else {
        panic!("ingested")
    };
    let (uid, blob) = db
        .read(move |c| {
            let uid: String = c.query_row(
                "SELECT uid FROM items WHERE id = ?1",
                [ingested.document],
                |r| r.get(0),
            )?;
            let blob = jkb_core::blob::load(c, &uid[3..])?;
            Ok((uid, blob))
        })
        .unwrap();
    assert_eq!(uid, format!("b3:{}", jkb_ingest::blob::hash_bytes(&raw)));
    assert_eq!(
        blob.as_deref(),
        Some(&raw[..]),
        "stored as the document's blob"
    );
    let e = call(
        &host,
        json!({ "op": "ingest.text", "text": "t", "mime": "x".repeat(super::ingest::MAX_MIME_BYTES + 1), "namespace": "inbox" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
}

/// The session registry through its ops, as the hook drives it: two processes on one id, the end of
/// one leaving the session live, a sweep verdict ending the other, a revival — with the wire shapes a
/// client in another version parses.
#[test]
fn the_session_ops_drive_the_registry() {
    let b = backend();
    let rows = |all: bool| -> Vec<super::ClaudeSession> {
        match b.call(Request::SessionList { all, after: None }).unwrap() {
            Response::ClaudeSessions {
                sessions,
                next: None,
            } => sessions,
            other => panic!("unexpected {other:?}"),
        }
    };
    let started = call(
        &b,
        json!({ "op": "session.started", "session": "s1", "source": "startup",
                "pid": "10", "instance": "h", "cwd": "/w" }),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&started).unwrap(),
        json!({ "result": "session_start", "was": "unknown" })
    );
    assert_eq!(
        call(
            &b,
            json!({ "op": "session.started", "session": "s1", "source": "resume",
                    "pid": "11", "instance": "h" }),
        )
        .unwrap(),
        Response::SessionStart { was: "live".into() },
        "a second process joins a running session"
    );
    let ended = call(
        &b,
        json!({ "op": "session.ended", "session": "s1", "reason": "other",
                "pid": "11", "instance": "h" }),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&ended).unwrap(),
        json!({ "result": "session_end", "outcome": "recorded" })
    );

    let mut live = serde_json::to_value(rows(false)).unwrap();
    let row = live[0].as_object_mut().unwrap();
    for stamp in ["started_at", "seen_at"] {
        let t = row.remove(stamp).unwrap();
        assert!(t.as_i64().is_some_and(|t| t > 0), "{stamp}: {t}");
    }
    assert_eq!(
        live,
        json!([{ "session": "s1", "pid": "10", "instance": "h", "cwd": "/w",
                 "start_source": "startup" }]),
        "the first process still holds it; a live row carries no end fields"
    );

    assert_eq!(
        call(
            &b,
            json!({ "op": "session.gone", "session": "s1", "pid": "10", "instance": "h" })
        )
        .unwrap(),
        Response::SessionGone { ended: true }
    );
    assert!(rows(false).is_empty(), "no live rows left");
    let reasons: Vec<Option<String>> = rows(true).into_iter().map(|r| r.end_reason).collect();
    assert_eq!(reasons.len(), 2);
    assert!(reasons.contains(&Some("gone".into())), "{reasons:?}");
    assert_eq!(
        call(
            &b,
            json!({ "op": "session.started", "session": "s1", "source": "resume",
                    "pid": "12", "instance": "h" }),
        )
        .unwrap(),
        Response::SessionStart {
            was: "ended".into()
        }
    );

    let err = call(
        &b,
        json!({ "op": "session.started", "session": "a b", "source": "startup" }),
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid, "{err:?}");
}

/// **A lost start is repaired by the next hook event**: a `notify.event` from a process marks it
/// running in the registry — reviving a row that had ended — except the `session_ended` event, which
/// the hook follows with `session.ended` and which must not revive what it is ending.
#[test]
fn a_notify_event_marks_its_process_running_except_at_the_end() {
    let b = backend();
    // An end withdraws whatever is on screen, so the topic must exist.
    b.call(Request::MqTopicCreate {
        topic: jkb_core::notify::TOPIC.into(),
        spec: SpecInput::default(),
    })
    .unwrap();
    let event = |event: &str| {
        call(
            &b,
            json!({ "op": "notify.event", "session": "s1", "event": event,
                    "owner": "10", "instance": "h", "cwd": "/w" }),
        )
        .unwrap();
    };
    let live = || match b
        .call(Request::SessionList {
            all: false,
            after: None,
        })
        .unwrap()
    {
        Response::ClaudeSessions { sessions, .. } => sessions.len(),
        other => panic!("unexpected {other:?}"),
    };
    event("tool_finished");
    assert_eq!(live(), 1, "a session nobody saw start is known live");
    call(
        &b,
        json!({ "op": "session.ended", "session": "s1", "reason": "other",
                "pid": "10", "instance": "h" }),
    )
    .unwrap();
    assert_eq!(live(), 0);
    event("session_ended");
    assert_eq!(live(), 0, "the end's own notify.event revives nothing");
    event("user_acted");
    assert_eq!(live(), 1, "a later event from the process revives it");
}

/// A `session.list` page's cursor is an opaque string: sent back as it came it continues the listing,
/// and anything else is refused rather than read as some other position.
#[test]
fn a_session_list_cursor_is_opaque() {
    let b = backend();
    for i in 0..=jkb_core::claude_session::LIST_CAP {
        b.call(Request::SessionStarted {
            session: format!("s{i:04}"),
            source: "startup".into(),
            pid: "1".into(),
            instance: "h".into(),
            cwd: String::new(),
        })
        .unwrap();
    }
    let page = b
        .call(Request::SessionList {
            all: false,
            after: None,
        })
        .unwrap();
    let wire = serde_json::to_value(&page).unwrap();
    assert!(wire["next"].is_string(), "{}", wire["next"]);
    let Response::ClaudeSessions { next, .. } = page else {
        panic!("expected sessions")
    };
    let Response::ClaudeSessions { sessions, next } = b
        .call(Request::SessionList {
            all: false,
            after: next,
        })
        .unwrap()
    else {
        panic!("expected sessions")
    };
    assert_eq!((sessions.len(), next), (1, None));
    for bad in [
        "",
        "{}",
        r#"{"seen_at":1,"session":"s","pid":"1","instance":"h","x":1}"#,
    ] {
        let err = b
            .call(Request::SessionList {
                all: false,
                after: Some(bad.into()),
            })
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid, "{bad}: {err:?}");
    }
}
