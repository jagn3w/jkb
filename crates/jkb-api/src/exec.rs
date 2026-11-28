//! Run requests as ops (the Executions design): a session asks the operator to run one command for
//! it, and the operator's runner moves the request through its states.
//!
//! **Pure database work**, as every op is: nothing here runs a command, reads a file or touches git.
//! The client computes the tree pin with jkb's hooks-off git and sends it; the runner recomputes it
//! at Run and hands it to `exec.begin`, which refuses the request when the two differ. Running is the
//! runner's — Code Factory's main process — never this crate's.
//!
//! **Who asks is the principal, never a field.** `exec.request` is held to a harness-attested tool
//! call ([`PrincipalKind::Ticket`]): the session, the subagent type and the bound task are the
//! ticket's, the role the principal's. The operator and a grant's holder name no session, so they
//! are refused — the container holds only a coordinator credential, so no session can approve its
//! own request, and nothing can ask in another session's name.
//!
//! **Who reads what.** The operator reads every request; an attested session reads only its own
//! session's (another's is `not_found`); a grant's holder reads none. The same holds for a request's
//! topic, which only `exec.follow` reads: the queue's own ops refuse `exec/` topics outright.

use base64::Engine as _;
use jkb_core::exec::{self, Ask, ExecRow, ExecState, ExecTarget, Outcome, RefuseReason, TreePin};
use jkb_core::exec_delivery as delivery;
use jkb_core::Db;
use jkb_rbac::Role as _;
use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};

use crate::rbac::{Principal, PrincipalKind};
use crate::{ApiError, ErrorCode};

/// A tree pin as the wire carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    /// The repository's `HEAD` commit — a label only.
    pub head: String,
    /// The blake3 of its pinned work tree.
    pub changes: String,
}

impl From<Pin> for TreePin {
    fn from(p: Pin) -> Self {
        Self {
            head: p.head,
            changes: p.changes,
        }
    }
}

impl From<TreePin> for Pin {
    fn from(p: TreePin) -> Self {
        Self {
            head: p.head,
            changes: p.changes,
        }
    }
}

/// What `exec.request` asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecAsk {
    /// `host` or `container`.
    pub target: String,
    /// The directory, resolved to its real path on the requester's side.
    pub dir: String,
    /// Which side's path `dir` is: `host` or `container`.
    pub dir_side: String,
    /// The program and its arguments, run without a shell.
    pub argv: Vec<String>,
    /// One sentence for the operator.
    #[serde(default)]
    pub reason: String,
    /// The repository's pin; absent for a directory outside any repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<Pin>,
    /// The run's timeout; 15 minutes when absent, two hours at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<i64>,
}

/// A request's stored output, as the wire carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputView {
    /// The last bytes of output (at most 256 KiB), base64.
    pub tail_b64: String,
    /// Every byte the run produced.
    pub total_bytes: i64,
    /// Whether the tail is less than all of it.
    pub truncated: bool,
    /// How many chunks were published on the topic.
    pub chunks: i64,
    /// Where the full output is, on the runner's side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
}

/// A request, as every exec answer carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecView {
    /// `exec-<n>`.
    pub id: String,
    /// Its message-queue topic, `exec/<n>`.
    pub topic: String,
    /// `pending`, `running`, `finished`, `declined`, `refused` or `expired`.
    pub state: String,
    /// `host` or `container`.
    pub target: String,
    /// The directory.
    pub dir: String,
    /// Which side's path `dir` is.
    pub dir_side: String,
    /// The program and its arguments.
    pub argv: Vec<String>,
    /// The requester's sentence.
    pub reason: String,
    /// The requesting Claude Code session.
    pub session: String,
    /// The requesting subagent's type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// The requester's role.
    pub role: String,
    /// The task the requester held, by uid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// The tree pin; absent for a directory outside any repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<Pin>,
    /// The run's timeout.
    pub timeout_ms: i64,
    /// When it was asked (Unix ms).
    pub created_at: i64,
    /// When it expires if still pending (Unix ms).
    pub expires_at: i64,
    /// Why it was refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refuse_reason: Option<String>,
    /// The operator's note, or why it expired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Who ran it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    /// When it started running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// When it was answered or ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// The exit status, or the signal number when signalled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i64>,
    /// `exited`, `signalled`, `timed_out` or `interrupted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Its stored output — on `exec.status` only, once it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputView>,
    /// The message reporting its result, as the push and the hook deliver it
    /// ([`jkb_core::exec::delivery_text`]) — on `exec.status` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_text: Option<String>,
}

/// A request still pending or running, as `exec.inbox.take` lists it for a session reminded of what
/// it filed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRequest {
    /// `exec-<n>`.
    pub id: String,
    /// `pending` or `running`.
    pub state: String,
    /// The requesting subagent's type, if a subagent asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// The requester's sentence.
    pub reason: String,
}

impl OpenRequest {
    /// The reminder line: `exec-12 running, filed by implementer: "<reason>"`, escaped.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "{} {}, filed by {}: \"{}\"",
            self.id,
            self.state,
            exec::escape(
                self.agent_type.as_deref().unwrap_or("the main agent"),
                false
            ),
            exec::escape(&self.reason, false)
        )
    }
}

fn invalid(why: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::Invalid, why)
}

fn forbidden(why: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::Forbidden, why)
}

fn not_found(id: &str) -> ApiError {
    ApiError::with_code(ErrorCode::NotFound, format!("no run request {id}"))
}

fn target(what: &str, s: &str) -> Result<ExecTarget, ApiError> {
    ExecTarget::parse(s)
        .ok_or_else(|| invalid(format!("{what} {s:?} is not `host` or `container`")))
}

/// The row id a wire id names.
///
/// # Errors
/// [`ErrorCode::Invalid`] for anything but `exec-<n>` or `<n>`.
pub fn id(s: &str) -> Result<i64, ApiError> {
    Ok(exec::parse_id(s)?)
}

/// A state a filter names.
///
/// # Errors
/// [`ErrorCode::Invalid`] for an unknown state.
pub fn state(s: &str) -> Result<ExecState, ApiError> {
    ExecState::parse(s).ok_or_else(|| invalid(format!("{s:?} is not a run request state")))
}

/// A refusal reason.
///
/// # Errors
/// [`ErrorCode::Invalid`] for an unknown reason.
pub fn refuse_reason(s: &str) -> Result<RefuseReason, ApiError> {
    RefuseReason::parse(s).ok_or_else(|| {
        invalid(format!(
            "{s:?} is not a refusal reason (`tree_changed`, `audit_refused`, `dir_gone`)"
        ))
    })
}

/// An outcome.
///
/// # Errors
/// [`ErrorCode::Invalid`] for an unknown outcome.
pub fn outcome(s: &str) -> Result<Outcome, ApiError> {
    Outcome::parse(s).ok_or_else(|| {
        invalid(format!(
            "{s:?} is not an outcome (`exited`, `signalled`, `timed_out`, `interrupted`)"
        ))
    })
}

/// Bytes the wire carries as base64.
///
/// # Errors
/// [`ErrorCode::Invalid`] for text that is not base64.
pub fn bytes(what: &str, b64: &str) -> Result<Vec<u8>, ApiError> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| invalid(format!("{what} is not base64: {e}")))
}

/// The requester `principal` is, for `exec.request`: an attested tool call's session, subagent type,
/// bound task and role.
///
/// # Errors
/// [`ErrorCode::Forbidden`] for the operator or a grant's holder, which name no session, and for a
/// principal with no role.
pub fn requester(principal: &Principal) -> Result<exec::Requester, ApiError> {
    let PrincipalKind::Ticket(t) = &principal.kind else {
        return Err(forbidden(format!(
            "`exec.request` refused: {} names no session. A run request is made by a Claude Code \
             session through its tool call's attestation ticket, so the operator can see who asked \
             and tell them the answer; the operator runs commands directly",
            principal.label
        )));
    };
    let role = principal
        .roles
        .first()
        .ok_or_else(|| forbidden("`exec.request` refused: this caller holds no role"))?;
    Ok(exec::Requester {
        session: t.session.clone(),
        agent_type: t.agent_type.clone(),
        agent_id: t.agent_id.clone(),
        role: role.name().to_owned(),
        task: principal.scope,
    })
}

/// Whose requests `principal` may read: `None` for every session's (the operator), else the one
/// session it is.
///
/// # Errors
/// [`ErrorCode::Forbidden`] for a grant's holder.
pub fn reader(principal: &Principal) -> Result<Option<String>, ApiError> {
    match &principal.kind {
        PrincipalKind::Operator => Ok(None),
        PrincipalKind::Ticket(t) => Ok(Some(t.session.clone())),
        PrincipalKind::Grant(_) => Err(forbidden(format!(
            "run requests are read by the session that made them, or the operator — not by {}",
            principal.label
        ))),
    }
}

impl ExecAsk {
    /// The core's ask.
    ///
    /// # Errors
    /// [`ErrorCode::Invalid`] for an unknown target or side.
    pub fn into_ask(self) -> Result<Ask, ApiError> {
        Ok(Ask {
            target: target("target", &self.target)?,
            dir: self.dir,
            dir_side: target("dir_side", &self.dir_side)?,
            argv: self.argv,
            reason: self.reason,
            pin: self.pin.map(TreePin::from),
            timeout_ms: self.timeout_ms,
        })
    }
}

/// `row` as the wire carries it, with its task resolved to a uid.
///
/// # Errors
/// A database error.
pub fn view(conn: &rusqlite::Connection, row: ExecRow) -> Result<ExecView, ApiError> {
    let task = match row.requester.task {
        Some(t) => conn
            .prepare_cached("SELECT uid FROM items WHERE id = ?1")
            .and_then(|mut s| s.query_row([t.get()], |r| r.get::<_, String>(0)).optional())
            .map_err(jkb_core::Error::from)?,
        None => None,
    };
    Ok(ExecView {
        id: row.wire_id(),
        topic: exec::topic(row.id),
        state: row.state.as_str().to_owned(),
        target: row.ask.target.as_str().to_owned(),
        dir: row.ask.dir,
        dir_side: row.ask.dir_side.as_str().to_owned(),
        argv: row.ask.argv,
        reason: row.ask.reason,
        session: row.requester.session,
        agent_type: row.requester.agent_type,
        role: row.requester.role,
        task,
        pin: row.ask.pin.map(Pin::from),
        timeout_ms: row.ask.timeout_ms.unwrap_or(exec::DEFAULT_TIMEOUT_MS),
        created_at: row.created_at,
        expires_at: row.expires_at,
        refuse_reason: row.refuse_reason.map(|r| r.as_str().to_owned()),
        note: row.note,
        runner: row.runner,
        started_at: row.started_at,
        ended_at: row.ended_at,
        exit_status: row.exit_status,
        outcome: row.outcome.map(|o| o.as_str().to_owned()),
        output: None,
        delivery_text: None,
    })
}

/// `exec.status`: the request and its output, held to whose requests `principal` may read.
///
/// # Errors
/// [`ErrorCode::NotFound`] for no such request, or another session's; [`ErrorCode::Forbidden`] for a
/// grant's holder; a database error.
pub fn status(
    db: &Db,
    actor: &str,
    principal: &Principal,
    wire: &str,
    now: i64,
) -> Result<ExecView, ApiError> {
    let reader = reader(principal)?;
    let n = id(wire)?;
    let wire = wire.to_owned();
    db.write_txn_with(actor, move |c, m| {
        let Some((row, out)) = exec::status(c, m, n, now)? else {
            return Err(not_found(&wire));
        };
        if reader.as_ref().is_some_and(|s| *s != row.requester.session) {
            return Err(not_found(&wire));
        }
        let delivery = delivery::get(c, n)?.unwrap_or_default();
        let text = exec::delivery_text(&row, out.as_ref(), &delivery);
        let mut v = view(c, row)?;
        v.delivery_text = Some(text);
        v.output = out.map(|o| OutputView {
            tail_b64: base64::engine::general_purpose::STANDARD.encode(&o.tail),
            total_bytes: o.total_bytes,
            truncated: o.truncated,
            chunks: o.chunks,
            log_path: o.log_path,
        });
        Ok(v)
    })
}

/// `exec.list`: a page of requests, held to whose requests `principal` may read.
///
/// # Errors
/// [`ErrorCode::Forbidden`] for a grant's holder; [`ErrorCode::Invalid`] for an unknown state; a
/// database error.
pub fn list(
    db: &Db,
    actor: &str,
    principal: &Principal,
    state_: Option<&str>,
    after: Option<i64>,
    newest_first: bool,
    now: i64,
) -> Result<(Vec<ExecView>, Option<i64>), ApiError> {
    let reader = reader(principal)?;
    let state_ = state_.map(state).transpose()?;
    db.write_txn_with(actor, move |c, m| {
        let filter = exec::Filter {
            state: state_,
            session: reader.as_deref(),
            newest_first,
        };
        let (rows, next) = exec::list(c, m, filter, after, now)?;
        let views = rows
            .into_iter()
            .map(|r| view(c, r))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((views, next))
    })
}

/// `exec.follow`: request `wire`'s topic after seq `after`, held to whose requests `principal` may
/// read — the requester's session, or the operator.
///
/// # Errors
/// [`ErrorCode::NotFound`] for no such request, or another session's; [`ErrorCode::Forbidden`] for a
/// grant's holder; a database error.
pub fn follow(
    db: &Db,
    principal: &Principal,
    wire: &str,
    after: i64,
    max: usize,
    now: i64,
) -> Result<Vec<jkb_core::mq::Delivered>, ApiError> {
    let reader = reader(principal)?;
    let n = id(wire)?;
    let wire = wire.to_owned();
    db.read_with(move |c| {
        let Some(row) = exec::get(c, n)? else {
            return Err(not_found(&wire));
        };
        if reader.as_ref().is_some_and(|s| *s != row.requester.session) {
            return Err(not_found(&wire));
        }
        Ok(exec::follow(c, n, after, max, now)?)
    })
}

/// `exec.inbox.take`: hand `session`'s `agent_id` every result due for it, under the hook's lease, and
/// with `open` list the session's open requests. A plain read first, so the common case — nothing due,
/// on every tool call of every session — opens no write transaction; the take itself in one
/// transaction. All of it on `db`, the writer's connection: the hook has a second, and must not wait
/// behind a long read on the reader.
///
/// # Errors
/// [`ErrorCode::Invalid`] for a malformed session; a database error.
pub fn inbox_take(
    db: &Db,
    actor: &str,
    session: &str,
    agent_id: Option<String>,
    open: bool,
    now: i64,
) -> Result<crate::Response, ApiError> {
    check_session(session)?;
    let (s, a) = (session.to_owned(), agent_id.clone());
    let any = db.read(move |c| delivery::any_for_hook(c, &s, a.as_deref(), now))?;
    let (lines, ids) = if any {
        let (s, a) = (session.to_owned(), agent_id);
        db.write_txn(actor, move |c, m| {
            let taken = delivery::take_for_hook(c, m, &s, a.as_deref(), now)?;
            Ok(taken.iter().map(|t| (t.text(), t.row.wire_id())).unzip())
        })?
    } else {
        (Vec::new(), Vec::new())
    };
    let open = if open {
        let s = session.to_owned();
        db.read(move |c| delivery::open(c, &s))?
            .into_iter()
            .map(|r| OpenRequest {
                id: r.wire_id(),
                state: r.state.as_str().to_owned(),
                agent_type: r.requester.agent_type,
                reason: r.ask.reason,
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(crate::Response::ExecInboxTaken { lines, ids, open })
}

/// `exec.inbox.ack`: the hook printed the messages of `ids`, which `exec.inbox.take` handed it for
/// `session`; they are delivered. Returns how many were.
///
/// # Errors
/// [`ErrorCode::Invalid`] for a malformed session or id; a database error.
pub fn inbox_ack(
    db: &Db,
    actor: &str,
    session: &str,
    ids: &[String],
    now: i64,
) -> Result<usize, ApiError> {
    check_session(session)?;
    let ids = ids.iter().map(|i| id(i)).collect::<Result<Vec<_>, _>>()?;
    let s = session.to_owned();
    Ok(db.write_txn(actor, move |c, _| delivery::ack_hook(c, &s, &ids, now))?)
}

/// Where `exec.inbox.claim` says to post a result: one live `claude` process's inbox socket and its
/// token. The only answer that carries a token, so its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxTarget {
    /// The process's `CLAUDE_CODE_MESSAGING_SOCKET`.
    pub socket: String,
    /// Its `CLAUDE_CODE_MESSAGING_TOKEN`.
    pub token: String,
}

impl std::fmt::Debug for InboxTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboxTarget")
            .field("socket", &self.socket)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// The longest container boot a forwarder names, in bytes: an instance is at most 150.
const MAX_BOOT_BYTES: usize = 150;

/// Refuse anything but a container boot, `host#boot`: a forwarder posts only to the sockets of its own
/// container's processes, and a boot is what tells one container's from another's (and from the
/// host's, whose instance names none).
///
/// # Errors
/// [`ErrorCode::Invalid`] for an empty host or boot, a `/` (a pid namespace: the boot is the part
/// before it), a control character, or more than 150 bytes.
pub fn check_boot(instance: &str) -> Result<(), ApiError> {
    let well_formed = instance.len() <= MAX_BOOT_BYTES
        && !instance.contains('/')
        && !instance.chars().any(char::is_control)
        && instance.split_once('#').is_some_and(|(host, boot)| {
            !host.is_empty() && !boot.is_empty() && !boot.contains('#')
        });
    if well_formed {
        Ok(())
    } else {
        Err(invalid(format!(
            "{instance:?} is not a container boot (`host#boot`, at most {MAX_BOOT_BYTES} bytes)"
        )))
    }
}

/// `exec.inbox.due`: the requests whose results are due for the sockets of boot `instance`, after
/// settling expiries — on the writer, as every exec op is.
///
/// # Errors
/// [`ErrorCode::Invalid`] for a malformed boot; a database error.
pub fn inbox_due(
    db: &Db,
    actor: &str,
    instance: &str,
    max: usize,
    now: i64,
) -> Result<Vec<String>, ApiError> {
    check_boot(instance)?;
    let boot = instance.to_owned();
    let ids = db.write_txn(actor, move |c, m| delivery::due(c, m, &boot, now, max))?;
    Ok(ids.into_iter().map(exec::wire_id).collect())
}

/// `exec.inbox.claim`: claim request `wire` for a post to boot `instance`'s sockets, for `lease_ms`.
///
/// # Errors
/// [`ErrorCode::NotFound`] when it is not due for this boot or another deliverer holds it;
/// [`ErrorCode::Invalid`] for a malformed id or boot, or a lease out of bounds; a database error.
pub fn inbox_claim(
    db: &Db,
    actor: &str,
    wire: &str,
    instance: &str,
    lease_ms: i64,
    now: i64,
) -> Result<crate::Response, ApiError> {
    check_boot(instance)?;
    let n = id(wire)?;
    let boot = instance.to_owned();
    let claim = db.write_txn(actor, move |c, _| {
        delivery::claim_for_socket(c, n, &boot, lease_ms, now)
    })?;
    let Some(claim) = claim else {
        return Err(ApiError::with_code(
            ErrorCode::NotFound,
            format!(
                "{wire} is not due for this container's sockets, or another deliverer holds it"
            ),
        ));
    };
    Ok(crate::Response::ExecInboxClaimed {
        text: claim.taken.text(),
        targets: claim
            .targets
            .into_iter()
            .map(|t| InboxTarget {
                socket: t.inbox.socket().to_owned(),
                token: t.inbox.token().to_owned(),
            })
            .collect(),
    })
}

/// `exec.inbox.report`: how the post of claimed request `wire` went. `false` when the claim had lapsed
/// and another deliverer took the row.
///
/// # Errors
/// [`ErrorCode::Invalid`] for a malformed id; a database error.
pub fn inbox_report(
    db: &Db,
    actor: &str,
    wire: &str,
    ok: bool,
    error: Option<&str>,
    now: i64,
) -> Result<bool, ApiError> {
    let n = id(wire)?;
    let why = error.unwrap_or("the forwarder gave no reason").to_owned();
    Ok(db.write_txn(actor, move |c, _| {
        delivery::report(c, n, if ok { Ok(()) } else { Err(why.as_str()) }, now)
    })?)
}

fn check_session(session: &str) -> Result<(), ApiError> {
    if jkb_types::is_session_id(session) {
        Ok(())
    } else {
        Err(invalid(format!("{session:?} is not a session id")))
    }
}

/// Whether an end reported for `session` by `principal` may expire that session's pending run
/// requests: the harness — the operator, or the dev container's own credential, which is what the
/// container's `SessionEnd` hook and its gone-sweep authenticate with, and which the model's
/// sandboxed tools cannot read — or the session's own attested tool call. Never another session's
/// ticket, and never another grant: `session.ended` and `session.gone` name any session, so an end
/// forged by a session must not cost another its requests.
#[must_use]
pub fn vouches_for_end(principal: &Principal, session: &str) -> bool {
    match &principal.kind {
        PrincipalKind::Operator => true,
        PrincipalKind::Grant(g) => g.container,
        PrincipalKind::Ticket(t) => t.session == session,
    }
}

#[cfg(test)]
mod tests;
