//! Typed operations over the knowledge base (design r3.2 H4, `openspec/changes/jkb-message-queue/`).
//!
//! **The op enum is the allowlist.** A process that must not open `jkb.db` itself — the dev
//! container, once stage 3's daemon exists — reaches the knowledge base only through a [`Request`],
//! and an operation that is not a variant cannot be asked for. An op is admissible only if it is
//! pure database work: nothing here makes the process that serves it read or write a file, fetch a
//! URL or run a command. Adding a variant is a code change reviewed against design H4's exclusion
//! table.
//!
//! **One dispatch.** [`LocalBackend`] serves requests in-process against a [`Db`]; the host CLI
//! uses it directly, and the daemon will be `LocalBackend` behind HTTP. So a request cannot be
//! answered one way locally and another way remotely.
//!
//! **Version skew is the normal state** between a container's `jkb` and the host's: an unknown op
//! or an unknown request field is refused (`deny_unknown_fields`), response fields are only ever
//! added, and an op never changes meaning — a changed meaning is a new op name.
//!
//! Stage 2 ships the message-queue ops (`mq.*`). The notification and agent read/write sets follow.

use jkb_core::mq::{self, Created, Draft, QueueError, Start, TopicSpec};
use jkb_core::Db;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One operation. Serialized with an `"op"` tag, e.g. `{"op":"mq.send","topic":"t",…}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
pub enum Request {
    /// Create a topic (idempotent for an identical spec).
    #[serde(rename = "mq.topic_create")]
    MqTopicCreate {
        /// The topic.
        topic: String,
        /// Its spec; omitted fields take the defaults.
        #[serde(default)]
        spec: SpecInput,
    },
    /// Send a message.
    #[serde(rename = "mq.send")]
    MqSend {
        /// The topic.
        topic: String,
        /// What the message is about.
        key: String,
        /// What consumers dispatch on.
        kind: String,
        /// The body.
        payload: Value,
        /// Overrides the topic's default TTL.
        #[serde(default)]
        ttl_ms: Option<i64>,
        /// Who sent it.
        producer: String,
    },
    /// Create a consumer group (an existing one keeps its position).
    #[serde(rename = "mq.group_create")]
    MqGroupCreate {
        /// The topic.
        topic: String,
        /// The group.
        group: String,
        /// Start before everything the topic holds, instead of after it.
        #[serde(default)]
        from_start: bool,
    },
    /// Messages after the group's position.
    #[serde(rename = "mq.poll")]
    MqPoll {
        /// The topic.
        topic: String,
        /// The group.
        group: String,
        /// At most this many.
        max: usize,
        /// The consumer's fetch position: read after this seq when it is past the committed
        /// position (messages handed on but not yet acked).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<i64>,
    },
    /// Commit the group's position through `seq`.
    #[serde(rename = "mq.ack")]
    MqAck {
        /// The topic.
        topic: String,
        /// The group.
        group: String,
        /// The seq to commit through.
        seq: i64,
    },
    /// Remove idle groups and reap consumed, expired messages.
    #[serde(rename = "mq.compact")]
    MqCompact {
        /// Ignore each topic's compaction interval.
        #[serde(default)]
        force: bool,
    },
    /// Every topic's holdings and groups.
    #[serde(rename = "mq.inspect")]
    MqInspect,
    /// The newest messages a topic holds, oldest first, without touching any group.
    #[serde(rename = "mq.tail")]
    MqTail {
        /// The topic.
        topic: String,
        /// At most this many.
        limit: usize,
    },
}

/// A topic spec as a request carries it: every field optional, defaults from [`TopicSpec`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecInput {
    /// Size cap in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<i64>,
    /// Message cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_messages: Option<i64>,
    /// Default TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ttl_ms: Option<i64>,
    /// Idle period after which a group is removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_idle_ms: Option<i64>,
    /// Minimum time between unforced compactions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_every_ms: Option<i64>,
}

impl SpecInput {
    /// The full spec, defaults filled in.
    #[must_use]
    pub fn resolve(&self) -> TopicSpec {
        let d = TopicSpec::default();
        TopicSpec {
            queue_type: d.queue_type,
            max_bytes: self.max_bytes.unwrap_or(d.max_bytes),
            max_messages: self.max_messages.unwrap_or(d.max_messages),
            default_ttl_ms: self.default_ttl_ms.or(d.default_ttl_ms),
            group_idle_ms: self.group_idle_ms.unwrap_or(d.group_idle_ms),
            compact_every_ms: self.compact_every_ms.unwrap_or(d.compact_every_ms),
        }
    }
}

/// A message as a response carries it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Queue-assigned order.
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
    /// Whether it had expired when read. Delivered anyway.
    pub expired: bool,
}

impl From<mq::Delivered> for Message {
    fn from(d: mq::Delivered) -> Self {
        Self {
            seq: d.seq,
            key: d.key,
            kind: d.kind,
            payload: d.payload,
            producer: d.producer,
            enqueued_at: d.enqueued_at,
            expires_at: d.expires_at,
            expired: d.expired,
        }
    }
}

/// A group as `mq.inspect` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// The group.
    pub name: String,
    /// Committed position.
    pub position: i64,
    /// Messages after it.
    pub backlog: i64,
    /// Created (Unix ms).
    pub created_at: i64,
    /// Last poll (Unix ms).
    pub last_poll_at: Option<i64>,
    /// Last ack (Unix ms).
    pub last_ack_at: Option<i64>,
}

/// A topic as `mq.inspect` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topic {
    /// The topic.
    pub name: String,
    /// Queue type.
    pub queue_type: String,
    /// Size cap in bytes.
    pub max_bytes: i64,
    /// Message cap.
    pub max_messages: i64,
    /// Default TTL.
    pub default_ttl_ms: Option<i64>,
    /// Group idle period.
    pub group_idle_ms: i64,
    /// Compaction interval.
    pub compact_every_ms: i64,
    /// Messages held.
    pub messages: i64,
    /// Bytes held.
    pub bytes: i64,
    /// Newest seq held.
    pub newest_seq: Option<i64>,
    /// Last compaction (Unix ms).
    pub compacted_at: Option<i64>,
    /// Oldest message not consumed by every group (Unix ms).
    pub oldest_unconsumed_at: Option<i64>,
    /// Its groups.
    pub groups: Vec<Group>,
}

impl From<mq::TopicReport> for Topic {
    fn from(t: mq::TopicReport) -> Self {
        Self {
            name: t.name,
            queue_type: t.spec.queue_type.as_str().to_owned(),
            max_bytes: t.spec.max_bytes,
            max_messages: t.spec.max_messages,
            default_ttl_ms: t.spec.default_ttl_ms,
            group_idle_ms: t.spec.group_idle_ms,
            compact_every_ms: t.spec.compact_every_ms,
            messages: t.messages,
            bytes: t.bytes,
            newest_seq: t.newest_seq,
            compacted_at: t.compacted_at,
            oldest_unconsumed_at: t.oldest_unconsumed_at,
            groups: t
                .groups
                .into_iter()
                .map(|g| Group {
                    name: g.name,
                    position: g.position,
                    backlog: g.backlog,
                    created_at: g.created_at,
                    last_poll_at: g.last_poll_at,
                    last_ack_at: g.last_ack_at,
                })
                .collect(),
        }
    }
}

/// The answer to a [`Request`]. Serialized with a `"result"` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// A topic or group create: whether it was new.
    Created {
        /// `false` when it already existed.
        created: bool,
    },
    /// A send: the assigned seq.
    Sent {
        /// The seq.
        seq: i64,
    },
    /// A poll or tail.
    Messages {
        /// In seq order.
        messages: Vec<Message>,
    },
    /// An ack: the position afterwards.
    Position {
        /// The committed position.
        position: i64,
    },
    /// A compaction.
    Compacted {
        /// Topics compacted.
        topics_compacted: usize,
        /// Topics skipped (compacted recently).
        topics_skipped: usize,
        /// Messages reaped.
        messages_reaped: usize,
        /// Groups removed.
        groups_removed: usize,
    },
    /// An inspect.
    Topics {
        /// By name.
        topics: Vec<Topic>,
    },
}

/// A stable error code, so a client in another process — or another version — can branch on it
/// without parsing a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// No such topic.
    NoSuchTopic,
    /// A topic exists with a different spec.
    TopicConflict,
    /// No such group (never created, or removed after idling).
    NoSuchGroup,
    /// The topic is full and nothing may be reaped.
    QueueFull,
    /// Something over its size limit.
    TooLarge,
    /// A name, key, payload or spec value that is not allowed.
    Invalid,
    /// An ack past the newest message.
    AckBeyondEnd,
    /// A stored message whose payload does not parse; `seq` names it.
    CorruptPayload,
    /// The request itself could not be read.
    BadRequest,
    /// Anything else: a database or internal failure.
    Internal,
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct ApiError {
    /// What kind of failure.
    pub code: ErrorCode,
    /// A human-readable account.
    pub message: String,
    /// The message concerned, for [`ErrorCode::CorruptPayload`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
}

impl ApiError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            seq: None,
        }
    }

    /// An error for a request that could not be parsed.
    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }
}

impl From<jkb_core::Error> for ApiError {
    fn from(e: jkb_core::Error) -> Self {
        let message = e.to_string();
        let code = match &e {
            jkb_core::Error::Queue(q) => match q {
                QueueError::NoSuchTopic(_) => ErrorCode::NoSuchTopic,
                QueueError::TopicConflict(_) => ErrorCode::TopicConflict,
                QueueError::NoSuchGroup { .. } => ErrorCode::NoSuchGroup,
                QueueError::QueueFull { .. } => ErrorCode::QueueFull,
                QueueError::TooLarge { .. } => ErrorCode::TooLarge,
                QueueError::Invalid { .. } => ErrorCode::Invalid,
                QueueError::AckBeyondEnd { .. } => ErrorCode::AckBeyondEnd,
                QueueError::CorruptPayload { seq, .. } => {
                    return Self {
                        code: ErrorCode::CorruptPayload,
                        message,
                        seq: Some(*seq),
                    }
                }
                _ => ErrorCode::Internal,
            },
            _ => ErrorCode::Internal,
        };
        Self::new(code, message)
    }
}

/// Where requests are served. `LocalBackend` in-process; a remote one (stage 3) over HTTP.
pub trait Backend {
    /// Serve one request.
    ///
    /// # Errors
    /// An [`ApiError`] describing why the request failed.
    fn call(&self, request: Request) -> Result<Response, ApiError>;
}

/// Serves requests against a [`Db`] in this process.
#[derive(Clone)]
pub struct LocalBackend {
    db: Db,
}

impl LocalBackend {
    /// A backend over `db`.
    #[must_use]
    pub const fn new(db: Db) -> Self {
        Self { db }
    }
}

const ACTOR: &str = "jkb-api";

impl Backend for LocalBackend {
    fn call(&self, request: Request) -> Result<Response, ApiError> {
        let now = mq::now_ms();
        let created = |c: Created| Response::Created {
            created: c == Created::New,
        };
        Ok(match request {
            Request::MqTopicCreate { topic, spec } => {
                let spec = spec.resolve();
                created(self.db.write_txn(ACTOR, move |c, m| {
                    mq::topic_create(c, m, &topic, &spec, now)
                })?)
            }
            Request::MqSend {
                topic,
                key,
                kind,
                payload,
                ttl_ms,
                producer,
            } => {
                let draft = Draft {
                    key,
                    kind,
                    payload,
                    ttl_ms,
                    producer,
                };
                Response::Sent {
                    seq: self
                        .db
                        .write_txn(ACTOR, move |c, m| mq::send(c, m, &topic, &draft, now))?,
                }
            }
            Request::MqGroupCreate {
                topic,
                group,
                from_start,
            } => {
                let start = if from_start {
                    Start::FromStart
                } else {
                    Start::FromNow
                };
                created(self.db.write_txn(ACTOR, move |c, m| {
                    mq::group_create(c, m, &topic, &group, start, now)
                })?)
            }
            Request::MqPoll {
                topic,
                group,
                max,
                after,
            } => Response::Messages {
                messages: self
                    .db
                    .write_txn(ACTOR, move |c, m| {
                        mq::poll(c, m, &topic, &group, max, after, now)
                    })?
                    .into_iter()
                    .map(Message::from)
                    .collect(),
            },
            Request::MqAck { topic, group, seq } => Response::Position {
                position: self
                    .db
                    .write_txn(ACTOR, move |c, m| mq::ack(c, m, &topic, &group, seq, now))?,
            },
            Request::MqCompact { force } => {
                let r = self
                    .db
                    .write_txn(ACTOR, move |c, m| mq::compact(c, m, now, force))?;
                Response::Compacted {
                    topics_compacted: r.topics_compacted,
                    topics_skipped: r.topics_skipped,
                    messages_reaped: r.messages_reaped,
                    groups_removed: r.groups_removed,
                }
            }
            Request::MqInspect => Response::Topics {
                topics: self
                    .db
                    .read(mq::inspect)?
                    .into_iter()
                    .map(Topic::from)
                    .collect(),
            },
            Request::MqTail { topic, limit } => Response::Messages {
                messages: self
                    .db
                    .read(move |c| mq::tail(c, &topic, limit, now))?
                    .into_iter()
                    .map(Message::from)
                    .collect(),
            },
        })
    }
}

#[cfg(test)]
mod tests;
