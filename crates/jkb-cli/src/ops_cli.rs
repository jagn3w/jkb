//! The commands served as typed operations: the agent read set (tasks S6.1) — `query`, `find`,
//! `recent`, `search`, `ls`, `tree`, `grep`, `cat`, and `task next`/`show`/`subtasks` — and the
//! task-mutate set (S6.2), rendered in [`super::task_cli`].
//!
//! **Every op here goes through a [`Backend`], on the host too.** The host passes a
//! `LocalBackend` over its database and remote mode passes the daemon's, so the same op answers
//! both and this module only renders — `jkb ls` in the dev container and on the host cannot
//! disagree about what a namespace holds, because there is no second implementation to disagree.
//! The renderings are the ones these commands printed before they were ported; the UI parses the
//! `--json` shapes (D31), so a change to one is a change to that contract.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::{bail, Result};
use jkb_api::kb::{
    Child, GrepAnswer, GrepMode, ItemRow, QueryOrder, SearchRoute, TaskDetail, TreeNode,
};
use jkb_api::{ApiError, Backend, ErrorCode, Request, Response};

use super::{first_line, output, output_line, Command, TaskCmd};

/// Depth `jkb tree` descends by default before eliding deeper folders with `…` — deep enough to map
/// any real subtree, shallow enough to bound the output and the per-namespace query fan-out.
const DEFAULT_TREE_DEPTH: usize = 4;

/// Whether `command` is one of this module's — what remote mode's dispatch routes here. [`Ops::run`]
/// refuses anything else. Remote mode's support table names the same commands in its own exhaustive
/// match; `the_ported_reads_are_the_ones_ops_cli_handles` holds the two lists together.
#[must_use]
pub const fn handles(command: &Command) -> bool {
    match command {
        Command::Query { .. }
        | Command::Search { .. }
        | Command::Find { .. }
        | Command::Recent { .. }
        | Command::Ls { .. }
        | Command::Tree { .. }
        | Command::Grep { .. }
        | Command::Cat { .. }
        | Command::Ingest { .. } => true,
        Command::Task { cmd } => matches!(
            cmd,
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
        ),
        _ => false,
    }
}

/// How a read is served.
pub struct Ops<'a> {
    backend: &'a dyn Backend,
    global: bool,
    pub(crate) json: bool,
    /// Served by `jkb serve` rather than in this process: the daemon budgets reads and embeds no
    /// search text.
    pub(crate) remote: bool,
    /// Notices printed on stderr, kept so a test can see them.
    notices: std::cell::RefCell<Vec<String>>,
}

impl<'a> Ops<'a> {
    /// Serves through `backend`, which is the daemon's when `remote`.
    #[must_use]
    pub const fn new(backend: &'a dyn Backend, global: bool, json: bool, remote: bool) -> Self {
        Self {
            backend,
            global,
            json,
            remote,
            notices: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// The route `jkb search` takes when `--route` is not given: hybrid where this process embeds, FTS
    /// through the daemon, which does not.
    const fn default_route(&self) -> SearchRoute {
        if self.remote {
            SearchRoute::Fts
        } else {
            SearchRoute::Hybrid
        }
    }

    /// Run one of the commands [`handles`] names.
    ///
    /// # Errors
    /// The op's refusal, or a command this module does not handle.
    pub fn run(&self, command: Command) -> Result<()> {
        match command {
            Command::Query {
                terms,
                limit,
                count,
            } => self.query(&terms.join(" "), limit, count),
            Command::Search {
                terms,
                route,
                limit,
                context,
            } => self.search(
                &terms.join(" "),
                route.map_or(self.default_route(), Into::into),
                limit,
                context,
            ),
            Command::Find {
                path,
                kind,
                tags,
                status,
                limit,
            } => self.find(
                path.as_deref(),
                kind.as_deref(),
                &tags,
                status.as_deref(),
                limit,
            ),
            Command::Recent { path, limit } => self.recent(path.as_deref(), limit),
            Command::Ls {
                path,
                all,
                long,
                recursive,
                time,
            } => self.ls(
                path.as_deref(),
                LsOpts {
                    all,
                    long,
                    recursive,
                    time,
                },
            ),
            Command::Tree { path, all, depth } => self.tree(path.as_deref(), all, depth),
            Command::Grep {
                pattern,
                path,
                ignore_case,
                names_only,
                count,
            } => self.grep(&pattern, path.as_deref(), ignore_case, names_only, count),
            Command::Cat { uid } => self.cat(&uid),
            Command::Ingest { path, ns } => self.ingest(&path, ns.as_deref()),
            Command::Task { cmd } => match cmd {
                TaskCmd::Next { terms, limit } => self.task_next(&terms.join(" "), limit),
                TaskCmd::Show { uid } => self.task_show(&uid),
                TaskCmd::Subtasks { uid, all } => self.task_subtasks(&uid, all),
                cmd => super::task_cli::run(self, cmd),
            },
            _ => bail!("internal: a command the read set does not handle"),
        }
    }

    /// Every op goes through here, so a cut answer is reported here — once, for every command, rather
    /// than in each place an answer is taken apart, where one arm could forget.
    pub(crate) fn call(&self, request: Request) -> Result<Response> {
        let response = self
            .backend
            .call(request)
            .map_err(|e: ApiError| match e.code {
                // The daemon's body cap is its own; the host takes the same request whole.
                ErrorCode::TooLarge if self.remote => anyhow::anyhow!(
                    "{} — larger than the daemon accepts in one request; run it on the host",
                    e.message
                ),
                _ => anyhow::Error::msg(e.message),
            })?;
        // What cut it decides what lifts it: the daemon's byte budget is lifted on the host, which has
        // none; a tree's node cap is the same everywhere.
        let notice = match &response {
            Response::Tree {
                at_node_cap: true, ..
            } => Some(format!(
                "jkb: this tree stopped at {} nodes — root it at a path, or lower --depth",
                jkb_api::kb::MAX_TREE_NODES
            )),
            r if r.truncated() && self.remote => Some(
                "jkb: this answer was cut short at the daemon's read budget — narrow it, or run it on \
                 the host"
                    .to_owned(),
            ),
            r if r.truncated() => Some("jkb: this answer was cut short — narrow it".to_owned()),
            _ => None,
        };
        if let Some(notice) = notice {
            eprintln!("{notice}");
            self.notices.borrow_mut().push(notice);
        }
        Ok(response)
    }

    /// `jkb ingest`: read and parse the source here — in the dev container, the container's file or a
    /// page fetched through its firewall — and send the host only the text (`ingest.text`). In this
    /// host's own process the raw bytes go too, so the document is addressed and stored by them as a
    /// host ingest always was; they never cross the wire.
    fn ingest(&self, source: &str, ns: Option<&str>) -> Result<()> {
        let namespace = match ns {
            Some(n) => n.to_owned(),
            None => self.ambient()?.unwrap_or_else(|| "inbox".to_owned()),
        };
        let (raw, parsed) = jkb_ingest::read_source(source)?;
        let ingested = match self.call(Request::IngestText(jkb_api::ingest::IngestAsk {
            text: parsed.text,
            mime: parsed.mime,
            namespace,
            raw: (!self.remote).then_some(raw),
        }))? {
            Response::Ingested { ingested } => ingested,
            other => return unexpected("ingest.text", &other),
        };
        if self.json {
            let v = serde_json::json!({
                "document": ingested.document,
                "chunk_count": ingested.chunk_count,
                "embedded": ingested.embedded,
                "already_ingested": ingested.already_ingested,
                "warnings": ingested.warnings,
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
            return Ok(());
        }
        let state = if ingested.already_ingested {
            "already ingested"
        } else if ingested.embedded {
            "ingested + embedded"
        } else {
            "captured (not embedded)"
        };
        println!(
            "{state}: document {} under {} ({} chunks)",
            ingested.document, ingested.namespace, ingested.chunk_count
        );
        for w in &ingested.warnings {
            println!("  warning: {w}");
        }
        Ok(())
    }

    /// The ambient namespace for this process's working directory, unless `--global`.
    fn ambient(&self) -> Result<Option<String>> {
        if self.global {
            return Ok(None);
        }
        self.ambient_here()
    }

    /// The ambient namespace, `--global` or not — task homing always reflects where you are.
    pub(crate) fn ambient_here(&self) -> Result<Option<String>> {
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        let home = std::env::var("HOME").unwrap_or_default();
        match self.call(Request::KbAmbient { cwd, home })? {
            Response::Ambient { namespace } => Ok(namespace),
            other => unexpected("kb.ambient", &other),
        }
    }

    fn items(&self, request: Request) -> Result<Vec<ItemRow>> {
        let op = request.op();
        match self.call(request)? {
            Response::Items { items, .. } => Ok(items),
            other => unexpected(op, &other),
        }
    }

    fn query(&self, dsl: &str, limit: Option<usize>, count: bool) -> Result<()> {
        let default_scope = self.ambient()?;
        if count {
            let n = match self.call(Request::KbQuery {
                dsl: dsl.to_owned(),
                default_scope,
                limit: None,
                count: true,
                order: QueryOrder::Id,
            })? {
                Response::Count { count } => count,
                other => return unexpected("kb.query", &other),
            };
            if self.json {
                println!("{}", serde_json::json!({ "count": n }));
            } else {
                println!("{n}");
            }
            return Ok(());
        }
        let items = self.items(Request::KbQuery {
            dsl: dsl.to_owned(),
            default_scope,
            limit,
            count: false,
            order: QueryOrder::Id,
        })?;
        output::print_items(&items, self.json);
        Ok(())
    }

    /// `jkb find [path] --kind --tag --status`: flags compiled to the query DSL.
    fn find(
        &self,
        path: Option<&str>,
        kind: Option<&str>,
        tags: &[String],
        status: Option<&str>,
        limit: Option<usize>,
    ) -> Result<()> {
        // Refuse the one footgun: no filter, no path, no ambient scope and no limit would list the
        // entire KB. Any of them (or being inside a mounted repo) makes it fine.
        let unfiltered = kind.is_none() && tags.is_empty() && status.is_none() && path.is_none();
        if unfiltered && limit.is_none() && self.ambient()?.is_none() {
            bail!(
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
        self.query(&terms.join(" "), limit, false)
    }

    /// `jkb recent [path]`: the most-recently-updated items in a subtree, newest first.
    fn recent(&self, path: Option<&str>, limit: usize) -> Result<()> {
        let items = self.items(Request::KbQuery {
            dsl: path.map(|p| format!("ns:{p}/**")).unwrap_or_default(),
            default_scope: self.ambient()?,
            limit: Some(limit),
            count: false,
            order: QueryOrder::UpdatedDesc,
        })?;
        output::print_items(&items, self.json);
        Ok(())
    }

    fn search(
        &self,
        dsl: &str,
        route: SearchRoute,
        limit: usize,
        context: Option<usize>,
    ) -> Result<()> {
        let hits = match self.call(Request::KbSearch {
            dsl: dsl.to_owned(),
            default_scope: self.ambient()?,
            route,
            limit,
            context,
        })? {
            Response::SearchHits { hits, .. } => hits,
            other => return unexpected("kb.search", &other),
        };
        if self.json {
            // Every hit resolved to a real item: a result identified only by a row id is not
            // interpretable by the agent that asked for it.
            let arr: Vec<serde_json::Value> = hits
                .iter()
                .map(|hit| {
                    let item = hit.row.as_ref();
                    serde_json::json!({
                        "item": hit.item,
                        "uid": item.map(|i| i.uid.clone()),
                        "kind": item.map(|i| i.kind.clone()),
                        "status": item.and_then(|i| i.status.clone()),
                        "snippet": item.and_then(|i| i.snippet.clone()),
                        "route": hit.route,
                        "score": hit.score,
                        "distance": hit.distance,
                        "namespace": hit.namespace,
                        "source_document": hit.source_document.as_ref()
                            .map(|d| serde_json::json!({ "id": d.id, "uid": d.uid, "kind": d.kind })),
                        "context": hit.context.iter().map(|c| serde_json::json!({
                            "item": c.item,
                            "position": c.position,
                            "is_hit": c.is_hit,
                            "content": c.content,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
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
        for hit in &hits {
            let Some(item) = &hit.row else {
                // A hit whose item is gone should be unreachable now that `knn_live` filters them,
                // so say so rather than skipping silently (design D42.4).
                eprintln!(
                    "warning: search hit {} has no item row; run `jkb index --sweep`",
                    hit.item
                );
                continue;
            };
            println!("[{} {:.3}] {}", hit.route, hit.score, output_line(item));
            if context.is_some() {
                for c in &hit.context {
                    let marker = if c.is_hit { "»" } else { " " };
                    println!("    {marker} {}: {}", c.position, first_line(&c.content));
                }
            }
        }
        Ok(())
    }

    fn ls(&self, path: Option<&str>, opts: LsOpts) -> Result<()> {
        let mut rows = match self.call(Request::KbLs {
            path: path.map(str::to_owned),
            all: opts.all,
            recursive: opts.recursive,
        })? {
            Response::Listing { rows, .. } => rows,
            other => return unexpected("kb.ls", &other),
        };
        if opts.time {
            // Most-recently-updated first; rows without an `updated` (namespaces) sort last.
            rows.sort_by(|a, b| b.child.updated.cmp(&a.child.updated));
        }
        if self.json {
            let children: Vec<_> = rows.iter().map(|r| child_json(&r.child)).collect();
            let v = serde_json::json!({ "path": path, "children": children });
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else if rows.is_empty() {
            println!("(empty)");
        } else {
            for r in &rows {
                print_ls_row(r.parent.as_deref(), &r.child, opts);
            }
        }
        Ok(())
    }

    fn tree(&self, path: Option<&str>, all: bool, depth: Option<usize>) -> Result<()> {
        let nodes = match self.call(Request::KbTree {
            path: path.map(str::to_owned),
            all,
            depth: Some(depth.unwrap_or(DEFAULT_TREE_DEPTH)),
        })? {
            Response::Tree { nodes, .. } => nodes,
            other => return unexpected("kb.tree", &other),
        };
        if self.json {
            let v = serde_json::json!({
                "path": path,
                "tree": nodes.iter().map(tree_json).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            println!("{}", path.unwrap_or("."));
            print_tree(&nodes, "");
        }
        Ok(())
    }

    /// `jkb grep`: prints `uid:line:text` per matching line (uids with `-l`, a count with `-c`), and
    /// **exits 1 when nothing matched**, like grep.
    fn grep(
        &self,
        pattern: &str,
        path: Option<&str>,
        ignore_case: bool,
        names_only: bool,
        count: bool,
    ) -> Result<()> {
        // An explicit path wins; otherwise the ambient namespace (nothing = search all).
        let scope = match path {
            Some(p) => Some(p.to_owned()),
            None => self.ambient()?,
        };
        // `--json` prints the lines, so it wins over `-l`, as it did before the op existed.
        let mode = if count {
            GrepMode::Count
        } else if names_only && !self.json {
            GrepMode::Names
        } else {
            GrepMode::Lines
        };
        let answer: GrepAnswer = match self.call(Request::KbGrep {
            pattern: pattern.to_owned(),
            scope,
            ignore_case,
            mode,
        })? {
            Response::GrepHits { answer } => answer,
            other => return unexpected("kb.grep", &other),
        };
        let hits = &answer.hits;
        match mode {
            GrepMode::Count => {
                if self.json {
                    println!("{}", serde_json::json!({ "count": answer.count }));
                } else {
                    println!("{}", answer.count);
                }
            }
            _ if self.json => {
                let arr: Vec<_> = hits
                    .iter()
                    .map(|h| {
                        let lines: Vec<_> = h
                            .lines
                            .iter()
                            .map(|l| serde_json::json!({ "line": l.line, "text": l.text }))
                            .collect();
                        serde_json::json!({ "uid": h.uid, "kind": h.kind, "matches": lines })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            }
            GrepMode::Names => {
                for h in hits {
                    println!("{}", h.uid);
                }
            }
            GrepMode::Lines => {
                for h in hits {
                    for l in &h.lines {
                        println!("{}:{}:{}", h.uid, l.line, l.text.trim_end());
                    }
                }
            }
        }
        if answer.truncated {
            eprintln!("jkb: {} items matched in all", answer.count);
        }
        if answer.count == 0 {
            std::process::exit(1);
        }
        Ok(())
    }

    /// `jkb cat <uid>`: the full content, no metadata, no truncation.
    fn cat(&self, uid: &str) -> Result<()> {
        match self.call(Request::KbCat {
            uid: uid.to_owned(),
        })? {
            Response::Content { content } => {
                print!("{content}");
                Ok(())
            }
            other => unexpected("kb.cat", &other),
        }
    }

    /// `jkb task next`: the ready frontier, scoped by default to the ambient repo's task tree
    /// (`tasks/<repo>/**`) inside a repo and to `tasks/**` outside one or with `--global`.
    fn task_next(&self, dsl: &str, limit: Option<usize>) -> Result<()> {
        let root = jkb_core::task::DEFAULT_ROOT;
        let base = if self.global {
            root.to_owned()
        } else {
            match self.ambient_here()? {
                Some(repo) => format!("{root}/{repo}"),
                None => root.to_owned(),
            }
        };
        let items = self.items(Request::TaskReady {
            dsl: dsl.to_owned(),
            default_scope: Some(base),
            limit,
        })?;
        output::print_items(&items, self.json);
        Ok(())
    }

    fn task_show(&self, uid: &str) -> Result<()> {
        let (task, truncated) = match self.call(Request::TaskShow {
            uid: uid.to_owned(),
        })? {
            Response::Task { task, truncated } => (task, truncated),
            other => return unexpected("task.show", &other),
        };
        print_task(&task, truncated, self.json)
    }

    /// `jkb task subtasks <uid>`: a parent's children, shaped exactly like `jkb ls` output, so the
    /// tree expands a namespace and a parent task with one parser.
    fn task_subtasks(&self, uid: &str, all: bool) -> Result<()> {
        let children = match self.call(Request::TaskSubtasks {
            uid: uid.to_owned(),
            all,
        })? {
            Response::Children { children, .. } => children,
            other => return unexpected("task.subtasks", &other),
        };
        if self.json {
            let arr: Vec<_> = children.iter().map(child_json).collect();
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
}

/// An answer of the wrong shape: a daemon from another build that means something else by the op.
pub(crate) fn unexpected<T>(op: &str, response: &Response) -> Result<T> {
    let kind = serde_json::to_value(response)
        .ok()
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(str::to_owned))
        .unwrap_or_default();
    bail!("{op} was answered with `{kind}`, which it never returns — the daemon's jkb and this one disagree about the op; rebuild whichever is older")
}

/// Flags for `jkb ls`.
#[derive(Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)] // a CLI flags bag, not state
struct LsOpts {
    all: bool,
    long: bool,
    recursive: bool,
    time: bool,
}

/// A per-kind leaf breakdown as `8 task · 4 document`, ordered by kind name. Kinds are not
/// pluralized: they are `items.kind` values verbatim, and pluralizing an open vocabulary goes wrong
/// fast (`hypothesis` → `hypothesiss`).
fn format_leaf_kinds(kinds: &BTreeMap<String, i64>) -> String {
    kinds
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(kind, n)| format!("{n} {kind}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The item's hidden chunk count as a suffix, e.g. ` (3 chunks)`, or empty.
fn chunk_label(c: &Child) -> String {
    c.chunk_count
        .filter(|n| *n > 0)
        .map(|n| format!(" ({n} chunk{})", if n == 1 { "" } else { "s" }))
        .unwrap_or_default()
}

/// The namespace's own type as a bracketed label, e.g. ` [tasks]`, or empty.
fn type_label(c: &Child) -> String {
    c.ns_type
        .as_deref()
        .map(|t| format!(" [{t}]"))
        .unwrap_or_default()
}

fn child_json(c: &Child) -> serde_json::Value {
    serde_json::json!({
        "kind": c.kind,
        "ref": c.reference,
        "label": c.label,
        "has_children": c.has_children,
        "status": c.status,
        "priority": c.priority,
        "leaf_count": c.leaf_count,
        "leaf_kinds": c.leaf_kinds,
        "type": c.ns_type,
        "type_about": c.ns_type_about,
        "chunk_count": c.chunk_count,
        "subtask_count": c.subtask_count,
        "open_subtask_count": c.open_subtask_count,
        "updated": c.updated,
    })
}

/// One human `ls` row. `-l` adds kind/status and the location; the default is the compact tree row.
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
            type_label(c)
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
            type_label(c),
            chunk_label(c)
        );
    }
}

fn tree_json(node: &TreeNode) -> serde_json::Value {
    let mut v = child_json(&node.child);
    if !node.children.is_empty() {
        v["children"] = node.children.iter().map(tree_json).collect();
    }
    v
}

/// One tree level with box-drawing prefixes; a namespace elided by the depth cap gets a `…`.
fn print_tree(nodes: &[TreeNode], prefix: &str) {
    for (i, node) in nodes.iter().enumerate() {
        let last = i + 1 == nodes.len();
        let (branch, cont) = if last {
            ("└─ ", "   ")
        } else {
            ("├─ ", "│  ")
        };
        // WHAT is in the subtree, not just how much: a bare number invites reading every leaf as a
        // task, which is what this display used to claim.
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
            "{prefix}{branch}{}{}{leaves}{}{status}{elided}",
            node.child.label,
            type_label(&node.child),
            chunk_label(&node.child)
        );
        print_tree(&node.children, &format!("{prefix}{cont}"));
    }
}

/// `jkb task show`: the task's fields, its recent transitions inside the header block, the body, and
/// — human output only — its subtasks, since a parent is off the ready frontier until they are all
/// terminal and "why isn't this actionable?" must be answerable from the command that shows it.
fn print_task(task: &TaskDetail, truncated: bool, json: bool) -> Result<()> {
    let item = &task.item;
    let transitions: Vec<serde_json::Value> = task
        .transitions
        .iter()
        .map(|r| {
            serde_json::json!({
                "at": r.at,
                "event": r.event,
                "to": r.to,
                "branch": r.branch,
                "onto": r.onto,
                "pr": r.pr,
            })
        })
        .collect();
    if json {
        // The transitions go in the SAME object: a `--json` consumer reading one document must not
        // get half the answer.
        let v = serde_json::json!({
            "id": item.id,
            "uid": item.uid,
            "kind": item.kind,
            "status": item.status,
            "priority": item.priority,
            "due": item.due,
            "namespace": item.namespace,
            "content": item.content,
            "tags": item.tags.iter()
                .map(|t| serde_json::json!({ "facet": t.facet, "value": t.value }))
                .collect::<Vec<_>>(),
            "transitions": transitions,
            // Additive: a `--json` consumer can see why a task is off the frontier too, and a notice
            // that the subtasks were cut refers to something in the document.
            "subtasks": task.subtasks.iter()
                .map(|t| serde_json::json!({ "uid": t.uid, "title": t.title, "status": t.status }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("uid:       {}", item.uid);
    println!("kind:      {}", item.kind);
    if let Some(s) = &item.status {
        println!("status:    {s}");
    }
    if let Some(p) = item.priority {
        println!("priority:  {p}");
    }
    if let Some(d) = &item.due {
        println!("due:       {d}");
    }
    if let Some(ns) = &item.namespace {
        println!("namespace: {ns}");
    }
    if !item.tags.is_empty() {
        let pairs: Vec<String> = item
            .tags
            .iter()
            .map(|t| format!("{}={}", t.facet, t.value))
            .collect();
        println!("tags:      {}", pairs.join(", "));
    }
    if !task.transitions.is_empty() {
        println!("recent transitions (`jkb task why` for all):");
        for r in &task.transitions {
            let mut line = format!("  {} {} -> {}", r.at, r.event, r.to);
            if let Some(b) = &r.branch {
                let _ = write!(line, " on {b}");
            }
            if let Some(o) = &r.onto {
                let _ = write!(line, " onto {o}");
            }
            if let Some(n) = r.pr {
                let _ = write!(line, " #{n}");
            }
            println!("{line}");
        }
    }
    println!();
    println!("{}", item.content.as_deref().unwrap_or("(no content)"));
    if !task.subtasks.is_empty() || truncated {
        let open = task
            .subtasks
            .iter()
            .filter(|t| !jkb_types::TaskStatus::is_terminal_str(t.status.as_deref()))
            .count();
        if truncated {
            // A prefix: its counts are lower bounds, and a verdict on the whole list is not drawn
            // from part of it.
            println!(
                "\nsubtasks (the first {}, {open} open; the list was cut short):",
                task.subtasks.len()
            );
        } else {
            println!("\nsubtasks ({open} open of {}):", task.subtasks.len());
        }
        for t in &task.subtasks {
            let status = t.status.as_deref().unwrap_or("?");
            println!("  [{status:^12}] {} — {}", t.uid, first_line(&t.title));
        }
        if open > 0 {
            println!("this task is held off the ready frontier until they are done");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use jkb_api::kb::{GrepAnswer, GrepHit, ItemDetail, TaskDetail};
    use jkb_api::{ApiError, Backend, Request, Response};

    use super::Ops;
    use crate::Cli;

    /// Answers every read with a cut answer of the right shape.
    struct CutShort;

    impl Backend for CutShort {
        fn call(&self, request: Request) -> Result<Response, ApiError> {
            Ok(match request {
                Request::KbAmbient { .. } => Response::Ambient {
                    namespace: Some("repos/x".into()),
                },
                Request::KbQuery { .. } | Request::TaskReady { .. } => Response::Items {
                    items: Vec::new(),
                    truncated: true,
                },
                Request::KbLs { .. } => Response::Listing {
                    rows: Vec::new(),
                    truncated: true,
                },
                Request::KbTree { depth, .. } => Response::Tree {
                    nodes: Vec::new(),
                    truncated: true,
                    // `--depth 3` asks for a node-cap cut; any other depth, a budget cut.
                    at_node_cap: depth == Some(3),
                },
                Request::KbGrep { .. } => Response::GrepHits {
                    answer: GrepAnswer {
                        hits: vec![GrepHit {
                            uid: "u".into(),
                            kind: "note".into(),
                            lines: Vec::new(),
                        }],
                        count: 2,
                        truncated: true,
                    },
                },
                Request::KbSearch { .. } => Response::SearchHits {
                    hits: Vec::new(),
                    truncated: true,
                },
                Request::TaskShow { .. } => Response::Task {
                    task: Box::new(TaskDetail {
                        item: ItemDetail {
                            id: 1,
                            uid: "task:t".into(),
                            kind: "task".into(),
                            status: None,
                            priority: None,
                            due: None,
                            namespace: None,
                            content: None,
                            tags: Vec::new(),
                        },
                        transitions: Vec::new(),
                        subtasks: Vec::new(),
                    }),
                    truncated: true,
                },
                Request::TaskWhy { .. } => Response::History {
                    entries: Vec::new(),
                    truncated: true,
                },
                Request::TaskSubtasks { .. } => Response::Children {
                    children: Vec::new(),
                    truncated: true,
                },
                other => {
                    return Err(ApiError::bad_request(format!(
                        "not scripted: {}",
                        other.op()
                    )))
                }
            })
        }
    }

    /// Every listing command says when its answer was cut — asked of each command, because a notice
    /// taken apart per command was one arm's to forget.
    #[test]
    fn every_listing_command_reports_a_cut_answer() {
        for args in [
            vec!["query", "kind:task"],
            vec!["find", "--kind", "task"],
            vec!["recent"],
            vec!["search", "x"],
            vec!["ls"],
            vec!["tree"],
            vec!["tree", "--depth", "3"],
            vec!["grep", "x"],
            vec!["task", "next"],
            vec!["task", "show", "t"],
            vec!["task", "subtasks", "t"],
            vec!["task", "why", "t"],
        ] {
            for remote in [true, false] {
                let cli = Cli::try_parse_from(std::iter::once("jkb").chain(args.iter().copied()))
                    .unwrap();
                let reads = Ops::new(&CutShort, false, false, remote);
                reads
                    .run(cli.command)
                    .unwrap_or_else(|e| panic!("{args:?}: {e:#}"));
                let notices = reads.notices.borrow();
                assert_eq!(notices.len(), 1, "{args:?} (remote {remote}): {notices:?}");
                let node_cap = args == ["tree", "--depth", "3"];
                assert_eq!(
                    notices[0].contains("run it on the host"),
                    remote && !node_cap,
                    "{args:?}: only the daemon's budget is lifted by running it on the host"
                );
                assert_eq!(
                    notices[0].contains("nodes"),
                    node_cap,
                    "{args:?}: {notices:?}"
                );
            }
        }
    }
}
