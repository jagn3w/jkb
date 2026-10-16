//! The run-request record's rules: the fold, at most one run, the pin, expiry, the runner, and that
//! neither `jkb undo` nor a queue client can rewrite what was asked or what it ended with.

use proptest::prelude::*;

use super::{
    begin, decline, expire_if_ended, expire_session, finish, get, list, output, output_of,
    parse_id, refuse, request, status, topic, Ask, Begun, ExecState, ExecTarget, Filter, Finished,
    Outcome, RefuseReason, Requested, Requester, TreePin, DEFAULT_TIMEOUT_MS, EXPIRES_AFTER_MS,
    MAX_ARGV_BYTES, MAX_CHUNK_BYTES, MAX_TIMEOUT_MS,
};
use crate::claude_session::{self, Process};
use crate::mq::{self, Draft, QueueError, TopicSpec};
use crate::{Db, Error};

const T0: i64 = 1_800_000_000_000;

fn pin(n: u8) -> TreePin {
    TreePin {
        head: format!("{n:02x}").repeat(20),
        changes: format!("{n:02x}").repeat(32),
    }
}

fn who(session: &str) -> Requester {
    Requester {
        session: session.to_owned(),
        agent_type: Some("implementer".to_owned()),
        role: "implementer".to_owned(),
        task: None,
    }
}

fn ask(argv: &[&str], pin_: Option<TreePin>) -> Ask {
    Ask {
        target: ExecTarget::Host,
        dir: "/w/repo".to_owned(),
        dir_side: ExecTarget::Host,
        argv: argv.iter().map(|s| (*s).to_owned()).collect(),
        reason: "run the gate the sandbox cannot".to_owned(),
        pin: pin_,
        timeout_ms: None,
    }
}

fn try_req(db: &Db, r: &Requester, a: &Ask, now: i64) -> crate::Result<Requested> {
    let (r, a) = (r.clone(), a.clone());
    db.write_txn("t", move |c, m| request(c, m, &r, &a, now))
}

fn req(db: &Db, r: &Requester, a: &Ask, now: i64) -> Requested {
    try_req(db, r, a, now).unwrap()
}

fn try_begin(db: &Db, id: i64, p: Option<TreePin>, runner: &str, now: i64) -> crate::Result<Begun> {
    let runner = runner.to_owned();
    db.write_txn("t", move |c, m| begin(c, m, id, p.as_ref(), &runner, now))
}

fn try_finish(
    db: &Db,
    id: i64,
    runner: &str,
    outcome: Outcome,
    status_: Option<i64>,
    now: i64,
) -> crate::Result<super::ExecRow> {
    let runner = runner.to_owned();
    db.write_txn("t", move |c, m| {
        finish(
            c,
            m,
            id,
            &Finished {
                runner: &runner,
                outcome,
                exit_status: status_,
                tail: b"ok\n",
                total_bytes: 3,
                log_path: Some("/logs/exec-1.log"),
            },
            now,
        )
    })
}

fn row(db: &Db, id: i64) -> super::ExecRow {
    db.read(move |c| get(c, id)).unwrap().unwrap()
}

fn messages(db: &Db, id: i64) -> Vec<(String, serde_json::Value)> {
    db.read(move |c| super::follow(c, id, 0, 256, T0))
        .unwrap()
        .into_iter()
        .map(|m| (m.kind, m.payload))
        .collect()
}

fn is_stale(e: &Error) -> bool {
    matches!(e, Error::Stale(_))
}

fn is_invalid(e: &Error) -> bool {
    matches!(e, Error::Types(jkb_types::Error::Validation(_)))
}

#[test]
fn a_request_is_pending_with_the_defaults_and_its_own_topic() {
    let db = Db::open_in_memory().unwrap();
    let r = req(
        &db,
        &who("s1"),
        &ask(&["./scripts/check.sh"], Some(pin(1))),
        T0,
    );
    assert!(!r.folded);
    assert_eq!(r.row.state, ExecState::Pending);
    assert_eq!(r.row.expires_at, T0 + EXPIRES_AFTER_MS);
    assert_eq!(r.row.ask.timeout_ms, Some(DEFAULT_TIMEOUT_MS));
    assert_eq!(r.row.ask.argv, vec!["./scripts/check.sh".to_owned()]);
    assert_eq!(r.row.wire_id(), format!("exec-{}", r.row.id));
    assert_eq!(parse_id(&r.row.wire_id()).unwrap(), r.row.id);
    assert!(
        messages(&db, r.row.id).is_empty(),
        "the topic exists, and holds nothing yet"
    );
}

#[test]
fn an_identical_pending_request_folds_and_nothing_else_does() {
    let db = Db::open_in_memory().unwrap();
    let a = ask(&["make", "test"], Some(pin(1)));
    let first = req(&db, &who("s1"), &a, T0);
    let again = req(&db, &who("s1"), &a, T0 + 1);
    assert!(again.folded, "the same session asked the same thing twice");
    assert_eq!(again.row.id, first.row.id);

    // Another session, another pin, another argv, another target, another directory: new rows.
    let distinct = [
        (who("s2"), a.clone()),
        (who("s1"), ask(&["make", "test"], Some(pin(2)))),
        (who("s1"), ask(&["make", "test"], None)),
        (who("s1"), ask(&["make", "check"], Some(pin(1)))),
        (
            who("s1"),
            Ask {
                target: ExecTarget::Container,
                ..a.clone()
            },
        ),
        (
            who("s1"),
            Ask {
                dir: "/w/other".to_owned(),
                ..a.clone()
            },
        ),
    ];
    let mut ids = vec![first.row.id];
    for (r, x) in &distinct {
        let got = req(&db, r, x, T0 + 2);
        assert!(!got.folded, "{x:?} from {} folded", r.session);
        assert!(!ids.contains(&got.row.id));
        ids.push(got.row.id);
    }
}

#[test]
fn a_request_no_longer_pending_absorbs_nothing() {
    let db = Db::open_in_memory().unwrap();
    let a = ask(&["make"], None);
    let first = req(&db, &who("s1"), &a, T0);
    db.write_txn("t", move |c, m| {
        decline(c, m, first.row.id, "ask for `make check`", T0 + 1)
    })
    .unwrap();
    let again = req(&db, &who("s1"), &a, T0 + 2);
    assert!(
        !again.folded,
        "a declined request is answered; asking again is a new request"
    );
    assert_ne!(again.row.id, first.row.id);
}

#[test]
fn an_expired_request_absorbs_nothing() {
    let db = Db::open_in_memory().unwrap();
    let a = ask(&["make"], None);
    let first = req(&db, &who("s1"), &a, T0);
    let later = req(&db, &who("s1"), &a, T0 + EXPIRES_AFTER_MS);
    assert!(!later.folded);
    assert_eq!(row(&db, first.row.id).state, ExecState::Expired);
}

#[test]
fn a_request_runs_at_most_once() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], Some(pin(1))), T0)
        .row
        .id;
    let Begun::Running(r) = try_begin(&db, id, Some(pin(1)), "app:1", T0 + 1).unwrap() else {
        panic!("not running")
    };
    assert_eq!(r.state, ExecState::Running);
    assert_eq!(r.runner.as_deref(), Some("app:1"));
    assert_eq!(r.started_at, Some(T0 + 1));
    for runner in ["app:1", "app:2"] {
        let e = try_begin(&db, id, Some(pin(1)), runner, T0 + 2).unwrap_err();
        assert!(is_stale(&e), "a second begin ran: {e}");
    }
    let kinds: Vec<_> = messages(&db, id)
        .into_iter()
        .map(|(k, p)| (k, p["state"].clone()))
        .collect();
    assert_eq!(
        kinds,
        vec![("exec.decision".to_owned(), serde_json::json!("running"))]
    );
}

#[test]
fn a_changed_pin_refuses_the_request_for_good() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], Some(pin(1))), T0)
        .row
        .id;
    let Begun::Refused(r) = try_begin(&db, id, Some(pin(2)), "app", T0 + 1).unwrap() else {
        panic!("a different tree ran")
    };
    assert_eq!(r.state, ExecState::Refused);
    assert_eq!(r.refuse_reason, Some(RefuseReason::TreeChanged));
    assert!(is_stale(
        &try_begin(&db, id, Some(pin(1)), "app", T0 + 2).unwrap_err()
    ));

    // A pin against no pin, either way round, is a change too.
    let none = req(&db, &who("s1"), &ask(&["ls"], None), T0).row.id;
    assert!(matches!(
        try_begin(&db, none, Some(pin(1)), "app", T0 + 1).unwrap(),
        Begun::Refused(_)
    ));
    let some = req(&db, &who("s1"), &ask(&["ls"], Some(pin(3))), T0).row.id;
    assert!(matches!(
        try_begin(&db, some, None, "app", T0 + 1).unwrap(),
        Begun::Refused(_)
    ));
    let msgs = messages(&db, id);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].1["refuse_reason"], "tree_changed");
}

#[test]
fn a_begin_after_the_expiry_expires_it() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    let got = try_begin(&db, id, None, "app", T0 + EXPIRES_AFTER_MS).unwrap();
    let Begun::Expired(r) = got else {
        panic!("an expired request ran: {got:?}")
    };
    assert_eq!(r.state, ExecState::Expired);
    assert!(r.note.unwrap().contains("two hours"));
    assert!(
        is_stale(&try_begin(&db, id, None, "app", T0 + EXPIRES_AFTER_MS + 1).unwrap_err()),
        "a later begin of an expired request is refused like any answered one"
    );
}

#[test]
fn a_read_after_the_expiry_sees_it_expired() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    let (r, _) = db
        .write_txn("t", move |c, m| status(c, m, id, T0 + EXPIRES_AFTER_MS - 1))
        .unwrap()
        .unwrap();
    assert_eq!(r.state, ExecState::Pending);
    let (rows, _) = db
        .write_txn("t", |c, m| {
            list(c, m, Filter::default(), None, T0 + EXPIRES_AFTER_MS)
        })
        .unwrap();
    assert_eq!(rows[0].state, ExecState::Expired);
}

#[test]
fn a_session_s_end_expires_its_pending_requests_only() {
    let db = Db::open_in_memory().unwrap();
    let pending = req(&db, &who("s1"), &ask(&["a"], None), T0).row.id;
    let running = req(&db, &who("s1"), &ask(&["b"], None), T0).row.id;
    let other = req(&db, &who("s2"), &ask(&["a"], None), T0).row.id;
    assert!(matches!(
        try_begin(&db, running, None, "app", T0 + 1).unwrap(),
        Begun::Running(_)
    ));
    db.write_txn("t", |c, m| {
        let p = Process {
            session: "s1",
            pid: "10",
            instance: "h",
        };
        claude_session::started(c, m, &p, "/w", "startup", T0)?;
        claude_session::ended(c, m, &p, "exit", T0 + 2)?;
        expire_if_ended(c, m, "s1", T0 + 2)
    })
    .unwrap();
    let r = row(&db, pending);
    assert_eq!(r.state, ExecState::Expired);
    assert!(r.note.unwrap().contains("session ended"));
    assert_eq!(
        row(&db, running).state,
        ExecState::Running,
        "an approved run runs on"
    );
    assert_eq!(
        row(&db, other).state,
        ExecState::Pending,
        "another session's request is not its"
    );
    assert_eq!(
        messages(&db, pending).last().unwrap().1["state"],
        "expired",
        "the waiting session is told"
    );
}

#[test]
fn a_session_still_held_by_another_process_keeps_its_requests() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["a"], None), T0).row.id;
    db.write_txn("t", |c, m| {
        let a = Process {
            session: "s1",
            pid: "10",
            instance: "h",
        };
        let b = Process {
            session: "s1",
            pid: "11",
            instance: "h",
        };
        claude_session::started(c, m, &a, "/w", "startup", T0)?;
        claude_session::started(c, m, &b, "/w", "startup", T0)?;
        claude_session::ended(c, m, &a, "exit", T0 + 1)?;
        expire_if_ended(c, m, "s1", T0 + 1)
    })
    .unwrap();
    assert_eq!(row(&db, id).state, ExecState::Pending);
    // …and once the last holder is proved gone, it expires.
    db.write_txn("t", |c, m| {
        let b = Process {
            session: "s1",
            pid: "11",
            instance: "h",
        };
        claude_session::gone(c, m, &b, T0 + 2)?;
        expire_if_ended(c, m, "s1", T0 + 2)
    })
    .unwrap();
    assert_eq!(row(&db, id).state, ExecState::Expired);
}

#[test]
fn expire_session_counts_what_it_expired() {
    let db = Db::open_in_memory().unwrap();
    req(&db, &who("s1"), &ask(&["a"], None), T0);
    req(&db, &who("s1"), &ask(&["b"], None), T0);
    let n = db
        .write_txn("t", |c, m| expire_session(c, m, "s1", T0 + 1))
        .unwrap();
    assert_eq!(n, 2);
    let n = db
        .write_txn("t", |c, m| expire_session(c, m, "s1", T0 + 2))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn finish_is_the_runner_s_and_idempotent_for_identical_values() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    assert!(
        is_stale(&try_finish(&db, id, "app", Outcome::Exited, Some(0), T0).unwrap_err()),
        "a pending request cannot finish"
    );
    try_begin(&db, id, None, "app", T0 + 1).unwrap();
    assert!(
        is_stale(&try_finish(&db, id, "other", Outcome::Exited, Some(0), T0 + 2).unwrap_err()),
        "another runner cannot finish it"
    );
    let done = try_finish(&db, id, "app", Outcome::Exited, Some(3), T0 + 2).unwrap();
    assert_eq!(done.state, ExecState::Finished);
    assert_eq!(done.exit_status, Some(3));
    let again = try_finish(&db, id, "app", Outcome::Exited, Some(3), T0 + 9).unwrap();
    assert_eq!(again, done, "a retried finish changes nothing");
    assert!(is_stale(
        &try_finish(&db, id, "app", Outcome::Exited, Some(0), T0 + 9).unwrap_err()
    ));
    assert!(is_stale(
        &try_finish(&db, id, "other", Outcome::Exited, Some(3), T0 + 9).unwrap_err()
    ));
    let out = db.read(move |c| output_of(c, id)).unwrap().unwrap();
    assert_eq!(out.tail, b"ok\n");
    assert_eq!(out.total_bytes, 3);
    assert!(!out.truncated);
    assert_eq!(out.log_path.as_deref(), Some("/logs/exec-1.log"));
    let ends: Vec<_> = messages(&db, id)
        .into_iter()
        .filter(|(k, _)| k == "exec.end")
        .collect();
    assert_eq!(ends.len(), 1, "the retry published nothing");
    assert_eq!(ends[0].1["exit_status"], 3);
}

#[test]
fn an_exit_needs_its_status() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    try_begin(&db, id, None, "app", T0 + 1).unwrap();
    for o in [Outcome::Exited, Outcome::Signalled] {
        assert!(is_invalid(
            &try_finish(&db, id, "app", o, None, T0 + 2).unwrap_err()
        ));
    }
    let r = try_finish(&db, id, "app", Outcome::TimedOut, None, T0 + 2).unwrap();
    assert_eq!(r.outcome, Some(Outcome::TimedOut));
}

#[test]
fn the_schema_ties_columns_to_the_state() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    try_begin(&db, id, None, "app", T0 + 1).unwrap();
    for sql in [
        // finished without an outcome
        "UPDATE exec_requests SET state = 'finished', ended_at = 1 WHERE id = ?1",
        // an exit without its status
        "UPDATE exec_requests SET state = 'finished', ended_at = 1, outcome = 'exited' WHERE id = ?1",
        // refused without a reason
        "UPDATE exec_requests SET state = 'refused' WHERE id = ?1",
        // a reason on a request that is not refused
        "UPDATE exec_requests SET refuse_reason = 'dir_gone' WHERE id = ?1",
        // half a pin
        "UPDATE exec_requests SET pin_head = 'ab' WHERE id = ?1",
        // an empty argv
        "UPDATE exec_requests SET argv = '[]' WHERE id = ?1",
        // a state nobody knows
        "UPDATE exec_requests SET state = 'approved' WHERE id = ?1",
        // running without its start
        "UPDATE exec_requests SET started_at = NULL WHERE id = ?1",
    ] {
        let e = db
            .write_txn("t", move |c, _| Ok(c.execute(sql, [id])?))
            .unwrap_err();
        assert!(e.to_string().contains("CHECK"), "{sql}: {e}");
    }
}

#[test]
fn decline_and_refuse_answer_only_a_pending_request() {
    let db = Db::open_in_memory().unwrap();
    let a = req(&db, &who("s1"), &ask(&["a"], None), T0).row.id;
    let r = db
        .write_txn("t", move |c, m| {
            decline(c, m, a, "ask for `make check` instead", T0 + 1)
        })
        .unwrap();
    assert_eq!(r.state, ExecState::Declined);
    assert_eq!(r.ended_at, Some(T0 + 1));
    let msgs = messages(&db, a);
    assert_eq!(msgs[0].1["note"], "ask for `make check` instead");
    for e in [
        db.write_txn("t", move |c, m| decline(c, m, a, "again", T0 + 2))
            .unwrap_err(),
        db.write_txn("t", move |c, m| {
            refuse(c, m, a, RefuseReason::DirGone, T0 + 2)
        })
        .unwrap_err(),
    ] {
        assert!(is_stale(&e), "{e}");
    }
    let b = req(&db, &who("s1"), &ask(&["b"], None), T0).row.id;
    let r = db
        .write_txn("t", move |c, m| {
            refuse(c, m, b, RefuseReason::AuditRefused, T0 + 1)
        })
        .unwrap();
    assert_eq!(r.refuse_reason, Some(RefuseReason::AuditRefused));
    let missing = db
        .write_txn("t", |c, m| decline(c, m, 999, "x", T0))
        .unwrap_err();
    assert!(
        matches!(missing, Error::Types(jkb_types::Error::NotFound(_))),
        "{missing}"
    );
}

#[test]
fn output_is_the_running_runner_s_and_numbered_from_zero() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    let out = |runner: &'static str, bytes: Vec<u8>| {
        db.write_txn("t", move |c, m| output(c, m, id, runner, &bytes, T0 + 5))
    };
    assert!(
        is_stale(&out("app", b"x".to_vec()).unwrap_err()),
        "not running yet"
    );
    try_begin(&db, id, None, "app", T0 + 1).unwrap();
    assert!(is_stale(&out("other", b"x".to_vec()).unwrap_err()));
    assert!(is_invalid(&out("app", Vec::new()).unwrap_err()));
    assert!(is_invalid(
        &out("app", vec![0; MAX_CHUNK_BYTES + 1]).unwrap_err()
    ));
    let (s0, n0) = out("app", b"hello ".to_vec()).unwrap();
    let (s1, n1) = out("app", vec![0xff; MAX_CHUNK_BYTES]).unwrap();
    assert!(s0.is_some() && s1.is_some(), "both were published");
    assert_eq!((n0, n1), (0, 1));
    let chunks: Vec<_> = messages(&db, id)
        .into_iter()
        .filter(|(k, _)| k == "exec.output")
        .collect();
    assert_eq!(chunks[0].1["b64"], "aGVsbG8g");
    assert_eq!(chunks[1].1["n"], 1);
}

#[test]
fn a_malformed_ask_is_refused() {
    let db = Db::open_in_memory().unwrap();
    let big = "x".repeat(MAX_ARGV_BYTES);
    let bad = [
        ask(&[], None),
        ask(&[""], None),
        ask(&["--help"], None),
        ask(&["ls", "a\0b"], None),
        ask(&["ls", &big], None),
        Ask {
            dir: "relative/dir".to_owned(),
            ..ask(&["ls"], None)
        },
        Ask {
            reason: "r".repeat(501),
            ..ask(&["ls"], None)
        },
        Ask {
            reason: "a\nb".to_owned(),
            ..ask(&["ls"], None)
        },
        Ask {
            timeout_ms: Some(MAX_TIMEOUT_MS + 1),
            ..ask(&["ls"], None)
        },
        Ask {
            timeout_ms: Some(0),
            ..ask(&["ls"], None)
        },
        Ask {
            pin: Some(TreePin {
                head: "HEAD".to_owned(),
                changes: "0".repeat(64),
            }),
            ..ask(&["ls"], None)
        },
    ];
    for a in &bad {
        let e = try_req(&db, &who("s1"), a, T0).unwrap_err();
        assert!(is_invalid(&e), "{a:?}: {e}");
    }
    assert!(is_invalid(
        &try_req(&db, &who(""), &ask(&["ls"], None), T0).unwrap_err()
    ));
    let ok = Ask {
        timeout_ms: Some(MAX_TIMEOUT_MS),
        ..ask(&["sh", "-c", "a | b"], None)
    };
    assert_eq!(
        req(&db, &who("s1"), &ok, T0).row.ask.timeout_ms,
        Some(MAX_TIMEOUT_MS)
    );
    for s in ["exec-0", "exec-", "exec-1x", "x", "-3", "exec-+3"] {
        assert!(parse_id(s).is_err(), "{s}");
    }
    assert_eq!(parse_id("7").unwrap(), 7);
}

/// A bare `jkb undo` reaches past run-request entries to the user's last real change — they are
/// written by transactions the user did not choose — while naming one explicitly refuses it by name.
#[test]
fn a_bare_undo_reaches_past_exec_entries_and_an_explicit_one_refuses_them() {
    use crate::item::{self, upsert, NewItem};
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        upsert(
            c,
            m,
            &NewItem {
                uid: "target".to_owned(),
                kind: "note".to_owned(),
                content: None,
                content_hash: None,
                mime: None,
            },
        )
    })
    .unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    db.write_txn("t", move |c, m| decline(c, m, id, "no", T0 + 1))
        .unwrap();
    let exec_txn: i64 = db
        .read(|c| {
            Ok(c.query_row(
                "SELECT MAX(txn_id) FROM changelog WHERE entity_type = 'exec_requests'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();

    db.write_txn("t", crate::undo::undo_last).unwrap();
    assert!(
        db.read(|c| item::id_for_uid(c, "target"))
            .unwrap()
            .is_none(),
        "the bare undo did not reach the user's last real change"
    );
    assert_eq!(
        row(&db, id).state,
        ExecState::Declined,
        "the request was not touched"
    );

    let e = db
        .write_txn("t", move |c, m| crate::undo::undo(c, m, exec_txn))
        .unwrap_err();
    assert!(e.to_string().contains("exec_requests"), "{e}");
    assert_eq!(
        row(&db, id).state,
        ExecState::Declined,
        "nothing was reverted"
    );
}

#[test]
fn a_queue_client_cannot_create_or_send_to_a_request_s_topic() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    let t = topic(id);
    let forged = Draft {
        key: t.clone(),
        kind: "exec.end".to_owned(),
        payload: serde_json::json!({"state": "finished", "outcome": "exited", "exit_status": 0}),
        ttl_ms: None,
        producer: "forger".to_owned(),
    };
    let t2 = t.clone();
    let e = db
        .write_txn("t", move |c, m| mq::send(c, m, &t2, &forged, T0))
        .unwrap_err();
    assert!(matches!(e, Error::Queue(QueueError::Invalid { .. })), "{e}");
    let e = db
        .write_txn("t", |c, m| {
            mq::topic_create(c, m, "exec/999", &TopicSpec::default(), T0)
        })
        .unwrap_err();
    assert!(matches!(e, Error::Queue(QueueError::Invalid { .. })), "{e}");
    // Nor read it, subscribe to it, ack in it or list it: the topic is the requester's, read only
    // through `follow`, which `jkb-api` holds to the requester's session.
    let refused = |e: Error| assert!(matches!(e, Error::Queue(QueueError::Invalid { .. })), "{e}");
    let t3 = t.clone();
    refused(
        db.write_txn("t", move |c, m| {
            mq::group_create(c, m, &t3, "wait-1", mq::Start::FromStart, T0)
        })
        .unwrap_err(),
    );
    let t3 = t.clone();
    refused(
        db.write_txn("t", move |c, m| mq::group_delete(c, m, &t3, "g"))
            .unwrap_err(),
    );
    let t3 = t.clone();
    refused(
        db.write_txn("t", move |c, m| mq::poll(c, m, &t3, "g", 10, None, T0))
            .unwrap_err(),
    );
    let t3 = t.clone();
    refused(
        db.write_txn("t", move |c, m| mq::ack(c, m, &t3, "g", 1, T0))
            .unwrap_err(),
    );
    let t3 = t.clone();
    refused(db.read(move |c| mq::tail(c, &t3, 10, T0)).unwrap_err());
    let t3 = t.clone();
    refused(
        db.read(move |c| mq::poll_needed(c, &t3, "g", 10, None, T0))
            .unwrap_err(),
    );
    let t3 = t.clone();
    refused(db.read(move |c| mq::group_count(c, &t3)).unwrap_err());
    let listed = db.read(mq::inspect).unwrap();
    assert!(
        listed.iter().all(|r| !r.name.starts_with("exec/")),
        "inspect lists a request's topic"
    );
    // A name that merely contains the prefix is not reserved.
    db.write_txn("t", |c, m| {
        mq::topic_create(c, m, "x/exec/1", &TopicSpec::default(), T0)
    })
    .unwrap();
}

#[test]
fn every_word_round_trips() {
    for s in ExecState::ALL {
        assert_eq!(ExecState::parse(s.as_str()), Some(*s));
    }
    for s in ExecTarget::ALL {
        assert_eq!(ExecTarget::parse(s.as_str()), Some(*s));
    }
    for s in RefuseReason::ALL {
        assert_eq!(RefuseReason::parse(s.as_str()), Some(*s));
    }
    for s in Outcome::ALL {
        assert_eq!(Outcome::parse(s.as_str()), Some(*s));
    }
    assert!(ExecState::Expired.is_terminal() && !ExecState::Running.is_terminal());
}

/// One step of the at-most-once property: whoever acts, in whatever order, with whatever pin.
#[derive(Debug, Clone)]
enum Step {
    Begin { runner: u8, pin: Option<u8> },
    Decline,
    Refuse,
    Finish { runner: u8 },
    Advance(i64),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0u8..2, proptest::option::of(0u8..2))
            .prop_map(|(runner, pin)| Step::Begin { runner, pin }),
        Just(Step::Decline),
        Just(Step::Refuse),
        (0u8..2).prop_map(|runner| Step::Finish { runner }),
        (0i64..EXPIRES_AFTER_MS).prop_map(Step::Advance),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// **A request runs at most once.** However begins, declines, refusals, finishes and the clock
    /// interleave, at most one begin ever succeeds; a terminal state is never left; and `finished` is
    /// reached only through that one begin's runner.
    #[test]
    fn a_request_runs_at_most_once_whatever_the_interleaving(
        stored_pin in proptest::option::of(0u8..2),
        steps in proptest::collection::vec(step(), 1..12),
    ) {
        let db = Db::open_in_memory().unwrap();
        let id = req(&db, &who("s1"), &ask(&["make"], stored_pin.map(pin)), T0).row.id;
        let mut now = T0;
        let mut began: Option<String> = None;
        let mut terminal: Option<ExecState> = None;
        for s in steps {
            now += 1;
            match s {
                Step::Begin { runner, pin: p } => {
                    let name = format!("app:{runner}");
                    if let Ok(Begun::Running(_)) = try_begin(&db, id, p.map(pin), &name, now) {
                        prop_assert!(began.is_none(), "a second begin ran");
                        began = Some(name);
                    }
                }
                Step::Decline => {
                    let _ = db.write_txn("t", move |c, m| decline(c, m, id, "no", now));
                }
                Step::Refuse => {
                    let _ = db.write_txn("t", move |c, m| refuse(c, m, id, RefuseReason::DirGone, now));
                }
                Step::Finish { runner } => {
                    let name = format!("app:{runner}");
                    if try_finish(&db, id, &name, Outcome::Exited, Some(0), now).is_ok() {
                        prop_assert_eq!(Some(name), began.clone(), "finished by a runner that never began it");
                    }
                }
                Step::Advance(ms) => now += ms,
            }
            let state = row(&db, id).state;
            if let Some(t) = terminal {
                prop_assert_eq!(state, t, "left a terminal state");
            }
            if state.is_terminal() {
                terminal = Some(state);
            }
            if state == ExecState::Running || state == ExecState::Finished {
                prop_assert!(began.is_some());
            }
        }
    }

    /// **The fold.** Whatever sessions ask for whatever, with answers in between, there is never more
    /// than one pending request per (session, argv, pin), and a request folds exactly when such a
    /// pending request already existed — into that one.
    #[test]
    fn identical_pending_requests_fold_into_one(
        ops in proptest::collection::vec((0u8..2, 0u8..2, proptest::option::of(0u8..2), proptest::bool::weighted(0.2)), 1..20),
    ) {
        let db = Db::open_in_memory().unwrap();
        let mut pending: std::collections::HashMap<(u8, u8, Option<u8>), i64> = std::collections::HashMap::new();
        let mut now = T0;
        for (s, a, p, answer) in ops {
            now += 1;
            let key = (s, a, p);
            let argv = format!("prog{a}");
            let got = req(&db, &who(&format!("s{s}")), &ask(&[&argv], p.map(pin)), now);
            if let Some(id) = pending.get(&key) {
                prop_assert!(got.folded);
                prop_assert_eq!(got.row.id, *id);
            } else {
                prop_assert!(!got.folded);
                pending.insert(key, got.row.id);
            }
            if answer {
                let id = got.row.id;
                db.write_txn("t", move |c, m| decline(c, m, id, "no", now)).unwrap();
                pending.remove(&key);
            }
        }
        let (rows, _) = db.write_txn("t", move |c, m| list(c, m, Filter { state: Some(ExecState::Pending), session: None }, None, now)).unwrap();
        prop_assert_eq!(rows.len(), pending.len());
    }
}

#[test]
fn a_run_whose_runner_went_quiet_ends_past_its_timeout_and_grace() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    try_begin(&db, id, None, "app", T0).unwrap();
    let due = T0 + DEFAULT_TIMEOUT_MS + super::RUN_GRACE_MS;
    let (r, _) = db
        .write_txn("t", move |c, m| status(c, m, id, due - 1))
        .unwrap()
        .unwrap();
    assert_eq!(
        r.state,
        ExecState::Running,
        "still within its timeout and grace"
    );
    let (r, _) = db
        .write_txn("t", move |c, m| status(c, m, id, due))
        .unwrap()
        .unwrap();
    assert_eq!(r.state, ExecState::Finished);
    assert_eq!(r.outcome, Some(Outcome::Interrupted));
    assert!(r.note.unwrap().contains("never reported an end"));
    let end = messages(&db, id)
        .into_iter()
        .find(|(k, _)| k == "exec.end")
        .unwrap();
    assert_eq!(end.1["outcome"], "interrupted");
    // Another runner cannot finish it.
    assert!(is_stale(
        &try_finish(&db, id, "other", Outcome::Exited, Some(0), due + 1).unwrap_err()
    ));
}

/// `settle` judges by wall-clock age alone, so its `interrupted` is provisional: a runner that was
/// only paused (a host asleep) reports late, and its own output and ending are what is recorded.
#[test]
fn a_late_runner_s_own_report_replaces_a_settled_interrupt() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    try_begin(&db, id, None, "app", T0).unwrap();
    let late = T0 + DEFAULT_TIMEOUT_MS + super::RUN_GRACE_MS + 60_000;
    // The runner wakes and reports: its output is still accepted…
    let (seq, n) = db
        .write_txn("t", move |c, m| {
            output(c, m, id, "app", b"still here\n", late)
        })
        .unwrap();
    assert!(seq.is_some());
    assert_eq!(n, 0);
    assert_eq!(
        row(&db, id).outcome,
        Some(Outcome::Interrupted),
        "settle ended it first"
    );
    // …another runner's is not…
    assert!(is_stale(
        &db.write_txn("t", move |c, m| output(c, m, id, "other", b"x", late))
            .unwrap_err()
    ));
    // …and its own ending replaces the assumed one.
    let done = try_finish(&db, id, "app", Outcome::Exited, Some(3), late + 1).unwrap();
    assert_eq!(
        (done.outcome, done.exit_status),
        (Some(Outcome::Exited), Some(3))
    );
    assert_eq!(done.note, None);
    let ends: Vec<_> = messages(&db, id)
        .into_iter()
        .filter(|(k, _)| k == "exec.end")
        .map(|(_, p)| p["outcome"].clone())
        .collect();
    assert_eq!(ends, vec!["interrupted", "exited"]);
    // Once it reported, the ending is its own and is not replaced again.
    assert!(is_stale(
        &try_finish(&db, id, "app", Outcome::Exited, Some(0), late + 2).unwrap_err()
    ));
    assert_eq!(
        try_finish(&db, id, "app", Outcome::Exited, Some(3), late + 3).unwrap(),
        done,
        "an identical retry is still idempotent"
    );
}

#[test]
fn a_re_ask_with_another_timeout_or_side_does_not_fold() {
    let db = Db::open_in_memory().unwrap();
    let a = ask(&["cargo", "test"], None);
    let first = req(&db, &who("s1"), &a, T0);
    let longer = Ask {
        timeout_ms: Some(MAX_TIMEOUT_MS),
        ..a.clone()
    };
    let got = req(&db, &who("s1"), &longer, T0 + 1);
    assert!(
        !got.folded,
        "a longer timeout folded into the shorter request"
    );
    assert_eq!(got.row.ask.timeout_ms, Some(MAX_TIMEOUT_MS));
    let side = Ask {
        dir_side: ExecTarget::Container,
        ..a.clone()
    };
    assert!(!req(&db, &who("s1"), &side, T0 + 1).folded);
    // The reason is not part of the fold.
    let reworded = Ask {
        reason: "in other words".to_owned(),
        ..a
    };
    let again = req(&db, &who("s1"), &reworded, T0 + 2);
    assert!(again.folded);
    assert_eq!(again.row.id, first.row.id);
}

#[test]
fn a_full_topic_drops_output_and_its_number_shows_the_gap() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    try_begin(&db, id, None, "app", T0).unwrap();
    let mut dropped = None;
    for i in 0..400 {
        let (seq, n) = db
            .write_txn("t", move |c, m| {
                output(c, m, id, "app", &vec![b'x'; MAX_CHUNK_BYTES], T0 + 1)
            })
            .unwrap();
        assert_eq!(n, i);
        if seq.is_none() {
            dropped = Some(n);
            break;
        }
    }
    let dropped = dropped.expect("an 8 MiB topic took 400 chunks of 47 KiB");
    // The run still finishes, and the end is recorded on the row.
    let done = try_finish(&db, id, "app", Outcome::Exited, Some(0), T0 + 2).unwrap();
    assert_eq!(done.state, ExecState::Finished);
    let out = db.read(move |c| output_of(c, id)).unwrap().unwrap();
    assert_eq!(out.chunks, dropped + 1, "the dropped chunk is counted");
}

#[test]
fn compaction_reaps_a_request_topic_s_expired_messages_with_no_group() {
    let db = Db::open_in_memory().unwrap();
    let id = req(&db, &who("s1"), &ask(&["make"], None), T0).row.id;
    db.write_txn("t", move |c, m| decline(c, m, id, "no", T0))
        .unwrap();
    assert_eq!(messages(&db, id).len(), 1);
    let report = db
        .write_txn("t", |c, m| {
            mq::compact(c, m, T0 + 2 * 24 * 60 * 60 * 1000, true)
        })
        .unwrap();
    assert!(report.messages_reaped >= 1);
    assert!(messages(&db, id).is_empty());
}
