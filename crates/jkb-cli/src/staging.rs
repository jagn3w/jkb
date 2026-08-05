//! Staging branches: what is in flight in this repo (design D38.1/D38.2).
//!
//! A **staging branch** is a git branch named by some task's `onto=` facet that still exists
//! in git. There is no `kind='staging'` item and no staging table — every fact here is
//! already authoritative somewhere else:
//!
//! | fact | source |
//! | --- | --- |
//! | which branches are staging branches | tasks' `onto=` facets |
//! | which tasks are on one | the same facets |
//! | whether a session exists | `session::discover` (git worktrees) |
//! | whether it merged to trunk | `gitrepo::is_merged` (squash-safe, D34.2) |
//! | what state a task is in | `items.status` (D27.7) |
//!
//! A staging *item* would add a title and a PR url and would then need reconciling against
//! git — a branch deleted by hand would leave an item claiming to be live. That is the
//! failure D36.2 avoided by refusing a session state file, and it applies here unchanged.
//!
//! This is the **one read** behind both the explorer's branch picker and its In Flight view,
//! so the two cannot disagree about what is live.

use std::collections::BTreeMap;

use anyhow::Result;
use jkb_core::Db;

use crate::{
    facet_one, facet_values, gitrepo, session, RepoCtx, RepoTask, FACET_BRANCH, FACET_ONTO,
};

/// Where a task sits in the pipeline. Derived, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    /// A session worktree exists and the task is not under review.
    Implementing,
    /// Status is `needs_review` — a reviewer is reviewing (D27.7).
    Review,
    /// Done, and its branch is in the staging branch.
    Landed,
}

impl State {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Implementing => "implementing",
            Self::Review => "review",
            Self::Landed => "landed",
        }
    }
}

/// One task as it appears on a staging branch.
pub(crate) struct StagedTask {
    pub(crate) uid: String,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) state: State,
    /// The session branch (`task/<session>`), when one is recorded.
    pub(crate) branch: Option<String>,
    pub(crate) worktree: Option<std::path::PathBuf>,
    pub(crate) dirty: bool,
    /// Commits the session branch has that the staging branch does not.
    pub(crate) commits: usize,
    /// The branch HEAD a review ran against (`reviewed=`), if any.
    pub(crate) reviewed: Option<String>,
    /// The review's findings namespace (`review=`), if any.
    pub(crate) review_ns: Option<String>,
    /// A recorded `--no-review` override (`review-waived=`), if any.
    pub(crate) review_waived: Option<String>,
    /// Open (non-terminal) must-fix findings in that review.
    pub(crate) open_must_fix: usize,
}

/// One staging branch and the tasks landing on it.
pub(crate) struct Staging {
    pub(crate) branch: String,
    /// Already merged into trunk (squash-safe), so it has nothing left to give.
    pub(crate) merged: bool,
    /// Commits it has that trunk does not.
    pub(crate) ahead: usize,
    /// Where it is checked out, if anywhere.
    pub(crate) checkout: Option<std::path::PathBuf>,
    pub(crate) tasks: Vec<StagedTask>,
}

/// Assemble every staging branch in this repo.
///
/// `include_merged` keeps branches that have already landed in trunk. They are hidden by
/// default because a spent batch must never be offered as a land target: joining one both
/// attracts new work onto a dead branch and blocks `git branch -d` (design D36.3).
///
/// # Errors
/// Returns an error if git or the database cannot be read.
pub(crate) fn collect(db: &Db, ctx: &RepoCtx, include_merged: bool) -> Result<Vec<Staging>> {
    let tasks = crate::repo_tasks(db, &ctx.key)?;
    let sessions = session::discover(&ctx.root)?;

    // Group tasks by the branch they land on. A task with no `onto=` is not on a staging
    // branch — it has never been through `task work` — and is simply not in this view.
    let mut by_onto: BTreeMap<String, Vec<&RepoTask>> = BTreeMap::new();
    for t in &tasks {
        if let Some(onto) = facet_one(&t.tags, FACET_ONTO) {
            by_onto.entry(onto.clone()).or_default().push(t);
        }
    }

    let mut out = Vec::new();
    for (branch, group) in by_onto {
        // A branch deleted by hand simply stops being a staging branch — the tags that named
        // it are stale bookkeeping, not evidence that it exists.
        if !gitrepo::has_branch(&ctx.root, &branch)? {
            continue;
        }
        // A branch that adds nothing to trunk is either **landed** or **freshly cut and still
        // empty** — refs alone cannot tell those apart, which is exactly the ambiguity D34.2
        // records. Live work is the tie-break: a batch with a non-terminal task on it is not
        // spent, however few commits it has yet. Without this, the branch cut by the very
        // first `jkb task work` is hidden from the picker that exists to offer it.
        let has_live_work = group
            .iter()
            .any(|t| !matches!(t.meta.status.as_deref(), Some("done" | "cancelled")));
        let merged = !has_live_work
            && match &ctx.trunk {
                Some(trunk) => {
                    gitrepo::is_merged(&ctx.root, &branch, trunk, None)?.0
                        == gitrepo::MergeState::Merged
                }
                None => false,
            };
        if merged && !include_merged {
            continue;
        }
        let ahead = match &ctx.trunk {
            Some(trunk) => gitrepo::ahead_count(&ctx.root, trunk, &branch)?,
            None => 0,
        };

        let mut staged = Vec::new();
        for t in group {
            staged.push(stage_task(ctx, &branch, t, &sessions)?);
        }
        // Most-recently-touched work is what you are looking for; a stable tie-break keeps
        // the listing from reshuffling between redraws.
        staged.sort_by(|a, b| {
            a.state
                .as_str()
                .cmp(b.state.as_str())
                .then(a.uid.cmp(&b.uid))
        });

        out.push(Staging {
            branch: branch.clone(),
            merged,
            ahead,
            checkout: gitrepo::worktree_for_branch(&ctx.root, &branch)?,
            tasks: staged,
        });
    }
    Ok(out)
}

/// Resolve one task's session, state and review standing.
fn stage_task(
    ctx: &RepoCtx,
    onto: &str,
    t: &RepoTask,
    sessions: &[session::Session],
) -> Result<StagedTask> {
    // Match by worktree rather than by "the task's branch tag": a task that picked up a
    // second `branch=` still resolves to the session that actually exists on disk (D36.2).
    let branches = facet_values(&t.tags, FACET_BRANCH);
    let sess = sessions.iter().find(|s| branches.contains(&s.branch));
    let status = t.meta.status.clone().unwrap_or_default();

    let state = if status == "needs_review" {
        State::Review
    } else if status == "done" || sess.is_none() && status != "open" && status != "in_progress" {
        State::Landed
    } else {
        State::Implementing
    };

    let (dirty, commits) = match sess {
        Some(s) => (
            gitrepo::is_dirty(&s.worktree)?,
            gitrepo::ahead_count(&ctx.root, onto, &s.branch)?,
        ),
        None => (false, 0),
    };

    Ok(StagedTask {
        uid: t.meta.uid.clone(),
        title: t.title(),
        status,
        state,
        branch: sess
            .map(|s| s.branch.clone())
            .or_else(|| branches.first().cloned()),
        worktree: sess.map(|s| s.worktree.clone()),
        dirty,
        commits,
        reviewed: facet_one(&t.tags, crate::review::FACET_REVIEWED).cloned(),
        review_ns: facet_one(&t.tags, crate::review::FACET_REVIEW).cloned(),
        review_waived: facet_one(&t.tags, crate::review::FACET_REVIEW_WAIVED).cloned(),
        open_must_fix: 0,
    })
}
