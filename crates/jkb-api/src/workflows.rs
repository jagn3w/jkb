//! The Workflows tab's ops (design D53.7): agent templates (`workflow.agents`, `workflow.agent`,
//! `workflow.agent_copy`, `workflow.agent_set`) over [`jkb_core::workflow::agents`], and a strategy's
//! machines as data (`workflow.graph`).
//!
//! `jkb workflow agent list|show|copy|set|export` and `jkb workflow show --graph` are the CLI for the
//! same ops. The graph is read from the compiled `jkb-fsm` tables ([`jkb_fsm::Machine::table`]) and
//! the strategy's permission table, so what the app draws is the table, never a second copy of it.

use std::collections::BTreeMap;

use jkb_core::roles::Role;
use jkb_core::workflow::agents::{self, Agent, AgentPermissions, Edit, Listed, Pick, Source};
use jkb_core::workflow::store as wf;
use jkb_core::workflow::strategy::StrategySpec;
use jkb_core::workflow::{Phase, WorkflowEvent};
use jkb_core::{task, WriteMeta};
use jkb_fsm::Table;
use jkb_rbac::Permission as _;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::rbac::OP_GRANTS;
use crate::{ApiError, ErrorCode};

fn invalid(why: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::Invalid, why)
}

/// An agent template as the ops answer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentView {
    /// Its name.
    pub name: String,
    /// The version shown: the packaged file's, or the operator copy's.
    pub version: i64,
    /// `packaged` (read-only) or `operator` (an editable copy).
    pub source: Source,
    /// The workflow script that runs it.
    pub workflow: String,
    /// The D52 role its calls act as.
    pub role: String,
    /// The jkb op classes that role may run (`jkb role matrix`): the half of its permissions jkb
    /// enforces.
    pub role_ops: Vec<String>,
    /// A piece other templates include, not an agent the graph draws.
    pub fragment: bool,
    /// One line on what it does.
    pub describe: String,
    /// The prompt, with `{{placeholders}}`.
    pub template: String,
    /// The placeholders the prompt names.
    pub placeholders: Vec<String>,
    /// Where it runs, on which model, and the most it may change: the half the workflow script
    /// enforces.
    pub permissions: AgentPermissions,
    /// The agents it hands its result to.
    pub hands_off_to: Vec<String>,
    /// What a copy was made from (`packaged:<name>@<v>` or `<name>@<v>`).
    pub based_on: Option<String>,
    /// When this copy's version was written.
    pub defined_at: Option<String>,
    /// The packaged template's version, when there is one of this name.
    pub packaged_version: Option<i64>,
    /// An operator copy is overriding the packaged template of its name.
    pub overrides_packaged: bool,
    /// The copy was taken from an older packaged version than the one in this jkb.
    pub behind_packaged: bool,
}

impl AgentView {
    /// The template this view shows, as [`jkb_core::workflow::agents`] takes it — what `jkb workflow
    /// agent export` writes into the packaged-templates file.
    ///
    /// # Errors
    /// [`ErrorCode::Invalid`] for a role this jkb does not know.
    pub fn to_agent(&self) -> Result<Agent, ApiError> {
        Ok(Agent {
            name: self.name.clone(),
            version: self.version,
            source: self.source,
            def: agents::AgentDef {
                workflow: self.workflow.clone(),
                role: Role::parse(&self.role)?,
                fragment: self.fragment,
                describe: self.describe.clone(),
                template: self.template.clone(),
                permissions: self.permissions.clone(),
                hands_off_to: self.hands_off_to.clone(),
            },
            based_on: self.based_on.clone(),
            defined_at: self.defined_at.clone(),
        })
    }
}

fn role_ops(role: Role) -> Vec<String> {
    OP_GRANTS
        .grants
        .iter()
        .find(|g| g.role == role)
        .map(|g| g.permits.iter().map(|p| p.name().to_owned()).collect())
        .unwrap_or_default()
}

/// `agent` as the ops answer it; `standing` is how its name stands against the packaged template of
/// that name (`Listed`'s flags), whichever version `agent` is.
fn view(agent: Agent, standing: &Listed) -> Result<AgentView, ApiError> {
    let placeholders = agents::placeholders(&agent.def.template)?;
    let (packaged_version, overrides_packaged, behind_packaged) = (
        standing.packaged_version,
        standing.overrides_packaged,
        standing.behind_packaged,
    );
    Ok(AgentView {
        role_ops: role_ops(agent.def.role),
        role: agent.def.role.as_str().to_owned(),
        name: agent.name,
        version: agent.version,
        source: agent.source,
        workflow: agent.def.workflow,
        fragment: agent.def.fragment,
        describe: agent.def.describe,
        template: agent.def.template,
        placeholders,
        permissions: agent.def.permissions,
        hands_off_to: agent.def.hands_off_to,
        based_on: agent.based_on,
        defined_at: agent.defined_at,
        packaged_version,
        overrides_packaged,
        behind_packaged,
    })
}

fn listed_view(l: &Listed) -> Result<AgentView, ApiError> {
    view(l.agent.clone(), l)
}

/// `workflow.agents`: every template, each as the one in effect.
///
/// # Errors
/// A database error, or a stored row this jkb cannot read.
pub fn list(conn: &Connection) -> Result<Vec<AgentView>, ApiError> {
    agents::list(conn)?.iter().map(listed_view).collect()
}

/// `workflow.agent`: one template — the one in effect, the packaged one, or a version of the copy —
/// and, given `vars`, its prompt filled in.
///
/// # Errors
/// [`ErrorCode::Invalid`] for an unknown name or version, both `packaged` and `version`, or `vars`
/// that do not fill the template exactly.
pub fn show(
    conn: &Connection,
    name: &str,
    packaged: bool,
    version: Option<i64>,
    vars: Option<&BTreeMap<String, String>>,
) -> Result<(AgentView, Option<String>), ApiError> {
    let pick = match (packaged, version) {
        (true, Some(_)) => {
            return Err(invalid(
                "name either the packaged template or a version of the copy, not both",
            ))
        }
        (true, None) => Pick::Packaged,
        (false, Some(v)) => Pick::Version(v),
        (false, None) => Pick::Effective,
    };
    let agent = agents::resolve(conn, name, pick)?;
    let rendered = vars
        .map(|v| agents::render(&agent.def.template, v))
        .transpose()?;
    // The flags describe the name, whichever version is shown: read for this name alone, so another
    // template's unreadable row cannot refuse this one.
    Ok((view(agent, &agents::standing(conn, name)?)?, rendered))
}

/// `workflow.agent_copy` (operator).
///
/// # Errors
/// As [`agents::copy`].
pub fn copy(
    conn: &Connection,
    meta: &WriteMeta,
    from: &str,
    packaged: bool,
    as_name: Option<&str>,
) -> Result<AgentView, ApiError> {
    let agent = agents::copy(conn, meta, from, packaged, as_name)?;
    let name = agent.name.clone();
    Ok(show(conn, &name, false, Some(agent.version), None)?.0)
}

/// What `workflow.agent_set` changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEdit {
    /// The prompt.
    #[serde(default)]
    pub template: Option<String>,
    /// The role.
    #[serde(default)]
    pub role: Option<String>,
    /// The description.
    #[serde(default)]
    pub describe: Option<String>,
    /// The permissions, whole.
    #[serde(default)]
    pub permissions: Option<AgentPermissions>,
    /// The hand-offs, whole.
    #[serde(default)]
    pub hands_off_to: Option<Vec<String>>,
}

/// `workflow.agent_set` (operator): edit an operator copy, appending a version when anything
/// changed. Answers the copy as it stands, and whether it wrote.
///
/// # Errors
/// As [`agents::set`]; [`ErrorCode::Invalid`] for an unknown role.
pub fn set(
    conn: &Connection,
    meta: &WriteMeta,
    name: &str,
    edit: AgentEdit,
) -> Result<(AgentView, bool), ApiError> {
    let role = edit.role.as_deref().map(Role::parse).transpose()?;
    let (agent, wrote) = agents::set(
        conn,
        meta,
        name,
        Edit {
            template: edit.template,
            role,
            describe: edit.describe,
            permissions: edit.permissions,
            hands_off_to: edit.hands_off_to,
        },
    )?;
    Ok((show(conn, name, false, Some(agent.version), None)?.0, wrote))
}

// -------------------------------------------------------------------------------------------
// The machines, as data
// -------------------------------------------------------------------------------------------

/// A state of a drawn machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphState {
    /// Its name.
    pub name: String,
    /// Objects start here.
    pub initial: bool,
    /// At rest: nothing is owed.
    pub settled: bool,
    /// Waiting on a person.
    pub awaits_input: bool,
    /// Who acts next here, under the strategy (the workflow machine only).
    pub next_role: Option<String>,
    /// What they do (`<uid>` stands for the task).
    pub next_step: Option<String>,
}

/// A transition of a drawn machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// From.
    pub from: String,
    /// The event.
    pub event: String,
    /// To; `None` where the caller names the destination (an operator override).
    pub to: Option<String>,
    /// An observation the facts decide, rather than an act.
    pub reconciled: bool,
    /// It has a guard.
    pub guarded: bool,
    /// It plans effects.
    pub planned: bool,
    /// The roles the strategy lets fire it (the workflow machine's acts only; an observation is
    /// anyone's, and its guard decides).
    pub roles: Vec<String>,
}

/// One machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineView {
    /// Its states, in declaration order.
    pub states: Vec<GraphState>,
    /// Its transitions, in declaration order.
    pub transitions: Vec<GraphEdge>,
}

/// `workflow.graph`'s answer: a strategy's workflow machine and the task lifecycle machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphView {
    /// The strategy drawn: a preset, `name@version`, or a task's own source.
    pub strategy: String,
    /// Its execution graph.
    pub graph: String,
    /// One line on the graph.
    pub describe: String,
    /// The strategy's spec.
    pub spec: serde_json::Value,
    /// The task named, when one was.
    pub task: Option<String>,
    /// That task's workflow phase.
    pub phase: Option<String>,
    /// That task's lifecycle status.
    pub status: Option<String>,
    /// The workflow machine (phases), with who may act where.
    pub workflow: MachineView,
    /// The task lifecycle machine (statuses).
    pub lifecycle: MachineView,
}

fn machine_view(
    table: &Table,
    next: impl Fn(&str) -> Option<(String, String)>,
    roles: impl Fn(&str, bool) -> Vec<String>,
) -> MachineView {
    MachineView {
        states: table
            .states
            .iter()
            .map(|s| {
                let (next_role, next_step) = next(s.name).unzip();
                GraphState {
                    name: s.name.to_owned(),
                    initial: s.initial,
                    settled: s.settled,
                    awaits_input: s.awaits_input,
                    next_role,
                    next_step,
                }
            })
            .collect(),
        transitions: table
            .transitions
            .iter()
            .map(|t| GraphEdge {
                from: t.from.to_owned(),
                event: t.event.to_owned(),
                to: t.to.map(str::to_owned),
                reconciled: t.reconciled,
                guarded: t.guarded,
                planned: t.planned,
                roles: roles(t.event, t.reconciled),
            })
            .collect(),
    }
}

/// `workflow.graph`: the machines of a task's strategy (`uid`), a named one (`strategy`), or the
/// default.
///
/// # Errors
/// [`ErrorCode::Invalid`] for both a task and a strategy, or an unknown strategy;
/// [`ErrorCode::NotFound`] for an unknown task; a database error.
pub fn graph(
    conn: &Connection,
    uid: Option<&str>,
    strategy: Option<&str>,
) -> Result<GraphView, ApiError> {
    let (source, spec, task, phase, status): (String, StrategySpec, _, _, _) = match (uid, strategy)
    {
        (Some(_), Some(_)) => {
            return Err(invalid(
                "name a task (its own strategy) or a strategy, not both",
            ))
        }
        (Some(uid), None) => {
            let id = task::resolve_ref(conn, uid)?.ok_or_else(|| {
                ApiError::with_code(ErrorCode::NotFound, format!("no task {uid}"))
            })?;
            let cur = wf::current(conn, id)?;
            let meta = jkb_core::item::get(conn, id)?.ok_or_else(|| {
                ApiError::with_code(ErrorCode::NotFound, format!("no task {uid}"))
            })?;
            (
                cur.source.clone(),
                cur.spec.clone(),
                Some(meta.uid),
                Some(cur.phase.as_str().to_owned()),
                meta.status,
            )
        }
        (None, Some(name)) => {
            let (source, spec) = wf::resolve_strategy(conn, name)?;
            (source, spec, None, None, None)
        }
        (None, None) => {
            let (source, spec) = wf::default_strategy(conn)?;
            (source, spec, None, None, None)
        }
    };
    let workflow = machine_view(
        &spec.graph.machine().table(),
        |state| {
            Phase::parse(state).map(|p| {
                let (role, step) = spec.next_actor(p);
                (role.as_str().to_owned(), step.to_owned())
            })
        },
        |event, reconciled| match WorkflowEvent::parse(event) {
            Some(e) if !reconciled => <Role as jkb_rbac::Role>::ALL
                .iter()
                .filter(|r| spec.authorize(&[**r], e).is_allowed())
                .map(|r| r.as_str().to_owned())
                .collect(),
            _ => Vec::new(),
        },
    );
    let lifecycle = machine_view(
        &jkb_core::lifecycle::machine().table(),
        |_| None,
        |_, _| Vec::new(),
    );
    Ok(GraphView {
        strategy: source,
        graph: spec.graph.as_str().to_owned(),
        describe: spec.graph.describe().to_owned(),
        spec: serde_json::to_value(&spec)
            .map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))?,
        task,
        phase,
        status,
        workflow,
        lifecycle,
    })
}

#[cfg(test)]
mod tests;
