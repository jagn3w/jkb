//! The check is the point of the module; the rest pin the behaviours four review rounds argued
//! about, and — since r3.2 N1 — what running the table against the database adds: the record and
//! the sends are one transaction, a topic nobody reads is not written to, and a sweep never takes
//! down a record that changed after it was probed.

use jkb_fsm::{Fact, Outcome};
use serde_json::json;

use super::{
    gone, machine, notification_id, observe, open_sessions, sanitize, tool_in, Applied, NotifCtx,
    NotifEffect, NotifEvent, NotifState, Observation, SessionRecord, KIND_POST, KIND_WITHDRAW,
    MAX_BODY_CHARS, POST_TTL_MS, TOPIC,
};
use crate::mq::{self, Delivered, Start, TopicSpec};
use crate::{Db, Error};

const T0: i64 = 1_800_000_000_000;

fn ctx(at: NotifState) -> NotifCtx {
    NotifCtx {
        at,
        tool_named: true,
        tool_matches: Fact::Yes,
        session_alive: Fact::Unknown,
    }
}

/// **The artefact.** Every state reaches rest, every state is reachable, no two rows compete, no
/// reconciliation is unguarded, every verb runs twice, and nothing dead-ends. Each of those was a
/// question this feature could not answer while its rules were shell conditionals.
#[test]
fn the_table_has_no_defects() {
    let defects = machine().check();
    assert!(defects.is_empty(), "{defects:#?}");
}

/// The concurrency defect, which is what forced the tool-scoped state in the first place: one
/// assistant message batches several calls, and a slow allowlisted one finishing must not take
/// down a prompt nobody has answered.
#[test]
fn a_different_tool_finishing_does_not_withdraw() {
    let m = machine();
    let mut c = ctx(NotifState::AwaitingTool);
    c.tool_matches = Fact::No;

    let out = m.apply(&c, NotifEvent::ToolFinished);
    assert!(!out.moved(), "a mismatched tool must not withdraw: {out:?}");
    assert!(out.effects().is_empty());

    c.tool_matches = Fact::Yes;
    let out = m.apply(&c, NotifEvent::ToolFinished);
    assert_eq!(out.state(), NotifState::Absent);
    assert_eq!(
        out.effects(),
        &[NotifEffect::Withdraw, NotifEffect::Forget],
        "withdrawing and forgetting are one plan, never half applied"
    );
}

/// A tool finishing says nothing about a notification whose prompt named no tool. There is no
/// question to ask, so there is no row — a named refusal rather than a silent no-op.
#[test]
fn a_tool_event_is_undefined_for_an_untooled_notification() {
    let out = machine().apply(&ctx(NotifState::AwaitingUser), NotifEvent::ToolFinished);
    assert!(matches!(out, Outcome::Undefined { .. }), "{out:?}");
}

/// The sweep does not consult the record, because the screen is a consumer's and may disagree with
/// it: a consumer whose withdraw failed has already acked it. So `Stop` withdraws anyway.
#[test]
fn the_sweep_withdraws_from_absent() {
    for event in [NotifEvent::TurnEnded, NotifEvent::SessionEnded] {
        let out = machine().apply(&ctx(NotifState::Absent), event);
        assert_eq!(out.effects(), &[NotifEffect::Withdraw], "{event:?}");
    }
}

/// Liveness by evidence, never by age (D27): `Unknown` must refuse, or a paused-but-alive
/// session loses the notification it is waiting on.
#[test]
fn an_unprovable_session_death_refuses() {
    let m = machine();
    let mut c = ctx(NotifState::AwaitingTool);

    c.session_alive = Fact::Unknown;
    assert!(!m.apply(&c, NotifEvent::SessionGone).moved());
    c.session_alive = Fact::Yes;
    assert!(!m.apply(&c, NotifEvent::SessionGone).moved());

    c.session_alive = Fact::No;
    let out = m.apply(&c, NotifEvent::SessionGone);
    assert_eq!(out.state(), NotifState::Absent);
    assert_eq!(out.effects(), &[NotifEffect::Withdraw, NotifEffect::Forget]);
}

/// Whether the prompt named a tool decides which posted state we land in — the parse failing is
/// a declared destination, not a fallen-through branch — and a `Needed` always posts (r3.2 N1):
/// whether it can be displayed is the consumer's question.
#[test]
fn a_needed_always_posts_and_lands_by_whether_a_tool_was_named() {
    let m = machine();
    let mut c = ctx(NotifState::Absent);

    let out = m.apply(&c, NotifEvent::Needed);
    assert_eq!(out.state(), NotifState::AwaitingTool);
    assert_eq!(out.effects(), &[NotifEffect::Post, NotifEffect::Remember]);

    c.tool_named = false;
    let out = m.apply(&c, NotifEvent::Needed);
    assert_eq!(out.state(), NotifState::AwaitingUser);
    assert_eq!(out.effects(), &[NotifEffect::Post, NotifEffect::Remember]);
}

/// A second prompt replaces the first rather than stacking, and re-posting over a live
/// notification is allowed from either posted state.
#[test]
fn a_second_prompt_re_posts() {
    let m = machine();
    for at in [NotifState::AwaitingTool, NotifState::AwaitingUser] {
        let out = m.apply(&ctx(at), NotifEvent::Needed);
        assert_eq!(out.state(), NotifState::AwaitingTool);
        assert_eq!(out.effects(), &[NotifEffect::Post, NotifEffect::Remember]);
    }
}

/// **The plan-shape rule, over every plan the table can produce.** Screen effects before record
/// effects. A rule each plan had to remember would drift; this walks them.
#[test]
fn plans_change_the_screen_before_the_record() {
    let m = machine();
    let facts = [Fact::Yes, Fact::No, Fact::Unknown];
    for &at in <NotifState as jkb_fsm::State>::ALL {
        for &event in <NotifEvent as jkb_fsm::Event>::ALL {
            for matches in facts {
                for alive in facts {
                    for named in [true, false] {
                        let c = NotifCtx {
                            at,
                            tool_named: named,
                            tool_matches: matches,
                            session_alive: alive,
                        };
                        let effects = m.apply(&c, event).effects().to_vec();
                        let first_record = effects.iter().position(|e| !e.touches_screen());
                        let last_screen = effects.iter().rposition(|e| e.touches_screen());
                        if let (Some(r), Some(sc)) = (first_record, last_screen) {
                            assert!(
                                sc < r,
                                "{at:?}/{event:?} plans a screen effect after a record one: \
                                 {effects:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// A path-like session id must not be able to name a path.
#[test]
fn a_path_like_session_id_is_sanitised() {
    assert_eq!(sanitize("../../etc/passwd"), "______etc_passwd");
    assert_eq!(sanitize("ok-9_A"), "ok-9_A");
    assert_eq!(notification_id("ok-9_A"), "jkb-claude-ok-9_A");
}

/// The tool is read out of prose, so its failure mode matters more than its success.
#[test]
fn the_tool_is_read_out_of_the_message() {
    assert_eq!(
        tool_in("Claude needs your permission to use Bash").as_deref(),
        Some("Bash")
    );
    assert_eq!(
        tool_in("permission to use Read the file").as_deref(),
        Some("Read")
    );
    assert_eq!(tool_in("Claude is waiting for your input"), None);
    assert_eq!(tool_in("permission to use "), None);
}

// ---------------------------------------------------------------------------------------------
// Against the database.

/// A database with the notify topic and, when `group`, one consumer group on it.
fn db(group: bool) -> Db {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", move |c, m| {
        mq::topic_create(c, m, TOPIC, &TopicSpec::default(), T0)?;
        if group {
            mq::group_create(c, m, TOPIC, "g", Start::FromStart, T0)?;
        }
        Ok(())
    })
    .unwrap();
    db
}

fn obs(event: NotifEvent, session: &str) -> Observation {
    Observation {
        session: session.to_owned(),
        event,
        finished_tool: String::new(),
        message: String::new(),
        cwd: "/repos/jkb/.jkb/work/wt".to_owned(),
        owner: "4242".to_owned(),
        instance: "host-a".to_owned(),
    }
}

fn needed(session: &str) -> Observation {
    Observation {
        message: "Claude needs your permission to use Bash".to_owned(),
        ..obs(NotifEvent::Needed, session)
    }
}

fn finished(session: &str, tool: &str) -> Observation {
    Observation {
        finished_tool: tool.to_owned(),
        ..obs(NotifEvent::ToolFinished, session)
    }
}

fn apply(db: &Db, o: Observation) -> crate::Result<Applied> {
    db.write_txn("t", move |c, m| observe(c, m, &o, T0))
}

fn sent(db: &Db) -> Vec<Delivered> {
    db.read(|c| mq::tail(c, TOPIC, 100, T0)).unwrap()
}

fn records(db: &Db) -> Vec<SessionRecord> {
    db.read(open_sessions).unwrap()
}

/// A permission prompt sends one post — carrying its id, text, and the 12 h TTL — and records the
/// tool, owner and instance the sweep will need.
#[test]
fn a_prompt_posts_and_records_in_one_transaction() {
    let db = db(true);
    let out = apply(&db, needed("s1")).unwrap();
    assert_eq!(out.state, NotifState::AwaitingTool);
    assert_eq!(out.sent, 1);

    let msgs = sent(&db);
    assert_eq!(msgs.len(), 1);
    let m = &msgs[0];
    assert_eq!(m.kind, KIND_POST);
    assert_eq!(m.key, "session/s1");
    assert_eq!(m.expires_at, Some(T0 + POST_TTL_MS));
    assert_eq!(
        m.payload,
        json!({
            "id": "jkb-claude-s1",
            "session": "s1",
            "title": "Claude Code",
            "subtitle": "wt",
            "body": "Claude needs your permission to use Bash",
        })
    );
    assert_eq!(m.producer, "notify@host-a");

    let recs = records(&db);
    assert_eq!(recs.len(), 1);
    assert_eq!(
        (
            recs[0].session.as_str(),
            recs[0].tool.as_str(),
            recs[0].owner.as_str(),
            recs[0].instance.as_str()
        ),
        ("s1", "Bash", "4242", "host-a")
    );
}

/// End to end through the record: a concurrent tool must not take the prompt down, and the
/// prompted one must — with a withdrawal that carries no TTL.
#[test]
fn the_record_and_the_sends_agree_about_a_concurrent_tool() {
    let db = db(true);
    apply(&db, needed("s1")).unwrap();

    let out = apply(&db, finished("s1", "Read")).unwrap();
    assert!(!out.moved, "{out:?}");
    assert_eq!(sent(&db).len(), 1, "a different tool sends nothing");
    assert_eq!(records(&db).len(), 1, "and keeps the record");

    let out = apply(&db, finished("s1", "Bash")).unwrap();
    assert_eq!(out.effects, [NotifEffect::Withdraw, NotifEffect::Forget]);
    let msgs = sent(&db);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1].kind, KIND_WITHDRAW);
    assert_eq!(
        msgs[1].payload,
        json!({ "id": "jkb-claude-s1", "session": "s1" })
    );
    assert_eq!(msgs[1].expires_at, None);
    assert!(records(&db).is_empty());
}

/// The idle prompt names no tool: it lands in `awaiting_user`, and typing clears it.
#[test]
fn the_idle_prompt_is_cleared_by_the_user_not_by_a_tool() {
    let db = db(true);
    let idle = Observation {
        message: "Claude is waiting for your input".to_owned(),
        ..obs(NotifEvent::Needed, "s1")
    };
    assert_eq!(apply(&db, idle).unwrap().state, NotifState::AwaitingUser);
    assert_eq!(records(&db)[0].tool, "");
    assert!(
        apply(&db, finished("s1", "Bash")).is_ok(),
        "an undefined event is not an error"
    );
    assert_eq!(
        records(&db).len(),
        1,
        "a tool does not clear the idle prompt"
    );
    apply(&db, obs(NotifEvent::UserActed, "s1")).unwrap();
    assert!(records(&db).is_empty());
}

/// **A topic nobody reads is not written to**, or on a machine with no notifier it fills to its
/// cap and then refuses every hook call. The record still moves, so the machine's state does not
/// depend on whether anyone is listening.
#[test]
fn a_topic_with_no_group_moves_the_record_and_sends_nothing() {
    let db = db(false);
    let out = apply(&db, needed("s1")).unwrap();
    assert_eq!(out.effects, [NotifEffect::Post, NotifEffect::Remember]);
    assert_eq!(out.sent, 0);
    assert!(sent(&db).is_empty());
    assert_eq!(records(&db).len(), 1);

    apply(&db, obs(NotifEvent::TurnEnded, "s1")).unwrap();
    assert!(sent(&db).is_empty());
    assert!(records(&db).is_empty());
}

/// A missing topic means setup never ran. The event is refused whole: no record is written for a
/// notification that was never sent.
#[test]
fn a_missing_topic_refuses_the_event_and_writes_no_record() {
    let db = Db::open_in_memory().unwrap();
    let err = apply(&db, needed("s1")).unwrap_err();
    assert!(
        matches!(err, Error::Queue(mq::QueueError::NoSuchTopic(ref t)) if t == TOPIC),
        "{err:?}"
    );
    assert!(records(&db).is_empty());
}

/// A send the queue refuses leaves the record as it was, so it still names what is on screen and
/// the next event tries again — never a record saying a notification is gone whose withdrawal was
/// never sent. (Every plan sends before it records, so this is the plan order and the transaction
/// agreeing; no plan can fail after a record write for a test to watch it roll back.)
#[test]
fn a_refused_send_leaves_the_record_as_it_was() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        let spec = TopicSpec {
            max_messages: 1,
            ..TopicSpec::default()
        };
        mq::topic_create(c, m, TOPIC, &spec, T0)?;
        mq::group_create(c, m, TOPIC, "g", Start::FromStart, T0)
    })
    .unwrap();
    apply(&db, needed("s1")).unwrap();

    // The post is unconsumed and the topic holds one message: the withdrawal cannot fit.
    let err = apply(&db, obs(NotifEvent::UserActed, "s1")).unwrap_err();
    assert!(
        matches!(err, Error::Queue(mq::QueueError::QueueFull { .. })),
        "{err:?}"
    );
    assert_eq!(
        records(&db).len(),
        1,
        "the record still names what is on screen"
    );
}

/// The daemon is reached from a container, so what an observation may carry is checked here rather
/// than trusted from the hook.
#[test]
fn malformed_observations_are_refused() {
    let db = db(true);
    for (o, what) in [
        (needed("../etc"), "an unsanitized session"),
        (needed(""), "an empty session"),
        (
            Observation {
                owner: "12a".to_owned(),
                ..needed("s1")
            },
            "a non-numeric owner",
        ),
        (
            Observation {
                instance: "a\nb".to_owned(),
                ..needed("s1")
            },
            "an instance with a newline",
        ),
        (
            Observation {
                instance: "x".repeat(151),
                ..needed("s1")
            },
            "an instance past its limit",
        ),
        (obs(NotifEvent::SessionGone, "s1"), "a session_gone"),
    ] {
        let err = apply(&db, o).unwrap_err();
        assert!(
            matches!(err, Error::Queue(mq::QueueError::Invalid { .. })),
            "{what}: {err:?}"
        );
    }
    assert!(sent(&db).is_empty());
    assert!(records(&db).is_empty());
}

/// A long message is shortened, not refused: a refusal would post nothing at all.
#[test]
fn a_long_body_is_shortened() {
    let db = db(true);
    apply(
        &db,
        Observation {
            message: "é".repeat(MAX_BODY_CHARS + 50),
            ..needed("s1")
        },
    )
    .unwrap();
    let body = sent(&db)[0].payload["body"].as_str().unwrap().to_owned();
    assert_eq!(body.chars().count(), MAX_BODY_CHARS);
}

/// The sweep's half in the database: withdraw and forget a provably-gone session — **only while the
/// record still names the owner that was probed**. A session resumed in between keeps its id and
/// posts under a new owner; withdrawing that is exactly the harm this feature exists to prevent.
#[test]
fn gone_withdraws_only_the_owner_that_was_probed() {
    let db = db(true);
    apply(&db, needed("s1")).unwrap();

    let stale = db
        .write_txn("t", |c, m| gone(c, m, "s1", "9999", T0))
        .unwrap();
    assert!(!stale.moved);
    assert!(stale.refusal.is_some());
    assert_eq!(records(&db).len(), 1, "a changed record is left alone");
    assert_eq!(sent(&db).len(), 1);

    let out = db
        .write_txn("t", |c, m| gone(c, m, "s1", "4242", T0))
        .unwrap();
    assert!(out.moved, "{out:?}");
    assert_eq!(out.effects, [NotifEffect::Withdraw, NotifEffect::Forget]);
    assert!(records(&db).is_empty());
    assert_eq!(sent(&db)[1].kind, KIND_WITHDRAW);

    let again = db
        .write_txn("t", |c, m| gone(c, m, "s1", "4242", T0))
        .unwrap();
    assert!(!again.moved, "nothing left to withdraw");
    assert_eq!(sent(&db).len(), 2);
}

/// A record with no owner proves nothing, whatever the caller claims: `Unknown` refuses.
#[test]
fn gone_refuses_a_record_with_no_owner() {
    let db = db(true);
    apply(
        &db,
        Observation {
            owner: String::new(),
            ..needed("s1")
        },
    )
    .unwrap();
    let out = db.write_txn("t", |c, m| gone(c, m, "s1", "", T0)).unwrap();
    assert!(!out.moved, "{out:?}");
    assert_eq!(records(&db).len(), 1);
}
