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
//! 6.1 the agent read set (`kb.*`, `task.ready`/`show`/`subtasks`, in [`kb`]), 6.2 the task-mutate
//! set, 6.3 `ingest.text`, and 6.4 the Claude Code session registry (`session.*`), fed by the hook.

use std::sync::Arc;

use jkb_core::claude_session;
use jkb_core::mq::{self, Created, Draft, QueueError, Start, TopicSpec};
use jkb_core::notify::{self, NotifEvent, Observation};
use jkb_core::Db;
use jkb_types::Embedder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod claims;
pub mod designs;
pub mod health;
pub mod ingest;
pub mod inv;
pub mod items;
pub mod kb;
pub mod namespaces;
pub mod prs;
pub mod rbac;
pub mod removals;
pub mod review;
pub mod sessions;
pub mod staging;
pub mod tasks;
pub mod workflows;

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
    /// machine against its record of the session and sends the effects on `claude/notify` — and,
    /// for every event but `session_ended`, records the process as running in the session registry
    /// ([`jkb_core::claude_session::seen`]), which repairs a `session.started` that was lost.
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
        /// The `claude` process's pid, or empty when the hook had none it could trust. A pid is refused
        /// without its `instance` (`jkb_core::notify::check_owner_and_instance`).
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
    /// A Claude Code session started, resumed, was cleared into or compacted
    /// ([`jkb_core::claude_session`]): this process holds it.
    #[serde(rename = "session.started")]
    SessionStarted {
        /// The session id, sanitized to `[A-Za-z0-9_-]` (anything else is refused).
        session: String,
        /// The payload's `source` (`startup`, `resume`, `clear`, `compact`, …).
        source: String,
        /// The `claude` process's pid, or empty when the hook had none it could trust. Refused without
        /// its `instance`.
        #[serde(default)]
        pid: String,
        /// Where `pid` means something: `host[#boot][/pidns]`.
        #[serde(default)]
        instance: String,
        /// The session's working directory.
        #[serde(default)]
        cwd: String,
    },
    /// A Claude Code session ended, as its own process reported: that process's hold ends. The session
    /// stays live while another process holds it.
    #[serde(rename = "session.ended")]
    SessionEnded {
        /// The session id.
        session: String,
        /// The payload's `reason` (`prompt_input_exit`, `clear`, `resume`, `other`, …).
        reason: String,
        /// The reporting `claude` process's pid, or empty.
        #[serde(default)]
        pid: String,
        /// Where `pid` means something.
        #[serde(default)]
        instance: String,
    },
    /// A producer proved a process gone: end its hold on the session, if still live.
    #[serde(rename = "session.gone")]
    SessionGone {
        /// The session id.
        session: String,
        /// The pid the producer probed, as `session.list` reported it.
        pid: String,
        /// Its instance, as `session.list` reported it.
        instance: String,
    },
    /// The session registry, one row per process holding a session: the live rows, least recently
    /// seen first (what a sweep probes), or every one — a page at a time.
    #[serde(rename = "session.list")]
    SessionList {
        /// Include ended rows, most recently seen first.
        #[serde(default)]
        all: bool,
        /// Continue after this: the `next` of the previous page, sent back as it came, with the same
        /// `all`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
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
    /// A task's whole transition history.
    #[serde(rename = "task.why")]
    TaskWhy {
        /// A task uid or bare slug.
        uid: String,
    },
    /// Create a task from a quick-add line.
    #[serde(rename = "task.add")]
    TaskAdd(tasks::AddAsk),
    /// Set a task's status, priority or due date.
    #[serde(rename = "task.set")]
    TaskSet {
        /// A task uid or bare slug.
        uid: String,
        /// `open`/`in_progress`/`needs_review`/`done`/`cancelled`.
        #[serde(default)]
        status: Option<String>,
        /// The priority.
        #[serde(default)]
        priority: Option<i64>,
        /// The due date.
        #[serde(default)]
        due: Option<String>,
    },
    /// Replace or append to a task's body.
    #[serde(rename = "task.edit")]
    TaskEdit {
        /// A task uid or bare slug.
        uid: String,
        /// The text.
        text: String,
        /// Append rather than replace.
        #[serde(default)]
        append: bool,
    },
    /// Add, set or remove a `facet=value` tag.
    #[serde(rename = "task.tag")]
    TaskTag {
        /// A task uid or bare slug.
        uid: String,
        /// `facet=value`.
        facet_value: String,
        /// Add, set or remove.
        mode: tasks::TagMode,
    },
    /// Make a task depend on another.
    #[serde(rename = "task.depend")]
    TaskDepend {
        /// The dependent task.
        uid: String,
        /// What it depends on.
        dep: String,
    },
    /// Remove a dependency.
    #[serde(rename = "task.undepend")]
    TaskUndepend {
        /// The dependent task.
        uid: String,
        /// What it depended on.
        dep: String,
    },
    /// Place a task under a namespace.
    #[serde(rename = "task.place")]
    TaskPlace {
        /// A task uid or bare slug.
        uid: String,
        /// The namespace.
        ns: String,
        /// As its primary home rather than a mirror.
        #[serde(default)]
        home: bool,
    },
    /// Remove a task's mirror placement.
    #[serde(rename = "task.unplace")]
    TaskUnplace {
        /// A task uid or bare slug.
        uid: String,
        /// The namespace.
        ns: String,
    },
    /// Bind a task to `managed:` storage, or to a file.
    #[serde(rename = "task.bind")]
    TaskBind {
        /// A task uid or bare slug.
        uid: String,
        /// The file uri; `managed:` when absent.
        #[serde(default)]
        sync: Option<String>,
    },
    /// Claim a task for an owner.
    #[serde(rename = "task.claim")]
    TaskClaim {
        /// A task uid or bare slug.
        uid: String,
        /// The owner id.
        owner: String,
    },
    /// Release an owner's claim.
    #[serde(rename = "task.release")]
    TaskRelease {
        /// A task uid or bare slug.
        uid: String,
        /// The owner id.
        owner: String,
    },
    /// Store a document whose text the client extracted, and chunk it.
    #[serde(rename = "ingest.text")]
    IngestText(ingest::IngestAsk),
    /// What the session verbs read about a task ([`sessions::facts`]).
    #[serde(rename = "task.facts")]
    TaskFacts {
        /// The task (a uid or bare slug).
        uid: String,
    },
    /// A repo's tasks by every branch they record ([`sessions::by_branch`]).
    #[serde(rename = "task.by_branch")]
    TaskByBranch {
        /// The repo key.
        repo: String,
    },
    /// `jkb task start`'s write: take the claim, record where the work is, note it
    /// ([`sessions::start`]).
    #[serde(rename = "task.start")]
    TaskStart(sessions::StartAsk),
    /// `jkb task work`'s claim ([`sessions::take`]).
    #[serde(rename = "task.take")]
    TaskTake(sessions::TakeAsk),
    /// Record where a claim holder's work is ([`sessions::locate`]).
    #[serde(rename = "task.locate")]
    TaskLocate {
        /// The task.
        uid: String,
        /// The owner that must hold the claim.
        owner: String,
        /// Where.
        place: sessions::Place,
    },
    /// `jkb task abandon`'s write: release the judged claim and reopen ([`sessions::abandon`]).
    #[serde(rename = "task.abandon")]
    TaskAbandon {
        /// The task.
        uid: String,
        /// The claim the caller read before its git work, or `None` for none.
        #[serde(default)]
        observed: Option<String>,
    },
    /// The gate command stored for a repo — read-only ([`sessions::gate`]).
    #[serde(rename = "repo.gate")]
    RepoGate {
        /// The repo key.
        repo: String,
    },
    /// Whether a Claude Code session is live, ended or unknown to the registry.
    #[serde(rename = "session.state")]
    SessionState {
        /// The session id.
        session: String,
    },
    /// Record a worktree disposal for the reap service ([`removals::add`]).
    #[serde(rename = "removal.add")]
    RemovalAdd {
        /// The record.
        removal: removals::Removal,
    },
    /// The worktree-removal records, a page at a time ([`removals::list`]).
    #[serde(rename = "removal.list")]
    RemovalList {
        /// Continue after this: the `next` of the previous page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<i64>,
    },
    /// A pending record's tree was archived ([`removals::archived`]).
    #[serde(rename = "removal.archived")]
    RemovalArchived {
        /// The record.
        id: i64,
        /// Where the tree went.
        archive: String,
        /// When (Unix seconds).
        at: i64,
    },
    /// Cancel pending records unless a sweep is in flight ([`removals::cancel`]).
    #[serde(rename = "removal.cancel")]
    RemovalCancel {
        /// The records.
        ids: Vec<i64>,
    },
    /// Forget a record ([`removals::drop_record`]).
    #[serde(rename = "removal.drop")]
    RemovalDrop {
        /// The record.
        id: i64,
    },
    /// Who holds a lease ([`removals::lease_get`]).
    #[serde(rename = "lease.get")]
    LeaseGet {
        /// The lease.
        name: String,
    },
    /// Take a lease, compare-and-set ([`removals::lease_take`]).
    #[serde(rename = "lease.take")]
    LeaseTake {
        /// The lease.
        name: String,
        /// `<owner id> <nonce>`.
        holder: String,
        /// The holder the caller judged gone, replaced only while it still holds the lease.
        #[serde(default)]
        displace: Option<String>,
    },
    /// Release a lease the caller holds ([`removals::lease_release`]).
    #[serde(rename = "lease.release")]
    LeaseRelease {
        /// The lease.
        name: String,
        /// The holder, exactly as taken.
        holder: String,
    },
    /// Drop a lease whoever holds it — host only ([`removals::lease_break`]).
    #[serde(rename = "lease.break")]
    LeaseBreak {
        /// The lease.
        name: String,
    },
    /// `jkb task land`'s record ([`sessions::land`]).
    #[serde(rename = "task.land")]
    TaskLand {
        /// The task.
        uid: String,
        /// Where it landed.
        landed: sessions::Landed,
    },
    /// Whether `task.land` with exactly this payload would be admitted — asked by `jkb task land`
    /// before it grafts or runs the gate, and answered by the same [`rbac::authorize`] as the record
    /// itself, so there is one rule rather than a copy of it. Records nothing.
    ///
    /// It exists because the Land permission used to be asked only by `task.land`, at the very end:
    /// a caller who may not land got its branch grafted onto the target and its gate run, and was
    /// refused only the record — landed in git, not in jkb.
    #[serde(rename = "task.land_check")]
    TaskLandCheck {
        /// The task.
        uid: String,
        /// Where it would land — carried so a merge queue re-landing the same branch is admitted
        /// here exactly as `task.land` admits it.
        landed: sessions::Landed,
    },
    /// A landing the merge queue performed, for one task ([`sessions::landed`]).
    #[serde(rename = "task.landed")]
    TaskLanded {
        /// The task.
        uid: String,
        /// Where it landed.
        landed: sessions::Landed,
    },
    /// The findings of a task's recorded reviews ([`sessions::review_findings`]).
    #[serde(rename = "task.review_findings")]
    TaskReviewFindings {
        /// The review namespaces.
        namespaces: Vec<String>,
    },
    /// File a review's findings as tasks ([`review::file`]).
    #[serde(rename = "task.review_file")]
    TaskReviewFile(review::FileAsk),
    /// Record a review against a branch ([`review::record`]).
    #[serde(rename = "task.review_record")]
    TaskReviewRecord(review::RecordAsk),
    /// Record that a landing waived the review gate (`jkb task land --no-review`) — in the task's
    /// review record, never a tag ([`jkb_core::reviews::waive`]).
    #[serde(rename = "task.review_waive")]
    TaskReviewWaive {
        /// The task.
        uid: String,
        /// The head the waived landing grafted.
        sha: String,
    },
    /// Record that a landing ran its gate on the host rather than in the dev container
    /// (`jkb task land --gate-on-host`; [`jkb_core::reviews::gate_on_host`]). Operator only.
    #[serde(rename = "task.ran_on_host")]
    TaskRanOnHost {
        /// The task.
        uid: String,
        /// The head whose gate ran on the host.
        sha: String,
    },
    /// Mint a role grant ([`rbac::grant`]); its token is answered once, and only its hash kept.
    #[serde(rename = "role.grant")]
    RoleGrant {
        /// The role.
        role: String,
        /// The task it is scoped to.
        #[serde(default)]
        task: Option<String>,
        /// Who it is handed to.
        agent: String,
    },
    /// Revoke a grant and everything it minted ([`rbac::revoke`]).
    #[serde(rename = "role.revoke")]
    RoleRevoke {
        /// The grant.
        id: i64,
    },
    /// The grants and the agent-type map ([`rbac::list`]).
    #[serde(rename = "role.list")]
    RoleList {
        /// Only those scoped to this task.
        #[serde(default)]
        task: Option<String>,
        /// Revoked ones too.
        #[serde(default)]
        all: bool,
    },
    /// Who the caller is ([`rbac::whoami`]).
    #[serde(rename = "role.whoami")]
    RoleWhoami {},
    /// Bind an attested subagent to the task it works on — first bind wins, and it cannot hop
    /// ([`rbac::authorize`] does the binding; the op itself does nothing more).
    #[serde(rename = "role.bind")]
    RoleBind {
        /// The task.
        uid: String,
    },
    /// Map a Claude Code agent type to a role, or clear it (operator; [`rbac::map`]).
    #[serde(rename = "role.map")]
    RoleMap {
        /// The agent definition's `name`.
        agent_type: String,
        /// The role; none clears the mapping.
        #[serde(default)]
        role: Option<String>,
    },
    /// Replace the dev container's credential (operator; [`jkb_core::roles::rotate_container`]).
    #[serde(rename = "role.rotate_container")]
    RoleRotateContainer {
        /// The credential written now: kept, and answered back, when it still names the live
        /// container grant — so a re-run of setup does not revoke every worker minted from it.
        #[serde(default)]
        keep: Option<String>,
    },
    /// A task's workflow: phase, strategy, who acts next ([`rbac::show`]).
    #[serde(rename = "workflow.show")]
    WorkflowShow {
        /// The task.
        uid: String,
    },
    /// Fire a workflow event ([`rbac::fire`]).
    #[serde(rename = "workflow.fire")]
    WorkflowFire {
        /// The task.
        uid: String,
        /// The event.
        event: String,
        /// Why — required by the events that decide who sees the work next.
        #[serde(default)]
        reason: Option<String>,
        /// The phase an `override` names.
        #[serde(default)]
        to: Option<String>,
    },
    /// Take the one reconciliation that applies ([`rbac::observe`]).
    #[serde(rename = "workflow.observe")]
    WorkflowObserve {
        /// The task.
        uid: String,
    },
    /// Pin a task to a strategy (operator; [`jkb_core::workflow::store::set_strategy`]).
    #[serde(rename = "workflow.set")]
    WorkflowSet {
        /// The task.
        uid: String,
        /// A preset or definition name.
        strategy: String,
    },
    /// Define or redefine a strategy (operator; [`rbac::define`]).
    #[serde(rename = "workflow.define")]
    WorkflowDefine {
        /// Its name.
        name: String,
        /// Its spec, as JSON.
        spec: Value,
    },
    /// The presets and definitions ([`rbac::strategies`]).
    #[serde(rename = "workflow.strategies")]
    WorkflowStrategies {},
    /// A strategy's workflow machine and the task lifecycle machine, as data
    /// ([`workflows::graph`]): a task's own strategy, a named one, or the default.
    #[serde(rename = "workflow.graph")]
    WorkflowGraph {
        /// The task whose strategy to draw.
        #[serde(default)]
        uid: Option<String>,
        /// The strategy to draw.
        #[serde(default)]
        strategy: Option<String>,
    },
    /// Every agent template, each as the one in effect ([`workflows::list`]).
    #[serde(rename = "workflow.agents")]
    WorkflowAgents {},
    /// One agent template, optionally filled in ([`workflows::show`]).
    #[serde(rename = "workflow.agent")]
    WorkflowAgent {
        /// Its name.
        name: String,
        /// The packaged template, whatever overrides it.
        #[serde(default)]
        packaged: bool,
        /// This version of the operator copy.
        #[serde(default)]
        version: Option<i64>,
        /// Fill its placeholders with these values: every one, and nothing else.
        #[serde(default)]
        vars: Option<std::collections::BTreeMap<String, String>>,
    },
    /// Copy a template into an operator copy (operator; [`workflows::copy`]).
    #[serde(rename = "workflow.agent_copy")]
    WorkflowAgentCopy {
        /// The template to copy.
        from: String,
        /// Copy the packaged text even when a copy overrides it.
        #[serde(default)]
        packaged: bool,
        /// The copy's name; by default `from`, which then overrides the packaged template.
        #[serde(default, rename = "as")]
        as_name: Option<String>,
    },
    /// Edit an operator copy (operator; [`workflows::set`]).
    #[serde(rename = "workflow.agent_set")]
    WorkflowAgentSet {
        /// The copy.
        name: String,
        /// What changes.
        edit: workflows::AgentEdit,
    },
    /// The designs, or one repo's ([`designs::list`]).
    #[serde(rename = "design.list")]
    DesignList {
        /// Only this repo's (`designs/<repo>`).
        #[serde(default)]
        repo: Option<String>,
    },
    /// Create a design under `designs/<repo>` ([`designs::create`]).
    #[serde(rename = "design.create")]
    DesignCreate {
        /// The repo it designs.
        repo: String,
        /// Its title.
        title: String,
        /// Its first text.
        #[serde(default)]
        body: String,
    },
    /// A design's text, spans and version token ([`designs::cat`]).
    #[serde(rename = "design.cat")]
    DesignCat {
        /// The design.
        uid: String,
    },
    /// A design rendered for its doc target, or every design with one ([`designs::export`]).
    #[serde(rename = "design.export")]
    DesignExport {
        /// The design; omitted for every design that has a doc target.
        #[serde(default)]
        uid: Option<String>,
        /// With no `uid`: only this repo's (`designs/<repo>`).
        #[serde(default)]
        repo: Option<String>,
    },
    /// Record where a design's export is written ([`designs::set_target`]).
    #[serde(rename = "design.target")]
    DesignTarget {
        /// The design.
        uid: String,
        /// Relative to the repository root, under `docs/`.
        path: String,
    },
    /// Record files a design was made from ([`designs::add_sources`]).
    #[serde(rename = "design.sources")]
    DesignSources {
        /// The design.
        uid: String,
        /// Each path, with the blake3 of its content now.
        sources: Vec<designs::Source>,
    },
    /// What a peer lacks, as one Yjs update ([`designs::state`]).
    #[serde(rename = "design.state")]
    DesignState {
        /// The design.
        uid: String,
        /// The peer's state vector, base64; omitted for the whole document.
        #[serde(default)]
        since: Option<String>,
    },
    /// A design's spans and their derived states ([`designs::spans`]).
    #[serde(rename = "design.spans")]
    DesignSpans {
        /// The design.
        uid: String,
    },
    /// Merge an editor's Yjs v1 update ([`designs::apply`]).
    #[serde(rename = "design.apply")]
    DesignApply {
        /// The design.
        uid: String,
        /// The update, base64.
        update: String,
    },
    /// Edit by quote, resolved against the version read ([`designs::edit`]).
    #[serde(rename = "design.edit")]
    DesignEdit {
        /// The design.
        uid: String,
        /// The version token the quote was read at.
        base: String,
        /// What to do.
        edit: designs::EditAsk,
    },
    /// Make a span over a quote, resolved against the version read ([`designs::span`]).
    #[serde(rename = "design.span")]
    DesignSpan {
        /// The design.
        uid: String,
        /// The version token the quote was read at.
        base: String,
        /// The quote.
        find: String,
        /// Which match, from 1.
        #[serde(default)]
        occurrence: Option<usize>,
        /// Who approves it: `operator` (the default) or `claude`.
        #[serde(default)]
        reviewer: Option<String>,
    },
    /// Approve a span, as the reviewer it names ([`designs::approve`]).
    #[serde(rename = "design.approve")]
    DesignApprove {
        /// The span.
        span: String,
    },
    /// Stage an approved span into a plan step ([`designs::stage`]).
    #[serde(rename = "design.stage")]
    DesignStage {
        /// The span.
        span: String,
        /// The plan step.
        step: String,
    },
    /// Create an execution plan under a design, with its steps in order
    /// ([`designs::plans::create`]).
    #[serde(rename = "design.plan_create")]
    DesignPlanCreate {
        /// The design.
        uid: String,
        /// The plan's title.
        title: String,
        /// Its steps, in order.
        #[serde(default)]
        steps: Vec<String>,
    },
    /// Append a step to an execution plan ([`designs::plans::add_step`]).
    #[serde(rename = "design.plan_step")]
    DesignPlanStep {
        /// The plan.
        plan: String,
        /// What the step is.
        text: String,
    },
    /// One execution plan: its steps, their tasks and staged spans ([`designs::plans::show`]).
    #[serde(rename = "design.plan")]
    DesignPlan {
        /// The plan.
        plan: String,
    },
    /// A design's execution plans and one-off tasks ([`designs::plans::list`]).
    #[serde(rename = "design.plans")]
    DesignPlans {
        /// The design.
        uid: String,
        /// Include archived plans (every task terminal).
        #[serde(default)]
        all: bool,
    },
    /// A prompt for a Claude session over a design: *Discuss* a selection, or *Play* a plan or a
    /// task ([`designs::prompt`]).
    #[serde(rename = "design.prompt")]
    DesignPrompt {
        /// Which prompt, over what.
        ask: designs::PromptAsk,
    },
    /// Record the Claude Code session a launch is about to start on a design, before it starts
    /// ([`designs::prompts::record`]).
    #[serde(rename = "design.prompt_record")]
    DesignPromptRecord {
        /// The design.
        uid: String,
        /// The pre-minted session uuid (`claude --session-id`).
        session: String,
        /// The absolute directory the session starts in, where it is resumed.
        cwd: String,
        /// What started it: `discuss`, `play`, `task` or `new`.
        launch: String,
        /// The plan or task it is started on.
        #[serde(default)]
        subject: Option<String>,
        /// Its title.
        title: String,
    },
    /// A design's recorded prompts, newest first ([`designs::prompts::list`]).
    #[serde(rename = "design.prompts")]
    DesignPrompts {
        /// The design.
        uid: String,
    },
    /// The prompt a Claude Code session was recorded with, if any — the link from a session back to
    /// the design it worked ([`designs::prompts::of_session`], D53.9).
    #[serde(rename = "design.prompt_of")]
    DesignPromptOf {
        /// The session id.
        session: String,
    },
    /// Fold a design's updates into its snapshot (operator; [`jkb_core::design::compact`]).
    #[serde(rename = "design.compact")]
    DesignCompact {
        /// The design.
        uid: String,
    },
    /// Mint a harness attestation ticket for one tool call (the container credential only;
    /// [`rbac::mint_ticket`]).
    #[serde(rename = "attest.mint")]
    AttestMint {
        /// The Claude Code session.
        session: String,
        /// The subagent, when one made the call.
        #[serde(default)]
        agent_id: Option<String>,
        /// Its type.
        #[serde(default)]
        agent_type: Option<String>,
        /// The tool call.
        tool_use_id: String,
    },
    /// Release tickets: one tool call's, one subagent's, or a whole session's ([`rbac::Tickets::release`]).
    #[serde(rename = "attest.release")]
    AttestRelease {
        /// The session.
        session: String,
        /// Only this subagent's.
        #[serde(default)]
        agent_id: Option<String>,
        /// Only this tool call's.
        #[serde(default)]
        tool_use_id: Option<String>,
    },
    /// A page of the held task claims ([`claims::claims`]).
    #[serde(rename = "task.claims")]
    TaskClaims {
        /// The previous page's `next`.
        #[serde(default)]
        after: Option<i64>,
    },
    /// Free the claims of owners the client proved gone ([`claims::reclaim`]).
    #[serde(rename = "task.reclaim")]
    TaskReclaim {
        /// The owners.
        dead: Vec<String>,
    },
    /// The database side of `jkb doctor` ([`health::health`]).
    #[serde(rename = "kb.health")]
    KbHealth {},
    /// A repo's tasks with a land target, for `jkb staging ls` ([`staging::staging`]).
    #[serde(rename = "task.staging")]
    TaskStaging {
        /// The repo key.
        repo: String,
        /// Spent batches too.
        #[serde(default)]
        all: bool,
    },
    /// Any item's details ([`items::show`]).
    #[serde(rename = "item.show")]
    ItemShow {
        /// The item's uid.
        uid: String,
        /// How many characters of its content to carry.
        #[serde(default)]
        preview: Option<usize>,
    },
    /// Delete an item ([`items::remove`]).
    #[serde(rename = "item.rm")]
    ItemRm {
        /// The item's uid.
        uid: String,
        /// Past the memory and synced-file guards.
        #[serde(default)]
        force: bool,
    },
    /// The items an item's edges reach ([`items::related`]).
    #[serde(rename = "kb.related")]
    KbRelated {
        /// The start item's uid.
        uid: String,
        /// The edge types to follow; any when empty.
        #[serde(default)]
        edges: Vec<String>,
        /// How many edges deep.
        depth: usize,
        /// Which way.
        #[serde(default)]
        direction: items::Direction,
    },
    /// The sync archive's blobs ([`items::blobs`]).
    #[serde(rename = "kb.blobs")]
    KbBlobs {
        /// Only blobs holding this text.
        #[serde(default)]
        contains: Option<String>,
        /// At most this many.
        limit: usize,
    },
    /// One archived blob's text ([`items::blob_text`]).
    #[serde(rename = "kb.blob")]
    KbBlob {
        /// A unique hash prefix.
        prefix: String,
    },
    /// An investigation read ([`inv::read`]).
    #[serde(rename = "inv.read")]
    InvRead(inv::InvRead),
    /// An investigation write ([`inv::write`]).
    #[serde(rename = "inv.write")]
    InvWrite(inv::InvWrite),
    /// What a task's history says about its landing ([`prs::facts`]).
    #[serde(rename = "task.pr_facts")]
    TaskPrFacts {
        /// A task uid or bare slug.
        uid: String,
    },
    /// A repo's unfinished tasks ([`prs::open_in_repo`]).
    #[serde(rename = "task.open_in_repo")]
    TaskOpenInRepo {
        /// The repo key.
        repo: String,
    },
    /// Record a task's pull request ([`prs::record`]).
    #[serde(rename = "task.pr_record")]
    TaskPrRecord {
        /// A task uid or bare slug.
        uid: String,
        /// The pull request's number.
        number: i64,
    },
    /// Close a task whose work merged ([`prs::close_merged`]).
    #[serde(rename = "task.close_merged")]
    TaskCloseMerged {
        /// A task uid or bare slug.
        uid: String,
        /// Whether the client established that it merged.
        merged: prs::Merged,
        /// The landing history the client judged it from.
        observed: prs::Observed,
        /// The pull request that proved it.
        #[serde(default)]
        pr: Option<i64>,
        /// Judge only.
        #[serde(default)]
        dry_run: bool,
    },
    /// A namespace's children, or the top level ([`namespaces::list`]).
    #[serde(rename = "ns.list")]
    NsList {
        /// The parent; the top level when absent.
        #[serde(default)]
        scope: Option<String>,
    },
    /// Move a namespace subtree ([`namespaces::mv`]).
    #[serde(rename = "ns.mv")]
    NsMv {
        /// The subtree.
        from: String,
        /// Its new path.
        to: String,
    },
    /// The chunks around an item ([`kb::context`]).
    #[serde(rename = "kb.context")]
    KbContext {
        /// The item's id.
        item: i64,
        /// How many chunks either side.
        n: usize,
    },
    /// Every saved view ([`kb::views`]).
    #[serde(rename = "view.list")]
    ViewList {},
    /// A saved view's items ([`kb::run_view`]).
    #[serde(rename = "view.run")]
    ViewRun {
        /// The view.
        name: String,
        /// At most this many.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// A synced file's versions ([`items::history`]).
    #[serde(rename = "kb.history")]
    KbHistory {
        /// The file, absolute in the client's filesystem.
        path: String,
        /// The client's `$HOME`.
        #[serde(default)]
        home: String,
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
    /// Where the notification stands: `awaiting_user` (no tool named — the idle prompt, and the
    /// Code Factory's needs-input dot, D53.9) or `awaiting_tool` (a permission prompt naming
    /// `tool`). Derived by [`notify::SessionRecord::state`], never by a reader. Empty from a daemon
    /// that predates it.
    #[serde(default)]
    pub state: String,
}

/// One process's hold on a Claude Code session, as `session.list` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeSession {
    /// The session id.
    pub session: String,
    /// The `claude` process, or empty when the hook had none it could trust.
    pub pid: String,
    /// Where `pid` means something: `host[#boot][/pidns]`.
    pub instance: String,
    /// Its working directory, as the hook reported it.
    pub cwd: String,
    /// When this process last started it (Unix ms); absent when it was first seen otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// How it last started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_source: Option<String>,
    /// The last event from this process (Unix ms), refreshed at most hourly.
    pub seen_at: i64,
    /// When it ended (Unix ms); absent while live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// Why it ended (`gone` when a sweep proved it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
}

impl From<jkb_core::claude_session::HolderRow> for ClaudeSession {
    fn from(r: jkb_core::claude_session::HolderRow) -> Self {
        Self {
            session: r.session,
            pid: r.pid,
            instance: r.instance,
            cwd: r.cwd,
            started_at: r.started_at,
            start_source: r.start_source,
            seen_at: r.seen_at,
            ended_at: r.ended_at,
            end_reason: r.end_reason,
        }
    }
}

/// Where a `session.list` page ended, as the daemon encodes it into the opaque `next` string. A string
/// on the wire, not an object, so a newer daemon can change what it holds without an older client —
/// which only sends it back — failing to decode the page around it (stage-1 review, round 4).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionCursor {
    seen_at: i64,
    session: String,
    pid: String,
    instance: String,
}

impl SessionCursor {
    fn encode(c: &jkb_core::claude_session::Cursor) -> String {
        serde_json::json!({
            "seen_at": c.seen_at,
            "session": c.session,
            "pid": c.pid,
            "instance": c.instance,
        })
        .to_string()
    }

    fn decode(s: &str) -> Result<jkb_core::claude_session::Cursor, ApiError> {
        let c: Self = serde_json::from_str(s).map_err(|e| {
            ApiError::with_code(
                ErrorCode::Invalid,
                format!("`after` is not a cursor this daemon issued: {e}"),
            )
        })?;
        Ok(jkb_core::claude_session::Cursor {
            seen_at: c.seen_at,
            session: c.session,
            pid: c.pid,
            instance: c.instance,
        })
    }
}

/// A Claude Code session's state, as `session.state` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateIs {
    /// No row: nothing is known.
    Unknown,
    /// Some process still holds it.
    Live,
    /// Every process that held it has ended.
    Ended,
}

impl From<claude_session::SessionState> for SessionStateIs {
    fn from(s: claude_session::SessionState) -> Self {
        match s {
            claude_session::SessionState::Unknown => Self::Unknown,
            claude_session::SessionState::Live => Self::Live,
            claude_session::SessionState::Ended => Self::Ended,
        }
    }
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
        "session.started",
        "session.ended",
        "session.gone",
        "session.list",
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
        "task.why",
        "task.add",
        "task.set",
        "task.edit",
        "task.tag",
        "task.depend",
        "task.undepend",
        "task.place",
        "task.unplace",
        "task.bind",
        "task.claim",
        "task.release",
        "ingest.text",
        "task.facts",
        "task.by_branch",
        "task.start",
        "task.take",
        "task.locate",
        "task.abandon",
        "repo.gate",
        "session.state",
        "removal.add",
        "removal.list",
        "removal.archived",
        "removal.cancel",
        "removal.drop",
        "lease.get",
        "lease.take",
        "lease.release",
        "lease.break",
        "task.land",
        "task.land_check",
        "task.landed",
        "task.review_findings",
        "task.review_file",
        "task.review_record",
        "task.review_waive",
        "task.ran_on_host",
        "role.grant",
        "role.revoke",
        "role.list",
        "role.whoami",
        "role.bind",
        "role.map",
        "role.rotate_container",
        "workflow.show",
        "workflow.fire",
        "workflow.observe",
        "workflow.set",
        "workflow.define",
        "workflow.strategies",
        "workflow.graph",
        "workflow.agents",
        "workflow.agent",
        "workflow.agent_copy",
        "workflow.agent_set",
        "design.list",
        "design.create",
        "design.cat",
        "design.export",
        "design.target",
        "design.sources",
        "design.state",
        "design.spans",
        "design.apply",
        "design.edit",
        "design.span",
        "design.approve",
        "design.stage",
        "design.plan_create",
        "design.plan_step",
        "design.plan",
        "design.plans",
        "design.prompt",
        "design.prompt_record",
        "design.prompts",
        "design.prompt_of",
        "design.compact",
        "attest.mint",
        "attest.release",
        "task.claims",
        "task.reclaim",
        "kb.health",
        "task.staging",
        "item.show",
        "item.rm",
        "kb.related",
        "kb.blobs",
        "kb.blob",
        "inv.read",
        "inv.write",
        "task.pr_facts",
        "task.open_in_repo",
        "task.pr_record",
        "task.close_merged",
        "ns.list",
        "ns.mv",
        "kb.context",
        "view.list",
        "view.run",
        "kb.history",
    ];

    /// This request's op name — the `"op"` tag it serializes with. Exhaustive, so a new op must be
    /// named here; `every_op_names_its_own_wire_tag_and_is_advertised` checks [`Request::OPS`]
    /// against the tags serde accepts and each op's name against the tag it serializes with.
    #[must_use]
    #[allow(clippy::too_many_lines)] // one arm per op, like `permission`
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
            Self::SessionStarted { .. } => "session.started",
            Self::SessionEnded { .. } => "session.ended",
            Self::SessionGone { .. } => "session.gone",
            Self::SessionList { .. } => "session.list",
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
            Self::TaskWhy { .. } => "task.why",
            Self::TaskAdd(_) => "task.add",
            Self::TaskSet { .. } => "task.set",
            Self::TaskEdit { .. } => "task.edit",
            Self::TaskTag { .. } => "task.tag",
            Self::TaskDepend { .. } => "task.depend",
            Self::TaskUndepend { .. } => "task.undepend",
            Self::TaskPlace { .. } => "task.place",
            Self::TaskUnplace { .. } => "task.unplace",
            Self::TaskBind { .. } => "task.bind",
            Self::TaskClaim { .. } => "task.claim",
            Self::TaskRelease { .. } => "task.release",
            Self::IngestText(_) => "ingest.text",
            Self::TaskFacts { .. } => "task.facts",
            Self::TaskByBranch { .. } => "task.by_branch",
            Self::TaskStart(_) => "task.start",
            Self::TaskTake(_) => "task.take",
            Self::TaskLocate { .. } => "task.locate",
            Self::TaskAbandon { .. } => "task.abandon",
            Self::RepoGate { .. } => "repo.gate",
            Self::SessionState { .. } => "session.state",
            Self::RemovalAdd { .. } => "removal.add",
            Self::RemovalList { .. } => "removal.list",
            Self::RemovalArchived { .. } => "removal.archived",
            Self::RemovalCancel { .. } => "removal.cancel",
            Self::RemovalDrop { .. } => "removal.drop",
            Self::LeaseGet { .. } => "lease.get",
            Self::LeaseTake { .. } => "lease.take",
            Self::LeaseRelease { .. } => "lease.release",
            Self::LeaseBreak { .. } => "lease.break",
            Self::TaskLand { .. } => "task.land",
            Self::TaskLandCheck { .. } => "task.land_check",
            Self::TaskLanded { .. } => "task.landed",
            Self::TaskReviewFindings { .. } => "task.review_findings",
            Self::TaskReviewFile(_) => "task.review_file",
            Self::TaskReviewRecord(_) => "task.review_record",
            Self::TaskReviewWaive { .. } => "task.review_waive",
            Self::TaskRanOnHost { .. } => "task.ran_on_host",
            Self::RoleGrant { .. } => "role.grant",
            Self::RoleRevoke { .. } => "role.revoke",
            Self::RoleList { .. } => "role.list",
            Self::RoleWhoami {} => "role.whoami",
            Self::RoleBind { .. } => "role.bind",
            Self::RoleMap { .. } => "role.map",
            Self::RoleRotateContainer { .. } => "role.rotate_container",
            Self::WorkflowShow { .. } => "workflow.show",
            Self::WorkflowFire { .. } => "workflow.fire",
            Self::WorkflowObserve { .. } => "workflow.observe",
            Self::WorkflowSet { .. } => "workflow.set",
            Self::WorkflowDefine { .. } => "workflow.define",
            Self::WorkflowStrategies {} => "workflow.strategies",
            Self::WorkflowGraph { .. } => "workflow.graph",
            Self::WorkflowAgents {} => "workflow.agents",
            Self::WorkflowAgent { .. } => "workflow.agent",
            Self::WorkflowAgentCopy { .. } => "workflow.agent_copy",
            Self::WorkflowAgentSet { .. } => "workflow.agent_set",
            Self::DesignList { .. } => "design.list",
            Self::DesignCreate { .. } => "design.create",
            Self::DesignCat { .. } => "design.cat",
            Self::DesignExport { .. } => "design.export",
            Self::DesignTarget { .. } => "design.target",
            Self::DesignSources { .. } => "design.sources",
            Self::DesignState { .. } => "design.state",
            Self::DesignSpans { .. } => "design.spans",
            Self::DesignApply { .. } => "design.apply",
            Self::DesignEdit { .. } => "design.edit",
            Self::DesignSpan { .. } => "design.span",
            Self::DesignApprove { .. } => "design.approve",
            Self::DesignStage { .. } => "design.stage",
            Self::DesignPlanCreate { .. } => "design.plan_create",
            Self::DesignPlanStep { .. } => "design.plan_step",
            Self::DesignPlan { .. } => "design.plan",
            Self::DesignPlans { .. } => "design.plans",
            Self::DesignPrompt { .. } => "design.prompt",
            Self::DesignPromptRecord { .. } => "design.prompt_record",
            Self::DesignPrompts { .. } => "design.prompts",
            Self::DesignPromptOf { .. } => "design.prompt_of",
            Self::DesignCompact { .. } => "design.compact",
            Self::AttestMint { .. } => "attest.mint",
            Self::AttestRelease { .. } => "attest.release",
            Self::TaskClaims { .. } => "task.claims",
            Self::TaskReclaim { .. } => "task.reclaim",
            Self::KbHealth {} => "kb.health",
            Self::TaskStaging { .. } => "task.staging",
            Self::ItemShow { .. } => "item.show",
            Self::ItemRm { .. } => "item.rm",
            Self::KbRelated { .. } => "kb.related",
            Self::KbBlobs { .. } => "kb.blobs",
            Self::KbBlob { .. } => "kb.blob",
            Self::InvRead(_) => "inv.read",
            Self::InvWrite(_) => "inv.write",
            Self::KbHistory { .. } => "kb.history",
            Self::NsList { .. } => "ns.list",
            Self::NsMv { .. } => "ns.mv",
            Self::KbContext { .. } => "kb.context",
            Self::ViewList {} => "view.list",
            Self::ViewRun { .. } => "view.run",
            Self::TaskPrFacts { .. } => "task.pr_facts",
            Self::TaskOpenInRepo { .. } => "task.open_in_repo",
            Self::TaskPrRecord { .. } => "task.pr_record",
            Self::TaskCloseMerged { .. } => "task.close_merged",
        }
    }

    /// Whether this op is in the agent read set (`kb.*`, `task.ready`/`show`/`subtasks`/`why`, and the
    /// session verbs' reads `task.facts`/`by_branch`/`review_findings` and `repo.gate`) — the one
    /// place that says so. [`LocalBackend`] serves it on its reader and within its read budget, and
    /// `jkb serve` counts it against its read permits, from this answer; no dispatch arm chooses.
    ///
    /// **Not "every op that does not write".** The reader serves one call at a time, and what queues
    /// there is a client's reads — a grep over the whole knowledge base among them. The queue's and the
    /// notification hook's own reads (`mq.inspect`, `mq.tail`, `notify.open_sessions`, `session.list`) are short and
    /// latency-bound — a `SessionStart` sweep has 1 s — so they stay on the writer, which otherwise runs
    /// only writes (`ingest.text`, the longest, under the daemon's own small budget); classing them by "does not write" put the sweep behind a container's grep (stage-6.1 review).
    /// Exhaustive, so a new op must say. A write classed here fails against the daemon's `query_only`
    /// reader rather than writing somewhere unguarded (`every_op_is_served_on_the_connection_its_class_names`).
    #[must_use]
    #[allow(clippy::too_many_lines)] // one arm per op, like `permission`
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
            | Self::TaskSubtasks { .. }
            | Self::TaskWhy { .. }
            | Self::TaskFacts { .. }
            | Self::TaskByBranch { .. }
            | Self::TaskReviewFindings { .. }
            | Self::TaskClaims { .. }
            | Self::TaskStaging { .. }
            | Self::ItemShow { .. }
            | Self::KbRelated { .. }
            | Self::KbBlobs { .. }
            | Self::KbBlob { .. }
            | Self::KbHistory { .. }
            | Self::InvRead(_)
            | Self::TaskPrFacts { .. }
            | Self::TaskOpenInRepo { .. }
            | Self::NsList { .. }
            | Self::KbContext { .. }
            | Self::ViewList {}
            | Self::ViewRun { .. }
            | Self::RepoGate { .. }
            | Self::RoleList { .. }
            | Self::RoleWhoami {}
            | Self::WorkflowShow { .. }
            | Self::WorkflowStrategies {}
            | Self::WorkflowGraph { .. }
            | Self::WorkflowAgents {}
            | Self::WorkflowAgent { .. }
            | Self::DesignList { .. }
            | Self::DesignCat { .. }
            | Self::DesignExport { .. }
            | Self::DesignState { .. }
            | Self::DesignSpans { .. }
            | Self::DesignPlan { .. }
            | Self::DesignPlans { .. }
            | Self::DesignPrompt { .. }
            | Self::DesignPrompts { .. }
            | Self::DesignPromptOf { .. } => true,
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
            | Self::NotifyGone { .. }
            | Self::SessionStarted { .. }
            | Self::SessionEnded { .. }
            | Self::SessionGone { .. }
            | Self::SessionList { .. }
            | Self::TaskAdd(_)
            | Self::TaskSet { .. }
            | Self::TaskEdit { .. }
            | Self::TaskTag { .. }
            | Self::TaskDepend { .. }
            | Self::TaskUndepend { .. }
            | Self::TaskPlace { .. }
            | Self::TaskUnplace { .. }
            | Self::TaskBind { .. }
            | Self::TaskClaim { .. }
            | Self::TaskRelease { .. }
            | Self::IngestText(_)
            | Self::TaskStart(_)
            | Self::TaskTake(_)
            | Self::TaskLocate { .. }
            | Self::TaskAbandon { .. }
            | Self::SessionState { .. }
            // The sweep's reads, short and on its clock, like `session.list`.
            | Self::RemovalList { .. }
            | Self::LeaseGet { .. }
            | Self::RemovalAdd { .. }
            | Self::RemovalArchived { .. }
            | Self::RemovalCancel { .. }
            | Self::RemovalDrop { .. }
            | Self::LeaseTake { .. }
            | Self::LeaseRelease { .. }
            | Self::LeaseBreak { .. }
            | Self::TaskLand { .. }
            // Records nothing itself, but its admission can bind an attested subagent to the task.
            | Self::TaskLandCheck { .. }
            | Self::TaskLanded { .. }
            | Self::TaskReviewFile(_)
            | Self::TaskReviewRecord(_)
            | Self::TaskReviewWaive { .. }
            | Self::TaskRanOnHost { .. }
            | Self::RoleGrant { .. }
            | Self::RoleRevoke { .. }
            // Its admission writes the binding, so it is served on the writer.
            | Self::RoleBind { .. }
            | Self::RoleMap { .. }
            | Self::RoleRotateContainer { .. }
            | Self::WorkflowFire { .. }
            | Self::WorkflowObserve { .. }
            | Self::WorkflowSet { .. }
            | Self::WorkflowDefine { .. }
            | Self::WorkflowAgentCopy { .. }
            | Self::WorkflowAgentSet { .. }
            // In memory: no database at all, so the writer is as good as anywhere.
            | Self::AttestMint { .. }
            | Self::AttestRelease { .. }
            | Self::TaskReclaim { .. }
            // FTS5's integrity check is an `INSERT`, which the `query_only` reader refuses.
            | Self::KbHealth {}
            | Self::ItemRm { .. }
            | Self::InvWrite(_)
            | Self::TaskPrRecord { .. }
            | Self::TaskCloseMerged { .. }
            | Self::NsMv { .. }
            | Self::DesignCreate { .. }
            | Self::DesignApply { .. }
            | Self::DesignEdit { .. }
            | Self::DesignSpan { .. }
            | Self::DesignApprove { .. }
            | Self::DesignStage { .. }
            | Self::DesignPlanCreate { .. }
            | Self::DesignPlanStep { .. }
            | Self::DesignPromptRecord { .. }
            | Self::DesignTarget { .. }
            | Self::DesignSources { .. }
            | Self::DesignCompact { .. } => false,
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
    /// A `session.started`.
    SessionStart {
        /// The session's state before: `unknown`, `live` (a compaction, or another process holds
        /// it too) or `ended` (a revival).
        was: String,
    },
    /// A `session.ended`.
    SessionEnd {
        /// `recorded`, or `already_ended` (the first reason stands).
        outcome: String,
    },
    /// A `session.gone`.
    SessionGone {
        /// Whether the process's hold was ended by it.
        ended: bool,
    },
    /// A `session.list` page.
    ClaudeSessions {
        /// In the order asked for, at most `jkb_core::claude_session::LIST_CAP`.
        sessions: Vec<ClaudeSession>,
        /// Where the next page starts — opaque, to be sent back as `after`; absent when this page is the
        /// last.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<String>,
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
    /// A write that answers with nothing but its success (`task.set`, `edit`, `tag`, `depend`,
    /// `undepend`, `place`, `bind`).
    Applied {},
    /// A `task.add`.
    Added {
        /// The task created.
        #[serde(flatten)]
        added: tasks::Added,
    },
    /// A `task.unplace`.
    Unplaced {
        /// Mirror placements removed.
        removed: usize,
    },
    /// A `task.claim`.
    Claimed {
        /// The answer.
        #[serde(flatten)]
        claimed: tasks::Claimed,
    },
    /// A `task.release`.
    Released {
        /// Whether the owner held the claim and it was dropped.
        released: bool,
    },
    /// A `task.why`.
    History {
        /// Oldest first.
        entries: Vec<tasks::HistoryEntry>,
        /// Cut short at the read's budget ([`kb::Budget`]).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// A `task.edit`.
    Edited {
        /// The task's content is written back to a file by the host's sync.
        file_backed: bool,
    },
    /// A `task.add` that created nothing: outside any repo, `backlog` needs the user's assent to use
    /// the global backlog. Ask, and send the request again with `global_backlog`.
    NeedsGlobalBacklogAssent {},
    /// An `ingest.text`.
    Ingested {
        /// What was stored.
        #[serde(flatten)]
        ingested: ingest::Ingested,
    },
    /// A `task.facts`.
    TaskState {
        /// The task as the session verbs see it.
        #[serde(flatten)]
        state: sessions::TaskState,
    },
    /// A `task.by_branch`.
    BranchTasks {
        /// By branch.
        tasks: std::collections::BTreeMap<String, Vec<sessions::BranchTask>>,
    },
    /// A `task.start` or `task.take`.
    Taken {
        /// `false` when the claim changed hands since the caller read it; nothing was written.
        taken: bool,
    },
    /// A `task.abandon`.
    Abandoned {
        /// What it did.
        #[serde(flatten)]
        abandoned: sessions::Abandoned,
    },
    /// A `repo.gate`.
    Gate {
        /// The stored command, if any.
        gate: Option<String>,
    },
    /// A `session.state`.
    SessionIs {
        /// The state — a closed set, so a state a newer daemon adds fails to decode rather than reading
        /// as some other one.
        state: SessionStateIs,
    },
    /// A `removal.add`.
    RemovalAdded {
        /// The new record.
        id: i64,
    },
    /// A `removal.list` page.
    Removals {
        /// Oldest first.
        records: Vec<removals::RemovalRecord>,
        /// Where the next page starts, to be sent back as `after`; absent on the last page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<i64>,
    },
    /// A `removal.archived`, `removal.drop`, `lease.take` or `lease.release`: whether it changed
    /// anything. `false` is an answer — the record or the lease was not as the caller read it.
    Changed {
        /// Whether it did.
        changed: bool,
    },
    /// A `removal.cancel`.
    RemovalsCancelled {
        /// What it did.
        #[serde(flatten)]
        cancelled: removals::Cancelled,
    },
    /// A `task.land` or `task.landed`.
    Landing {
        /// What it did.
        #[serde(flatten)]
        landing: sessions::Landing,
    },
    /// A `task.review_findings`.
    ReviewFindings {
        /// What the reviews hold.
        #[serde(flatten)]
        findings: sessions::ReviewFindings,
    },
    /// A `lease.get`.
    Lease {
        /// The lease, if anyone holds it.
        lease: Option<removals::LeaseHeld>,
    },
    /// A `lease.break`.
    LeaseBroken {
        /// The holder it displaced, if any.
        holder: Option<String>,
    },
    /// A `task.review_file`.
    ReviewFiled {
        /// What it filed.
        #[serde(flatten)]
        filed: review::Filed,
    },
    /// A `task.review_record`.
    ReviewRecorded {
        /// What it recorded.
        #[serde(flatten)]
        recording: review::Recording,
    },
    /// A `task.claims` page.
    Claims {
        /// The held claims.
        claims: Vec<claims::Claim>,
        /// The next page's cursor, when there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<i64>,
    },
    /// A `task.reclaim`.
    Reclaimed {
        /// What it freed.
        #[serde(flatten)]
        reclaimed: claims::Reclaimed,
    },
    /// A `kb.health`.
    Health {
        /// What it found.
        #[serde(flatten)]
        health: health::Health,
    },
    /// A `task.staging`.
    StagingTasks {
        /// The tasks.
        tasks: Vec<staging::StagingTask>,
        /// Cut short at the read's budget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// An `item.show`.
    Item {
        /// The item.
        item: Box<items::ItemInfo>,
    },
    /// An `item.rm`.
    ItemRemoved {
        /// What went.
        #[serde(flatten)]
        removed: items::Removed,
    },
    /// A `kb.related`.
    Related {
        /// The items reached.
        rows: Vec<items::RelatedRow>,
        /// Cut short at the read's budget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        /// The walk stopped at [`items::MAX_RELATED_NODES`], which is the same everywhere.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        at_node_cap: bool,
    },
    /// A `kb.blobs`.
    Blobs {
        /// The blobs.
        blobs: Vec<items::BlobRow>,
        /// Cut short at the read's budget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// A `kb.blob`.
    Blob {
        /// The blob's full hash.
        hash: String,
        /// Its text.
        text: String,
    },
    /// A `kb.history`.
    Versions {
        /// The file's journal uri.
        uri: String,
        /// Its versions, newest first.
        versions: Vec<items::Version>,
        /// Cut short at the read's budget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// An `inv.read` or `inv.write`.
    Inv {
        /// What it answered.
        answer: inv::InvAnswer,
        /// Cut short at the read's budget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// A `task.pr_facts`.
    PrFacts {
        /// The facts.
        #[serde(flatten)]
        facts: prs::PrFacts,
    },
    /// A `task.open_in_repo`.
    Uids {
        /// The tasks.
        uids: Vec<String>,
        /// Cut short at the read's budget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// A `kb.context`.
    Context {
        /// The chunks.
        chunks: Vec<kb::ContextLine>,
        /// Cut short at the read's budget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// A `view.list`.
    Views {
        /// The views.
        views: Vec<kb::View>,
        /// Cut short at the read's budget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// An `ns.list`.
    Namespaces {
        /// The paths.
        paths: Vec<String>,
        /// Cut short at the read's budget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// An `ns.mv`.
    Moved {
        /// How many namespaces moved.
        count: usize,
    },
    /// A `task.close_merged`.
    Closed {
        /// Why it was held, if it was.
        refusal: Option<String>,
    },
    /// A `task.show`.
    Task {
        /// The task.
        task: Box<kb::TaskDetail>,
        /// Its subtasks were cut short at the read's budget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// A `task.land_check` the caller passed: `task.land` with that payload would be admitted.
    /// A refusal is an error, like every other refused op.
    LandAdmitted {},
    /// A `role.grant` or `role.rotate_container`: the grant, and its token — answered this once.
    Granted {
        /// The grant.
        grant: rbac::GrantInfo,
        /// Its token.
        token: String,
    },
    /// A `role.revoke`.
    Revoked {
        /// How many grants it revoked (the grant and what it minted).
        count: usize,
    },
    /// A `role.list`.
    Grants {
        /// The listing.
        listing: rbac::GrantListing,
    },
    /// A `role.whoami`.
    WhoAmI {
        /// The caller.
        whoami: rbac::WhoAmI,
    },
    /// A `workflow.show`.
    Workflow {
        /// The task's workflow.
        workflow: Box<rbac::WorkflowView>,
    },
    /// A `workflow.fire` or `workflow.observe`.
    WorkflowMoved {
        /// What happened.
        outcome: rbac::WorkflowMove,
    },
    /// A `workflow.strategies`.
    Strategies {
        /// Presets, then definitions.
        strategies: Vec<rbac::StrategyInfo>,
        /// What a task with nothing pinned runs.
        default: String,
    },
    /// A `workflow.define`.
    Defined {
        /// The new version.
        version: i64,
    },
    /// A `workflow.graph`.
    WorkflowGraph {
        /// The machines.
        graph: Box<workflows::GraphView>,
    },
    /// A `workflow.agents`.
    WorkflowAgents {
        /// Every template, as the one in effect.
        agents: Vec<workflows::AgentView>,
    },
    /// A `workflow.agent`, `workflow.agent_copy` or `workflow.agent_set`.
    WorkflowAgent {
        /// The template.
        agent: Box<workflows::AgentView>,
        /// Its prompt filled in, when values were given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rendered: Option<String>,
        /// Whether anything was written (a copy always writes; an edit that changes nothing does
        /// not). `false` for a read.
        #[serde(default)]
        wrote: bool,
    },
    /// An `attest.mint`.
    Ticket {
        /// The ticket.
        token: String,
    },
    /// An `attest.release`.
    TicketsReleased {
        /// How many.
        count: usize,
    },
    /// A `design.list`.
    Designs {
        /// The designs, by uid.
        designs: Vec<designs::Design>,
    },
    /// A `design.create`.
    DesignCreated {
        /// The new design.
        design: designs::Design,
    },
    /// A `design.export`.
    DesignExports {
        /// The rendered designs, by uid.
        exports: Vec<designs::Export>,
    },
    /// A `design.target` or `design.sources`: the design after it.
    DesignMeta {
        /// The design.
        design: designs::Design,
    },
    /// A `design.cat`.
    DesignText {
        /// The design at its current version.
        design: Box<designs::DesignDoc>,
    },
    /// A `design.state`.
    DesignUpdate {
        /// What the peer lacks, as one Yjs v1 update, base64.
        update: String,
        /// The version it brings the peer to.
        version: String,
        /// That version's seq.
        seq: i64,
    },
    /// A `design.spans`.
    DesignSpans {
        /// The spans, in text order.
        spans: Vec<designs::Span>,
    },
    /// A `design.apply`, `design.edit` or `design.span`.
    DesignWritten {
        /// What it did.
        written: designs::Written,
    },
    /// A `design.approve` or `design.stage`.
    DesignSpan {
        /// The span after it.
        span: Box<designs::Span>,
    },
    /// A `design.prompt` for *Discuss*.
    DesignPrompt {
        /// The prompt and what it was built from.
        prompt: Box<designs::Prompt>,
    },
    /// A `design.prompt` for *Play* (a plan or a task).
    DesignWorkPrompt {
        /// The prompt and what it was built from.
        prompt: Box<designs::plans::WorkPrompt>,
    },
    /// A `design.prompt` for *New prompt*.
    DesignNewPrompt {
        /// The prompt and what it was built from.
        prompt: Box<designs::prompts::NewPrompt>,
    },
    /// A `design.prompt_record`.
    DesignPromptRecorded {
        /// The prompt as recorded.
        prompt: Box<designs::prompts::DesignPrompt>,
        /// Whether anything was written — and so announced on the design's topic. A session
        /// recorded again from where it already was writes nothing.
        wrote: bool,
    },
    /// A `design.prompts`.
    DesignPrompts {
        /// The design.
        uid: String,
        /// Its prompts, newest first.
        prompts: Vec<designs::prompts::DesignPrompt>,
    },
    /// A `design.prompt_of`.
    DesignPromptOf {
        /// The session asked about.
        session: String,
        /// The prompt it was recorded with; absent when no launch recorded it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<Box<designs::prompts::DesignPrompt>>,
    },
    /// A `design.plan`, `design.plan_create` or `design.plan_step`.
    DesignPlan {
        /// The plan after it.
        plan: Box<designs::plans::Plan>,
    },
    /// A `design.plans`.
    DesignPlans {
        /// The design's plans and one-off tasks.
        list: Box<designs::plans::PlanList>,
    },
    /// A `design.compact`.
    DesignCompacted {
        /// The snapshot covers every update through this seq.
        through: i64,
        /// How many update rows it replaced.
        removed: usize,
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
            | Self::Task { truncated, .. }
            | Self::StagingTasks { truncated, .. }
            | Self::Inv { truncated, .. }
            | Self::Namespaces { truncated, .. }
            | Self::Context { truncated, .. }
            | Self::Views { truncated, .. }
            | Self::Uids { truncated, .. }
            | Self::Related { truncated, .. }
            | Self::Blobs { truncated, .. }
            | Self::Versions { truncated, .. }
            | Self::History { truncated, .. } => *truncated,
            Self::GrepHits { answer } => answer.truncated,
            Self::Created { .. }
            | Self::Sent { .. }
            | Self::Messages { .. }
            | Self::Position { .. }
            | Self::Compacted { .. }
            | Self::Topics { .. }
            | Self::Notified { .. }
            | Self::Sessions { .. }
            | Self::SessionStart { .. }
            | Self::SessionEnd { .. }
            | Self::SessionGone { .. }
            | Self::ClaudeSessions { .. }
            | Self::Ambient { .. }
            | Self::Count { .. }
            | Self::Content { .. }
            | Self::Applied {}
            | Self::Added { .. }
            | Self::Unplaced { .. }
            | Self::Claimed { .. }
            | Self::Released { .. }
            | Self::Edited { .. }
            | Self::Ingested { .. }
            | Self::TaskState { .. }
            | Self::BranchTasks { .. }
            | Self::Taken { .. }
            | Self::Abandoned { .. }
            | Self::Gate { .. }
            | Self::SessionIs { .. }
            | Self::RemovalAdded { .. }
            | Self::Removals { .. }
            | Self::Changed { .. }
            | Self::RemovalsCancelled { .. }
            | Self::Landing { .. }
            | Self::ReviewFindings { .. }
            | Self::ReviewFiled { .. }
            | Self::ReviewRecorded { .. }
            | Self::Reclaimed { .. }
            | Self::Health { .. }
            | Self::Claims { .. }
            | Self::PrFacts { .. }
            | Self::Closed { .. }
            | Self::Moved { .. }
            | Self::Item { .. }
            | Self::ItemRemoved { .. }
            | Self::Blob { .. }
            | Self::Lease { .. }
            | Self::LeaseBroken { .. }
            | Self::LandAdmitted {}
            | Self::Granted { .. }
            | Self::Revoked { .. }
            | Self::Grants { .. }
            | Self::WhoAmI { .. }
            | Self::Workflow { .. }
            | Self::WorkflowMoved { .. }
            | Self::Strategies { .. }
            | Self::Defined { .. }
            | Self::WorkflowGraph { .. }
            | Self::WorkflowAgents { .. }
            | Self::WorkflowAgent { .. }
            | Self::Ticket { .. }
            | Self::TicketsReleased { .. }
            | Self::Designs { .. }
            | Self::DesignCreated { .. }
            | Self::DesignExports { .. }
            | Self::DesignMeta { .. }
            | Self::DesignText { .. }
            | Self::DesignUpdate { .. }
            | Self::DesignSpans { .. }
            | Self::DesignWritten { .. }
            | Self::DesignSpan { .. }
            | Self::DesignPrompt { .. }
            | Self::DesignWorkPrompt { .. }
            | Self::DesignNewPrompt { .. }
            | Self::DesignPromptRecorded { .. }
            | Self::DesignPrompts { .. }
            | Self::DesignPromptOf { .. }
            | Self::DesignPlan { .. }
            | Self::DesignPlans { .. }
            | Self::DesignCompacted { .. }
            | Self::NeedsGlobalBacklogAssent {} => false,
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
            // Every stored design update is announced on `design/<uid>` (D53.4).
            Self::DesignWritten { written } => written.seq.is_some(),
            Self::DesignCreated { design } => design.seq > 0,
            // A recorded prompt is announced on its design's topic (D53.6), when it wrote.
            Self::DesignPromptRecorded { wrote, .. } => *wrote,
            Self::Created { .. }
            | Self::Messages { .. }
            | Self::Position { .. }
            | Self::Compacted { .. }
            | Self::Topics { .. }
            | Self::Sessions { .. }
            | Self::SessionStart { .. }
            | Self::SessionEnd { .. }
            | Self::SessionGone { .. }
            | Self::ClaudeSessions { .. }
            | Self::Ambient { .. }
            | Self::Items { .. }
            | Self::Count { .. }
            | Self::Listing { .. }
            | Self::Tree { .. }
            | Self::Children { .. }
            | Self::Content { .. }
            | Self::GrepHits { .. }
            | Self::SearchHits { .. }
            | Self::Task { .. }
            | Self::StagingTasks { .. }
            | Self::Related { .. }
            | Self::Blobs { .. }
            | Self::Versions { .. }
            | Self::Applied {}
            | Self::Added { .. }
            | Self::Unplaced { .. }
            | Self::Claimed { .. }
            | Self::Released { .. }
            | Self::History { .. }
            | Self::Edited { .. }
            | Self::Ingested { .. }
            | Self::TaskState { .. }
            | Self::BranchTasks { .. }
            | Self::Taken { .. }
            | Self::Abandoned { .. }
            | Self::Gate { .. }
            | Self::SessionIs { .. }
            | Self::RemovalAdded { .. }
            | Self::Removals { .. }
            | Self::Changed { .. }
            | Self::RemovalsCancelled { .. }
            | Self::Landing { .. }
            | Self::ReviewFindings { .. }
            | Self::ReviewFiled { .. }
            | Self::ReviewRecorded { .. }
            | Self::Reclaimed { .. }
            | Self::Health { .. }
            | Self::Claims { .. }
            | Self::PrFacts { .. }
            | Self::Uids { .. }
            | Self::Closed { .. }
            | Self::Context { .. }
            | Self::Views { .. }
            | Self::Namespaces { .. }
            | Self::Moved { .. }
            | Self::Inv { .. }
            | Self::Item { .. }
            | Self::ItemRemoved { .. }
            | Self::Blob { .. }
            | Self::Lease { .. }
            | Self::LeaseBroken { .. }
            | Self::LandAdmitted {}
            | Self::Granted { .. }
            | Self::Revoked { .. }
            | Self::Grants { .. }
            | Self::WhoAmI { .. }
            | Self::Workflow { .. }
            | Self::WorkflowMoved { .. }
            | Self::Strategies { .. }
            | Self::Defined { .. }
            | Self::WorkflowGraph { .. }
            | Self::WorkflowAgents { .. }
            | Self::WorkflowAgent { .. }
            | Self::Ticket { .. }
            | Self::TicketsReleased { .. }
            | Self::Designs { .. }
            | Self::DesignExports { .. }
            | Self::DesignMeta { .. }
            | Self::DesignText { .. }
            | Self::DesignUpdate { .. }
            | Self::DesignSpans { .. }
            | Self::DesignSpan { .. }
            | Self::DesignPrompt { .. }
            | Self::DesignWorkPrompt { .. }
            | Self::DesignNewPrompt { .. }
            | Self::DesignPrompts { .. }
            | Self::DesignPromptOf { .. }
            | Self::DesignPlan { .. }
            | Self::DesignPlans { .. }
            | Self::DesignCompacted { .. }
            | Self::NeedsGlobalBacklogAssent {} => false,
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
    /// A write this backend's clients may not make: one that would have the host's sync write a file
    /// outside the directories they may cause host writes in ([`tasks::FileRoots`]).
    Forbidden,
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

/// Run a write to the task `uid` in one transaction, holding the task's tasks.md line to the file's
/// round trip ([`tasks::check_line`]) before it commits. Every task write op but `task.add`, which
/// checks the task it makes, goes through here, so no op can leave a line the next import misreads.
fn task_write<T: Send + 'static>(
    db: &Db,
    actor: &str,
    uid: String,
    op: impl FnOnce(&rusqlite::Connection, &jkb_core::WriteMeta, &str) -> Result<T, ApiError>
        + Send
        + 'static,
) -> Result<T, ApiError> {
    db.write_txn_with(actor, move |c, m| {
        let (line, before) = (tasks::line_of(c, &uid)?, tasks::line_problem(c, &uid)?);
        let out = op(c, m, &uid)?;
        // A write that moves the task to another line (`task.bind`) is judged as a new line: excused by
        // the old line's problem, a bind from an unreadable line onto another task's `#id` put two tasks
        // on one line, and the next export dropped one of them.
        let before = if tasks::line_of(c, &uid)? == line {
            before
        } else {
            None
        };
        tasks::check_line(c, &uid, before.as_deref())?;
        Ok(out)
    })
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

    /// Whether this backend embeds text, so a search may take the vector and hybrid routes. `jkb serve`
    /// does not (tasks F5), so its client defaults to FTS.
    fn embeds(&self) -> bool {
        false
    }

    /// Whether this backend runs in the caller's own process, so a source's raw bytes may travel with
    /// its text ([`ingest::IngestAsk::raw`]).
    fn in_process(&self) -> bool {
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
    /// Where a task write may cause the host's sync to write files ([`tasks::FileRoots`]); anywhere
    /// unless [`LocalBackend::with_file_roots`] set them.
    file_roots: Option<tasks::FileRoots>,
    /// Who the changelog records as making this backend's writes.
    actor: &'static str,
    embedder: Option<Arc<dyn Embedder + Send + Sync>>,
    /// Who is asking (D52.3): the operator unless [`LocalBackend::with_caller`] named a token, which is
    /// resolved and authorized on every call ([`rbac`]).
    caller: rbac::Caller,
    /// The harness tickets this process minted — a daemon's, shared by every per-request backend.
    tickets: Option<rbac::SharedTickets>,
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
            file_roots: None,
            actor: "api",
            embedder: None,
            caller: rbac::Caller::Operator,
            tickets: None,
        }
    }

    /// The same backend, serving `caller` rather than the operator: every call resolves its token to
    /// a principal and is refused what that principal's role, scope and task strategy do not allow.
    #[must_use]
    pub fn with_caller(mut self, caller: rbac::Caller) -> Self {
        self.caller = caller;
        self
    }

    /// The same backend, minting and resolving harness tickets in `tickets` — one store per daemon,
    /// shared by every request's backend.
    #[must_use]
    pub fn with_tickets(mut self, tickets: rbac::SharedTickets) -> Self {
        self.tickets = Some(tickets);
        self
    }

    /// The ticket store, if this backend has one.
    #[must_use]
    pub fn tickets(&self) -> Option<&rbac::SharedTickets> {
        self.tickets.as_ref()
    }

    /// Resolve this backend's caller and admit `request`, binding an attested subagent to its task on
    /// its first task-targeted write. The operator is admitted without a read.
    fn admit(&self, request: &Request) -> Result<(rbac::Principal, Option<NewBinding>), ApiError> {
        if self.caller == rbac::Caller::Operator {
            return Ok((rbac::Principal::operator(), None));
        }
        let caller = self.caller.clone();
        let tickets = self.tickets.clone();
        let req = request.clone();
        let (principal, admit) = self.reads.read_with(move |c| {
            let p = rbac::resolve(c, &caller, tickets.as_deref())?;
            let admit = rbac::authorize(c, &p, &req)?;
            Ok::<_, ApiError>((p, admit))
        })?;
        if let rbac::Admit::BindThenRun {
            session,
            agent_id,
            task,
        } = admit
        {
            let (s, a) = (session.clone(), agent_id.clone());
            let bound = self.db.write_txn(self.actor, move |c, m| {
                jkb_core::roles::bind_agent(c, m, &s, &a, task)
            })?;
            let fresh = match bound {
                jkb_core::roles::Bound::Elsewhere(_) => {
                    return Err(ApiError::with_code(
                        ErrorCode::Forbidden,
                        format!(
                            "`{}` refused: {} is already working on another task, and a worker \
                             does not hop",
                            request.op(),
                            principal.label
                        ),
                    ));
                }
                jkb_core::roles::Bound::Now(_) => Some(NewBinding {
                    session,
                    agent_id,
                    task,
                }),
                jkb_core::roles::Bound::To(_) => None,
            };
            let mut p = principal;
            p.scope = Some(task);
            return Ok((p, fresh));
        }
        Ok((principal, None))
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

    /// The same backend, recording its writes in the changelog as made by `actor` — `cli` for the host
    /// CLI, `serve` for the daemon's clients, so the audit trail says whether a change came from this
    /// host's command line or through `jkb serve`.
    #[must_use]
    pub const fn with_actor(mut self, actor: &'static str) -> Self {
        self.actor = actor;
        self
    }

    /// The same backend, refusing task writes that would have the host's sync write a file outside
    /// `roots` ([`tasks::FileRoots`]). What `jkb serve` sets: its clients are the dev container, which
    /// sees only `~/repos` of the host.
    #[must_use]
    pub fn with_file_roots(mut self, roots: tasks::FileRoots) -> Self {
        self.file_roots = Some(roots);
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

/// A binding [`LocalBackend::admit`] made for the call it admitted — undone if that call fails.
struct NewBinding {
    session: String,
    agent_id: String,
    task: jkb_types::ItemId,
}

impl Backend for LocalBackend {
    fn embeds(&self) -> bool {
        self.embedder.is_some()
    }

    fn in_process(&self) -> bool {
        true
    }

    fn call(&self, request: Request) -> Result<Response, ApiError> {
        // One call at a time per attested subagent, from admission to the undo below.
        let agent_lock = match (&self.caller, &self.tickets) {
            (rbac::Caller::Token(t), Some(tickets)) => tickets.agent_lock(t)?,
            _ => None,
        };
        let _serial = agent_lock
            .as_ref()
            .map(|m| m.lock())
            .transpose()
            .map_err(|_| ApiError::with_code(ErrorCode::Internal, "agent lock poisoned"))?;
        let (principal, bound) = self.admit(&request)?;
        let out = self.dispatch(request, &principal);
        // A binding this call made is kept only if the op it was made for ran: a first op refused
        // for any other reason (outside the file roots, malformed) must not leave the worker bound
        // for good to a task it never worked on.
        if out.is_err() {
            if let Some(b) = bound {
                self.db.write_txn(self.actor, move |c, m| {
                    jkb_core::roles::unbind_agent(c, m, &b.session, &b.agent_id, b.task)
                })?;
            }
        }
        out
    }
}

impl LocalBackend {
    #[allow(clippy::too_many_lines)] // a flat op dispatcher: one arm per op, as in the CLI's `run`
    fn dispatch(
        &self,
        request: Request,
        principal: &rbac::Principal,
    ) -> Result<Response, ApiError> {
        let now = mq::now_ms();
        // Chosen here, once, from the op's class — no arm picks a connection or a budget.
        let db = if request.is_agent_read() {
            &self.reads
        } else {
            &self.db
        };
        let mut budget = self.budget;
        let actor = self.actor;
        let created = |c: Created| Response::Created {
            created: c == Created::New,
        };
        Ok(match request {
            Request::MqTopicCreate { topic, spec } => {
                let spec = spec.resolve();
                created(db.write_txn(actor, move |c, m| {
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
                    seq: db.write_txn(actor, move |c, m| mq::send(c, m, &topic, &draft, now))?,
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
                created(db.write_txn(actor, move |c, m| {
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
                        .write_txn(actor, move |c, m| {
                            mq::poll(c, m, &topic, &group, max, after, now)
                        })?
                        .into_iter()
                        .map(Message::from)
                        .collect(),
                }
            }
            Request::MqAck { topic, group, seq } => Response::Position {
                position: db
                    .write_txn(actor, move |c, m| mq::ack(c, m, &topic, &group, seq, now))?,
            },
            Request::MqCompact { force } => {
                let r = db.write_txn(actor, move |c, m| mq::compact(c, m, now, force))?;
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
                let running = event != HookEvent::SessionEnded;
                let obs = Observation {
                    session,
                    event: event.into(),
                    finished_tool: tool,
                    message,
                    cwd,
                    owner,
                    instance,
                };
                db.write_txn(actor, move |c, m| {
                    let applied = notify::observe(c, m, &obs, now)?;
                    // After `observe`, which has refused anything malformed, so this cannot cost the
                    // notification its transaction on the same input.
                    if running {
                        let process = claude_session::Process {
                            session: &obs.session,
                            pid: &obs.owner,
                            instance: &obs.instance,
                        };
                        claude_session::seen(c, m, &process, &obs.cwd, now)?;
                    }
                    Ok(applied)
                })?
                .into()
            }
            Request::NotifyOpenSessions {} => Response::Sessions {
                sessions: db
                    .read(notify::open_sessions)?
                    .into_iter()
                    .map(|r| NotifySession {
                        state: r.state().as_str().to_owned(),
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
                .write_txn(actor, move |c, m| {
                    notify::gone(c, m, &session, &owner, &instance, now)
                })?
                .into(),
            Request::SessionStarted {
                session,
                source,
                pid,
                instance,
                cwd,
            } => {
                let was = db.write_txn(actor, move |c, m| {
                    let process = claude_session::Process {
                        session: &session,
                        pid: &pid,
                        instance: &instance,
                    };
                    claude_session::started(c, m, &process, &cwd, &source, now)
                })?;
                Response::SessionStart {
                    was: was.as_str().to_owned(),
                }
            }
            Request::SessionEnded {
                session,
                reason,
                pid,
                instance,
            } => Response::SessionEnd {
                outcome: db
                    .write_txn(actor, move |c, m| {
                        let process = claude_session::Process {
                            session: &session,
                            pid: &pid,
                            instance: &instance,
                        };
                        claude_session::ended(c, m, &process, &reason, now)
                    })?
                    .as_str()
                    .to_owned(),
            },
            Request::SessionGone {
                session,
                pid,
                instance,
            } => Response::SessionGone {
                ended: db.write_txn(actor, move |c, m| {
                    let process = claude_session::Process {
                        session: &session,
                        pid: &pid,
                        instance: &instance,
                    };
                    claude_session::gone(c, m, &process, now)
                })?,
            },
            Request::SessionList { all, after } => {
                let after = after.as_deref().map(SessionCursor::decode).transpose()?;
                let page = db.read(move |c| claude_session::list(c, all, after.as_ref()))?;
                Response::ClaudeSessions {
                    sessions: page.rows.into_iter().map(ClaudeSession::from).collect(),
                    next: page.next.as_ref().map(SessionCursor::encode),
                }
            }
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
            Request::TaskWhy { uid } => {
                let (entries, truncated) = db.read_with(move |c| {
                    let entries = tasks::why(c, &uid, &mut budget)?;
                    Ok::<_, ApiError>((entries, budget.exhausted()))
                })?;
                Response::History { entries, truncated }
            }
            Request::TaskAdd(ask) => {
                let server_home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                let roots = self.file_roots.clone();
                let scope = principal.scope;
                match db.write_txn_with(actor, move |c, m| {
                    tasks::add(c, m, &ask, server_home.as_deref(), roots.as_ref(), scope)
                }) {
                    Ok(added) => Response::Added { added },
                    Err(tasks::AddFailure::NeedsGlobalBacklogAssent) => {
                        Response::NeedsGlobalBacklogAssent {}
                    }
                    Err(tasks::AddFailure::Refused(e)) => return Err(e),
                }
            }
            Request::TaskSet {
                uid,
                status,
                priority,
                due,
            } => {
                let roots = self.file_roots.clone();
                task_write(db, actor, uid, move |c, m, uid| {
                    tasks::set(
                        c,
                        m,
                        uid,
                        status.as_deref(),
                        priority,
                        due.as_deref(),
                        roots.as_ref(),
                    )
                })?;
                Response::Applied {}
            }
            Request::TaskEdit { uid, text, append } => {
                let roots = self.file_roots.clone();
                Response::Edited {
                    file_backed: task_write(db, actor, uid, move |c, m, uid| {
                        tasks::edit(c, m, uid, &text, append, roots.as_ref())
                    })?,
                }
            }
            Request::TaskTag {
                uid,
                facet_value,
                mode,
            } => {
                let roots = self.file_roots.clone();
                task_write(db, actor, uid, move |c, m, uid| {
                    tasks::tag(c, m, uid, &facet_value, mode, roots.as_ref())
                })?;
                Response::Applied {}
            }
            Request::TaskDepend { uid, dep } => {
                let roots = self.file_roots.clone();
                task_write(db, actor, uid, move |c, m, uid| {
                    tasks::depend(c, m, uid, &dep, roots.as_ref())
                })?;
                Response::Applied {}
            }
            Request::TaskUndepend { uid, dep } => {
                let roots = self.file_roots.clone();
                task_write(db, actor, uid, move |c, m, uid| {
                    tasks::undepend(c, m, uid, &dep, roots.as_ref())
                })?;
                Response::Applied {}
            }
            Request::TaskPlace { uid, ns, home } => {
                let roots = self.file_roots.clone();
                let scope = principal.scope;
                task_write(db, actor, uid, move |c, m, uid| {
                    tasks::place(c, m, uid, &ns, home, roots.as_ref(), scope)
                })?;
                Response::Applied {}
            }
            Request::TaskUnplace { uid, ns } => {
                let roots = self.file_roots.clone();
                Response::Unplaced {
                    removed: task_write(db, actor, uid, move |c, m, uid| {
                        tasks::unplace(c, m, uid, &ns, roots.as_ref())
                    })?,
                }
            }
            Request::TaskBind { uid, sync } => {
                let roots = self.file_roots.clone();
                task_write(db, actor, uid, move |c, m, uid| {
                    tasks::bind(c, m, uid, sync.as_deref(), roots.as_ref())
                })?;
                Response::Applied {}
            }
            Request::TaskClaim { uid, owner } => {
                let roots = self.file_roots.clone();
                Response::Claimed {
                    claimed: task_write(db, actor, uid, move |c, m, uid| {
                        tasks::claim(c, m, uid, &owner, roots.as_ref())
                    })?,
                }
            }
            Request::TaskRelease { uid, owner } => {
                let roots = self.file_roots.clone();
                Response::Released {
                    released: task_write(db, actor, uid, move |c, m, uid| {
                        tasks::release(c, m, uid, &owner, roots.as_ref())
                    })?,
                }
            }
            Request::IngestText(ask) => Response::Ingested {
                ingested: ingest::ingest(db, actor, self.embedder.as_ref(), &ask)?,
            },
            Request::TaskFacts { uid } => Response::TaskState {
                state: {
                    let roots = self.file_roots.clone();
                    db.read_with(move |c| sessions::facts(c, &uid, roots.as_ref()))?
                },
            },
            Request::TaskByBranch { repo } => Response::BranchTasks {
                tasks: db.read_with(move |c| sessions::by_branch(c, &repo))?,
            },
            Request::TaskStart(ask) => {
                let roots = self.file_roots.clone();
                let uid = ask.uid.clone();
                Response::Taken {
                    taken: task_write(db, actor, uid, move |c, m, _| {
                        sessions::start(c, m, &ask, roots.as_ref())
                    })?,
                }
            }
            Request::TaskTake(ask) => {
                let roots = self.file_roots.clone();
                let uid = ask.uid.clone();
                Response::Taken {
                    taken: task_write(db, actor, uid, move |c, m, _| {
                        sessions::take(c, m, &ask, roots.as_ref())
                    })?,
                }
            }
            Request::TaskLocate { uid, owner, place } => {
                let roots = self.file_roots.clone();
                Response::Taken {
                    taken: task_write(db, actor, uid, move |c, m, uid| {
                        sessions::locate(c, m, uid, &owner, &place, roots.as_ref())
                    })?,
                }
            }
            Request::TaskAbandon { uid, observed } => {
                let roots = self.file_roots.clone();
                Response::Abandoned {
                    abandoned: task_write(db, actor, uid, move |c, m, uid| {
                        sessions::abandon(c, m, uid, observed.as_deref(), roots.as_ref())
                    })?,
                }
            }
            Request::RepoGate { repo } => Response::Gate {
                gate: db.read_with(move |c| sessions::gate(c, &repo))?,
            },
            Request::SessionState { session } => {
                // Held to the rule every other `session.*` op applies, so a malformed id is refused
                // rather than answered `unknown`.
                if !jkb_types::is_session_id(&session) {
                    return Err(ApiError::with_code(
                        ErrorCode::Invalid,
                        format!("{session:?} is not a session id"),
                    ));
                }
                Response::SessionIs {
                    state: db.read(move |c| claude_session::state(c, &session))?.into(),
                }
            }
            Request::RemovalAdd { removal } => {
                let roots = self.file_roots.clone();
                Response::RemovalAdded {
                    id: db.write_txn_with(actor, move |c, m| {
                        removals::add(c, m, removal, actor, roots.as_ref())
                    })?,
                }
            }
            Request::RemovalList { after } => {
                let (records, next) = db.read_with(move |c| removals::list(c, after))?;
                Response::Removals { records, next }
            }
            Request::RemovalArchived { id, archive, at } => {
                let roots = self.file_roots.clone();
                Response::Changed {
                    changed: db.write_txn_with(actor, move |c, m| {
                        removals::archived(c, m, id, &archive, at, roots.as_ref())
                    })?,
                }
            }
            Request::RemovalCancel { ids } => {
                let roots = self.file_roots.clone();
                Response::RemovalsCancelled {
                    cancelled: db.write_txn_with(actor, move |c, m| {
                        removals::cancel(c, m, &ids, roots.as_ref())
                    })?,
                }
            }
            Request::TaskLand { uid, landed } => {
                let roots = self.file_roots.clone();
                Response::Landing {
                    landing: task_write(db, actor, uid, move |c, m, uid| {
                        sessions::land(c, m, uid, &landed, roots.as_ref())
                    })?,
                }
            }
            // Admission asked who may land; this asks what `task.land` itself would refuse before
            // recording — the same function, so the answer cannot drift from the record's.
            Request::TaskLandCheck { uid, landed } => {
                let roots = self.file_roots.clone();
                db.read_with(move |c| sessions::land_check(c, &uid, &landed, roots.as_ref()))?;
                Response::LandAdmitted {}
            }
            Request::TaskLanded { uid, landed } => {
                let roots = self.file_roots.clone();
                Response::Landing {
                    landing: task_write(db, actor, uid, move |c, m, uid| {
                        sessions::landed(c, m, uid, &landed, roots.as_ref())
                    })?,
                }
            }
            Request::TaskReviewFile(ask) => {
                let (who, scope) = (principal.label.clone(), principal.scope);
                Response::ReviewFiled {
                    filed: db
                        .write_txn_with(actor, move |c, m| review::file(c, m, &ask, &who, scope))?,
                }
            }
            Request::TaskReviewRecord(ask) => {
                let roots = self.file_roots.clone();
                Response::ReviewRecorded {
                    recording: {
                        let who = principal.label.clone();
                        let scope = principal.scope;
                        // Only the operator records a round its findings reached the KB for some
                        // other way than a filing (a mount): what a round holds becomes the scope
                        // of the task's workers.
                        let operator = principal.is_operator();
                        db.write_txn_with(actor, move |c, m| {
                            let source = if operator {
                                jkb_core::reviews::RoundSource::AnyNamespace
                            } else {
                                jkb_core::reviews::RoundSource::Filed(&who)
                            };
                            review::record(c, m, &ask, roots.as_ref(), &who, scope, source)
                        })?
                    },
                }
            }
            Request::TaskReviewWaive { uid, sha } => {
                let roots = self.file_roots.clone();
                let who = principal.label.clone();
                db.write_txn_with(actor, move |c, m| {
                    let id = tasks::writable(c, &uid, roots.as_ref())?;
                    jkb_core::reviews::waive(c, m, id, &sha, &who).map_err(ApiError::from)
                })?;
                Response::Applied {}
            }
            Request::TaskRanOnHost { uid, sha } => {
                let roots = self.file_roots.clone();
                let who = principal.label.clone();
                db.write_txn_with(actor, move |c, m| {
                    let id = tasks::writable(c, &uid, roots.as_ref())?;
                    jkb_core::reviews::gate_on_host(c, m, id, &sha, &who).map_err(ApiError::from)
                })?;
                Response::Applied {}
            }
            Request::RoleGrant { role, task, agent } => {
                let p = principal.clone();
                let (grant, token) = db.write_txn_with(actor, move |c, m| {
                    rbac::grant(c, m, &p, &role, task.as_deref(), &agent)
                })?;
                Response::Granted { grant, token }
            }
            Request::RoleRevoke { id } => {
                let p = principal.clone();
                Response::Revoked {
                    count: db.write_txn_with(actor, move |c, m| rbac::revoke(c, m, &p, id))?,
                }
            }
            Request::RoleList { task, all } => Response::Grants {
                listing: db.read_with(move |c| rbac::list(c, task.as_deref(), all))?,
            },
            Request::RoleWhoami {} => {
                let p = principal.clone();
                Response::WhoAmI {
                    whoami: db.read_with(move |c| rbac::whoami(c, &p))?,
                }
            }
            Request::RoleBind { uid } => {
                // The binding was made by `admit`; what is left is to say the task is one this client
                // may write at all.
                let roots = self.file_roots.clone();
                db.read_with(move |c| tasks::writable(c, &uid, roots.as_ref()).map(|_| ()))?;
                Response::Applied {}
            }
            Request::RoleMap { agent_type, role } => {
                db.write_txn_with(actor, move |c, m| {
                    rbac::map(c, m, &agent_type, role.as_deref())
                })?;
                Response::Applied {}
            }
            Request::RoleRotateContainer { keep } => {
                let (grant, token) = db.write_txn_with(actor, move |c, m| {
                    if let Some(t) = keep {
                        if let Some(g) = jkb_core::roles::resolve(c, &t)? {
                            if g.container {
                                return Ok::<_, ApiError>((g, t));
                            }
                        }
                    }
                    let (row, token) = jkb_core::roles::rotate_container(c, m)?;
                    Ok((row, token))
                })?;
                let grant = db.read_with(move |c| {
                    rbac::list(c, None, false)
                        .map(|l| l.grants.into_iter().find(|g| g.id == grant.id))
                })?;
                let grant = grant.ok_or_else(|| {
                    ApiError::with_code(ErrorCode::Internal, "the new credential vanished")
                })?;
                Response::Granted { grant, token }
            }
            Request::WorkflowShow { uid } => {
                let p = principal.clone();
                Response::Workflow {
                    workflow: Box::new(db.read_with(move |c| rbac::show(c, &p, &uid))?),
                }
            }
            Request::WorkflowFire {
                uid,
                event,
                reason,
                to,
            } => {
                let p = principal.clone();
                let roots = self.file_roots.clone();
                Response::WorkflowMoved {
                    outcome: db.write_txn_with(actor, move |c, m| {
                        tasks::writable(c, &uid, roots.as_ref())?;
                        rbac::fire(c, m, &p, &uid, &event, reason.as_deref(), to.as_deref())
                    })?,
                }
            }
            Request::WorkflowObserve { uid } => {
                let p = principal.clone();
                let roots = self.file_roots.clone();
                Response::WorkflowMoved {
                    outcome: db.write_txn_with(actor, move |c, m| {
                        tasks::writable(c, &uid, roots.as_ref())?;
                        rbac::observe(c, m, &p, &uid)
                    })?,
                }
            }
            Request::WorkflowSet { uid, strategy } => {
                let a = principal.actor();
                let roots = self.file_roots.clone();
                db.write_txn_with(actor, move |c, m| {
                    let id = tasks::writable(c, &uid, roots.as_ref())?;
                    jkb_core::workflow::store::set_strategy(c, m, id, &strategy, &a)
                        .map_err(ApiError::from)
                })?;
                Response::Applied {}
            }
            Request::WorkflowDefine { name, spec } => Response::Defined {
                version: db.write_txn_with(actor, move |c, m| rbac::define(c, m, &name, &spec))?,
            },
            Request::WorkflowStrategies {} => {
                let (strategies, default) = db.read_with(rbac::strategies)?;
                Response::Strategies {
                    strategies,
                    default,
                }
            }
            Request::WorkflowGraph { uid, strategy } => Response::WorkflowGraph {
                graph: Box::new(db.read_with(move |c| {
                    workflows::graph(c, uid.as_deref(), strategy.as_deref())
                })?),
            },
            Request::WorkflowAgents {} => Response::WorkflowAgents {
                agents: db.read_with(workflows::list)?,
            },
            Request::WorkflowAgent {
                name,
                packaged,
                version,
                vars,
            } => {
                let (agent, rendered) = db.read_with(move |c| {
                    workflows::show(c, &name, packaged, version, vars.as_ref())
                })?;
                Response::WorkflowAgent {
                    agent: Box::new(agent),
                    rendered,
                    wrote: false,
                }
            }
            Request::WorkflowAgentCopy {
                from,
                packaged,
                as_name,
            } => Response::WorkflowAgent {
                agent: Box::new(db.write_txn_with(actor, move |c, m| {
                    workflows::copy(c, m, &from, packaged, as_name.as_deref())
                })?),
                rendered: None,
                wrote: true,
            },
            Request::WorkflowAgentSet { name, edit } => {
                let (agent, wrote) =
                    db.write_txn_with(actor, move |c, m| workflows::set(c, m, &name, edit))?;
                Response::WorkflowAgent {
                    agent: Box::new(agent),
                    rendered: None,
                    wrote,
                }
            }
            Request::DesignList { repo } => Response::Designs {
                designs: db.read_with(move |c| designs::list(c, repo.as_deref()))?,
            },
            Request::DesignCreate { repo, title, body } => Response::DesignCreated {
                design: db.write_txn_with(actor, move |c, m| {
                    designs::create(c, m, &repo, &title, &body)
                })?,
            },
            Request::DesignExport { uid, repo } => Response::DesignExports {
                exports: db
                    .read_with(move |c| designs::export(c, uid.as_deref(), repo.as_deref()))?,
            },
            Request::DesignTarget { uid, path } => Response::DesignMeta {
                design: db
                    .write_txn_with(actor, move |c, m| designs::set_target(c, m, &uid, &path))?,
            },
            Request::DesignSources { uid, sources } => Response::DesignMeta {
                design: db
                    .write_txn_with(actor, move |c, m| designs::add_sources(c, m, &uid, sources))?,
            },
            Request::DesignCat { uid } => Response::DesignText {
                design: Box::new(db.read_with(move |c| designs::cat(c, &uid))?),
            },
            Request::DesignState { uid, since } => {
                let (update, version) =
                    db.read_with(move |c| designs::state(c, &uid, since.as_deref()))?;
                Response::DesignUpdate {
                    update,
                    seq: version.seq,
                    version: version.token(),
                }
            }
            Request::DesignSpans { uid } => Response::DesignSpans {
                spans: db.read_with(move |c| designs::spans(c, &uid))?,
            },
            Request::DesignApply { uid, update } => Response::DesignWritten {
                written: db
                    .write_txn_with(actor, move |c, m| designs::apply(c, m, &uid, &update))?,
            },
            Request::DesignEdit { uid, base, edit } => Response::DesignWritten {
                written: db
                    .write_txn_with(actor, move |c, m| designs::edit(c, m, &uid, &base, edit))?,
            },
            Request::DesignSpan {
                uid,
                base,
                find,
                occurrence,
                reviewer,
            } => {
                let ask = designs::SpanAsk {
                    uid,
                    base,
                    find,
                    occurrence,
                    reviewer,
                };
                Response::DesignWritten {
                    written: db.write_txn_with(actor, move |c, m| designs::span(c, m, &ask))?,
                }
            }
            Request::DesignApprove { span } => {
                let who = principal.clone();
                Response::DesignSpan {
                    span: Box::new(
                        db.write_txn_with(actor, move |c, m| designs::approve(c, m, &span, &who))?,
                    ),
                }
            }
            Request::DesignStage { span, step } => Response::DesignSpan {
                span: Box::new(
                    db.write_txn_with(actor, move |c, m| designs::stage(c, m, &span, &step))?,
                ),
            },
            Request::DesignPlanCreate { uid, title, steps } => Response::DesignPlan {
                plan: Box::new(db.write_txn_with(actor, move |c, m| {
                    designs::plans::create(c, m, &uid, &title, &steps)
                })?),
            },
            Request::DesignPlanStep { plan, text } => Response::DesignPlan {
                plan: Box::new(db.write_txn_with(actor, move |c, m| {
                    designs::plans::add_step(c, m, &plan, &text)
                })?),
            },
            Request::DesignPlan { plan } => Response::DesignPlan {
                plan: Box::new(db.read_with(move |c| designs::plans::show(c, &plan))?),
            },
            Request::DesignPlans { uid, all } => Response::DesignPlans {
                list: Box::new(db.read_with(move |c| designs::plans::list(c, &uid, all))?),
            },
            Request::DesignPrompt { ask } => {
                match db.read_with(move |c| designs::prompt(c, &ask))? {
                    designs::PromptAnswer::Discuss(prompt) => Response::DesignPrompt {
                        prompt: Box::new(prompt),
                    },
                    designs::PromptAnswer::Work(prompt) => Response::DesignWorkPrompt {
                        prompt: Box::new(prompt),
                    },
                    designs::PromptAnswer::New(prompt) => Response::DesignNewPrompt {
                        prompt: Box::new(prompt),
                    },
                }
            }
            Request::DesignPromptRecord {
                uid,
                session,
                cwd,
                launch,
                subject,
                title,
            } => {
                let (prompt, wrote) = db.write_txn_with(actor, move |c, m| {
                    designs::prompts::record(
                        c,
                        m,
                        designs::prompts::RecordAsk {
                            uid,
                            session,
                            cwd,
                            launch,
                            subject,
                            title,
                        },
                    )
                })?;
                Response::DesignPromptRecorded {
                    prompt: Box::new(prompt),
                    wrote,
                }
            }
            Request::DesignPrompts { uid } => {
                let prompts = db.read_with({
                    let uid = uid.clone();
                    move |c| designs::prompts::list(c, &uid)
                })?;
                Response::DesignPrompts { uid, prompts }
            }
            Request::DesignPromptOf { session } => {
                let prompt = db.read_with({
                    let session = session.clone();
                    move |c| designs::prompts::of_session(c, &session)
                })?;
                Response::DesignPromptOf {
                    session,
                    prompt: prompt.map(Box::new),
                }
            }
            Request::DesignCompact { uid } => {
                let done = db.write_txn_with(actor, move |c, m| {
                    jkb_core::design::compact(c, m, &uid).map_err(ApiError::from)
                })?;
                Response::DesignCompacted {
                    through: done.through,
                    removed: done.removed,
                }
            }
            Request::AttestMint {
                session,
                agent_id,
                tool_use_id,
                agent_type,
            } => {
                let tickets = self.tickets.as_ref().ok_or_else(|| {
                    ApiError::with_code(
                        ErrorCode::Unsupported,
                        "attestation tickets are minted by jkb serve, not an in-process backend",
                    )
                })?;
                Response::Ticket {
                    token: rbac::mint_ticket(
                        tickets,
                        principal,
                        &session,
                        agent_id.as_deref(),
                        agent_type.as_deref(),
                        &tool_use_id,
                    )?,
                }
            }
            Request::AttestRelease {
                session,
                agent_id,
                tool_use_id,
            } => Response::TicketsReleased {
                count: match &self.tickets {
                    Some(t) => t.release(&session, agent_id.as_deref(), tool_use_id.as_deref())?,
                    None => 0,
                },
            },
            Request::TaskClaims { after } => {
                let (claims, next) = db.read_with(move |c| claims::claims(c, after))?;
                Response::Claims { claims, next }
            }
            Request::TaskReclaim { dead } => {
                let roots = self.file_roots.clone();
                Response::Reclaimed {
                    reclaimed: db.write_txn_with(actor, move |c, m| {
                        // Served with file roots means served to a client of `jkb serve`, which runs
                        // here: this process's host is the daemon's.
                        let server_host = jkb_core::host::name();
                        let asker = match &roots {
                            Some(roots) => claims::Asker::Remote {
                                server_host: &server_host,
                                roots,
                            },
                            None => claims::Asker::Local,
                        };
                        claims::reclaim(c, m, &dead, asker)
                    })?,
                }
            }
            Request::TaskStaging { repo, all } => {
                let (tasks, truncated) = db.read_with(move |c| {
                    let tasks = staging::staging(c, &repo, all, &mut budget)?;
                    Ok::<_, ApiError>((tasks, budget.exhausted()))
                })?;
                Response::StagingTasks { tasks, truncated }
            }
            Request::ItemShow { uid, preview } => Response::Item {
                item: Box::new(db.read_with(move |c| items::show(c, &uid, preview))?),
            },
            Request::ItemRm { uid, force } => {
                let roots = self.file_roots.clone();
                Response::ItemRemoved {
                    removed: db.write_txn_with(actor, move |c, m| {
                        items::remove(c, m, &uid, force, roots.as_ref())
                    })?,
                }
            }
            Request::KbRelated {
                uid,
                edges,
                depth,
                direction,
            } => {
                let (rows, truncated, at_node_cap) = db.read_with(move |c| {
                    let (rows, capped) =
                        items::related(c, &uid, &edges, depth, direction, &mut budget)?;
                    Ok::<_, ApiError>((rows, budget.exhausted(), capped))
                })?;
                Response::Related {
                    rows,
                    truncated,
                    at_node_cap,
                }
            }
            Request::KbBlobs { contains, limit } => {
                let (blobs, truncated) = db.read_with(move |c| {
                    let blobs = items::blobs(c, contains.as_deref(), limit, &mut budget)?;
                    Ok::<_, ApiError>((blobs, budget.exhausted()))
                })?;
                Response::Blobs { blobs, truncated }
            }
            Request::KbBlob { prefix } => {
                let (hash, text) = db.read_with(move |c| items::blob_text(c, &prefix))?;
                Response::Blob { hash, text }
            }
            Request::InvRead(ask) => {
                let (answer, truncated) = db.read_with(move |c| {
                    let answer = inv::read(c, &ask, &mut budget)?;
                    Ok::<_, ApiError>((answer, budget.exhausted()))
                })?;
                Response::Inv { answer, truncated }
            }
            Request::InvWrite(ask) => {
                let roots = self.file_roots.clone();
                Response::Inv {
                    answer: db.write_txn_with(actor, move |c, m| {
                        inv::write(c, m, &ask, roots.as_ref())
                    })?,
                    truncated: false,
                }
            }
            Request::TaskPrFacts { uid } => {
                let roots = self.file_roots.clone();
                Response::PrFacts {
                    facts: db.read_with(move |c| prs::facts(c, &uid, roots.as_ref()))?,
                }
            }
            Request::TaskOpenInRepo { repo } => {
                let (uids, truncated) = db.read_with(move |c| {
                    let uids = prs::open_in_repo(c, &repo, &mut budget)?;
                    Ok::<_, ApiError>((uids, budget.exhausted()))
                })?;
                Response::Uids { uids, truncated }
            }
            Request::TaskPrRecord { uid, number } => {
                let roots = self.file_roots.clone();
                task_write(db, actor, uid, move |c, m, uid| {
                    prs::record(c, m, uid, number, roots.as_ref())
                })?;
                Response::Applied {}
            }
            Request::TaskCloseMerged {
                uid,
                merged,
                observed,
                pr,
                dry_run,
            } => {
                let roots = self.file_roots.clone();
                let ask = prs::CloseAsk {
                    merged,
                    observed,
                    pr,
                    dry_run,
                };
                Response::Closed {
                    refusal: task_write(db, actor, uid, move |c, m, uid| {
                        prs::close_merged(c, m, uid, &ask, roots.as_ref())
                    })?,
                }
            }
            Request::KbContext { item, n } => {
                let reads = db.clone();
                let chunks = kb::context(&reads, item, n, &mut budget)?;
                Response::Context {
                    chunks,
                    truncated: budget.exhausted(),
                }
            }
            Request::ViewList {} => {
                let (views, truncated) = db.read_with(move |c| {
                    let views = kb::views(c, &mut budget)?;
                    Ok::<_, ApiError>((views, budget.exhausted()))
                })?;
                Response::Views { views, truncated }
            }
            Request::ViewRun { name, limit } => {
                let (items, truncated) = db.read_with(move |c| {
                    let items = kb::run_view(c, &name, limit, &mut budget)?;
                    Ok::<_, ApiError>((items, budget.exhausted()))
                })?;
                Response::Items { items, truncated }
            }
            Request::NsList { scope } => {
                let (paths, truncated) = db.read_with(move |c| {
                    let paths = namespaces::list(c, scope.as_deref(), &mut budget)?;
                    Ok::<_, ApiError>((paths, budget.exhausted()))
                })?;
                Response::Namespaces { paths, truncated }
            }
            Request::NsMv { from, to } => {
                let roots = self.file_roots.clone();
                Response::Moved {
                    count: db.write_txn_with(actor, move |c, m| {
                        namespaces::mv(c, m, &from, &to, roots.as_ref())
                    })?,
                }
            }
            Request::KbHistory { path, home } => {
                let server_home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                let (uri, versions, truncated) = db.read_with(move |c| {
                    let (uri, versions) =
                        items::history(c, &path, &home, server_home.as_deref(), &mut budget)?;
                    Ok::<_, ApiError>((uri, versions, budget.exhausted()))
                })?;
                Response::Versions {
                    uri,
                    versions,
                    truncated,
                }
            }
            Request::KbHealth {} => Response::Health {
                health: db.read_with(health::health)?,
            },
            Request::TaskReviewFindings { namespaces } => Response::ReviewFindings {
                findings: db.read_with(move |c| sessions::review_findings(c, &namespaces))?,
            },
            Request::RemovalDrop { id } => {
                let roots = self.file_roots.clone();
                Response::Changed {
                    changed: db.write_txn_with(actor, move |c, m| {
                        removals::drop_record(c, m, id, roots.as_ref())
                    })?,
                }
            }
            Request::LeaseGet { name } => Response::Lease {
                lease: db.read_with(move |c| removals::lease_get(c, &name))?,
            },
            Request::LeaseTake {
                name,
                holder,
                displace,
            } => Response::Changed {
                changed: db.write_txn_with(actor, move |c, m| {
                    removals::lease_take(c, m, &name, &holder, displace.as_deref())
                })?,
            },
            Request::LeaseRelease { name, holder } => Response::Changed {
                changed: db.write_txn_with(actor, move |c, m| {
                    removals::lease_release(c, m, &name, &holder)
                })?,
            },
            Request::LeaseBreak { name } => {
                let roots = self.file_roots.clone();
                Response::LeaseBroken {
                    holder: db.write_txn_with(actor, move |c, m| {
                        removals::lease_break(c, m, &name, roots.as_ref())
                    })?,
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
