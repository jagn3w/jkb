//! `design.plan*` and the *Play* prompts (design D53.6), over [`jkb_core::design::plan`].
//!
//! A plan, its steps and their tasks are items and edges; these ops carry the engine's view of
//! them. *Play* is a prompt the app starts Claude with — built here, never by the app (D53.5: "the
//! prompt is CLI output, not app string-building") — and `jkb design prompt play|task` prints the
//! same text.

use std::fmt::Write as _;

use jkb_core::design;
use jkb_core::workflow::store as workflow;
use jkb_core::WriteMeta;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::{invalid, Span};
use crate::{ApiError, ErrorCode};

/// A task under a plan's step, or a one-off under the design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTask {
    /// Its uid.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// Its status.
    pub status: Option<String>,
    /// Its priority.
    pub priority: Option<i64>,
    /// 0 for a step's own task, 1 for its subtask, ….
    pub depth: u32,
    /// Who holds its claim.
    pub claimed_by: Option<String>,
    /// The workflow strategy it runs: the name it was pinned to, or `default:<name>`.
    pub strategy: String,
}

impl From<design::PlanTask> for PlanTask {
    fn from(t: design::PlanTask) -> Self {
        Self {
            uid: t.uid,
            title: t.title,
            status: t.status,
            priority: t.priority,
            depth: t.depth,
            claimed_by: t.claimed_by,
            strategy: t.strategy,
        }
    }
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Its uid: what `jkb design stage <span> <step>` and `jkb task add --under` name.
    pub uid: String,
    /// What it is.
    pub text: String,
    /// The spans staged into it.
    pub spans: Vec<Span>,
    /// Its tasks, depth first.
    pub tasks: Vec<PlanTask>,
}

/// An execution plan, as `design.plan` and `design.plans` answer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Its uid.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// The design it plans.
    pub design: String,
    /// Every task under it is terminal, and it has at least one (derived, never stored).
    pub archived: bool,
    /// Its steps, in order.
    pub steps: Vec<Step>,
}

impl From<design::PlanView> for Plan {
    fn from(p: design::PlanView) -> Self {
        Self {
            uid: p.uid,
            title: p.title,
            design: p.design,
            archived: p.archived,
            steps: p
                .steps
                .into_iter()
                .map(|s| Step {
                    uid: s.uid,
                    text: s.text,
                    spans: s.spans.into_iter().map(Span::from).collect(),
                    tasks: s.tasks.into_iter().map(PlanTask::from).collect(),
                })
                .collect(),
        }
    }
}

/// `design.plans`' answer: the plans, how many archived ones were left out, and the one-offs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanList {
    /// The design.
    pub uid: String,
    /// Its plans, in creation order.
    pub plans: Vec<Plan>,
    /// How many archived plans were left out (0 when `all`).
    pub hidden: usize,
    /// Tasks directly under the design.
    pub tasks: Vec<PlanTask>,
}

/// `design.plan_create`.
///
/// # Errors
/// The engine's refusal.
pub fn create(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    title: &str,
    steps: &[String],
) -> Result<Plan, ApiError> {
    Ok(design::plan::create(conn, meta, uid, title, steps)?.into())
}

/// `design.plan_step`.
///
/// # Errors
/// The engine's refusal.
pub fn add_step(
    conn: &Connection,
    meta: &WriteMeta,
    plan: &str,
    text: &str,
) -> Result<Plan, ApiError> {
    Ok(design::plan::add_step(conn, meta, plan, text)?.into())
}

/// `design.plan`.
///
/// # Errors
/// An unknown plan.
pub fn show(conn: &Connection, plan: &str) -> Result<Plan, ApiError> {
    Ok(design::plan::show(conn, plan)?.into())
}

/// `design.plans`.
///
/// # Errors
/// An unknown design.
pub fn list(conn: &Connection, uid: &str, all: bool) -> Result<PlanList, ApiError> {
    let listed = design::plan::list(conn, uid, all)?;
    Ok(PlanList {
        uid: uid.to_owned(),
        plans: listed.plans.into_iter().map(Plan::from).collect(),
        hidden: listed.hidden,
        tasks: listed.tasks.into_iter().map(PlanTask::from).collect(),
    })
}

/// A *Play* prompt (`play` for a plan, `task` for one task), and what it was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkPrompt {
    /// `play` or `task`.
    pub kind: String,
    /// The plan or the task.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// The design it belongs to (a task under none has none).
    pub design: Option<String>,
    /// The strategy the prompt names, as a task pinned to it reports it (`name@version` for a
    /// definition): for a plan, the one the operator chose (`None` when none was — each task then
    /// runs its own); for a task, the one it runs (`default:<name>` when unpinned).
    pub strategy: Option<String>,
    /// The prompt itself: what the session is started with.
    pub prompt: String,
}

/// The longest span text a prompt quotes in full; `jkb design cat` has all of it.
const QUOTE_MAX_CHARS: usize = 400;

/// A span's text on one line, quoted, cut at [`QUOTE_MAX_CHARS`].
fn quoted(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= QUOTE_MAX_CHARS {
        format!("{flat:?}")
    } else {
        let cut: String = flat.chars().take(QUOTE_MAX_CHARS).collect();
        format!("{cut:?}…")
    }
}

fn task_line(t: &PlanTask) -> String {
    let indent = "  ".repeat(usize::try_from(t.depth).unwrap_or(0));
    let claim = t
        .claimed_by
        .as_deref()
        .map(|c| format!("; claimed by {c}"))
        .unwrap_or_default();
    format!(
        "{indent}- [{}] {} — {} (strategy: {}{claim})",
        t.status.as_deref().unwrap_or("?"),
        t.uid,
        t.title,
        t.strategy
    )
}

/// The plan prompt's strategy sentence: what the open tasks actually run under the pick (naming any
/// not on it), or that no strategy was chosen. `picked` is as a pinned task reports it.
fn strategy_lines(p: &mut String, view: &Plan, picked: Option<&str>, default: &str) {
    if let Some(chosen) = picked {
        // What the tasks run is read, not assumed: this prompt pins nothing (the app pins before it
        // asks; `jkb design prompt play --strategy` does not), so a task the pins missed is named.
        let off: Vec<&PlanTask> = view
            .steps
            .iter()
            .flat_map(|s| s.tasks.iter())
            .filter(|t| {
                !jkb_types::TaskStatus::is_terminal_str(t.status.as_deref()) && t.strategy != chosen
            })
            .collect();
        if off.is_empty() {
            let _ = writeln!(
                p,
                "Workflow strategy: {chosen}. The operator chose it for this plan, and its open \
                 tasks run under it."
            );
        } else {
            let _ = writeln!(
                p,
                "Workflow strategy: {chosen}. The operator chose it for this plan, but these open \
                 tasks are not pinned to it and run their own until the operator pins them: {}.",
                off.iter()
                    .map(|t| format!("{} ({})", t.uid, t.strategy))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let _ = writeln!(
            p,
            "A task you add runs the default ({default}) until the operator pins it, so name the \
             tasks you add when you finish."
        );
    } else {
        let _ = writeln!(
            p,
            "Workflow strategy: no choice for this plan — each task runs its own, named beside it \
             (`default:{default}` when unpinned). A task you add runs the default ({default}) until \
             the operator pins it, so name the tasks you add when you finish."
        );
    }
}

/// The *Play* prompt for a plan (D53.6): the plan, its steps, their tasks, the spans they stage,
/// and the workflow strategy the work runs under — `strategy` when the operator chose one, said to
/// cover only the open tasks actually pinned to it (any other is named); with none, each task's own
/// (the default for an unpinned one), and the prompt claims no choice. Pins nothing.
///
/// # Errors
/// An unknown plan or strategy.
pub fn play_prompt(
    conn: &Connection,
    plan: &str,
    strategy: Option<&str>,
) -> Result<WorkPrompt, ApiError> {
    let view: Plan = design::plan::show(conn, plan)?.into();
    let design_title = jkb_core::item::get(conn, design::design_id(conn, &view.design)?)?
        .and_then(|m| m.content)
        .unwrap_or_default();
    let (default, _) = workflow::default_strategy(conn)?;
    // The pick as a task pinned to it reports it (`resolve_strategy`'s name: `name@version` for a
    // definition), so it is compared with each task's strategy by one identity.
    let picked = strategy
        .map(|name| workflow::resolve_strategy(conn, name).map(|(n, _)| n))
        .transpose()?;
    let mut p = String::new();
    let _ = writeln!(
        p,
        "The operator pressed Play on the execution plan \"{}\" ({}) of the design \"{}\" ({}) in \
         Code Factory: work it.",
        view.title, view.uid, design_title, view.design
    );
    let _ = writeln!(p);
    strategy_lines(&mut p, &view, picked.as_deref(), &default);
    if view.archived {
        let _ = writeln!(
            p,
            "Every task of this plan is already done or cancelled (it is archived): ask the \
             operator what is left before adding work."
        );
    }
    let _ = writeln!(p);
    if view.steps.is_empty() {
        let _ = writeln!(
            p,
            "It has no steps yet — add them with `jkb design plan step {} <text>`.",
            view.uid
        );
    } else {
        let _ = writeln!(p, "Its steps, in order:");
    }
    for (n, step) in view.steps.iter().enumerate() {
        let _ = writeln!(p);
        let _ = writeln!(p, "{}. {} ({})", n + 1, step.text, step.uid);
        if step.spans.is_empty() {
            let _ = writeln!(p, "   Stages no span of the design yet.");
        } else {
            let _ = writeln!(
                p,
                "   Stages these spans of the design (what it implements):"
            );
            for s in &step.spans {
                let _ = writeln!(p, "   - {} {}: {}", s.uid, s.state, quoted(&s.text));
            }
        }
        if step.tasks.is_empty() {
            let _ = writeln!(p, "   Tasks: none yet.");
        } else {
            let _ = writeln!(p, "   Tasks:");
            for t in &step.tasks {
                let _ = writeln!(p, "   {}", task_line(t));
            }
        }
    }
    let _ = writeln!(p);
    let _ = writeln!(
        p,
        "Read the design first: `jkb design cat {}`; the plan as it stands: `jkb design plan show \
         {}`. Work the steps in order. A step with no tasks needs them: `jkb task add \"<title>\" \
         --under <step uid>`, one per piece of work a branch can carry. Take each task through its \
         lifecycle under its strategy — `jkb task show <uid>`, `jkb task work <uid>` for its own \
         worktree, `jkb task land <uid>` when its gate allows — and never mark one done by hand. \
         Change the design's text only when the operator asks: the staged spans are what this plan \
         implements.",
        view.design, view.uid
    );
    Ok(WorkPrompt {
        kind: "play".to_owned(),
        uid: view.uid,
        title: view.title,
        design: Some(view.design),
        strategy: picked,
        prompt: p,
    })
}

/// The *Play* prompt for one task (D53.6): what it is, where it sits in its design, and the
/// strategy it runs. The session it starts is in the task's own worktree (`jkb task work`).
///
/// # Errors
/// An unknown task, or an item that is not one.
pub fn task_prompt(conn: &Connection, reference: &str) -> Result<WorkPrompt, ApiError> {
    let missing = || ApiError::with_code(ErrorCode::NotFound, format!("no task `{reference}`"));
    let id = jkb_core::task::resolve_ref(conn, reference)?.ok_or_else(missing)?;
    let meta = jkb_core::item::get(conn, id)?.ok_or_else(missing)?;
    if meta.kind != "task" {
        return Err(invalid(format!(
            "`{reference}` is a {}, not a task",
            meta.kind
        )));
    }
    let title = jkb_core::item::title_from(&meta.uid, meta.content.as_deref());
    let strategy = workflow::current(conn, id)?.source;
    let place = design::plan::place_of(conn, id)?;
    let uid = meta.uid;
    let mut p = String::new();
    let _ = writeln!(
        p,
        "The operator pressed Play on the task \"{title}\" ({uid}) in Code Factory: work it. This \
         session is in the task's own worktree, opened by `jkb task work {uid}`."
    );
    let _ = writeln!(p);
    let _ = writeln!(
        p,
        "Status: {}. Workflow strategy: {strategy} — its gates decide what you may do next.",
        meta.status.as_deref().unwrap_or("?")
    );
    if let Some(place) = &place {
        let _ = writeln!(p);
        let _ = writeln!(
            p,
            "It belongs to the design \"{}\" ({}).",
            place.design.1, place.design.0
        );
        if let (Some(plan), Some(step)) = (&place.plan, &place.step) {
            let _ = writeln!(
                p,
                "It is work for the step \"{}\" ({}) of the execution plan \"{}\" ({}).",
                step.1, step.0, plan.1, plan.0
            );
        }
        if !place.spans.is_empty() {
            let _ = writeln!(p, "That step stages these spans of the design:");
            for s in &place.spans {
                let _ = writeln!(p, "- {} {}: {}", s.uid, s.state.as_str(), quoted(&s.text));
            }
        }
    }
    let _ = writeln!(p);
    let design_read = place
        .as_ref()
        .map(|pl| format!(", and the design with `jkb design cat {}`", pl.design.0))
        .unwrap_or_default();
    let _ = writeln!(
        p,
        "Read the task with `jkb task show {uid}`{design_read}. Implement it here, verify it with \
         the repo's own checks, and take it through its lifecycle under its strategy (`jkb task \
         land {uid}` when its gate allows). Never mark it done by hand."
    );
    Ok(WorkPrompt {
        kind: "task".to_owned(),
        design: place.map(|pl| pl.design.0),
        uid,
        title,
        strategy: Some(strategy),
        prompt: p,
    })
}
