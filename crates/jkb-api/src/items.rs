//! Any item, not only a task (tasks S6.4 stage 5): `jkb item show`/`rm`, `stat`, `related`, and the
//! sync archive's `blob ls`/`cat` and `history`.
//!
//! Reads, except `item.rm`, which is held to the file roots: a client under them may not delete an item
//! filed outside them. (A deleted item leaves no line to judge, so there is no round trip to hold.)

use std::path::Path;

use jkb_core::{binding, blob, edge, item, tag, WriteMeta};
use jkb_types::EdgeType;
use rusqlite::{Connection, OptionalExtension as _};
use serde::{Deserialize, Serialize};

use crate::kb::Budget;
use crate::tasks::{writable_id, FileRoots};
use crate::{ApiError, ErrorCode};

fn not_found(uid: &str) -> ApiError {
    ApiError::with_code(ErrorCode::NotFound, format!("no item with uid `{uid}`"))
}

fn invalid(why: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::Invalid, why)
}

/// One facet value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    /// The facet.
    pub facet: String,
    /// The value.
    pub value: String,
}

/// An item's details, as `item show` and `stat` print them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemInfo {
    /// Its uid.
    pub uid: String,
    /// Its kind.
    pub kind: String,
    /// Its status, for a task.
    pub status: Option<String>,
    /// How it ended.
    pub resolution: Option<String>,
    /// Its priority.
    pub priority: Option<i64>,
    /// Its due date.
    pub due: Option<String>,
    /// Its MIME type.
    pub mime: Option<String>,
    /// Its storage binding.
    pub binding: Option<String>,
    /// Its primary namespace (else its first placement).
    pub namespace: Option<String>,
    /// How many characters its content has.
    pub content_chars: usize,
    /// Its content hash.
    pub content_hash: Option<String>,
    /// When it was made.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
    /// Its tags.
    pub tags: Vec<Tag>,
    /// The first `preview` characters of its content.
    pub preview: String,
    /// The content was longer than the preview.
    pub preview_truncated: bool,
}

/// The longest preview one `item.show` carries, in characters.
pub const MAX_PREVIEW_CHARS: usize = 1_000_000;

fn primary_ns(conn: &Connection, id: jkb_types::ItemId) -> jkb_core::Result<Option<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
             WHERE p.item_id = ?1 ORDER BY (p.role = 'primary') DESC, p.position LIMIT 1",
        )?
        .query_row([id.get()], |r| r.get::<_, String>(0))
        .optional()?)
}

/// Whether an item's content is human-readable text worth showing in full (task notes, prose,
/// markdown) versus a heavy blob (PDF/image) that should stay a bounded preview.
fn is_text_like(kind: &str, mime: Option<&str>) -> bool {
    matches!(kind, "task" | "text" | "note" | "view")
        || mime.is_some_and(|m| m.starts_with("text/") || m.contains("markdown"))
}

/// Default preview (characters) for text-like kinds: generous, but finite — the explorer's details
/// pane shows a **bounded** preview, so a multi-megabyte document cannot spike its memory.
pub const TEXT_PREVIEW_MAX: usize = 100_000;

/// Default preview (characters) for heavy kinds (PDF/image blobs): a short excerpt.
pub const HEAVY_PREVIEW_MAX: usize = 800;

/// `item.show`: an item's details and the first `preview` characters of its content — by default
/// [`TEXT_PREVIEW_MAX`] for a text-like kind and [`HEAVY_PREVIEW_MAX`] otherwise, and at most
/// [`MAX_PREVIEW_CHARS`].
///
/// # Errors
/// [`ErrorCode::NotFound`], or a failed read.
pub fn show(conn: &Connection, uid: &str, preview: Option<usize>) -> Result<ItemInfo, ApiError> {
    let id = item::id_for_uid(conn, uid)?.ok_or_else(|| not_found(uid))?;
    let meta = item::get(conn, id)?.ok_or_else(|| not_found(uid))?;
    let max = preview
        .unwrap_or_else(|| {
            if is_text_like(&meta.kind, meta.mime.as_deref()) {
                TEXT_PREVIEW_MAX
            } else {
                HEAVY_PREVIEW_MAX
            }
        })
        .min(MAX_PREVIEW_CHARS);
    let content = meta.content.as_deref().unwrap_or("");
    let content_chars = content.chars().count();
    Ok(ItemInfo {
        binding: binding::get(conn, id)?.map(|b| b.uri),
        namespace: primary_ns(conn, id)?,
        tags: tag::applications(conn, id)?
            .into_iter()
            .map(|(facet, value)| Tag { facet, value })
            .collect(),
        preview: content.chars().take(max).collect(),
        preview_truncated: content_chars > max,
        content_chars,
        uid: meta.uid,
        kind: meta.kind,
        status: meta.status,
        resolution: meta.resolution,
        priority: meta.priority,
        due: meta.due,
        mime: meta.mime,
        content_hash: meta.content_hash,
        created_at: meta.created_at,
        updated_at: meta.updated_at,
    })
}

/// What `item.rm` removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    /// Its uid.
    pub uid: String,
    /// Its kind.
    pub kind: String,
    /// How many placements went with it.
    pub placements: usize,
    /// How many edges went with it.
    pub edges: usize,
    /// How many tag applications went with it.
    pub tags: usize,
}

/// `item.rm`: delete an item and everything that cascades with it, recorded in full so `undo` restores
/// it; `force` passes `item::remove`'s memory and synced-file guards. Refused under `roots` for an item
/// filed outside them.
///
/// # Errors
/// [`ErrorCode::NotFound`], [`ErrorCode::Forbidden`], a guard's refusal, or a failed write.
pub fn remove(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    force: bool,
    roots: Option<&FileRoots>,
) -> Result<Removed, ApiError> {
    let id = item::id_for_uid(conn, uid)?.ok_or_else(|| not_found(uid))?;
    writable_id(conn, id, uid, roots)?;
    let r = item::remove(conn, meta, id, force)?;
    Ok(Removed {
        uid: r.uid,
        kind: r.kind,
        placements: r.placements,
        edges: r.edges,
        tags: r.tags,
    })
}

/// One item `kb.related` reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedRow {
    /// Its uid.
    pub uid: String,
    /// Its kind.
    pub kind: String,
    /// Its status.
    pub status: Option<String>,
    /// How it ended.
    pub resolution: Option<String>,
    /// Edges from the start.
    pub depth: usize,
    /// The edge type that reached it.
    pub via: String,
    /// `out`, `in` or `both`.
    pub direction: String,
    /// The first line of its content.
    pub snippet: Option<String>,
}

/// How `kb.related` walks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Edges away from the start.
    #[default]
    Out,
    /// Edges into it.
    In,
    /// Both.
    Both,
}

impl From<Direction> for edge::Direction {
    fn from(d: Direction) -> Self {
        match d {
            Direction::Out => Self::Out,
            Direction::In => Self::In,
            Direction::Both => Self::Both,
        }
    }
}

const fn direction_name(d: edge::Direction) -> &'static str {
    match d {
        edge::Direction::Out => "out",
        edge::Direction::In => "in",
        edge::Direction::Both => "both",
    }
}

/// The deepest `kb.related` walks.
pub const MAX_RELATED_DEPTH: usize = 16;

/// The most items one `kb.related` answer reads. The walk has no bound of its own, and a knowledge
/// base whose documents, chunks and tasks are all connected reaches every item at depth 16.
pub const MAX_RELATED_NODES: usize = 1000;

/// How much of an item's body is read for its snippet, in bytes.
const SNIPPET_SOURCE_BYTES: i64 = 4096;

/// `kb.related`: the items reached from `uid` over `edges` (any type when empty), breadth-first up to
/// `depth`, each once at its shortest depth — the walk stopped at [`MAX_RELATED_NODES`], which the
/// second value reports, and the answer within `budget`, each item read without its body past its
/// first [`SNIPPET_SOURCE_BYTES`].
///
/// # Errors
/// [`ErrorCode::NotFound`], an unknown edge type or too deep a walk ([`ErrorCode::Invalid`]), or a
/// failed read.
pub fn related(
    conn: &Connection,
    uid: &str,
    edges: &[String],
    depth: usize,
    direction: Direction,
    budget: &mut Budget,
) -> Result<(Vec<RelatedRow>, bool), ApiError> {
    if depth > MAX_RELATED_DEPTH {
        return Err(invalid(format!(
            "a walk of at most {MAX_RELATED_DEPTH} edges"
        )));
    }
    let types = edges
        .iter()
        .map(|name| {
            EdgeType::from_str_opt(name).ok_or_else(|| {
                invalid(format!(
                    "unknown edge type `{name}`; available: {}",
                    EdgeType::ALL
                        .iter()
                        .map(|e| e.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let start = item::id_for_uid(conn, uid)?.ok_or_else(|| not_found(uid))?;
    let (hops, cut) = edge::walk_limited(
        conn,
        start,
        &types,
        depth,
        direction.into(),
        MAX_RELATED_NODES,
    )?;
    let mut stmt = conn
        .prepare_cached(
            "SELECT uid, kind, status, resolution, substr(content, 1, ?2) FROM items WHERE id = ?1",
        )
        .map_err(jkb_core::Error::from)?;
    let mut out = Vec::new();
    for hop in hops {
        let found = stmt
            .query_row(
                rusqlite::params![hop.item.get(), SNIPPET_SOURCE_BYTES],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(jkb_core::Error::from)?;
        let Some((uid, kind, status, resolution, head)) = found else {
            continue;
        };
        let row = RelatedRow {
            uid,
            kind,
            status,
            resolution,
            depth: hop.depth,
            via: hop.via.as_str().to_owned(),
            direction: direction_name(hop.direction).to_owned(),
            snippet: head
                .as_deref()
                .map(|c| item::snippet(c, item::SNIPPET_CHARS)),
        };
        if !budget.take(&row) {
            return Ok((out, cut));
        }
        out.push(row);
    }
    Ok((out, cut))
}

/// One blob in the archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRow {
    /// Its blake3 hash.
    pub hash: String,
    /// Its size in bytes.
    pub size: i64,
    /// Its MIME type.
    pub mime: Option<String>,
    /// When it was stored.
    pub created_at: String,
}

/// The most blobs one `kb.blobs` lists.
pub const MAX_BLOBS: usize = 10_000;

/// `kb.blobs`: the archive's blobs, newest first, those holding `contains` when given, at most
/// `limit` (≤ [`MAX_BLOBS`]) and within `budget`.
///
/// # Errors
/// [`ErrorCode::Invalid`] for too large a limit or an empty needle, or a failed read.
pub fn blobs(
    conn: &Connection,
    contains: Option<&str>,
    limit: usize,
    budget: &mut Budget,
) -> Result<Vec<BlobRow>, ApiError> {
    if limit > MAX_BLOBS {
        return Err(invalid(format!("a limit of at most {MAX_BLOBS}")));
    }
    if contains.is_some_and(str::is_empty) {
        return Err(invalid(
            "an empty needle matches every blob; give it some text",
        ));
    }
    let mut out = Vec::new();
    for b in blob::list(conn, contains.map(str::as_bytes), limit)? {
        let row = BlobRow {
            hash: b.hash,
            size: b.size,
            mime: b.mime,
            created_at: b.created_at,
        };
        if !budget.take(&row) {
            break;
        }
        out.push(row);
    }
    Ok(out)
}

/// The shortest hash prefix `kb.blob` takes.
pub const MIN_BLOB_PREFIX: usize = 4;

/// The largest blob `kb.blob` answers, in bytes: one answer, held whole in the daemon.
pub const MAX_BLOB_TEXT_BYTES: i64 = 8 * 1024 * 1024;

/// `kb.blob`: the text of the one blob whose hash starts with `prefix`.
///
/// **Text only, and at most [`MAX_BLOB_TEXT_BYTES`].** The answer is JSON: a blob that is not UTF-8
/// (a PDF an ingest archived) cannot be carried as text, and a larger one would be held whole in the
/// daemon. `jkb blob cat` on the host reads either in-process ([`blob_bytes`]).
///
/// # Errors
/// [`ErrorCode::NotFound`] for no match, [`ErrorCode::Invalid`] for a short, malformed or ambiguous
/// prefix, a binary blob or a large one, or a failed read.
pub fn blob_text(conn: &Connection, prefix: &str) -> Result<(String, String), ApiError> {
    let hash = blob_hash(conn, prefix)?;
    let size: i64 = conn
        .prepare_cached("SELECT size FROM blobs WHERE hash = ?1")
        .map_err(jkb_core::Error::from)?
        .query_row([&hash], |r| r.get(0))
        .map_err(jkb_core::Error::from)?;
    if size > MAX_BLOB_TEXT_BYTES {
        return Err(invalid(format!(
            "blob `{hash}` is {size} bytes, more than one answer carries \
             ({MAX_BLOB_TEXT_BYTES}); `jkb blob cat` it on the host"
        )));
    }
    let (hash, bytes) = blob_bytes(conn, &hash)?;
    let text = String::from_utf8(bytes).map_err(|_| {
        invalid(format!(
            "blob `{hash}` is not text; `jkb blob cat` it on the host"
        ))
    })?;
    Ok((hash, text))
}

/// The bytes of the one blob whose hash starts with `prefix`, whatever they are — for a process holding
/// the database, which writes them straight out.
///
/// # Errors
/// As [`blob_text`], less the text and size refusals.
pub fn blob_bytes(conn: &Connection, prefix: &str) -> Result<(String, Vec<u8>), ApiError> {
    let hash = blob_hash(conn, prefix)?;
    let bytes = blob::load(conn, &hash)?.ok_or_else(|| {
        ApiError::with_code(ErrorCode::NotFound, format!("blob `{hash}` is gone"))
    })?;
    Ok((hash, bytes))
}

/// The full hash of the one blob whose hash starts with `prefix`.
fn blob_hash(conn: &Connection, prefix: &str) -> Result<String, ApiError> {
    if prefix.len() < MIN_BLOB_PREFIX
        || prefix.len() > 64
        || !prefix.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid(format!(
            "a blob hash prefix of {MIN_BLOB_PREFIX} to 64 hex digits"
        )));
    }
    let lower = prefix.to_ascii_lowercase();
    let pattern = format!("{lower}%");
    let matches: Vec<String> = conn
        .prepare_cached("SELECT hash FROM blobs WHERE hash LIKE ?1 LIMIT 2")
        .map_err(jkb_core::Error::from)?
        .query_map([&pattern], |r| r.get::<_, String>(0))
        .map_err(jkb_core::Error::from)?
        .collect::<rusqlite::Result<_>>()
        .map_err(jkb_core::Error::from)?;
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(ApiError::with_code(
            ErrorCode::NotFound,
            format!("no blob with hash prefix `{prefix}`"),
        )),
        _ => Err(invalid(format!(
            "`{prefix}` matches more than one blob; use a longer prefix"
        ))),
    }
}

/// One synced version of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// When it was settled.
    pub ts: String,
    /// The blob holding its bytes.
    pub blob: String,
    /// The journal's status then.
    pub status: String,
}

/// `kb.history`: every synced version of the file at `path`, newest first, once per blob.
///
/// `path` is absolute in the CLIENT's filesystem, and `client_home` its `$HOME`: a path under that home
/// is re-rooted at `server_home`, the way [`crate::kb::ambient`] finds a mount, so the dev container's
/// `~/repos/…` finds the journal rows the host wrote for the same file.
///
/// # Errors
/// [`ErrorCode::Invalid`] for a relative path, or a failed read.
pub fn history(
    conn: &Connection,
    path: &str,
    client_home: &str,
    server_home: Option<&Path>,
    budget: &mut Budget,
) -> Result<(String, Vec<Version>), ApiError> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(invalid("an absolute file path"));
    }
    let uri = jkb_sync::file_uri(&crate::kb::rerooted(path, client_home, server_home));
    // The journal's changelog carries one entry per settle, each naming the blob holding that
    // version's bytes.
    let mut stmt = conn
        .prepare_cached(
            "SELECT ts, after FROM changelog
             WHERE entity_type = 'sync_state' AND entity_id = ?1 AND after IS NOT NULL
             ORDER BY id DESC",
        )
        .map_err(jkb_core::Error::from)?;
    let rows = stmt
        .query_map([&uri], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(jkb_core::Error::from)?;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let (ts, after) = row.map_err(jkb_core::Error::from)?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&after) else {
            continue;
        };
        let Some(hash) = v.get("base_blob_hash").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !seen.insert(hash.to_owned()) {
            continue;
        }
        let version = Version {
            ts,
            blob: hash.to_owned(),
            status: v
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("ok")
                .to_owned(),
        };
        if !budget.take(&version) {
            break;
        }
        out.push(version);
    }
    Ok((uri, out))
}

#[cfg(test)]
mod tests;
