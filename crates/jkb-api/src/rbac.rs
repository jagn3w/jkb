//! Who may run which op, and the ops that manage roles, workflows and harness attestation
//! (design D52.3, D52.4, D52.9; `openspec/changes/jkb-rbac-workflows/`).
//!
//! **One check, at the one dispatch.** [`LocalBackend::call`](crate::LocalBackend) resolves the
//! caller to a [`Principal`] and asks [`authorize`] before it serves anything, so the daemon, the MCP
//! server and host-local mode cannot disagree. What an op needs is [`Request::permission`], an
//! exhaustive `match`: a new op does not compile until somebody classes it.
//!
//! **Three kinds of caller.** The operator (the host CLI's own process, or the daemon's root token);
//! a **grant** token (`jkb role grant`, or the dev container's credential); and a harness **ticket**,
//! minted per tool call by the Claude Code hook with the container credential, carrying what the
//! harness — not the model — reported about which agent made the call. A ticket's role comes from
//! the operator's `agent_type → role` map; the main session's from the credential that minted it.
//!
//! **Tickets live in memory** ([`Tickets`]), in the daemon that minted them: they are per tool call,
//! gone at `PostToolUse`, `SubagentStop` or `SessionEnd`, and a daemon restart dropping them costs
//! one re-run command.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jkb_core::roles::{self, GrantRow, Minter, Role};
use jkb_core::workflow::store::{self as wf, Actor, Moved};
use jkb_core::workflow::strategy::{StrategySpec, PRESETS};
use jkb_core::workflow::{Phase, WorkflowEvent};
use jkb_core::{task, WriteMeta};
use jkb_rbac::{Decision, Grant, Grants as _, Refusal, Requirement, RoleBased, RoleTable};
use jkb_types::ItemId;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::{ApiError, ErrorCode, Request};

/// What running an op needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpPermission {
    /// Read the knowledge base.
    Read,
    /// What every agent's hooks send: notifications, the session registry, the queue.
    Hook,
    /// Change a task's content, placement or tags.
    TaskWrite,
    /// Change a task's status directly (`task.set --status`).
    TaskStatus,
    /// Claim, start, take, abandon or release work, and the session bookkeeping around it.
    TaskClaim,
    /// File or record a review.
    Review,
    /// Land — decided per task by its strategy's `lands` toggle, not by this table alone.
    Land,
    /// Fire or observe a workflow event — decided per event by the task's strategy.
    Workflow,
    /// Mint or revoke grants.
    Grant,
    /// Mint and release harness tickets — the container credential's own job.
    Attest,
    /// The operator's: reclaim, break leases, remove items, move namespaces, compact the queue,
    /// choose strategies, map agent types, rotate the container credential.
    Admin,
}

impl jkb_rbac::Permission for OpPermission {
    const ALL: &'static [Self] = &[
        Self::Read,
        Self::Hook,
        Self::TaskWrite,
        Self::TaskStatus,
        Self::TaskClaim,
        Self::Review,
        Self::Land,
        Self::Workflow,
        Self::Grant,
        Self::Attest,
        Self::Admin,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Hook => "hook",
            Self::TaskWrite => "task_write",
            Self::TaskStatus => "task_status",
            Self::TaskClaim => "task_claim",
            Self::Review => "review",
            Self::Land => "land",
            Self::Workflow => "workflow",
            Self::Grant => "grant",
            Self::Attest => "attest",
            Self::Admin => "admin",
        }
    }
}

/// Which role may run which class of op (D52.4). `Land` is held here by the operator only; a
/// strategy's `lands` toggle extends it per task ([`authorize`]). `Attest` is held by the operator
/// and granted by [`authorize`] to the dev container's credential alone — never to a ticket, or the
/// main session could mint itself a reviewer.
pub static OP_GRANTS: RoleTable<Role, OpPermission> = RoleTable {
    grants: &[
        Grant {
            role: Role::Operator,
            permits: <OpPermission as jkb_rbac::Permission>::ALL,
        },
        Grant {
            role: Role::Coordinator,
            permits: &[
                OpPermission::Read,
                OpPermission::Hook,
                OpPermission::TaskWrite,
                OpPermission::TaskStatus,
                OpPermission::TaskClaim,
                OpPermission::Review,
                OpPermission::Workflow,
                OpPermission::Grant,
            ],
        },
        Grant {
            role: Role::Designer,
            permits: &[
                OpPermission::Read,
                OpPermission::Hook,
                OpPermission::TaskWrite,
                OpPermission::Workflow,
            ],
        },
        Grant {
            role: Role::Implementer,
            permits: &[
                OpPermission::Read,
                OpPermission::Hook,
                OpPermission::TaskWrite,
                OpPermission::TaskClaim,
                OpPermission::Workflow,
            ],
        },
        Grant {
            role: Role::Reviewer,
            permits: &[
                OpPermission::Read,
                OpPermission::Hook,
                OpPermission::Review,
                OpPermission::Workflow,
            ],
        },
        Grant {
            role: Role::SystemicReviewer,
            permits: &[
                OpPermission::Read,
                OpPermission::Hook,
                OpPermission::TaskWrite,
                OpPermission::Workflow,
            ],
        },
    ],
};

impl Request {
    /// What this op needs (D52.4). Exhaustive, so a new op must be classed before it compiles.
    #[must_use]
    #[allow(clippy::too_many_lines)] // one arm per op, like `op` and `is_agent_read`
    pub fn permission(&self) -> OpPermission {
        match self {
            Self::KbAmbient { .. }
            | Self::KbQuery { .. }
            | Self::KbLs { .. }
            | Self::KbTree { .. }
            | Self::KbCat { .. }
            | Self::KbGrep { .. }
            | Self::KbSearch { .. }
            | Self::TaskReady { .. }
            | Self::TaskShow { .. }
            | Self::TaskSubtasks { .. }
            | Self::TaskWhy { .. }
            | Self::TaskFacts { .. }
            | Self::TaskByBranch { .. }
            | Self::RepoGate { .. }
            | Self::TaskReviewFindings { .. }
            | Self::TaskClaims { .. }
            | Self::KbHealth {}
            | Self::TaskStaging { .. }
            | Self::ItemShow { .. }
            | Self::KbRelated { .. }
            | Self::KbBlobs { .. }
            | Self::KbBlob { .. }
            | Self::InvRead(_)
            | Self::TaskPrFacts { .. }
            | Self::TaskOpenInRepo { .. }
            | Self::NsList { .. }
            | Self::KbContext { .. }
            | Self::ViewList {}
            | Self::ViewRun { .. }
            | Self::KbHistory { .. }
            | Self::RemovalList { .. }
            | Self::LeaseGet { .. }
            | Self::RoleList { .. }
            | Self::RoleWhoami {}
            | Self::WorkflowShow { .. }
            | Self::WorkflowStrategies {} => OpPermission::Read,
            Self::MqTopicCreate { .. }
            | Self::MqSend { .. }
            | Self::MqGroupCreate { .. }
            | Self::MqPoll { .. }
            | Self::MqAck { .. }
            | Self::MqInspect {}
            | Self::MqTail { .. }
            | Self::NotifyEvent { .. }
            | Self::NotifyOpenSessions {}
            | Self::NotifyGone { .. }
            | Self::SessionStarted { .. }
            | Self::SessionEnded { .. }
            | Self::SessionGone { .. }
            | Self::SessionList { .. }
            | Self::SessionState { .. } => OpPermission::Hook,
            Self::TaskSet {
                status: Some(_), ..
            } => OpPermission::TaskStatus,
            Self::TaskAdd(_)
            | Self::TaskSet { .. }
            | Self::TaskEdit { .. }
            | Self::TaskTag { .. }
            | Self::TaskDepend { .. }
            | Self::TaskUndepend { .. }
            | Self::TaskPlace { .. }
            | Self::TaskUnplace { .. }
            | Self::TaskBind { .. }
            | Self::IngestText(_)
            | Self::InvWrite(_)
            | Self::TaskLocate { .. }
            | Self::TaskPrRecord { .. } => OpPermission::TaskWrite,
            Self::TaskClaim { .. }
            | Self::TaskRelease { .. }
            | Self::TaskStart(_)
            | Self::TaskTake(_)
            | Self::TaskAbandon { .. }
            | Self::LeaseTake { .. }
            | Self::LeaseRelease { .. }
            | Self::RemovalAdd { .. }
            | Self::RemovalArchived { .. }
            | Self::RemovalCancel { .. }
            | Self::RemovalDrop { .. } => OpPermission::TaskClaim,
            Self::TaskReviewFile(_) | Self::TaskReviewRecord(_) => OpPermission::Review,
            Self::TaskLand { .. } | Self::TaskLanded { .. } | Self::TaskCloseMerged { .. } => {
                OpPermission::Land
            }
            Self::WorkflowFire { .. } | Self::WorkflowObserve { .. } => OpPermission::Workflow,
            Self::RoleGrant { .. } | Self::RoleRevoke { .. } => OpPermission::Grant,
            Self::AttestMint { .. } | Self::AttestRelease { .. } => OpPermission::Attest,
            // Waiving the review gate is the operator's escape hatch, whoever the strategy lets land.
            Self::TaskReviewWaive { .. }
            | Self::TaskReclaim { .. }
            | Self::LeaseBreak { .. }
            | Self::ItemRm { .. }
            | Self::NsMv { .. }
            | Self::MqCompact { .. }
            | Self::RoleMap { .. }
            | Self::RoleRotateContainer {}
            | Self::WorkflowSet { .. }
            | Self::WorkflowDefine { .. } => OpPermission::Admin,
        }
    }

    /// The one task (or item) this op writes, by reference, when it names one — what a scoped
    /// principal is held to. Reads are not scoped: reading is not what a role protects.
    #[must_use]
    pub fn target(&self) -> Option<&str> {
        match self {
            Self::TaskSet { uid, .. }
            | Self::TaskEdit { uid, .. }
            | Self::TaskTag { uid, .. }
            | Self::TaskDepend { uid, .. }
            | Self::TaskUndepend { uid, .. }
            | Self::TaskPlace { uid, .. }
            | Self::TaskUnplace { uid, .. }
            | Self::TaskBind { uid, .. }
            | Self::TaskClaim { uid, .. }
            | Self::TaskRelease { uid, .. }
            | Self::TaskLocate { uid, .. }
            | Self::TaskAbandon { uid, .. }
            | Self::TaskLand { uid, .. }
            | Self::TaskLanded { uid, .. }
            | Self::TaskReviewWaive { uid, .. }
            | Self::ItemRm { uid, .. }
            | Self::TaskPrRecord { uid, .. }
            | Self::TaskCloseMerged { uid, .. }
            | Self::WorkflowFire { uid, .. }
            | Self::WorkflowObserve { uid }
            | Self::WorkflowSet { uid, .. } => Some(uid),
            Self::TaskStart(ask) => Some(&ask.uid),
            Self::TaskTake(ask) => Some(&ask.uid),
            Self::TaskAdd(ask) => ask.under.as_deref(),
            Self::RoleGrant { task, .. } => task.as_deref(),
            _ => None,
        }
    }
}

// -------------------------------------------------------------------------------------------
// Tickets
// -------------------------------------------------------------------------------------------

/// The prefix that tells a ticket from a grant token.
pub const TICKET_PREFIX: &str = "t_";

/// How long a ticket lives if nothing releases it — the backstop for a Claude Code that crashed
/// before its `PostToolUse`, `SubagentStop` or `SessionEnd` (D52.9). The Bash tool's own maximum.
pub const TICKET_BACKSTOP: Duration = Duration::from_mins(10);

/// What a ticket carries: what the harness reported, and the credential that minted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    /// The Claude Code session.
    pub session: String,
    /// The subagent, when a subagent made the call.
    pub agent_id: Option<String>,
    /// Its type (the agent definition's `name`), as the harness reported it.
    pub agent_type: Option<String>,
    /// The tool call it was minted for.
    pub tool_use_id: String,
    /// The grant that minted it — the container credential.
    pub minted_by: i64,
    minted_at: Instant,
}

/// The daemon's live tickets, by token.
#[derive(Debug, Default)]
pub struct Tickets {
    live: Mutex<HashMap<String, Ticket>>,
}

impl Tickets {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Ticket>>, ApiError> {
        self.live
            .lock()
            .map_err(|_| ApiError::with_code(ErrorCode::Internal, "ticket store poisoned"))
    }

    /// The live ticket `token` names. An expired one is dropped on sight.
    ///
    /// # Errors
    /// [`ErrorCode::Internal`] for a poisoned store.
    pub fn get(&self, token: &str) -> Result<Option<Ticket>, ApiError> {
        let mut live = self.lock()?;
        live.retain(|_, t| t.minted_at.elapsed() < TICKET_BACKSTOP);
        Ok(live.get(token).cloned())
    }

    /// Whether `token` names a live ticket — for a daemon deciding whether a request authenticates.
    #[must_use]
    pub fn contains(&self, token: &str) -> bool {
        self.get(token).ok().flatten().is_some()
    }

    fn mint(&self, t: Ticket) -> Result<String, ApiError> {
        let token = format!(
            "{TICKET_PREFIX}{}",
            roles::fresh_token().map_err(ApiError::from)?
        );
        self.lock()?.insert(token.clone(), t);
        Ok(token)
    }

    /// Release tickets: the one for `tool_use_id` when given; else every ticket of `agent_id` in
    /// `session` when given; else every ticket of `session`. Returns how many.
    ///
    /// # Errors
    /// [`ErrorCode::Internal`] for a poisoned store.
    pub fn release(
        &self,
        session: &str,
        agent_id: Option<&str>,
        tool_use_id: Option<&str>,
    ) -> Result<usize, ApiError> {
        let mut live = self.lock()?;
        let before = live.len();
        live.retain(|_, t| {
            let hit = t.session == session
                && match (tool_use_id, agent_id) {
                    (Some(tool), _) => t.tool_use_id == tool,
                    (None, Some(agent)) => t.agent_id.as_deref() == Some(agent),
                    (None, None) => true,
                };
            !hit
        });
        Ok(before - live.len())
    }
}

// -------------------------------------------------------------------------------------------
// Principals
// -------------------------------------------------------------------------------------------

/// Who a backend serves: the operator, or the holder of a token it resolves per call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Caller {
    /// The operator: the host CLI's own process, or the daemon's root token.
    #[default]
    Operator,
    /// A grant token or a harness ticket, resolved on every call.
    Token(String),
}

/// What kind of principal it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalKind {
    /// The operator.
    Operator,
    /// A grant's holder.
    Grant(GrantRow),
    /// A harness-attested tool call.
    Ticket(Ticket),
}

/// A resolved caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The roles it holds. Empty for an attested subagent whose type the operator mapped to none.
    pub roles: Vec<Role>,
    /// The one task it may write, if it is scoped. An attested subagent with no binding yet is bound
    /// by its first task-targeted write.
    pub scope: Option<ItemId>,
    /// How it is recorded: `operator`, `grant:<id>`, `<agent_type>@<agent_id>`, `session:<id>`.
    pub label: String,
    /// What kind.
    pub kind: PrincipalKind,
}

impl Principal {
    /// The operator.
    #[must_use]
    pub fn operator() -> Self {
        Self {
            roles: vec![Role::Operator],
            scope: None,
            label: "operator".to_owned(),
            kind: PrincipalKind::Operator,
        }
    }

    /// The operator?
    #[must_use]
    pub fn is_operator(&self) -> bool {
        self.roles.contains(&Role::Operator)
    }

    /// As a workflow actor.
    #[must_use]
    pub fn actor(&self) -> Actor {
        Actor {
            roles: self.roles.clone(),
            principal: self.label.clone(),
        }
    }

    /// The attested subagent this is, if it is one: `(session, agent_id)`.
    fn attested_agent(&self) -> Option<(&str, &str)> {
        match &self.kind {
            PrincipalKind::Ticket(t) => t.agent_id.as_deref().map(|a| (t.session.as_str(), a)),
            _ => None,
        }
    }
}

fn unauthorized(why: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::Unauthorized, why)
}

fn forbidden(why: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::Forbidden, why)
}

fn invalid(why: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::Invalid, why)
}

/// Resolve `caller`. A token that names no live grant or ticket is `Unauthorized`.
///
/// # Errors
/// [`ErrorCode::Unauthorized`] for an unknown, revoked or expired token; a database error.
pub fn resolve(
    conn: &Connection,
    caller: &Caller,
    tickets: Option<&Tickets>,
) -> Result<Principal, ApiError> {
    let token = match caller {
        Caller::Operator => return Ok(Principal::operator()),
        Caller::Token(t) => t,
    };
    if token.starts_with(TICKET_PREFIX) {
        let ticket = tickets
            .map(|t| t.get(token))
            .transpose()?
            .flatten()
            .ok_or_else(|| {
                unauthorized(
                    "this tool call's attestation ticket is not live (released, expired, or minted \
                     by a jkb serve that has since restarted) — re-run the command",
                )
            })?;
        let minter = roles::get(conn, ticket.minted_by)?
            .filter(|g| g.revoked_at.is_none())
            .ok_or_else(|| unauthorized("the credential that minted this ticket was revoked"))?;
        return Ok(match &ticket.agent_id {
            None => Principal {
                roles: vec![minter.role],
                scope: minter.scope,
                label: format!("session:{}", ticket.session),
                kind: PrincipalKind::Ticket(ticket),
            },
            Some(agent) => {
                let role = match ticket.agent_type.as_deref() {
                    Some(t) => roles::role_for_agent_type(conn, t)?,
                    None => None,
                };
                // A mapped role never exceeds the credential that minted the ticket: the operator's
                // map cannot make a container subagent an operator.
                let role = role.filter(|r| *r != Role::Operator);
                Principal {
                    roles: role.into_iter().collect(),
                    scope: roles::agent_binding(conn, &ticket.session, agent)?,
                    label: format!(
                        "{}@{agent}",
                        ticket.agent_type.as_deref().unwrap_or("untyped")
                    ),
                    kind: PrincipalKind::Ticket(ticket),
                }
            }
        });
    }
    let grant = roles::resolve(conn, token)?
        .ok_or_else(|| unauthorized("an unknown or revoked jkb role token"))?;
    Ok(Principal {
        roles: vec![grant.role],
        scope: grant.scope,
        label: format!("grant:{}", grant.id),
        kind: PrincipalKind::Grant(grant),
    })
}

/// What authorization asks the dispatcher to do before the op runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// Run it.
    Run,
    /// Bind the attested subagent to this task first (its first task-targeted write), then run it.
    BindThenRun {
        /// The session.
        session: String,
        /// The subagent.
        agent_id: String,
        /// The task.
        task: ItemId,
    },
}

fn deny(no: &Refusal, op: &str) -> ApiError {
    forbidden(format!("`{op}` refused: {no}"))
}

/// May `principal` run `request`? The operator may run anything. Everyone else is held to
/// [`OP_GRANTS`], to its task scope for an op that writes one task, to the task's strategy for a
/// landing, and — for attestation — to being the container credential itself.
///
/// # Errors
/// [`ErrorCode::Forbidden`] naming why and who may; a database error.
pub fn authorize(
    conn: &Connection,
    principal: &Principal,
    request: &Request,
) -> Result<Admit, ApiError> {
    if principal.is_operator() {
        return Ok(Admit::Run);
    }
    let op = request.op();
    let permission = request.permission();
    if principal.roles.is_empty() {
        let why = match &principal.kind {
            PrincipalKind::Ticket(t) => format!(
                "this subagent's type `{}` holds no role — the operator maps agent types to roles \
                 (`jkb role map <type> <role>`); spawn workers with an explicit type \
                 (`subagent_type`, or `agentType` in a workflow script)",
                t.agent_type.as_deref().unwrap_or("(none reported)")
            ),
            _ => "this caller holds no role".to_owned(),
        };
        return Err(forbidden(format!("`{op}` refused: {why}")));
    }
    match permission {
        OpPermission::Attest => {
            let container = matches!(
                &principal.kind,
                PrincipalKind::Grant(g) if g.agent == roles::CONTAINER_AGENT && g.parent.is_none()
            );
            if !container {
                return Err(forbidden(format!(
                    "`{op}` refused: only the dev container's own credential mints attestation \
                     tickets — never a ticket or another grant"
                )));
            }
            return Ok(Admit::Run);
        }
        OpPermission::Land => {}
        _ => {
            if let Decision::Deny(no) = RoleBased::new(&OP_GRANTS)
                .decide(&principal.roles, Requirement::Permission(permission))
            {
                return Err(deny(&no, op));
            }
        }
    }
    let Some(reference) = request.target() else {
        if permission == OpPermission::Land {
            return Err(forbidden(format!("`{op}` refused: it names no task")));
        }
        return Ok(Admit::Run);
    };
    let Some(target) = task::resolve_ref(conn, reference)? else {
        // Nothing to scope against; the op itself answers `not_found`.
        return Ok(Admit::Run);
    };
    if permission == OpPermission::Land {
        let spec = wf::current(conn, target)?.spec;
        if let Decision::Deny(no) = spec.may_land(&principal.roles) {
            return Err(deny(&no, op));
        }
    }
    match (principal.scope, principal.attested_agent()) {
        (Some(scope), _) => {
            if !roles::in_scope(conn, scope, target)? {
                return Err(forbidden(format!(
                    "`{op}` refused: {} is scoped to another task, and `{reference}` is not it, one \
                     of its subtasks, or one of its findings",
                    principal.label
                )));
            }
            Ok(Admit::Run)
        }
        (None, Some((session, agent))) => Ok(Admit::BindThenRun {
            session: session.to_owned(),
            agent_id: agent.to_owned(),
            task: target,
        }),
        (None, None) => Ok(Admit::Run),
    }
}

// -------------------------------------------------------------------------------------------
// The ops
// -------------------------------------------------------------------------------------------

/// A grant as the role ops report it — never its token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantInfo {
    /// Its id.
    pub id: i64,
    /// Its role.
    pub role: String,
    /// Who it was handed to.
    pub agent: String,
    /// The task it is scoped to, by uid.
    pub task: Option<String>,
    /// The grant that minted it.
    pub parent: Option<i64>,
    /// When.
    pub granted_at: String,
    /// When revoked.
    pub revoked_at: Option<String>,
}

fn info(conn: &Connection, g: GrantRow) -> Result<GrantInfo, ApiError> {
    let task = match g.scope {
        Some(id) => jkb_core::item::get(conn, id)?.map(|m| m.uid),
        None => None,
    };
    Ok(GrantInfo {
        id: g.id,
        role: g.role.as_str().to_owned(),
        agent: g.agent,
        task,
        parent: g.parent,
        granted_at: g.granted_at,
        revoked_at: g.revoked_at,
    })
}

fn task_id(conn: &Connection, reference: &str) -> Result<ItemId, ApiError> {
    task::resolve_ref(conn, reference)?
        .ok_or_else(|| ApiError::with_code(ErrorCode::NotFound, format!("no task {reference}")))
}

/// `role.grant`: mint a grant. The operator mints any role; a grant holder only what
/// [`roles::GRANTABLE`] lets its role grant, within its own scope; the main session of an attested
/// container session mints through the credential that minted its ticket.
///
/// # Errors
/// [`ErrorCode::Forbidden`] for a refused grant; [`ErrorCode::Invalid`] for a malformed one.
pub fn grant(
    conn: &Connection,
    meta: &WriteMeta,
    principal: &Principal,
    role: &str,
    task: Option<&str>,
    agent: &str,
) -> Result<(GrantInfo, String), ApiError> {
    let role = Role::parse(role).map_err(ApiError::from)?;
    let scope = task.map(|t| task_id(conn, t)).transpose()?;
    let minter_row;
    let minter = match &principal.kind {
        PrincipalKind::Operator => Minter::Operator,
        PrincipalKind::Grant(g) => Minter::Grant(g),
        PrincipalKind::Ticket(t) if t.agent_id.is_none() => {
            minter_row = roles::get(conn, t.minted_by)?
                .ok_or_else(|| forbidden("the minting credential is gone"))?;
            Minter::Grant(&minter_row)
        }
        PrincipalKind::Ticket(_) => {
            return Err(forbidden("a subagent mints no grants"));
        }
    };
    let (row, token) = roles::mint(conn, meta, minter, role, agent, scope)
        .map_err(|e| forbidden(e.to_string()))?;
    Ok((info(conn, row)?, token))
}

/// `role.revoke`: the operator revokes anything; a grant holder only what it (transitively) minted.
///
/// # Errors
/// [`ErrorCode::Forbidden`] or [`ErrorCode::NotFound`].
pub fn revoke(
    conn: &Connection,
    meta: &WriteMeta,
    principal: &Principal,
    id: i64,
) -> Result<usize, ApiError> {
    if roles::get(conn, id)?.is_none() {
        return Err(ApiError::with_code(
            ErrorCode::NotFound,
            format!("no grant {id}"),
        ));
    }
    let own = match &principal.kind {
        PrincipalKind::Operator => None,
        PrincipalKind::Grant(g) => Some(g.id),
        PrincipalKind::Ticket(t) if t.agent_id.is_none() => Some(t.minted_by),
        PrincipalKind::Ticket(_) => return Err(forbidden("a subagent revokes nothing")),
    };
    if let Some(own) = own {
        if id == own || !roles::descends_from(conn, id, own)? {
            return Err(forbidden(format!(
                "grant {id} was not minted by this caller's credential"
            )));
        }
    }
    Ok(roles::revoke(conn, meta, id)?)
}

/// What `role.list` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantListing {
    /// The grants, newest first.
    pub grants: Vec<GrantInfo>,
    /// The operator's `agent_type → role` map.
    pub agent_types: Vec<(String, String)>,
}

/// `role.list`.
///
/// # Errors
/// A database error.
pub fn list(conn: &Connection, task: Option<&str>, all: bool) -> Result<GrantListing, ApiError> {
    let scope = task.map(|t| task_id(conn, t)).transpose()?;
    let grants = roles::list(conn, scope, all)?
        .into_iter()
        .map(|g| info(conn, g))
        .collect::<Result<_, _>>()?;
    Ok(GrantListing {
        grants,
        agent_types: roles::agent_type_map(conn)?
            .into_iter()
            .map(|(t, r)| (t, r.as_str().to_owned()))
            .collect(),
    })
}

/// Who the caller is, as `role.whoami` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoAmI {
    /// How it is recorded.
    pub label: String,
    /// Its roles.
    pub roles: Vec<String>,
    /// The task it is scoped to, by uid.
    pub task: Option<String>,
}

/// `role.whoami`.
///
/// # Errors
/// A database error.
pub fn whoami(conn: &Connection, principal: &Principal) -> Result<WhoAmI, ApiError> {
    Ok(WhoAmI {
        label: principal.label.clone(),
        roles: principal
            .roles
            .iter()
            .map(|r| r.as_str().to_owned())
            .collect(),
        task: match principal.scope {
            Some(id) => jkb_core::item::get(conn, id)?.map(|m| m.uid),
            None => None,
        },
    })
}

/// `role.map` (operator): map an agent type to a role, or clear it.
///
/// # Errors
/// [`ErrorCode::Invalid`] for an unknown role or `operator`, which no subagent may hold.
pub fn map(
    conn: &Connection,
    meta: &WriteMeta,
    agent_type: &str,
    role: Option<&str>,
) -> Result<(), ApiError> {
    let role = role.map(Role::parse).transpose().map_err(ApiError::from)?;
    if role == Some(Role::Operator) {
        return Err(invalid("no agent type may be mapped to operator"));
    }
    Ok(roles::map_agent_type(conn, meta, agent_type, role)?)
}

/// A task's workflow, as `workflow.show` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowView {
    /// The task.
    pub uid: String,
    /// Its phase.
    pub phase: String,
    /// Where its strategy came from.
    pub source: String,
    /// The strategy.
    pub spec: serde_json::Value,
    /// Who acts next.
    pub next_role: String,
    /// What they do.
    pub next_step: String,
    /// Events the caller may fire from here, that the graph accepts.
    pub may_fire: Vec<String>,
    /// Whether the caller is the one who acts next (or the operator).
    pub caller_acts_next: bool,
    /// The effective permission table, as Markdown.
    pub matrix: String,
    /// The history, oldest first: `at event from->to by actor (reason)`.
    pub history: Vec<String>,
}

/// `workflow.show`.
///
/// # Errors
/// [`ErrorCode::NotFound`]; a database error.
pub fn show(conn: &Connection, principal: &Principal, uid: &str) -> Result<WorkflowView, ApiError> {
    let id = task_id(conn, uid)?;
    let cur = wf::current(conn, id)?;
    let machine = cur.spec.graph.machine();
    let (next, step) = cur.spec.next_actor(cur.phase);
    let may_fire = machine
        .accepted_from(cur.phase)
        .into_iter()
        .filter(|e| {
            jkb_fsm::Event::kind(*e) == jkb_fsm::EventKind::Applied
                && cur.spec.authorize(&principal.roles, *e).is_allowed()
        })
        .map(|e| e.as_str().to_owned())
        .collect();
    let meta = jkb_core::item::get(conn, id)?
        .ok_or_else(|| ApiError::with_code(ErrorCode::NotFound, format!("no task {uid}")))?;
    Ok(WorkflowView {
        uid: meta.uid.clone(),
        phase: cur.phase.as_str().to_owned(),
        source: cur.source.clone(),
        spec: serde_json::to_value(&cur.spec)
            .map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))?,
        next_role: next.as_str().to_owned(),
        next_step: step.replace("<uid>", &meta.uid),
        may_fire,
        caller_acts_next: principal.is_operator() || principal.roles.contains(&next),
        matrix: cur.spec.permissions().matrix(),
        history: cur
            .history
            .iter()
            .map(|r| {
                let mut line = format!(
                    "{} {} {}->{} by {} ({})",
                    r.at,
                    r.event,
                    r.from_phase.as_deref().unwrap_or("-"),
                    r.to_phase,
                    r.actor,
                    r.role
                );
                if let Some(reason) = r.reason.as_deref().filter(|_| r.spec.is_none()) {
                    line.push_str(": ");
                    line.push_str(reason);
                }
                line
            })
            .collect(),
    })
}

/// What a workflow move did, on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowMove {
    /// It moved.
    pub moved: bool,
    /// The phase it is in now.
    pub phase: String,
    /// The event that fired, when one did.
    pub event: Option<String>,
    /// Why nothing moved, when it was refused.
    pub refusal: Option<String>,
    /// Worker grants revoked because the task settled.
    pub revoked: usize,
}

fn to_wire(moved: Moved) -> WorkflowMove {
    match moved {
        Moved::To {
            event, to, revoked, ..
        } => WorkflowMove {
            moved: true,
            phase: to.as_str().to_owned(),
            event: Some(event.as_str().to_owned()),
            refusal: None,
            revoked,
        },
        Moved::AlreadyThere(p) => WorkflowMove {
            moved: false,
            phase: p.as_str().to_owned(),
            event: None,
            refusal: None,
            revoked: 0,
        },
        Moved::Refused(why) => WorkflowMove {
            moved: false,
            phase: String::new(),
            event: None,
            refusal: Some(why),
            revoked: 0,
        },
    }
}

/// `workflow.fire`.
///
/// # Errors
/// [`ErrorCode::Invalid`] for an unknown event or phase, or a missing reason.
pub fn fire(
    conn: &Connection,
    meta: &WriteMeta,
    principal: &Principal,
    uid: &str,
    event: &str,
    reason: Option<&str>,
    to: Option<&str>,
) -> Result<WorkflowMove, ApiError> {
    let id = task_id(conn, uid)?;
    let event = WorkflowEvent::parse(event).ok_or_else(|| {
        invalid(format!(
            "no workflow event `{event}` (events: {})",
            <WorkflowEvent as jkb_fsm::Event>::ALL
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let stated = to
        .map(|p| Phase::parse(p).ok_or_else(|| invalid(format!("no phase `{p}`"))))
        .transpose()?;
    let mut out = to_wire(wf::fire(
        conn,
        meta,
        id,
        event,
        &principal.actor(),
        reason,
        stated,
    )?);
    if out.phase.is_empty() {
        wf::current(conn, id)?
            .phase
            .as_str()
            .clone_into(&mut out.phase);
    }
    Ok(out)
}

/// `workflow.observe`.
///
/// # Errors
/// [`ErrorCode::NotFound`]; a database error.
pub fn observe(
    conn: &Connection,
    meta: &WriteMeta,
    principal: &Principal,
    uid: &str,
) -> Result<WorkflowMove, ApiError> {
    let id = task_id(conn, uid)?;
    let mut out = to_wire(wf::observe(conn, meta, id, &principal.actor())?);
    if out.phase.is_empty() {
        wf::current(conn, id)?
            .phase
            .as_str()
            .clone_into(&mut out.phase);
    }
    Ok(out)
}

/// A strategy as `workflow.strategies` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyInfo {
    /// Its name (`name@version` for a definition).
    pub name: String,
    /// A preset, or an operator definition.
    pub preset: bool,
    /// One line.
    pub describe: String,
    /// Its spec.
    pub spec: serde_json::Value,
}

/// `workflow.strategies`: the presets, then the operator's definitions, and which is the default.
///
/// # Errors
/// A database error.
pub fn strategies(conn: &Connection) -> Result<(Vec<StrategyInfo>, String), ApiError> {
    let json = |s: &StrategySpec| {
        serde_json::to_value(s).map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))
    };
    let mut out = Vec::new();
    for p in PRESETS {
        out.push(StrategyInfo {
            name: p.name.to_owned(),
            preset: true,
            describe: p.describe.to_owned(),
            spec: json(&(p.spec)())?,
        });
    }
    for (name, version, spec) in wf::definitions(conn)? {
        let spec = StrategySpec::from_json(&spec)?;
        out.push(StrategyInfo {
            name: format!("{name}@{version}"),
            preset: false,
            describe: spec.graph.describe().to_owned(),
            spec: json(&spec)?,
        });
    }
    Ok((out, wf::default_strategy(conn)?.0))
}

/// `workflow.define` (operator).
///
/// # Errors
/// [`ErrorCode::Invalid`] for a spec that does not parse or validate.
pub fn define(
    conn: &Connection,
    meta: &WriteMeta,
    name: &str,
    spec: &serde_json::Value,
) -> Result<i64, ApiError> {
    let spec = StrategySpec::from_json(&spec.to_string())?;
    Ok(wf::define(conn, meta, name, &spec)?)
}

/// `attest.mint`: a ticket for one tool call, carrying what the harness reported. Only the container
/// credential gets here ([`authorize`]).
///
/// # Errors
/// [`ErrorCode::Invalid`] for malformed identifiers.
pub fn mint_ticket(
    tickets: &Tickets,
    principal: &Principal,
    session: &str,
    agent_id: Option<&str>,
    agent_type: Option<&str>,
    tool_use_id: &str,
) -> Result<String, ApiError> {
    let ok = |v: &str| {
        !v.is_empty()
            && v.len() <= 128
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    };
    for v in [Some(session), agent_id, agent_type, Some(tool_use_id)]
        .into_iter()
        .flatten()
    {
        if !ok(v) {
            return Err(invalid(format!("a malformed attestation field `{v}`")));
        }
    }
    let PrincipalKind::Grant(g) = &principal.kind else {
        return Err(forbidden("only the container credential mints tickets"));
    };
    tickets.mint(Ticket {
        session: session.to_owned(),
        agent_id: agent_id.map(str::to_owned),
        agent_type: agent_type.map(str::to_owned),
        tool_use_id: tool_use_id.to_owned(),
        minted_by: g.id,
        minted_at: Instant::now(),
    })
}

/// A backend's shared ticket store, for the daemon to hand every per-request backend.
pub type SharedTickets = Arc<Tickets>;

#[cfg(test)]
mod tests;
