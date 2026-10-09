//! `design.*`: design documents as CRDTs (design D53.4–5), over [`jkb_core::design`].
//!
//! The ops only carry the engine's answers; every rule — the version-token edit, the merge, the
//! reviewer a span names, the derived span states — is the engine's. Update bytes and state vectors
//! cross the wire as standard base64 (an app's `btoa`/`Uint8Array` round trip); a version is the
//! opaque token `jkb design cat` prints.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use jkb_core::design::{self, Approver, DesignRow, DesignText, Edit, SpanView};
use jkb_core::WriteMeta;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::rbac::Principal;
use crate::{ApiError, ErrorCode};

pub mod plans;
pub mod prompts;

fn invalid(why: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::Invalid, why)
}

/// Decode a base64 field.
fn bytes(field: &str, value: &str) -> Result<Vec<u8>, ApiError> {
    STANDARD
        .decode(value)
        .map_err(|e| invalid(format!("`{field}` is not base64: {e}")))
}

/// A design, as `design.list` and `design.create` answer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Design {
    /// Its uid.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// Where it lives (`designs/<repo>`).
    pub namespace: Option<String>,
    /// Its newest update's seq.
    pub seq: i64,
    /// The `mq` topic its updates are announced on.
    pub topic: String,
    /// Where `jkb design export` writes it, relative to the repository root (D55.6).
    #[serde(default)]
    pub doc_target: Option<String>,
    /// The files it was made from, each with the blake3 of its content then (D55.5).
    #[serde(default)]
    pub sources: Vec<Source>,
}

impl From<DesignRow> for Design {
    fn from(d: DesignRow) -> Self {
        Self {
            topic: design::topic(&d.uid),
            uid: d.uid,
            title: d.title,
            namespace: d.namespace,
            seq: d.seq,
            doc_target: d.meta.doc_target,
            sources: d.meta.sources.into_iter().map(Source::from).collect(),
        }
    }
}

/// A file a design was made from (D55.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Relative to the repository root.
    pub path: String,
    /// Lowercase hex blake3 of its content when it was recorded.
    pub blake3: String,
}

impl From<design::Source> for Source {
    fn from(s: design::Source) -> Self {
        Self {
            path: s.path,
            blake3: s.blake3,
        }
    }
}

impl From<Source> for design::Source {
    fn from(s: Source) -> Self {
        Self {
            path: s.path,
            blake3: s.blake3,
        }
    }
}

/// A design rendered for its doc target, as `design.export` answers it (D55.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Export {
    /// The design.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// Where it is written, relative to the repository root; `None` when it names no target.
    pub doc_target: Option<String>,
    /// The version token it was rendered at.
    pub version: String,
    /// The whole file: the generated header, then the approved text.
    pub text: String,
}

impl From<design::Exported> for Export {
    fn from(e: design::Exported) -> Self {
        Self {
            uid: e.uid,
            title: e.title,
            doc_target: e.doc_target,
            version: e.version.token(),
            text: e.text,
        }
    }
}

/// One piece of a span's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Piece {
    /// UTF-16 start in the text.
    pub start: u32,
    /// UTF-16 end (equal to `start` for removed words).
    pub end: u32,
    /// `PROPOSED`, `APPROVED`, `STAGED` or `IMPLEMENTED`.
    pub state: String,
    /// Words removed since the approval.
    pub removed: bool,
    /// Its text.
    pub text: String,
}

/// A span and its derived state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Its uid.
    pub uid: String,
    /// `operator` or `claude`.
    pub reviewer: String,
    /// Whether the document still anchors it.
    pub anchored: bool,
    /// UTF-16 start in the text.
    pub start: u32,
    /// UTF-16 end.
    pub end: u32,
    /// Its text now.
    pub text: String,
    /// `PROPOSED`, `APPROVED`, `STAGED` or `IMPLEMENTED`.
    pub state: String,
    /// Approved once and edited since.
    pub demoted: bool,
    /// Its text, piece by piece.
    pub pieces: Vec<Piece>,
    /// Who approved it.
    pub approved_by: Option<String>,
    /// When.
    pub approved_at: Option<String>,
    /// The plan steps it is staged into.
    pub steps: Vec<String>,
}

impl From<SpanView> for Span {
    fn from(v: SpanView) -> Self {
        Self {
            uid: v.uid,
            reviewer: v.reviewer.as_str().to_owned(),
            anchored: v.anchored,
            start: v.start,
            end: v.end,
            text: v.text,
            state: v.state.as_str().to_owned(),
            demoted: v.demoted,
            pieces: v
                .pieces
                .into_iter()
                .map(|p| Piece {
                    start: p.start,
                    end: p.end,
                    state: p.state.as_str().to_owned(),
                    removed: p.removed,
                    text: p.text,
                })
                .collect(),
            approved_by: v.approved_by,
            approved_at: v.approved_at,
            steps: v.steps,
        }
    }
}

/// A design's text at one version, as `design.cat` answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignDoc {
    /// Its uid.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// The plain text.
    pub text: String,
    /// The text with span markers (`⟦<span> STATE⟧ … ⟦/<span>⟧`).
    pub marked: String,
    /// The version token an edit's `base` takes.
    pub version: String,
    /// The version's seq.
    pub seq: i64,
    /// The `mq` topic its updates are announced on.
    pub topic: String,
    /// Its spans, in text order.
    pub spans: Vec<Span>,
}

impl From<DesignText> for DesignDoc {
    fn from(d: DesignText) -> Self {
        Self {
            marked: design::render(&d),
            topic: design::topic(&d.uid),
            version: d.version.token(),
            seq: d.version.seq,
            uid: d.uid,
            title: d.title,
            text: d.text,
            spans: d.spans.into_iter().map(Span::from).collect(),
        }
    }
}

/// What a design write did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Written {
    /// The design.
    pub uid: String,
    /// The update's seq; `None` when nothing changed (an update already merged).
    pub seq: Option<i64>,
    /// The design's version after it.
    pub version: String,
    /// Approved spans whose words this write changed.
    pub demoted: Vec<String>,
    /// The span a `design.span` made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<String>,
}

fn written(uid: String, w: design::Written) -> Written {
    Written {
        uid,
        seq: w.seq,
        version: w.version.token(),
        demoted: w.demoted,
        span: w.span,
    }
}

/// What a `design.edit` does. Quotes are matched in the base version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditAsk {
    /// Replace the quote's text (empty `with`: delete it).
    Replace {
        /// The quote.
        find: String,
        /// Which match, from 1.
        #[serde(default)]
        occurrence: Option<usize>,
        /// The replacement.
        with: String,
    },
    /// Insert `text` after the quote.
    InsertAfter {
        /// The quote.
        find: String,
        /// Which match, from 1.
        #[serde(default)]
        occurrence: Option<usize>,
        /// What to insert.
        text: String,
    },
    /// Replace a span's text; the span then covers the new text.
    Span {
        /// The span.
        span: String,
        /// Its new text.
        with: String,
    },
}

impl From<EditAsk> for Edit {
    fn from(a: EditAsk) -> Self {
        match a {
            EditAsk::Replace {
                find,
                occurrence,
                with,
            } => Self::Replace {
                find,
                occurrence,
                with,
            },
            EditAsk::InsertAfter {
                find,
                occurrence,
                text,
            } => Self::InsertAfter {
                find,
                occurrence,
                text,
            },
            EditAsk::Span { span, with } => Self::Span { span, with },
        }
    }
}

/// `design.list`.
///
/// # Errors
/// A database error.
pub fn list(conn: &Connection, repo: Option<&str>) -> Result<Vec<Design>, ApiError> {
    Ok(design::list(conn, repo)?
        .into_iter()
        .map(Design::from)
        .collect())
}

/// `design.create`.
///
/// # Errors
/// The engine's refusal.
pub fn create(
    conn: &Connection,
    meta: &WriteMeta,
    repo: &str,
    title: &str,
    body: &str,
) -> Result<Design, ApiError> {
    Ok(design::create(conn, meta, repo, title, body)?.into())
}

/// `design.export`: one design rendered for its doc target, or — with no `uid` — every design (of
/// `repo`, when named) that has one.
///
/// # Errors
/// An unknown design, or one whose text or metadata does not read.
pub fn export(
    conn: &Connection,
    uid: Option<&str>,
    repo: Option<&str>,
) -> Result<Vec<Export>, ApiError> {
    let exported = match uid {
        Some(uid) => vec![design::export::export(conn, uid)?],
        None => design::export::exports(conn, repo)?,
    };
    Ok(exported.into_iter().map(Export::from).collect())
}

/// `design.target`: record where a design's export is written.
///
/// # Errors
/// The engine's refusal of the path.
pub fn set_target(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    path: &str,
) -> Result<Design, ApiError> {
    design::export::set_doc_target(conn, meta, uid, path)?;
    Ok(design::row(conn, uid)?.into())
}

/// `design.sources`: record files a design was made from.
///
/// # Errors
/// The engine's refusal of a path or a hash.
pub fn add_sources(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    sources: Vec<Source>,
) -> Result<Design, ApiError> {
    let sources: Vec<design::Source> = sources.into_iter().map(design::Source::from).collect();
    design::export::add_sources(conn, meta, uid, &sources)?;
    Ok(design::row(conn, uid)?.into())
}

/// `design.cat`.
///
/// # Errors
/// An unknown design.
pub fn cat(conn: &Connection, uid: &str) -> Result<DesignDoc, ApiError> {
    Ok(design::read(conn, uid)?.into())
}

/// `design.state`: what a peer at `since` lacks (base64), and the version it brings it to.
///
/// # Errors
/// An unknown design, or `since` not a base64 state vector.
pub fn state(
    conn: &Connection,
    uid: &str,
    since: Option<&str>,
) -> Result<(String, design::Version), ApiError> {
    let since = since.map(|s| bytes("since", s)).transpose()?;
    let (update, version) = design::state(conn, uid, since.as_deref())?;
    Ok((STANDARD.encode(update), version))
}

/// `design.apply`.
///
/// # Errors
/// `update` not base64, or the engine's refusal.
pub fn apply(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    update: &str,
) -> Result<Written, ApiError> {
    let update = bytes("update", update)?;
    Ok(written(
        uid.to_owned(),
        design::apply(conn, meta, uid, &update)?,
    ))
}

/// `design.edit`.
///
/// # Errors
/// The engine's refusal.
pub fn edit(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    base: &str,
    ask: EditAsk,
) -> Result<Written, ApiError> {
    Ok(written(
        uid.to_owned(),
        design::edit(conn, meta, uid, base, &ask.into())?,
    ))
}

/// `design.span`.
///
/// # Errors
/// An unknown reviewer, or the engine's refusal.
pub fn span(conn: &Connection, meta: &WriteMeta, ask: &SpanAsk) -> Result<Written, ApiError> {
    let reviewer = design::Reviewer::parse(ask.reviewer.as_deref().unwrap_or("operator"))?;
    Ok(written(
        ask.uid.clone(),
        design::add_span(
            conn,
            meta,
            &ask.uid,
            &ask.base,
            &ask.find,
            ask.occurrence,
            reviewer,
        )?,
    ))
}

/// A `design.span`'s fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanAsk {
    /// The design.
    pub uid: String,
    /// The version the quote was read at.
    pub base: String,
    /// The quote the span covers.
    pub find: String,
    /// Which match, from 1.
    pub occurrence: Option<usize>,
    /// `operator` (the default) or `claude`.
    pub reviewer: Option<String>,
}

/// `design.approve`, as `principal`: the operator, or a Claude session recorded by its label. The
/// engine refuses an approver the span does not name (D53.5).
///
/// # Errors
/// The engine's refusal.
pub fn approve(
    conn: &Connection,
    meta: &WriteMeta,
    span: &str,
    principal: &Principal,
) -> Result<Span, ApiError> {
    let approver = if principal.is_operator() {
        Approver::Operator
    } else {
        Approver::Claude(principal.label.clone())
    };
    Ok(design::approve(conn, meta, span, &approver)?.into())
}

/// `design.stage`.
///
/// # Errors
/// The engine's refusal.
pub fn stage(
    conn: &Connection,
    meta: &WriteMeta,
    span: &str,
    step: &str,
) -> Result<Span, ApiError> {
    Ok(design::stage(conn, meta, span, step)?.into())
}

/// What prompt a `design.prompt` builds (D53.5–6): *Discuss* a selection (answered as a
/// [`Prompt`]), *Play* a plan or a task (answered as a [`WorkPrompt`]), or a *New prompt* with the
/// operator's own words (answered as a [`prompts::NewPrompt`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptAsk {
    /// *Play* an execution plan: its steps, their tasks, the spans they stage, and the strategy
    /// the work runs under.
    Play {
        /// The plan.
        plan: String,
        /// The workflow strategy the operator chose for the plan's tasks. Pins nothing: the prompt
        /// says which open tasks run it and names any that do not. Omitted, no choice is claimed and
        /// each task keeps its own (the default while unpinned).
        #[serde(default)]
        strategy: Option<String>,
    },
    /// *Play* one task: what it is, where it sits in its design, and the strategy it runs.
    Task {
        /// The task.
        uid: String,
    },
    /// A new session on a design, started with the operator's own words.
    New {
        /// The design.
        uid: String,
        /// What the operator asks; empty when they will say it in the session.
        #[serde(default)]
        text: String,
    },
    /// Discuss a selection of a design's text with Claude.
    Discuss {
        /// The design.
        uid: String,
        /// The version token the selection was made in; the current version when omitted.
        #[serde(default)]
        base: Option<String>,
        /// UTF-16 start of the selection.
        start: u32,
        /// UTF-16 end.
        end: u32,
    },
}

/// A prompt for a Claude session, and what it was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prompt {
    /// `discuss`.
    pub kind: String,
    /// The design.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// The version the selection was resolved in.
    pub version: String,
    /// UTF-16 start.
    pub start: u32,
    /// UTF-16 end.
    pub end: u32,
    /// The selected text, as an edit quotes it.
    pub quote: String,
    /// Which match of the quote the selection is, from 1, when it occurs more than once.
    pub occurrence: Option<usize>,
    /// The spans the selection touches.
    pub spans: Vec<String>,
    /// The prompt itself: what the session is started with.
    pub prompt: String,
}

/// The longest run of backticks in `s`, so a code fence around it can be one longer.
pub(crate) fn longest_backtick_run(s: &str) -> usize {
    s.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

/// How Claude edits a design's text, as every prompt that teaches it says it (*Discuss*, *New
/// prompt*): one spelling and one uniqueness rule, so the prompts cannot drift apart on either.
/// Whole sentences, ending in a full stop.
pub(crate) fn edit_usage(uid: &str) -> String {
    format!(
        "`jkb design edit {uid} --base <token> --find=<quote> --replace=<text>` (or \
         `--insert-after=<quote> --text=<text>`). Quote enough of the surrounding text that the \
         quote occurs once in the version you read."
    )
}

/// The *Discuss* prompt: the design, the version read, the selection as the quote an edit names it
/// by, the spans it touches, and how to read and edit the design through the CLI.
fn discuss_prompt(d: &design::Discussion) -> String {
    use std::fmt::Write as _;
    let version = d.version.token();
    let fence = "`".repeat(longest_backtick_run(&d.quote).max(2) + 1);
    let mut p = String::new();
    let _ = writeln!(
        p,
        "The operator selected a passage of the design \"{}\" ({}) in Code Factory and wants to \
         discuss it with you.",
        d.title, d.uid
    );
    let _ = writeln!(p);
    let _ = writeln!(
        p,
        "The passage (UTF-16 {}..{} of version {version}):",
        d.start, d.end
    );
    let _ = writeln!(p, "{fence}\n{}\n{fence}", d.quote);
    // The block cannot show whitespace or a line break at the passage's edges, and the quote and its
    // occurrence count them: the exact quote is given as a JSON string too, and an edge said aloud.
    let exact = serde_json::to_string(&d.quote).unwrap_or_default();
    let _ = writeln!(p, "Exactly, as a JSON string: {exact}");
    if d.quote.trim() != d.quote {
        let _ = writeln!(
            p,
            "It begins or ends with whitespace or a line break, which the block above does not \
             show: quote it as the JSON string has it. In bash, `$'…'` quoting passes a line break \
             as `\\n` (e.g. `--find=$'foo\\n'`), where a plain `'…'` or `\"…\"` passes the two \
             characters; inside `$'…'` an apostrophe is written `\\'` and a backslash `\\\\`."
        );
    }
    match d.occurrence {
        Some(k) => {
            let _ = writeln!(
                p,
                "In version {version} that text occurs {} times; the selection is occurrence {k}.",
                d.occurrences
            );
        }
        None => {
            let _ = writeln!(p, "In version {version} that text occurs once.");
        }
    }
    if d.spans.is_empty() {
        let _ = writeln!(
            p,
            "It is covered by no span, so it reads as PROPOSED (D53.5)."
        );
    } else {
        let _ = writeln!(p, "It touches these spans (D53.5):");
        for s in &d.spans {
            let _ = writeln!(
                p,
                "- {} {} (its reviewer: {})",
                s.uid,
                s.state.as_str(),
                s.reviewer.as_str()
            );
        }
    }
    let _ = writeln!(p);
    let _ = writeln!(
        p,
        "Read the whole design first: `jkb design cat {}` prints it with span markers and a \
         version token. Discuss the passage with the operator; change nothing until they ask you \
         to.",
        d.uid
    );
    let _ = writeln!(
        p,
        "When they do, edit through the CLI against a version you read — your edit merges with \
         anything written since, and the quote is matched in that version, never the latest: \
         {}",
        edit_usage(&d.uid)
    );
    // The occurrence counts matches in the version the selection was made in, and in no other: it
    // is given only beside that version's token, never as part of a command with another one.
    let pinned = match d.occurrence {
        Some(k) => format!(
            " --occurrence {k}`. That occurrence holds only with `--base {version}`; never pair it \
             with a newer token"
        ),
        None => "`".to_owned(),
    };
    let _ = writeln!(
        p,
        "This passage, in the version it was selected in, is `--base {version} --find=<the \
         passage above>{pinned}. Editing approved text makes it PROPOSED again until it is \
         re-approved."
    );
    p
}

/// What a `design.prompt` answers: a *Discuss* prompt, a *Play* one, or a *New prompt*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptAnswer {
    /// `discuss`.
    Discuss(Prompt),
    /// `play` or `task`.
    Work(plans::WorkPrompt),
    /// `new`.
    New(prompts::NewPrompt),
}

/// `design.prompt`.
///
/// # Errors
/// The engine's refusal of the selection, or an unknown plan, task or strategy.
pub fn prompt(conn: &Connection, ask: &PromptAsk) -> Result<PromptAnswer, ApiError> {
    match ask {
        PromptAsk::Discuss {
            uid,
            base,
            start,
            end,
        } => {
            let d = design::discussion(conn, uid, base.as_deref(), *start, *end)?;
            Ok(PromptAnswer::Discuss(Prompt {
                kind: "discuss".to_owned(),
                prompt: discuss_prompt(&d),
                uid: d.uid,
                title: d.title,
                version: d.version.token(),
                start: d.start,
                end: d.end,
                quote: d.quote,
                occurrence: d.occurrence,
                spans: d.spans.into_iter().map(|s| s.uid).collect(),
            }))
        }
        PromptAsk::Play { plan, strategy } => {
            plans::play_prompt(conn, plan, strategy.as_deref()).map(PromptAnswer::Work)
        }
        PromptAsk::Task { uid } => plans::task_prompt(conn, uid).map(PromptAnswer::Work),
        PromptAsk::New { uid, text } => prompts::new_prompt(conn, uid, text).map(PromptAnswer::New),
    }
}

/// `design.spans`.
///
/// # Errors
/// An unknown design.
pub fn spans(conn: &Connection, uid: &str) -> Result<Vec<Span>, ApiError> {
    Ok(design::spans(conn, uid)?
        .into_iter()
        .map(Span::from)
        .collect())
}

#[cfg(test)]
mod tests;
