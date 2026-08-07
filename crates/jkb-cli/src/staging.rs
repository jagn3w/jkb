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

use crate::repo::{facet_one, facet_values, RepoCtx, RepoTask, FACET_BRANCH, FACET_ONTO};
use crate::{gitrepo, review, session};

/// Where a task sits in the pipeline. Derived, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    /// A session worktree exists and the task is not under review.
    Implementing,
    /// Status is `needs_review` — a reviewer is reviewing (D27.7).
    Review,
    /// The task is `done`. That is **all** this means (design D38.2): the status is the only
    /// evidence consulted, so a task closed by `close-merged` (its own PR squash-landed on
    /// trunk) or by hand reads `landed` here too. It deliberately does not assert that this
    /// branch contains the task's commits — nothing checks that, and a glyph that claimed it
    /// would be asserting something never verified.
    Landed,
    /// Cancelled: it was on this branch and will not be landing. Distinct from `Landed`,
    /// which is the opposite outcome — reporting the two as one would say a dropped task
    /// shipped.
    Dropped,
}

impl State {
    /// Position in the pipeline, for ordering a listing: live work first, history last.
    fn rank(self) -> u8 {
        match self {
            Self::Implementing => 0,
            Self::Review => 1,
            Self::Landed => 2,
            Self::Dropped => 3,
        }
    }

    /// The pipeline position a task's `items.status` puts it in.
    ///
    /// The one status→state mapping: `land_preflight` had its own terminal-task bail beside
    /// this, so the centralized `Landed`/`Dropped` arm of `land_blocker` was unreachable from
    /// the command that rule is the authority for.
    pub(crate) fn from_status(status: &str) -> Self {
        match status {
            "needs_review" => Self::Review,
            "done" => Self::Landed,
            "cancelled" => Self::Dropped,
            _ => Self::Implementing,
        }
    }

    /// Whether the pipeline is finished with this task, whichever way it ended.
    ///
    /// The one spelling. It was written three times — a `matches!` on the two terminal
    /// statuses, a `matches!` on the two terminal states, and this enum — so the question
    /// "is this task still live" had three answers that had to be kept in step.
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Landed | Self::Dropped)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Implementing => "implementing",
            Self::Review => "review",
            Self::Landed => "landed",
            Self::Dropped => "dropped",
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
    /// **Every** findings namespace this task records (`review=`), in tag order (which is
    /// lexicographic — an application carries no timestamp, so there is no "most recent" to
    /// report). All of them, because the gate unions them: a surface offering only the one
    /// that happened to sort last can open a clean namespace while the count came from
    /// another.
    pub(crate) review_nss: Vec<String>,
    /// A recorded `--no-review` override (`review-waived=`), if any.
    pub(crate) review_waived: Option<String>,
    /// Open (non-terminal) must-fix findings in that review.
    pub(crate) open_must_fix: usize,
    /// Whether the **review** half of the land gate passes on its own. Reported separately
    /// from `land_blocked`, which also carries session and git preconditions — a surface that
    /// used the combined verdict to label a row "reviewed" dropped the label whenever the
    /// session merely had an uncommitted file.
    pub(crate) review_ok: bool,
    /// Why `jkb task land` would refuse this task right now, or `None` if it would go ahead.
    ///
    /// Computed **here**, by the same code that owns the rule, and rendered verbatim by the
    /// UI. The UI used to re-derive it from this row's fields, which could not express two of
    /// the CLI's preconditions — a missing worktree and a review namespace holding no
    /// findings at all — so a row read "Landable" for tasks `land` then refused outright.
    pub(crate) land_blocked: Option<String>,
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
    let tasks = crate::repo::repo_tasks(db, &ctx.key)?;
    let sessions = session::discover(&ctx.root)?;

    // Group tasks by the branch they land on. A task with no `onto=` is not on a staging
    // branch — it has never been through `task work` — and is simply not in this view.
    let mut by_onto: BTreeMap<String, Vec<&RepoTask>> = BTreeMap::new();
    for t in &tasks {
        if let Some(onto) = facet_one(&t.tags, FACET_ONTO) {
            by_onto.entry(onto.clone()).or_default().push(t);
        }
    }

    // Everything git-wide is resolved ONCE, before the loop. This read backs a view that
    // refreshes on every database write, and the per-branch shape (a `show-ref` plus a
    // `merge-tree` probe plus a full `worktree list`, ~6 spawns at ~11ms each) meant a repo
    // with a dozen branches spent about a second per redraw — which during a swarm run, where
    // claims and tags land continuously, never caught up.
    let existing = gitrepo::local_branches(&ctx.root)?;
    let worktrees = gitrepo::worktrees(&ctx.root)?;
    let mut cache = Cache::default();

    let mut out = Vec::new();
    for (branch, group) in by_onto {
        // A branch deleted by hand simply stops being a staging branch — the tags that named
        // it are stale bookkeeping, not evidence that it exists.
        if !existing.contains(&branch) {
            continue;
        }
        // A branch that adds nothing to trunk is either **landed** or **freshly cut and still
        // empty** — refs alone cannot tell those apart, which is exactly the ambiguity D34.2
        // records. Live work is the tie-break: a batch with a non-terminal task on it is not
        // spent, however few commits it has yet. Without this, the branch cut by the very
        // first `jkb task work` is hidden from the picker that exists to offer it.
        let has_live_work = group.iter().any(|t| {
            !State::from_status(t.meta.status.as_deref().unwrap_or_default()).is_terminal()
        });
        // The merge probe (`merge-tree --write-tree`, several `rev-parse`s) only runs when
        // live work has not already answered the question — and its answer is only *needed*
        // when a merged branch would be hidden.
        let merged = if has_live_work {
            false
        } else {
            match &ctx.trunk {
                Some(trunk) => {
                    // The **local** ref, not `origin/<branch>` (design D34.2's preference is
                    // right for `close-merged` and wrong here): a staging branch whose pushed
                    // copy merged, but which has since had another task landed onto it
                    // locally, still has commits to give. Asking the remote reported it
                    // merged and hid it — while the row's own `ahead`, read from the local
                    // ref, said otherwise.
                    gitrepo::is_merged(&ctx.root, &branch, trunk, None, gitrepo::Prefer::Local)?.0
                        == gitrepo::MergeState::Merged
                }
                None => false,
            }
        };
        if merged && !include_merged {
            continue;
        }
        let ahead = match &ctx.trunk {
            Some(trunk) => gitrepo::ahead_count(&ctx.root, trunk, &branch)?,
            None => 0,
        };

        // Per branch, not per task: whether the target checkout is dirty is a property of
        // the branch, and it is the one land precondition a row could not previously see.
        let target_dirty =
            target_dirty_reason(&worktrees, &ctx.root, &branch, &mut cache.target_dirty)?;
        let branch_ctx = BranchCtx {
            onto: &branch,
            sessions: &sessions,
            existing: &existing,
            target_dirty: target_dirty.as_deref(),
        };
        let mut staged = Vec::new();
        for t in group {
            staged.push(stage_task(db, ctx, &branch_ctx, t, &mut cache)?);
        }
        // Pipeline order — what is being built, then what is under review, then what is
        // finished — with a stable uid tie-break so the listing does not reshuffle between
        // redraws. Sorting on the state *string* was alphabetical, which put `dropped` first.
        staged.sort_by_key(|t| (t.state.rank(), t.uid.clone()));

        out.push(Staging {
            branch: branch.clone(),
            merged,
            ahead,
            checkout: worktrees
                .iter()
                .find(|w| w.branch.as_deref() == Some(branch.as_str()))
                .map(|w| w.path.clone()),
            tasks: staged,
        });
    }
    Ok(out)
}

/// Per-`collect` memo for the two answers that are **per branch and per review**, not per
/// task, and were being recomputed identically for every row.
///
/// `/task-swarm` tags every task of a group with the same `branch=`/`onto=` pair, and
/// `merge-queue.sh` never deletes a group branch, so a run that worked 40 tasks left ~10
/// distinct pairs behind 40 rows: 40 `git rev-list --count` spawns and 40 byte-identical
/// findings queries — each a subtree scan serialized on the writer thread — on a view the
/// extension refreshes on every database write. The git calls that vary only per *branch*
/// were already hoisted out of this loop (see `collect`); these two were left in it.
#[derive(Default)]
struct Cache {
    /// Commits `.1` has that `.0` does not.
    ahead: BTreeMap<(String, String), usize>,
    /// The findings of a set of review namespaces, keyed by that set.
    findings: BTreeMap<Vec<String>, std::rc::Rc<review::Findings>>,
    /// Whether a checkout has uncommitted changes, keyed by directory.
    target_dirty: BTreeMap<std::path::PathBuf, bool>,
}

impl Cache {
    fn ahead(&mut self, root: &std::path::Path, onto: &str, branch: &str) -> Result<usize> {
        let key = (onto.to_owned(), branch.to_owned());
        if let Some(n) = self.ahead.get(&key) {
            return Ok(*n);
        }
        let n = gitrepo::ahead_count(root, onto, branch)?;
        self.ahead.insert(key, n);
        Ok(n)
    }

    fn findings(&mut self, db: &Db, nss: &[String]) -> Result<std::rc::Rc<review::Findings>> {
        if let Some(f) = self.findings.get(nss) {
            return Ok(f.clone());
        }
        let f = std::rc::Rc::new(review::findings_in(db, nss)?);
        self.findings.insert(nss.to_vec(), f.clone());
        Ok(f)
    }
}

/// What every task on one staging branch shares: the branch itself, the sessions and branches
/// that exist in git, and whether that branch's checkout is too dirty to land into. Resolved
/// once per branch in [`collect`] and passed down, so no row repeats the work.
struct BranchCtx<'a> {
    onto: &'a str,
    sessions: &'a [session::Session],
    existing: &'a std::collections::BTreeSet<String>,
    target_dirty: Option<&'a str>,
}

/// Resolve one task's session, state and review standing.
fn stage_task(
    db: &Db,
    ctx: &RepoCtx,
    branch_ctx: &BranchCtx<'_>,
    t: &RepoTask,
    cache: &mut Cache,
) -> Result<StagedTask> {
    let BranchCtx {
        onto,
        sessions,
        existing,
        target_dirty,
    } = *branch_ctx;
    // Match by worktree rather than by "the task's branch tag": a task that picked up a
    // second `branch=` still resolves to the session that actually exists on disk (D36.2).
    let branches = facet_values(&t.tags, FACET_BRANCH);
    let sess = sessions.iter().find(|s| branches.contains(&s.branch));
    let status = t.meta.status.clone().unwrap_or_default();

    // Read the status directly. An earlier version inferred `landed` from "no session and
    // not open/in_progress", which quietly called a **cancelled** task landed — the one
    // status that means the opposite.
    let state = State::from_status(&status);

    // The branch this task's work is on: its session's, or — for a swarm task, whose branch
    // is not a `.jkb/work` session — whichever `branch=` it recorded that still exists.
    let work_branch = sess.map(|s| s.branch.clone()).or_else(|| {
        branches
            .iter()
            .find(|b| existing.contains(*b))
            .or_else(|| branches.first())
            .cloned()
    });

    // A terminal task is history: it is not landing, so its commit count cannot change what
    // the row says or what the gate would do, and counting it costs a `git rev-list` per row
    // — the bulk of the rows after a swarm run. (Only the commit count is skipped; see
    // `dirty` below.)
    //
    // Its FINDINGS are still read. They are not a land precondition for a finished task, but
    // "3 must-fix open" is true of a task landed with `--no-review` over three of them, and
    // zeroing it made the row read `landed · reviewed` — the field's own doc says "open
    // must-fix findings in that review", not "…unless it landed". The cache collapses them to
    // one query per distinct namespace set, so this is nearly free.
    let terminal = state.is_terminal();

    // `dirty` is only knowable with a checkout, but commits are not: counting them from the
    // recorded branch is what makes a swarm task — which `/task-swarm` tags specifically so
    // it shows up here — report the work it actually has. Deriving both from a session
    // worktree made every swarm row claim 0 commits, and the tooltip assert it had nothing
    // to land while its branch was several commits ahead.
    // `dirty` stays real whenever a checkout exists, terminal or not. Forcing it false for
    // terminal rows hid uncommitted work in the worktree of a cancelled task — precisely the
    // row where Abandon is now offered, so the confirmation dropped its "changes will be
    // lost" warning and the command then refused without `--force`. The cost the skip was
    // for is the commit count (`git rev-list` per row), not `git status`.
    let dirty = match sess {
        Some(s) => gitrepo::is_dirty(&s.worktree)?,
        None => false,
    };
    let commits = match &work_branch {
        Some(b) if !terminal && existing.contains(b) => cache.ahead(&ctx.root, onto, b)?,
        _ => 0,
    };

    // The same findings the land gate reads, through the same function — so a row can never
    // say "reviewed" about a task the gate is about to refuse.
    // Every recorded review, not just the newest: a second `/review-log` run must not retire
    // the first run's still-open must-fix findings.
    let review_nss = facet_values(&t.tags, review::FACET_REVIEW).to_vec();
    let found = cache.findings(db, &review_nss)?;
    let worktree = sess.map(|s| s.worktree.clone());
    let verdict = review::gate_with(&found, &t.tags, &review_nss);
    let review_ok = matches!(verdict, review::GateVerdict::Passed);
    let land_blocked = land_blocker(&LandFacts {
        state,
        worktree: worktree.as_deref(),
        dirty,
        commits,
        branch_exists: work_branch.as_ref().is_some_and(|b| existing.contains(b)),
        target_dirty,
        verdict: Some(&verdict),
    });

    Ok(StagedTask {
        uid: t.meta.uid.clone(),
        title: t.title(),
        status,
        state,
        branch: work_branch,
        worktree,
        dirty,
        commits,
        reviewed: facet_one(&t.tags, review::FACET_REVIEWED).cloned(),
        review_nss,
        review_waived: facet_one(&t.tags, review::FACET_REVIEW_WAIVED).cloned(),
        open_must_fix: found.open_must_fix.len(),
        review_ok,
        land_blocked,
    })
}

/// Which checkout a land onto `onto` will graft in, given the worktrees that exist.
///
/// `land_dir_for` resolves this same thing and then *mutates* — creating or switching
/// `.jkb/base` — so only the lookup lives here, and both it and the row read the same answer
/// from the same list. `None` means the graft will land in a `.jkb/base` that has to be
/// created, which cannot be dirty because it does not exist yet.
fn land_dir_in(
    worktrees: &[gitrepo::Worktree],
    root: &std::path::Path,
    onto: &str,
) -> Option<std::path::PathBuf> {
    if let Some(w) = worktrees.iter().find(|w| w.branch.as_deref() == Some(onto)) {
        return Some(w.path.clone());
    }
    // No checkout of its own: the graft reuses `.jkb/base`, whatever branch that currently
    // holds. A dirty one refuses the landing — which is why the row must consider it too, and
    // did not: it asked only about a checkout of `onto` itself and reported "Landable" for
    // every task whose target had none.
    let base = session::base_worktree(root);
    worktrees
        .iter()
        .find(|w| session::same_path(&w.path, &base))
        .map(|w| w.path.clone())
}

/// Why the land target cannot be grafted into right now, or `None`.
///
/// A pure read, so the CLI's preflight and this view's rows ask the same question of the same
/// `git worktree list` — `collect` fetches that list once per run, rather than re-spawning
/// `git worktree list` per branch as the first version did.
pub(crate) fn target_dirty_reason(
    worktrees: &[gitrepo::Worktree],
    root: &std::path::Path,
    onto: &str,
    dirty: &mut BTreeMap<std::path::PathBuf, bool>,
) -> Result<Option<String>> {
    let Some(dir) = land_dir_in(worktrees, root, onto) else {
        return Ok(None);
    };
    // Memoized per directory: staging branches usually have no checkout of their own, so
    // every one of them resolves to the single `.jkb/base` and asked it the same
    // `git status --porcelain` — once per branch, on a view that re-reads on every database
    // write. Only the message differs per branch.
    let is_dirty = if let Some(known) = dirty.get(&dir) {
        *known
    } else {
        let answer = gitrepo::is_dirty(&dir)?;
        dirty.insert(dir.clone(), answer);
        answer
    };
    if !is_dirty {
        return Ok(None);
    }
    Ok(Some(format!(
        "{} (the checkout a land onto {onto} would graft in) has uncommitted changes — \
         landing rolls it back on a red gate, which would take them with it. Commit or stash \
         them first.",
        dir.display()
    )))
}

/// The facts a land verdict is computed from. Assembled by the row (`stage_task`) and by the
/// command (`land_preflight`) from the same git and KB reads.
pub(crate) struct LandFacts<'a> {
    pub(crate) state: State,
    pub(crate) worktree: Option<&'a std::path::Path>,
    pub(crate) dirty: bool,
    /// Commits the work branch has that the staging branch does not.
    pub(crate) commits: usize,
    /// Whether the task's recorded work branch exists in git. Distinguishes a swarm task
    /// (branch, no session) from an abandoned one (neither).
    pub(crate) branch_exists: bool,
    /// Why the checkout the graft would happen in cannot take it, if it cannot.
    pub(crate) target_dirty: Option<&'a str>,
    /// The review gate's verdict, or `None` when the caller applies the review rule itself.
    ///
    /// `cmd_task_land` passes `None`: it calls `review::enforce` a moment later, which is the
    /// same [`review::GateVerdict`] rendered at length — naming each open finding and the way
    /// out — and which is also where `--no-review` records a waiver instead of refusing. One
    /// rule, two renderings; the row gets the one-liner.
    pub(crate) verdict: Option<&'a review::GateVerdict>,
}

/// Why `jkb task land` would refuse this task, or `None` if it would go ahead.
///
/// **The** rule, in one place: `cmd_task_land` bails with this string and the In Flight row
/// renders it, so the two cannot describe different rules. They did, twice — the row was a
/// TypeScript restatement that could not see whether a worktree existed, and then a Rust
/// restatement that did not know which checkout the graft would use — and each time the row
/// said "Landable" for a task the command refused outright.
///
/// Only the checks a *row* can be missing are here. `land_preflight` keeps three of its own
/// (no branch recorded, no `onto=`, a land target that no longer exists), because a task in
/// that state is not on a staging branch at all and so has no row to disagree with.
pub(crate) fn land_blocker(facts: &LandFacts<'_>) -> Option<String> {
    match facts.state {
        State::Landed => {
            return Some(
                "It is done — there is nothing left to land. Reopen it (`jkb task set <uid> \
                 --status open`) if the work should go ahead after all."
                    .to_owned(),
            )
        }
        State::Dropped => {
            return Some(
                "It was cancelled, so it will not be landing. Reopen it (`jkb task set <uid> \
                 --status open`) if the work should go ahead after all."
                    .to_owned(),
            );
        }
        State::Implementing | State::Review => {}
    }
    if facts.worktree.is_none() {
        // A task whose branch exists but has no `.jkb/work` checkout never had a session:
        // that is what a `/task-swarm` task looks like, and this view shows those on purpose.
        // Telling its owner to run `jkb task work` was worse than unhelpful — following the
        // advice cuts a second branch and overwrites the group's `branch=`/`onto=` facets,
        // detaching the task from the batch the merge queue is about to land.
        return Some(if facts.branch_exists {
            "It has no session checkout — it is being built elsewhere (a swarm group lands \
             through the merge queue, not through `jkb task land`)."
                .to_owned()
        } else {
            "It has no session checkout — its worktree was abandoned or removed. Re-open one \
             with `jkb task work`."
                .to_owned()
        });
    }
    if facts.dirty {
        return Some("It has uncommitted changes — commit them in the session first.".to_owned());
    }
    if facts.commits == 0 {
        return Some("It has no commits that the staging branch does not.".to_owned());
    }
    if let Some(reason) = facts.target_dirty {
        return Some(reason.to_owned());
    }
    facts.verdict.and_then(review::GateVerdict::short)
}
