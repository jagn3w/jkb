//! The registry's rules: what a start, a sighting, an end and a sweep verdict do, and above all that
//! one process's end or death never ends a session another process still holds.

use super::{
    ended, gone, holders, list, seen, started, state, Ended, HolderRow, Process, SessionState,
    GONE, LIST_CAP, MAX_CWD_BYTES, PRUNE_AFTER_MS, SEEN_REFRESH_MS, UNKNOWN,
};
use crate::mq::QueueError;
use crate::{Db, Error};

const T0: i64 = 1_800_000_000_000;

fn p<'a>(session: &'a str, pid: &'a str, instance: &'a str) -> Process<'a> {
    Process {
        session,
        pid,
        instance,
    }
}

/// Owned copies, for a closure the writer thread runs.
fn own(p: &Process<'_>) -> (String, String, String) {
    (
        p.session.to_owned(),
        p.pid.to_owned(),
        p.instance.to_owned(),
    )
}

fn try_start(
    db: &Db,
    proc_: &Process<'_>,
    dir: &str,
    source: &str,
    now: i64,
) -> crate::Result<SessionState> {
    let (s, i, n) = own(proc_);
    let (dir, source) = (dir.to_owned(), source.to_owned());
    db.write_txn("t", move |c, m| {
        started(c, m, &p(&s, &i, &n), &dir, &source, now)
    })
}

fn start(db: &Db, proc_: &Process<'_>, source: &str, now: i64) -> SessionState {
    try_start(db, proc_, "/w/repo", source, now).unwrap()
}

fn see(db: &Db, proc_: &Process<'_>, now: i64) {
    let (s, i, n) = own(proc_);
    db.write_txn("t", move |c, m| seen(c, m, &p(&s, &i, &n), "/w/seen", now))
        .unwrap();
}

fn end(db: &Db, proc_: &Process<'_>, reason: &str, now: i64) -> Ended {
    let (s, i, n) = own(proc_);
    let reason = reason.to_owned();
    db.write_txn("t", move |c, m| ended(c, m, &p(&s, &i, &n), &reason, now))
        .unwrap()
}

fn prove_gone(db: &Db, proc_: &Process<'_>, now: i64) -> bool {
    let (s, i, n) = own(proc_);
    db.write_txn("t", move |c, m| gone(c, m, &p(&s, &i, &n), now))
        .unwrap()
}

fn st(db: &Db, session: &str) -> SessionState {
    let s = session.to_owned();
    db.read(move |c| state(c, &s)).unwrap()
}

fn rows(db: &Db, session: &str) -> Vec<HolderRow> {
    let s = session.to_owned();
    db.read(move |c| holders(c, &s)).unwrap()
}

fn one(db: &Db, session: &str) -> HolderRow {
    let mut r = rows(db, session);
    assert_eq!(r.len(), 1, "{r:?}");
    r.remove(0)
}

fn live(db: &Db) -> Vec<(String, String)> {
    db.read(|c| list(c, false, None))
        .unwrap()
        .rows
        .into_iter()
        .map(|r| (r.session, r.pid))
        .collect()
}

/// The measured lifecycle for one process: start, end, a second end keeping the first reason, and a
/// `--resume` bringing the session back to live.
#[test]
fn a_session_starts_ends_and_is_revived_by_a_resume() {
    let db = Db::open_in_memory().unwrap();
    let a = p("s1", "10", "h");
    assert_eq!(st(&db, "s1"), SessionState::Unknown);
    assert_eq!(
        start(&db, &a, "startup", T0),
        SessionState::Unknown,
        "state before"
    );
    assert_eq!(st(&db, "s1"), SessionState::Live);
    let r = one(&db, "s1");
    assert_eq!(
        (
            r.started_at,
            r.start_source.as_deref(),
            r.seen_at,
            r.cwd.as_str()
        ),
        (Some(T0), Some("startup"), T0, "/w/repo")
    );

    assert_eq!(end(&db, &a, "prompt_input_exit", T0 + 1), Ended::Recorded);
    assert_eq!(st(&db, "s1"), SessionState::Ended);
    assert_eq!(
        end(&db, &a, "other", T0 + 2),
        Ended::AlreadyEnded,
        "a second end keeps the first reason"
    );
    let r = one(&db, "s1");
    assert_eq!(
        (r.ended_at, r.end_reason.as_deref()),
        (Some(T0 + 1), Some("prompt_input_exit"))
    );

    assert_eq!(start(&db, &a, "resume", T0 + 3), SessionState::Ended);
    let r = one(&db, "s1");
    assert_eq!(
        (r.ended_at, r.end_reason, r.start_source.as_deref()),
        (None, None, Some("resume"))
    );
    assert_eq!(
        start(&db, &a, "compact", T0 + 4),
        SessionState::Live,
        "a compaction finds it live"
    );
}

/// **The race the per-process rows exist for.** Two processes hold one id — `claude --resume` in a
/// second terminal — and the end or proven death of either leaves the session live while the other
/// runs. Only when both have ended is the session ended.
#[test]
fn one_process_ending_leaves_a_session_another_still_holds() {
    let db = Db::open_in_memory().unwrap();
    let first = p("s1", "10", "h#b1");
    let second = p("s1", "20", "h#b1");
    start(&db, &first, "startup", T0);
    assert_eq!(start(&db, &second, "resume", T0 + 1), SessionState::Live);

    assert_eq!(end(&db, &second, "other", T0 + 2), Ended::Recorded);
    assert_eq!(st(&db, "s1"), SessionState::Live, "the first still runs it");
    assert!(
        !prove_gone(&db, &p("s1", "10", "h#b2"), T0 + 2),
        "the same pid in another boot is another process"
    );
    assert_eq!(st(&db, "s1"), SessionState::Live);

    assert!(prove_gone(&db, &first, T0 + 3));
    assert_eq!(st(&db, "s1"), SessionState::Ended);
    let reasons: Vec<Option<String>> = rows(&db, "s1").into_iter().map(|r| r.end_reason).collect();
    assert_eq!(reasons, [Some(GONE.to_owned()), Some("other".to_owned())]);
    assert!(
        !prove_gone(&db, &first, T0 + 4),
        "a row already ended is not ended again"
    );
    assert_eq!(
        rows(&db, "s1")[0].ended_at,
        Some(T0 + 3),
        "and keeps when it ended"
    );
}

/// A verdict with no pid proves nothing: nothing was probed. And a pid-less end is never evidence that
/// the session ended: every process the hook could not name shares that row, so a second one — a
/// `--resume` of the same id — may still be running.
#[test]
fn a_pid_less_process_never_proves_a_session_ended() {
    let db = Db::open_in_memory().unwrap();
    let blind = p("s1", "", "h");
    start(&db, &blind, "startup", T0);
    assert!(!prove_gone(&db, &blind, T0 + 1));
    assert_eq!(st(&db, "s1"), SessionState::Live);
    assert_eq!(end(&db, &blind, "clear", T0 + 2), Ended::Recorded);
    assert_eq!(
        one(&db, "s1").end_reason.as_deref(),
        Some("clear"),
        "still recorded"
    );
    assert_eq!(st(&db, "s1"), SessionState::Unknown, "but not evidence");

    // Beside named processes too: they all ended, the pid-less row cannot say it has.
    start(&db, &p("s2", "10", "h"), "startup", T0);
    end(&db, &p("s2", "", "h"), "other", T0 + 1);
    end(&db, &p("s2", "10", "h"), "other", T0 + 2);
    assert_eq!(st(&db, "s2"), SessionState::Unknown);
}

/// A process whose start was never seen (it began before the hook shipped) still has its end recorded
/// — that is evidence — and a verdict about an unknown process creates nothing.
#[test]
fn an_end_with_no_start_is_still_evidence() {
    let db = Db::open_in_memory().unwrap();
    assert!(!prove_gone(&db, &p("never", "10", "h"), T0));
    assert_eq!(st(&db, "never"), SessionState::Unknown);
    assert_eq!(end(&db, &p("old", "10", "h"), "other", T0), Ended::Recorded);
    assert_eq!(st(&db, "old"), SessionState::Ended);
    let r = one(&db, "old");
    assert_eq!((r.started_at, r.seen_at, r.ended_at), (None, T0, Some(T0)));
    assert!(live(&db).is_empty());
}

/// **A lost start is repaired by the next event.** A process that is seen — any hook event — is live:
/// a session nobody saw start becomes known, and an ended row (its revival start was lost, or a sweep
/// wrongly proved it gone) comes back. A known live row is refreshed only once it is stale, so a tool
/// call changes nothing.
#[test]
fn any_event_from_a_process_makes_it_live() {
    let db = Db::open_in_memory().unwrap();
    let a = p("s1", "10", "h");
    see(&db, &a, T0);
    assert_eq!(st(&db, "s1"), SessionState::Live);
    let r = one(&db, "s1");
    assert_eq!(
        (r.started_at, r.start_source, r.seen_at, r.cwd.as_str()),
        (None, None, T0, "/w/seen")
    );

    end(&db, &a, "prompt_input_exit", T0 + 1);
    see(&db, &a, T0 + 2);
    assert_eq!(st(&db, "s1"), SessionState::Live, "revived");
    assert_eq!(one(&db, "s1").seen_at, T0 + 2);

    see(&db, &a, T0 + 2 + SEEN_REFRESH_MS - 1);
    assert_eq!(one(&db, "s1").seen_at, T0 + 2, "fresh enough: untouched");
    see(&db, &a, T0 + 3 + SEEN_REFRESH_MS);
    assert_eq!(
        one(&db, "s1").seen_at,
        T0 + 3 + SEEN_REFRESH_MS,
        "stale: refreshed"
    );

    start(&db, &a, "startup", T0 + 5 + SEEN_REFRESH_MS);
    see(&db, &a, T0 + 6 + 2 * SEEN_REFRESH_MS);
    let r = one(&db, "s1");
    assert_eq!(
        (r.start_source.as_deref(), r.cwd.as_str()),
        (Some("startup"), "/w/repo"),
        "a sighting keeps what the start recorded"
    );
}

/// The sweep's listing is the live rows, least recently seen first; `all` adds the ended ones, most
/// recent first.
#[test]
fn the_listing_is_the_live_rows_least_recently_seen_first() {
    let db = Db::open_in_memory().unwrap();
    start(&db, &p("b", "1", "h"), "startup", T0 + 2);
    start(&db, &p("a", "2", "h"), "startup", T0 + 1);
    start(&db, &p("c", "3", "h"), "startup", T0 + 3);
    end(&db, &p("c", "3", "h"), "other", T0 + 4);
    assert_eq!(
        live(&db),
        [
            ("a".to_owned(), "2".to_owned()),
            ("b".to_owned(), "1".to_owned())
        ]
    );
    let all: Vec<String> = db
        .read(|c| list(c, true, None))
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r.session)
        .collect();
    assert_eq!(all, ["c", "b", "a"]);
}

/// The listing is paged, so a registry nobody pruned cannot make the sweep's one read unbounded — and
/// a page that was cut says where to continue, in both orders, so no row is skipped or repeated.
#[test]
fn the_listing_is_paged() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, _| {
        for i in 0..=LIST_CAP {
            c.execute(
                "INSERT INTO claude_sessions (session, pid, instance, cwd, seen_at) \
                 VALUES (?1, '1', 'h', '', ?2)",
                rusqlite::params![format!("s{i:04}"), T0 + i64::try_from(i % 7).unwrap()],
            )?;
        }
        Ok(())
    })
    .unwrap();
    for all in [false, true] {
        let first = db.read(move |c| list(c, all, None)).unwrap();
        assert_eq!(first.rows.len(), LIST_CAP, "all={all}");
        let next = first.next.clone().expect("a cut page says where to go on");
        let second = db.read(move |c| list(c, all, Some(&next))).unwrap();
        assert_eq!((second.rows.len(), second.next), (1, None), "all={all}");
        let mut seen: Vec<String> = first
            .rows
            .iter()
            .chain(&second.rows)
            .map(|r| r.session.clone())
            .collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), LIST_CAP + 1, "every row once, all={all}");
    }
    // Rows alike in `seen_at` and session, told apart only by pid and instance, across the cut: the
    // cursor must carry all four.
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, _| {
        for i in 0..=LIST_CAP {
            c.execute(
                "INSERT INTO claude_sessions (session, pid, instance, cwd, seen_at) \
                 VALUES ('s', ?1, ?2, '', ?3)",
                rusqlite::params![(i / 2).to_string(), ["a", "b"][i % 2], T0],
            )?;
        }
        Ok(())
    })
    .unwrap();
    for all in [false, true] {
        let first = db.read(move |c| list(c, all, None)).unwrap();
        let next = first.next.clone().expect("cut");
        let second = db.read(move |c| list(c, all, Some(&next))).unwrap();
        let mut keys: Vec<(String, String)> = first
            .rows
            .iter()
            .chain(&second.rows)
            .map(|r| (r.pid.clone(), r.instance.clone()))
            .collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), LIST_CAP + 1, "every row once, all={all}");
    }
    let whole = db.read(|c| list(c, false, None)).unwrap();
    assert!(whole.next.is_some());
    db.write_txn("t", |c, _| {
        c.execute(
            "DELETE FROM claude_sessions WHERE pid = '0' AND instance = 'a'",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        db.read(|c| list(c, false, None)).unwrap().next,
        None,
        "an uncut page has no cursor"
    );
}

/// **A start never ends a session it is not about**, even one the same process held: a process may
/// host several sessions (the Agent SDK), and ending one of them would record a running session as
/// ended. The cost, chosen deliberately: after `/clear` with its `SessionEnd` lost, the old session
/// stays live until its process is proved gone.
#[test]
fn a_start_never_ends_another_session_of_the_same_process() {
    let db = Db::open_in_memory().unwrap();
    start(&db, &p("a", "10", "h"), "startup", T0);
    start(&db, &p("b", "10", "h"), "clear", T0 + 1);
    assert_eq!(st(&db, "a"), SessionState::Live);
    assert_eq!(st(&db, "b"), SessionState::Live);
    assert!(prove_gone(&db, &p("a", "10", "h"), T0 + 2));
    assert!(prove_gone(&db, &p("b", "10", "h"), T0 + 2));
    assert_eq!(
        st(&db, "a"),
        SessionState::Ended,
        "once the process is gone, both end"
    );
}

/// Sessions none of whose rows was seen within the prune age go when another session starts — live,
/// ended, or known only by their end — and the rest stay. **Whole sessions only**: a session with one
/// stale live row and one recent ended row keeps both, since deleting the live one alone would turn it
/// into an ended session. The starting session is never pruned out from under its own start.
#[test]
fn a_start_prunes_whole_sessions_not_seen_within_the_prune_age() {
    let db = Db::open_in_memory().unwrap();
    let now = T0 + PRUNE_AFTER_MS;
    start(&db, &p("old-live", "1", "gone-host"), "startup", T0 - 1);
    start(&db, &p("old-ended", "2", "h"), "startup", T0 - 5);
    end(&db, &p("old-ended", "2", "h"), "other", T0 - 1);
    end(&db, &p("old-end-only", "3", "h"), "other", T0 - 1);
    start(&db, &p("recent", "4", "h"), "startup", T0 + 1);
    start(&db, &p("mixed", "7", "idle-host"), "startup", T0 - 1);
    end(&db, &p("mixed", "8", "h"), "other", T0 + 1);
    start(&db, &p("idle", "5", "h"), "startup", T0 - 10);

    assert_eq!(
        start(&db, &p("idle", "6", "h"), "resume", now),
        SessionState::Live,
        "the session starting keeps its idle row"
    );
    for gone_ in ["old-live", "old-ended", "old-end-only"] {
        assert_eq!(st(&db, gone_), SessionState::Unknown, "{gone_} was pruned");
    }
    assert_eq!(st(&db, "recent"), SessionState::Live, "seen within the age");
    assert_eq!(
        st(&db, "mixed"),
        SessionState::Live,
        "a stale live row is kept beside a recent ended one"
    );
    assert_eq!(rows(&db, "mixed").len(), 2);
    assert_eq!(rows(&db, "idle").len(), 2);
}

/// Identity is refused when malformed, with the notification ops' own rule; what is only shown is
/// normalised instead, because refusing a start is what leaves a running session recorded as ended.
#[test]
fn identity_is_refused_and_the_rest_normalised() {
    let db = Db::open_in_memory().unwrap();
    let long_instance = "h".repeat(151);
    for (bad, why) in [
        (p("", "1", "h"), "empty id"),
        (p("a/b", "1", "h"), "unsanitized id"),
        (p("s", "-1", "h"), "not a pid"),
        (p("s", "123456789012345678901", "h"), "long pid"),
        (p("s", "1", "h\n"), "control in instance"),
        (p("s", "1", &long_instance), "long instance"),
        (
            p("s", "1", ""),
            "a pid with no instance to mean something in",
        ),
    ] {
        let err = try_start(&db, &bad, "/w", "startup", T0).unwrap_err();
        assert!(
            matches!(err, Error::Queue(QueueError::Invalid { .. })),
            "{why}: {err}"
        );
        let (s, i, n) = own(&bad);
        let err = db
            .write_txn("t", move |c, m| ended(c, m, &p(&s, &i, &n), "other", T0))
            .unwrap_err();
        assert!(matches!(err, Error::Queue(_)), "{why}: {err}");
    }
    assert!(db.read(|c| list(c, true, None)).unwrap().rows.is_empty());
    try_start(&db, &p("s", "", ""), "/w", "startup", T0).expect("no pid needs no instance");
    db.write_txn("t", |c, _| {
        c.execute("DELETE FROM claude_sessions", [])?;
        Ok(())
    })
    .unwrap();

    let wide = "é".repeat(MAX_CWD_BYTES); // two bytes each
    for (source, recorded) in [
        ("", UNKNOWN),
        ("Startup", UNKNOWN),
        ("resume-fork", UNKNOWN),
        (&*"x".repeat(33), UNKNOWN),
        ("startup_2", "startup_2"),
    ] {
        try_start(&db, &p("s", "1", "h"), &wide, source, T0).unwrap();
        let r = one(&db, "s");
        assert_eq!(r.start_source.as_deref(), Some(recorded), "{source:?}");
        assert_eq!(r.cwd.len(), MAX_CWD_BYTES, "cut at a character boundary");
        assert!(r.cwd.chars().all(|c| c == 'é'));
    }
    try_start(
        &db,
        &p("s", "1", "h"),
        "/w/a\nfake  ended (gone)\u{1b}[2J",
        "startup",
        T0,
    )
    .unwrap();
    assert_eq!(
        one(&db, "s").cwd,
        "/w/a?fake  ended (gone)?[2J",
        "control characters replaced"
    );
    end(&db, &p("s", "1", "h"), "Other!", T0 + 1);
    assert_eq!(one(&db, "s").end_reason.as_deref(), Some(UNKNOWN));
}
