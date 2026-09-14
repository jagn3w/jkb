//! The permission-notification lifecycle as a checkable table, run against the database (design N,
//! `openspec/changes/jkb-notification-lifecycle/`; moved here by r3.2 N1,
//! `openspec/changes/jkb-message-queue/design-r3.md`).
//!
//! Claude Code's "needs your permission" notification has to stay on screen and then come down
//! by itself once you have acted. That rule lived as conditionals spread across a shell hook,
//! and four review rounds produced 4, then 8, then 11, then 13 findings — the count rising each
//! time, because each round's fix was where the next round's defect lived. Nothing could be
//! *asked* of the rules: not "can this notification always come down", not "which events move
//! it", not "is there a state nothing exits". Every answer came from re-reading the script.
//!
//! Here the rules are a `&'static` table [`jkb_fsm`] can walk, and the questions above are its
//! [`jkb_fsm::Defect`] checks. The test asserting the table is defect-free is the artefact this
//! module exists for; without it this is the same conditionals in a new shape.
//!
//! **Where it runs (r3.2 N1).** The hook no longer performs anything. It sends what it observed —
//! [`observe`] — to the daemon that owns the database, and the machine runs there against its own
//! per-session record (the `notify_sessions` table). Effects on the **screen** become messages on
//! the [`TOPIC`] queue, written in the **same transaction** as the record, so the record can no
//! longer say one thing while the screen was told another; displaying them is the consumer's job
//! (`jkb-notifier serve` on macOS). What the machine lost in the move is everything about *whether
//! a notification can be displayed* — `Banner` and `notifier_usable` — because that is a fact about
//! the machine the consumer runs on, which a producer in a container cannot know.
//!
//! **A topic nobody reads is not written to.** With no consumer group, a message is never consumed,
//! so nothing is ever reapable and the topic fills to its cap and then refuses every hook call. On a
//! machine with no notifier — Linux, CI, a Mac that has not installed one — that is the whole
//! lifetime of the topic. So [`observe`] still moves the record but sends only when the topic has a
//! group, and a consumer that subscribes later starts from now, when nothing was on screen anyway.

use jkb_fsm::{
    require_no, require_yes, Denial, Dest, Event, EventKind, Fact, Machine, State, Stateful,
    Transition, Verdict,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use crate::mq::{self, Draft, QueueError};
use crate::{Result, WriteMeta};

/// The topic notifications are sent on. Created by `scripts/setup.sh`, never by a producer.
pub const TOPIC: &str = "claude/notify";

/// A post's time to live: 12 hours (decided by the user, 2026-09-13). An expired post is still
/// delivered, and shown marked stale with its age; the TTL only lets it be reaped once consumed.
pub const POST_TTL_MS: i64 = 12 * 60 * 60 * 1000;

/// What a consumer dispatches on: show this notification (replacing one with the same id).
pub const KIND_POST: &str = "notify.post";

/// ...and take it down. Carries no TTL: it is reaped once consumed, under cap pressure.
pub const KIND_WITHDRAW: &str = "notify.withdraw";

/// The longest body a post carries, in characters. A Claude Code notification is a sentence; this
/// bounds one that is not, so it is shortened rather than refused as too large — a refusal rolls
/// the whole transaction back and posts nothing at all.
pub const MAX_BODY_CHARS: usize = 1000;

/// The title every post carries.
pub const TITLE: &str = "Claude Code";

/// Where one session's notification stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotifState {
    /// Nothing withdrawable of ours is on screen. Initial, and the only resting state.
    Absent,
    /// Posted, and the prompt named the tool it is about.
    AwaitingTool,
    /// Posted, with no tool known — the idle prompt, or a message we could not parse.
    AwaitingUser,
}

impl NotifState {
    /// The state's name, as the table and the wire spell it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::AwaitingTool => "awaiting_tool",
            Self::AwaitingUser => "awaiting_user",
        }
    }
}

impl State for NotifState {
    const ALL: &'static [Self] = &[Self::Absent, Self::AwaitingTool, Self::AwaitingUser];

    fn name(self) -> &'static str {
        self.as_str()
    }

    /// Only `Absent`. A posted notification owes the system a withdrawal, which is the whole
    /// point: [`jkb_fsm::Defect::Wedged`] then insists every way of being posted has a way back,
    /// and the orphan left by a killed session — the defect a reviewer found by hand — is
    /// exactly a state with no such edge.
    fn is_settled(self) -> bool {
        matches!(self, Self::Absent)
    }

    /// Deliberately left `false` everywhere. A posted notification *is* waiting on a person, but
    /// the system can still move it — a tool finishes, the turn ends — so declaring it would make
    /// [`jkb_fsm::Defect::DeadEnd`] vacuous on the two states where it does the work.
    fn awaits_input(self) -> bool {
        false
    }
}

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotifEvent {
    /// Claude Code's `Notification` hook: permission is needed, or it is idle waiting for input.
    Needed,
    /// `PostToolUse`. Evidence about the prompt only when it names the same tool.
    ToolFinished,
    /// `UserPromptSubmit` — the user is demonstrably back.
    UserActed,
    /// `Stop`. The turn is over, so nothing of this turn's is still waiting.
    TurnEnded,
    /// `SessionEnd`.
    SessionEnded,
    /// Nobody asked: a producer looked, and the session that posted this is provably gone.
    SessionGone,
}

impl Event for NotifEvent {
    const ALL: &'static [Self] = &[
        Self::Needed,
        Self::ToolFinished,
        Self::UserActed,
        Self::TurnEnded,
        Self::SessionEnded,
        Self::SessionGone,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Needed => "needed",
            Self::ToolFinished => "tool_finished",
            Self::UserActed => "user_acted",
            Self::TurnEnded => "turn_ended",
            Self::SessionEnded => "session_ended",
            Self::SessionGone => "session_gone",
        }
    }

    /// `SessionGone` is the only thing here nobody asks for, and the crate then refuses to let it
    /// be declared without a guard — which is the rule an age-based liveness shortcut breaks.
    fn kind(self) -> EventKind {
        match self {
            Self::SessionGone => EventKind::Reconciled,
            _ => EventKind::Applied,
        }
    }
}

/// Something applying a move must do. Produced *with* the move, as one value, so half a
/// transition cannot be applied — clearing the record without withdrawing, or withdrawing
/// without clearing, are both real defects from this feature's history.
///
/// Each effect changes **the screen** (by sending a message) or **the record**, and within a plan
/// every screen effect comes before every record effect. Both now happen inside one transaction, so
/// a plan is applied whole or not at all; the ordering is kept because it is still the right answer
/// for any step that is not transactional, and `plans_change_the_screen_before_the_record` walks
/// every plan the table can produce and holds it to that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifEffect {
    /// Send a post for this session's notification.
    Post,
    /// Send a withdrawal of it.
    Withdraw,
    /// Record that a withdrawable notification is on screen, which tool it is about, and who owns it.
    Remember,
    /// Forget that record.
    Forget,
}

impl NotifEffect {
    /// The effect's name on the wire (`jkb-api`'s `notified` response).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Post => "post",
            Self::Withdraw => "withdraw",
            Self::Remember => "remember",
            Self::Forget => "forget",
        }
    }

    /// Whether this effect changes what the user can see, as opposed to what we remember.
    #[must_use]
    pub const fn touches_screen(self) -> bool {
        matches!(self, Self::Post | Self::Withdraw)
    }
}

/// What a guard reads. The state lives **in here** and is read out via [`Stateful`], never passed
/// beside it — the crate's own history records a machine whose guard branched on one state while
/// it was being asked about another.
pub struct NotifCtx {
    /// Read from the per-session record.
    pub at: NotifState,
    /// Did the prompt name a tool? Decides which of the two posted states we land in.
    pub tool_named: bool,
    /// Is the tool that just finished the one the prompt named?
    pub tool_matches: Fact,
    /// Is the session that posted this still alive? `No` only on positive evidence — never age.
    pub session_alive: Fact,
}

impl Stateful<NotifState> for NotifCtx {
    fn state(&self) -> NotifState {
        self.at
    }
}

/// Where a `Needed` lands, stated by the context rather than by the table.
///
/// Two destinations share one `(state, event)` pair — tool named or not — and two rows would be
/// [`jkb_fsm::Defect::Nondeterministic`]. It always answers; the `Option` is [`Dest::Stated`]'s
/// signature, not a possibility this function has — hence the narrow allow.
#[allow(clippy::unnecessary_wraps)]
fn needed_dest(c: &NotifCtx) -> Option<NotifState> {
    Some(if c.tool_named {
        NotifState::AwaitingTool
    } else {
        NotifState::AwaitingUser
    })
}

/// A `Needed` always posts and records (r3.2 N1). Whether it can be *displayed* — the old
/// `notifier_usable` fact and its `Banner` fallback — is decided by the consumer, which is the only
/// process that can ask the notification centre.
fn needed_plan(_: &NotifCtx) -> Vec<NotifEffect> {
    vec![NotifEffect::Post, NotifEffect::Remember]
}

/// Withdraw and forget, together, always.
fn clear_plan(_: &NotifCtx) -> Vec<NotifEffect> {
    vec![NotifEffect::Withdraw, NotifEffect::Forget]
}

/// The blind sweep: withdraw without believing anything is there.
fn sweep_plan(_: &NotifCtx) -> Vec<NotifEffect> {
    vec![NotifEffect::Withdraw]
}

/// A tool finishing is evidence about *this* prompt only when it is the same tool. One assistant
/// message routinely batches several calls, so a slow allowlisted one finishes while another's
/// prompt is still unanswered — withdrawing there leaves the session blocked with nothing on
/// screen, which is the state the feature exists to prevent.
fn tool_matches_guard(c: &NotifCtx) -> Verdict<NotifEvent> {
    require_yes(c.tool_matches, || {
        Denial::new("a different tool finished; this prompt is still waiting.")
    })
}

/// Liveness is by evidence, never by age (D27). `Unknown` refuses, so an orphan survives rather
/// than a paused-but-alive session losing the notification it is waiting on.
fn session_gone_guard(c: &NotifCtx) -> Verdict<NotifEvent> {
    require_no(c.session_alive, || {
        Denial::new("the session that posted this may still be alive.")
    })
}

const ROWS: &[Transition<NotifState, NotifEvent, NotifCtx, NotifEffect>] = &[
    // --- something needs attention -----------------------------------------------------------
    // Re-posting over a live notification is deliberate: the id is the session's, so a second
    // prompt replaces the first rather than stacking.
    Transition {
        from: NotifState::Absent,
        event: NotifEvent::Needed,
        to: Dest::Stated(needed_dest),
        guard: None,
        plan: Some(needed_plan),
    },
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::Needed,
        to: Dest::Stated(needed_dest),
        guard: None,
        plan: Some(needed_plan),
    },
    Transition {
        from: NotifState::AwaitingUser,
        event: NotifEvent::Needed,
        to: Dest::Stated(needed_dest),
        guard: None,
        plan: Some(needed_plan),
    },
    // --- the user dealt with it --------------------------------------------------------------
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::ToolFinished,
        to: Dest::To(NotifState::Absent),
        guard: Some(tool_matches_guard),
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::UserActed,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingUser,
        event: NotifEvent::UserActed,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    // --- the turn or session ended -----------------------------------------------------------
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::TurnEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingUser,
        event: NotifEvent::TurnEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::SessionEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingUser,
        event: NotifEvent::SessionEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    // The blind sweeps. `Absent` can be WRONG about the screen. The record and the sends are one
    // transaction now, so they cannot disagree with each other — but the screen is a consumer's,
    // across a queue, and a consumer whose withdraw failed (a notification centre that timed out)
    // has already acked it. These carry a plan, so the destination does not absorb them, and one
    // send per turn bounds that failure to the turn.
    Transition {
        from: NotifState::Absent,
        event: NotifEvent::TurnEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(sweep_plan),
    },
    Transition {
        from: NotifState::Absent,
        event: NotifEvent::SessionEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(sweep_plan),
    },
    // Nothing pending. These are the two most frequent events in the whole system — a tool
    // finishes after every single call — and `check()` named them `Unrepeatable` before this
    // ever ran: withdraw once and the next `PostToolUse` had no row at all. Declared with no
    // guard and no plan, so the destination absorbs them and they cost nothing, which is exactly
    // what "there is no notification, carry on" should cost.
    Transition {
        from: NotifState::Absent,
        event: NotifEvent::ToolFinished,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: None,
    },
    Transition {
        from: NotifState::Absent,
        event: NotifEvent::UserActed,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: None,
    },
    // --- the world moved -------------------------------------------------------------------
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::SessionGone,
        to: Dest::To(NotifState::Absent),
        guard: Some(session_gone_guard),
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingUser,
        event: NotifEvent::SessionGone,
        to: Dest::To(NotifState::Absent),
        guard: Some(session_gone_guard),
        plan: Some(clear_plan),
    },
];

/// The lifecycle.
#[must_use]
pub fn machine() -> Machine<NotifState, NotifEvent, NotifCtx, NotifEffect> {
    Machine {
        transitions: ROWS,
        initial: NotifState::Absent,
    }
}

/// Reduce a session id to characters inert in a notification id, a queue key and a filename.
#[must_use]
pub fn sanitize(session: &str) -> String {
    session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The notification id for a session — the session's, so parallel worktree sessions cannot clear
/// each other's, and a second prompt replaces the first rather than stacking.
#[must_use]
pub fn notification_id(session: &str) -> String {
    format!("jkb-claude-{session}")
}

/// The tool a permission prompt is about, read out of the message text because the payload names
/// none. A failure here is not an error: it lands the notification in `AwaitingUser`, which is a
/// declared state with declared behaviour rather than a fallen-through branch.
#[must_use]
pub fn tool_in(message: &str) -> Option<String> {
    let rest = message.split("permission to use ").nth(1)?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// What a producer observed: one Claude Code hook event, as the hook read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The session, already [`sanitize`]d — anything else is refused, never silently rewritten, so
    /// two producers cannot address one session by two spellings.
    pub session: String,
    /// What happened. Never [`NotifEvent::SessionGone`], which only [`gone`] may assert.
    pub event: NotifEvent,
    /// The tool that just finished (`PostToolUse`'s `tool_name`), or empty.
    pub finished_tool: String,
    /// The notification text (`Notification`'s `message`), or empty.
    pub message: String,
    /// The session's working directory; its last component becomes the subtitle.
    pub cwd: String,
    /// The `claude` process's pid as the hook saw it, or empty when it had none it could trust.
    pub owner: String,
    /// The pid namespace `owner` belongs to (see `jkb notify`'s sweep).
    pub instance: String,
}

/// What applying an event did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The state afterwards.
    pub state: NotifState,
    /// Whether a row of the table fired.
    pub moved: bool,
    /// The plan that was carried out.
    pub effects: Vec<NotifEffect>,
    /// Why nothing moved, when a guard refused.
    pub refusal: Option<String>,
    /// Messages actually sent: fewer than the plan's screen effects when the topic has no group.
    pub sent: usize,
}

/// A notification on screen, as the record holds it — what the sweep is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// The session.
    pub session: String,
    /// The tool the prompt named, or empty.
    pub tool: String,
    /// The owner pid the hook recorded, or empty.
    pub owner: String,
    /// The pid namespace that pid belongs to.
    pub instance: String,
    /// When the record was last written (Unix ms).
    pub updated_at: i64,
}

const MAX_SESSION_BYTES: usize = 200;
const MAX_INSTANCE_BYTES: usize = 150;
const MAX_OWNER_BYTES: usize = 20;

fn invalid(what: &'static str, why: impl Into<String>) -> crate::Error {
    QueueError::Invalid {
        what,
        why: why.into(),
    }
    .into()
}

fn check_session(session: &str) -> Result<()> {
    if session.is_empty() || session.len() > MAX_SESSION_BYTES {
        return Err(invalid(
            "session",
            format!("must be 1..={MAX_SESSION_BYTES} bytes"),
        ));
    }
    if sanitize(session) != session {
        return Err(invalid(
            "session",
            format!("{session:?} has characters other than [A-Za-z0-9_-]"),
        ));
    }
    Ok(())
}

fn check_owner_and_instance(owner: &str, instance: &str) -> Result<()> {
    if owner.len() > MAX_OWNER_BYTES || !owner.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid("owner", format!("{owner:?} is not a pid")));
    }
    if instance.len() > MAX_INSTANCE_BYTES || instance.chars().any(char::is_control) {
        return Err(invalid(
            "instance",
            format!("must be at most {MAX_INSTANCE_BYTES} bytes with no control characters"),
        ));
    }
    Ok(())
}

fn record(conn: &Connection, session: &str) -> Result<Option<SessionRecord>> {
    Ok(conn
        .prepare_cached(
            "SELECT session, tool, owner, instance, updated_at FROM notify_sessions \
             WHERE session = ?1",
        )?
        .query_row([session], row_to_record)
        .optional()?)
}

fn row_to_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        session: r.get(0)?,
        tool: r.get(1)?,
        owner: r.get(2)?,
        instance: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

fn state_of(rec: Option<&SessionRecord>) -> NotifState {
    match rec {
        None => NotifState::Absent,
        Some(r) if r.tool.is_empty() => NotifState::AwaitingUser,
        Some(_) => NotifState::AwaitingTool,
    }
}

/// What the effects of one plan are about: the post's text and the record to write.
struct Target<'a> {
    session: &'a str,
    tool: String,
    subtitle: &'a str,
    body: String,
    owner: &'a str,
    instance: &'a str,
}

/// Carry out a plan inside the caller's transaction. An error rolls the whole transaction back, so
/// a plan is applied entirely or not at all.
fn perform(
    conn: &Connection,
    meta: &WriteMeta,
    target: &Target<'_>,
    effects: &[NotifEffect],
    now: i64,
) -> Result<usize> {
    let mut sent = 0;
    // Asked once per plan, and before anything is written: a missing topic is an error (setup did
    // not run) and refuses the whole event, record included.
    let heard = effects.iter().any(|e| e.touches_screen()) && mq::group_count(conn, TOPIC)? > 0;
    let id = notification_id(target.session);
    for effect in effects {
        match effect {
            NotifEffect::Post | NotifEffect::Withdraw if !heard => {}
            NotifEffect::Post => {
                send(
                    conn,
                    meta,
                    target,
                    KIND_POST,
                    json!({
                        "id": id,
                        "session": target.session,
                        "title": TITLE,
                        "subtitle": target.subtitle,
                        "body": target.body,
                    }),
                    Some(POST_TTL_MS),
                    now,
                )?;
                sent += 1;
            }
            NotifEffect::Withdraw => {
                send(
                    conn,
                    meta,
                    target,
                    KIND_WITHDRAW,
                    json!({ "id": id, "session": target.session }),
                    None,
                    now,
                )?;
                sent += 1;
            }
            NotifEffect::Remember => {
                conn.prepare_cached(
                    "INSERT INTO notify_sessions (session, tool, owner, instance, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(session) DO UPDATE SET tool = excluded.tool, \
                         owner = excluded.owner, instance = excluded.instance, \
                         updated_at = excluded.updated_at",
                )?
                .execute(params![
                    target.session,
                    target.tool,
                    target.owner,
                    target.instance,
                    now
                ])?;
            }
            NotifEffect::Forget => {
                conn.prepare_cached("DELETE FROM notify_sessions WHERE session = ?1")?
                    .execute([target.session])?;
            }
        }
    }
    Ok(sent)
}

fn send(
    conn: &Connection,
    meta: &WriteMeta,
    target: &Target<'_>,
    kind: &str,
    payload: serde_json::Value,
    ttl_ms: Option<i64>,
    now: i64,
) -> Result<i64> {
    mq::send(
        conn,
        meta,
        TOPIC,
        &Draft {
            key: format!("session/{}", target.session),
            kind: kind.to_owned(),
            payload,
            ttl_ms,
            // Within `mq`'s producer limit: the instance is bounded well below it.
            producer: format!("notify@{}", target.instance),
        },
        now,
    )
}

fn outcome_to_applied(
    out: &jkb_fsm::Outcome<NotifState, NotifEvent, NotifEffect>,
    sent: usize,
) -> Applied {
    Applied {
        state: out.state(),
        moved: out.moved(),
        effects: out.effects().to_vec(),
        refusal: out.refusal(),
        sent,
    }
}

/// Apply one observed hook event: read the session's record, run the machine, and carry out its
/// plan — sends and record together, in the caller's transaction.
///
/// # Errors
/// [`QueueError::Invalid`] for an unsanitized session, a malformed owner or instance, or a
/// `SessionGone` (only [`gone`] asserts that); [`QueueError::NoSuchTopic`] when [`TOPIC`] was never
/// created; a send's own refusal; or a database error.
pub fn observe(
    conn: &Connection,
    meta: &WriteMeta,
    obs: &Observation,
    now: i64,
) -> Result<Applied> {
    check_session(&obs.session)?;
    check_owner_and_instance(&obs.owner, &obs.instance)?;
    if obs.event == NotifEvent::SessionGone {
        return Err(invalid(
            "event",
            "session_gone is asserted by notify.gone, with the owner it probed",
        ));
    }
    let rec = record(conn, &obs.session)?;
    let prompted = tool_in(&obs.message);
    let ctx = NotifCtx {
        at: state_of(rec.as_ref()),
        tool_named: prompted.is_some(),
        tool_matches: match rec.as_ref() {
            Some(r) if !r.tool.is_empty() && !obs.finished_tool.is_empty() => {
                Fact::from(r.tool == obs.finished_tool)
            }
            _ => Fact::Unknown,
        },
        // One live session is being observed here; proving another one dead is `gone`'s job.
        session_alive: Fact::Unknown,
    };
    let out = machine().apply(&ctx, obs.event);
    let target = Target {
        session: &obs.session,
        tool: prompted.unwrap_or_default(),
        subtitle: obs.cwd.rsplit('/').next().unwrap_or_default(),
        body: obs.message.chars().take(MAX_BODY_CHARS).collect(),
        owner: &obs.owner,
        instance: &obs.instance,
    };
    let sent = perform(conn, meta, &target, out.effects(), now)?;
    Ok(outcome_to_applied(&out, sent))
}

/// Every notification on screen, by session — what a producer's `SessionStart` sweep probes.
///
/// # Errors
/// A database error.
pub fn open_sessions(conn: &Connection) -> Result<Vec<SessionRecord>> {
    Ok(conn
        .prepare_cached(
            "SELECT session, tool, owner, instance, updated_at FROM notify_sessions \
             ORDER BY session",
        )?
        .query_map([], row_to_record)?
        .collect::<rusqlite::Result<_>>()?)
}

/// A producer has proved `session` gone: withdraw its notification and forget it.
///
/// **Only if the record still names `owner`** — the pid the producer probed. Between reading
/// [`open_sessions`] and calling this, the session can be resumed (`claude --resume` keeps the id
/// and runs a new process) and post again; withdrawing then would take a live prompt off the
/// screen, which is the harm this feature exists to prevent. A record that changed, or is gone,
/// is left alone and reported as not moved.
///
/// # Errors
/// [`QueueError::Invalid`] for an unsanitized session or malformed owner; a send's refusal; or a
/// database error.
pub fn gone(
    conn: &Connection,
    meta: &WriteMeta,
    session: &str,
    owner: &str,
    now: i64,
) -> Result<Applied> {
    check_session(session)?;
    check_owner_and_instance(owner, "")?;
    let rec = record(conn, session)?;
    let still = rec.as_ref().filter(|r| r.owner == owner);
    let ctx = NotifCtx {
        at: state_of(still),
        tool_named: still.is_some_and(|r| !r.tool.is_empty()),
        tool_matches: Fact::Unknown,
        // The producer's probe, asserted by the call. An empty owner proves nothing.
        session_alive: if still.is_some() && !owner.is_empty() {
            Fact::No
        } else {
            Fact::Unknown
        },
    };
    let Some(still) = still else {
        return Ok(Applied {
            state: state_of(rec.as_ref()),
            moved: false,
            effects: Vec::new(),
            refusal: Some("the record changed since it was probed, or is gone.".to_owned()),
            sent: 0,
        });
    };
    let out = machine().apply(&ctx, NotifEvent::SessionGone);
    let target = Target {
        session,
        tool: still.tool.clone(),
        subtitle: "",
        body: String::new(),
        owner,
        instance: &still.instance,
    };
    let sent = perform(conn, meta, &target, out.effects(), now)?;
    Ok(outcome_to_applied(&out, sent))
}

#[cfg(test)]
mod tests;
