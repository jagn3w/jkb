//! The message queue: Kafka-shaped topics, keys and consumer positions, stored in `jkb.db`
//! (design r3.2 Q1–Q5, `openspec/changes/jkb-message-queue/design-r3.md`).
//!
//! - A **topic** is a named stream with its own size cap, default TTL and consumer groups.
//! - A **message** gets a queue-assigned `seq`, carries an opaque `key` (what it is about), a
//!   `kind` (what consumers dispatch on) and a JSON `payload`.
//! - A **consumer group** has one committed `position`: everything with `seq <= position` is
//!   consumed by it. There is no server-side filter — every group is handed every message and skips
//!   what it does not want, as a Kafka consumer does.
//!
//! **Order comes from `seq`, never from a clock.** `seq` is `AUTOINCREMENT`, allocated inside the
//! inserting transaction, and `SQLite` admits one write transaction at a time and holds its lock until
//! commit — across every process on one kernel. So no message can commit after a message allocated
//! a higher `seq`: a later-committed message always has a higher one. (That is `SQLite`'s writer lock
//! doing it, not jkb's `BEGIN IMMEDIATE`, which decides only when the lock is taken; design r3.1 named
//! the wrong mechanism.) A topic's seqs have gaps — the sequence is shared by every topic, and
//! reaped messages leave holes; a rolled-back insert's seq is reused, which is harmless — but they
//! are never reordered, and so a cumulative [`ack`] is sound. Exercised across two processes by
//! `two_processes_never_hand_a_reader_a_lower_seq`. Its guard for the cross-process claim is the
//! per-writer delivery count: a message committed with a seq below the reader's acked position is
//! never handed over, so an inversion shows up as a lost message, not as an out-of-order one
//! (watched failing with `poll` skipping one seq).
//!
//! **Reaping (the user's rules, 2026-09-13).** Expired messages are still delivered — a TTL never
//! skips anything, it only makes a message *eligible* to be reaped. A message is reaped only when
//! it has been **consumed by every group of its topic** (and the topic has at least one group)
//! **and** it has either **expired** or the topic is **at its size cap**. At the cap, [`send`]
//! reaps what that allows, oldest first; if nothing may go, the write is refused with
//! [`QueueError::QueueFull`]. Groups nobody has polled or acked for `group_idle_ms` are removed by
//! [`compact`], so an abandoned consumer cannot hold a topic full for ever.
//!
//! Every function takes `now` (Unix milliseconds) rather than reading a clock, so the rules are
//! testable; callers pass [`now_ms`]. Writes take a [`WriteMeta`] so they can only happen inside
//! `Db::write_txn`, but — like `blobs` and `task_transitions` — nothing here is changelogged: the
//! queue is transport, and `jkb undo` reaching into it would replay or erase deliveries.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::{Result, WriteMeta};

/// Largest accepted payload, in bytes of its JSON text.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
/// The most messages one [`poll`] hands over or one [`tail`] reads. Both run on the daemon's writer,
/// beside the notification hook's 1 s round trip, and each message is parsed from up to
/// [`MAX_PAYLOAD_BYTES`]: this keeps the worst read near 16 MiB rather than whatever a topic's creator
/// allowed it to hold. Asking either for more is refused rather than quietly served short: a consumer
/// reads a batch shorter than it asked for as having caught up, so a clamp announced `caught_up` after
/// every full batch of a backlog (stage-6.1 review).
pub const MAX_BATCH: usize = 256;
/// Largest accepted key, in bytes.
pub const MAX_KEY_BYTES: usize = 512;
/// Largest accepted topic, group, kind or producer name, in bytes.
pub const MAX_NAME_BYTES: usize = 200;
/// Default per-topic size cap in bytes (user, 2026-09-13).
pub const DEFAULT_MAX_BYTES: i64 = 10 * 1024 * 1024;
/// Default per-topic message cap (user, 2026-09-13).
pub const DEFAULT_MAX_MESSAGES: i64 = 10_000;
/// A group nobody has polled or acked for this long is removed by [`compact`] (user: 7 days).
pub const DEFAULT_GROUP_IDLE_MS: i64 = 7 * DAY_MS;
/// [`compact`] does nothing for a topic compacted more recently than this, unless forced.
pub const DEFAULT_COMPACT_EVERY_MS: i64 = 3 * DAY_MS;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Why a queue operation was refused.
///
/// Deliberately NOT `#[non_exhaustive]`: `jkb-api` maps every variant to a stable wire code, and an
/// exhaustive match there is what makes a new refusal decide its code instead of arriving at clients
/// as `internal`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QueueError {
    /// The topic does not exist. Producers never create one implicitly: a topic's type and caps
    /// are decided once, by [`topic_create`].
    #[error("no such topic: {0}")]
    NoSuchTopic(String),
    /// [`topic_create`] named an existing topic with a different spec.
    #[error("topic {0} already exists with a different spec")]
    TopicConflict(String),
    /// The group does not exist on this topic — never created, or removed by [`compact`] after
    /// idling. The documented response is to recreate it from now and accept the gap.
    #[error("no such group {group} on topic {topic}")]
    NoSuchGroup {
        /// The topic.
        topic: String,
        /// The group.
        group: String,
    },
    /// The topic is at its cap and reaping everything every group has consumed would still not
    /// make room — the rest is unread by at least one group, or the topic has no groups at all.
    #[error(
        "topic {topic} is full ({messages} messages, {bytes} bytes held) and reaping what every \
         group has consumed would not make room"
    )]
    QueueFull {
        /// The topic.
        topic: String,
        /// Messages held when the send was refused (the refused send reaps nothing).
        messages: i64,
        /// Bytes held when the send was refused.
        bytes: i64,
    },
    /// A payload, key or name over its limit, or a message larger than its topic's whole cap.
    #[error("{what} is {bytes} bytes; the limit is {max}")]
    TooLarge {
        /// What was too large.
        what: &'static str,
        /// Its size.
        bytes: usize,
        /// The limit.
        max: usize,
    },
    /// A name, key or spec value that is not allowed.
    #[error("invalid {what}: {why}")]
    Invalid {
        /// What was invalid.
        what: &'static str,
        /// Why.
        why: String,
    },
    /// An ack beyond both the newest message the topic currently holds and the group's own position
    /// would pre-consume the future.
    #[error(
        "cannot ack seq {seq} on topic {topic}: the newest it can be acked through is {high} (the \
         newest message it holds, or the group's position)"
    )]
    AckBeyondEnd {
        /// The topic.
        topic: String,
        /// The seq acked.
        seq: i64,
        /// The highest seq that may be acked.
        high: i64,
    },
    /// A stored row that should be valid is not.
    #[error("corrupt queue row: {0}")]
    Corrupt(String),
    /// The first message a poll would hand over has a payload that does not parse. Named by `seq`
    /// so the consumer can ack past it; `send` refuses anything that would not parse back, so this
    /// needs a row written some other way.
    #[error("message {seq} on topic {topic} has an unreadable payload: {why}")]
    CorruptPayload {
        /// The topic.
        topic: String,
        /// The message.
        seq: i64,
        /// The parse error.
        why: String,
    },
}

/// How a topic's messages are consumed. A closed set: adding `Work` or `Compacted` (design Q9) must
/// decide, at every `match`, what the new type does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueType {
    /// Append-only; each group has its own position; every group sees every message.
    Log,
}

impl QueueType {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "log" => Some(Self::Log),
            _ => None,
        }
    }
}

/// A topic's type and limits, decided once at creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicSpec {
    /// How it is consumed.
    pub queue_type: QueueType,
    /// Size cap in bytes (key + kind + payload per message).
    pub max_bytes: i64,
    /// Message cap.
    pub max_messages: i64,
    /// TTL applied to a message that names none. `None`: messages never expire by default.
    pub default_ttl_ms: Option<i64>,
    /// A group idle this long is removed by [`compact`].
    pub group_idle_ms: i64,
    /// Minimum time between two unforced compactions.
    pub compact_every_ms: i64,
}

impl Default for TopicSpec {
    fn default() -> Self {
        Self {
            queue_type: QueueType::Log,
            max_bytes: DEFAULT_MAX_BYTES,
            max_messages: DEFAULT_MAX_MESSAGES,
            default_ttl_ms: None,
            group_idle_ms: DEFAULT_GROUP_IDLE_MS,
            compact_every_ms: DEFAULT_COMPACT_EVERY_MS,
        }
    }
}

/// Whether a create made something or found it already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Created {
    /// Created now.
    New,
    /// Already existed, unchanged.
    Existing,
}

/// A message to send.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    /// What the message is about (Kafka's key). Opaque to the queue; never empty.
    pub key: String,
    /// What consumers dispatch on.
    pub kind: String,
    /// The body.
    pub payload: Value,
    /// Overrides the topic's default TTL.
    pub ttl_ms: Option<i64>,
    /// Who sent it, for diagnosis.
    pub producer: String,
}

/// Where a new group starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// After every message the topic holds now. A daemon installed today must not replay last
    /// week's messages.
    FromNow,
    /// Before every message the topic holds.
    FromStart,
}

/// A message handed to a consumer.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivered {
    /// The queue-assigned order.
    pub seq: i64,
    /// What it is about.
    pub key: String,
    /// What to dispatch on.
    pub kind: String,
    /// The body.
    pub payload: Value,
    /// Who sent it.
    pub producer: String,
    /// When it was enqueued (Unix ms).
    pub enqueued_at: i64,
    /// When it expires (Unix ms), if ever.
    pub expires_at: Option<i64>,
    /// Whether it had expired at the poll. Delivered anyway: a TTL never skips anything.
    pub expired: bool,
    /// Only from [`tail`]: the stored payload does not parse, and `payload` holds its raw text. A
    /// poll never hands such a message over (it names it with [`QueueError::CorruptPayload`]).
    pub unreadable: bool,
}

/// What [`compact`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactReport {
    /// Topics compacted.
    pub topics_compacted: usize,
    /// Topics skipped because they were compacted recently.
    pub topics_skipped: usize,
    /// Messages deleted (consumed by every group and expired).
    pub messages_reaped: usize,
    /// Idle groups removed.
    pub groups_removed: usize,
}

/// One group's state, for `jkb mq groups` and `jkb doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupReport {
    /// The group.
    pub name: String,
    /// Its committed position.
    pub position: i64,
    /// When it was created (Unix ms) — the idle clock's start when it has never polled or acked.
    pub created_at: i64,
    /// Messages after its position.
    pub backlog: i64,
    /// Last poll (Unix ms), if any.
    pub last_poll_at: Option<i64>,
    /// Last ack (Unix ms), if any.
    pub last_ack_at: Option<i64>,
}

/// One topic's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicReport {
    /// The topic.
    pub name: String,
    /// Its spec.
    pub spec: TopicSpec,
    /// Messages held.
    pub messages: i64,
    /// Bytes held.
    pub bytes: i64,
    /// The highest seq it holds, if any.
    pub newest_seq: Option<i64>,
    /// When [`compact`] last ran for it (Unix ms), if ever — a reap service that is not running shows
    /// up here.
    pub compacted_at: Option<i64>,
    /// When the oldest message not yet consumed by every group was enqueued, if any.
    pub oldest_unconsumed_at: Option<i64>,
    /// Its groups, by name.
    pub groups: Vec<GroupReport>,
}

/// The current time in Unix milliseconds, for callers of this module.
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn invalid(what: &'static str, why: impl Into<String>) -> QueueError {
    QueueError::Invalid {
        what,
        why: why.into(),
    }
}

/// Topic and group names: `/`-separated segments of `[A-Za-z0-9._-]`, no empty, `.` or `..`
/// segment, at most [`MAX_NAME_BYTES`].
fn check_name(what: &'static str, name: &str) -> std::result::Result<(), QueueError> {
    if name.len() > MAX_NAME_BYTES {
        return Err(QueueError::TooLarge {
            what,
            bytes: name.len(),
            max: MAX_NAME_BYTES,
        });
    }
    if name.is_empty() {
        return Err(invalid(what, "empty"));
    }
    for segment in name.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(invalid(
                what,
                format!("{name:?} has an empty, `.` or `..` segment"),
            ));
        }
        if let Some(c) = segment
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        {
            return Err(invalid(what, format!("{name:?} contains {c:?}")));
        }
    }
    Ok(())
}

fn check_positive(what: &'static str, v: i64) -> std::result::Result<(), QueueError> {
    if v > 0 {
        Ok(())
    } else {
        Err(invalid(what, format!("{v} is not positive")))
    }
}

fn to_usize(v: i64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

fn topic_row(conn: &Connection, name: &str) -> Result<(i64, TopicSpec)> {
    let row = conn
        .prepare_cached(
            "SELECT id, type, max_bytes, max_messages, default_ttl_ms, group_idle_ms, \
                    compact_every_ms \
             FROM mq_topics WHERE name = ?1",
        )?
        .query_row([name], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })
        .optional()?;
    let Some((id, ty, max_bytes, max_messages, default_ttl_ms, group_idle_ms, compact_every_ms)) =
        row
    else {
        return Err(QueueError::NoSuchTopic(name.to_owned()).into());
    };
    let queue_type = QueueType::parse(&ty)
        .ok_or_else(|| QueueError::Corrupt(format!("topic {name} has type {ty:?}")))?;
    Ok((
        id,
        TopicSpec {
            queue_type,
            max_bytes,
            max_messages,
            default_ttl_ms,
            group_idle_ms,
            compact_every_ms,
        },
    ))
}

fn group_position(conn: &Connection, topic: &str, topic_id: i64, group: &str) -> Result<i64> {
    conn.prepare_cached("SELECT position FROM mq_groups WHERE topic_id = ?1 AND name = ?2")?
        .query_row(params![topic_id, group], |r| r.get(0))
        .optional()?
        .ok_or_else(|| {
            QueueError::NoSuchGroup {
                topic: topic.to_owned(),
                group: group.to_owned(),
            }
            .into()
        })
}

fn holdings(conn: &Connection, topic_id: i64) -> Result<(i64, i64)> {
    Ok(conn
        .prepare_cached(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM mq_messages WHERE topic_id = ?1",
        )?
        .query_row([topic_id], |r| Ok((r.get(0)?, r.get(1)?)))?)
}

/// The lowest group position, i.e. the newest seq every group has consumed. `None` when the topic
/// has no groups, in which case nothing is consumed.
fn consumed_through(conn: &Connection, topic_id: i64) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT MIN(position) FROM mq_groups WHERE topic_id = ?1")?
        .query_row([topic_id], |r| r.get(0))?)
}

/// How many consumer groups `topic` has.
///
/// # Errors
/// [`QueueError::NoSuchTopic`], or a database error.
pub fn group_count(conn: &Connection, topic: &str) -> Result<i64> {
    let (topic_id, _) = topic_row(conn, topic)?;
    Ok(conn
        .prepare_cached("SELECT COUNT(*) FROM mq_groups WHERE topic_id = ?1")?
        .query_row([topic_id], |r| r.get(0))?)
}

/// Create a topic. Idempotent for an identical spec; a different spec is refused rather than
/// silently kept or overwritten, since both would leave a producer or consumer wrong about the caps.
///
/// # Errors
/// [`QueueError::Invalid`] for a bad name or spec, [`QueueError::TopicConflict`] for an existing
/// topic with a different spec, or a database error.
pub fn topic_create(
    conn: &Connection,
    _meta: &WriteMeta,
    name: &str,
    spec: &TopicSpec,
    now: i64,
) -> Result<Created> {
    check_name("topic name", name)?;
    check_positive("max_bytes", spec.max_bytes)?;
    check_positive("max_messages", spec.max_messages)?;
    check_positive("group_idle_ms", spec.group_idle_ms)?;
    check_positive("compact_every_ms", spec.compact_every_ms)?;
    if let Some(ttl) = spec.default_ttl_ms {
        check_positive("default_ttl_ms", ttl)?;
    }
    match topic_row(conn, name) {
        Ok((_, existing)) if existing == *spec => return Ok(Created::Existing),
        Ok(_) => return Err(QueueError::TopicConflict(name.to_owned()).into()),
        Err(crate::Error::Queue(QueueError::NoSuchTopic(_))) => {}
        Err(e) => return Err(e),
    }
    conn.prepare_cached(
        "INSERT INTO mq_topics (name, type, max_bytes, max_messages, default_ttl_ms, \
                                group_idle_ms, compact_every_ms, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?
    .execute(params![
        name,
        spec.queue_type.as_str(),
        spec.max_bytes,
        spec.max_messages,
        spec.default_ttl_ms,
        spec.group_idle_ms,
        spec.compact_every_ms,
        now
    ])?;
    Ok(Created::New)
}

/// Send a message; returns its `seq`.
///
/// At the topic's cap this reaps messages consumed by every group, oldest first, until the new one
/// fits — and refuses with [`QueueError::QueueFull`] when that is not enough.
///
/// # Errors
/// [`QueueError::NoSuchTopic`], [`QueueError::TooLarge`] / [`QueueError::Invalid`] for the draft,
/// [`QueueError::QueueFull`], or a database error.
pub fn send(
    conn: &Connection,
    _meta: &WriteMeta,
    topic: &str,
    draft: &Draft,
    now: i64,
) -> Result<i64> {
    let (topic_id, spec) = topic_row(conn, topic)?;
    let (payload, size) = validate_draft(draft, &spec)?;
    make_room(conn, topic, topic_id, &spec, size)?;
    let expires_at = draft
        .ttl_ms
        .or(spec.default_ttl_ms)
        .map(|ttl| now.saturating_add(ttl));
    let seq: i64 = conn
        .prepare_cached(
            "INSERT INTO mq_messages (topic_id, key, kind, payload, size, producer, enqueued_at, \
                                      expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) RETURNING seq",
        )?
        .query_row(
            params![
                topic_id,
                draft.key,
                draft.kind,
                payload,
                size,
                draft.producer,
                now,
                expires_at
            ],
            |r| r.get(0),
        )?;
    Ok(seq)
}

/// The draft's payload as stored JSON text and its size against the cap, or why it is refused.
fn validate_draft(
    draft: &Draft,
    spec: &TopicSpec,
) -> std::result::Result<(String, i64), QueueError> {
    let too_large = |what, bytes, max| QueueError::TooLarge { what, bytes, max };
    if draft.key.is_empty() {
        return Err(invalid("key", "empty"));
    }
    // One rule, here: the table's `length(key) > 0` CHECK stops counting at a NUL, so a key starting
    // with one would pass `is_empty` and then fail as a raw SQLite error.
    if draft.key.contains('\0') {
        return Err(invalid("key", "contains NUL"));
    }
    if draft.producer.contains('\0') {
        return Err(invalid("producer", "contains NUL"));
    }
    if draft.key.len() > MAX_KEY_BYTES {
        return Err(too_large("key", draft.key.len(), MAX_KEY_BYTES));
    }
    check_name("kind", &draft.kind)?;
    if draft.producer.len() > MAX_NAME_BYTES {
        return Err(too_large("producer", draft.producer.len(), MAX_NAME_BYTES));
    }
    if let Some(ttl) = draft.ttl_ms {
        check_positive("ttl_ms", ttl)?;
    }
    let payload =
        serde_json::to_string(&draft.payload).map_err(|e| invalid("payload", e.to_string()))?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(too_large("payload", payload.len(), MAX_PAYLOAD_BYTES));
    }
    // What is sent must come back out. serde_json serializes any depth but parses at most 128 levels,
    // so a deeply nested value would be stored and then fail every poll of the topic — which, never
    // consumed, is never reaped, and fills the topic. Refused here instead.
    if let Err(e) = serde_json::from_str::<Value>(&payload) {
        return Err(invalid("payload", format!("would not parse back: {e}")));
    }
    let size = draft.key.len() + draft.kind.len() + payload.len();
    let size_i = i64::try_from(size).unwrap_or(i64::MAX);
    if size_i > spec.max_bytes {
        return Err(too_large("message", size, to_usize(spec.max_bytes)));
    }
    Ok((payload, size_i))
}

/// Ensure a message of `size` bytes fits under the topic's caps: at the cap, reap what every group
/// has consumed, oldest first, and refuse with [`QueueError::QueueFull`] if that is not enough.
fn make_room(
    conn: &Connection,
    topic: &str,
    topic_id: i64,
    spec: &TopicSpec,
    size: i64,
) -> Result<()> {
    let (held_messages, held_bytes) = holdings(conn, topic_id)?;
    let fits = |messages: i64, bytes: i64| {
        messages < spec.max_messages && bytes.saturating_add(size) <= spec.max_bytes
    };
    if fits(held_messages, held_bytes) {
        return Ok(());
    }
    let (mut messages, mut bytes) = (held_messages, held_bytes);
    let mut cutoff = None;
    if let Some(through) = consumed_through(conn, topic_id)? {
        // Walked lazily, oldest first, stopping as soon as the message fits: at a steady cap a send
        // frees one slot, and collecting the whole consumed range to use its first row made every
        // such send read up to `max_messages` rows.
        let mut stmt = conn.prepare_cached(
            "SELECT seq, size FROM mq_messages WHERE topic_id = ?1 AND seq <= ?2 ORDER BY seq",
        )?;
        let mut rows = stmt.query(params![topic_id, through])?;
        while !fits(messages, bytes) {
            let Some(row) = rows.next()? else { break };
            messages -= 1;
            bytes -= row.get::<_, i64>(1)?;
            cutoff = Some(row.get::<_, i64>(0)?);
        }
    }
    if !fits(messages, bytes) {
        // Nothing is deleted on refusal, so the held totals are the ones reported.
        return Err(QueueError::QueueFull {
            topic: topic.to_owned(),
            messages: held_messages,
            bytes: held_bytes,
        }
        .into());
    }
    if let Some(cutoff) = cutoff {
        conn.prepare_cached("DELETE FROM mq_messages WHERE topic_id = ?1 AND seq <= ?2")?
            .execute(params![topic_id, cutoff])?;
    }
    Ok(())
}

/// Create a consumer group. Idempotent: an existing group keeps its position, whatever `start`
/// says, so a restarted consumer resumes rather than skipping or replaying.
///
/// # Errors
/// [`QueueError::NoSuchTopic`], [`QueueError::Invalid`] for the name, or a database error.
pub fn group_create(
    conn: &Connection,
    _meta: &WriteMeta,
    topic: &str,
    group: &str,
    start: Start,
    now: i64,
) -> Result<Created> {
    check_name("group name", group)?;
    let (topic_id, _) = topic_row(conn, topic)?;
    let position: i64 = match start {
        Start::FromStart => 0,
        Start::FromNow => conn
            .prepare_cached("SELECT COALESCE(MAX(seq), 0) FROM mq_messages WHERE topic_id = ?1")?
            .query_row([topic_id], |r| r.get(0))?,
    };
    let inserted = conn
        .prepare_cached(
            "INSERT INTO mq_groups (topic_id, name, position, created_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (topic_id, name) DO NOTHING",
        )?
        .execute(params![topic_id, group, position, now])?;
    Ok(if inserted == 1 {
        Created::New
    } else {
        Created::Existing
    })
}

/// Up to `max` messages after the group's position — or after `after`, when that is further on — in
/// `seq` order. Records the poll (which is what keeps an idle-but-alive consumer's group from being
/// removed), so it is a write.
///
/// `after` is the consumer's FETCH position, kept apart from its committed one as Kafka does: a
/// consumer that has handed messages on but not yet acked them asks for what comes after them.
/// Without it a batch full of unacked messages is returned again on every poll, and a consumer that
/// acks late never sees past its first batch.
///
/// # Errors
/// [`QueueError::Invalid`] for a `max` over [`MAX_BATCH`], [`QueueError::NoSuchTopic`],
/// [`QueueError::NoSuchGroup`], [`QueueError::CorruptPayload`] when the first message to hand over has
/// a payload that does not parse, or a database error.
pub fn poll(
    conn: &Connection,
    _meta: &WriteMeta,
    topic: &str,
    group: &str,
    max: usize,
    after: Option<i64>,
    now: i64,
) -> Result<Vec<Delivered>> {
    if max > MAX_BATCH {
        return Err(invalid("max", format!("at most {MAX_BATCH} messages")).into());
    }
    let (topic_id, _) = topic_row(conn, topic)?;
    let position = group_position(conn, topic, topic_id, group)?.max(after.unwrap_or(0));
    conn.prepare_cached(
        "UPDATE mq_groups SET last_poll_at = ?1 WHERE topic_id = ?2 AND name = ?3",
    )?
    .execute(params![now, topic_id, group])?;
    let limit = i64::try_from(max).unwrap_or(i64::MAX);
    let rows = message_rows(
        conn,
        "SELECT seq, key, kind, payload, producer, enqueued_at, expires_at FROM mq_messages \
         WHERE topic_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        params![topic_id, position, limit],
        now,
    )?;
    parse_payloads(topic, rows)
}

/// The newest `limit` messages a topic holds, oldest first, without touching any group — for
/// `jkb mq tail`. A payload that does not parse is returned flagged [`Delivered::unreadable`].
///
/// # Errors
/// [`QueueError::Invalid`] for a limit over [`MAX_BATCH`], [`QueueError::NoSuchTopic`], or a database
/// error.
pub fn tail(conn: &Connection, topic: &str, limit: usize, now: i64) -> Result<Vec<Delivered>> {
    if limit > MAX_BATCH {
        return Err(invalid("limit", format!("at most {MAX_BATCH} messages")).into());
    }
    let (topic_id, _) = topic_row(conn, topic)?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = message_rows(
        conn,
        "SELECT * FROM (SELECT seq, key, kind, payload, producer, enqueued_at, expires_at \
                        FROM mq_messages WHERE topic_id = ?1 ORDER BY seq DESC LIMIT ?2) \
         ORDER BY seq",
        params![topic_id, limit],
        now,
    )?;
    // A diagnostic view, so an unreadable payload is SHOWN (raw text, flagged) rather than ending
    // the listing: stopping at it would hide exactly the newest messages someone tailed to see.
    Ok(rows
        .into_iter()
        .map(|mut d| {
            if let Value::String(text) = &d.payload {
                match serde_json::from_str(text) {
                    Ok(value) => d.payload = value,
                    Err(_) => d.unreadable = true,
                }
            }
            d
        })
        .collect())
}

/// Rows of `seq, key, kind, payload, producer, enqueued_at, expires_at`, with the payload still its
/// stored text (in `Value::String`) so a corrupt one is a named error rather than a conversion
/// failure inside the query.
fn message_rows(
    conn: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
    now: i64,
) -> Result<Vec<Delivered>> {
    Ok(conn
        .prepare_cached(sql)?
        .query_map(args, |r| {
            let expires_at: Option<i64> = r.get(6)?;
            Ok(Delivered {
                seq: r.get(0)?,
                key: r.get(1)?,
                kind: r.get(2)?,
                payload: Value::String(r.get(3)?),
                producer: r.get(4)?,
                enqueued_at: r.get(5)?,
                expires_at,
                expired: expires_at.is_some_and(|at| at <= now),
                unreadable: false,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Parse stored payloads. A payload that does not parse ends the batch just before it, so everything
/// earlier is still handed over; when it is the FIRST message, the error names its seq so a consumer
/// can ack past it rather than the whole topic wedging behind it.
fn parse_payloads(topic: &str, rows: Vec<Delivered>) -> Result<Vec<Delivered>> {
    let mut out = Vec::with_capacity(rows.len());
    for mut d in rows {
        let parsed = match &d.payload {
            Value::String(text) => serde_json::from_str(text).map_err(|e| e.to_string()),
            _ => Err("not stored as text".to_owned()),
        };
        match parsed {
            Ok(value) => {
                d.payload = value;
                out.push(d);
            }
            Err(_) if !out.is_empty() => break,
            Err(why) => {
                return Err(QueueError::CorruptPayload {
                    topic: topic.to_owned(),
                    seq: d.seq,
                    why,
                }
                .into())
            }
        }
    }
    Ok(out)
}

/// How often [`poll`] refreshes a group's `last_poll_at` when it hands nothing over: at most every
/// hour, and at most a quarter of the topic's `group_idle_ms`, so a live consumer is never removed as
/// idle. Refreshing on every empty poll made an idle subscriber take the write lock four times a
/// second to rewrite a value compaction reads at day resolution.
pub const POLL_TOUCH_MS: i64 = 60 * 60 * 1000;

/// Whether a [`poll`] with these arguments has anything to do — messages to hand over, or a
/// `last_poll_at` due a refresh. A read, so an idle consumer can ask without taking the write lock;
/// callers skip the (writing) poll when it answers `false`.
///
/// # Errors
/// [`QueueError::NoSuchTopic`], [`QueueError::NoSuchGroup`], or a database error — the same refusals
/// the poll itself would give.
pub fn poll_needed(
    conn: &Connection,
    topic: &str,
    group: &str,
    after: Option<i64>,
    now: i64,
) -> Result<bool> {
    let (topic_id, spec) = topic_row(conn, topic)?;
    let (position, last_poll_at): (i64, Option<i64>) = conn
        .prepare_cached(
            "SELECT position, last_poll_at FROM mq_groups WHERE topic_id = ?1 AND name = ?2",
        )?
        .query_row(params![topic_id, group], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .ok_or_else(|| QueueError::NoSuchGroup {
            topic: topic.to_owned(),
            group: group.to_owned(),
        })?;
    let touch_every = POLL_TOUCH_MS.min((spec.group_idle_ms / 4).max(1));
    if last_poll_at.is_none_or(|at| now.saturating_sub(at) >= touch_every) {
        return Ok(true);
    }
    let from = position.max(after.unwrap_or(0));
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM mq_messages WHERE topic_id = ?1 AND seq > ?2)",
        )?
        .query_row(params![topic_id, from], |r| r.get(0))?)
}

/// Commit the group's position through `seq` (cumulative; never moves backwards). Returns the
/// position afterwards.
///
/// # Errors
/// [`QueueError::NoSuchTopic`], [`QueueError::NoSuchGroup`], [`QueueError::AckBeyondEnd`] for a
/// `seq` past the newest message the topic holds (and past the group's own position), or a database
/// error.
pub fn ack(
    conn: &Connection,
    _meta: &WriteMeta,
    topic: &str,
    group: &str,
    seq: i64,
    now: i64,
) -> Result<i64> {
    let (topic_id, _) = topic_row(conn, topic)?;
    let position = group_position(conn, topic, topic_id, group)?;
    let newest: Option<i64> = conn
        .prepare_cached("SELECT MAX(seq) FROM mq_messages WHERE topic_id = ?1")?
        .query_row([topic_id], |r| r.get(0))?;
    let high = newest.unwrap_or(0).max(position);
    if seq > high {
        return Err(QueueError::AckBeyondEnd {
            topic: topic.to_owned(),
            seq,
            high,
        }
        .into());
    }
    let position = position.max(seq);
    conn.prepare_cached(
        "UPDATE mq_groups SET position = ?1, last_ack_at = ?2 WHERE topic_id = ?3 AND name = ?4",
    )?
    .execute(params![position, now, topic_id, group])?;
    Ok(position)
}

/// Periodic housekeeping, run by the reap service. For each topic not compacted within its
/// `compact_every_ms` (or every topic when `force`): remove groups idle for `group_idle_ms`, then
/// delete messages consumed by every remaining group **and** expired.
///
/// Groups go first so a message only an abandoned group was holding back becomes reapable in the
/// same pass. A topic whose last group is removed has nothing consumed, so nothing is deleted.
///
/// # Errors
/// Returns a database error.
pub fn compact(
    conn: &Connection,
    _meta: &WriteMeta,
    now: i64,
    force: bool,
) -> Result<CompactReport> {
    let topics: Vec<(i64, i64, i64, Option<i64>)> = conn
        .prepare_cached("SELECT id, group_idle_ms, compact_every_ms, compacted_at FROM mq_topics")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut report = CompactReport::default();
    for (topic_id, idle_ms, every_ms, compacted_at) in topics {
        if !force && compacted_at.is_some_and(|at| now.saturating_sub(at) < every_ms) {
            report.topics_skipped += 1;
            continue;
        }
        report.groups_removed += conn
            .prepare_cached(
                "DELETE FROM mq_groups WHERE topic_id = ?1 \
                 AND ?2 - MAX(created_at, COALESCE(last_poll_at, 0), COALESCE(last_ack_at, 0)) \
                     >= ?3",
            )?
            .execute(params![topic_id, now, idle_ms])?;
        if let Some(through) = consumed_through(conn, topic_id)? {
            report.messages_reaped += conn
                .prepare_cached(
                    "DELETE FROM mq_messages WHERE topic_id = ?1 AND seq <= ?2 \
                     AND expires_at IS NOT NULL AND expires_at <= ?3",
                )?
                .execute(params![topic_id, through, now])?;
        }
        conn.prepare_cached("UPDATE mq_topics SET compacted_at = ?1 WHERE id = ?2")?
            .execute(params![now, topic_id])?;
        report.topics_compacted += 1;
    }
    Ok(report)
}

/// Every topic's holdings and groups, for `jkb mq topics|groups` and `jkb doctor`.
///
/// # Errors
/// Returns a database error.
pub fn inspect(conn: &Connection) -> Result<Vec<TopicReport>> {
    let names: Vec<String> = conn
        .prepare_cached("SELECT name FROM mq_topics ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let (topic_id, spec) = topic_row(conn, &name)?;
        let (messages, bytes) = holdings(conn, topic_id)?;
        let newest_seq: Option<i64> = conn
            .prepare_cached("SELECT MAX(seq) FROM mq_messages WHERE topic_id = ?1")?
            .query_row([topic_id], |r| r.get(0))?;
        let compacted_at: Option<i64> = conn
            .prepare_cached("SELECT compacted_at FROM mq_topics WHERE id = ?1")?
            .query_row([topic_id], |r| r.get(0))?;
        let through = consumed_through(conn, topic_id)?.unwrap_or(0);
        let oldest_unconsumed_at: Option<i64> = conn
            .prepare_cached(
                "SELECT enqueued_at FROM mq_messages WHERE topic_id = ?1 AND seq > ?2 \
                 ORDER BY seq LIMIT 1",
            )?
            .query_row(params![topic_id, through], |r| r.get(0))
            .optional()?;
        let groups = conn
            .prepare_cached(
                "SELECT g.name, g.position, g.last_poll_at, g.last_ack_at, \
                        (SELECT COUNT(*) FROM mq_messages m \
                         WHERE m.topic_id = g.topic_id AND m.seq > g.position), \
                        g.created_at \
                 FROM mq_groups g WHERE g.topic_id = ?1 ORDER BY g.name",
            )?
            .query_map([topic_id], |r| {
                Ok(GroupReport {
                    name: r.get(0)?,
                    position: r.get(1)?,
                    last_poll_at: r.get(2)?,
                    last_ack_at: r.get(3)?,
                    backlog: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        out.push(TopicReport {
            name,
            spec,
            messages,
            bytes,
            newest_seq,
            compacted_at,
            oldest_unconsumed_at,
            groups,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
