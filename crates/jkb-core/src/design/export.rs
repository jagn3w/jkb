//! `docs/` generated from designs (design D55.5–6).
//!
//! A design's **export** is its approved text with no span markers, under one header line naming
//! the design, the version it was rendered at, and the blake3 of the text below the header. The
//! header is also how a generated file is recognised: [`parse`] reads it back, and a file without
//! it is hand-written and never checked.
//!
//! The hash is what makes the drift check database-free (D55.6, amended): `jkb design export
//! --check` re-hashes each generated file's body and compares it with the hash its header records,
//! so a hand edit fails the gate on any machine — CI included — without opening a database or
//! reaching a daemon. Whether the design has moved on since is a different question, asked only
//! with the live design at hand (`--check --against-db`). Rendering is pure ([`render`]), so that
//! comparison re-renders in memory exactly what an export writes.
//!
//! A design's doc target (a repo-relative path under `docs/`) and the files it was made from (each
//! with the blake3 of its content then) are rows of their own (V026), so each write undoes alone.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use super::{design_id, invalid, read, DesignText, SpanState, Version};
use crate::changelog::{self, Entity};
use crate::{Error, Result};
use jkb_types::ItemId;

/// The start of a generated file's first line.
pub const GENERATED: &str = "<!-- generated from jkb design ";

/// The directory every doc target lives under, so the drift check (which scans it) sees every
/// generated file.
pub const DOCS_DIR: &str = "docs/";

/// Between the design's uid and the version token in the header.
const VERSION_AT: &str = ", edit there (version ";
/// Between the version token and the body's hash.
const HASH_AT: &str = ", blake3 ";
/// The header's end.
const CLOSE: &str = ") -->";

/// A file a design was made from: its repo-relative path and the blake3 of its content then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Relative to the repository root.
    pub path: String,
    /// Lowercase hex blake3 of the content when it was recorded.
    pub blake3: String,
}

/// What a design records about its export and its sources.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesignMeta {
    /// Where its export is written, relative to the repository root.
    pub doc_target: Option<String>,
    /// The files it was made from, in the order they were first recorded.
    pub sources: Vec<Source>,
}

/// A design rendered for its doc target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exported {
    /// The design.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// The repo it belongs to (the segment after `designs/` in its namespace); its doc target is a
    /// path in that repo's checkout.
    pub repo: Option<String>,
    /// Where it is written; `None` when the design names no target yet.
    pub doc_target: Option<String>,
    /// The version it was rendered at.
    pub version: Version,
    /// The whole file: the header line, then the approved text.
    pub text: String,
}

/// Lowercase hex blake3 of an export's body, as its header records it.
#[must_use]
pub fn body_hash(body: &str) -> String {
    crate::blob::hash_bytes(body.as_bytes())
}

/// The header line (without its newline) of a design exported at `version` with `body` below it.
#[must_use]
pub fn header(uid: &str, version: &Version, body: &str) -> String {
    format!(
        "{GENERATED}{uid}{VERSION_AT}{}{HASH_AT}{}{CLOSE}",
        version.token(),
        body_hash(body)
    )
}

/// What a file's first line says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Generated<'a> {
    /// No generated header: hand-written, and not checked.
    Hand,
    /// A first line that claims to be the generated header but does not read as one — trimmed by
    /// hand, or preceded by a byte-order mark or whitespace. Never silently taken as hand-written:
    /// that would let a generated file out of the check by damaging its first line.
    Malformed(&'static str),
    /// A generated file.
    File {
        /// The design it names.
        uid: &'a str,
        /// The version token it was rendered at (information for the reader only).
        version: &'a str,
        /// The blake3 its header records for the body.
        blake3: &'a str,
        /// Everything after the header line.
        body: &'a str,
    },
}

impl Generated<'_> {
    /// For a generated file, whether its body is still the one its header records — `false` means
    /// it was edited by hand since it was exported.
    #[must_use]
    pub fn intact(&self) -> bool {
        match self {
            Self::File { blake3, body, .. } => body_hash(body) == *blake3,
            Self::Hand | Self::Malformed(_) => false,
        }
    }
}

/// Whether `line`, leading whitespace and a byte-order mark aside, starts as the header does.
fn starts_like_header(line: &str) -> bool {
    line.trim_start_matches(|c: char| c == '\u{feff}' || c.is_whitespace())
        .starts_with(GENERATED.trim_end())
}

/// Read a file's generated header, if it has one.
///
/// Only the first line decides. A first line that starts like the header but is not exactly one
/// (trimmed, indented, a byte-order mark in front) is [`Generated::Malformed`], never hand-written.
/// A header pushed below the first line (something added above it) is not looked for: the file
/// reads as hand-written. Telling that apart from a hand-written doc that quotes the header — in a
/// fenced or indented code block, in any of their nestings — was a Markdown parser's job, and the
/// guarantee is the narrower one.
#[must_use]
pub fn parse(file: &str) -> Generated<'_> {
    let (first, body) = file.split_once('\n').unwrap_or((file, ""));
    let Some(rest) = first.strip_prefix(GENERATED) else {
        return if starts_like_header(first) {
            Generated::Malformed(
                "its first line looks like the generated header but is not exactly one",
            )
        } else {
            Generated::Hand
        };
    };
    let parsed = rest.split_once(VERSION_AT).and_then(|(uid, tail)| {
        let (version, blake3) = tail.strip_suffix(CLOSE)?.split_once(HASH_AT)?;
        let hex = blake3.len() == 64
            && blake3
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let word = |w: &str| !w.is_empty() && !w.contains(char::is_whitespace);
        (word(uid) && word(version) && hex).then_some((uid, version, blake3))
    });
    match parsed {
        Some((uid, version, blake3)) => Generated::File {
            uid,
            version,
            blake3,
            body,
        },
        None => Generated::Malformed(
            "its generated header does not read (it must name the design, the version and the \
             body's blake3)",
        ),
    }
}

/// The design a generated file names in its header; `None` for a file that is not (or not
/// readably) generated.
#[must_use]
pub fn generated_from(file: &str) -> Option<&str> {
    match parse(file) {
        Generated::File { uid, .. } => Some(uid),
        Generated::Hand | Generated::Malformed(_) => None,
    }
}

/// The design's approved text: every span the document still anchors and that is APPROVED (or
/// STAGED, or IMPLEMENTED), whole, in text order. Everything else is PROPOSED and left out —
/// uncovered text, an unapproved span, and **a demoted span as a whole** (D53.5): once any of an
/// approved span's words changed, what is left of it is not text anyone approved. Joining its
/// surviving pieces would publish exactly that — "We must not log tokens." with "not" deleted
/// would export as "We must  log tokens.".
#[must_use]
pub fn approved_text(design: &DesignText) -> String {
    let text = &design.text;
    let mut ranges: Vec<(usize, usize)> = design
        .spans
        .iter()
        .filter(|s| s.anchored && s.state != SpanState::Proposed && s.end > s.start)
        .map(|s| {
            (
                super::crdt::utf16_to_byte(text, s.start),
                super::crdt::utf16_to_byte(text, s.end),
            )
        })
        .collect();
    ranges.sort_unstable();
    let mut out = String::new();
    // Overlapping spans may both hold a word: each byte is written once.
    let mut at = 0;
    for (start, end) in ranges {
        let start = start.max(at);
        if end > start {
            if !out.is_empty() && start > at {
                separate(&mut out, &text[at..start]);
            }
            out.push_str(&text[start..end]);
            at = end;
        }
    }
    out
}

/// What a skipped `gap` of PROPOSED text leaves between two approved spans: nothing when the
/// output already ends a line, else one line break if the gap had any, else a space — never the
/// gap's words. Without it, two spans quoted without their trailing newline ran together
/// (`Decided.## D2`); counting the gap's own line ends instead put a blank line where one table row
/// or list item was skipped, which splits the table.
fn separate(out: &mut String, gap: &str) {
    if out.ends_with('\n') {
        return;
    }
    out.push(if gap.contains('\n') { '\n' } else { ' ' });
}

/// The whole generated file for `design`: the header, then its approved text, ending in a newline
/// (an editor or formatter adds one, and that alone must not read as drift).
#[must_use]
pub fn render(design: &DesignText) -> String {
    let mut body = approved_text(design);
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    let mut out = header(&design.uid, &design.version, &body);
    out.push('\n');
    out.push_str(&body);
    out
}

/// The repo a design belongs to: the segment after `designs/` in its primary namespace.
///
/// # Errors
/// A database error.
pub fn repo_of(conn: &Connection, id: ItemId) -> Result<Option<String>> {
    Ok(crate::item::primary_namespace(conn, id)?.and_then(|ns| {
        ns.strip_prefix(super::ROOT)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|rest| rest.split('/').next())
            .filter(|r| !r.is_empty())
            .map(str::to_owned)
    }))
}

/// A design's export metadata, by its item id.
pub(super) fn meta_of(conn: &Connection, id: ItemId) -> Result<DesignMeta> {
    let doc_target = conn
        .prepare_cached("SELECT path FROM design_doc_targets WHERE design_id = ?1")?
        .query_row([id.get()], |r| r.get(0))
        .optional()?;
    let sources = conn
        .prepare_cached("SELECT path, blake3 FROM design_sources WHERE design_id = ?1 ORDER BY id")?
        .query_map([id.get()], |r| {
            Ok(Source {
                path: r.get(0)?,
                blake3: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(DesignMeta {
        doc_target,
        sources,
    })
}

/// A design's export metadata.
///
/// # Errors
/// An unknown design, or a database error.
pub fn meta(conn: &Connection, uid: &str) -> Result<DesignMeta> {
    meta_of(conn, design_id(conn, uid)?)
}

/// `path` as a stored repo-relative path: not empty, not absolute, and with no `.`, `..` or empty
/// component — so it names one file inside the repository however it is later joined.
fn repo_path(path: &str, what: &str) -> Result<String> {
    let bad = |why: &str| invalid(format!("{what} `{path}` {why}"));
    if path.is_empty() {
        return Err(invalid(format!("{what} is empty")));
    }
    if path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return Err(bad(
            "must be a path relative to the repository root, with `/` separators",
        ));
    }
    if path
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(bad("must not have an empty, `.` or `..` component"));
    }
    Ok(path.to_owned())
}

/// Record where a design's export is written: `path`, relative to the root of the design's repo and
/// under `docs/`, and named by no other design of that repo (two designs rendering one file would
/// each read the other's export as drift). An undo restoring an older target does not come through
/// here, so [`exports`] refuses a repo where it left two designs on one file.
///
/// # Errors
/// An unknown design, a path that is not a `docs/` file, or one another design already targets.
pub fn set_doc_target(
    conn: &Connection,
    meta: &crate::WriteMeta,
    uid: &str,
    path: &str,
) -> Result<()> {
    let path = repo_path(path, "a doc target")?;
    if !path.starts_with(DOCS_DIR) || path.len() == DOCS_DIR.len() {
        return Err(invalid(format!(
            "a doc target is a file under `{DOCS_DIR}` (where `jkb design export --check` looks), \
             not `{path}`"
        )));
    }
    let id = design_id(conn, uid)?;
    let repo = repo_of(conn, id)?.ok_or_else(|| {
        invalid(format!(
            "design {uid} is in no repo (`{}/<repo>`), so its doc target names no checkout",
            super::ROOT
        ))
    })?;
    // One file of a repo is one design's: checked against each holder's repo NOW, since a repo is
    // its namespace and a stored copy would go stale on `jkb ns mv`.
    let holders: Vec<(i64, String)> = conn
        .prepare_cached(
            "SELECT t.design_id, i.uid FROM design_doc_targets t JOIN items i ON i.id = t.design_id
              WHERE t.path = ?1 AND t.design_id <> ?2",
        )?
        .query_map(params![path, id.get()], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (other, other_uid) in holders {
        if repo_of(conn, ItemId::new(other))?.as_deref() == Some(repo.as_str()) {
            return Err(invalid(format!(
                "design {other_uid} already exports to `{path}` in repo {repo} — give this one its \
                 own file"
            )));
        }
    }
    let current: Option<(String, i64)> = conn
        .prepare_cached("SELECT path, txn_id FROM design_doc_targets WHERE design_id = ?1")?
        .query_row([id.get()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    if current.as_ref().is_some_and(|(p, _)| *p == path) {
        return Ok(());
    }
    conn.prepare_cached(
        "INSERT INTO design_doc_targets (design_id, path, txn_id) VALUES (?1, ?2, ?3)
         ON CONFLICT (design_id) DO UPDATE SET path = excluded.path, txn_id = excluded.txn_id",
    )?
    .execute(params![id.get(), path, meta.txn_id])?;
    changelog::upsert(
        conn,
        meta,
        Entity::DesignDocTargets,
        &id.get().to_string(),
        current
            .map(|(p, txn)| json!({ "path": p, "txn_id": txn }))
            .as_ref(),
        Some(&json!({ "design_id": id.get(), "path": path, "txn_id": meta.txn_id })),
    )
}

/// Record files a design was made from (D55.5). A path recorded before is re-hashed in place; the
/// others are kept. Each path is its own row, so undoing one recording takes back only it.
///
/// # Errors
/// An unknown design, a path that is not repo-relative, or a hash that is not blake3 hex.
pub fn add_sources(
    conn: &Connection,
    meta: &crate::WriteMeta,
    uid: &str,
    sources: &[Source],
) -> Result<()> {
    let id = design_id(conn, uid)?;
    for s in sources {
        let path = repo_path(&s.path, "a source path")?;
        let hex = s.blake3.len() == 64
            && s.blake3
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex {
            return Err(invalid(format!(
                "source `{path}`: `{}` is not a lowercase hex blake3 hash",
                s.blake3
            )));
        }
        let before: Option<(i64, String, i64)> = conn
            .prepare_cached(
                "SELECT id, blake3, txn_id FROM design_sources WHERE design_id = ?1 AND path = ?2",
            )?
            .query_row(params![id.get(), path], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        let row = match &before {
            Some((_, hash, _)) if *hash == s.blake3 => continue,
            Some((row, _, _)) => {
                conn.prepare_cached(
                    "UPDATE design_sources SET blake3 = ?2, txn_id = ?3 WHERE id = ?1",
                )?
                .execute(params![row, s.blake3, meta.txn_id])?;
                *row
            }
            None => conn
                .prepare_cached(
                    "INSERT INTO design_sources (design_id, path, blake3, txn_id)
                     VALUES (?1, ?2, ?3, ?4) RETURNING id",
                )?
                .query_row(params![id.get(), path, s.blake3, meta.txn_id], |r| r.get(0))?,
        };
        changelog::upsert(
            conn,
            meta,
            Entity::DesignSources,
            &row.to_string(),
            before
                .map(|(_, hash, txn)| json!({ "blake3": hash, "txn_id": txn }))
                .as_ref(),
            Some(&json!({
                "design_id": id.get(),
                "path": path,
                "blake3": s.blake3,
                "txn_id": meta.txn_id,
            })),
        )?;
    }
    Ok(())
}

/// The design `uid` rendered for its doc target.
///
/// # Errors
/// An unknown design, or one whose stored text or metadata does not read.
pub fn export(conn: &Connection, uid: &str) -> Result<Exported> {
    let design = read(conn, uid)?;
    let id = design_id(conn, uid)?;
    let doc_target = meta_of(conn, id)?.doc_target;
    Ok(Exported {
        text: render(&design),
        repo: repo_of(conn, id)?,
        uid: design.uid,
        title: design.title,
        doc_target,
        version: design.version,
    })
}

/// Every design (of `repo`, when named) that has a doc target, rendered for it, by uid.
///
/// # Errors
/// A design whose stored text or metadata does not read, or two designs of one repo naming one
/// file — which `set_doc_target` refuses, but an undo restoring an older target can leave.
pub fn exports(conn: &Connection, repo: Option<&str>) -> Result<Vec<Exported>> {
    let out = super::list(conn, repo)?
        .into_iter()
        .filter(|d| d.meta.doc_target.is_some())
        .map(|d| export(conn, &d.uid))
        .collect::<std::result::Result<Vec<_>, Error>>()?;
    for (i, a) in out.iter().enumerate() {
        if let Some(b) = out[..i]
            .iter()
            .find(|b| b.repo == a.repo && b.doc_target == a.doc_target)
        {
            return Err(invalid(format!(
                "designs {} and {} of repo {} both export to `{}` — point one elsewhere with \
                 `jkb design export <design> --to docs/<file>`",
                b.uid,
                a.uid,
                a.repo.as_deref().unwrap_or("(none)"),
                a.doc_target.as_deref().unwrap_or_default()
            )));
        }
    }
    Ok(out)
}
