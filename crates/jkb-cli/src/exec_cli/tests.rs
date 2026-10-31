//! `jkb exec request`'s wait: the output it prints and the status it exits with, against a backend
//! standing in for the daemon, with the runner's moves made directly on the record.

use std::path::Path;
use std::time::Duration;

use jkb_api::exec::ExecView;
use jkb_api::{Backend, LocalBackend, Request, Response};
use jkb_core::exec::{self, Ask, ExecTarget, Finished, Outcome, RefuseReason, Requester};
use jkb_core::Db;

use super::{check_visible, exit_code, parse_timeout, wait, TargetArg};

fn requested(db: &Db, argv: &str) -> (i64, ExecView) {
    let argv = argv.to_owned();
    let id = db
        .write_txn("t", move |c, m| {
            let r = exec::request(
                c,
                m,
                &Requester {
                    session: "s1".into(),
                    agent_type: None,
                    role: "coordinator".into(),
                    task: None,
                },
                &Ask {
                    target: ExecTarget::Host,
                    dir: "/w".into(),
                    dir_side: ExecTarget::Host,
                    argv: vec![argv],
                    reason: String::new(),
                    pin: None,
                    timeout_ms: None,
                },
                jkb_core::mq::now_ms(),
            )?;
            Ok(r.row.id)
        })
        .unwrap();
    let b = LocalBackend::new(db.clone());
    let view = match b
        .call(Request::ExecStatus {
            id: exec::wire_id(id),
        })
        .unwrap()
    {
        Response::ExecRequest { request } => request,
        other => panic!("{other:?}"),
    };
    (id, view)
}

fn ran(db: &Db, id: i64, chunks: &[&[u8]], outcome: Outcome, status: Option<i64>) {
    let chunks: Vec<Vec<u8>> = chunks.iter().map(|c| c.to_vec()).collect();
    db.write_txn("t", move |c, m| {
        let now = jkb_core::mq::now_ms();
        exec::begin(c, m, id, None, "app", now)?;
        for chunk in &chunks {
            exec::output(c, m, id, "app", chunk, now)?;
        }
        exec::finish(
            c,
            m,
            id,
            &Finished {
                runner: "app",
                outcome,
                exit_status: status,
                tail: b"",
                total_bytes: 0,
                log_path: Some("/logs/x.log"),
            },
            now,
        )?;
        Ok(())
    })
    .unwrap();
}

fn waited(db: &Db, view: &ExecView) -> (i32, String, String) {
    let b = LocalBackend::new(db.clone());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = wait(&b, view, &mut out, &mut err, Duration::from_millis(10)).unwrap();
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn the_output_streams_and_the_exit_status_passes_through() {
    let db = Db::open_in_memory().unwrap();
    let (id, view) = requested(&db, "make");
    ran(&db, id, &[b"one\n", b"two\n"], Outcome::Exited, Some(3));
    let (code, out, _) = waited(&db, &view);
    assert_eq!(code, 3);
    assert_eq!(out, "one\ntwo\n");
}

#[test]
fn a_request_answered_while_waiting_is_followed_to_its_end() {
    let db = Db::open_in_memory().unwrap();
    let (id, view) = requested(&db, "make");
    let runner = {
        let db = db.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            ran(&db, id, &[b"late\n"], Outcome::Exited, Some(0));
        })
    };
    let (code, out, err) = waited(&db, &view);
    runner.join().unwrap();
    assert_eq!((code, out.as_str()), (0, "late\n"));
    assert!(err.contains("/logs/x.log"), "{err}");
}

#[test]
fn a_decline_exits_77_with_the_note_on_stderr() {
    let db = Db::open_in_memory().unwrap();
    let (id, view) = requested(&db, "make");
    db.write_txn("t", move |c, m| {
        exec::decline(
            c,
            m,
            id,
            "ask for `make check` instead",
            jkb_core::mq::now_ms(),
        )
    })
    .unwrap();
    let (code, out, err) = waited(&db, &view);
    assert_eq!(code, super::EXIT_DECLINED);
    assert!(out.is_empty());
    assert!(err.contains("ask for `make check` instead"), "{err}");
}

#[test]
fn a_refusal_and_an_expiry_have_their_own_codes() {
    let db = Db::open_in_memory().unwrap();
    let (a, va) = requested(&db, "a");
    db.write_txn("t", move |c, m| {
        exec::refuse(c, m, a, RefuseReason::TreeChanged, jkb_core::mq::now_ms())
    })
    .unwrap();
    let (code, _, err) = waited(&db, &va);
    assert_eq!(code, super::EXIT_REFUSED);
    assert!(err.contains("tree_changed"), "{err}");

    let (_, vb) = requested(&db, "b");
    db.write_txn("t", |c, m| {
        exec::expire_session(c, m, "s1", jkb_core::mq::now_ms())
    })
    .unwrap();
    assert_eq!(waited(&db, &vb).0, super::EXIT_EXPIRED);
}

#[test]
fn a_signal_and_a_timeout_map_as_a_shell_would() {
    let db = Db::open_in_memory().unwrap();
    let (a, va) = requested(&db, "a");
    ran(&db, a, &[], Outcome::Signalled, Some(9));
    assert_eq!(waited(&db, &va).0, 137);
    let (b, vb) = requested(&db, "b");
    ran(&db, b, &[], Outcome::TimedOut, None);
    assert_eq!(waited(&db, &vb).0, super::EXIT_TIMED_OUT);
}

/// A full topic drops chunks; the waiter says so, naming the log, rather than passing the run off
/// as complete.
#[test]
fn output_the_topic_dropped_is_reported_as_a_gap() {
    let db = Db::open_in_memory().unwrap();
    let (id, view) = requested(&db, "make");
    db.write_txn("t", move |c, m| {
        let now = jkb_core::mq::now_ms();
        exec::begin(c, m, id, None, "app", now)?;
        let chunk = vec![b'x'; exec::MAX_CHUNK_BYTES];
        // Fill the topic, then one more chunk that is dropped.
        while exec::output(c, m, id, "app", &chunk, now)?.0.is_some() {}
        exec::finish(
            c,
            m,
            id,
            &Finished {
                runner: "app",
                outcome: Outcome::Exited,
                exit_status: Some(0),
                tail: b"",
                total_bytes: 0,
                log_path: Some("/logs/x.log"),
            },
            now,
        )?;
        Ok(())
    })
    .unwrap();
    let (code, out, err) = waited(&db, &view);
    assert_eq!(code, 0);
    assert!(!out.is_empty(), "what reached the stream is printed");
    assert!(err.contains("did not reach this stream"), "{err}");
    assert!(
        err.contains("output missing") && err.contains("/logs/x.log"),
        "{err}"
    );
}

/// A gap in the middle of the stream is reported where it happens.
#[test]
fn a_missing_chunk_number_is_reported() {
    let mut f = super::Follow {
        after: 0,
        next_n: 0,
        printed: 0,
        gaps: Vec::new(),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    for n in [0, 3, 3, 4] {
        f.chunk(
            &serde_json::json!({ "n": n, "b64": "eA==" }),
            &mut out,
            &mut err,
        )
        .unwrap();
    }
    assert_eq!(out, b"xxx", "a repeated chunk is printed once");
    assert_eq!(f.gaps, vec![(1, 2)]);
    assert!(String::from_utf8(err).unwrap().contains("chunks 1–2"));
}

/// A waiter gives up once nothing it waits for can still arrive.
#[test]
fn a_wait_past_the_latest_end_gives_up() {
    let db = Db::open_in_memory().unwrap();
    let (_, mut view) = requested(&db, "make");
    view.created_at = 0;
    let b = LocalBackend::new(db.clone());
    let e = wait(
        &b,
        &view,
        &mut Vec::new(),
        &mut Vec::new(),
        Duration::from_millis(1),
    )
    .unwrap_err();
    assert!(e.to_string().contains("latest a request can end"), "{e}");
}

#[test]
fn exit_codes_are_none_until_the_request_ends() {
    let db = Db::open_in_memory().unwrap();
    let (_, view) = requested(&db, "make");
    assert_eq!(exit_code(&view), None);
}

#[test]
fn a_timeout_reads_as_seconds_minutes_or_hours() {
    assert_eq!(parse_timeout("90s"), Ok(90_000));
    assert_eq!(parse_timeout("15m"), Ok(900_000));
    assert_eq!(parse_timeout("2h"), Ok(7_200_000));
    assert_eq!(parse_timeout("30"), Ok(30_000));
    for bad in ["", "m", "1d", "1.5h", "-1s"] {
        assert!(parse_timeout(bad).is_err(), "{bad}");
    }
}

#[test]
fn the_host_runs_only_what_it_can_see() {
    let repos = Path::new(crate::session::CONTAINER_REPOS);
    check_visible(&repos.join("jkb"), TargetArg::Container, TargetArg::Host).unwrap();
    let e = check_visible(Path::new("/tmp/x"), TargetArg::Container, TargetArg::Host).unwrap_err();
    assert!(e.to_string().contains("--target container"), "{e}");
    check_visible(
        Path::new("/tmp/x"),
        TargetArg::Container,
        TargetArg::Container,
    )
    .unwrap();
    check_visible(Path::new("/tmp/x"), TargetArg::Host, TargetArg::Host).unwrap();
}

#[test]
fn exec_runs_through_the_daemon_and_its_pin_needs_neither() {
    use crate::remote::{support, Support};
    use clap::Parser as _;
    let parse = |args: &[&str]| {
        crate::Cli::try_parse_from(std::iter::once("jkb").chain(args.iter().copied()))
            .unwrap()
            .command
    };
    assert_eq!(
        support(&parse(&[
            "exec", "request", "--target", "host", "--dir", ".", "--", "ls"
        ])),
        Support::Ported
    );
    assert_eq!(
        support(&parse(&["exec", "status", "exec-1"])),
        Support::Ported
    );
    assert_eq!(support(&parse(&["exec", "pin"])), Support::NoDatabase);
    // The argv is everything after `--`, options included.
    match parse(&[
        "exec",
        "request",
        "--target",
        "container",
        "--dir",
        "/w",
        "--",
        "ls",
        "-la",
    ]) {
        crate::Command::Exec {
            cmd: super::ExecCmd::Request { argv, .. },
        } => assert_eq!(argv, vec!["ls", "-la"]),
        _ => panic!("not an exec request"),
    }
}

/// Begin `id` as long ago as a settle needs to call it interrupted at the next read.
fn began_long_ago(db: &Db, id: i64) {
    db.write_txn("t", move |c, m| {
        let long_ago =
            jkb_core::mq::now_ms() - exec::DEFAULT_TIMEOUT_MS - exec::RUN_GRACE_MS - 60_000;
        exec::begin(c, m, id, None, "app", long_ago)?;
        Ok(())
    })
    .unwrap();
}

/// `settle`'s interrupted is provisional: the waiter keeps following, and exits with the runner's
/// own late ending.
#[test]
fn a_settled_interrupt_is_waited_past_for_the_runner_s_own_end() {
    let db = Db::open_in_memory().unwrap();
    let (id, view) = requested(&db, "make");
    began_long_ago(&db, id);
    let runner = {
        let db = db.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            db.write_txn("t", move |c, m| {
                exec::finish(
                    c,
                    m,
                    id,
                    &Finished {
                        runner: "app",
                        outcome: Outcome::Exited,
                        exit_status: Some(0),
                        tail: b"",
                        total_bytes: 0,
                        log_path: None,
                    },
                    jkb_core::mq::now_ms(),
                )
            })
            .unwrap();
        })
    };
    let (code, _, err) = waited(&db, &view);
    runner.join().unwrap();
    assert_eq!(
        code, 0,
        "the waiter acted on the provisional interrupt: {err}"
    );
    assert!(err.contains("waiting for its runner"), "{err}");
}

/// With no runner report by the latest a request can end, the provisional ending stands.
#[test]
fn a_settled_interrupt_stands_at_the_deadline() {
    let db = Db::open_in_memory().unwrap();
    let (id, mut view) = requested(&db, "make");
    began_long_ago(&db, id);
    view.created_at = jkb_core::mq::now_ms() - exec::LATEST_END_MS - 60_000 + 300;
    assert_eq!(waited(&db, &view).0, super::EXIT_TIMED_OUT);
}
