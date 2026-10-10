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
    dir: tempfile::TempDir,
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
            dir,
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
            let _ = h.shutdown();
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
    b.shutdown().unwrap();
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

/// An oversized body is read to its end — to a bound — before the 413 is answered. Answering first
/// closed a socket with the upload unread, and the reset that sends can make the client's kernel drop
/// the 413 unread: `raw_http_is_held_to_the_same_rules` failed that way on macOS under load.
#[test]
fn an_oversized_body_is_drained_before_the_refusal() {
    use std::io::{Read as _, Write as _};
    let f = Fixture::new();
    let token = std::fs::read_to_string(&f.token).unwrap();
    let addr = f.base.trim_start_matches("http://").to_owned();
    let mut sock = std::net::TcpStream::connect(&addr).unwrap();
    let len = 2 * jkb_daemon::MAX_BODY_BYTES;
    write!(
        sock,
        "POST /v1/op HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {}\r\n\
         content-type: application/json\r\ncontent-length: {len}\r\n\r\n",
        token.trim()
    )
    .unwrap();
    let most = len * 3 / 4;
    sock.write_all(&vec![b'x'; most]).unwrap();
    // Past the limit, but the body is not all here: no answer yet.
    sock.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut early = [0u8; 1];
    let premature = sock.read(&mut early);
    assert!(
        premature.is_err(),
        "the refusal went out with the upload unread: {premature:?}"
    );
    sock.write_all(&vec![b'x'; len - most]).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut head = [0u8; 12];
    sock.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"HTTP/1.1 413", "{}", String::from_utf8_lossy(&head));
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

/// A loopback port nothing listens on: bound, then released.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A connect that fails transiently — measured at about 0.5% through Docker Desktop's port forward,
/// with the daemon up throughout — is retried, and does not mark the daemon down. Before the retry,
/// the first refused connect failed the call and wrote the marker, and every client then failed
/// untried for `UNREACHABLE_FOR`. Both send paths are held to it: `call` and `hello`.
#[test]
fn a_connect_refused_briefly_is_retried_and_does_not_mark_the_daemon_down() {
    use std::io::{Read as _, Write as _};
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    jkb_daemon::token::write(&token, "t").unwrap();
    let marker = dir.path().join("unreachable");
    let body = serde_json::to_string(&Response::Position { position: 1 }).unwrap();
    let hello: &dyn Fn(&RemoteBackend) -> serde_json::Value = &|c| c.hello().unwrap();
    let call: &dyn Fn(&RemoteBackend) -> serde_json::Value =
        &|c| serde_json::to_value(c.call(Request::MqInspect {}).unwrap()).unwrap();
    for (name, ask) in [("call", call), ("hello", hello)] {
        let port = free_port();
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: \
             close\r\n\r\n{body}",
            body.len()
        );
        // Nothing listens when the call starts; the port is bound ~120 ms in, between the first
        // retry (at 100 ms) and the second (at 400 ms).
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            stream.write_all(reply.as_bytes()).unwrap();
        });
        let c = RemoteBackend::new(&format!("http://127.0.0.1:{port}"), token.clone())
            .unwrap()
            .with_down_marker(marker.clone());
        assert_eq!(
            ask(&c),
            serde_json::to_value(Response::Position { position: 1 }).unwrap(),
            "{name}"
        );
        server.join().unwrap();
        assert!(
            !marker.exists(),
            "{name}: a retried connect is not remembered as down"
        );
    }
}

/// When every attempt fails to connect, the call fails after all the retries — and only then is the
/// daemon remembered as down.
#[test]
fn a_daemon_refusing_every_connect_is_marked_down_after_the_retries() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    jkb_daemon::token::write(&token, "t").unwrap();
    let marker = dir.path().join("unreachable");
    let c = RemoteBackend::new(&format!("http://127.0.0.1:{}", free_port()), token)
        .unwrap()
        .with_down_marker(marker.clone());
    let started = Instant::now();
    let err = c.call(Request::MqInspect {}).unwrap_err();
    let took = started.elapsed();
    assert_eq!(err.code, ErrorCode::Unavailable, "{}", err.message);
    let retried: Duration = jkb_daemon::client::CONNECT_RETRY_DELAYS.iter().sum();
    assert!(
        took >= retried,
        "gave up after {took:?}, before the {retried:?} of retries"
    );
    assert!(marker.exists(), "every attempt failed: remembered as down");
}

/// The retries live inside the request's own deadline. Two refused connects, then a daemon that
/// accepts and never answers: the call ends by the deadline it started with. When each attempt got a
/// fresh deadline of its own, this ran to ~400 ms of retries plus a whole second deadline — a hook's
/// request past Claude Code's `SessionEnd` budget.
#[test]
fn connect_retries_and_a_slow_answer_finish_by_the_one_deadline() {
    use std::io::Read as _;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    jkb_daemon::token::write(&token, "t").unwrap();
    let marker = dir.path().join("unreachable");
    let port = free_port();
    // Bound ~200 ms in: after the first retry (100 ms), before the second (400 ms).
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let _ = stream.read(&mut buf);
        std::thread::sleep(Duration::from_secs(5));
    });
    let total = Duration::from_millis(700);
    let c = RemoteBackend::new(&format!("http://127.0.0.1:{port}"), token)
        .unwrap()
        .with_deadlines(Duration::from_millis(200), total)
        .unwrap()
        .with_down_marker(marker.clone());
    let started = Instant::now();
    let err = c.call(Request::NotifyOpenSessions {}).unwrap_err();
    let took = started.elapsed();
    assert_eq!(err.code, ErrorCode::Unavailable, "{}", err.message);
    // The second retry connected, so it was the slow answer that ended it, at the deadline.
    assert!(
        took < total + Duration::from_millis(200),
        "took {took:?}, past the request's {total:?} deadline"
    );
    assert!(
        !marker.exists(),
        "the last attempt connected: busy, not down"
    );
}

/// A retry whose pause and connect timeout would not fit before the request's deadline is not made:
/// with 250 ms in all and 200 ms to connect, even the first (100 ms) pause leaves no room, so the
/// refused connect is the only attempt — at once, and remembered as down.
#[test]
fn a_retry_that_would_not_fit_before_the_deadline_is_not_made() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    jkb_daemon::token::write(&token, "t").unwrap();
    let marker = dir.path().join("unreachable");
    let c = RemoteBackend::new(&format!("http://127.0.0.1:{}", free_port()), token)
        .unwrap()
        .with_deadlines(Duration::from_millis(200), Duration::from_millis(250))
        .unwrap()
        .with_down_marker(marker.clone());
    let started = Instant::now();
    let err = c.call(Request::NotifyOpenSessions {}).unwrap_err();
    let took = started.elapsed();
    assert_eq!(err.code, ErrorCode::Unavailable, "{}", err.message);
    assert!(
        took < Duration::from_millis(90),
        "a retry was made: took {took:?}"
    );
    assert!(
        marker.exists(),
        "the only attempt failed to connect: remembered as down"
    );
}

/// A request that CONNECTED and then ran past its deadline is not retried: its one attempt had the
/// whole deadline (a hook's is shorter than the attempt budget), and nothing is left for another.
/// Exactly one connection is made.
#[test]
fn a_request_that_connected_and_timed_out_is_not_retried() {
    use std::io::Read as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    jkb_daemon::token::write(&token, "t").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let accepted = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&accepted);
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            held.push(stream);
        }
    });
    let marker = dir.path().join("unreachable");
    let c = RemoteBackend::new(&base, token)
        .unwrap()
        .with_deadlines(Duration::from_millis(200), Duration::from_millis(300))
        .unwrap()
        .with_down_marker(marker.clone());
    let err = c.call(Request::NotifyOpenSessions {}).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable, "{}", err.message);
    // Long enough for a retry, had there been one, to have connected.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "sent once, never retried"
    );
    assert!(
        !marker.exists(),
        "a timeout after connecting is not a daemon down"
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
    h.shutdown().unwrap();
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
    h.shutdown().unwrap();
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

/// Ingests run under a budget of their own too, in place of an op permit: a burst of large captures,
/// each holding the one writer, is refused past its small limit rather than queued ahead of the hook's
/// writes.
#[test]
fn ingests_have_their_own_permits() {
    // One at a time by default: a capture at the body cap holds the writer about half a second, and two
    // left a hook's write most of its 1 s (measured by the stage-6.3 review; see `max_ingests`).
    let defaults = ServeConfig::new("127.0.0.1:0".parse().unwrap(), "token".into());
    assert_eq!(defaults.max_ingests, 1);
    let f = Fixture::with(|cfg| cfg.max_ingests = 0);
    let c = f.client();
    let refused = c
        .call(Request::IngestText(jkb_api::ingest::IngestAsk {
            text: "t".into(),
            mime: "text/plain".into(),
            namespace: "inbox".into(),
            raw: None,
        }))
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Busy, "{refused:?}");
    assert!(refused.message.contains("ingest limit"), "{refused:?}");
    c.call(Request::MqTopicCreate {
        topic: "claude/notify".into(),
        spec: SpecInput::default(),
    })
    .expect("a write is served while ingests are at their limit");
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

/// Roles over the wire (design D52.3): a role token and a harness ticket reach the daemon as the
/// bearer, are served as the principal they name and held to its role, and a token that names
/// nothing is refused before the body is read. The root token stays the operator.
#[test]
fn a_role_token_and_a_ticket_are_served_as_their_principal_over_http() {
    let f = Fixture::new();
    let op = f.client();
    let call = |b: &RemoteBackend, r: serde_json::Value| {
        b.call(serde_json::from_value(r).expect("request parses"))
    };
    let Response::Added { added } =
        call(&op, json!({ "op": "task.add", "text": "the work" })).unwrap()
    else {
        panic!("added")
    };
    let uid = added.uid;
    let Response::Granted { token, .. } = call(
        &op,
        json!({ "op": "role.grant", "role": "reviewer", "task": uid, "agent": "rev" }),
    )
    .unwrap() else {
        panic!("granted")
    };
    let reviewer = f.client().with_token(token.clone());
    let Response::WhoAmI { whoami } = call(&reviewer, json!({ "op": "role.whoami" })).unwrap()
    else {
        panic!("whoami")
    };
    assert_eq!(whoami.roles, vec!["reviewer"]);
    let e = call(
        &reviewer,
        json!({ "op": "task.edit", "uid": uid, "text": "x", "append": true }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");

    // A token naming nothing is unauthorized — and not retried, having nothing fresher to try.
    let nobody = f.client().with_token("0".repeat(64));
    assert_eq!(
        call(&nobody, json!({ "op": "kb.ls" })).unwrap_err().code,
        ErrorCode::Unauthorized
    );

    // The container credential mints a ticket; the ticket authenticates; released, it does not.
    let Response::Granted {
        token: container, ..
    } = call(&op, json!({ "op": "role.rotate_container" })).unwrap()
    else {
        panic!("rotated")
    };
    let hook = f.client().with_token(container);
    let Response::Ticket { token: ticket } = call(
        &hook,
        json!({ "op": "attest.mint", "session": "s", "tool_use_id": "t1" }),
    )
    .unwrap() else {
        panic!("ticket")
    };
    let main = f.client().with_token(ticket.clone());
    call(&main, json!({ "op": "kb.ls" })).expect("a live ticket authenticates");
    call(
        &hook,
        json!({ "op": "attest.release", "session": "s", "tool_use_id": "t1" }),
    )
    .unwrap();
    assert_eq!(
        call(&main, json!({ "op": "kb.ls" })).unwrap_err().code,
        ErrorCode::Unauthorized
    );

    // A revoked grant is refused on its very next request, cache or no cache.
    let Response::Grants { listing } = call(&op, json!({ "op": "role.list" })).unwrap() else {
        panic!("listed")
    };
    let rev_id = listing.grants.iter().find(|g| g.agent == "rev").unwrap().id;
    call(&op, json!({ "op": "role.revoke", "id": rev_id })).unwrap();
    assert_eq!(
        call(&reviewer, json!({ "op": "kb.ls" })).unwrap_err().code,
        ErrorCode::Unauthorized
    );
}

/// A grant revoked behind the daemon's back — by the host CLI, straight into the database — is still
/// in the daemon's cache. Its next request is refused, and the connection with it, so the holder
/// cannot keep a slot open past the read timeout on a token that no longer names anything.
#[test]
fn a_grant_revoked_behind_the_cache_is_refused_and_its_connection_closed() {
    let f = Fixture::new();
    let op = f.client();
    let Response::Granted { token, grant } = op
        .call(
            serde_json::from_value(
                json!({ "op": "role.grant", "role": "coordinator", "agent": "c" }),
            )
            .unwrap(),
        )
        .unwrap()
    else {
        panic!("granted")
    };
    let http = reqwest::blocking::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let ls = || {
        http.post(format!("{}/v1/op", f.base))
            .bearer_auth(&token)
            .header("content-type", "application/json")
            .body(json!({ "op": "kb.ls" }).to_string())
            .send()
            .unwrap()
    };
    assert_eq!(ls().status(), 200, "admitted, and now in the cache");
    let id = grant.id;
    f.db.write_txn("host-cli", move |c, m| jkb_core::roles::revoke(c, m, id))
        .unwrap();
    let r = ls();
    assert_eq!(r.status(), 401);
    assert_eq!(
        r.headers().get("connection").map(|v| v.to_str().unwrap()),
        Some("close"),
        "refused like any unauthenticated request, not served on a kept-alive connection"
    );
}

/// `POST /v1/op` with `token`, and an `Idempotency-Key` when one is given: the status and the body.
/// `POST /v1/op`, or the keyed route when there is a key.
fn post(f: &Fixture, token: &str, key: Option<&str>, body: &serde_json::Value) -> (u16, String) {
    let path = if key.is_some() {
        jkb_daemon::server::KEYED_PATH
    } else {
        "/v1/op"
    };
    post_at(f, path, token, key, body)
}

fn post_at(
    f: &Fixture,
    path: &str,
    token: &str,
    key: Option<&str>,
    body: &serde_json::Value,
) -> (u16, String) {
    let http = reqwest::blocking::Client::new();
    let mut req = http
        .post(format!("{}{path}", f.base))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(key) = key {
        req = req.header("idempotency-key", key);
    }
    let resp = req.send().unwrap();
    (resp.status().as_u16(), resp.text().unwrap())
}

/// Wait, up to 5 s, for `done`.
fn wait_until(done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(5), "never happened");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn root_token(f: &Fixture) -> String {
    jkb_daemon::token::read(&f.token).unwrap()
}

fn tasks(f: &Fixture) -> i64 {
    f.db.read(|c| {
        Ok(
            c.query_row("SELECT count(*) FROM items WHERE kind = 'task'", [], |r| {
                r.get(0)
            })?,
        )
    })
    .unwrap()
}

/// A write sent twice with one key is applied once, and both sends get the same answer. A key reused
/// for a different request is refused; a request refused before it ran (a body that does not parse)
/// records nothing, so its key then runs the real request.
#[test]
fn a_keyed_write_sent_twice_is_applied_once_and_answered_alike() {
    let f = Fixture::new();
    let token = root_token(&f);
    let add = json!({ "op": "task.add", "text": "once" });
    let first = post(&f, &token, Some("k1"), &add);
    assert_eq!(first.0, 200, "{first:?}");
    assert_eq!(post(&f, &token, Some("k1"), &add), first, "replayed");
    assert_eq!(tasks(&f), 1, "applied once");

    let other = post(
        &f,
        &token,
        Some("k1"),
        &json!({ "op": "task.add", "text": "other" }),
    );
    assert_eq!(other.0, 400, "{other:?}");
    assert!(other.1.contains("different request"), "{other:?}");

    let malformed = post(&f, &token, Some("k2"), &json!({ "op": "no.such.op" }));
    assert_eq!(malformed.0, 400, "{malformed:?}");
    assert_eq!(
        post(&f, &token, Some("k2"), &add).0,
        200,
        "the key was not spent"
    );
    assert_eq!(tasks(&f), 2);
}

/// `/v1/op` runs every request, as before keys existed — with or without the header, which it
/// ignores.
#[test]
fn a_request_to_v1_op_runs_every_time_key_or_not() {
    let f = Fixture::new();
    let token = root_token(&f);
    let add = json!({ "op": "task.add", "text": "twice" });
    for key in [None, Some("k")] {
        let before = tasks(&f);
        let a = post_at(&f, "/v1/op", &token, key, &add);
        let b = post_at(&f, "/v1/op", &token, key, &add);
        assert_eq!((a.0, b.0), (200, 200), "{key:?}");
        assert_ne!(a.1, b.1, "{key:?}: two tasks, two answers");
        assert_eq!(tasks(&f) - before, 2, "{key:?}");
    }
}

/// The keyed route requires the header, and refuses a long-poll, which is never keyed (it must be
/// served on its handler, so a client that hangs up frees its group's slot). Neither runs the op.
#[test]
fn the_keyed_route_needs_a_key_and_refuses_a_long_poll() {
    let f = Fixture::new();
    topic_and_group(&f.client());
    let token = root_token(&f);
    let keyed = jkb_daemon::server::KEYED_PATH;
    let add = json!({ "op": "task.add", "text": "t" });
    let missing = post_at(&f, keyed, &token, None, &add);
    assert_eq!(missing.0, 400, "{missing:?}");
    assert!(
        missing.1.contains("needs an Idempotency-Key"),
        "{missing:?}"
    );
    assert_eq!(tasks(&f), 0);
    let poll = json!({ "op": "mq.poll", "topic": "t", "group": "g", "max": 10 });
    let refused = post_at(
        &f,
        &format!("{keyed}?wait_ms=100"),
        &token,
        Some("k"),
        &poll,
    );
    assert_eq!(refused.0, 400, "{refused:?}");
    assert!(refused.1.contains("not keyed"), "{refused:?}");
}

/// A resend that arrives while the original is still running waits for it and gets its answer,
/// rather than running the write a second time. The original is held up by a write transaction the
/// test holds on the daemon's one writer, released only once the daemon reports the resend waiting.
///
/// A resend waits no longer than `duplicate_wait`: past it, it is told `unavailable` — the original
/// is still running and may yet apply, so not `busy`, which says nothing ran — and the original still
/// answers and is applied once.
///
/// With one op permit, held by the original, the resend still waits rather than being refused `busy`:
/// a known key is looked up before the permit is asked, and a client told `busy` would retry under a
/// fresh key.
#[test]
fn a_resend_while_the_original_runs_waits_for_its_answer() {
    // `outwait`: the resend's wait runs out before the original is released.
    let send_twice = |f: &Fixture, outwait: bool| {
        let token = root_token(f);
        let handle = f.handle.as_ref().unwrap();
        let (holding, held) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let db = f.db.clone();
        let writer = std::thread::spawn(move || {
            db.write_txn("test", move |_, _| {
                holding.send(()).unwrap();
                let _ = released.recv();
                Ok(())
            })
            .unwrap();
        });
        held.recv().unwrap();
        let add = json!({ "op": "task.add", "text": "once" });
        let answers = std::thread::scope(|s| {
            let first = s.spawn(|| post(f, &token, Some("k"), &add));
            wait_until(|| handle.keyed_in_progress() == 1);
            let second = s.spawn(|| post(f, &token, Some("k"), &add));
            wait_until(|| handle.keyed_waiting() == 1);
            let second = if outwait {
                let second = second.join().unwrap();
                release.send(()).unwrap();
                second
            } else {
                release.send(()).unwrap();
                second.join().unwrap()
            };
            (first.join().unwrap(), second)
        });
        writer.join().unwrap();
        answers
    };
    let f = Fixture::with(|cfg| cfg.max_ops = 1);
    let (first, second) = send_twice(&f, false);
    assert_eq!(first.0, 200, "{first:?}");
    assert_eq!(second, first, "the same answer");
    assert_eq!(tasks(&f), 1, "applied once");

    let f = Fixture::with(|cfg| cfg.duplicate_wait = Duration::from_millis(200));
    let (first, second) = send_twice(&f, true);
    assert_eq!(first.0, 200, "{first:?}");
    assert_eq!(second.0, 503, "{second:?}");
    assert!(
        second.1.contains("unavailable") && second.1.contains("may yet apply"),
        "{second:?}"
    );
    assert_eq!(tasks(&f), 1, "applied once");
}

/// A burst of keyed writes cannot evict a write's answer its client may still resend for: past the
/// cap, the burst is refused `busy` — nothing ran — and the young answer is still replayed.
#[test]
fn a_burst_of_keyed_writes_does_not_evict_a_young_answer() {
    let f = Fixture::with(|cfg| cfg.idempotency_entries = 2);
    let token = root_token(&f);
    let add = json!({ "op": "task.add", "text": "t" });
    let first = post(&f, &token, Some("first"), &add);
    assert_eq!(first.0, 200, "{first:?}");
    let mut refused = 0;
    for i in 0..8 {
        let (status, body) = post(&f, &token, Some(&format!("burst-{i}")), &add);
        if status == 503 {
            assert!(body.contains("busy") && body.contains("full"), "{body}");
            refused += 1;
        }
    }
    assert!(refused > 0, "the store filled");
    let tasks_before = tasks(&f);
    assert_eq!(post(&f, &token, Some("first"), &add), first, "replayed");
    assert_eq!(tasks(&f), tasks_before, "not run again");
}

/// Keys are scoped by the token presented: another caller sending the same key is served its own
/// answer, not the first caller's.
#[test]
fn another_caller_s_key_is_not_answered_from_mine() {
    let f = Fixture::new();
    let token = root_token(&f);
    let (status, granted) = post(
        &f,
        &token,
        None,
        &json!({ "op": "role.grant", "role": "reviewer", "agent": "rev" }),
    );
    assert_eq!(status, 200, "{granted}");
    let granted: Response = serde_json::from_str(&granted).unwrap();
    let Response::Granted {
        token: reviewer, ..
    } = granted
    else {
        panic!("granted")
    };
    let whoami = json!({ "op": "role.whoami" });
    let mine = post(&f, &token, Some("shared"), &whoami);
    let theirs = post(&f, &reviewer, Some("shared"), &whoami);
    assert_eq!((mine.0, theirs.0), (200, 200), "{mine:?} {theirs:?}");
    assert_ne!(theirs.1, mine.1, "served the operator's answer");
    assert!(theirs.1.contains("reviewer"), "{theirs:?}");
}

/// An answer is kept for the configured time and within the configured count: past either, the key
/// runs its request again.
#[test]
fn expired_and_evicted_keys_run_again() {
    let add = json!({ "op": "task.add", "text": "t" });
    let f = Fixture::with(|cfg| cfg.idempotency_ttl = Duration::from_millis(300));
    let token = root_token(&f);
    post(&f, &token, Some("k"), &add);
    post(&f, &token, Some("k"), &add);
    assert_eq!(tasks(&f), 1, "within the TTL");
    std::thread::sleep(Duration::from_millis(400));
    post(&f, &token, Some("k"), &add);
    assert_eq!(tasks(&f), 2, "expired: run again");

    // Past the cap, an answer old enough — past `idempotency_min_age` — goes for a new one.
    let f = Fixture::with(|cfg| {
        cfg.idempotency_entries = 1;
        cfg.idempotency_min_age = Duration::from_millis(100);
    });
    let token = root_token(&f);
    post(&f, &token, Some("a"), &add);
    std::thread::sleep(Duration::from_millis(150));
    post(&f, &token, Some("b"), &add);
    post(&f, &token, Some("b"), &add);
    assert_eq!(tasks(&f), 2, "b's answer kept");
    std::thread::sleep(Duration::from_millis(150));
    post(&f, &token, Some("a"), &add);
    assert_eq!(tasks(&f), 3, "a's answer was evicted for b's");
}

/// A keyed request refused before it ran — here a write refused `schema_newer` by the check ahead of
/// every op, which goes through the keyed path — records nothing: once the database is servable again,
/// a resend with the same key runs, rather than being answered with the refusal.
#[test]
fn a_keyed_request_refused_before_it_ran_is_not_recorded() {
    let f = Fixture::new();
    let token = root_token(&f);
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
    let add = json!({ "op": "task.add", "text": "once" });
    let refused = post(&f, &token, Some("k"), &add);
    assert_eq!(refused.0, 503, "{refused:?}");
    assert!(refused.1.contains("schema_newer"), "{refused:?}");
    // Undone behind the daemon's back: every write through it now refuses the newer schema.
    rusqlite::Connection::open(f.dir.path().join("jkb.db"))
        .unwrap()
        .execute(
            "DELETE FROM refinery_schema_history WHERE version = ?1",
            [future],
        )
        .unwrap();
    let ran = post(&f, &token, Some("k"), &add);
    assert_eq!(ran.0, 200, "{ran:?}");
    assert_eq!(tasks(&f), 1);
}
