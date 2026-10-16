use rusqlite::{params, Connection};

use super::{
    page, plan_points, session, sessions, task, Event, Source, PAGE_CAP, STALLED_AFTER_MS,
};
use crate::task::{create, NewTask};
use crate::Db;
use jkb_types::ItemId;

/// A fixed epoch for the fixtures (Unix ms), so every time below is `T0 + minutes`.
const T0: i64 = 1_800_000_000_000;
const MIN: i64 = 60 * 1000;

const S1: &str = "11111111-aaaa-4bbb-8ccc-000000000001";
const S2: &str = "22222222-aaaa-4bbb-8ccc-000000000002";

/// The ISO-8601 spelling jkb writes, for a Unix-ms time — computed by `SQLite`, as jkb writes it.
const ISO: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', ?1 / 1000.0, 'unixepoch')";

fn db() -> Db {
    Db::open_in_memory().unwrap()
}

fn new_task(db: &Db, uid: &str) -> ItemId {
    let uid = uid.to_owned();
    db.write_txn("test", move |c, m| {
        create(c, m, &NewTask::new(uid, "a task"))
    })
    .unwrap()
}

/// Run raw fixture SQL in a write transaction.
fn with(db: &Db, f: impl FnOnce(&Connection) + Send + 'static) {
    db.write_txn("test", move |c, _| {
        f(c);
        Ok(())
    })
    .unwrap();
}

fn claim_as(c: &Connection, id: ItemId, owner: &str, at: i64) {
    c.execute(
        &format!("UPDATE items SET claimant_id = ?2, claimed_at = {ISO} WHERE id = ?3"),
        params![at, owner, id.get()],
    )
    .unwrap();
}

fn status_move(
    c: &Connection,
    id: ItemId,
    at: i64,
    from: Option<&str>,
    to: &str,
    agent: Option<&str>,
) {
    c.execute(
        &format!(
            "INSERT INTO task_transitions (txn_id, item_id, at, event, from_status, to_status, agent_id)
             VALUES ('t', ?2, {ISO}, 'move', ?3, ?4, ?5)"
        ),
        params![at, id.get(), from, to, agent],
    )
    .unwrap();
}

fn phase_move(c: &Connection, id: ItemId, at: i64, from: Option<&str>, to: &str, actor: &str) {
    c.execute(
        &format!(
            "INSERT INTO workflow_transitions (txn_id, item_id, at, event, from_phase, to_phase, role, actor)
             VALUES ('t', ?2, {ISO}, 'fire', ?3, ?4, 'implementer', ?5)"
        ),
        params![at, id.get(), from, to, actor],
    )
    .unwrap();
}

fn bind(c: &Connection, session: &str, agent: &str, id: ItemId) {
    c.execute(
        "INSERT INTO agent_bindings (session, agent_id, item_id, bound_at) VALUES (?1, ?2, ?3, 'x')",
        params![session, agent, id.get()],
    )
    .unwrap();
}

fn registry(c: &Connection, session: &str, started: i64, ended: Option<i64>) {
    c.execute(
        "INSERT INTO claude_sessions (session, pid, instance, cwd, started_at, start_source, seen_at,
                                      ended_at, end_reason)
         VALUES (?1, '7', 'h', '/w', ?2, 'startup', ?2, ?3, CASE WHEN ?3 IS NULL THEN NULL ELSE 'other' END)",
        params![session, started, ended],
    )
    .unwrap();
}

fn round(c: &Connection, id: ItemId, ns: &str, actor: &str, at: i64, must_fix: usize) {
    c.execute(
        &format!("INSERT INTO review_rounds (ns, actor, recorded_at) VALUES (?2, ?3, {ISO})"),
        params![at, ns, actor],
    )
    .unwrap();
    let round: i64 = c.last_insert_rowid();
    for n in 0..must_fix {
        c.execute(
            "INSERT INTO review_round_findings (round_id, item_id, must_fix) VALUES (?1, ?2, 1)",
            params![round, 10_000 + i64::try_from(n).unwrap()],
        )
        .unwrap();
    }
    c.execute(
        &format!(
            "INSERT INTO reviews (item_id, kind, ns, sha, actor, at) VALUES (?2, 'recorded', ?3, 'abc', ?4, {ISO})"
        ),
        params![at, id.get(), ns, actor],
    )
    .unwrap();
}

#[test]
fn a_sessions_claim_and_its_bound_subagents_workflow_events_attribute_to_it() {
    let db = db();
    let a = new_task(&db, "task:a");
    let b = new_task(&db, "task:b");
    let c_ = new_task(&db, "task:c");
    with(&db, move |c| {
        claim_as(c, a, &format!("session:12@{S1}:/tmp/wt-a"), T0);
        status_move(
            c,
            a,
            T0,
            Some("open"),
            "in_progress",
            Some(&format!("session:12@{S1}:/tmp/wt-a")),
        );
        bind(c, S1, "agent-x", b);
        phase_move(
            c,
            b,
            T0 + MIN,
            Some("implement"),
            "review",
            "swarm-implementer@agent-x",
        );
        phase_move(
            c,
            a,
            T0 + 2 * MIN,
            Some("design"),
            "implement",
            &format!("session:{S1}"),
        );
        // Not S1's: an unbound subagent, another session's worktree claim.
        phase_move(
            c,
            c_,
            T0 + 3 * MIN,
            Some("implement"),
            "review",
            "swarm-implementer@agent-y",
        );
        status_move(
            c,
            c_,
            T0 + 4 * MIN,
            Some("open"),
            "in_progress",
            Some(&format!("session:3@{S2}:/w")),
        );
        registry(c, S1, T0 - MIN, None);
    });
    let t = db.read(|c| session(c, S1, None, T0 + 5 * MIN)).unwrap();
    let seen: Vec<(Source, Option<&str>)> = t
        .events
        .iter()
        .map(|e| (e.source, e.task.as_deref()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (Source::Session, None),
            (Source::Claim, Some("task:a")),
            (Source::Status, Some("task:a")),
            (Source::Workflow, Some("task:b")),
            (Source::Workflow, Some("task:a")),
        ]
    );
    let lanes: Vec<(&str, bool, Vec<String>)> = t
        .tasks
        .iter()
        .map(|l| (l.uid.as_str(), l.holds_claim, l.bound_agents.clone()))
        .collect();
    assert_eq!(
        lanes,
        vec![
            ("task:a", true, vec![]),
            ("task:b", false, vec!["agent-x".to_owned()]),
        ]
    );
    assert_eq!(t.summary.claims, 1);
    assert_eq!(t.summary.state, "live");
    assert_eq!(t.summary.last_event_ms, Some(T0 + 2 * MIN));
    assert!(!t.summary.stalled);

    // The other session sees only its own row.
    let other = db.read(|c| session(c, S2, None, T0 + 5 * MIN)).unwrap();
    assert_eq!(other.events.len(), 1);
    assert_eq!(other.events[0].task.as_deref(), Some("task:c"));
    assert_eq!(other.summary.state, "unknown");
}

#[test]
fn stalled_is_a_claim_held_quiet_past_thirty_minutes_by_a_session_that_has_not_ended() {
    let db = db();
    let a = new_task(&db, "task:a");
    with(&db, move |c| {
        registry(c, S1, T0 - MIN, None);
        claim_as(c, a, &format!("session:12@{S1}:/tmp/wt"), T0);
    });
    let at = |now: i64| {
        let o = db.read(move |c| sessions(c, now)).unwrap();
        o.sessions.into_iter().find(|s| s.session == S1).unwrap()
    };
    let calm = at(T0 + 29 * MIN);
    assert!(!calm.stalled);
    assert_eq!(calm.stalled_for_ms, None);
    let quiet = at(T0 + 31 * MIN);
    assert!(quiet.stalled);
    assert_eq!(quiet.stalled_for_ms, Some(31 * MIN));
    const { assert!(31 * MIN > STALLED_AFTER_MS && 29 * MIN < STALLED_AFTER_MS) };

    // An event resets the clock.
    with(&db, move |c| {
        phase_move(
            c,
            a,
            T0 + 20 * MIN,
            Some("design"),
            "implement",
            &format!("session:{S1}"),
        );
    });
    assert!(!at(T0 + 31 * MIN).stalled);

    // An ended session holding the same claim is not stalled: nothing is still at work.
    with(&db, |c| {
        c.execute(
            "UPDATE claude_sessions SET ended_at = ?1, end_reason = 'other'",
            params![T0 + 40 * MIN],
        )
        .unwrap();
    });
    let ended = at(T0 + 120 * MIN);
    assert_eq!(ended.state, "ended");
    assert!(!ended.stalled);
    assert_eq!(ended.ended_at_ms, Some(T0 + 40 * MIN));
}

#[test]
fn a_session_with_no_claim_is_never_stalled() {
    let db = db();
    with(&db, |c| registry(c, S1, T0, None));
    let o = db.read(|c| sessions(c, T0 + 600 * MIN)).unwrap();
    let s = o.sessions.iter().find(|s| s.session == S1).unwrap();
    assert_eq!(s.claims, 0);
    assert!(!s.stalled);
}

#[test]
fn a_process_claim_is_listed_unattributed_never_dropped() {
    let db = db();
    let a = new_task(&db, "task:a");
    let b = new_task(&db, "task:b");
    with(&db, move |c| {
        claim_as(c, a, "build-host:4242", T0);
        // An agent id no session bound is unattributed too.
        claim_as(c, b, "agent:nobody", T0);
    });
    let o = db.read(|c| sessions(c, T0 + MIN)).unwrap();
    assert!(o.sessions.is_empty(), "{:?}", o.sessions);
    let owners: Vec<(&str, &str)> = o
        .unattributed
        .iter()
        .map(|h| (h.uid.as_str(), h.owner.as_str()))
        .collect();
    assert_eq!(
        owners,
        vec![("task:a", "build-host:4242"), ("task:b", "agent:nobody")]
    );
    assert_eq!(o.unattributed[0].claimed_at_ms, Some(T0));
    assert_eq!(o.unattributed[0].title, "a task");
}

#[test]
fn a_bound_agent_claim_attributes_to_its_session() {
    let db = db();
    let a = new_task(&db, "task:a");
    with(&db, move |c| {
        bind(c, S1, "agent-x", a);
        claim_as(c, a, "agent:agent-x", T0);
    });
    let o = db.read(|c| sessions(c, T0 + MIN)).unwrap();
    assert!(o.unattributed.is_empty());
    assert_eq!(o.sessions.len(), 1);
    assert_eq!(o.sessions[0].session, S1);
    assert_eq!(o.sessions[0].claims, 1);
    // No registry row: unknown, and not ended, so it can stall.
    assert_eq!(o.sessions[0].state, "unknown");
}

#[test]
fn a_tasks_four_sources_merge_in_one_stable_order() {
    let db = db();
    let a = new_task(&db, "task:a");
    let owner = format!("session:12@{S1}:/tmp/wt");
    let held = owner.clone();
    with(&db, move |c| {
        // Every source at the same millisecond, written in an order unlike the merged one.
        round(c, a, "reviews/r1", "reviewer@r", T0, 2);
        phase_move(c, a, T0, Some("implement"), "review", "x@y");
        phase_move(c, a, T0, Some("review"), "implement", "x@y");
        status_move(c, a, T0, Some("open"), "in_progress", None);
        claim_as(c, a, &held, T0);
        status_move(c, a, T0 - MIN, None, "open", None);
    });
    let read = || db.read(|c| task(c, "task:a", None)).unwrap();
    let t = read();
    let order: Vec<(Source, Option<String>, Option<String>)> = t
        .events
        .iter()
        .map(|e| (e.source, e.from.clone(), e.to.clone()))
        .collect();
    let s = |v: &str| Some(v.to_owned());
    assert_eq!(
        order,
        vec![
            (Source::Status, None, s("open")),
            (Source::Claim, None, None),
            (Source::Status, s("open"), s("in_progress")),
            (Source::Workflow, s("implement"), s("review")),
            (Source::Workflow, s("review"), s("implement")),
            (Source::Review, None, None),
        ]
    );
    assert_eq!(
        t.events[5].detail.as_deref(),
        Some("reviews/r1: 2 must-fix")
    );
    assert_eq!(t.events[5].actor.as_deref(), Some("reviewer@r"));
    assert_eq!(
        t.claim.as_ref().map(|c| c.owner.as_str()),
        Some(owner.as_str())
    );
    assert_eq!(t.landing.state, "none");
    assert_eq!(read(), t, "the same history reads the same way every time");
}

#[test]
fn visited_counts_only_moves_through_the_real_verbs_and_a_reentered_phase_each_time() {
    use crate::workflow::store::{fire, Actor};
    use crate::workflow::{Phase, WorkflowEvent};
    use jkb_types::TaskStatus;
    let db = db();
    let a = new_task(&db, "task:a");
    db.write_txn("test", move |c, m| {
        let op = Actor::operator();
        // The first fire pins the strategy (a `pin_strategy` row, design -> design) before it moves.
        for (event, reason, stated) in [
            (WorkflowEvent::SubmitDesign, None, None),
            (WorkflowEvent::ApproveDesign, None, None),
            (WorkflowEvent::SubmitWork, None, None),
            (
                WorkflowEvent::Override,
                Some("again"),
                Some(Phase::Implement),
            ),
            (WorkflowEvent::SubmitWork, None, None),
        ] {
            fire(c, m, a, event, &op, reason, stated)?;
        }
        crate::task::set_status(c, m, a, TaskStatus::InProgress)?;
        // A note (`jkb task start --onto`) is a row, never a move.
        let facts = crate::task::observe(c, a)?;
        crate::transition::note(c, m, a, &facts, &crate::transition::Labels::default())?;
        Ok(())
    })
    .unwrap();
    let t = db.read(|c| task(c, "task:a", None)).unwrap();
    let phases: Vec<(&str, &str, &str, u32)> = t
        .visited
        .iter()
        .map(|v| (v.from.as_str(), v.event.as_str(), v.to.as_str(), v.count))
        .collect();
    let statuses: Vec<(&str, &str, &str, u32)> = t
        .visited_status
        .iter()
        .map(|v| (v.from.as_str(), v.event.as_str(), v.to.as_str(), v.count))
        .collect();
    assert!(
        t.events.iter().any(|e| e.event == "pin_strategy"),
        "the pin is on the timeline"
    );
    assert!(t.events.iter().any(|e| e.event == "note"), "and the note");
    assert_eq!(
        phases,
        vec![
            ("design", "submit_design", "design_review", 1),
            ("design_review", "approve_design", "implement", 1),
            ("implement", "submit_work", "review", 2),
            ("review", "override", "implement", 1),
        ]
    );
    assert_eq!(
        statuses,
        vec![("open", "override", "in_progress", 1)],
        "the note is not a move"
    );
}

#[test]
fn plan_change_points_follow_a_task_through_open_progress_done_and_reopened() {
    let db = db();
    let a = new_task(&db, "task:a");
    let b = new_task(&db, "task:b");
    with(&db, move |c| {
        for id in [a, b] {
            c.execute(
                &format!("UPDATE items SET created_at = {ISO} WHERE id = ?2"),
                params![T0, id.get()],
            )
            .unwrap();
        }
        status_move(c, a, T0 + MIN, Some("open"), "in_progress", None);
        status_move(
            c,
            a,
            T0 + 2 * MIN,
            Some("in_progress"),
            "needs_review",
            None,
        );
        status_move(c, a, T0 + 3 * MIN, Some("needs_review"), "done", None);
        status_move(c, a, T0 + 4 * MIN, Some("done"), "open", None);
        status_move(c, b, T0 + 4 * MIN, Some("open"), "cancelled", None);
    });
    let points = db.read(move |c| plan_points(c, &[a, b])).unwrap();
    let got: Vec<(i64, u32, u32, u32, u32)> = points
        .iter()
        .map(|p| (p.at_ms - T0, p.open, p.in_progress, p.done, p.cancelled))
        .collect();
    assert_eq!(
        got,
        vec![
            (0, 2, 0, 0, 0),
            (MIN, 1, 1, 0, 0),
            // needs_review is still in progress: no point of its own.
            (3 * MIN, 1, 0, 1, 0),
            (4 * MIN, 1, 0, 0, 1),
        ]
    );
}

#[test]
fn a_long_history_pages_by_cursor_and_loses_nothing() {
    let db = db();
    let a = new_task(&db, "task:a");
    with(&db, move |c| {
        for i in 0..(PAGE_CAP + 20) {
            let i = i64::try_from(i).unwrap();
            // Pairs at the same millisecond, so the cursor must break ties by id.
            status_move(c, a, T0 + i / 2, Some("open"), "open", None);
        }
    });
    let first = db.read(|c| task(c, "task:a", None)).unwrap();
    assert_eq!(first.events.len(), PAGE_CAP);
    let next = first.next.clone().unwrap();
    let second = db.read(move |c| task(c, "task:a", Some(&next))).unwrap();
    assert_eq!(second.events.len(), 20);
    assert_eq!(second.next, None);
    let mut ids: Vec<i64> = first
        .events
        .iter()
        .chain(&second.events)
        .map(|e| e.id)
        .collect();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n);
    assert_eq!(n, PAGE_CAP + 20);
    // A row no machine has (`move`, open -> open) is on the timeline but is not a move.
    assert!(
        first.visited_status.is_empty(),
        "{:?}",
        first.visited_status
    );
}

#[test]
fn a_malformed_cursor_or_session_is_refused() {
    assert!(page(Vec::<Event>::new(), Some("nope")).is_err());
    assert!(page(Vec::<Event>::new(), Some("1.bogus.2")).is_err());
    let db = db();
    assert!(db.read(|c| session(c, "not a session", None, T0)).is_err());
    assert!(db.read(|c| task(c, "task:missing", None)).is_err());
}

#[test]
fn a_registry_events_id_does_not_move_when_another_process_is_seen() {
    use crate::claude_session::HolderRow;
    let row = |pid: &str, seen_at: i64| HolderRow {
        session: S1.to_owned(),
        pid: pid.to_owned(),
        instance: "h".to_owned(),
        cwd: "/w".to_owned(),
        started_at: Some(T0),
        // Which process a start came from, so the ids can be checked against it.
        start_source: Some(format!("started by {pid}")),
        seen_at,
        ended_at: None,
        end_reason: None,
    };
    // The registry lists the most recently seen first: the same two processes, seen in turn.
    let before = super::registry_events(&[row("7", 2), row("9", 1)]);
    let after = super::registry_events(&[row("9", 3), row("7", 2)]);
    let ids = |es: &[Event]| {
        es.iter()
            .map(|e| (e.id, e.detail.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&before), ids(&after));
}

#[test]
fn a_sessions_pages_read_each_source_to_a_page_and_lose_nothing_across_them() {
    let db = db();
    let a = new_task(&db, "task:a");
    let b = new_task(&db, "task:b");
    let n = PAGE_CAP + 150;
    with(&db, move |c| {
        registry(c, S1, T0 - MIN, None);
        bind(c, S1, "agent-x", b);
        let owner = format!("session:12@{S1}:/tmp/wt");
        for i in 0..n {
            let at = T0 + i64::try_from(i / 3).unwrap();
            // Two sources interleaved in time, with ties, and the other session's rows between.
            status_move(c, a, at, Some("open"), "open", Some(&owner));
            phase_move(
                c,
                b,
                at,
                Some("implement"),
                "review",
                "swarm-implementer@agent-x",
            );
            status_move(
                c,
                a,
                at,
                Some("open"),
                "open",
                Some(&format!("session:3@{S2}:/w")),
            );
        }
    });
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let cursor = after.clone();
        let p = db
            .read(move |c| session(c, S1, cursor.as_deref(), T0))
            .unwrap();
        assert!(p.events.len() <= PAGE_CAP);
        pages += 1;
        seen.extend(p.events.iter().map(|e| (e.at_ms, e.source, e.id)));
        // Every page heads with the session's whole summary, not the page's.
        assert_eq!(
            p.summary.last_event_ms,
            Some(T0 + i64::try_from((n - 1) / 3).unwrap())
        );
        match p.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert!(pages >= 3, "{pages}");
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(seen, sorted, "in order, and nothing twice");
    // The registry start, and both of S1's sources in full; none of S2's.
    assert_eq!(seen.len(), 1 + 2 * n);
    let overview = db.read(|c| sessions(c, T0)).unwrap();
    let s1 = overview.sessions.iter().find(|s| s.session == S1).unwrap();
    assert_eq!(
        s1.last_event_ms,
        Some(T0 + i64::try_from((n - 1) / 3).unwrap())
    );
}
