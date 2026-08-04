//! Task DAG: first-class task ergonomics on the item substrate (design D5/D19).
//!
//! A task is an item of `kind = 'task'` whose lifecycle lives in the real `status`
//! column (`open`/`in_progress`/`done`/`cancelled`); `blocked` is **derived** — a
//! task with a `depends_on` edge to a non-`done` task — never stored, so there is a
//! single source of truth. `priority` and `due` are indexed columns (not tags), so
//! the ready frontier can order by them cheaply.
//!
//! This module is the typed repo API over that substrate: [`create`] (multi-placed,
//! bindable, with dependency edges), status/priority/due setters, the derived
//! [`is_blocked`] check, and the [`ready`] frontier. [`parse_quick_add`] turns the
//! one-line quick-add DSL (`!p<n> @<date> +<ns> #<facet>=<value> ^<uid>`) into a
//! [`QuickAdd`], which [`NewTask::from_quick_add`] lifts into a create spec with the
//! default home (`tasks/inbox`) and `managed:` binding.

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::{EdgeType, Error as TypeError, ItemId, NamespaceId, PlacementRole, TaskStatus};

use crate::dsl::{has_unterminated_quote, tokenize_escaped, unquote_unescape};
use crate::query::{Query, Scope, TagPred};
use crate::store::WriteMeta;
use crate::{binding, changelog, edge, item, ns, placement, tag, Error, Result};

/// The tasks root: a **reserved root** of the fixed D32 layout, not something discovered.
///
/// It is a literal because the layout says so. Resolving it from the namespace *type*
/// system was tried and removed (design D33.5): that made a type mean both "what may live
/// here" (naturally many-to-many) and "where this subsystem points" (singular), which then
/// needed a uniqueness guard, an escape hatch and a re-seeding guard to hold together. The
/// `tasks` contract says only what may live under this root.
pub const DEFAULT_ROOT: &str = "tasks";
/// The default logical home a task is placed under when none is given (design D19).
pub const DEFAULT_HOME: &str = "tasks/inbox";
/// The default binding for a new task: not written to any repo (design D19/D3).
pub const MANAGED_BINDING: &str = "managed:";

/// The longest slug prefix carried into a minted `task:` uid before the suffix.
const UID_SLUG_MAX: usize = 32;

/// Mint a unique-ish `task:<slug>-<nanos>` uid from a `title`. The slug is the shared
/// [`crate::dsl::slug`] (unicode-aware, so the CLI, the MCP server, and file sync all
/// derive the same slug from the same title), capped at [`UID_SLUG_MAX`] characters and
/// falling back to `task` when the title has no alphanumerics; the suffix is a
/// hex nanosecond timestamp for uniqueness across calls.
///
/// This is the single minting entry point — do not re-implement it at the CLI or MCP edge.
#[must_use]
pub fn mint_uid(title: &str) -> String {
    let slug: String = crate::dsl::slug(title).chars().take(UID_SLUG_MAX).collect();
    let slug = if slug.is_empty() { "task" } else { &slug };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("task:{slug}-{nanos:x}")
}

/// A specification for creating a task. Build it directly (fields are public) or via
/// [`NewTask::new`] / [`NewTask::from_quick_add`] for sane defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTask {
    /// Stable string identity (e.g. `task:fix-flaky-test`).
    pub uid: String,
    /// The task's title, stored as the item's `content`.
    pub title: String,
    /// Optional priority (lower is more important; `!p1` is above `!p2`).
    pub priority: Option<i64>,
    /// Optional ISO due date (e.g. `2026-07-15`).
    pub due: Option<String>,
    /// The primary/home namespace path (default [`DEFAULT_HOME`]).
    pub home: String,
    /// Additional `reference` placements (e.g. a `repos/…` mirror).
    pub mirrors: Vec<String>,
    /// `facet=value` tags to apply.
    pub tags: Vec<(String, String)>,
    /// Uids of tasks this one `depends_on` (dependency edges; cycles are rejected).
    pub depends_on: Vec<String>,
    /// The storage binding uri (default [`MANAGED_BINDING`]).
    pub binding: String,
}

impl NewTask {
    /// A task with the given `uid` and `title` and all defaults (home
    /// [`DEFAULT_HOME`], [`MANAGED_BINDING`] binding, no priority/due/tags/deps).
    #[must_use]
    pub fn new(uid: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            uid: uid.into(),
            title: title.into(),
            priority: None,
            due: None,
            home: DEFAULT_HOME.to_owned(),
            mirrors: Vec::new(),
            tags: Vec::new(),
            depends_on: Vec::new(),
            binding: MANAGED_BINDING.to_owned(),
        }
    }

    /// Lift a parsed [`QuickAdd`] into a create spec (design D26.1). The **first**
    /// `+<ns>` placement becomes the Primary `home`; any further placements stay
    /// `mirrors` (Reference). [`DEFAULT_HOME`] is used as the home only when no placement
    /// was given. Binding defaults to [`MANAGED_BINDING`]. `uid` is supplied by the caller
    /// (the CLI derives one).
    #[must_use]
    pub fn from_quick_add(uid: impl Into<String>, qa: QuickAdd) -> Self {
        let (home, mirrors) = match qa.placements.split_first() {
            Some((first, rest)) => (first.clone(), rest.to_vec()),
            None => (DEFAULT_HOME.to_owned(), Vec::new()),
        };
        Self {
            uid: uid.into(),
            title: qa.title,
            priority: qa.priority,
            due: qa.due,
            home,
            mirrors,
            tags: qa.tags,
            depends_on: qa.depends_on,
            binding: MANAGED_BINDING.to_owned(),
        }
    }
}

/// One row of the ready frontier / a task listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    /// The item id.
    pub id: ItemId,
    /// The stable uid.
    pub uid: String,
    /// The task title (item `content`).
    pub title: Option<String>,
    /// The current status.
    pub status: Option<String>,
    /// The priority, if set.
    pub priority: Option<i64>,
    /// The due date, if set.
    pub due: Option<String>,
}

/// Create a task: insert the item (status `open`), place it under its home (and any
/// mirrors), set its binding, apply tags, and link its `depends_on` dependencies.
///
/// # Errors
/// Returns a validation error if a named dependency uid does not exist or a
/// dependency edge would create a cycle; otherwise a database error.
pub fn create(conn: &Connection, meta: &WriteMeta, task: &NewTask) -> Result<ItemId> {
    let status = TaskStatus::Open;
    let id: i64 = conn
        .prepare_cached(
            "INSERT INTO items (uid, kind, content, status, priority, due)
             VALUES (?1, 'task', ?2, ?3, ?4, ?5) RETURNING id",
        )?
        .query_row(
            params![
                task.uid,
                task.title,
                status.as_str(),
                task.priority,
                task.due
            ],
            |row| row.get(0),
        )?;
    let item_id = ItemId::new(id);
    changelog::append(
        conn,
        meta,
        "insert",
        "items",
        &id.to_string(),
        None,
        Some(&json!({
            "uid": task.uid,
            "kind": "task",
            "status": status.as_str(),
            "priority": task.priority,
            "due": task.due,
        })),
    )?;

    let home = ns::ensure(conn, &task.home)?;
    placement::place(conn, meta, item_id, home, PlacementRole::Primary, 0)?;
    for mirror in &task.mirrors {
        let ns_id = ns::ensure(conn, mirror)?;
        placement::place(conn, meta, item_id, ns_id, PlacementRole::Reference, 0)?;
    }

    binding::set(conn, meta, item_id, &task.binding, None, None)?;

    for (facet, value) in &task.tags {
        tag::apply(conn, meta, item_id, facet, value)?;
    }
    for dep_uid in &task.depends_on {
        add_dependency(conn, meta, item_id, dep_uid)?;
    }
    ensure_task_mirror(conn, meta, item_id, &task.home)?;
    Ok(item_id)
}

/// The `tasks/` mirror namespace for a task homed at `home`, or `None` if `home` is already
/// under `tasks/`. A leading `repos/` is stripped so `repos/jkb/openspec/x` mirrors at
/// `tasks/jkb/openspec/x`, keeping all of a repo's tasks under `tasks/<repo>/**`.
fn tasks_mirror_ns(home: &str) -> Option<String> {
    if home == DEFAULT_ROOT || home.starts_with(&format!("{DEFAULT_ROOT}/")) {
        return None;
    }
    let rest = home.strip_prefix("repos/").unwrap_or(home);
    Some(format!("{DEFAULT_ROOT}/{rest}"))
}

/// Ensure a task homed at `home` (its Primary namespace) has a `tasks/…` Reference mirror
/// (a symbolic link) when `home` is outside `tasks/`, so `tasks/**` is the complete task
/// index. Idempotent; returns whether a mirror was added.
///
/// # Errors
/// Returns an error if a query or placement fails.
pub fn ensure_task_mirror(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    home: &str,
) -> Result<bool> {
    let Some(mirror) = tasks_mirror_ns(home) else {
        return Ok(false);
    };
    let ns_id = ns::ensure(conn, &mirror)?;
    let exists = conn
        .prepare_cached(
            "SELECT 1 FROM placements WHERE item_id = ?1 AND namespace_id = ?2 LIMIT 1",
        )?
        .query_row(params![item.get(), ns_id.get()], |_| Ok(true))
        .optional()?
        .unwrap_or(false);
    if exists {
        return Ok(false);
    }
    placement::place(conn, meta, item, ns_id, PlacementRole::Reference, 0)?;
    Ok(true)
}

/// Re-home a task: make `namespace` its sole primary placement (via
/// [`placement::set_primary`]) and reconcile the auto-managed `tasks/…` mirror so it tracks
/// the new home. The stale mirror for the *previous* home ([`tasks_mirror_ns`]) is unplaced
/// and a fresh mirror for the new home is ensured, so `tasks/**` never surfaces a task under
/// a home it no longer has (design D26). Non-task items and moves within `tasks/` are
/// re-homed without touching mirrors. Idempotent when the home is unchanged.
///
/// # Errors
/// Returns an error if a query, placement, or changelog append fails.
pub fn set_primary_home(
    conn: &Connection,
    meta: &WriteMeta,
    item: ItemId,
    namespace: NamespaceId,
    position: i64,
) -> Result<()> {
    // Capture the pre-move primary home and kind before `set_primary` rewrites the placement.
    let old_home: Option<String> = conn
        .prepare_cached(
            "SELECT n.path FROM placements p
             JOIN namespaces n ON n.id = p.namespace_id
             WHERE p.item_id = ?1 AND p.role = 'primary' LIMIT 1",
        )?
        .query_row([item.get()], |r| r.get(0))
        .optional()?;
    let is_task: bool = conn
        .prepare_cached("SELECT kind = 'task' FROM items WHERE id = ?1")?
        .query_row([item.get()], |r| r.get(0))?;

    placement::set_primary(conn, meta, item, namespace, position)?;

    // Only tasks carry a `tasks/…` mirror; other kinds are simply re-homed.
    if !is_task {
        return Ok(());
    }
    let new_home: String = conn
        .prepare_cached("SELECT path FROM namespaces WHERE id = ?1")?
        .query_row([namespace.get()], |r| r.get(0))?;
    let new_mirror = tasks_mirror_ns(&new_home);
    // Drop the previous home's auto-mirror when it no longer matches the current home.
    if let Some(old_home) = old_home {
        if let Some(old_mirror) = tasks_mirror_ns(&old_home) {
            if new_mirror.as_ref() != Some(&old_mirror) {
                if let Some(ns_id) = ns::get(conn, &old_mirror)? {
                    placement::unplace(conn, meta, item, ns_id)?;
                }
            }
        }
    }
    ensure_task_mirror(conn, meta, item, &new_home)?;
    Ok(())
}

/// Ensure every task whose Primary home is outside `tasks/` has a `tasks/…` mirror. Backs
/// the sync-time reconcile (file-backed tasks under `repos/<repo>/…`) and the one-shot
/// `jkb task mirror`. Idempotent; returns the number of mirrors added.
///
/// # Errors
/// Returns an error if a query or placement fails.
pub fn ensure_all_mirrors(conn: &Connection, meta: &WriteMeta) -> Result<usize> {
    let tasks: Vec<(ItemId, String)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT i.id, n.path FROM items i
             JOIN placements p ON p.item_id = i.id AND p.role = 'primary'
             JOIN namespaces n ON n.id = p.namespace_id
             WHERE i.kind = 'task'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((ItemId::new(r.get::<_, i64>(0)?), r.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut added = 0;
    for (item, home) in tasks {
        if ensure_task_mirror(conn, meta, item, &home)? {
            added += 1;
        }
    }
    Ok(added)
}

/// Make `child` a subtask of `parent` (a `parent_of` edge, parent -> child).
///
/// A parent with any non-terminal child leaves the ready frontier (design D34.3), so this
/// is how a task too big for one branch is split into the pieces that actually get worked.
/// Cycle-guarded by [`edge::link`], so a task cannot become its own ancestor.
///
/// # Errors
/// Returns a validation error if the edge would create a cycle; otherwise a database error.
pub fn add_subtask(
    conn: &Connection,
    meta: &WriteMeta,
    parent: ItemId,
    child: ItemId,
) -> Result<()> {
    edge::link(conn, meta, parent, child, EdgeType::ParentOf, None)?;
    Ok(())
}

/// The direct subtasks of `parent`, ordered by priority (asc, nulls last) then uid — the
/// same ordering [`ready`] uses, so the list reads as a work queue.
///
/// # Errors
/// Returns an error if the query fails.
pub fn subtasks(conn: &Connection, parent: ItemId) -> Result<Vec<TaskRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.id, i.uid, i.content, i.status, i.priority, i.due
           FROM edges e JOIN items i ON i.id = e.dst_item_id
          WHERE e.src_item_id = ?1 AND e.type = 'parent_of'
          ORDER BY i.priority IS NULL, i.priority, i.uid",
    )?;
    let rows = stmt.query_map([parent.get()], |r| {
        Ok(TaskRow {
            id: ItemId::new(r.get(0)?),
            uid: r.get(1)?,
            title: r.get(2)?,
            status: r.get(3)?,
            priority: r.get(4)?,
            due: r.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Subtask counts for many parents at once, as `parent -> (total, open)`; parents with no
/// subtasks are absent.
///
/// Batched because the tree asks this for every item it lists — one query per row is the
/// N+1 the listing already avoids for namespace leaf counts.
///
/// # Errors
/// Returns an error if the query fails.
pub fn subtask_counts(
    conn: &Connection,
    parents: &[ItemId],
) -> Result<std::collections::HashMap<ItemId, (i64, i64)>> {
    let mut out = std::collections::HashMap::new();
    if parents.is_empty() {
        return Ok(out);
    }
    let placeholders = (1..=parents.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    // Placeholders are generated from a count; every value is bound.
    let sql = format!(
        "SELECT e.src_item_id,
                COUNT(*),
                SUM(CASE WHEN c.status IS NOT 'done' AND c.status IS NOT 'cancelled'
                         THEN 1 ELSE 0 END)
           FROM edges e JOIN items c ON c.id = e.dst_item_id
          WHERE e.type = 'parent_of' AND e.src_item_id IN ({placeholders})
          GROUP BY e.src_item_id"
    );
    let params: Vec<rusqlite::types::Value> = parents.iter().map(|p| p.get().into()).collect();
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params_from_iter(params), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (id, total, open) = row?;
        out.insert(ItemId::new(id), (total, open));
    }
    Ok(out)
}

/// Whether every subtask of `parent` has reached a terminal status (`done`/`cancelled`).
/// True when there are no subtasks at all — a leaf is trivially complete.
///
/// This is the second half of the auto-close rule (design D34.4): a merged branch is
/// evidence the work landed, but a task with unfinished children did not finish.
///
/// # Errors
/// Returns an error if the query fails.
pub fn subtasks_all_terminal(conn: &Connection, parent: ItemId) -> Result<bool> {
    let open: i64 = conn
        .prepare_cached(
            "SELECT count(*) FROM edges e JOIN items c ON c.id = e.dst_item_id
              WHERE e.src_item_id = ?1 AND e.type = 'parent_of'
                AND c.status IS NOT 'done' AND c.status IS NOT 'cancelled'",
        )?
        .query_row([parent.get()], |r| r.get(0))?;
    Ok(open == 0)
}

/// Add a `depends_on` edge from `task` to the task with uid `dep_uid`. The edge is
/// rejected if it would introduce a cycle (via [`edge::link`]).
///
/// # Errors
/// Returns a validation error if `dep_uid` names no item or the edge would create a
/// cycle; otherwise a database error.
pub fn add_dependency(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    dep_uid: &str,
) -> Result<()> {
    let dep = item::id_for_uid(conn, dep_uid)?
        .ok_or_else(|| Error::Types(TypeError::NotFound(format!("dependency task `{dep_uid}`"))))?;
    edge::link(conn, meta, task, dep, EdgeType::DependsOn, None)?;
    Ok(())
}

/// Transition `task` to a new [`TaskStatus`], recording the change in the changelog.
/// `blocked` is unrepresentable here — it is derived, never set (design D19).
///
/// # Errors
/// Returns [`jkb_types::Error::NotFound`] if `task` does not exist; otherwise a
/// database error.
pub fn set_status(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    status: TaskStatus,
) -> Result<()> {
    let before: Option<String> = conn
        .prepare_cached("SELECT status FROM items WHERE id = ?1")?
        .query_row([task.get()], |row| row.get::<_, Option<String>>(0))
        .optional()?
        .ok_or_else(|| Error::Types(TypeError::NotFound(format!("task {task}"))))?;
    conn.prepare_cached(
        "UPDATE items SET status = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?1",
    )?
    .execute(params![task.get(), status.as_str()])?;
    changelog::append(
        conn,
        meta,
        "update",
        "items",
        &task.get().to_string(),
        Some(&json!({ "status": before })),
        Some(&json!({ "status": status.as_str() })),
    )?;
    // A terminal task holds no claim. Claiming marks work in flight, and finished work is
    // not in flight — so the claim is cleared here rather than at each caller, which is
    // what stops a completed task turning up later as an orphan in `jkb doctor`. Cleared
    // regardless of holder (unlike `claim::release`) because with the work over there is
    // nothing left to hand out and no one to protect it from.
    if status.is_terminal() {
        crate::claim::clear(conn, meta, task)?;
    }
    Ok(())
}

/// Transition `task` to the status named by `status`, parsed from a manual string.
///
/// This is the boundary that enforces design D19: `blocked` (and any unknown
/// string) is **rejected** — blockedness is derived from `depends_on` edges.
///
/// # Errors
/// Returns a validation error if `status` is not a settable status
/// (`open`/`in_progress`/`needs_review`/`done`/`cancelled`), or the errors of
/// [`set_status`].
pub fn set_status_str(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    status: &str,
) -> Result<()> {
    let parsed = TaskStatus::from_manual_str(status).ok_or_else(|| {
        Error::Types(TypeError::Validation(format!(
            "cannot set status `{status}`: settable statuses are open, in_progress, \
             needs_review, done, cancelled (blocked is derived from depends_on edges, not set)"
        )))
    })?;
    set_status(conn, meta, task, parsed)
}

/// Set (or clear, with `None`) `task`'s priority.
///
/// # Errors
/// Returns [`jkb_types::Error::NotFound`] if `task` does not exist; otherwise a
/// database error.
pub fn set_priority(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    priority: Option<i64>,
) -> Result<()> {
    let before: Option<i64> = conn
        .prepare_cached("SELECT priority FROM items WHERE id = ?1")?
        .query_row([task.get()], |row| row.get::<_, Option<i64>>(0))
        .optional()?
        .ok_or_else(|| Error::Types(TypeError::NotFound(format!("task {task}"))))?;
    conn.prepare_cached(
        "UPDATE items SET priority = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?1",
    )?
    .execute(params![task.get(), priority])?;
    changelog::append(
        conn,
        meta,
        "update",
        "items",
        &task.get().to_string(),
        Some(&json!({ "priority": before })),
        Some(&json!({ "priority": priority })),
    )?;
    Ok(())
}

/// Set (or clear, with `None`) `task`'s due date.
///
/// # Errors
/// Returns [`jkb_types::Error::NotFound`] if `task` does not exist; otherwise a
/// database error.
pub fn set_due(conn: &Connection, meta: &WriteMeta, task: ItemId, due: Option<&str>) -> Result<()> {
    let before: Option<String> = conn
        .prepare_cached("SELECT due FROM items WHERE id = ?1")?
        .query_row([task.get()], |row| row.get::<_, Option<String>>(0))
        .optional()?
        .ok_or_else(|| Error::Types(TypeError::NotFound(format!("task {task}"))))?;
    conn.prepare_cached(
        "UPDATE items SET due = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?1",
    )?
    .execute(params![task.get(), due])?;
    changelog::append(
        conn,
        meta,
        "update",
        "items",
        &task.get().to_string(),
        Some(&json!({ "due": before })),
        Some(&json!({ "due": due })),
    )?;
    Ok(())
}

/// Whether `task` is **blocked**: it has a `depends_on` edge to a task that is not yet
/// settled — i.e. not terminal (`done`/`cancelled`; see
/// `TaskStatus::unblocks_dependents`). This is the derived state that stands in for a
/// stored `blocked` status (design D19); it mirrors the anti-join in [`crate::query`]'s
/// `is:ready`. A cancelled dependency will never complete, so it unblocks; a
/// `needs_review` dependency is not yet landed and may bounce back, so it blocks
/// (design D27.7).
///
/// # Errors
/// Returns an error if the query fails.
pub fn is_blocked(conn: &Connection, task: ItemId) -> Result<bool> {
    let hit: Option<i64> = conn
        .prepare_cached(
            "SELECT 1 FROM edges e JOIN items d ON e.dst_item_id = d.id
             WHERE e.src_item_id = ?1 AND e.type = 'depends_on'
               AND d.status IS NOT 'done' AND d.status IS NOT 'cancelled'
             LIMIT 1",
        )?
        .query_row([task.get()], |row| row.get(0))
        .optional()?;
    Ok(hit.is_some())
}

/// The **ready frontier**: tasks (`kind = 'task'`) whose status is non-terminal and
/// which have no `depends_on` edge to a non-`done` task, optionally narrowed to a
/// `scope` and `tags`. Ordered by priority (ascending, nulls last) then due date
/// (ascending, nulls last).
///
/// The structural filter (scope/tags + the ready anti-join) is delegated to
/// [`Query::evaluate`] so there is one source of truth for readiness; this only adds
/// the task-specific ordering over the resulting ids.
///
/// # Errors
/// Returns an error if evaluation or the ordered load fails.
pub fn ready(conn: &Connection, scope: Scope, tags: &[TagPred]) -> Result<Vec<TaskRow>> {
    let query = Query {
        kind: Some("task".to_owned()),
        ready: true,
        scope,
        tags: tags.to_vec(),
        ..Query::default()
    };
    let ids = query.evaluate(conn)?;
    load_ordered(conn, &ids)
}

/// Load the given items as [`TaskRow`]s, ordered by priority then due (nulls last).
fn load_ordered(conn: &Connection, ids: &[ItemId]) -> Result<Vec<TaskRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT id, uid, content, status, priority, due FROM items
         WHERE id IN ({placeholders})
         ORDER BY priority IS NULL, priority ASC, due IS NULL, date(due) ASC, id"
    );
    let params: Vec<Value> = ids.iter().map(|id| Value::Integer(id.get())).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), |row| {
            Ok(TaskRow {
                id: ItemId::new(row.get(0)?),
                uid: row.get(1)?,
                title: row.get(2)?,
                status: row.get(3)?,
                priority: row.get(4)?,
                due: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The parsed result of the quick-add DSL. Placements/tags/deps preserve their order
/// of appearance; the title is the concatenation of the non-modifier words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuickAdd {
    /// The task title (all bare/quoted words, space-joined).
    pub title: String,
    /// `!p<n>` — priority.
    pub priority: Option<i64>,
    /// `@<date>` — due date.
    pub due: Option<String>,
    /// `+<ns>` — additional (reference) placements.
    pub placements: Vec<String>,
    /// `#<facet>=<value>` — tags.
    pub tags: Vec<(String, String)>,
    /// `^<uid>` — dependency uids.
    pub depends_on: Vec<String>,
}

/// Parse the quick-add DSL: `<title words> !p<n> @<date> +<ns> #<facet>=<value>
/// ^<uid>` (modifiers in any order; `"…"` groups spaces in the title). Mirrors the
/// quote-aware tokenizer of the query DSL (see [`crate::query`]).
///
/// # Errors
/// Returns a validation error naming the offending token if a modifier is malformed,
/// or if no title words are present.
pub fn parse_quick_add(input: &str) -> Result<QuickAdd> {
    let mut qa = QuickAdd::default();
    let mut title_words: Vec<String> = Vec::new();

    if has_unterminated_quote(input) {
        return Err(bad(input, "unterminated `\"` quote"));
    }
    for token in tokenize_escaped(input) {
        if let Some(v) = token.strip_prefix("!p") {
            qa.priority = Some(
                v.parse::<i64>()
                    .map_err(|_| bad(&token, "`!p` takes an integer, e.g. `!p1`"))?,
            );
        } else if let Some(v) = token.strip_prefix('@') {
            if v.is_empty() {
                return Err(bad(&token, "`@` takes a date, e.g. `@2026-07-15`"));
            }
            qa.due = Some(v.to_owned());
        } else if let Some(v) = token.strip_prefix('+') {
            if v.is_empty() {
                return Err(bad(&token, "`+` takes a namespace, e.g. `+repos/app`"));
            }
            qa.placements.push(v.to_owned());
        } else if let Some(v) = token.strip_prefix('#') {
            let (facet, value) = v
                .split_once('=')
                .ok_or_else(|| bad(&token, "`#` takes facet=value, e.g. `#size=small`"))?;
            if facet.is_empty() || value.is_empty() {
                return Err(bad(&token, "`#` takes a non-empty facet and value"));
            }
            qa.tags.push((facet.to_owned(), value.to_owned()));
        } else if let Some(v) = token.strip_prefix('^') {
            if v.is_empty() {
                return Err(bad(&token, "`^` takes a dependency uid, e.g. `^task:abc`"));
            }
            qa.depends_on.push(v.to_owned());
        } else {
            title_words.push(unquote_unescape(&token));
        }
    }

    qa.title = title_words.join(" ");
    if qa.title.is_empty() {
        return Err(bad(input, "a task needs a title"));
    }
    Ok(qa)
}

/// Build an actionable parse error naming the offending token.
fn bad(token: &str, expected: &str) -> Error {
    Error::Types(TypeError::Validation(format!(
        "invalid quick-add token `{token}`: {expected}"
    )))
}

#[cfg(test)]
mod tests {
    /// A finished task must not keep a claim. `jkb task start` claims and
    /// `jkb task close-merged` completes, so without this every auto-closed task turned up
    /// as an orphaned claim in `doctor` — the manual cleanup that automation was removing.
    #[test]
    fn completing_a_task_clears_its_claim() {
        use crate::claim;
        let db = crate::Db::open_in_memory().unwrap();
        let id = db
            .write_txn("t", |conn, meta| {
                let id = super::create(conn, meta, &super::NewTask::new("task:c", "claimed"))?;
                claim::claim(conn, meta, id, "host:1")?;
                Ok(id)
            })
            .unwrap();
        let held = |want: jkb_types::ItemId| {
            db.read(claim::claimed)
                .unwrap()
                .into_iter()
                .any(|c| c.id == want)
        };
        assert!(held(id));

        // Terminal in either direction clears it...
        db.write_txn("t", move |conn, meta| {
            super::set_status(conn, meta, id, jkb_types::TaskStatus::Done)
        })
        .unwrap();
        assert!(!held(id), "a done task must not still be claimed");

        // ...while a non-terminal transition leaves an in-flight claim alone.
        let id2 = db
            .write_txn("t", |conn, meta| {
                let id = super::create(conn, meta, &super::NewTask::new("task:d", "wip"))?;
                claim::claim(conn, meta, id, "host:2")?;
                super::set_status(conn, meta, id, jkb_types::TaskStatus::NeedsReview)?;
                Ok(id)
            })
            .unwrap();
        assert!(held(id2), "needs_review is not terminal — the claim stays");
    }

    use super::{
        add_dependency, create, is_blocked, mint_uid, parse_quick_add, ready, set_due,
        set_primary_home, set_priority, set_status_str, NewTask, QuickAdd,
    };
    use crate::query::Scope;
    use crate::{binding, item, ns, Db};
    use jkb_types::TaskStatus;

    fn uids(rows: &[super::TaskRow]) -> Vec<String> {
        rows.iter().map(|r| r.uid.clone()).collect()
    }

    #[test]
    fn mint_uid_slugs_the_title_with_a_task_prefix() {
        let uid = mint_uid("Fix the flaky test");
        assert!(uid.starts_with("task:fix-the-flaky-test-"), "{uid}");
        // The slug is the shared unicode-aware slug (matches the file-sync path).
        assert!(mint_uid("Café résumé").starts_with("task:café-résumé-"));
        // No alphanumerics → the `task` fallback, still unique via the suffix.
        assert!(mint_uid("!!! ???").starts_with("task:task-"));
    }

    #[test]
    fn quick_add_parses_every_modifier() {
        let qa = parse_quick_add(
            "fix flaky test !p1 @2026-07-15 +repos/monorepo/backend #size=small ^task:dep",
        )
        .unwrap();
        assert_eq!(qa.title, "fix flaky test");
        assert_eq!(qa.priority, Some(1));
        assert_eq!(qa.due.as_deref(), Some("2026-07-15"));
        assert_eq!(qa.placements, vec!["repos/monorepo/backend".to_owned()]);
        assert_eq!(qa.tags, vec![("size".to_owned(), "small".to_owned())]);
        assert_eq!(qa.depends_on, vec!["task:dep".to_owned()]);
    }

    #[test]
    fn quick_add_quotes_and_errors() {
        assert_eq!(
            parse_quick_add("\"exact title\" !p2").unwrap().title,
            "exact title"
        );
        assert!(parse_quick_add("!px review").is_err());
        assert!(parse_quick_add("#nofacet review").is_err());
        assert!(parse_quick_add("!p1 @2026-01-01").is_err()); // no title
    }

    #[test]
    fn quick_add_rejects_unterminated_quote() {
        let err = parse_quick_add("fix bug \"quoted title").unwrap_err();
        assert!(err.to_string().contains("unterminated"), "{err}");
    }

    #[test]
    fn from_quick_add_homes_first_placement_rest_mirror() {
        // No placement → the default inbox is the home, no mirrors (design D26.1).
        let t = NewTask::from_quick_add("task:a", parse_quick_add("capture this").unwrap());
        assert_eq!(t.home, super::DEFAULT_HOME);
        assert!(t.mirrors.is_empty());

        // `+a` alone → home `a`, no forced inbox mirror.
        let t = NewTask::from_quick_add("task:b", parse_quick_add("do it +a").unwrap());
        assert_eq!(t.home, "a");
        assert!(t.mirrors.is_empty());

        // `+a +b` → home `a`, mirror `b`.
        let t = NewTask::from_quick_add("task:c", parse_quick_add("do it +a +b").unwrap());
        assert_eq!(t.home, "a");
        assert_eq!(t.mirrors, vec!["b".to_owned()]);
    }

    #[test]
    fn create_places_home_primary_and_mirrors_reference() {
        let db = Db::open_in_memory().unwrap();
        let qa = parse_quick_add("do it +proj/x +repos/y").unwrap();
        let id = db
            .write_txn("t", move |conn, meta| {
                create(conn, meta, &NewTask::from_quick_add("task:h", qa))
            })
            .unwrap();

        let roles = db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT n.path, p.role FROM placements p
                     JOIN namespaces n ON n.id = p.namespace_id
                     WHERE p.item_id = ?1 ORDER BY n.path",
                )?;
                let rows = stmt
                    .query_map([id.get()], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .unwrap();

        assert_eq!(
            roles,
            vec![
                ("proj/x".to_owned(), "primary".to_owned()),
                ("repos/y".to_owned(), "reference".to_owned()),
                // A task homed outside `tasks/` is auto-mirrored so `tasks/**` indexes it.
                ("tasks/proj/x".to_owned(), "reference".to_owned()),
            ]
        );
    }

    #[test]
    fn rehoming_a_task_moves_its_tasks_mirror() {
        let db = Db::open_in_memory().unwrap();
        let qa = parse_quick_add("do it +repos/jkb/a").unwrap();
        let id = db
            .write_txn("t", move |conn, meta| {
                create(conn, meta, &NewTask::from_quick_add("task:h", qa))
            })
            .unwrap();

        // Re-home to a different repo subtree; the stale `tasks/jkb/a` mirror must go.
        db.write_txn("t", move |conn, meta| {
            let dst = ns::ensure(conn, "repos/jkb/b")?;
            set_primary_home(conn, meta, id, dst, 0)
        })
        .unwrap();

        let roles = db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT n.path, p.role FROM placements p
                     JOIN namespaces n ON n.id = p.namespace_id
                     WHERE p.item_id = ?1 ORDER BY n.path",
                )?;
                let rows = stmt
                    .query_map([id.get()], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .unwrap();

        assert_eq!(
            roles,
            vec![
                ("repos/jkb/b".to_owned(), "primary".to_owned()),
                // The mirror tracks the new home; the old `tasks/jkb/a` mirror is gone.
                ("tasks/jkb/b".to_owned(), "reference".to_owned()),
            ]
        );
    }

    #[test]
    fn create_defaults_are_managed_under_inbox() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .write_txn("t", |conn, meta| {
                create(conn, meta, &NewTask::new("task:a", "write docs"))
            })
            .unwrap();

        // Managed binding — no repo file is ever written (scenario in the spec).
        let b = db
            .read(move |conn| binding::get(conn, id))
            .unwrap()
            .unwrap();
        assert_eq!(b.uri, "managed:");

        // Placed under the default home and defaults to `open`.
        let row = db
            .read(move |conn| {
                Ok(
                    conn.query_row("SELECT status FROM items WHERE id = ?1", [id.get()], |r| {
                        r.get::<_, String>(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(row, "open");
    }

    #[test]
    fn blocked_status_is_not_settable() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .write_txn("t", |conn, meta| {
                create(conn, meta, &NewTask::new("task:a", "a"))
            })
            .unwrap();

        let err = db.write_txn("t", move |conn, meta| {
            set_status_str(conn, meta, id, "blocked")
        });
        assert!(err.is_err());

        // A real manual status is accepted.
        db.write_txn("t", move |conn, meta| {
            set_status_str(conn, meta, id, "in_progress")
        })
        .unwrap();
    }

    #[test]
    fn dependency_cycle_is_rejected() {
        let db = Db::open_in_memory().unwrap();
        let (a, _b) = db
            .write_txn("t", |conn, meta| {
                let a = create(conn, meta, &NewTask::new("task:a", "a"))?;
                let mut b_spec = NewTask::new("task:b", "b");
                b_spec.depends_on = vec!["task:a".to_owned()]; // b depends on a
                let b = create(conn, meta, &b_spec)?;
                Ok((a, b))
            })
            .unwrap();

        // a depends_on b would close the cycle a -> b -> a.
        let cycle = db.write_txn("t", move |conn, meta| {
            add_dependency(conn, meta, a, "task:b")
        });
        assert!(cycle.is_err());
    }

    #[test]
    fn task_becomes_ready_when_dependency_completes() {
        let db = Db::open_in_memory().unwrap();
        let (a, b) = db
            .write_txn("t", |conn, meta| {
                let b = create(conn, meta, &NewTask::new("task:b", "b"))?;
                let mut a_spec = NewTask::new("task:a", "a");
                a_spec.depends_on = vec!["task:b".to_owned()];
                let a = create(conn, meta, &a_spec)?;
                Ok((a, b))
            })
            .unwrap();

        // While b is open: a is blocked, only b is ready.
        assert!(db.read(move |conn| is_blocked(conn, a)).unwrap());
        let frontier = db.read(|conn| ready(conn, Scope::All, &[])).unwrap();
        assert_eq!(uids(&frontier), vec!["task:b".to_owned()]);

        // Complete b.
        db.write_txn("t", move |conn, meta| set_status_str(conn, meta, b, "done"))
            .unwrap();

        // Now a is unblocked and ready; b (done) drops out.
        assert!(!db.read(move |conn| is_blocked(conn, a)).unwrap());
        let frontier = db.read(|conn| ready(conn, Scope::All, &[])).unwrap();
        assert_eq!(uids(&frontier), vec!["task:a".to_owned()]);
    }

    #[test]
    fn cancelled_dependency_unblocks_its_dependents() {
        let db = Db::open_in_memory().unwrap();
        let (a, b) = db
            .write_txn("t", |conn, meta| {
                let b = create(conn, meta, &NewTask::new("task:b", "b"))?;
                let mut a_spec = NewTask::new("task:a", "a");
                a_spec.depends_on = vec!["task:b".to_owned()];
                let a = create(conn, meta, &a_spec)?;
                Ok((a, b))
            })
            .unwrap();

        // While b is open, a is blocked.
        assert!(db.read(move |conn| is_blocked(conn, a)).unwrap());

        // Cancelling b (it will never complete) unblocks a — a becomes ready, and b
        // (terminal) is not itself ready.
        db.write_txn("t", move |conn, meta| {
            set_status_str(conn, meta, b, "cancelled")
        })
        .unwrap();
        assert!(!db.read(move |conn| is_blocked(conn, a)).unwrap());
        let frontier = db.read(|conn| ready(conn, Scope::All, &[])).unwrap();
        assert_eq!(uids(&frontier), vec!["task:a".to_owned()]);
    }

    #[test]
    fn needs_review_dependency_still_blocks_its_dependents() {
        let db = Db::open_in_memory().unwrap();
        let (a, b) = db
            .write_txn("t", |conn, meta| {
                let b = create(conn, meta, &NewTask::new("task:b", "b"))?;
                let mut a_spec = NewTask::new("task:a", "a");
                a_spec.depends_on = vec!["task:b".to_owned()];
                let a = create(conn, meta, &a_spec)?;
                Ok((a, b))
            })
            .unwrap();

        // While b is open, a is blocked.
        assert!(db.read(move |conn| is_blocked(conn, a)).unwrap());

        // Move b to needs_review (a reviewer is reviewing): its work is NOT yet landed on
        // the feature branch and may bounce back, so a stays blocked (design D27.7). b
        // itself, being non-terminal and unclaimed, is in the frontier — but a is not.
        db.write_txn("t", move |conn, meta| {
            set_status_str(conn, meta, b, "needs_review")
        })
        .unwrap();
        assert!(db.read(move |conn| is_blocked(conn, a)).unwrap());
        let frontier = db.read(|conn| ready(conn, Scope::All, &[])).unwrap();
        assert_eq!(uids(&frontier), vec!["task:b".to_owned()]);

        // Only once b lands (`done`) does a unblock.
        db.write_txn("t", move |conn, meta| set_status_str(conn, meta, b, "done"))
            .unwrap();
        assert!(!db.read(move |conn| is_blocked(conn, a)).unwrap());
        let frontier = db.read(|conn| ready(conn, Scope::All, &[])).unwrap();
        assert_eq!(uids(&frontier), vec!["task:a".to_owned()]);
    }

    #[test]
    fn ready_orders_by_priority_then_due() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            // Two due today, one high-priority one low; one with no priority.
            let mut p2 = NewTask::new("task:p2", "p2");
            p2.priority = Some(2);
            p2.due = Some("2026-07-02".to_owned());
            create(conn, meta, &p2)?;

            let mut p1 = NewTask::new("task:p1", "p1");
            p1.priority = Some(1);
            p1.due = Some("2026-07-02".to_owned());
            create(conn, meta, &p1)?;

            create(conn, meta, &NewTask::new("task:none", "none"))?;
            Ok(())
        })
        .unwrap();

        let frontier = db.read(|conn| ready(conn, Scope::All, &[])).unwrap();
        // p1 before p2 (priority asc), null-priority last.
        assert_eq!(
            uids(&frontier),
            vec![
                "task:p1".to_owned(),
                "task:p2".to_owned(),
                "task:none".to_owned()
            ]
        );
    }

    #[test]
    fn ready_is_scopable_to_a_repo_mirror() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            let mut spec = NewTask::new("task:repo", "repo task");
            spec.mirrors = vec!["repos/monorepo/backend".to_owned()];
            create(conn, meta, &spec)?;
            create(conn, meta, &NewTask::new("task:inbox", "inbox only"))?;
            Ok(())
        })
        .unwrap();

        let in_repo = db
            .read(|conn| ready(conn, Scope::Subtree("repos/monorepo".to_owned()), &[]))
            .unwrap();
        assert_eq!(uids(&in_repo), vec!["task:repo".to_owned()]);

        let elsewhere = db
            .read(|conn| ready(conn, Scope::Subtree("repos/other".to_owned()), &[]))
            .unwrap();
        assert!(elsewhere.is_empty());
    }

    #[test]
    fn set_priority_and_due_update_columns() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .write_txn("t", |conn, meta| {
                create(conn, meta, &NewTask::new("task:a", "a"))
            })
            .unwrap();
        db.write_txn("t", move |conn, meta| {
            set_priority(conn, meta, id, Some(3))?;
            set_due(conn, meta, id, Some("2026-08-01"))
        })
        .unwrap();

        let (p, d): (Option<i64>, Option<String>) = db
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT priority, due FROM items WHERE id = ?1",
                    [id.get()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(p, Some(3));
        assert_eq!(d.as_deref(), Some("2026-08-01"));
    }

    #[test]
    fn quick_add_create_roundtrips_through_new_task() {
        let db = Db::open_in_memory().unwrap();
        let qa = parse_quick_add("release notes !p1 #size=small").unwrap();
        assert_eq!(
            qa,
            QuickAdd {
                title: "release notes".to_owned(),
                priority: Some(1),
                tags: vec![("size".to_owned(), "small".to_owned())],
                ..QuickAdd::default()
            }
        );

        let id = db
            .write_txn("t", move |conn, meta| {
                create(conn, meta, &NewTask::from_quick_add("task:rn", qa))
            })
            .unwrap();

        // Status set to open on create, tag applied.
        let status = db
            .read(move |conn| {
                Ok(
                    conn.query_row("SELECT status FROM items WHERE id = ?1", [id.get()], |r| {
                        r.get::<_, String>(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(status, TaskStatus::Open.as_str());

        let tagged = db
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM tag_applications WHERE item_id = ?1 AND facet = 'size'",
                    [id.get()],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .unwrap();
        assert_eq!(tagged, 1);

        // Sanity: the uid resolves back to the created id.
        let looked_up = db
            .read(move |conn| item::id_for_uid(conn, "task:rn"))
            .unwrap();
        assert_eq!(looked_up, Some(id));
    }
}
