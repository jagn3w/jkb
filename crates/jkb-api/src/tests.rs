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
