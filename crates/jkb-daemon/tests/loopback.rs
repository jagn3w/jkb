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
    //
    // What this does NOT pin: the narrow race `enable()` in `serve_op` closes — a send landing
    // between the poll's read and its wait. The 300 ms sleep puts the send well inside the wait, and
    // no sleep can place it in a window of microseconds; deleting `enable()` leaves this green. The
    // floor bounds that miss at 250 ms in production.
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

// The Mac failure of 2026-09-14: something else held the port, and the daemon said only "Address
// already in use". The refusal must name the port and how to find the holder — and write no token,
// or a client would find a fresh token with no daemon behind it.
#[test]
fn a_port_held_by_another_process_is_named_with_how_to_find_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("jkb.db")).unwrap();
    let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = holder.local_addr().unwrap();
    let token = dir.path().join("daemon/token");
    let err = spawn(db, &ServeConfig::new(addr, token.clone()))
        .err()
        .expect("refused");
    assert!(
        matches!(err, ServeError::AddrInUse(a) if a == addr),
        "{err}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains(&format!("lsof -nP -iTCP:{} -sTCP:LISTEN", addr.port())),
        "{msg}"
    );
    assert!(
        !token.exists(),
        "a token was written with no daemon behind it"
    );

    // ...and a daemon that opens its own database must not open it first: under a supervisor that
    // restarts it for as long as the port is held, that is a migration attempt per restart.
    let opens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = opens.clone();
    let path = dir.path().join("jkb.db");
    let opener: jkb_daemon::server::Opener = Box::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Db::open(&path).map_err(jkb_api::ApiError::from)
    });
    let err = jkb_daemon::server::spawn_opening(opener, &ServeConfig::new(addr, token.clone()))
        .err()
        .expect("refused");
    assert!(matches!(err, ServeError::AddrInUse(_)), "{err}");
    assert_eq!(
        opens.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the database was opened by a daemon that could not listen"
    );
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
    // No token at all — and the connection is closed after the refusal, so an unauthenticated client
    // cannot keep a slot by sending refused requests down one kept-alive connection.
    let r = http
        .post(format!("{}/v1/op", f.base))
        .body("{}")
        .send()
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(
        r.headers().get("connection").map(|v| v.to_str().unwrap()),
        Some("close")
    );
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
    assert!(
        c.schema_newer_clears(),
        "setup.sh restarts a daemon on the newer jkb, so its subscribers wait for that"
    );
    let future = jkb_core::supported_schema_version() + 1;
    f.db.write_txn("test", move |conn, _| {
        conn.execute(
            "INSERT INTO refinery_schema_history (version, name, applied_on, checksum) \
                 VALUES (?1, 'from_the_future', '2030-01-01T00:00:00Z', '0')",
            [future],
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        c.call(Request::MqInspect {}).unwrap_err().code,
        ErrorCode::SchemaNewer
    );
    let sent = c.call(Request::MqSend {
        topic: "t".into(),
        key: "k".into(),
        kind: "k.m".into(),
        payload: json!(1),
        ttl_ms: None,
        producer: "test".into(),
    });
    assert_eq!(sent.unwrap_err().code, ErrorCode::SchemaNewer);
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

/// A stub HTTP server answering every request with `response`, counting the requests it saw.
fn stub(response: &'static str) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (base, seen)
}

#[test]
fn an_answer_from_something_other_than_jkb_serve_means_unavailable_not_internal() {
    // A proxy in the path answers for a daemon that is down. That is not the daemon refusing the
    // request, so it is `unavailable` — retried by a subscriber, remembered by the down marker —
    // and it is not a jkb `unauthorized`, so the token is not re-read and the request not re-sent.
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    jkb_daemon::token::write(&token, "t").unwrap();
    for response in [
        "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 11\r\nConnection: close\r\n\r\nBad Gateway",
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope",
    ] {
        let (base, seen) = stub(response);
        let marker = dir.path().join("unreachable");
        let _ = std::fs::remove_file(&marker);
        let c = RemoteBackend::new(&base, token.clone())
            .unwrap()
            .with_down_marker(marker.clone());
        let err = c.call(Request::MqInspect {}).unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::Unavailable,
            "{response}: {}",
            err.message
        );
        assert!(marker.exists(), "{response}: remembered as down");
        assert_eq!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{response}: sent once"
        );
    }
}

#[test]
fn a_daemon_whose_database_will_not_open_answers_why_and_recovers_when_it_does() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("daemon/token");
    let db = Db::open(dir.path().join("jkb.db")).unwrap();
    let fixed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let opener: jkb_daemon::server::Opener = {
        let (fixed, attempts) = (fixed.clone(), attempts.clone());
        Box::new(move || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if fixed.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(db.clone())
            } else {
                Err(jkb_api::ApiError::with_code(
                    ErrorCode::SchemaNewer,
                    "migrated by a newer jkb",
                ))
            }
        })
    };
    let mut cfg = ServeConfig::new("127.0.0.1:0".parse().unwrap(), token.clone());
    cfg.reopen_every = Duration::from_millis(300);
    let h = jkb_daemon::server::spawn_opening(opener, &cfg).unwrap();
    let c = RemoteBackend::new(&format!("http://{}", h.addr), token).unwrap();
    assert_eq!(
        c.call(Request::MqInspect {}).unwrap_err().code,
        ErrorCode::SchemaNewer
    );
    assert_eq!(c.hello().unwrap_err().code, ErrorCode::SchemaNewer);
    let tried = attempts.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(tried, 1, "re-opens are rate-limited, not one per request");
    fixed.store(true, std::sync::atomic::Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(400));
    c.call(Request::MqInspect {})
        .expect("served once the database opens, with no restart");
    h.shutdown();
}

/// The authentication deadline closes only connections that never authenticated: an authenticated
/// long-poll held past `read_timeout` still returns normally.
#[test]
fn an_authenticated_long_poll_outlives_the_authentication_deadline() {
    let f = Fixture::with(|cfg| cfg.read_timeout = Duration::from_millis(300));
    let c = f.client().with_poll_wait(Duration::from_millis(1200));
    topic_and_group(&c);
    let started = Instant::now();
    let polled = c.call(Request::MqPoll {
        topic: "t".into(),
        group: "g".into(),
        max: 10,
        after: None,
    });
    assert_eq!(
        polled.unwrap(),
        Response::Messages {
            messages: Vec::new()
        },
        "held for its wait, then answered — not cut at the deadline"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

/// One re-open at a time even when the request that started it goes away mid-open: the open holds the
/// guard itself, so a second request waits for it rather than starting another beside it.
#[test]
fn a_re_open_is_single_flight_even_when_its_request_is_cancelled() {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("daemon/token");
    let (inflight, most) = (
        std::sync::Arc::new(AtomicUsize::new(0)),
        std::sync::Arc::new(AtomicUsize::new(0)),
    );
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let opener: jkb_daemon::server::Opener = {
        let (inflight, most, calls) = (inflight.clone(), most.clone(), calls.clone());
        Box::new(move || {
            // The first open, at start, fails at once; each re-open is slow, and fails too.
            if calls.fetch_add(1, Ordering::SeqCst) > 0 {
                let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(600));
                inflight.fetch_sub(1, Ordering::SeqCst);
            }
            Err(jkb_api::ApiError::with_code(
                ErrorCode::Unavailable,
                "not yet",
            ))
        })
    };
    let mut cfg = ServeConfig::new("127.0.0.1:0".parse().unwrap(), token.clone());
    cfg.reopen_every = Duration::ZERO;
    let h = jkb_daemon::server::spawn_opening(opener, &cfg).unwrap();
    let secret = std::fs::read_to_string(&token).unwrap();
    // A raw request that starts a re-open, then goes away while it runs.
    let mut first = std::net::TcpStream::connect(h.addr).unwrap();
    write!(
        first,
        "GET /v1/hello HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {}\r\n\r\n",
        secret.trim()
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    drop(first);
    std::thread::sleep(Duration::from_millis(100));
    let c = RemoteBackend::new(&format!("http://{}", h.addr), token).unwrap();
    assert_eq!(
        c.call(Request::MqInspect {}).unwrap_err().code,
        ErrorCode::Unavailable
    );
    assert_eq!(most.load(Ordering::SeqCst), 1, "two re-opens ran at once");
    h.shutdown();
}

#[test]
fn a_connection_that_never_authenticates_is_closed_even_while_pipelining() {
    // Two guards close this connection — the refusal's `Connection: close` and the deadline to
    // authenticate — and this test needs only one of them (measured: it fails with both removed,
    // passes with either). `raw_http_is_held_to_the_same_rules` pins the first on its own.
    use std::io::Write as _;
    // One slot, so a held connection is a lockout the client below would see.
    let f = Fixture::with(|cfg| {
        cfg.max_connections = 1;
        cfg.read_timeout = Duration::from_millis(500);
    });
    let addr = f.handle.as_ref().unwrap().addr;
    let mut hog = std::net::TcpStream::connect(addr).unwrap();
    // Pipelined unauthenticated requests, never reading an answer: hyper stops reading once its
    // answers back up, and its header timer never restarts.
    let burst = "GET /v1/hello HTTP/1.1\r\nhost: x\r\n\r\n".repeat(20_000);
    let _ = hog.set_write_timeout(Some(Duration::from_millis(200)));
    let _ = hog.write_all(burst.as_bytes());
    std::thread::sleep(Duration::from_millis(1200));
    f.client()
        .call(Request::MqInspect {})
        .expect("the hog was closed within the read timeout");
    drop(hog);
}

#[test]
fn a_long_poll_slot_is_released_when_its_client_goes_away() {
    use std::io::Write as _;
    let f = Fixture::new();
    let c = f.client();
    topic_and_group(&c);
    let token = std::fs::read_to_string(&f.token).unwrap();
    let body = json!({ "op": "mq.poll", "topic": "t", "group": "g", "max": 10 }).to_string();
    let mut raw = std::net::TcpStream::connect(f.handle.as_ref().unwrap().addr).unwrap();
    write!(
        raw,
        "POST /v1/op?wait_ms=20000 HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {}\r\n\
         content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        token.trim(),
        body.len()
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let short = f.client().with_poll_wait(Duration::from_millis(200));
    let poll = || {
        short.call(Request::MqPoll {
            topic: "t".into(),
            group: "g".into(),
            max: 10,
            after: None,
        })
    };
    assert_eq!(
        poll().unwrap_err().code,
        ErrorCode::Busy,
        "held by the raw poll"
    );
    drop(raw);
    let started = Instant::now();
    while poll().is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the slot outlived its client"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_group_holds_one_long_poll_at_a_time() {
    let f = Fixture::new();
    let c = f.client();
    topic_and_group(&c);
    c.call(Request::MqGroupCreate {
        topic: "t".into(),
        group: "other".into(),
        from_start: true,
    })
    .unwrap();
    let poll = |group: &str| Request::MqPoll {
        topic: "t".into(),
        group: group.into(),
        max: 10,
        after: None,
    };
    let held = {
        let (base, token) = (f.base.clone(), f.token.clone());
        std::thread::spawn(move || {
            RemoteBackend::new(&base, token)
                .unwrap()
                .with_poll_wait(Duration::from_secs(3))
                .call(Request::MqPoll {
                    topic: "t".into(),
                    group: "g".into(),
                    max: 10,
                    after: None,
                })
        })
    };
    std::thread::sleep(Duration::from_millis(500));
    let short = f.client().with_poll_wait(Duration::from_millis(300));
    let err = short.call(poll("g")).unwrap_err();
    assert_eq!(err.code, ErrorCode::Busy, "{}", err.message);
    short
        .call(poll("other"))
        .expect("another group is not held up");
    held.join().unwrap().expect("the first poll ends normally");
    short
        .call(poll("g"))
        .expect("released once the first poll returned");
}

#[test]
fn wait_ms_holds_only_a_poll() {
    let f = Fixture::new();
    let c = f.client();
    topic_and_group(&c);
    let token = std::fs::read_to_string(&f.token).unwrap();
    let started = Instant::now();
    let r = reqwest::blocking::Client::new()
        .post(format!("{}/v1/op?wait_ms=5000", f.base))
        .bearer_auth(token.trim())
        .body(json!({ "op": "mq.tail", "topic": "t", "limit": 10 }).to_string())
        .send()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "an empty tail is answered at once, not held: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_long_poll_notices_a_newer_schema_while_it_waits() {
    let f = Fixture::new();
    let c = f.client().with_poll_wait(Duration::from_secs(20));
    topic_and_group(&c);
    let db = f.db.clone();
    let migrator = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        let future = jkb_core::supported_schema_version() + 1;
        db.write_txn("test", move |conn, _| {
            conn.execute(
                "INSERT INTO refinery_schema_history (version, name, applied_on, checksum) \
                 VALUES (?1, 'from_the_future', '2030-01-01T00:00:00Z', '0')",
                [future],
            )?;
            Ok(())
        })
        .unwrap();
    });
    let started = Instant::now();
    let err = c
        .call(Request::MqPoll {
            topic: "t".into(),
            group: "g".into(),
            max: 10,
            after: None,
        })
        .unwrap_err();
    migrator.join().unwrap();
    assert_eq!(err.code, ErrorCode::SchemaNewer);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn connections_are_capped_and_a_silent_one_is_closed() {
    use std::io::{Read as _, Write as _};
    let f = Fixture::with(|cfg| {
        cfg.max_connections = 2;
        cfg.read_timeout = Duration::from_millis(600);
    });
    let addr = f.handle.as_ref().unwrap().addr;
    let closed_within = |stream: &mut std::net::TcpStream| {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let started = Instant::now();
        let mut buf = [0u8; 512];
        let n = stream.read(&mut buf).unwrap_or(0);
        (
            started.elapsed(),
            String::from_utf8_lossy(&buf[..n]).into_owned(),
        )
    };
    let mut first = std::net::TcpStream::connect(addr).unwrap();
    let mut second = std::net::TcpStream::connect(addr).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let mut third = std::net::TcpStream::connect(addr).unwrap();
    let (took, said) = closed_within(&mut third);
    assert!(
        took < Duration::from_millis(500) && said.is_empty(),
        "over the cap: {took:?} {said}"
    );

    // An authorized request whose body never finishes is answered within the read timeout.
    let token = std::fs::read_to_string(&f.token).unwrap();
    write!(
        second,
        "POST /v1/op HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {}\r\ncontent-length: 50\r\n\r\n{{",
        token.trim()
    )
    .unwrap();
    let (took, said) = closed_within(&mut second);
    assert!(
        said.starts_with("HTTP/1.1 400") && took < Duration::from_secs(3),
        "{took:?} {said}"
    );
    // Headers that never arrive: the connection is closed after the read timeout.
    let (took, said) = closed_within(&mut first);
    assert!(
        said.is_empty() && took < Duration::from_secs(3),
        "{took:?} {said}"
    );
    drop((first, second));
    std::thread::sleep(Duration::from_millis(300));
    f.client()
        .call(Request::MqInspect {})
        .expect("slots freed when connections close");
}

#[test]
fn a_notification_event_wakes_a_long_poll_like_a_send() {
    // `notify.event` puts its effects on `claude/notify` inside the daemon, so it must announce them
    // as `mq.send` does (`Response::announces_a_send`) — or a consumer's long-poll hears about a
    // permission prompt only at the floor. The floor is pushed out past the test, so only the wake
    // can deliver.
    let f = Fixture::with(|cfg| cfg.poll_floor = Duration::from_mins(1));
    let c = f.client().with_poll_wait(Duration::from_secs(20));
    c.call(Request::MqTopicCreate {
        topic: jkb_core::notify::TOPIC.into(),
        spec: SpecInput::default(),
    })
    .unwrap();
    c.call(Request::MqGroupCreate {
        topic: jkb_core::notify::TOPIC.into(),
        group: "g".into(),
        from_start: true,
    })
    .unwrap();
    let base = f.base.clone();
    let token = f.token.clone();
    let producer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        RemoteBackend::new(&base, token)
            .unwrap()
            .call(Request::NotifyEvent {
                session: "s1".into(),
                event: jkb_api::HookEvent::Needed,
                tool: String::new(),
                message: "Claude needs your permission to use Bash".into(),
                cwd: "/w/wt".into(),
                owner: "4242".into(),
                instance: "host".into(),
            })
            .unwrap()
    });
    let started = Instant::now();
    let Response::Messages { messages } = c
        .call(Request::MqPoll {
            topic: jkb_core::notify::TOPIC.into(),
            group: "g".into(),
            max: 10,
            after: None,
        })
        .unwrap()
    else {
        panic!("expected messages")
    };
    let Response::Notified { sent, .. } = producer.join().unwrap() else {
        panic!("expected notified")
    };
    assert_eq!(sent, 1);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].kind, jkb_core::notify::KIND_POST);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "woken by the event, not the 20 s wait: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_hook_deadline_bounds_a_daemon_that_accepts_and_never_answers() {
    // A hook blocks the Claude Code session for as long as it runs. A daemon wedged mid-request
    // accepts the connection, so the connect timeout does not help; the total deadline must.
    use std::io::Read as _;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    jkb_daemon::token::write(&token, "t").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            held.push(stream);
        }
    });
    let marker = dir.path().join("unreachable");
    let c = RemoteBackend::new(&base, token)
        .unwrap()
        .with_deadlines(Duration::from_millis(200), Duration::from_millis(700))
        .unwrap()
        .with_down_marker(marker.clone());
    let started = Instant::now();
    let err = c.call(Request::NotifyOpenSessions {}).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable, "{}", err.message);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "bounded by the total deadline, not the default 30 s: {:?}",
        started.elapsed()
    );
    // ...and a daemon that CONNECTED and was slow is busy, not down. Marking it down made every
    // other hook in the next few seconds — a permission prompt's among them — give up untried.
    assert!(
        !marker.exists(),
        "a timeout after connecting must not mark the daemon unreachable"
    );
}

/// Reads wait under a budget of their own, so a burst of them cannot take the op permits a hook's
/// write needs; and the daemon bounds each read's answer, which says when it was cut.
#[test]
fn reads_have_their_own_permits_and_a_bounded_answer() {
    let f = Fixture::with(|cfg| cfg.max_reads = 0);
    let c = f.client();
    let refused = c
        .call(Request::KbLs {
            path: None,
            all: false,
            recursive: false,
        })
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Busy, "{refused:?}");
    assert!(refused.message.contains("read limit"), "{refused:?}");
    c.call(Request::MqTopicCreate {
        topic: "claude/notify".into(),
        spec: SpecInput::default(),
    })
    .expect("a write is served while reads are at their limit");

    let f = Fixture::with(|cfg| cfg.read_budget_bytes = 512);
    f.db.write_txn("t", |c, _| {
        for i in 0..100 {
            jkb_core::ns::ensure(c, &format!("n/{i:03}"))?;
        }
        Ok(())
    })
    .unwrap();
    let Response::Listing { rows, truncated } = f
        .client()
        .call(Request::KbLs {
            path: Some("n".into()),
            all: false,
            recursive: false,
        })
        .unwrap()
    else {
        panic!("kb.ls answers with a listing")
    };
    assert!(truncated && rows.len() < 100, "{} rows", rows.len());
}

/// A response body keeps its request's permit until it is written, and a client that never reads it
/// keeps that permit only until the write deadline closes the connection — not until the daemon
/// restarts, which is what a body holding a permit meant before there was a deadline.
#[test]
fn an_unread_answer_holds_its_permit_until_the_write_deadline() {
    use std::io::Write as _;
    let f = Fixture::with(|cfg| {
        cfg.max_reads = 1;
        cfg.write_stall = Duration::from_secs(3);
    });
    // `kb.cat` answers with the whole body, unbudgeted: far more than any socket buffer holds.
    f.db.write_txn("t", |c, m| {
        jkb_core::item::upsert(
            c,
            m,
            &jkb_core::item::NewItem {
                uid: "doc:big".into(),
                kind: "note".into(),
                content: Some("x".repeat(16 * 1024 * 1024)),
                content_hash: None,
                mime: None,
            },
        )
        .map(|_| ())
    })
    .unwrap();
    let token = std::fs::read_to_string(&f.token).unwrap();
    let addr = f.base.trim_start_matches("http://");
    let mut stalled = std::net::TcpStream::connect(addr).unwrap();
    let body = r#"{"op":"kb.cat","uid":"doc:big"}"#;
    write!(
        stalled,
        "POST /v1/op HTTP/1.1\r\nHost: jkb\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        token.trim(),
        body.len()
    )
    .unwrap();
    // Read only the head: it is written after the call returned, so what holds the permit from here is
    // the unwritten body rather than the read still running. Then never read again.
    {
        use std::io::BufRead as _;
        let reader = stalled.try_clone().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut head = std::io::BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            let read = head
                .read_line(&mut line)
                .expect("the response head arrives within 10 s");
            assert_ne!(read, 0, "the daemon closed the connection before its head");
            if line == "\r\n" {
                break;
            }
        }
    }
    std::thread::sleep(Duration::from_millis(300));

    let c = f.client();
    let ls = || {
        c.call(Request::KbLs {
            path: None,
            all: false,
            recursive: false,
        })
    };
    let refused = ls().unwrap_err();
    assert_eq!(
        refused.code,
        ErrorCode::Busy,
        "the unwritten answer still holds the one read permit: {refused:?}"
    );
    let started = Instant::now();
    loop {
        match ls() {
            Ok(_) => break,
            Err(e) if e.code == ErrorCode::Busy && started.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("after {:?}: {e:?}", started.elapsed()),
        }
    }
    drop(stalled);
}
