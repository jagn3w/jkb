//! Whether a run request's result has reached the session that filed it (the "session that files a
//! run request hears its result" design).
//!
//! **One row per request, made with it** ([`crate::exec::request`] calls [`create`] in its own
//! transaction), in `exec_delivery`. Its state moves only here, and each move is one compare-and-set:
//!
//! | from → to | function |
//! |---|---|
//! | (new) → `waiting` | [`create`] |
//! | `waiting`/`failed`, or `sending` with a lapsed lease → `sending`, for the forwarder | [`claim_for_socket`] |
//! | `sending` → `delivered` (via `socket`) or `failed` | [`report`] |
//! | anything not `delivered` and not under a live lease → `sending`, for [`HOOK_LEASE_MS`] | [`take_for_hook`] |
//! | `sending` → `delivered` (via `hook`) | [`ack_hook`] |
//!
//! A row reaches `delivered` once, and nothing moves it out. A deliverer that claims and then dies
//! loses nothing: the lease lapses, and the hook path or the next claim picks the row up, because a
//! claim is not delivery until [`report`] or [`ack_hook`] says the message went out. The hook's take is
//! two-phase for that reason: the daemon can commit a take after the hook's client has given up, or
//! the hook can die before it prints, and a take that marked the rows delivered at once lost them both
//! times. The cost is the other direction: a hook that printed and then lost its ack hands the result
//! over again once the lease lapses — said twice rather than never.
//!
//! **When a result is due.** Its request is terminal, ended within [`WINDOW_MS`], and is not an
//! ending `settle` wrote by age alone while its runner may still report its own
//! ([`crate::exec::SETTLED_INTERRUPT`]). A row a subagent filed is due for the session's socket only
//! once that subagent is gone ([`agent_gone`]) or [`SUBAGENT_GRACE_MS`] after the request ended — the
//! socket reaches the main agent, and the subagent hears it from its own next tool call until then.
//! And it is due for **a** forwarder's socket only while its session has a live process with an inbox
//! in that forwarder's container boot (`host#boot`): a result with no socket there to post to is left
//! `waiting` for the hook path, not claimed and failed — and never claimed by another container's
//! forwarder, which would fail it for want of a socket and cost the right one its push.
//!
//! Like `exec_output` and `claude_sessions`, this is observation: not changelogged, and `jkb undo`
//! never reaches it.

use rusqlite::{params, Connection, OptionalExtension};

use crate::claude_session;
use crate::exec::{self, ExecRow, LATEST_END_MS, SETTLED_INTERRUPT};
use crate::{Result, WriteMeta};

/// A result undelivered this long after its request ended is never pushed: a day, the request topic's
/// TTL. `jkb exec status` still shows it.
pub const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// How long after its request ended a subagent's result waits for that subagent's own next tool call
/// before the session's socket may carry it: ten minutes.
pub const SUBAGENT_GRACE_MS: i64 = 10 * 60 * 1000;

/// The most socket attempts a result gets; past them only the hook path delivers it.
pub const MAX_ATTEMPTS: i64 = 5;

/// The longest error [`report`] records, in characters.
pub const MAX_ERROR_CHARS: usize = 500;

/// How long the hook's [`take_for_hook`] holds what it handed over for its [`ack_hook`]: past it, the
/// rows are taken again — the hook is bounded by Claude Code's 10-second hook timeout.
pub const HOOK_LEASE_MS: i64 = 30_000;

/// The most ids one [`due`] returns.
pub const MAX_DUE: usize = 256;

/// The longest lease [`claim_for_socket`] grants: five minutes. A post is one local socket write, and
/// a forwarder that claims and dies must not keep the hook path off the row for longer than this.
pub const MAX_SOCKET_LEASE_MS: i64 = 5 * 60 * 1000;

macro_rules! words {
    ($(#[$doc:meta])* $name:ident { $( $(#[$vdoc:meta])* $variant:ident => $s:literal ),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub enum $name {
            #[default]
            $( $(#[$vdoc])* $variant, )+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [Self] = &[ $( Self::$variant, )+ ];

            /// The stored spelling.
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

words! {
    /// Where a result's delivery stands.
    DeliveryState {
        /// Not yet handed to anyone.
        Waiting => "waiting",
        /// A deliverer claimed it and is posting it, until its lease lapses.
        Sending => "sending",
        /// It reached the session. Final.
        Delivered => "delivered",
        /// The last socket attempt failed.
        Failed => "failed",
    }
}

words! {
    /// How a result was delivered.
    Via {
        /// The session's inbox socket, by the container's forwarder.
        Socket => "socket",
        /// The attest hook's context injection, on the session's next event.
        Hook => "hook",
        /// Made before delivery was recorded: counted as delivered by the migration.
        Backfill => "backfill",
    }
}

/// A request's delivery row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Delivery {
    /// The request.
    pub request_id: i64,
    /// The subagent that filed it, or `None` for the main agent (or a fold across agents).
    pub agent_id: Option<String>,
    /// When that subagent stopped (Unix ms).
    pub agent_gone_at: Option<i64>,
    /// Where it stands.
    pub state: DeliveryState,
    /// A `sending` row's lease.
    pub lease_until: Option<i64>,
    /// How it was delivered.
    pub via: Option<Via>,
    /// When it was delivered (Unix ms).
    pub delivered_at: Option<i64>,
    /// Failed socket attempts so far.
    pub attempts: i64,
    /// When the last socket attempt was reported (Unix ms).
    pub last_attempt_at: Option<i64>,
    /// Why the last socket attempt failed.
    pub last_error: Option<String>,
}

fn corrupt(id: i64, what: &str, value: &str) -> crate::Error {
    crate::mq::QueueError::Corrupt(format!(
        "exec delivery {id} has {what} {value:?}, which this jkb does not know"
    ))
    .into()
}

/// Request `id`'s delivery row.
///
/// # Errors
/// A database error, or a row this jkb cannot read.
pub fn get(conn: &Connection, id: i64) -> Result<Option<Delivery>> {
    let raw = conn
        .prepare_cached(
            "SELECT request_id, agent_id, agent_gone_at, state, lease_until, via, delivered_at, \
                    attempts, last_attempt_at, last_error \
             FROM exec_delivery WHERE request_id = ?1",
        )?
        .query_row([id], |r| {
            Ok((
                Delivery {
                    request_id: r.get(0)?,
                    agent_id: r.get(1)?,
                    agent_gone_at: r.get(2)?,
                    state: DeliveryState::Waiting,
                    lease_until: r.get(4)?,
                    via: None,
                    delivered_at: r.get(6)?,
                    attempts: r.get(7)?,
                    last_attempt_at: r.get(8)?,
                    last_error: r.get(9)?,
                },
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })
        .optional()?;
    let Some((mut d, state, via)) = raw else {
        return Ok(None);
    };
    d.state = DeliveryState::parse(&state).ok_or_else(|| corrupt(id, "state", &state))?;
    d.via = via
        .as_deref()
        .map(|v| Via::parse(v).ok_or_else(|| corrupt(id, "via", v)))
        .transpose()?;
    Ok(Some(d))
}

/// Make request `id`'s row, `waiting`, for `agent_id`. Only [`crate::exec::request`] calls this, in
/// the request's own transaction.
///
/// # Errors
/// A database error (a second row for one request among them).
pub(crate) fn create(conn: &Connection, id: i64, agent_id: Option<&str>) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO exec_delivery (request_id, agent_id, state) VALUES (?1, ?2, 'waiting')",
    )?
    .execute(params![id, agent_id])?;
    Ok(())
}

/// An identical request from `agent_id` folded into `id`: unless the same agent filed both, the
/// result goes to the session — its main agent — not to either agent.
///
/// # Errors
/// A database error.
pub(crate) fn fold(conn: &Connection, id: i64, agent_id: Option<&str>) -> Result<()> {
    conn.prepare_cached(
        "UPDATE exec_delivery SET agent_id = NULL \
         WHERE request_id = ?1 AND agent_id IS NOT NULL AND agent_id IS NOT ?2",
    )?
    .execute(params![id, agent_id])?;
    Ok(())
}

/// The condition, over `r` (`exec_requests`) and `d` (`exec_delivery`), that `r`'s result can be
/// delivered at `?1`: terminal, ended within [`WINDOW_MS`] (`?2`), and not an ending `settle` wrote by
/// age alone while the runner may still report its own (`?3` is [`SETTLED_INTERRUPT`], `?4`
/// [`LATEST_END_MS`]). Not yet delivered, and not under a live lease.
const DELIVERABLE: &str = "r.state IN ('finished', 'declined', 'refused', 'expired') \
     AND r.ended_at IS NOT NULL AND r.ended_at > ?1 - ?2 \
     AND NOT (r.state = 'finished' AND r.outcome = 'interrupted' AND r.note IS ?3 \
              AND r.created_at + ?4 > ?1) \
     AND d.state <> 'delivered' AND NOT (d.state = 'sending' AND d.lease_until > ?1)";

/// The condition, beyond [`DELIVERABLE`], that the result is due for the socket of the forwarder of
/// container boot `?7` (`host#boot`): a subagent's only once it is gone or past its grace (`?5`); a
/// failed one only with fewer than `?6` attempts; and only while the session has a live process with
/// an inbox in that boot — for a failed one, an inbox registered after the last attempt (it was
/// resumed). The boot match is [`claude_session::inbox_targets`]'s, so what is due is exactly what
/// has a target.
const SOCKET_DUE: &str = "(d.agent_id IS NULL OR d.agent_gone_at IS NOT NULL \
          OR r.ended_at + ?5 <= ?1) \
     AND (d.state <> 'failed' OR d.attempts < ?6) \
     AND EXISTS (SELECT 1 FROM claude_sessions s WHERE s.session = r.session \
          AND s.ended_at IS NULL AND s.inbox_socket IS NOT NULL AND s.inbox_token IS NOT NULL \
          AND (s.instance = ?7 OR substr(s.instance, 1, length(?7) + 1) = ?7 || '/') \
          AND (d.state <> 'failed' OR s.started_at > d.last_attempt_at))";

/// The ids due at `now` for the socket of the forwarder of container boot `host_boot`, oldest first,
/// at most `max` (capped at [`MAX_DUE`]). What the forwarder sweeps: no token, no text. Settles
/// expiries first, as every exec function taking `meta` does, so an expired request is due without
/// waiting for another exec op.
///
/// # Errors
/// A database error.
pub fn due(
    conn: &Connection,
    meta: &WriteMeta,
    host_boot: &str,
    now: i64,
    max: usize,
) -> Result<Vec<i64>> {
    exec::settle(conn, meta, now)?;
    let max = i64::try_from(max.min(MAX_DUE)).unwrap_or(0);
    Ok(conn
        .prepare_cached(&format!(
            "SELECT r.id FROM exec_requests r JOIN exec_delivery d ON d.request_id = r.id \
             WHERE d.state <> 'delivered' AND {DELIVERABLE} AND {SOCKET_DUE} \
             ORDER BY r.id LIMIT ?8"
        ))?
        .query_map(
            params![
                now,
                WINDOW_MS,
                SETTLED_INTERRUPT,
                LATEST_END_MS,
                SUBAGENT_GRACE_MS,
                MAX_ATTEMPTS,
                host_boot,
                max
            ],
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?)
}

/// A result claimed for a socket post: its message, and where to post it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// The result, as the hook would be handed it ([`Taken::text`] is the message).
    pub taken: Taken,
    /// The session's live processes with an inbox in the claiming boot, newest start first
    /// ([`claude_session::inbox_targets`]).
    pub targets: Vec<claude_session::Target>,
}

/// Claim `id` for a socket post by the forwarder of container boot `host_boot`, for `lease_ms`: the
/// message and its targets when this call won it — it was due ([`due`]'s rule) and nobody else holds
/// it — else `None`. A claim is not delivery: [`report`] says how the post went, and a claim never
/// reported lapses, so the hook path or the next claim takes the row.
///
/// # Errors
/// A validation error for a lease that is not positive or is longer than [`MAX_SOCKET_LEASE_MS`];
/// a database error.
pub fn claim_for_socket(
    conn: &Connection,
    id: i64,
    host_boot: &str,
    lease_ms: i64,
    now: i64,
) -> Result<Option<Claim>> {
    if !(1..=MAX_SOCKET_LEASE_MS).contains(&lease_ms) {
        return Err(jkb_types::Error::Validation(format!(
            "a lease of {lease_ms} ms is not 1 to {MAX_SOCKET_LEASE_MS} ms"
        ))
        .into());
    }
    let changed = conn
        .prepare_cached(&format!(
            "UPDATE exec_delivery SET state = 'sending', lease_until = ?9 \
             WHERE request_id = ?8 AND request_id IN ( \
                 SELECT r.id FROM exec_requests r JOIN exec_delivery d ON d.request_id = r.id \
                 WHERE r.id = ?8 AND {DELIVERABLE} AND {SOCKET_DUE})"
        ))?
        .execute(params![
            now,
            WINDOW_MS,
            SETTLED_INTERRUPT,
            LATEST_END_MS,
            SUBAGENT_GRACE_MS,
            MAX_ATTEMPTS,
            host_boot,
            id,
            now.saturating_add(lease_ms)
        ])?;
    if changed == 0 {
        return Ok(None);
    }
    let taken = taken(conn, id)?;
    let targets = claude_session::inbox_targets(conn, &taken.row.requester.session, host_boot)?;
    Ok(Some(Claim { taken, targets }))
}

/// Report how the socket post of claimed `id` went: `Ok` delivers it (via `socket`), `Err` records the
/// failure and counts the attempt. `false` when the row was not `sending` — the lease lapsed and
/// another deliverer took it, which is then that deliverer's to report.
///
/// # Errors
/// A database error.
pub fn report(
    conn: &Connection,
    id: i64,
    outcome: std::result::Result<(), &str>,
    now: i64,
) -> Result<bool> {
    let changed = match outcome {
        Ok(()) => conn
            .prepare_cached(
                "UPDATE exec_delivery SET state = 'delivered', via = 'socket', delivered_at = ?2, \
                     lease_until = NULL \
                 WHERE request_id = ?1 AND state = 'sending'",
            )?
            .execute(params![id, now])?,
        Err(why) => {
            let why: String = why
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_ERROR_CHARS)
                .collect();
            conn.prepare_cached(
                "UPDATE exec_delivery SET state = 'failed', lease_until = NULL, last_error = ?3, \
                     attempts = attempts + 1, last_attempt_at = ?2 \
                 WHERE request_id = ?1 AND state = 'sending'",
            )?
            .execute(params![id, now, why])?
        }
    };
    Ok(changed > 0)
}

/// The condition that a row belongs to the agent a hook take speaks for (`?6`): that subagent's rows
/// alone, or — for the main agent (`?6` NULL) — the main agent's, those of subagents that are gone,
/// and those of any subagent whose request ended [`SUBAGENT_GRACE_MS`] (`?7`) ago or more: the
/// `SubagentStop` that marks a subagent gone is sent fail-open, and a lost one must not strand its
/// results.
const AGENT_OF_TAKE: &str = "CASE WHEN ?6 IS NULL \
     THEN (d.agent_id IS NULL OR d.agent_gone_at IS NOT NULL OR r.ended_at + ?7 <= ?1) \
     ELSE d.agent_id = ?6 END";

/// Whether [`take_for_hook`] could take anything for `session`'s `agent_id` at `now` — a read, so a
/// hook that runs on every tool call costs a write only when there is something to hand over. Counts
/// a pending request past its expiry, or a run past its timeout and grace, as a candidate: the take
/// settles those first.
///
/// # Errors
/// A database error.
pub fn any_for_hook(
    conn: &Connection,
    session: &str,
    agent_id: Option<&str>,
    now: i64,
) -> Result<bool> {
    let n: i64 = conn
        .prepare_cached(&format!(
            "SELECT count(*) FROM exec_requests r JOIN exec_delivery d ON d.request_id = r.id \
             WHERE r.session = ?5 AND d.state <> 'delivered' AND {AGENT_OF_TAKE} AND ( \
                 ({DELIVERABLE}) \
                 OR (r.state = 'pending' AND r.expires_at <= ?1) \
                 OR (r.state = 'running' AND r.started_at + r.timeout_ms + ?8 <= ?1))"
        ))?
        .query_row(
            params![
                now,
                WINDOW_MS,
                SETTLED_INTERRUPT,
                LATEST_END_MS,
                session,
                agent_id,
                SUBAGENT_GRACE_MS,
                exec::RUN_GRACE_MS
            ],
            |r| r.get(0),
        )?;
    Ok(n > 0)
}

/// Hand the hook every result of `session` due for `agent_id` (the main agent for `None`: its own,
/// those of subagents that are gone, and those past [`SUBAGENT_GRACE_MS`]): each moves to `sending`
/// under a [`HOOK_LEASE_MS`] lease in this one transaction, and is returned, oldest first, with its
/// row, its output and its delivery — what [`crate::exec::delivery_text`] composes the message from.
/// Delivered only by the hook's [`ack_hook`] once it has printed them; unacknowledged, they are taken
/// again when the lease lapses. Settles expiries first, so a result is never held back for want of an
/// exec op. A row under a live lease is left to its holder.
///
/// # Errors
/// A database error.
pub fn take_for_hook(
    conn: &Connection,
    meta: &WriteMeta,
    session: &str,
    agent_id: Option<&str>,
    now: i64,
) -> Result<Vec<Taken>> {
    exec::settle(conn, meta, now)?;
    let mut ids: Vec<i64> = conn
        .prepare_cached(&format!(
            "UPDATE exec_delivery SET state = 'sending', lease_until = ?8 \
             WHERE request_id IN ( \
                 SELECT r.id FROM exec_requests r JOIN exec_delivery d ON d.request_id = r.id \
                 WHERE r.session = ?5 AND {DELIVERABLE} AND {AGENT_OF_TAKE}) \
             RETURNING request_id"
        ))?
        .query_map(
            params![
                now,
                WINDOW_MS,
                SETTLED_INTERRUPT,
                LATEST_END_MS,
                session,
                agent_id,
                SUBAGENT_GRACE_MS,
                now.saturating_add(HOOK_LEASE_MS)
            ],
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?;
    ids.sort_unstable();
    ids.into_iter().map(|id| taken(conn, id)).collect()
}

/// The hook printed what [`take_for_hook`] handed it: each of `ids` (of `session`) still `sending`
/// moves to `delivered` via `hook`. Returns how many moved — fewer when a lease lapsed and another
/// deliverer took a row, which is then that deliverer's.
///
/// # Errors
/// A database error.
pub fn ack_hook(conn: &Connection, session: &str, ids: &[i64], now: i64) -> Result<usize> {
    let mut n = 0;
    for id in ids {
        n += conn
            .prepare_cached(
                "UPDATE exec_delivery SET state = 'delivered', via = 'hook', delivered_at = ?3, \
                     lease_until = NULL \
                 WHERE request_id = ?1 AND state = 'sending' \
                   AND request_id IN (SELECT id FROM exec_requests WHERE session = ?2)",
            )?
            .execute(params![id, session, now])?;
    }
    Ok(n)
}

/// A result handed over: what its message is composed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taken {
    /// The request.
    pub row: ExecRow,
    /// Its output, if it ran.
    pub output: Option<exec::Output>,
    /// Its delivery row.
    pub delivery: Delivery,
}

impl Taken {
    /// The message, through the one composer.
    #[must_use]
    pub fn text(&self) -> String {
        exec::delivery_text(&self.row, self.output.as_ref(), &self.delivery)
    }
}

fn taken(conn: &Connection, id: i64) -> Result<Taken> {
    let row = exec::get(conn, id)?.ok_or_else(|| {
        crate::Error::from(jkb_types::Error::NotFound(format!(
            "no run request {}",
            exec::wire_id(id)
        )))
    })?;
    let delivery = get(conn, id)?.unwrap_or_else(|| Delivery {
        request_id: id,
        ..Delivery::default()
    });
    Ok(Taken {
        output: exec::output_of(conn, id)?,
        row,
        delivery,
    })
}

/// Subagent `agent_id` of `session` stopped: its undelivered results are due for the session's socket
/// at once. Returns how many rows it marked.
///
/// # Errors
/// A database error.
pub fn agent_gone(conn: &Connection, session: &str, agent_id: &str, now: i64) -> Result<usize> {
    Ok(conn
        .prepare_cached(
            "UPDATE exec_delivery SET agent_gone_at = ?3 \
             WHERE agent_id = ?2 AND agent_gone_at IS NULL \
               AND request_id IN (SELECT id FROM exec_requests WHERE session = ?1)",
        )?
        .execute(params![session, agent_id, now])?)
}

/// `session`'s requests that are still open — pending or running — oldest first: what a session told
/// nothing since a compaction or a resume is reminded of.
///
/// # Errors
/// A database error, or a row this jkb cannot read.
pub fn open(conn: &Connection, session: &str) -> Result<Vec<ExecRow>> {
    let ids: Vec<i64> = conn
        .prepare_cached(
            "SELECT id FROM exec_requests WHERE session = ?1 AND state IN ('pending', 'running') \
             ORDER BY id LIMIT 100",
        )?
        .query_map([session], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter()
        .filter_map(|id| exec::get(conn, id).transpose())
        .collect()
}

#[cfg(test)]
mod tests;
