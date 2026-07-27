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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use jkb_core::query::{Query, Scope};
use jkb_core::{binding, claim, edge, item, mount, ns, placement, tag, task, undo, view, Db};
use jkb_embed::{OllamaConfig, OllamaEmbedder};
use jkb_ingest::Pipeline;
use jkb_search::{Route, Searcher};
use jkb_types::{ConflictPolicy, EdgeType, Embedder, ItemId, PlacementRole, SyncMode};

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
        /// Include terminal (`done`/`cancelled`) tasks (hidden by default).
        #[arg(long)]
        all: bool,
    },
    /// Inspect items (generic, kind-aware).
    Item {
        #[command(subcommand)]
        cmd: ItemCmd,
    },
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
        Command::Ls { path, all } => cmd_ls(&db, path.as_deref(), all, json),
        Command::Item { cmd } => match cmd {
            ItemCmd::Show { uid, preview } => cmd_item_show(&db, &uid, preview, json),
            ItemCmd::Edit {
                uid,
                text,
                stdin,
                append,
            } => cmd_item_edit(&db, &uid, &text, stdin, append, json),
        },
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
        let base = if global {
            "tasks".to_owned()
        } else {
            match ambient_repo(db)? {
                Some(repo) => format!("tasks/{repo}"),
                None => "tasks".to_owned(),
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
            arr.push(serde_json::json!({
                "item": hit.item.get(),
                "route": hit.route.as_str(),
                "score": hit.score,
                "distance": hit.distance,
                "namespace": hit.namespace_path,
                "source_document": hit.source_document.map(ItemId::get),
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
    /// lead to real content.
    leaf_count: Option<i64>,
}

impl Child {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind,
            "ref": self.reference,
            "label": self.label,
            "has_children": self.has_children,
            "status": self.status,
            "priority": self.priority,
            "leaf_count": self.leaf_count,
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
        // Any placement role, so `tasks/…` mirror namespaces (Reference placements) count
        // as having children — the symbolic-link view of tasks homed elsewhere.
        let has_items = !placement::items_in(conn, ns_id, None)?.is_empty();
        let leaf_count = leaf_counts.get(&ns_id).copied().unwrap_or(0);
        out.push(Child {
            kind: "namespace".to_owned(),
            reference: ns_path,
            label,
            has_children: has_sub || has_items,
            status: None,
            priority: None,
            leaf_count: Some(leaf_count),
        });
    }

    if let Some(p) = path {
        if let Some(ns_id) = ns::get(conn, p)? {
            // Any placement role: a `tasks/…` mirror surfaces the task even though its
            // primary home is elsewhere (the symbolic-link view).
            for item_id in placement::items_in(conn, ns_id, None)? {
                let Some(meta) = item::get(conn, item_id)? else {
                    continue;
                };
                // Hide any terminal-status item (done/cancelled) unless `all` — like
                // ignored files, revealed only on explicit toggle.
                let terminal = matches!(meta.status.as_deref(), Some("done" | "cancelled"));
                if !all && terminal {
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
                });
            }
        }
    }
    out.sort_by_key(Child::sort_key);
    Ok(out)
}

/// `jkb ls [path]` — the direct children of a namespace, for lazy tree expansion.
fn cmd_ls(db: &Db, path: Option<&str>, all: bool, json: bool) -> Result<()> {
    let owned = path.map(str::to_owned);
    let children = db.read(move |conn| list_children(conn, owned.as_deref(), all))?;
    if json {
        let v = serde_json::json!({
            "path": path,
            "children": children.iter().map(Child::to_json).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if children.is_empty() {
        println!("(empty)");
    } else {
        for c in &children {
            let arrow = if c.has_children { "▸" } else { " " };
            let status = c
                .status
                .as_deref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            println!("{arrow} {:<10} {}{status}", c.kind, c.label);
        }
    }
    Ok(())
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
        Conflict, Created, Exported, Imported, Merged, Quarantined, Skipped, UpToDate,
    };
    let report = jkb_sync::sync(db, ns_path)?;
    println!(
        "sync {ns_path}: {} created, {} imported, {} exported, {} merged, {} conflicts, \
         {} quarantined, {} up-to-date, {} skipped",
        report.count(Created),
        report.count(Imported),
        report.count(Exported),
        report.count(Merged),
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
        match ambient_repo(db)? {
            Some(repo) => spec.home = format!("tasks/{repo}/.backlog"),
            None if confirm_global_backlog()? => spec.home = String::from("tasks/.backlog"),
            None => anyhow::bail!(
                "--backlog needs an ambient repo; run inside a mounted repo or use `+<ns>`"
            ),
        }
    } else if let Some(repo) = ambient_repo(db)? {
        // Inside a repo with no target: home at the per-repo inbox, mirrored into
        // the global inbox so it stays a complete capture view (D26.3).
        spec.home = format!("tasks/{repo}/inbox");
        spec.mirrors = vec![task::DEFAULT_HOME.to_owned()];
    }
    // else: outside a repo with no target → home stays `tasks/inbox` (DEFAULT_HOME).
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
    // A file-backed task is a single line in its source file: the tasks serializer renders
    // the item's `content` verbatim as the checkbox line (`render_task`). Multi-line content
    // — always the case for `--append`, and for any replacement containing a newline — would
    // split that line on sync, detaching the trailing `^id` and losing the task's identity.
    // Refuse it and point at the source-file flow the design gate already prescribes.
    let file_backed = uid.starts_with("file://");
    if file_backed && (append || new_text.contains('\n')) {
        anyhow::bail!(
            "`{uid}` is a file-backed task; its source line is single-line, so `--append` \
             or multi-line content would corrupt it on sync. Edit the source file directly \
             (add indented notes beneath its `^id` line), then run `jkb sync`."
        );
    }
    db.write_txn("cli", move |conn, meta| {
        let content = if append {
            match item::get_content(conn, id)? {
                Some(existing) if !existing.is_empty() => format!("{existing}\n\n{new_text}"),
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
    let embedded = pipeline.index_pending(db)?;
    println!("index: embedded {embedded} item(s)");
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
