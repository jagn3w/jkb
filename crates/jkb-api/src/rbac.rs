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

use jkb_fsm::State as _;
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
    /// Write a design document: create one, edit or merge into it, make or stage a span (D53.4).
    Design,
    /// Approve a design span — and only as the reviewer the span names, which the op itself holds
    /// it to (D53.5).
    DesignApprove,
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
        Self::Design,
        Self::DesignApprove,
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
            Self::Design => "design",
            Self::DesignApprove => "design_approve",
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
                // Not `Review` (round 3): a coordinator drives the work, and one that could file an
                // empty round and record it against the branch it drove satisfied the land gate
                // with no reviewer involved. A round is a reviewer's, or the operator's.
                OpPermission::Workflow,
                OpPermission::Grant,
                OpPermission::Design,
                OpPermission::DesignApprove,
            ],
        },
        Grant {
            role: Role::Designer,
            permits: &[
                OpPermission::Read,
                OpPermission::Hook,
                OpPermission::TaskWrite,
                OpPermission::Workflow,
                // The design is the designer's to write, and a span naming `claude` is a Claude
                // session's to approve (D53.5).
                OpPermission::Design,
                OpPermission::DesignApprove,
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
                OpPermission::DesignApprove,
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
            | Self::RoleBind { .. }
            | Self::WorkflowShow { .. }
            | Self::WorkflowStrategies {}
            | Self::DesignList { .. }
            | Self::DesignCat { .. }
            | Self::DesignState { .. }
            | Self::DesignSpans { .. } => OpPermission::Read,
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
            Self::TaskLand { .. }
            | Self::TaskLandCheck { .. }
            | Self::TaskLanded { .. }
            | Self::TaskCloseMerged { .. } => OpPermission::Land,
            Self::WorkflowFire { .. } | Self::WorkflowObserve { .. } => OpPermission::Workflow,
            Self::RoleGrant { .. } | Self::RoleRevoke { .. } => OpPermission::Grant,
            Self::DesignCreate { .. }
            | Self::DesignApply { .. }
            | Self::DesignEdit { .. }
            | Self::DesignSpan { .. }
            | Self::DesignStage { .. } => OpPermission::Design,
            Self::DesignApprove { .. } => OpPermission::DesignApprove,
            Self::AttestMint { .. } | Self::AttestRelease { .. } => OpPermission::Attest,
            // Waiving the review gate is the operator's escape hatch, whoever the strategy lets land.
            Self::TaskReviewWaive { .. }
            | Self::TaskRanOnHost { .. }
            | Self::TaskReclaim { .. }
            | Self::LeaseBreak { .. }
            | Self::ItemRm { .. }
            | Self::NsMv { .. }
            | Self::MqCompact { .. }
            | Self::RoleMap { .. }
            | Self::RoleRotateContainer { .. }
            | Self::WorkflowSet { .. }
            | Self::WorkflowDefine { .. }
            | Self::DesignCompact { .. } => OpPermission::Admin,
        }
    }

    /// What a principal held to one task is held to for this op (D52.4). **Exhaustive, with no
    /// wildcard**, like [`Request::permission`]: a new op must say what it writes before it compiles.
    /// A `_ => None` arm here once admitted every write op nobody had listed — `removal.add` naming
    /// another task's worktree, `lease.take` displacing the merge queue — as unscoped.
    #[must_use]
    #[allow(clippy::too_many_lines)] // one arm per op, like `permission`
    pub fn target(&self) -> Target<'_> {
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
            | Self::TaskLandCheck { uid, .. }
            | Self::TaskLanded { uid, .. }
            | Self::TaskReviewWaive { uid, .. }
            | Self::TaskRanOnHost { uid, .. }
            | Self::ItemRm { uid, .. }
            | Self::TaskPrRecord { uid, .. }
            | Self::TaskCloseMerged { uid, .. }
            | Self::WorkflowFire { uid, .. }
            | Self::WorkflowObserve { uid }
            | Self::WorkflowSet { uid, .. }
            | Self::RoleBind { uid } => Target::Task(uid),
            Self::TaskStart(ask) => Target::Task(&ask.uid),
            Self::TaskTake(ask) => Target::Task(&ask.uid),
            // A new top-level task is where a scope would otherwise stop.
            Self::TaskAdd(ask) => ask.under.as_deref().map_or(Target::Shared, Target::Task),
            Self::RoleGrant { task, .. } => task.as_deref().map_or(Target::Shared, Target::Task),
            // What no one task owns: a namespace, an item outside the task tree, a repo's land lease,
            // a worktree removal (its path and branch are its own fields, whatever uid it names), and
            // every operator-only op.
            Self::IngestText(_)
            | Self::InvWrite(_)
            | Self::LeaseTake { .. }
            | Self::LeaseRelease { .. }
            | Self::LeaseBreak { .. }
            | Self::RemovalAdd { .. }
            | Self::RemovalArchived { .. }
            | Self::RemovalCancel { .. }
            | Self::RemovalDrop { .. }
            | Self::TaskReclaim { .. }
            | Self::NsMv { .. }
            | Self::MqCompact { .. }
            | Self::RoleMap { .. }
            | Self::RoleRotateContainer { .. }
            | Self::WorkflowDefine { .. }
            // A design is no one task's.
            | Self::DesignCreate { .. }
            | Self::DesignApply { .. }
            | Self::DesignEdit { .. }
            | Self::DesignSpan { .. }
            | Self::DesignApprove { .. }
            | Self::DesignStage { .. }
            | Self::DesignCompact { .. } => Target::Shared,
            // Held by their own callee: a filing writes only a namespace nobody holds, and becomes
            // a task's round only when recorded; recording holds a scoped caller to its task
            // (`review::record`); revoking holds a grant to what it minted; attesting is the
            // container credential's alone.
            Self::TaskReviewFile(_)
            | Self::TaskReviewRecord(_)
            | Self::RoleRevoke { .. }
            | Self::AttestMint { .. }
            | Self::AttestRelease { .. }
            // Reads, and the hooks' own records.
            | Self::KbAmbient { .. }
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
            | Self::WorkflowStrategies {}
            | Self::DesignList { .. }
            | Self::DesignCat { .. }
            | Self::DesignState { .. }
            | Self::DesignSpans { .. }
            | Self::MqTopicCreate { .. }
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
            | Self::SessionState { .. } => Target::Free,
        }
    }
}

/// What an op writes, as far as a task scope is concerned ([`Request::target`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
    /// Nothing a task scope protects, or something its own callee holds to the scope.
    Free,
    /// This one task, by reference.
    Task(&'a str),
    /// State no one task owns — refused to a principal held to one task.
    Shared,
}

/// Whether a principal held to `scope` may put a task in namespace `ns`: only where `scope` itself is
/// placed. A review round's namespace is never one — a scoped caller filing a line into its own
/// older round, or a `!p0` into another task's, was the rest of what D52.4's scope left open.
///
/// # Errors
/// [`ErrorCode::Forbidden`] naming the namespace, or a database error.
pub fn check_destination(conn: &Connection, scope: ItemId, ns: &str) -> Result<(), ApiError> {
    let ns = jkb_core::ns::normalize(ns)?;
    if let Some(round) = jkb_core::reviews::review_namespace_containing(conn, &ns)? {
        return Err(forbidden(format!(
            "a caller held to one task places tasks only where that task is, and never in a review \
             round (`{ns}` is in `{round}`)"
        )));
    }
    let homes = jkb_core::item::namespaces_of(conn, scope)?;
    if homes.contains(&ns) {
        return Ok(());
    }
    Err(forbidden(format!(
        "a caller held to one task places tasks only where that task is ({}), not in `{ns}`",
        if homes.is_empty() {
            "nowhere".to_owned()
        } else {
            homes.join(", ")
        }
    )))
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

/// The most tickets live at once, daemon-wide. A ticket lives for one tool call, and a session holds
/// one per call in flight, so this is far past real use — it bounds what a credential holder that
/// mints and never releases can pile up (each ticket is otherwise kept until [`TICKET_BACKSTOP`]).
pub const MAX_LIVE_TICKETS: usize = 4096;

/// The most tickets one session holds live at once.
pub const MAX_SESSION_TICKETS: usize = 256;

/// The daemon's live tickets, by token.
#[derive(Debug, Default)]
pub struct Tickets {
    live: Mutex<Live>,
    /// One lock per attested subagent, `(session, agent_id)`, held across a call's admission, its op
    /// and the undo of a binding that op's failure owes ([`Tickets::agent_lock`]).
    agents: Mutex<HashMap<(String, String), AgentLock>>,
}

/// One attested subagent's lock ([`Tickets::agent_lock`]).
pub type AgentLock = Arc<Mutex<()>>;

/// The tickets, and the order they expire in. Tickets are minted with a monotonic clock and all live
/// for the same [`TICKET_BACKSTOP`], so mint order **is** expiry order: expiring is popping from the
/// front, never a scan of every ticket on every lookup.
#[derive(Debug, Default)]
struct Live {
    by_token: HashMap<String, Ticket>,
    expiry: std::collections::VecDeque<(Instant, String)>,
}

impl Live {
    fn expire(&mut self) {
        while let Some((at, _)) = self.expiry.front() {
            if at.elapsed() < TICKET_BACKSTOP {
                break;
            }
            if let Some((_, token)) = self.expiry.pop_front() {
                self.by_token.remove(&token);
            }
        }
        // A released ticket's queue entry outlives it; drop those once they are most of the queue,
        // so the queue stays proportional to the live set.
        if self.expiry.len() > 2 * self.by_token.len() + 64 {
            let by_token = &self.by_token;
            self.expiry.retain(|(_, t)| by_token.contains_key(t));
        }
    }
}

impl Tickets {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Live>, ApiError> {
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
        live.expire();
        Ok(live.by_token.get(token).cloned())
    }

    /// Whether `token` names a live ticket — for a daemon deciding whether a request authenticates.
    #[must_use]
    pub fn contains(&self, token: &str) -> bool {
        self.get(token).ok().flatten().is_some()
    }

    /// The lock of the attested subagent `token` names, if it names one. Held by a call for as long as
    /// it may bind that subagent and undo the binding: a binding is then removed only by the call that
    /// made it, with no other call of the same subagent in flight — undone unconditionally before, a
    /// failed first op could delete the binding a concurrent, successful one had just relied on, and
    /// the worker could bind a second task.
    ///
    /// # Errors
    /// [`ErrorCode::Internal`] for a poisoned store.
    pub fn agent_lock(&self, token: &str) -> Result<Option<AgentLock>, ApiError> {
        let Some(t) = self.get(token)? else {
            return Ok(None);
        };
        let Some(agent) = t.agent_id else {
            return Ok(None);
        };
        let mut agents = self
            .agents
            .lock()
            .map_err(|_| ApiError::with_code(ErrorCode::Internal, "agent locks poisoned"))?;
        // Bounded like the tickets beside it: once the map outgrows the subagents that hold a live
        // ticket, drop the rest — a subagent whose tickets all expired unreleased holds nothing a
        // lock could serialize. (Lock order: agents, then live; nothing takes them the other way.)
        if agents.len() > 2 * MAX_SESSION_TICKETS {
            let live = self.lock()?;
            agents.retain(|(s, a), lock| {
                Arc::strong_count(lock) > 1
                    || live
                        .by_token
                        .values()
                        .any(|t| &t.session == s && t.agent_id.as_deref() == Some(a))
            });
        }
        Ok(Some(Arc::clone(
            agents.entry((t.session, agent)).or_default(),
        )))
    }

    #[cfg(test)]
    fn agent_locks(&self) -> usize {
        self.agents.lock().map_or(0, |a| a.len())
    }

    fn mint(&self, t: Ticket) -> Result<String, ApiError> {
        let token = format!(
            "{TICKET_PREFIX}{}",
            roles::fresh_token().map_err(ApiError::from)?
        );
        let mut live = self.lock()?;
        live.expire();
        let busy = |why: String| ApiError::with_code(ErrorCode::Busy, why);
        if live.by_token.len() >= MAX_LIVE_TICKETS {
            return Err(busy(format!(
                "{MAX_LIVE_TICKETS} attestation tickets are live — something mints them without \
                 releasing them"
            )));
        }
        let held = live
            .by_token
            .values()
            .filter(|x| x.session == t.session)
            .count();
        if held >= MAX_SESSION_TICKETS {
            return Err(busy(format!(
                "session `{}` holds {MAX_SESSION_TICKETS} live attestation tickets — its tool calls \
                 are not releasing them",
                t.session
            )));
        }
        live.expiry.push_back((t.minted_at, token.clone()));
        live.by_token.insert(token.clone(), t);
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
        let before = live.by_token.len();
        live.by_token.retain(|_, t| {
            let hit = t.session == session
                && match (tool_use_id, agent_id) {
                    (Some(tool), _) => t.tool_use_id == tool,
                    (None, Some(agent)) => t.agent_id.as_deref() == Some(agent),
                    (None, None) => true,
                };
            !hit
        });
        let released = before - live.by_token.len();
        drop(live);
        // A subagent that stopped, or a session that ended, needs no lock any more.
        if tool_use_id.is_none() {
            if let Ok(mut agents) = self.agents.lock() {
                agents.retain(|(s, a), _| !(s == session && agent_id.is_none_or(|want| want == a)));
            }
        }
        Ok(released)
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

/// The task `request` is held to, or — for an op that names none — its admission, or its refusal
/// for a principal held to one task.
fn reference_of<'r>(
    principal: &Principal,
    request: &'r Request,
    permission: OpPermission,
) -> Result<Result<&'r str, Admit>, ApiError> {
    let op = request.op();
    let scoped = principal.scope.is_some() || principal.attested_agent().is_some();
    Ok(match request.target() {
        Target::Task(r) => Ok(r),
        Target::Shared if scoped => {
            let hint = if matches!(request, Request::TaskAdd(_)) {
                ", and adds tasks only under it (`--under <uid>`)"
            } else {
                ": this writes state no one task owns"
            };
            return Err(forbidden(format!(
                "`{op}` refused: {} is held to one task{hint}",
                principal.label
            )));
        }
        Target::Shared | Target::Free => {
            if permission == OpPermission::Land {
                return Err(forbidden(format!("`{op}` refused: it names no task")));
            }
            // An attested subagent records a review only once it is bound: a review is keyed by
            // branch, and `review::record` holds a scoped caller to its task — which an unbound one
            // does not have.
            // Filing too: a filing's home is judged against the task the filer is held to.
            if principal.scope.is_none()
                && principal.attested_agent().is_some()
                && matches!(
                    request,
                    Request::TaskReviewRecord(_) | Request::TaskReviewFile(_)
                )
            {
                return Err(forbidden(format!(
                    "`{op}` refused: {} has not bound to the task it reviews — run `jkb role bind \
                     <uid>` first",
                    principal.label
                )));
            }
            Err(Admit::Run)
        }
    })
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
                PrincipalKind::Grant(g) if g.container
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
                let mut e = deny(&no, op);
                if permission == OpPermission::Review {
                    e.message.push_str(
                        ". A review round is filed and recorded by a reviewer — a \
                         `reviewer`-typed subagent, recording its own filing — or by the operator \
                         on the host (`jkb task review record --branch <b> --findings <ns>`)",
                    );
                }
                return Err(e);
            }
        }
    }
    let reference = match reference_of(principal, request, permission)? {
        Ok(r) => r,
        Err(admit) => return Ok(admit),
    };
    let Some(target) = task::resolve_ref(conn, reference)? else {
        // Nothing to scope against; the op itself answers `not_found`.
        return Ok(Admit::Run);
    };
    if permission == OpPermission::Land {
        let current = wf::current(conn, target)?;
        if let Decision::Deny(no) = current.spec.may_land(&principal.roles) {
            return Err(deny(&no, op));
        }
        // A workflow parked at `landed`/`cancelled` is picked back up by the operator alone
        // (`reopen`), so nobody else lands the task again first — reopening its status and landing
        // it would re-land a finished task with its workflow never reopened (review round 3).
        // Except the same landing again: a merge queue re-running a branch must be told the task
        // landed (round 5) — asked about the branch and destination its live landing already
        // records. Anything else — a cancelled task ticked
        // `done`, another destination — would record a landing it never had (round 6).
        if current.phase.is_settled() && !repeats_landing(conn, target, current.phase, request)? {
            return Err(forbidden(format!(
                "`{op}` refused: {reference}'s workflow is parked at `{}`, and only the operator \
                 picks it back up (`jkb task set {reference} --status open`, then `jkb workflow \
                 fire {reference} reopen`). A merge queue records the branch's other tasks and \
                 reports this one held.",
                current.phase.as_str()
            )));
        }
    }
    let admit = match (principal.scope, principal.attested_agent()) {
        (Some(scope), _) => {
            if !roles::in_scope(conn, scope, target)? {
                return Err(forbidden(format!(
                    "`{op}` refused: {} is scoped to another task, and `{reference}` is not it, one \
                     of its subtasks, or one of its findings",
                    principal.label
                )));
            }
            Admit::Run
        }
        (None, Some((session, agent))) => Admit::BindThenRun {
            session: session.to_owned(),
            agent_id: agent.to_owned(),
            task: target,
        },
        (None, None) => Admit::Run,
    };
    // Once, after scope and before any admit, so no arm (and no arm added later) can skip it.
    closing_unstarted(conn, target, request, reference)?;
    Ok(admit)
}

/// Refuse a non-operator `task.set --status done` on an `open` task; the operator returned earlier.
///
/// # Errors
/// [`ErrorCode::Forbidden`] for that write; a database error.
///
/// Closing a task nobody started is the operator's. A task's work closes it — its landing
/// (`task.landed`, `observed_landed`) or the session that started it — and an `open` task has had
/// neither, so a `done` here is a claim with nothing behind it that unblocks every dependent at
/// once. That is what a swarm's "mark the group done" agent did twice on 2026-10-08: each time,
/// after the merge queue had already closed its group, it set the NEXT task in the chain `done`,
/// and the swarm then started that task's dependents on work that did not exist. In the callee,
/// because a rule each agent prompt must remember is the defect.
fn closing_unstarted(
    conn: &Connection,
    target: ItemId,
    request: &Request,
    reference: &str,
) -> Result<(), ApiError> {
    let op = request.op();
    if let Request::TaskSet {
        status: Some(status),
        ..
    } = request
    {
        let now = jkb_core::item::get(conn, target)?.and_then(|m| m.status);
        if status == jkb_types::TaskStatus::Done.as_str()
            && now.as_deref() == Some(jkb_types::TaskStatus::Open.as_str())
        {
            return Err(forbidden(format!(
                "`{op}` refused: {reference} is `open` — nobody has started it, so `done` would \
                 close work that never happened and release its dependents. A task closes when its \
                 work lands (the merge queue's `jkb task landed`) or when the session that started \
                 it finishes; to close it anyway, the operator runs `jkb task set {reference} \
                 --status done` on the host, or `--status cancelled` if it will not be done"
            )));
        }
    }
    Ok(())
}

/// Whether `request` lands `task` exactly as its live landing already records — a workflow parked at
/// `landed`, the same branch onto the same destination at the same head — which the lifecycle answers
/// with a no-op. Parked at `landed`, because a live landing row is not proof of one: a landing the
/// guard **held** (an open subtask) still records a row, and a task then cancelled and ticked `done`
/// kept it live (review round 7). The same head, because new commits on the same branch are new work
/// (round 7).
fn repeats_landing(
    conn: &Connection,
    task: ItemId,
    phase: jkb_core::workflow::Phase,
    request: &Request,
) -> Result<bool, ApiError> {
    let (Request::TaskLand { landed, .. }
    | Request::TaskLandCheck { landed, .. }
    | Request::TaskLanded { landed, .. }) = request
    else {
        return Ok(false);
    };
    if phase != jkb_core::workflow::Phase::Landed {
        return Ok(false);
    }
    let landing = jkb_core::transition::landing(conn, task)?;
    Ok(landing.live().is_some_and(|row| {
        row.labels.onto.as_deref() == Some(landed.onto.as_str())
            && row.labels.branch.as_deref() == Some(landed.branch.as_str())
            && match (&row.labels.ref_commit, &landed.head) {
                (Some(was), Some(now)) => was == now,
                _ => true,
            }
    }))
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
    /// Whether its minter may still grant its role — `false` for one minted before the table was
    /// tightened, which no longer authenticates although it was never revoked.
    #[serde(default = "yes")]
    pub grantable: bool,
}

const fn yes() -> bool {
    true
}

fn info(conn: &Connection, g: GrantRow) -> Result<GrantInfo, ApiError> {
    let task = match g.scope {
        Some(id) => jkb_core::item::get(conn, id)?.map(|m| m.uid),
        None => None,
    };
    let grantable = roles::grantable_now(conn, &g)?;
    Ok(GrantInfo {
        grantable,
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
    let (mut next, mut step) = cur.spec.next_actor(cur.phase);
    // Settled, with the task itself back to work: the operator's `reopen` is what is next, not
    // "nothing".
    if cur.phase.is_settled() {
        let status = jkb_core::item::get(conn, id)?.and_then(|m| m.status);
        if !jkb_types::TaskStatus::is_terminal_str(status.as_deref()) {
            (next, step) = (
                jkb_core::roles::Role::Operator,
                "the task is back to work but its workflow is parked: `jkb workflow fire <uid> \
                 reopen`",
            );
        }
    }
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
