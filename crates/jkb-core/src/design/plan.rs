//! Execution plans (design D53.6): how a design's approved words become tasks.
//!
//! A plan is an item (`kind = 'exec_plan'`) contained by its design; its **steps** are items
//! (`kind = 'plan_step'`) contained by the plan, in order — coarse, natural-language stages such as
//! *scaffold → database → frontend → deploy*. A span is staged into a step by a `stages` edge
//! ([`super::stage`]), and the work is ordinary tasks contained under a step (`jkb task add …
//! --under <step>`), or directly under the design for a one-off.
//!
//! Nothing here is stored that the graph already says. Which tasks a plan has is containment; which
//! spans a step implements is the `stages` edges; and **archived** — a plan whose tasks are all
//! terminal — is derived on every read, so a reopened task brings its plan back without a write.

use rusqlite::{params, Connection};

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
    for (step, step_uid, text) in step_items(conn, plan_item)? {
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
    // Archived is judged on exactly the tasks the view lists — one walk — so a task the listing
    // cannot show (one contained by the plan itself, say) can neither hold the plan open unseen nor
    // be silently left out of a plan called finished. `task::add_subtask` refuses such a parent.
    let mut count = 0usize;
    let mut open = 0usize;
    for t in steps.iter().flat_map(|s: &StepView| s.tasks.iter()) {
        count += 1;
        if !TaskStatus::is_terminal_str(t.status.as_deref()) {
            open += 1;
        }
    }
    Ok(PlanView {
        uid,
        title,
        design: design.to_owned(),
        // A plan with no tasks yet is a draft, not finished work: vacuous truth is refused, as
        // IMPLEMENTED refuses it.
        archived: count > 0 && open == 0,
        steps,
    })
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

/// The design (and plan step) task `task` is contained under, walking its containers up; `None`
/// for a task under no design.
///
/// # Errors
/// A database error.
pub fn place_of(conn: &Connection, task: ItemId) -> Result<Option<TaskPlace>> {
    let mut step: Option<(ItemId, String, String)> = None;
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
            STEP_KIND if step.is_none() => {
                step = Some((parent, meta.uid, meta.content.unwrap_or_default()));
            }
            KIND => {
                let design = (meta.uid.clone(), meta.content.unwrap_or_default());
                let plan = match &step {
                    Some((step_id, _, _)) => match containment::parent(conn, *step_id)? {
                        Some(p) => item::get(conn, p)?
                            .filter(|m| m.kind == PLAN_KIND)
                            .map(|m| (m.uid, m.content.unwrap_or_default())),
                        None => None,
                    },
                    None => None,
                };
                let spans = match &step {
                    Some((_, step_uid, _)) => {
                        let design_item = parent;
                        let (doc, _) = super::load(conn, design_item)?;
                        span_views(conn, design_item, &doc)?
                            .into_iter()
                            .filter(|s| s.steps.contains(step_uid))
                            .collect()
                    }
                    None => Vec::new(),
                };
                return Ok(Some(TaskPlace {
                    design,
                    plan,
                    step: step.map(|(_, uid, text)| (uid, text)),
                    spans,
                }));
            }
            _ => {}
        }
        at = parent;
    }
    Ok(None)
}
