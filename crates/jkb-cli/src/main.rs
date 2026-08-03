//! `jkb` — command-line interface for the jkb knowledge base (Section 12).
//!
//! Thin edge over the library crates: `clap` parses subcommands, each wires to
//! `jkb-core`/`-ingest`/`-search`/`-sync`, and results print as human lines or
//! `--json`. Errors collapse into `anyhow` here (libraries use `thiserror`).
//! Read/task/query commands default their namespace scope to the mount covering the
//! current directory (design D19), overridable with `--global`.

mod commands;
mod output;
mod owner;
mod service;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use jkb_core::query::{Query, Scope};
use jkb_core::{
    binding, blob, claim, edge, investigation, item, mount, ns, nstype, placement, tag, task, undo,
    view, Db,
};
use jkb_embed::{OllamaConfig, OllamaEmbedder};
use jkb_ingest::Pipeline;
use jkb_search::{Route, Searcher};
use jkb_types::{ConflictPolicy, EdgeType, Embedder, ItemId, PlacementRole, Resolution, SyncMode};

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
        /// Which route to use.
        #[arg(long, value_enum, default_value_t = RouteArg::Hybrid)]
        route: RouteArg,
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
    Index,
    /// Health checks, integrity, and backup.
    Doctor {
        /// Also write a checkpointed backup to this path.
        #[arg(long)]
        backup: Option<PathBuf>,
        /// Apply repairs: clear claims whose owner process no longer exists.
        #[arg(long)]
        fix: bool,
    },
    /// Run the MCP server (Section 13, not yet available).
    Mcp,
    /// List the direct children of a namespace (sub-namespaces + items homed there) —
    /// the lazy tree-expansion primitive for the UI. Omit `path` for top-level namespaces.
    Ls {
        /// The namespace whose children to list (default: top-level namespaces).
        path: Option<String>,
        /// Show hidden entries: terminal (`done`/`cancelled`) tasks and `chunk` items,
        /// which are derived index units rather than content.
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
        /// Show hidden entries: terminal (`done`/`cancelled`) items and `chunk` items,
        /// in both the listing and the counts.
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
        /// Sync direction.
        #[arg(long, value_enum, default_value_t = ModeArg::Bidirectional)]
        mode: ModeArg,
        /// File-format serializer.
        #[arg(long, default_value = "document")]
        serializer: String,
        /// Include glob (e.g. `**/*.md`).
        #[arg(long)]
        include: Option<String>,
        /// Exclude glob.
        #[arg(long)]
        exclude: Option<String>,
        /// Conflict policy.
        #[arg(long, value_enum, default_value_t = PolicyArg::Manual)]
        policy: PolicyArg,
    },
    /// List all mounts (namespace → serializer → backing directory).
    Ls,
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
    /// Release a task's claim held by an owner (defaults to this process).
    Release {
        /// The task uid.
        uid: String,
        /// The owner id whose claim to release (default: this process's `host:pid`).
        #[arg(long)]
        owner: Option<String>,
    },
    /// Reclaim claims whose owner process is gone (the deterministic crash-recovery
    /// scan). Keeps claims whose pid is alive plus any `--keep` owners — so a live
    /// coordinator passes its own owner to never reclaim its own in-flight work.
    Reclaim {
        /// Owner id(s) to always preserve (repeatable), e.g. this run's own owner.
        #[arg(long)]
        keep: Vec<String>,
    },
    /// Ensure every task homed outside `tasks/` has a `tasks/…` mirror (symbolic link),
    /// so `tasks/**` is the complete task index. Idempotent; sync does this automatically.
    Mirror,
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

impl From<RouteArg> for Route {
    fn from(r: RouteArg) -> Self {
        match r {
            RouteArg::Vector => Route::Vector,
            RouteArg::Fts => Route::Fts,
            RouteArg::Hybrid => Route::Hybrid,
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
    // Keep the bundled Claude Code commands/workflows fresh in the user's config dir
    // (best-effort, silent). Skipped for explicit `jkb commands …` so it never fights the
    // user's own install/uninstall.
    if !matches!(cli.command, Command::Commands { .. }) {
        commands::ensure_installed();
    }

    let db_path = cli.db.clone().unwrap_or_else(default_db_path);
    let db = open_db(&db_path)?;
    let json = cli.json;
    let global = cli.global;

    match cli.command {
        Command::Ingest { path, ns } => cmd_ingest(&db, &path, ns.as_deref(), global, json),
        Command::Query {
            terms,
            limit,
            count,
        } => cmd_query(&db, &terms.join(" "), limit, count, global, json),
        Command::Search {
            terms,
            route,
            limit,
            context,
        } => cmd_search(
            &db,
            &terms.join(" "),
            route.into(),
            limit,
            context,
            global,
            json,
        ),
        Command::Ns { cmd } => cmd_ns(&db, cmd, json),
        Command::Tag { cmd } => cmd_tag(&db, cmd, json),
        Command::Mount { cmd } => cmd_mount(&db, cmd, json),
        Command::Sync { ns, watch } => cmd_sync(&db, ns.as_deref(), watch),
        Command::Service { cmd } => match cmd {
            ServiceCmd::Print => service::print(&db_path),
            ServiceCmd::Install => service::install(&db_path),
            ServiceCmd::Uninstall => service::uninstall(&db_path),
        },
        Command::Commands { cmd } => match cmd {
            CommandsCmd::Install => commands::install(),
            CommandsCmd::Uninstall => commands::uninstall(),
            CommandsCmd::List => commands::list(),
        },
        Command::Task { cmd } => cmd_task(&db, cmd, global, json),
        Command::View { cmd } => cmd_view(&db, cmd, json),
        Command::Undo { txn } => cmd_undo(&db, txn),
        Command::Index => cmd_index(&db),
        Command::Doctor { backup, fix } => cmd_doctor(&db, &db_path, backup.as_deref(), fix),
        Command::Mcp => jkb_mcp::run_stdio(db, embedder()?),
        Command::Ls {
            path,
            all,
            long,
            recursive,
            time,
        } => cmd_ls(
            &db,
            path.as_deref(),
            LsOpts {
                all,
                long,
                recursive,
                time,
            },
            json,
        ),
        Command::Grep {
            pattern,
            path,
            ignore_case,
            names_only,
            count,
        } => cmd_grep(
            &db,
            &pattern,
            path.as_deref(),
            GrepOpts {
                ignore_case,
                names_only,
                count,
            },
            global,
            json,
        ),
        Command::Cat { uid } => cmd_cat(&db, &uid),
        Command::Tree { path, all, depth } => cmd_tree(&db, path.as_deref(), all, depth, json),
        Command::Find {
            path,
            kind,
            tags,
            status,
            limit,
        } => cmd_find(
            &db,
            path.as_deref(),
            kind.as_deref(),
            &tags,
            status.as_deref(),
            limit,
            global,
            json,
        ),
        Command::Recent { path, limit } => cmd_recent(&db, path.as_deref(), limit, global, json),
        Command::Stat { uid } => cmd_stat(&db, &uid, json),
        Command::Guide => {
            cmd_guide();
            Ok(())
        }
        Command::Item { cmd } => match cmd {
            ItemCmd::Show { uid, preview } => cmd_item_show(&db, &uid, preview, json),
            ItemCmd::Rm { uid, force } => cmd_item_rm(&db, &uid, force, json),
            ItemCmd::Edit {
                uid,
                text,
                stdin,
                append,
            } => cmd_item_edit(&db, &uid, &text, stdin, append, json),
        },
        Command::Related {
            uid,
            edges,
            depth,
            direction,
        } => cmd_related(&db, &uid, &edges, depth, direction.into(), json),
        Command::Inv { cmd } => cmd_inv(&db, cmd, global, json),
        Command::Blob { cmd } => match cmd {
            BlobCmd::Ls { contains, limit } => cmd_blob_ls(&db, contains.as_deref(), limit, json),
            BlobCmd::Cat { hash } => cmd_blob_cat(&db, &hash),
        },
        Command::History { path } => cmd_history(&db, &path, json),
    }
}

/// `jkb blob ls` — list the archive, optionally searching blob bytes.
fn cmd_blob_ls(db: &Db, contains: Option<&str>, limit: usize, json: bool) -> Result<()> {
    let needle = contains.map(|s| s.as_bytes().to_vec());
    let blobs = db.read(move |conn| blob::list(conn, needle.as_deref(), limit))?;
    if json {
        let arr: Vec<serde_json::Value> = blobs
            .iter()
            .map(|b| {
                serde_json::json!({
                    "hash": b.hash, "size": b.size, "mime": b.mime, "created_at": b.created_at,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if blobs.is_empty() {
        println!("(no matching blobs)");
    } else {
        for b in &blobs {
            println!(
                "{}  {:>9}  {}",
                &b.hash[..16.min(b.hash.len())],
                b.size,
                b.created_at
            );
        }
    }
    Ok(())
}

/// `jkb blob cat <hash>` — raw bytes to stdout, accepting a unique hash prefix.
fn cmd_blob_cat(db: &Db, hash: &str) -> Result<()> {
    use std::io::Write as _;
    let prefix = hash.to_owned();
    // A full hash is 64 hex chars; anything shorter is treated as a prefix and must be
    // unambiguous, so `cat` can never print the wrong version.
    let matches = db.read(move |conn| {
        let all = blob::list(conn, None, usize::MAX)?;
        Ok(all
            .into_iter()
            .filter(|b| b.hash.starts_with(&prefix))
            .collect::<Vec<_>>())
    })?;
    let found = match matches.as_slice() {
        [one] => one.hash.clone(),
        [] => anyhow::bail!("no blob with hash prefix `{hash}`"),
        many => anyhow::bail!("`{hash}` matches {} blobs; use a longer prefix", many.len()),
    };
    let bytes = db
        .read(move |conn| blob::load(conn, &found))?
        .with_context(|| format!("blob `{hash}` vanished between listing and reading"))?;
    std::io::stdout().write_all(&bytes)?;
    Ok(())
}

/// `jkb history <path>` — every synced version of a file, newest first.
fn cmd_history(db: &Db, path: &str, json: bool) -> Result<()> {
    // Accept a bare path or a `file://` uri, and canonicalize so a relative path matches the
    // absolute uri the journal stores.
    let uri = if path.starts_with("file://") {
        path.to_owned()
    } else {
        let abs = std::fs::canonicalize(path)
            .unwrap_or_else(|_| std::path::PathBuf::from(path))
            .to_string_lossy()
            .into_owned();
        format!("file://{abs}")
    };

    let versions = db.read({
        let uri = uri.clone();
        move |conn| {
            // The journal's changelog carries one entry per settle, each naming the blob
            // holding that version's bytes.
            let mut stmt = conn.prepare(
                "SELECT ts, after FROM changelog
                 WHERE entity_type = 'sync_state' AND entity_id = ?1 AND after IS NOT NULL
                 ORDER BY id DESC",
            )?;
            let rows = stmt.query_map([&uri], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut out: Vec<(String, String, String)> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for row in rows {
                let (ts, after) = row?;
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&after) else {
                    continue;
                };
                let Some(hash) = v.get("base_blob_hash").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let status = v
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("ok")
                    .to_owned();
                if seen.insert(hash.to_owned()) {
                    out.push((ts, hash.to_owned(), status));
                }
            }
            Ok(out)
        }
    })?;

    if json {
        let arr: Vec<serde_json::Value> = versions
            .iter()
            .map(|(ts, hash, status)| {
                serde_json::json!({ "ts": ts, "blob": hash, "status": status })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if versions.is_empty() {
        println!(
            "(no recorded history for {uri})\n\
             Versions synced before this build did not journal their blob hash — search the \
             archive instead: jkb blob ls --contains \"<a line you remember>\""
        );
    } else {
        for (ts, hash, status) in &versions {
            println!("{ts}  {}  [{status}]", &hash[..16.min(hash.len())]);
        }
        println!("\nRead one with: jkb blob cat <hash>");
    }
    Ok(())
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

/// The ambient namespace for the current directory, unless `--global`.
fn ambient(db: &Db, global: bool) -> Result<Option<String>> {
    if global {
        return Ok(None);
    }
    let cwd = std::env::current_dir()?;
    Ok(db.read(move |conn| mount::ambient_namespace(conn, &cwd))?)
}

/// If a query has no explicit scope, default it to the ambient namespace subtree.
fn apply_ambient(query: &mut Query, db: &Db, global: bool) -> Result<()> {
    if query.scope == Scope::All {
        if let Some(path) = ambient(db, global)? {
            query.scope = Scope::Subtree(path);
        }
    }
    Ok(())
}

/// The ambient repo key: the full namespace path of the `file://` mount covering the
/// current directory (design D26.2), or `None` outside any mount. Tasks home under
/// `tasks/<repo>/…` using this key. Unlike [`ambient`], `--global` does not apply — homing
/// always reflects where the task was captured.
fn ambient_repo(db: &Db) -> Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    Ok(db.read(move |conn| mount::ambient_namespace(conn, &cwd))?)
}

/// Default an unscoped task query to the ambient repo's task tree (`tasks/<repo>/**`) when
/// inside a repo, else the global `tasks/**` tree (design D26, open-question 4). `--global`
/// forces the global tree.
fn apply_ambient_tasks(query: &mut Query, db: &Db, global: bool) -> Result<()> {
    if query.scope == Scope::All {
        let root = task::DEFAULT_ROOT;
        let base = if global {
            root.to_owned()
        } else {
            match ambient_repo(db)? {
                Some(repo) => format!("{root}/{repo}"),
                None => root.to_owned(),
            }
        };
        query.scope = Scope::Subtree(base);
    }
    Ok(())
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

fn cmd_ingest(db: &Db, path: &str, ns: Option<&str>, global: bool, json: bool) -> Result<()> {
    let namespace = match ns {
        Some(n) => n.to_owned(),
        None => ambient(db, global)?.unwrap_or_else(|| "inbox".to_owned()),
    };
    let pipeline = Pipeline::new(embedder()?);
    let is_url = path.starts_with("http://") || path.starts_with("https://");
    let outcome = if is_url {
        pipeline.ingest_url(db, path, &namespace)?
    } else {
        pipeline.ingest_path(db, Path::new(path), &namespace)?
    };

    if json {
        let v = serde_json::json!({
            "document": outcome.document.get(),
            "chunk_count": outcome.chunk_count,
            "embedded": outcome.embedded,
            "already_ingested": outcome.already_ingested,
            "warnings": outcome.warnings,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        let state = if outcome.already_ingested {
            "already ingested"
        } else if outcome.embedded {
            "ingested + embedded"
        } else {
            "captured (not embedded)"
        };
        println!(
            "{state}: document {} under {namespace} ({} chunks)",
            outcome.document, outcome.chunk_count
        );
        for w in &outcome.warnings {
            println!("  warning: {w}");
        }
    }
    Ok(())
}

fn cmd_query(
    db: &Db,
    dsl: &str,
    limit: Option<usize>,
    count: bool,
    global: bool,
    json: bool,
) -> Result<()> {
    let mut query = jkb_core::query::parse(dsl)?;
    apply_ambient(&mut query, db, global)?;
    // `--count` reports the total; `--limit` only caps a listing.
    if !count {
        if let Some(limit) = limit {
            query.limit = Some(limit);
        }
    }
    let ids = db.read(move |conn| query.evaluate(conn))?;
    if count {
        if json {
            println!("{}", serde_json::json!({ "count": ids.len() }));
        } else {
            println!("{}", ids.len());
        }
        return Ok(());
    }
    let items = output::fetch_items(db, &ids)?;
    output::print_items(&items, json);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_search(
    db: &Db,
    dsl: &str,
    route: Route,
    limit: usize,
    context: Option<usize>,
    global: bool,
    json: bool,
) -> Result<()> {
    let mut query = jkb_core::query::parse(dsl)?;
    apply_ambient(&mut query, db, global)?;
    let searcher = Searcher::new(embedder()?);
    let hits = searcher.search(db, &query, route, limit)?;

    if json {
        // Resolve every hit (and every `source_document`) to a real item before emitting.
        // A search result identified only by a row id is not interpretable by the agent that
        // asked for it: `jkb query --json` returns uid/kind/snippet, and search — the
        // flagship read — must not be the one surface that answers in opaque integers.
        let mut ids: Vec<ItemId> = hits.iter().map(|h| h.item).collect();
        ids.extend(hits.iter().filter_map(|h| h.source_document));
        ids.sort_unstable_by_key(|i| i.get());
        ids.dedup_by_key(|i| i.get());
        let resolved: std::collections::HashMap<i64, output::DisplayItem> =
            output::fetch_items(db, &ids)?
                .into_iter()
                .map(|i| (i.id, i))
                .collect();

        let mut arr = Vec::new();
        for hit in &hits {
            let ctx: Vec<serde_json::Value> = match context {
                Some(n) => searcher
                    .get_context(db, hit.item, n)?
                    .into_iter()
                    .map(|c| {
                        serde_json::json!({
                            "item": c.item.get(),
                            "position": c.position,
                            "is_hit": c.is_hit,
                            "content": c.content,
                        })
                    })
                    .collect(),
                None => Vec::new(),
            };
            let item = resolved.get(&hit.item.get());
            let source = hit
                .source_document
                .and_then(|d| resolved.get(&d.get()))
                .map(|d| serde_json::json!({ "id": d.id, "uid": d.uid, "kind": d.kind }));
            arr.push(serde_json::json!({
                "item": hit.item.get(),
                "uid": item.map(|i| i.uid.clone()),
                "kind": item.map(|i| i.kind.clone()),
                "status": item.and_then(|i| i.status.clone()),
                "snippet": item.and_then(|i| i.snippet.clone()),
                "route": hit.route.as_str(),
                "score": hit.score,
                "distance": hit.distance,
                "namespace": hit.namespace_path,
                "source_document": source,
                "context": ctx,
            }));
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(arr))?
        );
        return Ok(());
    }

    if hits.is_empty() {
        println!("(no results)");
        return Ok(());
    }
    let ids: Vec<ItemId> = hits.iter().map(|h| h.item).collect();
    let items = output::fetch_items(db, &ids)?;
    for (hit, item) in hits.iter().zip(items.iter()) {
        println!(
            "[{} {:.3}] {}",
            hit.route.as_str(),
            hit.score,
            output_line(item)
        );
        if let Some(n) = context {
            for c in searcher.get_context(db, hit.item, n)? {
                let marker = if c.is_hit { "»" } else { " " };
                println!("    {marker} {}: {}", c.position, first_line(&c.content));
            }
        }
    }
    Ok(())
}

/// A direct child of a namespace in the tree: a sub-namespace, or an item homed there.
struct Child {
    kind: String,
    reference: String,
    label: String,
    has_children: bool,
    status: Option<String>,
    priority: Option<i64>,
    /// For namespaces: count of visible item leaves anywhere in the subtree (respecting the
    /// terminal-status toggle). `None` for item children. Lets the pane flag which folders
    /// lead to real content. This is the sum of [`Child::leaf_kinds`].
    leaf_count: Option<i64>,
    /// For namespaces: the same leaves broken down by item `kind`, ordered by kind name.
    /// A folder holding 8 tasks and 4 documents is not described by "12", and calling that
    /// 12 tasks is simply wrong — so the breakdown, not the total, is what a tree renders.
    leaf_kinds: Option<BTreeMap<String, i64>>,
    /// For namespaces: the type recorded on **this** namespace, if any. Deliberately its
    /// *own* type rather than the inherited one — a label on every namespace under a typed
    /// root would be noise, and the interesting fact is where the type was applied.
    ns_type: Option<String>,
    /// The one-line description of [`Child::ns_type`], for a tooltip.
    ns_type_about: Option<String>,
    /// For an item that others were derived from: how many `chunk` items came out of it.
    /// Chunks are index units, not content — the tree hides them and shows their count here,
    /// against the document they belong to. `None` when there are none.
    chunk_count: Option<i64>,
    /// The item's `updated_at` (for `ls -t`); `None` for namespaces.
    updated: Option<String>,
}

/// The item kind ingest produces per document fragment. Chunks are derived index units:
/// they are rebuildable from the VFS, nothing links *to* them, and listing them buries each
/// ingested document under its own pieces. The tree hides them unless `--all` and surfaces
/// their count against the document they came from.
const KIND_CHUNK: &str = "chunk";

/// Render a per-kind leaf breakdown as `8 task · 4 document`, ordered by kind name.
///
/// Kinds are **not** pluralized: they are `items.kind` values verbatim, and English
/// pluralization of an open vocabulary goes wrong fast (`hypothesis` → `hypothesiss`). The
/// count in front makes the reading unambiguous without it.
fn format_leaf_kinds(kinds: &BTreeMap<String, i64>) -> String {
    kinds
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(kind, n)| format!("{n} {kind}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

impl Child {
    /// The item's hidden chunk count as a suffix, e.g. ` (3 chunks)`, or empty. Shows where
    /// the fragments went for a document the tree no longer expands into.
    fn chunk_label(&self) -> String {
        self.chunk_count
            .filter(|n| *n > 0)
            .map(|n| format!(" ({n} chunk{})", if n == 1 { "" } else { "s" }))
            .unwrap_or_default()
    }

    /// The namespace's own type as a bracketed label, e.g. ` [tasks]`, or empty.
    fn type_label(&self) -> String {
        self.ns_type
            .as_deref()
            .map(|t| format!(" [{t}]"))
            .unwrap_or_default()
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind,
            "ref": self.reference,
            "label": self.label,
            "has_children": self.has_children,
            "status": self.status,
            "priority": self.priority,
            "leaf_count": self.leaf_count,
            "leaf_kinds": self.leaf_kinds,
            "type": self.ns_type,
            "type_about": self.ns_type_about,
            "chunk_count": self.chunk_count,
            "updated": self.updated,
        })
    }

    /// Ordering key: namespaces first, then tasks (most important — lowest priority
    /// number — first), then other items; ties broken by label. Nulls sort last.
    fn sort_key(&self) -> (u8, i64, String) {
        let group = match self.kind.as_str() {
            "namespace" => 0,
            "task" => 1,
            _ => 2,
        };
        (
            group,
            self.priority.unwrap_or(i64::MAX),
            self.label.to_lowercase(),
        )
    }
}

/// A short label for an item: its first non-empty content line (≤80 chars), else its uid.
fn item_label(meta: &item::ItemMeta) -> String {
    let line = meta
        .content
        .as_deref()
        .unwrap_or("")
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.is_empty() {
        return meta.uid.clone();
    }
    let mut s: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        s.push('…');
    }
    s
}

/// The direct children of `path` (or top-level namespaces when `None`): sub-namespaces
/// followed by items whose **primary** placement is `path`. Terminal (`done`/`cancelled`)
/// tasks are hidden unless `all`.
fn list_children(
    conn: &rusqlite::Connection,
    path: Option<&str>,
    all: bool,
) -> jkb_core::Result<Vec<Child>> {
    let mut out = Vec::new();

    let ns_children = match path {
        None => ns::roots(conn)?,
        Some(p) => ns::children(conn, p)?,
    };
    // All children's subtree leaf counts in one grouped recursive query, rather than a
    // separate descendant walk per child (an N+1 the tree hit on every expand).
    let leaf_counts = ns::subtree_leaf_counts(conn, path, all)?;
    for (ns_id, ns_path) in ns_children {
        let label = ns_path.rsplit('/').next().unwrap_or(&ns_path).to_owned();
        let has_sub = !ns::children(conn, &ns_path)?.is_empty();
        let mut leaf_kinds = leaf_counts.get(&ns_id).cloned().unwrap_or_default();
        if !all {
            // Chunks are hidden below, so they must not be counted here either — a folder
            // reporting "1 chunk" that shows nothing when opened is worse than no count.
            leaf_kinds.remove(KIND_CHUNK);
        }
        let leaf_count: i64 = leaf_kinds.values().sum();
        // The namespace's OWN type, not `effective_type`: labelling every namespace under a
        // typed root would be noise, and where the type was *applied* is the useful fact.
        let ns_type = ns::get_type_by_id(conn, ns_id)?;
        let ns_type_about = ns_type
            .as_deref()
            .and_then(|name| nstype::resolve(name).ok())
            .map(|t| t.about().to_owned());
        out.push(Child {
            kind: "namespace".to_owned(),
            reference: ns_path,
            label,
            has_children: has_sub || leaf_count > 0,
            status: None,
            priority: None,
            leaf_count: Some(leaf_count),
            leaf_kinds: Some(leaf_kinds),
            ns_type,
            ns_type_about,
            chunk_count: None,
            updated: None,
        });
    }

    if let Some(p) = path {
        if let Some(ns_id) = ns::get(conn, p)? {
            // Any placement role: a `tasks/…` mirror surfaces the task even though its
            // primary home is elsewhere (the symbolic-link view).
            let placed = placement::items_in(conn, ns_id, None)?;
            // One grouped query for every document's chunk count, not one per document.
            let chunk_counts = item::derived_kind_counts(conn, &placed, KIND_CHUNK)?;
            for item_id in placed {
                let Some(meta) = item::get(conn, item_id)? else {
                    continue;
                };
                // Hide any terminal-status item (done/cancelled) unless `all` — like
                // ignored files, revealed only on explicit toggle.
                let terminal = matches!(meta.status.as_deref(), Some("done" | "cancelled"));
                if !all && terminal {
                    continue;
                }
                // Same treatment for chunks, and for the same reason: they are derived index
                // units, not content. Listing them doubles every ingested document under its
                // own fragments. Their count rides on the document instead (below).
                if !all && meta.kind == KIND_CHUNK {
                    continue;
                }
                out.push(Child {
                    label: item_label(&meta),
                    kind: meta.kind,
                    reference: meta.uid,
                    has_children: false,
                    status: meta.status,
                    priority: meta.priority,
                    leaf_count: None,
                    leaf_kinds: None,
                    ns_type: None,
                    ns_type_about: None,
                    chunk_count: chunk_counts.get(&item_id).copied(),
                    updated: Some(meta.updated_at.clone()),
                });
            }
        }
    }
    out.sort_by_key(Child::sort_key);
    Ok(out)
}

/// Flags for `jkb ls` (the ergonomic listing verb).
#[derive(Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // a CLI flags bag, not state
struct LsOpts {
    all: bool,
    long: bool,
    recursive: bool,
    time: bool,
}

/// `jkb ls [path]` — namespaces + items under a namespace (the lazy tree primitive plus
/// familiar `-l`/`-R`/`-t` ergonomics). `-R` walks the subtree depth-first; without it,
/// just the direct children.
fn cmd_ls(db: &Db, path: Option<&str>, opts: LsOpts, json: bool) -> Result<()> {
    let owned = path.map(str::to_owned);
    let all = opts.all;
    let recursive = opts.recursive;
    // (namespace shown as the row's "parent", child) pairs — the parent gives `-l`/`-R`
    // rows a stable location column even when descending.
    let rows: Vec<(Option<String>, Child)> = db.read(move |conn| {
        let mut acc = Vec::new();
        collect_ls(conn, owned.as_deref(), all, recursive, &mut acc)?;
        Ok(acc)
    })?;

    let mut rows = rows;
    if opts.time {
        // Most-recently-updated first; rows without an `updated` (namespaces) sort last.
        rows.sort_by(|a, b| b.1.updated.cmp(&a.1.updated));
    }

    if json {
        let children: Vec<_> = rows.iter().map(|(_, c)| c.to_json()).collect();
        let v = serde_json::json!({ "path": path, "children": children });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if rows.is_empty() {
        println!("(empty)");
    } else {
        for (parent, c) in &rows {
            print_ls_row(parent.as_deref(), c, opts);
        }
    }
    Ok(())
}

/// Accumulate `ls` rows, optionally recursing into sub-namespaces depth-first. Each row is
/// `(parent namespace path, child)`.
fn collect_ls(
    conn: &rusqlite::Connection,
    path: Option<&str>,
    all: bool,
    recursive: bool,
    acc: &mut Vec<(Option<String>, Child)>,
) -> jkb_core::Result<()> {
    let children = list_children(conn, path, all)?;
    for c in children {
        let is_ns = c.kind == "namespace";
        let ns_path = c.reference.clone();
        acc.push((path.map(str::to_owned), c));
        if recursive && is_ns {
            collect_ls(conn, Some(&ns_path), all, recursive, acc)?;
        }
    }
    Ok(())
}

/// One human-readable `ls` row. `-l` adds kind/status and the location (namespace path for a
/// sub-namespace, or `parent → uid` for an item); the default is the compact tree row.
fn print_ls_row(parent: Option<&str>, c: &Child, opts: LsOpts) {
    let status = c
        .status
        .as_deref()
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    if opts.long {
        let loc = if c.kind == "namespace" {
            c.reference.clone()
        } else {
            match parent {
                Some(p) => format!("{p} → {}", c.reference),
                None => c.reference.clone(),
            }
        };
        let updated = c.updated.as_deref().unwrap_or("");
        println!(
            "{:<10} {:<12} {:<24} {}{}{status}",
            c.kind,
            updated,
            loc,
            c.label,
            c.type_label()
        );
    } else {
        let arrow = if c.has_children { "▸" } else { " " };
        // When recursing, prefix items with their namespace so the flattened list stays legible.
        let loc = match (opts.recursive, parent, c.kind.as_str()) {
            (true, Some(p), k) if k != "namespace" => format!("{p}/"),
            _ => String::new(),
        };
        println!(
            "{arrow} {:<10} {loc}{}{}{}{status}",
            c.kind,
            c.label,
            c.type_label(),
            c.chunk_label()
        );
    }
}

/// Flags for `jkb grep`.
#[derive(Clone, Copy)]
struct GrepOpts {
    ignore_case: bool,
    names_only: bool,
    count: bool,
}

/// `jkb grep <pattern> [path]` — literal-substring content search over a namespace subtree.
/// Prints `uid:line` per matching line (or just uids with `-l`, or a count with `-c`), and
/// **exits 1 when nothing matched** so it composes in scripts like real grep.
fn cmd_grep(
    db: &Db,
    pattern: &str,
    path: Option<&str>,
    opts: GrepOpts,
    global: bool,
    json: bool,
) -> Result<()> {
    // Explicit path wins; otherwise scope to the ambient namespace (nothing = search all).
    let scope = match path {
        Some(p) => Some(p.to_owned()),
        None => ambient(db, global)?,
    };
    let (pat, scope2) = (pattern.to_owned(), scope.clone());
    let hits = db.read(move |conn| item::grep(conn, &pat, scope2.as_deref(), opts.ignore_case))?;

    // Extract the matching lines per item (the SQL already confirmed a match exists).
    let needle = if opts.ignore_case {
        pattern.to_lowercase()
    } else {
        pattern.to_owned()
    };
    let matches_line = |line: &str| {
        if opts.ignore_case {
            line.to_lowercase().contains(&needle)
        } else {
            line.contains(&needle)
        }
    };

    if opts.count {
        let n = hits.len();
        if json {
            println!("{}", serde_json::json!({ "count": n }));
        } else {
            println!("{n}");
        }
        if n == 0 {
            std::process::exit(1);
        }
        return Ok(());
    }

    if json {
        let arr: Vec<_> = hits
            .iter()
            .map(|h| {
                let lines: Vec<_> = h
                    .content
                    .lines()
                    .enumerate()
                    .filter(|(_, l)| matches_line(l))
                    .map(|(i, l)| serde_json::json!({ "line": i + 1, "text": l }))
                    .collect();
                serde_json::json!({ "uid": h.uid, "kind": h.kind, "matches": lines })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if opts.names_only {
        for h in &hits {
            println!("{}", h.uid);
        }
    } else {
        for h in &hits {
            for (i, line) in h.content.lines().enumerate() {
                if matches_line(line) {
                    println!("{}:{}:{}", h.uid, i + 1, line.trim_end());
                }
            }
        }
    }

    if hits.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// `jkb cat <uid>` — print an item's full content to stdout (no metadata, no truncation),
/// so a task/note/document body pipes cleanly into another tool or an agent's context.
fn cmd_cat(db: &Db, uid: &str) -> Result<()> {
    let u = uid.to_owned();
    let content = db.read(move |conn| {
        let Some(id) = item::id_for_uid(conn, &u)? else {
            return Ok(None);
        };
        item::get_content(conn, id).map(Some)
    })?;
    match content {
        None => anyhow::bail!("no item with uid `{uid}`"),
        Some(c) => print!("{}", c.unwrap_or_default()),
    }
    Ok(())
}

/// `jkb find [path] --kind --tag --status` — structured item search: familiar flags that
/// compile to the query DSL (the typed complement to `grep`). Empty filters = list the
/// scope (like `find .`); scope defaults to the ambient namespace.
#[allow(clippy::too_many_arguments)]
fn cmd_find(
    db: &Db,
    path: Option<&str>,
    kind: Option<&str>,
    tags: &[String],
    status: Option<&str>,
    limit: Option<usize>,
    global: bool,
    json: bool,
) -> Result<()> {
    // Refuse the one footgun: no filter, no path, no ambient scope, no limit would list the
    // entire KB. Any filter / path / --limit (or being inside a mounted repo) makes it fine.
    let unfiltered = kind.is_none() && tags.is_empty() && status.is_none() && path.is_none();
    if unfiltered && limit.is_none() && ambient(db, global)?.is_none() {
        anyhow::bail!(
            "`find` with no filters would list the entire KB — add --kind/--tag/--status, a path, or --limit"
        );
    }

    let mut terms: Vec<String> = Vec::new();
    if let Some(k) = kind {
        terms.push(format!("kind:{k}"));
    }
    for t in tags {
        terms.push(format!("tag:{t}"));
    }
    if let Some(s) = status {
        terms.push(format!("status:{s}"));
    }
    if let Some(p) = path {
        terms.push(format!("ns:{p}/**"));
    }
    cmd_query(db, &terms.join(" "), limit, false, global, json)
}

/// `jkb recent [path]` — the most-recently-updated items in a subtree, newest first.
fn cmd_recent(db: &Db, path: Option<&str>, limit: usize, global: bool, json: bool) -> Result<()> {
    let dsl = path.map(|p| format!("ns:{p}/**")).unwrap_or_default();
    let mut query = jkb_core::query::parse(&dsl)?;
    apply_ambient(&mut query, db, global)?;
    let ids = db.read(move |conn| query.evaluate(conn))?;
    let mut items = output::fetch_items(db, &ids)?;
    // Newest first; missing timestamps (shouldn't happen) sort last.
    items.sort_by(|a, b| b.updated.cmp(&a.updated));
    items.truncate(limit);
    output::print_items(&items, json);
    Ok(())
}

/// `jkb stat <uid>` — compact metadata for one item (no body).
fn cmd_stat(db: &Db, uid: &str, json: bool) -> Result<()> {
    let u = uid.to_owned();
    let found = db.read(move |conn| {
        let Some(id) = item::id_for_uid(conn, &u)? else {
            return Ok(None);
        };
        let Some(meta) = item::get(conn, id)? else {
            return Ok(None);
        };
        let binding = binding::get(conn, id)?.map(|b| b.uri);
        let tags = tag::applications(conn, id)?;
        let namespace = primary_ns(conn, id)?;
        Ok(Some((meta, binding, tags, namespace)))
    })?;
    let Some((meta, binding, tags, namespace)) = found else {
        anyhow::bail!("no item with uid `{uid}`");
    };
    let chars = meta.content.as_ref().map_or(0, |c| c.chars().count());
    if json {
        println!(
            "{}",
            serde_json::json!({
                "uid": meta.uid, "kind": meta.kind, "status": meta.status,
                "resolution": meta.resolution,
                "priority": meta.priority, "due": meta.due, "mime": meta.mime,
                "namespace": namespace, "binding": binding, "content_chars": chars,
                "tags": tags.iter().map(|(f, v)| serde_json::json!({"facet": f, "value": v})).collect::<Vec<_>>(),
                "created_at": meta.created_at, "updated_at": meta.updated_at,
            })
        );
    } else {
        print_item_detail(&meta, binding.as_deref(), namespace.as_deref(), &tags);
        println!("content:   {chars} chars");
    }
    Ok(())
}

/// One node in the `jkb tree` output: a listed child plus its recursively-listed children.
struct TreeNode {
    child: Child,
    children: Vec<TreeNode>,
}

fn tree_nodes(
    conn: &rusqlite::Connection,
    path: Option<&str>,
    all: bool,
    depth_left: Option<usize>,
) -> jkb_core::Result<Vec<TreeNode>> {
    let mut out = Vec::new();
    for child in list_children(conn, path, all)? {
        let descend = child.kind == "namespace" && depth_left != Some(0);
        let children = if descend {
            tree_nodes(conn, Some(&child.reference), all, depth_left.map(|d| d - 1))?
        } else {
            Vec::new()
        };
        out.push(TreeNode { child, children });
    }
    Ok(out)
}

fn tree_to_json(node: &TreeNode) -> serde_json::Value {
    let mut v = node.child.to_json();
    if !node.children.is_empty() {
        v["children"] = node.children.iter().map(tree_to_json).collect();
    }
    v
}

/// Depth `jkb tree` descends by default before eliding deeper folders with `…` — deep
/// enough to map any real subtree, shallow enough to bound the output and the per-namespace
/// query fan-out (each level lists its children). `--depth` overrides.
const DEFAULT_TREE_DEPTH: usize = 4;

/// Render one tree level with box-drawing prefixes (`├─`/`└─`); a namespace elided by the
/// depth cap (it has children we didn't descend into) gets a trailing `…`.
fn print_tree(nodes: &[TreeNode], prefix: &str) {
    for (i, node) in nodes.iter().enumerate() {
        let last = i + 1 == nodes.len();
        let (branch, cont) = if last {
            ("└─ ", "   ")
        } else {
            ("├─ ", "│  ")
        };
        // Show WHAT is in the subtree, not just how much: a bare number invites reading
        // every leaf as a task, which is what this display used to claim.
        let leaves = node
            .child
            .leaf_kinds
            .as_ref()
            .filter(|_| node.child.kind == "namespace")
            .map(|kinds| match format_leaf_kinds(kinds) {
                s if s.is_empty() => String::new(),
                s => format!(" ({s})"),
            })
            .unwrap_or_default();
        let ns_type = node.child.type_label();
        let status = node
            .child
            .status
            .as_deref()
            .map(|s| format!(" [{s}]"))
            .unwrap_or_default();
        let elided = if node.children.is_empty()
            && node.child.has_children
            && node.child.kind == "namespace"
        {
            " …"
        } else {
            ""
        };
        println!(
            "{prefix}{branch}{}{ns_type}{leaves}{}{status}{elided}",
            node.child.label,
            node.child.chunk_label()
        );
        print_tree(&node.children, &format!("{prefix}{cont}"));
    }
}

/// `jkb tree [path]` — a recursive map of the namespace subtree with per-folder counts.
fn cmd_tree(
    db: &Db,
    path: Option<&str>,
    all: bool,
    depth: Option<usize>,
    json: bool,
) -> Result<()> {
    let owned = path.map(str::to_owned);
    let depth = Some(depth.unwrap_or(DEFAULT_TREE_DEPTH));
    let nodes = db.read(move |conn| tree_nodes(conn, owned.as_deref(), all, depth))?;
    if json {
        let v = serde_json::json!({
            "path": path,
            "tree": nodes.iter().map(tree_to_json).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("{}", path.unwrap_or("."));
        print_tree(&nodes, "");
    }
    Ok(())
}

/// `jkb item rm <uid>` — delete an item and its cascade, reversibly.
fn cmd_item_rm(db: &Db, uid: &str, force: bool, json: bool) -> Result<()> {
    let id = require_uid(db, uid)?;
    let removed = db.write_txn("cli", move |conn, meta| item::remove(conn, meta, id, force))?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "uid": removed.uid,
                "kind": removed.kind,
                "placements": removed.placements,
                "edges": removed.edges,
                "tags": removed.tags,
            })
        );
    } else {
        println!(
            "removed {} [{}] — {} placement(s), {} edge(s), {} tag(s)",
            removed.uid, removed.kind, removed.placements, removed.edges, removed.tags
        );
        println!("`jkb undo` restores it, including its edges.");
    }
    Ok(())
}

// ---- `jkb related` + `jkb inv …` (investigations, design Dmem.5/Dmem.9) ----

/// Parse `--edge <type>` values into [`EdgeType`]s, rejecting unknown names with the list.
fn parse_edge_types(names: &[String]) -> Result<Vec<EdgeType>> {
    names
        .iter()
        .map(|name| {
            EdgeType::from_str_opt(name).with_context(|| {
                format!(
                    "unknown edge type `{name}`; available: {}",
                    EdgeType::ALL
                        .iter()
                        .map(|e| e.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        })
        .collect()
}

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
fn cmd_related(
    db: &Db,
    uid: &str,
    edge_names: &[String],
    depth: usize,
    direction: edge::Direction,
    json: bool,
) -> Result<()> {
    let types = parse_edge_types(edge_names)?;
    let start = require_uid(db, uid)?;
    let hops = db.read(move |conn| edge::walk(conn, start, &types, depth, direction))?;

    let mut rows = Vec::new();
    for hop in &hops {
        let id = hop.item;
        let Some(meta) = db.read(move |conn| item::get(conn, id))? else {
            continue;
        };
        rows.push((hop, meta));
    }

    if json {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|(hop, meta)| {
                serde_json::json!({
                    "uid": meta.uid,
                    "kind": meta.kind,
                    "status": meta.status,
                    "resolution": meta.resolution,
                    "depth": hop.depth,
                    "via": hop.via.as_str(),
                    "direction": match hop.direction {
                        edge::Direction::Out => "out",
                        edge::Direction::In => "in",
                        edge::Direction::Both => "both",
                    },
                    "snippet": meta.content.as_deref().map(first_line),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if rows.is_empty() {
        println!("(no related items)");
    } else {
        for (hop, meta) in &rows {
            let arrow = match hop.direction {
                edge::Direction::In => "<-",
                edge::Direction::Out | edge::Direction::Both => "->",
            };
            println!(
                "{:>2}  {arrow} {:<26} [{}]{} — {}",
                hop.depth,
                format!("{} {}", hop.via.as_str(), meta.uid),
                meta.kind,
                meta.resolution
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default(),
                meta.content.as_deref().map(first_line).unwrap_or_default(),
            );
        }
    }
    Ok(())
}

/// Resolve `jkb inv new`'s namespace: an explicit `memory/…` path is used as given; a bare
/// name is homed under the ambient repo (`memory/<repo>/<name>`, mirroring task homing,
/// design D26/D32) or at `memory/<name>` outside a repo or with `--global`.
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

/// `jkb guide` — a one-page cheat-sheet of the agent-facing command surface.
fn cmd_guide() {
    print!(
        r#"jkb — agent quickstart

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

READ ONE ITEM
  jkb cat <uid>               the raw body to stdout (pipe it, no metadata).
  jkb stat <uid>              compact metadata (kind, namespace, tags, timestamps).
  jkb item show <uid>         metadata + a (bounded) body preview.

TASKS
  jkb task add "text !p1 @2026-07-15 +ns #facet=value"   quick-add.
  jkb task next [DSL]         the ready frontier (unblocked, by priority then due).
  jkb task set <uid> --status done|open|in_progress|needs_review
  jkb task show <uid>         the full task body.

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
  jkb task tag add <uid> facet=value      apply a tag.
  jkb undo                                revert the last change.

Tips: prefer `find`/`query` (structured) over `grep` when you know the kind/tag; add
`--json` and parse; scope with a path or rely on the ambient cwd namespace.
"#
    );
}

/// The item's primary (home) namespace path, if placed.
fn primary_ns(conn: &rusqlite::Connection, id: ItemId) -> jkb_core::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .prepare_cached(
            "SELECT n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
             WHERE p.item_id = ?1 ORDER BY (p.role = 'primary') DESC, p.position LIMIT 1",
        )?
        .query_row([id.get()], |r| r.get::<_, String>(0))
        .optional()?)
}

/// Whether an item's content is human-readable text worth showing in full (task notes,
/// prose, markdown) versus a heavy blob (PDF/image) that should stay a bounded preview.
fn is_text_like(kind: &str, mime: Option<&str>) -> bool {
    matches!(kind, "task" | "text" | "note" | "view")
        || mime.is_some_and(|m| m.starts_with("text/") || m.contains("markdown"))
}

/// Default content cap (chars) for text-like kinds (task notes, prose, markdown, ingested
/// text documents). Generous, but finite — the details pane shows a **bounded** preview,
/// never the whole document, so a multi-MB ingested doc can't spike webview latency/memory
/// (ui/README). Override with `--preview <n>` to read more.
const TEXT_PREVIEW_MAX: usize = 100_000;

/// Default content cap (chars) for heavy kinds (PDF/image blobs) — a short excerpt only.
const HEAVY_PREVIEW_MAX: usize = 800;

/// `jkb item show <uid>` — generic, kind-aware item details + content. `preview_arg` caps
/// the content; when `None`, text-like kinds cap at [`TEXT_PREVIEW_MAX`] and heavy kinds at
/// [`HEAVY_PREVIEW_MAX`]. A larger document is truncated (flagged `preview_truncated`);
/// override with `--preview <n>`.
fn cmd_item_show(db: &Db, uid: &str, preview_arg: Option<usize>, json: bool) -> Result<()> {
    let u = uid.to_owned();
    let found = db.read(move |conn| {
        let Some(id) = item::id_for_uid(conn, &u)? else {
            return Ok(None);
        };
        let Some(meta) = item::get(conn, id)? else {
            return Ok(None);
        };
        let binding = binding::get(conn, id)?.map(|b| b.uri);
        let tags = tag::applications(conn, id)?;
        let namespace = primary_ns(conn, id)?;
        Ok(Some((meta, binding, tags, namespace)))
    })?;
    let Some((meta, binding, tags, namespace)) = found else {
        anyhow::bail!("no item with uid `{uid}`");
    };

    let preview_max = preview_arg.unwrap_or_else(|| {
        if is_text_like(&meta.kind, meta.mime.as_deref()) {
            TEXT_PREVIEW_MAX
        } else {
            HEAVY_PREVIEW_MAX
        }
    });
    let content_chars = meta.content.as_ref().map_or(0, |c| c.chars().count());
    let preview: String = meta
        .content
        .as_deref()
        .unwrap_or("")
        .chars()
        .take(preview_max)
        .collect();
    let preview_truncated = content_chars > preview_max;

    if json {
        let v = serde_json::json!({
            "uid": meta.uid,
            "kind": meta.kind,
            "status": meta.status,
            "resolution": meta.resolution,
            "priority": meta.priority,
            "due": meta.due,
            "mime": meta.mime,
            "binding": binding,
            "namespace": namespace,
            "content_chars": content_chars,
            "content_hash": meta.content_hash,
            "created_at": meta.created_at,
            "updated_at": meta.updated_at,
            "tags": tags
                .iter()
                .map(|(f, v)| serde_json::json!({"facet": f, "value": v}))
                .collect::<Vec<_>>(),
            "preview": preview,
            "preview_truncated": preview_truncated,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print_item_detail(&meta, binding.as_deref(), namespace.as_deref(), &tags);
        println!(
            "content:   {content_chars} chars{}",
            if preview_truncated {
                " (preview truncated)"
            } else {
                ""
            }
        );
        if !preview.is_empty() {
            println!("\n{preview}");
        }
    }
    Ok(())
}

/// Human-readable header lines for `item show`.
fn print_item_detail(
    meta: &item::ItemMeta,
    binding: Option<&str>,
    namespace: Option<&str>,
    tags: &[(String, String)],
) {
    println!("uid:       {}", meta.uid);
    println!("kind:      {}", meta.kind);
    if let Some(s) = &meta.status {
        println!("status:    {s}");
    }
    if let Some(r) = &meta.resolution {
        println!("resolution: {r}");
    }
    if let Some(ns) = namespace {
        println!("namespace: {ns}");
    }
    if let Some(m) = &meta.mime {
        println!("mime:      {m}");
    }
    if let Some(b) = binding {
        println!("binding:   {b}");
    }
    if !tags.is_empty() {
        let t: Vec<String> = tags.iter().map(|(f, v)| format!("{f}={v}")).collect();
        println!("tags:      {}", t.join(", "));
    }
    println!("updated:   {}", meta.updated_at);
}

/// `item edit <uid>` — replace (or `--append` to) any item's content through the audited
/// `item::set_content` seam (`content_hash` cleared, like `task edit`).
fn cmd_item_edit(
    db: &Db,
    uid: &str,
    text: &[String],
    stdin: bool,
    append: bool,
    json: bool,
) -> Result<()> {
    let new_text = if stdin {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("reading item content from stdin")?;
        buf.trim_end().to_owned()
    } else if text.is_empty() {
        anyhow::bail!("provide new content as arguments, or pass --stdin");
    } else {
        text.join(" ")
    };
    let u = uid.to_owned();
    let found = db.write_txn("cli", move |conn, meta| {
        let Some(id) = item::id_for_uid(conn, &u)? else {
            return Ok(false);
        };
        let content = if append {
            match item::get_content(conn, id)? {
                Some(existing) if !existing.is_empty() => format!("{existing}\n\n{new_text}"),
                _ => new_text,
            }
        } else {
            new_text
        };
        item::set_content(conn, meta, id, &content, None)?;
        Ok(true)
    })?;
    if !found {
        anyhow::bail!("no item with uid `{uid}`");
    }
    report(json, uid, if append { "appended" } else { "edited" });
    if uid.starts_with("file://") && !json {
        eprintln!(
            "note: this is a file-backed item; run `jkb sync` to propagate the edit to its file."
        );
    }
    Ok(())
}

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
            exclude,
            policy,
        } => cmd_mount_create(
            db,
            &ns,
            &dir,
            mode.into(),
            &serializer,
            include.as_deref(),
            exclude.as_deref(),
            policy.into(),
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
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if mounts.is_empty() {
        println!("(no mounts)");
    } else {
        for (path, m) in &mounts {
            println!("{path}  [{}]  {}", m.serializer, m.backing_uri);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_mount_create(
    db: &Db,
    ns_path: &str,
    dir: &Path,
    mode: SyncMode,
    serializer: &str,
    include: Option<&str>,
    exclude: Option<&str>,
    policy: ConflictPolicy,
) -> Result<()> {
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
    let (ns_path, serializer) = (ns_path.to_owned(), serializer.to_owned());
    let include = include.map(str::to_owned);
    let exclude = exclude.map(str::to_owned);
    let ns_display = ns_path.clone();
    db.write_txn("cli", move |conn, meta| {
        let ns_id = ns::ensure(conn, &ns_path)?;
        mount::create(
            conn,
            meta,
            ns_id,
            &backing,
            mode,
            &serializer,
            include.as_deref(),
            exclude.as_deref(),
            policy,
        )
    })?;
    println!("mounted {ns_display} -> {}", abs.display());
    Ok(())
}

fn cmd_sync(db: &Db, ns_path: Option<&str>, watch: bool) -> Result<()> {
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

    if let Some(ns) = ns_path {
        report_sync(db, ns)?;
    } else {
        let paths = db.read(jkb_core::mount::all_paths)?;
        if paths.is_empty() {
            println!("no mounts configured");
        }
        for ns in paths {
            report_sync(db, &ns)?;
        }
    }
    Ok(())
}

/// Reconcile one mount and print its summary.
fn report_sync(db: &Db, ns_path: &str) -> Result<()> {
    use jkb_sync::Outcome::{
        Conflict, Created, Exported, Imported, Merged, Normalized, Quarantined, Skipped, UpToDate,
    };
    let report = jkb_sync::sync(db, ns_path)?;
    println!(
        "sync {ns_path}: {} created, {} imported, {} exported, {} merged, {} normalized, \
         {} conflicts, {} quarantined, {} up-to-date, {} skipped",
        report.count(Created),
        report.count(Imported),
        report.count(Exported),
        report.count(Merged),
        report.count(Normalized),
        report.count(Conflict),
        report.count(Quarantined),
        report.count(UpToDate),
        report.count(Skipped),
    );
    for path in report.conflicts() {
        println!("  conflict: {}", path.display());
    }
    for path in report.quarantined() {
        println!("  needs attention (parse failed): {}", path.display());
    }
    Ok(())
}

/// The `--backlog`/`--sync`/`--managed` flags of `task add`, grouped so the helper
/// signatures stay under the bool-argument lint.
struct AddFlags {
    backlog: bool,
    sync: bool,
    managed: bool,
}

/// Derive a task's home namespace from `--backlog` and the ambient repo (design D26),
/// mutating `spec.home`/`spec.mirrors`. `had_explicit` is set when an explicit `+<ns>`
/// already chose the home.
fn resolve_task_home(
    db: &Db,
    spec: &mut task::NewTask,
    flags: &AddFlags,
    had_explicit: bool,
) -> Result<()> {
    if had_explicit {
        if flags.backlog {
            anyhow::bail!("--backlog conflicts with an explicit `+<ns>` placement");
        }
    } else if flags.backlog {
        let root = task::DEFAULT_ROOT;
        match ambient_repo(db)? {
            Some(repo) => spec.home = format!("{root}/{repo}/.backlog"),
            None if confirm_global_backlog()? => spec.home = format!("{root}/.backlog"),
            None => anyhow::bail!(
                "--backlog needs an ambient repo; run inside a mounted repo or use `+<ns>`"
            ),
        }
    } else if let Some(repo) = ambient_repo(db)? {
        // Inside a repo with no target: home at the per-repo inbox, mirrored into
        // the global inbox so it stays a complete capture view (D26.3).
        spec.home = format!("{}/{repo}/inbox", task::DEFAULT_ROOT);
        spec.mirrors = vec![task::DEFAULT_HOME.to_owned()];
    }
    // else: outside a repo with no target → the home stays `DEFAULT_HOME`.
    Ok(())
}

/// Derive a task's storage binding (design D26.5), setting `spec.binding` and returning
/// the synced `file://` uri if one applies. `--managed` forces KB-only; `--sync` requires
/// a covering `tasks` mount.
fn resolve_task_binding(
    db: &Db,
    spec: &mut task::NewTask,
    flags: &AddFlags,
    uid: &str,
) -> Result<Option<String>> {
    let synced_file = if flags.managed {
        None
    } else {
        jkb_sync::tasks_mount_file(db, &spec.home)?
    };
    match &synced_file {
        Some(bare) => {
            let local_id = uid.strip_prefix("task:").unwrap_or(uid);
            spec.binding = format!("{bare}#{local_id}");
        }
        None if flags.sync => anyhow::bail!(
            "--sync: no `tasks`-serializer file mount covers the home `{}`",
            spec.home
        ),
        None => {} // spec.binding stays `managed:` (from_quick_add default)
    }
    Ok(synced_file)
}

/// Handle `task add`: parse the quick-add line, derive the home (design D26 homing) and
/// the storage binding (D26.5), then create the task through the writer-actor.
fn cmd_task_add(db: &Db, text: &[String], flags: &AddFlags, json: bool) -> Result<()> {
    let input = text.join(" ");
    let qa = task::parse_quick_add(&input)?;
    let had_explicit_placement = !qa.placements.is_empty();
    let uid = task::mint_uid(&qa.title);
    let mut spec = task::NewTask::from_quick_add(uid.clone(), qa);

    resolve_task_home(db, &mut spec, flags, had_explicit_placement)?;
    let synced_file = resolve_task_binding(db, &mut spec, flags, &uid)?;

    let home = spec.home.clone();
    let id = db.write_txn("cli", move |conn, meta| task::create(conn, meta, &spec))?;
    if json {
        println!(
            "{}",
            serde_json::json!({"id": id.get(), "uid": uid, "home": home,
                "binding": synced_file.as_deref().unwrap_or("managed:")})
        );
    } else {
        println!("added task {uid} (item {id}) at {home}");
        if synced_file.is_some() {
            println!("  synced binding — run `jkb sync` to write it to the file");
        }
    }
    Ok(())
}

fn cmd_task(db: &Db, cmd: TaskCmd, global: bool, json: bool) -> Result<()> {
    match cmd {
        TaskCmd::Add {
            text,
            backlog,
            sync,
            managed,
        } => cmd_task_add(
            db,
            &text,
            &AddFlags {
                backlog,
                sync,
                managed,
            },
            json,
        )?,
        TaskCmd::Next { terms, limit } => {
            let mut query = jkb_core::query::parse(&terms.join(" "))?;
            apply_ambient_tasks(&mut query, db, global)?;
            let (scope, tags) = (query.scope.clone(), query.tags.clone());
            let mut rows = db.read(move |conn| task::ready(conn, scope, &tags))?;
            if let Some(limit) = limit {
                rows.truncate(limit);
            }
            let ids: Vec<ItemId> = rows.iter().map(|r| r.id).collect();
            let items = output::fetch_items(db, &ids)?;
            output::print_items(&items, json);
        }
        TaskCmd::Show { uid } => {
            let id = resolve_task_uid(db, &uid)?;
            output::print_item_full(db, id, json)?;
        }
        TaskCmd::Mirror => cmd_task_mirror(db, json)?,
        other => cmd_task_mutate(db, other, json)?,
    }
    Ok(())
}

/// Remove a task's reference (mirror) placement under `ns` (inverse of `task place`). A
/// missing namespace or absent mirror is a no-op that reports `0` removed.
fn cmd_task_unplace(db: &Db, uid: &str, ns: &str, json: bool) -> Result<()> {
    let id = resolve_task_uid(db, uid)?;
    let ns_path = ns.to_owned();
    let removed = db.write_txn("cli", move |conn, meta| {
        match jkb_core::ns::get(conn, &ns_path)? {
            Some(ns_id) => placement::unplace(conn, meta, id, ns_id),
            None => Ok(0),
        }
    })?;
    if json {
        println!("{}", serde_json::json!({ "uid": uid, "removed": removed }));
    } else {
        println!("unplaced {uid} from {ns} ({removed} mirror(s) removed)");
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
    match cmd {
        TaskCmd::Set {
            uid,
            status,
            priority,
            due,
        } => cmd_task_set(db, &uid, status, priority, due, json)?,
        TaskCmd::Edit {
            uid,
            text,
            stdin,
            append,
        } => cmd_task_edit(db, &uid, &text, stdin, append, json)?,
        TaskCmd::Tag { cmd } => {
            let (uid, facet_value, adding) = match cmd {
                TaskTagCmd::Add { uid, facet_value } => (uid, facet_value, true),
                TaskTagCmd::Rm { uid, facet_value } => (uid, facet_value, false),
            };
            let (facet, value) = facet_value
                .split_once('=')
                .context("tag must be `facet=value`, e.g. `size=small`")?;
            let (facet, value) = (facet.to_owned(), value.to_owned());
            let id = resolve_task_uid(db, &uid)?;
            db.write_txn("cli", move |conn, meta| {
                if adding {
                    tag::apply(conn, meta, id, &facet, &value)
                } else {
                    tag::remove(conn, meta, id, &facet, &value)
                }
            })?;
            report(json, &uid, if adding { "tagged" } else { "untagged" });
        }
        TaskCmd::Depend { uid, dep } => {
            let id = resolve_task_uid(db, &uid)?;
            let dep_uid = canonical_task_uid(&dep);
            db.write_txn("cli", move |conn, meta| {
                task::add_dependency(conn, meta, id, &dep_uid)
            })?;
            report(json, &uid, "depends_on set");
        }
        TaskCmd::Undepend { uid, dep } => {
            let id = resolve_task_uid(db, &uid)?;
            let dep_id = resolve_task_uid(db, &dep)?;
            db.write_txn("cli", move |conn, meta| {
                edge::unlink(conn, meta, id, dep_id, EdgeType::DependsOn)
            })?;
            report(json, &uid, "depends_on removed");
        }
        TaskCmd::Place { uid, ns, home } => {
            let id = resolve_task_uid(db, &uid)?;
            let ns_path = ns.clone();
            db.write_txn("cli", move |conn, meta| {
                let ns_id = jkb_core::ns::ensure(conn, &ns_path)?;
                if home {
                    task::set_primary_home(conn, meta, id, ns_id, 0)
                } else {
                    placement::place(conn, meta, id, ns_id, PlacementRole::Reference, 0)
                }
            })?;
            report(json, &uid, "placed");
        }
        TaskCmd::Unplace { uid, ns } => cmd_task_unplace(db, &uid, &ns, json)?,
        TaskCmd::Bind { uid, managed, sync } => {
            let (uri, mode) = match (managed, sync) {
                (_, Some(uri)) => (uri, Some(SyncMode::Bidirectional)),
                (true, None) => (task::MANAGED_BINDING.to_owned(), None),
                (false, None) => {
                    anyhow::bail!("pass --managed or --sync <uri>");
                }
            };
            let id = resolve_task_uid(db, &uid)?;
            db.write_txn("cli", move |conn, meta| {
                binding::set(conn, meta, id, &uri, mode, None)
            })?;
            report(json, &uid, "bound");
        }
        TaskCmd::Claim { uid, owner } => cmd_task_claim(db, &uid, owner, true, json)?,
        TaskCmd::Release { uid, owner } => cmd_task_claim(db, &uid, owner, false, json)?,
        TaskCmd::Reclaim { keep } => cmd_task_reclaim(db, &keep, json)?,
        // The read subcommands are dispatched by `cmd_task` and never reach here.
        TaskCmd::Add { .. } | TaskCmd::Next { .. } | TaskCmd::Show { .. } | TaskCmd::Mirror => {
            unreachable!()
        }
    }
    Ok(())
}

/// `task set`: update any of a task's `--status`/`--priority`/`--due` in one txn.
fn cmd_task_set(
    db: &Db,
    uid: &str,
    status: Option<String>,
    priority: Option<i64>,
    due: Option<String>,
    json: bool,
) -> Result<()> {
    if status.is_none() && priority.is_none() && due.is_none() {
        anyhow::bail!("nothing to set: pass at least one of --status/--priority/--due");
    }
    let id = resolve_task_uid(db, uid)?;
    db.write_txn("cli", move |conn, meta| {
        if let Some(s) = &status {
            task::set_status_str(conn, meta, id, s)?;
        }
        if let Some(p) = priority {
            task::set_priority(conn, meta, id, Some(p))?;
        }
        if let Some(d) = &due {
            task::set_due(conn, meta, id, Some(d))?;
        }
        Ok(())
    })?;
    report(json, uid, "updated");
    Ok(())
}

/// `task edit`: replace (or `--append` to) a task's body text through the audited
/// `item::set_content` seam. Content comes from `text` or, with `stdin`, from stdin.
fn cmd_task_edit(
    db: &Db,
    uid: &str,
    text: &[String],
    stdin: bool,
    append: bool,
    json: bool,
) -> Result<()> {
    let new_text = if stdin {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("reading task content from stdin")?;
        buf.trim_end().to_owned()
    } else if text.is_empty() {
        anyhow::bail!("provide new content as arguments, or pass --stdin");
    } else {
        text.join(" ")
    };
    let id = resolve_task_uid(db, uid)?;
    // A file-backed task is no longer a single line: the `tasks` serializer renders content
    // after the first line as the task's indented **body**, so `--append` round-trips. One
    // limit remains — a BLANK line closes a body on re-parse, so anything after it would
    // detach from the task and drift into section prose. Refuse that precisely, rather than
    // refusing every multi-line edit.
    let file_backed = uid.starts_with("file://");
    if file_backed && new_text.contains("\n\n") {
        anyhow::bail!(
            "`{uid}` is a file-backed task: a blank line ends its body in the source file, so \
             text after one would detach from the task on sync. Use single newlines, or edit \
             the source file directly and run `jkb sync`."
        );
    }
    db.write_txn("cli", move |conn, meta| {
        let content = if append {
            // A file-backed task's body is contiguous indented lines, so append with a single
            // newline; a managed task's content is free-form, so keep the blank-line break.
            let separator = if file_backed { "\n" } else { "\n\n" };
            match item::get_content(conn, id)? {
                Some(existing) if !existing.is_empty() => {
                    format!("{existing}{separator}{new_text}")
                }
                _ => new_text,
            }
        } else {
            new_text
        };
        item::set_content(conn, meta, id, &content, None)
    })?;
    report(json, uid, if append { "appended" } else { "edited" });
    if file_backed && !json {
        eprintln!(
            "note: this is a file-backed task; run `jkb sync` to propagate the edit to its file."
        );
    }
    Ok(())
}

/// `task reclaim` (design D27.1/D27.6.6b): the deterministic owner-existence scan,
/// exposed so the coordinator can run it SQL-free. Clears claims whose owner pid is
/// gone, preserving `keep` owners (the live run passes its own owner so it never
/// reclaims its own in-flight work).
fn cmd_task_reclaim(db: &Db, keep: &[String], json: bool) -> Result<()> {
    let (held, cleared) = reclaim_orphaned(db, keep, true)?;
    if json {
        let uids: Vec<&str> = cleared.iter().map(|c| c.uid.as_str()).collect();
        println!("{}", serde_json::json!({"held": held, "reclaimed": uids}));
    } else {
        println!("reclaimed {} of {held} claim(s)", cleared.len());
        for c in &cleared {
            println!("  {} (dead owner {})", c.uid, c.owner);
        }
    }
    Ok(())
}

/// `task claim` / `task release` (design D27.3): CAS-acquire or clear a task's claim
/// through the 17.2 core seams, so the coordinator never touches SQL. `owner` defaults
/// to this process's liveness-checkable `host:pid` id. `claim` also flips the task to
/// `in_progress`.
fn cmd_task_claim(
    db: &Db,
    uid: &str,
    owner: Option<String>,
    acquire: bool,
    json: bool,
) -> Result<()> {
    let owner = owner.unwrap_or_else(owner::self_owner);
    let id = resolve_task_uid(db, uid)?;
    let owner2 = owner.clone();
    let ok = db.write_txn("cli", move |conn, meta| {
        if acquire {
            claim::claim(conn, meta, id, &owner2)
        } else {
            claim::release(conn, meta, id, &owner2)
        }
    })?;
    let key = if acquire { "acquired" } else { "released" };
    if json {
        println!(
            "{}",
            serde_json::json!({"uid": uid, "owner": owner, key: ok})
        );
    } else {
        match (acquire, ok) {
            (true, true) => println!("claimed {uid} for {owner} (now in_progress)"),
            (true, false) => println!("{uid} is already claimed by another live owner"),
            (false, true) => println!("released {uid} (was held by {owner})"),
            (false, false) => println!("{uid} was not claimed by {owner}"),
        }
    }
    Ok(())
}

/// Print a short human/JSON confirmation for a task mutation.
fn report(json: bool, uid: &str, action: &str) {
    if json {
        println!("{}", serde_json::json!({"uid": uid, "action": action}));
    } else {
        println!("{action}: {uid}");
    }
}

/// The owner-existence reclaim (design D27.1/D27.2): clear every claim whose owner
/// process no longer exists, keeping claims whose pid is alive plus any `keep` owners.
/// Returns `(total_held, cleared_or_orphaned_claims)`. Used by `task reclaim`,
/// `doctor` (report), and `doctor --fix`.
///
/// When `fix` is false this is **report-only**: it returns the held claims that *would*
/// be reclaimed (a stale-snapshot read is fine — nothing is written). When `fix` is true
/// the reclaim runs **inside the write transaction** (via [`claim::reclaim_dead`]) so
/// liveness is re-evaluated against the current claim set, closing the race where a claim
/// acquired concurrently by a live owner could be reclaimed from a snapshot.
///
/// # Errors
/// Errors if a database read/write fails.
fn reclaim_orphaned(db: &Db, keep: &[String], fix: bool) -> Result<(usize, Vec<claim::ClaimInfo>)> {
    let held = db.read(claim::claimed)?;
    let total = held.len();
    if !fix {
        return Ok((total, orphaned_claims(held, keep)));
    }
    let keep = keep.to_vec();
    let cleared = db.write_txn("cli", move |conn, meta| {
        claim::reclaim_dead(conn, meta, &keep, owner::is_alive)
    })?;
    Ok((total, cleared))
}

/// The held claims whose owner process no longer exists (owners in `keep` are alive by
/// fiat and never probed). Probes each **distinct** owner at most once via
/// [`owner::is_alive`] — the single source of the liveness rule, shared with the
/// txn-internal probe in [`claim::reclaim_dead`]. Report-only: it never writes.
fn orphaned_claims(held: Vec<claim::ClaimInfo>, keep: &[String]) -> Vec<claim::ClaimInfo> {
    let mut alive: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    held.into_iter()
        .filter(|c| {
            let live = *alive
                .entry(c.owner.clone())
                .or_insert_with(|| keep.iter().any(|o| o == &c.owner) || owner::is_alive(&c.owner));
            !live
        })
        .collect()
}

/// Canonicalize a task uid: leave a `:`-bearing uid alone, else prefix `task:`.
fn canonical_task_uid(uid: &str) -> String {
    if uid.contains(':') {
        uid.to_owned()
    } else {
        format!("task:{uid}")
    }
}

/// Resolve a task reference (full `task:<slug>` uid or bare slug) to its item id.
///
/// # Errors
/// Errors if no item matches either the given uid or `task:<uid>`.
fn resolve_task_uid(db: &Db, uid: &str) -> Result<ItemId> {
    // Accept either the full `task:<slug>` uid or the bare slug.
    let candidates = if uid.contains(':') {
        vec![uid.to_owned()]
    } else {
        vec![format!("task:{uid}"), uid.to_owned()]
    };
    let id = db.read(move |conn| {
        for cand in &candidates {
            if let Some(id) = jkb_core::item::id_for_uid(conn, cand)? {
                return Ok(Some(id));
            }
        }
        Ok(None)
    })?;
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
    let n = db.write_txn("cli", move |conn, meta| match txn {
        Some(txn) => undo::undo(conn, meta, txn),
        None => undo::undo_last(conn, meta),
    })?;
    println!("reverted {n} change(s)");
    Ok(())
}

fn cmd_index(db: &Db) -> Result<()> {
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

fn cmd_doctor(db: &Db, db_path: &Path, backup: Option<&Path>, fix: bool) -> Result<()> {
    // Embedder health.
    let embed_status = match embedder().and_then(|e| e.health_check().map_err(Into::into)) {
        Ok(()) => "ok".to_owned(),
        Err(e) => format!("unavailable: {e}"),
    };
    println!("embedder: {embed_status}");

    // FTS integrity.
    let fts = db.read(|conn| {
        let indexer = jkb_index::FtsIndexer::new();
        Ok(indexer.integrity_check(conn).is_ok())
    })?;
    println!("fts integrity: {}", if fts { "ok" } else { "FAILED" });

    // Schema version.
    let user_version: i64 =
        db.read(|conn| Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?))?;
    println!("schema user_version: {user_version}");

    // Un-embedded backlog.
    match embedder() {
        Ok(e) => {
            let pending = Pipeline::new(e).unembedded_count(db)?;
            println!("un-embedded items: {pending}");
        }
        Err(e) => println!("un-embedded items: unknown ({e})"),
    }

    // Files needing sync attention: conflicts and quarantined parse failures (D25).
    let flagged = db.read(jkb_core::sync_state::needs_attention)?;
    if flagged.is_empty() {
        println!("sync journal: ok");
    } else {
        println!("sync journal: {} file(s) need attention", flagged.len());
        for s in &flagged {
            let detail = s.parse_error.as_deref().unwrap_or("both sides changed");
            println!("  {} [{}]: {detail}", s.uri, s.status);
        }
    }

    // Stale task claims: owner-existence reclaim (design D27.2). For each claimed task
    // probe whether the recorded owner still exists (`kill -0`); a claim whose owner is
    // gone is orphaned (no time-based staleness — a paused-but-alive owner is retained).
    // A bare run reports; `--fix` clears orphaned claims so their tasks return to the
    // ready frontier.
    // One `reclaim_orphaned(.., false)` computes the report (shared with `task reclaim`,
    // no rule duplication). On `--fix` the reclaim re-probes inside the write txn — that
    // repeat is deliberate: the race-free clear must evaluate liveness against the current
    // claim set, not the report's snapshot.
    let (held_count, orphaned) = reclaim_orphaned(db, &[], false)?;
    if held_count == 0 {
        println!("task claims: none held");
    } else if orphaned.is_empty() {
        println!("task claims: {held_count} held, all owners alive");
    } else {
        println!(
            "task claims: {} orphaned (owner gone) of {held_count} held",
            orphaned.len(),
        );
        for c in &orphaned {
            println!("  {} claimed by dead owner {}", c.uid, c.owner);
        }
        if fix {
            let (_, cleared) = reclaim_orphaned(db, &[], true)?;
            println!("  cleared {} orphaned claim(s)", cleared.len());
        } else {
            println!("  run `jkb doctor --fix` to clear them");
        }
    }

    // Cloud-sync-folder warning (design D23).
    match jkb_core::cloud_sync_warning(db_path) {
        Some(w) => println!("warning: {w}"),
        None => println!("db location: ok ({})", db_path.display()),
    }

    if let Some(dest) = backup {
        db.backup(dest)?;
        println!("backup written to {}", dest.display());
    }
    Ok(())
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

fn first_line(content: &str) -> String {
    let line = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    line.trim().chars().take(100).collect()
}
