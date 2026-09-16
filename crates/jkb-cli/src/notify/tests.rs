//! The hook's half: what a payload asks of the daemon, who owns a session, where its pids mean
//! something, and the `SessionStart` sweep. The lifecycle table itself is tested beside it, in
//! `jkb_core::notify`.

use std::cell::Cell;
use std::time::Duration;

use jkb_api::{ApiError, Backend, ErrorCode, HookEvent, LocalBackend, Request, Response};
use jkb_core::Db;
use jkb_fsm::Fact;
use serde_json::json;

use super::{
    append_log, ask, handle, instance_from, owner_from, verdict, Ask, Edge, HOOK_EVENTS,
    LOG_CAP_BYTES,
};

/// Exactly one hook event drives the sweep, and every entry sends something, so a name in the table
/// is never inert.
#[test]
fn exactly_one_hook_event_drives_the_sweep() {
    let sweepers: Vec<&str> = HOOK_EVENTS
        .iter()
        .filter(|(_, e)| e.is_none())
        .map(|(n, _)| *n)
        .collect();
    assert_eq!(sweepers, ["SessionStart"]);
    for (name, _) in HOOK_EVENTS {
        let raw = json!({ "hook_event_name": name, "session_id": "s1" }).to_string();
        let asked = ask(&raw, "", "").unwrap();
        assert!(!asked.requests.is_empty(), "{name} is inert");
        assert_eq!(asked.sweep, *name == "SessionStart", "{name}");
    }
}

/// **The registry's feed.** A session's start and end reach the registry with the process and the
/// payload's own word for why, and the end still withdraws the notification — first, since that is
/// the one a person sees.
#[test]
fn a_session_start_and_end_feed_the_registry() {
    let raw = json!({
        "hook_event_name": "SessionStart", "session_id": "s1", "source": "resume", "cwd": "/w",
    })
    .to_string();
    assert_eq!(
        ask(&raw, "4242", "host").unwrap(),
        Ask {
            requests: vec![Request::SessionStarted {
                session: "s1".into(),
                source: "resume".into(),
                pid: "4242".into(),
                instance: "host".into(),
                cwd: "/w".into(),
            }],
            sweep: true,
            session: "s1".into(),
        }
    );
    let raw = json!({ "hook_event_name": "SessionEnd", "session_id": "s1", "reason": "clear" })
        .to_string();
    let asked = ask(&raw, "4242", "host").unwrap();
    assert!(!asked.sweep);
    let ops: Vec<&str> = asked.requests.iter().map(Request::op).collect();
    assert_eq!(ops, ["notify.event", "session.ended"]);
    assert_eq!(
        asked.requests[1],
        Request::SessionEnded {
            session: "s1".into(),
            reason: "clear".into(),
            pid: "4242".into(),
            instance: "host".into(),
        }
    );
    // A payload without its word still records the event.
    let raw = json!({ "hook_event_name": "SessionEnd", "session_id": "s1" }).to_string();
    let Request::SessionEnded { reason, .. } = &ask(&raw, "", "").unwrap().requests[1] else {
        panic!("expected an end")
    };
    assert_eq!(reason, "unknown");
    // With no session to address, a start still sweeps — the sweep is about other sessions.
    let raw = json!({ "hook_event_name": "SessionStart" }).to_string();
    assert_eq!(
        ask(&raw, "", "").unwrap(),
        Ask {
            requests: Vec::new(),
            sweep: true,
            session: String::new(),
        }
    );
}

/// The payload → request layer: every field the daemon needs, the session sanitized, and the
/// owner and instance handed in rather than read here.
#[test]
fn a_payload_becomes_one_notify_event() {
    let raw = json!({
        "hook_event_name": "Notification",
        "session_id": "../s1",
        "message": "Claude needs your permission to use Bash",
        "cwd": "/w/wt",
    })
    .to_string();
    assert_eq!(
        ask(&raw, "4242", "host#pid:[1]").unwrap(),
        Ask {
            requests: vec![Request::NotifyEvent {
                session: "___s1".into(),
                event: HookEvent::Needed,
                tool: String::new(),
                message: "Claude needs your permission to use Bash".into(),
                cwd: "/w/wt".into(),
                owner: "4242".into(),
                instance: "host#pid:[1]".into(),
            }],
            sweep: false,
            session: "___s1".into(),
        }
    );
    let raw = json!({ "hook_event_name": "PostToolUse", "session_id": "s1", "tool_name": "Bash" })
        .to_string();
    let Some(Request::NotifyEvent { event, tool, .. }) = ask(&raw, "", "").unwrap().requests.pop()
    else {
        panic!("expected an event")
    };
    assert_eq!((event, tool.as_str()), (HookEvent::ToolFinished, "Bash"));
}

/// A payload that names no session, or an event we do not act on, asks nothing — rather than
/// sending an event addressed at an empty id. Malformed input is an error to log, not a panic.
#[test]
fn an_unusable_payload_asks_nothing() {
    for raw in [
        r#"{"hook_event_name":"PostToolUse","session_id":""}"#,
        r#"{"hook_event_name":"PreToolUse","session_id":"s1"}"#,
        r#"{"session_id":"s1"}"#,
        "{}",
    ] {
        assert_eq!(ask(raw, "", "").unwrap(), Ask::default(), "{raw}");
    }
    assert!(ask("not json", "", "").is_err());
}

/// The owner rule, which was the whole of the fifth notification review's must-fix.
///
/// Recording a pid that dies with this invocation made every record read as *provably dead*, so
/// the next session's sweep withdrew a live session's pending prompt. Everything it cannot tell
/// apart from its own short-lived ancestry must come back empty.
#[test]
fn an_owner_that_dies_with_this_call_is_refused() {
    const PARENT: u32 = 111;
    const ME: u32 = 222;
    assert_eq!(owner_from("999", PARENT, ME), "999");
    for (raw, why) in [
        ("111", "the shim, which exits the moment this call returns"),
        ("222", "this process, which is the shim's child"),
        ("0", "not a process"),
        ("", "nothing was passed"),
        ("-1", "not a pid"),
        ("abc", "not a number"),
        ("4294967296", "beyond u32"),
    ] {
        assert_eq!(owner_from(raw, PARENT, ME), "", "{why}");
    }
}

/// The instance is the host alone, or the host and the container boot the namespace marker names —
/// cleaned so it can neither break a log line nor forge the `#` that separates the two.
#[test]
fn the_instance_is_the_host_and_the_container_boot() {
    assert_eq!(instance_from("mac", None, None), "mac", "macOS: no /proc");
    assert_eq!(
        instance_from(
            "c7b8",
            Some("pid=pid:[4026532556]\nmnt=mnt:[4026532553]\n"),
            Some("pid:[4026532556]")
        ),
        "c7b8#pid:[4026532556]/pid:[4026532556]"
    );
    assert_eq!(
        instance_from("c7b8", Some("mnt=mnt:[1]\n"), Some("pid:[7]")),
        "c7b8/pid:[7]",
        "a marker without a pid line names no boot"
    );
    assert_eq!(
        instance_from("a#b/\n", Some("pid=x#y/\u{7}"), Some("n/s")),
        "ab#xy/ns"
    );
}

/// **What the running process actually sends carries its own pid namespace.** `instance_from`'s table
/// above passes the namespace in; this pins that `instance()` reads it — reverting that one argument
/// to `None` left every other test green while a `claude` in a nested sandbox would again probe the
/// outer namespace's pids and withdraw every live prompt (stage-5 re-review). Linux only: macOS has
/// no `/proc`, and its instance is the hostname alone.
#[cfg(target_os = "linux")]
#[test]
fn the_instance_sent_ends_with_this_process_s_pid_namespace() {
    let ns = std::fs::read_link("/proc/self/ns/pid").expect("a pid namespace link on Linux");
    let ns = ns.to_string_lossy();
    let mine = super::instance();
    assert!(
        mine.ends_with(&format!("/{ns}")),
        "{mine:?} does not end with this process's namespace {ns:?}"
    );
}

/// **Proving a session gone is the one decision here that can take a live prompt off the screen**,
/// so everything short of proof is `Unknown`.
#[test]
fn only_a_dead_pid_here_or_an_earlier_boot_of_this_container_is_gone() {
    let me = "c7b8#pid:[2]/pid:[2]";
    let dead = |_| Fact::No;
    let alive = |_| Fact::Yes;
    let unknown = |_| Fact::Unknown;

    assert_eq!(verdict("10", me, me, dead), Fact::No, "a dead pid here");
    assert_eq!(verdict("10", me, me, alive), Fact::Unknown);
    assert_eq!(
        verdict("10", me, me, unknown),
        Fact::Unknown,
        "a probe that could not answer proves nothing"
    );
    assert_eq!(
        verdict("10", "c7b8#pid:[1]/pid:[1]", me, alive),
        Fact::No,
        "another boot of this container: its processes are gone, whatever a pid here says"
    );
    assert_eq!(
        verdict("10", "c7b8#pid:[1]/pid:[9]", me, alive),
        Fact::No,
        "...including a nested sandbox of that earlier boot"
    );
    for (other, why) in [
        ("mac", "the host, seen from a container"),
        ("d9e0#pid:[1]/pid:[1]", "another container"),
        ("c7b8/pid:[2]", "this host with no boot recorded"),
        (
            "c7b8#pid:[2]/pid:[9]",
            "a nested sandbox of THIS boot: its pids are not this namespace's (the review's case)",
        ),
    ] {
        assert_eq!(verdict("10", other, me, dead), Fact::Unknown, "{why}");
    }
    assert_eq!(
        verdict("10", "c7b8#pid:[1]/pid:[1]", "c7b8/pid:[3]", dead),
        Fact::Unknown,
        "a process with no boot of its own cannot call another boot earlier"
    );
    assert_eq!(
        verdict("10", me, "c7b8#pid:[2]/pid:[9]", dead),
        Fact::Unknown,
        "and a nested sandbox cannot probe the outer namespace's pids either"
    );
    assert_eq!(
        verdict("10", "Johns-MBP.local", "johns-mbp.lan", dead),
        Fact::No,
        "the macOS host under a new name is still this machine: probe it"
    );
    assert_eq!(
        verdict("10", "Johns-MBP.local", "johns-mbp.lan", alive),
        Fact::Unknown
    );
    assert_eq!(
        verdict("10", "Johns-MBP.local", me, dead),
        Fact::Unknown,
        "but a container never probes the host's pids"
    );
    assert_eq!(
        verdict("10", me, "Johns-MBP.local", dead),
        Fact::Unknown,
        "nor the host a container's"
    );
    assert_eq!(
        verdict("", me, me, dead),
        Fact::Unknown,
        "no owner, no probe"
    );
}

/// A daemon-backed database for the sweep, with the topic and a group.
fn backend() -> LocalBackend {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    b.call(Request::MqTopicCreate {
        topic: jkb_core::notify::TOPIC.into(),
        spec: jkb_api::SpecInput::default(),
    })
    .unwrap();
    b.call(Request::MqGroupCreate {
        topic: jkb_core::notify::TOPIC.into(),
        group: "g".into(),
        from_start: true,
    })
    .unwrap();
    b
}

fn post(b: &dyn Backend, session: &str, owner: &str, instance: &str) {
    b.call(Request::NotifyEvent {
        session: session.into(),
        event: HookEvent::Needed,
        tool: String::new(),
        message: "Claude needs your permission to use Bash".into(),
        cwd: "/w".into(),
        owner: owner.into(),
        instance: instance.into(),
    })
    .unwrap();
}

fn open(b: &dyn Backend) -> Vec<String> {
    let Response::Sessions { sessions } = b.call(Request::NotifyOpenSessions {}).unwrap() else {
        panic!("expected sessions")
    };
    sessions.into_iter().map(|s| s.session).collect()
}

fn session_start() -> String {
    json!({ "hook_event_name": "SessionStart", "session_id": "new" }).to_string()
}

/// The sweep end to end through the daemon's operations: what is provably gone comes down, and
/// nothing else does.
#[test]
fn the_sweep_withdraws_what_is_provably_gone_and_spares_the_rest() {
    let b = backend();
    let me = "c7b8#pid:[2]";
    post(&b, "dead", "10", me);
    post(&b, "live", "20", me);
    post(&b, "earlier-boot", "20", "c7b8#pid:[1]");
    post(&b, "host", "10", "mac");
    post(&b, "no-owner", "", me);

    let probe = |pid| if pid == 10 { Fact::No } else { Fact::Yes };
    let failures = handle(
        &session_start(),
        &Edge {
            began: std::time::Instant::now(),
            backend: &b,
            owner: "30".into(),
            instance: me.into(),
            probe: &probe,
        },
    );
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(open(&b), ["host", "live", "no-owner"]);
}

/// A backend that re-posts a session under a new owner the moment the sweep has read the records —
/// `claude --resume` keeps the session id and runs a new process.
struct ResumedMidSweep {
    inner: LocalBackend,
    fired: Cell<bool>,
}

impl Backend for ResumedMidSweep {
    fn call(&self, request: Request) -> Result<Response, ApiError> {
        let listing = matches!(request, Request::NotifyOpenSessions {});
        let out = self.inner.call(request);
        if listing && !self.fired.replace(true) {
            post(&self.inner, "s1", "99", "host");
        }
        out
    }
}

/// The race the owner in `notify.gone` exists for: a session resumed between the sweep's read and
/// its withdrawal keeps its new prompt.
#[test]
fn the_sweep_spares_a_session_resumed_after_it_looked() {
    let inner = backend();
    post(&inner, "s1", "10", "host");
    let b = ResumedMidSweep {
        inner,
        fired: Cell::new(false),
    };
    let failures = handle(
        &session_start(),
        &Edge {
            began: std::time::Instant::now(),
            backend: &b,
            owner: "30".into(),
            instance: "host".into(),
            probe: &|_| Fact::No,
        },
    );
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(open(&b.inner), ["s1"], "the resumed session's prompt stays");
}

struct Down;

impl Backend for Down {
    fn call(&self, _: Request) -> Result<Response, ApiError> {
        Err(ApiError::with_code(ErrorCode::Unavailable, "no daemon"))
    }
}

/// A daemon that cannot be reached is a line to log, never an error the hook returns.
#[test]
fn an_unreachable_daemon_is_a_logged_failure() {
    let edge = Edge {
        began: std::time::Instant::now(),
        backend: &Down,
        owner: String::new(),
        instance: "host".into(),
        probe: &|_| Fact::Unknown,
    };
    let stop = json!({ "hook_event_name": "Stop", "session_id": "s1" }).to_string();
    assert_eq!(
        handle(&stop, &edge),
        ["notify.event: Unavailable: no daemon"]
    );
    assert_eq!(
        handle(&session_start(), &edge),
        [
            "session.started: Unavailable: no daemon",
            "notify.open_sessions: Unavailable: no daemon",
            "session.list: Unavailable: no daemon",
        ]
    );
    assert_eq!(handle("not json", &edge).len(), 1);
    let ignored = json!({ "hook_event_name": "PreToolUse", "session_id": "s1" }).to_string();
    assert!(
        handle(&ignored, &edge).is_empty(),
        "nothing asked, nothing logged"
    );
}

/// A daemon that stays down logs a line per tool call, so the log is capped: past the cap it is
/// moved aside and a new one started.
#[test]
fn the_log_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("logs/notify-hook.log");
    append_log(&log, &["first".into()]);
    assert!(std::fs::read_to_string(&log).unwrap().ends_with(" first\n"));
    std::fs::write(&log, vec![b'x'; usize::try_from(LOG_CAP_BYTES).unwrap()]).unwrap();
    append_log(&log, &["after\nthe cap".into()]);
    assert!(dir.path().join("logs/notify-hook.log.1").exists());
    let fresh = std::fs::read_to_string(&log).unwrap();
    assert!(
        fresh.ends_with(" after the cap\n"),
        "one line per failure: {fresh:?}"
    );
    append_log(&log, &[]);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        fresh,
        "nothing to log, nothing written"
    );
}

/// The hook's own client, against a real `jkb serve` on loopback, with the hook's deadlines: one
/// round trip puts the post on the queue.
#[test]
fn a_hook_event_reaches_a_real_daemon_within_the_hook_deadlines() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("jkb.db")).unwrap();
    let local = LocalBackend::new(db.clone());
    local
        .call(Request::MqTopicCreate {
            topic: jkb_core::notify::TOPIC.into(),
            spec: jkb_api::SpecInput::default(),
        })
        .unwrap();
    local
        .call(Request::MqGroupCreate {
            topic: jkb_core::notify::TOPIC.into(),
            group: "g".into(),
            from_start: true,
        })
        .unwrap();
    let token = dir.path().join("daemon/token");
    let cfg = jkb_daemon::server::ServeConfig::new("127.0.0.1:0".parse().unwrap(), token.clone());
    let handle_ = jkb_daemon::server::spawn(db, &cfg).unwrap();
    let remote = jkb_daemon::client::RemoteBackend::new(&format!("http://{}", handle_.addr), token)
        .unwrap()
        .with_deadlines(super::CONNECT, super::TOTAL)
        .unwrap();
    let raw = json!({
        "hook_event_name": "Notification",
        "session_id": "s1",
        "message": "Claude needs your permission to use Bash",
    })
    .to_string();
    let started = std::time::Instant::now();
    let failures = handle(
        &raw,
        &Edge {
            began: std::time::Instant::now(),
            backend: &remote,
            owner: "4242".into(),
            instance: "host".into(),
            probe: &|_| Fact::Unknown,
        },
    );
    assert!(failures.is_empty(), "{failures:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
    let Response::Messages { messages } = local
        .call(Request::MqTail {
            topic: jkb_core::notify::TOPIC.into(),
            limit: 10,
        })
        .unwrap()
    else {
        panic!("expected messages")
    };
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].kind, jkb_core::notify::KIND_POST);
    handle_.shutdown();
}

fn register(b: &dyn Backend, session: &str, pid: &str, instance: &str) {
    b.call(Request::SessionStarted {
        session: session.into(),
        source: "startup".into(),
        pid: pid.into(),
        instance: instance.into(),
        cwd: "/w".into(),
    })
    .unwrap();
}

fn live_sessions(b: &dyn Backend) -> Vec<String> {
    let Response::ClaudeSessions { sessions } =
        b.call(Request::SessionList { all: false }).unwrap()
    else {
        panic!("expected sessions")
    };
    let mut ids: Vec<String> = sessions.into_iter().map(|s| s.session).collect();
    ids.sort();
    ids
}

/// The registry half of the sweep: the starting session registers itself, and the live sessions
/// provably gone — a dead pid here, an earlier boot of this container — are ended; nothing else is.
#[test]
fn the_sweep_ends_the_registry_sessions_that_are_provably_gone() {
    let b = backend();
    let me = "c7b8#pid:[2]";
    register(&b, "dead", "10", me);
    register(&b, "live", "20", me);
    register(&b, "earlier-boot", "20", "c7b8#pid:[1]");
    register(&b, "host", "10", "mac");
    register(&b, "no-pid", "", me);

    let probe = |pid| if pid == 10 { Fact::No } else { Fact::Yes };
    let failures = handle(
        &session_start(),
        &Edge {
            began: std::time::Instant::now(),
            backend: &b,
            owner: "30".into(),
            instance: me.into(),
            probe: &probe,
        },
    );
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(live_sessions(&b), ["host", "live", "new", "no-pid"]);
    let Response::ClaudeSessions { sessions } = b.call(Request::SessionList { all: true }).unwrap()
    else {
        panic!("expected sessions")
    };
    let gone: Vec<(&str, Option<&str>)> = sessions
        .iter()
        .filter(|s| s.ended_at.is_some())
        .map(|s| (s.session.as_str(), s.end_reason.as_deref()))
        .collect();
    assert_eq!(gone.len(), 2, "{gone:?}");
    assert!(gone.iter().all(|(_, r)| *r == Some("gone")), "{gone:?}");
}

/// A backend that restarts a session under a new process the moment the sweep has listed the
/// registry — the `claude --resume` race, on the registry's side.
struct RegistryResumedMidSweep {
    inner: LocalBackend,
    fired: Cell<bool>,
}

impl Backend for RegistryResumedMidSweep {
    fn call(&self, request: Request) -> Result<Response, ApiError> {
        let listing = matches!(request, Request::SessionList { .. });
        let out = self.inner.call(request);
        if listing && !self.fired.replace(true) {
            register(&self.inner, "s1", "99", "host");
        }
        out
    }
}

/// A session resumed between the sweep's listing and its verdict stays live.
#[test]
fn the_sweep_spares_a_registry_session_resumed_after_it_looked() {
    let inner = backend();
    register(&inner, "s1", "10", "host");
    let b = RegistryResumedMidSweep {
        inner,
        fired: Cell::new(false),
    };
    let failures = handle(
        &session_start(),
        &Edge {
            began: std::time::Instant::now(),
            backend: &b,
            owner: "30".into(),
            instance: "host".into(),
            probe: &|pid| if pid == 30 { Fact::Yes } else { Fact::No },
        },
    );
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(live_sessions(&b.inner), ["new", "s1"]);
}

/// A backend whose first answer takes `delay`, recording every op it was asked.
struct SlowFirst {
    inner: LocalBackend,
    delay: Duration,
    asked: std::cell::RefCell<Vec<&'static str>>,
}

impl Backend for SlowFirst {
    fn call(&self, request: Request) -> Result<Response, ApiError> {
        let mut asked = self.asked.borrow_mut();
        if asked.is_empty() {
            std::thread::sleep(self.delay);
        }
        asked.push(request.op());
        drop(asked);
        self.inner.call(request)
    }
}

fn slow_first(delay: Duration) -> SlowFirst {
    SlowFirst {
        inner: backend(),
        delay,
        asked: std::cell::RefCell::new(Vec::new()),
    }
}

/// **The hook stays inside its budget.** A slow first request still goes, but nothing starts after
/// the event's last start time: at `SessionEnd`, whose hooks get 1.5 s, the registry end is not sent
/// once half a second has gone; at `SessionStart` the sweep is not begun once the full request
/// deadline has.
#[test]
fn a_slow_first_request_leaves_the_rest_unsent() {
    let end = json!({ "hook_event_name": "SessionEnd", "session_id": "s1" }).to_string();
    let b = slow_first(super::SESSION_END_SECOND_REQUEST);
    let edge = |b| Edge {
        began: std::time::Instant::now(),
        backend: b,
        owner: "1".into(),
        instance: "host".into(),
        probe: &|_| Fact::Unknown,
    };
    assert_eq!(
        handle(&end, &edge(&b)),
        ["session.ended: out of time; not sent"]
    );
    assert_eq!(*b.asked.borrow(), ["notify.event"]);

    let b = slow_first(Duration::from_millis(1));
    assert!(handle(&end, &edge(&b)).is_empty());
    assert_eq!(
        *b.asked.borrow(),
        ["notify.event", "session.ended"],
        "in time, both go"
    );

    let b = slow_first(super::TOTAL);
    assert_eq!(
        handle(&session_start(), &edge(&b)),
        [format!("notify.open_sessions: {}", super::OUT_OF_TIME)]
    );
    assert_eq!(*b.asked.borrow(), ["session.started"]);
}

/// **The invoking session is never judged by its own sweep.** It was killed and is now being resumed:
/// its earlier process is provably dead, but ending that row now would record a running session as
/// ended if this start's own `session.started` had been lost. The row is left for a later sweep.
#[test]
fn the_sweep_never_judges_the_session_that_is_starting() {
    let b = backend();
    let me = "host";
    register(&b, "new", "10", me);
    register(&b, "other", "10", me);
    let failures = handle(
        &session_start(),
        &Edge {
            began: std::time::Instant::now(),
            backend: &b,
            owner: "30".into(),
            instance: me.into(),
            probe: &|pid| if pid == 30 { Fact::Yes } else { Fact::No },
        },
    );
    assert!(failures.is_empty(), "{failures:?}");
    let Response::ClaudeSessions { sessions } =
        b.call(Request::SessionList { all: false }).unwrap()
    else {
        panic!("expected sessions")
    };
    let mut live: Vec<(String, String)> =
        sessions.into_iter().map(|s| (s.session, s.pid)).collect();
    live.sort();
    assert_eq!(
        live,
        [
            ("new".to_owned(), "10".to_owned()),
            ("new".to_owned(), "30".to_owned())
        ],
        "the other session's dead process is ended; this session's is left"
    );
}
