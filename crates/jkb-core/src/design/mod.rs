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
//! span's item, as a snapshot of the document at the approval; which words are still the approved
//! ones is derived by comparing the text with that snapshot. STAGED (a `stages` edge to a plan step)
//! and IMPLEMENTED (every task under those steps `done`) are read from the graph, never stored.

pub mod crdt;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use jkb_types::{EdgeType, Error as TypeError, ItemId, PlacementRole};

use crate::changelog::{self, Entity, Op};
use crate::mq::{self, Draft, QueueError, TopicSpec};
use crate::store::WriteMeta;
use crate::{containment, edge, item, ns, placement, Error, Result};
use crdt::{Crdt, Piece, PieceKind};
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
    /// Staged, and every task under those steps is `done`. Derived.
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
            uid,
            title,
            namespace,
        });
    }
    Ok(out)
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
    Ok(seq)
}

/// Announce an update on `design/<uid>`: subscribers merge it (updates are idempotent and
/// commutative, so at-least-once is enough). Best effort by design — the table is the truth, and a
/// full or oversized queue costs a subscriber a re-read, never the write.
fn publish(conn: &Connection, meta: &WriteMeta, uid: &str, seq: i64, update: &[u8]) -> Result<()> {
    let name = topic(uid);
    let spec = TopicSpec {
        default_ttl_ms: Some(TOPIC_TTL_MS),
        ..TopicSpec::default()
    };
    let now = mq::now_ms();
    match mq::topic_create(conn, meta, &name, &spec, now) {
        Ok(_) | Err(Error::Queue(QueueError::TopicConflict(_))) => {}
        Err(e) => return Err(e),
    }
    let inline = (update.len() <= INLINE_UPDATE_MAX).then(|| STANDARD.encode(update));
    let sent = mq::send(
        conn,
        meta,
        &name,
        &Draft {
            key: uid.to_owned(),
            kind: "update".to_owned(),
            payload: json!({ "design": uid, "seq": seq, "update": inline }),
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
    // (byte, closes-first, marker): a span closing where another opens closes first.
    let mut marks: Vec<(usize, u8, String)> = Vec::new();
    for s in design.spans.iter().filter(|s| s.anchored) {
        marks.push((
            crdt::utf16_to_byte(text, s.start),
            1,
            format!("⟦{} {}⟧", s.uid, s.state.as_str()),
        ));
        marks.push((crdt::utf16_to_byte(text, s.end), 0, format!("⟦/{}⟧", s.uid)));
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
/// # Errors
/// Bytes that are not a Yjs v1 update, or one that builds on changes this design does not have.
pub fn apply(conn: &Connection, meta: &WriteMeta, uid: &str, update: &[u8]) -> Result<Written> {
    let id = design_id(conn, uid)?;
    let (doc, _) = load(conn, id)?;
    let before = span_views(conn, id, &doc)?;
    let was = doc.snapshot();
    doc.apply(update)?;
    if doc.has_missing() {
        return Err(invalid(
            "the update builds on changes this design does not have — send the updates it \
             depends on first (or re-sync with `design.state`)",
        ));
    }
    if doc.snapshot() == was {
        let (_, version) = load(conn, id)?;
        return Ok(Written {
            seq: None,
            version,
            demoted: Vec::new(),
            span: None,
        });
    }
    let seq = append_row(conn, meta, id, uid, update)?;
    finish(conn, id, &doc, &before, Some(seq), None)
}

/// The answer to a write that merged `doc`: its version, and the spans it demoted.
fn finish(
    conn: &Connection,
    id: ItemId,
    doc: &Crdt,
    before: &[SpanView],
    seq: Option<i64>,
    span: Option<String>,
) -> Result<Written> {
    let after = span_views(conn, id, doc)?;
    let demoted = after
        .iter()
        .filter(|a| a.demoted)
        .filter(|a| {
            before
                .iter()
                .any(|b| b.uid == a.uid && b.approved_by.is_some() && !b.demoted)
        })
        .map(|a| a.uid.clone())
        .collect();
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
    let before = span_views(conn, id, &now)?;
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
    let before = span_views(conn, id, &now)?;
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
    if let Some((ns, ne)) = now.range(&span) {
        for other in now.span_uids().into_iter().filter(|o| *o != span) {
            if let Some((os, oe)) = now.range(&other) {
                if ns < oe && os < ne {
                    return Err(invalid(format!(
                        "the quote overlaps span {other}: each piece of a design is in exactly one \
                         state, so spans do not overlap"
                    )));
                }
            }
        }
    }
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
            })
        })
        .collect()
}

/// The state a span's approved words are in: APPROVED, or STAGED / IMPLEMENTED from the graph.
fn approved_state(conn: &Connection, span: ItemId) -> Result<(SpanState, Vec<String>)> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.id, i.uid FROM edges e JOIN items i ON i.id = e.dst_item_id
          WHERE e.src_item_id = ?1 AND e.type = ?2 AND i.kind = ?3 ORDER BY i.uid",
    )?;
    let steps = stmt
        .query_map(
            params![span.get(), EdgeType::Stages.as_str(), STEP_KIND],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if steps.is_empty() {
        return Ok((SpanState::Approved, Vec::new()));
    }
    // Every task contained, at any depth, under the steps. IMPLEMENTED needs at least one: a step
    // with no tasks under it has implemented nothing.
    let mut implemented = true;
    let mut tasks = 0;
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE under(id) AS (
             SELECT child_item_id FROM containment WHERE parent_item_id = ?1
             UNION
             SELECT c.child_item_id FROM containment c JOIN under u ON c.parent_item_id = u.id
         )
         SELECT COUNT(*), COALESCE(SUM(i.status IS NOT 'done'), 0)
           FROM under u JOIN items i ON i.id = u.id WHERE i.kind = 'task'",
    )?;
    for (step, _) in &steps {
        let (count, open): (i64, i64) = stmt.query_row([step], |r| Ok((r.get(0)?, r.get(1)?)))?;
        tasks += count;
        implemented &= open == 0;
    }
    let state = if implemented && tasks > 0 {
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
    let approval = span.metadata.get("approval").filter(|a| !a.is_null());
    let field = |key: &str| {
        approval
            .and_then(|a| a.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let range = doc.range(&span.uid);
    let (start, end) = range.unwrap_or((0, 0));
    let (words, steps) = approved_state(conn, span.id)?;
    let pieces = match (&approval, range) {
        (_, None) => Vec::new(),
        (None, Some(_)) => vec![SpanPiece {
            start,
            end,
            state: SpanState::Proposed,
            removed: false,
            text: slice(text, start, end),
        }],
        (Some(_), Some(_)) => {
            let snapshot = field("snapshot").ok_or_else(|| {
                invalid(format!(
                    "span {} has an approval with no snapshot",
                    span.uid
                ))
            })?;
            let snapshot = STANDARD
                .decode(snapshot)
                .map_err(|e| invalid(format!("span {}'s approval is unreadable: {e}", span.uid)))?;
            doc.pieces(&span.uid, &crdt::decode_snapshot(&snapshot)?)?
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
                        text,
                    },
                )
                .collect()
        }
    };
    let demoted = approval.is_some() && pieces.iter().any(|p| p.state == SpanState::Proposed);
    let state = if approval.is_none() || demoted || range.is_none() {
        SpanState::Proposed
    } else {
        words
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

/// Approve a span: record the document as it is now as the words `approver` attests to (D53.5).
/// Only the reviewer the span names approves it — a span naming the operator is the operator's
/// alone; one naming Claude is a Claude session's, or the operator's, who holds every permission.
///
/// A span approved and unchanged since is answered as it is, writing nothing.
///
/// # Errors
/// An unknown span, an approver it does not name, a span whose anchors are gone, or a database
/// error.
pub fn approve(
    conn: &Connection,
    meta: &WriteMeta,
    span_uid: &str,
    approver: &Approver,
) -> Result<SpanView> {
    let (mut span, design, _) = span_item(conn, span_uid)?;
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
    let (doc, _) = load(conn, design)?;
    let current = view(conn, &span, &doc, &doc.text())?;
    if !current.anchored {
        return Err(invalid(format!(
            "span {span_uid} is no longer anchored in its design — there are no words to approve"
        )));
    }
    if current.approved_by.is_some() && !current.demoted {
        return Ok(current);
    }
    let snapshot = STANDARD.encode(crdt::encode_snapshot(&doc.snapshot()));
    let at: String = conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |r| {
        r.get(0)
    })?;
    if let Some(obj) = span.metadata.as_object_mut() {
        obj.insert(
            "approval".to_owned(),
            json!({ "snapshot": snapshot, "by": approver.label(), "at": at }),
        );
    }
    set_metadata(conn, meta, span.id, &span.metadata)?;
    view(conn, &span, &doc, &doc.text())
}

/// Stage an approved span into a plan step: a `stages` edge (D53.6). STAGED is derived from it.
///
/// # Errors
/// An unknown span or step, a span not (or no longer wholly) approved, or a database error.
pub fn stage(
    conn: &Connection,
    meta: &WriteMeta,
    span_uid: &str,
    step_uid: &str,
) -> Result<SpanView> {
    let (span, design, _) = span_item(conn, span_uid)?;
    let step = item::id_for_uid(conn, step_uid)?
        .ok_or_else(|| not_found(format!("no plan step `{step_uid}`")))?;
    let kind = item::get(conn, step)?.map(|m| m.kind).unwrap_or_default();
    if kind != STEP_KIND {
        return Err(invalid(format!(
            "`{step_uid}` is a {kind}, not a plan step ({STEP_KIND})"
        )));
    }
    let current = span_now(conn, &span, design)?;
    if current.approved_by.is_none() || current.demoted {
        return Err(invalid(format!(
            "span {span_uid} is {}: only approved words are staged — approve it first (`jkb design \
             approve {span_uid}`)",
            current.state.as_str()
        )));
    }
    edge::link(conn, meta, span.id, step, EdgeType::Stages, None)?;
    span_now(conn, &span, design)
}

/// Fold a design's updates into its snapshot. Versions older than the result can no longer be
/// edited against; a version read after it can.
///
/// # Errors
/// An unknown design, or a database error.
pub fn compact(conn: &Connection, meta: &WriteMeta, uid: &str) -> Result<Compacted> {
    let id = design_id(conn, uid)?;
    let (doc, version) = load(conn, id)?;
    let previous = snapshot_row(conn, id)?.map(|(seq, _)| seq);
    if version.seq == 0 || previous == Some(version.seq) {
        return Ok(Compacted {
            through: version.seq,
            removed: 0,
        });
    }
    conn.prepare_cached(
        "INSERT INTO design_snapshots (design_id, seq, state_v1) VALUES (?1, ?2, ?3)
         ON CONFLICT(design_id) DO UPDATE SET seq = excluded.seq, state_v1 = excluded.state_v1,
             created_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )?
    .execute(params![id.get(), version.seq, doc.encode_state()])?;
    let removed = conn
        .prepare_cached("DELETE FROM design_updates WHERE design_id = ?1 AND seq <= ?2")?
        .execute(params![id.get(), version.seq])?;
    changelog::append(
        conn,
        meta,
        Op::Update,
        Entity::DesignSnapshots,
        &id.get().to_string(),
        Some(&json!({ "seq": previous })),
        Some(&json!({ "seq": version.seq, "removed": removed })),
    )?;
    Ok(Compacted {
        through: version.seq,
        removed,
    })
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
    let Some(revert) = doc.revert(&bytes)? else {
        return Ok(0);
    };
    store_row(conn, meta, design, &uid, &revert)?;
    Ok(1)
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
