//! `jkb serve` and `RemoteBackend` over real loopback TCP — the path the dev container's `jkb` will
//! take to the host, minus the container. Runs on Linux CI.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use jkb_api::{Backend, ErrorCode, Request, Response, SpecInput};
use jkb_core::Db;
use jkb_daemon::client::RemoteBackend;
use jkb_daemon::server::{spawn, ServeConfig, ServeError};
use serde_json::json;

struct Fixture {
    _dir: tempfile::TempDir,
    db: Db,
    token: PathBuf,
    handle: Option<jkb_daemon::server::Handle>,
    base: String,
}

impl Fixture {
    fn new() -> Self {
        Self::with(|_| {})
    }

    fn with(tune: impl FnOnce(&mut ServeConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("jkb.db")).unwrap();
        let token = dir.path().join("daemon/token");
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut cfg = ServeConfig::new(addr, token.clone());
        tune(&mut cfg);
        let handle = spawn(db.clone(), &cfg).unwrap();
        let base = format!("http://{}", handle.addr);
        Self {
            _dir: dir,
            db,
            token,
            handle: Some(handle),
            base,
        }
    }

    fn client(&self) -> RemoteBackend {
        RemoteBackend::new(&self.base, self.token.clone()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown();
        }
    }
}

fn topic_and_group(b: &dyn Backend) {
    b.call(Request::MqTopicCreate {
        topic: "t".into(),
        spec: SpecInput::default(),
    })
    .unwrap();
    b.call(Request::MqGroupCreate {
        topic: "t".into(),
        group: "g".into(),
        from_start: true,
    })
    .unwrap();
}

fn send(b: &dyn Backend, n: i64) -> i64 {
    match b
        .call(Request::MqSend {
            topic: "t".into(),
            key: "k".into(),
            kind: "k.m".into(),
            payload: json!(n),
            ttl_ms: None,
            producer: "test".into(),
        })
        .unwrap()
    {
        Response::Sent { seq } => seq,
        other => panic!("{other:?}"),
    }
}

#[test]
fn operations_round_trip_over_http_exactly_as_they_do_locally() {
    let f = Fixture::new();
    let c = f.client();
    topic_and_group(&c);
    let seq = send(&c, 7);
    let Response::Messages { messages } = c
        .call(Request::MqPoll {
            topic: "t".into(),
            group: "g".into(),
            max: 10,
            after: None,
        })
        .unwrap()
    else {
        panic!("expected messages")
    };
    assert_eq!(
        (messages[0].seq, messages[0].payload.clone()),
        (seq, json!(7))
    );
    assert_eq!(
        c.call(Request::MqAck {
            topic: "t".into(),
            group: "g".into(),
            seq
        })
        .unwrap(),
        Response::Position { position: seq }
    );
    // A refusal keeps its code across the wire.
    let err = c
        .call(Request::MqAck {
            topic: "t".into(),
            group: "nope".into(),
            seq,
        })
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoSuchGroup);
    let hello = c.hello().unwrap();
    assert_eq!(hello["protocol"], 1);
    assert_eq!(
        hello["schema_version"],
        jkb_core::supported_schema_version()
    );
    assert!(hello["ops"].as_array().unwrap().contains(&json!("mq.send")));
}

#[test]
fn a_long_poll_is_woken_by_a_send_rather_than_waiting_out_its_timeout() {
    // The floor is pushed out past the test, so ONLY the send's wake-up can deliver in time.
    let f = Fixture::with(|cfg| cfg.poll_floor = Duration::from_mins(1));
    let c = f.client().with_poll_wait(Duration::from_secs(20));
    topic_and_group(&c);
    let base = f.base.clone();
    let token = f.token.clone();
    let sender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        send(&RemoteBackend::new(&base, token).unwrap(), 1)
    });
    let started = Instant::now();
    let Response::Messages { messages } = c
        .call(Request::MqPoll {
            topic: "t".into(),
            group: "g".into(),
            max: 10,
            after: None,
        })
        .unwrap()
    else {
        panic!("expected messages")
    };
    let seq = sender.join().unwrap();
    assert_eq!(
        messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        vec![seq]
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "woken by the send, not the 20 s wait: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_message_written_outside_the_daemon_wakes_a_long_poll_within_the_floor() {
    // The host CLI opens the database directly, so the daemon's own send notification never fires
    // for it; the 250 ms floor is what delivers it.
    let f = Fixture::new();
    let c = f.client().with_poll_wait(Duration::from_secs(20));
    topic_and_group(&c);
    let direct = jkb_api::LocalBackend::new(f.db.clone());
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        send(&direct, 2)
    });
    let started = Instant::now();
    let Response::Messages { messages } = c
        .call(Request::MqPoll {
            topic: "t".into(),
            group: "g".into(),
            max: 10,
            after: None,
        })
        .unwrap()
    else {
        panic!("expected messages")
    };
    writer.join().unwrap();
    assert_eq!(messages.len(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_wrong_or_missing_token_is_unauthorized_and_a_rotated_one_is_picked_up() {
    let f = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let wrong = dir.path().join("token");
    std::fs::write(&wrong, "0000").unwrap();
    let c = RemoteBackend::new(&f.base, wrong).unwrap();
    let err = c.call(Request::MqInspect {}).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unauthorized);

    let missing = RemoteBackend::new(&f.base, dir.path().join("absent")).unwrap();
    assert_eq!(
        missing.call(Request::MqInspect {}).unwrap_err().code,
        ErrorCode::Unavailable
    );

    // Rotation: daemon B writes a NEW token over A's file. A client that read the file before the
    // rotation holds A's token; on 401 it re-reads the file once and carries on.
    let token_a = std::fs::read_to_string(&f.token).unwrap();
    let b = spawn(
        f.db.clone(),
        &ServeConfig::new("127.0.0.1:0".parse().unwrap(), f.token.clone()),
    )
    .unwrap();
    let token_b = std::fs::read_to_string(&f.token).unwrap();
    assert_ne!(token_a, token_b, "each start rotates the token");
    jkb_daemon::token::write(&f.token, &token_a).unwrap();
    let client_b = RemoteBackend::new(&format!("http://{}", b.addr), f.token.clone()).unwrap();
    assert_eq!(
        client_b.call(Request::MqInspect {}).unwrap_err().code,
        ErrorCode::Unauthorized,
        "a stale token, still stale on re-read, is refused (once — not retried in a loop)"
    );
    jkb_daemon::token::write(&f.token, &token_b).unwrap();
    client_b
        .call(Request::MqInspect {})
        .expect("the cached stale token is replaced after the 401");
    b.shutdown();
}

#[test]
fn an_unspecified_address_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("jkb.db")).unwrap();
    for addr in ["0.0.0.0:0", "[::]:0"] {
        let err = spawn(
            db.clone(),
            &ServeConfig::new(addr.parse().unwrap(), dir.path().join("t")),
        )
        .err()
        .expect("refused");
        assert!(matches!(err, ServeError::Unspecified(_)), "{err}");
    }
}

#[test]
fn raw_http_is_held_to_the_same_rules() {
    let f = Fixture::new();
    let token = std::fs::read_to_string(&f.token).unwrap();
    // No connection pooling: the 413 below closes its connection mid-upload, and a pooled client
    // would send the NEXT request down that dead connection and fail on the transport, not the rule.
    let http = reqwest::blocking::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let post = |body: String| {
        http.post(format!("{}/v1/op", f.base))
            .bearer_auth(token.trim())
            .header("content-type", "application/json")
            .body(body)
            .send()
            .unwrap()
    };
    // An unknown field or op is refused, not ignored.
    let r = post(json!({ "op": "mq.inspect", "filter": "x" }).to_string());
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<serde_json::Value>().unwrap()["code"],
        "bad_request"
    );
    let r = post(json!({ "op": "fs.read", "path": "/etc/passwd" }).to_string());
    assert_eq!(r.status(), 400);
    // An oversized body is refused before it is parsed.
    let r = post("x".repeat(2 * 1024 * 1024));
    assert_eq!(r.status(), 413);
    // No token at all.
    let r = http
        .post(format!("{}/v1/op", f.base))
        .body("{}")
        .send()
        .unwrap();
    assert_eq!(r.status(), 401);
    // No such endpoint.
    let r = http
        .get(format!("{}/etc/passwd", f.base))
        .bearer_auth(token.trim())
        .send()
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[test]
fn a_database_migrated_past_this_build_is_refused() {
    let f = Fixture::new();
    let c = f.client();
    let future = jkb_core::supported_schema_version() + 1;
    f.db.write_txn("test", move |conn, _| {
        conn.pragma_update(None, "user_version", future)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        c.call(Request::MqInspect {}).unwrap_err().code,
        ErrorCode::SchemaNewer
    );
}

#[test]
fn an_unreachable_daemon_is_remembered_briefly_across_clients() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    jkb_daemon::token::write(&token, "t").unwrap();
    let marker = dir.path().join("unreachable");
    // Nothing listens on this port (bound then released).
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let base = format!("http://127.0.0.1:{port}");
    let c = RemoteBackend::new(&base, token.clone())
        .unwrap()
        .with_down_marker(marker.clone());
    assert_eq!(
        c.call(Request::MqInspect {}).unwrap_err().code,
        ErrorCode::Unavailable
    );
    assert!(marker.exists(), "a failed connect is remembered");
    let started = Instant::now();
    let other = RemoteBackend::new(&base, token)
        .unwrap()
        .with_down_marker(marker);
    let err = other.call(Request::MqInspect {}).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(err.message.contains("less than"), "{}", err.message);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "no second connect attempt"
    );
}
