//! `jkb` — command-line interface for the jkb knowledge base (Section 12).
//!
//! Thin edge over the library crates: `clap` parses subcommands, each wires to
//! `jkb-core`/`-ingest`/`-search`/`-sync`, and results print as human lines or
//! `--json`. Errors collapse into `anyhow` here (libraries use `thiserror`).
//! Read/task/query commands default their namespace scope to the mount covering the
//! current directory (design D19), overridable with `--global`.

mod archive;
mod atomic;
mod commands;
mod doctor;
mod gitrepo;
mod item_cli;
mod mq_cli;
mod notify;
mod ops_cli;
mod output;
mod owner;
mod pr;
mod presence;
mod remote;
mod repo;
mod review;
mod service;
mod session;
mod session_cli;
mod staging;
mod task_cli;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use jkb_api::kb::SearchRoute;

use jkb_core::lifecycle;
use jkb_core::{edge, investigation, item, mount, ns, nstype, tag, task, undo, view, Db};
use jkb_embed::{OllamaConfig, OllamaEmbedder};
use jkb_fsm::Fact;
use jkb_ingest::Pipeline;
use jkb_types::{ConflictPolicy, EdgeType, Embedder, ItemId, Resolution, SyncMode};

/// A local-first, agent-native knowledge base.
#[derive(Parser)]
#[command(name = "jkb", version, about)]
struct Cli {
    /// Path to the jkb database (default: `$JKB_DB` or `~/.jkb/jkb.db`).
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Ignore ambient (cwd-based) namespace scoping.
    #[arg(long, global = true)]
    global: bool,
    #[command(subcommand)]
    command: Command,
}

/// `jkb notify` verbs.
#[derive(Subcommand)]
enum NotifyCmd {
    /// Read a hook payload on stdin and send it to `jkb serve`, which decides what the notification
    /// does; at `SessionStart`, withdraw what provably-gone sessions left. What the hook shim calls.
    /// Silent: failures go to `~/.jkb/logs/notify-hook.log`.
    Hook,
    /// Print the Claude Code hook events this command answers to, for the registration
    /// cross-check in `scripts/tests/notify-hook.test.sh`.
    Events,
    /// Print the queue topic notifications are sent on, for `scripts/setup.sh` (which creates it) and
    /// `scripts/build-notifier.sh` (whose agent subscribes to it) — so the name is spelled once.
    Topic,
    /// List the Claude Code sessions the daemon's registry holds, one line per process holding one:
    /// the live ones, or with `--all` the ended ones too. Asked of `jkb serve`, like the hook.
    Sessions {
        /// Include ended rows, most recently seen first.
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Ingest a file or URL into the knowledge base.
    Ingest {
        /// Path to a local file, or an http(s) URL (rendered via a headless browser).
        path: String,
        /// Namespace to place the document under (default: ambient or `inbox`).
        #[arg(long)]
        ns: Option<String>,
    },
    /// Run a structured query and list the matching items.
    Query {
        /// Query DSL terms, e.g. `kind:task is:ready ns:tasks/**`.
        #[arg(required = true, num_args = 1..)]
        terms: Vec<String>,
        /// Maximum number of results.
        #[arg(long)]
        limit: Option<usize>,
        /// Print only the number of matches (ignores `--limit`).
        #[arg(long)]
        count: bool,
    },
    /// Search (vector / fts / hybrid), optionally with neighbour context.
    Search {
        /// Query DSL terms; `~"…"` is the vector term, bare words are FTS.
        #[arg(required = true, num_args = 1..)]
        terms: Vec<String>,
        /// Which route to use (default: hybrid; fts with `JKB_REMOTE` set, since the daemon embeds
        /// no query text).
        #[arg(long, value_enum)]
        route: Option<RouteArg>,
        /// Maximum number of hits.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Expand each hit into ±N neighbour chunks.
        #[arg(long)]
        context: Option<usize>,
    },
    /// Namespace browsing and moves.
    Ns {
        #[command(subcommand)]
        cmd: NsCmd,
    },
    /// Tag facet browsing and renames.
    Tag {
        #[command(subcommand)]
        cmd: TagCmd,
    },
    /// File-sync mounts: create one (bind a namespace to a directory) or list them.
    Mount {
        #[command(subcommand)]
        cmd: MountCmd,
    },
    /// Reconcile a mount (one-shot, or `--watch`). With no namespace, all mounts.
    Sync {
        /// The mounted namespace (omit to reconcile every mount).
        ns: Option<String>,
        /// Keep watching for changes until interrupted (Ctrl-C).
        #[arg(long)]
        watch: bool,
        /// Override the mount's conflict policy for THIS RUN only, to get a stuck file
        /// moving without editing the mount.
        #[arg(long, value_enum, conflicts_with = "watch")]
        conflict: Option<PolicyArg>,
    },
    /// Staging branches: what is in flight, and where it will land.
    ///
    /// A staging branch is the branch a batch of tasks lands on before it reaches trunk —
    /// the same thing `/task-swarm` calls its integration branch. It is derived from the land
    /// targets recorded for branches, plus git; the branch itself is never stored (D38.1).
    Staging {
        #[command(subcommand)]
        cmd: StagingCmd,
    },
    /// Install/print the sync watcher as an OS service (launchd/systemd).
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Install jkb's bundled Claude Code slash commands (`/jkb-…`) for this machine.
    Commands {
        #[command(subcommand)]
        cmd: CommandsCmd,
    },
    /// Task DAG: quick-add and the ready frontier.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Saved views.
    View {
        #[command(subcommand)]
        cmd: ViewCmd,
    },
    /// Revert the last (or a named) transaction.
    Undo {
        /// The transaction id to undo (default: the most recent).
        txn: Option<i64>,
    },
    /// Embed content-bearing items not yet in the vector index (needs the embedder).
    Index {
        /// Remove derived-index rows whose item is gone, instead of embedding. Needs no
        /// embedder, so it works offline.
        #[arg(long)]
        sweep: bool,
    },
    /// Health checks, integrity, and backup.
    Doctor {
        /// Write a consistent copy of the database to this path first (replaced if it exists).
        #[arg(long)]
        backup: Option<PathBuf>,
        /// Apply repairs: clear claims whose owner process no longer exists.
        #[arg(long)]
        fix: bool,
    },
    /// Decide what a Claude Code notification hook event should do, from the payload on stdin.
    ///
    /// Runs before the database is opened, because it needs none and fires after every tool call
    /// (design N7).
    Notify {
        #[command(subcommand)]
        cmd: NotifyCmd,
    },
    /// Run the MCP server over stdio (read + audited write tools).
    Mcp,
    /// The message queue: topics, sends, NDJSON subscriptions (design r3.2 Q6).
    Mq {
        #[command(subcommand)]
        cmd: mq_cli::MqCmd,
    },
    /// Serve the knowledge base's operations over HTTP for processes that must not open the
    /// database themselves — the dev container's `jkb` (design r3.2 H3). Runs on the host, usually as
    /// the `com.jkb.serve` service; writes a fresh bearer token beside the database each start.
    Serve {
        /// Where to listen. An unspecified address (0.0.0.0, ::) is refused.
        #[arg(long, default_value = jkb_daemon::DEFAULT_ADDR)]
        addr: std::net::SocketAddr,
        /// Where to write the token (default: `~/.jkb/daemon/<port>/token`, whichever database is
        /// served; refused on a filesystem shared with another kernel, such as the dev container's
        /// view of the host's `~/.jkb`).
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// List the direct children of a namespace (sub-namespaces + items homed there) —
    /// the lazy tree-expansion primitive for the UI. Omit `path` for top-level namespaces.
    Ls {
        /// The namespace whose children to list (default: top-level namespaces).
        path: Option<String>,
        /// Show terminal (`done`/`cancelled`) tasks, and count `chunk` items in the
        /// per-folder totals. Chunks are always reachable by expanding their document.
        #[arg(short = 'a', long)]
        all: bool,
        /// Long format: kind, status, and namespace/uid per row.
        #[arg(short = 'l', long)]
        long: bool,
        /// Recurse into sub-namespaces (depth-first).
        #[arg(short = 'R', long)]
        recursive: bool,
        /// Sort by most-recently-updated instead of by name.
        #[arg(short = 't', long)]
        time: bool,
    },
    /// Literal-substring content search over a namespace subtree (grep semantics). Exit 0
    /// if any item matched, 1 if none. `[path]` scopes the search (default: ambient/cwd).
    Grep {
        /// The substring to find (literal, not a regex).
        pattern: String,
        /// Namespace subtree to search (default: ambient scope, or everything with --global).
        path: Option<String>,
        /// Case-insensitive matching.
        #[arg(short = 'i', long)]
        ignore_case: bool,
        /// List only the matching items' uids, not the matching lines.
        #[arg(short = 'l', long = "files-with-matches")]
        names_only: bool,
        /// Print only a count of matching items.
        #[arg(short = 'c', long)]
        count: bool,
    },
    /// Print an item's full content to stdout (like `cat`). A convenience over
    /// `item show --preview` for piping a task/note/document body to a tool or an agent.
    Cat {
        /// The item uid.
        uid: String,
    },
    /// Recursive namespace tree (like `tree`), with a leaf count per folder. One call maps
    /// a whole subtree so an agent can orient before drilling in. Omit `path` for the roots.
    Tree {
        /// The namespace to root the tree at (default: top-level roots).
        path: Option<String>,
        /// Show terminal (`done`/`cancelled`) items, and count `chunk` items in the
        /// per-folder totals. Chunks are always reachable by expanding their document.
        #[arg(short = 'a', long)]
        all: bool,
        /// Maximum depth to descend (default: 4; deeper folders show `…`). Raise for more.
        #[arg(long)]
        depth: Option<usize>,
    },
    /// Structured item search by kind/tag/status over a namespace subtree — the typed
    /// complement to `grep`'s text search. Sugar over the query DSL with familiar flags.
    Find {
        /// Namespace subtree to search (default: ambient scope, or all with --global).
        path: Option<String>,
        /// Restrict to a kind (e.g. `task`, `document`, `note`).
        #[arg(long)]
        kind: Option<String>,
        /// Require a tag `facet=value` (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Restrict to a task status (e.g. `open`, `done`).
        #[arg(long)]
        status: Option<String>,
        /// Maximum number of results.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// The most-recently-updated items in a subtree — what changed lately. Sugar for a
    /// time-sorted listing so an agent can catch up quickly.
    Recent {
        /// Namespace subtree (default: ambient scope, or everything with --global).
        path: Option<String>,
        /// How many to show (default: 20).
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Compact metadata for one item (kind, namespace, tags, sizes, timestamps) — the quick
    /// `stat`, without the body. Use `jkb cat`/`item show` for content.
    Stat {
        /// The item uid.
        uid: String,
    },
    /// Print a one-page cheat-sheet of the agent-facing command surface (verbs, flags,
    /// exit-code and `--json` conventions). Start here when driving jkb from an agent.
    Guide,
    /// Inspect items (generic, kind-aware).
    Item {
        #[command(subcommand)]
        cmd: ItemCmd,
    },
    /// Walk the typed edge graph out from one item — the traversal read. Reconstructs
    /// context an item's own body doesn't carry: what it depends on, what killed it, what
    /// it answers. `--edge` narrows to specific edge types (repeatable).
    Related {
        /// The item uid to start from.
        uid: String,
        /// Only follow these edge types (repeatable; default: any).
        #[arg(long = "edge")]
        edges: Vec<String>,
        /// How many hops to walk (default 1 = direct neighbours).
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// Which way to follow edges.
        #[arg(long, value_enum, default_value_t = DirArg::Both)]
        direction: DirArg,
    },
    /// Investigations: open-ended, multi-agent knowledge work over a typed namespace
    /// (frontier / confirmed core / tombstones). Run `jkb inv ls` to see yours.
    Inv {
        #[command(subcommand)]
        cmd: InvCmd,
    },
    /// The content-addressed blob archive. File sync stores the bytes of every version it
    /// settles and blobs are never deleted, so this is a complete history of every synced
    /// file — the recovery path when a sync has written a wrong version over your work.
    Blob {
        #[command(subcommand)]
        cmd: BlobCmd,
    },
    /// A synced file's history: every version the KB has bytes for, newest first.
    /// Pair with `jkb blob cat <hash>` to read or diff any of them.
    History {
        /// The file path (or its `file://` uri).
        path: String,
    },
}

#[derive(Subcommand)]
enum BlobCmd {
    /// List stored blobs, newest first. `--contains` searches their bytes, which is how you
    /// find the version of a file that still has a line you remember.
    Ls {
        /// Only blobs whose bytes contain this text.
        #[arg(long)]
        contains: Option<String>,
        /// Maximum number of blobs (default 20).
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Write a blob's raw bytes to stdout (pipe it to a file or `diff`).
    Cat {
        /// The blake3 hash (a unique prefix is enough).
        hash: String,
    },
}

#[derive(Subcommand)]
enum InvCmd {
    /// List every investigation and its strategy type.
    Ls,
    /// Create a typed investigation and seed its goal unit. `<path>` may be a bare name
    /// (homed at `memory/<repo>/<name>` from the ambient repo, or `memory/<name>` outside
    /// one) or an explicit `memory/…` path.
    New {
        /// The strategy type (`jkb inv ls` prints what is available).
        #[arg(value_name = "TYPE")]
        type_name: String,
        /// The investigation name, or an explicit `memory/…` namespace path.
        path: String,
        /// The root intent. The acceptance predicate is appended for `--accept` presets.
        #[arg(long, num_args = 1..)]
        goal: Vec<String>,
        /// Acceptance preset for `conjecture-attack`: prove / disprove / either.
        #[arg(long)]
        accept: Option<String>,
        /// The goal unit's kind (default: the strategy's own goal kind).
        #[arg(long = "goal-kind")]
        goal_kind: Option<String>,
    },
    /// List the verbs the investigation's strategy provides.
    Verbs {
        /// The investigation namespace.
        ns: String,
    },
    /// List the unit kinds and edge types the investigation's strategy uses.
    Kinds {
        /// The investigation namespace.
        ns: String,
    },
    /// The ranked frontier: live, unblocked units — the work queue. Start here.
    Frontier {
        /// The investigation namespace.
        ns: String,
        /// Include units another agent has already claimed.
        #[arg(long)]
        all: bool,
        /// Maximum number of units.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// The tombstones: dead ends and what killed each — read this BEFORE starting work.
    Tombstones {
        /// The investigation namespace.
        ns: String,
    },
    /// The confirmed core: settled results, the current best model.
    Core {
        /// The investigation namespace.
        ns: String,
    },
    /// The anti-retread check for one unit: dead ends in its neighbourhood.
    Retread {
        /// The unit uid about to be worked on.
        uid: String,
        /// How many hops to search for prior attempts (default 2).
        #[arg(long, default_value_t = 2)]
        depth: usize,
    },
    /// The signed-evidence balance for a unit, itemized by contributing edge.
    Evidence {
        /// The unit uid.
        uid: String,
    },
    /// (Re)write the state-digest reflection unit — the default cold-start read.
    Digest {
        /// The investigation namespace.
        ns: String,
        /// Print the digest without writing the reflection unit.
        #[arg(long)]
        dry_run: bool,
    },
    /// Recompute every unit's resolution from its edges, and report what changed.
    Rollup {
        /// The investigation namespace.
        ns: String,
    },
    /// Apply a strategy verb: the normal way to add to an investigation.
    Do {
        /// The investigation namespace.
        ns: String,
        /// The verb (see `jkb inv verbs <ns>`).
        verb: String,
        /// The new unit's body text.
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// The unit this verb acts on.
        #[arg(long = "on")]
        target: Option<String>,
        /// Weight for a signed evidence edge (`supports`/`contradicts`).
        #[arg(long)]
        weight: Option<f64>,
        /// Extra `facet=value` tag (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Add a unit of an explicit kind, with explicit edges — the escape hatch under `do`.
    Add {
        /// The investigation namespace.
        ns: String,
        /// The unit kind (see `jkb inv kinds <ns>`).
        kind: String,
        /// The unit's body text.
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// An edge from the new unit as `<type>:<target-uid>` (repeatable).
        #[arg(long = "edge")]
        edges: Vec<String>,
        /// Weight applied to the edges (signed evidence).
        #[arg(long)]
        weight: Option<f64>,
        /// A `facet=value` tag (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Link two existing units — including `equivalent_in_strength_to`, the anti-progress
    /// edge, which is a judgement about two existing statements rather than a new unit.
    Link {
        /// The source unit uid.
        src: String,
        /// The edge type.
        edge: String,
        /// The destination unit uid.
        dst: String,
        /// Weight (signed evidence edges only).
        #[arg(long)]
        weight: Option<f64>,
    },
    /// Set a unit's `promise=` rank (the frontier ordering knob).
    Promise {
        /// The unit uid.
        uid: String,
        /// The rank; higher sorts first.
        value: f64,
    },
    /// Set a unit's resolution: unresolved / success / `dead_end` / superseded / abandoned.
    /// A dead end is retained, never deleted — link what killed it so it teaches.
    Resolve {
        /// The unit uid.
        uid: String,
        /// The resolution.
        resolution: String,
    },
    /// Check whether a blocked route may be reopened: only a materially new mechanism,
    /// invariant, construction, or obstruction qualifies (`conjecture-attack`).
    Reopen {
        /// The blocked route's uid.
        route: String,
        /// The uid of the new mechanism/invariant/construction/obstruction.
        #[arg(long)]
        mechanism: String,
    },
    /// Mark observations stale because the code moved (`debugging`): every observation whose
    /// `commit-range=` is not `--window` is excluded from the frontier — never deleted.
    Stale {
        /// The investigation namespace.
        ns: String,
        /// The current commit range, e.g. `def456..HEAD`.
        #[arg(long)]
        window: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum DirArg {
    /// Follow edges away from the item ("what does this point at").
    Out,
    /// Follow edges into the item ("what points at this").
    In,
    /// Both directions.
    Both,
}

impl From<DirArg> for edge::Direction {
    fn from(d: DirArg) -> Self {
        match d {
            DirArg::Out => edge::Direction::Out,
            DirArg::In => edge::Direction::In,
            DirArg::Both => edge::Direction::Both,
        }
    }
}

#[derive(Subcommand)]
enum ItemCmd {
    /// Show one item's details + content. Text-like kinds (task/text/note/markdown) show
    /// in full; heavy kinds (pdf/image) show a bounded preview. `--preview N` caps either.
    Show {
        /// The item uid.
        uid: String,
        /// Max preview characters. Default: unbounded for text-like kinds, 800 otherwise.
        #[arg(long)]
        preview: Option<usize>,
    },
    /// Delete an item and everything that cascades with it (placements, edges, tags, its
    /// binding). Recorded in full, so `jkb undo` puts it all back. Refuses by default to
    /// delete investigation memory (a `dead_end`/`superseded` tombstone, or a unit an edge
    /// records as killed) or a synced-file-backed item that sync would just recreate.
    Rm {
        /// The item uid.
        uid: String,
        /// Delete anyway, past the memory / synced-file guards.
        #[arg(long)]
        force: bool,
    },
    /// Replace (or `--append` to) any item's content.
    Edit {
        /// The item uid.
        uid: String,
        /// New content (omit and pass `--stdin` to read from stdin).
        #[arg(num_args = 0..)]
        text: Vec<String>,
        /// Read the new content from stdin instead of the trailing args.
        #[arg(long)]
        stdin: bool,
        /// Append to the existing content (blank-line separated) instead of replacing.
        #[arg(long)]
        append: bool,
    },
}

#[derive(Subcommand)]
enum MountCmd {
    /// Bind a namespace subtree to a directory for file sync.
    Create {
        /// Namespace to mount.
        ns: String,
        /// Backing directory.
        dir: PathBuf,
        /// Sync direction (default `bidirectional`; kept as-is when re-running).
        #[arg(long, value_enum)]
        mode: Option<ModeArg>,
        /// File-format serializer (default `document`; kept as-is when re-running).
        #[arg(long)]
        serializer: Option<String>,
        /// Include glob (e.g. `**/*.md`). Kept as-is when re-running; clear with
        /// `--no-include`.
        #[arg(long, conflicts_with = "no_include")]
        include: Option<String>,
        /// Drop the stored include glob, syncing every file under the directory.
        #[arg(long)]
        no_include: bool,
        /// Exclude glob. Kept as-is when re-running; clear with `--no-exclude`.
        #[arg(long, conflicts_with = "no_exclude")]
        exclude: Option<String>,
        /// Drop the stored exclude glob.
        #[arg(long)]
        no_exclude: bool,
        /// Conflict policy (default `manual`; kept as-is when re-running).
        #[arg(long, value_enum)]
        policy: Option<PolicyArg>,
    },
    /// List all mounts (namespace → serializer → backing directory).
    Ls,
}

#[derive(Subcommand)]
enum StagingCmd {
    /// List staging branches and the tasks landing on each.
    Ls {
        /// Include branches already merged into trunk (hidden by default: a spent batch
        /// must never be offered as a land target).
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum NsCmd {
    /// List namespaces (children of `scope`, or top-level if omitted).
    Ls { scope: Option<String> },
    /// Create one or more (nested) namespaces if absent. Idempotent — safe to re-run,
    /// e.g. to scaffold the standard roots (`repos tasks media references memory`).
    Mk {
        #[arg(required = true, num_args = 1..)]
        paths: Vec<String>,
    },
    /// Move a subtree to a new path.
    Mv { from: String, to: String },
    /// Remove an empty namespace (no child namespaces or item placements).
    Rm { path: String },
    /// Show or set a namespace's type. A type states what may live in the namespace
    /// (enforced on every write) and, for an investigation strategy, the verbs that
    /// drive it. Inherited by the whole subtree. With no `<type>`, shows the current one.
    Type {
        /// The namespace path. Omit with `--list`.
        path: Option<String>,
        /// The type to apply; omit to show the current one.
        type_name: Option<String>,
        /// List every registered namespace type and exit.
        #[arg(long, conflicts_with_all = ["path", "type_name"])]
        list: bool,
        /// Remove the namespace's own type, reverting it to untyped (it then inherits its
        /// nearest typed ancestor's, if any). Items already placed are untouched.
        #[arg(long, conflicts_with_all = ["type_name", "list"])]
        clear: bool,
    },
}

#[derive(Subcommand)]
enum TagCmd {
    /// List declared facets.
    Ls,
    /// Rename a facet across all applications.
    Rename { old: String, new: String },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Quick-add a task: `"text" !p1 @2026-07-15 +ns #facet=value ^dep-uid`.
    Add {
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// Home the task in the ambient repo's backlog (`tasks/<repo>/.backlog`)
        /// instead of its inbox. Outside a repo, confirms a global `tasks/.backlog`.
        #[arg(long)]
        backlog: bool,
        /// Force a synced file binding into the home's `tasks` mount (errors if none).
        #[arg(long, conflicts_with = "managed")]
        sync: bool,
        /// Force a `managed:` (KB-only) binding, overriding mount inference.
        #[arg(long)]
        managed: bool,
        /// Make this a subtask of `<uid>`: the parent leaves the ready frontier until every
        /// subtask is terminal, so a task too big for one branch is split into the pieces
        /// that get worked. Defaults the new task's home to the parent's.
        #[arg(long)]
        under: Option<String>,
        /// Home the task in this namespace, taken **verbatim**.
        ///
        /// The quick-add `+<ns>` form is re-tokenized on whitespace along with the rest of
        /// the line, so a namespace containing a space (which `ns::normalize` permits, and
        /// which a synced directory named `my change` produces) creates a different, wrong
        /// namespace and swallows the remainder into the title. Pass the path here when it
        /// comes from a picker rather than from a person typing.
        #[arg(long)]
        home: Option<String>,
    },
    /// List the ready frontier (optionally scoped/filtered by DSL terms).
    Next {
        #[arg(num_args = 0..)]
        terms: Vec<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Show a single task in full: metadata and untruncated content.
    Show {
        /// The task uid (the `task:` prefix is optional).
        uid: String,
    },
    /// Edit a task's metadata: any of `--status` / `--priority` / `--due`.
    Set {
        /// The task uid (the `task:` prefix is optional).
        uid: String,
        /// New status (`open`/`in_progress`/`needs_review`/`done`/`cancelled`;
        /// `blocked` is derived and rejected).
        #[arg(long)]
        status: Option<String>,
        /// New priority (lower is more important).
        #[arg(long)]
        priority: Option<i64>,
        /// New ISO due date, e.g. `2026-07-15`.
        #[arg(long)]
        due: Option<String>,
    },
    /// Edit a task's body text: replace it, or `--append` to it. Content comes from
    /// the trailing args, or from stdin with `--stdin` (handy for multi-line notes).
    Edit {
        /// The task uid (the `task:` prefix is optional).
        uid: String,
        /// The new content (omit and pass `--stdin` to read it from stdin).
        #[arg(num_args = 0..)]
        text: Vec<String>,
        /// Read the new content from stdin instead of the trailing args.
        #[arg(long)]
        stdin: bool,
        /// Append to the existing content (blank-line separated) instead of replacing.
        #[arg(long)]
        append: bool,
    },
    /// Add or remove a `facet=value` tag on a task.
    Tag {
        #[command(subcommand)]
        cmd: TaskTagCmd,
    },
    /// Add a `depends_on` edge (cycle-guarded): `<uid>` now depends on `<dep>`.
    Depend {
        /// The dependent task uid.
        uid: String,
        /// The dependency task uid it should wait on.
        dep: String,
    },
    /// Remove a `depends_on` edge from `<uid>` to `<dep>`.
    Undepend {
        /// The dependent task uid.
        uid: String,
        /// The dependency task uid to detach.
        dep: String,
    },
    /// Place a task under a namespace: a reference mirror, or its primary `--home`.
    Place {
        /// The task uid.
        uid: String,
        /// The namespace path to place it under.
        ns: String,
        /// Make this the task's sole primary home (default: a reference mirror).
        #[arg(long)]
        home: bool,
    },
    /// Remove a task's reference (mirror) placement under a namespace (inverse of `place`).
    Unplace {
        /// The task uid.
        uid: String,
        /// The namespace path whose mirror to remove.
        ns: String,
    },
    /// Bind a task to storage: `--managed` (no file) or `--sync <uri>` (a file mount).
    Bind {
        /// The task uid.
        uid: String,
        /// Bind as `managed:` — not written to any repo.
        #[arg(long, conflicts_with = "sync")]
        managed: bool,
        /// Bind to a synced `file://` uri.
        #[arg(long)]
        sync: Option<String>,
    },
    /// Claim a task for an owner (defaults to this process), atomically starting it.
    Claim {
        /// The task uid.
        uid: String,
        /// The liveness-checkable owner id (default: this process's `host:pid`).
        #[arg(long)]
        owner: Option<String>,
    },
    /// List a task's subtasks. Emits the same shape as `jkb ls`, so a tree can expand a
    /// parent into its children with the same parser it uses for a namespace.
    Subtasks {
        /// The parent task uid.
        uid: String,
        /// Include terminal (`done`/`cancelled`) subtasks.
        #[arg(short = 'a', long)]
        all: bool,
    },
    /// Start work: claim the task and record the branch and repo it is being done on, so
    /// `jkb task close-merged` can close it once that branch lands. Both default from the
    /// git repo in the current directory.
    Start {
        /// The task uid.
        uid: String,
        /// The branch (default: the current branch here).
        #[arg(long)]
        branch: Option<String>,
        /// The branch this one was cut from and will land on. Recorded as `--branch`'s land
        /// target, and it is what the cut point is measured against — without it only the
        /// branch's own tip can be measured, which reads as "nothing has happened here" on a
        /// branch that already has commits.
        #[arg(long)]
        onto: Option<String>,
        /// The repo key (default: the basename of this git repo's root).
        #[arg(long)]
        repo: Option<String>,
        /// The liveness-checkable owner id (default: this process's `host:pid`).
        #[arg(long)]
        owner: Option<String>,
    },
    /// Open an isolated session for a task: its own git worktree and branch, claimed so no
    /// other terminal — or swarm run — starts the same task. Re-running returns the same
    /// session, so it is safe to invoke from a button.
    Work {
        /// The task uid.
        uid: String,
        /// The branch this session's work will land on (default: the branch you are on, or
        /// a new one cut from trunk and named after this task).
        #[arg(long)]
        onto: Option<String>,
    },
    /// Land a session: rebase its branch onto the target, fast-forward, run the gate, and on
    /// green mark the task done and clean the session up. Serialized per repo.
    Land {
        /// The task uid.
        #[arg(required_unless_present = "break_lock")]
        uid: Option<String>,
        /// The command that verifies the integrated result (remembered for this repo).
        #[arg(long)]
        gate: Option<String>,
        /// Land without running a gate.
        #[arg(long, conflicts_with = "gate")]
        no_gate: bool,
        /// Keep the session worktree and branch after landing.
        #[arg(long)]
        keep_worktree: bool,
        /// Land without a recorded review. The waiver is recorded on the task, so a
        /// bypass is visible rather than invisible.
        #[arg(long)]
        no_review: bool,
        /// Drop this repo's land lease, whoever holds it, and land nothing — for a holder that is
        /// gone for good but cannot be proven so (another machine, a session never seen to end).
        /// Host only.
        #[arg(long, conflicts_with_all = ["gate", "no_gate", "keep_worktree", "no_review"])]
        break_lock: bool,
    },
    /// File a code review's findings, and record that it ran so `task land` can require one.
    Review {
        #[command(subcommand)]
        cmd: TaskReviewCmd,
    },
    /// Drop a session without landing it: remove the worktree, and release the claim and
    /// reopen the task unless it has already finished or someone else has claimed it. The
    /// branch is kept unless you ask for it to go.
    Abandon {
        /// The task uid.
        uid: String,
        /// Discard uncommitted changes in the session worktree.
        #[arg(long)]
        force: bool,
        /// Also delete the session branch (its commits are lost).
        #[arg(long)]
        delete_branch: bool,
    },
    /// List the task sessions in flight in this repo.
    Sessions,
    /// Show, set, or clear the command that verifies a landing in this repo.
    Gate {
        /// The command to remember (omit to show the current one).
        cmd: Option<String>,
        /// Forget this repo's gate command.
        #[arg(long, conflicts_with = "cmd")]
        clear: bool,
    },
    /// Close tasks whose work is proven to have landed, and whose subtasks are all terminal;
    /// anything else is reported with the reason.
    ///
    /// Two proofs, in this order: a landing **jkb itself recorded** (`jkb task land`, or the
    /// merge queue's `jkb task landed`), and otherwise a recorded pull request in state `MERGED`.
    /// The first is what a locally-grafted branch has, since there is no pull request to ask
    /// about. Either is **spent** once the task has been put back to work since — a reopened task
    /// is not closed again on the strength of the landing it was reopened from.
    ///
    /// Works with merge-commit, squash and rebase merges alike, and needs no cut point, because
    /// it asks GitHub about an id rather than asking the commit graph about a branch name.
    CloseMerged {
        /// Only consider tasks tagged with this repo (default: this git repo's key).
        #[arg(long)]
        repo: Option<String>,
        /// Report what would close without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Record that a branch was grafted onto another — for the merge queue, which is bash and
    /// cannot write the record directly the way `jkb task land` does.
    ///
    /// A landing event is a **trusted** fact — readers act on it without re-deriving anything
    /// from refs — and this verb verifies nothing of its own: the caller performed the graft and
    /// gated it, and is reporting what it did. That is why the recorded event is
    /// `observed_landed` and not `land`, whose guard asks whether jkb *may perform* a graft that
    /// here has already happened. Run by hand for work that did not land, it records a falsehood,
    /// so it is the merge queue's verb rather than a general one.
    Landed {
        /// The branch that was grafted.
        branch: String,
        /// The branch it was grafted onto.
        #[arg(long)]
        onto: String,
    },
    /// Release a task's claim held by an owner (defaults to this process).
    Release {
        /// The task uid.
        uid: String,
        /// The owner id whose claim to release (default: this process's `host:pid`).
        #[arg(long)]
        owner: Option<String>,
    },
    /// Archive session worktrees a landing could not move, and delete aged-out archives. Also
    /// compacts the message queue (idle groups, consumed-and-expired messages); `--dry-run` skips
    /// the compaction, and a database that cannot be opened stops only the compaction.
    ///
    /// A session cannot remove its OWN worktree — Claude Code protects a project's `.claude`
    /// policy files from the agent whose policy they are, and the refusal propagates up to the
    /// directory containing them — so `land` records what it could not move and this finishes
    /// the job from anywhere else. Disposal is a rename into `<repo>/.jkb/archive`, never a
    /// delete; the delete happens here, once an archive is past `--retain-days`.
    Reap {
        /// Delete archives older than this many days.
        #[arg(long, default_value_t = archive::RETAIN_DAYS)]
        retain_days: u64,
        /// Report what would happen and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Drop the sweep's lease when its holder is gone for good.
        ///
        /// There is no automatic escape and there should not be: a holder on another host
        /// cannot be probed, and breaking a live sweeper's lock is what the lock prevents. A
        /// container killed mid-sweep and then rebuilt leaves one nothing can ever clear.
        #[arg(long)]
        break_lock: bool,
        /// Keep sweeping, for the installed service. Ctrl-C stops it.
        #[arg(long)]
        watch: bool,
        /// Seconds between sweeps under `--watch`.
        #[arg(long, default_value_t = 900)]
        interval_secs: u64,
    },
    /// Reclaim claims whose owner is **proven** gone (the deterministic crash-recovery scan).
    /// Keeps every other claim — one whose owner is alive, one in `--keep`, and one whose owner
    /// cannot be established at all, which is reported and never cleared, since treating an
    /// unobtainable answer as "dead" frees a live agent's task. A live coordinator passes its
    /// own owner so it never reclaims its own in-flight work.
    Reclaim {
        /// Owner id(s) to always preserve (repeatable), e.g. this run's own owner.
        #[arg(long)]
        keep: Vec<String>,
    },
    /// Ensure every task homed outside `tasks/` has a `tasks/…` mirror (symbolic link),
    /// so `tasks/**` is the complete task index. Idempotent; sync does this automatically.
    Mirror,
    /// Show how a task reached its current state: every lifecycle transition, who applied it,
    /// and the evidence each one fired on.
    ///
    /// The read this area most obviously lacked. Fourteen must-fix findings in the
    /// `staging-workflow` corpus are a task held in some state with no way to see why, and each
    /// one was a debugging session; the history makes it one command.
    Why {
        /// The task uid.
        uid: String,
    },
    /// Show or record the pull request that will prove this task's work landed.
    ///
    /// With no number, discovers it from the task's branch and records what it finds. The number
    /// is what is kept: it is minted by GitHub and never reused, so once recorded, a branch that
    /// is deleted, renamed or reused cannot change the answer.
    Pr {
        /// The task uid.
        uid: String,
        /// The pull request number (omit to discover it from the task's branch).
        number: Option<i64>,
    },
}

#[derive(Subcommand)]
enum TaskReviewCmd {
    /// File a review's findings as tasks under a namespace of their own, one section per severity
    /// (`must-fix` blocks landing). Reads the reviewer workflow's result — a JSON object with a
    /// `findings` array of `{severity, summary, file, line, scenario, fix}` — from a file, or from
    /// stdin with `-`.
    File {
        /// The namespace to file them under, e.g. `repos/<repo>/codereviews/<folder>`. Must be new.
        #[arg(long)]
        findings: String,
        /// The workflow's result, as JSON (`-` for stdin).
        #[arg(long)]
        from: PathBuf,
    },
    /// Record a review against a branch: tags every task working that branch with the
    /// reviewed SHA and the findings namespace, and moves `in_progress` to `needs_review`.
    Record {
        /// The reviewed branch (default: the current branch here).
        #[arg(long)]
        branch: Option<String>,
        /// The reviewed HEAD (default: this branch's HEAD).
        #[arg(long)]
        sha: Option<String>,
        /// The namespace holding the review's findings.
        #[arg(long)]
        findings: String,
    },
}

#[derive(Subcommand)]
enum TaskTagCmd {
    /// Apply `facet=value` to a task.
    Add {
        /// The task uid.
        uid: String,
        /// The tag as `facet=value`.
        facet_value: String,
    },
    /// Make `facet=value` the facet's **only** value, replacing any others.
    ///
    /// Use for facets with one true answer — `branch=`, `repo=` — where a second value is a
    /// contradiction rather than extra information (design D36.6).
    Set {
        /// The task uid.
        uid: String,
        /// The tag as `facet=value`.
        facet_value: String,
    },
    /// Remove `facet=value` from a task.
    Rm {
        /// The task uid.
        uid: String,
        /// The tag as `facet=value`.
        facet_value: String,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Print the service unit for this platform (a dry run of `install`).
    Print,
    /// Write the service unit and print the command to activate it.
    Install,
    /// Remove the installed service unit.
    Uninstall,
    /// Print every unit `install` writes, one per line as `label<TAB>path<TAB>role` — what setup.sh
    /// activates, so neither the list nor the paths are copied into it by hand.
    Units,
    /// Print where `jkb serve` writes its token for this database (it does so once it is listening).
    TokenPath,
    /// Print the address the `com.jkb.serve` unit listens on, as a `JKB_REMOTE` URL.
    ServeUrl,
}

#[derive(Subcommand)]
enum CommandsCmd {
    /// Write the bundled slash commands into the Claude Code commands directory.
    Install,
    /// Remove the bundled slash commands.
    Uninstall,
    /// List the bundled commands and their install location (a dry run).
    List,
}

#[derive(Subcommand)]
enum ViewCmd {
    /// Save (or overwrite) a named view from DSL terms.
    Save {
        name: String,
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
    },
    /// List saved views.
    Ls,
    /// Run a saved view.
    Run { name: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum RouteArg {
    Vector,
    Fts,
    Hybrid,
}

impl From<RouteArg> for SearchRoute {
    fn from(r: RouteArg) -> Self {
        match r {
            RouteArg::Vector => Self::Vector,
            RouteArg::Fts => Self::Fts,
            RouteArg::Hybrid => Self::Hybrid,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Import,
    Export,
    Bidirectional,
}

impl From<ModeArg> for SyncMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Import => SyncMode::Import,
            ModeArg::Export => SyncMode::Export,
            ModeArg::Bidirectional => SyncMode::Bidirectional,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum PolicyArg {
    DiskWins,
    KbWins,
    Manual,
}

impl From<PolicyArg> for ConflictPolicy {
    fn from(p: PolicyArg) -> Self {
        match p {
            PolicyArg::DiskWins => ConflictPolicy::DiskWins,
            PolicyArg::KbWins => ConflictPolicy::KbWins,
            PolicyArg::Manual => ConflictPolicy::Manual,
        }
    }
}

fn main() {
    if let Err(err) = run(Cli::parse()) {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)] // a flat command dispatcher; one arm per subcommand
fn run(cli: Cli) -> Result<()> {
    // THE ORDER, in one place. (1) `notify`, in every mode — the one dispatch, not one per mode.
    // (2) Remote mode, before anything else with a side effect. (3) Everything that opens a database.
    //
    // (1) `notify` touches no database in any mode, so it is safe ahead of remote mode — its only
    // writes are its own log and the daemon-unreachable marker. It must be: `open_db` verifies the migrations and spawns the writer thread — 110 ms
    // against the real database — and `notify hook` runs after EVERY tool call, the cost design N7
    // measured and rejected; and when a newer branch's migration locks an older binary out of the
    // database, `open_db` fails and nothing would ever withdraw. Ahead of remote mode because remote
    // mode's refusals (`JKB_DB` beside `JKB_REMOTE`) exit before the hook can log, onto a stderr
    // the shim discards — every notification lost silently (stage-5 review).
    if let Command::Notify { cmd } = &cli.command {
        return notify::run(cmd, cli.json);
    }

    // (2) With JKB_REMOTE set this process must never open a database — on the dev container's
    // kernel that is the host's `jkb.db`, which a process on each side of the bind corrupts — so
    // every other command either goes to the daemon or is refused here, at dispatch, before it has
    // run git, written a file or opened anything.
    if let Some(remote) = remote::target() {
        return remote::run(cli, &remote);
    }

    // Keep the bundled Claude Code commands/workflows fresh in the user's config dir
    // (best-effort, silent). Skipped for explicit `jkb commands …` so it never fights the
    // user's own install/uninstall.
    if !matches!(cli.command, Command::Commands { .. }) {
        commands::ensure_installed();
    }

    let db_path = cli.db.clone().unwrap_or_else(default_db_path);
    // THE SWEEP OPENS THE DATABASE PER PASS, never before the loop. `open_db` runs the migrations,
    // which refuse outright when the shared `~/.jkb/jkb.db` carries one this binary does not know —
    // routine across branches here. The host's `com.jkb.reap` unit is whichever binary `setup.sh`
    // last installed, so an open up front turned the one process that finishes every deferred
    // landing into a launchd restart-loop, with the only symptom in reap.log. The sweep's records
    // were beside the database for that reason; they are in it now (tasks S6.4 stage 3), so each
    // pass opens it (`reap_once`), as the queue's compaction does (`compact_queue`), and a failure is
    // reported and never stops the service.
    if let Command::Task {
        cmd: cmd @ TaskCmd::Reap { .. },
    } = cli.command
    {
        let TaskCmd::Reap {
            retain_days,
            dry_run,
            break_lock,
            watch,
            interval_secs,
        } = cmd
        else {
            unreachable!("matched above")
        };
        return cmd_task_reap(
            &db_path,
            ReapFlags {
                retain_days,
                dry_run,
                break_lock,
                watch,
                interval_secs,
            },
            cli.json,
        );
    }
    // The daemon opens the database itself, so that a database it cannot serve still gets a daemon
    // that says why rather than a supervisor restart-loop — see `cmd_serve`.
    if let Command::Serve { addr, token_file } = cli.command {
        return cmd_serve(&db_path, addr, token_file);
    }
    // `jkb service` writes and describes units; it reads no rows. Opened first, a database a newer jkb
    // migrated stopped setup.sh at `service install` — so it never started the daemon that would
    // have said `schema_newer`, and reported the watcher as unwritable instead.
    if let Command::Service { cmd } = cli.command {
        return match cmd {
            ServiceCmd::Print => service::print(&db_path),
            ServiceCmd::Install => service::install(&db_path),
            ServiceCmd::Uninstall => service::uninstall(&db_path),
            ServiceCmd::Units => service::units(&db_path),
            ServiceCmd::ServeUrl => {
                // The unit passes no `--addr`, so it listens on serve's default.
                println!("http://{}", jkb_daemon::DEFAULT_ADDR);
                Ok(())
            }
            ServiceCmd::TokenPath => {
                // The unit passes no `--addr`, so its token is keyed by serve's default port.
                let port = jkb_daemon::DEFAULT_ADDR
                    .parse::<std::net::SocketAddr>()
                    .map_or(7117, |a| a.port());
                println!("{}", service::serve_token_path(port).display());
                Ok(())
            }
        };
    }
    let db = match open_db(&db_path) {
        Ok(db) => db,
        Err(e) => {
            // `jkb mq`'s `--json` rule covers a database that will not open too: a scripted producer
            // branches on `schema_newer` whichever side of the open it meets it on.
            if let Command::Mq { cmd } = &cli.command {
                if cli.json {
                    let code = mq_cli::open_failure_code(&e);
                    mq_cli::print_failure(cmd, &mq_cli::refused(code, format!("{e:#}")));
                }
            }
            return Err(e);
        }
    };
    let json = cli.json;
    let global = cli.global;

    match cli.command {
        // Dispatched above, before the database is opened — this arm is the correct answer if
        // that ever stops happening, not a silent fallthrough to the database path. Which of the
        // two runs is pinned by `notify_needs_no_database` in tests/cli.rs, so the fast path is
        // load-bearing rather than an optimisation someone can quietly drop.
        Command::Notify { cmd } => notify::run(&cmd, json),
        // Ahead of every other arm, and asked through the same predicate remote mode dispatches on:
        // a read ported later is then served by its op here too, rather than by an arm below that
        // still compiles.
        cmd if ops_cli::handles(&cmd) => local_ops(&db, &db_path, cmd, global, json),
        Command::Query { .. }
        | Command::Search { .. }
        | Command::Find { .. }
        | Command::Recent { .. }
        | Command::Ls { .. }
        | Command::Tree { .. }
        | Command::Grep { .. }
        | Command::Cat { .. }
        | Command::Ingest { .. } => {
            anyhow::bail!("internal: a read-set command missed ops_cli's dispatch")
        }
        Command::Ns { cmd } => cmd_ns(&db, cmd, json),
        Command::Tag { cmd } => cmd_tag(&db, cmd, json),
        Command::Mount { cmd } => cmd_mount(&db, cmd, json),
        Command::Sync {
            ns,
            watch,
            conflict,
        } => cmd_sync(
            &db,
            ns.as_deref(),
            watch,
            conflict.map(ConflictPolicy::from),
        ),
        Command::Staging { .. }
        | Command::Stat { .. }
        | Command::Item { .. }
        | Command::Related { .. }
        | Command::Blob { .. }
        | Command::History { .. } => {
            anyhow::bail!("internal: a command served as an op missed ops_cli's dispatch")
        }
        Command::Commands { cmd } => match cmd {
            CommandsCmd::Install => commands::install(),
            CommandsCmd::Uninstall => commands::uninstall(),
            CommandsCmd::List => commands::list(),
        },
        Command::Task { cmd } => cmd_task(&db, &db_path, cmd, json),
        Command::View { cmd } => cmd_view(&db, cmd, json),
        Command::Undo { txn } => cmd_undo(&db, txn),
        Command::Index { sweep } => cmd_index(&db, sweep),
        Command::Doctor { backup, fix } => {
            let backend = jkb_api::LocalBackend::new(db.clone()).with_actor("cli");
            doctor::run(
                &session_cli::Kb::new(&backend),
                Some(&doctor::Host {
                    db: &db,
                    path: &db_path,
                }),
                backup.as_deref(),
                fix,
            )
        }
        Command::Mcp => jkb_mcp::run_stdio(db, embedder()?),
        Command::Mq { cmd } => {
            mq_cli::run(&jkb_api::LocalBackend::new(db).with_actor("cli"), cmd, json)
        }
        Command::Serve { .. } | Command::Service { .. } => {
            unreachable!("dispatched before the database is opened")
        }
        Command::Guide => {
            cmd_guide();
            Ok(())
        }
        Command::Inv { cmd } => cmd_inv(&db, cmd, global, json),
    }
}

// ---- shared helpers -------------------------------------------------------

fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("JKB_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home).join(".jkb").join("jkb.db")
}

fn open_db(path: &Path) -> Result<Db> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory {}", parent.display()))?;
        }
    }
    Db::open_with(path, &[jkb_index::register])
        .with_context(|| format!("opening {}", path.display()))
}

fn embedder() -> Result<Arc<dyn Embedder + Send + Sync>> {
    let e = OllamaEmbedder::new(OllamaConfig::default())?;
    Ok(Arc::new(e))
}

/// The agent read set on this host (`ops_cli`): through a `LocalBackend` over `db`, the same op
/// `jkb serve` answers the dev container with, so the two cannot list different things.
fn local_ops(db: &Db, db_path: &Path, command: Command, global: bool, json: bool) -> Result<()> {
    let mut backend = jkb_api::LocalBackend::new(db.clone()).with_actor("cli");
    // Only a search or an ingest embeds; building the embedder is not a cost `ls` should pay.
    if matches!(command, Command::Search { .. } | Command::Ingest { .. }) {
        backend = backend.with_embedder(embedder()?);
    }
    ops_cli::Ops::new(&backend, global, json, false)
        .with_local(db, db_path)
        .run(command)
}

/// The ambient repo key: the full namespace path of the `file://` mount covering the
/// current directory (design D26.2), or `None` outside any mount. Tasks home under
/// `tasks/<repo>/…` using this key. Unlike `ops_cli::Ops::ambient`, `--global` does not apply — homing
/// always reflects where the task was captured.
fn ambient_repo(db: &Db) -> Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    Ok(db.read(move |conn| mount::ambient_namespace(conn, &cwd))?)
}

/// Confirm a global `tasks/.backlog` fallback when `--backlog` is used outside any repo
/// (design D26.4). Returns `true` only on interactive assent; when stdin is not a TTY
/// (non-interactive/headless) it returns `false` so the caller errors instead of silently
/// creating a global backlog task.
fn confirm_global_backlog() -> Result<bool> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("Not inside a mounted repo. Home this task at the global `tasks/.backlog`? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

// ---- commands -------------------------------------------------------------

// ---- `jkb related` + `jkb inv …` (investigations, design Dmem.5/Dmem.9) ----

/// Parse repeated `facet=value` arguments.
fn parse_tag_args(tags: &[String]) -> Result<Vec<(String, String)>> {
    tags.iter()
        .map(|t| {
            let (facet, value) = t
                .split_once('=')
                .with_context(|| format!("tag `{t}` must be `facet=value`"))?;
            if facet.is_empty() {
                anyhow::bail!("tag `{t}` needs a facet before `=`");
            }
            Ok((facet.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Look up an item by uid or fail with a message naming it.
fn require_uid(db: &Db, uid: &str) -> Result<ItemId> {
    let owned = uid.to_owned();
    db.read(move |conn| item::id_for_uid(conn, &owned))?
        .with_context(|| format!("no item with uid `{uid}`"))
}

/// `jkb related <uid>` — walk the typed edge graph out from one item.
fn investigation_path(db: &Db, name: &str, global: bool) -> Result<String> {
    let root = investigation::MEMORY_ROOT;
    if name == root || name.starts_with(&format!("{root}/")) {
        return Ok(name.to_owned());
    }
    if global {
        return Ok(format!("{root}/{name}"));
    }
    // The ambient mount namespace is e.g. `repos/jkb/openspec`; the repo *key* is the first
    // segment after `repos/`, so every investigation about a repo lands under one root.
    let repo = ambient_repo(db)?.and_then(|mount| {
        mount
            .strip_prefix("repos/")
            .unwrap_or(&mount)
            .split('/')
            .next()
            .map(str::to_owned)
    });
    Ok(match repo {
        Some(repo) => format!("{root}/{repo}/{name}"),
        None => format!("{root}/{name}"),
    })
}

/// Print a bucket of investigation units, human or JSON.
fn print_units(units: &[investigation::UnitRow], json: bool, show_rank: bool) {
    if json {
        let arr: Vec<serde_json::Value> = units
            .iter()
            .map(|u| {
                serde_json::json!({
                    "uid": u.uid,
                    "kind": u.kind,
                    "resolution": u.resolution,
                    "rank": u.rank,
                    "evidence": u.evidence,
                    "namespace": u.namespace,
                    "snippet": u.content.as_deref().map(first_line),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(arr)).unwrap_or_default()
        );
    } else if units.is_empty() {
        println!("(empty)");
    } else {
        for u in units {
            let rank = if show_rank {
                format!(" rank {:.2}", u.rank)
            } else {
                String::new()
            };
            let evidence = if u.evidence.abs() < f64::EPSILON {
                String::new()
            } else {
                format!(" ev {:+.2}", u.evidence)
            };
            println!(
                "{:<34} [{}]{rank}{evidence} — {}",
                u.uid,
                u.kind,
                u.content.as_deref().map(first_line).unwrap_or_default(),
            );
        }
    }
}

#[allow(clippy::too_many_lines)] // a flat dispatcher; one arm per `jkb inv` subcommand
fn cmd_inv(db: &Db, cmd: InvCmd, global: bool, json: bool) -> Result<()> {
    match cmd {
        InvCmd::Ls => {
            let rows = db.read(investigation::list)?;
            if json {
                let arr: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "ns": r.ns_path, "type": r.type_name, "units": r.units,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else if rows.is_empty() {
                println!(
                    "(no investigations yet) available types: {}",
                    nstype::AVAILABLE.join(", ")
                );
            } else {
                for r in &rows {
                    println!("{:<40} [{}] {} unit(s)", r.ns_path, r.type_name, r.units);
                }
            }
            Ok(())
        }
        InvCmd::New {
            type_name,
            path,
            goal,
            accept,
            goal_kind,
        } => {
            let strategy = nstype::resolve_strategy(&type_name)?;
            let ns_path = investigation_path(db, &path, global)?;
            // Default the goal unit to the strategy's own goal kind (`symptom`,
            // `conjecture`, …) so the seeded unit reads naturally in its investigation.
            let goal_kind = goal_kind.unwrap_or_else(|| {
                strategy
                    .node_kinds()
                    .iter()
                    .find(|k| k.base == nstype::BaseKind::Goal)
                    .map_or(nstype::KIND_GOAL, |k| k.kind)
                    .to_owned()
            });
            let mut body = goal.join(" ");
            if body.trim().is_empty() {
                body = format!("(state the goal for {ns_path} here)");
            }
            let mut tags = Vec::new();
            if let Some(preset) = accept {
                // The presets belong to the STRATEGY, so one strategy's predicate can never
                // be stamped onto another's goal (a `debugging` symptom must not acquire the
                // mathematical proof bar, which its `goal_predicate` would then ignore).
                let presets = strategy.acceptance_presets();
                anyhow::ensure!(
                    !presets.is_empty(),
                    "the `{}` strategy has no acceptance presets, so --accept does not apply \
                     to it; state the bar in --goal instead",
                    strategy.name()
                );
                let text = strategy.acceptance_text(&preset).with_context(|| {
                    format!(
                        "unknown acceptance preset `{preset}` for `{}`; expected one of {}",
                        strategy.name(),
                        presets.join(", ")
                    )
                })?;
                // The acceptance predicate lives IN the goal body: the investigation
                // terminates on it, so every agent that picks this up must read the same bar.
                body = format!("{body}\n\n{text}");
                tags.push((nstype::conjecture::FACET_ACCEPTANCE.to_owned(), preset));
            }
            let (ns_for_txn, body_for_txn) = (ns_path.clone(), body.clone());
            let (uid, existed) = db.write_txn("cli", move |conn, meta| {
                // Whether this namespace was ALREADY an investigation decides the wording
                // below: `create` is idempotent, so a re-run must not claim to have created
                // anything.
                let existed = ns::get_type(conn, &ns_for_txn)?.is_some();
                let id = investigation::create(
                    conn,
                    meta,
                    &ns_for_txn,
                    &type_name,
                    &goal_kind,
                    &body_for_txn,
                    &tags,
                )?;
                Ok((
                    item::get(conn, id)?.map(|m| m.uid).unwrap_or_default(),
                    existed,
                ))
            })?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ns": ns_path, "goal_uid": uid, "type": strategy.name(),
                        "created": !existed,
                    })
                );
            } else if existed {
                println!(
                    "investigation {ns_path} [{}] already exists — left as it is",
                    strategy.name()
                );
                println!("goal: {uid}");
                println!("next: jkb inv digest {ns_path}");
            } else {
                println!("created investigation {ns_path} [{}]", strategy.name());
                println!("goal: {uid}");
                println!("next: jkb inv verbs {ns_path}");
            }
            Ok(())
        }
        InvCmd::Verbs { ns } => {
            let strategy = investigation_strategy(db, &ns)?;
            if json {
                let arr: Vec<serde_json::Value> = strategy
                    .verbs()
                    .iter()
                    .map(|v| {
                        serde_json::json!({
                            "verb": v.verb, "creates": v.kind, "about": v.about,
                            "edge": v.edge.map(EdgeType::as_str),
                            "target": match v.target {
                                nstype::TargetRule::Required => "required",
                                nstype::TargetRule::Optional => "optional",
                                nstype::TargetRule::Forbidden => "none",
                            },
                            "resolves_target": v.resolves_target.map(Resolution::as_str),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                println!("{} [{}]\n{}", ns, strategy.name(), strategy.about());
                for v in strategy.verbs() {
                    let target = match v.target {
                        nstype::TargetRule::Required => " --on <uid>",
                        nstype::TargetRule::Optional => " [--on <uid>]",
                        nstype::TargetRule::Forbidden => "",
                    };
                    println!("  {:<24}{target:<14} {}", v.verb, v.about);
                }
            }
            Ok(())
        }
        InvCmd::Kinds { ns } => {
            let strategy = investigation_strategy(db, &ns)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "type": strategy.name(),
                        "base_kinds": nstype::BASE_KINDS,
                        "kinds": strategy.node_kinds().iter().map(|k| serde_json::json!({
                            "kind": k.kind, "about": k.about,
                        })).collect::<Vec<_>>(),
                        "edges": strategy.edge_types().iter().map(|e| e.as_str())
                            .collect::<Vec<_>>(),
                    }))?
                );
            } else {
                println!("{} [{}]", ns, strategy.name());
                println!("base kinds: {}", nstype::BASE_KINDS.join(", "));
                for k in strategy.node_kinds() {
                    println!("  {:<24} {}", k.kind, k.about);
                }
                println!(
                    "edges: {}",
                    strategy
                        .edge_types()
                        .iter()
                        .map(|e| e.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            Ok(())
        }
        InvCmd::Frontier { ns, all, limit } => {
            let units = db.read(move |conn| investigation::frontier(conn, &ns, all, limit))?;
            print_units(&units, json, true);
            Ok(())
        }
        InvCmd::Core { ns } => {
            let units = db.read(move |conn| investigation::confirmed_core(conn, &ns))?;
            print_units(&units, json, false);
            Ok(())
        }
        InvCmd::Tombstones { ns } => {
            let tombs = db.read(move |conn| investigation::tombstones(conn, &ns))?;
            if json {
                let arr: Vec<serde_json::Value> = tombs
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "uid": t.unit.uid,
                            "kind": t.unit.kind,
                            "resolution": t.unit.resolution,
                            "snippet": t.unit.content.as_deref().map(first_line),
                            "killed_by": t.killed_by.iter().map(|(e, uid, body)| {
                                serde_json::json!({
                                    "edge": e.as_str(), "uid": uid,
                                    "snippet": body.as_deref().map(first_line),
                                })
                            }).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else if tombs.is_empty() {
                println!("(no dead ends recorded yet)");
            } else {
                for t in &tombs {
                    println!(
                        "{:<34} [{}] {} — {}",
                        t.unit.uid,
                        t.unit.kind,
                        t.unit.resolution.as_deref().unwrap_or("unresolved"),
                        t.unit
                            .content
                            .as_deref()
                            .map(first_line)
                            .unwrap_or_default(),
                    );
                    for (edge_type, uid, body) in &t.killed_by {
                        println!(
                            "    {} by {uid}: {}",
                            edge_type.as_str(),
                            body.as_deref().map(first_line).unwrap_or_default()
                        );
                    }
                    if t.killed_by.is_empty() {
                        println!("    (no edge records why — link what killed it)");
                    }
                }
            }
            Ok(())
        }
        InvCmd::Retread { uid, depth } => {
            let start = require_uid(db, &uid)?;
            let units = db.read(move |conn| investigation::anti_retread(conn, start, depth))?;
            if !json && units.is_empty() {
                println!("(nothing related has been ruled out — clear to proceed)");
                return Ok(());
            }
            print_units(&units, json, false);
            Ok(())
        }
        InvCmd::Evidence { uid } => {
            let id = require_uid(db, &uid)?;
            let (total, edges) = db.read(move |conn| {
                Ok((
                    edge::evidence_for(conn, id)?,
                    edge::evidence_edges(conn, id)?,
                ))
            })?;
            let mut rows = Vec::new();
            for e in &edges {
                let src = e.src;
                if let Some(meta) = db.read(move |conn| item::get(conn, src))? {
                    rows.push((e, meta));
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "uid": uid,
                        "balance": total,
                        "edges": rows.iter().map(|(e, meta)| serde_json::json!({
                            "edge": e.edge_type.as_str(),
                            "uid": meta.uid,
                            "contribution": e.contribution,
                            "snippet": meta.content.as_deref().map(first_line),
                        })).collect::<Vec<_>>(),
                    }))?
                );
            } else {
                println!("{uid}: balance {total:+.2}");
                for (e, meta) in &rows {
                    println!(
                        "  {:+.2} {:<12} {:<30} {}",
                        e.contribution,
                        e.edge_type.as_str(),
                        meta.uid,
                        meta.content.as_deref().map(first_line).unwrap_or_default()
                    );
                }
                if rows.is_empty() {
                    println!("  (no supports/contradicts edges)");
                }
            }
            Ok(())
        }
        InvCmd::Digest { ns, dry_run } => {
            if dry_run {
                let body = db.read(move |conn| Ok(investigation::digest(conn, &ns)?.render()))?;
                print!("{body}");
                return Ok(());
            }
            let (uid, body) = db.write_txn("cli", move |conn, meta| {
                let (id, body) = investigation::write_digest(conn, meta, &ns)?;
                Ok((
                    item::get(conn, id)?.map(|m| m.uid).unwrap_or_default(),
                    body,
                ))
            })?;
            if json {
                println!("{}", serde_json::json!({"uid": uid, "digest": body}));
            } else {
                print!("{body}");
                println!("\n(written to {uid})");
            }
            Ok(())
        }
        InvCmd::Rollup { ns } => {
            let changed = db.write_txn("cli", move |conn, meta| {
                investigation::roll_up(conn, meta, &ns)
            })?;
            if json {
                let arr: Vec<serde_json::Value> = changed
                    .iter()
                    .map(|(uid, from, to)| {
                        serde_json::json!({
                            "uid": uid, "from": from.as_str(), "to": to.as_str(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else if changed.is_empty() {
                println!("(every resolution already matches its edges)");
            } else {
                for (uid, from, to) in &changed {
                    println!("{uid}: {} -> {}", from.as_str(), to.as_str());
                }
            }
            Ok(())
        }
        InvCmd::Do {
            ns,
            verb,
            text,
            target,
            weight,
            tags,
        } => {
            let tags = parse_tag_args(&tags)?;
            let content = text.join(" ");
            let outcome = db.write_txn("cli", move |conn, meta| {
                let call = investigation::VerbCall {
                    verb: &verb,
                    content: &content,
                    target_uid: target.as_deref(),
                    weight,
                    tags: &tags,
                };
                investigation::apply_verb(conn, meta, &ns, &call)
            })?;
            let resolved = outcome.target_resolution.map(Resolution::as_str);
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "uid": outcome.uid, "target_resolution": resolved,
                    })
                );
            } else {
                println!("{}", outcome.uid);
                if let Some(r) = resolved {
                    println!("target resolution -> {r}");
                }
            }
            Ok(())
        }
        InvCmd::Add {
            ns,
            kind,
            text,
            edges,
            weight,
            tags,
        } => {
            let tags = parse_tag_args(&tags)?;
            let mut parsed_edges = Vec::new();
            for spec in &edges {
                let (type_name, target) = spec
                    .split_once(':')
                    .with_context(|| format!("edge `{spec}` must be `<type>:<target-uid>`"))?;
                let edge_type = EdgeType::from_str_opt(type_name)
                    .with_context(|| format!("unknown edge type `{type_name}`"))?;
                parsed_edges.push((edge_type, target.to_owned(), weight));
            }
            let content = text.join(" ");
            let uid = db.write_txn("cli", move |conn, meta| {
                let unit = investigation::NewUnit {
                    kind,
                    content,
                    namespace: ns,
                    tags,
                    edges: parsed_edges,
                    reverse_edges: Vec::new(),
                };
                let id = investigation::add(conn, meta, &unit)?;
                Ok(item::get(conn, id)?.map(|m| m.uid).unwrap_or_default())
            })?;
            if json {
                println!("{}", serde_json::json!({"uid": uid}));
            } else {
                println!("{uid}");
            }
            Ok(())
        }
        InvCmd::Link {
            src,
            edge: edge_name,
            dst,
            weight,
        } => {
            let edge_type = EdgeType::from_str_opt(&edge_name).with_context(|| {
                format!(
                    "unknown edge type `{edge_name}`; available: {}",
                    EdgeType::ALL
                        .iter()
                        .map(|e| e.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            db.write_txn("cli", move |conn, meta| {
                investigation::link(conn, meta, &src, edge_type, &dst, weight)
            })?;
            if !json {
                println!("linked");
            }
            Ok(())
        }
        InvCmd::Promise { uid, value } => {
            db.write_txn("cli", move |conn, meta| {
                investigation::set_promise(conn, meta, &uid, value)
            })?;
            if !json {
                println!("promise = {value}");
            }
            Ok(())
        }
        InvCmd::Resolve { uid, resolution } => {
            // `resolve_unit` owns the guard (a task's lifecycle is `status`, not
            // `resolution`) so every caller inherits it, not just this one.
            db.write_txn("cli", move |conn, meta| {
                investigation::resolve_unit(conn, meta, &uid, &resolution)
            })?;
            if !json {
                println!("resolution set (the unit is retained — link what changed it)");
            }
            Ok(())
        }
        InvCmd::Reopen { route, mechanism } => {
            // The whole operation (strategy check, gate, edges, gap supersession) lives in
            // the engine so it is testable and every caller inherits the gate.
            let outcome = db.write_txn("cli", move |conn, meta| {
                investigation::reopen(conn, meta, &route, &mechanism)
            })?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "mechanism_kind": outcome.mechanism_kind,
                        "superseded_gaps": outcome.superseded_gaps,
                        "reopened": !outcome.superseded_gaps.is_empty(),
                    })
                );
            } else if outcome.superseded_gaps.is_empty() {
                // Nothing was blocking it, so nothing was reopened — say so plainly rather
                // than reporting a state change that did not happen.
                println!(
                    "nothing to reopen: no open gap was blocking it (recorded the {} as \
                     informing the route)",
                    outcome.mechanism_kind
                );
            } else {
                println!("reopened on a new {}", outcome.mechanism_kind);
                for uid in &outcome.superseded_gaps {
                    println!("  superseded gap {uid}");
                }
            }
            Ok(())
        }
        InvCmd::Stale { ns, window } => {
            let marked = db.write_txn("cli", move |conn, meta| {
                nstype::debugging::mark_stale_observations(conn, meta, &ns, &window)
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&marked)?);
            } else if marked.is_empty() {
                println!("(no observations went stale)");
            } else {
                for uid in &marked {
                    println!("{uid} -> staleness=stale (excluded, not deleted)");
                }
            }
            Ok(())
        }
    }
}

/// The cheat-sheet itself. A constant rather than a literal inside [`cmd_guide`], because it is
/// **data**: as a function body it counted against `clippy::too_many_lines`, so every line added
/// to the guide was priced against a limit that has nothing to say about a block of text.
///
/// Mirrored by the root `AGENTS.md`. Keep the two in step — an agent reads whichever it meets
/// first, and a command documented in only one of them is a command half the readers never learn.
const GUIDE: &str = r#"jkb — agent quickstart

CONVENTIONS
  --json      every read command emits machine-readable JSON; parse that, not the text.
  --global    ignore the cwd-based ambient namespace scope (search/list everything).
  exit codes  `grep` exits 1 when nothing matched (0 = found). Lookups of a missing uid
              error (nonzero). Everything else is 0 on success.
  namespaces  the KB is a virtual filesystem: `repos/<repo>/…`, `tasks/…`, `media/`,
              `references/`, `memory/`. Items are "files"; typed edges + tags cross-cut.

ORIENT (read-only)
  jkb tree [path]              map a subtree (folders + per-folder counts), one call.
  jkb ls [path] [-l -R -t -a]  list a namespace's children (long / recursive / by-time / all).
  jkb recent [path]           most-recently-updated items — what changed lately.
  jkb find [path] --kind K --tag f=v --status S   structured search (typed; → query DSL).
  jkb grep <pat> [path] [-i -l -c]   literal-substring content search; exit 1 on no match.
  jkb query "<DSL>"           full query DSL (kind: tag: status: ns: is:ready due<= …).
  jkb search "<terms>" --route hybrid   ranked vector/FTS retrieval (needs the embedder).
      Pick by what you already know: `find`/`query` when you know the KIND/TAG/STATUS,
      `grep` for a LITERAL string in content, `search` for fuzzy/semantic ranking.

READ ONE ITEM
  jkb cat <uid>               the raw body to stdout (pipe it, no metadata).
  jkb stat <uid>              compact metadata (kind, namespace, tags, timestamps).
  jkb item show <uid>         metadata + a (bounded) body preview.

TASKS
  jkb task add "text !p1 @2026-07-15 +ns #facet=value"   quick-add.
  jkb task next [DSL]         the ready frontier (unblocked, by priority then due).
  jkb task set <uid> --status open|in_progress|needs_review|done|cancelled
  jkb task show <uid>         the full task body.

WORKING A TASK IN PARALLEL (each session is its own git worktree)
  jkb task work <uid>         open (or return to) this task's session: its own checkout and
                              branch `task/<session>`, claimed so nothing else starts it.
                              Work and COMMIT inside the printed worktree, nowhere else.
                              --onto <branch> names the STAGING branch it lands on; omit it
                              and jkb joins the batch in flight, or cuts one from trunk.
  jkb task land <uid>         rebase the session onto its target, run the repo's gate, and
                              on green mark the task done and ARCHIVE the session (moved to
                              .jkb/archive, deleted after 30 days — never deleted here). Serial:
                              one land at a time, so a red gate means YOUR branch broke it.
                              REFUSES a task with no recorded review, or whose review left a
                              must-fix finding open — anything at priority <= 1, so !p0 blocks
                              as well as !p1. --no-review records a waiver.
  jkb task abandon <uid>      drop the session and reopen the task (the branch is kept).
  jkb task reap               finish landings that could not move their own worktree, delete
                              archives past 30 days, and compact the message queue. A session may not unlink its own
                              .claude policy files, so it cannot archive itself — land records
                              it and this, run anywhere else, finishes it. The watcher service
                              installed by `jkb service install` runs it on a timer.
  jkb task sessions           what is in flight here, with uncommitted work and commits ahead.
  jkb task gate ["<cmd>"]     show or set the command that verifies a landing in this repo.
      If you are inside a session, land is the human's call — commit, and say you are done.

STAGING BRANCHES (where a batch lands before trunk — the swarm's integration branch)
  jkb staging ls [--all]      every staging branch and the tasks landing on it: each task's state
                              (implementing / review / landed / dropped), its commits, and how
                              many must-fix findings its review left open. `dropped` is a
                              cancelled task, never folded into `landed`. --all shows merged ones.
  jkb task review record --findings <ns>
                              record that a review ran against the current branch, so `land`
                              can require one. /jkb-review-log does this for you.
  jkb task tag set <uid> <f>=<v>
                              make <v> the facet's ONLY value (add appends). Use for the
                              single-answer facets: repo=. Writing branch= also records where
                              that branch was cut, but prefer `task start`, which can be told
                              the branch it lands ON.
  jkb task start <uid> [--branch B] [--onto S]
                              claim it and record the branch, the repo and where B lands.
                              Prefer it to tagging branch= by hand.
  jkb task why <uid>          how this task reached its state: every transition, who applied it,
                              and the evidence each one fired on. Run this FIRST when a task
                              is stuck.
  jkb task pr <uid> [number]  record (or discover, from the branch) the pull request that will
                              prove this task's work landed. `close-merged` closes a task on a
                              landing jkb recorded, else on that PR having MERGED — and neither
                              counts once the task has been put back to work since. Anything it
                              cannot prove is reported with the reason. No verb takes a sha.

RECOVERY (the archive nothing else exposes)
  jkb history <path>          every synced version of a file, newest first.
  jkb blob ls --contains "…"  find the version still carrying a line you remember.
  jkb blob cat <hash>         write those bytes to stdout (pipe to a file or `diff`).
      File sync stores the bytes of every version it settles and blobs are never deleted,
      so a bad write that already landed on disk is recoverable.

GRAPH
  jkb related <uid> [--edge T] [--depth N] [--direction out|in|both]
      Walk the typed edges out from an item — the context its own body doesn't carry
      (what it depends on, what killed it, what it answers).

INVESTIGATIONS (open-ended work with durable state — `memory/…`)
  An investigation is a typed namespace holding a graph of units. Orient by reading three
  buckets, in this order:
    1. jkb inv digest <ns>          the state digest: all three buckets + the "done" test.
    2. jkb inv tombstones <ns>      dead ends + WHAT KILLED EACH. Read before working.
    3. jkb inv frontier <ns>        live, unblocked units, ranked — pick work here.
  Then, before starting on a unit:
       jkb inv retread <uid>        has anything near this already been ruled out?
       jkb related <uid>            how does it connect to the goal?
  Recording what you learn (each write is audited + undoable):
       jkb inv verbs <ns>           the strategy's verbs — the normal way to add units.
       jkb inv do <ns> <verb> "text" [--on <uid>] [--weight N] [--tag f=v]
       jkb inv evidence <uid>       the signed supports/contradicts balance for a unit.
       jkb inv link <src> <edge> <dst>          an edge no verb covers.
       jkb inv resolve <uid> <resolution>       unresolved|success|dead_end|superseded|abandoned
  Starting one:
       jkb inv ls                   your investigations and their strategy types.
       jkb inv new <type> <name> --goal "…" [--accept prove|disprove|either]
  A dead end is NEVER deleted: resolve it `dead_end` and link what killed it (`refutes`,
  `rules_out`). That graveyard is the memory — it is what stops the next agent re-treading.

WRITE (all audited + undoable)
  jkb item edit <uid> [--append] <text>   replace/append an item's content.
  jkb item rm <uid> [--force]             delete an item + its cascade; `jkb undo` restores
                                          it. Refuses tombstones and synced-file items.
  jkb task tag add <uid> facet=value      apply a tag (additive; `tag set` replaces).
  jkb task depend <uid> <dep-uid>         add a dependency edge (`undepend` removes it).
  jkb undo                                revert the last change.

Tips: prefer `find`/`query` (structured) over `grep` when you know the kind/tag; add
`--json` and parse; scope with a path or rely on the ambient cwd namespace.
"#;

/// `jkb guide` — a one-page cheat-sheet of the agent-facing command surface.
fn cmd_guide() {
    print!("{GUIDE}");
}

/// The item's primary (home) namespace path, if placed.
fn cmd_ns(db: &Db, cmd: NsCmd, json: bool) -> Result<()> {
    match cmd {
        NsCmd::Ls { scope } => {
            let paths = match scope {
                Some(path) => db.read(move |conn| ns::children(conn, &path))?,
                None => db.read(ns::roots)?,
            };
            if json {
                let arr: Vec<_> = paths.iter().map(|(_, p)| p.clone()).collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else if paths.is_empty() {
                println!("(no namespaces)");
            } else {
                for (_, p) in paths {
                    println!("{p}");
                }
            }
        }
        NsCmd::Mk { paths } => {
            let to_make = paths.clone();
            db.write_txn("cli", move |conn, _meta| {
                for p in &to_make {
                    ns::ensure(conn, p)?;
                }
                Ok(())
            })?;
            for p in &paths {
                report(json, p, "namespace ready");
            }
        }
        NsCmd::Mv { from, to } => {
            let (from2, to2) = (from.clone(), to.clone());
            let moved = db.write_txn("cli", move |conn, meta| {
                ns::move_subtree(conn, meta, &from2, &to2)
            })?;
            println!("moved {moved} namespace(s): {from} -> {to}");
        }
        NsCmd::Rm { path } => {
            let p = path.clone();
            db.write_txn("cli", move |conn, meta| ns::remove(conn, meta, &p))?;
            report(json, &path, "removed namespace");
        }
        NsCmd::Type {
            path,
            type_name,
            list,
            clear,
        } => cmd_ns_type(db, path, type_name, list, clear, json)?,
    }
    Ok(())
}

/// The investigation strategy governing `ns`, refusing an untyped namespace and one typed
/// with a *contract* (design D33.1) — a contract type has no verbs, frontier or acceptance
/// predicate, so `jkb inv` on one is a user error, not an empty listing.
fn investigation_strategy(db: &Db, ns: &str) -> Result<&'static dyn nstype::NamespaceType> {
    let owned = ns.to_owned();
    let (source, strategy) = db
        .read(move |conn| nstype::for_namespace(conn, &owned))?
        .with_context(|| format!("`{ns}` is not an investigation namespace"))?;
    anyhow::ensure!(
        strategy.role() == nstype::TypeRole::Investigation,
        "`{ns}` is typed `{}` (from `{source}`), a contract that {} — it is not an \
         investigation, so it has no verbs or frontier",
        strategy.name(),
        strategy.about()
    );
    Ok(strategy)
}

/// `jkb ns type` — show, set, or list namespace types (design D33).
fn cmd_ns_type(
    db: &Db,
    path: Option<String>,
    type_name: Option<String>,
    list: bool,
    clear: bool,
    json: bool,
) -> Result<()> {
    if list {
        return list_namespace_types(json);
    }
    let path = path.context("`jkb ns type` needs a <path> (or `--list`)")?;

    if clear {
        let p = path.clone();
        let had = db.write_txn("cli", move |conn, meta| {
            let Some(id) = ns::get(conn, &p)? else {
                return Ok(None);
            };
            let had = ns::get_type_by_id(conn, id)?;
            if had.is_some() {
                ns::clear_type(conn, meta, id)?;
            }
            Ok(had)
        })?;
        match had {
            Some(name) => report(json, &path, &format!("cleared type `{name}`")),
            None => report(json, &path, "already untyped"),
        }
        return Ok(());
    }

    let Some(type_name) = type_name else {
        // Show: report the namespace's OWN type and, separately, the one it inherits, so
        // "why is this enforced here?" is answerable without walking the tree by hand.
        let (exists, own, effective) = {
            let (p1, p2, p3) = (path.clone(), path.clone(), path.clone());
            db.read(move |conn| {
                Ok((
                    ns::get(conn, &p1)?.is_some(),
                    ns::get_type(conn, &p2)?,
                    ns::effective_type(conn, &p3)?,
                ))
            })?
        };
        // A namespace that does not exist must not read as "untyped" — that is the answer a
        // typo gets, and it looks exactly like a valid one.
        anyhow::ensure!(
            exists,
            "namespace `{path}` does not exist (create it with `jkb ns mk {path}`)"
        );
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ns": path,
                    "type": own,
                    "effective_type": effective.as_ref().map(|(_, t)| t),
                    "inherited_from": effective.as_ref().map(|(src, _)| src),
                }))?
            );
        } else if let Some((source, name)) = effective {
            let ty = nstype::resolve(&name)?;
            if source == path {
                println!("{path}: {name} — {}", ty.about());
            } else {
                println!("{path}: {name} (inherited from {source}) — {}", ty.about());
            }
        } else {
            println!("{path}: untyped");
        }
        return Ok(());
    };

    // Reject an unknown type before opening a transaction, so the error names what IS
    // available rather than leaving a namespace typed with something unresolvable.
    let ty = nstype::resolve(&type_name)?;
    let (p, t) = (path.clone(), type_name.clone());
    db.write_txn("cli", move |conn, meta| {
        let id = ns::ensure(conn, &p)?;
        ns::set_type(conn, meta, id, &t)
    })?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ns": path, "type": ty.name(),
            }))?
        );
    } else {
        println!("{path}: {} — {}", ty.name(), ty.about());
    }
    Ok(())
}

/// `jkb ns type --list` — every registered type, grouped by role.
fn list_namespace_types(json: bool) -> Result<()> {
    let rows = nstype::AVAILABLE
        .iter()
        .map(|name| nstype::resolve(name))
        .collect::<Result<Vec<_>, _>>()?;
    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|ty| {
                serde_json::json!({
                    "type": ty.name(),
                    "role": match ty.role() {
                        nstype::TypeRole::Investigation => "investigation",
                        nstype::TypeRole::Contract => "contract",
                    },
                    "about": ty.about(),
                    "accepts": ty.accepted_kinds(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }
    for (role, label) in [
        (nstype::TypeRole::Investigation, "investigation strategies"),
        (nstype::TypeRole::Contract, "contracts"),
    ] {
        println!("{label}:");
        for ty in rows.iter().filter(|t| t.role() == role) {
            println!("  {:<18} {}", ty.name(), ty.about());
        }
    }
    Ok(())
}

fn cmd_tag(db: &Db, cmd: TagCmd, json: bool) -> Result<()> {
    match cmd {
        TagCmd::Ls => {
            let facets = db.read(tag::facets)?;
            if json {
                let arr: Vec<_> = facets
                    .iter()
                    .map(|(f, k)| serde_json::json!({"facet": f, "value_kind": k}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else if facets.is_empty() {
                println!("(no facets)");
            } else {
                for (f, k) in facets {
                    println!("{f} ({k})");
                }
            }
        }
        TagCmd::Rename { old, new } => {
            let (old2, new2) = (old.clone(), new.clone());
            let n = db.write_txn("cli", move |conn, meta| {
                tag::rename_facet(conn, meta, &old2, &new2)
            })?;
            println!("renamed facet {old} -> {new} ({n} application(s))");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_mount(db: &Db, cmd: MountCmd, json: bool) -> Result<()> {
    match cmd {
        MountCmd::Create {
            ns,
            dir,
            mode,
            serializer,
            include,
            no_include,
            exclude,
            no_exclude,
            policy,
        } => cmd_mount_create(
            db,
            &ns,
            &dir,
            MountEdit {
                mode: mode.map(Into::into),
                serializer,
                include: FieldEdit::from_flags(include, no_include),
                exclude: FieldEdit::from_flags(exclude, no_exclude),
                policy: policy.map(Into::into),
            },
        ),
        MountCmd::Ls => cmd_mount_ls(db, json),
    }
}

/// `mount ls` — list every mount as `namespace → serializer → backing directory`.
fn cmd_mount_ls(db: &Db, json: bool) -> Result<()> {
    let mounts = db.read(mount::all)?;
    if json {
        let v: Vec<_> = mounts
            .iter()
            .map(|(path, m)| {
                serde_json::json!({
                    "namespace": path,
                    "serializer": m.serializer,
                    "backing": m.backing_uri,
                    "sync_mode": m.sync_mode,
                    "conflict_policy": m.conflict_policy,
                    "include_glob": m.include_glob,
                    "exclude_glob": m.exclude_glob,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if mounts.is_empty() {
        println!("(no mounts)");
    } else {
        // The globs and the policy decide which files a sync will touch and what it does when
        // both sides changed. Listing only the serializer and directory made a mount whose
        // include glob had been dropped look identical to one that still had it.
        for (path, m) in &mounts {
            println!("{path}  [{}]  {}", m.serializer, m.backing_uri);
            println!(
                "    mode={}  policy={}  include={}  exclude={}",
                m.sync_mode,
                m.conflict_policy,
                m.include_glob.as_deref().unwrap_or("(none)"),
                m.exclude_glob.as_deref().unwrap_or("(none)"),
            );
        }
    }
    Ok(())
}

/// What a `mount create` invocation asked to change. Every field distinguishes "not
/// mentioned" from "set to this", because `mount create` doubles as the update command and a
/// re-run that silently reset the fields you did not name is how a mount's include glob was
/// once dropped — after which the `tasks` serializer discovered every file in the tree and
/// overwrote 62 of them.
struct MountEdit {
    mode: Option<SyncMode>,
    serializer: Option<String>,
    include: FieldEdit,
    exclude: FieldEdit,
    policy: Option<ConflictPolicy>,
}

/// An optional field's requested change: leave it, set it, or clear it. A bare `Option`
/// cannot express all three, and collapsing "leave" onto "clear" is the whole bug.
enum FieldEdit {
    Keep,
    Set(String),
    Clear,
}

impl FieldEdit {
    fn from_flags(value: Option<String>, clear: bool) -> Self {
        match (value, clear) {
            (Some(v), _) => Self::Set(v),
            (None, true) => Self::Clear,
            (None, false) => Self::Keep,
        }
    }

    /// Resolve against what the mount already stores.
    fn apply(&self, current: Option<String>) -> Option<String> {
        match self {
            Self::Keep => current,
            Self::Set(v) => Some(v.clone()),
            Self::Clear => None,
        }
    }
}

fn cmd_mount_create(db: &Db, ns_path: &str, dir: &Path, edit: MountEdit) -> Result<()> {
    let abs = std::fs::canonicalize(dir)
        .with_context(|| format!("resolving mount directory {}", dir.display()))?;
    // The backing path is stored verbatim in the `file://` uri and later resolved for sync,
    // so a lossy conversion (U+FFFD for non-UTF-8 bytes) would silently point the mount at a
    // different/nonexistent directory. Reject such paths outright rather than corrupting the
    // mount.
    let abs_str = abs.to_str().ok_or_else(|| {
        anyhow::anyhow!("mount directory path is not valid UTF-8: {}", abs.display())
    })?;
    let backing = format!("file://{abs_str}");
    let ns_path = ns_path.to_owned();
    let ns_display = ns_path.clone();

    // Read what the mount already is, so an update only changes what was actually named.
    let existing = {
        let ns_path = ns_path.clone();
        db.read(move |conn| match ns::get(conn, &ns_path)? {
            Some(id) => mount::get(conn, id),
            None => Ok(None),
        })?
    };

    let mode = edit.mode.unwrap_or_else(|| {
        existing
            .as_ref()
            .and_then(|m| SyncMode::from_db_str(&m.sync_mode))
            .unwrap_or(SyncMode::Bidirectional)
    });
    let serializer = edit
        .serializer
        .or_else(|| existing.as_ref().map(|m| m.serializer.clone()))
        .unwrap_or_else(|| "document".to_owned());
    let include = edit
        .include
        .apply(existing.as_ref().and_then(|m| m.include_glob.clone()));
    let exclude = edit
        .exclude
        .apply(existing.as_ref().and_then(|m| m.exclude_glob.clone()));
    let policy = edit.policy.unwrap_or_else(|| {
        existing
            .as_ref()
            .and_then(|m| ConflictPolicy::from_db_str(&m.conflict_policy))
            .unwrap_or(ConflictPolicy::Manual)
    });

    let updating = existing.is_some();
    let (ser, inc, exc) = (serializer.clone(), include.clone(), exclude.clone());
    db.write_txn("cli", move |conn, meta| {
        let ns_id = ns::ensure(conn, &ns_path)?;
        mount::create(
            conn,
            meta,
            ns_id,
            &backing,
            mode,
            &ser,
            inc.as_deref(),
            exc.as_deref(),
            policy,
        )
    })?;
    let verb = if updating { "updated mount" } else { "mounted" };
    println!("{verb} {ns_display} -> {}", abs.display());
    // Print the resulting configuration, not just the path: an update that silently kept or
    // dropped a glob is exactly the failure this command now guards against, so the answer
    // has to be visible at the moment it is decided.
    println!(
        "  serializer={serializer}  mode={}  policy={}  include={}  exclude={}",
        mode.as_str(),
        policy.as_str(),
        include.as_deref().unwrap_or("(none)"),
        exclude.as_deref().unwrap_or("(none)"),
    );
    Ok(())
}

fn cmd_sync(
    db: &Db,
    ns_path: Option<&str>,
    watch: bool,
    conflict: Option<ConflictPolicy>,
) -> Result<()> {
    let debounce = std::time::Duration::from_millis(300);
    if watch {
        let stop = Arc::new(AtomicBool::new(false));
        let handler_stop = Arc::clone(&stop);
        ctrlc::set_handler(move || handler_stop.store(true, Ordering::Relaxed))
            .context("installing Ctrl-C handler")?;
        if let Some(ns) = ns_path {
            println!("watching {ns} (Ctrl-C to stop)…");
            jkb_sync::watch(db, ns, debounce, &stop)?;
        } else {
            println!("watching all mounts (Ctrl-C to stop)…");
            jkb_sync::watch_all(db, debounce, &stop)?;
        }
        println!("stopped watching");
        return Ok(());
    }

    let mut failed = 0usize;
    if let Some(ns) = ns_path {
        failed += report_sync(db, ns, conflict)?;
    } else {
        // `--conflict` is a per-run override for unwedging ONE stuck file. Applied across
        // every mount it silently resolves every conflict in the KB the same way, and
        // `kb_wins` overwrites disk bytes that were never blobbed — unrecoverable, unlike a
        // bad import. Requiring the namespace keeps the blast radius the size of the
        // intention.
        anyhow::ensure!(
            conflict.is_none(),
            "--conflict needs a namespace: it resolves conflicts destructively, and across \
             every mount `kb_wins` would overwrite disk edits that no blob holds. Name the \
             mount you are unwedging, e.g. `jkb sync <ns> --conflict disk-wins`."
        );
        let paths = db.read(jkb_core::mount::all_paths)?;
        if paths.is_empty() {
            println!("no mounts configured");
        }
        for ns in paths {
            // No `?`. The loop is total by construction: a mount that fails is reported and
            // counted, and every later mount still reconciles. Pass 12 moved the raise out of
            // `report_sync` and left the `?` here, so the abort it was fixing simply moved up
            // one level — the whole point is that no single mount can end the run.
            match report_sync(db, &ns, conflict) {
                Ok(n) => failed += n,
                Err(e) => {
                    // A whole mount, counted as one — the closing line says "file(s) or
                    // mount(s)" rather than pretending to know how many files were behind it.
                    println!("sync {ns}: FAILED: {e:#}");
                    failed += 1;
                }
            }
        }
    }
    // Every mount has been reconciled by now; only the exit code is left to decide.
    anyhow::ensure!(
        failed == 0,
        "{failed} file(s) or mount(s) need attention; see the lines above"
    );
    Ok(())
}

/// Reconcile one mount and print its summary.
fn report_sync(db: &Db, ns_path: &str, conflict: Option<ConflictPolicy>) -> Result<usize> {
    use jkb_sync::Outcome::{
        Conflict, Created, Exported, Failed, Imported, Merged, Normalized, Quarantined, Refused,
        ResolvedFromDisk, ResolvedFromKb, Skipped, UpToDate,
    };
    let report = jkb_sync::sync_with_policy(db, ns_path, conflict)?;
    println!(
        "sync {ns_path}: {} created, {} imported, {} exported, {} merged, {} normalized, \
         {} conflicts, {} resolved, {} quarantined, {} up-to-date, {} skipped, {} refused, \
         {} failed",
        report.count(Created),
        report.count(Imported),
        report.count(Exported),
        report.count(Merged),
        report.count(Normalized),
        report.count(Conflict),
        report.count(ResolvedFromDisk) + report.count(ResolvedFromKb),
        report.count(Quarantined),
        report.count(UpToDate),
        report.count(Skipped),
        report.count(Refused),
        report.count(Failed),
    );
    for path in report.conflicts() {
        println!("  conflict: {}", path.display());
    }
    // A policy resolution throws one side's edits away. Say which side won, per file, so a
    // destructive resolution is visible at the moment it happens rather than discovered later.
    for (path, how) in report.resolved() {
        println!("  RESOLVED {} — {how}", path.display());
    }
    for path in report.quarantined() {
        println!("  needs attention (parse failed): {}", path.display());
    }
    // A refusal wrote nothing, so it must be visible or the file silently stops syncing.
    for (path, reason) in report.refused() {
        println!("  REFUSED {}: {reason}", path.display());
    }
    let failures = report.failed();
    for (path, err) in &failures {
        println!("  FAILED {}: {err}", path.display());
    }
    // "Unhealthy" is asked of the ONE authority that already answers it: the journal. Listing
    // outcomes by hand meant two definitions that disagreed — the exit code counted `Failed` and
    // `Quarantined` while `jkb doctor` reads `sync_state.status`, so a file left completely
    // unsynced by a `Conflict` or `Refused` exited 0 and was simultaneously reported as needing
    // attention. `/review-log` chains `jkb mount create … && jkb sync "$ns"`, so a zero there
    // let a run record a review over nothing.
    //
    // Counted from this mount's own files, so one mount's stuck file does not make another
    // mount's summary look bad.
    // Keyed with `jkb-sync`'s own spelling. Hand-rebuilding `file://{path}` here made the exit
    // code depend on a cross-crate string convention with no owner — and a third copy in this
    // file already canonicalizes, so the spellings had already diverged.
    let paths: std::collections::HashSet<String> = report
        .results
        .iter()
        .map(|r| jkb_sync::file_uri(&r.path))
        .collect();
    let flagged = db.read(move |conn| {
        Ok(jkb_core::sync_state::needs_attention(conn)?
            .into_iter()
            .filter(|s| paths.contains(&s.uri))
            .count())
    })?;
    // The UNION, not the replacement. The journal is the better authority — it is what
    // `jkb doctor` reads, so the two can no longer disagree — but flagging a failure is itself a
    // database write, and the failures that matter most (disk full, a lost write-lock race) are
    // exactly the ones that cannot perform it. Counting only journal rows meant those printed
    // FAILED and still exited 0. A per-file failure must never need a successful write to be
    // visible.
    let unhealthy = flagged.max(failures.len() + report.quarantined().len());
    // COUNTED, not raised. A failed file must not leave a zero exit — `/review-log` chains
    // `jkb mount create … && jkb sync "$ns"`, and a silent zero let a run record a review over
    // zero imported findings — but raising here aborted the all-mounts loop, so one bad file in
    // the first mount silently skipped every mount after it. That is exactly what `reconcile_all`
    // forbids one level down ("a per-file failure is a RESULT, not a run-ending error"),
    // reinstated at mount granularity. The caller reconciles everything, then decides.
    Ok(unhealthy)
}

fn cmd_task(db: &Db, db_path: &Path, cmd: TaskCmd, json: bool) -> Result<()> {
    match cmd {
        TaskCmd::Next { .. }
        | TaskCmd::Show { .. }
        | TaskCmd::Subtasks { .. }
        | TaskCmd::Why { .. }
        | TaskCmd::Add { .. }
        | TaskCmd::Set { .. }
        | TaskCmd::Edit { .. }
        | TaskCmd::Tag { .. }
        | TaskCmd::Depend { .. }
        | TaskCmd::Undepend { .. }
        | TaskCmd::Place { .. }
        | TaskCmd::Unplace { .. }
        | TaskCmd::Bind { .. }
        | TaskCmd::Claim { .. }
        | TaskCmd::Release { .. }
        | TaskCmd::Start { .. }
        | TaskCmd::Work { .. }
        | TaskCmd::Abandon { .. }
        | TaskCmd::Sessions
        | TaskCmd::Land {
            break_lock: false, ..
        }
        | TaskCmd::Landed { .. }
        | TaskCmd::Review { .. }
        | TaskCmd::Reclaim { .. } => {
            anyhow::bail!("internal: a task verb served as an op missed ops_cli's dispatch")
        }
        TaskCmd::Mirror => cmd_task_mirror(db, json)?,
        TaskCmd::Pr { uid, number } => cmd_task_pr(db, &uid, number, json)?,
        cmd @ (TaskCmd::Gate { .. }
        | TaskCmd::Land {
            break_lock: true, ..
        }) => {
            cmd_task_session(db, db_path, cmd, json)?;
        }
        TaskCmd::Reap {
            retain_days,
            dry_run,
            break_lock,
            watch,
            interval_secs,
        } => cmd_task_reap(
            db_path,
            ReapFlags {
                retain_days,
                dry_run,
                break_lock,
                watch,
                interval_secs,
            },
            json,
        )?,
        other => cmd_task_mutate(db, other, json)?,
    }
    Ok(())
}

/// Ensure every task homed outside `tasks/` has a `tasks/…` mirror (the task index).
/// Idempotent; `jkb sync` does this automatically, so this is a one-shot migration for
/// tasks created before the mirror existed.
fn cmd_task_mirror(db: &Db, json: bool) -> Result<()> {
    let added = db.write_txn("cli", task::ensure_all_mirrors)?;
    if json {
        println!("{}", serde_json::json!({ "mirrors_added": added }));
    } else {
        println!("added {added} tasks/ mirror(s)");
    }
    Ok(())
}

/// Handle the task mutation subcommands (`set`/`tag`/`depend`/`undepend`/`place`/`unplace`/
/// `bind`/`claim`/`release`) — the D27.3 write surface. Each is a thin edge over an
/// existing audited, cycle-checked `jkb-core` seam through the writer-actor.
fn cmd_task_mutate(db: &Db, cmd: TaskCmd, json: bool) -> Result<()> {
    cmd_task_landing(db, cmd, json)
}

/// The verbs about a task's **work** rather than its fields that still run only here: what proves
/// it landed (`task pr` is dispatched separately, `task review` through the ops).
///
/// Split from [`cmd_task_mutate`] because they read a git checkout and a pull request, where the
/// field setters read only the database — and because one dispatch holding every task verb had
/// grown past what one function should.
fn cmd_task_landing(db: &Db, cmd: TaskCmd, json: bool) -> Result<()> {
    match cmd {
        TaskCmd::CloseMerged { repo, dry_run } => cmd_task_close_merged(db, repo, dry_run, json)?,
        // The read and session subcommands are dispatched by `cmd_task` and never reach here.
        // Listed rather than caught by `_`, so a new variant is a compile error instead of an
        // `unreachable!` at run time.
        TaskCmd::Add { .. }
        | TaskCmd::Next { .. }
        | TaskCmd::Show { .. }
        | TaskCmd::Subtasks { .. }
        | TaskCmd::Mirror
        | TaskCmd::Why { .. }
        | TaskCmd::Pr { .. }
        | TaskCmd::Work { .. }
        | TaskCmd::Land { .. }
        | TaskCmd::Landed { .. }
        | TaskCmd::Abandon { .. }
        | TaskCmd::Sessions
        | TaskCmd::Gate { .. }
        | TaskCmd::Set { .. }
        | TaskCmd::Edit { .. }
        | TaskCmd::Tag { .. }
        | TaskCmd::Depend { .. }
        | TaskCmd::Undepend { .. }
        | TaskCmd::Place { .. }
        | TaskCmd::Unplace { .. }
        | TaskCmd::Bind { .. }
        | TaskCmd::Claim { .. }
        | TaskCmd::Start { .. }
        | TaskCmd::Release { .. }
        | TaskCmd::Reap { .. }
        | TaskCmd::Review { .. }
        | TaskCmd::Reclaim { .. } => unreachable!(),
    }
    Ok(())
}

/// `task pr <uid> [number]` — show, or record, the pull request that proves this work landed.
///
/// With a number, records it. Without, discovers it from the task's recorded branch and records
/// what it finds — **once**. After that the number is what is consulted, and the branch name
/// never is: a number is minted by GitHub and never reused, so a branch deleted, renamed or
/// reused afterwards cannot change the answer. That property is the whole reason this replaced
/// the commit-graph inference.
///
/// # Errors
/// Errors if the uid does not resolve, or a read or write fails.
fn cmd_task_pr(db: &Db, uid: &str, number: Option<i64>, json: bool) -> Result<()> {
    let id = resolve_task_uid(db, uid)?;
    let recorded = db.read(move |conn| Ok(jkb_core::transition::landing(conn, id)?.pr_number()))?;
    let number = match number {
        Some(n) => Some(n),
        None if recorded.is_some() => recorded,
        None => discover_pr(db, id)?,
    };
    let Some(number) = number else {
        if json {
            println!("{}", serde_json::json!({"uid": uid, "pr": null}));
        }
        return Ok(());
    };
    if recorded != Some(number) {
        record_pr(db, id, number)?;
    }
    let ctx = repo::repo_ctx().ok();
    let (merged, why) = ctx.as_ref().map_or_else(
        || (Fact::Unknown, Some("not in a git repository".to_owned())),
        // `None`: this verb reports a fact about the pull request — *did it merge* — and is not
        // deciding whether to close anything. The staleness rule belongs to the close decision,
        // where the question is whether the merge speaks for the work in flight.
        |c| pr::merged_fact(&c.root, Some(number), None),
    );
    if json {
        println!(
            "{}",
            serde_json::json!({"uid": uid, "pr": number, "merged": merged.as_str(), "why": why})
        );
    } else {
        println!(
            "{uid}: pull request #{number} — merged: {}",
            merged.as_str()
        );
        if let Some(why) = why {
            println!("  {why}");
        }
    }
    Ok(())
}

/// Find the pull request for a task's recorded branch, refusing to guess when a reused branch
/// name matches more than one.
fn discover_pr(db: &Db, id: ItemId) -> Result<Option<i64>> {
    let Ok(ctx) = repo::repo_ctx() else {
        anyhow::bail!(
            "not in a git repository, so there is no branch to look a pull request up by"
        );
    };
    let branch = db
        .read(move |conn| jkb_core::transition::latest_with_branch(conn, id))?
        .and_then(|r| r.labels.branch);
    let Some(branch) = branch else {
        anyhow::bail!(
            "this task records no branch, so there is nothing to look a pull request up by — \
             pass the number: `jkb task pr <uid> <number>`"
        );
    };
    match pr::discover(&ctx.root, &branch) {
        pr::Discovery::One(found) => Ok(Some(found.number)),
        pr::Discovery::None => {
            println!("no pull request has `{branch}` as its head branch");
            Ok(None)
        }
        // The recycled-name case, reported rather than guessed. Picking one is exactly how the
        // inference this replaced closed work that had not landed.
        pr::Discovery::Ambiguous(numbers) => anyhow::bail!(
            "`{branch}` is the head branch of more than one pull request ({}) — that branch name \
             has been reused, so which one is this task's work is not something to guess. Pass \
             the number: `jkb task pr <uid> <number>`",
            numbers
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // The remedy `gh` itself names — "close the task by hand" — is `close-merged`'s, and
        // this is not that command: what to do *here* is name the number, which needs no `gh` at
        // all. A message carried through from where it was written is how advice comes to be
        // about somebody else's problem.
        pr::Discovery::Unavailable(why) => anyhow::bail!(
            "{why}\n  ...or name it directly: `jkb task pr <uid> <number>`, which needs no `gh`."
        ),
    }
}

/// Record a task's pull request number as a transition, so it lands in the history beside
/// everything else that happened to the task.
fn record_pr(db: &Db, id: ItemId, number: i64) -> Result<()> {
    let branch = db
        .read(move |conn| jkb_core::transition::latest_with_branch(conn, id))?
        .and_then(|r| r.labels.branch);
    db.write_txn_with::<_, anyhow::Error, _>("cli", move |conn, meta| {
        let facts = task::observe(conn, id)?;
        let labels = jkb_core::transition::Labels {
            branch: branch.clone(),
            pr_number: Some(number),
            ..jkb_core::transition::Labels::default()
        };
        jkb_core::transition::note(conn, meta, id, &facts, &labels)?;
        Ok(())
    })
}

/// `task landed <branch> --onto <target>` — the merge queue reporting a graft it performed.
///
/// **It does not verify the graft, and no longer claims to.** The predecessor refused unless the
/// work was demonstrably in the target, judged from the commit graph; that inference is gone, and
/// with it the check. What remains is a trusted report from a caller that ran the graft itself and
/// gated it — `scripts/merge-queue.sh`, whose own REVIEWER is a stricter gate than `task land`'s
/// (D38). Recorded as `observed_landed`, which is the event for a landing jkb did not perform
/// through `task land`.
///
/// # Errors
/// Errors if either name is not usable as a git ref, if this is not a git repository, or if no
/// task in it records `branch`.
pub(crate) fn cmd_task_landed(
    kb: &session_cli::Kb<'_>,
    branch: &str,
    onto: &str,
    json: bool,
) -> Result<()> {
    gitrepo::valid_ref(branch)?;
    gitrepo::valid_ref(onto)?;
    let ctx = repo::repo_ctx()?;
    // The branch's own tip, read **before** anything else could move it: what a person reading
    // the history needs to recognize which work this was.
    let head = gitrepo::branch_ref(&ctx.root, branch, gitrepo::Prefer::Local)?
        .and_then(|r| gitrepo::rev_commit(&ctx.root, &r).transpose())
        .transpose()?;

    // Every task recorded on this branch. The queue lands a whole group at once, so this is
    // many-to-one by nature — and it needs no per-branch record to find them, because a task's
    // own facets say which branch it is on.
    let by_branch = kb.by_branch(&ctx.key)?;
    let uids: Vec<String> = by_branch
        .get(branch)
        .into_iter()
        .flatten()
        .map(|t| t.uid.clone())
        .collect();
    anyhow::ensure!(
        !uids.is_empty(),
        "no task in {} records branch={branch}, so there is nothing to record a landing for",
        ctx.key
    );

    let mut recorded = Vec::new();
    let mut not_closed = Vec::new();
    for uid in &uids {
        // `task.landed` states `landed_elsewhere` — the merge queue performed and gated the graft
        // itself (D38), which is why this is `observed_landed` and not `land`, whose guard asks
        // whether jkb may *perform* it. A guard's refusal still records the landing, as an entry
        // that moves nothing; an event the task's state does not define (an abandoned task, which
        // keeps `branch=`) records nothing. See `jkb_api::sessions::landed`.
        let outcome = kb.landed(
            uid,
            jkb_api::sessions::Landed {
                branch: branch.to_owned(),
                onto: onto.to_owned(),
                head: head.clone(),
            },
        )?;
        match outcome.refusal {
            None => recorded.push(uid.clone()),
            Some(why) => not_closed.push((uid.clone(), why)),
        }
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "repo": ctx.key, "branch": branch, "onto": onto, "head": head,
                "landed": recorded,
                "held": not_closed.iter().map(|(uid, why)| serde_json::json!({
                    "uid": uid, "reason": why,
                })).collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }
    println!("recorded: {branch} landed on {onto}");
    for uid in &recorded {
        println!("  {uid}");
    }
    // Reported rather than swallowed: a task the queue could not close is one the queue's caller
    // will otherwise believe is done. The commonest reason is open subtasks, which is D34.4's
    // rule holding — a merged branch is evidence, not proof that the work finished.
    for (uid, why) in &not_closed {
        eprintln!("  {uid} not closed — {why}");
    }
    Ok(())
}

/// The host-only session verbs: configure the gate that guards a landing (design D36.5), and break a
/// repo's land lease. The other session verbs are served as ops (`task_cli`), in both modes.
fn cmd_task_session(db: &Db, db_path: &Path, cmd: TaskCmd, json: bool) -> Result<()> {
    let backend = jkb_api::LocalBackend::new(db.clone()).with_actor("cli");
    let kb = session_cli::Kb::new(&backend);
    match cmd {
        TaskCmd::Land {
            break_lock: true, ..
        } => cmd_task_land(
            &kb,
            &archive::Stores::new(kb, Some(db_path)),
            Some(db),
            None,
            LandFlags {
                gate: None,
                no_gate: false,
                keep_worktree: false,
                no_review: false,
                break_lock: true,
            },
            json,
        ),
        // Storing a gate is a host command (decision A), so it is done here, against the database,
        // and never through an op. Showing one is served as an op in both modes.
        TaskCmd::Gate { cmd, clear } => {
            let ctx = repo::repo_ctx()?;
            if clear {
                session::set_gate(db, &ctx.key, None)?;
            } else if let Some(cmd) = &cmd {
                session::set_gate(db, &ctx.key, Some(cmd))?;
            }
            session_cli::gate(&kb, json)
        }
        _ => unreachable!("cmd_task_mutate routes only session subcommands here"),
    }
}

/// `task land` — the merge queue for one session (design D36.4).
/// The flags of `task land`, grouped so the signature stays under the bool-argument lint.
#[allow(clippy::struct_excessive_bools)] // a command's flags, not state
pub(crate) struct LandFlags {
    pub(crate) gate: Option<String>,
    pub(crate) no_gate: bool,
    pub(crate) keep_worktree: bool,
    pub(crate) no_review: bool,
    pub(crate) break_lock: bool,
}

/// What `land` needs from the task and its session once every precondition has held.
struct Preflight {
    sess: session::Session,
    branch: String,
    onto: String,
    ahead: usize,
}

/// Everything `land` checks before the review gate: it must be landable *at all*, and nothing
/// here has moved a branch, so a refusal leaves the repo exactly as it was.
///
/// These are the same conditions `staging::land_blocker` reports per row (design D38.8) —
/// this is the authority, and the row renders the verdict this side computes rather than
/// re-deriving it from a projection that cannot express half of them.
///
/// Runs with the land lock already held, which is what lets these be checked **once**: a target
/// checkout found clean here is still the one the graft a moment later goes into, so the second
/// dirty check that used to sit on the other side of the lock closed no window and was simply a
/// second wording of one rule.
fn land_preflight(
    ctx: &repo::RepoCtx,
    uid: &str,
    facts: &jkb_api::sessions::TaskState,
) -> Result<Preflight> {
    let tags = &facts.tags;
    // The task's own pipeline state, mapped by the one function that does that — so the
    // terminal arm of `land_blocker` below is the arm that actually fires here, rather than a
    // second bail beside it saying the same thing in its own words.
    let state = staging::State::from_status(&facts.status);
    anyhow::ensure!(
        !repo::facet_values(tags, repo::FACET_BRANCH).is_empty(),
        "{uid} has no session — run `jkb task work {uid}` first"
    );
    // A missing worktree is NOT bailed on here. `land_blocker` below already judges it, and
    // judges it better — it distinguishes a swarm task being built elsewhere from an abandoned
    // checkout, and tells the first not to run `jkb task work` (which would cut a second branch
    // and detach it from its group). A bail here made that arm unreachable from the one command
    // it is the authority for, so the In Flight row and `land` explained the same task
    // differently.
    //
    // Session and branch together, through the one rule the In Flight row uses. Sharing the
    // existence *predicate* was not enough: the row preferred a recorded branch that resolves
    // while this took whichever came first, so the one shared blocker explained the same task two
    // opposite ways — and this side's advice, `jkb task work`, cuts a second branch and detaches
    // the task from its batch.
    let repo::Work {
        session: sess,
        branch,
    } = repo::work_for(ctx, tags)?;
    let branch = branch.context("this task records no branch")?;
    // Where this task's work lands, from its own history — the last time anybody said so.
    let onto = facts
        .land_target
        .clone()
        .context("this session records no land target — re-run `jkb task work` with --onto")?;
    // The same question the session paths ask, and materialised for the same reason: the land
    // path checks the target out, so a target that exists only on `origin/` is usable — refusing
    // it meant `land` rejected a branch it would itself have created a moment later.
    //
    // This **writes** — it can create a local ref — which is worth saying out loud in something
    // called a preflight. It is confined to `cmd_task_land`, this function's only caller, which
    // materialises the same branch moments later anyway; the listing surfaces use
    // `staging::land_blocker`, which reads and never creates.
    anyhow::ensure!(
        gitrepo::adopt_remote(&ctx.root, &onto)?,
        "the land target {onto} no longer exists"
    );
    // Everything from here is `staging::land_blocker` — the ONE derivation of "may this
    // land", which the In Flight row renders verbatim. It was restated per surface twice, and
    // each time a row claimed "Landable" for a task this command then refused: the uncommitted
    // session, the empty branch, the dirty target checkout, the review gate. Assembling the
    // facts is this side's job; judging them is not.
    //
    // ONE question about the work branch, asked the same way the listing asks it: does it exist
    // here — counting a remote-tracking copy — and under what name? Asking `has_branch` here while
    // the row asked remote-inclusively made the one shared blocker print two opposite explanations
    // for the same task: the row said it is being built elsewhere, the command said its worktree
    // was abandoned and told the owner to open a new session, which detaches it from its group.
    // The resolved ref is also what the count is taken with — a bare remote-only name resolves to
    // nothing, and `ahead_count` refuses rather than answering zero.
    // BOTH operands resolved through the same map. The work branch was, and the land target was
    // still passed as a bare name — so a target living only as `origin/<onto>` aborted the command
    // with a raw git error before any of the graceful refusals below, while `staging ls` counted
    // the identical pair without trouble. `adopt_remote` above has just materialised it locally,
    // so the map is re-read rather than reused from before that.
    let refs = gitrepo::branch_refs(&ctx.root)?;
    let work_ref = refs.get(&branch);
    let onto_ref = refs.get(&onto);
    let ahead = match (work_ref, onto_ref) {
        (Some(work), Some(target)) => gitrepo::ahead_count(&ctx.root, target, work)?,
        _ => 0,
    };
    // NOT collapsed here any more. `target_dirty_reason` takes the `Option` and says what an
    // unanswered `git worktree list` means for a landing — which is a refusal, because the
    // checkout the graft would land in cannot even be identified. Collapsed to an empty list it
    // reported the target CLEAN, and the bespoke argument written here for why that was safe was
    // the second such argument in the tree for one value; one helper stating it once is the fix.
    let worktrees = gitrepo::worktrees(&ctx.root)?;
    // The SAME silence reaches `land_blocker` twice: through `target_dirty` below, and — four
    // arms earlier — through `sess`, because `repo::work_for` -> `session::discover` collapses
    // this failed listing to "no sessions". The second route answered first, so the refusal
    // added beside `target_dirty` could not fire and the operator was told to run `jkb task
    // work`, which cuts a second branch and detaches the task from its batch.
    let listing_failed = worktrees
        .is_none()
        .then(|| staging::worktree_listing_refusal(&ctx.root));
    let mut dirty_cache = BTreeMap::new();
    let target_dirty =
        staging::target_dirty_reason(worktrees.as_deref(), &ctx.root, &onto, &mut dirty_cache)?;
    // Three-valued: `land_blocker` requires it PROVEN clean, so a session checkout git cannot
    // read refuses here — before the graft — instead of at `archive::dispose`, which runs after
    // it and leaves the task frozen over work already in the target.
    let dirty = match &sess {
        Some(s) => gitrepo::is_dirty(&s.worktree, &ctx.root)?,
        None => Fact::No,
    };
    // The same question the machine's `land` guard asks, asked **before** the graft. Both read
    // `containment`, which is where the answer lives (D35), so the row, the command and the
    // machine cannot disagree about which parents are held.
    let open_subtasks = facts.open_subtasks;
    if let Some(reason) = staging::land_blocker(&staging::LandFacts {
        state,
        open_subtasks,
        worktree: sess.as_ref().map(|s| s.worktree.as_path()),
        dirty,
        commits: ahead,
        branch_exists: work_ref.is_some(),
        target_dirty: target_dirty.as_deref(),
        listing_failed: listing_failed.as_deref(),
        // The review is enforced a moment later by `review::enforce`, which renders the same
        // verdict at length and is where `--no-review` records a waiver instead of refusing.
        verdict: None,
    }) {
        // A tree that is only MISSING files is not work in progress, and the two want opposite
        // advice: "commit them in the session first" over 152 deletions commits the wreckage of a
        // part-way removal. Said as an extra sentence rather than by changing `land_blocker`,
        // which is the one shared rule the In Flight row renders too — the verdict is identical,
        // only the remedy differs, and only when the difference is observable.
        // Only onto the refusal it explains. `land_blocker` returns ONE reason, and appending a
        // `git restore .` remedy to "it has no commits" or "its review left a must-fix open" is
        // advice about a different problem — the tree being deletions-only is a fact about the
        // dirt, and the dirt is only what was refused when the reason says so.
        // From `Deletions::caveat`, the ONE wording — not a second one written here. This arm used
        // to match `Only(n)` and fall through everything else to silence, so an unanswered probe
        // (`Deletions::Unknown`) read exactly like ordinary work and the operator was told to
        // commit what may be a part-way removal: the collapse `archive::verdict_pending` had just
        // been corrected for, still in place at the site of the incident that motivated it.
        // The session is carried THROUGH the probe rather than re-matched after it. Written as
        // `match (d.caveat(), &sess)` there was an arm for `sess` being `None`, which cannot
        // happen — the probe only runs inside `sess.as_ref().map(..)` — so it was a branch no
        // input reaches wearing the costume of a safeguard.
        let hint = match sess
            .as_ref()
            .filter(|_| dirty.is_yes() && reason.contains("uncommitted changes"))
            .map(|s| (s, gitrepo::deletions_only(&s.worktree, &ctx.root)))
        {
            Some((s, Ok(d))) => match d.caveat() {
                None => String::new(),
                // The remedy is the land path's own, which is why `caveat` does not carry one: it
                // belongs only to the arm where putting the files back is the answer.
                Some(c) => {
                    // By characters, not by bytes. `c[..1]` panics on any caveat whose first
                    // character is multi-byte, and the wordings live in `Deletions::caveat` where
                    // nothing warns a future editor that a caller is slicing them.
                    let mut ch = c.chars();
                    let capped = ch.next().map_or_else(String::new, |f| {
                        f.to_uppercase().collect::<String>() + ch.as_str()
                    });
                    match d {
                        gitrepo::Deletions::Only(_) => format!(
                            " {capped}. `git -C {} restore .` puts them all back.",
                            s.worktree.display()
                        ),
                        _ => format!(" {capped}."),
                    }
                }
            },
            _ => String::new(),
        };
        anyhow::bail!("{uid} cannot land. {reason}{hint}");
    }
    // Unreachable: `land_blocker` refuses `worktree: None` above. Written as an error rather
    // than an `expect` so the no-panic rule holds even if that arm is ever weakened.
    let sess = sess.context("this task has no session worktree")?;
    Ok(Preflight {
        sess,
        branch,
        onto,
        ahead,
    })
}

///
/// Served as ops in both modes (tasks S6.4 stage 4): its git work runs here, its database work
/// through `kb`. `store` is this process's own database, when it has one — the only way a gate is
/// stored (decision A); a client of `jkb serve` runs the gate it is given or finds, and stores none.
pub(crate) fn cmd_task_land(
    kb: &session_cli::Kb<'_>,
    stores: &archive::Stores<'_>,
    store: Option<&Db>,
    uid: Option<&str>,
    flags: LandFlags,
    json: bool,
) -> Result<()> {
    let LandFlags {
        gate: gate_flag,
        no_gate,
        keep_worktree,
        no_review,
        break_lock,
    } = flags;
    let gate_flag = gate_flag.as_deref();
    let ctx = repo::repo_ctx()?;
    if break_lock {
        return break_land_lease(kb, &ctx.key, json);
    }
    let uid = uid.context("a task to land")?;
    // Asked before anything moves: a task this client may not write would otherwise be grafted and
    // its session disposed of, and only then refused its record — landed, in progress, sessionless.
    let facts = kb.facts_for_write(uid)?;
    let tags = facts.tags.clone();

    // The lock is taken **before** anything is checked, not just before the graft.
    //
    // It used to be taken afterwards, which left a window between deciding the target checkout was
    // clean and grafting into it — and the answer was a second, independently worded dirty check
    // on the other side of the lock. That does not close the window, it moves it: the second check
    // has exactly the same gap to the graft. What actually closes it is checking under the lock,
    // so there is now one rule (`staging::target_dirty_reason`, shared with the In Flight row)
    // evaluated once. A redundant guard that reads as protection is worse than none.
    //
    // Acquiring costs nothing here: it fails fast rather than waiting, and every other precondition
    // below is equally worth serialising against a concurrent land.
    //
    // `.jkb/` is excluded first, in `.git/info/exclude` exactly as `task work` does it — local to this
    // clone, never their committed `.gitignore` (D36.2) — because a land may create `.jkb/base` and
    // `.jkb/archive` in a repo where `task work` has never run. The lock itself is a database lease
    // now, and leaves nothing on disk.
    session::ensure_excluded(&ctx.root)?;
    let _lock = session_cli::LandLease::acquire(kb, &ctx.key)?;

    let Preflight {
        sess,
        branch,
        onto,
        ahead,
    } = land_preflight(&ctx, uid, &facts)?;

    // The review gate (design D38.5), before the graft: a refusal must not have moved a
    // branch first. Concerns and nits do not block — only must-fix findings do. A waiver is
    // only *owed* here; it is written after the landing actually happens, so a land that then
    // fails on the graft or the gate build leaves no waiver for something that never occurred.
    let head = gitrepo::rev(&ctx.root, &branch)?.unwrap_or_else(|| "unknown".to_owned());
    let waiver_owed = review::enforce(kb, uid, &tags, no_review, json)?;

    let land_dir = land_dir_for(&ctx, &onto)?;

    let (outcome, pre) = gitrepo::graft(&land_dir, &branch, &onto)?;
    // TWO FAILURES, TWO REMEDIES. They were one arm, and the message it printed was written for
    // the rebase conflict — so a refused fast-forward sent the user to rebase a branch that had
    // rebased cleanly, which reproduces every time it is tried. `scripts/merge-queue.sh` splits
    // the same pair (exit 1 against exit 4) and for the same reason: only one of them is the
    // branch's fault.
    let grafted = match outcome {
        gitrepo::Graft::Landed { grafted } => grafted,
        gitrepo::Graft::Conflict => anyhow::bail!(
            "{branch} does not rebase cleanly onto {onto} — nothing changed. Rebase it where \
             the context is: cd {} && git rebase {onto}, fix the conflict, then land again",
            sess.worktree.display()
        ),
        gitrepo::Graft::CouldNotAdvance { why } => anyhow::bail!(
            "{branch} rebased onto {onto} cleanly, but {onto} could not be advanced onto the \
             result. Nothing changed, and the branch is fine — rebasing it will not help. This is \
             usually transient: something else moved or held {onto} while the graft ran. git \
             said: {why}\n\nTry landing again; if it repeats, look at what else is writing to \
             {onto} (`git worktree list`, a running watcher, a held index.lock in {})",
            land_dir.display()
        ),
    };

    let (gate, source) = session::resolve_gate(store, kb, &ctx.root, &ctx.key, gate_flag, no_gate)?;
    if !json {
        println!(
            "gate: {} ({})",
            gate.as_deref().unwrap_or("(none)"),
            source.label()
        );
    }
    if let Some(cmd) = &gate {
        let (passed, output) = session::run_gate(&land_dir, cmd, json)?;
        if !passed {
            gitrepo::reset_hard(&land_dir, &pre)?;
            let tail = output
                .map(|o| format!("\n{}", tail_lines(&o, 20)))
                .unwrap_or_default();
            anyhow::bail!(
                "gate failed on the integrated result — {onto} rolled back to {}, {branch} \
                 untouched. Reproduce in the session (cd {} && {cmd}) and land again.{tail}",
                &pre[..pre.len().min(8)],
                sess.worktree.display()
            );
        }
    }

    settle_landing(
        kb,
        stores,
        &facts.uid,
        &ctx,
        &sess,
        Landed {
            uid,
            branch: &branch,
            onto: &onto,
            grafted: &grafted,
            ahead,
            gate: gate.as_deref(),
            gate_source: source.label(),
            keep_worktree,
            waiver: waiver_owed.then_some(head.as_str()),
            // Only a real commit id: `head` falls back to the literal "unknown" for the waiver
            // string, and a landing event whose `landed_head` is not a commit can never be
            // credited — it would silently mean "never credited" rather than "not recorded".
            head: (head != "unknown").then_some(head.as_str()),
        },
        json,
    )
}

/// `task land --break-lock`: drop this repo's land lease, whoever holds it.
fn break_land_lease(kb: &session_cli::Kb<'_>, repo_key: &str, json: bool) -> Result<()> {
    let broken = session_cli::LandLease::break_held(kb, repo_key)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "repo": repo_key,
                "broken_holder": broken.as_ref().map(|(raw, _)| raw),
            })
        );
    } else {
        match broken {
            Some((_, holder)) => println!("broke {repo_key}'s land lease held by {holder}"),
            None => println!("no land lease was held for {repo_key}"),
        }
    }
    Ok(())
}

/// What a successful graft produced, for the bookkeeping that follows it.
#[derive(Clone, Copy)]
struct Landed<'a> {
    uid: &'a str,
    branch: &'a str,
    onto: &'a str,
    grafted: &'a str,
    ahead: usize,
    gate: Option<&'a str>,
    gate_source: &'a str,
    keep_worktree: bool,
    /// The branch HEAD to record as `review-waived=`, when `--no-review` carried this land.
    waiver: Option<&'a str>,
    /// The branch's **own tip** at the moment of the graft, recorded as `landed_head`.
    ///
    /// `graft` rebases a detached HEAD, so the branch ref still points here afterwards — which is
    /// what lets a later reader tell this landing from one belonging to a namesake branch cut
    /// under the same name after the fact.
    head: Option<&'a str>,
}

/// Mark the task done, free the claim, and dispose of the session (design D36.4).
fn settle_landing(
    kb: &session_cli::Kb<'_>,
    stores: &archive::Stores<'_>,
    task_uid: &str,
    ctx: &repo::RepoCtx,
    sess: &session::Session,
    landed: Landed<'_>,
    json: bool,
) -> Result<()> {
    // The waiver first, in its own transaction, because it describes something that has
    // **already** happened: the commits are on the target before this function is called.
    // Written together with the status below, it was lost every time the dirty-session guard
    // bailed — the override had landed, and nothing anywhere recorded that the review gate was
    // skipped, which is precisely the state `--no-review` records a facet to avoid (D38.5). It
    // is also recorded for a task somebody finished during the gate: the waived landing is what
    // it describes, not the status.
    if let Some(sha) = landed.waiver {
        kb.set_facet(task_uid, review::FACET_REVIEW_WAIVED, sha)?;
    }

    // Is the session still there at all? `git status` in a directory that no longer exists
    // exits non-zero, and `gitrepo::git` maps that to `Ok(None)` — so `is_dirty` answers
    // "clean" for a vanished worktree and the disposal below then fails on it. A concurrent
    // `jkb task abandon` removes exactly this directory, so the case is real, and "gone" is
    // the one state where disposal has nothing left to do.
    // `is_no()`, for the reason every absence in the disposal path is asked that way: a stat
    // error is not a removal. Read as one, this skipped both the guard below and the disposal, so
    // a checkout that was still there ended up orphaned — on disk with no record naming it.
    // `Unknown` leaves `disposed_already` false, and the guard below then keeps the session and
    // says so, which is what it already does for a tree it cannot read.
    let disposed_already = presence::present_under(&sess.worktree, &ctx.root)
        .fact()
        .is_no();

    // The session was verified clean in `land_preflight`, but that was before a graft and a
    // gate build that can run for minutes — long enough for the agent sitting in the session
    // to write a file. Every disposal below is destructive (`reset --hard`, `worktree
    // remove`), so the check is taken **again**, here, against the state we are about to
    // discard. The landing itself already happened and is not undone by this; the session is
    // simply kept, with its work, for the person to deal with.
    // `is_no()`, not `!is_yes()`: a checkout git cannot read is not a clean one, and every
    // disposal below this line is destructive. The session is kept instead, which is exactly
    // what this guard does for a genuinely dirty tree.
    anyhow::ensure!(
        disposed_already || gitrepo::is_dirty(&sess.worktree, &ctx.root)?.is_no(),
        "{branch} landed on {onto} — the commits are there — but {} has uncommitted changes \
         written since the landing began, so the session is kept exactly as it is rather than \
         reset over them. Deal with them, then close the task with \
         `jkb task set {uid} --status done` and drop the session with \
         `jkb task abandon {uid} --force`.",
        sess.worktree.display(),
        branch = landed.branch,
        onto = landed.onto,
        uid = landed.uid,
    );

    let disposal = dispose_session(stores, ctx, sess, &landed, disposed_already)?;

    // Landed: the task is done, the claim is free, and the session branch is a duplicate of
    // commits now in `onto`.
    //
    // The status is re-read **inside** the transaction: `land_preflight` checked it before a
    // multi-minute gate, and nothing serializes a `jkb task set --status cancelled` against a
    // land (the land lease only excludes a second land). Writing `Done` over a cancellation made
    // this the one transition the guard exists to prevent. Same reasoning as `review::record`.
    // Whether the status was left as somebody else set it during the gate. Reported, not
    // returned as an error: the session HAS been disposed of by this point, so bailing left
    // the claim held on a worktree that no longer exists — freed only by `doctor --fix` — and
    // said nothing about what had just been removed.
    // THE PLAN IS APPLIED LAST, after every fallible git step above has succeeded. That is the
    // rule that makes a git failure survivable: a landing whose session could not be disposed of
    // leaves the task where it was, and the verb is simply re-runnable. The incident this
    // replaces set the status, cleared the claim, and *then* asked git to remove a worktree git
    // refused to remove — a task `done`, unclaimed, with a live session.
    //
    // The status is re-read **inside** the transaction: `land_preflight` checked it before a
    // multi-minute gate, and nothing serializes a `jkb task set --status cancelled` against a
    // land (the land lease only excludes a second land). The machine has no `land` from `cancelled`,
    // so a cancellation that arrived during the gate is not overwritten — and it says so rather
    // than being silently skipped.
    //
    // `done` is deliberately *not* symmetrical: it has a self-loop, because a verb must survive
    // its own second run (`Defect::Unrepeatable`). Nothing is lost by that here — the plan
    // re-asserts the status it already has and re-releases a freed claim — and re-landing is
    // refused far earlier anyway, by `staging::land_blocker`, before any graft happens.
    // Through `task.land`, which states the facts this command established — the graft, the green
    // gate, the disposal — and re-reads the status in its own transaction.
    let outcome = kb.land(
        task_uid,
        jkb_api::sessions::Landed {
            branch: landed.branch.to_owned(),
            onto: landed.onto.to_owned(),
            head: landed.head.map(str::to_owned),
        },
    )?;
    let kept_status = outcome.refusal.map(|why| (outcome.status, why));
    if let Some((status, why)) = &kept_status {
        eprintln!(
            "note: {} was left `{status}` — {why} Its commits are on {}, and its session has been \
             disposed of.",
            landed.uid, landed.onto
        );
    }

    // ASKED OF GIT, once, after everything that could have removed it — the same rule
    // `cmd_task_abandon` already applies, and folded into the same one value so no two lines can
    // disagree. `land` always plans the deletion (its branch is a duplicate of commits now in the
    // target), and this used to be derived from WHICH DISPOSAL ARM RAN: `Archived` asserted
    // `branch_deleted: true` even though `dispose` only `eprintln!`s when `git branch -D` fails,
    // and `Deferred` — every landing in the container, where a session cannot archive its own
    // checkout — reported a live branch with no mention of the reaper that will delete it. The
    // operator then reaches for `git branch -D` and git refuses, because the deferred worktree
    // still holds it.
    // `has_branch` answers a `Fact`, and only a PROVEN absence is a deletion. It used to collapse
    // any non-zero git exit to `false`, so an unreadable `packed-refs` printed "removed its
    // branch" — the one direction that costs something, because it stops the operator looking.
    // `Err` — git not executable at all — is the same unestablished answer, so it is spelled as
    // one rather than as a fourth arm that has to be kept in step with the other three.
    let branch_fate = branch_fate(
        // `land` always plans the deletion: its branch is a duplicate of commits now in the
        // target. `archive::Plan { delete_branch: true }`, a few lines above.
        true,
        gitrepo::has_branch(&ctx.root, landed.branch).unwrap_or(Fact::Unknown),
        match &disposal {
            Disposal::Deferred(d) => d.will_be_swept(),
            _ => false,
        },
    );
    report_landing(
        &landed,
        sess,
        &disposal,
        branch_fate,
        kept_status.as_ref(),
        json,
    );
    Ok(())
}

/// Say what happened, never what was intended. Two claims here were once simply false:
/// `"{uid} is done"` after a status the transaction deliberately left as `cancelled`, and
/// "removed session and its branch" in the arm that had only run `git worktree prune` because
/// somebody else had already removed the directory.
fn report_landing(
    landed: &Landed<'_>,
    sess: &session::Session,
    disposal: &Disposal,
    branch_fate: BranchFate,
    kept_status: Option<&(String, String)>,
    json: bool,
) {
    // Reported from what actually happened, never from what was intended. Two claims here were
    // simply false: `"{uid} is done"` after a status this transaction deliberately left as
    // `cancelled`, and "removed session and its branch" in the arm that only ran
    // `git worktree prune` because somebody else had already removed the directory.
    let status = kept_status.map_or("done", |(s, _)| s.as_str());
    if json {
        println!(
            "{}",
            serde_json::json!({
                "uid": landed.uid, "landed": true, "branch": landed.branch, "onto": landed.onto,
                "commits": landed.ahead, "gate": landed.gate, "gate_source": landed.gate_source,
                "session_removed": disposal.session_gone(), "status": status,
                "branch_deleted": branch_fate == BranchFate::Deleted,
                "branch_owed_to_reaper": branch_fate == BranchFate::OwedToTheReaper,
                "session_archived": match &disposal {
                    Disposal::Archived(dest) => Some(dest.display().to_string()),
                    _ => None,
                },
                "session_deferred": match &disposal {
                    Disposal::Deferred(d) => Some(d.why.clone()),
                    _ => None,
                },
                // Whether anything WILL finish it, from the same verdict the sweep executes.
                // A consumer that saw only the reason could not tell a deferral in hand from one
                // nothing can act on.
                "session_deferred_sweepable": match &disposal {
                    Disposal::Deferred(d) => Some(d.will_be_swept()),
                    _ => None,
                },
            })
        );
    } else {
        println!(
            "landed: {} → {} ({} commit(s)); {} is {status}",
            landed.branch, landed.onto, landed.ahead, landed.uid
        );
        match &disposal {
            Disposal::AlreadyGone => {
                println!("  session was already gone; pruned its registration");
            }
            Disposal::Archived(dest) => {
                println!(
                    "  archived session {} to {} (deleted after {} days)",
                    sess.name,
                    dest.display(),
                    archive::RETAIN_DAYS
                );
            }
            // Said plainly rather than buried: the landing is complete, and what is left is a
            // directory somebody else will move. Reading this as a failure is what sent the last
            // operator to re-run a land that had nothing left to do.
            // DERIVED, never asserted. This used to promise unconditionally that `jkb task
            // reap` would finish the job — while for a tree that could not answer git for
            // itself, no sweep could act on the record at all, and the operator was sent to run
            // a command that would report the same hold for ever.
            Disposal::Deferred(d) => {
                println!(
                    "  session {} could not be archived from in here ({}), so {}. The landing is \
                     done.",
                    sess.name,
                    d.why,
                    d.outlook()
                );
            }
            Disposal::Kept => println!("  kept session {} and its branch", sess.name),
        }
        // The branch, said separately and from what git reports — the disposal arms above say
        // what happened to the DIRECTORY, which is a different question.
        match branch_fate {
            BranchFate::Deleted => println!("  removed its branch {}", landed.branch),
            BranchFate::OwedToTheReaper => println!(
                "  its branch {} is still checked out by the deferred session; `jkb task reap` \
                 deletes it once the tree is archived",
                landed.branch
            ),
            BranchFate::Kept => println!("  branch {} is still there", landed.branch),
            BranchFate::Absent => {}
        }
    }
}

/// `task reap` — archive worktrees a landing could not move, then delete archives past the
/// retention window (design D49).
///
/// Takes the database **path** rather than a handle, and opens it per pass (`reap_once`), so a
/// database this binary cannot open fails a pass rather than the service. The records need no repo
/// context, so one service sweeps every repo on the machine.
#[derive(Clone, Copy)]
struct ReapFlags {
    retain_days: u64,
    dry_run: bool,
    break_lock: bool,
    watch: bool,
    interval_secs: u64,
}

fn cmd_task_reap(db_path: &Path, flags: ReapFlags, json: bool) -> Result<()> {
    let ReapFlags {
        retain_days,
        dry_run,
        break_lock,
        watch,
        interval_secs,
    } = flags;
    if break_lock {
        let db = open_db(db_path)?;
        let backend = jkb_api::LocalBackend::new(db).with_actor("reap");
        let stores = archive::Stores::new(session_cli::Kb::new(&backend), Some(db_path));
        // `--dry-run` promises to change nothing, and this ran before that was consulted — so
        // `--dry-run --break-lock` removed a live sweeper's lock while saying it would not.
        // On stderr under `--json`, whose stdout is the one report document.
        let say = |line: String| {
            if json {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        };
        if dry_run {
            say(match archive::lock_holder(&stores)? {
                Some(holder) => format!("would break the sweep lease held by {holder}"),
                None => "no sweep lease is held".to_owned(),
            });
        } else {
            say(match archive::break_lock(&stores)? {
                Some(holder) => format!("broke the sweep lease held by {holder}"),
                None => "no sweep lease was held".to_owned(),
            });
        }
    }
    if !watch {
        let report = reap_once(db_path, retain_days, dry_run)?;
        let compaction = (!dry_run).then(|| compact_queue(db_path));
        report_reap(&report, dry_run, json, compaction.as_ref());
        return Ok(());
    }
    // The service form. Ctrl-C stops it, the same shared-flag shape `sync --watch` uses.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&stop);
    let _ = ctrlc::set_handler(move || flag.store(true, std::sync::atomic::Ordering::SeqCst));
    // Never zero: a sweep every 0 seconds is a busy loop that would keep a laptop awake.
    let interval = std::time::Duration::from_secs(interval_secs.max(60));
    // What the last sweep could only observe, so an unchanged observation is not re-printed. A
    // held cross-boundary record never changes, and a service that restates it every quarter hour
    // is a log nobody reads the rest of; saying it once, and again when it changes, is the whole
    // of the signal.
    let mut last_observed = String::new();
    let mut last_compaction_failure = String::new();
    let mut last_sweep_failure = String::new();
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        // The queue's compaction rides the same timer (design r3.2 Q3). Work done is always
        // printed — two passes that each reaped one message are two events, not a repeat. Only a
        // FAILURE is silenced while unchanged, for the same reason as the sweep's silence below.
        if !dry_run {
            let c = compact_queue(db_path);
            match &c {
                Compaction::Failed(why) if *why == last_compaction_failure => {}
                Compaction::Failed(why) => {
                    last_compaction_failure.clone_from(why);
                    print_compaction(&c, json);
                }
                Compaction::Did(_) => {
                    last_compaction_failure.clear();
                    print_compaction(&c, json);
                }
                Compaction::Quiet => last_compaction_failure.clear(),
            }
        }
        match reap_once(db_path, retain_days, dry_run) {
            // Silence when there is nothing to say: this runs every quarter hour for ever, and a
            // log that says "nothing to do" 96 times a day is a log nobody reads the rest of.
            Ok(r) if r.is_empty() && r.observed() == last_observed => last_sweep_failure.clear(),
            Ok(r) => {
                last_sweep_failure.clear();
                last_observed = r.observed();
                report_reap(&r, dry_run, json, None);
            }
            // A sweep that failed must not stop the service — the next one may well succeed, and
            // this is the process that finishes every deferred landing on the machine. Said once
            // while it stays the same, as the compaction's failure is: a database a newer jkb
            // migrated fails every pass until that jkb's `setup.sh` replaces this one.
            Err(e) => {
                let why = format!("{e:#}");
                if why != last_sweep_failure {
                    eprintln!("reap: {why}");
                    last_sweep_failure = why;
                }
            }
        }
        // Slept in slices so Ctrl-C is answered promptly rather than up to `interval` later.
        let deadline = std::time::Instant::now() + interval;
        while std::time::Instant::now() < deadline
            && !stop.load(std::sync::atomic::Ordering::SeqCst)
        {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    Ok(())
}

/// One sweep, with the database opened for it and closed after.
///
/// **Per sweep, not once for the service**, for the reason the queue's compaction is: the records are
/// in the database now (tasks S6.4 stage 3), and a database this binary cannot open — one a newer jkb
/// migrated, routine across branches here — must fail this pass, not the process. The service loop
/// reports the failure and tries again next interval, where exiting put launchd into a restart loop.
fn reap_once(db_path: &Path, retain_days: u64, dry_run: bool) -> Result<archive::Report> {
    let db = open_db(db_path)?;
    let backend = jkb_api::LocalBackend::new(db).with_actor("reap");
    let stores = archive::Stores::new(session_cli::Kb::new(&backend), Some(db_path));
    archive::reap(&stores, retain_days, dry_run)
}

/// `jkb serve`: run the daemon until Ctrl-C.
///
/// A database this build cannot open does not stop it. Exiting would put launchd/systemd into a
/// restart loop — the reap unit's history with a newer branch's migration — and leave every client
/// with `unavailable` and no hint. Instead it binds, writes the token, answers each request with the
/// reason — `schema_newer` when a newer jkb migrated the database (setup.sh restarts the daemon from
/// that jkb), `unavailable` for any other failure to open it — and tries the open again every few
/// seconds, so a failure that passes (a lock held past the busy timeout) needs no restart
/// (`jkb_daemon::server::spawn_opening`).
fn cmd_serve(
    db_path: &Path,
    addr: std::net::SocketAddr,
    token_file: Option<PathBuf>,
) -> Result<()> {
    let token_path = serve_token_for(
        token_file,
        addr.port(),
        std::env::var_os("JKB_NS_MARKER").is_some(),
        jkb_core::refuse_shared_filesystem,
    )?;
    let cfg = jkb_daemon::server::ServeConfig::new(addr, token_path.clone());
    let path = db_path.to_path_buf();
    let open: jkb_daemon::server::Opener = Box::new(move || {
        open_db(&path).map_err(|e| {
            jkb_api::ApiError::with_code(
                mq_cli::open_failure_code(&e),
                format!("jkb serve cannot open the database: {e:#}"),
            )
        })
    });
    let handle = jkb_daemon::server::spawn_opening(open, &cfg).context("starting jkb serve")?;
    // One line, flushed, that a supervisor log and a test can both read: the address actually
    // bound (a `:0` port is resolved) and where the token went.
    println!(
        "jkb serve listening on http://{} (token: {})",
        handle.addr,
        token_path.display()
    );
    {
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = ctrlc::set_handler(move || {
        let _ = tx.send(());
    });
    let _ = rx.recv();
    handle.shutdown();
    Ok(())
}

/// The token path `jkb serve` writes: the one it was given, or the default for its port.
///
/// The default is refused in two cases, and an explicit `--token-file` in neither — it is the
/// caller's own decision.
///
/// * **Inside the dev container** (its image sets `JKB_NS_MARKER`), or **on a filesystem shared with
///   another kernel.** There `~/.jkb` IS the host's, through a bind, so a `jkb serve` run inside (an
///   agent's smoke test, say) replaced the host daemon's live token: the host daemon kept the old one
///   in memory, and every client — the Mac's notifier, every hook — was refused until it restarted.
///   The filesystem type alone misses a native-Linux engine, whose bind is the host's own ext4
///   (stage-5 review); the marker is the container saying what it is. Residual: another container,
///   with no marker, on a native-Linux bind.
/// * **Port 0.** The port is chosen at bind, so `daemon/0/token` is a path no client can derive, and
///   two such daemons would overwrite each other's.
fn serve_token_for(
    explicit: Option<PathBuf>,
    port: u16,
    in_container: bool,
    refuse: impl Fn(&Path) -> jkb_core::Result<()>,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if port == 0 {
        anyhow::bail!(
            "jkb serve on port 0 has no default token path a client could find (the port is chosen \
             at bind); pass --token-file"
        );
    }
    let path = service::serve_token_path(port);
    if in_container {
        anyhow::bail!(
            "jkb serve would write its token to {}, and this is the dev container (JKB_NS_MARKER is \
             set), whose ~/.jkb is the host's — the host's jkb serve owns that token; run jkb serve \
             on the host, or pass --token-file",
            path.display()
        );
    }
    refuse(&path).with_context(|| {
        format!(
            "jkb serve would write its token to {}, which another kernel's jkb serve may own (the \
             dev container sees the host's ~/.jkb); run jkb serve on the host, or pass --token-file",
            path.display()
        )
    })?;
    Ok(path)
}

/// What one compaction pass of the message queue did.
enum Compaction {
    /// Nothing to reap or remove.
    Quiet,
    /// Reaped or removed something; the summary.
    Did(String),
    /// Could not run; why.
    Failed(String),
}

fn print_compaction(c: &Compaction, json: bool) {
    let (outcome, detail) = match c {
        Compaction::Quiet => return,
        Compaction::Did(d) => ("compacted", d),
        Compaction::Failed(d) => ("failed", d),
    };
    if json {
        println!(
            "{}",
            serde_json::json!({ "mq_compact": { "outcome": outcome, "detail": detail } })
        );
    } else {
        println!("mq compact: {detail}");
    }
}

/// One compaction pass of the message queue, for the reap service.
///
/// It opens the database itself and never lets that failure reach the sweep. The sweep deliberately
/// does not open the database — a schema this binary does not know would put the service into a
/// restart loop — and compaction must not reintroduce that dependency through the back door.
/// Compaction is a no-op for a topic compacted within its own interval, so asking every pass is
/// cheap; "every few days" is the queue's rule, not this timer's.
fn compact_queue(db_path: &Path) -> Compaction {
    use jkb_api::{Backend as _, Response};
    let db = match open_db(db_path) {
        Ok(db) => db,
        Err(e) => return Compaction::Failed(format!("not run ({e:#})")),
    };
    match jkb_api::LocalBackend::new(db)
        .with_actor("reap")
        .call(jkb_api::Request::MqCompact { force: false })
    {
        Ok(Response::Compacted {
            messages_reaped,
            groups_removed,
            ..
        }) if messages_reaped > 0 || groups_removed > 0 => Compaction::Did(format!(
            "reaped {messages_reaped} message(s), removed {groups_removed} idle group(s)"
        )),
        Ok(_) => Compaction::Quiet,
        Err(e) => Compaction::Failed(e.message),
    }
}

fn report_reap(r: &archive::Report, dry_run: bool, json: bool, compaction: Option<&Compaction>) {
    if json {
        // The queue's compaction rides in the same object: a second JSON document after this one
        // made `jkb --json task reap`'s stdout neither JSON nor NDJSON.
        let mq_compact = match compaction {
            None | Some(Compaction::Quiet) => serde_json::Value::Null,
            Some(Compaction::Did(d)) => serde_json::json!({ "outcome": "compacted", "detail": d }),
            Some(Compaction::Failed(d)) => serde_json::json!({ "outcome": "failed", "detail": d }),
        };
        println!(
            "{}",
            serde_json::json!({
                "mq_compact": mq_compact,
                "dry_run": dry_run,
                "archived": r.archived.iter()
                    .map(|(uid, p)| serde_json::json!({ "uid": uid, "archive": p.display().to_string() }))
                    .collect::<Vec<_>>(),
                "held": r.held.iter()
                    .map(|(uid, why)| serde_json::json!({ "uid": uid, "reason": why }))
                    .collect::<Vec<_>>(),
                "deleted": r.deleted.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "cleared": r.cleared,
                "kept_branches": r.kept_branches.iter()
                    .map(|(uid, why)| serde_json::json!({ "uid": uid, "reason": why }))
                    .collect::<Vec<_>>(),
                                "skipped": r.skipped.as_ref().map(|h| serde_json::json!({
                    "lock": jkb_api::removals::SWEEP_LEASE,
                    "holder": h.holder,
                })),
                "old_records": r.old_records,
                "retained": r.retained.len(),
                "retained_bytes": r.retained.iter().map(|p| archive::dir_size(p)).sum::<u64>(),
            })
        );
        return;
    }
    // Two verb forms rather than one prefix: a `{would}` prefix produces "would archived", and
    // this is the line an operator reads to decide whether to run it for real.
    let (archive_verb, delete_verb) = if dry_run {
        ("would archive", "would delete")
    } else {
        ("archived", "deleted")
    };
    for (uid, dest) in &r.archived {
        println!("{archive_verb} {uid} → {}", dest.display());
    }
    for path in &r.deleted {
        println!(
            "{delete_verb} {} (past the retention window)",
            path.display()
        );
    }
    for uid in &r.cleared {
        println!("cleared {uid} (nothing left to archive)");
    }
    // A plan the operator asked for that jkb declined to apply. Printed with the archive lines
    // rather than with the holds: the tree DID move, and only the branch survives.
    for (uid, why) in &r.kept_branches {
        println!("kept branch for {uid}: {why}");
    }
    // Held is the normal state when this is run from the session that owns the worktree, so it
    // says what would move it rather than reading as a failure.
    for (uid, why) in &r.held {
        println!("held {uid}: {why}");
    }
    for line in &r.old_records {
        println!("{line}");
    }
    if let Some(held) = &r.skipped {
        // Named in full: an owner on another host is Unknown and unknown frees nothing, so a lock
        // left by a container that has since been rebuilt is respected for ever by every sweep on
        // both sides. `--break-lock` is the way out, and it needs something to point at.
        println!(
            "another sweep holds the `{}` lease ({}); this one looked at nothing",
            jkb_api::removals::SWEEP_LEASE,
            if held.holder.is_empty() {
                "holder unknown"
            } else {
                &held.holder
            }
        );
        println!("  if that holder is gone for good: jkb task reap --break-lock");
    } else if r.is_empty() && r.retained.is_empty() {
        println!("nothing to reap");
    } else if !r.retained.is_empty() {
        // The size, because the alternative signal is a full disk: a session worktree carries the
        // repo's build output, which on a Rust repo is gigabytes, and it is kept for the whole
        // retention window. `--retain-days` is the knob if that matters more than the safety net.
        println!(
            "{} archive(s) still inside the retention window ({})",
            r.retained.len(),
            human_bytes(r.retained.iter().map(|p| archive::dir_size(p)).sum())
        );
    }
    if let Some(c) = compaction {
        print_compaction(c, false);
    }
}

/// What becomes of the session worktree once its commits are on the target.
///
/// Split out of [`settle_landing`] because it is the whole of the fallible half: everything here
/// runs BEFORE the task's plan is applied, so a refusal leaves the task exactly where it was and
/// the verb is re-runnable (D48).
fn dispose_session(
    stores: &archive::Stores<'_>,
    ctx: &repo::RepoCtx,
    sess: &session::Session,
    landed: &Landed<'_>,
    disposed_already: bool,
) -> Result<Disposal> {
    // Dispose of the session FIRST, because it is the fallible half. Doing it after the status
    // write left the task marked `done` with its claim freed and its worktree still there — a
    // state both escape hatches then refuse ("is done — there is nothing to land", "abandoning it
    // would reopen finished work"), recoverable only by hand-editing the status.
    //
    // DISPOSAL IS A RENAME, not a recursive delete. `git worktree remove` unlinks the tree and
    // stops at the first refusal, so landing from a sandboxed agent session — where Claude Code
    // protects the worktree's own `.claude` policy files from the agent whose policy they are —
    // gutted 152 files and then reported an error about the *directory*. `archive::stow` moves
    // the whole tree into `<repo>/.jkb/archive` in one atomic `rename`: it either happens or
    // nothing happens, and a worktree disposed of by mistake is still there to be moved back.
    // Deleting it is a separate, later decision, taken by `jkb task reap` once it has aged out.
    let mut disposal = Disposal::Kept;
    if disposed_already {
        // Somebody removed it while the gate ran. Nothing to dispose of; its registration is dropped,
        // by path, so git stops listing a worktree whose directory is gone.
        let _ = gitrepo::forget_worktree(&ctx.root, &sess.worktree);
        disposal = Disposal::AlreadyGone;
    } else if landed.keep_worktree {
        // `graft` rebased a detached HEAD, so the branch ref still points at its pre-rebase
        // commits. Left there, the kept session reads as N commits ahead of a target that
        // already contains its work, and a second `land` re-runs the whole graft. Move it to
        // what actually landed; the worktree is verified clean just above, so nothing is lost.
        gitrepo::reset_hard(&sess.worktree, landed.grafted)?;
    } else {
        // THE RESET COMES FIRST, and the ordering is load-bearing rather than tidy. `graft`
        // rebased a detached HEAD, so the branch still points at its pre-rebase commits — left
        // there, a session this cannot archive reads as N commits ahead of a target that already
        // contains its work. Moving it is cosmetic; doing it AFTER `dispose` is not, because
        // `dispose` records the worktree's HEAD as the record's instance identity and the reset
        // changes exactly that. The reaper would then find a tree that is not on the commit the
        // record names, correctly conclude it is a different session reusing the name, and hold
        // it for ever — defeating the whole deferred path with a message that reads as a guard
        // working. Reset first and `dispose` records whatever HEAD actually is, in either arm.
        //
        // Best-effort: the worktree is verified clean above, so nothing is lost either way, and a
        // landing must not be undone by a cosmetic ref move.
        if let Err(reset) = gitrepo::reset_hard(&sess.worktree, landed.grafted) {
            eprintln!(
                "note: could not point {} at what landed: {reset}",
                landed.branch
            );
        }
        // The one disposal both `land` and `abandon` call — see `archive::dispose` for why that
        // matters. A landing's branch is a duplicate of commits now in the target, so it goes.
        match archive::dispose(
            stores,
            &ctx.root,
            &sess.worktree,
            landed.branch,
            landed.uid,
            archive::Plan {
                // A landing's branch is a duplicate of commits now in the target.
                delete_branch: true,
                // Verified clean a few lines above, and nothing has run since.
                accept_dirty: false,
            },
        )? {
            archive::Disposed::Archived(dest) => disposal = Disposal::Archived(dest),
            archive::Disposed::Deferred(d) => disposal = Disposal::Deferred(d),
        }
    }

    Ok(disposal)
}

/// Bytes as a person reads them. Reported only — nothing decides on this number.
fn human_bytes(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let bytes = n as f64;
    for (unit, scale) in [("GB", 1e9), ("MB", 1e6), ("kB", 1e3)] {
        if bytes >= scale {
            return format!("{:.1} {unit}", bytes / scale);
        }
    }
    format!("{n} B")
}

/// What actually became of the session worktree. Three of these used to be one `bool`, which is
/// how "removed session and its branch" got printed by the arm that had only run `worktree prune`.
enum Disposal {
    /// Somebody else removed it while the gate ran.
    AlreadyGone,
    /// Moved into the repo's archive; the branch is deleted and the retention sweep owns it now.
    Archived(PathBuf),
    /// Nothing moved — this process may not unlink the tree — and a record was left for the
    /// reaper. Carries the refusal AND the verdict on the record, because a report that says
    /// "`jkb task reap` will finish it" has to have asked the thing that decides that.
    Deferred(archive::Deferral),
    /// `--keep-worktree`: the session stays, by request.
    Kept,
}

impl Disposal {
    /// Whether the session directory is no longer where it was.
    fn session_gone(&self) -> bool {
        matches!(self, Self::AlreadyGone | Self::Archived(_))
    }
}

/// The working tree to graft in: wherever `onto` is already checked out, else a checkout of
/// it under `.jkb/base`. `git` refuses to check one branch out twice, so borrowing an
/// existing checkout is not an optimization — it is the only option when there is one.
///
/// `.jkb/base` is **reused**, switched to whatever branch this land needs. `git worktree add`
/// refuses a path that already exists, so adding a second one would fail the moment a batch
/// landed onto a different branch than the last, and keep failing until the directory was
/// deleted by hand.
fn land_dir_for(ctx: &repo::RepoCtx, onto: &str) -> Result<PathBuf> {
    if let Some(dir) = gitrepo::worktree_for_branch(&ctx.root, onto)? {
        return Ok(dir);
    }
    let base = session::base_worktree(&ctx.root);
    // Whether git knows a worktree there — NOT whether it has a branch. A **detached**
    // `.jkb/base` has no branch, so the branch test sent it to the "exists but git does not
    // know it" bail below and refused every landing, while `staging::land_dir_in` matched the
    // same directory by path and reported the task landable. `switch_to` attaches it either
    // way, so a detached base is an ordinary reusable cache.
    // Collapsing to "not registered" is the safe direction: the arm it selects only refuses,
    // with git's own message, rather than reusing a checkout on an unverified premise.
    let base_registered = gitrepo::worktrees(&ctx.root)?
        .unwrap_or_default()
        .iter()
        .any(|w| session::same_path(&w.path, &base));
    if base_registered {
        // It exists and holds some other branch (or none) — if it held `onto`, the lookup
        // above would have found it. Reuse it: it is a cache, and switching keeps its
        // build artifacts.
        //
        // This is not a second copy of the land gate's dirty rule (that one lives in
        // `staging::target_dirty_reason` and is asked once, under the lock). It guards the
        // mutation on the *next* line: `git switch` across branches carries uncommitted changes
        // over, or refuses outright with a message about neither jkb nor the task. Hence its own
        // remedy, which is about this scratch checkout rather than about landing.
        anyhow::ensure!(
            gitrepo::is_dirty(&base, &ctx.root)?.is_no(),
            "{} could not be established as clean — it is jkb's own scratch checkout, so commit \
             or discard any changes, or remove it with `git worktree remove --force` and let \
             jkb recreate it",
            base.display()
        );
        gitrepo::switch_to(&base, onto)?;
        return Ok(base);
    }
    anyhow::ensure!(
        !base.exists(),
        "{} exists but git does not know it as a worktree — look at it, then move it \
         out of the way (not `git worktree prune`, which drops every checkout this side cannot see)",
        base.display()
    );
    if let Some(parent) = base.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    gitrepo::worktree_add(&ctx.root, &base, onto, onto)?;
    Ok(base)
}

/// The last `n` lines of `text` — enough of a failed gate to act on, without replaying the
/// whole build.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// What happened to a disposed session's branch. Shared by `land` and `abandon`, which asked the
/// same question two different ways until one of them started answering it from which code path
/// had run rather than from git.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BranchFate {
    /// Deletion was not asked for; it is still there.
    Kept,
    /// Deletion was not asked for and there is nothing there anyway — somebody removed it, or
    /// `dispose` did. Distinct from `Kept`, whose remedy (`git branch -D`) would error with
    /// "branch not found" on a branch that is already gone.
    Absent,
    /// Gone.
    Deleted,
    /// Asked for, and owed: the deferred worktree still has it checked out, so `git branch -D`
    /// would refuse. `jkb task reap` deletes it once the tree is archived.
    OwedToTheReaper,
}

/// The one rule, applied by both callers — which is what the enum above says it is for, and what
/// the two of them had stopped doing again.
///
/// `present` is three-valued and only a **proven** absence is a deletion. The arm that was wrong
/// is the other one: "asked to delete and not proven absent" was read as `OwedToTheReaper`
/// without asking whether anything had been deferred. In `abandon` nothing has been, in the only
/// case that reaches it — the branch is deleted right here when it is proven there, so arriving
/// undeleted with no record means the answer was `Unknown` — and the line it prints then promises
/// a deletion `jkb task reap` will never perform, because there is no record for it to act on.
///
/// **The reaper is owed something only when a record exists.** That is the fact the message is
/// about, so it is the fact the fate is derived from.
///
/// The two `bool`s are deliberately not adjacent: with `present` between them a transposition is
/// a type error rather than a silently wrong answer, which is the one thing a three-argument
/// signature of mostly-`bool` can do about its own worst failure mode.
fn branch_fate(asked_to_delete: bool, present: Fact, reaper_will_act: bool) -> BranchFate {
    if present.is_no() {
        // Nothing there. Which of the two got it there is not a distinction with a remedy, and
        // neither fate prints one; they differ only in what the JSON reports.
        return if asked_to_delete {
            BranchFate::Deleted
        } else {
            BranchFate::Absent
        };
    }
    // `reaper_will_act`, NOT merely "a record was written". A record the sweep is going to hold
    // — a tree it cannot identify — owes nobody anything, and saying otherwise sent an operator
    // to run `jkb task reap` for a branch no reap would ever delete. It comes from
    // `Deferral::will_be_swept`, i.e. from the verdict the sweep itself executes.
    if asked_to_delete && reaper_will_act {
        BranchFate::OwedToTheReaper
    } else {
        // `Unknown` lands here rather than on `Deleted`: reporting a deletion nobody observed is
        // the one direction that costs something, because it stops the operator looking. The
        // remedy this prints (`git branch -D`) merely errors on a branch that is already gone.
        BranchFate::Kept
    }
}

/// The `doctor` line for this repo's task sessions. Best-effort: `doctor` is often run
/// outside a git repo entirely, and that is not a fault to report.
///
/// Every session is listed, not just some subset flagged as neglected — nothing here can tell
/// a session you are working in from one you walked away from (design D36.6), and a report
/// that guesses would tell you to abandon the work you are doing.
/// Worktrees a landing disposed of, or could not (design D49).
///
/// Reported by `doctor` because both halves are otherwise invisible: a deferred removal is a
/// session directory sitting where a completed landing left it, and an archive is disk that will
/// be deleted on a schedule nobody was told about. `--fix` runs the same sweep the service runs,
/// which is the whole point of the record living in the database rather than in a repo.
fn report_worktree_removals(db: &Db, db_path: &Path, fix: bool) {
    let backend = jkb_api::LocalBackend::new(db.clone()).with_actor("cli");
    let stores = archive::Stores::new(session_cli::Kb::new(&backend), Some(db_path));
    let store = match archive::entries(&stores) {
        Ok(s) => s,
        Err(e) => {
            println!("worktree removals: unknown ({e})");
            return;
        }
    };
    let legacy = store.legacy.lines();
    if store.records.is_empty() && store.rejected.is_empty() && legacy.is_empty() {
        println!("worktree removals: none pending");
        if fix {
            fix_worktree_removals(&stores);
        }
        return;
    }
    let archived: Vec<_> = store
        .records
        .iter()
        .filter(|(_, e)| e.archive.is_some())
        .collect();
    // THE GOVERNING records, and their verdicts, from the one read the sweep itself uses. Asking
    // `pending_verdict` per record was two-thirds of the rule: the sweep drops a record a later
    // disposal replaced BEFORE it evaluates anything, so a withdrawn record was getting a vote
    // here and re-printing advice the operator had already taken — and the count above it said
    // "2 awaiting archive" for one worktree.
    //
    // Asked of the `store` ALREADY READ, not of the path again. Two reads meant the archived count
    // and the pending count came from two views of one file, so a record a concurrent sweep
    // archived between them appeared in neither; and the second read's error was swallowed to an
    // empty list, which printed "0 awaiting archive" over a store full of deferred checkouts and
    // suppressed the `--fix` line with it.
    let pending = archive::pending_outlook_in(&store);
    // AWAITING AND HELD ARE COUNTED APART, and this is the surface `jkb task sessions` sends you
    // to. That listing was changed this round to stop saying "awaiting archive" about a record
    // nothing will ever move — `[awaiting archive]` is a promise that something is going to finish
    // this — and then the header here went on making exactly that promise, for three checkouts at
    // once, with the `--fix` line suppressed because every one of them was held.
    let (held, awaiting): (Vec<_>, Vec<_>) = pending
        .iter()
        .partition(|(_, v)| matches!(v, archive::Verdict::Hold(_)));
    println!(
        "worktree removals: {} awaiting archive, {} held, {} archived",
        awaiting.len(),
        held.len(),
        archived.len()
    );
    // WHY it has not moved, from the verdict the sweep executes. "not yet moved" beside "run
    // `jkb doctor --fix`" was true of a record waiting for the service and false of one no sweep
    // can act on — and the two rendered identically, so the operator ran the fix repeatedly
    // against a record it would report the same way for ever.
    for (e, verdict) in &pending {
        match verdict {
            archive::Verdict::Hold(b) => {
                println!(
                    "  {} — {} is HELD: {} — {}",
                    e.uid,
                    e.worktree.display(),
                    b.reason,
                    b.remedy.advice()
                );
                // SAID OUT LOUD when the way out destroys something. Every other remedy here is
                // recoverable — commit, restore, re-record, fix a permission — and exactly one
                // is not, so a reader skimming a list of holds must not have to recognise which
                // by its wording. jkb never takes this step itself; it is the operator's, and
                // the reason it is offered at all is that the tree could not be shown to be ours
                // (D34.4: where the reversible act is available, it wins).
                if b.remedy.is_destructive() {
                    println!("      this one is not reversible — jkb will not do it for you");
                }
            }
            _ => println!("  {} — {} not yet moved", e.uid, e.worktree.display()),
        }
    }
    for (_, e) in &archived {
        if let Some(dir) = &e.archive {
            println!("  {} — archived at {}", e.uid, dir.display());
        }
    }
    if !archived.is_empty() {
        // What the safety net costs, said out loud. A landed session's checkout carries the
        // repo's build output, so this is the number that decides whether 30 days is the right
        // window here — and the alternative way to learn it is a full disk.
        // Asked only of archives this machine can actually see. `dir_size` walks nothing for a
        // path that is not there and returns 0, so a container-written archive read as "0 B held"
        // on the host while occupying gigabytes of the same disk — a measurement that could not be
        // taken, reported as a measurement of none.
        let (seen, unseen): (Vec<_>, Vec<_>) = archived
            .iter()
            .filter_map(|(_, e)| e.archive.as_deref())
            .partition(|dir| dir.exists());
        // Only when there is something to have measured. With every archive on the other side of
        // the container bind, "0 B held until they age out" printed beside "1 whose size is
        // unknown" reads as "these occupy nothing" — a measurement of none where none was taken.
        if !seen.is_empty() {
            let bytes: u64 = seen.iter().copied().map(archive::dir_size).sum();
            println!("  {} held until they age out", human_bytes(bytes));
        }
        if !unseen.is_empty() {
            println!(
                "  and {} archive(s) whose size is unknown from here (their repo is not reachable)",
                unseen.len()
            );
        }
    }
    // The old file store: what an older jkb left there, for the operator to judge.
    for line in &legacy {
        println!("  {line}");
    }
    // Refused records were invisible here while `reap` reported them held — so a store holding
    // nothing BUT refused records read as "none pending", which is the one state a person needs
    // to be told about.
    for r in &store.rejected {
        println!("  {} — REFUSED: {} ({})", r.uid, r.why, r.marker);
    }
    if fix {
        fix_worktree_removals(&stores);
    } else if !awaiting.is_empty() {
        println!("  run `jkb doctor --fix` or `jkb task reap` (the watcher service runs it)");
    }
}

/// `doctor --fix`'s sweep — the one the service runs.
fn fix_worktree_removals(stores: &archive::Stores<'_>) {
    match archive::reap(stores, archive::RETAIN_DAYS, false) {
        // The old store was listed a few lines above; once is enough.
        Ok(r) => report_reap(
            &archive::Report {
                old_records: Vec::new(),
                ..r
            },
            false,
            false,
            None,
        ),
        Err(e) => println!("  sweep failed: {e}"),
    }
}

fn report_sessions(kb: &session_cli::Kb<'_>) {
    let Ok(ctx) = repo::repo_ctx() else { return };
    let Ok(sessions) = session::discover(&ctx.root) else {
        return;
    };
    if sessions.is_empty() {
        return;
    }
    let by_branch = kb.by_branch(&ctx.key).unwrap_or_default();
    println!("task sessions: {} in flight", sessions.len());
    for s in &sessions {
        match by_branch
            .get(&s.branch)
            .and_then(|ts| session_cli::task_on(ts))
        {
            Some(t) => println!(
                "  {} — {uid}: resume with `cd {}`, land it with `jkb task land {uid}`, or drop \
                 it with `jkb task abandon {uid}`",
                s.name,
                s.worktree.display(),
                uid = t.uid
            ),
            // No task records this branch — a `task work` that stopped before recording where it
            // was, typically. The task's own `task work` or `task abandon` finds it through its claim.
            None => println!(
                "  {} — no task records {}; `jkb task work <uid>` resumes it, `jkb task abandon \
                 <uid>` drops it, for the task that opened it",
                s.name, s.branch
            ),
        }
    }
}

/// One task's verdict in a `close-merged` run.
struct CloseVerdict {
    uid: String,
    /// The pull request consulted, when there was one.
    pr: Option<i64>,
    /// Why it was **not** closed, or `None` if it was.
    held: Option<String>,
}

/// `task close-merged`: close every task in this repo whose pull request has merged.
///
/// **A lookup, not an inference.** This used to ask the commit graph *"does this branch add
/// anything to trunk?"*, which cannot distinguish a branch whose work was squash-merged away
/// from one that never started — so making it answerable needed a stored cut point per branch,
/// a reflog-derived anchor saying which *instance* of a reusable name that cut point described,
/// and a supersede rule for when the name changed hands. Roughly a quarter of the
/// `staging-workflow` review corpus's must-fix findings lived in that apparatus, and it is gone:
/// a pull request number is minted by GitHub and never reused, so there is nothing to
/// disambiguate.
///
/// It closes nothing it cannot prove. No number recorded, no `gh`, no network, a branch name
/// that matches two pull requests — every one of those is [`Fact::Unknown`], the lifecycle holds
/// the task, and the reason is printed. A missed close costs one command; a wrong one buries
/// work still in flight (design D34.4).
///
/// # Errors
/// Errors if the repo cannot be resolved or a database read or write fails.
fn cmd_task_close_merged(db: &Db, repo: Option<String>, dry_run: bool, json: bool) -> Result<()> {
    let ctx = repo::repo_ctx().map_err(|e| anyhow::anyhow!("{e}"))?;
    let repo = repo.unwrap_or_else(|| ctx.key.clone());
    // **Refused when `--repo` names somewhere else.** Pull request numbers are per-repository
    // and low ones collide by construction, so resolving another repo's task against *this*
    // checkout asks `gh pr view 31` here and closes on an unrelated merge — D34.4's "a wrong
    // close buries work still in flight". The predecessor refused this outright; deleting the
    // whole inference took its guard with it.
    anyhow::ensure!(
        repo == ctx.key,
        "`--repo {repo}` names a different repository from this checkout ({}), and pull request \
         numbers are per-repository — asking `gh` here would resolve {}'s numbers against {}'s. \
         Run it from {repo}'s checkout.",
        ctx.key,
        repo,
        ctx.key,
    );
    // Typed, not interpolated into the DSL: `--repo` is user-typed, and a value with whitespace
    // would re-tokenize into a different query that matches nothing — closing no task and
    // reporting no error.
    let query = jkb_core::location::tasks_in_repo(&repo);
    let ids = db.read(move |conn| query.evaluate(conn))?;

    let mut verdicts = Vec::new();
    for id in ids {
        // A finished task costs nothing. `tasks_in_repo` deliberately keeps terminal tasks (the
        // staging view needs them), so the filter lives here — without it this fired a `gh`
        // subprocess and a write transaction per long-`done` task on **every `git pull`**, via
        // the `post-merge` hook, and then reported them under "closed N task(s)" because
        // `Outcome::Idempotent` has no refusal to print.
        let status = db
            .read(move |conn| item::get(conn, id))?
            .and_then(|m| m.status);
        if jkb_types::TaskStatus::is_terminal_str(status.as_deref()) {
            continue;
        }
        verdicts.push(close_one(db, &ctx.root, id, dry_run)?);
    }
    report_close_merged(&verdicts, dry_run, json);
    Ok(())
}

/// Decide one task, and close it if a merged pull request proves it landed.
///
/// The decision is the lifecycle's, not this function's: it gathers facts and asks for
/// [`lifecycle::TaskEvent::ObservedLanded`], whose guard requires the merge **proven** and no
/// open subtasks. Everything this used to decide for itself — is a missing branch a landing? is
/// a zero-commit branch merged? does this record describe this branch? — was a question only the
/// graph inference had to ask.
fn close_one(db: &Db, root: &Path, id: ItemId, dry_run: bool) -> Result<CloseVerdict> {
    let uid = db
        .read(move |conn| item::get(conn, id))?
        .map(|m| m.uid)
        .unwrap_or_default();
    // **A landing jkb itself recorded is asked about first**, because when the merge queue
    // grafted locally it is the only evidence that exists — there is no pull request to ask
    // about. A task held for an open subtask has exactly that entry (see `cmd_task_landed`), and
    // asking GitHub first meant it was never reached: `discover_quietly` returns a hold whenever
    // it cannot name a pull request, which is always for such a branch.
    //
    // One read of the history for all of it — the landing, whether it still counts, when the task
    // was last put back to work, and any recorded pull request number.
    let landing = db.read(move |conn| jkb_core::transition::landing(conn, id))?;

    // **A superseded landing is context, never a verdict.** It says the local graft is stale; it
    // says nothing about whether the work reached its destination another way. Returning early on
    // it left a task whose work was redone and merged as a pull request permanently unclosable —
    // printing "it will close when the new work lands" after the new work had landed. So it falls
    // through to the pull-request evidence and only colours the reason if that proves nothing
    // either.
    let superseded = landing.superseded().map(|(landed, resumed)| {
        format!(
            "its earlier landing onto {} was superseded when the task went back to work ({} at {})",
            landed.labels.onto.as_deref().unwrap_or("its target"),
            resumed.event,
            resumed.at
        )
    });
    let with_context = |why: String| match &superseded {
        Some(note) => format!("{why}; {note}"),
        None => why,
    };

    let (number, merged, why) = if landing.live().is_some() {
        // The evidence used was the recorded landing, so no pull request number is reported: this
        // path never asked about one, and printing "closed (pull request #N)" would credit a
        // number that had no part in the decision.
        (None, Fact::Yes, None)
    } else {
        // Discover once, from the branch, and record what is found — after which the number is
        // what is consulted and the branch name never is again.
        let number = match landing.pr_number() {
            Some(n) => Some(n),
            None => match discover_quietly(db, root, id)? {
                Ok(found) => found,
                Err(why) => {
                    return Ok(CloseVerdict {
                        uid,
                        pr: None,
                        held: Some(with_context(why)),
                    })
                }
            },
        };
        // A merge older than the last resumption is not proof about the work in flight. Both
        // evidence paths answer to the one rule.
        let (merged, why) = pr::merged_fact(root, number, landing.resumed_at());
        (number, merged, why.map(with_context))
    };
    let outcome = db.write_txn_with::<_, anyhow::Error, _>("cli", move |conn, meta| {
        // The status is re-read **inside** the transaction. This snapshots every candidate up
        // front and then runs a subprocess per task, and it runs from a post-merge hook over all
        // of them at once — long enough for a `jkb task set --status cancelled` to land in
        // between and be silently overwritten with `done`.
        let facts = lifecycle::TaskFacts {
            landed_elsewhere: merged,
            ..task::observe(conn, id)?
        };
        if dry_run {
            return Ok(lifecycle::apply(
                &facts,
                lifecycle::TaskEvent::ObservedLanded,
            ));
        }
        Ok(jkb_core::transition::perform(
            conn,
            meta,
            id,
            &facts,
            lifecycle::TaskEvent::ObservedLanded,
            &jkb_core::transition::Labels {
                pr_number: number,
                ..jkb_core::transition::Labels::default()
            },
        )?)
    })?;
    // A refusal carries its own sentence; `why` explains an unobtainable answer, which the
    // guard can only report as "not proven". Both, when there are both: the guard says what it
    // needed and `why` says what stopped us getting it.
    let held = outcome.refusal().map(|r| match &why {
        Some(w) => format!("{r} ({w})"),
        None => r,
    });
    Ok(CloseVerdict {
        uid,
        pr: number,
        held,
    })
}

/// Try to find this task's pull request from its recorded branch, without failing the run.
///
/// `Err` is a *reason to hold this task*, not an error: `close-merged` runs over every task in a
/// repo from a `post-merge` hook, and one task with no branch, an ambiguous branch name or no
/// `gh` must not stop the rest from closing. That was a real must-fix here — a single malformed
/// value aborted the entire run, silently.
fn discover_quietly(db: &Db, root: &Path, id: ItemId) -> Result<Result<Option<i64>, String>> {
    let branch = db
        .read(move |conn| jkb_core::transition::latest_with_branch(conn, id))?
        .and_then(|r| r.labels.branch);
    let Some(branch) = branch else {
        return Ok(Err(
            "no branch recorded, so there is no pull request to look up —              `jkb task pr <uid> <number>` to name one"
                .to_owned(),
        ));
    };
    Ok(match pr::discover(root, &branch) {
        pr::Discovery::One(found) => {
            let number = found.number;
            record_pr(db, id, number)?;
            Ok(Some(number))
        }
        pr::Discovery::None => Err(format!("no pull request has `{branch}` as its head branch")),
        // The recycled-name case, held rather than guessed — which is what the old inference
        // could not do, because a name was all it had.
        pr::Discovery::Ambiguous(numbers) => Err(format!(
            "`{branch}` is the head branch of {} pull requests — pick one with              `jkb task pr <uid> <number>`",
            numbers.len()
        )),
        pr::Discovery::Unavailable(why) => Err(why),
    })
}

/// Print what a `close-merged` run decided.
///
/// Two buckets, where there used to be six. The five hold-reasons the old version distinguished
/// — no cut point, an unusable one, a stale record, a gone branch, genuinely in flight — were
/// five ways for one inference to fail, and each needed its own remedy sentence. A held task now
/// carries the reason the guard gave.
fn report_close_merged(verdicts: &[CloseVerdict], dry_run: bool, json: bool) {
    let (closed, held): (Vec<_>, Vec<_>) = verdicts.iter().partition(|v| v.held.is_none());
    if json {
        println!(
            "{}",
            serde_json::json!({
                "dry_run": dry_run,
                "closed": closed.iter().map(|v| serde_json::json!({"uid": v.uid, "pr": v.pr}))
                    .collect::<Vec<_>>(),
                "held": held.iter().map(|v| serde_json::json!({
                    "uid": v.uid, "pr": v.pr, "reason": v.held
                })).collect::<Vec<_>>(),
            })
        );
        return;
    }
    let verb = if dry_run { "would close" } else { "closed" };
    println!("{verb} {} task(s)", closed.len());
    for v in &closed {
        match v.pr {
            Some(n) => println!("  {} (pull request #{n})", v.uid),
            None => println!("  {}", v.uid),
        }
    }
    if !held.is_empty() {
        println!("held {}:", held.len());
        for v in &held {
            println!("  {} — {}", v.uid, v.held.as_deref().unwrap_or(""));
        }
    }
}

/// Print a short human/JSON confirmation for a task mutation.
fn report(json: bool, uid: &str, action: &str) {
    if json {
        println!("{}", serde_json::json!({"uid": uid, "action": action}));
    } else {
        println!("{action}: {uid}");
    }
}

/// Resolve a task reference (full `task:<slug>` uid or bare slug) to its item id.
///
/// # Errors
/// Errors if no item matches either the given uid or `task:<uid>`.
fn resolve_task_uid(db: &Db, uid: &str) -> Result<ItemId> {
    let reference = uid.to_owned();
    let id = db.read(move |conn| task::resolve_ref(conn, &reference))?;
    id.ok_or_else(|| anyhow::anyhow!("no item with uid {uid}"))
}

fn cmd_view(db: &Db, cmd: ViewCmd, json: bool) -> Result<()> {
    match cmd {
        ViewCmd::Save { name, query } => {
            let (name2, dsl) = (name.clone(), query.join(" "));
            db.write_txn("cli", move |conn, meta| {
                view::save(conn, meta, &name2, &dsl)
            })?;
            println!("saved view {name}");
        }
        ViewCmd::Ls => {
            let views = db.read(view::list)?;
            if json {
                let arr: Vec<_> = views
                    .iter()
                    .map(|(n, q)| serde_json::json!({"name": n, "query": q}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else if views.is_empty() {
                println!("(no views)");
            } else {
                for (n, q) in views {
                    println!("{n}: {q}");
                }
            }
        }
        ViewCmd::Run { name } => {
            let name2 = name.clone();
            let ids = db.read(move |conn| view::run(conn, &name2))?;
            let items = output::fetch_items(db, &ids)?;
            output::print_items(&items, json);
        }
    }
    Ok(())
}

fn cmd_undo(db: &Db, txn: Option<i64>) -> Result<()> {
    // No vector sweep: see `cmd_item_rm`. Undoing an ingest leaves the chunks' vector rows
    // behind, and since D40 that is inert rather than the permanent corruption it once was.
    let n = db.write_txn("cli", move |conn, meta| match txn {
        Some(txn) => undo::undo(conn, meta, txn),
        None => undo::undo_last(conn, meta),
    })?;
    println!("reverted {n} change(s)");
    Ok(())
}

/// Remove stale derived-index rows, returning what went.
///
/// The **one** cleanup, called from `jkb index --sweep` and `jkb doctor --fix`. Nothing sweeps
/// implicitly any more: before D40 every path that deleted an item had to sweep in its own
/// transaction or a new item inherited a dead embedding, and that obligation was discovered
/// one missed call site at a time over four review passes. `AUTOINCREMENT` removed the hazard,
/// which turns cleanup from an invariant every writer must uphold into housekeeping one verb
/// can do whenever it is convenient.
fn sweep_stale(db: &Db) -> Result<jkb_index::StaleRows> {
    db.write_txn_with::<_, anyhow::Error, _>("cli", |conn, _meta| Ok(jkb_index::sweep_stale(conn)?))
}

fn cmd_index(db: &Db, sweep: bool) -> Result<()> {
    // The on-demand half of the derived-index hygiene pair (design D40). `AUTOINCREMENT`
    // makes a leftover vector row harmless — its id is never reissued, so nothing can inherit
    // its embedding — but not absent, and an index that only grows is worth being able to
    // clean without running the whole of `doctor`. Deliberately its own flag rather than
    // something `index` always does: embedding needs a live embedder and this must not.
    if sweep {
        let removed = sweep_stale(db)?;
        if removed.is_empty() {
            println!("index: no stale rows");
        } else {
            println!("index: removed {} stale vector row(s)", removed.vectors);
        }
        return Ok(());
    }
    // Embed every content-bearing item not yet in the vector index (D21): this covers
    // items created by file sync, not just the ingest pipeline. Needs a live embedder.
    let pipeline = Pipeline::new(embedder()?);
    let pending = pipeline.unembedded_count(db)?;
    if pending == 0 {
        println!("index: nothing to embed (all content items are indexed)");
        return Ok(());
    }
    println!("index: embedding {pending} pending item(s)…");
    let report = pipeline.index_pending(db)?;
    println!(
        "index: {} vector(s) written — {} embedded, {} derived from chunks",
        report.total(),
        report.embedded,
        report.derived
    );
    if report.failed > 0 {
        // Report rather than fail: the run wrote everything it could, and the skipped items
        // stay pending so a later run retries them.
        eprintln!(
            "index: {} item(s) skipped; rerun to retry. first error: {}",
            report.failed,
            report.first_error.as_deref().unwrap_or("(none recorded)")
        );
    }
    Ok(())
}

/// The embedder half of `jkb doctor`: whether the model answers, and how much waits for it. Host-only:
/// `jkb serve` calls no model.
fn report_embedder(db: &Db) {
    let embed_status = match embedder().and_then(|e| e.health_check().map_err(Into::into)) {
        Ok(()) => "ok".to_owned(),
        Err(e) => format!("unavailable: {e}"),
    };
    println!("embedder: {embed_status}");
    match embedder().and_then(|e| Ok(Pipeline::new(e).unembedded_count(db)?)) {
        Ok(pending) => println!("un-embedded items: {pending}"),
        Err(e) => println!("un-embedded items: unknown ({e})"),
    }
}

// ---- small formatting helpers ---------------------------------------------

fn output_line(item: &output::DisplayItem) -> String {
    let ns = item
        .namespace
        .as_ref()
        .map_or(String::new(), |n| format!(" <{n}>"));
    let snip = item
        .snippet
        .as_deref()
        .filter(|s| !s.is_empty())
        .map_or(String::new(), |s| format!(" — {s}"));
    format!("{}{ns}{snip}", item.uid)
}

/// The first line of a body, for a one-line report. The derivation is `output::first_nonblank`
/// (the one copy); only the width is this function's own.
fn first_line(content: &str) -> String {
    truncate(output::first_nonblank(content), 100)
}

/// `jkb staging ls` — the staging branches in this repo and what is landing on each.
///
/// The one read behind both the explorer's branch picker and its In Flight view (design
/// D38.2), so the two cannot disagree about what is live.
pub(crate) fn cmd_staging_ls(kb: &session_cli::Kb<'_>, all: bool, json: bool) -> Result<()> {
    let ctx = repo::repo_ctx()?;
    let rows = staging::collect(kb, &ctx, all)?;

    if json {
        let v: Vec<_> = rows
            .iter()
            .map(|s| {
                serde_json::json!({
                    "branch": s.branch,
                    "merged": s.merged,
                    "ahead": s.ahead,
                    "checkout": s.checkout,
                    "tasks": s.tasks.iter().map(|t| serde_json::json!({
                        "uid": t.uid,
                        "title": t.title,
                        "status": t.status,
                        "state": t.state.as_str(),
                        "branch": t.branch,
                        "worktree": t.worktree,
                        "dirty": t.dirty.as_str(),
                        "commits": t.commits,
                        "reviewed": t.reviewed,
                        "review_nss": t.review_nss,
                        "review_waived": t.review_waived,
                        "open_must_fix": t.open_must_fix,
                        "review_ok": t.review_ok,
                        "land_blocked": t.land_blocked,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("(no staging branches in {})", ctx.key);
        println!("one is created the first time you run `jkb task work <uid>`");
        return Ok(());
    }
    for s in &rows {
        let merged = if s.merged { "  [merged]" } else { "" };
        println!(
            "{}  {} commit(s) vs trunk · {} task(s){merged}",
            s.branch,
            s.ahead,
            s.tasks.len()
        );
        for t in &s.tasks {
            let mut notes = vec![t.state.as_str().to_owned()];
            if t.commits > 0 {
                notes.push(format!("{} commit(s)", t.commits));
            }
            // Three-valued: an unreadable checkout is not the clean case, and saying nothing
            // about it would leave the row looking landable while `land_blocked` refuses it.
            match t.dirty {
                Fact::Yes => notes.push("uncommitted".to_owned()),
                Fact::Unknown => notes.push("unreadable checkout".to_owned()),
                Fact::No => {}
            }
            if t.open_must_fix > 0 {
                notes.push(format!("{} must-fix open", t.open_must_fix));
            } else if t.reviewed.is_some() && t.review_ok {
                // "reviewed" only when the gate would actually pass on it. A review whose
                // findings never reached the KB leaves `reviewed=` on the task and is refused
                // by the gate, so printing "reviewed" told a terminal user the opposite of
                // what `jkb task land` was about to do.
                notes.push("reviewed".to_owned());
            }
            if t.review_waived.is_some() {
                notes.push("review waived".to_owned());
            }
            println!("    {}  [{}]", truncate(&t.title, 60), notes.join(" · "));
            println!("      {}", t.uid);
            // The verdict itself, not just its symptoms: this is the same string the In
            // Flight tooltip shows, and without it the terminal listing was the one surface
            // that could not say why a landing would be refused.
            if let Some(reason) = &t.land_blocked {
                println!("      cannot land: {reason}");
            }
        }
    }
    Ok(())
}

/// Shorten `s` to `n` characters with an ellipsis, for one-line listings.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    /// `jkb serve`'s default token path is refused where the refusal says so, and an explicit one is
    /// the caller's decision. The refusal itself is `jkb_core`'s shared-filesystem rule, measured in
    /// the dev container against the `~/.jkb` bind (FUSE).
    #[test]
    fn a_default_token_on_a_shared_filesystem_is_refused_and_an_explicit_one_is_not() {
        let shared = |p: &std::path::Path| -> jkb_core::Result<()> {
            Err(jkb_core::Error::SharedFilesystem {
                path: p.to_path_buf(),
                kind: "FUSE",
            })
        };
        let local = |_: &std::path::Path| -> jkb_core::Result<()> { Ok(()) };
        let err = super::serve_token_for(None, 7117, false, shared).unwrap_err();
        assert!(format!("{err:#}").contains("--token-file"), "{err:#}");
        assert!(super::serve_token_for(None, 7117, false, local)
            .unwrap()
            .ends_with(".jkb/daemon/7117/token"));
        let err = super::serve_token_for(None, 7117, true, local).unwrap_err();
        assert!(
            format!("{err:#}").contains("dev container"),
            "the container marker refuses on a local filesystem too (a native-Linux bind): {err:#}"
        );
        let err = super::serve_token_for(None, 0, false, local).unwrap_err();
        assert!(format!("{err:#}").contains("port 0"), "{err:#}");
        let given = std::path::PathBuf::from("/x/token");
        assert_eq!(
            super::serve_token_for(Some(given.clone()), 0, true, shared).unwrap(),
            given
        );
    }

    use super::GUIDE;
    use jkb_types::TaskStatus;

    /// The reaper is owed a branch only when there is a RECORD for it to act on.
    ///
    /// Round 8 made `has_branch` three-valued, correctly, and then derived the fate from
    /// `(asked_to_delete, present.is_no())` alone — so an `Unknown` with nothing deferred printed
    /// "branch X will be deleted when `jkb task reap` archives the checkout" about a checkout
    /// that had already been archived. Nothing would ever delete it, and the operator was told
    /// not to.
    ///
    /// Reachable exactly where the three-valued answer is: `cmd_task_abandon` deletes the branch
    /// itself when it is proven present, so the only way to reach the fate undeleted with no
    /// record is git failing to answer — an unreadable `packed-refs`, the state the round-8 test
    /// builds.
    #[test]
    fn a_branch_is_owed_to_the_reaper_only_when_a_record_will_reach_it() {
        use super::{branch_fate, BranchFate};
        use jkb_fsm::Fact;

        for present in [Fact::Yes, Fact::Unknown] {
            assert!(
                branch_fate(true, present, true) == BranchFate::OwedToTheReaper,
                "a deferred tree still holds the branch, and its record carries the plan"
            );
            assert!(
                branch_fate(true, present, false) == BranchFate::Kept,
                "with no record nothing will ever apply the plan, so promising the reaper will \
                 is telling the operator not to do the one thing left to do"
            );
            assert!(
                branch_fate(false, present, true) == BranchFate::Kept,
                "a deletion nobody asked for is not owed to anyone"
            );
        }
        // A proven absence is the only deletion, whatever else is true.
        for deferred in [true, false] {
            assert!(branch_fate(true, Fact::No, deferred) == BranchFate::Deleted);
            assert!(branch_fate(false, Fact::No, deferred) == BranchFate::Absent);
        }
    }

    /// Both `--status` enumerations name every status the CLI accepts.
    ///
    /// Neither agent-facing surface may advertise a verb the binary does not have.
    ///
    /// `jkb guide` is compiled into the binary and `AGENTS.md` sits at the repo root, so a verb
    /// deleted from `clap` leaves both untouched — and an agent following either runs a command
    /// that does not exist. `jkb task base` was deleted with the cut point and stayed in `GUIDE`
    /// for a whole branch; the class is what is closed here, not the instance.
    ///
    /// Checked by asking `clap` itself for the subcommand names, so this cannot drift.
    #[test]
    fn neither_agent_surface_advertises_a_verb_that_was_deleted() {
        use clap::CommandFactory;
        let task = super::Cli::command();
        let task = task
            .get_subcommands()
            .find(|c| c.get_name() == "task")
            .expect("`jkb task` exists");
        let verbs: Vec<&str> = task
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert!(verbs.contains(&"why") && verbs.contains(&"pr"), "{verbs:?}");

        let agents = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../AGENTS.md"),
        )
        .expect("AGENTS.md sits at the repo root");
        // Matched **anywhere on the line**, not at its start. Anchoring it to the start read
        // every one of `GUIDE`'s bare command lines and none of AGENTS.md's, where the same
        // commands are markdown bullets (`- ` + a backtick) — so half this guard was inert, and
        // inert in exactly the way that looks like a clean result. One surface reporting nothing
        // is indistinguishable from one with nothing to report.
        let mut checked = 0_usize;
        for (surface, text) in [("jkb guide", GUIDE), ("AGENTS.md", agents.as_str())] {
            let mut seen_here = 0_usize;
            for line in text.lines() {
                for (i, _) in line.match_indices("jkb task ") {
                    let rest = &line[i + "jkb task ".len()..];
                    let verb: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                        .collect();
                    // Placeholders and prose (`jkb task <uid>`, `jkb task ...`) carry no verb.
                    // A verb has to be the whole token, so `jkb task work-tree` is not read as
                    // `work`; anything else is skipped rather than guessed at.
                    if verb.is_empty()
                        || !rest[verb.len()..]
                            .chars()
                            .next()
                            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
                    {
                        continue;
                    }
                    seen_here += 1;
                    assert!(
                        verbs.contains(&verb.as_str()),
                        "{surface} advertises `jkb task {verb}`, which the binary does not have"
                    );
                }
            }
            // A surface this found no verb in is a surface this did not check, which is the
            // failure the anchoring bug produced silently for a whole branch.
            assert!(
                seen_here > 0,
                "{surface} yielded no `jkb task <verb>` to check"
            );
            checked += seen_here;
        }
        assert!(checked > 20, "only {checked} verb mentions were checked");
    }

    /// `GUIDE` declares `AGENTS.md` its mirror and nothing held the two in step: both omitted
    /// `cancelled`, while AGENTS.md's own landing section tells you to set exactly that to clear
    /// a must-fix and land. The set comes from `TaskStatus::ALL`, generated with the enum, so
    /// adding a status fails here rather than shipping a verb an agent never learns about.
    #[test]
    fn both_status_enumerations_name_every_status_the_cli_accepts() {
        let agents = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../AGENTS.md"),
        )
        .expect("AGENTS.md sits at the repo root");
        for (surface, text) in [("jkb guide", GUIDE), ("AGENTS.md", agents.as_str())] {
            // Anchored on the verb, not on `--status`: `jkb find --status S` is a different line
            // in both files and matching it first would make this test pass against nothing.
            let line = text
                .lines()
                .find(|l| l.contains("task set <uid> --status"))
                .unwrap_or_else(|| panic!("{surface} has no `task set <uid> --status` line"));
            for status in TaskStatus::ALL {
                assert!(
                    line.contains(status.as_str()),
                    "{surface} omits `{}` from `{line}`, so an agent reading it never learns \
                     that the status exists",
                    status.as_str()
                );
            }
        }
    }
}
