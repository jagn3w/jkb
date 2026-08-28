//! Staging branches: what is in flight in this repo (design D38.1/D38.2).
//!
//! A **staging branch** is a git branch named as some branch's **land target** that still
//! exists in git. There is no `kind='staging'` item and no staging table — every fact here is
//! already authoritative somewhere else:
//!
//! | fact | source |
//! | --- | --- |
//! | which branches are staging branches | the `onto` label on each task's latest transition |
//! | which branches exist | `gitrepo::branch_refs` (D38.1: a record is never evidence one does) |
//! | whether a session exists | `session::discover` (git worktrees) |
//! | whether a batch is spent | every task on it is terminal (D48) |
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
use jkb_fsm::Fact;

use crate::repo::{facet_one, facet_values, RepoCtx, RepoTask, FACET_BRANCH};
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
            // Split rather than collapsed to one terminal arm: `landed` and `dropped` are
            // opposite outcomes, and reporting them as one says a dropped task shipped.
            "done" => Self::Landed,
            "cancelled" => Self::Dropped,
            _ => Self::Implementing,
        }
    }

    /// Whether the pipeline is finished with this task, whichever way it ended.
    ///
    /// The `State`-level counterpart of `jkb_types::TaskStatus::is_terminal_str`, which is the
    /// authority over the raw strings. Kept separate because it answers a different question —
    /// `State` folds a session's existence in beside the status — but the two must agree on
    /// which statuses end a task, so this is derived from `from_status`, never re-listed.
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
    /// Whether the session checkout has uncommitted changes — **three-valued**, because a
    /// checkout git cannot read is not a clean one and the land gate refuses it. Rendered as
    /// `"yes"`/`"no"`/`"unknown"`, never as a boolean that would make the third case look
    /// like the landable one.
    pub(crate) dirty: Fact,
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

    // Group tasks by the branch they land on, read from each task's own **history** — the last
    // time anybody said where its work lands. A task that has never been through `task work` or
    // `task start` has no such entry and is simply not in this view.
    //
    // (D38.1 still holds: which branches *exist* comes from git, below. A recorded target is
    // never evidence that a branch is there.)
    //
    // This used to read a `land_target` column keyed by branch, which had to be kept in agreement
    // with a world where branches are deleted and their names reused. A history entry is a
    // statement about a moment, so it needs no such agreement — and two tasks told different
    // targets are now two entries with timestamps rather than one row silently keeping whichever
    // wrote last.
    // **One** read for the whole listing, not one per task. This view redraws on every database
    // write and every `db.read` is a round trip to the single writer thread, so a per-task read
    // is the N+1 shape `repo_tasks` already exists to avoid — and it replaced a batched read when
    // the branch records went.
    //
    // The two per-task facts are gathered together: where the task's work lands, and whether it
    // is held by an open subtask.
    let ids: Vec<jkb_types::ItemId> = tasks.iter().map(|t| t.meta.id).collect();
    let per_task: Vec<(Option<String>, bool)> = db.read(move |conn| {
        ids.iter()
            .map(|id| {
                Ok((
                    jkb_core::transition::land_target(conn, *id)?,
                    !jkb_core::task::subtasks_all_terminal(conn, *id)?,
                ))
            })
            .collect()
    })?;
    let held: std::collections::HashSet<jkb_types::ItemId> = tasks
        .iter()
        .zip(&per_task)
        .filter(|(_, (_, open_subtasks))| *open_subtasks)
        .map(|(t, _)| t.meta.id)
        .collect();

    let mut by_onto: BTreeMap<String, Vec<&RepoTask>> = BTreeMap::new();
    for (t, (target, _)) in tasks.iter().zip(&per_task) {
        if let Some(target) = target {
            by_onto.entry(target.clone()).or_default().push(t);
        }
    }

    // Everything git-wide is resolved ONCE, before the loop. This read backs a view that
    // refreshes on every database write, and the per-branch shape (a `show-ref` plus a
    // `merge-tree` probe plus a full `worktree list`, ~6 spawns at ~11ms each) meant a repo
    // with a dozen branches spent about a second per redraw — which during a swarm run, where
    // claims and tags land continuously, never caught up.
    // Counting remote-tracking copies, because `task work` and `task land` do: a batch whose
    // local ref was pruned is still live, and dropping it here hid the task from the picker and
    // from In Flight while both of those went on acting on it.
    //
    // The **resolved ref**, not membership. A remote-only branch's bare name resolves to nothing,
    // so counting commits with it failed and read as zero: the row said "0 commits" and refused a
    // landing the command performed — the row-versus-command divergence this single read exists to
    // prevent, in the exact case remote-inclusive existence was added to support.
    let existing = gitrepo::branch_refs(&ctx.root)?;
    // NOT collapsed here. `target_dirty_reason` takes the `Option` and states what an unanswered
    // listing means for a landing; `Staging::checkout` below takes the "no checkouts" reading,
    // which is the direction the row already takes for a branch it cannot resolve.
    let worktrees = gitrepo::worktrees(&ctx.root)?;
    let mut cache = Cache::default();

    let mut out = Vec::new();
    for (branch, group) in by_onto {
        // A branch deleted by hand simply stops being a staging branch — the tags that named
        // it are stale bookkeeping, not evidence that it exists.
        let Some(branch_ref) = existing.get(&branch).cloned() else {
            continue;
        };
        // A batch is **spent** when every task on it has finished, one way or the other. That
        // is now the whole test, and it is the answer the old merge probe was reaching for.
        //
        // It used to ask `merge-tree` whether the branch added anything to trunk, and then had
        // to hand-correct the answer, because a branch that adds nothing is either landed *or*
        // freshly cut and still empty and refs cannot tell those apart — so live work was the
        // tie-break, or the branch cut by the very first `jkb task work` was hidden from the
        // picker that exists to offer it. With live work deciding both ways, the probe only
        // ever confirmed what the statuses already said, at the cost of several git spawns per
        // branch on a view that redraws on every database write.
        let spent = group.iter().all(|t| {
            State::from_status(t.meta.status.as_deref().unwrap_or_default()).is_terminal()
        });
        if spent && !include_merged {
            continue;
        }
        let ahead = match &ctx.trunk {
            Some(trunk) => gitrepo::ahead_count(&ctx.root, trunk, &branch_ref)?,
            None => 0,
        };

        // Per branch, not per task: whether the target checkout is dirty is a property of
        // the branch, and it is the one land precondition a row could not previously see.
        let target_dirty = target_dirty_reason(
            worktrees.as_deref(),
            &ctx.root,
            &branch,
            &mut cache.target_dirty,
        )?;
        let branch_ctx = BranchCtx {
            onto_ref: &branch_ref,
            sessions: &sessions,
            existing: &existing,
            target_dirty: target_dirty.as_deref(),
        };
        let mut staged = Vec::new();
        for t in group {
            staged.push(stage_task(db, ctx, &branch_ctx, t, &held, &mut cache)?);
        }
        // Pipeline order — what is being built, then what is under review, then what is
        // finished — with a stable uid tie-break so the listing does not reshuffle between
        // redraws. Sorting on the state *string* was alphabetical, which put `dropped` first.
        staged.sort_by_key(|t| (t.state.rank(), t.uid.clone()));

        out.push(Staging {
            branch: branch.clone(),
            merged: spent,
            ahead,
            // "no checkout" either way — the same direction the row already takes for a branch
            // it cannot resolve, and unlike the dirty question there is no act gated on it.
            checkout: worktrees
                .iter()
                .flat_map(|ws| ws.iter())
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
/// `/task-swarm` puts every task of a group on the same branch, with one land target, and
/// `merge-queue.sh` never deletes a group branch, so a run that worked 40 tasks left ~10
/// distinct pairs behind 40 rows: 40 `git rev-list --count` spawns and 40 byte-identical
/// findings queries — each a subtree scan serialized on the writer thread — on a view the
/// extension refreshes on every database write. The git calls that vary only per *branch*
/// were already hoisted out of this loop (see `collect`); these two were left in it.
#[derive(Default)]
struct Cache {
    /// Commits `.1` has that `.0` does not. Keyed on the **resolved refs**, which is what
    /// `ahead_count` takes — a bare branch name may not resolve at all.
    ahead: BTreeMap<(String, String), usize>,
    /// The findings of a set of review namespaces, keyed by that set.
    findings: BTreeMap<Vec<String>, std::rc::Rc<review::Findings>>,
    /// Whether a checkout has uncommitted changes, keyed by directory.
    target_dirty: BTreeMap<std::path::PathBuf, Fact>,
}

impl Cache {
    /// Both arguments are refs that resolve here (see `gitrepo::branch_refs`), never bare
    /// branch names.
    fn ahead(&mut self, root: &std::path::Path, onto_ref: &str, work_ref: &str) -> Result<usize> {
        let key = (onto_ref.to_owned(), work_ref.to_owned());
        if let Some(n) = self.ahead.get(&key) {
            return Ok(*n);
        }
        let n = gitrepo::ahead_count(root, onto_ref, work_ref)?;
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
    /// The ref that resolves to the staging branch here — its own name, or `origin/<onto>` when
    /// only the remote-tracking copy exists. Every git question about it is asked with this, and
    /// the plain name is kept out so no question can be asked with a name git cannot resolve.
    onto_ref: &'a str,
    sessions: &'a [session::Session],
    /// Branch name → the ref that resolves to it, for every branch this repo has.
    existing: &'a BTreeMap<String, String>,
    target_dirty: Option<&'a str>,
}

/// Resolve one task's session, state and review standing.
fn stage_task(
    db: &Db,
    ctx: &RepoCtx,
    branch_ctx: &BranchCtx<'_>,
    t: &RepoTask,
    held: &std::collections::HashSet<jkb_types::ItemId>,
    cache: &mut Cache,
) -> Result<StagedTask> {
    let BranchCtx {
        onto_ref,
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
    let work_branch = crate::repo::work_branch(sess.map(|s| s.branch.as_str()), branches, existing);

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
        Some(s) => gitrepo::is_dirty(&s.worktree, &ctx.root)?,
        // No checkout to be dirty. `Fact::No` and not `Unknown`: this is established — there is
        // nothing there — rather than unobserved, and `land_blocker` refuses a session-less task
        // on its own arm above this one anyway.
        None => Fact::No,
    };
    // Both operands are refs that resolve here, never bare names: `ahead_count` now refuses an
    // unresolvable one rather than reporting the count as zero, and zero is what tells the row
    // there is nothing to land.
    let work_ref = work_branch.as_ref().and_then(|b| existing.get(b));
    let commits = match work_ref {
        Some(r) if !terminal => cache.ahead(&ctx.root, onto_ref, r)?,
        _ => 0,
    };

    // The same findings the land gate reads, through the same function — so a row can never
    // say "reviewed" about a task the gate is about to refuse.
    // Every recorded review, not just the newest: a second `/review-log` run must not retire
    // the first run's still-open must-fix findings.
    let review_nss = facet_values(&t.tags, review::FACET_REVIEW).to_vec();
    let found = cache.findings(db, &review_nss)?;
    // Read once for the whole listing, from `containment` — the same source the command and the
    // machine read, so the row and the land it describes cannot disagree about which parents are
    // held. Terminal work is past the question.
    let open_subtasks = !terminal && held.contains(&t.meta.id);
    let worktree = sess.map(|s| s.worktree.clone());
    let verdict = review::gate_with(&found, &t.tags, &review_nss);
    let review_ok = matches!(verdict, review::GateVerdict::Passed);
    let land_blocked = land_blocker(&LandFacts {
        state,
        open_subtasks,
        worktree: worktree.as_deref(),
        dirty,
        commits,
        branch_exists: work_ref.is_some(),
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
    // holds — or none, if it is detached. A dirty one refuses the landing, which is why the
    // row must consider it too, and did not: it asked only about a checkout of `onto` itself
    // and reported "Landable" for every task whose target had none.
    //
    // Matched by **path**, exactly as `main::land_dir_for` decides whether to reuse it. Those
    // two must agree on what counts as a usable base or the row and the command describe
    // different landings; they diverged once on a detached base, where the command required a
    // branch and refused while the row matched by path and promised.
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
///
/// **`worktrees` is `Option`, and the collapse lives here rather than at each caller.** Both
/// callers used to `unwrap_or_default()` a `git worktree list` that had not answered, each with
/// its own written argument for why that was safe, and for this consumer neither argument held:
/// an empty list makes `land_dir_in` answer `None`, whose premise is *the graft will land in a
/// `.jkb/base` that has to be created, which cannot be dirty because it does not exist yet* — and
/// an existing, dirty `.jkb/base` falsifies exactly that. The row then read "Landable" for a task
/// `jkb task land` refuses with a different message, which is the row-versus-command divergence
/// [`land_blocker`](crate::repo) exists to prevent. A question git did not answer is not an
/// answer, and one helper stating that once beats two call sites reasoning about it.
pub(crate) fn target_dirty_reason(
    worktrees: Option<&[gitrepo::Worktree]>,
    root: &std::path::Path,
    onto: &str,
    dirty: &mut BTreeMap<std::path::PathBuf, Fact>,
) -> Result<Option<String>> {
    let Some(worktrees) = worktrees else {
        return Ok(Some(format!(
            "git could not list this repository's worktrees, so the checkout a land onto {onto} \
             would graft in cannot be identified, let alone established as clean. \
             `git -C {} worktree list` will say what it makes of the repository.",
            root.display()
        )));
    };
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
        let answer = gitrepo::is_dirty(&dir, root)?;
        dirty.insert(dir.clone(), answer);
        answer
    };
    // Proven clean, or the landing is refused. A checkout git could not read is not one to
    // graft into and roll back on a red gate.
    if is_dirty.is_no() {
        return Ok(None);
    }
    if is_dirty.is_unknown() {
        return Ok(Some(format!(
            "{} (the checkout a land onto {onto} would graft in) could not be read by git, so \
             it cannot be established as safe to graft into. `git -C {} status` will say what \
             it makes of the directory.",
            dir.display(),
            dir.display()
        )));
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
    /// Whether the task still has a non-terminal subtask (design D34.1: you work the leaves).
    ///
    /// Judged **here**, with every other precondition, so the refusal happens before the graft.
    /// It was checked only inside the machine's `land` guard, which `transition::perform` runs
    /// *last* — after the rebase, the fast-forward, the gate and the session disposal — so the
    /// rule reported on a branch that had already moved and a worktree that was already gone,
    /// three lines below a comment saying a refusal must not have moved a branch first.
    pub(crate) open_subtasks: bool,
    pub(crate) worktree: Option<&'a std::path::Path>,
    /// Whether the session checkout has uncommitted changes. `land_blocker` requires it
    /// **proven** clean (`is_no`), so an unreadable checkout refuses rather than landing.
    pub(crate) dirty: Fact,
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
/// (no branch recorded, no land target, one that no longer exists), because a task in
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
        // advice cuts a second branch and overwrites the group's recorded branch and target,
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
    // PROVEN CLEAN, or it does not land. `is_dirty` used to collapse a failed `git status` into
    // `false`, so a session whose `.git` had been unlinked part-way passed every check here,
    // was grafted, and only then hit `archive::dispose`'s refusal to record a session with no
    // readable HEAD — which is after the graft, so the work was in the target, the task was not
    // `done`, and the second run was refused by the `commits == 0` arm below. Frozen, holding a
    // live session, with the branch pointing at what had already landed.
    if !facts.dirty.is_no() {
        return Some(if facts.dirty.is_unknown() {
            "Git could not read its session checkout, so it cannot be established as clean. \
             `git -C <worktree> status` will say what git makes of the directory."
                .to_owned()
        } else {
            "It has uncommitted changes — commit them in the session first.".to_owned()
        });
    }
    if facts.commits == 0 {
        return Some("It has no commits that the staging branch does not.".to_owned());
    }
    if facts.open_subtasks {
        return Some(
            "It still has open subtasks — you work the leaves, and the parent lands after them."
                .to_owned(),
        );
    }

    if let Some(reason) = facts.target_dirty {
        return Some(reason.to_owned());
    }
    facts.verdict.and_then(review::GateVerdict::short)
}

#[cfg(test)]
mod tests {
    use super::{land_blocker, target_dirty_reason, LandFacts, State};
    use jkb_fsm::Fact;
    use std::collections::BTreeMap;
    use std::path::Path;

    /// A `git worktree list` that did not answer REFUSES, and says so about the listing.
    ///
    /// The arm has no other route into it — it needs git to fail in the repo — so it is asked of
    /// the helper directly. It is worth pinning because the direction is the whole point of
    /// taking an `Option` here: both callers used to `unwrap_or_default()`, and an empty list
    /// makes `land_dir_in` answer `None`, which this function reads as *nothing to be dirty*.
    /// Silence and cleanliness must not share a spelling in the one precondition a row renders
    /// and the command enforces.
    #[test]
    fn a_worktree_listing_that_did_not_answer_is_not_a_clean_target() {
        let mut cache = BTreeMap::new();
        let reason = target_dirty_reason(None, Path::new("/repo"), "batch", &mut cache)
            .expect("pure, so it cannot fail here")
            .expect("an unanswered listing is not a clean target");
        assert!(
            reason.contains("could not list") && reason.contains("batch"),
            "refused for the listing, and about the branch asked about: {reason}"
        );
        // And the control: a listing that answered, holding no checkout for the branch, is clean.
        assert_eq!(
            target_dirty_reason(Some(&[]), Path::new("/repo"), "batch", &mut cache).expect("pure"),
            None,
            "an empty ANSWER is a real answer — only the absent one refuses"
        );
    }

    /// Everything else about the task is landable, so only `dirty` decides.
    fn facts(dirty: Fact) -> LandFacts<'static> {
        LandFacts {
            state: State::Implementing,
            open_subtasks: false,
            worktree: Some(Path::new("/tmp/session")),
            dirty,
            commits: 3,
            branch_exists: true,
            target_dirty: None,
            verdict: None,
        }
    }

    /// A session checkout git could not read does NOT land.
    ///
    /// The chain this closes, which round 7 opened: `is_dirty` mapped a failed `git status` to
    /// `false`, so a worktree whose `.git` had been unlinked part-way read as clean here, was
    /// grafted, and only then met `archive::dispose`'s refusal to record a session with no
    /// readable HEAD. That refusal is correct and it is far too late — `dispose` runs after the
    /// graft, so the work was already in the target while the task stayed non-terminal, and the
    /// second run was refused by the `commits == 0` arm because the branch now added nothing.
    /// Frozen, holding a live session, with no verb that moves it.
    #[test]
    fn a_session_checkout_git_cannot_read_does_not_land() {
        let blocked = land_blocker(&facts(Fact::Unknown));
        let reason = blocked.expect("an unreadable checkout is not landable");
        assert!(
            reason.contains("could not read"),
            "refused for being unreadable, not for some other reason: {reason}"
        );

        // The polarity, stated as the assertion: `is_no()` and not `!is_yes()`. This is what
        // fails if the guard is ever written as `if facts.dirty.is_yes()`.
        assert!(
            land_blocker(&facts(Fact::Yes)).is_some(),
            "a dirty checkout still does not land"
        );
        assert!(
            land_blocker(&facts(Fact::No)).is_none(),
            "and a checkout PROVEN clean still does — the guard must not refuse everything"
        );
    }
}
