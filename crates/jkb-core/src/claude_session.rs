//! The Claude Code session registry (tasks S6.4, `openspec/changes/jkb-message-queue/design-s6-4.md`).
//!
//! A verb that finds a claim or a lock held by a Claude Code session asks here whether that session has
//! ended. The hook feeds it: `SessionStart` makes a session live, `SessionEnd` ends it, and the
//! `SessionStart` sweep ends a session it proved gone. What was measured decides the shape (the table
//! in the design): a `SessionEnd` proves an end, but a killed `claude` or a restarted container sends
//! none, so **only an ended row is evidence** — a live row is a claim nobody has disproved yet, and a
//! session with no row is unknown. An ended session is not final either: `claude --resume` keeps the id
//! and starts it again.
//!
//! **An end applies only to the process it is about.** `claude --resume` runs the same id in a new
//! process, possibly while the old one is still exiting, so an end or a sweep verdict carries the pid
//! and instance it came from and is ignored once the row names another. The rule is
//! [`crate::notify::gone`]'s, for the same race.
//!
//! Like `notify_sessions`, this is observation rather than content: not changelogged, and never
//! touched by `jkb undo`.

use rusqlite::{params, Connection, OptionalExtension};

use crate::notify::sanitize;
use crate::{Result, WriteMeta};

/// A row neither started nor ended for this long is deleted when another session starts. Deleting
/// one makes its session unknown, which licenses nothing — the safe direction for a record nobody
/// can prove anything about any more (a rebuilt container's sessions, whose hostname never recurs).
pub const PRUNE_AFTER_MS: i64 = 90 * 24 * 60 * 60 * 1000;

/// The most rows [`list`] returns.
pub const LIST_CAP: usize = 1000;

const MAX_SESSION_BYTES: usize = 200;
const MAX_PID_BYTES: usize = 20;
const MAX_INSTANCE_BYTES: usize = 150;
const MAX_CWD_BYTES: usize = 4096;
const MAX_WORD_BYTES: usize = 32;

/// A session as the registry holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// The session id.
    pub session: String,
    /// The `claude` process that last started it, or empty.
    pub pid: String,
    /// Where `pid` means something: `host[#boot][/pidns]`.
    pub instance: String,
    /// Its working directory, as the hook reported it.
    pub cwd: String,
    /// When it last started (Unix ms), or `None` when only its end was seen.
    pub started_at: Option<i64>,
    /// How it last started (`startup`, `resume`, `clear`, `compact`, …).
    pub start_source: Option<String>,
    /// When it ended (Unix ms), or `None` while live.
    pub ended_at: Option<i64>,
    /// Why (`prompt_input_exit`, `clear`, `resume`, `other`, …, or [`GONE`]).
    pub end_reason: Option<String>,
}

impl SessionRow {
    /// Whether it has ended — the one answer that is evidence.
    #[must_use]
    pub const fn ended(&self) -> bool {
        self.ended_at.is_some()
    }
}

/// The end reason a sweep records for a session it proved gone.
pub const GONE: &str = "gone";

/// A session start, as the hook observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start<'a> {
    /// The session id, already [`sanitize`]d.
    pub session: &'a str,
    /// The `claude` process, or empty.
    pub pid: &'a str,
    /// Where `pid` means something.
    pub instance: &'a str,
    /// The working directory.
    pub cwd: &'a str,
    /// The payload's `source`.
    pub source: &'a str,
}

/// What a start did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    /// The session was not known.
    New,
    /// It was live; the row now names this process.
    Restarted,
    /// It had ended, and is live again.
    Revived,
}

impl Started {
    /// The snake-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Restarted => "restarted",
            Self::Revived => "revived",
        }
    }
}

/// What an end did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The session is now ended.
    Recorded,
    /// It had already ended; the first reason stands.
    AlreadyEnded,
    /// The row names another process — the id was resumed elsewhere — so nothing changed.
    OtherProcess,
}

impl Ended {
    /// The snake-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::AlreadyEnded => "already_ended",
            Self::OtherProcess => "other_process",
        }
    }
}

fn invalid(why: String) -> crate::Error {
    jkb_types::Error::Validation(why).into()
}

fn check_session(session: &str) -> Result<()> {
    if session.is_empty() || session.len() > MAX_SESSION_BYTES || sanitize(session) != session {
        return Err(invalid(format!(
            "a session id is 1..={MAX_SESSION_BYTES} bytes of [A-Za-z0-9_-] ({session:?} given)"
        )));
    }
    Ok(())
}

fn check_process(pid: &str, instance: &str) -> Result<()> {
    if pid.len() > MAX_PID_BYTES || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid(format!("{pid:?} is not a pid")));
    }
    if instance.len() > MAX_INSTANCE_BYTES || instance.chars().any(char::is_control) {
        return Err(invalid(format!(
            "an instance is at most {MAX_INSTANCE_BYTES} bytes with no control characters"
        )));
    }
    Ok(())
}

/// A start source or end reason: Claude Code names them, and may add more, so any short snake-case
/// word is accepted rather than a list this build happens to know.
fn check_word(what: &str, word: &str) -> Result<()> {
    if word.is_empty()
        || word.len() > MAX_WORD_BYTES
        || !word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(invalid(format!(
            "a {what} is 1..={MAX_WORD_BYTES} bytes of [a-z0-9_] ({word:?} given)"
        )));
    }
    Ok(())
}

const COLUMNS: &str = "session, pid, instance, cwd, started_at, start_source, ended_at, end_reason";

fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session: r.get(0)?,
        pid: r.get(1)?,
        instance: r.get(2)?,
        cwd: r.get(3)?,
        started_at: r.get(4)?,
        start_source: r.get(5)?,
        ended_at: r.get(6)?,
        end_reason: r.get(7)?,
    })
}

/// One session, or `None` when it is unknown.
///
/// # Errors
/// A database error.
pub fn get(conn: &Connection, session: &str) -> Result<Option<SessionRow>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM claude_sessions WHERE session = ?1"
        ))?
        .query_row([session], row_to_session)
        .optional()?)
}

/// A session started (or resumed, cleared into, compacted): it is live, held by `start`'s process.
/// Also prunes rows idle past [`PRUNE_AFTER_MS`].
///
/// # Errors
/// A validation error for a malformed id, pid, instance, cwd or source; or a database error.
pub fn started(
    conn: &Connection,
    _meta: &WriteMeta,
    start: &Start<'_>,
    now: i64,
) -> Result<Started> {
    check_session(start.session)?;
    check_process(start.pid, start.instance)?;
    check_word("start source", start.source)?;
    if start.cwd.len() > MAX_CWD_BYTES {
        return Err(invalid(format!(
            "a working directory of at most {MAX_CWD_BYTES} bytes"
        )));
    }
    conn.prepare_cached(
        "DELETE FROM claude_sessions WHERE coalesce(ended_at, started_at) < ?1 AND session <> ?2",
    )?
    .execute(params![now.saturating_sub(PRUNE_AFTER_MS), start.session])?;
    let before = get(conn, start.session)?;
    conn.prepare_cached(
        "INSERT INTO claude_sessions (session, pid, instance, cwd, started_at, start_source) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(session) DO UPDATE SET pid = excluded.pid, instance = excluded.instance, \
             cwd = excluded.cwd, started_at = excluded.started_at, \
             start_source = excluded.start_source, ended_at = NULL, end_reason = NULL",
    )?
    .execute(params![
        start.session,
        start.pid,
        start.instance,
        start.cwd,
        now,
        start.source
    ])?;
    Ok(match before {
        None => Started::New,
        Some(r) if r.ended() => Started::Revived,
        Some(_) => Started::Restarted,
    })
}

/// A session ended, as the process `pid` in `instance` reported. A session never seen starting is
/// recorded as ended all the same: that is still evidence.
///
/// # Errors
/// A validation error for a malformed id, pid, instance or reason; or a database error.
pub fn ended(
    conn: &Connection,
    _meta: &WriteMeta,
    session: &str,
    pid: &str,
    instance: &str,
    reason: &str,
    now: i64,
) -> Result<Ended> {
    check_session(session)?;
    check_process(pid, instance)?;
    check_word("end reason", reason)?;
    match get(conn, session)? {
        None => {
            conn.prepare_cached(
                "INSERT INTO claude_sessions (session, pid, instance, cwd, ended_at, end_reason) \
                 VALUES (?1, ?2, ?3, '', ?4, ?5)",
            )?
            .execute(params![session, pid, instance, now, reason])?;
            Ok(Ended::Recorded)
        }
        Some(r) if r.ended() => Ok(Ended::AlreadyEnded),
        Some(r) if r.pid != pid || r.instance != instance => Ok(Ended::OtherProcess),
        Some(_) => {
            end(conn, session, pid, instance, reason, now)?;
            Ok(Ended::Recorded)
        }
    }
}

/// A producer proved `session` gone — its process `pid` in `instance` no longer exists. Ends it only
/// while the row is live and still names exactly that process; an empty pid proves nothing. Returns
/// whether it ended.
///
/// # Errors
/// A validation error for a malformed id, pid or instance; or a database error.
pub fn gone(
    conn: &Connection,
    _meta: &WriteMeta,
    session: &str,
    pid: &str,
    instance: &str,
    now: i64,
) -> Result<bool> {
    check_session(session)?;
    check_process(pid, instance)?;
    if pid.is_empty() {
        return Ok(false);
    }
    Ok(end(conn, session, pid, instance, GONE, now)? > 0)
}

/// End a live row still naming `pid` in `instance`, in one statement, so nothing can change between
/// the check and the write.
fn end(
    conn: &Connection,
    session: &str,
    pid: &str,
    instance: &str,
    reason: &str,
    now: i64,
) -> Result<usize> {
    Ok(conn
        .prepare_cached(
            "UPDATE claude_sessions SET ended_at = ?1, end_reason = ?2 \
             WHERE session = ?3 AND pid = ?4 AND instance = ?5 AND ended_at IS NULL",
        )?
        .execute(params![now, reason, session, pid, instance])?)
}

/// Sessions, at most [`LIST_CAP`]: the live ones, oldest start first — what a sweep probes, and the
/// likeliest to be gone come first — or, with `all`, every one, most recent activity first.
///
/// # Errors
/// A database error.
pub fn list(conn: &Connection, all: bool) -> Result<Vec<SessionRow>> {
    let sql = if all {
        format!(
            "SELECT {COLUMNS} FROM claude_sessions \
             ORDER BY coalesce(ended_at, started_at) DESC, session LIMIT ?1"
        )
    } else {
        format!(
            "SELECT {COLUMNS} FROM claude_sessions WHERE ended_at IS NULL \
             ORDER BY started_at, session LIMIT ?1"
        )
    };
    let cap = i64::try_from(LIST_CAP).unwrap_or(i64::MAX);
    Ok(conn
        .prepare_cached(&sql)?
        .query_map([cap], row_to_session)?
        .collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests;
