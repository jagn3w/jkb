//! A task's workflow in the database: the log, the strategy it runs, the facts its guards read, and
//! the one seam through which it moves ([`fire`] / [`observe`]).
//!
//! Like [`crate::transition`], the history is append-only and makes no claim about the present: the
//! current phase is the latest row's `to_phase`, the current strategy the latest row carrying a
//! spec. **Permission is checked here, in the callee** — [`fire`] and [`observe`] take the caller's
//! roles and ask the task's strategy — so no op, CLI verb or future caller can move a workflow
//! without asking.

use std::collections::BTreeSet;

use jkb_fsm::{Event as _, Fact, Outcome, Reconciliation, State as _};
use jkb_rbac::Decision;
use jkb_types::{Error as TypeError, ItemId, TaskStatus};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use super::strategy::{self, AreaScope, StrategySpec, DEFAULT_PRESET};
use super::{Phase, WorkflowEvent, WorkflowFacts};
use crate::query::{Query, Scope};
use crate::roles::{self, Role};
use crate::store::WriteMeta;
use crate::{item, tag, transition, Error, Result};

/// The facet a review round's findings namespace is recorded under on the task it reviewed.
pub const FACET_REVIEW: &str = "review";

/// The facet a finding's area (the file it is in) is recorded under.
pub const FACET_AREA: &str = "area";

/// The name of the strategy definition that, when defined, replaces [`DEFAULT_PRESET`].
pub const DEFAULT_DEFINITION: &str = "default";

fn invalid(msg: impl Into<String>) -> Error {
    Error::Types(TypeError::Validation(msg.into()))
}

// -------------------------------------------------------------------------------------------
// Review rounds
// -------------------------------------------------------------------------------------------

/// One review round, as read from its findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Round {
    /// Its findings namespace.
    pub ns: String,
    /// When it was filed, as an order: the highest item id among its findings. Findings are
    /// created when the review is filed, so this orders rounds by filing with no clock.
    pub filed: i64,
    /// How many findings it filed as must-fix (priority 1 or above), **at any status**: fixing a
    /// finding does not make the round that found it clean.
    pub must_fix: usize,
    /// The files its must-fix findings name.
    pub areas: Vec<String>,
}

/// The review namespaces recorded against `task` — the one place that says where they live.
///
/// # Errors
/// A database error.
pub fn review_namespaces(conn: &Connection, task: ItemId) -> Result<Vec<String>> {
    Ok(tag::applications(conn, task)?
        .into_iter()
        .filter(|(f, _)| f == FACET_REVIEW)
        .map(|(_, v)| v)
        .collect())
}

/// The area a finding names: its `area=` facet, or — for a finding filed before that facet existed —
/// the file in its title, which `review file` wrote as `summary — file[:line]`.
fn area_of(meta: &item::ItemMeta, tags: &[(String, String)]) -> Option<String> {
    if let Some((_, v)) = tags.iter().find(|(f, _)| f == FACET_AREA) {
        return Some(v.clone());
    }
    let title = item::title_of(meta);
    let (_, tail) = title.rsplit_once(" — ")?;
    let file = match tail.rsplit_once(':') {
        Some((f, line)) if line.chars().all(|c| c.is_ascii_digit()) && !line.is_empty() => f,
        _ => tail,
    };
    (!file.is_empty() && !file.contains(' ')).then(|| file.to_owned())
}

/// The rounds under `namespaces`, oldest first. A namespace holding nothing is not a round.
///
/// # Errors
/// A database error.
pub fn rounds_in(conn: &Connection, namespaces: &[String]) -> Result<Vec<Round>> {
    let mut out = Vec::new();
    for ns in namespaces {
        let ids = Query {
            kind: Some("task".to_owned()),
            scope: Scope::Subtree(ns.clone()),
            ..Query::default()
        }
        .evaluate(conn)?;
        let Some(filed) = ids.iter().map(|id| id.get()).max() else {
            continue;
        };
        let metas = item::get_many(conn, &ids)?;
        let tags = tag::applications_for(conn, &ids)?;
        let mut must_fix = 0;
        let mut areas = BTreeSet::new();
        for id in &ids {
            let Some(m) = metas.get(id) else { continue };
            if m.priority.unwrap_or(i64::MAX) > 1 {
                continue;
            }
            must_fix += 1;
            if let Some(a) = area_of(m, tags.get(id).map_or(&[][..], Vec::as_slice)) {
                areas.insert(a);
            }
        }
        out.push(Round {
            ns: ns.clone(),
            filed,
            must_fix,
            areas: areas.into_iter().collect(),
        });
    }
    out.sort_by_key(|r| r.filed);
    Ok(out)
}

/// The rounds recorded against `task`, oldest first.
///
/// # Errors
/// A database error.
pub fn rounds(conn: &Connection, task: ItemId) -> Result<Vec<Round>> {
    rounds_in(conn, &review_namespaces(conn, task)?)
}

/// Whether the newest `n` rounds all found must-fixes in a shared area, at `scope`.
#[must_use]
pub fn repeated(rounds: &[Round], n: u32, scope: AreaScope) -> bool {
    let n = n as usize;
    if n < 2 || rounds.len() < n {
        return false;
    }
    let key = |a: &str| -> String {
        match scope {
            AreaScope::File => a.to_owned(),
            AreaScope::Directory => a.rsplit_once('/').map_or("", |(d, _)| d).to_owned(),
        }
    };
    let recent = &rounds[rounds.len() - n..];
    if recent.iter().any(|r| r.must_fix == 0) {
        return false;
    }
    let mut shared: BTreeSet<String> = recent[0].areas.iter().map(|a| key(a)).collect();
    for r in &recent[1..] {
        let these: BTreeSet<String> = r.areas.iter().map(|a| key(a)).collect();
        shared = shared.intersection(&these).cloned().collect();
    }
    !shared.is_empty()
}

// -------------------------------------------------------------------------------------------
// Strategy definitions
// -------------------------------------------------------------------------------------------

/// Define (or redefine) the strategy `name` as `spec`, returning its new version. Redefining
/// appends; a task that pinned an earlier version keeps it.
///
/// # Errors
/// [`Error::Types`] for a malformed name or an invalid spec, or a database error.
pub fn define(
    conn: &Connection,
    _meta: &WriteMeta,
    name: &str,
    spec: &StrategySpec,
) -> Result<i64> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(invalid(format!(
            "a strategy name of 1 to 64 letters, digits, `-` and `_` (got `{name}`)"
        )));
    }
    if strategy::preset(name).is_some() {
        return Err(invalid(format!(
            "`{name}` is a preset; define a strategy under another name (`--from {name}` starts \
             from it)"
        )));
    }
    spec.validate()?;
    let version: i64 = conn
        .prepare_cached(
            "INSERT INTO workflow_strategies (name, version, spec, defined_at)
             VALUES (?1, (SELECT coalesce(max(version), 0) + 1 FROM workflow_strategies WHERE name = ?1),
                     ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             RETURNING version",
        )?
        .query_row(params![name, spec.to_json()?], |r| r.get(0))?;
    Ok(version)
}

/// The spec `name` resolves to: an operator definition (its newest version) or a preset.
///
/// # Errors
/// [`Error::Types`] for an unknown name or an unreadable stored spec.
pub fn resolve_strategy(conn: &Connection, name: &str) -> Result<(String, StrategySpec)> {
    let defined: Option<(i64, String)> = conn
        .prepare_cached(
            "SELECT version, spec FROM workflow_strategies WHERE name = ?1
             ORDER BY version DESC LIMIT 1",
        )?
        .query_row([name], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    if let Some((version, json)) = defined {
        return Ok((format!("{name}@{version}"), StrategySpec::from_json(&json)?));
    }
    if let Some(spec) = strategy::preset(name) {
        return Ok((name.to_owned(), spec));
    }
    Err(invalid(format!(
        "no strategy `{name}` (presets: {}; `jkb workflow strategies` lists definitions)",
        strategy::PRESETS
            .iter()
            .map(|p| p.name)
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Every operator definition's newest version, by name.
///
/// # Errors
/// A database error.
pub fn definitions(conn: &Connection) -> Result<Vec<(String, i64, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name, max(version), (SELECT spec FROM workflow_strategies w2
                                     WHERE w2.name = w.name ORDER BY version DESC LIMIT 1)
         FROM workflow_strategies w GROUP BY name ORDER BY name",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The strategy a task with nothing pinned runs: the `default` definition if the operator made one,
/// else [`DEFAULT_PRESET`].
///
/// # Errors
/// [`Error::Types`] for an unreadable stored definition.
pub fn default_strategy(conn: &Connection) -> Result<(String, StrategySpec)> {
    match resolve_strategy(conn, DEFAULT_DEFINITION) {
        Ok(found) => Ok(found),
        Err(_) => resolve_strategy(conn, DEFAULT_PRESET),
    }
}

// -------------------------------------------------------------------------------------------
// The log
// -------------------------------------------------------------------------------------------

/// One row of a task's workflow history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRow {
    /// Row id, ascending with time.
    pub id: i64,
    /// When.
    pub at: String,
    /// The event (or `set_strategy`).
    pub event: String,
    /// From.
    pub from_phase: Option<String>,
    /// To.
    pub to_phase: String,
    /// The acting role.
    pub role: String,
    /// The acting principal.
    pub actor: String,
    /// The newest round's filing mark when this was written.
    pub round_mark: Option<i64>,
    /// The written reason, where the event takes one.
    pub reason: Option<String>,
    /// The resolved strategy, on rows that set one.
    pub spec: Option<String>,
    /// The facts the guard fired on.
    pub evidence: Option<String>,
}

/// A task's workflow history, oldest first.
///
/// # Errors
/// A database error.
pub fn history(conn: &Connection, task: ItemId) -> Result<Vec<WorkflowRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, at, event, from_phase, to_phase, role, actor, round_mark, reason, spec, evidence
         FROM workflow_transitions WHERE item_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([task.get()], |r| {
        Ok(WorkflowRow {
            id: r.get(0)?,
            at: r.get(1)?,
            event: r.get(2)?,
            from_phase: r.get(3)?,
            to_phase: r.get(4)?,
            role: r.get(5)?,
            actor: r.get(6)?,
            round_mark: r.get(7)?,
            reason: r.get(8)?,
            spec: r.get(9)?,
            evidence: r.get(10)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Where a task's workflow stands.
#[derive(Debug, Clone)]
pub struct Current {
    /// Its phase.
    pub phase: Phase,
    /// The strategy it runs.
    pub spec: StrategySpec,
    /// Where that strategy came from: the name it was pinned from, or `default:<name>` when the task
    /// has pinned nothing and runs the current default.
    pub source: String,
    /// Its history.
    pub history: Vec<WorkflowRow>,
}

/// Where `task`'s workflow stands.
///
/// # Errors
/// [`Error::Types`] for a stored spec this binary cannot read (refused, never ignored), or a
/// database error.
pub fn current(conn: &Connection, task: ItemId) -> Result<Current> {
    let history = history(conn, task)?;
    let phase = match history.last() {
        Some(r) => Phase::parse(&r.to_phase)
            .ok_or_else(|| invalid(format!("an unknown workflow phase `{}`", r.to_phase)))?,
        None => Phase::Design,
    };
    let pinned = history.iter().rev().find_map(|r| {
        r.spec
            .as_ref()
            .map(|s| (s.clone(), r.reason.clone().unwrap_or_default()))
    });
    let (spec, source) = if let Some((json, source)) = pinned {
        (StrategySpec::from_json(&json)?, source)
    } else {
        let (name, spec) = default_strategy(conn)?;
        (spec, format!("default:{name}"))
    };
    Ok(Current {
        phase,
        spec,
        source,
        history,
    })
}

/// Who is acting on a workflow: the roles held, and the principal to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// The roles held.
    pub roles: Vec<Role>,
    /// The principal, as recorded (`operator`, `grant:<id>`, `<agent_type>@<agent_id>`).
    pub principal: String,
}

impl Actor {
    /// The operator.
    #[must_use]
    pub fn operator() -> Self {
        Self {
            roles: vec![Role::Operator],
            principal: "operator".to_owned(),
        }
    }

    fn role_label(&self) -> String {
        self.roles
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Gather the facts `task`'s guards read, at phase `current.phase`, under `current.spec`.
///
/// # Errors
/// A database error.
pub fn observe_facts(conn: &Connection, task: ItemId, current: &Current) -> Result<WorkflowFacts> {
    let meta = item::get(conn, task)?
        .ok_or_else(|| Error::Types(TypeError::NotFound(format!("task {task}"))))?;
    let task_status = meta
        .status
        .as_deref()
        .and_then(TaskStatus::from_db_str)
        .unwrap_or_default();
    let landed = Fact::from(transition::landing(conn, task)?.live().is_some());
    let rounds = rounds(conn, task)?;
    let newest = rounds.last();
    // The mark on the row that last took the task INTO review: a round filed after it is new.
    let entered_review = current
        .history
        .iter()
        .rev()
        .find(|r| {
            r.to_phase == Phase::Review.as_str()
                && r.from_phase.as_deref() != Some(Phase::Review.as_str())
        })
        .map(|r| r.round_mark.unwrap_or(0));
    let new_round = match (entered_review, newest) {
        (Some(mark), Some(r)) => Fact::from(r.filed > mark),
        _ => Fact::No,
    };
    let area = current.spec.attributes.repeated_area;
    Ok(WorkflowFacts {
        phase: current.phase,
        stated: None,
        task_status,
        landed,
        new_round,
        last_round_must_fix: Fact::from(newest.is_some_and(|r| r.must_fix > 0)),
        repeated_areas: Fact::from(repeated(&rounds, area.rounds, area.scope)),
    })
}

fn evidence(f: &WorkflowFacts) -> String {
    json!({
        "task_status": f.task_status.as_str(),
        "landed": f.landed.as_str(),
        "new_round": f.new_round.as_str(),
        "last_round_must_fix": f.last_round_must_fix.as_str(),
        "repeated_areas": f.repeated_areas.as_str(),
    })
    .to_string()
}

#[allow(clippy::too_many_arguments)]
fn append(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    event: &str,
    from: Option<Phase>,
    to: Phase,
    actor: &Actor,
    reason: Option<&str>,
    spec: Option<&str>,
    evidence: Option<&str>,
) -> Result<()> {
    let mark = rounds(conn, task)?.last().map(|r| r.filed);
    conn.prepare_cached(
        "INSERT INTO workflow_transitions
             (txn_id, item_id, at, event, from_phase, to_phase, role, actor, round_mark, reason, spec, evidence)
         VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?
    .execute(params![
        meta.txn_id,
        task.get(),
        event,
        from.map(Phase::as_str),
        to.as_str(),
        actor.role_label(),
        actor.principal,
        mark,
        reason,
        spec,
        evidence,
    ])?;
    Ok(())
}

/// What a workflow move did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Moved {
    /// It moved.
    To {
        /// From.
        from: Phase,
        /// The event.
        event: WorkflowEvent,
        /// To.
        to: Phase,
        /// Worker grants revoked because the task settled.
        revoked: usize,
    },
    /// Nothing to do: it was already there.
    AlreadyThere(Phase),
    /// Refused, and why.
    Refused(String),
}

fn check_reason(event: WorkflowEvent, reason: Option<&str>) -> Result<()> {
    if event.needs_reason() && reason.is_none_or(|r| r.trim().is_empty()) {
        return Err(invalid(format!(
            "`{}` needs a written reason (`--reason …`): it decides who sees the work next",
            event.as_str()
        )));
    }
    if reason.is_some_and(|r| r.len() > 4096) {
        return Err(invalid("a reason of at most 4096 bytes"));
    }
    Ok(())
}

fn settle(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    outcome: Outcome<Phase, WorkflowEvent, ()>,
    actor: &Actor,
    reason: Option<&str>,
    facts: &WorkflowFacts,
) -> Result<Moved> {
    Ok(match outcome {
        Outcome::Moved {
            from, event, to, ..
        } => {
            append(
                conn,
                meta,
                task,
                event.as_str(),
                Some(from),
                to,
                actor,
                reason,
                None,
                Some(&evidence(facts)),
            )?;
            let revoked = if to.is_settled() {
                roles::revoke_task(conn, meta, task)?
            } else {
                0
            };
            Moved::To {
                from,
                event,
                to,
                revoked,
            }
        }
        Outcome::Idempotent { state, .. } => Moved::AlreadyThere(state),
        other => Moved::Refused(
            other
                .refusal()
                .unwrap_or_else(|| format!("that does not apply in `{}`", facts.phase.as_str())),
        ),
    })
}

/// Fire `event` on `task` as `actor` — the **one** way a workflow moves by request. The task's
/// strategy decides whether `actor` may; its graph decides whether the event applies.
///
/// `stated` is the phase an `override` names.
///
/// # Errors
/// [`Error::Types`] for a missing reason or an unreadable strategy, or a database error. A refusal —
/// by permission or by the graph — is not an error: it is [`Moved::Refused`].
pub fn fire(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    event: WorkflowEvent,
    actor: &Actor,
    reason: Option<&str>,
    stated: Option<Phase>,
) -> Result<Moved> {
    check_reason(event, reason)?;
    let current = current(conn, task)?;
    if let Decision::Deny(no) = current.spec.authorize(&actor.roles, event) {
        return Ok(Moved::Refused(no.to_string()));
    }
    let mut facts = observe_facts(conn, task, &current)?;
    facts.stated = stated;
    let outcome = current.spec.graph.machine().apply(&facts, event);
    settle(conn, meta, task, outcome, actor, reason, &facts)
}

/// Observe `task` and take the one reconciliation that applies — after a review round, this is what
/// moves the task on with no human prompt. [`Moved::AlreadyThere`] when nothing applies.
///
/// # Errors
/// A database error, or an unreadable strategy.
pub fn observe(conn: &Connection, meta: &WriteMeta, task: ItemId, actor: &Actor) -> Result<Moved> {
    let current = current(conn, task)?;
    let facts = observe_facts(conn, task, &current)?;
    match current.spec.graph.machine().reconcile(&facts) {
        Reconciliation::Settled => Ok(Moved::AlreadyThere(current.phase)),
        Reconciliation::Ambiguous(events) => Ok(Moved::Refused(format!(
            "more than one observation applies at once ({}) — this is a defect in the graph",
            events
                .iter()
                .map(|e| e.name())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        Reconciliation::Fired(outcome) => settle(conn, meta, task, outcome, actor, None, &facts),
    }
}

/// Pin `task` to the strategy `name` (operator only). Recorded as a row at the current phase, with
/// the resolved spec, so redefining `name` later never changes this task.
///
/// # Errors
/// [`Error::Types`] for a non-operator, an unknown or invalid strategy, or a database error.
pub fn set_strategy(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    name: &str,
    actor: &Actor,
) -> Result<StrategySpec> {
    if !actor.roles.contains(&Role::Operator) {
        return Err(invalid(
            "only the operator chooses a task's workflow strategy — an agent choosing a laxer one \
             for itself is what the strategy exists to prevent",
        ));
    }
    let (source, spec) = resolve_strategy(conn, name)?;
    let phase = current(conn, task)?.phase;
    append(
        conn,
        meta,
        task,
        "set_strategy",
        Some(phase),
        phase,
        actor,
        Some(&source),
        Some(&spec.to_json()?),
        None,
    )?;
    Ok(spec)
}

#[cfg(test)]
mod tests;
