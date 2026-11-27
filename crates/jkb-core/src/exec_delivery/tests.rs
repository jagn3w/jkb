//! Delivery state, against real `SQLite`: a result reaches its session once, by the socket or by the
//! hook, and only when it is due.

use proptest::prelude::*;

use super::{
    ack_hook, agent_gone, any_for_hook, claim_for_socket, due, get, open, report, take_for_hook,
    DeliveryState, Via, HOOK_LEASE_MS, MAX_ATTEMPTS, SUBAGENT_GRACE_MS, WINDOW_MS,
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

/// The container boot every test's inbox is registered in ([`start_inbox`]).
const BOOT: &str = "h#b";

fn due_at(db: &Db, now: i64) -> Vec<i64> {
    db.write_txn("t", move |c, m| due(c, m, BOOT, now, 100))
        .unwrap()
}

fn claim(db: &Db, id: i64, now: i64) -> bool {
    db.write_txn("t", move |c, _| claim_for_socket(c, id, BOOT, LEASE, now))
        .unwrap()
        .is_some()
}

fn reported(db: &Db, id: i64, ok: bool, now: i64) -> bool {
    db.write_txn("t", move |c, _| {
        report(c, id, if ok { Ok(()) } else { Err("refused") }, now)
    })
    .unwrap()
}

/// The hook's first phase: what it is handed, under its lease.
fn hand(db: &Db, agent: Option<&str>, now: i64) -> Vec<i64> {
    let agent = agent.map(str::to_owned);
    db.write_txn("t", move |c, m| {
        take_for_hook(c, m, "s1", agent.as_deref(), now)
    })
    .unwrap()
    .into_iter()
    .map(|t| t.row.id)
    .collect()
}

/// The hook's second phase: it printed `ids`.
fn ack(db: &Db, ids: &[i64], now: i64) -> usize {
    let ids = ids.to_vec();
    db.write_txn("t", move |c, _| ack_hook(c, "s1", &ids, now))
        .unwrap()
}

/// A hook take that printed what it was handed: the ids delivered.
fn take(db: &Db, agent: Option<&str>, now: i64) -> Vec<i64> {
    let ids = hand(db, agent, now);
    assert_eq!(ack(db, &ids, now), ids.len());
    ids
}

fn start_inbox(db: &Db, pid: &str, now: i64) {
    start_inbox_in(db, pid, "h#b/ns", now);
}

/// Session `s1`'s process `pid`, in `instance`, registers an inbox at `now`.
fn start_inbox_in(db: &Db, pid: &str, instance: &str, now: i64) {
    let (pid, instance) = (pid.to_owned(), instance.to_owned());
    db.write_txn("t", move |c, m| {
        let p = Process {
            session: "s1",
            pid: &pid,
            instance: &instance,
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
    start_inbox(&db, "1", T0);
    let done = finished(&db, None, "f", T0 + 10);
    let old = finished(&db, None, "o", T0 + 5);
    let at = T0 + 5 + WINDOW_MS - 1;
    // Filed late enough that the sweeps below, which settle expiries, find it still pending.
    let filed = at - 1000;
    let pending = request(&db, None, "p", filed).row.id;
    assert_eq!(due_at(&db, at), vec![done, old]);
    assert_eq!(
        due_at(&db, T0 + 5 + WINDOW_MS),
        vec![done],
        "{old} ended a day ago"
    );
    assert!(!due_at(&db, at).contains(&pending));
    assert!(!claim(&db, old, T0 + 5 + WINDOW_MS));
    assert_eq!(
        take(&db, None, filed + EXPIRES_AFTER_MS),
        vec![pending],
        "the two finished ones ended a day ago; the pending one expires in this take, and is new"
    );
}

#[test]
fn a_claim_reported_ok_is_delivered_once() {
    let db = Db::open_in_memory().unwrap();
    start_inbox(&db, "1", T0);
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
    start_inbox(&db, "1", T0);
    let id = finished(&db, None, "a", T0 + 10);
    assert!(claim(&db, id, T0 + 20));
    assert!(due_at(&db, T0 + 20 + LEASE - 1).is_empty());
    assert_eq!(due_at(&db, T0 + 20 + LEASE), vec![id]);
    assert!(claim(&db, id, T0 + 20 + LEASE));
    // The socket's second lease lapses too, and the hook is handed the row; whichever then says the
    // message went out first delivers it, and the other finds it delivered.
    let at = T0 + 20 + 2 * LEASE;
    assert_eq!(hand(&db, None, at), vec![id]);
    assert!(
        reported(&db, id, true, at + 1),
        "the late post did reach the session"
    );
    assert_eq!(ack(&db, &[id], at + 2), 0);
    assert_eq!(
        db.read(move |c| get(c, id)).unwrap().unwrap().via,
        Some(Via::Socket)
    );
}

/// The hook's take is two-phase: a take whose answer never reached the session — the hook's client
/// gave up, or the hook died before it printed — is handed over again once its lease lapses, and only
/// an acknowledgement delivers it.
#[test]
fn a_take_never_acknowledged_is_handed_over_again() {
    let db = Db::open_in_memory().unwrap();
    start_inbox(&db, "1", T0);
    let id = finished(&db, None, "a", T0 + 10);
    let at = T0 + 20;
    assert_eq!(
        hand(&db, None, at),
        vec![id],
        "the reply to this take is lost"
    );
    assert_eq!(state(&db, id), DeliveryState::Sending);
    assert!(
        hand(&db, None, at + HOOK_LEASE_MS - 1).is_empty(),
        "held for its ack"
    );
    assert!(due_at(&db, at + HOOK_LEASE_MS - 1).is_empty());
    assert_eq!(
        take(&db, None, at + HOOK_LEASE_MS),
        vec![id],
        "handed over again"
    );
    let d = db.read(move |c| get(c, id)).unwrap().unwrap();
    assert_eq!(
        (d.state, d.via),
        (DeliveryState::Delivered, Some(Via::Hook))
    );
    assert_eq!(ack(&db, &[id], at + HOOK_LEASE_MS + 1), 0, "delivered once");
    // An ack names its own session's requests only.
    let other = finished(&db, None, "b", T0 + 10);
    hand(&db, None, at + HOOK_LEASE_MS + 2);
    let n = db
        .write_txn("t", move |c, _| ack_hook(c, "s2", &[other], at))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn a_subagents_row_waits_for_its_end_or_the_grace() {
    let db = Db::open_in_memory().unwrap();
    start_inbox(&db, "1", T0);
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
    let now = T0 + 10 + SUBAGENT_GRACE_MS - 1;
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

/// A subagent whose `SubagentStop` was lost — the hook is fail-open — does not strand its results:
/// once its request ended the grace ago, the main agent is handed them.
#[test]
fn a_subagents_result_is_the_main_agents_after_the_grace() {
    let db = Db::open_in_memory().unwrap();
    let theirs = finished(&db, Some("ag1"), "b", T0 + 10);
    assert!(take(&db, None, T0 + 10 + SUBAGENT_GRACE_MS - 1).is_empty());
    let at = T0 + 10 + SUBAGENT_GRACE_MS;
    assert!(db.read(move |c| any_for_hook(c, "s1", None, at)).unwrap());
    assert_eq!(take(&db, None, at), vec![theirs]);
}

#[test]
fn a_failed_row_is_due_again_only_after_a_newer_inbox_and_at_most_five_times() {
    let db = Db::open_in_memory().unwrap();
    start_inbox(&db, "1", T0);
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
    start_inbox(&db, "1", T0);
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

/// A result is due for a forwarder's socket only while its session has a live inbox in that
/// forwarder's container boot: with none, it waits for the hook path rather than being claimed and
/// failed; with one only in another container, that container's forwarder is the one to post it.
#[test]
fn a_result_is_due_only_where_its_session_has_a_live_inbox() {
    let db = Db::open_in_memory().unwrap();
    let id = finished(&db, None, "a", T0 + 10);
    assert!(due_at(&db, T0 + 20).is_empty(), "no inbox anywhere");
    assert!(!claim(&db, id, T0 + 20));
    assert_eq!(state(&db, id), DeliveryState::Waiting, "left for the hook");

    start_inbox_in(&db, "7", "h#other/ns", T0 + 21);
    assert!(due_at(&db, T0 + 22).is_empty(), "another boot's inbox");
    assert!(!claim(&db, id, T0 + 22));
    let theirs = db
        .write_txn("t", move |c, m| due(c, m, "h#other", T0 + 22, 10))
        .unwrap();
    assert_eq!(theirs, vec![id]);
    // A boot is matched whole: `h#b` is not a prefix match of `h#bb`.
    start_inbox_in(&db, "8", "h#bb/ns", T0 + 23);
    assert!(due_at(&db, T0 + 24).is_empty());

    start_inbox(&db, "9", T0 + 25);
    assert_eq!(due_at(&db, T0 + 26), vec![id]);
    // An inbox whose process ended is no target.
    db.write_txn("t", move |c, m| {
        let p = Process {
            session: "s1",
            pid: "9",
            instance: "h#b/ns",
        };
        claude_session::ended(c, m, &p, "exit", T0 + 27).map(|_| ())
    })
    .unwrap();
    assert!(due_at(&db, T0 + 28).is_empty(), "its only inbox here ended");
}

/// A claim hands over the message and the session's targets in the claiming boot, newest start
/// first; a lease outside its bounds is refused, and claims nothing.
#[test]
fn a_claim_carries_the_message_and_its_targets_newest_first() {
    let db = Db::open_in_memory().unwrap();
    start_inbox(&db, "1", T0);
    start_inbox(&db, "2", T0 + 5);
    start_inbox_in(&db, "3", "h#other/ns", T0 + 6);
    let id = finished(&db, None, "a", T0 + 10);
    for bad in [0, -1, super::MAX_SOCKET_LEASE_MS + 1] {
        assert!(db
            .write_txn("t", move |c, _| claim_for_socket(c, id, BOOT, bad, T0 + 20))
            .is_err());
    }
    assert_eq!(state(&db, id), DeliveryState::Waiting);
    let c = db
        .write_txn("t", move |c, _| {
            claim_for_socket(c, id, BOOT, LEASE, T0 + 20)
        })
        .unwrap()
        .expect("claimed");
    assert_eq!(c.taken.row.id, id);
    assert!(c
        .taken
        .text()
        .starts_with(&format!("[jkb run request] exec-{id} finished exit 0")));
    let pids: Vec<&str> = c.targets.iter().map(|t| t.pid.as_str()).collect();
    assert_eq!(pids, vec!["2", "1"], "this boot's only, newest first");
    assert_eq!(c.targets[0].inbox.socket(), "/tmp/cc-socks/1.sock");
    assert_eq!(state(&db, id), DeliveryState::Sending);
}

/// The forwarder's sweep settles an expiry itself: a request nobody answered is due at its expiry,
/// without waiting for some other exec op to expire it.
#[test]
fn the_sweep_settles_an_expiry_and_finds_it_due() {
    let db = Db::open_in_memory().unwrap();
    start_inbox(&db, "1", T0);
    let id = request(&db, None, "a", T0).row.id;
    assert!(due_at(&db, T0 + EXPIRES_AFTER_MS - 1).is_empty());
    assert_eq!(due_at(&db, T0 + EXPIRES_AFTER_MS), vec![id]);
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
    AckMain,
    Advance(i64),
    Inbox,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::Claim),
        Just(Op::ReportOk),
        Just(Op::ReportErr),
        Just(Op::TakeMain),
        Just(Op::AckMain),
        (1..2 * LEASE).prop_map(Op::Advance),
        Just(Op::Inbox),
    ]
}

proptest! {
    /// However the socket's claims and reports interleave with the hook's takes and acks, a result is
    /// delivered at most once, and once delivered nothing hands it over again.
    #[test]
    fn a_result_is_delivered_at_most_once(ops in proptest::collection::vec(op(), 1..40)) {
        let db = Db::open_in_memory().unwrap();
        start_inbox(&db, "1", T0);
        let id = finished(&db, None, "a", T0 + 10);
        let mut now = T0 + 20;
        let mut deliveries = 0;
        let mut pid = 0;
        let mut handed: Vec<i64> = Vec::new();
        for o in ops {
            match o {
                Op::Claim => { claim(&db, id, now); }
                Op::ReportOk => if reported(&db, id, true, now) { deliveries += 1; },
                Op::ReportErr => { reported(&db, id, false, now); }
                Op::TakeMain => handed = hand(&db, None, now),
                Op::AckMain => deliveries += ack(&db, &handed, now),
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
