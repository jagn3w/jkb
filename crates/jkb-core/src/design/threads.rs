//! Comment threads on a design's text (operator feedback, 2026-10-11): selected words start a
//! thread, anchored to them as a span is, so it follows them through edits. **The operator starts
//! and resolves threads; Claude replies** — who may call which is the API's RBAC, and this module
//! records who wrote each comment.
//!
//! A thread is an item (`kind = 'design_thread'`) contained by its design, its anchors in the
//! document's `threads` map (never the spans' map, so threads take no part in the spans' rules and
//! may cover the same words); its comments are items (`kind = 'design_comment'`) contained by it, in
//! order. Each write is announced on the design's topic (`kind = "thread"`).

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use jkb_types::ItemId;

use super::{
    append_row, design_id, invalid, item, load, load_base, locate, mint, not_found, set_metadata,
    Crdt, Version, KIND,
};
use crate::store::WriteMeta;
use crate::{containment, Result};

/// A thread's item kind.
pub const THREAD_KIND: &str = "design_thread";
/// A comment's item kind.
pub const COMMENT_KIND: &str = "design_comment";

/// The words a thread (or an approval of a selection) is about, in the version the caller read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Words {
    /// The `occurrence`th (from 1) or only match of a quote.
    Quote {
        /// The quote.
        find: String,
        /// Which match, when it occurs more than once.
        occurrence: Option<usize>,
    },
    /// UTF-16 `[start, end)` — what an editor's selection is.
    Range {
        /// Start.
        start: u32,
        /// End.
        end: u32,
    },
}

impl Words {
    /// The UTF-16 range these words name in `text`.
    pub(super) fn resolve(&self, text: &str) -> Result<(u32, u32)> {
        match self {
            Self::Quote { find, occurrence } => locate(text, find, *occurrence),
            Self::Range { start, end } => {
                let len = super::crdt::utf16_len(text)?;
                if start >= end || *end > len {
                    return Err(invalid(format!(
                        "[{start}, {end}) is not a selection of the version you read ({len} \
                         characters)"
                    )));
                }
                Ok((*start, *end))
            }
        }
    }
}

/// One comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Its uid.
    pub uid: String,
    /// Who wrote it: `operator`, or the Claude principal's label.
    pub by: String,
    /// When.
    pub at: String,
    /// What it says.
    pub text: String,
}

/// A thread as it reads now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    /// Its uid.
    pub uid: String,
    /// The design it is on.
    pub design: String,
    /// Whether its words are still in the text; a thread whose words were deleted is *detached*.
    pub anchored: bool,
    /// UTF-16 start of its words now (0 when detached).
    pub start: u32,
    /// UTF-16 end.
    pub end: u32,
    /// Its words now.
    pub quote: String,
    /// The words it was started on.
    pub started_on: String,
    /// `open` or `resolved`.
    pub status: String,
    /// Its comments, in order; the first is the one that started it.
    pub comments: Vec<Comment>,
}

fn now(conn: &Connection) -> Result<String> {
    Ok(
        conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |r| {
            r.get(0)
        })?,
    )
}

fn clean(text: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(invalid("a comment says something — the text is empty"));
    }
    Ok(text.to_owned())
}

/// An item of `kind` by uid, and its metadata.
fn item_of(conn: &Connection, uid: &str, kind: &str, what: &str) -> Result<(ItemId, Value)> {
    let row: Option<(i64, String, String)> = conn
        .prepare_cached("SELECT id, kind, metadata FROM items WHERE uid = ?1")?
        .query_row([uid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .ok();
    let Some((id, found, metadata)) = row else {
        return Err(not_found(format!("no {what} `{uid}`")));
    };
    if found != kind {
        return Err(invalid(format!("`{uid}` is a {found}, not a {what}")));
    }
    let metadata = serde_json::from_str(&metadata)
        .map_err(|e| invalid(format!("{what} {uid} has unreadable metadata: {e}")))?;
    Ok((ItemId::new(id), metadata))
}

/// The design containing `thread`, by id and uid.
fn design_of(conn: &Connection, thread: ItemId, uid: &str) -> Result<(ItemId, String)> {
    let parent = containment::parent(conn, thread)?
        .ok_or_else(|| invalid(format!("thread {uid} is contained by no design")))?;
    let meta = item::get(conn, parent)?
        .filter(|m| m.kind == KIND)
        .ok_or_else(|| invalid(format!("thread {uid} is not on a design")))?;
    Ok((parent, meta.uid))
}

fn add_comment(
    conn: &Connection,
    meta: &WriteMeta,
    thread: ItemId,
    by: &str,
    text: &str,
) -> Result<Comment> {
    let uid = mint("comment", text)?;
    let at = now(conn)?;
    let id = item::upsert(
        conn,
        meta,
        &item::NewItem {
            uid: uid.clone(),
            kind: COMMENT_KIND.to_owned(),
            content: Some(text.to_owned()),
            content_hash: None,
            mime: None,
        },
    )?;
    set_metadata(conn, meta, id, &json!({ "by": by, "at": at }))?;
    let position = i64::try_from(containment::children(conn, thread)?.len()).unwrap_or(i64::MAX);
    containment::contain(conn, meta, id, thread, position)?;
    Ok(Comment {
        uid,
        by: by.to_owned(),
        at,
        text: text.to_owned(),
    })
}

/// Start a thread on `words` of design `uid`, as read at `base`, with `text` as its first comment
/// by `by`.
///
/// # Errors
/// An unknown design, a token that is not one of its versions, words not in that version or
/// deleted since, an empty comment, or a database error.
pub fn start(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    base: &str,
    words: &Words,
    by: &str,
    text: &str,
) -> Result<Thread> {
    let text = clean(text)?;
    let design = design_id(conn, uid)?;
    let base = Version::parse(base)?;
    let (now_doc, _) = load(conn, design)?;
    let read = load_base(conn, design, &base, &now_doc)?;
    let (s, e) = words.resolve(&read.text())?;
    let ids = read.ids(s, e);
    if !now_doc.all_present(&ids) {
        return Err(invalid(
            "the selected words were deleted after the version you read — re-read and select \
             them again",
        ));
    }
    let quote = super::slice(&read.text(), s, e);
    let thread = mint("thread", &quote)?;
    let ((), update) =
        read.change_threads(|txn, body, threads| Crdt::anchor(txn, body, threads, &thread, s, e))?;
    let update = update.ok_or_else(|| invalid("anchoring the thread wrote nothing"))?;
    now_doc.apply(&update)?;
    let id = item::upsert(
        conn,
        meta,
        &item::NewItem {
            uid: thread.clone(),
            kind: THREAD_KIND.to_owned(),
            content: Some(quote.clone()),
            content_hash: None,
            mime: None,
        },
    )?;
    set_metadata(
        conn,
        meta,
        id,
        &json!({ "design": uid, "status": "open", "started_by": by }),
    )?;
    let position = i64::try_from(containment::children(conn, design)?.len()).unwrap_or(i64::MAX);
    containment::contain(conn, meta, id, design, position)?;
    append_row(conn, meta, design, uid, &update)?;
    add_comment(conn, meta, id, by, &text)?;
    announce(conn, meta, uid, &thread, "started")?;
    show(conn, &thread)
}

/// Reply `text` in `thread`, as `by`. A resolved thread takes no reply until it is reopened by a
/// new thread on its words: replying would hide a conversation behind the filter that folds it.
///
/// # Errors
/// An unknown thread, a resolved one, an empty reply, or a database error.
pub fn reply(
    conn: &Connection,
    meta: &WriteMeta,
    thread: &str,
    by: &str,
    text: &str,
) -> Result<Thread> {
    let text = clean(text)?;
    let (id, metadata) = item_of(conn, thread, THREAD_KIND, "thread")?;
    if metadata.get("status").and_then(Value::as_str) == Some("resolved") {
        return Err(invalid(format!(
            "thread {thread} is resolved — start a new thread on its words to take it up again"
        )));
    }
    let (_, design) = design_of(conn, id, thread)?;
    add_comment(conn, meta, id, by, &text)?;
    announce(conn, meta, &design, thread, "replied")?;
    show(conn, thread)
}

/// Resolve `thread`, as `by`. Resolving one already resolved writes nothing.
///
/// # Errors
/// An unknown thread, or a database error.
pub fn resolve(conn: &Connection, meta: &WriteMeta, thread: &str, by: &str) -> Result<Thread> {
    let (id, mut metadata) = item_of(conn, thread, THREAD_KIND, "thread")?;
    if metadata.get("status").and_then(Value::as_str) == Some("resolved") {
        return show(conn, thread);
    }
    let (_, design) = design_of(conn, id, thread)?;
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert("status".to_owned(), json!("resolved"));
        obj.insert("resolved_by".to_owned(), json!(by));
        obj.insert("resolved_at".to_owned(), json!(now(conn)?));
    }
    set_metadata(conn, meta, id, &metadata)?;
    announce(conn, meta, &design, thread, "resolved")?;
    show(conn, thread)
}

fn announce(
    conn: &Connection,
    meta: &WriteMeta,
    design: &str,
    thread: &str,
    what: &str,
) -> Result<()> {
    super::announce(
        conn,
        meta,
        design,
        "thread",
        json!({ "design": design, "thread": thread, "what": what }),
    )
}

fn comments(conn: &Connection, thread: ItemId) -> Result<Vec<Comment>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.uid, COALESCE(i.content, ''), i.metadata FROM containment c
           JOIN items i ON i.id = c.child_item_id
          WHERE c.parent_item_id = ?1 AND i.kind = ?2 ORDER BY c.position, i.id",
    )?;
    let rows = stmt
        .query_map(params![thread.get(), COMMENT_KIND], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(uid, text, metadata)| {
            let m: Value = serde_json::from_str(&metadata)
                .map_err(|e| invalid(format!("comment {uid} has unreadable metadata: {e}")))?;
            let field = |k: &str| {
                m.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Ok(Comment {
                by: field("by"),
                at: field("at"),
                uid,
                text,
            })
        })
        .collect()
}

fn view(
    conn: &Connection,
    id: ItemId,
    uid: String,
    metadata: &Value,
    design: &str,
    doc: &Crdt,
) -> Result<Thread> {
    let text = doc.text();
    let range = doc.thread_range(&uid).filter(|(s, e)| s < e);
    let (start, end) = range.unwrap_or((0, 0));
    let field = |k: &str| {
        metadata
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok(Thread {
        design: design.to_owned(),
        anchored: range.is_some(),
        start,
        end,
        quote: super::slice(&text, start, end),
        started_on: item::get(conn, id)?
            .and_then(|m| m.content)
            .unwrap_or_default(),
        status: field("status"),
        comments: comments(conn, id)?,
        uid,
    })
}

/// One thread as it reads now.
///
/// # Errors
/// An unknown thread, or a database error.
pub fn show(conn: &Connection, thread: &str) -> Result<Thread> {
    let (id, metadata) = item_of(conn, thread, THREAD_KIND, "thread")?;
    let (design, design_uid) = design_of(conn, id, thread)?;
    let (doc, _) = load(conn, design)?;
    view(conn, id, thread.to_owned(), &metadata, &design_uid, &doc)
}

/// Every thread on design `uid`, in text order (detached ones last).
///
/// # Errors
/// An unknown design, or a database error.
pub fn list(conn: &Connection, uid: &str) -> Result<Vec<Thread>> {
    let design = design_id(conn, uid)?;
    let (doc, _) = load(conn, design)?;
    let mut stmt = conn.prepare_cached(
        "SELECT i.id, i.uid, i.metadata FROM containment c JOIN items i ON i.id = c.child_item_id
          WHERE c.parent_item_id = ?1 AND i.kind = ?2 ORDER BY c.position, i.id",
    )?;
    let rows = stmt
        .query_map(params![design.get(), THREAD_KIND], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for (id, thread, metadata) in rows {
        let metadata: Value = serde_json::from_str(&metadata)
            .map_err(|e| invalid(format!("thread {thread} has unreadable metadata: {e}")))?;
        out.push(view(conn, ItemId::new(id), thread, &metadata, uid, &doc)?);
    }
    out.sort_by_key(|t| (!t.anchored, t.start, t.end));
    Ok(out)
}
