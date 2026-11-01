//! Run requests: a session blocked by its own permissions asks the operator to run one command for
//! it, on the host or in the dev container outside the agent's sandbox (the Executions design).
//!
//! **What the operator approves is exactly what runs.** A request is a typed record — `argv` as a
//! list run without a shell, the requester's real directory, the target, and a **tree pin** of the
//! directory's repository (a blake3 of its work tree, labelled with its `HEAD`). Nothing edits a
//! request after it is made: the operator runs it, declines it with a note, or it is refused or
//! expires, and a session that wants something else asks again.
//!
//! **Every rule lives here, in the callee.** This module is the only writer of `exec_requests`, and
//! each state move is one compare-and-set `UPDATE … WHERE state = …` — so "runs at most once" is a
//! schema fact, not a promise each caller keeps:
//!
//! | from → to | function |
//! |---|---|
//! | (new) → `pending` | [`request`]; an identical pending request from the same session folds into the existing one |
//! | `pending` → `running` | [`begin`], which refuses the row (`tree_changed`) instead when the pin differs |
//! | `pending` → `declined` | [`decline`] |
//! | `pending` → `refused` | [`refuse`] |
//! | `pending` → `expired` | every function here, lazily, once `expires_at` has passed; [`expire_session`] when the requester's session ended |
//! | `running` → `finished` | [`finish`], idempotent for identical values, refused to another runner |
//! | `running` → `finished` (`interrupted`) | every function here, lazily, once `started_at + timeout + `[`RUN_GRACE_MS`] has passed with no end reported — provisional: the recorded runner's own late [`output`] is accepted and its [`finish`] replaces the outcome ([`SETTLED_INTERRUPT`]) |
//!
//! **Who may call which** is `jkb-api`'s: [`request`] is any non-operator session's, the moves out of
//! `pending` and the output are the operator's. The requester is the server-side principal, never a
//! request field.
//!
//! **The topic is a hint.** Each request has a message-queue topic `exec/<id>` (created with it) on
//! which this module — and only this module ([`mq::RESERVED_PREFIXES`]) — publishes `exec.decision`,
//! `exec.output` and `exec.end`, and which only [`follow`] reads. A waiter always confirms the
//! terminal state with a read of the row. A message that cannot be published because the topic is
//! full is dropped rather than failing what it announces — an output chunk too: chunks are numbered
//! whether published or not, so a reader sees the gap, and the row's tail and the runner's log are
//! the record.
//!
//! **Audit.** `exec_requests` is changelogged; `jkb undo` refuses every entry of it by name
//! ([`crate::changelog`]'s `InsertInverse::Irreversible`, and no `update` inverse): undo cannot
//! un-run a command or un-ask a request. `exec_output` is not changelogged, like `blobs`.

use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::changelog::{self, Entity};
use crate::mq::{self, Draft, QueueError, TopicSpec};
use crate::{Result, WriteMeta};
use jkb_types::ItemId;

/// The wire form of a request id is `exec-<id>`.
pub const ID_PREFIX: &str = "exec-";

/// A request's topic is `exec/<id>`.
pub const TOPIC_PREFIX: &str = "exec/";

/// The largest `argv`, in bytes of its JSON text.
pub const MAX_ARGV_BYTES: usize = 64 * 1024;

/// The longest reason, in characters.
pub const MAX_REASON_CHARS: usize = 500;

/// The longest operator note, in characters.
pub const MAX_NOTE_CHARS: usize = 2000;

/// The longest directory or log path, in bytes.
pub const MAX_PATH_BYTES: usize = 4096;

/// The longest session, agent type, role or runner, in bytes.
pub const MAX_NAME_BYTES: usize = 256;

/// The most output a row keeps: the last 256 KiB. The full output goes to the log at `log_path`.
pub const MAX_TAIL_BYTES: usize = 256 * 1024;

/// The largest output chunk one [`output`] call publishes. Its base64 must fit a queue payload
/// ([`mq::MAX_PAYLOAD_BYTES`], 64 KiB) with the JSON around it: 47 KiB encodes to 64,172 bytes.
pub const MAX_CHUNK_BYTES: usize = 47 * 1024;

/// A pending request expires when it has gone unanswered this long: two hours (operator,
/// 2026-10-16).
pub const EXPIRES_AFTER_MS: i64 = 2 * 60 * 60 * 1000;

/// A run's timeout when the request names none: 15 minutes (operator, 2026-10-16).
pub const DEFAULT_TIMEOUT_MS: i64 = 15 * 60 * 1000;

/// The longest timeout a request may ask for: two hours (operator, 2026-10-16).
pub const MAX_TIMEOUT_MS: i64 = 2 * 60 * 60 * 1000;

/// How long past its timeout a running request may go without its runner reporting an end before
/// [`settle`] ends it as [`Outcome::Interrupted`]: the callee bounds a run, not the runner's survival.
/// A runner that crashed mid-run would otherwise leave the row `running` for ever, and the session
/// waiting on it with it.
pub const RUN_GRACE_MS: i64 = 5 * 60 * 1000;

/// The note [`settle`] writes when it ends a quiet run as interrupted — and how [`finish`] and
/// [`output`] recognise that ending as provisional. Wall-clock age is all `settle` has: a runner
/// merely paused (a host asleep with its lid closed) is not dead, and its own report, when it comes,
/// is the truth.
pub const SETTLED_INTERRUPT: &str =
    "interrupted: the runner never reported an end, past the timeout and its grace";

/// The latest a request can still be answered, measured from its creation: unanswered past
/// [`EXPIRES_AFTER_MS`] it expires, and begun at the last moment it ends within the longest timeout
/// and its grace. A waiter that has heard nothing by then has lost its way to the record.
pub const LATEST_END_MS: i64 = EXPIRES_AFTER_MS + MAX_TIMEOUT_MS + RUN_GRACE_MS;

/// The shortest timeout a request may ask for.
pub const MIN_TIMEOUT_MS: i64 = 1000;

/// The most rows one [`list`] page returns.
pub const PAGE: usize = 100;

/// About the most bytes of argv, directory, reason and note one [`list`] page carries: a page ends
/// at the first row past it, so one row of the largest argv still fits a `jkb serve` answer (1 MiB)
/// and a page of them does not grow to [`PAGE`] times that.
pub const PAGE_BYTES: usize = 512 * 1024;

/// A request topic's byte cap.
const TOPIC_MAX_BYTES: i64 = 8 * 1024 * 1024;

/// A request topic's message TTL: a day.
const TOPIC_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// The index topic: one `exec.changed {id, state}` hint per request made and per move, so the
/// operator's app re-reads the queue when something changed rather than polling it. Reserved like a
/// request's own topic ([`mq::RESERVED_PREFIXES`]): only this module sends on it, and the only
/// consumer groups on it are the operator's, through [`index_group_create`] and its siblings, which
/// `jkb-api` serves to the operator alone. A hint carries an id and a state, never argv or a reason:
/// the record is read for those.
pub const INDEX_TOPIC: &str = "exec/requests";

/// The kind of every message on [`INDEX_TOPIC`].
pub const INDEX_KIND: &str = "exec.changed";

/// The index topic's byte cap. A hint is under a hundred bytes.
const INDEX_MAX_BYTES: i64 = 4 * 1024 * 1024;

/// A request's notification id on `claude/notify` ([`crate::notify`]): `exec-<id>`, its wire id.
#[must_use]
pub fn notification_id(id: i64) -> String {
    wire_id(id)
}

macro_rules! word_enum {
    ($(#[$doc:meta])* $name:ident { $( $(#[$vdoc:meta])* $variant:ident => $s:literal ),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$vdoc])* $variant, )+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [Self] = &[ $( Self::$variant, )+ ];

            /// The stored (and wire) spelling.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $s, )+
                }
            }

            /// The value `s` spells, if any.
            #[must_use]
            pub fn parse(s: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|v| v.as_str() == s)
            }
        }
    };
}

word_enum! {
    /// Where a request runs, or which side's path a directory is.
    ExecTarget {
        /// On the host.
        Host => "host",
        /// In the dev container, through `docker exec`, outside the agent's sandbox.
        Container => "container",
    }
}

word_enum! {
    /// A request's state.
    ExecState {
        /// Asked, not yet answered.
        Pending => "pending",
        /// The operator ran it; it has not ended.
        Running => "running",
        /// It ran and ended ([`Outcome`]).
        Finished => "finished",
        /// The operator declined it, with a note for the session.
        Declined => "declined",
        /// It could not run as approved ([`RefuseReason`]).
        Refused => "refused",
        /// Nobody answered it in time, or the requesting session ended.
        Expired => "expired",
    }
}

impl ExecState {
    /// Whether nothing more happens to a request in this state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }
}

word_enum! {
    /// Why a request was refused.
    RefuseReason {
        /// The tree pin recomputed at Run differs from the one the operator reviewed.
        TreeChanged => "tree_changed",
        /// The hooks-off git audit refused the repository.
        AuditRefused => "audit_refused",
        /// The directory is gone.
        DirGone => "dir_gone",
    }
}

word_enum! {
    /// How a run ended.
    Outcome {
        /// The program exited with a status.
        Exited => "exited",
        /// A signal ended it; `exit_status` is the signal number.
        Signalled => "signalled",
        /// It ran past its timeout and was stopped.
        TimedOut => "timed_out",
        /// The runner stopped it (the app quit, the operator stopped it).
        Interrupted => "interrupted",
    }
}

/// A repository's work tree when the request was made: the blake3 over every path its `.gitignore`
/// files do not cover, and its `HEAD` as a label (the framing, v3, is `jkb-cli`'s
/// `gitrepo::pin_tree`, which trusts nothing in `.git`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreePin {
    /// The `HEAD` commit, hex — a label for the operator; the digest does not depend on it.
    pub head: String,
    /// The blake3 of the pinned work tree, 64 hex digits.
    pub changes: String,
}

/// Who asked — taken by `jkb-api` from the server-side principal, never from the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requester {
    /// The Claude Code session.
    pub session: String,
    /// The subagent type, when a subagent asked.
    pub agent_type: Option<String>,
    /// The role it holds.
    pub role: String,
    /// The task it holds, if it is bound to one.
    pub task: Option<ItemId>,
}

/// What is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    /// Where it runs.
    pub target: ExecTarget,
    /// The directory, as the requester resolved it (its real path, absolute).
    pub dir: String,
    /// Which side's path `dir` is.
    pub dir_side: ExecTarget,
    /// The program and its arguments, run without a shell.
    pub argv: Vec<String>,
    /// One sentence for the operator.
    pub reason: String,
    /// The repository's pin; `None` for a directory outside any repository.
    pub pin: Option<TreePin>,
    /// The run's timeout; [`DEFAULT_TIMEOUT_MS`] when `None`.
    pub timeout_ms: Option<i64>,
}

/// One request, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRow {
    /// The row id: the wire id is `exec-<id>`.
    pub id: i64,
    /// When it was asked (Unix ms).
    pub created_at: i64,
    /// When it expires if still pending (Unix ms).
    pub expires_at: i64,
    /// Who asked.
    pub requester: Requester,
    /// What was asked.
    pub ask: Ask,
    /// Its state.
    pub state: ExecState,
    /// Why it was refused, when it was.
    pub refuse_reason: Option<RefuseReason>,
    /// The operator's note (a decline), or why it expired.
    pub note: Option<String>,
    /// Who ran it.
    pub runner: Option<String>,
    /// When it started running.
    pub started_at: Option<i64>,
    /// When it left `pending` or `running` for good.
    pub ended_at: Option<i64>,
    /// The exit status, or the signal number for [`Outcome::Signalled`].
    pub exit_status: Option<i64>,
    /// How the run ended.
    pub outcome: Option<Outcome>,
}

impl ExecRow {
    /// The wire id, `exec-<id>`.
    #[must_use]
    pub fn wire_id(&self) -> String {
        wire_id(self.id)
    }
}

/// The wire id of row `id`.
#[must_use]
pub fn wire_id(id: i64) -> String {
    format!("{ID_PREFIX}{id}")
}

/// The row id a wire id names: `exec-<id>`, or a bare number.
///
/// # Errors
/// A validation error for anything else.
pub fn parse_id(s: &str) -> Result<i64> {
    let digits = s.strip_prefix(ID_PREFIX).unwrap_or(s);
    digits
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0 && digits.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| validation(format!("{s:?} is not a run request id (`exec-<n>`)")))
}

/// A request's topic.
#[must_use]
pub fn topic(id: i64) -> String {
    format!("{TOPIC_PREFIX}{id}")
}

/// What [`request`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requested {
    /// The request — the existing one when folded.
    pub row: ExecRow,
    /// An identical pending request from the same session already existed, and this is it.
    pub folded: bool,
}

/// What [`begin`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begun {
    /// It is running now, under the runner given.
    Running(ExecRow),
    /// The pin differed: it is refused (`tree_changed`) and will never run.
    Refused(ExecRow),
    /// It had gone unanswered past its expiry, and expired in this call.
    Expired(ExecRow),
}

/// What [`finish`] stores of the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished<'a> {
    /// Who ran it — must be the runner [`begin`] recorded.
    pub runner: &'a str,
    /// How it ended.
    pub outcome: Outcome,
    /// The exit status (required for [`Outcome::Exited`]) or signal (for [`Outcome::Signalled`]).
    pub exit_status: Option<i64>,
    /// The last [`MAX_TAIL_BYTES`] at most.
    pub tail: &'a [u8],
    /// Every byte the run produced.
    pub total_bytes: i64,
    /// Where the full output is.
    pub log_path: Option<&'a str>,
}

/// A request's stored output.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    /// The last bytes, at most [`MAX_TAIL_BYTES`].
    pub tail: Vec<u8>,
    /// Every byte the run produced.
    pub total_bytes: i64,
    /// Whether `tail` is less than all of it.
    pub truncated: bool,
    /// How many chunks were published.
    pub chunks: i64,
    /// Where the full output is.
    pub log_path: Option<String>,
}

/// Which rows [`list`] returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Filter<'a> {
    /// Only rows in this state.
    pub state: Option<ExecState>,
    /// Only this session's rows.
    pub session: Option<&'a str>,
}

fn validation(why: impl Into<String>) -> crate::Error {
    jkb_types::Error::Validation(why.into()).into()
}

fn stale(why: impl Into<String>) -> crate::Error {
    crate::Error::Stale(why.into())
}

fn check_text(what: &str, value: &str, max_bytes: usize, empty_ok: bool) -> Result<()> {
    if !empty_ok && value.is_empty() {
        return Err(validation(format!("{what} is empty")));
    }
    if value.len() > max_bytes {
        return Err(validation(format!(
            "{what} is {} bytes; the limit is {max_bytes}",
            value.len()
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(validation(format!("{what} contains a control character")));
    }
    Ok(())
}

fn check_hex(what: &str, value: &str, lens: &[usize]) -> Result<()> {
    if !lens.contains(&value.len())
        || !value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(validation(format!(
            "{what} must be lower-case hex of length {lens:?}"
        )));
    }
    Ok(())
}

impl Requester {
    fn check(&self) -> Result<()> {
        check_text("session", &self.session, MAX_NAME_BYTES, false)?;
        if let Some(t) = &self.agent_type {
            check_text("agent type", t, MAX_NAME_BYTES, false)?;
        }
        check_text("role", &self.role, MAX_NAME_BYTES, false)
    }
}

impl Ask {
    /// The argv as stored: its JSON text, after every argv rule.
    fn argv_json(&self) -> Result<String> {
        let Some(program) = self.argv.first() else {
            return Err(validation("argv is empty: name the program to run"));
        };
        if program.is_empty() {
            return Err(validation("argv[0] is empty"));
        }
        // An option as the program is a request to run whatever the runner's own launcher makes of
        // it — never a program the operator can read.
        if program.starts_with('-') {
            return Err(validation(format!(
                "argv[0] {program:?} is an option, not a program"
            )));
        }
        if self.argv.iter().any(|a| a.contains('\0')) {
            return Err(validation("an argument contains a NUL byte"));
        }
        let text = serde_json::to_string(&self.argv)
            .map_err(|e| validation(format!("argv does not serialize: {e}")))?;
        if text.len() > MAX_ARGV_BYTES {
            return Err(validation(format!(
                "argv is {} bytes of JSON; the limit is {MAX_ARGV_BYTES}",
                text.len()
            )));
        }
        Ok(text)
    }

    fn check(&self) -> Result<String> {
        let argv = self.argv_json()?;
        check_text("dir", &self.dir, MAX_PATH_BYTES, false)?;
        if !self.dir.starts_with('/') {
            return Err(validation(format!("dir {:?} is not absolute", self.dir)));
        }
        if self.reason.chars().count() > MAX_REASON_CHARS {
            return Err(validation(format!(
                "the reason is longer than {MAX_REASON_CHARS} characters"
            )));
        }
        check_text("reason", &self.reason, usize::MAX, true)?;
        if let Some(pin) = &self.pin {
            check_hex("pin head", &pin.head, &[40, 64])?;
            check_hex("pin changes", &pin.changes, &[64])?;
        }
        if let Some(t) = self.timeout_ms {
            if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&t) {
                return Err(validation(format!(
                    "a timeout of {t} ms is outside {MIN_TIMEOUT_MS}..={MAX_TIMEOUT_MS} ms (two hours \
                     at most)"
                )));
            }
        }
        Ok(argv)
    }
}

const COLUMNS: &str =
    "id, created_at, expires_at, session, agent_type, role, task_id, target, dir, \
                       dir_side, argv, reason, pin_head, pin_changes, timeout_ms, state, \
                       refuse_reason, note, runner, started_at, ended_at, exit_status, outcome";

fn corrupt(id: i64, what: &str, value: &str) -> crate::Error {
    QueueError::Corrupt(format!(
        "exec request {id} has {what} {value:?}, which this jkb does not know"
    ))
    .into()
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        id: r.get(0)?,
        created_at: r.get(1)?,
        expires_at: r.get(2)?,
        session: r.get(3)?,
        agent_type: r.get(4)?,
        role: r.get(5)?,
        task_id: r.get(6)?,
        target: r.get(7)?,
        dir: r.get(8)?,
        dir_side: r.get(9)?,
        argv: r.get(10)?,
        reason: r.get(11)?,
        pin_head: r.get(12)?,
        pin_changes: r.get(13)?,
        timeout_ms: r.get(14)?,
        state: r.get(15)?,
        refuse_reason: r.get(16)?,
        note: r.get(17)?,
        runner: r.get(18)?,
        started_at: r.get(19)?,
        ended_at: r.get(20)?,
        exit_status: r.get(21)?,
        outcome: r.get(22)?,
    })
}

/// A row as `SQLite` hands it back, before its words are parsed.
struct RawRow {
    id: i64,
    created_at: i64,
    expires_at: i64,
    session: String,
    agent_type: Option<String>,
    role: String,
    task_id: Option<i64>,
    target: String,
    dir: String,
    dir_side: String,
    argv: String,
    reason: String,
    pin_head: Option<String>,
    pin_changes: Option<String>,
    timeout_ms: i64,
    state: String,
    refuse_reason: Option<String>,
    note: Option<String>,
    runner: Option<String>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    exit_status: Option<i64>,
    outcome: Option<String>,
}

impl RawRow {
    fn parse(self) -> Result<ExecRow> {
        let id = self.id;
        let target =
            ExecTarget::parse(&self.target).ok_or_else(|| corrupt(id, "target", &self.target))?;
        let dir_side = ExecTarget::parse(&self.dir_side)
            .ok_or_else(|| corrupt(id, "dir_side", &self.dir_side))?;
        let state =
            ExecState::parse(&self.state).ok_or_else(|| corrupt(id, "state", &self.state))?;
        let refuse_reason = self
            .refuse_reason
            .as_deref()
            .map(|s| RefuseReason::parse(s).ok_or_else(|| corrupt(id, "refuse_reason", s)))
            .transpose()?;
        let outcome = self
            .outcome
            .as_deref()
            .map(|s| Outcome::parse(s).ok_or_else(|| corrupt(id, "outcome", s)))
            .transpose()?;
        let argv: Vec<String> =
            serde_json::from_str(&self.argv).map_err(|_| corrupt(id, "argv", &self.argv))?;
        let pin = match (self.pin_head, self.pin_changes) {
            (Some(head), Some(changes)) => Some(TreePin { head, changes }),
            _ => None,
        };
        Ok(ExecRow {
            id,
            created_at: self.created_at,
            expires_at: self.expires_at,
            requester: Requester {
                session: self.session,
                agent_type: self.agent_type,
                role: self.role,
                task: self.task_id.map(ItemId::new),
            },
            ask: Ask {
                target,
                dir: self.dir,
                dir_side,
                argv,
                reason: self.reason,
                pin,
                timeout_ms: Some(self.timeout_ms),
            },
            state,
            refuse_reason,
            note: self.note,
            runner: self.runner,
            started_at: self.started_at,
            ended_at: self.ended_at,
            exit_status: self.exit_status,
            outcome,
        })
    }
}

/// Request `id`, as stored — a plain read, which expires nothing.
///
/// # Errors
/// A database error, or a row this jkb cannot read.
pub fn get(conn: &Connection, id: i64) -> Result<Option<ExecRow>> {
    conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM exec_requests WHERE id = ?1"
    ))?
    .query_row([id], row_from)
    .optional()?
    .map(RawRow::parse)
    .transpose()
}

fn must_get(conn: &Connection, id: i64) -> Result<ExecRow> {
    get(conn, id)?
        .ok_or_else(|| jkb_types::Error::NotFound(format!("no run request {}", wire_id(id))).into())
}

/// The row as one JSON object of its columns — the changelog's before and after state.
fn row_json(conn: &Connection, id: i64) -> Result<Value> {
    let text: String = conn
        .prepare_cached(
            "SELECT json_object('id', id, 'created_at', created_at, 'expires_at', expires_at, \
                    'session', session, 'agent_type', agent_type, 'role', role, 'task_id', task_id, \
                    'target', target, 'dir', dir, 'dir_side', dir_side, 'argv', argv, \
                    'reason', reason, 'pin_head', pin_head, 'pin_changes', pin_changes, \
                    'timeout_ms', timeout_ms, 'state', state, 'refuse_reason', refuse_reason, \
                    'note', note, 'runner', runner, 'started_at', started_at, 'ended_at', ended_at, \
                    'exit_status', exit_status, 'outcome', outcome) \
             FROM exec_requests WHERE id = ?1",
        )?
        .query_row([id], |r| r.get(0))?;
    serde_json::from_str(&text).map_err(|e| validation(format!("exec row {id} as JSON: {e}")))
}

/// Publish on request `id`'s topic. `must` says whether a full topic fails the caller: an output
/// chunk does, a decision or an end does not — the state move it announces is the fact, the message
/// only a hint, and a waiter confirms with a read.
fn publish(
    conn: &Connection,
    meta: &WriteMeta,
    id: i64,
    kind: &str,
    payload: Value,
    now: i64,
    must: bool,
) -> Result<Option<i64>> {
    let draft = Draft {
        key: topic(id),
        kind: kind.to_owned(),
        payload,
        ttl_ms: None,
        producer: "jkb".to_owned(),
    };
    match mq::send_reserved(conn, meta, &topic(id), &draft, now) {
        Ok(seq) => Ok(Some(seq)),
        Err(crate::Error::Queue(QueueError::QueueFull { .. } | QueueError::NoSuchTopic(_)))
            if !must =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

fn index_spec() -> TopicSpec {
    TopicSpec {
        max_bytes: INDEX_MAX_BYTES,
        default_ttl_ms: Some(TOPIC_TTL_MS),
        ..TopicSpec::default()
    }
}

/// [`INDEX_TOPIC`], created if it is not there yet.
fn ensure_index(conn: &Connection, meta: &WriteMeta, now: i64) -> Result<()> {
    mq::topic_create_reserved(conn, meta, INDEX_TOPIC, &index_spec(), now)?;
    Ok(())
}

/// Announce `row`'s state on [`INDEX_TOPIC`] — only while a group reads it, by `claude/notify`'s rule
/// that a topic nobody reads is not written to, and never failing what it announces: a full topic
/// drops the hint (the record is the answer).
fn announce(conn: &Connection, meta: &WriteMeta, row: &ExecRow, now: i64) -> Result<()> {
    let groups = match mq::group_count_reserved(conn, INDEX_TOPIC) {
        Ok(n) => n,
        Err(crate::Error::Queue(QueueError::NoSuchTopic(_))) => 0,
        Err(e) => return Err(e),
    };
    if groups == 0 {
        return Ok(());
    }
    let draft = Draft {
        key: INDEX_TOPIC.to_owned(),
        kind: INDEX_KIND.to_owned(),
        payload: json!({ "id": row.wire_id(), "state": row.state.as_str() }),
        ttl_ms: None,
        producer: "jkb".to_owned(),
    };
    match mq::send_reserved(conn, meta, INDEX_TOPIC, &draft, now) {
        Ok(_) | Err(crate::Error::Queue(QueueError::QueueFull { .. })) => Ok(()),
        Err(e) => Err(e),
    }
}

/// The notification a new request posts: who asked, and why — never the argv, because a
/// notification is not where a request is reviewed (the Executions tab is).
fn notification_body(row: &ExecRow) -> String {
    let who = &row.requester;
    let kind = who
        .agent_type
        .as_deref()
        .map(|t| format!(", {t}"))
        .unwrap_or_default();
    let why = if row.ask.reason.is_empty() {
        ".".to_owned()
    } else {
        format!(": {}", row.ask.reason)
    };
    format!(
        "{} ({}{kind}) asks to run a command{why}",
        who.session, who.role
    )
}

fn decision_payload(row: &ExecRow) -> Value {
    let mut p = json!({ "id": row.wire_id(), "state": row.state.as_str() });
    if let Some(note) = &row.note {
        p["note"] = json!(note);
    }
    if let Some(reason) = row.refuse_reason {
        p["refuse_reason"] = json!(reason.as_str());
    }
    p
}

/// Move `id` with `sql` (a compare-and-set `UPDATE` whose `?1` is the id), log the move, and
/// publish its decision. `None` when the compare failed: the row was not in the state `sql` names.
fn moved(
    conn: &Connection,
    meta: &WriteMeta,
    id: i64,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
    now: i64,
) -> Result<Option<ExecRow>> {
    let before = row_json(conn, id)?;
    let changed = conn.prepare_cached(sql)?.execute(params)?;
    if changed == 0 {
        return Ok(None);
    }
    let after = row_json(conn, id)?;
    changelog::upsert(
        conn,
        meta,
        Entity::ExecRequests,
        &id.to_string(),
        Some(&before),
        Some(&after),
    )?;
    let row = must_get(conn, id)?;
    if row.state != ExecState::Finished {
        publish(
            conn,
            meta,
            id,
            "exec.decision",
            decision_payload(&row),
            now,
            false,
        )?;
    }
    // Every way out of `pending` — run, decline, refusal, expiry — takes its notification down, in
    // the same transaction as the move: no caller has to remember to.
    if before.get("state").and_then(Value::as_str) == Some(ExecState::Pending.as_str()) {
        crate::notify::withdraw_direct(
            conn,
            meta,
            &notification_id(id),
            &row.requester.session,
            now,
        )?;
    }
    announce(conn, meta, &row, now)?;
    Ok(Some(row))
}

/// Expire every pending request whose `expires_at` has passed, returning their ids. Called at the
/// start of every function here that takes `meta`, so no path sees a request it should not.
fn settle(conn: &Connection, meta: &WriteMeta, now: i64) -> Result<Vec<i64>> {
    let due: Vec<i64> = conn
        .prepare_cached(
            "SELECT id FROM exec_requests WHERE state = 'pending' AND expires_at <= ?1 ORDER BY id",
        )?
        .query_map([now], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut expired = Vec::with_capacity(due.len());
    for id in due {
        let note = "expired: nobody answered it in two hours".to_owned();
        if moved(
            conn,
            meta,
            id,
            "UPDATE exec_requests SET state = 'expired', ended_at = ?2, note = ?3 \
             WHERE id = ?1 AND state = 'pending'",
            &[&id, &now, &note],
            now,
        )?
        .is_some()
        {
            expired.push(id);
        }
    }
    // A run whose runner went quiet past its timeout and grace: ended here, as interrupted.
    let overdue: Vec<i64> = conn
        .prepare_cached(
            "SELECT id FROM exec_requests WHERE state = 'running' \
               AND started_at + timeout_ms + ?2 <= ?1 ORDER BY id",
        )?
        .query_map(params![now, RUN_GRACE_MS], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in overdue {
        let note = SETTLED_INTERRUPT.to_owned();
        if let Some(row) = moved(
            conn,
            meta,
            id,
            "UPDATE exec_requests SET state = 'finished', outcome = 'interrupted', \
                 ended_at = ?2, note = ?3 WHERE id = ?1 AND state = 'running'",
            &[&id, &now, &note],
            now,
        )? {
            publish(conn, meta, id, "exec.end", end_payload(&row), now, false)?;
        }
    }
    Ok(expired)
}

fn end_payload(row: &ExecRow) -> Value {
    json!({
        "id": row.wire_id(),
        "state": row.state.as_str(),
        "outcome": row.outcome.map(Outcome::as_str),
        "exit_status": row.exit_status,
    })
}

/// Ask for a run. An identical pending request from the same session — same argv, directory and its
/// side, target, pin and timeout — is returned instead (`folded`), so a session that asks twice is
/// answered once. The reason is not part of the fold: a re-ask in other words is the same request.
///
/// # Errors
/// A validation error for a malformed requester or ask (an empty argv, an option as `argv[0]`, a NUL
/// byte, more than [`MAX_ARGV_BYTES`], a relative directory, a reason over [`MAX_REASON_CHARS`], a
/// malformed pin, a timeout outside [`MIN_TIMEOUT_MS`]`..=`[`MAX_TIMEOUT_MS`]); a database error.
pub fn request(
    conn: &Connection,
    meta: &WriteMeta,
    requester: &Requester,
    ask: &Ask,
    now: i64,
) -> Result<Requested> {
    requester.check()?;
    let argv = ask.check()?;
    settle(conn, meta, now)?;
    let timeout = ask.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let (pin_head, pin_changes) = ask.pin.as_ref().map_or((None, None), |p| {
        (Some(p.head.as_str()), Some(p.changes.as_str()))
    });
    let inserted: Option<i64> = conn
        .prepare_cached(
            "INSERT INTO exec_requests (created_at, expires_at, session, agent_type, role, task_id, \
                 target, dir, dir_side, argv, reason, pin_head, pin_changes, timeout_ms, state) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 'pending') \
             ON CONFLICT DO NOTHING RETURNING id",
        )?
        .query_row(
            params![
                now,
                now.saturating_add(EXPIRES_AFTER_MS),
                requester.session,
                requester.agent_type,
                requester.role,
                requester.task.map(ItemId::get),
                ask.target.as_str(),
                ask.dir,
                ask.dir_side.as_str(),
                argv,
                ask.reason,
                pin_head,
                pin_changes,
                timeout,
            ],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = inserted {
        let after = row_json(conn, id)?;
        changelog::upsert(
            conn,
            meta,
            Entity::ExecRequests,
            &id.to_string(),
            None,
            Some(&after),
        )?;
        let spec = TopicSpec {
            max_bytes: TOPIC_MAX_BYTES,
            default_ttl_ms: Some(TOPIC_TTL_MS),
            ..TopicSpec::default()
        };
        mq::topic_create_reserved(conn, meta, &topic(id), &spec, now)?;
        let row = must_get(conn, id)?;
        // Every way of asking notifies, because it is done here, in the request's own transaction.
        crate::notify::post_direct(
            conn,
            meta,
            &notification_id(id),
            &row.requester.session,
            &format!("Run request {}", row.wire_id()),
            &notification_body(&row),
            now,
        )?;
        announce(conn, meta, &row, now)?;
        return Ok(Requested { row, folded: false });
    }
    // The fold: the one pending row the partial unique index let this insert collide with.
    let id: i64 = conn
        .prepare_cached(
            "SELECT id FROM exec_requests WHERE state = 'pending' AND session = ?1 AND argv = ?2 \
               AND dir = ?3 AND target = ?4 AND ifnull(pin_head, '') = ifnull(?5, '') \
               AND ifnull(pin_changes, '') = ifnull(?6, '') AND dir_side = ?7 AND timeout_ms = ?8",
        )?
        .query_row(
            params![
                requester.session,
                argv,
                ask.dir,
                ask.target.as_str(),
                pin_head,
                pin_changes,
                ask.dir_side.as_str(),
                timeout,
            ],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| {
            validation("the request collided with no pending request it could fold into")
        })?;
    Ok(Requested {
        row: must_get(conn, id)?,
        folded: true,
    })
}

fn not_pending(row: &ExecRow, verb: &str) -> crate::Error {
    stale(format!(
        "cannot {verb} {}: it is {}, not pending",
        row.wire_id(),
        row.state.as_str()
    ))
}

/// Start running `id` under `runner`, if it is still pending and `pin` — recomputed by the runner —
/// is the pin the request was made with. A different pin refuses the request instead
/// (`tree_changed`), in this transaction; an expiry that came due expires it.
///
/// # Errors
/// [`crate::Error::Stale`] for a request that is not pending (it ran, or was answered, already);
/// not found; a validation error for a malformed runner or pin; a database error.
pub fn begin(
    conn: &Connection,
    meta: &WriteMeta,
    id: i64,
    pin: Option<&TreePin>,
    runner: &str,
    now: i64,
) -> Result<Begun> {
    check_text("runner", runner, MAX_NAME_BYTES, false)?;
    if let Some(p) = pin {
        check_hex("pin head", &p.head, &[40, 64])?;
        check_hex("pin changes", &p.changes, &[64])?;
    }
    let expired = settle(conn, meta, now)?;
    let row = must_get(conn, id)?;
    if expired.contains(&id) {
        return Ok(Begun::Expired(row));
    }
    if row.state != ExecState::Pending {
        return Err(not_pending(&row, "begin"));
    }
    if row.ask.pin.as_ref() != pin {
        let refused = moved(
            conn,
            meta,
            id,
            "UPDATE exec_requests SET state = 'refused', refuse_reason = 'tree_changed', \
                 ended_at = ?2 WHERE id = ?1 AND state = 'pending'",
            &[&id, &now],
            now,
        )?
        .ok_or_else(|| not_pending(&row, "refuse"))?;
        return Ok(Begun::Refused(refused));
    }
    let running = moved(
        conn,
        meta,
        id,
        "UPDATE exec_requests SET state = 'running', runner = ?2, started_at = ?3 \
         WHERE id = ?1 AND state = 'pending'",
        &[&id, &runner, &now],
        now,
    )?
    .ok_or_else(|| not_pending(&row, "begin"))?;
    conn.prepare_cached("INSERT OR IGNORE INTO exec_output (request_id) VALUES (?1)")?
        .execute([id])?;
    Ok(Begun::Running(running))
}

/// Decline `id` with a note for the session saying what to ask for instead.
///
/// # Errors
/// [`crate::Error::Stale`] unless it is pending; not found; a validation error for a note over
/// [`MAX_NOTE_CHARS`]; a database error.
pub fn decline(
    conn: &Connection,
    meta: &WriteMeta,
    id: i64,
    note: &str,
    now: i64,
) -> Result<ExecRow> {
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(validation(format!(
            "the note is longer than {MAX_NOTE_CHARS} characters"
        )));
    }
    if note.contains('\0') {
        return Err(validation("the note contains a NUL byte"));
    }
    settle(conn, meta, now)?;
    let row = must_get(conn, id)?;
    moved(
        conn,
        meta,
        id,
        "UPDATE exec_requests SET state = 'declined', note = ?2, ended_at = ?3 \
         WHERE id = ?1 AND state = 'pending'",
        &[&id, &note, &now],
        now,
    )?
    .ok_or_else(|| not_pending(&row, "decline"))
}

/// Refuse `id`: it cannot run as approved.
///
/// # Errors
/// [`crate::Error::Stale`] unless it is pending; not found; a database error.
pub fn refuse(
    conn: &Connection,
    meta: &WriteMeta,
    id: i64,
    reason: RefuseReason,
    now: i64,
) -> Result<ExecRow> {
    settle(conn, meta, now)?;
    let row = must_get(conn, id)?;
    moved(
        conn,
        meta,
        id,
        "UPDATE exec_requests SET state = 'refused', refuse_reason = ?2, ended_at = ?3 \
         WHERE id = ?1 AND state = 'pending'",
        &[&id, &reason.as_str(), &now],
        now,
    )?
    .ok_or_else(|| not_pending(&row, "refuse"))
}

/// Publish one chunk of `id`'s output, as its runner. Returns the chunk's seq on the topic — `None`
/// when the topic is full and the chunk was dropped — and its 0-based number, which counts every
/// chunk whether published or not, so a reader sees a dropped one as a gap. The row's tail and the
/// runner's log are the record; the stream is a hint, and a full topic must not stop a run's
/// output being recorded.
///
/// # Errors
/// [`crate::Error::Stale`] unless `id` is running under `runner`; not found; a validation error for an
/// empty chunk or one over [`MAX_CHUNK_BYTES`]; a database error.
pub fn output(
    conn: &Connection,
    meta: &WriteMeta,
    id: i64,
    runner: &str,
    chunk: &[u8],
    now: i64,
) -> Result<(Option<i64>, i64)> {
    if chunk.is_empty() || chunk.len() > MAX_CHUNK_BYTES {
        return Err(validation(format!(
            "an output chunk is 1..={MAX_CHUNK_BYTES} bytes; this one is {}",
            chunk.len()
        )));
    }
    settle(conn, meta, now)?;
    let row = must_get(conn, id)?;
    // A run `settle` ended by age is still its runner's to report on: the runner was only late.
    if !settled_by_age(&row, runner) {
        held_by(&row, runner, "publish output for")?;
    }
    let n: i64 = conn
        .prepare_cached(
            "INSERT INTO exec_output (request_id, chunks) VALUES (?1, 1) \
             ON CONFLICT(request_id) DO UPDATE SET chunks = chunks + 1 RETURNING chunks - 1",
        )?
        .query_row([id], |r| r.get(0))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(chunk);
    let seq = publish(
        conn,
        meta,
        id,
        "exec.output",
        json!({ "n": n, "b64": b64 }),
        now,
        false,
    )?;
    Ok((seq, n))
}

/// Whether `row` is a run [`settle`] ended by age alone, under `runner` — which that runner's own
/// late report may still correct.
fn settled_by_age(row: &ExecRow, runner: &str) -> bool {
    row.state == ExecState::Finished
        && row.outcome == Some(Outcome::Interrupted)
        && row.note.as_deref() == Some(SETTLED_INTERRUPT)
        && row.runner.as_deref() == Some(runner)
}

/// A finished run's tail, total and log path.
fn store_output(conn: &Connection, id: i64, done: &Finished<'_>, tail_len: i64) -> Result<()> {
    let truncated = done.total_bytes > tail_len;
    conn.prepare_cached(
        "INSERT INTO exec_output (request_id, tail, total_bytes, truncated, log_path) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(request_id) DO UPDATE SET tail = excluded.tail, \
             total_bytes = excluded.total_bytes, truncated = excluded.truncated, \
             log_path = excluded.log_path",
    )?
    .execute(params![
        id,
        done.tail,
        done.total_bytes,
        truncated,
        done.log_path
    ])?;
    Ok(())
}

/// Refuse anyone but the runner of a running request.
fn held_by(row: &ExecRow, runner: &str, verb: &str) -> Result<()> {
    if row.state != ExecState::Running {
        return Err(stale(format!(
            "cannot {verb} {}: it is {}, not running",
            row.wire_id(),
            row.state.as_str()
        )));
    }
    if row.runner.as_deref() != Some(runner) {
        return Err(stale(format!(
            "cannot {verb} {}: it is running under another runner",
            row.wire_id()
        )));
    }
    Ok(())
}

/// Record that `id` ended. Idempotent for identical values (a runner retrying an answer it lost);
/// refused to a runner other than the one [`begin`] recorded.
///
/// # Errors
/// [`crate::Error::Stale`] unless `id` is running under `done.runner` (or finished with exactly these
/// values); not found; a validation error for a tail over [`MAX_TAIL_BYTES`], a total below the tail,
/// a missing exit status for [`Outcome::Exited`]/[`Outcome::Signalled`], or a malformed log path; a
/// database error.
pub fn finish(
    conn: &Connection,
    meta: &WriteMeta,
    id: i64,
    done: &Finished<'_>,
    now: i64,
) -> Result<ExecRow> {
    if done.tail.len() > MAX_TAIL_BYTES {
        return Err(validation(format!(
            "the tail is {} bytes; a row keeps at most {MAX_TAIL_BYTES}",
            done.tail.len()
        )));
    }
    let tail_len = i64::try_from(done.tail.len()).unwrap_or(i64::MAX);
    if done.total_bytes < tail_len {
        return Err(validation("total_bytes is less than the tail it ends with"));
    }
    if matches!(done.outcome, Outcome::Exited | Outcome::Signalled) && done.exit_status.is_none() {
        return Err(validation(format!(
            "an outcome of {} needs its exit status",
            done.outcome.as_str()
        )));
    }
    if let Some(p) = done.log_path {
        check_text("log path", p, MAX_PATH_BYTES, false)?;
    }
    settle(conn, meta, now)?;
    let row = must_get(conn, id)?;
    if settled_by_age(&row, done.runner) {
        // The recorded runner was only late: its own ending replaces the one `settle` assumed.
        let finished = moved(
            conn,
            meta,
            id,
            "UPDATE exec_requests SET outcome = ?2, exit_status = ?3, ended_at = ?4, note = NULL \
             WHERE id = ?1 AND state = 'finished' AND outcome = 'interrupted' AND note = ?5 \
               AND runner = ?6",
            &[
                &id,
                &done.outcome.as_str(),
                &done.exit_status,
                &now,
                &SETTLED_INTERRUPT,
                &done.runner,
            ],
            now,
        )?
        .ok_or_else(|| stale(format!("{} changed under its finish", row.wire_id())))?;
        store_output(conn, id, done, tail_len)?;
        publish(
            conn,
            meta,
            id,
            "exec.end",
            end_payload(&finished),
            now,
            false,
        )?;
        return Ok(finished);
    }
    if row.state == ExecState::Finished {
        let same = row.runner.as_deref() == Some(done.runner)
            && row.outcome == Some(done.outcome)
            && row.exit_status == done.exit_status;
        return if same {
            Ok(row)
        } else {
            Err(stale(format!(
                "{} already finished with other values, or under another runner",
                row.wire_id()
            )))
        };
    }
    held_by(&row, done.runner, "finish")?;
    let finished = moved(
        conn,
        meta,
        id,
        "UPDATE exec_requests SET state = 'finished', outcome = ?2, exit_status = ?3, ended_at = ?4 \
         WHERE id = ?1 AND state = 'running' AND runner = ?5",
        &[
            &id,
            &done.outcome.as_str(),
            &done.exit_status,
            &now,
            &done.runner,
        ],
        now,
    )?
    .ok_or_else(|| not_pending(&row, "finish"))?;
    store_output(conn, id, done, tail_len)?;
    publish(
        conn,
        meta,
        id,
        "exec.end",
        end_payload(&finished),
        now,
        false,
    )?;
    Ok(finished)
}

/// Request `id` and its output, after expiring whatever came due.
///
/// # Errors
/// A database error, or a row this jkb cannot read.
pub fn status(
    conn: &Connection,
    meta: &WriteMeta,
    id: i64,
    now: i64,
) -> Result<Option<(ExecRow, Option<Output>)>> {
    settle(conn, meta, now)?;
    let Some(row) = get(conn, id)? else {
        return Ok(None);
    };
    Ok(Some((row, output_of(conn, id)?)))
}

/// Request `id`'s stored output, if it ever ran.
///
/// # Errors
/// A database error.
pub fn output_of(conn: &Connection, id: i64) -> Result<Option<Output>> {
    Ok(conn
        .prepare_cached(
            "SELECT tail, total_bytes, truncated, chunks, log_path FROM exec_output \
             WHERE request_id = ?1",
        )?
        .query_row([id], |r| {
            Ok(Output {
                tail: r.get(0)?,
                total_bytes: r.get(1)?,
                truncated: r.get(2)?,
                chunks: r.get(3)?,
                log_path: r.get(4)?,
            })
        })
        .optional()?)
}

/// A page of requests after id `after`, oldest first, after expiring whatever came due. Returns the
/// rows and, when there are more, the id to continue after.
///
/// # Errors
/// A database error, or a row this jkb cannot read.
pub fn list(
    conn: &Connection,
    meta: &WriteMeta,
    filter: Filter<'_>,
    after: Option<i64>,
    now: i64,
) -> Result<(Vec<ExecRow>, Option<i64>)> {
    settle(conn, meta, now)?;
    let fetch = i64::try_from(PAGE + 1).unwrap_or(i64::MAX);
    let raw: Vec<RawRow> = conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM exec_requests \
             WHERE id > ?1 AND (?2 IS NULL OR state = ?2) AND (?3 IS NULL OR session = ?3) \
             ORDER BY id LIMIT ?4"
        ))?
        .query_map(
            params![
                after.unwrap_or(0),
                filter.state.map(ExecState::as_str),
                filter.session,
                fetch
            ],
            row_from,
        )?
        .collect::<rusqlite::Result<_>>()?;
    let more = raw.len() > PAGE;
    let mut rows = Vec::with_capacity(raw.len().min(PAGE));
    let mut bytes = 0usize;
    let mut cut = false;
    for r in raw.into_iter().take(PAGE) {
        if !rows.is_empty() && bytes >= PAGE_BYTES {
            cut = true;
            break;
        }
        bytes +=
            r.argv.len() + r.dir.len() + r.reason.len() + r.note.as_ref().map_or(0, String::len);
        rows.push(r.parse()?);
    }
    let next = if more || cut {
        rows.last().map(|r: &ExecRow| r.id)
    } else {
        None
    };
    Ok((rows, next))
}

/// Up to `max` messages of request `id`'s topic after seq `after` — how its requester follows it. The
/// topic is reserved ([`mq::RESERVED_PREFIXES`]): no queue operation reads it, and who may call this
/// is `jkb-api`'s (the requester's session, or the operator).
///
/// # Errors
/// Not found; [`QueueError::Invalid`] for a `max` over [`mq::MAX_BATCH`]; a database error.
pub fn follow(
    conn: &Connection,
    id: i64,
    after: i64,
    max: usize,
    now: i64,
) -> Result<Vec<mq::Delivered>> {
    must_get(conn, id)?;
    match mq::read_reserved(conn, &topic(id), after, max, now) {
        Err(crate::Error::Queue(QueueError::NoSuchTopic(_))) => Ok(Vec::new()),
        other => other,
    }
}

/// Expire `session`'s pending requests if the session registry now says it has ended. For a caller
/// that has just recorded an end **the session itself reported** (or the operator did): an end any
/// queue client can forge for another session must not cost that session its pending requests.
///
/// # Errors
/// A database error.
pub fn expire_if_ended(
    conn: &Connection,
    meta: &WriteMeta,
    session: &str,
    now: i64,
) -> Result<usize> {
    if crate::claude_session::state(conn, session)? == crate::claude_session::SessionState::Ended {
        expire_session(conn, meta, session, now)
    } else {
        Ok(0)
    }
}

/// The requester's session ended: expire its pending requests (a running one runs on — the operator
/// already approved it). Returns how many expired. Through [`expire_if_ended`], whose caller vouches
/// the end came from the session itself.
///
/// # Errors
/// A database error.
pub fn expire_session(
    conn: &Connection,
    meta: &WriteMeta,
    session: &str,
    now: i64,
) -> Result<usize> {
    settle(conn, meta, now)?;
    let ids: Vec<i64> = conn
        .prepare_cached(
            "SELECT id FROM exec_requests WHERE state = 'pending' AND session = ?1 ORDER BY id",
        )?
        .query_map([session], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut n = 0;
    for id in ids {
        let note = "expired: the requesting session ended".to_owned();
        if moved(
            conn,
            meta,
            id,
            "UPDATE exec_requests SET state = 'expired', ended_at = ?2, note = ?3 \
             WHERE id = ?1 AND state = 'pending'",
            &[&id, &now, &note],
            now,
        )?
        .is_some()
        {
            n += 1;
        }
    }
    Ok(n)
}

/// Put the operator's consumer group `group` on [`INDEX_TOPIC`], creating the topic if need be.
/// Only `jkb-api` calls this, for the operator; idempotent as [`mq::group_create`] is.
///
/// # Errors
/// A malformed group name, or a database error.
pub fn index_group_create(
    conn: &Connection,
    meta: &WriteMeta,
    group: &str,
    start: mq::Start,
    now: i64,
) -> Result<mq::Created> {
    ensure_index(conn, meta, now)?;
    mq::group_create_reserved(conn, meta, INDEX_TOPIC, group, start, now)
}

/// Take the operator's group `group` off [`INDEX_TOPIC`]; `false` when it was not there.
///
/// # Errors
/// As [`mq::group_delete`].
pub fn index_group_delete(conn: &Connection, meta: &WriteMeta, group: &str) -> Result<bool> {
    mq::group_delete_reserved(conn, meta, INDEX_TOPIC, group)
}

/// [`mq::poll`] on [`INDEX_TOPIC`], for the operator's group.
///
/// # Errors
/// As [`mq::poll`].
pub fn index_poll(
    conn: &Connection,
    meta: &WriteMeta,
    group: &str,
    max: usize,
    after: Option<i64>,
    now: i64,
) -> Result<Vec<mq::Delivered>> {
    mq::poll_reserved(conn, meta, INDEX_TOPIC, group, max, after, now)
}

/// [`mq::poll_needed`] on [`INDEX_TOPIC`], for the operator's group.
///
/// # Errors
/// As [`mq::poll_needed`].
pub fn index_poll_needed(
    conn: &Connection,
    group: &str,
    max: usize,
    after: Option<i64>,
    now: i64,
) -> Result<bool> {
    mq::poll_needed_reserved(conn, INDEX_TOPIC, group, max, after, now)
}

/// [`mq::ack`] on [`INDEX_TOPIC`], for the operator's group.
///
/// # Errors
/// As [`mq::ack`].
pub fn index_ack(
    conn: &Connection,
    meta: &WriteMeta,
    group: &str,
    seq: i64,
    now: i64,
) -> Result<i64> {
    mq::ack_reserved(conn, meta, INDEX_TOPIC, group, seq, now)
}

#[cfg(test)]
mod tests;
