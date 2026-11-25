//! Delivery state, against real `SQLite`: a result reaches its session once, by the socket or by the
//! hook, and only when it is due.

use proptest::prelude::*;

use super::{
    agent_gone, any_for_hook, claim_for_socket, due, get, open, report, take_for_hook,
    DeliveryState, Via, MAX_ATTEMPTS, SUBAGENT_GRACE_MS, WINDOW_MS,
};
use crate::claude_session::{self, Inbox, Process};
use crate::exec::{
    self, Ask, ExecState, ExecTarget, Finished, Outcome, Requester, EXPIRES_AFTER_MS,
    LATEST_END_MS, RUN_GRACE_MS,
};
use crate::Db;

const T0: i64 = 1_800_000_000_000;
const LEASE: i64 = 30_000;

fn who(agent: Option<&str>) -> Requester {
    Requester {
        session: "s1".to_owned(),
        agent_type: agent.map(|_| "implementer".to_owned()),
        agent_id: agent.map(str::to_owned),
        role: "implementer".to_owned(),
        task: None,
    }
}

fn ask(argv: &str) -> Ask {
    Ask {
        target: ExecTarget::Host,
        dir: "/w".to_owned(),
        dir_side: ExecTarget::Host,
        argv: vec![argv.to_owned()],
        reason: "land it".to_owned(),
        pin: None,
        timeout_ms: None,
    }
}

fn request(db: &Db, agent: Option<&str>, argv: &str, now: i64) -> exec::Requested {
    let (r, a) = (who(agent), ask(argv));
    db.write_txn("t", move |c, m| exec::request(c, m, &r, &a, now))
        .unwrap()
}

/// A request filed by `agent` at `T0`, run and finished at `ended`.
fn finished(db: &Db, agent: Option<&str>, argv: &str, ended: i64) -> i64 {
    let id = request(db, agent, argv, T0).row.id;
    db.write_txn("t", move |c, m| {
        exec::begin(c, m, id, None, "app", T0 + 1)?;
        exec::finish(
            c,
            m,
            id,
            &Finished {
                runner: "app",
                outcome: Outcome::Exited,
                exit_status: Some(0),
                tail: b"ok\n",
                total_bytes: 3,
                log_path: None,
            },
            ended,
        )
    })
    .unwrap();
    id
}

fn state(db: &Db, id: i64) -> DeliveryState {
    db.read(move |c| get(c, id)).unwrap().unwrap().state
}

fn due_at(db: &Db, now: i64) -> Vec<i64> {
    db.read(move |c| due(c, now, 100)).unwrap()
}

fn claim(db: &Db, id: i64, now: i64) -> bool {
    db.write_txn("t", move |c, _| claim_for_socket(c, id, LEASE, now))
        .unwrap()
}

fn reported(db: &Db, id: i64, ok: bool, now: i64) -> bool {
    db.write_txn("t", move |c, _| {
        report(c, id, if ok { Ok(()) } else { Err("refused") }, now)
    })
    .unwrap()
}

fn take(db: &Db, agent: Option<&str>, now: i64) -> Vec<i64> {
    let agent = agent.map(str::to_owned);
    db.write_txn("t", move |c, m| {
        take_for_hook(c, m, "s1", agent.as_deref(), now)
    })
    .unwrap()
    .into_iter()
    .map(|t| t.row.id)
    .collect()
}

fn start_inbox(db: &Db, pid: &str, now: i64) {
    let pid = pid.to_owned();
    db.write_txn("t", move |c, m| {
        let p = Process {
            session: "s1",
            pid: &pid,
            instance: "h#b/ns",
        };
        let inbox = Inbox::new("/tmp/cc-socks/1.sock", "tok").unwrap();
        claude_session::started_with_inbox(c, m, &p, "/w", "resume", Some(&inbox), now)
    })
    .unwrap();
}

#[test]
fn a_request_makes_a_waiting_row_with_its_agent() {
    let db = Db::open_in_memory().unwrap();
    let main = request(&db, None, "a", T0).row;
    let sub = request(&db, Some("ag1"), "b", T0).row;
    let d = db.read(move |c| get(c, sub.id)).unwrap().unwrap();
    assert_eq!(d.state, DeliveryState::Waiting);
    assert_eq!(d.agent_id.as_deref(), Some("ag1"));
    assert_eq!(sub.requester.agent_id.as_deref(), Some("ag1"));
    assert_eq!(main.requester.agent_id, None);
    assert_eq!(state(&db, main.id), DeliveryState::Waiting);
}

#[test]
fn a_fold_by_another_agent_sends_the_result_to_the_session() {
    let db = Db::open_in_memory().unwrap();
    let id = request(&db, Some("ag1"), "a", T0).row.id;
    let same = request(&db, Some("ag1"), "a", T0 + 1);
    assert!(same.folded);
    assert_eq!(same.row.requester.agent_id.as_deref(), Some("ag1"));
    let other = request(&db, Some("ag2"), "a", T0 + 2);
    assert!(other.folded);
    assert_eq!(other.row.id, id);
    assert_eq!(other.row.requester.agent_id, None);

    let main_first = request(&db, Some("ag3"), "b", T0).row.id;
    let by_main = request(&db, None, "b", T0 + 1);
    assert_eq!(by_main.row.id, main_first);
    assert_eq!(
        by_main.row.requester.agent_id, None,
        "the main agent folded"
    );
}

#[test]
fn only_a_terminal_request_ended_within_the_window_is_due() {
    let db = Db::open_in_memory().unwrap();
    let pending = request(&db, None, "p", T0).row.id;
    let done = finished(&db, None, "f", T0 + 10);
    let old = finished(&db, None, "o", T0 + 5);
    let at = T0 + 5 + WINDOW_MS - 1;
    assert_eq!(due_at(&db, at), vec![done, old]);
    assert_eq!(
        due_at(&db, T0 + 5 + WINDOW_MS),
        vec![done],
        "{old} ended a day ago"
    );
    assert!(!due_at(&db, at).contains(&pending));
    assert!(!claim(&db, old, T0 + 5 + WINDOW_MS));
    assert_eq!(
        take(&db, None, T0 + 10 + WINDOW_MS),
        vec![pending],
        "the two finished ones ended a day ago; the pending one expires in this take, and is new"
    );
}

#[test]
fn a_claim_reported_ok_is_delivered_once() {
    let db = Db::open_in_memory().unwrap();
    let id = finished(&db, None, "a", T0 + 10);
    assert!(claim(&db, id, T0 + 20));
    assert!(!claim(&db, id, T0 + 21), "claimed already");
    assert!(
        take(&db, None, T0 + 22).is_empty(),
        "the hook leaves a live lease to its holder"
    );
    assert!(reported(&db, id, true, T0 + 23));
    let d = db.read(move |c| get(c, id)).unwrap().unwrap();
    assert_eq!(d.state, DeliveryState::Delivered);
    assert_eq!(d.via, Some(Via::Socket));
    assert_eq!(d.delivered_at, Some(T0 + 23));
    assert!(!claim(&db, id, T0 + 100_000));
    assert!(take(&db, None, T0 + 100_000).is_empty());
    assert!(
        !reported(&db, id, true, T0 + 24),
        "a delivered row is never reported again"
    );
    assert!(due_at(&db, T0 + 100_000).is_empty());
}

#[test]
fn a_lapsed_lease_is_claimable_again_and_the_hook_takes_it() {
    let db = Db::open_in_memory().unwrap();
    let id = finished(&db, None, "a", T0 + 10);
    assert!(claim(&db, id, T0 + 20));
    assert!(due_at(&db, T0 + 20 + LEASE - 1).is_empty());
    assert_eq!(due_at(&db, T0 + 20 + LEASE), vec![id]);
    assert!(claim(&db, id, T0 + 20 + LEASE));
    // The first claimer's report is now the second's to make: a row reports from `sending` only.
    assert_eq!(take(&db, None, T0 + 20 + 2 * LEASE), vec![id]);
    assert!(!reported(&db, id, true, T0 + 20 + 2 * LEASE));
    assert_eq!(
        db.read(move |c| get(c, id)).unwrap().unwrap().via,
        Some(Via::Hook)
    );
}

#[test]
fn a_subagents_row_waits_for_its_end_or_the_grace() {
    let db = Db::open_in_memory().unwrap();
    let id = finished(&db, Some("ag1"), "a", T0 + 10);
    assert!(due_at(&db, T0 + 11).is_empty());
    assert!(!claim(&db, id, T0 + 11));
    assert_eq!(due_at(&db, T0 + 10 + SUBAGENT_GRACE_MS), vec![id]);

    let other = finished(&db, Some("ag2"), "b", T0 + 10);
    assert!(!due_at(&db, T0 + 12).contains(&other));
    let n = db
        .write_txn("t", |c, _| agent_gone(c, "s1", "ag2", T0 + 12))
        .unwrap();
    assert_eq!(n, 1);
    assert!(due_at(&db, T0 + 12).contains(&other));
}

#[test]
fn a_main_agent_take_never_takes_a_live_subagents_row() {
    let db = Db::open_in_memory().unwrap();
    let mine = finished(&db, None, "a", T0 + 10);
    let theirs = finished(&db, Some("ag1"), "b", T0 + 10);
    let now = T0 + 10 + SUBAGENT_GRACE_MS * 3;
    assert!(db.read(move |c| any_for_hook(c, "s1", None, now)).unwrap());
    assert_eq!(take(&db, None, now), vec![mine]);
    assert!(!db.read(move |c| any_for_hook(c, "s1", None, now)).unwrap());
    assert!(take(&db, Some("ag2"), now).is_empty(), "another subagent's");
    assert_eq!(take(&db, Some("ag1"), now), vec![theirs]);

    let later = finished(&db, Some("ag3"), "c", T0 + 10);
    db.write_txn("t", move |c, _| agent_gone(c, "s1", "ag3", now))
        .unwrap();
    assert_eq!(take(&db, None, now), vec![later], "a stopped subagent's");
}

#[test]
fn a_failed_row_is_due_again_only_after_a_newer_inbox_and_at_most_five_times() {
    let db = Db::open_in_memory().unwrap();
    let id = finished(&db, None, "a", T0 + 10);
    let mut now = T0 + 20;
    for attempt in 0..MAX_ATTEMPTS {
        assert!(claim(&db, id, now), "attempt {attempt}");
        assert!(reported(&db, id, false, now + 1));
        assert!(
            due_at(&db, now + 2).is_empty(),
            "no newer inbox after attempt {attempt}"
        );
        start_inbox(&db, "100", now + 3);
        now += 10;
    }
    let d = db.read(move |c| get(c, id)).unwrap().unwrap();
    assert_eq!((d.state, d.attempts), (DeliveryState::Failed, MAX_ATTEMPTS));
    assert_eq!(d.last_error.as_deref(), Some("refused"));
    assert!(due_at(&db, now).is_empty(), "five attempts is the cap");
    assert_eq!(take(&db, None, now), vec![id], "the hook still delivers it");
}

#[test]
fn a_provisional_interrupt_is_not_due_until_the_runner_can_no_longer_report() {
    let db = Db::open_in_memory().unwrap();
    let id = request(&db, None, "a", T0).row.id;
    db.write_txn("t", move |c, m| exec::begin(c, m, id, None, "app", T0))
        .unwrap();
    let late = T0 + exec::DEFAULT_TIMEOUT_MS + RUN_GRACE_MS;
    let r = db
        .write_txn("t", move |c, m| exec::status(c, m, id, late))
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(r.outcome, Some(Outcome::Interrupted));
    assert!(due_at(&db, late).is_empty());
    assert!(take(&db, None, late).is_empty());
    assert_eq!(due_at(&db, T0 + LATEST_END_MS), vec![id]);
}

#[test]
fn the_hook_take_settles_an_expiry_and_delivers_it() {
    let db = Db::open_in_memory().unwrap();
    let id = request(&db, None, "a", T0).row.id;
    let now = T0 + EXPIRES_AFTER_MS;
    assert!(db.read(move |c| any_for_hook(c, "s1", None, now)).unwrap());
    let taken = db
        .write_txn("t", move |c, m| take_for_hook(c, m, "s1", None, now))
        .unwrap();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].row.id, id);
    assert_eq!(taken[0].row.state, ExecState::Expired);
    assert!(taken[0].text().starts_with(&format!(
        "[jkb run request] exec-{id} expired: unanswered for two hours"
    )));
}

#[test]
fn open_lists_the_sessions_pending_and_running_requests() {
    let db = Db::open_in_memory().unwrap();
    let p = request(&db, None, "p", T0).row.id;
    let r = request(&db, None, "r", T0).row.id;
    db.write_txn("t", move |c, m| exec::begin(c, m, r, None, "app", T0))
        .unwrap();
    finished(&db, None, "f", T0 + 10);
    let ids: Vec<i64> = db
        .read(|c| open(c, "s1"))
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![p, r]);
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Claim,
    ReportOk,
    ReportErr,
    TakeMain,
    Advance(i64),
    Inbox,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::Claim),
        Just(Op::ReportOk),
        Just(Op::ReportErr),
        Just(Op::TakeMain),
        (1..2 * LEASE).prop_map(Op::Advance),
        Just(Op::Inbox),
    ]
}

proptest! {
    /// However the socket's claims and reports interleave with the hook's takes, a result is
    /// delivered at most once, and once delivered nothing hands it over again.
    #[test]
    fn a_result_is_delivered_at_most_once(ops in proptest::collection::vec(op(), 1..40)) {
        let db = Db::open_in_memory().unwrap();
        let id = finished(&db, None, "a", T0 + 10);
        let mut now = T0 + 20;
        let mut deliveries = 0;
        let mut pid = 0;
        for o in ops {
            match o {
                Op::Claim => { claim(&db, id, now); }
                Op::ReportOk => if reported(&db, id, true, now) { deliveries += 1; },
                Op::ReportErr => { reported(&db, id, false, now); }
                Op::TakeMain => deliveries += take(&db, None, now).len(),
                Op::Advance(ms) => now += ms,
                Op::Inbox => {
                    pid += 1;
                    start_inbox(&db, &pid.to_string(), now);
                }
            }
            prop_assert!(deliveries <= 1);
            if deliveries == 1 {
                prop_assert_eq!(state(&db, id), DeliveryState::Delivered);
            }
        }
        // Whatever happened, the hook delivers what the socket did not.
        deliveries += take(&db, None, now + 2 * LEASE).len();
        prop_assert_eq!(deliveries, 1);
    }
}
