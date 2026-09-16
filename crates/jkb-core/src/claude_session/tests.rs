//! The registry's rules: what a start, an end and a sweep verdict do, and above all that an end
//! about one process never ends the same id held by another.

use super::{
    ended, get, gone, list, started, Ended, SessionRow, Start, Started, GONE, LIST_CAP,
    PRUNE_AFTER_MS,
};
use crate::{Db, Error};

const T0: i64 = 1_800_000_000_000;

fn start<'a>(session: &'a str, pid: &'a str, instance: &'a str, source: &'a str) -> Start<'a> {
    Start {
        session,
        pid,
        instance,
        cwd: "/w/repo",
        source,
    }
}

fn try_start(db: &Db, s: &Start<'_>, now: i64) -> crate::Result<Started> {
    let (session, pid, instance, cwd, source) = (
        s.session.to_owned(),
        s.pid.to_owned(),
        s.instance.to_owned(),
        s.cwd.to_owned(),
        s.source.to_owned(),
    );
    db.write_txn("t", move |c, m| {
        started(
            c,
            m,
            &Start {
                session: &session,
                pid: &pid,
                instance: &instance,
                cwd: &cwd,
                source: &source,
            },
            now,
        )
    })
}

fn do_start(db: &Db, s: &Start<'_>, now: i64) -> Started {
    try_start(db, s, now).unwrap()
}

fn do_end(db: &Db, session: &str, pid: &str, instance: &str, reason: &str, now: i64) -> Ended {
    let (s, p, i, r) = (
        session.to_owned(),
        pid.to_owned(),
        instance.to_owned(),
        reason.to_owned(),
    );
    db.write_txn("t", move |c, m| ended(c, m, &s, &p, &i, &r, now))
        .unwrap()
}

fn do_gone(db: &Db, session: &str, pid: &str, instance: &str, now: i64) -> bool {
    let (s, p, i) = (session.to_owned(), pid.to_owned(), instance.to_owned());
    db.write_txn("t", move |c, m| gone(c, m, &s, &p, &i, now))
        .unwrap()
}

fn row(db: &Db, session: &str) -> Option<SessionRow> {
    let s = session.to_owned();
    db.read(move |c| get(c, &s)).unwrap()
}

fn live(db: &Db) -> Vec<String> {
    db.read(|c| list(c, false))
        .unwrap()
        .into_iter()
        .map(|r| r.session)
        .collect()
}

/// The measured lifecycle: start, end, and a `claude --resume` in a new process bringing the same id
/// back to live.
#[test]
fn a_session_starts_ends_and_is_revived_by_a_resume() {
    let db = Db::open_in_memory().unwrap();
    assert_eq!(row(&db, "s1"), None, "unknown before any event");
    assert_eq!(
        do_start(&db, &start("s1", "10", "h", "startup"), T0),
        Started::New
    );
    let r = row(&db, "s1").unwrap();
    assert!(!r.ended());
    assert_eq!(
        (r.pid.as_str(), r.started_at, r.start_source.as_deref()),
        ("10", Some(T0), Some("startup"))
    );

    assert_eq!(
        do_end(&db, "s1", "10", "h", "prompt_input_exit", T0 + 1),
        Ended::Recorded
    );
    let r = row(&db, "s1").unwrap();
    assert!(r.ended());
    assert_eq!(
        (r.ended_at, r.end_reason.as_deref()),
        (Some(T0 + 1), Some("prompt_input_exit"))
    );
    assert_eq!(
        do_end(&db, "s1", "10", "h", "other", T0 + 2),
        Ended::AlreadyEnded,
        "a second end keeps the first reason"
    );
    assert_eq!(
        row(&db, "s1").unwrap().end_reason.as_deref(),
        Some("prompt_input_exit")
    );

    assert_eq!(
        do_start(&db, &start("s1", "20", "h", "resume"), T0 + 3),
        Started::Revived
    );
    let r = row(&db, "s1").unwrap();
    assert_eq!(
        (
            r.pid.as_str(),
            r.ended_at,
            r.end_reason,
            r.start_source.as_deref()
        ),
        ("20", None, None, Some("resume"))
    );
    assert_eq!(
        do_start(&db, &start("s1", "30", "h", "compact"), T0 + 4),
        Started::Restarted
    );
}

/// **The race this module exists around.** `claude --resume` runs the id in a new process; the old
/// process's end, arriving after, must not end the session the new one holds — nor may a sweep's
/// verdict about the old process, or about the same pid in another instance.
#[test]
fn an_end_about_another_process_leaves_the_session_live() {
    let db = Db::open_in_memory().unwrap();
    do_start(&db, &start("s1", "10", "h#b1", "startup"), T0);
    do_start(&db, &start("s1", "20", "h#b1", "resume"), T0 + 1);

    assert_eq!(
        do_end(&db, "s1", "10", "h#b1", "other", T0 + 2),
        Ended::OtherProcess
    );
    assert!(!do_gone(&db, "s1", "10", "h#b1", T0 + 2), "the old pid");
    assert!(
        !do_gone(&db, "s1", "20", "h#b2", T0 + 2),
        "the same pid in another boot is another process"
    );
    assert!(
        !row(&db, "s1").unwrap().ended(),
        "the resumed session is still live"
    );

    assert!(do_gone(&db, "s1", "20", "h#b1", T0 + 3));
    let r = row(&db, "s1").unwrap();
    assert_eq!(r.end_reason.as_deref(), Some(GONE));
    assert!(
        !do_gone(&db, "s1", "20", "h#b1", T0 + 4),
        "a session already ended is not ended again"
    );
    assert_eq!(row(&db, "s1").unwrap().ended_at, Some(T0 + 3));
}

/// A verdict with no pid proves nothing, even against a row that recorded none: the hook had no
/// process it could trust, so nothing was probed.
#[test]
fn a_sweep_with_no_pid_proves_nothing() {
    let db = Db::open_in_memory().unwrap();
    do_start(&db, &start("s1", "", "h", "startup"), T0);
    assert!(!do_gone(&db, "s1", "", "h", T0 + 1));
    assert!(!row(&db, "s1").unwrap().ended());
    // The session's own end, from the same untrusted-pid process, still counts: it is not a probe.
    assert_eq!(do_end(&db, "s1", "", "h", "clear", T0 + 2), Ended::Recorded);
}

/// A session whose start was never seen (it began before the hook shipped) still has its end
/// recorded — that is evidence — and a sweep verdict about an unknown session changes nothing.
#[test]
fn an_end_with_no_start_is_still_evidence() {
    let db = Db::open_in_memory().unwrap();
    assert!(!do_gone(&db, "never", "10", "h", T0));
    assert_eq!(row(&db, "never"), None);
    assert_eq!(do_end(&db, "old", "10", "h", "other", T0), Ended::Recorded);
    let r = row(&db, "old").unwrap();
    assert_eq!((r.started_at, r.ended_at), (None, Some(T0)));
    assert!(live(&db).is_empty());
}

/// The sweep's listing is the live sessions, oldest start first; `all` adds the ended ones, most
/// recent first.
#[test]
fn the_listing_is_the_live_sessions_oldest_first() {
    let db = Db::open_in_memory().unwrap();
    do_start(&db, &start("b", "1", "h", "startup"), T0 + 2);
    do_start(&db, &start("a", "2", "h", "startup"), T0 + 1);
    do_start(&db, &start("c", "3", "h", "startup"), T0 + 3);
    do_end(&db, "c", "3", "h", "other", T0 + 4);
    assert_eq!(live(&db), ["a", "b"]);
    let all: Vec<String> = db
        .read(|c| list(c, true))
        .unwrap()
        .into_iter()
        .map(|r| r.session)
        .collect();
    assert_eq!(all, ["c", "b", "a"]);
}

/// The listing is bounded, so a registry nobody pruned cannot make the sweep's one read unbounded.
#[test]
fn the_listing_is_capped() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, _| {
        for i in 0..=LIST_CAP {
            c.execute(
                "INSERT INTO claude_sessions (session, pid, instance, cwd, started_at, start_source) \
                 VALUES (?1, '1', 'h', '', ?2, 'startup')",
                rusqlite::params![format!("s{i}"), T0],
            )?;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(live(&db).len(), LIST_CAP);
}

/// Rows idle past the prune age go when another session starts — live or ended, since neither can
/// be proved anything about any more — and the rest stay.
#[test]
fn a_start_prunes_rows_idle_past_the_prune_age() {
    let db = Db::open_in_memory().unwrap();
    do_start(&db, &start("old-live", "1", "gone-host", "startup"), T0);
    do_start(&db, &start("old-ended", "2", "h", "startup"), T0);
    do_end(&db, "old-ended", "2", "h", "other", T0 + 1);
    do_start(&db, &start("recent", "3", "h", "startup"), T0 + 2);
    do_end(&db, "recent", "3", "h", "other", T0 + PRUNE_AFTER_MS);

    let now = T0 + PRUNE_AFTER_MS + 2;
    do_start(&db, &start("new", "4", "h", "startup"), now);
    assert_eq!(row(&db, "old-live"), None);
    assert_eq!(row(&db, "old-ended"), None);
    assert!(row(&db, "recent").is_some(), "ended within the prune age");
    assert!(row(&db, "new").is_some());

    // The session starting is never pruned out from under its own start.
    do_start(&db, &start("idle", "5", "h", "startup"), T0);
    do_start(
        &db,
        &start("idle", "5", "h", "resume"),
        now + PRUNE_AFTER_MS,
    );
    assert_eq!(
        row(&db, "idle").unwrap().start_source.as_deref(),
        Some("resume")
    );
}

/// Every input is bounded and shaped before it is stored; a refusal writes nothing.
#[test]
fn malformed_input_is_refused() {
    let db = Db::open_in_memory().unwrap();
    let long_cwd = "x".repeat(4097);
    let long_instance = "h".repeat(151);
    for (s, why) in [
        (start("", "1", "h", "startup"), "empty id"),
        (start("a/b", "1", "h", "startup"), "unsanitized id"),
        (start("s", "-1", "h", "startup"), "not a pid"),
        (
            start("s", "123456789012345678901", "h", "startup"),
            "long pid",
        ),
        (start("s", "1", "h\n", "startup"), "control in instance"),
        (start("s", "1", &long_instance, "startup"), "long instance"),
        (start("s", "1", "h", ""), "empty source"),
        (start("s", "1", "h", "Startup"), "not snake case"),
        (start("s", "1", "h", &"x".repeat(33)), "long source"),
        (
            Start {
                cwd: &long_cwd,
                ..start("s", "1", "h", "startup")
            },
            "long cwd",
        ),
    ] {
        let err = try_start(&db, &s, T0).unwrap_err();
        assert!(
            matches!(err, Error::Types(jkb_types::Error::Validation(_))),
            "{why}: {err}"
        );
    }
    assert!(db.read(|c| list(c, true)).unwrap().is_empty());
    let err = db
        .write_txn("t", |c, m| ended(c, m, "s", "1", "h", "", T0))
        .unwrap_err();
    assert!(matches!(err, Error::Types(_)), "{err}");
    let err = db
        .write_txn("t", |c, m| gone(c, m, "s!", "1", "h", T0))
        .unwrap_err();
    assert!(matches!(err, Error::Types(_)), "{err}");
}
