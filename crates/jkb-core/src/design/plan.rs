//! Execution plans (design D53.6): how a design's approved words become tasks.
//!
//! A plan is an item (`kind = 'exec_plan'`) contained by its design; its **steps** are items
//! (`kind = 'plan_step'`) contained by the plan, in order — coarse, natural-language stages such as
//! *scaffold → database → frontend → deploy*. A span is staged into a step by a `stages` edge
//! ([`super::stage`]), and the work is ordinary tasks contained under a step (`jkb task add …
//! --under <step>`). **Every new task is under a step** ([`homes_tasks`], checked in
//! `task::create`); one with no plan of its own is captured in its repo's Open work › Inbox step
//! ([`triage_step`]). A task under the design directly (a one-off) is only ever one an older build
//! wrote, and is still listed.
//!
//! Nothing here is stored that the graph already says. Which tasks a plan has is containment; which
//! spans a step implements is the `stages` edges; and **archived** — a plan every one of whose
//! steps has tasks, all terminal ([`StepTasks::finished`]) — is derived on every read, so a
//! reopened task brings its plan back without a write.

use rusqlite::{params, Connection, OptionalExtension};

use jkb_types::{ItemId, TaskStatus};

use super::{design_id, invalid, mint, not_found, span_views, SpanView, KIND, STEP_KIND};
use crate::store::WriteMeta;
use crate::workflow::store as workflow;
use crate::{claim, containment, item, Result};

/// The item kind of an execution plan.
pub const PLAN_KIND: &str = "exec_plan";
/// The longest step or plan title, in bytes: a step is a coarse stage, not a specification.
pub const MAX_STEP_BYTES: usize = 4096;
/// The most steps one plan holds.
pub const MAX_STEPS: usize = 64;

/// A task under a plan's step (or a one-off under the design), as the Tasks pane lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTask {
    /// Its uid.
    pub uid: String,
    /// Its title (the first line of its content).
    pub title: String,
    /// Its status.
    pub status: Option<String>,
    /// Its priority.
    pub priority: Option<i64>,
    /// How deep under its step it is: 0 for a step's own task, 1 for that task's subtask, ….
    pub depth: u32,
    /// Who holds its claim, if anyone.
    pub claimed_by: Option<String>,
    /// The workflow strategy it runs (D52): the name it was pinned to, or `default:<name>`.
    pub strategy: String,
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepView {
    /// Its uid — what `jkb design stage <span> <step>` and `jkb task add --under` name.
    pub uid: String,
    /// What the step is, in words.
    pub text: String,
    /// The spans staged into it, in text order.
    pub spans: Vec<SpanView>,
    /// Its tasks, depth first in containment order.
    pub tasks: Vec<PlanTask>,
}

/// A plan with its steps and their tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanView {
    /// Its uid.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// The design it plans.
    pub design: String,
    /// Every task its steps list is terminal (and it has at least one): hidden from listings
    /// unless asked, shown in the history drawer. Derived, never stored, from the same walk as
    /// `steps[*].tasks`.
    pub archived: bool,
    /// Its steps, in order.
    pub steps: Vec<StepView>,
}

impl PlanView {
    /// Every task under the plan's steps, in step order.
    pub fn tasks(&self) -> impl Iterator<Item = &PlanTask> {
        self.steps.iter().flat_map(|s| s.tasks.iter())
    }
}

/// A design's plans and its one-off tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plans {
    /// The plans, in creation order: the live ones, and the archived ones too when asked.
    pub plans: Vec<PlanView>,
    /// How many archived plans the listing left out (0 when they were asked for).
    pub hidden: usize,
    /// Tasks contained directly under the design (one-offs), depth first.
    pub tasks: Vec<PlanTask>,
}

fn clean(what: &str, text: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(invalid(format!("a {what} needs some text")));
    }
    if text.len() > MAX_STEP_BYTES {
        return Err(invalid(format!(
            "a {what} is at most {MAX_STEP_BYTES} bytes — a step is a coarse stage, and its detail \
             belongs in its tasks"
        )));
    }
    Ok(text.to_owned())
}

/// An item's id and kind by uid, refused unless it is a `kind`.
fn item_of_kind(conn: &Connection, uid: &str, kind: &str, what: &str) -> Result<ItemId> {
    let id = item::id_for_uid(conn, uid)?.ok_or_else(|| not_found(format!("no {what} `{uid}`")))?;
    let found = item::get(conn, id)?.map(|m| m.kind).unwrap_or_default();
    if found != kind {
        return Err(invalid(format!(
            "`{uid}` is a {found}, not a {what} ({kind})"
        )));
    }
    Ok(id)
}

/// Make an item of `kind` and contain it under `parent`, after its existing children.
fn contained_item(
    conn: &Connection,
    meta: &WriteMeta,
    parent: ItemId,
    uid: &str,
    kind: &str,
    content: &str,
) -> Result<ItemId> {
    let id = item::upsert(
        conn,
        meta,
        &item::NewItem {
            uid: uid.to_owned(),
            kind: kind.to_owned(),
            content: Some(content.to_owned()),
            content_hash: None,
            mime: None,
        },
    )?;
    let position = i64::try_from(containment::children(conn, parent)?.len()).unwrap_or(i64::MAX);
    containment::contain(conn, meta, id, parent, position)?;
    Ok(id)
}

/// Create an execution plan titled `title` under design `design`, with `steps` in order.
///
/// # Errors
/// An unknown design, an empty or oversized title or step, more than [`MAX_STEPS`] steps, or a
/// database error.
pub fn create(
    conn: &Connection,
    meta: &WriteMeta,
    design: &str,
    title: &str,
    steps: &[String],
) -> Result<PlanView> {
    let design_item = design_id(conn, design)?;
    let title = clean("plan title", title)?;
    if steps.len() > MAX_STEPS {
        return Err(invalid(format!("a plan has at most {MAX_STEPS} steps")));
    }
    let steps = steps
        .iter()
        .map(|s| clean("plan step", s))
        .collect::<Result<Vec<_>>>()?;
    let uid = mint("plan", &title)?;
    let plan = contained_item(conn, meta, design_item, &uid, PLAN_KIND, &title)?;
    for text in &steps {
        contained_item(conn, meta, plan, &mint("step", text)?, STEP_KIND, text)?;
    }
    show(conn, &uid)
}

/// Append a step to plan `plan`.
///
/// # Errors
/// An unknown plan, an empty or oversized step, a plan already at [`MAX_STEPS`], or a database
/// error.
pub fn add_step(conn: &Connection, meta: &WriteMeta, plan: &str, text: &str) -> Result<PlanView> {
    let plan_item = item_of_kind(conn, plan, PLAN_KIND, "plan")?;
    let text = clean("plan step", text)?;
    if step_items(conn, plan_item)?.len() >= MAX_STEPS {
        return Err(invalid(format!(
            "plan {plan} already has {MAX_STEPS} steps"
        )));
    }
    contained_item(
        conn,
        meta,
        plan_item,
        &mint("step", &text)?,
        STEP_KIND,
        &text,
    )?;
    show(conn, plan)
}

/// The items `parent` contains, of `kind`, in containment order: `(id, uid, content)`.
fn children_of_kind(
    conn: &Connection,
    parent: ItemId,
    kind: &str,
) -> Result<Vec<(ItemId, String, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.id, i.uid, COALESCE(i.content, '') FROM containment c
           JOIN items i ON i.id = c.child_item_id
          WHERE c.parent_item_id = ?1 AND i.kind = ?2 ORDER BY c.position, i.id",
    )?;
    let rows = stmt
        .query_map(params![parent.get(), kind], |r| {
            Ok((ItemId::new(r.get(0)?), r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn step_items(conn: &Connection, plan: ItemId) -> Result<Vec<(ItemId, String, String)>> {
    children_of_kind(conn, plan, STEP_KIND)
}

/// The tasks under `parent`, depth first in containment order, from depth `depth`.
fn tasks_under(
    conn: &Connection,
    parent: ItemId,
    depth: u32,
    out: &mut Vec<PlanTask>,
) -> Result<()> {
    // Containment is a tree (one parent per child, and no item contains itself), so this ends; the
    // depth bound only stops a corrupted table from recursing without end.
    if depth > 64 {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached(
        "SELECT i.id, i.uid, i.content, i.status, i.priority FROM containment c
           JOIN items i ON i.id = c.child_item_id
          WHERE c.parent_item_id = ?1 AND i.kind = 'task' ORDER BY c.position, i.id",
    )?;
    let rows = stmt
        .query_map([parent.get()], |r| {
            Ok((
                ItemId::new(r.get(0)?),
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, uid, content, status, priority) in rows {
        out.push(PlanTask {
            title: item::title_from(&uid, content.as_deref()),
            claimed_by: claim::holder(conn, id)?,
            strategy: workflow::current(conn, id)?.source,
            uid,
            status,
            priority,
            depth,
        });
        tasks_under(conn, id, depth + 1, out)?;
    }
    Ok(())
}

/// The view of plan `plan_item`, with `spans` the design's spans (for what each step stages).
fn view(
    conn: &Connection,
    plan_item: ItemId,
    uid: String,
    title: String,
    design: &str,
    spans: &[SpanView],
) -> Result<PlanView> {
    let mut steps = Vec::new();
    // Per step, by the same count IMPLEMENTED reads ([`step_tasks`]): every step has tasks, all
    // terminal. A plan with an empty step is a draft, not finished work.
    let mut archived = true;
    for (step, step_uid, text) in step_items(conn, plan_item)? {
        archived &= step_tasks(conn, step)?.finished();
        let mut tasks = Vec::new();
        tasks_under(conn, step, 0, &mut tasks)?;
        steps.push(StepView {
            spans: spans
                .iter()
                .filter(|s| s.steps.contains(&step_uid))
                .cloned()
                .collect(),
            uid: step_uid,
            text,
            tasks,
        });
    }
    // Judged from the steps alone, as the view lists tasks: a task contained by the plan itself (only
    // a pre-guard build could write one, `task::add_subtask` refuses a plan as a parent) is neither
    // listed nor counted, so it does not hold the plan open.
    archived &= !steps.is_empty();
    Ok(PlanView {
        uid,
        title,
        design: design.to_owned(),
        archived,
        steps,
    })
}

/// `WITH RECURSIVE under(id) AS (…)`: every item contained, at any depth, under the containment
/// children `$roots` selects. **The one walk** of what a step holds: [`step_tasks`] (behind a span's
/// IMPLEMENTED and a plan's `archived`) runs it from one step, and [`UNPLANNED_SQL`] from every
/// step, so what a plan counts as its work and what the unplanned read counts as homed cannot
/// disagree.
///
/// It goes down through items of any kind. `tasks_under`, which lists a step's tasks, goes down
/// through tasks only; the two differ only for a task under a non-task item under a step.
/// Containment is a tree (`child_item_id` is its primary key), and `UNION` rather than `UNION ALL`
/// ends the recursion even on a corrupted cycle.
macro_rules! under_steps {
    ($($roots:tt)+) => {
        concat!(
            "WITH RECURSIVE under(id) AS (",
            $($roots)+,
            " UNION SELECT c.child_item_id FROM containment c JOIN under u ON c.parent_item_id = u.id)"
        )
    };
}

/// The roots of the every-step walk: each item a plan step contains directly. One text, so
/// [`UNPLANNED_SQL`] and the downward form of [`homes_tasks`] the proptest checks it against expand
/// the same walk.
macro_rules! every_step_roots {
    () => {
        concat!(
            "SELECT child_item_id FROM containment WHERE parent_item_id IN (SELECT id FROM items WHERE kind = '",
            step_kind!(),
            "')"
        )
    };
}

/// *"Item `i` is an unplanned task"*: a task, not terminal, and contained at no depth under any
/// plan step (by [`under_steps!`], the walk `archived` uses). A task under a plan directly, a
/// design's one-off, and a task homed only by namespace (an inbox, a `.backlog`, a review's
/// findings, a synced `tasks.md`) are all unplanned: where a task is placed plays no part.
///
/// Binds **no parameter**, so it can sit among the query DSL's anonymous `?` bindings (a `?1` here
/// would renumber every later one), and it is never NULL: `sql_not_terminal!` is NULL-safe and the
/// walk yields only the non-null `child_item_id`. Behind both `is:unplanned` and [`unplanned`].
pub(crate) const UNPLANNED_SQL: &str = concat!(
    "(i.kind = 'task' AND ",
    jkb_types::sql_not_terminal!("i.status"),
    " AND i.id NOT IN (",
    under_steps!(every_step_roots!()),
    " SELECT id FROM under))"
);

/// Whether `parent` may hold a task: a plan step, or a task with a plan step among its containers
/// at any depth (through items of any kind). **The one rule** behind `task::create`, `add_subtask`,
/// `set_container` and `move_under`, and behind the tasks.md import's question of whether a task
/// already placed is in a plan. A child of a parent it accepts is one [`UNPLANNED_SQL`] counts as
/// planned. Whether the step's design is retired plays no part: retirement is a fact that changes
/// after a task is placed, so a write-time gate on it would refuse every later re-assertion of a
/// placement it once allowed. What could file *new* work into a retired design unasked — a
/// namespace mapped to one of its steps — is closed where the mapping is read instead:
/// [`mapped_step`] refuses a mapping into a retired design as stale. (`design::retire` refusing
/// while a namespace maps into the tree, and [`map_namespace`] refusing a retired design's step,
/// are the early refusals; `jkb undo` can get past both.)
///
/// It walks **up** from `parent` — one indexed lookup per level, since `child_item_id` is
/// containment's primary key — rather than down from every step as [`UNPLANNED_SQL`] does: the two
/// ask the same question ("is a step a proper ancestor?") from opposite ends. Measured on a
/// synthetic store the size of the real one (233 steps, 3,560 tasks all planned, release build), the
/// downward walk cost 6.3 ms a call, over the 5 ms a write on the one writer thread may spend on it;
/// this one cost 0.08 ms for a task three levels deep, being bounded by the parent's depth. The
/// proptest in `triage_tests` holds the two to the same answer ([`HOMES_TASKS_DOWNWARD_SQL`]) and to
/// the unplanned read.
///
/// # Errors
/// A database error.
pub fn homes_tasks(conn: &Connection, parent: ItemId) -> Result<bool> {
    // The nearest step: `parent` itself, or for a task the closest step above it. The depth bound
    // only stops a corrupted (cyclic) table from recursing without end.
    let step: Option<i64> = conn
        .prepare_cached(concat!(
            "WITH RECURSIVE up(id, depth) AS (
               SELECT parent_item_id, 1 FROM containment WHERE child_item_id = ?1
               UNION ALL SELECT c.parent_item_id, u.depth + 1 FROM containment c
                 JOIN up u ON c.child_item_id = u.id WHERE u.depth < 1024)
             SELECT CASE p.kind WHEN '",
            step_kind!(),
            "' THEN p.id WHEN 'task' THEN (
                 SELECT u.id FROM up u JOIN items a ON a.id = u.id WHERE a.kind = '",
            step_kind!(),
            "' ORDER BY u.depth LIMIT 1) END
               FROM items p WHERE p.id = ?1"
        ))?
        .query_row([parent.get()], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(step.is_some())
}

/// [`homes_tasks`] asked from the other end, by the every-step walk [`UNPLANNED_SQL`] runs: the
/// oracle the proptest holds the upward walk to.
#[cfg(test)]
pub(crate) const HOMES_TASKS_DOWNWARD_SQL: &str = concat!(
    under_steps!(every_step_roots!()),
    " SELECT EXISTS (SELECT 1 FROM items p WHERE p.id = ?1 AND (p.kind = '",
    step_kind!(),
    "' OR (p.kind = 'task' AND p.id IN (SELECT id FROM under))))"
);

/// The refusal for a task's container written to `uid`, a `kind` under no plan step: the same
/// words on every surface.
pub(crate) fn unplanned_parent(uid: &str, kind: &str) -> crate::Error {
    let plan_it = if kind == "task" {
        format!(", or plan `{uid}` first (`jkb task move {uid} --under <step>`)")
    } else {
        String::new()
    };
    invalid(format!(
        "`{uid}` is a {kind} under no plan step, so a task under it would be in no plan: add it \
         under a plan step or a task under one (`--under <step>`; `jkb design plan ls <design>` \
         lists a design's plans and their steps){plan_it}"
    ))
}

/// Why a new task nothing files under a step is refused: no `--under`, and no repo to fall back to.
pub const NO_REPO: &str = "a new task is filed under a plan step, and nothing names one here: \
outside a mounted repo there is no Open work inbox to fall back to. Pass `--under <step>` (`jkb \
design ls --repo <repo>`, then `jkb design plan ls <design>`, lists the steps), or run it inside a \
mounted repo, where it lands in that repo's Open work \u{203a} Inbox";

fn no_repo() -> crate::Error {
    invalid(NO_REPO)
}

/// The title of every repo's triage design, and of its one plan.
pub const TRIAGE_TITLE: &str = "Open work";
/// The text of the triage plan's one step.
pub const TRIAGE_STEP_TEXT: &str = "Inbox";
/// The triage design's body.
const TRIAGE_BODY: &str = "Where work is captured before it is planned: `jkb task add` with no \
`--under`, `--backlog`, review findings about pre-existing code, and synced tasks.md tasks whose \
section maps to no step land in Open work \u{203a} Inbox. Triage moves each to the step of the \
plan it belongs to (`jkb task move <uid> --under <step>`) or cancels it.";
/// The namespace metadata key mapping a namespace (and everything under it) to a plan step, by uid.
pub const PLAN_STEP_KEY: &str = "plan_step";

/// The uid of repo `repo`'s triage design. Minted uids are `<prefix>:<slug>-<hex>`, and a slug never
/// holds `@`, so these fixed ones cannot collide with a minted one.
#[must_use]
pub fn triage_design_uid(repo: &str) -> String {
    format!("{KIND}:open-work@{repo}")
}

/// The uid of repo `repo`'s triage plan.
#[must_use]
pub fn triage_plan_uid(repo: &str) -> String {
    format!("plan:open-work@{repo}")
}

/// The uid of repo `repo`'s triage step, where a task with no plan of its own is filed.
#[must_use]
pub fn triage_step_uid(repo: &str) -> String {
    format!("step:inbox@{repo}")
}

/// Whether `uid` names a repo's triage step ([`triage_step_uid`]).
#[must_use]
pub fn is_triage_step_uid(uid: &str) -> bool {
    uid.strip_prefix("step:inbox@")
        .is_some_and(|repo| !repo.is_empty())
}

/// The item `uid`, when it exists: refused unless it is a `kind` contained by `parent` (when one is
/// given), so a mismatched tree is named rather than built beside.
fn triage_level(
    conn: &Connection,
    uid: &str,
    kind: &str,
    parent: Option<ItemId>,
) -> Result<Option<ItemId>> {
    let Some(id) = item::id_for_uid(conn, uid)? else {
        return Ok(None);
    };
    let found = item::get(conn, id)?.map(|m| m.kind).unwrap_or_default();
    if found != kind {
        return Err(invalid(format!(
            "`{uid}` is a {found}, not the {kind} the Open work triage tree needs there"
        )));
    }
    if let Some(parent) = parent {
        if containment::parent(conn, id)? != Some(parent) {
            return Err(invalid(format!(
                "`{uid}` is not where the Open work triage tree keeps it (under `{}`): move it back \
                 or remove it",
                item::get(conn, parent)?.map(|m| m.uid).unwrap_or_default()
            )));
        }
    }
    Ok(Some(id))
}

/// Repo `repo`'s triage step, `step:inbox@<repo>`: found by uid, or created on demand — the design
/// "Open work" at `designs/<repo>`, its plan "Open work", and that plan's step "Inbox" — one level
/// at a time, each verified to be the kind it should be where it should be. Runs inside the
/// caller's transaction, which `write_txn` begins `IMMEDIATE`, and `items.uid` is unique, so two
/// creators never build two trees.
///
/// # Errors
/// A repo that is not one namespace segment, a uid of the tree that names the wrong kind or sits in
/// the wrong place, or a database error.
pub fn triage_step(conn: &Connection, meta: &WriteMeta, repo: &str) -> Result<ItemId> {
    let design_uid = triage_design_uid(repo);
    let design = if let Some(id) = triage_level(conn, &design_uid, KIND, None)? {
        id
    } else {
        super::create_as(conn, meta, &design_uid, repo, TRIAGE_TITLE, TRIAGE_BODY)?;
        design_id(conn, &design_uid)?
    };
    let plan_uid = triage_plan_uid(repo);
    let plan = match triage_level(conn, &plan_uid, PLAN_KIND, Some(design))? {
        Some(id) => id,
        None => contained_item(conn, meta, design, &plan_uid, PLAN_KIND, TRIAGE_TITLE)?,
    };
    let step_uid = triage_step_uid(repo);
    match triage_level(conn, &step_uid, STEP_KIND, Some(plan))? {
        Some(id) => Ok(id),
        None => contained_item(conn, meta, plan, &step_uid, STEP_KIND, TRIAGE_STEP_TEXT),
    }
}

/// The step a new task homed at `home` is filed under when it names none, in order: the nearest
/// namespace at or above `home` whose metadata maps it to a step ([`PLAN_STEP_KEY`]); else the
/// triage step of the repo `home` names ([`crate::mount::repo_of_namespace`]), refused when this
/// knowledge base does not know that repo; else that of `ambient_repo`; else refused.
///
/// # Errors
/// A mapping to a uid that is gone or not a plan step (named, with how to clear it), a home naming a
/// repo this knowledge base does not know, no repo to fall back to, a malformed home, or a database
/// error.
pub fn step_for_home(
    conn: &Connection,
    meta: &WriteMeta,
    home: &str,
    ambient_repo: Option<&str>,
) -> Result<ItemId> {
    if let Some(step) = mapped_step(conn, home)? {
        return Ok(step);
    }
    let normalized = crate::ns::normalize(home)?;
    if let Some(repo) = crate::mount::repo_of_namespace(&normalized) {
        known_repo(conn, &repo, &normalized)?;
        return triage_step(conn, meta, &repo);
    }
    match ambient_repo {
        Some(repo) => triage_step(conn, meta, repo),
        None => Err(no_repo()),
    }
}

/// Whether this knowledge base knows repo `repo`: it has a `repos/<repo>` namespace or an Open work
/// design for it. What [`step_for_home`] holds a home naming a repo to before filing there, so a
/// surface with its own remedy (a review's filing) can ask first and say it in its own words.
///
/// # Errors
/// A database error.
pub fn knows_repo(conn: &Connection, repo: &str) -> Result<bool> {
    Ok(crate::ns::get(conn, &format!("repos/{repo}"))?.is_some()
        || item::id_for_uid(conn, &triage_design_uid(repo))?.is_some())
}

/// Refuse to build an Open work tree for repo `repo`, named by home `home`, that this knowledge base
/// does not know: no `repos/<repo>` namespace and no Open work design for it. A mistyped
/// `+repos/<repo>/…` would otherwise create a whole design for a repo that does not exist. Checked
/// before anything of the capture is written, so the home's own namespace has not made
/// `repos/<repo>` yet. The ambient repo needs no check: it is the repo of the mount the caller stands
/// in, whose namespace exists.
fn known_repo(conn: &Connection, repo: &str, home: &str) -> Result<()> {
    if knows_repo(conn, repo)? {
        return Ok(());
    }
    let path = format!("repos/{repo}");
    Err(invalid(format!(
        "`{home}` names repo `{repo}`, which this knowledge base does not know (there is no \
         `{path}`): check the repo name, or file the task under a step with `--under <step>`"
    )))
}

/// Map namespace `path` — a tasks.md section's, a file's or a mount's — to plan step `step`, or with
/// `None` clear its mapping: a new task homed there or below with no step of its own is filed under
/// that step ([`step_for_home`]). The key is merged into the namespace's metadata, never replacing
/// it, so its `type` and a file's section keys survive. Answers the step it was mapped to before.
///
/// # Errors
/// A `step` that is no plan step or a step of a retired design, a malformed path, a namespace that
/// does not exist, or a database error.
pub fn map_namespace(
    conn: &Connection,
    meta: &WriteMeta,
    path: &str,
    step: Option<&str>,
) -> Result<Option<String>> {
    let path = crate::ns::normalize(path)?;
    if let Some(step) = step {
        let id = item_of_kind(conn, step, STEP_KIND, "plan step")?;
        refuse_retired_step(conn, step, id)?;
    }
    // Never created here: a mistyped path would be made and mapped while the real one stayed not.
    let id = crate::ns::get(conn, &path)?.ok_or_else(|| {
        not_found(format!(
            "no namespace `{path}` (a mount, a synced file or a section names one; `jkb ns mk \
             {path}` makes it)"
        ))
    })?;
    let mut md = crate::ns::get_metadata(conn, id)?
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(map) = md.as_object_mut() else {
        return Ok(None);
    };
    let before = match step {
        Some(step) => map.insert(PLAN_STEP_KEY.to_owned(), serde_json::json!(step)),
        None => map.remove(PLAN_STEP_KEY),
    };
    let before = before.and_then(|v| v.as_str().map(str::to_owned));
    if before.as_deref() != step {
        crate::ns::set_metadata(conn, meta, id, &md)?;
    }
    Ok(before)
}

/// Refuse a mapping to step `uid` (item `step`) while its design is retired: a new task filed there
/// would be in a plan no listing shows. `design::retire` refuses while a namespace maps into the
/// tree, so between them no *write* makes a mapping into a retired design. `jkb undo` still can — it
/// restores a namespace's metadata or a design's resolution straight from the changelog (undo a
/// `--clear` after a retire, or a restore after a mapping) — so these two are early refusals, and
/// the rule itself is asked where a mapping is read: [`mapped_step`].
fn refuse_retired_step(conn: &Connection, uid: &str, step: ItemId) -> Result<()> {
    if let Some(d) = retired_design_of_step(conn, step)? {
        return Err(invalid(format!(
            "`{uid}` is a step of design {d}, which is retired: a task filed there would be in a \
             plan no listing shows. Map the namespace to a step of a live design, or restore {d} \
             first (`jkb design restore {d}`)"
        )));
    }
    Ok(())
}

/// The uid of the nearest retired design at or above plan step `step`'s design (step → plan →
/// design, then up through the designs it is nested in), if any. A step under no design answers
/// `None`.
fn retired_design_of_step(conn: &Connection, step: ItemId) -> Result<Option<String>> {
    let design = match containment::parent(conn, step)? {
        Some(plan) => containment::parent(conn, plan)?,
        None => None,
    };
    let mut at = match design {
        Some(d) if item::get(conn, d)?.is_some_and(|m| m.kind == KIND) => Some(d),
        _ => None,
    };
    while let Some(design) = at {
        if super::retired_item(conn, design)? {
            return Ok(item::get(conn, design)?.map(|m| m.uid));
        }
        at = super::tree::parent_of(conn, design)?;
    }
    Ok(None)
}

/// The step the nearest namespace at or above `home` is mapped to ([`PLAN_STEP_KEY`]), if any.
///
/// # Errors
/// A mapping that names no plan step, or a step of a retired design (a nested design counts as
/// retired when an enclosing one is) — each refused, naming the namespace and how to clear it — a
/// malformed home, or a database error.
pub fn mapped_step(conn: &Connection, home: &str) -> Result<Option<ItemId>> {
    let mut cur = Some(crate::ns::normalize(home)?);
    while let Some(path) = cur {
        if let Some(ns_id) = crate::ns::get(conn, &path)? {
            let uid = crate::ns::get_metadata(conn, ns_id)?.and_then(|m| {
                m.get(PLAN_STEP_KEY)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            });
            if let Some(uid) = uid {
                let found = match item::id_for_uid(conn, &uid)? {
                    Some(id) => item::get(conn, id)?.map(|m| (id, m.kind)),
                    None => None,
                };
                let why = match found {
                    Some((id, kind)) if kind == STEP_KIND => {
                        // Asked here, where the mapping is used, not only where it is written:
                        // `jkb undo` can bring back a mapping into a retired design (see
                        // `refuse_retired_step`).
                        let Some(d) = retired_design_of_step(conn, id)? else {
                            return Ok(Some(id));
                        };
                        return Err(invalid(format!(
                            "`{path}` maps its tasks to `{uid}`, a step of design {d}, which is \
                             retired: a task filed there would be in a plan no listing shows. \
                             Clear the stale mapping (`jkb ns step {path} --clear`) or map it to a \
                             live design's step (`jkb ns step {path} <step>`), or restore {d} (`jkb \
                             design restore {d}`)"
                        )));
                    }
                    Some((_, kind)) => format!("is a {kind}, not a plan step"),
                    None => "no longer exists".to_owned(),
                };
                return Err(invalid(format!(
                    "`{path}` maps its tasks to `{uid}`, which {why}: map it to a step (`jkb ns step \
                     {path} <step>`) or clear the mapping (`jkb ns step {path} --clear`)"
                )));
            }
        }
        cur = path.rsplit_once('/').map(|(parent, _)| parent.to_owned());
    }
    Ok(None)
}

/// A task no execution plan's step reaches ([`UNPLANNED_SQL`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnplannedTask {
    /// Its uid.
    pub uid: String,
    /// Its title (the first line of its content).
    pub title: String,
}

/// Every unplanned task ([`UNPLANNED_SQL`]), most urgent first: by priority (none last), then by
/// age.
///
/// # Errors
/// A database error.
pub fn unplanned(conn: &Connection) -> Result<Vec<UnplannedTask>> {
    let sql = format!(
        "SELECT i.uid, i.content FROM items i WHERE {UNPLANNED_SQL}
          ORDER BY i.priority IS NULL, i.priority, i.id"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(uid, content)| UnplannedTask {
            title: item::title_from(&uid, content.as_deref()),
            uid,
        })
        .collect())
}

/// The tasks contained under a plan step, at any depth: how many, how many are `done`, and how
/// many are terminal (`done` or `cancelled`). **The one count** behind both a span's IMPLEMENTED
/// and a plan's `archived`, so the two cannot disagree about what a step holds — they once did, a
/// plan counting all its tasks at once while IMPLEMENTED counted per step, and a plan with an empty
/// step was archived while a span staged into that step stayed STAGED.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StepTasks {
    count: i64,
    done: i64,
    terminal: i64,
}

impl StepTasks {
    /// The step implemented what was staged into it: it has tasks, all `done`. A cancelled task
    /// implemented nothing, and a step with no tasks implemented nothing (vacuous truth refused).
    pub(crate) fn implemented(self) -> bool {
        self.count > 0 && self.done == self.count
    }

    /// The step's work is over: it has tasks, all terminal. Cancelled counts as over — nothing is
    /// left to do — which is why this is not [`StepTasks::implemented`].
    pub(crate) fn finished(self) -> bool {
        self.count > 0 && self.terminal == self.count
    }
}

/// What [`StepTasks`] counts for `step`.
///
/// # Errors
/// A database error.
pub(crate) fn step_tasks(conn: &Connection, step: ItemId) -> Result<StepTasks> {
    Ok(conn
        .prepare_cached(concat!(
            under_steps!("SELECT child_item_id FROM containment WHERE parent_item_id = ?1"),
            "
             SELECT COUNT(*), COALESCE(SUM(i.status IS ?2), 0),
                    COALESCE(SUM(i.status IN ",
            jkb_types::sql_terminal_list!(),
            "), 0)
               FROM under u JOIN items i ON i.id = u.id WHERE i.kind = 'task'",
        ))?
        .query_row(
            // `done` is its own count (a step is IMPLEMENTED only by landed work); the
            // terminal count comes from the shared terminal-set fragment above.
            params![step.get(), TaskStatus::Done.as_str()],
            |r| {
                Ok(StepTasks {
                    count: r.get(0)?,
                    done: r.get(1)?,
                    terminal: r.get(2)?,
                })
            },
        )?)
}

/// The design a plan step belongs to: its plan's container. What [`super::stage`] holds a span to —
/// a span staged into another design's plan would derive STAGED and IMPLEMENTED from tasks no view
/// of its own design connects to it.
///
/// # Errors
/// A step no plan contains, a plan no design contains, or a database error.
pub(crate) fn design_of_step(conn: &Connection, step: ItemId, step_uid: &str) -> Result<ItemId> {
    let plan = containment::parent(conn, step)?
        .ok_or_else(|| invalid(format!("plan step {step_uid} is contained by no plan")))?;
    let meta = item::get(conn, plan)?
        .ok_or_else(|| invalid(format!("plan step {step_uid}'s plan is gone")))?;
    if meta.kind != PLAN_KIND {
        return Err(invalid(format!(
            "plan step {step_uid} is contained by a {}, not a plan",
            meta.kind
        )));
    }
    Ok(design_of(conn, plan, &meta.uid)?.0)
}

/// The design a plan belongs to: its container.
fn design_of(conn: &Connection, plan_item: ItemId, plan: &str) -> Result<(ItemId, String)> {
    let parent = containment::parent(conn, plan_item)?
        .ok_or_else(|| invalid(format!("plan {plan} is contained by no design")))?;
    let meta =
        item::get(conn, parent)?.ok_or_else(|| invalid(format!("plan {plan}'s design is gone")))?;
    if meta.kind != KIND {
        return Err(invalid(format!(
            "plan {plan} is contained by a {}, not a design",
            meta.kind
        )));
    }
    Ok((parent, meta.uid))
}

/// One plan, archived or not.
///
/// # Errors
/// An unknown plan, one no design contains, or a database error.
pub fn show(conn: &Connection, plan: &str) -> Result<PlanView> {
    let plan_item = item_of_kind(conn, plan, PLAN_KIND, "plan")?;
    let (_, design) = design_of(conn, plan_item, plan)?;
    let title = item::get(conn, plan_item)?
        .and_then(|m| m.content)
        .unwrap_or_default();
    let spans = super::spans(conn, &design)?;
    view(conn, plan_item, plan.to_owned(), title, &design, &spans)
}

/// A design's plans — the archived ones only with `all` — and its one-off tasks.
///
/// # Errors
/// An unknown design, or a database error.
pub fn list(conn: &Connection, design: &str, all: bool) -> Result<Plans> {
    let design_item = design_id(conn, design)?;
    let spans = super::spans(conn, design)?;
    let mut plans = Vec::new();
    let mut hidden = 0;
    for (id, uid, title) in children_of_kind(conn, design_item, PLAN_KIND)? {
        let v = view(conn, id, uid, title, design, &spans)?;
        if v.archived && !all {
            hidden += 1;
        } else {
            plans.push(v);
        }
    }
    let mut tasks = Vec::new();
    tasks_under(conn, design_item, 0, &mut tasks)?;
    Ok(Plans {
        plans,
        hidden,
        tasks,
    })
}

/// Where a task sits in a design: the design, and the plan and step when it is under one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPlace {
    /// The design's uid and title.
    pub design: (String, String),
    /// The plan's uid and title, when the task is under a step.
    pub plan: Option<(String, String)>,
    /// The step's uid and text, when the task is under one.
    pub step: Option<(String, String)>,
    /// The spans its step stages.
    pub spans: Vec<SpanView>,
}

/// An item a task sits under: its uid and its text (a design's or plan's title, a step's words).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// Its uid.
    pub uid: String,
    /// Its content, as stored.
    pub text: String,
}

/// The plan step a task sits under, with where it falls in its plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepPlace {
    /// The step.
    pub step: Named,
    /// Its place among its plan's steps, from 1.
    pub number: usize,
    /// The plan it is a step of.
    pub plan: Named,
}

/// What a task sits under, for `jkb task show`: the step and plan when it is under one, the design,
/// and the root of the design's tree when that design is nested under another. Read by
/// [`ancestry_of`] without loading the design's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ancestry {
    /// The plan step, when the task (or a task it is a subtask of) is under one.
    pub step: Option<StepPlace>,
    /// The design whose plan (or whose one-off) the task is.
    pub design: Named,
    /// The top of `design`'s tree ([`super::tree::root_of`]); `None` when `design` is the top.
    pub root: Option<Named>,
}

/// The first step and the design above `task`, found by walking its containers up: the one walk
/// [`place_of`] and [`ancestry_of`] share.
struct Climb {
    step: Option<(ItemId, Named)>,
    design: (ItemId, Named),
}

fn named(meta: item::ItemMeta) -> Named {
    Named {
        uid: meta.uid,
        text: meta.content.unwrap_or_default(),
    }
}

fn climb(conn: &Connection, task: ItemId) -> Result<Option<Climb>> {
    let mut step: Option<(ItemId, Named)> = None;
    let mut at = task;
    // Bounded like `tasks_under`: a tree ends, a corrupted table should not spin.
    for _ in 0..128 {
        let Some(parent) = containment::parent(conn, at)? else {
            return Ok(None);
        };
        let Some(meta) = item::get(conn, parent)? else {
            return Ok(None);
        };
        match meta.kind.as_str() {
            STEP_KIND if step.is_none() => step = Some((parent, named(meta))),
            KIND => {
                return Ok(Some(Climb {
                    step,
                    design: (parent, named(meta)),
                }))
            }
            _ => {}
        }
        at = parent;
    }
    Ok(None)
}

/// The plan `step` is a step of, and its number in that plan from 1; `None` for a step no plan
/// contains.
fn plan_of_step(conn: &Connection, step: ItemId) -> Result<Option<(Named, usize)>> {
    let Some(plan) = containment::parent(conn, step)? else {
        return Ok(None);
    };
    let Some(meta) = item::get(conn, plan)?.filter(|m| m.kind == PLAN_KIND) else {
        return Ok(None);
    };
    let number = step_items(conn, plan)?
        .iter()
        .position(|(id, _, _)| *id == step)
        .map_or(0, |i| i + 1);
    Ok(Some((named(meta), number)))
}

/// What task `task` sits under — its plan step, that step's plan, the design, and the design's
/// root — walking its containers up, so a subtask reports its parent's step. `None` for a task
/// under no design.
///
/// # Errors
/// A database error.
pub fn ancestry_of(conn: &Connection, task: ItemId) -> Result<Option<Ancestry>> {
    let Some(Climb { step, design }) = climb(conn, task)? else {
        return Ok(None);
    };
    let step = match step {
        Some((id, step)) => {
            plan_of_step(conn, id)?.map(|(plan, number)| StepPlace { step, number, plan })
        }
        None => None,
    };
    let root_id = super::tree::root_of(conn, design.0)?;
    let root = if root_id == design.0 {
        None
    } else {
        item::get(conn, root_id)?.map(named)
    };
    Ok(Some(Ancestry {
        step,
        design: design.1,
        root,
    }))
}

/// The design (and plan step) task `task` is contained under, walking its containers up; `None`
/// for a task under no design.
///
/// # Errors
/// A database error.
pub fn place_of(conn: &Connection, task: ItemId) -> Result<Option<TaskPlace>> {
    let Some(Climb { step, design }) = climb(conn, task)? else {
        return Ok(None);
    };
    let (design_item, design) = design;
    let Some((step_id, step)) = step else {
        return Ok(Some(TaskPlace {
            design: (design.uid, design.text),
            plan: None,
            step: None,
            spans: Vec::new(),
        }));
    };
    let plan = plan_of_step(conn, step_id)?.map(|(p, _)| (p.uid, p.text));
    let (doc, _) = super::load(conn, design_item)?;
    let spans = span_views(conn, design_item, &doc)?
        .into_iter()
        .filter(|s| s.steps.contains(&step.uid))
        .collect();
    Ok(Some(TaskPlace {
        design: (design.uid, design.text),
        plan,
        step: Some((step.uid, step.text)),
        spans,
    }))
}
