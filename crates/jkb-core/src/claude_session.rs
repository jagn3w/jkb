//! The Claude Code session registry (tasks S6.4, `openspec/changes/jkb-message-queue/design-s6-4.md`).
//!
//! A verb that finds a claim or a lock held by a Claude Code session asks here whether that session has
//! ended. What was measured decides the shape (the table in the design): a `SessionEnd` proves an end,
//! but a killed `claude` or a restarted container sends none, so **only an ended session is evidence**
//! — a live one is a claim nobody has disproved yet, and a session with no rows is unknown.
//!
//! **A row is a process holding a session**, keyed by (session, pid, instance). `claude --resume` runs
//! an id in a new process, possibly while another process still runs it, so a session is
//! [`SessionState::Live`] while any of its rows is live, and one process's end or proven death ends
//! only its own row. A row-per-session design let the later process's exit mark a session ended that
//! the earlier one was still running (stage-1 review).
//!
//! The hook feeds it:
//! - `SessionStart` makes this process's row live, reviving it if it had ended ([`started`]). It does
//!   **not** end the process's rows for other sessions: after `/clear` or `/resume` a lost `SessionEnd`
//!   therefore leaves the old session live until the process exits, which costs a `--force`. Ending
//!   them would assume one process runs one session at a time, and a process hosting several (the
//!   Agent SDK) would then have a running session recorded as ended — the one answer that is evidence.
//!   Of the two ways to be wrong, the recoverable one is chosen (stage-1 review, round 2);
//! - every other hook event marks the process seen ([`seen`]), which repairs a start that was lost
//!   (daemon busy or restarting) — otherwise a resumed session could run for hours recorded as ended,
//!   since nothing else ever makes a row live;
//! - `SessionEnd` ends the row ([`ended`]);
//! - the `SessionStart` sweep ends the rows whose process it proved gone ([`gone`]).
//!
//! Like `notify_sessions`, this is observation rather than content: not changelogged, and never
//! touched by `jkb undo`.

use rusqlite::{params, Connection, OptionalExtension};

use crate::notify::{check_owner_and_instance, check_session};
use crate::{Result, WriteMeta};

/// A session none of whose rows has been seen for this long is deleted, whole, when another session
/// starts. Deleting it makes it unknown, which licenses nothing — the safe direction for a record nobody
/// can prove anything about any more (a rebuilt container's sessions, whose hostname never recurs).
/// Whole sessions only: deleting one stale live row beside a recent ended one would turn a live session
/// into an ended one, the single answer that is evidence (stage-1 review, round 2).
pub const PRUNE_AFTER_MS: i64 = 90 * 24 * 60 * 60 * 1000;

/// How stale a live row's `seen_at` may get before [`seen`] rewrites it. `seen` runs on every tool call,
/// and a write per call on the daemon's one writer buys nothing: the prune works in days.
pub const SEEN_REFRESH_MS: i64 = 60 * 60 * 1000;

/// The most rows one [`list`] page returns.
pub const LIST_CAP: usize = 1000;

/// The longest working directory kept, in bytes; a longer one is cut.
pub const MAX_CWD_BYTES: usize = 4096;

const MAX_WORD_BYTES: usize = 32;

/// The word recorded for a start source or end reason that is absent or not a short snake-case word.
pub const UNKNOWN: &str = "unknown";

/// The end reason a sweep records for a process it proved gone.
pub const GONE: &str = "gone";

/// One process's hold on a session, as the registry keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolderRow {
    /// The session id.
    pub session: String,
    /// The `claude` process, or empty when the hook had none it could trust.
    pub pid: String,
    /// Where `pid` means something: `host[#boot][/pidns]`.
    pub instance: String,
    /// The working directory, as the hook reported it.
    pub cwd: String,
    /// When this process last started the session (Unix ms), or `None` when it was first seen otherwise.
    pub started_at: Option<i64>,
    /// How it last started (`startup`, `resume`, `clear`, `compact`, …).
    pub start_source: Option<String>,
    /// The last event from this process (Unix ms), refreshed at most every [`SEEN_REFRESH_MS`].
    pub seen_at: i64,
    /// When it ended (Unix ms), or `None` while live.
    pub ended_at: Option<i64>,
    /// Why (`prompt_input_exit`, `clear`, `resume`, `other`, …, or [`GONE`]).
    pub end_reason: Option<String>,
}

impl HolderRow {
    /// Whether this process's hold has ended.
    #[must_use]
    pub const fn ended(&self) -> bool {
        self.ended_at.is_some()
    }
}

/// A session's state, from all its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// No row: nothing is known, and nothing is licensed.
    Unknown,
    /// Some process still holds it, as far as anyone has shown.
    Live,
    /// Every process that held it has ended — the one answer that is evidence. Never the answer for a
    /// session with a pid-less row: processes the hook could not name share that one row, so its end
    /// is only one of theirs.
    Ended,
}

impl SessionState {
    /// The snake-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Live => "live",
            Self::Ended => "ended",
        }
    }
}

/// Which process an event is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Process<'a> {
    /// The session id, already [`crate::notify::sanitize`]d.
    pub session: &'a str,
    /// The `claude` process, or empty.
    pub pid: &'a str,
    /// Where `pid` means something.
    pub instance: &'a str,
}

impl Process<'_> {
    /// The identity checks the notification ops use — including that a pid needs its instance.
    fn check(&self) -> Result<()> {
        check_session(self.session)?;
        check_owner_and_instance(self.pid, self.instance)
    }
}

/// What an end did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// This process's hold is now ended.
    Recorded,
    /// It had already ended; the first reason stands.
    AlreadyEnded,
}

impl Ended {
    /// The snake-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::AlreadyEnded => "already_ended",
        }
    }
}

/// A start source or end reason. Claude Code names them and may add more, and neither decides anything,
/// so an unusual one is recorded as [`UNKNOWN`] rather than refused: a refused start is the one lost
/// write that leaves a running session recorded as ended until its next event.
fn word(w: &str) -> &str {
    let ok = !w.is_empty()
        && w.len() <= MAX_WORD_BYTES
        && w.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        w
    } else {
        UNKNOWN
    }
}

/// A working directory, cut to [`MAX_CWD_BYTES`] at a character boundary: it is only shown.
fn cwd(c: &str) -> &str {
    if c.len() <= MAX_CWD_BYTES {
        return c;
    }
    let mut end = MAX_CWD_BYTES;
    while !c.is_char_boundary(end) {
        end -= 1;
    }
    &c[..end]
}

const COLUMNS: &str =
    "session, pid, instance, cwd, started_at, start_source, seen_at, ended_at, end_reason";

fn row_to_holder(r: &rusqlite::Row<'_>) -> rusqlite::Result<HolderRow> {
    Ok(HolderRow {
        session: r.get(0)?,
        pid: r.get(1)?,
        instance: r.get(2)?,
        cwd: r.get(3)?,
        started_at: r.get(4)?,
        start_source: r.get(5)?,
        seen_at: r.get(6)?,
        ended_at: r.get(7)?,
        end_reason: r.get(8)?,
    })
}

/// A session's state, from all its rows.
///
/// A session with a pid-less row is never [`SessionState::Ended`]: every process the hook could not
/// name, on one instance, shares that row, so one of them ending ends it for all — the false end the
/// per-process rows exist to prevent (stage-1 review, round 3). Such a session is live while any row is,
/// and unknown after.
///
/// # Errors
/// A database error.
pub fn state(conn: &Connection, session: &str) -> Result<SessionState> {
    let (rows, live, blind): (i64, i64, i64) = conn
        .prepare_cached(
            "SELECT count(*), count(*) FILTER (WHERE ended_at IS NULL), \
                    count(*) FILTER (WHERE pid = '') \
             FROM claude_sessions WHERE session = ?1",
        )?
        .query_row([session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(match (rows, live, blind) {
        (0, _, _) => SessionState::Unknown,
        (_, 0, 0) => SessionState::Ended,
        (_, 0, _) => SessionState::Unknown,
        _ => SessionState::Live,
    })
}

/// Every process that has held `session`, most recently seen first.
///
/// # Errors
/// A database error.
pub fn holders(conn: &Connection, session: &str) -> Result<Vec<HolderRow>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM claude_sessions WHERE session = ?1 \
             ORDER BY seen_at DESC, pid, instance"
        ))?
        .query_map([session], row_to_holder)?
        .collect::<rusqlite::Result<_>>()?)
}

fn holder(conn: &Connection, p: &Process<'_>) -> Result<Option<HolderRow>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM claude_sessions \
             WHERE session = ?1 AND pid = ?2 AND instance = ?3"
        ))?
        .query_row(params![p.session, p.pid, p.instance], row_to_holder)
        .optional()?)
}

/// `p` started its session (or resumed it, cleared into it, compacted): its row is live. Its rows for
/// other sessions are left alone (the module doc says why). Also prunes sessions not seen for
/// [`PRUNE_AFTER_MS`]. Returns the session's state **before** — a revival is
/// [`SessionState::Ended`], a compaction or a second process joining a running session
/// [`SessionState::Live`].
///
/// # Errors
/// A validation error for a malformed session, pid or instance; or a database error.
pub fn started(
    conn: &Connection,
    _meta: &WriteMeta,
    p: &Process<'_>,
    dir: &str,
    source: &str,
    now: i64,
) -> Result<SessionState> {
    p.check()?;
    conn.prepare_cached(
        "DELETE FROM claude_sessions WHERE session IN ( \
             SELECT session FROM claude_sessions GROUP BY session HAVING max(seen_at) < ?1 \
         ) AND session <> ?2",
    )?
    .execute(params![now.saturating_sub(PRUNE_AFTER_MS), p.session])?;
    let before = state(conn, p.session)?;
    conn.prepare_cached(
        "INSERT INTO claude_sessions \
             (session, pid, instance, cwd, started_at, start_source, seen_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?5) \
         ON CONFLICT(session, pid, instance) DO UPDATE SET cwd = excluded.cwd, \
             started_at = excluded.started_at, start_source = excluded.start_source, \
             seen_at = excluded.seen_at, ended_at = NULL, end_reason = NULL",
    )?
    .execute(params![
        p.session,
        p.pid,
        p.instance,
        cwd(dir),
        now,
        word(source)
    ])?;
    Ok(before)
}

/// `p` sent a hook event other than its start or end: it is running, so its row is live. Inserts or
/// revives the row; a live row's `seen_at` is refreshed only once it is [`SEEN_REFRESH_MS`] old, so the
/// common case — every tool call of a known session — changes nothing.
///
/// # Errors
/// A validation error for a malformed session, pid or instance; or a database error.
pub fn seen(
    conn: &Connection,
    _meta: &WriteMeta,
    p: &Process<'_>,
    dir: &str,
    now: i64,
) -> Result<()> {
    p.check()?;
    conn.prepare_cached(
        "INSERT INTO claude_sessions (session, pid, instance, cwd, seen_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(session, pid, instance) DO UPDATE SET seen_at = excluded.seen_at, \
             ended_at = NULL, end_reason = NULL \
         WHERE claude_sessions.ended_at IS NOT NULL OR claude_sessions.seen_at < ?6",
    )?
    .execute(params![
        p.session,
        p.pid,
        p.instance,
        cwd(dir),
        now,
        now.saturating_sub(SEEN_REFRESH_MS)
    ])?;
    Ok(())
}

/// `p` reported its session ended. Ends only `p`'s row; a process never seen starting is recorded as
/// ended all the same — that is still evidence about it.
///
/// # Errors
/// A validation error for a malformed session, pid or instance; or a database error.
pub fn ended(
    conn: &Connection,
    _meta: &WriteMeta,
    p: &Process<'_>,
    reason: &str,
    now: i64,
) -> Result<Ended> {
    p.check()?;
    if holder(conn, p)?.is_some_and(|h| h.ended()) {
        return Ok(Ended::AlreadyEnded);
    }
    conn.prepare_cached(
        "INSERT INTO claude_sessions (session, pid, instance, cwd, seen_at, ended_at, end_reason) \
         VALUES (?1, ?2, ?3, '', ?4, ?4, ?5) \
         ON CONFLICT(session, pid, instance) DO UPDATE SET seen_at = excluded.seen_at, \
             ended_at = excluded.ended_at, end_reason = excluded.end_reason",
    )?
    .execute(params![p.session, p.pid, p.instance, now, word(reason)])?;
    Ok(Ended::Recorded)
}

/// A producer proved `p` gone — no such process exists any more. Ends `p`'s row if it is live; a pid-less
/// process was never probed, so nothing is ended for one. Returns whether a row ended.
///
/// # Errors
/// A validation error for a malformed session, pid or instance; or a database error.
pub fn gone(conn: &Connection, _meta: &WriteMeta, p: &Process<'_>, now: i64) -> Result<bool> {
    p.check()?;
    if p.pid.is_empty() {
        return Ok(false);
    }
    let changed = conn
        .prepare_cached(
            "UPDATE claude_sessions SET ended_at = ?1, end_reason = ?2, seen_at = ?1 \
             WHERE session = ?3 AND pid = ?4 AND instance = ?5 AND ended_at IS NULL",
        )?
        .execute(params![now, GONE, p.session, p.pid, p.instance])?;
    Ok(changed > 0)
}

/// Where a [`list`] page ended: the last row it returned, in the listing's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// The row's `seen_at`.
    pub seen_at: i64,
    /// The row's session.
    pub session: String,
    /// The row's pid.
    pub pid: String,
    /// The row's instance.
    pub instance: String,
}

/// One page of rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// At most [`LIST_CAP`].
    pub rows: Vec<HolderRow>,
    /// Where to continue, when there are more.
    pub next: Option<Cursor>,
}

/// A page of rows, after `after`: the live ones, least recently seen first — what a sweep probes, and
/// the likeliest to be gone come first — or, with `all`, every one, most recently seen first.
///
/// The pages are keyset-ordered on (`seen_at`, session, pid, instance), so rows that do not change
/// between two pages are each returned once. A row written between pages moves: its `seen_at` rises, so
/// in the live order it may be returned again (harmless to a sweep, whose verdicts are compare-and-set)
/// and in the `all` order it may be skipped — `jkb notify sessions --all` is a listing, not a snapshot.
///
/// # Errors
/// A database error.
pub fn list(conn: &Connection, all: bool, after: Option<&Cursor>) -> Result<Page> {
    let (filter, cmp, dir) = if all {
        ("1", "<", "DESC")
    } else {
        ("ended_at IS NULL", ">", "ASC")
    };
    let sql = format!(
        "SELECT {COLUMNS} FROM claude_sessions \
         WHERE {filter} AND (?1 = 0 OR (seen_at, session, pid, instance) {cmp} (?2, ?3, ?4, ?5)) \
         ORDER BY seen_at {dir}, session {dir}, pid {dir}, instance {dir} LIMIT ?6"
    );
    let fetch = i64::try_from(LIST_CAP + 1).unwrap_or(i64::MAX);
    let (seen_at, session, pid, instance) = after.map_or((0, "", "", ""), |c| {
        (
            c.seen_at,
            c.session.as_str(),
            c.pid.as_str(),
            c.instance.as_str(),
        )
    });
    let mut rows: Vec<HolderRow> = conn
        .prepare_cached(&sql)?
        .query_map(
            params![after.is_some(), seen_at, session, pid, instance, fetch],
            row_to_holder,
        )?
        .collect::<rusqlite::Result<_>>()?;
    let next = if rows.len() > LIST_CAP {
        rows.truncate(LIST_CAP);
        rows.last().map(|r| Cursor {
            seen_at: r.seen_at,
            session: r.session.clone(),
            pid: r.pid.clone(),
            instance: r.instance.clone(),
        })
    } else {
        None
    };
    Ok(Page { rows, next })
}

#[cfg(test)]
mod tests;
