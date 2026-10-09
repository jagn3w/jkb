//! `design.prompt_record`, `design.prompts` and the *New prompt* prompt (design D53.6), over
//! [`jkb_core::design::prompts`].
//!
//! A design prompt is the record of one Claude Code session that worked the design: its pre-minted
//! session uuid and the directory it runs in, written by the launch before Claude starts so the
//! Prompts pane can resume it (`claude --resume <uuid>` there). `jkb design prompt record|ls` is the
//! CLI for the same ops, and `jkb design prompt new` prints the prompt a *New prompt* starts with.

use std::fmt::Write as _;

use jkb_core::design::{self, prompts, Launch, PromptRecord};
use jkb_core::WriteMeta;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::{invalid, longest_backtick_run};
use crate::ApiError;

/// The longest request a *New prompt* carries, in bytes. The prompt is an argument of the program
/// it starts (the terminal caps its argv at 64 KiB), and a longer brief belongs in the design.
pub const MAX_NEW_PROMPT_BYTES: usize = 16 * 1024;

/// A recorded prompt, as `design.prompts` and `design.prompt_record` answer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignPrompt {
    /// Its uid: `prompt:<session>`.
    pub uid: String,
    /// The design it worked.
    pub design: String,
    /// The Claude Code session id, what `claude --resume` takes.
    pub session: String,
    /// The directory the session runs in, where it is resumed.
    pub cwd: String,
    /// What started it: `discuss`, `play`, `task` or `new`.
    pub launch: String,
    /// The plan or task it was started on.
    pub subject: Option<String>,
    /// Its title.
    pub title: String,
    /// When it was first recorded.
    pub created_at: String,
}

impl From<PromptRecord> for DesignPrompt {
    fn from(p: PromptRecord) -> Self {
        Self {
            uid: p.uid,
            design: p.design,
            session: p.session,
            cwd: p.cwd,
            launch: p.launch.as_str().to_owned(),
            subject: p.subject,
            title: p.title,
            created_at: p.created_at,
        }
    }
}

/// A `design.prompt_record`'s fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordAsk {
    /// The design.
    pub uid: String,
    /// The pre-minted session uuid.
    pub session: String,
    /// The absolute directory the session starts in.
    pub cwd: String,
    /// `discuss`, `play`, `task` or `new`.
    pub launch: String,
    /// The plan or task it is started on.
    pub subject: Option<String>,
    /// Its title.
    pub title: String,
}

/// `design.prompt_record`: the prompt as it now stands, and whether anything was written (and so
/// announced on the design's topic).
///
/// # Errors
/// An unknown launch, or the engine's refusal.
pub fn record(
    conn: &Connection,
    meta: &WriteMeta,
    ask: RecordAsk,
) -> Result<(DesignPrompt, bool), ApiError> {
    let launch = Launch::parse(&ask.launch)?;
    let done = prompts::record(
        conn,
        meta,
        &prompts::RecordPrompt {
            design: ask.uid,
            session: ask.session,
            cwd: ask.cwd,
            launch,
            subject: ask.subject,
            title: ask.title,
        },
    )?;
    Ok((done.prompt.into(), done.wrote))
}

/// `design.prompts`: a design's prompts, newest first.
///
/// # Errors
/// An unknown design.
pub fn list(conn: &Connection, uid: &str) -> Result<Vec<DesignPrompt>, ApiError> {
    Ok(prompts::list(conn, uid)?
        .into_iter()
        .map(DesignPrompt::from)
        .collect())
}

/// `design.prompt_of`: the prompt a session was recorded with, or `None` when no launch recorded
/// it (D53.9).
///
/// # Errors
/// A database error, or a record that cannot be read.
pub fn of_session(conn: &Connection, session: &str) -> Result<Option<DesignPrompt>, ApiError> {
    Ok(prompts::of_session(conn, session)?.map(DesignPrompt::from))
}

/// A *New prompt*'s prompt, and what it was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPrompt {
    /// `new`.
    pub kind: String,
    /// The design.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// The prompt itself: what the session is started with.
    pub prompt: String,
}

/// The *New prompt* prompt (D53.6): a session on the design started with the operator's own
/// words, told what the design is and how Claude reads and edits it.
///
/// # Errors
/// An unknown design, or a request longer than [`MAX_NEW_PROMPT_BYTES`].
pub fn new_prompt(conn: &Connection, uid: &str, text: &str) -> Result<NewPrompt, ApiError> {
    let text = text.trim();
    if text.len() > MAX_NEW_PROMPT_BYTES {
        return Err(invalid(format!(
            "a new prompt is at most {MAX_NEW_PROMPT_BYTES} bytes — put a longer brief in the \
             design and point at it"
        )));
    }
    let title = jkb_core::item::get(conn, design::design_id(conn, uid)?)?
        .and_then(|m| m.content)
        .unwrap_or_default();
    let mut p = String::new();
    let _ = writeln!(
        p,
        "The operator started a Claude session on the design \"{title}\" ({uid}) in Code Factory."
    );
    let _ = writeln!(p);
    let _ = writeln!(
        p,
        "Read the design first: `jkb design cat {uid}` prints it with span markers and a version \
         token, and `jkb design plan ls {uid}` its execution plans and their tasks. Change its text \
         only through the CLI, against the version you read — {} — and only when the operator \
         asks. Editing approved text makes it PROPOSED again until it is re-approved.",
        super::edit_usage(uid)
    );
    let _ = writeln!(p);
    if text.is_empty() {
        let _ = writeln!(
            p,
            "The operator has not said what they want yet: read the design, then ask them."
        );
    } else {
        let fence = "`".repeat(longest_backtick_run(text).max(2) + 1);
        let _ = writeln!(p, "The operator's request:");
        let _ = writeln!(p, "{fence}\n{text}\n{fence}");
    }
    Ok(NewPrompt {
        kind: "new".to_owned(),
        uid: uid.to_owned(),
        title,
        prompt: p,
    })
}
