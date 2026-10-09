//! `docs/` generated from designs (design D55.5–6).
//!
//! A design's **export** is its text with every PROPOSED range left out and no span markers, under
//! one header line naming the design and the version it was rendered at. The header is also how a
//! generated file is recognised: [`generated_from`] reads it back, and a file without it is
//! hand-written and never checked. Rendering is pure ([`render`]) so the drift check re-renders
//! in memory exactly what an export writes — there is one definition of the file, not two.
//!
//! A design's metadata records where its export goes (`doc_target`, a repo-relative path under
//! `docs/`) and the files it was made from (`sources`, each with the blake3 of its content then).

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::{crdt, design_id, invalid, read, set_metadata, DesignText, SpanState, Version};
use crate::{Error, Result};

/// The start of a generated file's first line. Everything after the design's uid is information
/// for the reader; only the uid is read back.
pub const GENERATED: &str = "<!-- generated from jkb design ";

/// The directory every doc target lives under, so the drift check (which scans it) sees every
/// generated file.
pub const DOCS_DIR: &str = "docs/";

/// A file a design was made from: its repo-relative path and the blake3 of its content then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Relative to the repository root.
    pub path: String,
    /// Lowercase hex blake3 of the content when it was recorded.
    pub blake3: String,
}

/// What a design's metadata says about its export and its sources.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesignMeta {
    /// Where its export is written, relative to the repository root.
    pub doc_target: Option<String>,
    /// The files it was made from.
    pub sources: Vec<Source>,
}

/// A design rendered for its doc target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exported {
    /// The design.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// Where it is written; `None` when the design names no target yet.
    pub doc_target: Option<String>,
    /// The version it was rendered at.
    pub version: Version,
    /// The whole file: the header line, then the approved text.
    pub text: String,
}

/// The header line (without its newline) of a design exported at `version`.
#[must_use]
pub fn header(uid: &str, version: &Version) -> String {
    format!(
        "{GENERATED}{uid}, edit there (version {}) -->",
        version.token()
    )
}

/// The design a generated file names in its header; `None` for a file that is not generated.
#[must_use]
pub fn generated_from(file: &str) -> Option<&str> {
    let first = file.lines().next()?;
    let (uid, _) = first.strip_prefix(GENERATED)?.split_once(',')?;
    (!uid.is_empty()).then_some(uid)
}

/// The design's text with every PROPOSED range left out: only words some anchored span holds as
/// approved (or staged, or implemented) are kept, in text order. Words removed since an approval
/// are gone from the text and so from the export.
#[must_use]
pub fn approved_text(design: &DesignText) -> String {
    let text = &design.text;
    let mut ranges: Vec<(usize, usize)> = design
        .spans
        .iter()
        .filter(|s| s.anchored)
        .flat_map(|s| &s.pieces)
        .filter(|p| p.state != SpanState::Proposed && !p.removed && p.end > p.start)
        .map(|p| {
            (
                crdt::utf16_to_byte(text, p.start),
                crdt::utf16_to_byte(text, p.end),
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
            out.push_str(&text[start..end]);
            at = end;
        }
    }
    out
}

/// The whole generated file for `design`: the header, then its approved text, ending in a newline
/// (an editor or formatter adds one, and that alone must not read as drift).
#[must_use]
pub fn render(design: &DesignText) -> String {
    let body = approved_text(design);
    let mut out = header(&design.uid, &design.version);
    out.push('\n');
    out.push_str(&body);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn item_metadata(conn: &Connection, uid: &str) -> Result<(jkb_types::ItemId, Value)> {
    let id = design_id(conn, uid)?;
    let raw: String = conn
        .prepare_cached("SELECT metadata FROM items WHERE id = ?1")?
        .query_row([id.get()], |r| r.get(0))?;
    let value = serde_json::from_str(&raw)
        .map_err(|e| invalid(format!("design {uid} has unreadable metadata: {e}")))?;
    Ok((id, value))
}

/// Read a design's export metadata out of its item's `metadata` JSON.
pub(super) fn meta_of(uid: &str, metadata: &Value) -> Result<DesignMeta> {
    let doc_target = metadata
        .get("doc_target")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let sources = match metadata.get("sources") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) => list
            .iter()
            .map(|s| {
                let field = |k: &str| s.get(k).and_then(Value::as_str).map(str::to_owned);
                Ok(Source {
                    path: field("path").ok_or_else(|| {
                        invalid(format!("design {uid} has a source with no path"))
                    })?,
                    blake3: field("blake3").ok_or_else(|| {
                        invalid(format!("design {uid} has a source with no blake3"))
                    })?,
                })
            })
            .collect::<Result<_>>()?,
        Some(_) => return Err(invalid(format!("design {uid}'s sources are not a list"))),
    };
    Ok(DesignMeta {
        doc_target,
        sources,
    })
}

/// A design's export metadata.
///
/// # Errors
/// An unknown design, or metadata that does not read.
pub fn meta(conn: &Connection, uid: &str) -> Result<DesignMeta> {
    let (_, value) = item_metadata(conn, uid)?;
    meta_of(uid, &value)
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

fn set_key(
    conn: &Connection,
    meta: &crate::WriteMeta,
    uid: &str,
    key: &str,
    v: Value,
) -> Result<()> {
    let (id, mut value) = item_metadata(conn, uid)?;
    match value.as_object_mut() {
        Some(obj) => {
            obj.insert(key.to_owned(), v);
        }
        None => value = json!({ key: v }),
    }
    set_metadata(conn, meta, id, &value)
}

/// Record where a design's export is written: `path`, relative to the repository root and under
/// `docs/`, and named by no other design (two designs rendering one file would each read the
/// other's export as drift).
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
    let current = self::meta(conn, uid)?.doc_target;
    let taken: Option<String> = conn
        .prepare_cached(
            "SELECT uid FROM items
              WHERE kind = ?1 AND uid <> ?2 AND json_extract(metadata, '$.doc_target') = ?3",
        )?
        .query_row(params![super::KIND, uid, path], |r| r.get(0))
        .optional()?;
    if let Some(other) = taken {
        return Err(invalid(format!(
            "design {other} already exports to `{path}` — give this one its own file"
        )));
    }
    if current.as_deref() == Some(path.as_str()) {
        return Ok(());
    }
    set_key(conn, meta, uid, "doc_target", Value::String(path))
}

/// Record files a design was made from (D55.5). A path recorded before is replaced, so recording
/// a source again re-hashes it; the others are kept.
///
/// # Errors
/// An unknown design, a path that is not repo-relative, or a hash that is not blake3 hex.
pub fn add_sources(
    conn: &Connection,
    meta: &crate::WriteMeta,
    uid: &str,
    sources: &[Source],
) -> Result<()> {
    let mut now = self::meta(conn, uid)?.sources;
    let before = now.clone();
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
        match now.iter_mut().find(|n| n.path == path) {
            Some(n) => n.blake3.clone_from(&s.blake3),
            None => now.push(Source {
                path,
                blake3: s.blake3.clone(),
            }),
        }
    }
    if now == before {
        return Ok(());
    }
    let list = now
        .iter()
        .map(|s| json!({ "path": s.path, "blake3": s.blake3 }))
        .collect();
    set_key(conn, meta, uid, "sources", Value::Array(list))
}

/// The design `uid` rendered for its doc target.
///
/// # Errors
/// An unknown design, or one whose stored text or metadata does not read.
pub fn export(conn: &Connection, uid: &str) -> Result<Exported> {
    let design = read(conn, uid)?;
    let doc_target = meta(conn, uid)?.doc_target;
    Ok(Exported {
        text: render(&design),
        uid: design.uid,
        title: design.title,
        doc_target,
        version: design.version,
    })
}

/// Every design (of `repo`, when named) that has a doc target, rendered for it, by uid.
///
/// # Errors
/// A design whose stored text or metadata does not read.
pub fn exports(conn: &Connection, repo: Option<&str>) -> Result<Vec<Exported>> {
    super::list(conn, repo)?
        .into_iter()
        .filter(|d| d.meta.doc_target.is_some())
        .map(|d| export(conn, &d.uid))
        .collect::<std::result::Result<_, Error>>()
}
