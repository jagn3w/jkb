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
//! Stage 2 shipped the message-queue ops (`mq.*`), stage 5 the notification ops (`notify.*`), stage
//! 6.1 the agent read set (`kb.*`, `task.ready`/`show`/`subtasks`, in [`kb`]); the task-mutate set
//! follows.

use std::sync::Arc;

use jkb_core::mq::{self, Created, Draft, QueueError, Start, TopicSpec};
use jkb_core::notify::{self, NotifEvent, Observation};
use jkb_core::Db;
use jkb_types::Embedder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod kb;

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
    ///
    /// An EMPTY STRUCT variant, not a unit one: serde ignores extra keys on a unit variant of an
    /// internally tagged enum even under `deny_unknown_fields`, so `{"op":"mq.inspect","topic":…}`
    /// parsed and answered for every topic. Every field-less op must be written this way.
    #[serde(rename = "mq.inspect")]
    MqInspect {},
    /// The newest messages a topic holds, oldest first, without touching any group.
    #[serde(rename = "mq.tail")]
    MqTail {
        /// The topic.
        topic: String,
        /// At most this many.
        limit: usize,
    },
    /// One Claude Code hook event, as the hook observed it. The daemon runs the notification
    /// machine against its record of the session and sends the effects on `claude/notify`.
    #[serde(rename = "notify.event")]
    NotifyEvent {
        /// The session id, sanitized to `[A-Za-z0-9_-]` (anything else is refused).
        session: String,
        /// What happened.
        event: HookEvent,
        /// The tool that just finished (`PostToolUse`), if any.
        #[serde(default)]
        tool: String,
        /// The notification text (`Notification`), if any.
        #[serde(default)]
        message: String,
        /// The session's working directory.
        #[serde(default)]
        cwd: String,
        /// The `claude` process's pid, or empty when the hook had none it could trust.
        #[serde(default)]
        owner: String,
        /// Where `owner` means something: `host[#boot][/pidns]`, built by `jkb notify hook`.
        #[serde(default)]
        instance: String,
    },
    /// Every notification the daemon holds a record of, for a producer's `SessionStart` sweep.
    #[serde(rename = "notify.open_sessions")]
    NotifyOpenSessions {},
    /// A producer judged the session gone from its record: withdraw the notification, but only if
    /// the record still names the owner and instance that judgement was made from.
    #[serde(rename = "notify.gone")]
    NotifyGone {
        /// The session.
        session: String,
        /// The record's owner pid, as the producer read it.
        owner: String,
        /// The record's instance, as the producer read it.
        instance: String,
    },
    /// The namespace of the mount holding a working directory — what an unscoped read defaults to.
    #[serde(rename = "kb.ambient")]
    KbAmbient {
        /// The client's working directory, absolute, in its own filesystem.
        cwd: String,
        /// The client's `$HOME`, so a directory under it is looked up under the server's
        /// ([`kb::ambient`]). Empty for none.
        #[serde(default)]
        home: String,
    },
    /// The items a query DSL matches.
    #[serde(rename = "kb.query")]
    KbQuery {
        /// The query DSL.
        dsl: String,
        /// The scope when the DSL names none.
        #[serde(default)]
        default_scope: Option<String>,
        /// At most this many (ignored with `count`).
        #[serde(default)]
        limit: Option<usize>,
        /// Answer how many instead of which.
        #[serde(default)]
        count: bool,
        /// The order to list in, applied before `limit`.
        #[serde(default)]
        order: kb::QueryOrder,
    },
    /// The children of a namespace or container.
    #[serde(rename = "kb.ls")]
    KbLs {
        /// The namespace path or item uid; top level when absent.
        #[serde(default)]
        path: Option<String>,
        /// Include terminal items and chunks.
        #[serde(default)]
        all: bool,
        /// Descend into every namespace below.
        #[serde(default)]
        recursive: bool,
    },
    /// A subtree.
    #[serde(rename = "kb.tree")]
    KbTree {
        /// The namespace path or item uid; top level when absent.
        #[serde(default)]
        path: Option<String>,
        /// Include terminal items and chunks.
        #[serde(default)]
        all: bool,
        /// Levels to descend; at most [`kb::MAX_TREE_DEPTH`], which is also what absent asks for.
        #[serde(default)]
        depth: Option<usize>,
    },
    /// An item's full content.
    #[serde(rename = "kb.cat")]
    KbCat {
        /// The item.
        uid: String,
    },
    /// Items whose content holds a literal pattern, with the matching lines.
    #[serde(rename = "kb.grep")]
    KbGrep {
        /// The literal pattern.
        pattern: String,
        /// The namespace subtree to search; everywhere when absent.
        #[serde(default)]
        scope: Option<String>,
        /// Fold case (Unicode).
        #[serde(default)]
        ignore_case: bool,
        /// Lines, names only, or a count.
        #[serde(default)]
        mode: kb::GrepMode,
    },
    /// Ranked search.
    #[serde(rename = "kb.search")]
    KbSearch {
        /// The query DSL.
        dsl: String,
        /// The scope when the DSL names none.
        #[serde(default)]
        default_scope: Option<String>,
        /// The route; a backend without an embedder serves only `fts`.
        route: kb::SearchRoute,
        /// At most this many hits (at most [`kb::MAX_SEARCH_LIMIT`]).
        limit: usize,
        /// ±N neighbour chunks per hit (at most [`kb::MAX_SEARCH_CONTEXT`]).
        #[serde(default)]
        context: Option<usize>,
    },
    /// The ready frontier.
    #[serde(rename = "task.ready")]
    TaskReady {
        /// Query DSL: its scope and tags narrow the frontier.
        dsl: String,
        /// The scope when the DSL names none.
        #[serde(default)]
        default_scope: Option<String>,
        /// At most this many.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// A task in full.
    #[serde(rename = "task.show")]
    TaskShow {
        /// A task uid or bare slug.
        uid: String,
    },
    /// A task's subtasks.
    #[serde(rename = "task.subtasks")]
    TaskSubtasks {
        /// A task uid or bare slug.
        uid: String,
        /// Include terminal subtasks.
        #[serde(default)]
        all: bool,
    },
}

/// A hook event on the wire. `session_gone` is deliberately not one: only `notify.gone` asserts it,
/// with the owner it probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// `Notification`.
    Needed,
    /// `PostToolUse`.
    ToolFinished,
    /// `UserPromptSubmit`.
    UserActed,
    /// `Stop`.
    TurnEnded,
    /// `SessionEnd`.
    SessionEnded,
}

impl From<HookEvent> for NotifEvent {
    fn from(e: HookEvent) -> Self {
        match e {
            HookEvent::Needed => Self::Needed,
            HookEvent::ToolFinished => Self::ToolFinished,
            HookEvent::UserActed => Self::UserActed,
            HookEvent::TurnEnded => Self::TurnEnded,
            HookEvent::SessionEnded => Self::SessionEnded,
        }
    }
}

/// A notification record as `notify.open_sessions` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifySession {
    /// The session.
    pub session: String,
    /// The tool the prompt named, or empty.
    pub tool: String,
    /// The owner pid the hook recorded, or empty.
    pub owner: String,
    /// Where that pid means something: `host[#boot][/pidns]`.
    pub instance: String,
    /// When the record was last written (Unix ms).
    pub updated_at: i64,
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
    /// Only from `mq.tail`: the stored payload does not parse and `payload` is its raw text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unreadable: bool,
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
            unreadable: d.unreadable,
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

impl Request {
    /// Every op name, as `GET /v1/hello` advertises them.
    pub const OPS: &'static [&'static str] = &[
        "mq.topic_create",
        "mq.send",
        "mq.group_create",
        "mq.poll",
        "mq.ack",
        "mq.compact",
        "mq.inspect",
        "mq.tail",
        "notify.event",
        "notify.open_sessions",
        "notify.gone",
        "kb.ambient",
        "kb.query",
        "kb.ls",
        "kb.tree",
        "kb.cat",
        "kb.grep",
        "kb.search",
        "task.ready",
        "task.show",
        "task.subtasks",
    ];

    /// This request's op name — the `"op"` tag it serializes with. Exhaustive, so a new op must be
    /// named here; `every_op_names_its_own_wire_tag_and_is_advertised` checks [`Request::OPS`]
    /// against the tags serde accepts and each op's name against the tag it serializes with.
    #[must_use]
    pub const fn op(&self) -> &'static str {
        match self {
            Self::MqTopicCreate { .. } => "mq.topic_create",
            Self::MqSend { .. } => "mq.send",
            Self::MqGroupCreate { .. } => "mq.group_create",
            Self::MqPoll { .. } => "mq.poll",
            Self::MqAck { .. } => "mq.ack",
            Self::MqCompact { .. } => "mq.compact",
            Self::MqInspect {} => "mq.inspect",
            Self::MqTail { .. } => "mq.tail",
            Self::NotifyEvent { .. } => "notify.event",
            Self::NotifyOpenSessions {} => "notify.open_sessions",
            Self::NotifyGone { .. } => "notify.gone",
            Self::KbAmbient { .. } => "kb.ambient",
            Self::KbQuery { .. } => "kb.query",
            Self::KbLs { .. } => "kb.ls",
            Self::KbTree { .. } => "kb.tree",
            Self::KbCat { .. } => "kb.cat",
            Self::KbGrep { .. } => "kb.grep",
            Self::KbSearch { .. } => "kb.search",
            Self::TaskReady { .. } => "task.ready",
            Self::TaskShow { .. } => "task.show",
            Self::TaskSubtasks { .. } => "task.subtasks",
        }
    }

    /// Whether this op is in the agent read set (`kb.*`, `task.ready`/`show`/`subtasks`) — the one
    /// place that says so. [`LocalBackend`] serves it on its reader and within its read budget, and
    /// `jkb serve` counts it against its read permits, from this answer; no dispatch arm chooses.
    ///
    /// **Not "every op that does not write".** The reader serves one call at a time, and what queues
    /// there is a client's reads — a grep over the whole knowledge base among them. The queue's and the
    /// notification hook's own reads (`mq.inspect`, `mq.tail`, `notify.open_sessions`) are short and
    /// latency-bound — a `SessionStart` sweep has 1 s — so they stay on the writer, which runs only such
    /// ops; classing them by "does not write" put the sweep behind a container's grep (stage-6.1 review).
    /// Exhaustive, so a new op must say. A write classed here fails against the daemon's `query_only`
    /// reader rather than writing somewhere unguarded (`every_op_is_served_on_the_connection_its_class_names`).
    #[must_use]
    pub const fn is_agent_read(&self) -> bool {
        match self {
            Self::KbAmbient { .. }
            | Self::KbQuery { .. }
            | Self::KbLs { .. }
            | Self::KbTree { .. }
            | Self::KbCat { .. }
            | Self::KbGrep { .. }
            | Self::KbSearch { .. }
            | Self::TaskReady { .. }
            | Self::TaskShow { .. }
            | Self::TaskSubtasks { .. } => true,
            Self::MqTopicCreate { .. }
            | Self::MqSend { .. }
            | Self::MqGroupCreate { .. }
            | Self::MqPoll { .. }
            | Self::MqAck { .. }
            | Self::MqCompact { .. }
            | Self::MqInspect {}
            | Self::MqTail { .. }
            | Self::NotifyEvent { .. }
            | Self::NotifyOpenSessions {}
            | Self::NotifyGone { .. } => false,
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
    /// A `notify.event` or `notify.gone`: what the notification machine did.
    Notified {
        /// The session's state afterwards (`absent`, `awaiting_tool`, `awaiting_user`).
        state: String,
        /// Whether a transition fired.
        moved: bool,
        /// The plan carried out, in order (`post`, `withdraw`, `remember`, `forget`).
        effects: Vec<String>,
        /// Why nothing moved, when something refused.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refusal: Option<String>,
        /// Messages sent — none when `claude/notify` has no consumer group.
        sent: usize,
    },
    /// A `notify.open_sessions`.
    Sessions {
        /// By session.
        sessions: Vec<NotifySession>,
    },
    /// A `kb.ambient`.
    Ambient {
        /// The mount's namespace, or `None` outside every mount.
        namespace: Option<String>,
    },
    /// A `kb.query` or `task.ready` listing.
    Items {
        /// In the query's order.
        items: Vec<kb::ItemRow>,
        /// Cut short at the read's budget ([`kb::Budget`]).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// A `kb.query` with `count`.
    Count {
        /// How many matched.
        count: usize,
    },
    /// A `kb.ls`.
    Listing {
        /// Depth-first.
        rows: Vec<kb::ListRow>,
        /// Cut short at the read's budget ([`kb::Budget`]).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// A `kb.tree`.
    Tree {
        /// The top level.
        nodes: Vec<kb::TreeNode>,
        /// Cut short, at the read's budget ([`kb::Budget`]) or the node cap.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        /// Cut at [`kb::MAX_TREE_NODES`] — a cut no backend lifts, unlike the budget's.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        at_node_cap: bool,
    },
    /// A `task.subtasks`.
    Children {
        /// In containment order.
        children: Vec<kb::Child>,
        /// Cut short at the read's budget ([`kb::Budget`]).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// A `kb.cat`.
    Content {
        /// The text; empty when the item has none.
        content: String,
    },
    /// A `kb.grep`.
    GrepHits {
        /// The answer.
        #[serde(flatten)]
        answer: kb::GrepAnswer,
    },
    /// A `kb.search`.
    SearchHits {
        /// Best first.
        hits: Vec<kb::SearchHit>,
        /// Cut short at the read's budget ([`kb::Budget`]).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// A `task.show`.
    Task {
        /// The task.
        task: Box<kb::TaskDetail>,
        /// Its subtasks were cut short at the read's budget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
}

impl Response {
    /// Whether this answer was cut short at its read's budget ([`kb::Budget`]) or, for a tree, its
    /// node cap — asked once by a client rather than remembered in each place it takes one apart.
    /// Exhaustive, so a new answer that can be cut must say.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        match self {
            Self::Items { truncated, .. }
            | Self::Listing { truncated, .. }
            | Self::Tree { truncated, .. }
            | Self::Children { truncated, .. }
            | Self::SearchHits { truncated, .. }
            | Self::Task { truncated, .. } => *truncated,
            Self::GrepHits { answer } => answer.truncated,
            Self::Created { .. }
            | Self::Sent { .. }
            | Self::Messages { .. }
            | Self::Position { .. }
            | Self::Compacted { .. }
            | Self::Topics { .. }
            | Self::Notified { .. }
            | Self::Sessions { .. }
            | Self::Ambient { .. }
            | Self::Count { .. }
            | Self::Content { .. } => false,
        }
    }

    /// Whether this answer reports a message put on a topic, so a daemon holding long-polls wakes
    /// them. Asked of what was DONE, not of the request's type: most `notify.event`s — every tool
    /// call from every session with nothing on screen — send nothing, and waking every subscriber for
    /// each made them all re-poll for no message. Exhaustive, so a new answer that can carry a send
    /// must say so; one the daemon did not announce is delivered only by its slower `data_version`
    /// floor.
    #[must_use]
    pub const fn announces_a_send(&self) -> bool {
        match self {
            Self::Sent { .. } => true,
            Self::Notified { sent, .. } => *sent > 0,
            Self::Created { .. }
            | Self::Messages { .. }
            | Self::Position { .. }
            | Self::Compacted { .. }
            | Self::Topics { .. }
            | Self::Sessions { .. }
            | Self::Ambient { .. }
            | Self::Items { .. }
            | Self::Count { .. }
            | Self::Listing { .. }
            | Self::Tree { .. }
            | Self::Children { .. }
            | Self::Content { .. }
            | Self::GrepHits { .. }
            | Self::SearchHits { .. }
            | Self::Task { .. } => false,
        }
    }
}

impl From<notify::Applied> for Response {
    fn from(a: notify::Applied) -> Self {
        Self::Notified {
            state: a.state.as_str().to_owned(),
            moved: a.moved,
            effects: a.effects.iter().map(|e| e.as_str().to_owned()).collect(),
            refusal: a.refusal,
            sent: a.sent,
        }
    }
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
    /// The database was locked by another writer for longer than the busy timeout, or the daemon is
    /// at its concurrency limit. Transient: retry.
    Busy,
    /// Served over HTTP without a valid bearer token (`jkb serve`).
    Unauthorized,
    /// The database has been migrated past what the serving `jkb` knows; rebuild the host's `jkb`.
    SchemaNewer,
    /// The daemon could not be reached (client-side: refused, timed out, or recently unreachable).
    Unavailable,
    /// The thing a read names does not exist (an item uid, say).
    NotFound,
    /// A request this backend does not serve in this form (a search route that embeds text, on a
    /// backend with no embedder).
    Unsupported,
    /// Anything else: a database or internal failure.
    Internal,
    /// A code this build does not know, from a newer peer. Clients treat it like `internal`.
    #[serde(other)]
    Unknown,
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

    /// An error with this code and message.
    #[must_use]
    pub fn with_code(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(code, message)
    }
}

impl From<jkb_core::Error> for ApiError {
    fn from(e: jkb_core::Error) -> Self {
        let message = e.to_string();
        let code = match &e {
            // Exhaustive on purpose (QueueError is not `#[non_exhaustive]`): a new refusal must
            // choose its wire code here rather than reach clients as `internal`.
            jkb_core::Error::Queue(q) => match q {
                QueueError::NoSuchTopic(_) => ErrorCode::NoSuchTopic,
                QueueError::TopicConflict(_) => ErrorCode::TopicConflict,
                QueueError::NoSuchGroup { .. } => ErrorCode::NoSuchGroup,
                QueueError::QueueFull { .. } => ErrorCode::QueueFull,
                QueueError::TooLarge { .. } => ErrorCode::TooLarge,
                QueueError::Invalid { .. } => ErrorCode::Invalid,
                QueueError::AckBeyondEnd { .. } => ErrorCode::AckBeyondEnd,
                QueueError::Corrupt(_) => ErrorCode::Internal,
                QueueError::CorruptPayload { seq, .. } => {
                    return Self {
                        code: ErrorCode::CorruptPayload,
                        message,
                        seq: Some(*seq),
                    }
                }
            },
            jkb_core::Error::SchemaNewer { .. } => ErrorCode::SchemaNewer,
            jkb_core::Error::Types(jkb_types::Error::Validation(_)) => ErrorCode::Invalid,
            jkb_core::Error::Types(jkb_types::Error::NotFound(_)) => ErrorCode::NotFound,
            jkb_core::Error::Sqlite(rusqlite::Error::SqliteFailure(f, _))
                if matches!(
                    f.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                ErrorCode::Busy
            }
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

    /// Whether a `schema_newer` refusal from this backend can clear while the caller keeps running.
    /// A remote daemon is restarted on the newer jkb by `setup.sh`, so it can; in-process it cannot —
    /// this process's own code is what is too old, and only exiting lets its supervisor start the
    /// newer binary.
    fn schema_newer_clears(&self) -> bool {
        false
    }
}

/// Serves requests against a [`Db`] in this process.
#[derive(Clone)]
pub struct LocalBackend {
    db: Db,
    /// Where the read set (`kb.*`, `task.ready`/`show`/`subtasks`) is served: `db` unless
    /// [`LocalBackend::with_reader`] gave it a connection of its own.
    reads: Db,
    /// What each read may answer with ([`kb::Budget`]); unlimited unless
    /// [`LocalBackend::with_read_budget`] set one.
    budget: kb::Budget,
    embedder: Option<Arc<dyn Embedder + Send + Sync>>,
}

impl LocalBackend {
    /// A backend over `db`, with no embedder: `kb.search` serves only the FTS route. What `jkb serve`
    /// runs, so a client's search never makes the host call a model.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self {
            reads: db.clone(),
            db,
            budget: kb::Budget::UNLIMITED,
            embedder: None,
        }
    }

    /// The same backend, bounding every read's answer to about `bytes` of JSON, cut short and marked
    /// `truncated` past it. What `jkb serve` sets: a client's request, not the CLI, decides what is
    /// asked.
    #[must_use]
    pub const fn with_read_budget(mut self, bytes: usize) -> Self {
        self.budget = kb::Budget::new(bytes);
        self
    }

    /// The same backend, serving the read set on `reader` — a [`Db::reader`] of the same database.
    /// `Db` runs every call on one thread, so without this a long read (a wide grep, a deep tree) held
    /// up every write behind it, the notification hook's 1 s round trip among them. The reader is
    /// `query_only`, so a read op cannot write through it either.
    #[must_use]
    pub fn with_reader(mut self, reader: Db) -> Self {
        self.reads = reader;
        self
    }

    /// The handle the read set is served on — `db` itself unless [`LocalBackend::with_reader`] was
    /// given one.
    #[must_use]
    pub const fn reads(&self) -> &Db {
        &self.reads
    }

    /// The same backend, embedding search text with `embedder` for the vector and hybrid routes.
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder + Send + Sync>) -> Self {
        self.embedder = Some(embedder);
        self
    }
}

const ACTOR: &str = "jkb-api";

impl Backend for LocalBackend {
    #[allow(clippy::too_many_lines)] // a flat op dispatcher: one arm per op, as in the CLI's `run`
    fn call(&self, request: Request) -> Result<Response, ApiError> {
        let now = mq::now_ms();
        // Chosen here, once, from the op's class — no arm picks a connection or a budget.
        let db = if request.is_agent_read() {
            &self.reads
        } else {
            &self.db
        };
        let mut budget = self.budget;
        let created = |c: Created| Response::Created {
            created: c == Created::New,
        };
        Ok(match request {
            Request::MqTopicCreate { topic, spec } => {
                let spec = spec.resolve();
                created(db.write_txn(ACTOR, move |c, m| {
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
                    seq: db.write_txn(ACTOR, move |c, m| mq::send(c, m, &topic, &draft, now))?,
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
                created(db.write_txn(ACTOR, move |c, m| {
                    mq::group_create(c, m, &topic, &group, start, now)
                })?)
            }
            Request::MqPoll {
                topic,
                group,
                max,
                after,
            } => {
                // Asked with a read first: an idle subscriber polls several times a second, and a
                // poll that hands nothing over and has no touch due must not take the write lock.
                let (t, g) = (topic.clone(), group.clone());
                if !db.read(move |c| mq::poll_needed(c, &t, &g, max, after, now))? {
                    return Ok(Response::Messages {
                        messages: Vec::new(),
                    });
                }
                Response::Messages {
                    messages: db
                        .write_txn(ACTOR, move |c, m| {
                            mq::poll(c, m, &topic, &group, max, after, now)
                        })?
                        .into_iter()
                        .map(Message::from)
                        .collect(),
                }
            }
            Request::MqAck { topic, group, seq } => Response::Position {
                position: db
                    .write_txn(ACTOR, move |c, m| mq::ack(c, m, &topic, &group, seq, now))?,
            },
            Request::MqCompact { force } => {
                let r = db.write_txn(ACTOR, move |c, m| mq::compact(c, m, now, force))?;
                Response::Compacted {
                    topics_compacted: r.topics_compacted,
                    topics_skipped: r.topics_skipped,
                    messages_reaped: r.messages_reaped,
                    groups_removed: r.groups_removed,
                }
            }
            Request::MqInspect {} => Response::Topics {
                topics: db.read(mq::inspect)?.into_iter().map(Topic::from).collect(),
            },
            Request::MqTail { topic, limit } => Response::Messages {
                messages: db
                    .read(move |c| mq::tail(c, &topic, limit, now))?
                    .into_iter()
                    .map(Message::from)
                    .collect(),
            },
            Request::NotifyEvent {
                session,
                event,
                tool,
                message,
                cwd,
                owner,
                instance,
            } => {
                let obs = Observation {
                    session,
                    event: event.into(),
                    finished_tool: tool,
                    message,
                    cwd,
                    owner,
                    instance,
                };
                db.write_txn(ACTOR, move |c, m| notify::observe(c, m, &obs, now))?
                    .into()
            }
            Request::NotifyOpenSessions {} => Response::Sessions {
                sessions: db
                    .read(notify::open_sessions)?
                    .into_iter()
                    .map(|r| NotifySession {
                        session: r.session,
                        tool: r.tool,
                        owner: r.owner,
                        instance: r.instance,
                        updated_at: r.updated_at,
                    })
                    .collect(),
            },
            Request::NotifyGone {
                session,
                owner,
                instance,
            } => db
                .write_txn(ACTOR, move |c, m| {
                    notify::gone(c, m, &session, &owner, &instance, now)
                })?
                .into(),
            Request::KbAmbient { cwd, home } => {
                let server_home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                Response::Ambient {
                    namespace: db
                        .read(move |c| kb::ambient(c, &cwd, &home, server_home.as_deref()))?,
                }
            }
            Request::KbQuery {
                dsl,
                default_scope,
                limit,
                count,
                order,
            } => {
                if count {
                    Response::Count {
                        count: db
                            .read(move |c| kb::query_count(c, &dsl, default_scope.as_deref()))?,
                    }
                } else {
                    let (items, truncated) = db.read(move |c| {
                        let items = kb::query_items(
                            c,
                            &dsl,
                            default_scope.as_deref(),
                            limit,
                            order,
                            &mut budget,
                        )?;
                        Ok((items, budget.exhausted()))
                    })?;
                    Response::Items { items, truncated }
                }
            }
            Request::KbLs {
                path,
                all,
                recursive,
            } => {
                let (rows, truncated) = db.read(move |c| {
                    let rows = kb::ls(c, path.as_deref(), all, recursive, &mut budget)?;
                    Ok((rows, budget.exhausted()))
                })?;
                Response::Listing { rows, truncated }
            }
            Request::KbTree { path, all, depth } => {
                let (nodes, cut) =
                    db.read(move |c| kb::tree(c, path.as_deref(), all, depth, &mut budget))?;
                Response::Tree {
                    nodes,
                    truncated: cut != kb::TreeCut::Whole,
                    at_node_cap: cut == kb::TreeCut::NodeCap,
                }
            }
            Request::KbCat { uid } => Response::Content {
                content: db.read_with(move |c| kb::cat(c, &uid))?,
            },
            Request::KbGrep {
                pattern,
                scope,
                ignore_case,
                mode,
            } => Response::GrepHits {
                answer: db.read_with(move |c| {
                    kb::grep(
                        c,
                        &pattern,
                        scope.as_deref(),
                        ignore_case,
                        mode,
                        &mut budget,
                    )
                })?,
            },
            Request::KbSearch {
                dsl,
                default_scope,
                route,
                limit,
                context,
            } => {
                let hits = kb::search(
                    db,
                    self.embedder.as_ref(),
                    &kb::SearchAsk {
                        dsl,
                        default_scope,
                        route,
                        limit,
                        context,
                    },
                    &mut budget,
                )?;
                Response::SearchHits {
                    hits,
                    truncated: budget.exhausted(),
                }
            }
            Request::TaskReady {
                dsl,
                default_scope,
                limit,
            } => {
                let (items, truncated) = db.read(move |c| {
                    let items = kb::ready(c, &dsl, default_scope.as_deref(), limit, &mut budget)?;
                    Ok((items, budget.exhausted()))
                })?;
                Response::Items { items, truncated }
            }
            Request::TaskShow { uid } => {
                let (task, truncated) = db.read_with(move |c| {
                    let task = kb::task_show(c, &uid, &mut budget)?;
                    Ok::<_, ApiError>((task, budget.exhausted()))
                })?;
                Response::Task {
                    task: Box::new(task),
                    truncated,
                }
            }
            Request::TaskSubtasks { uid, all } => {
                let (children, truncated) = db.read_with(move |c| {
                    let children = kb::subtasks(c, &uid, all, &mut budget)?;
                    Ok::<_, ApiError>((children, budget.exhausted()))
                })?;
                Response::Children {
                    children,
                    truncated,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
