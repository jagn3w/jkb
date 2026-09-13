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
//! the wrong mechanism.) Gaps are possible — a rolled-back insert — reordering is not, and so a
//! cumulative [`ack`] is sound. Exercised across two processes by
//! `two_processes_never_hand_a_reader_a_lower_seq`, which fails on any delivery that hands a reader
//! a lower `seq` than one it already saw (watched failing with `poll` ordered descending).
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
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
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
    /// The topic is at its cap and nothing in it may be reaped: every remaining message is unread
    /// by at least one group (or the topic has no groups at all).
    #[error(
        "topic {topic} is full ({messages} messages, {bytes} bytes) and nothing in it has been \
         consumed by every group"
    )]
    QueueFull {
        /// The topic.
        topic: String,
        /// Messages held.
        messages: i64,
        /// Bytes held.
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
    /// An ack beyond the newest message the topic has ever held would pre-consume the future.
    #[error("cannot ack seq {seq} on topic {topic}: nothing past {high} has been sent")]
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
    let (mut messages, mut bytes) = holdings(conn, topic_id)?;
    let fits = |messages: i64, bytes: i64| {
        messages < spec.max_messages && bytes.saturating_add(size) <= spec.max_bytes
    };
    if fits(messages, bytes) {
        return Ok(());
    }
    if let Some(through) = consumed_through(conn, topic_id)? {
        let candidates: Vec<(i64, i64)> = conn
            .prepare_cached(
                "SELECT seq, size FROM mq_messages WHERE topic_id = ?1 AND seq <= ?2 ORDER BY seq",
            )?
            .query_map(params![topic_id, through], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut cutoff = None;
        for (seq, sz) in candidates {
            if fits(messages, bytes) {
                break;
            }
            messages -= 1;
            bytes -= sz;
            cutoff = Some(seq);
        }
        if let Some(cutoff) = cutoff {
            conn.prepare_cached("DELETE FROM mq_messages WHERE topic_id = ?1 AND seq <= ?2")?
                .execute(params![topic_id, cutoff])?;
        }
    }
    if fits(messages, bytes) {
        Ok(())
    } else {
        Err(QueueError::QueueFull {
            topic: topic.to_owned(),
            messages,
            bytes,
        }
        .into())
    }
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

/// Up to `max` messages after the group's position, in `seq` order. Records the poll (which is
/// what keeps an idle-but-alive consumer's group from being removed), so it is a write.
///
/// # Errors
/// [`QueueError::NoSuchTopic`], [`QueueError::NoSuchGroup`], [`QueueError::Corrupt`] for a stored
/// payload that no longer parses, or a database error.
pub fn poll(
    conn: &Connection,
    _meta: &WriteMeta,
    topic: &str,
    group: &str,
    max: usize,
    now: i64,
) -> Result<Vec<Delivered>> {
    let (topic_id, _) = topic_row(conn, topic)?;
    let position = group_position(conn, topic, topic_id, group)?;
    conn.prepare_cached(
        "UPDATE mq_groups SET last_poll_at = ?1 WHERE topic_id = ?2 AND name = ?3",
    )?
    .execute(params![now, topic_id, group])?;
    let limit = i64::try_from(max).unwrap_or(i64::MAX);
    // The payload is parsed after the query, so a corrupt one is a named error rather than a
    // rusqlite conversion failure: `payload` is carried as its stored text in `Value::String`.
    let rows: Vec<Delivered> = conn
        .prepare_cached(
            "SELECT seq, key, kind, payload, producer, enqueued_at, expires_at FROM mq_messages \
             WHERE topic_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?
        .query_map(params![topic_id, position, limit], |r| {
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
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|mut d| {
            let Value::String(text) = &d.payload else {
                return Err(QueueError::Corrupt(format!("seq {} payload", d.seq)).into());
            };
            d.payload = serde_json::from_str(text)
                .map_err(|e| QueueError::Corrupt(format!("seq {} payload: {e}", d.seq)))?;
            Ok(d)
        })
        .collect()
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
                         WHERE m.topic_id = g.topic_id AND m.seq > g.position) \
                 FROM mq_groups g WHERE g.topic_id = ?1 ORDER BY g.name",
            )?
            .query_map([topic_id], |r| {
                Ok(GroupReport {
                    name: r.get(0)?,
                    position: r.get(1)?,
                    last_poll_at: r.get(2)?,
                    last_ack_at: r.get(3)?,
                    backlog: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        out.push(TopicReport {
            name,
            spec,
            messages,
            bytes,
            newest_seq,
            oldest_unconsumed_at,
            groups,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
