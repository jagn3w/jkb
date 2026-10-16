//! The lifecycle timeline: sessions at work, a task's path through its strategy, and a plan's
//! progress over time — read from facts jkb already records, with no table of its own.
//!
//! The sources are the transition logs (`task_transitions`, `workflow_transitions`), claims
//! (`items.claimant_id`/`claimed_at`), review rounds (`reviews`, `review_rounds`), the landing
//! [`transition::landing`] reads, the agent bindings (`agent_bindings`) and the session registry
//! (`claude_sessions`). This module does every join, so an app draws data instead of deriving it.
//!
//! **Three id vocabularies meet here.** A claim and a `task_transitions` row name an
//! [`AgentId`]; a workflow transition and a review round name an RBAC principal (`operator`,
//! `grant:<id>`, `session:<claude session>`, `<agent type>@<agent id>`); the registry names the
//! Claude session. "Per session" rests on the two links that exist: the `@<claude session>`
//! provenance an [`AgentId::Session`] carries, and `agent_bindings (session, agent_id)` for an
//! attested subagent. [`speaks_for`] is the one place that applies them. A claim neither link
//! reaches — a swarm coordinator's `host:pid`, say — is listed as unattributed, never dropped.
//!
//! **Stalled** is decided here, from `now`: a session that holds a claim, has not ended, and has
//! fired no event for [`STALLED_AFTER_MS`].

use std::collections::{BTreeSet, HashMap};

use jkb_types::{is_session_id, AgentId, Error as TypeError, ItemId};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::claude_session::{self, SessionState};
use crate::{design, item, transition, Error, Result};

/// How long a session holding a claim may go without an event before it is shown as stalled
/// (operator decision, 2026-10-16): 30 minutes.
pub const STALLED_AFTER_MS: i64 = 30 * 60 * 1000;

/// The most events one page of a session's or a task's timeline carries; the rest are read by
/// passing the page's `next` back as `after`.
pub const PAGE_CAP: usize = 500;

/// The SQL that turns an ISO-8601 column, as jkb writes them (`strftime('%Y-%m-%dT%H:%M:%fZ')`), into
/// Unix milliseconds. A column name goes in, never a value.
fn ms(column: &str) -> String {
    format!("CAST(round((julianday({column}) - 2440587.5) * 86400000.0) AS INTEGER)")
}

fn invalid(why: impl Into<String>) -> Error {
    Error::Types(TypeError::Validation(why.into()))
}

fn not_found(why: impl Into<String>) -> Error {
    Error::Types(TypeError::NotFound(why.into()))
}

/// Which record an [`Event`] came from. The declaration order is the tie-break between sources
/// for events at the same millisecond, so a merged timeline is ordered the same way every read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The session registry: a process started or ended the session.
    Session,
    /// A claim taken (the claim a task holds now; a released claim leaves no row).
    Claim,
    /// The task lifecycle log (`task_transitions`): a status move.
    Status,
    /// The workflow log (`workflow_transitions`): a phase move.
    Workflow,
    /// A review round recorded, or the review gate waived or run on the host (`reviews`).
    Review,
}

impl Source {
    /// The snake-case name, as the wire carries it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Claim => "claim",
            Self::Status => "status",
            Self::Workflow => "workflow",
            Self::Review => "review",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [
            Self::Session,
            Self::Claim,
            Self::Status,
            Self::Workflow,
            Self::Review,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }
}

/// One thing that happened, from one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// When, in Unix milliseconds.
    pub at_ms: i64,
    /// The record it came from.
    pub source: Source,
    /// Its row id within that source: with `at_ms` and `source`, what orders and pages a timeline.
    pub id: i64,
    /// The task it is about; `None` for a session's own start or end.
    pub task: Option<String>,
    /// What happened: the transition's event, `claimed`, a review's kind, `start` or `end`.
    pub event: String,
    /// Where it moved from (a phase or a status), where the source says.
    pub from: Option<String>,
    /// Where it moved to.
    pub to: Option<String>,
    /// Who acted, in its source's vocabulary (an [`AgentId`] or an RBAC principal).
    pub actor: Option<String>,
    /// The role that acted (workflow moves).
    pub role: Option<String>,
    /// The reason written with it (workflow moves).
    pub reason: Option<String>,
    /// A line of context: a status move's branch, destination and PR; a round's must-fix count; the
    /// strategy a workflow row pinned; how a session started or ended.
    pub detail: Option<String>,
}

impl Event {
    fn key(&self) -> (i64, Source, i64) {
        (self.at_ms, self.source, self.id)
    }

    fn bare(at_ms: i64, source: Source, id: i64, event: impl Into<String>) -> Self {
        Self {
            at_ms,
            source,
            id,
            task: None,
            event: event.into(),
            from: None,
            to: None,
            actor: None,
            role: None,
            reason: None,
            detail: None,
        }
    }
}

/// The order every timeline is in: by time, then source, then row id within the source.
fn sort(events: &mut [Event]) {
    events.sort_by_key(Event::key);
}

fn cursor_of(e: &Event) -> String {
    format!("{}.{}.{}", e.at_ms, e.source.as_str(), e.id)
}

fn parse_cursor(s: &str) -> Result<(i64, Source, i64)> {
    let bad = || invalid(format!("`{s}` is not a timeline cursor (a page's `next`)"));
    let mut parts = s.splitn(3, '.');
    let at = parts.next().and_then(|p| p.parse::<i64>().ok());
    let source = parts.next().and_then(Source::parse);
    let id = parts.next().and_then(|p| p.parse::<i64>().ok());
    match (at, source, id) {
        (Some(at), Some(source), Some(id)) => Ok((at, source, id)),
        _ => Err(bad()),
    }
}

/// One page of `events` (already in timeline order) after `after`, and the cursor for the next page
/// when there is one. A keyset page: rows appended between two reads do not shift it.
fn page(events: Vec<Event>, after: Option<&str>) -> Result<(Vec<Event>, Option<String>)> {
    let from = after.map(parse_cursor).transpose()?;
    let mut rest: Vec<Event> = events
        .into_iter()
        .filter(|e| from.is_none_or(|c| e.key() > c))
        .collect();
    let next = if rest.len() > PAGE_CAP {
        rest.truncate(PAGE_CAP);
        rest.last().map(cursor_of)
    } else {
        None
    };
    Ok((rest, next))
}

// -------------------------------------------------------------------------------------------
// Attribution: who an id speaks for
// -------------------------------------------------------------------------------------------

/// What an id in one of the three vocabularies names, as far as attribution goes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Who {
    /// A Claude Code session, directly.
    Session(String),
    /// An attested subagent, by its agent id: its sessions are its bindings.
    Agent(String),
    /// Nothing a session can be found from.
    Nobody,
}

/// An RBAC principal as `workflow_transitions.actor` and `review_rounds.actor` record it.
fn principal(actor: &str) -> Who {
    if let Some(s) = actor.strip_prefix("session:") {
        return if is_session_id(s) {
            Who::Session(s.to_owned())
        } else {
            Who::Nobody
        };
    }
    match actor.split_once('@') {
        Some((_, agent)) if !agent.is_empty() => Who::Agent(agent.to_owned()),
        _ => Who::Nobody,
    }
}

/// A claimant or transition agent, as `items.claimant_id` and `task_transitions.agent_id` record it.
fn claimant(raw: &str) -> Who {
    match AgentId::parse(raw) {
        AgentId::Session {
            opened_by: Some(s), ..
        } => Who::Session(s),
        AgentId::Agent { id } => Who::Agent(id),
        AgentId::Session {
            opened_by: None, ..
        }
        | AgentId::Process { .. }
        | AgentId::Unrecognized { .. } => Who::Nobody,
    }
}

/// Every attested subagent's sessions, from `agent_bindings`.
struct Bindings {
    by_agent: HashMap<String, BTreeSet<String>>,
    by_session: HashMap<String, Vec<(String, ItemId)>>,
}

impl Bindings {
    fn read(conn: &Connection) -> Result<Self> {
        let mut by_agent: HashMap<String, BTreeSet<String>> = HashMap::new();
        let mut by_session: HashMap<String, Vec<(String, ItemId)>> = HashMap::new();
        let mut stmt = conn.prepare_cached(
            "SELECT session, agent_id, item_id FROM agent_bindings ORDER BY session, agent_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (session, agent, item) = row?;
            by_agent
                .entry(agent.clone())
                .or_default()
                .insert(session.clone());
            by_session
                .entry(session)
                .or_default()
                .push((agent, ItemId::new(item)));
        }
        Ok(Self {
            by_agent,
            by_session,
        })
    }

    /// The sessions `who` speaks for: one for a session, its bindings' for a subagent.
    fn speaks_for(&self, who: &Who) -> Vec<String> {
        match who {
            Who::Session(s) => vec![s.clone()],
            Who::Agent(a) => self
                .by_agent
                .get(a)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default(),
            Who::Nobody => Vec::new(),
        }
    }
}

/// The sessions an id speaks for, in whichever vocabulary its source uses — the one place
/// attribution is decided.
fn speaks_for(bindings: &Bindings, source: Source, actor: &str) -> Vec<String> {
    let who = match source {
        Source::Status | Source::Claim => claimant(actor),
        Source::Workflow | Source::Review => principal(actor),
        Source::Session => Who::Session(actor.to_owned()),
    };
    bindings.speaks_for(&who)
}

// -------------------------------------------------------------------------------------------
// Reading the sources
// -------------------------------------------------------------------------------------------

/// Which rows a source read takes: one task's, or every row an actor could attribute to a session.
#[derive(Debug, Clone, Copy)]
enum Rows {
    Task(ItemId),
    Attributable,
}

impl Rows {
    const fn item(self) -> i64 {
        match self {
            Self::Task(id) => id.get(),
            Self::Attributable => -1,
        }
    }

    const fn all(self) -> bool {
        matches!(self, Self::Attributable)
    }
}

fn status_rows(conn: &Connection, rows: Rows) -> Result<Vec<Event>> {
    // A row whose time does not parse is not placeable on a timeline; jkb writes none.
    let sql = format!(
        "SELECT t.id, {at}, i.uid, t.event, t.from_status, t.to_status, t.agent_id,
                t.branch, t.onto, t.pr_number
           FROM task_transitions t JOIN items i ON i.id = t.item_id
          WHERE {at} IS NOT NULL
            AND (CASE WHEN ?1 THEN t.agent_id LIKE 'session:%' OR t.agent_id LIKE 'agent:%'
                      ELSE t.item_id = ?2 END)
          ORDER BY t.id",
        at = ms("t.at")
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let out = stmt
        .query_map(params![rows.all(), rows.item()], |r| {
            let mut e = Event::bare(r.get(1)?, Source::Status, r.get(0)?, r.get::<_, String>(3)?);
            e.task = Some(r.get(2)?);
            e.from = r.get(4)?;
            e.to = Some(r.get(5)?);
            e.actor = r.get(6)?;
            let labels: Vec<String> = [
                r.get::<_, Option<String>>(7)?
                    .map(|b| format!("branch {b}")),
                r.get::<_, Option<String>>(8)?.map(|o| format!("onto {o}")),
                r.get::<_, Option<i64>>(9)?.map(|p| format!("PR #{p}")),
            ]
            .into_iter()
            .flatten()
            .collect();
            e.detail = (!labels.is_empty()).then(|| labels.join(", "));
            Ok(e)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

fn workflow_rows(conn: &Connection, rows: Rows) -> Result<Vec<Event>> {
    let sql = format!(
        "SELECT w.id, {at}, i.uid, w.event, w.from_phase, w.to_phase, w.role, w.actor, w.reason,
                w.spec IS NOT NULL
           FROM workflow_transitions w JOIN items i ON i.id = w.item_id
          WHERE {at} IS NOT NULL
            AND (CASE WHEN ?1 THEN w.actor LIKE 'session:%' OR instr(w.actor, '@') > 0
                      ELSE w.item_id = ?2 END)
          ORDER BY w.id",
        at = ms("w.at")
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let out = stmt
        .query_map(params![rows.all(), rows.item()], |r| {
            let mut e = Event::bare(
                r.get(1)?,
                Source::Workflow,
                r.get(0)?,
                r.get::<_, String>(3)?,
            );
            e.task = Some(r.get(2)?);
            e.from = r.get(4)?;
            e.to = Some(r.get(5)?);
            e.role = Some(r.get(6)?);
            e.actor = Some(r.get(7)?);
            e.reason = r.get(8)?;
            // A row that pins a strategy records where it came from as its reason (`current`).
            if r.get::<_, bool>(9)? {
                e.detail = Some(format!("strategy {}", e.reason.clone().unwrap_or_default()));
            }
            Ok(e)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

/// Review facts: a recorded round carries its round's actor, time and must-fix count; a waiver or a
/// host gate its own row's.
fn review_rows(conn: &Connection, rows: Rows) -> Result<Vec<Event>> {
    let sql = format!(
        "SELECT v.id, {rr_at}, {v_at}, i.uid, v.kind, v.ns, rr.actor, v.actor,
                (SELECT count(*) FROM review_round_findings f
                  WHERE f.round_id = rr.id AND f.must_fix = 1),
                rr.id IS NOT NULL
           FROM reviews v
           JOIN items i ON i.id = v.item_id
           LEFT JOIN review_rounds rr ON v.kind = 'recorded' AND rr.ns = v.ns
          WHERE coalesce({rr_at}, {v_at}) IS NOT NULL
            AND (CASE WHEN ?1 THEN 1 ELSE v.item_id = ?2 END)
          ORDER BY v.id",
        rr_at = ms("rr.recorded_at"),
        v_at = ms("v.at"),
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let out = stmt
        .query_map(params![rows.all(), rows.item()], |r| {
            let round_at: Option<i64> = r.get(1)?;
            let own_at: Option<i64> = r.get(2)?;
            let kind: String = r.get(4)?;
            let ns: Option<String> = r.get(5)?;
            let has_round: bool = r.get(9)?;
            let at = round_at.or(own_at).unwrap_or_default();
            let mut e = Event::bare(at, Source::Review, r.get(0)?, kind);
            e.task = Some(r.get(3)?);
            e.actor = if has_round { r.get(6)? } else { r.get(7)? };
            if has_round {
                let must_fix: i64 = r.get(8)?;
                e.detail = Some(format!("{}: {must_fix} must-fix", ns.unwrap_or_default()));
            } else {
                e.detail = ns;
            }
            Ok(e)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

/// A claim held now, read once for every caller.
#[derive(Debug, Clone)]
struct Held {
    id: ItemId,
    uid: String,
    owner: String,
    at_ms: Option<i64>,
}

/// The claims held now: every one, or only `only`'s.
fn held_claims(conn: &Connection, only: Option<ItemId>) -> Result<Vec<Held>> {
    let sql = format!(
        "SELECT id, uid, claimant_id, {at} FROM items
          WHERE claimant_id IS NOT NULL AND (?1 IS NULL OR id = ?1) ORDER BY id",
        at = ms("claimed_at")
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let out = stmt
        .query_map([only.map(ItemId::get)], |r| {
            Ok(Held {
                id: ItemId::new(r.get(0)?),
                uid: r.get(1)?,
                owner: r.get(2)?,
                at_ms: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

fn claim_event(h: &Held) -> Option<Event> {
    h.at_ms.map(|at| {
        let mut e = Event::bare(at, Source::Claim, h.id.get(), "claimed");
        e.task = Some(h.uid.clone());
        e.actor = Some(h.owner.clone());
        e
    })
}

/// A session's registry rows as events: each process's start and end. The ids number the rows in
/// their key's order (pid, instance), never the registry's most-recently-seen order, which moves as a
/// process is seen and would shift a page's cursor.
fn registry_events(rows: &[claude_session::HolderRow]) -> Vec<Event> {
    let mut rows: Vec<&claude_session::HolderRow> = rows.iter().collect();
    rows.sort_by(|a, b| (&a.pid, &a.instance).cmp(&(&b.pid, &b.instance)));
    let mut out = Vec::new();
    for (i, row) in rows.into_iter().enumerate() {
        let id = i64::try_from(i).unwrap_or(i64::MAX);
        if let Some(at) = row.started_at {
            let mut e = Event::bare(at, Source::Session, id * 2, "start");
            e.actor = Some(row.session.clone());
            e.detail.clone_from(&row.start_source);
            out.push(e);
        }
        if let Some(at) = row.ended_at {
            let mut e = Event::bare(at, Source::Session, id * 2 + 1, "end");
            e.actor = Some(row.session.clone());
            e.detail.clone_from(&row.end_reason);
            out.push(e);
        }
    }
    out
}

/// Every attributable event, grouped by the sessions it speaks for (one event can speak for two,
/// when a subagent id is bound in both). Each session's list is in timeline order.
fn attributed(
    conn: &Connection,
    bindings: &Bindings,
    held: &[Held],
) -> Result<HashMap<String, Vec<Event>>> {
    let mut by: HashMap<String, Vec<Event>> = HashMap::new();
    let mut add = |source: Source, e: Event| {
        if let Some(actor) = e.actor.as_deref() {
            for s in speaks_for(bindings, source, actor) {
                by.entry(s).or_default().push(e.clone());
            }
        }
    };
    for e in status_rows(conn, Rows::Attributable)? {
        add(Source::Status, e);
    }
    for e in workflow_rows(conn, Rows::Attributable)? {
        add(Source::Workflow, e);
    }
    for e in review_rows(conn, Rows::Attributable)? {
        add(Source::Review, e);
    }
    for e in held.iter().filter_map(claim_event) {
        add(Source::Claim, e);
    }
    for list in by.values_mut() {
        sort(list);
    }
    Ok(by)
}

// -------------------------------------------------------------------------------------------
// Sessions
// -------------------------------------------------------------------------------------------

/// A session at a glance: what the overview lists and a session's timeline heads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// The Claude Code session id.
    pub session: String,
    /// `live`, `ended` or `unknown` (no registry row: never recorded, or pruned).
    pub state: String,
    /// The working directory its most recently seen process reported.
    pub cwd: Option<String>,
    /// When it first started, as far as the registry remembers (Unix ms).
    pub started_at_ms: Option<i64>,
    /// When it ended, once every process holding it has (Unix ms).
    pub ended_at_ms: Option<i64>,
    /// How many tasks it holds claims on now.
    pub claims: usize,
    /// Its newest event (Unix ms).
    pub last_event_ms: Option<i64>,
    /// It holds a claim, has not ended, and has fired nothing for [`STALLED_AFTER_MS`].
    pub stalled: bool,
    /// How long ago its last event was, when stalled.
    pub stalled_for_ms: Option<i64>,
}

/// A claim no session can be found from: a process's (`host:pid`), a session worktree opened
/// outside Claude Code, an unbound agent id, or an id in no known shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldClaim {
    /// The task.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// Its status.
    pub status: Option<String>,
    /// The claimant, as stored.
    pub owner: String,
    /// When the claim was taken (Unix ms).
    pub claimed_at_ms: Option<i64>,
}

/// `timeline.sessions`: the sessions worth showing, and the claims no session is attributed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overview {
    /// The `now` stalled was judged against (Unix ms).
    pub now_ms: i64,
    /// Every live session, and every other session that holds a claim; newest event first.
    pub sessions: Vec<SessionSummary>,
    /// Claims held by no session this can name.
    pub unattributed: Vec<HeldClaim>,
}

/// A task in a session's timeline: one swimlane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTask {
    /// The task.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// Its status now.
    pub status: Option<String>,
    /// The session holds its claim now.
    pub holds_claim: bool,
    /// When that claim was taken (Unix ms).
    pub claimed_at_ms: Option<i64>,
    /// The session's subagents bound to it.
    pub bound_agents: Vec<String>,
}

/// `timeline.session`: one session's tasks and the events it fired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTimeline {
    /// The session at a glance.
    pub summary: SessionSummary,
    /// The tasks it holds, acted on, or is bound to: held first, then by first event, then bound.
    pub tasks: Vec<SessionTask>,
    /// One page of its events, oldest first.
    pub events: Vec<Event>,
    /// The cursor for the next page, when there is one.
    pub next: Option<String>,
}

/// A session's swimlanes, in the order they were first named.
#[derive(Default)]
struct Lanes {
    order: Vec<String>,
    lanes: HashMap<String, SessionTask>,
}

impl Lanes {
    /// The lane for `uid`, opened at the end of the order if it is new.
    fn lane(&mut self, uid: &str) -> &mut SessionTask {
        if !self.lanes.contains_key(uid) {
            self.order.push(uid.to_owned());
        }
        self.lanes
            .entry(uid.to_owned())
            .or_insert_with(|| SessionTask {
                uid: uid.to_owned(),
                title: String::new(),
                status: None,
                holds_claim: false,
                claimed_at_ms: None,
                bound_agents: Vec::new(),
            })
    }
}

fn summarize(
    conn: &Connection,
    session: &str,
    claims: usize,
    events: &[Event],
    now_ms: i64,
) -> Result<SessionSummary> {
    let rows = claude_session::holders(conn, session)?;
    let state = claude_session::state(conn, session)?;
    let newest_seen = rows.iter().max_by_key(|r| r.seen_at);
    let registry = registry_events(&rows);
    let last_event_ms = events.iter().chain(registry.iter()).map(|e| e.at_ms).max();
    let quiet_for = last_event_ms.map(|at| now_ms.saturating_sub(at));
    let stalled = claims > 0
        && state != SessionState::Ended
        && quiet_for.is_none_or(|q| q > STALLED_AFTER_MS);
    Ok(SessionSummary {
        session: session.to_owned(),
        state: state.as_str().to_owned(),
        cwd: newest_seen.map(|r| r.cwd.clone()),
        started_at_ms: rows.iter().filter_map(|r| r.started_at).min(),
        ended_at_ms: (state == SessionState::Ended)
            .then(|| rows.iter().filter_map(|r| r.ended_at).max())
            .flatten(),
        claims,
        last_event_ms,
        stalled,
        stalled_for_ms: if stalled { quiet_for } else { None },
    })
}

/// Each held claim's sessions, and the claims none is found for.
fn split_claims<'h>(
    bindings: &Bindings,
    held: &'h [Held],
) -> (HashMap<String, Vec<&'h Held>>, Vec<&'h Held>) {
    let mut by: HashMap<String, Vec<&Held>> = HashMap::new();
    let mut nobody = Vec::new();
    for h in held {
        let sessions = speaks_for(bindings, Source::Claim, &h.owner);
        if sessions.is_empty() {
            nobody.push(h);
        }
        for s in sessions {
            by.entry(s).or_default().push(h);
        }
    }
    (by, nobody)
}

/// `timeline.sessions`: every live session (the registry's) and every other session holding a claim,
/// each with its claim count, newest event and stalled flag judged at `now_ms`; and the claims no
/// session is attributed.
///
/// # Errors
/// A database error.
pub fn sessions(conn: &Connection, now_ms: i64) -> Result<Overview> {
    let bindings = Bindings::read(conn)?;
    let held = held_claims(conn, None)?;
    let events = attributed(conn, &bindings, &held)?;
    let (claims, nobody) = split_claims(&bindings, &held);
    let mut names: BTreeSet<String> = claims.keys().cloned().collect();
    // The live page holds up to `LIST_CAP` processes, far beyond any real fleet.
    for row in claude_session::list(conn, false, None)?.rows {
        names.insert(row.session);
    }
    let mut sessions = names
        .iter()
        .map(|s| {
            summarize(
                conn,
                s,
                claims.get(s).map_or(0, Vec::len),
                events.get(s).map_or(&[][..], Vec::as_slice),
                now_ms,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    sessions.sort_by(|a, b| {
        b.last_event_ms
            .cmp(&a.last_event_ms)
            .then_with(|| a.session.cmp(&b.session))
    });
    let ids: Vec<ItemId> = nobody.iter().map(|h| h.id).collect();
    let metas = item::get_many(conn, &ids)?;
    let unattributed = nobody
        .iter()
        .map(|h| {
            let meta = metas.get(&h.id);
            HeldClaim {
                uid: h.uid.clone(),
                title: meta.map_or_else(|| h.uid.clone(), item::title_of),
                status: meta.and_then(|m| m.status.clone()),
                owner: h.owner.clone(),
                claimed_at_ms: h.at_ms,
            }
        })
        .collect();
    Ok(Overview {
        now_ms,
        sessions,
        unattributed,
    })
}

/// `timeline.session`: the tasks `session` holds or is bound to, and its events — what its claims'
/// session provenance and its subagents' bindings attribute to it, plus its registry starts and
/// ends — one page after `after`.
///
/// # Errors
/// [`Error::Types`] (validation) for a malformed session id or cursor; a database error.
pub fn session(
    conn: &Connection,
    session: &str,
    after: Option<&str>,
    now_ms: i64,
) -> Result<SessionTimeline> {
    if !is_session_id(session) {
        return Err(invalid(format!(
            "`{session}` is not a Claude Code session id"
        )));
    }
    let bindings = Bindings::read(conn)?;
    let held = held_claims(conn, None)?;
    let mut all = attributed(conn, &bindings, &held)?;
    let mine = all.remove(session).unwrap_or_default();
    let (claims, _) = split_claims(&bindings, &held);
    let my_claims = claims.get(session).cloned().unwrap_or_default();
    let summary = summarize(conn, session, my_claims.len(), &mine, now_ms)?;

    // The swimlanes: held claims first, then tasks it acted on by first event, then bound tasks.
    let mut lanes = Lanes::default();
    for h in &my_claims {
        let t = lanes.lane(&h.uid);
        t.holds_claim = true;
        t.claimed_at_ms = h.at_ms;
    }
    for e in &mine {
        if let Some(uid) = &e.task {
            lanes.lane(uid);
        }
    }
    let bound = bindings
        .by_session
        .get(session)
        .cloned()
        .unwrap_or_default();
    let bound_ids: Vec<ItemId> = bound.iter().map(|(_, id)| *id).collect();
    let bound_metas = item::get_many(conn, &bound_ids)?;
    for (agent, id) in &bound {
        if let Some(m) = bound_metas.get(id) {
            lanes.lane(&m.uid).bound_agents.push(agent.clone());
        }
    }
    let Lanes { order, mut lanes } = lanes;
    let mut ids = Vec::new();
    for uid in &order {
        if let Some(id) = item::id_for_uid(conn, uid)? {
            ids.push((uid.clone(), id));
        }
    }
    let metas = item::get_many(conn, &ids.iter().map(|(_, id)| *id).collect::<Vec<_>>())?;
    for (uid, id) in &ids {
        if let (Some(t), Some(m)) = (lanes.get_mut(uid), metas.get(id)) {
            t.title = item::title_of(m);
            t.status.clone_from(&m.status);
        }
    }
    let tasks = order
        .iter()
        .filter_map(|u| lanes.remove(u))
        .map(|mut t| {
            if t.title.is_empty() {
                t.title.clone_from(&t.uid);
            }
            t
        })
        .collect();

    let mut events = mine;
    events.extend(registry_events(&claude_session::holders(conn, session)?));
    sort(&mut events);
    let (events, next) = page(events, after)?;
    Ok(SessionTimeline {
        summary,
        tasks,
        events,
        next,
    })
}

// -------------------------------------------------------------------------------------------
// A task
// -------------------------------------------------------------------------------------------

/// An edge a task's history has taken, and how often.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Visited {
    /// From.
    pub from: String,
    /// To.
    pub to: String,
    /// How many times.
    pub count: u32,
    /// The last time (Unix ms).
    pub last_at_ms: i64,
}

/// Where a task's work went, from [`transition::landing`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingView {
    /// `landed` (a live graft), `superseded` (a graft the task has since been put back to work
    /// past), or `none`.
    pub state: String,
    /// The branch it landed from.
    pub branch: Option<String>,
    /// Where it landed.
    pub onto: Option<String>,
    /// The newest pull request its history records.
    pub pr: Option<i64>,
    /// When it landed (Unix ms).
    pub at_ms: Option<i64>,
}

/// The claim a task holds now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimNow {
    /// The claimant, as stored.
    pub owner: String,
    /// Since when (Unix ms).
    pub claimed_at_ms: Option<i64>,
}

/// `timeline.task`'s facts about one task (the op adds the strategy's graph).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTimeline {
    /// The task.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// Its status now.
    pub status: Option<String>,
    /// The strategy its newest pinning row names; `None` when it never pinned one (it runs the
    /// default, or predates the workflow log).
    pub strategy: Option<String>,
    /// Who holds it now.
    pub claim: Option<ClaimNow>,
    /// Where its work went.
    pub landing: LandingView,
    /// The phase edges its workflow history took, in first-taken order.
    pub visited: Vec<Visited>,
    /// The status edges its lifecycle history took, in first-taken order.
    pub visited_status: Vec<Visited>,
    /// One page of its events, oldest first.
    pub events: Vec<Event>,
    /// The cursor for the next page, when there is one.
    pub next: Option<String>,
}

/// The edges `events` of one source took, counted, in first-taken order.
fn visited(events: &[Event], source: Source) -> Vec<Visited> {
    let mut out: Vec<Visited> = Vec::new();
    for e in events.iter().filter(|e| e.source == source) {
        let (Some(from), Some(to)) = (&e.from, &e.to) else {
            continue;
        };
        if let Some(v) = out.iter_mut().find(|v| &v.from == from && &v.to == to) {
            v.count += 1;
            v.last_at_ms = v.last_at_ms.max(e.at_ms);
        } else {
            out.push(Visited {
                from: from.clone(),
                to: to.clone(),
                count: 1,
                last_at_ms: e.at_ms,
            });
        }
    }
    out
}

fn at_ms(conn: &Connection, iso: &str) -> Result<Option<i64>> {
    let sql = format!("SELECT {}", ms("?1"));
    Ok(conn
        .prepare_cached(&sql)?
        .query_row([iso], |r| r.get::<_, Option<i64>>(0))
        .optional()?
        .flatten())
}

/// `timeline.task`: a task's workflow moves, status moves, current claim and review facts merged
/// into one list (by time, then source, then row id), the edges it visited, and where its work went.
///
/// # Errors
/// [`Error::Types`] not-found for no such task, validation for a malformed cursor; a database error.
pub fn task(conn: &Connection, reference: &str, after: Option<&str>) -> Result<TaskTimeline> {
    let id = crate::task::resolve_ref(conn, reference)?
        .ok_or_else(|| not_found(format!("no task {reference}")))?;
    let meta = item::get(conn, id)?.ok_or_else(|| not_found(format!("no task {reference}")))?;
    let mut events = status_rows(conn, Rows::Task(id))?;
    let workflow = workflow_rows(conn, Rows::Task(id))?;
    let strategy = workflow
        .iter()
        .rev()
        .find(|e| e.detail.is_some())
        .map(|e| e.reason.clone().unwrap_or_default());
    events.extend(workflow);
    events.extend(review_rows(conn, Rows::Task(id))?);
    let claim = held_claims(conn, Some(id))?.pop().map(|h| {
        events.extend(claim_event(&h));
        ClaimNow {
            owner: h.owner,
            claimed_at_ms: h.at_ms,
        }
    });
    sort(&mut events);
    let landing = transition::landing(conn, id)?;
    let landing = match (landing.live(), landing.superseded()) {
        (Some(row), _) => LandingView {
            state: "landed".to_owned(),
            branch: row.labels.branch.clone(),
            onto: row.labels.onto.clone(),
            pr: landing.pr_number(),
            at_ms: at_ms(conn, &row.at)?,
        },
        (None, Some((row, _))) => LandingView {
            state: "superseded".to_owned(),
            branch: row.labels.branch.clone(),
            onto: row.labels.onto.clone(),
            pr: landing.pr_number(),
            at_ms: at_ms(conn, &row.at)?,
        },
        (None, None) => LandingView {
            state: "none".to_owned(),
            branch: None,
            onto: None,
            pr: landing.pr_number(),
            at_ms: None,
        },
    };
    let visited_phases = visited(&events, Source::Workflow);
    let visited_status = visited(&events, Source::Status);
    let (events, next) = page(events, after)?;
    Ok(TaskTimeline {
        uid: meta.uid.clone(),
        title: item::title_of(&meta),
        status: meta.status,
        strategy,
        claim,
        landing,
        visited: visited_phases,
        visited_status,
        events,
        next,
    })
}

// -------------------------------------------------------------------------------------------
// A plan
// -------------------------------------------------------------------------------------------

/// How many of a plan's tasks stood where, from one moment on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPoint {
    /// From when (Unix ms).
    pub at_ms: i64,
    /// Open or blocked.
    pub open: u32,
    /// In progress or awaiting review.
    pub in_progress: u32,
    /// Done.
    pub done: u32,
    /// Cancelled, shown apart from done.
    pub cancelled: u32,
}

/// `timeline.plan`: a plan's tasks replayed into change points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTimeline {
    /// The plan.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// How many tasks its steps hold.
    pub tasks: usize,
    /// One point per moment the counts changed, oldest first.
    pub points: Vec<PlanPoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bucket {
    Open,
    InProgress,
    Done,
    Cancelled,
}

fn bucket(status: &str) -> Option<Bucket> {
    match status {
        "open" | "blocked" => Some(Bucket::Open),
        "in_progress" | "needs_review" => Some(Bucket::InProgress),
        "done" => Some(Bucket::Done),
        "cancelled" => Some(Bucket::Cancelled),
        _ => None,
    }
}

impl PlanPoint {
    fn slot(&mut self, b: Bucket) -> &mut u32 {
        match b {
            Bucket::Open => &mut self.open,
            Bucket::InProgress => &mut self.in_progress,
            Bucket::Done => &mut self.done,
            Bucket::Cancelled => &mut self.cancelled,
        }
    }
}

/// The change points of `tasks`: each enters at its creation in the status its history starts from
/// (or its status now, with no history), and moves with each status transition after.
///
/// # Errors
/// A database error.
pub fn plan_points(conn: &Connection, tasks: &[ItemId]) -> Result<Vec<PlanPoint>> {
    // (at, order, leaving, entering)
    let mut moves: Vec<(i64, usize, Option<Bucket>, Option<Bucket>)> = Vec::new();
    let created_sql = format!(
        "SELECT {}, status FROM items WHERE id = ?1",
        ms("created_at")
    );
    for &id in tasks {
        let Some((created, now_status)) = conn
            .prepare_cached(&created_sql)?
            .query_row([id.get()], |r| {
                Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .optional()?
        else {
            continue;
        };
        let history = status_rows(conn, Rows::Task(id))?;
        let first = history
            .first()
            .map_or(now_status, |e| e.from.clone().or_else(|| e.to.clone()));
        let mut current = first.as_deref().and_then(bucket);
        let start = created
            .or_else(|| history.first().map(|e| e.at_ms))
            .unwrap_or_default();
        moves.push((start, moves.len(), None, current));
        for e in &history {
            let next = e.to.as_deref().and_then(bucket);
            if next != current {
                moves.push((e.at_ms.max(start), moves.len(), current, next));
                current = next;
            }
        }
    }
    moves.sort_by_key(|m| (m.0, m.1));
    let mut points: Vec<PlanPoint> = Vec::new();
    let mut now = PlanPoint {
        at_ms: 0,
        open: 0,
        in_progress: 0,
        done: 0,
        cancelled: 0,
    };
    for (at, _, leaving, entering) in moves {
        if let Some(b) = leaving {
            let n = now.slot(b);
            *n = n.saturating_sub(1);
        }
        if let Some(b) = entering {
            *now.slot(b) += 1;
        }
        now.at_ms = at;
        match points.last_mut() {
            Some(p) if p.at_ms == at => *p = now,
            _ => points.push(now),
        }
    }
    Ok(points)
}

/// `timeline.plan`: the plan's step tasks (subtasks included, as the plan lists them) replayed into
/// change points of open, in progress, done and cancelled.
///
/// # Errors
/// The plan engine's refusal for an unknown plan; a database error.
pub fn plan(conn: &Connection, uid: &str) -> Result<PlanTimeline> {
    let view = design::plan::show(conn, uid)?;
    let mut ids = Vec::new();
    for t in view.tasks() {
        if let Some(id) = item::id_for_uid(conn, &t.uid)? {
            ids.push(id);
        }
    }
    Ok(PlanTimeline {
        uid: view.uid.clone(),
        title: view.title.clone(),
        tasks: ids.len(),
        points: plan_points(conn, &ids)?,
    })
}

#[cfg(test)]
mod tests;
