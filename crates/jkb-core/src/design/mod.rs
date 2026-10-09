//! Design documents (design D53.4–5, `docs/code-factory.md`).
//!
//! A design is an item (`kind = 'design'`) homed under `designs/<repo>/`. Its body is a Yjs
//! document whose text is one `Y.Text` (`body`), stored as the append-only `design_updates` log
//! (plus a `design_snapshots` compaction) — the table is the source of truth, and every rendering
//! of the text is derived from it here, in a throwaway [`crdt::Crdt`].
//!
//! **Every update is a changelogged write** (`Entity::DesignUpdates`), and `jkb undo` of one is a
//! *new* update that reverts it ([`revert_update`]), never a deleted row: a peer that merged an
//! update keeps it, so deleting history would only make the table disagree with every editor.
//!
//! **An edit is resolved against the version its author read.** [`read`] answers a version token
//! with the text; [`edit`] finds its quote in *that* version, builds the change against that
//! version's items, and merges it over everything written since — the way two editors of one
//! paragraph merge. It is refused only where no merge exists: the quoted text was deleted after the
//! base, or the base predates the last compaction. A quote that is missing or ambiguous *in the
//! base* is refused, never guessed at.
//!
//! **Span state is one recorded fact and two derived ones** (D53.5). APPROVED is recorded on the
//! span's item, as the Yjs ids of the words approved, in the version the reviewer read
//! ([`crdt::Crdt::attest`]); whether those are still the span's words is derived by diffing the
//! text against them. STAGED (a `stages` edge to a plan step) and IMPLEMENTED (every staged step has
//! tasks, all `done`) are read from the graph, never stored.

pub mod crdt;
mod discuss;
pub mod export;
pub mod plan;
pub mod prompts;

use std::collections::BTreeSet;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use jkb_types::{EdgeType, Error as TypeError, ItemId, PlacementRole};

use crate::changelog::{self, Entity, Op};
use crate::mq::{self, Draft, QueueError, TopicSpec};
use crate::store::WriteMeta;
use crate::{containment, edge, item, ns, placement, Error, Result};
use crdt::{Crdt, Piece, PieceKind, Pieces};
pub use discuss::{discussion, Discussion, Touched, MAX_DISCUSS_UNITS};
pub use export::{DesignMeta, Exported, Source};
pub use plan::{PlanTask, PlanView, Plans, StepView, TaskPlace, PLAN_KIND};
pub use prompts::{Launch, PromptRecord, RecordPrompt, Recorded, PROMPT_KIND};
use yrs::Text as _;

/// The item kind of a design.
pub const KIND: &str = "design";
/// The item kind of a span: the item a design's span is mirrored to, so edges can point at it.
pub const SPAN_KIND: &str = "design_span";
/// The item kind of an execution plan's step — what a span is staged into (D53.6).
pub const STEP_KIND: &str = "plan_step";
/// The namespace root designs live under: `designs/<repo>/`.
pub const ROOT: &str = "designs";
/// The longest slug prefix carried into a minted design uid.
const UID_SLUG_MAX: usize = 32;
/// The largest update a live-update message carries inline. A larger one is announced by its seq
/// alone, and a subscriber fetches it with `design.state` — the queue caps a payload at 64 KiB.
const INLINE_UPDATE_MAX: usize = 32 * 1024;
/// How long a live-update message is kept: a subscriber that was away longer re-reads the state.
const TOPIC_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// How many update rows a design holds before a write folds the older ones into its snapshot. Every
/// write rebuilds the document from its rows, so without a bound one editor sending an update per
/// keystroke made each write slower than the last, on the one writer thread every write waits on.
const COMPACT_AT: i64 = if cfg!(test) { 24 } else { 4096 };
/// How many of the newest rows an automatic compaction keeps: a version read that recently can still
/// be edited against, and an update that recent can still be undone.
const COMPACT_KEEP: i64 = if cfg!(test) { 8 } else { 1024 };

fn invalid(why: impl Into<String>) -> Error {
    TypeError::Validation(why.into()).into()
}

fn not_found(why: impl Into<String>) -> Error {
    TypeError::NotFound(why.into()).into()
}

/// Who a span names as the one to approve it (D53.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reviewer {
    /// The human.
    Operator,
    /// A Claude session.
    Claude,
}

impl Reviewer {
    /// The stored and printed name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Claude => "claude",
        }
    }

    /// The reviewer a name means.
    ///
    /// # Errors
    /// A name that is neither `operator` nor `claude`.
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "operator" => Ok(Self::Operator),
            "claude" => Ok(Self::Claude),
            other => Err(invalid(format!(
                "a span's reviewer is `operator` or `claude`, not `{other}`"
            ))),
        }
    }
}

/// Who is approving: the operator, or a Claude principal (its label is recorded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approver {
    /// The human.
    Operator,
    /// A Claude session, by its principal label.
    Claude(String),
}

impl Approver {
    fn label(&self) -> &str {
        match self {
            Self::Operator => "operator",
            Self::Claude(label) => label,
        }
    }
}

/// The state of a piece of design text (D53.5): `PROPOSED → APPROVED → STAGED → IMPLEMENTED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SpanState {
    /// Not (or no longer) the words someone approved. New text, covered by no span, reads so too.
    Proposed,
    /// Approved by the reviewer the span names — the one state that is recorded.
    Approved,
    /// Approved, and staged into at least one plan step (a `stages` edge). Derived.
    Staged,
    /// Staged, and every staged step has at least one task, all `done`. Derived.
    Implemented,
}

impl SpanState {
    /// The printed name: `PROPOSED`, `APPROVED`, `STAGED`, `IMPLEMENTED`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "PROPOSED",
            Self::Approved => "APPROVED",
            Self::Staged => "STAGED",
            Self::Implemented => "IMPLEMENTED",
        }
    }
}

/// A version of a design: the updates through `seq`, and the Yjs state vector they make.
///
/// Its [`Version::token`] is what `jkb design cat` prints and what `jkb design edit --base` takes.
/// The state vector rides along so a token cannot be replayed against another design, and so an
/// editor can ask for exactly what it lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// The newest update the version includes; 0 for a design with none.
    pub seq: i64,
    /// The v1-encoded state vector at that seq.
    pub state_vector: Vec<u8>,
}

impl Version {
    /// `<seq>.<state vector, base64url>`.
    #[must_use]
    pub fn token(&self) -> String {
        format!(
            "{}.{}",
            self.seq,
            URL_SAFE_NO_PAD.encode(&self.state_vector)
        )
    }

    /// The version a token names.
    ///
    /// # Errors
    /// A token not of the shape [`Version::token`] writes.
    pub fn parse(token: &str) -> Result<Self> {
        let bad = || {
            invalid(format!(
                "`{token}` is not a design version token — use the one `jkb design cat` printed"
            ))
        };
        let (seq, sv) = token.split_once('.').ok_or_else(bad)?;
        let seq: i64 = seq.parse().map_err(|_| bad())?;
        if seq < 0 {
            return Err(bad());
        }
        let state_vector = URL_SAFE_NO_PAD.decode(sv).map_err(|_| bad())?;
        Ok(Self { seq, state_vector })
    }
}

/// A design, as the listing shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignRow {
    /// Its uid.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// Where it lives (`designs/<repo>`).
    pub namespace: Option<String>,
    /// Its newest update.
    pub seq: i64,
    /// Where its export is written, and the files it was made from (D55.5–6).
    pub meta: DesignMeta,
}

/// One piece of a span's text and the state it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanPiece {
    /// UTF-16 start in the current text.
    pub start: u32,
    /// UTF-16 end; equal to `start` for words removed since the approval.
    pub end: u32,
    /// The piece's state.
    pub state: SpanState,
    /// Words removed since the approval (zero width now): what the demotion took out.
    pub removed: bool,
    /// Words written inside the span since the approval: what the demotion put in. With `removed`,
    /// the provenance a demoted span keeps — its `state` is PROPOSED throughout (D53.5), so this is
    /// how a reader still tells the new words from the ones that were approved.
    pub added: bool,
    /// The piece's text.
    pub text: String,
}

/// A span with its derived state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanView {
    /// The span's uid (its item's).
    pub uid: String,
    /// Who approves it.
    pub reviewer: Reviewer,
    /// Whether the document still anchors it. A peer can delete the anchors; the item stays.
    pub anchored: bool,
    /// UTF-16 start in the current text.
    pub start: u32,
    /// UTF-16 end.
    pub end: u32,
    /// The span's text now.
    pub text: String,
    /// The span's state: its approved words' state, or PROPOSED when any of its words changed.
    pub state: SpanState,
    /// Approved once, and edited since: some of its words are no longer the approved ones.
    pub demoted: bool,
    /// Its text, piece by piece.
    pub pieces: Vec<SpanPiece>,
    /// Who approved it, if anyone.
    pub approved_by: Option<String>,
    /// When.
    pub approved_at: Option<String>,
    /// The plan steps it is staged into.
    pub steps: Vec<String>,
}

/// A design's text and spans at one version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignText {
    /// The design's uid.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// The plain text.
    pub text: String,
    /// The version it was read at.
    pub version: Version,
    /// Its spans, in text order.
    pub spans: Vec<SpanView>,
}

/// What a write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The update's seq; `None` when the write changed nothing (an update already merged).
    pub seq: Option<i64>,
    /// The design's version after it.
    pub version: Version,
    /// Approved spans whose words this write changed — no longer wholly approved.
    pub demoted: Vec<String>,
    /// The span a span write created.
    pub span: Option<String>,
}

/// What an edit does, by quote (D53.4). Quotes are matched in the base version, never the latest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Replace the quote's text with `with` (empty: delete it).
    Replace {
        /// The quote.
        find: String,
        /// Which match, from 1, when it occurs more than once.
        occurrence: Option<usize>,
        /// Its replacement.
        with: String,
    },
    /// Insert `text` right after the quote.
    InsertAfter {
        /// The quote.
        find: String,
        /// Which match, from 1.
        occurrence: Option<usize>,
        /// What to insert.
        text: String,
    },
    /// Replace a span's whole text with `with`; the span then covers the new text.
    Span {
        /// The span's uid.
        span: String,
        /// Its new text (not empty: a span covers something).
        with: String,
    },
}

/// What a compaction did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compacted {
    /// The snapshot now covers every update through this seq.
    pub through: i64,
    /// How many update rows it replaced.
    pub removed: usize,
}

/// The live-update topic for a design: `design/<uid>`, with the uid's `:` (not a topic character)
/// spelled `.`.
#[must_use]
pub fn topic(uid: &str) -> String {
    let safe: String = uid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '.'
            }
        })
        .collect();
    format!("design/{safe}")
}

/// A design's id by uid.
///
/// # Errors
/// [`TypeError::NotFound`] when no item has the uid, a validation error when it is not a design.
pub fn design_id(conn: &Connection, uid: &str) -> Result<ItemId> {
    let row: Option<(i64, String)> = conn
        .prepare_cached("SELECT id, kind FROM items WHERE uid = ?1")?
        .query_row([uid], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    match row {
        None => Err(not_found(format!("no design `{uid}`"))),
        Some((id, kind)) if kind == KIND => Ok(ItemId::new(id)),
        Some((_, kind)) => Err(invalid(format!("`{uid}` is a {kind}, not a design"))),
    }
}

fn mint(prefix: &str, title: &str) -> Result<String> {
    let slug: String = crate::dsl::slug(title).chars().take(UID_SLUG_MAX).collect();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut salt = [0u8; 3];
    getrandom::getrandom(&mut salt).map_err(|e| invalid(format!("no randomness: {e}")))?;
    let salt = u32::from_be_bytes([0, salt[0], salt[1], salt[2]]);
    Ok(if slug.is_empty() {
        format!("{prefix}:{nanos:x}{salt:06x}")
    } else {
        format!("{prefix}:{slug}-{nanos:x}{salt:06x}")
    })
}

/// Create a design titled `title` under `designs/<repo>`, its body `body`.
///
/// # Errors
/// An empty title or a repo that is not one namespace segment, or a database error.
pub fn create(
    conn: &Connection,
    meta: &WriteMeta,
    repo: &str,
    title: &str,
    body: &str,
) -> Result<DesignRow> {
    let title = title.trim();
    if title.is_empty() {
        return Err(invalid("a design needs a title"));
    }
    if repo.is_empty() || repo.contains('/') {
        return Err(invalid(format!(
            "`{repo}` is not a repo name: designs live under `{ROOT}/<repo>`"
        )));
    }
    let path = ns::normalize(&format!("{ROOT}/{repo}"))?;
    let uid = mint(KIND, title)?;
    let id = item::upsert(
        conn,
        meta,
        &item::NewItem {
            uid: uid.clone(),
            kind: KIND.to_owned(),
            content: Some(title.to_owned()),
            content_hash: None,
            mime: None,
        },
    )?;
    let home = ns::ensure(conn, &path)?;
    placement::place(conn, meta, id, home, PlacementRole::Primary, 0)?;
    ensure_topic(conn, meta, &uid)?;
    let mut seq = 0;
    if !body.is_empty() {
        let doc = Crdt::new();
        let ((), update) = doc.change(|txn, text, _| {
            text.insert(txn, 0, body);
            Ok(())
        })?;
        if let Some(update) = update {
            seq = append_row(conn, meta, id, &uid, &update)?;
        }
    }
    Ok(DesignRow {
        uid,
        title: title.to_owned(),
        namespace: Some(path),
        seq,
        meta: DesignMeta::default(),
    })
}

/// Every design, or those of one repo, by uid.
///
/// # Errors
/// A database error.
pub fn list(conn: &Connection, repo: Option<&str>) -> Result<Vec<DesignRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.id, i.uid, COALESCE(i.content, '') FROM items i WHERE i.kind = ?1 ORDER BY i.uid",
    )?;
    let rows = stmt
        .query_map([KIND], |r| {
            Ok((
                ItemId::new(r.get(0)?),
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let wanted = repo.map(|r| format!("{ROOT}/{r}"));
    let mut out = Vec::new();
    for (id, uid, title) in rows {
        let namespace = item::primary_namespace(conn, id)?;
        if let Some(w) = &wanted {
            let under = namespace
                .as_deref()
                .is_some_and(|n| n == w || n.starts_with(&format!("{w}/")));
            if !under {
                continue;
            }
        }
        out.push(DesignRow {
            seq: newest_seq(conn, id)?,
            meta: export::meta_of(conn, id)?,
            uid,
            title,
            namespace,
        });
    }
    Ok(out)
}

/// One design, as the listing shows it.
///
/// # Errors
/// An unknown design, or metadata that does not read.
pub fn row(conn: &Connection, uid: &str) -> Result<DesignRow> {
    let id = design_id(conn, uid)?;
    Ok(DesignRow {
        uid: uid.to_owned(),
        title: item::get(conn, id)?
            .and_then(|m| m.content)
            .unwrap_or_default(),
        namespace: item::primary_namespace(conn, id)?,
        seq: newest_seq(conn, id)?,
        meta: export::meta_of(conn, id)?,
    })
}

/// The newest seq a design holds, counting its compaction.
fn newest_seq(conn: &Connection, design: ItemId) -> Result<i64> {
    let updates: Option<i64> = conn
        .prepare_cached("SELECT MAX(seq) FROM design_updates WHERE design_id = ?1")?
        .query_row([design.get()], |r| r.get(0))?;
    let snapshot = snapshot_row(conn, design)?.map_or(0, |(seq, _)| seq);
    Ok(updates.unwrap_or(0).max(snapshot))
}

fn snapshot_row(conn: &Connection, design: ItemId) -> Result<Option<(i64, Vec<u8>)>> {
    Ok(conn
        .prepare_cached("SELECT seq, state_v1 FROM design_snapshots WHERE design_id = ?1")?
        .query_row([design.get()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// Merge into `doc` the design's snapshot and its updates through `through` (all of them for
/// `None`), and answer the seq it reached.
fn load_into(conn: &Connection, design: ItemId, through: Option<i64>, doc: &Crdt) -> Result<i64> {
    let mut seq = 0;
    if let Some((snap_seq, state)) = snapshot_row(conn, design)? {
        if through.is_some_and(|t| t < snap_seq) {
            return Err(invalid(format!(
                "that version (seq {}) predates the design's last compaction (seq {snap_seq}), so \
                 the text it read can no longer be rebuilt — re-read with `jkb design cat` and \
                 retry against the new version",
                through.unwrap_or_default()
            )));
        }
        doc.apply(&state)?;
        seq = snap_seq;
    }
    let mut stmt = conn.prepare_cached(
        "SELECT seq, update_v1 FROM design_updates
          WHERE design_id = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq",
    )?;
    let mut rows = stmt.query(params![design.get(), seq, through.unwrap_or(i64::MAX)])?;
    while let Some(row) = rows.next()? {
        let bytes: Vec<u8> = row.get(1)?;
        doc.apply(&bytes)?;
        seq = row.get(0)?;
    }
    Ok(seq)
}

/// The design's document now, and its version.
fn load(conn: &Connection, design: ItemId) -> Result<(Crdt, Version)> {
    let doc = Crdt::new();
    let seq = load_into(conn, design, None, &doc)?;
    let version = Version {
        seq,
        state_vector: doc.state_vector(),
    };
    Ok((doc, version))
}

/// The document at `base`, written into as a client the current document has never seen.
fn load_base(conn: &Connection, design: ItemId, base: &Version, now: &Crdt) -> Result<Crdt> {
    let doc = Crdt::writer_for(&now.state_vector())?;
    let reached = load_into(conn, design, Some(base.seq), &doc)?;
    let same = |a: &[u8], b: &[u8]| -> Result<bool> {
        use yrs::updates::decoder::Decode as _;
        let read = |v: &[u8]| {
            yrs::StateVector::decode_v1(v).map_err(|_| invalid("unreadable state vector"))
        };
        Ok(read(a)? == read(b)?)
    };
    if reached != base.seq || !same(&doc.state_vector(), &base.state_vector)? {
        return Err(invalid(format!(
            "version `{}` is not a version of this design — use the token `jkb design cat` printed \
             for it",
            base.token()
        )));
    }
    Ok(doc)
}

/// Insert one update row and announce it on the design's topic. The one writer of
/// `design_updates`: [`append_row`] logs it, [`revert_update`] does not (the `undo` marker is its
/// record).
fn store_row(
    conn: &Connection,
    meta: &WriteMeta,
    design: ItemId,
    uid: &str,
    update: &[u8],
) -> Result<(i64, i64)> {
    let seq = newest_seq(conn, design)? + 1;
    let rowid: i64 = conn
        .prepare_cached(
            "INSERT INTO design_updates (design_id, seq, update_v1, actor, txn_id)
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
        )?
        .query_row(
            params![design.get(), seq, update, meta.actor, meta.txn_id],
            |r| r.get(0),
        )?;
    publish(conn, meta, uid, seq, update)?;
    Ok((rowid, seq))
}

/// Store `update` as the design's next row, logged so `jkb undo` reverts it.
fn append_row(
    conn: &Connection,
    meta: &WriteMeta,
    design: ItemId,
    uid: &str,
    update: &[u8],
) -> Result<i64> {
    let (rowid, seq) = store_row(conn, meta, design, uid, update)?;
    changelog::upsert(
        conn,
        meta,
        Entity::DesignUpdates,
        &rowid.to_string(),
        None,
        Some(&json!({ "design": uid, "seq": seq, "bytes": update.len() })),
    )?;
    compact_if_due(conn, meta, design)?;
    Ok(seq)
}

/// Make sure the design's live-update topic exists, and answer its name. Every design has one from
/// its creation, so a subscriber can join it before the first update (`mq.group_create` refuses a
/// topic that does not exist); [`publish`] calls it too, for a design made before that was so.
fn ensure_topic(conn: &Connection, meta: &WriteMeta, uid: &str) -> Result<String> {
    let name = topic(uid);
    let spec = TopicSpec {
        default_ttl_ms: Some(TOPIC_TTL_MS),
        ..TopicSpec::default()
    };
    match mq::topic_create(conn, meta, &name, &spec, mq::now_ms()) {
        Ok(_) | Err(Error::Queue(QueueError::TopicConflict(_))) => Ok(name),
        Err(e) => Err(e),
    }
}

/// Announce an update on `design/<uid>`: subscribers merge it (updates are idempotent and
/// commutative, so at-least-once is enough). Best effort by design — the table is the truth, and a
/// full or oversized queue costs a subscriber a re-read, never the write.
fn publish(conn: &Connection, meta: &WriteMeta, uid: &str, seq: i64, update: &[u8]) -> Result<()> {
    let inline = (update.len() <= INLINE_UPDATE_MAX).then(|| STANDARD.encode(update));
    announce(
        conn,
        meta,
        uid,
        "update",
        json!({ "design": uid, "seq": seq, "update": inline }),
    )
}

/// Put a `kind` message on `design/<uid>` — an `update`, or a `prompt` recorded (D53.6) — best
/// effort, as [`publish`] says: a full or oversized queue costs a subscriber a re-read, never the
/// write.
fn announce(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    kind: &str,
    payload: Value,
) -> Result<()> {
    let name = ensure_topic(conn, meta, uid)?;
    let now = mq::now_ms();
    let sent = mq::send(
        conn,
        meta,
        &name,
        &Draft {
            key: uid.to_owned(),
            kind: kind.to_owned(),
            payload,
            ttl_ms: None,
            producer: "design".to_owned(),
        },
        now,
    );
    match sent {
        Ok(_) | Err(Error::Queue(QueueError::QueueFull { .. } | QueueError::TooLarge { .. })) => {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// The design's text and spans now.
///
/// # Errors
/// An unknown design, or a stored update that does not decode.
pub fn read(conn: &Connection, uid: &str) -> Result<DesignText> {
    let id = design_id(conn, uid)?;
    let title = item::get(conn, id)?
        .and_then(|m| m.content)
        .unwrap_or_default();
    let (doc, version) = load(conn, id)?;
    let spans = span_views(conn, id, &doc)?;
    Ok(DesignText {
        uid: uid.to_owned(),
        title,
        text: doc.text(),
        version,
        spans,
    })
}

/// The text with each anchored span marked: `⟦<span> STATE⟧ … ⟦/<span>⟧`. Uncovered text is
/// PROPOSED and carries no marker.
#[must_use]
pub fn render(design: &DesignText) -> String {
    let text = &design.text;
    // (byte, closes-first, marker): a span closing where another opens closes first. A span with no
    // width is one marker pair, so its close cannot sort before its own open.
    let mut marks: Vec<(usize, u8, String)> = Vec::new();
    for s in design.spans.iter().filter(|s| s.anchored) {
        let open = format!("⟦{} {}⟧", s.uid, s.state.as_str());
        let close = format!("⟦/{}⟧", s.uid);
        let start = crdt::utf16_to_byte(text, s.start);
        if s.end <= s.start {
            marks.push((start, 1, format!("{open}{close}")));
            continue;
        }
        marks.push((start, 1, open));
        marks.push((crdt::utf16_to_byte(text, s.end), 0, close));
    }
    marks.sort_by_key(|a| (a.0, a.1));
    let mut out = String::with_capacity(text.len() + marks.len() * 24);
    let mut at = 0;
    for (byte, _, mark) in marks {
        out.push_str(&text[at..byte]);
        out.push_str(&mark);
        at = byte;
    }
    out.push_str(&text[at..]);
    out
}

/// Everything a peer at state vector `since` lacks (the whole document for `None`), and the
/// version it brings that peer to.
///
/// # Errors
/// An unknown design or an unreadable state vector.
pub fn state(conn: &Connection, uid: &str, since: Option<&[u8]>) -> Result<(Vec<u8>, Version)> {
    let id = design_id(conn, uid)?;
    let (doc, version) = load(conn, id)?;
    let update = match since {
        Some(sv) => doc.encode_since(sv)?,
        None => doc.encode_state(),
    };
    Ok((update, version))
}

/// Merge an editor's update (`design.apply`): validated against the document, stored, announced.
///
/// What is stored and announced is the change the update made here ([`Crdt::merge`]), never the
/// bytes as sent: a peer's update may carry what the design already has, and `jkb undo` reverts the
/// stored row. An update may also rewrite span anchors (the `spans` map is the document's); one that
/// would make two spans overlap is refused, as [`add_span`] refuses one, and one that moves an
/// approved span's anchors off its words demotes it ([`Crdt::pieces`]).
///
/// # Errors
/// Bytes that are not a Yjs v1 update, one that builds on changes this design does not have, or one
/// that makes two spans overlap.
pub fn apply(conn: &Connection, meta: &WriteMeta, uid: &str, update: &[u8]) -> Result<Written> {
    let id = design_id(conn, uid)?;
    let (doc, version) = load(conn, id)?;
    let before = before_write(conn, id, &doc)?;
    let delta = doc.merge(update)?;
    if doc.has_missing() {
        return Err(invalid(
            "the update builds on changes this design does not have — send the updates it \
             depends on first (or re-sync with `design.state`)",
        ));
    }
    let Some(delta) = delta else {
        return Ok(Written {
            seq: None,
            version,
            demoted: Vec::new(),
            span: None,
        });
    };
    let seq = append_row(conn, meta, id, uid, &delta)?;
    finish(conn, id, &doc, &before, Some(seq), None)
}

/// What a write is checked against afterwards ([`finish`]): the span pairs that already overlapped,
/// and the spans whose approval held.
struct Before {
    overlaps: BTreeSet<(String, String)>,
    held: Vec<String>,
}

/// The pairs of a design's spans whose ranges overlap in `doc`, each pair in uid order. A span with
/// no width overlaps nothing.
fn overlaps(doc: &Crdt, items: &[SpanItem]) -> BTreeSet<(String, String)> {
    let ranges: Vec<(&str, u32, u32)> = items
        .iter()
        .filter_map(|s| {
            doc.range(&s.uid)
                .filter(|(a, b)| a < b)
                .map(|(a, b)| (s.uid.as_str(), a, b))
        })
        .collect();
    let mut out = BTreeSet::new();
    for (i, (u, s, e)) in ranges.iter().enumerate() {
        for (v, os, oe) in &ranges[i + 1..] {
            if s < oe && os < e {
                let (a, b) = if u < v { (u, v) } else { (v, u) };
                out.insert(((*a).to_owned(), (*b).to_owned()));
            }
        }
    }
    out
}

/// Refuse a document in which two spans overlap that did not in `before`. Every writer of a row
/// that can move an anchor asks it: [`finish`] for the edits and merges, [`revert_update`] for an
/// undo — which once skipped it, so undoing an anchor-shrinking update laid a span back over a
/// neighbour added since.
fn refuse_new_overlaps(
    doc: &Crdt,
    items: &[SpanItem],
    before: &BTreeSet<(String, String)>,
) -> Result<()> {
    match overlaps(doc, items).difference(before).next() {
        Some((a, b)) => Err(invalid(format!(
            "span {a} would overlap span {b}: each piece of a design is in exactly one state, so \
             spans do not overlap"
        ))),
        None => Ok(()),
    }
}

/// What [`finish`] compares a write against, read from the document before it. Only spans with an
/// approval are diffed — the costly part — so a design with none approved pays for none.
fn before_write(conn: &Connection, design: ItemId, doc: &Crdt) -> Result<Before> {
    let items = span_items(conn, design)?;
    let mut held = Vec::new();
    if items.iter().any(SpanItem::has_approval) {
        let text = doc.text();
        for span in items.iter().filter(|s| s.has_approval()) {
            if view(conn, span, doc, &text)?.state != SpanState::Proposed {
                held.push(span.uid.clone());
            }
        }
    }
    Ok(Before {
        overlaps: overlaps(doc, &items),
        held,
    })
}

/// The answer to a write that merged `doc`: its version, and the spans it demoted.
///
/// **Spans do not overlap**, however the write reached the document: a pair overlapping now that
/// did not before refuses the write (the transaction rolls back, row and announcement with it),
/// through [`refuse_new_overlaps`] — which [`revert_update`] asks too, for `jkb undo`.
fn finish(
    conn: &Connection,
    id: ItemId,
    doc: &Crdt,
    before: &Before,
    seq: Option<i64>,
    span: Option<String>,
) -> Result<Written> {
    let items = span_items(conn, id)?;
    refuse_new_overlaps(doc, &items, &before.overlaps)?;
    let text = doc.text();
    let mut demoted = Vec::new();
    for span in items.iter().filter(|s| before.held.contains(&s.uid)) {
        if view(conn, span, doc, &text)?.state == SpanState::Proposed {
            demoted.push(span.uid.clone());
        }
    }
    Ok(Written {
        seq,
        version: Version {
            seq: newest_seq(conn, id)?,
            state_vector: doc.state_vector(),
        },
        demoted,
        span,
    })
}

/// Where the `n`th (from 1) or only occurrence of `quote` is in `text`, as UTF-16 `[start, end)`.
fn locate(text: &str, quote: &str, occurrence: Option<usize>) -> Result<(u32, u32)> {
    if quote.is_empty() {
        return Err(invalid(
            "the quote is empty — quote the text exactly as `jkb design cat` printed it",
        ));
    }
    let hits: Vec<usize> = text.match_indices(quote).map(|(at, _)| at).collect();
    let at = match (occurrence, hits.len()) {
        (_, 0) => {
            return Err(invalid(format!(
                "the quote {quote:?} is not in the version you read — quote the text exactly as \
                 `jkb design cat` printed it, without its span markers"
            )))
        }
        (None, 1) => hits[0],
        (None, n) => {
            return Err(invalid(format!(
                "the quote {quote:?} occurs {n} times in the version you read — quote more of \
                 it, or pass --occurrence 1..{n}"
            )))
        }
        (Some(k), n) if k == 0 || k > n => {
            return Err(invalid(format!(
                "--occurrence {k}: the quote {quote:?} occurs {n} time(s) in the version you read"
            )))
        }
        (Some(k), _) => hits[k - 1],
    };
    Ok((
        crdt::byte_to_utf16(text, at)?,
        crdt::byte_to_utf16(text, at + quote.len())?,
    ))
}

/// Make an edit against the version `base` (a token from `jkb design cat`) and merge it.
///
/// # Errors
/// A malformed or foreign token, a base older than the last compaction, a quote missing or
/// ambiguous in the base, text it targets that was deleted after the base, or a database error.
pub fn edit(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    base: &str,
    edit: &Edit,
) -> Result<Written> {
    let id = design_id(conn, uid)?;
    let base = Version::parse(base)?;
    let (now, _) = load(conn, id)?;
    let before = before_write(conn, id, &now)?;
    let doc = load_base(conn, id, &base, &now)?;
    let text = doc.text();
    let (targeted, update) = match edit {
        Edit::Replace {
            find,
            occurrence,
            with,
        } => {
            let (s, e) = locate(&text, find, *occurrence)?;
            let ids = doc.ids(s, e);
            let ((), update) = doc.change(|txn, body, _| {
                body.remove_range(txn, s, e - s);
                if !with.is_empty() {
                    body.insert(txn, s, with);
                }
                Ok(())
            })?;
            (ids, update)
        }
        Edit::InsertAfter {
            find,
            occurrence,
            text: inserted,
        } => {
            if inserted.is_empty() {
                return Err(invalid("nothing to insert"));
            }
            let (s, e) = locate(&text, find, *occurrence)?;
            let ids = doc.ids(s, e);
            let ((), update) = doc.change(|txn, body, _| {
                body.insert(txn, e, inserted);
                Ok(())
            })?;
            (ids, update)
        }
        Edit::Span { span, with } => {
            if with.is_empty() {
                return Err(invalid(
                    "a span covers at least one character — to delete its text, use --find with \
                     an empty --replace",
                ));
            }
            let (s, e) = doc
                .range(span)
                .ok_or_else(|| invalid(format!("span {span} is not in the version you read")))?;
            let ids = doc.ids(s, e);
            let len = crdt::utf16_len(with)?;
            let ((), update) = doc.change(|txn, body, spans| {
                body.remove_range(txn, s, e - s);
                body.insert(txn, s, with);
                Crdt::anchor(txn, body, spans, span, s, s + len)
            })?;
            (ids, update)
        }
    };
    if !now.all_present(&targeted) {
        return Err(invalid(
            "the text this edit targets was deleted after the version you read, so there is \
             nothing for it to merge with — re-read with `jkb design cat` and retry",
        ));
    }
    let Some(update) = update else {
        return Ok(Written {
            seq: None,
            version: Version {
                seq: newest_seq(conn, id)?,
                state_vector: now.state_vector(),
            },
            demoted: Vec::new(),
            span: None,
        });
    };
    now.apply(&update)?;
    let seq = append_row(conn, meta, id, uid, &update)?;
    finish(conn, id, &now, &before, Some(seq), None)
}

/// Make a span over the quote (matched in `base`), to be approved by `reviewer`.
///
/// # Errors
/// As [`edit`]'s quote resolution, or a span overlapping another once merged.
#[allow(clippy::too_many_arguments)] // the op's own fields, one each
pub fn add_span(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    base: &str,
    find: &str,
    occurrence: Option<usize>,
    reviewer: Reviewer,
) -> Result<Written> {
    let id = design_id(conn, uid)?;
    let base = Version::parse(base)?;
    let (now, _) = load(conn, id)?;
    let before = before_write(conn, id, &now)?;
    let doc = load_base(conn, id, &base, &now)?;
    let (s, e) = locate(&doc.text(), find, occurrence)?;
    let ids = doc.ids(s, e);
    if !now.all_present(&ids) {
        return Err(invalid(
            "the quoted text was deleted after the version you read — re-read with `jkb design \
             cat` and retry",
        ));
    }
    let span = mint("span", "")?;
    let ((), update) =
        doc.change(|txn, body, spans| Crdt::anchor(txn, body, spans, &span, s, e))?;
    let update = update.ok_or_else(|| invalid("the span wrote nothing"))?;
    now.apply(&update)?;
    let span_id = item::upsert(
        conn,
        meta,
        &item::NewItem {
            uid: span.clone(),
            kind: SPAN_KIND.to_owned(),
            content: None,
            content_hash: None,
            mime: None,
        },
    )?;
    set_metadata(
        conn,
        meta,
        span_id,
        &json!({ "design": uid, "reviewer": reviewer.as_str() }),
    )?;
    let position = i64::try_from(containment::children(conn, id)?.len()).unwrap_or(i64::MAX);
    containment::contain(conn, meta, span_id, id, position)?;
    let seq = append_row(conn, meta, id, uid, &update)?;
    finish(conn, id, &now, &before, Some(seq), Some(span))
}

/// Replace an item's `metadata`, logged so `jkb undo` puts the old one back.
fn set_metadata(conn: &Connection, meta: &WriteMeta, id: ItemId, value: &Value) -> Result<()> {
    let old: String = conn
        .prepare_cached("SELECT metadata FROM items WHERE id = ?1")?
        .query_row([id.get()], |r| r.get(0))?;
    let new = value.to_string();
    conn.prepare_cached(
        "UPDATE items SET metadata = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
          WHERE id = ?1",
    )?
    .execute(params![id.get(), new])?;
    changelog::append(
        conn,
        meta,
        Op::Update,
        Entity::Items,
        &id.get().to_string(),
        Some(&json!({ "metadata": old })),
        Some(&json!({ "metadata": new })),
    )?;
    Ok(())
}

/// A span item's id, metadata and design.
struct SpanItem {
    id: ItemId,
    uid: String,
    metadata: Value,
    /// The design that contains it.
    design: ItemId,
}

impl SpanItem {
    /// The approval the item records, if any.
    fn approval(&self) -> Option<&Value> {
        self.metadata.get("approval").filter(|a| !a.is_null())
    }

    fn has_approval(&self) -> bool {
        self.approval().is_some()
    }

    /// The words its approval attests to ([`Crdt::attest`]); `None` with no approval, or one that
    /// records no readable attestation — which reads as demoted, never as approved.
    fn attested(&self) -> Option<yrs::Snapshot> {
        let b64 = self.approval()?.get("attested")?.as_str()?;
        let bytes = STANDARD.decode(b64).ok()?;
        crdt::decode_snapshot(&bytes).ok()
    }
}

/// Whether a span's approval holds now: it has one, none of its words changed or moved, and it is
/// anchored over at least one character. **The one predicate** behind a span's derived state
/// ([`view`]) and the gate on staging it ([`stage`]) — two hand-copied versions once disagreed, and
/// `stage` took an unanchored span whose state read PROPOSED.
fn approval_holds(approved: bool, demoted: bool, range: Option<(u32, u32)>) -> bool {
    approved && !demoted && range.is_some_and(|(s, e)| s < e)
}

fn span_item(conn: &Connection, uid: &str) -> Result<(SpanItem, ItemId, String)> {
    let row: Option<(i64, String, String)> = conn
        .prepare_cached("SELECT id, kind, metadata FROM items WHERE uid = ?1")?
        .query_row([uid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((id, kind, metadata)) = row else {
        return Err(not_found(format!("no span `{uid}`")));
    };
    if kind != SPAN_KIND {
        return Err(invalid(format!("`{uid}` is a {kind}, not a design span")));
    }
    let metadata: Value = serde_json::from_str(&metadata)
        .map_err(|e| invalid(format!("span {uid} has unreadable metadata: {e}")))?;
    let design_uid = metadata
        .get("design")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("span {uid} names no design")))?
        .to_owned();
    let design = design_id(conn, &design_uid)?;
    Ok((
        SpanItem {
            id: ItemId::new(id),
            uid: uid.to_owned(),
            metadata,
            design,
        },
        design,
        design_uid,
    ))
}

/// Every span item a design contains.
fn span_items(conn: &Connection, design: ItemId) -> Result<Vec<SpanItem>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.id, i.uid, i.metadata FROM containment c JOIN items i ON i.id = c.child_item_id
          WHERE c.parent_item_id = ?1 AND i.kind = ?2 ORDER BY c.position, i.id",
    )?;
    let rows = stmt
        .query_map(params![design.get(), SPAN_KIND], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(id, uid, metadata)| {
            let metadata = serde_json::from_str(&metadata)
                .map_err(|e| invalid(format!("span {uid} has unreadable metadata: {e}")))?;
            Ok(SpanItem {
                id: ItemId::new(id),
                uid,
                metadata,
                design,
            })
        })
        .collect()
}

/// The state a span's approved words are in: APPROVED, or STAGED / IMPLEMENTED from the graph.
///
/// Only `stages` edges into a step of the span's **own** design count ([`plan::design_of_step`]),
/// the same rule [`stage`] refuses by: an edge written another way (`jkb inv link <span> stages
/// <step>`) into another design's plan does not make the span STAGED. IMPLEMENTED needs every
/// counted step to be implemented ([`plan::StepTasks::implemented`]).
fn approved_state(
    conn: &Connection,
    span: ItemId,
    design: ItemId,
) -> Result<(SpanState, Vec<String>)> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.id, i.uid FROM edges e JOIN items i ON i.id = e.dst_item_id
          WHERE e.src_item_id = ?1 AND e.type = ?2 AND i.kind = ?3 ORDER BY i.uid",
    )?;
    let edges = stmt
        .query_map(
            params![span.get(), EdgeType::Stages.as_str(), STEP_KIND],
            |r| Ok((ItemId::new(r.get::<_, i64>(0)?), r.get::<_, String>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut steps = Vec::new();
    for (step, uid) in edges {
        if plan::design_of_step(conn, step, &uid).ok() == Some(design) {
            steps.push((step, uid));
        }
    }
    if steps.is_empty() {
        return Ok((SpanState::Approved, Vec::new()));
    }
    let mut implemented = true;
    for (step, _) in &steps {
        implemented &= plan::step_tasks(conn, *step)?.implemented();
    }
    let state = if implemented {
        SpanState::Implemented
    } else {
        SpanState::Staged
    };
    Ok((state, steps.into_iter().map(|(_, uid)| uid).collect()))
}

/// Each span's view against `doc`, in text order (unanchored spans last).
fn span_views(conn: &Connection, design: ItemId, doc: &Crdt) -> Result<Vec<SpanView>> {
    let text = doc.text();
    let mut views = span_items(conn, design)?
        .into_iter()
        .map(|span| view(conn, &span, doc, &text))
        .collect::<Result<Vec<_>>>()?;
    views.sort_by_key(|v| (!v.anchored, v.start, v.end));
    Ok(views)
}

fn slice(text: &str, start: u32, end: u32) -> String {
    text[crdt::utf16_to_byte(text, start)..crdt::utf16_to_byte(text, end)].to_owned()
}

/// One span's derived state.
fn view(conn: &Connection, span: &SpanItem, doc: &Crdt, text: &str) -> Result<SpanView> {
    let reviewer = Reviewer::parse(
        span.metadata
            .get("reviewer")
            .and_then(Value::as_str)
            .unwrap_or("operator"),
    )?;
    let approval = span.approval();
    let field = |key: &str| {
        approval
            .and_then(|a| a.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let range = doc.range(&span.uid);
    let (start, end) = range.unwrap_or((0, 0));
    let (words, steps) = approved_state(conn, span.id, span.design)?;
    let whole = |state| {
        vec![SpanPiece {
            start,
            end,
            state,
            removed: false,
            added: false,
            text: slice(text, start, end),
        }]
    };
    let (pieces, displaced) = match (approval, range) {
        (_, None) => (Vec::new(), false),
        (None, Some(_)) => (whole(SpanState::Proposed), false),
        (Some(_), Some(_)) => match span.attested() {
            // An approval with no record of which words it attests to cannot vouch for any.
            None => (whole(SpanState::Proposed), true),
            Some(attested) => {
                let Pieces { pieces, displaced } =
                    doc.pieces(&span.uid, &attested)?.unwrap_or(Pieces {
                        pieces: Vec::new(),
                        displaced: false,
                    });
                let pieces = pieces
                    .into_iter()
                    .map(
                        |Piece {
                             start,
                             end,
                             kind,
                             text,
                         }| SpanPiece {
                            start,
                            end,
                            state: if kind == PieceKind::Approved {
                                words
                            } else {
                                SpanState::Proposed
                            },
                            removed: kind == PieceKind::Removed,
                            added: kind == PieceKind::Added,
                            text,
                        },
                    )
                    .collect();
                (pieces, displaced)
            }
        },
    };
    let demoted = approval.is_some()
        && (displaced
            || pieces
                .iter()
                .any(|p: &SpanPiece| p.state == SpanState::Proposed));
    // ONE RULE FOR A DEMOTED SPAN: the whole span reads PROPOSED, every piece of it too. The editor
    // draws from the pieces (`stateRuns`), and with per-word states a demoted span's untouched
    // words drew APPROVED while `render` and `stage` said PROPOSED. Which words were removed or
    // added since the approval is still told apart (`removed`, `added`).
    let pieces = if demoted {
        pieces
            .into_iter()
            .map(|p| SpanPiece {
                state: SpanState::Proposed,
                ..p
            })
            .collect()
    } else {
        pieces
    };
    let state = if approval_holds(approval.is_some(), demoted, range) {
        words
    } else {
        SpanState::Proposed
    };
    Ok(SpanView {
        uid: span.uid.clone(),
        reviewer,
        anchored: range.is_some(),
        start,
        end,
        text: slice(text, start, end),
        state,
        demoted,
        pieces,
        approved_by: field("by"),
        approved_at: field("at"),
        steps,
    })
}

/// A design's spans with their derived states.
///
/// # Errors
/// An unknown design, or a database error.
pub fn spans(conn: &Connection, uid: &str) -> Result<Vec<SpanView>> {
    let id = design_id(conn, uid)?;
    let (doc, _) = load(conn, id)?;
    span_views(conn, id, &doc)
}

/// One span's view now.
fn span_now(conn: &Connection, span: &SpanItem, design: ItemId) -> Result<SpanView> {
    let (doc, _) = load(conn, design)?;
    view(conn, span, &doc, &doc.text())
}

/// Approve a span **as the reviewer read it**: `base` is the version token the reviewer read the
/// span at (D53.4 — read-version semantics, never read-latest). The approval records the Yjs ids of
/// the span's words in that version ([`Crdt::attest`]), and is refused when those words are not the
/// span's words now — changed, removed, or added to since — so no one approves words they did not
/// read. Only the reviewer the span names approves it: a span naming the operator is the operator's
/// alone; one naming Claude is a Claude session's, or the operator's, who holds every permission.
///
/// A span whose approval already holds is answered as it is, writing nothing.
///
/// # Errors
/// An unknown span, an approver it does not name, a malformed or foreign token, a span unanchored
/// or covering no words (now or in `base`), one changed since `base`, or a database error.
pub fn approve(
    conn: &Connection,
    meta: &WriteMeta,
    span_uid: &str,
    base: &str,
    approver: &Approver,
) -> Result<SpanView> {
    let (mut span, design, design_uid) = span_item(conn, span_uid)?;
    let reviewer = Reviewer::parse(
        span.metadata
            .get("reviewer")
            .and_then(Value::as_str)
            .unwrap_or("operator"),
    )?;
    if reviewer == Reviewer::Operator && *approver != Approver::Operator {
        return Err(invalid(format!(
            "span {span_uid} names the operator as its reviewer, so only the operator approves it"
        )));
    }
    let base = Version::parse(base)?;
    let (doc, _) = load(conn, design)?;
    let text = doc.text();
    let current = view(conn, &span, &doc, &text)?;
    if !current.anchored {
        return Err(invalid(format!(
            "span {span_uid} is no longer anchored in its design — there are no words to approve"
        )));
    }
    if current.state != SpanState::Proposed {
        return Ok(current);
    }
    let read = load_base(conn, design, &base, &doc)?;
    let (s, e) = read.range(span_uid).ok_or_else(|| {
        invalid(format!(
            "span {span_uid} is not anchored in the version you read — re-read with `jkb design \
             cat` and approve the version you read"
        ))
    })?;
    if s >= e {
        return Err(invalid(format!(
            "span {span_uid} covers no words in the version you read — an approval attests to \
             words, so there is nothing to approve"
        )));
    }
    let attested = STANDARD.encode(crdt::encode_snapshot(&read.attest(s, e)?));
    let at: String = conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |r| {
        r.get(0)
    })?;
    if let Some(obj) = span.metadata.as_object_mut() {
        obj.insert(
            "approval".to_owned(),
            json!({
                "attested": attested,
                "version": base.token(),
                "by": approver.label(),
                "at": at,
            }),
        );
    }
    let signed = view(conn, &span, &doc, &text)?;
    if signed.state == SpanState::Proposed {
        return Err(invalid(format!(
            "span {span_uid} changed since the version you read, so its words now are not the \
             words you read — re-read with `jkb design cat` and approve the version you read"
        )));
    }
    set_metadata(conn, meta, span.id, &span.metadata)?;
    announce_span(conn, meta, &design_uid, &signed)?;
    Ok(signed)
}

/// Stage an approved span into a plan step: a `stages` edge (D53.6). STAGED is derived from it.
///
/// # Errors
/// An unknown span or step, a span whose derived state is PROPOSED (not, or no longer, approved),
/// or a database error.
pub fn stage(
    conn: &Connection,
    meta: &WriteMeta,
    span_uid: &str,
    step_uid: &str,
) -> Result<SpanView> {
    let (span, design, design_uid) = span_item(conn, span_uid)?;
    let step = item::id_for_uid(conn, step_uid)?
        .ok_or_else(|| not_found(format!("no plan step `{step_uid}`")))?;
    let kind = item::get(conn, step)?.map(|m| m.kind).unwrap_or_default();
    if kind != STEP_KIND {
        return Err(invalid(format!(
            "`{step_uid}` is a {kind}, not a plan step ({STEP_KIND})"
        )));
    }
    if plan::design_of_step(conn, step, step_uid)? != design {
        return Err(invalid(format!(
            "plan step {step_uid} belongs to another design's plan: a span is staged only into a \
             step of its own design ({design_uid})"
        )));
    }
    let current = span_now(conn, &span, design)?;
    if current.state == SpanState::Proposed {
        return Err(invalid(format!(
            "span {span_uid} is PROPOSED: only approved words are staged — approve it first (`jkb \
             design approve {span_uid} --base <token>`)"
        )));
    }
    edge::link(conn, meta, span.id, step, EdgeType::Stages, None)?;
    let staged = span_now(conn, &span, design)?;
    announce_span(conn, meta, &design_uid, &staged)?;
    Ok(staged)
}

/// Tell a design's subscribers a span's state changed (`kind = "span"` on `design/<uid>`): an
/// approval or a staging writes item metadata and edges, not the document, so no `update` carries
/// it. Best effort, as [`publish`] is; `jkb undo` of either announces through [`announce_spans`]. A
/// task finishing (IMPLEMENTED) is not announced here — it is
/// derived from task status, which this module does not write — so a subscriber re-reads
/// `design.spans` on any message, and on a `gap`.
fn announce_span(conn: &Connection, meta: &WriteMeta, design: &str, span: &SpanView) -> Result<()> {
    announce(
        conn,
        meta,
        design,
        "span",
        json!({ "design": design, "span": span.uid, "state": span.state.as_str() }),
    )
}

/// Fold a design's updates into its snapshot. Versions older than the result can no longer be
/// edited against; a version read after it can.
///
/// # Errors
/// An unknown design, or a database error.
pub fn compact(conn: &Connection, meta: &WriteMeta, uid: &str) -> Result<Compacted> {
    let id = design_id(conn, uid)?;
    let (doc, version) = load(conn, id)?;
    compact_into(conn, meta, id, &doc, version.seq)
}

/// Store `doc` — the design through `through` — as its snapshot and delete the rows it covers.
fn compact_into(
    conn: &Connection,
    meta: &WriteMeta,
    id: ItemId,
    doc: &Crdt,
    through: i64,
) -> Result<Compacted> {
    let previous = snapshot_row(conn, id)?.map(|(seq, _)| seq);
    if through == 0 || previous.is_some_and(|p| p >= through) {
        return Ok(Compacted {
            through: previous.unwrap_or(through),
            removed: 0,
        });
    }
    conn.prepare_cached(
        "INSERT INTO design_snapshots (design_id, seq, state_v1) VALUES (?1, ?2, ?3)
         ON CONFLICT(design_id) DO UPDATE SET seq = excluded.seq, state_v1 = excluded.state_v1,
             created_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )?
    .execute(params![id.get(), through, doc.encode_state()])?;
    let removed = conn
        .prepare_cached("DELETE FROM design_updates WHERE design_id = ?1 AND seq <= ?2")?
        .execute(params![id.get(), through])?;
    changelog::append(
        conn,
        meta,
        Op::Update,
        Entity::DesignSnapshots,
        &id.get().to_string(),
        Some(&json!({ "seq": previous })),
        Some(&json!({ "seq": through, "removed": removed })),
    )?;
    Ok(Compacted { through, removed })
}

/// Past [`COMPACT_AT`] rows, fold all but the newest [`COMPACT_KEEP`] into the snapshot, so the
/// cost of rebuilding the document for a write stays bounded however long an editor types. What it
/// costs is the same as an operator's compaction, on older rows only: a version read before the
/// fold can no longer be edited against (re-read and retry), and an update folded away can no
/// longer be undone (refused by name, [`unrevertable`]).
fn compact_if_due(conn: &Connection, meta: &WriteMeta, id: ItemId) -> Result<()> {
    let (rows, newest): (i64, i64) = conn
        .prepare_cached(
            "SELECT COUNT(*), COALESCE(MAX(seq), 0) FROM design_updates WHERE design_id = ?1",
        )?
        .query_row([id.get()], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if rows <= COMPACT_AT {
        return Ok(());
    }
    let through = newest - COMPACT_KEEP;
    let doc = Crdt::new();
    load_into(conn, id, Some(through), &doc)?;
    compact_into(conn, meta, id, &doc, through)?;
    Ok(())
}

/// The spans an undo's changelog `entries` touch: a span item's metadata (its approval), or a
/// `stages` edge out of a span. Read **before** the inversion, while an inserted edge still exists;
/// [`announce_spans`] tells their designs' subscribers afterwards. Unreadable entries are skipped:
/// this is bookkeeping around the inversion, never what makes an undo fail.
///
/// # Errors
/// A database error.
pub(crate) fn spans_touched(
    conn: &Connection,
    entries: &[(String, String, String, Option<String>)],
) -> Result<Vec<ItemId>> {
    let span_kind = |id: i64| -> Result<bool> {
        Ok(conn
            .prepare_cached("SELECT kind FROM items WHERE id = ?1")?
            .query_row([id], |r| r.get::<_, String>(0))
            .optional()?
            .is_some_and(|k| k == SPAN_KIND))
    };
    let mut out = Vec::new();
    for (op, table, entity_id, before) in entries {
        let src = if table == Entity::Items.as_str() {
            entity_id.parse::<i64>().ok()
        } else if table == Entity::Edges.as_str() && op == Op::Delete.as_str() {
            before
                .as_deref()
                .and_then(|b| serde_json::from_str::<Value>(b).ok())
                .filter(|b| b["type"] == EdgeType::Stages.as_str())
                .and_then(|b| b["src_item_id"].as_i64())
        } else if table == Entity::Edges.as_str() {
            match entity_id.parse::<i64>() {
                Ok(row) => conn
                    .prepare_cached("SELECT src_item_id FROM edges WHERE id = ?1 AND type = ?2")?
                    .query_row(params![row, EdgeType::Stages.as_str()], |r| r.get(0))
                    .optional()?,
                Err(_) => None,
            }
        } else {
            None
        };
        if let Some(id) = src {
            if span_kind(id)? && !out.contains(&ItemId::new(id)) {
                out.push(ItemId::new(id));
            }
        }
    }
    Ok(out)
}

/// Announce each of `spans`' state now on its design's topic, as [`approve`] and [`stage`] do — what
/// `jkb undo` calls after reverting an approval or a staging, so an open editor redraws it. A span
/// gone, or one that no longer reads, is skipped.
///
/// # Errors
/// A database error from the announcement.
pub(crate) fn announce_spans(conn: &Connection, meta: &WriteMeta, spans: &[ItemId]) -> Result<()> {
    for id in spans {
        let uid: Option<String> = conn
            .prepare_cached("SELECT uid FROM items WHERE id = ?1 AND kind = ?2")?
            .query_row(params![id.get(), SPAN_KIND], |r| r.get(0))
            .optional()?;
        let Some(uid) = uid else { continue };
        let Ok((span, design, design_uid)) = span_item(conn, &uid) else {
            continue;
        };
        let Ok(now) = span_now(conn, &span, design) else {
            continue;
        };
        announce_span(conn, meta, &design_uid, &now)?;
    }
    Ok(())
}

/// Why the update row `rowid` cannot be reverted, or `None` when it can — `undo`'s pre-flight.
///
/// # Errors
/// A database error.
pub(crate) fn unrevertable(conn: &Connection, rowid: i64) -> Result<Option<String>> {
    let exists: bool = conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM design_updates WHERE id = ?1)")?
        .query_row([rowid], |r| r.get(0))?;
    Ok((!exists).then(|| {
        format!(
            "design update {rowid} is gone — folded into its design's snapshot by a compaction, \
             or removed with its design — so what it changed can no longer be told apart from the \
             rest"
        )
    }))
}

/// `jkb undo` of a design update: append a forward update that reverts it — deletes what it
/// inserted, puts back what it deleted — and delete nothing (D53.4). Returns 1 when it wrote one,
/// 0 when the update's effect was already gone.
///
/// # Errors
/// A compacted update ([`unrevertable`]), or a database error.
pub(crate) fn revert_update(conn: &Connection, meta: &WriteMeta, rowid: i64) -> Result<usize> {
    if let Some(why) = unrevertable(conn, rowid)? {
        return Err(invalid(why));
    }
    let (design, bytes): (i64, Vec<u8>) = conn
        .prepare_cached("SELECT design_id, update_v1 FROM design_updates WHERE id = ?1")?
        .query_row([rowid], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let design = ItemId::new(design);
    let uid: String = conn
        .prepare_cached("SELECT uid FROM items WHERE id = ?1")?
        .query_row([design.get()], |r| r.get(0))?;
    let (doc, _) = load(conn, design)?;
    let items = span_items(conn, design)?;
    let before = overlaps(&doc, &items);
    let Some(revert) = doc.revert(&bytes)? else {
        return Ok(0);
    };
    refuse_new_overlaps(&doc, &items, &before).map_err(|e| {
        invalid(format!(
            "reverting design update {rowid} would put a span back over another written since \
             ({e}) — move or remove that span first"
        ))
    })?;
    store_row(conn, meta, design, &uid, &revert)?;
    Ok(1)
}

/// **Every table a design owns whose rows `jkb undo` of its creation must not cascade away**, as
/// `(table, design column)`. Each has a `txn_id` column naming the transaction that wrote the row —
/// that is how a later row is told from the creation's own, and a table without one does not belong
/// here. One line per table: a design-owned table a migration adds is one line here and is then held
/// to the rule with no other change. The names
/// are spliced into the query as identifiers, never values.
const DESIGN_OWNED: &[(&str, &str)] = &[
    ("design_updates", "design_id"),
    ("design_doc_targets", "design_id"),
    ("design_sources", "design_id"),
];

/// Why `jkb undo` of transaction `txn`'s insert of design `item` would lose later work, or `None`
/// (and `None` for an item that is not a design). Undoing the insert deletes the item, and
/// `ON DELETE CASCADE` takes every row of every [`DESIGN_OWNED`] table with it. So the undo is
/// refused while one of those tables holds a row a later transaction wrote — one not itself undone,
/// and not an `undo` (whose forward revert of an undone edit is no work of its own) — and while the
/// design has a compaction (`design_snapshots` has no `txn_id`, folds later rows into itself, and
/// is never undone). Asked by `undo`'s pre-flight for every `(insert, items)` entry. Design-only on
/// purpose; why is recorded under D47 in docs/namespaces-and-sync.md.
///
/// # Errors
/// A database error.
pub(crate) fn undo_would_lose(conn: &Connection, item: ItemId, txn: i64) -> Result<Option<String>> {
    let kind: Option<String> = conn
        .prepare_cached("SELECT kind FROM items WHERE id = ?1")?
        .query_row([item.get()], |r| r.get(0))
        .optional()?;
    if kind.as_deref() != Some(KIND) {
        return Ok(None);
    }
    // The permanent refusal first: undoing later work cannot clear a compaction, so naming a
    // transaction to undo instead would send the user to undo their own edits for nothing.
    if snapshot_row(conn, item)?.is_some() {
        return Ok(Some(
            "it created a design that has been compacted (`design_snapshots`): the compaction \
             folded later work into itself and is never undone, so the design keeps its creation"
                .to_owned(),
        ));
    }
    // The newest later transaction, the one `jkb undo` can take back next.
    for (table, column) in DESIGN_OWNED {
        let sql = format!(
            "SELECT t.txn_id FROM {table} t
              WHERE t.{column} = ?1 AND t.txn_id <> ?2 AND NOT {}
                AND NOT EXISTS (SELECT 1 FROM changelog x
                                 WHERE x.txn_id = t.txn_id AND x.op = 'undo')
              ORDER BY t.txn_id DESC LIMIT 1",
            crate::undo::undone_sql("t.txn_id")
        );
        let later: Option<i64> = conn
            .prepare_cached(&sql)?
            .query_row(params![item.get(), txn], |r| r.get(0))
            .optional()?;
        if let Some(later) = later {
            return Ok(Some(format!(
                "it created a design that transaction {later} has written `{table}` rows for \
                 since, and deleting the design would cascade them away — undo transaction \
                 {later} first"
            )));
        }
    }
    Ok(None)
}

/// Whether removing `item` would lose a design document — its updates are not part of an item
/// delete's snapshot, so `jkb undo` could not bring them back. `item::remove` refuses it.
///
/// # Errors
/// A database error.
pub(crate) fn holds_document(conn: &Connection, item: ItemId) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM design_updates WHERE design_id = ?1)
                 OR EXISTS (SELECT 1 FROM design_snapshots WHERE design_id = ?1)",
        )?
        .query_row([item.get()], |r| r.get(0))?)
}

#[cfg(test)]
mod tests;
