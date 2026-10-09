//! A design's prompts (design D53.6): one item per Claude Code session that worked the design.
//!
//! A prompt is an item (`kind = 'design_prompt'`) contained by its design, its uid
//! `prompt:<session uuid>`. The app **pre-mints** the session uuid and starts
//! `claude --session-id <uuid>`, and the launch records the prompt *before* Claude starts — so the
//! link between a design and the sessions that worked it is written, not reconstructed from hooks
//! afterwards. Resuming one is `claude --resume <uuid>` in the cwd recorded here.
//!
//! The uid is the session's, so "one prompt per session" is the schema's rule (a uid is unique),
//! not something every caller must remember: recording the same session again is the same item. A
//! terminal restarted on the other side of the container toggle records it again from where it now
//! runs, which moves the recorded cwd; recording it under another design is refused.
//!
//! A record that wrote something is announced on the design's topic (`design/<uid>`, message kind
//! `prompt`), so a Prompts pane showing the design re-reads its list when a launch records — the
//! subscription the editor already holds, never a poll.

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use jkb_types::ItemId;

use super::{announce, design_id, invalid, not_found, set_metadata, PLAN_KIND};
use crate::nstype::tasks::KIND_TASK;
use crate::store::WriteMeta;
use crate::{containment, item, Result};

/// The item kind of a design prompt.
pub const PROMPT_KIND: &str = "design_prompt";
/// The longest prompt title, in characters — what a terminal tab's title may be.
pub const MAX_TITLE_CHARS: usize = 200;
/// The longest recorded cwd, in bytes — what a terminal's cwd may be.
pub const MAX_CWD_BYTES: usize = 4096;

/// What started the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// *Discuss* on a selection of the design's text.
    Discuss,
    /// *Play* on an execution plan.
    Play,
    /// *Play* on one task.
    Task,
    /// *New prompt*: a session the operator started on the design with their own words.
    New,
}

impl Launch {
    /// The stored and printed name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discuss => "discuss",
            Self::Play => "play",
            Self::Task => "task",
            Self::New => "new",
        }
    }

    /// The launch a name means.
    ///
    /// # Errors
    /// A name that is none of `discuss`, `play`, `task`, `new`.
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "discuss" => Ok(Self::Discuss),
            "play" => Ok(Self::Play),
            "task" => Ok(Self::Task),
            "new" => Ok(Self::New),
            other => Err(invalid(format!(
                "a prompt was started by `discuss`, `play`, `task` or `new`, not `{other}`"
            ))),
        }
    }
}

/// A recorded prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRecord {
    /// Its uid: `prompt:<session>`.
    pub uid: String,
    /// The design it worked.
    pub design: String,
    /// The Claude Code session id — what `claude --resume` takes.
    pub session: String,
    /// Where the session runs: the directory `claude --resume` must be run in.
    pub cwd: String,
    /// What started it.
    pub launch: Launch,
    /// What it was started on: the plan or task a *Play* named; `None` for the design itself.
    pub subject: Option<String>,
    /// Its title.
    pub title: String,
    /// When it was first recorded.
    pub created_at: String,
}

/// What [`record`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// The prompt as it now stands.
    pub prompt: PromptRecord,
    /// Whether anything was written (and announced): `false` for a session recorded again from
    /// where it already was.
    pub wrote: bool,
}

/// What [`record`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordPrompt {
    /// The design.
    pub design: String,
    /// The pre-minted session uuid.
    pub session: String,
    /// The absolute directory the session starts in.
    pub cwd: String,
    /// What started it.
    pub launch: Launch,
    /// The plan a *Play* or the task a task's *Play* was started on; `None` for the others.
    pub subject: Option<String>,
    /// Its title.
    pub title: String,
}

/// `session` if it is a lowercase uuid, or why it is not one. Claude Code's `--session-id` takes a
/// uuid, and the uid is built from it, so nothing else is accepted. Another spelling of one is
/// refused rather than rewritten: the launch hands Claude the caller's spelling, so a rewritten
/// record could name a session other than the one Claude was given (unmeasured whether Claude
/// folds case — the refusal makes the question moot). The app mints lowercase ids.
fn session_id(session: &str) -> Result<&str> {
    let groups: Vec<&str> = session.split('-').collect();
    let shaped = groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups.iter().all(|g| {
            g.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        });
    if !shaped {
        return Err(invalid(format!(
            "`{session}` is not a lowercase session uuid — `claude --session-id` takes one"
        )));
    }
    Ok(session)
}

/// Whether `item` is contained, at any depth, by `ancestor`.
fn contained_by(conn: &Connection, item: ItemId, ancestor: ItemId) -> Result<bool> {
    let mut at = item;
    // Bounded like `plan::place_of`: a tree ends, a corrupted table should not spin.
    for _ in 0..128 {
        match containment::parent(conn, at)? {
            Some(p) if p == ancestor => return Ok(true),
            Some(p) => at = p,
            None => return Ok(false),
        }
    }
    Ok(false)
}

/// `subject` checked against the launch: a *Play* names one of this design's plans, a task's *Play*
/// one of its tasks (under a step or directly under the design), and a *Discuss* or *New prompt*
/// nothing — it works the design itself.
fn check_subject(
    conn: &Connection,
    design: ItemId,
    launch: Launch,
    subject: Option<&str>,
) -> Result<()> {
    let Some(s) = subject else {
        return Ok(());
    };
    let (kind, what) = match launch {
        Launch::Play => (PLAN_KIND, "plan"),
        Launch::Task => (KIND_TASK, "task"),
        Launch::Discuss | Launch::New => {
            return Err(invalid(format!(
                "a `{}` prompt works the design itself, so it names no subject, not `{s}`",
                launch.as_str()
            )))
        }
    };
    let id = item::id_for_uid(conn, s)?.ok_or_else(|| not_found(format!("no {what} `{s}`")))?;
    let found = item::get(conn, id)?.map(|m| m.kind).unwrap_or_default();
    if found != kind {
        return Err(invalid(format!(
            "a `{}` prompt is started on a {what}, and `{s}` is a {found}",
            launch.as_str()
        )));
    }
    if !contained_by(conn, id, design)? {
        return Err(invalid(format!("{what} `{s}` is not this design's")));
    }
    Ok(())
}

fn clean_cwd(cwd: &str) -> Result<&str> {
    if !cwd.starts_with('/') || cwd.contains('\0') || cwd.len() > MAX_CWD_BYTES {
        return Err(invalid(format!(
            "a prompt's cwd is an absolute path of at most {MAX_CWD_BYTES} bytes, not `{cwd}`"
        )));
    }
    Ok(cwd)
}

fn clean_title(title: &str) -> Result<String> {
    let line = title.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return Err(invalid("a prompt needs a title"));
    }
    Ok(if line.chars().count() > MAX_TITLE_CHARS {
        let cut: String = line.chars().take(MAX_TITLE_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        line.to_owned()
    })
}

/// The uid a session's prompt has.
#[must_use]
pub fn uid_for(session: &str) -> String {
    format!("prompt:{session}")
}

fn from_row(
    uid: String,
    title: String,
    metadata: &str,
    created_at: String,
) -> Result<PromptRecord> {
    let m: Value = serde_json::from_str(metadata)
        .map_err(|e| invalid(format!("prompt {uid} has unreadable metadata: {e}")))?;
    let text = |key: &str| m.get(key).and_then(Value::as_str).map(str::to_owned);
    let missing = |key: &str| invalid(format!("prompt {uid} records no {key}"));
    Ok(PromptRecord {
        design: text("design").ok_or_else(|| missing("design"))?,
        session: text("session").ok_or_else(|| missing("session"))?,
        cwd: text("cwd").ok_or_else(|| missing("cwd"))?,
        launch: Launch::parse(&text("launch").ok_or_else(|| missing("launch"))?)?,
        subject: text("subject"),
        uid,
        title,
        created_at,
    })
}

/// The prompt item `uid`, if one exists: its id, metadata and record.
fn existing(conn: &Connection, uid: &str) -> Result<Option<(ItemId, Value, PromptRecord)>> {
    let Some(id) = item::id_for_uid(conn, uid)? else {
        return Ok(None);
    };
    let (kind, title, metadata, created_at): (String, Option<String>, String, String) = conn
        .prepare_cached("SELECT kind, content, metadata, created_at FROM items WHERE id = ?1")?
        .query_row([id.get()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
    if kind != PROMPT_KIND {
        return Err(invalid(format!("`{uid}` is a {kind}, not a design prompt")));
    }
    let value: Value = serde_json::from_str(&metadata)
        .map_err(|e| invalid(format!("prompt {uid} has unreadable metadata: {e}")))?;
    let record = from_row(
        uid.to_owned(),
        title.unwrap_or_default(),
        &metadata,
        created_at,
    )?;
    Ok(Some((id, value, record)))
}

/// Record the prompt a session is started with, before it starts. Recording a session already
/// recorded for the same design is that prompt again, its cwd moved to `ask.cwd` (a terminal
/// restarted on the other target).
///
/// # Errors
/// An unknown design, a subject that is not one of the design's plans (for a *Play*) or tasks (for
/// a task's *Play*) or any subject on a *Discuss* or *New prompt*, a session that is not a lowercase
/// uuid, a relative cwd, an empty title, a session already recorded for another design, or a
/// database error.
pub fn record(conn: &Connection, meta: &WriteMeta, ask: &RecordPrompt) -> Result<Recorded> {
    let design = design_id(conn, &ask.design)?;
    let session = session_id(&ask.session)?;
    let cwd = clean_cwd(&ask.cwd)?;
    let title = clean_title(&ask.title)?;
    let subject = ask
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    check_subject(conn, design, ask.launch, subject)?;
    let uid = uid_for(session);
    if let Some((id, mut value, found)) = existing(conn, &uid)? {
        if found.design != ask.design {
            return Err(invalid(format!(
                "session {session} is already recorded for design {} — a session works one design",
                found.design
            )));
        }
        if found.cwd == cwd {
            return Ok(Recorded {
                prompt: found,
                wrote: false,
            });
        }
        value["cwd"] = json!(cwd);
        set_metadata(conn, meta, id, &value)?;
        return recorded(conn, meta, &ask.design, &uid);
    }
    let id = item::upsert(
        conn,
        meta,
        &item::NewItem {
            uid: uid.clone(),
            kind: PROMPT_KIND.to_owned(),
            content: Some(title),
            content_hash: None,
            mime: None,
        },
    )?;
    let mut value = json!({
        "design": ask.design,
        "session": session,
        "cwd": cwd,
        "launch": ask.launch.as_str(),
    });
    if let Some(s) = subject {
        value["subject"] = json!(s);
    }
    set_metadata(conn, meta, id, &value)?;
    let position = i64::try_from(containment::children(conn, design)?.len()).unwrap_or(i64::MAX);
    containment::contain(conn, meta, id, design, position)?;
    recorded(conn, meta, &ask.design, &uid)
}

/// The prompt `uid` as just written, announced on its design's topic so an open Prompts pane
/// re-reads the list — a subscription, never a poll (D53.1).
fn recorded(conn: &Connection, meta: &WriteMeta, design: &str, uid: &str) -> Result<Recorded> {
    announce(
        conn,
        meta,
        design,
        "prompt",
        json!({ "design": design, "prompt": uid }),
    )?;
    let prompt = existing(conn, uid)?
        .map(|(_, _, r)| r)
        .ok_or_else(|| invalid(format!("prompt {uid} vanished while it was recorded")))?;
    Ok(Recorded {
        prompt,
        wrote: true,
    })
}

/// The prompt a Claude Code session was recorded with, if it was one a launch recorded: the link
/// from a session back to the design it worked (the Code Factory's *Jump to context*, D53.9).
/// `None` for a session no launch recorded — one started outside the app, or with an id that is
/// not a uuid, which no launch can have recorded.
///
/// # Errors
/// An item named like a prompt that is not one, unreadable metadata, or a database error.
pub fn of_session(conn: &Connection, session: &str) -> Result<Option<PromptRecord>> {
    // Looked up in any case: every record is lowercase (`record` refuses another spelling), so
    // folding here can only find the one session it names.
    let lower = session.to_ascii_lowercase();
    let Ok(session) = session_id(&lower) else {
        return Ok(None);
    };
    Ok(existing(conn, &uid_for(session))?.map(|(_, _, record)| record))
}

/// A design's prompts, newest first.
///
/// # Errors
/// An unknown design, a prompt with unreadable metadata, or a database error.
pub fn list(conn: &Connection, design: &str) -> Result<Vec<PromptRecord>> {
    let design_item = design_id(conn, design)?;
    let mut stmt = conn.prepare_cached(
        "SELECT i.uid, COALESCE(i.content, ''), i.metadata, i.created_at FROM containment c
           JOIN items i ON i.id = c.child_item_id
          WHERE c.parent_item_id = ?1 AND i.kind = ?2
          ORDER BY c.position DESC, i.id DESC",
    )?;
    let rows = stmt
        .query_map(params![design_item.get(), PROMPT_KIND], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(uid, title, metadata, created_at)| from_row(uid, title, &metadata, created_at))
        .collect()
}
