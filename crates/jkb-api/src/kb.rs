//! The agent read set (tasks S6.1): the knowledge-base reads a container agent needs, as ops.
//!
//! **Each function here is the one implementation of its read.** [`crate::LocalBackend`] serves it
//! in-process for the host CLI and behind `jkb serve` for the dev container, and the CLI only renders
//! what comes back — so `jkb ls` cannot list one thing on the host and another in the container.
//!
//! All pure database reads (design r3.2 H4). Two requests carry something from the client's world,
//! and neither reaches the host's: a working directory ([`ambient`]) is only compared, as a string,
//! against the mounts table; a search route that would embed text ([`search`]) is refused unless the
//! backend was given an embedder, which the daemon never is.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jkb_core::query::{self, Scope};
use jkb_core::{containment, item, mount, ns, nstype, placement, tag, task, transition, Db};
use jkb_search::{Route, Searcher};
use jkb_types::{Embedder, ItemId, TaskStatus};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{ApiError, ErrorCode};

/// The most hits one `kb.search` may ask for: it bounds the answer, and the hybrid route — served
/// only where there is an embedder, the host CLI — fuses from twice the limit, which an unbounded
/// value overflows.
pub const MAX_SEARCH_LIMIT: usize = 1000;

/// The most neighbour chunks per side `kb.search` expands a hit into. Without it one request could
/// ask for every hit's whole document.
pub const MAX_SEARCH_CONTEXT: usize = 50;

/// The deepest `kb.tree` descends, whatever it is asked. Each level is two levels of JSON nesting,
/// and `serde_json` refuses to decode past 128 — a deeper tree printed on the host and failed to
/// decode through the daemon.
pub const MAX_TREE_DEPTH: usize = 48;

/// The most nodes one `kb.tree` lists, whatever its byte [`Budget`]: past it the tree is cut short
/// and marked truncated. A reference is looked up as a namespace before an item, so data can make
/// nodes list each other, and a walk that only skips its own ancestors can still grow as a power of
/// its depth — this bounds the work, where the budget bounds the answer.
pub const MAX_TREE_NODES: usize = 10_000;

/// What one read may put in its answer, in bytes of the JSON the answer is sent as.
///
/// **One bound for every read that lists**, charged row by row as the answer is built, rather than a
/// cap per op: per-op caps were each measured in something other than what reaches the wire — a count
/// of chunks while a document hit's context is its whole body, a sum of line bytes while each line
/// carries its own JSON — and one op had none. An answer that stops at the budget is a prefix of the
/// full one and says so (`truncated`). `jkb serve` gives every read a budget; the host CLI's is
/// unlimited.
///
/// What it does not bound, stated: `kb.cat` and `task.show`'s own body are the one item asked for,
/// and a namespace's children are gathered before they are sorted and charged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    left: usize,
    exhausted: bool,
}

impl Budget {
    /// No bound.
    pub const UNLIMITED: Self = Self::new(usize::MAX);

    /// A budget of `bytes`.
    #[must_use]
    pub const fn new(bytes: usize) -> Self {
        Self {
            left: bytes,
            exhausted: false,
        }
    }

    /// Whether `value` fits, charging it when it does. Once something has not fitted nothing does, so
    /// an answer is always a prefix.
    pub fn take<T: Serialize>(&mut self, value: &T) -> bool {
        if self.exhausted {
            return false;
        }
        let mut counted = Counted(0);
        let cost = serde_json::to_writer(&mut counted, value).map_or(usize::MAX, |()| counted.0);
        // A separator and some structure per entry, so a million empty rows still cost something.
        let cost = cost.saturating_add(8);
        if cost > self.left {
            self.exhausted = true;
            return false;
        }
        self.left -= cost;
        true
    }

    /// Whether something did not fit.
    #[must_use]
    pub const fn exhausted(&self) -> bool {
        self.exhausted
    }
}

/// Counts bytes written, keeping none.
struct Counted(usize);

impl std::io::Write for Counted {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// How many of a task's transitions `task.show` carries — the recent ones; `jkb task why` has all.
pub const RECENT_TRANSITIONS: usize = 5;

/// The item kind ingest produces per document fragment. Chunks are derived index units: they are
/// rebuildable from the VFS, nothing links *to* them, and listing them buries each ingested document
/// under its own pieces. Listings hide them unless `all` and surface their count against the
/// document they came from.
pub const KIND_CHUNK: &str = "chunk";

/// A search route on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchRoute {
    /// Nearest neighbours of the embedded query text.
    Vector,
    /// Keyword (FTS5).
    Fts,
    /// Both, fused.
    Hybrid,
}

impl From<SearchRoute> for Route {
    fn from(r: SearchRoute) -> Self {
        match r {
            SearchRoute::Vector => Self::Vector,
            SearchRoute::Fts => Self::Fts,
            SearchRoute::Hybrid => Self::Hybrid,
        }
    }
}

/// What `kb.grep` answers with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrepMode {
    /// Each matched item with its matching lines.
    #[default]
    Lines,
    /// Each matched item, no lines (`grep -l`).
    Names,
    /// Only how many items matched (`grep -c`).
    Count,
}

/// The order `kb.query` lists items in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryOrder {
    /// By item id.
    #[default]
    Id,
    /// Most recently updated first, then by id — `jkb recent`, so its limit is applied here rather
    /// than after every item in scope crossed the wire.
    UpdatedDesc,
}

/// A denormalized item row for listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRow {
    /// Row id.
    pub id: i64,
    /// Stable uid.
    pub uid: String,
    /// Item kind.
    pub kind: String,
    /// Status (tasks).
    pub status: Option<String>,
    /// Resolution — how a unit ended (investigation units); `None` = unresolved.
    pub resolution: Option<String>,
    /// Priority (tasks).
    pub priority: Option<i64>,
    /// Due date (tasks).
    pub due: Option<String>,
    /// A one-line content snippet.
    pub snippet: Option<String>,
    /// The namespace the item is placed under (primary preferred).
    pub namespace: Option<String>,
    /// Last-update timestamp (ISO).
    pub updated: Option<String>,
}

/// A direct child of a namespace or container: a sub-namespace, or an item placed there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Child {
    /// `namespace`, or the item's kind.
    pub kind: String,
    /// The namespace path, or the item's uid.
    pub reference: String,
    /// A short label: the last path segment, or the item's title (≤ 80 chars).
    pub label: String,
    /// Whether it expands.
    pub has_children: bool,
    /// The item's status.
    pub status: Option<String>,
    /// The item's priority.
    pub priority: Option<i64>,
    /// For namespaces: visible item leaves anywhere in the subtree — the sum of `leaf_kinds`.
    pub leaf_count: Option<i64>,
    /// For namespaces: those leaves by item kind. A folder holding 8 tasks and 4 documents is not
    /// described by "12", so the breakdown is what a tree renders.
    pub leaf_kinds: Option<BTreeMap<String, i64>>,
    /// For namespaces: the type recorded on THIS namespace, not the inherited one — where a type was
    /// applied is the useful fact, and a label on every namespace under a typed root is noise.
    pub ns_type: Option<String>,
    /// The one-line description of `ns_type`.
    pub ns_type_about: Option<String>,
    /// For an item with subtasks: how many. A parent with open subtasks is held off the ready
    /// frontier, so a tree must be able to show it as a container.
    pub subtask_count: Option<i64>,
    /// For an item with subtasks: how many are open.
    pub open_subtask_count: Option<i64>,
    /// For an item other items were derived from: its hidden `chunk` count.
    pub chunk_count: Option<i64>,
    /// The item's `updated_at`; `None` for namespaces.
    pub updated: Option<String>,
}

impl Child {
    /// Ordering key: namespaces first, then tasks (lowest priority number first), then other items;
    /// ties by label. Nulls sort last.
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

/// One `kb.ls` row: the namespace it was listed under, and the child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListRow {
    /// The namespace listed (`None` for the top level).
    pub parent: Option<String>,
    /// What was found there.
    pub child: Child,
}

/// One `kb.tree` node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeNode {
    /// The node.
    pub child: Child,
    /// What it contains, down to the requested depth.
    pub children: Vec<TreeNode>,
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepLine {
    /// 1-based line number.
    pub line: usize,
    /// The line, as stored.
    pub text: String,
}

/// One item `kb.grep` matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepHit {
    /// Its uid.
    pub uid: String,
    /// Its kind.
    pub kind: String,
    /// The lines holding the pattern. Can be empty: a pattern spanning a line break matches the
    /// item but no single line.
    pub lines: Vec<GrepLine>,
}

/// `kb.grep`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepAnswer {
    /// The matched items, by uid — empty for [`GrepMode::Count`], and cut short when `truncated`
    /// (the last one possibly with only some of its lines).
    pub hits: Vec<GrepHit>,
    /// How many items matched, whether or not all of them are in `hits`.
    pub count: usize,
    /// `hits` stopped at the read's [`Budget`].
    pub truncated: bool,
}

/// One chunk of a hit's context window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextLine {
    /// The chunk's item id.
    pub item: i64,
    /// Its position among its document's chunks.
    pub position: i64,
    /// Whether it is the hit itself.
    pub is_hit: bool,
    /// Its text.
    pub content: String,
}

/// One `kb.search` hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The matched item's id.
    pub item: i64,
    /// The matched item, or `None` if its row is gone.
    pub row: Option<ItemRow>,
    /// The route that produced it.
    pub route: String,
    /// Score, higher is better.
    pub score: f64,
    /// Cosine distance, when a vector match contributed.
    pub distance: Option<f32>,
    /// The namespace the hit is placed under.
    pub namespace: Option<String>,
    /// For a chunk: the document it came from.
    pub source_document: Option<ItemRow>,
    /// The ±N chunks around it, when asked for.
    pub context: Vec<ContextLine>,
}

/// A tag application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagPair {
    /// The facet.
    pub facet: String,
    /// The value.
    pub value: String,
}

/// An item in full, for `task.show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemDetail {
    /// Row id.
    pub id: i64,
    /// Uid.
    pub uid: String,
    /// Kind.
    pub kind: String,
    /// Status.
    pub status: Option<String>,
    /// Priority.
    pub priority: Option<i64>,
    /// Due date.
    pub due: Option<String>,
    /// Primary namespace (else the first placement).
    pub namespace: Option<String>,
    /// Full content.
    pub content: Option<String>,
    /// Tags, by facet then value.
    pub tags: Vec<TagPair>,
}

/// One task transition, as `task.show` summarizes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionSummary {
    /// When.
    pub at: String,
    /// The event.
    pub event: String,
    /// The status it moved to.
    pub to: String,
    /// The branch the work is on.
    pub branch: Option<String>,
    /// The branch it lands on.
    pub onto: Option<String>,
    /// The pull request that proved a landing.
    pub pr: Option<i64>,
}

/// A subtask, as `task.show` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtaskSummary {
    /// Its uid.
    pub uid: String,
    /// Its label: the title rule `kb.ls` labels items by (`item::title_from`, ≤ 80 chars).
    pub title: String,
    /// Its status.
    pub status: Option<String>,
}

/// `task.show`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDetail {
    /// The task.
    pub item: ItemDetail,
    /// Its last [`RECENT_TRANSITIONS`] transitions, oldest first.
    pub transitions: Vec<TransitionSummary>,
    /// Its subtasks, in containment order.
    pub subtasks: Vec<SubtaskSummary>,
}

fn not_found(what: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::NotFound, what)
}

/// The ambient namespace for a client's working directory: the mount whose directory holds it.
///
/// `cwd` is a path in the CLIENT's filesystem and `client_home` its `$HOME`; a `cwd` under that home
/// is re-rooted at `server_home` before the lookup. That is what makes the dev container's
/// `/home/vscode/repos/jkb` find the mount the host recorded as `/Users/<u>/repos/jkb` — the
/// container binds the host's `~/repos` at its own `~/repos` (`.container/container.json`). In one
/// process the two homes are the same and the path is unchanged. The path is only compared against
/// the mounts table; nothing opens it.
///
/// Residual, stated: a container path under its home that is NOT bound from the host (its own
/// `~/scratch`, say) re-roots onto a host path that may be a mount, and so scopes to a namespace that
/// is not the directory the agent is in. Scoping only; no content moves.
///
/// # Errors
/// Returns an error if the read fails.
pub fn ambient(
    conn: &Connection,
    cwd: &str,
    client_home: &str,
    server_home: Option<&Path>,
) -> jkb_core::Result<Option<String>> {
    let cwd = rerooted(Path::new(cwd), client_home, server_home);
    mount::ambient_namespace(conn, &cwd)
}

fn rerooted(cwd: &Path, client_home: &str, server_home: Option<&Path>) -> PathBuf {
    match (client_home, server_home) {
        ("", _) | (_, None) => cwd.to_path_buf(),
        (client, Some(server)) => cwd
            .strip_prefix(client)
            .map_or_else(|_| cwd.to_path_buf(), |rest| server.join(rest)),
    }
}

/// Apply a default scope to a parsed query that names none.
fn scoped(dsl: &str, default_scope: Option<&str>) -> jkb_core::Result<query::Query> {
    let mut q = query::parse(dsl)?;
    if q.scope == Scope::All {
        if let Some(path) = default_scope {
            q.scope = Scope::Subtree(path.to_owned());
        }
    }
    Ok(q)
}

/// `kb.query`: the items a DSL query matches, as listing rows, within `budget`; `default_scope`
/// applies when the query names no scope.
///
/// # Errors
/// A malformed query, or a failed read.
pub fn query_items(
    conn: &Connection,
    dsl: &str,
    default_scope: Option<&str>,
    limit: Option<usize>,
    order: QueryOrder,
    budget: &mut Budget,
) -> jkb_core::Result<Vec<ItemRow>> {
    let mut q = scoped(dsl, default_scope)?;
    let ids = match order {
        QueryOrder::Id => {
            q.limit = limit;
            q.evaluate(conn)?
        }
        QueryOrder::UpdatedDesc => {
            // The matching ids, reordered and cut in one statement — only ids cross into it.
            let ids: Vec<i64> = q.evaluate(conn)?.iter().map(|i| i.get()).collect();
            let limit = limit.map_or(-1, |l| i64::try_from(l).unwrap_or(i64::MAX));
            let mut stmt = conn.prepare_cached(
                "SELECT id FROM items WHERE id IN (SELECT value FROM json_each(?1))
                 ORDER BY updated_at DESC, id LIMIT ?2",
            )?;
            let ordered = stmt
                .query_map(
                    rusqlite::params![serde_json::to_string(&ids).unwrap_or_default(), limit],
                    |r| Ok(ItemId::new(r.get(0)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ordered
        }
    };
    item_rows(conn, &ids, budget)
}

/// `kb.query` with `count`: how many items it matches, ignoring any limit.
///
/// # Errors
/// A malformed query, or a failed read.
pub fn query_count(
    conn: &Connection,
    dsl: &str,
    default_scope: Option<&str>,
) -> jkb_core::Result<usize> {
    Ok(scoped(dsl, default_scope)?.evaluate(conn)?.len())
}

/// `task.ready`: the ready frontier for a DSL's scope and tags, by priority then due.
///
/// # Errors
/// A malformed query, or a failed read.
pub fn ready(
    conn: &Connection,
    dsl: &str,
    default_scope: Option<&str>,
    limit: Option<usize>,
    budget: &mut Budget,
) -> jkb_core::Result<Vec<ItemRow>> {
    let q = scoped(dsl, default_scope)?;
    // Ordered and limited over ids alone, then loaded a row at a time within the budget: loading the
    // frontier first held every task's body before `limit` cut it to one.
    let ids = task::ready_ids(conn, q.scope, &q.tags, limit)?;
    item_rows(conn, &ids, budget)
}

/// A one-line snippet: the first non-blank line, trimmed to 80 chars.
fn snippet(content: &str) -> String {
    let line = item::first_nonblank(content);
    let mut out: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        out.push('…');
    }
    out
}

fn truncated(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

fn primary_namespace(conn: &Connection, id: i64) -> jkb_core::Result<Option<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
             WHERE p.item_id = ?1
             ORDER BY (p.role = 'primary') DESC, p.position LIMIT 1",
        )?
        .query_row([id], |r| r.get::<_, String>(0))
        .optional()?)
}

/// Listing rows for `ids`, in their order, skipping any that no longer exist, until one does not fit
/// `budget`.
///
/// # Errors
/// Returns an error if a read fails.
pub fn item_rows(
    conn: &Connection,
    ids: &[ItemId],
    budget: &mut Budget,
) -> jkb_core::Result<Vec<ItemRow>> {
    let mut out = Vec::new();
    for id in ids {
        let row = conn
            .prepare_cached(
                "SELECT id, uid, kind, status, resolution, priority, due, content, updated_at
                 FROM items WHERE id = ?1",
            )?
            .query_row([id.get()], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, String>(8)?,
                ))
            })
            .optional()?;
        let Some((id, uid, kind, status, resolution, priority, due, content, updated)) = row else {
            continue;
        };
        let row = ItemRow {
            namespace: primary_namespace(conn, id)?,
            id,
            uid,
            kind,
            status,
            resolution,
            priority,
            due,
            snippet: content.as_deref().map(snippet),
            updated: Some(updated),
        };
        if !budget.take(&row) {
            break;
        }
        out.push(row);
    }
    Ok(out)
}

/// An item's label in a listing: its title (`item::title_from`), at most 80 chars.
fn label(uid: &str, content: Option<&str>) -> String {
    truncated(&item::title_from(uid, content), 80)
}

fn item_child(meta: item::ItemMeta, subtasks: Option<(i64, i64)>, chunks: Option<i64>) -> Child {
    let chunks = chunks.filter(|n| *n > 0);
    Child {
        label: label(&meta.uid, meta.content.as_deref()),
        kind: meta.kind,
        reference: meta.uid,
        // Anything that contains expands: a task into its subtasks, a document into its chunks.
        has_children: subtasks.is_some_and(|(total, _)| total > 0) || chunks.is_some(),
        status: meta.status,
        priority: meta.priority,
        leaf_count: None,
        leaf_kinds: None,
        ns_type: None,
        ns_type_about: None,
        subtask_count: subtasks.map(|(total, _)| total),
        open_subtask_count: subtasks.map(|(_, open)| open),
        chunk_count: chunks,
        updated: Some(meta.updated_at),
    }
}

/// The children of an item that contains others — a task's subtasks, a document's chunks. One read
/// for every container, because containment is recorded the same way for both (design D35).
/// Terminal items are hidden unless `all`.
///
/// # Errors
/// Returns an error if a read fails.
pub fn contained_children(
    conn: &Connection,
    parent: ItemId,
    all: bool,
) -> jkb_core::Result<Vec<Child>> {
    let ids = containment::children(conn, parent)?;
    let subtask_counts = containment::child_counts(conn, &ids)?;
    let chunk_counts = item::derived_kind_counts(conn, &ids, KIND_CHUNK)?;
    let mut out = Vec::new();
    for id in ids {
        let Some(meta) = item::get(conn, id)? else {
            continue;
        };
        if !all && TaskStatus::is_terminal_str(meta.status.as_deref()) {
            continue;
        }
        out.push(item_child(
            meta,
            subtask_counts.get(&id).copied(),
            chunk_counts.get(&id).copied(),
        ));
    }
    Ok(out)
}

/// The direct children of `path` (top-level namespaces when `None`): sub-namespaces, then items
/// placed directly there. A path that names no namespace but an item uid lists that item's contained
/// children — "container" is a behaviour, not a node kind. Terminal items are hidden unless `all`.
///
/// # Errors
/// Returns an error if a read fails.
pub fn children(conn: &Connection, path: Option<&str>, all: bool) -> jkb_core::Result<Vec<Child>> {
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
    // Every child's subtree leaf counts in one grouped query, not a walk per child.
    let leaf_counts = ns::subtree_leaf_counts(conn, path, all)?;
    for (ns_id, ns_path) in ns_children {
        let label = ns_path.rsplit('/').next().unwrap_or(&ns_path).to_owned();
        let has_sub = !ns::children(conn, &ns_path)?.is_empty();
        let mut leaf_kinds = leaf_counts.get(&ns_id).cloned().unwrap_or_default();
        if !all {
            // Chunks are hidden, so they must not be counted either — a folder reporting "1 chunk"
            // that shows nothing when opened is worse than no count.
            leaf_kinds.remove(KIND_CHUNK);
        }
        let leaf_count: i64 = leaf_kinds.values().sum();
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
            subtask_count: None,
            open_subtask_count: None,
            chunk_count: None,
            updated: None,
        });
    }
    if let Some(p) = path {
        if let Some(ns_id) = ns::get(conn, p)? {
            // Any placement role — a `tasks/…` mirror surfaces the task — but directly placed only:
            // a contained node is listed under its container, not beside it.
            let placed = placement::items_directly_in(conn, ns_id)?;
            let chunk_counts = item::derived_kind_counts(conn, &placed, KIND_CHUNK)?;
            let subtask_counts = containment::child_counts(conn, &placed)?;
            for item_id in placed {
                let Some(meta) = item::get(conn, item_id)? else {
                    continue;
                };
                if !all && TaskStatus::is_terminal_str(meta.status.as_deref()) {
                    continue;
                }
                out.push(item_child(
                    meta,
                    subtask_counts.get(&item_id).copied(),
                    chunk_counts.get(&item_id).copied(),
                ));
            }
        }
    }
    out.sort_by_key(Child::sort_key);
    Ok(out)
}

/// `kb.ls`: the children of `path`, and with `recursive` every namespace below it depth-first, each
/// row naming the namespace it was listed under — until a row does not fit `budget`.
///
/// # Errors
/// Returns an error if a read fails.
pub fn ls(
    conn: &Connection,
    path: Option<&str>,
    all: bool,
    recursive: bool,
    budget: &mut Budget,
) -> jkb_core::Result<Vec<ListRow>> {
    fn walk(
        conn: &Connection,
        path: Option<&str>,
        all: bool,
        recursive: bool,
        acc: &mut Vec<ListRow>,
        budget: &mut Budget,
    ) -> jkb_core::Result<()> {
        for child in children(conn, path, all)? {
            let descend = (recursive && child.kind == "namespace").then(|| child.reference.clone());
            let row = ListRow {
                parent: path.map(str::to_owned),
                child,
            };
            if !budget.take(&row) {
                return Ok(());
            }
            acc.push(row);
            if let Some(ns_path) = descend {
                walk(conn, Some(&ns_path), all, recursive, acc, budget)?;
            }
        }
        Ok(())
    }
    let mut acc = Vec::new();
    walk(conn, path, all, recursive, &mut acc, budget)?;
    Ok(acc)
}

/// `kb.tree`: the subtree under `path`, descending into any container — not only namespaces, or a
/// subtask de-duplicated out of its namespace listing would be unreachable — to `depth` levels,
/// never more than [`MAX_TREE_DEPTH`] (`None` asks for that). A node whose reference is one of its
/// own ancestors' is listed but not descended into. The walk stops, cut short, at a node that does
/// not fit `budget` or past [`MAX_TREE_NODES`]; the returned flag says whether it did.
///
/// # Errors
/// Returns an error if a read fails.
pub fn tree(
    conn: &Connection,
    path: Option<&str>,
    all: bool,
    depth: Option<usize>,
    budget: &mut Budget,
) -> jkb_core::Result<(Vec<TreeNode>, bool)> {
    let mut walk = TreeWalk {
        conn,
        all,
        ancestors: path.map(str::to_owned).into_iter().collect(),
        listed: 0,
        cut: false,
        budget,
    };
    let nodes = walk.level(
        path,
        depth.map_or(MAX_TREE_DEPTH, |d| d.min(MAX_TREE_DEPTH)),
    )?;
    Ok((nodes, walk.cut))
}

struct TreeWalk<'c, 'b> {
    conn: &'c Connection,
    all: bool,
    /// The references on the path from the root to the level being listed.
    ancestors: Vec<String>,
    /// Nodes listed so far.
    listed: usize,
    /// The walk stopped early.
    cut: bool,
    budget: &'b mut Budget,
}

impl TreeWalk<'_, '_> {
    fn level(&mut self, path: Option<&str>, depth: usize) -> jkb_core::Result<Vec<TreeNode>> {
        let mut out = Vec::new();
        for child in children(self.conn, path, self.all)? {
            if self.cut || self.listed >= MAX_TREE_NODES || !self.budget.take(&child) {
                self.cut = true;
                break;
            }
            self.listed += 1;
            let descend =
                child.has_children && depth > 0 && !self.ancestors.contains(&child.reference);
            let nested = if descend {
                self.ancestors.push(child.reference.clone());
                let nested = self.level(Some(&child.reference), depth - 1);
                self.ancestors.pop();
                nested?
            } else {
                Vec::new()
            };
            out.push(TreeNode {
                child,
                children: nested,
            });
        }
        Ok(out)
    }
}

/// `kb.cat`: an item's full content (empty when it has none).
///
/// # Errors
/// [`ErrorCode::NotFound`] when no item has `uid`; else a failed read.
pub fn cat(conn: &Connection, uid: &str) -> Result<String, ApiError> {
    let Some(id) = item::id_for_uid(conn, uid)? else {
        return Err(not_found(format!("no item with uid `{uid}`")));
    };
    Ok(item::get_content(conn, id)?.unwrap_or_default())
}

/// `kb.grep`: items under `scope` whose content holds `pattern` literally, answered as `mode` asks.
/// Case folding, when asked, is Unicode — the same fold `item::grep` filters with. Items are read one
/// at a time and only what the answer keeps is held, line by line within `budget`; counting goes on
/// past it.
///
/// # Errors
/// [`ErrorCode::Invalid`] for an empty pattern, which matches every line of every item; else a
/// failed read.
pub fn grep(
    conn: &Connection,
    pattern: &str,
    scope: Option<&str>,
    ignore_case: bool,
    mode: GrepMode,
    budget: &mut Budget,
) -> Result<GrepAnswer, ApiError> {
    if pattern.is_empty() {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            "an empty pattern matches every line of every item; give grep some text",
        ));
    }
    let needle = if ignore_case {
        pattern.to_lowercase()
    } else {
        pattern.to_owned()
    };
    let matches = |line: &str| {
        if ignore_case {
            line.to_lowercase().contains(&needle)
        } else {
            line.contains(&needle)
        }
    };
    let mut answer = GrepAnswer {
        hits: Vec::new(),
        count: 0,
        truncated: false,
    };
    item::grep_each(conn, pattern, scope, ignore_case, |row| {
        answer.count += 1;
        if mode == GrepMode::Count {
            return true;
        }
        let mut hit = GrepHit {
            uid: row.uid,
            kind: row.kind,
            lines: Vec::new(),
        };
        if !budget.take(&hit) {
            return true;
        }
        if mode == GrepMode::Lines {
            for (i, text) in row.content.lines().enumerate() {
                if !matches(text) {
                    continue;
                }
                let line = GrepLine {
                    line: i + 1,
                    text: text.to_owned(),
                };
                if !budget.take(&line) {
                    break;
                }
                hit.lines.push(line);
            }
        }
        answer.hits.push(hit);
        true
    })?;
    answer.truncated = budget.exhausted();
    Ok(answer)
}

/// What `kb.search` was asked.
#[derive(Debug, Clone)]
pub struct SearchAsk {
    /// The DSL: `~"…"` is the vector term, bare words FTS.
    pub dsl: String,
    /// The scope when the DSL names none.
    pub default_scope: Option<String>,
    /// The route.
    pub route: SearchRoute,
    /// At most this many hits (≤ [`MAX_SEARCH_LIMIT`]).
    pub limit: usize,
    /// ±N neighbour chunks per hit.
    pub context: Option<usize>,
}

/// `kb.search`: hits best-first, each resolved to its item.
///
/// # Errors
/// [`ErrorCode::Unsupported`] for a route that embeds text with no `embedder`; [`ErrorCode::Invalid`]
/// for a limit over [`MAX_SEARCH_LIMIT`] or a malformed query; else a failed read.
pub fn search(
    db: &Db,
    embedder: Option<&Arc<dyn Embedder + Send + Sync>>,
    ask: &SearchAsk,
    budget: &mut Budget,
) -> Result<Vec<SearchHit>, ApiError> {
    if ask.limit > MAX_SEARCH_LIMIT {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            format!("a search limit of at most {MAX_SEARCH_LIMIT}"),
        ));
    }
    if ask.context.is_some_and(|n| n > MAX_SEARCH_CONTEXT) {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            format!("a search context of at most {MAX_SEARCH_CONTEXT} chunks either side"),
        ));
    }
    let query = scoped(&ask.dsl, ask.default_scope.as_deref())?;
    let searcher = match (ask.route, embedder) {
        (_, Some(e)) => Searcher::new(e.clone()),
        (SearchRoute::Fts, None) => Searcher::new(Arc::new(NoEmbedder)),
        (SearchRoute::Vector | SearchRoute::Hybrid, None) => {
            return Err(ApiError::with_code(
                ErrorCode::Unsupported,
                "this backend has no embedder, so it serves only --route fts: the vector and \
                 hybrid routes embed the query text, which the daemon does not do for a client",
            ))
        }
    };
    let hits = searcher
        .search(db, &query, ask.route.into(), ask.limit)
        .map_err(search_error)?;
    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        if budget.exhausted() {
            break;
        }
        let context = match ask.context {
            Some(n) => searcher
                .get_context(db, hit.item, n)
                .map_err(search_error)?
                .into_iter()
                .map(|c| ContextLine {
                    item: c.item.get(),
                    position: c.position,
                    is_hit: c.is_hit,
                    content: c.content,
                })
                .collect(),
            None => Vec::new(),
        };
        let (item, source) = (hit.item, hit.source_document);
        let (row, source_document) = db.read(move |conn| {
            let one = |id: ItemId| -> jkb_core::Result<Option<ItemRow>> {
                Ok(item_rows(conn, &[id], &mut Budget::new(usize::MAX))?
                    .into_iter()
                    .next())
            };
            Ok((one(item)?, source.map(one).transpose()?.flatten()))
        })?;
        let hit = SearchHit {
            item: hit.item.get(),
            row,
            route: hit.route.as_str().to_owned(),
            score: hit.score,
            distance: hit.distance,
            namespace: hit.namespace_path,
            source_document,
            context,
        };
        // Charged whole: a hit's context is the part that is large — a document hit, which has no
        // chunks, carries its entire body at any context.
        if !budget.take(&hit) {
            break;
        }
        out.push(hit);
    }
    Ok(out)
}

fn search_error(e: jkb_search::Error) -> ApiError {
    match e {
        jkb_search::Error::Core(core) => core.into(),
        jkb_search::Error::Types(t) => jkb_core::Error::Types(t).into(),
        jkb_search::Error::Sqlite(s) => jkb_core::Error::Sqlite(s).into(),
        other => ApiError::with_code(ErrorCode::Internal, other.to_string()),
    }
}

/// The embedder an FTS-only search is built with. `Searcher` embeds only for the vector and hybrid
/// routes, which [`search`] refuses before constructing one of these; if that ever changes this
/// refuses rather than returning a vector.
struct NoEmbedder;

impl Embedder for NoEmbedder {
    fn model(&self) -> &'static str {
        "none"
    }
    fn dim(&self) -> usize {
        0
    }
    fn embed(&self, _text: &str) -> jkb_types::Result<Vec<f32>> {
        Err(jkb_types::Error::EmbedderUnavailable(
            "no embedder: this backend serves only the fts route".to_owned(),
        ))
    }
    fn health_check(&self) -> jkb_types::Result<()> {
        self.embed("").map(|_| ())
    }
}

/// `task.show`: a task (or any item a task reference names) in full, with its recent transitions and
/// its subtasks — as many as fit `budget` once the task itself is charged. The task is shown whatever
/// its size: it is what was asked for.
///
/// # Errors
/// [`ErrorCode::NotFound`] when the reference names no item; else a failed read.
pub fn task_show(
    conn: &Connection,
    reference: &str,
    budget: &mut Budget,
) -> Result<TaskDetail, ApiError> {
    let Some(id) = task::resolve_ref(conn, reference)? else {
        return Err(not_found(format!("no item with uid {reference}")));
    };
    let Some(meta) = item::get(conn, id)? else {
        return Err(not_found(format!("no item with uid {reference}")));
    };
    let history = transition::history(conn, id)?;
    let skip = history.len().saturating_sub(RECENT_TRANSITIONS);
    let mut detail = TaskDetail {
        item: ItemDetail {
            id: id.get(),
            namespace: primary_namespace(conn, id.get())?,
            uid: meta.uid,
            kind: meta.kind,
            status: meta.status,
            priority: meta.priority,
            due: meta.due,
            content: meta.content,
            tags: tag::applications(conn, id)?
                .into_iter()
                .map(|(facet, value)| TagPair { facet, value })
                .collect(),
        },
        transitions: history
            .into_iter()
            .skip(skip)
            .map(|r| TransitionSummary {
                at: r.at,
                event: r.event,
                to: r.to_status,
                branch: r.labels.branch,
                onto: r.labels.onto,
                pr: r.labels.pr_number,
            })
            .collect(),
        subtasks: Vec::new(),
    };
    let _ = budget.take(&detail.item);
    task::subtasks_each(conn, id, |t| {
        let summary = SubtaskSummary {
            title: label(&t.uid, t.title.as_deref()),
            uid: t.uid,
            status: t.status,
        };
        if !budget.take(&summary) {
            return false;
        }
        detail.subtasks.push(summary);
        true
    })?;
    Ok(detail)
}

/// `task.subtasks`: a task's contained children, shaped like `kb.ls` children, as many as fit
/// `budget`.
///
/// # Errors
/// [`ErrorCode::NotFound`] when the reference names no item; else a failed read.
pub fn subtasks(
    conn: &Connection,
    reference: &str,
    all: bool,
    budget: &mut Budget,
) -> Result<Vec<Child>, ApiError> {
    let Some(id) = task::resolve_ref(conn, reference)? else {
        return Err(not_found(format!("no item with uid {reference}")));
    };
    Ok(contained_children(conn, id, all)?
        .into_iter()
        .take_while(|c| budget.take(c))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::rerooted;

    #[test]
    fn a_client_path_under_its_home_is_looked_up_under_the_server_s() {
        let server = Some(Path::new("/Users/u"));
        assert_eq!(
            rerooted(Path::new("/home/vscode/repos/jkb"), "/home/vscode", server),
            Path::new("/Users/u/repos/jkb")
        );
        assert_eq!(
            rerooted(Path::new("/home/vscode"), "/home/vscode", server),
            Path::new("/Users/u")
        );
        assert_eq!(
            rerooted(Path::new("/home/vscodex/repos"), "/home/vscode", server),
            Path::new("/home/vscodex/repos"),
            "a prefix of the home's NAME is not under it"
        );
        assert_eq!(
            rerooted(Path::new("/tmp/w"), "/home/vscode", server),
            Path::new("/tmp/w"),
            "outside the home: as given"
        );
        assert_eq!(
            rerooted(Path::new("/home/vscode/r"), "", server),
            Path::new("/home/vscode/r"),
            "no client home: as given"
        );
        assert_eq!(
            rerooted(Path::new("/home/vscode/r"), "/home/vscode", None),
            Path::new("/home/vscode/r"),
            "no server home: as given"
        );
    }
}
