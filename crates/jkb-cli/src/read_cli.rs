//! The agent read set's commands (tasks S6.1): `query`, `find`, `recent`, `search`, `ls`, `tree`,
//! `grep`, `cat`, and `task next`/`show`/`subtasks`.
//!
//! **Every read here goes through a [`Backend`], on the host too.** The host passes a
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
use jkb_api::{ApiError, Backend, Request, Response};

use super::{first_line, output, output_line, Command, TaskCmd};

/// Depth `jkb tree` descends by default before eliding deeper folders with `…` — deep enough to map
/// any real subtree, shallow enough to bound the output and the per-namespace query fan-out.
const DEFAULT_TREE_DEPTH: usize = 4;

/// Whether `command` is one of this module's — what remote mode's dispatch routes here. [`Reads::run`]
/// refuses anything else. Remote mode's support table names the same commands in its own exhaustive
/// match; `the_ported_reads_are_the_ones_read_cli_handles` holds the two lists together.
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
        | Command::Cat { .. } => true,
        Command::Task { cmd } => matches!(
            cmd,
            TaskCmd::Next { .. } | TaskCmd::Show { .. } | TaskCmd::Subtasks { .. }
        ),
        _ => false,
    }
}

/// How a read is served.
pub struct Reads<'a> {
    backend: &'a dyn Backend,
    global: bool,
    json: bool,
    /// The route `jkb search` takes when `--route` is not given: hybrid where the backend embeds,
    /// FTS through the daemon, which does not.
    default_route: SearchRoute,
}

impl<'a> Reads<'a> {
    /// Reads through `backend`.
    #[must_use]
    pub const fn new(
        backend: &'a dyn Backend,
        global: bool,
        json: bool,
        default_route: SearchRoute,
    ) -> Self {
        Self {
            backend,
            global,
            json,
            default_route,
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
                route.map_or(self.default_route, Into::into),
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
            Command::Task { cmd } => match cmd {
                TaskCmd::Next { terms, limit } => self.task_next(&terms.join(" "), limit),
                TaskCmd::Show { uid } => self.task_show(&uid),
                TaskCmd::Subtasks { uid, all } => self.task_subtasks(&uid, all),
                _ => bail!("internal: a task subcommand the read set does not handle"),
            },
            _ => bail!("internal: a command the read set does not handle"),
        }
    }

    fn call(&self, request: Request) -> Result<Response> {
        self.backend
            .call(request)
            .map_err(|e: ApiError| anyhow::Error::msg(e.message))
    }

    /// The ambient namespace for this process's working directory, unless `--global`.
    fn ambient(&self) -> Result<Option<String>> {
        if self.global {
            return Ok(None);
        }
        self.ambient_here()
    }

    /// The ambient namespace, `--global` or not — task homing always reflects where you are.
    fn ambient_here(&self) -> Result<Option<String>> {
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
            Response::Items { items } => Ok(items),
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
            Response::SearchHits { hits } => hits,
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
            Response::Listing { rows } => rows,
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
            Response::Tree { nodes } => nodes,
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
            eprintln!(
                "jkb grep: output stopped at {} MiB of matches; {} items matched in all — narrow the \
                 pattern or give a path",
                jkb_api::kb::MAX_GREP_BYTES / (1024 * 1024),
                answer.count
            );
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
        let task = match self.call(Request::TaskShow {
            uid: uid.to_owned(),
        })? {
            Response::Task { task } => task,
            other => return unexpected("task.show", &other),
        };
        print_task(&task, self.json)
    }

    /// `jkb task subtasks <uid>`: a parent's children, shaped exactly like `jkb ls` output, so the
    /// tree expands a namespace and a parent task with one parser.
    fn task_subtasks(&self, uid: &str, all: bool) -> Result<()> {
        let children = match self.call(Request::TaskSubtasks {
            uid: uid.to_owned(),
            all,
        })? {
            Response::Children { children } => children,
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
fn unexpected<T>(op: &str, response: &Response) -> Result<T> {
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
fn print_task(task: &TaskDetail, json: bool) -> Result<()> {
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
    if !task.subtasks.is_empty() {
        let open = task
            .subtasks
            .iter()
            .filter(|t| !jkb_types::TaskStatus::is_terminal_str(t.status.as_deref()))
            .count();
        println!("\nsubtasks ({open} open of {}):", task.subtasks.len());
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
