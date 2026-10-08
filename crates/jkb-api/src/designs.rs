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
}

impl From<DesignRow> for Design {
    fn from(d: DesignRow) -> Self {
        Self {
            topic: design::topic(&d.uid),
            uid: d.uid,
            title: d.title,
            namespace: d.namespace,
            seq: d.seq,
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
