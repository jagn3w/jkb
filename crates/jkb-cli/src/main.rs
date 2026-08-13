//! `jkb` — command-line interface for the jkb knowledge base (Section 12).
//!
//! Thin edge over the library crates: `clap` parses subcommands, each wires to
//! `jkb-core`/`-ingest`/`-search`/`-sync`, and results print as human lines or
//! `--json`. Errors collapse into `anyhow` here (libraries use `thiserror`).
//! Read/task/query commands default their namespace scope to the mount covering the
//! current directory (design D19), overridable with `--global`.

mod base;
mod commands;
mod gitrepo;
mod output;
mod owner;
mod repo;
mod review;
mod service;
mod session;
mod staging;

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
        /// Override the mount's conflict policy for THIS RUN only, to get a stuck file
        /// moving without editing the mount.
        #[arg(long, value_enum, conflicts_with = "watch")]
        conflict: Option<PolicyArg>,
    },
    /// Staging branches: what is in flight, and where it will land.
    ///
    /// A staging branch is the branch a batch of tasks lands on before it reaches trunk —
    /// the same thing `/task-swarm` calls its integration branch. It is derived from tasks'
    /// `onto=` facets plus git, never stored (design D38.1).
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
    /// Run the MCP server (Section 13, not yet available).
    Mcp,
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
    /// Record the commit a branch was cut from, replacing only that branch's record.
    ///
    /// `task start` and `task work` record this themselves; this is for a branch created by
    /// hand, or to correct one. Without it a rebase-merged branch — which GitHub fast-forwards,
    /// leaving it byte-identical to trunk — cannot be told apart from a branch that was just
    /// created and never touched, so `close-merged` and `review record` both decline to act.
    Base {
        /// The task uid.
        uid: String,
        /// The branch the cut point belongs to.
        branch: String,
        /// The commit it was cut from (any git revision; resolved here).
        sha: String,
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
        uid: String,
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
    },
    /// Record that a code review ran, so `task land` can require one.
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
    /// Close tasks whose `branch=` has landed in this repo's trunk. A task closes only when
    /// its branch is merged AND all of its subtasks are terminal; anything else is
    /// reported. Works with merge-commit, squash, and rebase merges alike.
    CloseMerged {
        /// Only consider tasks tagged with this repo (default: this git repo's key).
        #[arg(long)]
        repo: Option<String>,
        /// The trunk to measure against (default: `origin/HEAD`, else main/master/trunk).
        #[arg(long)]
        trunk: Option<String>,
        /// Report what would close without changing anything.
        #[arg(long)]
        dry_run: bool,
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
enum TaskReviewCmd {
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
    /// Use for facets with one true answer — `branch=`, `repo=`, `onto=` — where a second
    /// value is a contradiction rather than extra information (design D36.6).
    ///
    /// **Not `base=`**: a cut point belongs to one branch, so a task working two of them has two
    /// records and this would delete one. `jkb task base` writes that one, and this refuses it.
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
        Command::Staging { cmd } => match cmd {
            StagingCmd::Ls { all } => cmd_staging_ls(&db, all, cli.json),
        },
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
        Command::Index { sweep } => cmd_index(&db, sweep),
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

/// Resolve a path the way the sync journal's uris were built: canonicalized.
///
/// `canonicalize` needs the file to exist, which is precisely what `jkb history` is often asked
/// about, and plain absolutisation resolves no symlinks — so on macOS a deleted file under
/// `/tmp` or `/var` produced a uri the journal never wrote. Canonicalizing the deepest ancestor
/// that DOES exist (normally the parent) and rejoining the rest gets both: the symlinks are
/// resolved and the missing leaf is preserved.
fn resolve_for_journal(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut rest = Vec::new();
    let mut cur = abs.as_path();
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            rest.push(name.to_owned());
        }
        if let Ok(real) = std::fs::canonicalize(parent) {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        cur = parent;
    }
    abs
}

/// `jkb history <path>` — every synced version of a file, newest first.
fn cmd_history(db: &Db, path: &str, json: bool) -> Result<()> {
    // Accept a bare path or a `file://` uri, and canonicalize so a relative path matches the
    // absolute uri the journal stores.
    let uri = if path.starts_with("file://") {
        path.to_owned()
    } else {
        // Absolutised WITHOUT requiring the file to exist, then keyed with `jkb-sync`'s own
        // spelling. `canonicalize` fails for a deleted file, which left a relative uri that
        // matched no journal row — so `jkb history <deleted file>` reported "no recorded
        // history" and blamed the build version, on exactly the recovery path the archive
        // exists to serve.
        // Canonicalize when the file is there — the journal's uris come from a canonicalized
        // mount directory, so on macOS `/var/...` must become `/private/var/...` to match — and
        // fall back to plain absolutisation when it is not, which is the case `jkb history`
        // exists for. Using only one of the two fails half the time: `canonicalize` alone left a
        // *relative* uri for a deleted file, and `absolute` alone misses the symlink.
        jkb_sync::file_uri(&resolve_for_journal(std::path::Path::new(path)))
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
    // Resolved BY ID, never by position — the JSON branch above already does this. `fetch_items`
    // drops rows it cannot find, so zipping made one missing item print nothing at all (and exit
    // 0), and a gap mid-list mislabelled every hit after it (design D42.4). Never pair two lists
    // by index when one of them can be shorter.
    let ids: Vec<ItemId> = hits.iter().map(|h| h.item).collect();
    let by_id: std::collections::HashMap<i64, output::DisplayItem> = output::fetch_items(db, &ids)?
        .into_iter()
        .map(|i| (i.id, i))
        .collect();
    for hit in &hits {
        let Some(item) = by_id.get(&hit.item.get()) else {
            // A hit whose item is gone should be unreachable now that `knn_live` filters them,
            // so say so rather than skipping silently — a search that quietly drops results is
            // the failure this fix exists to remove.
            eprintln!(
                "warning: search hit {} has no item row; run `jkb index --sweep`",
                hit.item.get()
            );
            continue;
        };
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
    /// For a task with subtasks: `(total, open)`. A parent with open subtasks is held off
    /// the ready frontier, so the tree must be able to show it as a container rather than
    /// as one more pickable task sitting beside its own children.
    subtasks: Option<(i64, i64)>,
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
            "subtask_count": self.subtasks.map(|(total, _)| total),
            "open_subtask_count": self.subtasks.map(|(_, open)| open),
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
    // The derivation is `output::title_of` — the one copy. Only the width is this function's
    // own, which is the split that helper's doc describes: a tree row, a staging row and a
    // gate refusal have different widths but must agree on what the task is *called*. This
    // was the fourth surviving copy, and the one that names tasks in the explorer tree.
    let title = output::title_of(meta);
    truncate(&title, 80)
}

/// The direct children of `path` (or top-level namespaces when `None`): sub-namespaces
/// followed by items whose **primary** placement is `path`. Terminal (`done`/`cancelled`)
/// tasks are hidden unless `all`.
/// The children of any item that contains others — the container behaviour a node takes on
/// (design D35).
///
/// One read for every container. A task's subtasks and a document's chunks are the same
/// query because containment is recorded the same way for both: on the placement. Nothing
/// here branches on what kind of node it is.
fn contained_children(
    conn: &rusqlite::Connection,
    parent: jkb_types::ItemId,
    all: bool,
) -> jkb_core::Result<Vec<Child>> {
    let ids = jkb_core::containment::children(conn, parent)?;
    let subtask_counts = jkb_core::containment::child_counts(conn, &ids)?;
    let chunk_counts = item::derived_kind_counts(conn, &ids, KIND_CHUNK)?;
    let mut out = Vec::new();
    for id in ids {
        let Some(meta) = item::get(conn, id)? else {
            continue;
        };
        if !all && jkb_types::TaskStatus::is_terminal_str(meta.status.as_deref()) {
            continue;
        }
        let subtasks = subtask_counts.get(&id).copied();
        let chunks = chunk_counts.get(&id).copied().unwrap_or(0);
        out.push(Child {
            label: item_label(&meta),
            kind: meta.kind,
            reference: meta.uid,
            // Containment nests: a child that contains in turn expands in turn.
            has_children: subtasks.is_some_and(|(total, _)| total > 0) || chunks > 0,
            status: meta.status,
            priority: meta.priority,
            leaf_count: None,
            leaf_kinds: None,
            ns_type: None,
            ns_type_about: None,
            chunk_count: (chunks > 0).then_some(chunks),
            subtasks,
            updated: Some(meta.updated_at),
        });
    }
    Ok(out)
}

fn list_children(
    conn: &rusqlite::Connection,
    path: Option<&str>,
    all: bool,
) -> jkb_core::Result<Vec<Child>> {
    // "Container" is a behaviour, not a node kind. A pure namespace is a node that ONLY
    // contains; a parent task both is a task and contains its subtasks. So `ls` resolves a
    // namespace first (the common case, and the historical meaning) and falls back to an
    // item uid — one command lists the children of anything that has any.
    if let Some(p) = path {
        if ns::get(conn, p)?.is_none() {
            if let Some(id) = item::id_for_uid(conn, p)? {
                return contained_children(conn, id, all);
            }
        }
    }
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
            subtasks: None,
            updated: None,
        });
    }

    if let Some(p) = path {
        if let Some(ns_id) = ns::get(conn, p)? {
            // Any placement role: a `tasks/…` mirror surfaces the task even though its
            // primary home is elsewhere (the symbolic-link view).
            // Directly placed only: a contained node is listed under its container, not
            // beside it. It is still IN this namespace — `ns:` scoping finds it — which is
            // exactly why the placement keeps both the namespace and the parent.
            let placed = placement::items_directly_in(conn, ns_id)?;
            // One grouped query for every document's chunk count, not one per document.
            let chunk_counts = item::derived_kind_counts(conn, &placed, KIND_CHUNK)?;
            let subtask_counts = jkb_core::containment::child_counts(conn, &placed)?;
            for item_id in placed {
                let Some(meta) = item::get(conn, item_id)? else {
                    continue;
                };
                // Hide any terminal-status item (done/cancelled) unless `all` — like
                // ignored files, revealed only on explicit toggle.
                let terminal = jkb_types::TaskStatus::is_terminal_str(meta.status.as_deref());
                if !all && terminal {
                    continue;
                }
                let subtasks = subtask_counts.get(&item_id).copied();
                let chunks = chunk_counts.get(&item_id).copied().unwrap_or(0);
                out.push(Child {
                    label: item_label(&meta),
                    kind: meta.kind,
                    reference: meta.uid,
                    // Anything that contains expands: a task into its subtasks, a document
                    // into its chunks.
                    has_children: subtasks.is_some_and(|(total, _)| total > 0) || chunks > 0,
                    status: meta.status,
                    priority: meta.priority,
                    leaf_count: None,
                    leaf_kinds: None,
                    ns_type: None,
                    ns_type_about: None,
                    chunk_count: chunk_counts.get(&item_id).copied(),
                    subtasks,
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
#[derive(Default)]
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
        // Descend into any container, not just namespaces — otherwise de-duplicating a
        // subtask out of its namespace listing would make it unreachable in `tree`.
        let descend = child.has_children && depth_left != Some(0);
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
    // Deliberately no vector sweep, and none is needed: the `vec_items_<dim>_gc` trigger
    // (D42.2) removes the vector with the item, in the same statement, for every connection and
    // every caller. Two belts remain behind that brace — an id is never reissued (D40), so even
    // a row that somehow survives cannot be inherited, and `jkb index --sweep` collects rows
    // written before the trigger existed.
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

WORKING A TASK IN PARALLEL (each session is its own git worktree)
  jkb task work <uid>         open (or return to) this task's session: its own checkout and
                              branch `task/<session>`, claimed so nothing else starts it.
                              Work and COMMIT inside the printed worktree, nowhere else.
                              --onto <branch> names the STAGING branch it lands on; omit it
                              and jkb joins the batch in flight, or cuts one from trunk.
  jkb task land <uid>         rebase the session onto its target, run the repo's gate, and
                              on green mark the task done and remove the session. Serial:
                              one land at a time, so a red gate means YOUR branch broke it.
                              REFUSES a task with no recorded review, or whose review left a
                              must-fix (!p1) finding open. --no-review records a waiver.
  jkb task abandon <uid>      drop the session and reopen the task (the branch is kept).
  jkb task sessions           what is in flight here, with uncommitted work and commits ahead.
  jkb task gate ["<cmd>"]     show or set the command that verifies a landing in this repo.
      If you are inside a session, land is the human's call — commit, and say you are done.

STAGING BRANCHES (where a batch lands before trunk — the swarm's integration branch)
  jkb staging ls [--all]      every staging branch and the tasks landing on it: each task's state
                              (implementing / review / landed), its commits, and how many
                              must-fix findings its review left open. --all shows merged ones.
  jkb task review record --findings <ns>
                              record that a review ran against the current branch, so `land`
                              can require one. /review-log does this for you.
  jkb task tag set <uid> <f>=<v>
                              make <v> the facet's ONLY value (add appends). Use for the
                              single-answer facets: branch=, onto=, repo=.
  jkb task base <uid> <branch> <sha>
                              record where a branch was cut. Per branch, so `tag set` would
                              delete a sibling's and refuses; this is the verb for it.

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

/// The `--backlog`/`--sync`/`--managed` flags of `task add`, grouped so the helper
/// signatures stay under the bool-argument lint.
struct AddFlags {
    backlog: bool,
    sync: bool,
    managed: bool,
    /// An explicit home namespace, taken verbatim rather than lexed out of the quick-add
    /// line — see the `--home` flag.
    home: Option<String>,
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
            anyhow::bail!(
                "--backlog conflicts with an explicit placement (`--home`, or a `+<ns>` in the \
                 task line)"
            );
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
fn cmd_task_add(
    db: &Db,
    text: &[String],
    flags: &AddFlags,
    under: Option<&str>,
    json: bool,
) -> Result<()> {
    let input = text.join(" ");
    let qa = task::parse_quick_add(&input)?;
    let mut had_explicit_placement = !qa.placements.is_empty();
    let uid = task::mint_uid(&qa.title);
    let mut spec = task::NewTask::from_quick_add(uid.clone(), qa);

    // `--home` wins over a `+<ns>` in the line: it is the unambiguous form, and it is the
    // only one that can carry a path containing whitespace.
    if let Some(home) = &flags.home {
        spec.home.clone_from(home);
        had_explicit_placement = true;
    }

    // A subtask defaults to living beside its parent: splitting a task should not scatter
    // the pieces across namespaces, and `--under` is the only signal about where it belongs.
    let parent = match under {
        Some(p) => {
            let pid = resolve_task_uid(db, p)?;
            if !had_explicit_placement {
                if let Some(home) = db.read(move |conn| item::primary_namespace(conn, pid))? {
                    spec.home = home;
                    had_explicit_placement = true;
                }
            }
            Some(pid)
        }
        None => None,
    };

    resolve_task_home(db, &mut spec, flags, had_explicit_placement)?;
    let synced_file = resolve_task_binding(db, &mut spec, flags, &uid)?;

    let home = spec.home.clone();
    let id = db.write_txn("cli", move |conn, meta| {
        let id = task::create(conn, meta, &spec)?;
        if let Some(parent) = parent {
            task::add_subtask(conn, meta, parent, id)?;
        }
        Ok(id)
    })?;
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
            under,
            home,
        } => cmd_task_add(
            db,
            &text,
            &AddFlags {
                backlog,
                sync,
                managed,
                home,
            },
            under.as_deref(),
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
            // Subtasks are shown after the body: a parent is off the ready frontier until
            // they are all terminal, so "why isn't this actionable?" must be answerable
            // from the same command that shows the task.
            let subs = db.read(move |conn| task::subtasks(conn, id))?;
            if !subs.is_empty() && !json {
                let open = subs
                    .iter()
                    .filter(|t| !jkb_types::TaskStatus::is_terminal_str(t.status.as_deref()))
                    .count();
                println!("\nsubtasks ({open} open of {}):", subs.len());
                for t in &subs {
                    let status = t.status.as_deref().unwrap_or("?");
                    let title = t.title.as_deref().unwrap_or("");
                    println!("  [{status:^12}] {} — {}", t.uid, first_line(title));
                }
                if open > 0 {
                    println!("this task is held off the ready frontier until they are done");
                }
            }
        }
        TaskCmd::Subtasks { uid, all } => cmd_task_subtasks(db, &uid, all, json)?,
        TaskCmd::Mirror => cmd_task_mirror(db, json)?,
        cmd @ (TaskCmd::Work { .. }
        | TaskCmd::Land { .. }
        | TaskCmd::Abandon { .. }
        | TaskCmd::Sessions
        | TaskCmd::Gate { .. }) => cmd_task_session(db, cmd, json)?,
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
        TaskCmd::Tag { cmd } => cmd_task_tag(db, cmd, json)?,
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
        TaskCmd::Start {
            uid,
            branch,
            repo,
            owner,
        } => cmd_task_start(db, &uid, branch, repo, owner, json)?,
        TaskCmd::Base { uid, branch, sha } => cmd_task_base(db, &uid, &branch, &sha, json)?,
        TaskCmd::CloseMerged {
            repo,
            trunk,
            dry_run,
        } => cmd_task_close_merged(db, repo, trunk, dry_run, json)?,
        TaskCmd::Release { uid, owner } => cmd_task_claim(db, &uid, owner, false, json)?,
        TaskCmd::Review { cmd } => cmd_task_review(db, cmd, json)?,
        TaskCmd::Reclaim { keep } => cmd_task_reclaim(db, &keep, json)?,
        // The read and session subcommands are dispatched by `cmd_task` and never reach here.
        TaskCmd::Add { .. }
        | TaskCmd::Next { .. }
        | TaskCmd::Show { .. }
        | TaskCmd::Subtasks { .. }
        | TaskCmd::Mirror
        | TaskCmd::Work { .. }
        | TaskCmd::Land { .. }
        | TaskCmd::Abandon { .. }
        | TaskCmd::Sessions
        | TaskCmd::Gate { .. } => {
            unreachable!()
        }
    }
    Ok(())
}

/// `task subtasks <uid>` — a parent's children, shaped exactly like `jkb ls` output.
///
/// Sharing the shape is the point: the tree expands a namespace and a parent task with one
/// parser, so nesting subtasks costs the UI a different *command*, not a different model.
fn cmd_task_subtasks(db: &Db, uid: &str, all: bool, json: bool) -> Result<()> {
    // A thin alias over the container read: `jkb ls <task-uid>` is the same call. It exists
    // for discoverability from the task surface, not as a second implementation.
    let id = resolve_task_uid(db, uid)?;
    let children = db.read(move |conn| contained_children(conn, id, all))?;
    if json {
        let arr: Vec<_> = children.iter().map(Child::to_json).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "path": uid, "children": arr }))?
        );
    } else if children.is_empty() {
        println!("(no subtasks)");
    } else {
        for c in &children {
            print_ls_row(None, c, LsOpts::default());
        }
    }
    Ok(())
}

/// `task start` — claim the task and record where the work is happening.
///
/// Claiming and tagging together is the point: "I am starting this" and "here is the branch
/// that will finish it" are the same moment, and splitting them is how the tag ends up
/// missing on exactly the tasks that needed it.
fn cmd_task_start(
    db: &Db,
    uid: &str,
    branch: Option<String>,
    repo: Option<String>,
    owner: Option<String>,
    json: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let branch = match branch {
        Some(b) => b,
        None => gitrepo::current_branch(&cwd)?.context(
            "not on a branch here (detached HEAD?) — pass --branch, or run inside a git repo",
        )?,
    };
    // Through the MAIN copy, never `key(&cwd)`: inside a `jkb task work` session that is the
    // session's own directory, so the key came out as the session name — and now that these
    // facets are *set* rather than added, that replaced the real `repo=` instead of sitting
    // beside it. Every `repo=`-keyed surface (`staging ls`, In Flight, `task sessions`,
    // `batch_onto`, `task review record`) then stopped seeing the task, and `review record`
    // matching nothing is indistinguishable from a review that was never run.
    let repo = match repo {
        Some(r) => r,
        None => {
            repo::repo_ctx()
                .context("not inside a git repo — pass --repo, or run this from the repo")?
                .key
        }
    };
    // Refuse the trunk: tagging a task with `branch=main` would make it close the instant
    // anything merged, since trunk is trivially "merged into" itself.
    if let Some(t) = gitrepo::trunk(&cwd)? {
        let trunk_name = t.rsplit('/').next().unwrap_or(&t);
        anyhow::ensure!(
            branch != trunk_name,
            "`{branch}` is this repo's trunk — start work on a feature branch, or the task \
             would auto-close immediately"
        );
    }

    let id = resolve_task_uid(db, uid)?;
    // Where this branch *began*, offered to `base::ensure_recorded` as a guess and used only if
    // nothing is on record for this branch. `task work` guesses the tip of the branch it hangs
    // the session off; this guesses trunk's tip. Both are right only at the moment the branch is
    // created, which is why deciding whether to use one is not this command's business.
    let cut = gitrepo::rev(
        &cwd,
        &gitrepo::trunk(&cwd)?.unwrap_or_else(|| "HEAD".to_owned()),
    )?;

    let owner = owner.unwrap_or_else(owner::self_owner);
    let (o, b, r, cut_sha) = (owner.clone(), branch.clone(), repo.clone(), cut.clone());
    // Who holds it, and may we take it? **Liveness**, not string equality (D27.1). The bare
    // `claim::claim` CAS accepts only a free task or a byte-identical owner, so using its
    // answer as a refusal meant `task start` refused its own second run under a new pid, and
    // refused after `task work` — the very sequence the facet writing below exists for, since
    // a session claims as `session:<pid>:<worktree>`.
    let held = current_claim(db, id)?;
    let mut keep_claim = false;
    if let Some(prev) = &held {
        if prev != &owner && owner::is_alive(prev) {
            // A live session for this task that we are standing **inside** keeps its claim:
            // replacing a `session:` owner with this one-second process's `host:pid` would
            // make the task read as dead to `doctor --fix` the moment it exits, freeing a
            // session someone is working in (D36.6). Any other live owner is someone else.
            let inside =
                owner::session_worktree(prev).is_some_and(|w| session::is_within(&cwd, &w));
            anyhow::ensure!(
                inside,
                "{uid} is already claimed by {prev}, which is still alive — nothing was \
                 changed. Finish or abandon that work, or use `jkb task release {uid} \
                 --owner {prev}` if you are sure it is gone."
            );
            keep_claim = true;
        }
    }
    let displaced = held.clone();
    db.write_txn("cli", move |conn, meta| {
        // The CAS answer is checked rather than discarded: losing it means someone claimed the
        // task between the probe and here, and reporting "started" while writing this session's
        // branch onto their task is exactly the confusion the liveness guard above prevents.
        if !keep_claim && !swap_claim(conn, meta, id, displaced.as_deref(), &o)? {
            return Err(jkb_types::Error::Validation(
                "the task was claimed by someone else while this command was checking — \
                 nothing was changed; run it again"
                    .to_owned(),
            )
            .into());
        }
        // Through the one location-facet writer, exactly as `task work` does. These were
        // additive here, so `task work` followed by `task start` — which the guide encourages
        // — left the task carrying two `branch=` values for one worktree.
        repo::set_location_facets(
            conn,
            meta,
            id,
            &repo::Location {
                branch: Some(&b),
                repo: Some(&r),
                cut_from: cut_sha.as_deref(),
                ..repo::Location::default()
            },
        )?;
        Ok(())
    })?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "uid": uid, "branch": branch, "repo": repo, "owner": owner })
        );
    } else {
        println!("started {uid} on {repo}@{branch} (owner {owner})");
    }
    Ok(())
}

/// `task base <uid> <branch> <sha>` — record where a branch was cut (design D34.2).
///
/// The only cut-point writer a user or a workflow can reach; `base::ensure_recorded` is the other
/// one and refuses to overwrite. `sha` is resolved through git when we are in a repo, so a
/// caller may pass `HEAD`, a branch name or a short hash and the record is a full commit id —
/// `is_merged` compares it against `rev-parse` output, and a value git cannot resolve silently
/// disables the guard it exists to provide.
fn cmd_task_base(db: &Db, uid: &str, branch: &str, sha: &str, json: bool) -> Result<()> {
    gitrepo::valid_ref(branch)?;
    let id = resolve_task_uid(db, uid)?;
    let cwd = std::env::current_dir()?;
    // Refuse a revision that cannot be resolved rather than storing it verbatim. `landed_with_base`
    // treats an unresolvable cut point as no cut point, so a typo is no longer *dangerous* — but it
    // is still silent, and a task that quietly stops auto-closing is a bad way to learn you
    // fat-fingered a sha.
    //
    // Only when we are standing in **the task's own repo**, though. The database is global across
    // repos (D32), so this command is reachable from anywhere, and validating against whatever
    // repo the cwd happens to be rejected a correct sha typed from a sibling checkout. Where the
    // value cannot be checked the literal is kept and the fact that nothing verified it is said
    // out loud, rather than implied by silence.
    let task_repo = repo::task_tags(db, id)?;
    let task_repo = repo::facet_one(&task_repo, repo::FACET_REPO).cloned();
    let here = repo::repo_ctx().ok();
    let checkable = match (&task_repo, &here) {
        (Some(want), Some(ctx)) => *want == ctx.key,
        (None, Some(_)) => true,
        _ => false,
    };
    // Resolution and refusal are gated on the SAME condition, deliberately. Gating only the
    // refusal left the resolution running against whatever repo the cwd was in: standing in a
    // sibling checkout where the value happened to resolve, that repo's full commit id was written
    // as this task's cut point and printed as though verified. Rejecting a good sha was the nit
    // being fixed; accepting a foreign one silently is worse than either.
    let resolved = if checkable {
        let Some(resolved) = gitrepo::rev_commit(&cwd, sha)? else {
            anyhow::bail!(
                "`{sha}` is not a revision this repo can resolve, so nothing was recorded — a cut \
                 point git cannot resolve is treated as no cut point at all, and {branch} would \
                 silently never auto-close. Pass a commit that exists here."
            );
        };
        resolved
    } else {
        eprintln!(
            "warning: recorded `{sha}` unverified — this is not {}'s checkout, so nothing here \
             could resolve it. If it is wrong, {branch} will silently never auto-close.",
            task_repo.as_deref().unwrap_or("the task's repo")
        );
        sha.to_owned()
    };
    let (b, s) = (branch.to_owned(), resolved.clone());
    db.write_txn("cli", move |conn, meta| base::write(conn, meta, id, &b, &s))?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "uid": uid, "branch": branch, "base": resolved })
        );
    } else {
        println!("{uid}: {branch} was cut from {resolved}");
    }
    Ok(())
}

/// `task tag add|rm <uid> <facet>=<value>` — apply or remove one facet tag.
fn cmd_task_tag(db: &Db, cmd: TaskTagCmd, json: bool) -> Result<()> {
    let (uid, facet_value, mode) = match cmd {
        TaskTagCmd::Add { uid, facet_value } => (uid, facet_value, TagMode::Add),
        TaskTagCmd::Set { uid, facet_value } => (uid, facet_value, TagMode::Set),
        TaskTagCmd::Rm { uid, facet_value } => (uid, facet_value, TagMode::Rm),
    };
    let (facet, value) = facet_value
        .split_once('=')
        .context("tag must be `facet=value`, e.g. `size=small`")?;
    // A facet with structure the generic commands do not understand is refused, not mangled. The
    // cut point is per-branch multi-valued, so `set` clears other *branches'* records and `add`
    // appends an unattributable one — and the previous refusal message named `tag set base=` as
    // the remedy, which `/task-swarm` then adopted, so the tool was recommending the write that
    // destroys the records it refuses to act without.
    //
    // `rm` is still allowed: deleting a wrong record is a legitimate repair, and its effect is to
    // leave the branch with none — which both readers treat as "do not act".
    anyhow::ensure!(
        !base::is_reserved_facet(facet) || matches!(mode, TagMode::Rm),
        "`{facet}` records where a *branch* was cut, not where a task is: `add` would leave a \
         value naming no branch, and `set` would delete the records of the task's other \
         branches. Use `{}` instead.",
        base::VERB
    );
    // The ref-valued facets are read back and handed to git, so the same rule applies here as at
    // the location writer: a value git would read as an option must not reach the store.
    if matches!(facet, repo::FACET_BRANCH | repo::FACET_ONTO) && !matches!(mode, TagMode::Rm) {
        gitrepo::valid_ref(value)?;
    }
    let (facet, value) = (facet.to_owned(), value.to_owned());
    let id = resolve_task_uid(db, &uid)?;
    db.write_txn("cli", move |conn, meta| {
        match mode {
            // `add` is additive, honest to its name: an open-ended facet legitimately holds
            // several values, and a command called `add` must not silently delete one.
            TagMode::Add => tag::apply(conn, meta, id, &facet, &value),
            // `set` replaces the facet's other values. Right for the facets answering "where
            // is this being worked" — a second `onto=` is a contradiction, not extra
            // information, and a reader collapsing the multi-map picks one at random (D36.6).
            TagMode::Set => repo::set_facet(conn, meta, id, &facet, &value),
            TagMode::Rm => tag::remove(conn, meta, id, &facet, &value),
        }
    })?;
    report(
        json,
        &uid,
        match mode {
            TagMode::Add | TagMode::Set => "tagged",
            TagMode::Rm => "untagged",
        },
    );
    Ok(())
}

/// Dispatch the parallel-session subcommands (design D36): open a session, land it, drop it,
/// list what is in flight, or configure the gate that guards a landing.
fn cmd_task_session(db: &Db, cmd: TaskCmd, json: bool) -> Result<()> {
    match cmd {
        TaskCmd::Work { uid, onto } => cmd_task_work(db, &uid, onto.as_deref(), json),
        TaskCmd::Land {
            uid,
            gate,
            no_gate,
            keep_worktree,
            no_review,
        } => cmd_task_land(
            db,
            &uid,
            LandFlags {
                gate: gate.clone(),
                no_gate,
                keep_worktree,
                no_review,
            },
            json,
        ),
        TaskCmd::Abandon {
            uid,
            force,
            delete_branch,
        } => cmd_task_abandon(db, &uid, force, delete_branch, json),
        TaskCmd::Sessions => cmd_task_sessions(db, json),
        TaskCmd::Gate { cmd, clear } => cmd_task_gate(db, cmd.as_deref(), clear, json),
        _ => unreachable!("cmd_task_mutate routes only session subcommands here"),
    }
}

/// How `task tag` should write a facet.
#[derive(Clone, Copy)]
enum TagMode {
    /// Append a value, keeping any others.
    Add,
    /// Make this the facet's only value.
    Set,
    /// Remove this value.
    Rm,
}

/// The live session for a task, matched against **all** the branches it records.
///
/// Matching by worktree rather than by "the task's branch tag" is what makes a task that
/// picked up a second `branch=` (from `jkb task start`, or an earlier `--onto`) still resolve
/// to the session that actually exists on disk.
fn session_for(
    ctx: &repo::RepoCtx,
    tags: &BTreeMap<String, Vec<String>>,
) -> Result<Option<session::Session>> {
    let branches = repo::facet_values(tags, repo::FACET_BRANCH);
    Ok(session::discover(&ctx.root)?
        .into_iter()
        .find(|s| branches.contains(&s.branch)))
}

/// `task work` — open (or return) an isolated session for a task (design D36.2).
///
/// Idempotent by construction: the task's own `branch=` tag names its session, so a second
/// invocation hands back the same worktree instead of forking the work onto a second branch.
fn cmd_task_work(db: &Db, uid: &str, onto: Option<&str>, json: bool) -> Result<()> {
    let ctx = repo::repo_ctx()?;
    let cwd = std::env::current_dir()?;
    let id = resolve_task_uid(db, uid)?;

    let status = db
        .read(move |conn| item::get(conn, id))?
        .and_then(|m| m.status);
    if let Some(status) = status.as_deref() {
        anyhow::ensure!(
            !jkb_types::TaskStatus::is_terminal_str(Some(status)),
            "{uid} is already {status} — there is nothing to work"
        );
    }

    // Session worktrees live inside the repo, so the first one must not make it dirty.
    session::ensure_excluded(&ctx.root)?;

    let tags = repo::task_tags(db, id)?;
    let sessions = session::discover(&ctx.root)?;
    // A session already recorded on the task keeps its name, so a second invocation returns
    // the same worktree instead of forking the work onto a second branch. A task may record
    // more than one branch (a `jkb task start` before a `task work`, or an earlier `--onto`),
    // so prefer the one that has a live worktree over merely the first that parses.
    let recorded = repo::facet_values(&tags, repo::FACET_BRANCH);
    let existing = recorded
        .iter()
        .find(|b| sessions.iter().any(|s| s.branch == **b))
        .or_else(|| {
            recorded
                .iter()
                .find(|b| session::name_from_branch(b).is_some())
        })
        .and_then(|b| session::name_from_branch(b));
    let name = if let Some(existing) = existing {
        existing.to_owned()
    } else {
        let taken: std::collections::HashSet<String> =
            sessions.iter().map(|s| s.name.clone()).collect();
        session::mint_name(uid, |n| {
            taken.contains(n) || session::worktree_path(&ctx.root, n).exists()
        })
    };
    let branch = session::branch_for(&name);
    let worktree = session::worktree_path(&ctx.root, &name);
    let onto = resolve_onto(db, &ctx, &cwd, &tags, onto, &name)?;

    // Claim first: if someone else is on this task, stop before making a worktree they
    // would have to clean up.
    let owner = owner::session_owner(&worktree);
    claim_session(db, id, uid, &owner, &worktree)?;

    let resumed = sessions.iter().any(|s| s.branch == branch);
    if !resumed {
        if let Some(parent) = worktree.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        anyhow::ensure!(
            !worktree.exists(),
            "{} exists but git does not know it as a worktree — remove it, or run \
             `git worktree prune`",
            worktree.display()
        );
        if let Err(e) = gitrepo::worktree_add(&ctx.root, &worktree, &branch, &onto) {
            // Do not leave the task claimed for a session that failed to open.
            let _ = db.write_txn("cli", move |conn, m| claim::clear(conn, m, id));
            return Err(e);
        }
    }

    // Record where the work is happening, exactly as `task start` does (D34.1), plus the
    // land target so `land` and a resumed `work` agree on it. These three facets are *set*,
    // not added: a second value would be a contradiction rather than extra information, and
    // is how a task ends up with two branches and one worktree.
    //
    // The cut point is offered as a guess, and `base::ensure_recorded` decides. This used to be
    // gated on `resumed` — worktree existence — which is a proxy for the wrong thing: re-working
    // a branch after `abandon` leaves the branch but not the worktree, so `resumed` was false
    // while `worktree_add` merely re-attached the existing branch, and the guess overwrote a real
    // cut point with the land target's *current* tip. The branch tip then differed from its
    // recorded base, the empty-branch guard was skipped, and `close-merged` closed a task with no
    // work on it. The only question that answers this correctly is "is a cut point already
    // recorded for this branch", and it now has exactly one implementation.
    // Measured on the branch, not guessed from `onto` — and taken *after* the branch exists.
    //
    // A cut point is where this branch begins, and once the branch is there that is simply its
    // tip: a session has no commits yet, so `rev(branch) == rev(onto)` for one freshly cut and the
    // two forms agree. They come apart exactly when the branch was **not** cut here and now — left
    // behind by a `task work` that failed after creating it, made by hand, or adopted from the
    // remote by `ensure_branch`. There `onto` has since moved, so the guess records a commit the
    // branch never sat on: the tip no longer equals the base, `is_merged` skips its freshly-cut
    // guard, and an empty branch reads as merged and closes the task.
    //
    // Measuring is also fail-safe where guessing is not. On an adopted branch that *does* carry
    // commits, base == tip means "nothing to merge" and the readers decline to act — a missed
    // auto-close, which costs one command, rather than a false one, which buries work (D34.4).
    let cut = gitrepo::rev_commit(&ctx.root, &branch)?;
    let (b, r, o) = (branch.clone(), ctx.key.clone(), onto.clone());
    db.write_txn("cli", move |conn, meta| {
        repo::set_location_facets(
            conn,
            meta,
            id,
            &repo::Location {
                branch: Some(&b),
                repo: Some(&r),
                onto: Some(&o),
                cut_from: cut.as_deref(),
            },
        )
    })?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "uid": uid,
                "session": name,
                "worktree": worktree,
                "branch": branch,
                "onto": onto,
                "resumed": resumed,
                "owner": owner,
            })
        );
    } else {
        let verb = if resumed { "resumed" } else { "opened" };
        println!("{verb} session {name} for {uid}");
        println!("  worktree: {}", worktree.display());
        println!("  branch:   {branch} (lands on {onto})");
        println!("  next:     cd {} && claude", worktree.display());
        println!("  finish:   jkb task land {uid}");
    }
    Ok(())
}

/// Decide which branch this session's work will land on (design D36.3).
fn resolve_onto(
    db: &Db,
    ctx: &repo::RepoCtx,
    cwd: &Path,
    tags: &BTreeMap<String, Vec<String>>,
    flag: Option<&str>,
    session_name: &str,
) -> Result<String> {
    if let Some(branch) = flag {
        anyhow::ensure!(
            Some(branch) != ctx.trunk_name(),
            "refusing to land on {branch}: it is this repo's trunk, and a task tagged with \
             it would read as merged the moment anything lands"
        );
        // `ensure_branch`, not `create_branch`: the user has *named* an existing branch, so one
        // that exists only on `origin/` must be checked out rather than replaced by an empty
        // namesake cut from trunk.
        gitrepo::ensure_branch(&ctx.root, branch, &batch_start(ctx, cwd)?)?;
        return Ok(branch.to_owned());
    }
    // A session that already has a target keeps it — counting the remote-tracking copy, or a
    // batch whose local ref was pruned would silently retarget the session somewhere else.
    if let Some(branch) = repo::facet_one(tags, repo::FACET_ONTO) {
        if gitrepo::branch_ref(&ctx.root, branch, gitrepo::Prefer::Local)?.is_some() {
            return Ok(branch.clone());
        }
    }
    // Join the batch the other live sessions are landing on.
    if let Some(branch) = batch_onto(db, ctx)? {
        return Ok(branch);
    }
    // The branch you invoked from — unless that is trunk (landing there closes tasks
    // instantly, D34.3) or another session's branch (that would stack sessions).
    if let Some(branch) = gitrepo::current_branch(cwd)? {
        if Some(branch.as_str()) != ctx.trunk_name() && session::name_from_branch(&branch).is_none()
        {
            return Ok(branch);
        }
    }
    // Cut the batch branch from trunk, named after this task — the first of the batch.
    // `create_branch`, deliberately: this is cutting a NEW batch at a known start point, so a
    // same-named branch left on the remote by an earlier, possibly already-merged batch must not
    // be adopted in its place.
    gitrepo::create_branch(&ctx.root, session_name, &batch_start(ctx, cwd)?)?;
    Ok(session_name.to_owned())
}

/// The commit a new batch branch is cut from.
///
/// The **local** trunk when that is where you are standing, and only otherwise the trunk ref
/// — which is usually `origin/main`. A local trunk ahead of its remote is the ordinary case
/// (you just merged a PR and pulled, or you commit locally first), and cutting the batch from
/// the remote ref there would silently start the work behind commits you already have, then
/// land it as if it were on top of them.
fn batch_start(ctx: &repo::RepoCtx, cwd: &Path) -> Result<String> {
    if let Some(branch) = gitrepo::current_branch(cwd)? {
        if Some(branch.as_str()) == ctx.trunk_name() {
            return Ok(branch);
        }
    }
    ctx.trunk.clone().context(
        "could not determine this repo's trunk, so there is nothing to cut a branch from \
         — pass --onto <branch> naming one that exists",
    )
}

/// The land target the repo's other live sessions share, if they agree on one.
///
/// This is what makes a second session started from trunk join the first one's batch instead
/// of cutting a branch of its own. Sessions that disagree are left alone: guessing which of
/// two batches a new task belongs to is worse than asking for `--onto`.
fn batch_onto(db: &Db, ctx: &repo::RepoCtx) -> Result<Option<String>> {
    let by_branch = repo::tasks_by_branch(db, &ctx.key)?;
    let mut found: Option<String> = None;
    for s in session::discover(&ctx.root)? {
        let Some(onto) = by_branch.get(&s.branch).and_then(|t| t.onto.clone()) else {
            continue;
        };
        // Remote-aware for the same reason: a live batch whose local ref is gone must still be
        // joinable, or the next session cuts a second batch beside it.
        if gitrepo::branch_ref(&ctx.root, &onto, gitrepo::Prefer::Local)?.is_none() {
            continue;
        }
        match &found {
            None => found = Some(onto),
            Some(f) if *f == onto => {}
            Some(_) => return Ok(None),
        }
    }
    if found.is_none() {
        // No sessions right now, but a batch checkout may survive from an earlier round —
        // you landed one task and are starting the next. Join it only while that batch is
        // still LIVE: once it has merged (or never had a commit), holding on would both
        // attract new work onto a dead branch and keep `git branch -d` from deleting it. So
        // a merged batch's checkout is released here rather than reused.
        if let Some(branch) = base_branch(ctx)? {
            if !batch_is_spent(ctx, &branch)? {
                return Ok(Some(branch));
            }
            release_base_worktree(ctx)?;
        }
    }
    Ok(found)
}

/// The branch checked out in `.jkb/base`, if that worktree exists.
fn base_branch(ctx: &repo::RepoCtx) -> Result<Option<String>> {
    let base = session::base_worktree(&ctx.root);
    Ok(gitrepo::worktrees(&ctx.root)?
        .into_iter()
        .find(|w| session::same_path(&w.path, &base))
        .and_then(|w| w.branch))
}

/// Whether a batch branch has nothing left to give: already merged into trunk, or never
/// carried a commit of its own. Both are answered by [`gitrepo::is_merged`] with no recorded
/// base — an empty branch re-merges to trunk's own tree exactly as a landed one does — which
/// is also why this is not `--is-ancestor`: a squash-merged batch must read as merged too.
///
/// A repo with no discoverable trunk cannot answer the question, so the batch is kept: losing
/// a live batch is worse than reusing a spent one.
fn batch_is_spent(ctx: &repo::RepoCtx, branch: &str) -> Result<bool> {
    let Some(trunk) = &ctx.trunk else {
        return Ok(false);
    };
    // The **local** branch: this asks whether the batch here has anything left to give,
    // and a local ref that has had another task landed onto it is not spent, whatever its
    // pushed copy did.
    Ok(
        gitrepo::is_merged(&ctx.root, branch, trunk, None, gitrepo::Prefer::Local)?.0
            == gitrepo::MergeState::Merged,
    )
}

/// Remove `.jkb/base`, freeing the branch it holds. It is only ever a checkout cache; `land`
/// makes a new one on demand.
fn release_base_worktree(ctx: &repo::RepoCtx) -> Result<()> {
    let base = session::base_worktree(&ctx.root);
    if base.exists() {
        gitrepo::worktree_remove(&ctx.root, &base, true)?;
    }
    Ok(())
}

/// Who holds `id` right now, or `None` if it is free.
///
/// The read half of every claim takeover: the owner string this returns is the one the caller
/// judges (liveness, same-session, same-worktree) and the one [`swap_claim`] must later CAS
/// against, so the two always talk about the same claim.
fn current_claim(db: &Db, id: ItemId) -> Result<Option<String>> {
    Ok(db
        .read(claim::claimed)?
        .into_iter()
        .find(|c| c.id == id)
        .map(|c| c.owner))
}

/// Move the claim on `id` to `owner`, atomically against `displaced` — the **exact** owner
/// [`current_claim`] returned and the caller judged. `Ok(false)` means the claim changed hands
/// in between and nothing was written; every caller turns that into "run it again".
///
/// Clearing first is what lets a resumed session re-take its own claim under a new pid: the CAS
/// in [`claim::claim`] accepts only a free task or a byte-identical owner. Clearing only the
/// *judged* owner is what stops it from throwing away a claim it never looked at — the liveness
/// probe forks `ps` outside the transaction, and a `jkb task work` landing in that window would
/// otherwise lose its fresh claim to a decision taken about a different, dead owner.
///
/// The one rendering of that dance. It was written twice, and the copies had already drifted:
/// one checked the CAS answer and one did not, so half the callers could report success while
/// writing this session's branch onto somebody else's task.
///
/// # Errors
/// Returns an error if either claim write fails.
fn swap_claim(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    id: ItemId,
    displaced: Option<&str>,
    owner: &str,
) -> jkb_core::Result<bool> {
    if let Some(prev) = displaced {
        if !claim::clear_if(conn, meta, id, prev)? {
            return Ok(false);
        }
    }
    claim::claim(conn, meta, id, owner)
}

/// Take the session's claim, taking over from this session's own previous process (a resume)
/// or from a dead owner, and refusing any other live owner **by name** (design D36.6).
fn claim_session(db: &Db, id: ItemId, uid: &str, owner: &str, worktree: &Path) -> Result<()> {
    let held = current_claim(db, id)?;
    if let Some(prev) = &held {
        let same_session =
            owner::session_worktree(prev).is_some_and(|w| session::same_path(&w, worktree));
        if !same_session && owner::is_alive(prev) {
            let where_ = owner::session_worktree(prev).map_or_else(
                || format!("owner {prev}"),
                |w| format!("a session in {}", w.display()),
            );
            anyhow::bail!(
                "{uid} is already being worked by {where_} — finish or abandon that session, \
                 or work a different task"
            );
        }
    }
    let (o, displaced) = (owner.to_owned(), held.clone());
    let ok = db.write_txn("cli", move |conn, meta| {
        swap_claim(conn, meta, id, displaced.as_deref(), &o)
    })?;
    anyhow::ensure!(
        ok,
        "{uid} was claimed by someone else while this command was checking — nothing was \
         changed; run it again"
    );
    Ok(())
}

/// `task land` — the merge queue for one session (design D36.4).
/// The flags of `task land`, grouped so the signature stays under the bool-argument lint.
struct LandFlags {
    gate: Option<String>,
    no_gate: bool,
    keep_worktree: bool,
    no_review: bool,
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
fn land_preflight(
    db: &Db,
    ctx: &repo::RepoCtx,
    uid: &str,
    id: ItemId,
    tags: &BTreeMap<String, Vec<String>>,
) -> Result<Preflight> {
    // The task's own pipeline state, mapped by the one function that does that — so the
    // terminal arm of `land_blocker` below is the arm that actually fires here, rather than a
    // second bail beside it saying the same thing in its own words.
    let state = staging::State::from_status(
        &db.read(move |conn| item::get(conn, id))?
            .and_then(|m| m.status)
            .unwrap_or_default(),
    );
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
    let sess = session_for(ctx, tags)?;
    let branch = sess
        .as_ref()
        .map(|s| s.branch.clone())
        .or_else(|| repo::facet_one(tags, repo::FACET_BRANCH).cloned())
        .context("this task records no branch")?;
    let onto = repo::facet_one(tags, repo::FACET_ONTO)
        .cloned()
        .context("this session records no land target — re-run `jkb task work` with --onto")?;
    anyhow::ensure!(
        gitrepo::has_branch(&ctx.root, &onto)?,
        "the land target {onto} no longer exists"
    );
    // Everything from here is `staging::land_blocker` — the ONE derivation of "may this
    // land", which the In Flight row renders verbatim. It was restated per surface twice, and
    // each time a row claimed "Landable" for a task this command then refused: the uncommitted
    // session, the empty branch, the dirty target checkout, the review gate. Assembling the
    // facts is this side's job; judging them is not.
    let ahead = gitrepo::ahead_count(&ctx.root, &onto, &branch)?;
    let worktrees = gitrepo::worktrees(&ctx.root)?;
    let mut dirty_cache = BTreeMap::new();
    let target_dirty =
        staging::target_dirty_reason(&worktrees, &ctx.root, &onto, &mut dirty_cache)?;
    let dirty = match &sess {
        Some(s) => gitrepo::is_dirty(&s.worktree)?,
        None => false,
    };
    if let Some(reason) = staging::land_blocker(&staging::LandFacts {
        state,
        worktree: sess.as_ref().map(|s| s.worktree.as_path()),
        dirty,
        commits: ahead,
        branch_exists: gitrepo::has_branch(&ctx.root, &branch)?,
        target_dirty: target_dirty.as_deref(),
        // The review is enforced a moment later by `review::enforce`, which renders the same
        // verdict at length and is where `--no-review` records a waiver instead of refusing.
        verdict: None,
    }) {
        anyhow::bail!("{uid} cannot land. {reason}");
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

fn cmd_task_land(db: &Db, uid: &str, flags: LandFlags, json: bool) -> Result<()> {
    let LandFlags {
        gate: gate_flag,
        no_gate,
        keep_worktree,
        no_review,
    } = flags;
    let gate_flag = gate_flag.as_deref();
    let ctx = repo::repo_ctx()?;
    let id = resolve_task_uid(db, uid)?;
    let tags = repo::task_tags(db, id)?;
    let Preflight {
        sess,
        branch,
        onto,
        ahead,
    } = land_preflight(db, &ctx, uid, id, &tags)?;

    // The review gate (design D38.5), before the graft: a refusal must not have moved a
    // branch first. Concerns and nits do not block — only must-fix findings do. A waiver is
    // only *owed* here; it is written after the landing actually happens, so a land that then
    // fails on the graft or the gate build leaves no waiver for something that never occurred.
    let head = gitrepo::rev(&ctx.root, &branch)?.unwrap_or_else(|| "unknown".to_owned());
    let waiver_owed = review::enforce(db, uid, &tags, no_review, json)?;

    // Landing is serial: two grafts at once would each gate a tree the other is changing.
    let _lock = session::LandLock::acquire(&ctx.root)?;

    let land_dir = land_dir_for(&ctx, &onto)?;
    anyhow::ensure!(
        !gitrepo::is_dirty(&land_dir)?,
        "{} (checked out to {onto}) has uncommitted changes — landing would roll them back \
         on a red gate",
        land_dir.display()
    );

    let (outcome, pre) = gitrepo::graft(&land_dir, &branch, &onto)?;
    let gitrepo::Graft::Landed { grafted } = outcome else {
        anyhow::bail!(
            "{branch} does not rebase cleanly onto {onto} — nothing changed. Rebase it where \
             the context is: cd {} && git rebase {onto}, fix the conflict, then land again",
            sess.worktree.display()
        );
    };

    let (gate, source) = session::resolve_gate(db, &ctx.root, &ctx.key, gate_flag, no_gate)?;
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
        db,
        id,
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
        },
        json,
    )
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
}

/// Mark the task done, free the claim, and dispose of the session (design D36.4).
fn settle_landing(
    db: &Db,
    id: ItemId,
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
        let sha = sha.to_owned();
        db.write_txn("cli", move |conn, meta| {
            repo::set_facet(conn, meta, id, review::FACET_REVIEW_WAIVED, &sha)
        })?;
    }

    // Is the session still there at all? `git status` in a directory that no longer exists
    // exits non-zero, and `gitrepo::git` maps that to `Ok(None)` — so `is_dirty` answers
    // "clean" for a vanished worktree and the disposal below then fails on it. A concurrent
    // `jkb task abandon` removes exactly this directory, so the case is real, and "gone" is
    // the one state where disposal has nothing left to do.
    let disposed_already = !sess.worktree.exists();

    // The session was verified clean in `land_preflight`, but that was before a graft and a
    // gate build that can run for minutes — long enough for the agent sitting in the session
    // to write a file. Every disposal below is destructive (`reset --hard`, `worktree
    // remove`), so the check is taken **again**, here, against the state we are about to
    // discard. The landing itself already happened and is not undone by this; the session is
    // simply kept, with its work, for the person to deal with.
    anyhow::ensure!(
        disposed_already || !gitrepo::is_dirty(&sess.worktree)?,
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

    // Dispose of the session FIRST, because it is the fallible half. `worktree_remove` without
    // `--force` is refused by git on a dirty tree, and doing it after the status write left
    // the task marked `done` with its claim freed and its worktree still there — a state both
    // escape hatches then refuse ("is done — there is nothing to land", "abandoning it would
    // reopen finished work"), recoverable only by hand-editing the status.
    let mut cleaned = false;
    if disposed_already {
        // Somebody removed it while the gate ran. Nothing to dispose of, and prune the
        // registration so git stops listing a worktree whose directory is gone.
        let _ = gitrepo::prune_worktrees(&ctx.root);
        cleaned = true;
    } else if landed.keep_worktree {
        // `graft` rebased a detached HEAD, so the branch ref still points at its pre-rebase
        // commits. Left there, the kept session reads as N commits ahead of a target that
        // already contains its work, and a second `land` re-runs the whole graft. Move it to
        // what actually landed; the worktree is verified clean just above, so nothing is lost.
        gitrepo::reset_hard(&sess.worktree, landed.grafted)?;
    } else {
        gitrepo::worktree_remove(&ctx.root, &sess.worktree, false)?;
        gitrepo::delete_branch(&ctx.root, landed.branch, true)?;
        cleaned = true;
    }

    // Landed: the task is done, the claim is free, and the session branch is a duplicate of
    // commits now in `onto`.
    //
    // The status is re-read **inside** the transaction: `land_preflight` checked it before a
    // multi-minute gate, and nothing serializes a `jkb task set --status cancelled` against a
    // land (`LandLock` only excludes a second land). Writing `Done` over a cancellation made
    // this the one transition the guard exists to prevent. Same reasoning as `review::record`.
    // Whether the status was left as somebody else set it during the gate. Reported, not
    // returned as an error: the session HAS been disposed of by this point, so bailing left
    // the claim held on a worktree that no longer exists — freed only by `doctor --fix` — and
    // said nothing about what had just been removed.
    let kept_status = db.write_txn("cli", move |conn, meta| {
        // No claim CAS here. `task::set_status` clears the claim unconditionally on a terminal
        // status (task.rs:446) and `claim::clear` has no owner predicate, so an owner-scoped
        // clear a few lines above it decided nothing on the path that matters — and a guard that
        // does nothing is worse than none, because it reads as protection.
        //
        // Two things this deliberately leaves OPEN rather than pretending to solve: on the
        // early-return below (the task is already terminal) `set_status` never runs, so nothing
        // clears the claim there; and `land_preflight` never checks who holds the claim, so
        // `jkb task land` can still free a live non-session claim through that same transition.
        let current = item::get(conn, id)?.and_then(|m| m.status);
        if jkb_types::TaskStatus::is_terminal_str(current.as_deref()) {
            return Ok(current);
        }
        task::set_status(conn, meta, id, jkb_types::TaskStatus::Done)?;
        Ok(None)
    })?;
    if let Some(status) = &kept_status {
        eprintln!(
            "note: {} became {status} while the gate was running, so its status was left \
             alone. Its commits are on {}, and its session has been disposed of.",
            landed.uid, landed.onto
        );
    }

    // Reported from what actually happened, never from what was intended. Two claims here were
    // simply false: `"{uid} is done"` after a status this transaction deliberately left as
    // `cancelled`, and "removed session and its branch" in the arm that only ran
    // `git worktree prune` because somebody else had already removed the directory.
    let status = kept_status.as_deref().unwrap_or("done");
    if json {
        println!(
            "{}",
            serde_json::json!({
                "uid": landed.uid, "landed": true, "branch": landed.branch, "onto": landed.onto,
                "commits": landed.ahead, "gate": landed.gate, "gate_source": landed.gate_source,
                "session_removed": cleaned, "status": status,
                "branch_deleted": cleaned && !disposed_already,
            })
        );
    } else {
        println!(
            "landed: {} → {} ({} commit(s)); {} is {status}",
            landed.branch, landed.onto, landed.ahead, landed.uid
        );
        if cleaned && disposed_already {
            println!("  session was already gone; pruned its registration");
        } else if cleaned {
            println!("  removed session {} and its branch", sess.name);
        }
    }
    Ok(())
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
    let base_registered = gitrepo::worktrees(&ctx.root)?
        .iter()
        .any(|w| session::same_path(&w.path, &base));
    if base_registered {
        // It exists and holds some other branch (or none) — if it held `onto`, the lookup
        // above would have found it. Reuse it: it is a cache, and switching keeps its
        // build artifacts.
        anyhow::ensure!(
            !gitrepo::is_dirty(&base)?,
            "{} has uncommitted changes — it is jkb's own scratch checkout, so commit or \
             discard them, or remove it with `git worktree remove --force`",
            base.display()
        );
        gitrepo::switch_to(&base, onto)?;
        return Ok(base);
    }
    anyhow::ensure!(
        !base.exists(),
        "{} exists but git does not know it as a worktree — remove it, or run \
         `git worktree prune`",
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

/// `task abandon` — drop a session without landing it (design D36.6).
fn cmd_task_abandon(
    db: &Db,
    uid: &str,
    force: bool,
    delete_branch: bool,
    json: bool,
) -> Result<()> {
    let ctx = repo::repo_ctx()?;
    let id = resolve_task_uid(db, uid)?;
    // Abandon does two separable things: it **disposes of the session**, and it **reopens the
    // task**. Only the second is wrong for a terminal task — a landed one is already merged
    // and a cancelled one was deliberately dropped, so putting either back on the ready
    // frontier (still tagged with its branch, re-dispatchable to the swarm) is the harm.
    //
    // Refusing outright was the first fix, and it stranded the session instead: `task set
    // --status cancelled` leaves the worktree, branch and claim in place, no other verb
    // removes them, and the workaround the refusal suggested — reopen, then abandon — caused
    // exactly the reopening it was guarding against. So the cleanup runs and the status is
    // left alone. The decision is taken inside the transaction below, against the status as
    // it is *then* — not against a snapshot from before a worktree removal that can take
    // long enough for a concurrent land to finish.
    let tags = repo::task_tags(db, id)?;
    let sess = session_for(&ctx, &tags)?;
    // Prefer the branch that actually has a worktree; fall back to the recorded one so a
    // session whose checkout was deleted by hand can still be cleaned up in the KB.
    let branch = sess
        .as_ref()
        .map(|s| s.branch.clone())
        .or_else(|| repo::facet_one(&tags, repo::FACET_BRANCH).cloned())
        .with_context(|| format!("{uid} has no session"))?;

    // Abandoning is for **this** session's work. Since the swarm now tags its tasks with
    // `branch=`/`onto=` so they appear in the same views (D38), a task another IMPLEMENTER is
    // actively building is one right-click away — and `claim::clear` has no owner CAS, so it
    // would free a live claim and let the next SCHEDULER pass dispatch a second builder while
    // the first keeps going. Refuse a claim this session does not hold unless forced.
    let held = current_claim(db, id)?;
    if let Some(owner) = &held {
        let mine = sess.as_ref().is_some_and(|s| {
            owner::session_worktree(owner).is_some_and(|w| session::same_path(&w, &s.worktree))
        });
        // **Liveness**, the same rule `task work` and `task start` follow (D27.1/D36.6) —
        // not owner-string identity. Judging by name refused a claim left behind by a crashed
        // implementer or a session whose worktree was deleted by hand, so the one command that
        // exists to clean a session up was blocked by the wreckage it was there to remove, and
        // pointed the user at `jkb task release` for an owner that provably no longer exists.
        // What must be protected is work someone is *still doing*.
        if !mine && owner::is_alive(owner) && !force {
            anyhow::bail!(
                "{uid} is claimed by {owner}, which is still alive — abandoning it would free \
                 a claim someone is working under. Finish or abandon that work, use `jkb task \
                 release {uid} --owner {owner}` if you are sure it is gone, or pass --force."
            );
        }
    }

    if let Some(sess) = &sess {
        if !force {
            anyhow::ensure!(
                !gitrepo::is_dirty(&sess.worktree)?,
                "{} has uncommitted changes — commit them, or pass --force to discard them",
                sess.worktree.display()
            );
        }
        gitrepo::worktree_remove(&ctx.root, &sess.worktree, force)?;
    }
    if delete_branch && gitrepo::has_branch(&ctx.root, &branch)? {
        gitrepo::delete_branch(&ctx.root, &branch, true)?;
    }
    // Release, and reopen unless the task is already finished.
    //
    // `onto=` is cleared **only** when the task is reopened, which is exactly when it stops
    // being true: an abandoned task is no longer landing on that batch, and leaving the facet
    // made it keep rendering as live `implementing` work — which in turn kept its staging
    // branch classified unmerged and offered as a land target long after the batch was spent
    // (D36.3). For a task that stays `done` or `cancelled` the facet is history: it records
    // which batch the work went to (or was dropped from), and the In Flight view reads it.
    //
    // The status is re-read inside the transaction, so a task that finished while this
    // command was removing a worktree is not reopened by a decision taken before that.
    // Reported from what the transaction actually did, not from the snapshot taken before the
    // worktree removal: a task that finished while this command was running was correctly
    // left alone, and then announced as "open again" with `"reopened": true`, which the
    // extension believes.
    let observed = held.clone();
    let (reopened, final_status) = db.write_txn("cli", move |conn, meta| {
        // Only the claim judged above, and nothing at all when there was none. `held` was read
        // before two git subprocesses (`worktree remove`, `delete-branch`) — a far wider window
        // than the `ps` fork that motivated `clear_if` — so a claim taken in the meantime
        // belongs to a worker whose claim this command never looked at. An unconditional clear
        // freed a task the next SCHEDULER pass had just handed to an implementer, and then
        // reopened it, which is how two builders end up on one task.
        // Also covers the case the guard above missed: no claim was observed before the git
        // subprocesses, but one exists now. `set_status(Open)` is non-terminal so `task.rs:446`
        // does not fire and the new owner's claim survives either way — the harm is a cleared
        // `onto=` and a `"reopened": true` that is not true, rather than two builders on one
        // task, since `ready` requires `claimant_id IS NULL`.
        if observed.is_none() && claim::claimed(conn)?.iter().any(|c| c.id == id) {
            let current = item::get(conn, id)?
                .and_then(|m| m.status)
                .unwrap_or_default();
            return Ok((false, current));
        }
        if let Some(prev) = &observed {
            if !claim::clear_if(conn, meta, id, prev)? {
                // The claim changed hands while the worktrees were being removed. Whoever
                // holds it now was never judged by this command, so reopening the task and
                // clearing `onto=` would take work off a live worker — the same reasoning
                // that makes the clear itself a CAS. Report what is true and change nothing.
                let current = item::get(conn, id)?
                    .and_then(|m| m.status)
                    .unwrap_or_default();
                return Ok((false, current));
            }
        }
        let current = item::get(conn, id)?
            .and_then(|m| m.status)
            .unwrap_or_default();
        if jkb_types::TaskStatus::is_terminal_str(Some(current.as_str())) {
            return Ok((false, current));
        }
        repo::clear_facet(conn, meta, id, repo::FACET_ONTO)?;
        task::set_status(conn, meta, id, jkb_types::TaskStatus::Open)?;
        Ok((true, "open".to_owned()))
    })?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "uid": uid, "abandoned": true, "branch": branch, "reopened": reopened,
                "status": final_status,
                "worktree_removed": sess.is_some(), "branch_deleted": delete_branch,
            })
        );
    } else if !reopened {
        println!("abandoned the session for {uid}; it stays {final_status}");
        if !delete_branch && gitrepo::has_branch(&ctx.root, &branch)? {
            println!("  branch {branch} kept — delete it with `git branch -D {branch}`");
        }
    } else {
        println!("abandoned {uid}; it is open again");
        if !delete_branch && gitrepo::has_branch(&ctx.root, &branch)? {
            println!("  branch {branch} kept — delete it with `git branch -D {branch}`");
        }
    }
    Ok(())
}

/// `task sessions` — what is in flight in this repo.
fn cmd_task_sessions(db: &Db, json: bool) -> Result<()> {
    let ctx = repo::repo_ctx()?;
    let sessions = session::discover(&ctx.root)?;
    let by_branch = repo::tasks_by_branch(db, &ctx.key)?;

    let mut rows = Vec::new();
    for s in &sessions {
        let task = by_branch.get(&s.branch);
        let onto = task.and_then(|t| t.onto.clone());
        let ahead = match &onto {
            Some(o) => gitrepo::ahead_count(&ctx.root, o, &s.branch)?,
            None => 0,
        };
        // Deliberately no "attended" flag: nothing here can observe whether anyone is sitting
        // in a session. The owner's pid belongs to the one-second `jkb task work` process, so
        // a flag built on it reads "unattended" for the session you are working in and tells
        // you to abandon it. What IS observable — uncommitted work, commits ahead — is
        // reported instead (design D36.6).
        rows.push(serde_json::json!({
            "session": s.name,
            "worktree": s.worktree,
            "branch": s.branch,
            "onto": onto,
            "uid": task.map(|t| t.uid.clone()),
            "status": task.map(|t| t.status.clone()),
            "dirty": gitrepo::is_dirty(&s.worktree)?,
            "commits": ahead,
        }));
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        println!("(no sessions in {})", ctx.key);
    } else {
        for r in &rows {
            let dirty = if r["dirty"].as_bool().unwrap_or(false) {
                " [uncommitted]"
            } else {
                ""
            };
            println!(
                "{:<28} {} → {}  {} commit(s){dirty}",
                r["session"].as_str().unwrap_or("?"),
                r["branch"].as_str().unwrap_or("?"),
                r["onto"].as_str().unwrap_or("?"),
                r["commits"],
            );
            if let Some(uid) = r["uid"].as_str() {
                println!("  {uid} ({})", r["status"].as_str().unwrap_or("?"));
            }
        }
    }
    Ok(())
}

/// `task gate` — show, set, or clear the command that verifies a landing here (D36.5).
fn cmd_task_gate(db: &Db, cmd: Option<&str>, clear: bool, json: bool) -> Result<()> {
    let ctx = repo::repo_ctx()?;
    if clear {
        session::set_gate(db, &ctx.key, None)?;
    } else if let Some(cmd) = cmd {
        session::set_gate(db, &ctx.key, Some(cmd))?;
    }
    let stored = session::stored_gate(db, &ctx.key)?;
    let detected = if stored.is_none() {
        session::autodetect_gate(&ctx.root)
    } else {
        None
    };
    if json {
        println!(
            "{}",
            serde_json::json!({"repo": ctx.key, "gate": stored, "would_detect": detected})
        );
    } else {
        match (&stored, &detected) {
            (Some(g), _) => println!("gate for {}: {g}", ctx.key),
            (None, Some(d)) => println!("gate for {}: (none stored; would use {d})", ctx.key),
            (None, None) => println!(
                "gate for {}: (none — landings here are UNVERIFIED; set one with \
                 `jkb task gate '<cmd>'`)",
                ctx.key
            ),
        }
    }
    Ok(())
}

/// The `doctor` line for this repo's task sessions. Best-effort: `doctor` is often run
/// outside a git repo entirely, and that is not a fault to report.
///
/// Every session is listed, not just some subset flagged as neglected — nothing here can tell
/// a session you are working in from one you walked away from (design D36.6), and a report
/// that guesses would tell you to abandon the work you are doing.
fn report_sessions(db: &Db) {
    let Ok(ctx) = repo::repo_ctx() else { return };
    let Ok(sessions) = session::discover(&ctx.root) else {
        return;
    };
    if sessions.is_empty() {
        return;
    }
    let by_branch = repo::tasks_by_branch(db, &ctx.key).unwrap_or_default();
    println!("task sessions: {} in flight", sessions.len());
    for s in &sessions {
        let uid = by_branch
            .get(&s.branch)
            .map_or("(no task)", |t| t.uid.as_str());
        println!(
            "  {} — {uid}: resume with `cd {}`, land it with `jkb task land {uid}`, or drop \
             it with `jkb task abandon {uid}`",
            s.name,
            s.worktree.display()
        );
    }
}

/// Mark `id` done, unless it finished on its own since the caller last looked. Returns whether
/// the status was written.
///
/// The status is re-read **inside** the transaction. `close-merged` snapshots every candidate
/// up front and then runs several git subprocesses per task, and it runs from a post-merge
/// hook over all of them at once — long enough for a `jkb task set --status cancelled` to land
/// in between and be silently overwritten with `done`. Same reasoning as `settle_landing` and
/// `review::record`.
fn close_if_still_open(db: &Db, id: ItemId) -> Result<bool> {
    Ok(db.write_txn("cli", move |conn, meta| {
        let current = item::get(conn, id)?.and_then(|m| m.status);
        if jkb_types::TaskStatus::is_terminal_str(current.as_deref()) {
            return Ok(false);
        }
        task::set_status(conn, meta, id, jkb_types::TaskStatus::Done)?;
        Ok(true)
    })?)
}

/// Which of `branches` do not exist here at all — asked **per branch**, with the same `Prefer`
/// `close-merged` uses everywhere else.
///
/// Both arms that report goneness call this. They each had their own loop for one commit, which is
/// how one of them came to interpolate the joined branch list into the message and call a live
/// branch gone alongside a deleted one. A local-only `refs/heads/` probe is also wrong here: a
/// branch living solely on the remote is the ordinary state after a merged PR deletes the local
/// copy.
fn gone_branches(cwd: &Path, branches: &[String]) -> Result<Vec<String>> {
    let mut gone = Vec::new();
    for b in branches {
        if gitrepo::branch_ref(cwd, b, gitrepo::Prefer::Remote)?.is_none() {
            gone.push(b.clone());
        }
    }
    Ok(gone)
}

/// The merge state of a task, given **every** branch it records: anything short of `Merged`
/// wins, so one unmerged branch holds the task whatever order they come back in.
///
/// Taking one branch — the lexicographically smallest, as `tag::applications` orders them — meant
/// a task with `a-merged` and `z-live` closed while `z-live` was still in flight.
fn merged_state_of_all(
    cwd: &Path,
    branches: &[String],
    trunk_ref: &str,
    tags: &BTreeMap<String, Vec<String>>,
    warned_fallback: &mut bool,
) -> Result<gitrepo::MergeState> {
    let mut state = gitrepo::MergeState::Merged;
    for b in branches {
        // `landed_for_action`, not `is_merged`: closing a task is acting on the answer, so a
        // branch with no recorded base must not count as landed. The remote copy answers "did
        // this work ship?" — after a merged PR the local branch is usually stale or already
        // deleted (design D34.2).
        let (s, fell_back) =
            repo::landed_for_action(cwd, b, trunk_ref, tags, gitrepo::Prefer::Remote)?;
        if fell_back && !*warned_fallback {
            *warned_fallback = true;
            eprintln!(
                "warning: this git lacks `merge-tree --write-tree` (needs 2.38+), so \
                 squash-merged branches will read as unmerged"
            );
        }
        if !matches!(s, gitrepo::MergeState::Merged) {
            return Ok(s);
        }
        state = s;
    }
    Ok(state)
}

/// `task close-merged` — close tasks whose branch has landed (design D34.4).
/// The uid, status and facets `close-merged` needs for one task.
type CloseMergedRow = (String, Option<String>, BTreeMap<String, Vec<String>>);

/// Read one task's uid, status and facet tags in a single database round-trip.
///
/// The whole multi-map, not a hand-picked facet or two. The previous reader collected `branch=`
/// with `tags.iter().find(...)`, and `tag::applications` is `ORDER BY facet, value`, so it
/// silently took the lexicographically smallest branch: with `a-merged` and `z-live` recorded,
/// the task closed while `z-live` was still in flight.
fn close_merged_row(db: &Db, id: ItemId) -> Result<CloseMergedRow> {
    Ok(db
        .read(move |conn| {
            let meta = item::get(conn, id)?;
            let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (facet, value) in tag::applications(conn, id)? {
                grouped.entry(facet).or_default().push(value);
            }
            Ok(meta.map(|m| (m.uid, m.status, grouped)))
        })?
        .unwrap_or_default())
}

fn cmd_task_close_merged(
    db: &Db,
    repo: Option<String>,
    trunk: Option<String>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    // The main copy's key, for the same reason `task start` uses it: run from a session
    // worktree, `key(&cwd)` is the session's name and this silently matches no tasks at all.
    let repo = match repo {
        Some(r) => r,
        None => {
            repo::repo_ctx()
                .context("not inside a git repo — pass --repo, or run this from the repo")?
                .key
        }
    };
    // `--repo` selects which tasks to consider; every git question below is still asked of the
    // repository we are standing in. Those must be the same place or the command probes one repo
    // about another's branches — reporting live work as gone and advising its tag be deleted. It
    // is a filter, not a redirect, so a mismatch is refused rather than guessed at.
    if let Ok(here) = repo::repo_ctx() {
        anyhow::ensure!(
            here.key == repo,
            "--repo {repo} does not match the repository here ({}), and the branches would be \
             looked up in this one. Run it from {repo}'s checkout.",
            here.key
        );
    }
    let trunk_ref = match trunk {
        Some(t) => t,
        None => gitrepo::trunk(&cwd)?.context(
            "could not determine this repo's trunk (no origin/HEAD and no main/master/trunk) \
             — pass --trunk",
        )?,
    };
    // Checked where the flag is accepted, not only where it is eventually used: with no candidate
    // task the probe never runs, so an unusable `--trunk` was silently accepted and the run
    // reported "nothing to close" as though it had asked.
    gitrepo::valid_ref(&trunk_ref)?;

    // Every open task tagged for this repo that names a branch. Typed, not interpolated into
    // the DSL: `--repo` is user-typed and a value with whitespace would re-tokenize into a
    // different query that matches nothing, closing no task and reporting no error.
    let query = repo::tasks_in_repo(&repo);
    let ids = db.read(move |conn| query.evaluate(conn))?;

    let mut closed = Vec::new();
    let mut blocked = Vec::new();
    let mut pending = Vec::new();
    let mut undecidable = Vec::new();
    let mut warned_fallback = false;

    for id in ids {
        let (uid, status, tags) = close_merged_row(db, id)?;
        let branches = repo::facet_values(&tags, repo::FACET_BRANCH).to_vec();
        if branches.is_empty() || jkb_types::TaskStatus::is_terminal_str(status.as_deref()) {
            continue;
        }
        let branch = branches.join(", ");

        let state = merged_state_of_all(&cwd, &branches, &trunk_ref, &tags, &mut warned_fallback)?;
        match state {
            gitrepo::MergeState::Merged => {
                // A merged branch is evidence, not proof: a task with unfinished subtasks
                // did not finish, whatever landed (design D34.4).
                if db.read(move |conn| task::subtasks_all_terminal(conn, id))? {
                    let wrote = dry_run || close_if_still_open(db, id)?;
                    if wrote {
                        closed.push((uid, branch));
                    } else {
                        blocked.push((uid, format!("{branch} (finished while checking)")));
                    }
                } else {
                    blocked.push((uid, branch));
                }
            }
            // Not merged — but "still working on it" and "we declined to decide" are different
            // answers and only one of them has a remedy. A branch with no cut point we can use
            // (never recorded, removed by `task tag rm`, dropped as an unattributable legacy
            // value, or present but unresolvable here) can *never* close, and reported as "in
            // flight" it looks exactly like one that simply is. `BranchMissing` below has named
            // its own way out since D34; this did not.
            //
            // Asked of the **branches**, not of the collapsed state, so the label does not depend
            // on which branch answered first. `merged_state_of_all` returns the first non-`Merged`
            // state it meets and `tag::applications` orders by value, so a task with one unmerged
            // branch and one unusable cut point would otherwise be labelled by whichever sorted
            // lower. The decision to hold was never in doubt either way; the explanation was.
            gitrepo::MergeState::Unmerged | gitrepo::MergeState::NothingToMerge => {
                let mut all_usable = true;
                for b in &branches {
                    all_usable &= repo::base_is_usable(&cwd, base::resolve(&tags, b))?;
                }
                let gone = gone_branches(&cwd, &branches)?;
                // A vanished branch keeps its own message. Hoisting the base check above
                // `is_merged` meant `landed_with_base` short-circuits before branch existence is
                // ever probed, so a task that was BOTH gone and missing a cut point stopped
                // reporting `BranchMissing` and told the user to record a cut point — advice that
                // changes nothing except which problem the next run names.
                if !gone.is_empty() {
                    blocked.push((
                        uid,
                        format!(
                            "{} gone — `jkb task tag rm <uid> branch=<name>` if stale",
                            gone.join(", ")
                        ),
                    ));
                } else if all_usable {
                    pending.push((uid, branch));
                } else {
                    undecidable.push((uid, branch));
                }
            }
            // A missing branch is ambiguous — merged-and-deleted, or a typo — so it HOLDS.
            //
            // Decided explicitly, because the two readers of this state disagree on purpose:
            // `review::work_is_in` counts `BranchMissing` as *covered*, this counts it as
            // *blocked*. Auto-closing on ambiguity is the one thing this verb must not do; the
            // cost is that a stale recorded branch holds the task, so the message names the way
            // out rather than leaving the user to find it.
            // Name only the branches that are actually missing. `merged_state_of_all` short-
            // circuits on the first non-`Merged` state and `tag::applications` orders by value, so
            // a task recording a deleted branch and a live one answered `BranchMissing` from the
            // first and never probed the second — then printed both as gone, telling the user to
            // delete the tag that is the only record of the work still in flight. Same per-branch
            // question the arm above asks, for the same reason.
            gitrepo::MergeState::BranchMissing => blocked.push((
                uid,
                format!(
                    "{} gone — `jkb task tag rm <uid> branch=<name>` if stale",
                    gone_branches(&cwd, &branches)?.join(", ")
                ),
            )),
            gitrepo::MergeState::NoTrunk => {
                anyhow::bail!("trunk `{trunk_ref}` does not resolve in this repo")
            }
        }
    }

    report_close_merged(
        &CloseMergedReport {
            repo: &repo,
            trunk_ref: &trunk_ref,
            dry_run,
            closed: &closed,
            blocked: &blocked,
            pending: &pending,
            undecidable: &undecidable,
        },
        json,
    )
}

/// What one `close-merged` run decided, split by what the user can do about it.
struct CloseMergedReport<'a> {
    repo: &'a str,
    trunk_ref: &'a str,
    dry_run: bool,
    /// Marked done (or would be, under `--dry-run`).
    closed: &'a [(String, String)],
    /// Merged, but something else holds them — usually open subtasks.
    blocked: &'a [(String, String)],
    /// Genuinely still in flight. Counted, not listed: this is the ordinary case and naming
    /// every open task on every run buries the two buckets that need a decision.
    pending: &'a [(String, String)],
    /// We declined to decide, because there is no usable cut point. Listed **individually** even
    /// though it is a form of "not closed": unlike `pending` it will never resolve on its own,
    /// and it has a remedy the user cannot guess.
    undecidable: &'a [(String, String)],
}

/// Print what a `close-merged` run decided.
fn report_close_merged(r: &CloseMergedReport<'_>, json: bool) -> Result<()> {
    let rows = |v: &[(String, String)]| {
        v.iter()
            .map(|(u, b)| serde_json::json!({"uid": u, "branch": b}))
            .collect::<Vec<_>>()
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "repo": r.repo,
                "trunk": r.trunk_ref,
                "dry_run": r.dry_run,
                "closed": rows(r.closed),
                "blocked": rows(r.blocked),
                "pending": rows(r.pending),
                "undecidable": rows(r.undecidable),
            }))?
        );
        return Ok(());
    }
    let verb = if r.dry_run { "would close" } else { "closed" };
    for (uid, branch) in r.closed {
        println!("{verb} {uid} ({branch} merged)");
    }
    for (uid, branch) in r.blocked {
        println!("held  {uid} ({branch}) — `jkb task show {uid}` says why (usually open subtasks)");
    }
    for (uid, branch) in r.undecidable {
        println!(
            "unknown {uid} ({branch}) — no usable cut point, so whether it landed cannot be \
             decided: `jkb task base {uid} <branch> <sha>`"
        );
    }
    // Independent of the other buckets. Gated on them, the count vanished in exactly the runs
    // where something else printed — so a run showing two `unknown` lines silently stopped
    // accounting for the tasks that are simply still being worked on.
    if !r.pending.is_empty() {
        println!("{} task(s) still in flight", r.pending.len());
    }
    if r.closed.is_empty()
        && r.blocked.is_empty()
        && r.undecidable.is_empty()
        && r.pending.is_empty()
    {
        println!("nothing to close");
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

fn cmd_doctor(db: &Db, db_path: &Path, backup: Option<&Path>, fix: bool) -> Result<()> {
    // FIRST, before any diagnostic and before `--fix` mutates anything. `--backup` is the
    // safety copy you take *before* a repair; taken at the end it held post-repair state, so
    // `jkb doctor --backup ~/pre-fix.db --fix` produced a file that was the opposite of what its
    // name and its help said.
    if let Some(dest) = backup {
        db.backup(dest)?;
        println!("backup written to {}", dest.display());
    }

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

    report_vector_index(db, fix)?;

    // Task sessions in this repo (design D36.6). A session's worktree keeps its claim on
    // purpose — the half-written branch is still there — so a session is never reported as
    // orphaned. Doctor lists every one, because nothing observable distinguishes a session
    // you are working in from one you walked away from.
    report_sessions(db);

    // Cloud-sync-folder warning (design D23).
    match jkb_core::cloud_sync_warning(db_path) {
        Some(w) => println!("warning: {w}"),
        None => println!("db location: ok ({})", db_path.display()),
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

/// The first line of a body, for a one-line report. The derivation is `output::first_nonblank`
/// (the one copy); only the width is this function's own.
fn first_line(content: &str) -> String {
    truncate(output::first_nonblank(content), 100)
}

/// `jkb staging ls` — the staging branches in this repo and what is landing on each.
///
/// The one read behind both the explorer's branch picker and its In Flight view (design
/// D38.2), so the two cannot disagree about what is live.
fn cmd_staging_ls(db: &Db, all: bool, json: bool) -> Result<()> {
    let ctx = repo::repo_ctx()?;
    let rows = staging::collect(db, &ctx, all)?;

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
                        "dirty": t.dirty,
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
            if t.dirty {
                notes.push("uncommitted".to_owned());
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

/// `jkb task review record` — record that a review ran against a branch (design D38.4).
fn cmd_task_review(db: &Db, cmd: TaskReviewCmd, json: bool) -> Result<()> {
    let TaskReviewCmd::Record {
        branch,
        sha,
        findings,
    } = cmd;
    let ctx = repo::repo_ctx()?;
    let cwd = std::env::current_dir()?;
    let branch = match branch {
        Some(b) => b,
        None => gitrepo::current_branch(&cwd)?
            .context("not on a branch here (detached HEAD?) — pass --branch")?,
    };
    let sha = match sha {
        Some(s) => Some(s),
        None => gitrepo::rev(&ctx.root, &branch)?,
    };

    // Refuse a findings namespace that holds nothing, here, where the caller can still fix it.
    // A review recorded against an empty namespace is a review whose findings never reached
    // the KB — a quarantined `tasks.md`, a typo, a namespace renamed since — and the land gate
    // must never read that as a clean review. It is caught at both ends deliberately: this is
    // the actionable moment, the gate is the one that must not be bypassed.
    let found = review::findings_in(db, std::slice::from_ref(&findings))?;
    anyhow::ensure!(
        found.total > 0,
        "no findings found under `{findings}` — nothing was recorded. A review whose findings \
         never reached the KB must not be recorded as one: check the namespace exists \
         (`jkb ls {findings}`), that `jkb sync` imported the review's tasks.md, and that it \
         was not quarantined (`jkb doctor`)."
    );

    let review::Recording {
        recorded,
        skipped_unlanded,
        skipped_no_base,
    } = review::record(db, &ctx.root, &ctx.key, &branch, sha.as_deref(), &findings)?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "branch": branch,
                "sha": sha,
                "findings": findings,
                "tasks": recorded.iter().map(|r| serde_json::json!({
                    "uid": r.uid, "moved_to_review": r.moved_to_review,
                })).collect::<Vec<_>>(),
                "skipped_unlanded": skipped_unlanded,
                "skipped_no_base": skipped_no_base,
            })
        );
        return Ok(());
    }
    if recorded.is_empty() {
        // Reviewing an arbitrary range is a legitimate thing to do, so this is a note and
        // not an error (design D38.4). But "no task records this branch" and "tasks record it
        // and every one was skipped" are different facts, and printing the first while the
        // skipped list appears directly beneath it contradicted the very next line.
        if skipped_unlanded.is_empty() && skipped_no_base.is_empty() {
            println!("no task records branch={branch} — nothing to tag (review still filed)");
        } else {
            println!("nothing tagged for branch={branch} — every matching task was skipped, below (review still filed)");
        }
    } else {
        println!(
            "recorded review of {branch}@{} -> {findings}",
            sha.as_deref().unwrap_or("unknown")
        );
        for r in &recorded {
            let moved = if r.moved_to_review {
                " (now needs_review)"
            } else {
                ""
            };
            println!("  {}{moved}", r.uid);
        }
    }
    // Said out loud, because a task landing on this branch whose work is not in it yet has
    // NOT been reviewed, and silence would read as "everything was tagged".
    if !skipped_no_base.is_empty() {
        println!(
            "not tagged — no cut point recorded for their work branch, so whether this review \
             saw them cannot be decided (run `jkb task start`, or `jkb task base <uid> <branch> \
             <sha>`):"
        );
        for uid in &skipped_no_base {
            println!("  {uid}");
        }
    }
    if !skipped_unlanded.is_empty() {
        println!(
            "not tagged — landing on {branch} but not merged into it yet, so this review did \
             not see them:"
        );
        for uid in &skipped_unlanded {
            println!("  {uid}");
        }
        println!("  review each in its own session (`/review-log` there), or land first.");
    }
    Ok(())
}

/// Report — and with `--fix`, sweep — derived-index rows whose item is gone.
///
/// A `vec0` virtual table cannot carry a foreign key to `items`, so a deleted item leaves its
/// vector behind. Since D40 (`items.id AUTOINCREMENT`) that row is **stale, not dangerous** —
/// the freed id is never reissued, so no new item can inherit its embedding — which is why
/// this reads as housekeeping rather than corruption, and why nothing sweeps implicitly.
fn report_vector_index(db: &Db, fix: bool) -> Result<()> {
    // What counts as one of our derived-index tables, and what counts as stale in one, are
    // `jkb-index`'s to say — the CLI asks. It used to carry its own copy of both queries,
    // including the `vec0` shadow-table filter that is the non-obvious part, so `doctor`'s
    // report and `doctor --fix`'s delete were two statements that had to be kept in step.
    let tables =
        db.read_with::<Vec<String>, anyhow::Error, _>(|conn| Ok(jkb_index::vector_tables(conn)?))?;
    if tables.is_empty() {
        println!("vector index: no vector table yet");
    } else if fix {
        println!(
            "vector index: removed {} stale row(s)",
            sweep_stale(db)?.vectors
        );
    } else {
        let stale =
            db.read_with::<_, anyhow::Error, _>(|conn| Ok(jkb_index::count_stale(conn)?))?;
        if stale.is_empty() {
            println!("vector index: ok");
        } else {
            println!(
                "vector index: {} stale row(s) whose item is gone",
                stale.vectors
            );
            println!("  run `jkb index --sweep` (or `jkb doctor --fix`) to remove them");
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
