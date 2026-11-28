//! The forwarder: the bytes it posts, how it picks a target, its sweep against a fake backend, its
//! lock and re-pin check, and a full round against a real `jkb serve` and a real socket.

use std::io::Read as _;
use std::os::unix::net::UnixListener;
use std::sync::Mutex;
use std::time::Duration;

use jkb_api::exec::InboxTarget;
use jkb_api::{ApiError, Backend, ErrorCode, Request, Response};
use serde_json::json;

use super::{
    auth_line, group, host_boot, inbox_body, lock, post, post_any, sweep, Repin, Swept, LEASE_MS,
};

fn target(socket: &std::path::Path, token: &str) -> InboxTarget {
    InboxTarget {
        socket: socket.to_string_lossy().into_owned(),
        token: token.to_owned(),
    }
}

/// Whether this process may make Unix sockets at all. Claude Code's Bash sandbox denies `socket(AF_UNIX)`
/// outright (measured: `EPERM` from Python's `socket.socket(AF_UNIX)` and from `UnixListener::bind`),
/// so a socket test there can only fail for a reason that says nothing about the code. Anywhere else —
/// CI, a plain shell — this is `true`, and every test below runs. A refusal for any other reason is
/// not taken for the sandbox: it fails the test.
fn unix_sockets() -> bool {
    let dir = tempfile::tempdir().unwrap();
    match UnixListener::bind(dir.path().join("probe.sock")) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!(
                "SKIPPED: this sandbox denies Unix sockets ({e}); run outside it to cover the post"
            );
            false
        }
        Err(e) => panic!("binding a Unix socket: {e}"),
    }
}

/// A listener that reads one connection to its end, on a thread.
fn listen(path: &std::path::Path) -> std::thread::JoinHandle<Vec<u8>> {
    let l = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut got = Vec::new();
        s.read_to_end(&mut got).unwrap();
        got
    })
}

/// Version 1's exact bytes (experiment E1, Claude Code 2.1.317): a user message, one JSON line. A
/// change here is a new body version, and must be measured against a live session first.
#[test]
fn the_body_is_version_1_s_bytes_exactly() {
    assert_eq!(jkb_core::exec::INBOX_BODY_VERSION, 1);
    assert_eq!(
        inbox_body("exec-3 \"done\"\nwarning: x"),
        b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"exec-3 \\\"done\\\"\\nwarning: x\"}}\n"
            .to_vec()
    );
    assert_eq!(
        auth_line("t0k"),
        b"{\"type\":\"auth\",\"token\":\"t0k\"}\n".to_vec()
    );
}

/// A post is exactly the auth line and the body, then the connection closes: nothing is read back.
#[test]
fn a_post_sends_the_auth_line_then_the_body_and_closes() {
    if !unix_sockets() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("1.sock");
    let got = listen(&sock);
    post(&target(&sock, "tok"), "hello").unwrap();
    let mut want = auth_line("tok");
    want.extend(inbox_body("hello"));
    assert_eq!(got.join().unwrap(), want);
}

/// The first target that takes the post wins; one gone (no file) or refusing (a socket nobody
/// listens on) is skipped; all failing is an error naming each socket and no token.
#[test]
fn a_post_tries_each_target_and_fails_only_when_all_do() {
    if !unix_sockets() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("gone.sock");
    let refusing = dir.path().join("refusing.sock");
    drop(UnixListener::bind(&refusing).unwrap());
    let live = dir.path().join("live.sock");
    let got = listen(&live);
    let targets = [
        target(&gone, "secret-a"),
        target(&refusing, "secret-b"),
        target(&live, "secret-c"),
    ];
    post_any(&targets, "m").unwrap();
    assert!(got.join().unwrap().ends_with(&inbox_body("m")));

    let why = post_any(&targets[..2], "m").unwrap_err();
    assert!(
        why.contains("gone.sock") && why.contains("refusing.sock"),
        "{why}"
    );
    assert!(!why.contains("secret"), "{why}");
    assert!(post_any(&[], "m").unwrap_err().contains("no live inbox"));
}

/// A backend answering the forwarder's three ops from a script, recording what it was sent.
struct Fake {
    due: Vec<String>,
    claims: Mutex<Vec<Result<Response, ApiError>>>,
    sent: Mutex<Vec<Request>>,
}

impl Backend for Fake {
    fn call(&self, request: Request) -> Result<Response, ApiError> {
        self.sent.lock().unwrap().push(request.clone());
        match request {
            Request::ExecInboxDue { .. } => Ok(Response::ExecInboxDue {
                ids: self.due.clone(),
            }),
            Request::ExecInboxClaim { .. } => self.claims.lock().unwrap().remove(0),
            Request::ExecInboxReport { .. } => Ok(Response::ExecInboxReported { reported: true }),
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// A sweep claims each due id, posts what it won, reports each post — `Ok`, or `Err` with why — and
/// reports nothing for a claim another deliverer won.
#[test]
fn a_sweep_claims_posts_and_reports_each_due_result() {
    if !unix_sockets() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("live.sock");
    let got = listen(&live);
    let claimed = |sock: &std::path::Path| {
        Ok(Response::ExecInboxClaimed {
            text: "[jkb run request] exec-1 finished exit 0: land".into(),
            targets: vec![target(sock, "tok")],
        })
    };
    let fake = Fake {
        due: vec!["exec-1".into(), "exec-2".into(), "exec-3".into()],
        claims: Mutex::new(vec![
            claimed(&live),
            Err(ApiError::with_code(ErrorCode::NotFound, "held")),
            claimed(&dir.path().join("gone.sock")),
        ]),
        sent: Mutex::new(Vec::new()),
    };
    let mut log = Vec::new();
    let swept = sweep(&fake, "h#b", post_any, &mut log).unwrap();
    assert_eq!(
        swept,
        Swept {
            posted: 1,
            failed: 1,
            lost: 1
        }
    );
    assert!(String::from_utf8_lossy(&got.join().unwrap()).contains("exec-1 finished exit 0"));
    let sent = fake.sent.lock().unwrap();
    let reports: Vec<(String, bool, bool)> = sent
        .iter()
        .filter_map(|r| match r {
            Request::ExecInboxReport { id, ok, error } => Some((
                id.clone(),
                *ok,
                error.as_deref().is_some_and(|e| e.contains("gone.sock")),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        reports,
        vec![
            ("exec-1".into(), true, false),
            ("exec-3".into(), false, true)
        ]
    );
    assert!(sent.iter().any(|r| matches!(
        r,
        Request::ExecInboxClaim { instance, lease_ms, .. } if instance == "h#b" && *lease_ms == LEASE_MS
    )));
    let log = String::from_utf8(log).unwrap();
    assert!(
        log.contains("exec-1: posted to") && log.contains("exec-3: not posted"),
        "{log}"
    );
    assert!(!log.contains("tok\""), "{log}");
}

/// A second forwarder finds the lock held, and the first's release frees it.
#[test]
fn the_lock_admits_one_forwarder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("exec-forward.lock");
    let first = lock(&path).unwrap().expect("free");
    assert!(lock(&path).unwrap().is_none(), "held");
    drop(first);
    assert!(lock(&path).unwrap().is_some());
}

/// The re-pin check fires when the watched binary is replaced (a new inode, as `mv -f` makes) or
/// removed, and not while it is the running one.
#[test]
fn the_re_pin_check_fires_when_the_binary_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("jkb");
    std::fs::write(&bin, b"one").unwrap();
    let r = Repin::new(bin.clone(), &bin);
    assert!(!r.replaced());
    let next = dir.path().join(".jkb.new");
    std::fs::write(&next, b"two").unwrap();
    std::fs::rename(&next, &bin).unwrap();
    assert!(r.replaced());
    let r = Repin::new(bin.clone(), &bin);
    std::fs::remove_file(&bin).unwrap();
    assert!(r.replaced());
    assert!(Repin::new(bin.clone(), &dir.path().join("absent")).replaced());
}

#[test]
fn the_boot_and_the_group_are_derived_from_the_instance_and_hostname() {
    assert_eq!(host_boot("h#pid:[1]/pid:[2]"), Some("h#pid:[1]"));
    assert_eq!(host_boot("h#b"), Some("h#b"));
    assert_eq!(host_boot("h/pid:[2]"), None, "the host: no boot");
    assert_eq!(host_boot("h"), None);
    assert_eq!(host_boot("#b"), None);
    assert_eq!(group("a1b2c3"), "inbox/a1b2c3");
    assert_eq!(group("my.host name"), "inbox/my-host-name");
    assert_eq!(group(""), "inbox/host");
}

/// A `jkb serve` with a container credential, a session with an inbox in boot `h#b`, and a run
/// request it filed through a ticket.
struct Served {
    _dir: tempfile::TempDir,
    db: jkb_core::Db,
    handle: Option<jkb_daemon::server::Handle>,
    base: String,
    token_file: std::path::PathBuf,
    container: String,
}

impl Served {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = jkb_core::Db::open(dir.path().join("jkb.db")).unwrap();
        let token_file = dir.path().join("daemon/token");
        let cfg = jkb_daemon::server::ServeConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            token_file.clone(),
        );
        let handle = jkb_daemon::server::spawn(db.clone(), &cfg).unwrap();
        let base = format!("http://{}", handle.addr);
        let mut s = Self {
            _dir: dir,
            db,
            handle: Some(handle),
            base,
            token_file,
            container: String::new(),
        };
        s.container = match s
            .op()
            .call(serde_json::from_value(json!({ "op": "role.rotate_container" })).unwrap())
            .unwrap()
        {
            Response::Granted { token, .. } => token,
            other => panic!("{other:?}"),
        };
        s
    }

    fn op(&self) -> jkb_daemon::client::RemoteBackend {
        jkb_daemon::client::RemoteBackend::new(&self.base, self.token_file.clone()).unwrap()
    }

    fn as_token(&self, token: &str) -> jkb_daemon::client::RemoteBackend {
        self.op().with_token(token.to_owned())
    }

    fn call(b: &dyn Backend, v: &serde_json::Value) -> Response {
        b.call(serde_json::from_value(v.clone()).unwrap())
            .unwrap_or_else(|e| panic!("{v}: {e:?}"))
    }

    /// Session `s1` registers `socket` as its inbox, and files a request through a ticket; the
    /// operator runs it to its end with a `warning:` line. The request's id.
    fn file_and_finish(&self, socket: &std::path::Path) -> String {
        let hook = self.as_token(&self.container);
        Self::call(
            &hook,
            &json!({ "op": "session.started", "session": "s1", "source": "startup", "pid": "42",
                    "instance": "h#b/pid:[9]",
                    "inbox": { "socket": socket, "token": "inbox-token" } }),
        );
        let ticket = match Self::call(
            &hook,
            &json!({ "op": "attest.mint", "session": "s1", "tool_use_id": "t1" }),
        ) {
            Response::Ticket { token } => token,
            other => panic!("{other:?}"),
        };
        let id = match Self::call(
            &self.as_token(&ticket),
            &json!({ "op": "exec.request", "target": "host", "dir": "/w", "dir_side": "host",
                    "argv": ["jkb", "task", "land"], "reason": "land the branch" }),
        ) {
            Response::ExecRequested { request, .. } => request.id,
            other => panic!("{other:?}"),
        };
        self.finish(&id);
        id
    }

    fn finish(&self, id: &str) {
        use base64::Engine as _;
        let op = self.op();
        Self::call(
            &op,
            &json!({ "op": "exec.begin", "id": id, "runner": "app" }),
        );
        let chunk = b"building\nwarning: 1 task is unplanned\n";
        Self::call(
            &op,
            &json!({ "op": "exec.output", "id": id, "runner": "app",
                    "chunk_b64": base64::engine::general_purpose::STANDARD.encode(chunk) }),
        );
        Self::call(
            &op,
            &json!({ "op": "exec.finish", "id": id, "runner": "app", "outcome": "exited",
                    "exit_status": 0, "total_bytes": chunk.len() }),
        );
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.shutdown();
        }
    }
}

/// The whole round against a real `jkb serve`: one sweep with the container credential posts the
/// result — `exec-N finished exit 0: <reason>` and its warning — to the session's socket, and the
/// hook's take afterwards finds nothing, because it was delivered.
#[test]
fn a_sweep_against_jkb_serve_posts_the_result_to_the_session_s_socket() {
    if !unix_sockets() {
        return;
    }
    let s = Served::new();
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("cc.sock");
    let got = listen(&sock);
    let id = s.file_and_finish(&sock);
    let fwd = s.as_token(&s.container);
    let mut log = Vec::new();
    let swept = sweep(&fwd, "h#b", post_any, &mut log).unwrap();
    assert_eq!(swept.posted, 1, "{}", String::from_utf8_lossy(&log));
    let posted = got.join().unwrap();
    let lines: Vec<&[u8]> = posted.split(|b| *b == b'\n').collect();
    assert_eq!(lines.len(), 3, "the auth line, the body, and the end");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(lines[0]).unwrap(),
        json!({ "type": "auth", "token": "inbox-token" })
    );
    let body: serde_json::Value = serde_json::from_slice(lines[1]).unwrap();
    assert_eq!(body["type"], "user");
    let content = body["message"]["content"].as_str().unwrap();
    assert!(
        content.starts_with(&format!(
            "[jkb run request] {id} finished exit 0: land the branch"
        )),
        "{content}"
    );
    assert!(
        content.contains("\nwarning: 1 task is unplanned\n"),
        "{content}"
    );

    match Served::call(
        &fwd,
        &json!({ "op": "exec.inbox.take", "session": "s1", "open": true }),
    ) {
        Response::ExecInboxTaken { lines, open, .. } => {
            assert!(lines.is_empty() && open.is_empty(), "{lines:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        sweep(&fwd, "h#b", post_any, &mut log).unwrap(),
        Swept::default()
    );
    let n = jkb_core::exec::parse_id(&id).unwrap();
    let d =
        s.db.read(move |c| jkb_core::exec_delivery::get(c, n))
            .unwrap()
            .unwrap();
    assert_eq!(d.via, Some(jkb_core::exec_delivery::Via::Socket));
}

/// What the recording poster of [`the_forwarder_posts_on_a_hint_and_exits_when_re_pinned`] was handed:
/// each post's sockets and message.
static LOOP_POSTS: Mutex<Vec<(Vec<String>, String)>> = Mutex::new(Vec::new());

#[allow(clippy::unnecessary_wraps)] // a `Poster`
fn record_loop_post(targets: &[InboxTarget], text: &str) -> Result<(), String> {
    LOOP_POSTS.lock().unwrap().push((
        targets.iter().map(|t| t.socket.clone()).collect(),
        text.to_owned(),
    ));
    Ok(())
}

/// What the recording poster of [`a_sweep_against_jkb_serve_hands_the_poster_the_result`] was handed.
static SERVE_POSTS: Mutex<Vec<(Vec<String>, String)>> = Mutex::new(Vec::new());

#[allow(clippy::unnecessary_wraps)] // a `Poster`
fn record_serve_post(targets: &[InboxTarget], text: &str) -> Result<(), String> {
    SERVE_POSTS.lock().unwrap().push((
        targets.iter().map(|t| t.token.clone()).collect(),
        text.to_owned(),
    ));
    Ok(())
}

/// The round against a real `jkb serve`, with no socket — so it runs inside the agent's sandbox too:
/// the sweep claims the result with the container credential, hands the poster the message and the
/// session's targets, and reports it delivered; the hook's take afterwards finds nothing.
#[test]
fn a_sweep_against_jkb_serve_hands_the_poster_the_result() {
    let s = Served::new();
    let id = s.file_and_finish(std::path::Path::new("/tmp/cc-socks/42.sock"));
    let fwd = s.as_token(&s.container);
    let mut log = Vec::new();
    assert!(
        sweep(&fwd, "h#other", record_serve_post, &mut log)
            .unwrap()
            .posted
            == 0,
        "another container's forwarder"
    );
    let swept = sweep(&fwd, "h#b", record_serve_post, &mut log).unwrap();
    assert_eq!(swept.posted, 1, "{}", String::from_utf8_lossy(&log));
    let posts = SERVE_POSTS.lock().unwrap().clone();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].0, vec!["inbox-token".to_owned()]);
    assert!(
        posts[0].1.starts_with(&format!(
            "[jkb run request] {id} finished exit 0: land the branch"
        )),
        "{}",
        posts[0].1
    );
    assert!(posts[0].1.contains("\nwarning: 1 task is unplanned\n"));
    match Served::call(
        &fwd,
        &json!({ "op": "exec.inbox.take", "session": "s1", "open": true }),
    ) {
        Response::ExecInboxTaken { lines, .. } => assert!(lines.is_empty(), "{lines:?}"),
        other => panic!("{other:?}"),
    }
    let log = String::from_utf8(log).unwrap();
    assert!(
        log.contains(&format!("{id}: posted to /tmp/cc-socks/42.sock")),
        "{log}"
    );
    assert!(!log.contains("inbox-token"), "{log}");
}

/// A sweep refused for its credential stops the forwarder (exit 1), so the supervisor restarts it
/// with the credential re-read; a refusal that clears (the daemon down) is waited out.
#[test]
fn a_refused_credential_stops_the_forwarder() {
    let s = Served::new();
    let cfg = super::Config {
        boot: "h#b".into(),
        group: group("refused"),
        repin: None,
        sweep_every: Duration::from_hours(1),
        repin_every: Duration::from_hours(1),
        poll_interval: Duration::from_millis(50),
        post: record_loop_post,
    };
    let nobody = s.as_token(&"0".repeat(64));
    let mut log = Vec::new();
    assert_eq!(super::run(&nobody, &cfg, &mut log), 1);
    let log = String::from_utf8(log).unwrap();
    assert!(log.contains("sweep refused, stopping"), "{log}");
}

/// The loop wakes on the index topic's hint, not only on its minute's sweep: a result that ends
/// while it runs is posted within the poll interval; and it exits 0 when its binary is replaced.
#[test]
fn the_forwarder_posts_on_a_hint_and_exits_when_re_pinned() {
    let s = Served::new();
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("jkb");
    std::fs::write(&bin, b"pinned").unwrap();
    let cfg = super::Config {
        boot: "h#b".into(),
        group: group("test"),
        repin: Some(Repin::new(bin.clone(), &bin)),
        // Long enough that only a hint can explain a prompt post.
        sweep_every: Duration::from_hours(1),
        repin_every: Duration::from_millis(200),
        poll_interval: Duration::from_millis(50),
        post: record_loop_post,
    };
    let fwd = s.as_token(&s.container);
    std::thread::scope(|scope| {
        let running = scope.spawn(|| {
            let mut log = Vec::new();
            (super::run(&fwd, &cfg, &mut log), log)
        });
        // Let the consumer make its group before the request's hints are sent.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !s
            .db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM mq_groups WHERE name = 'inbox/test'",
                    [],
                    |r| r.get::<_, i64>(0),
                )? > 0)
            })
            .unwrap()
        {
            if std::time::Instant::now() >= deadline || running.is_finished() {
                let (code, log) = running.join().unwrap();
                panic!("no group (exit {code}): {}", String::from_utf8_lossy(&log));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let id = s.file_and_finish(std::path::Path::new("/tmp/cc-socks/7.sock"));
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while LOOP_POSTS.lock().unwrap().is_empty() {
            if std::time::Instant::now() >= deadline {
                std::fs::remove_file(&bin).unwrap();
                let (code, log) = running.join().unwrap();
                panic!(
                    "the hint was not acted on (exit {code}): {}",
                    String::from_utf8_lossy(&log)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let posts = LOOP_POSTS.lock().unwrap().clone();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].0, vec!["/tmp/cc-socks/7.sock".to_owned()]);
        assert!(posts[0].1.contains(&format!("{id} finished exit 0")));
        let next = dir.path().join(".jkb.new");
        std::fs::write(&next, b"re-pinned").unwrap();
        std::fs::rename(&next, &bin).unwrap();
        let (code, log) = running.join().unwrap();
        let log = String::from_utf8(log).unwrap();
        assert_eq!(code, 0, "{log}");
        assert!(log.contains("replaced"), "{log}");
    });
}
