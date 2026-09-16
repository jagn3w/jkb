//! The session verbs — `jkb task start`, `work`, `abandon`, `sessions` and `gate` — as clients of the
//! typed operations (tasks S6.4, `openspec/changes/jkb-message-queue/design-s6-4.md`).
//!
//! Each does its git and filesystem work where it runs and its database work through a [`Backend`]
//! ([`Kb`]): the host CLI's in-process one, or `jkb serve` from the dev container — the same code
//! either way, as for every ported command. The rules these verbs follow are unchanged by the move;
//! what changed is only where each database step runs (`jkb_api::sessions`), and that every write the
//! verbs make is now a compare-and-set on the owner they judged.
//!
//! `start` and the gate read run in both modes. `work`, `abandon` and `sessions` still read the
//! host's worktree-removal records beside the database, so they run on the host only until those
//! records move into the database (stage 3).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jkb_api::sessions::{Abandoned, BranchTask, Place, StartAsk, Take, TakeAsk, TaskState};
use jkb_api::{Backend, Request, Response, SessionStateIs};
use jkb_types::AgentId;

use crate::ops_cli::{op_error, unexpected, Ops};
use crate::{archive, branch_fate, gitrepo, owner, presence, repo, session, BranchFate};

/// The database, through whichever backend serves this command.
pub(crate) struct Kb<'a> {
    backend: &'a dyn Backend,
    remote: bool,
}

impl<'a> Kb<'a> {
    /// Serves through `backend`, in this process.
    pub(crate) const fn new(backend: &'a dyn Backend) -> Self {
        Self {
            backend,
            remote: false,
        }
    }

    /// Serves through whatever `ops` serves through — `jkb serve`, in remote mode.
    pub(crate) fn from_ops(ops: &Ops<'a>) -> Self {
        Self {
            backend: ops.backend(),
            remote: ops.is_remote(),
        }
    }

    fn call(&self, request: Request) -> Result<Response> {
        self.backend
            .call(request)
            .map_err(|e| op_error(e, self.remote))
    }

    /// `task.facts`.
    pub(crate) fn facts(&self, uid: &str) -> Result<TaskState> {
        match self.call(Request::TaskFacts {
            uid: uid.to_owned(),
        })? {
            Response::TaskState { state } => Ok(state),
            other => unexpected("task.facts", &other),
        }
    }

    /// `task.by_branch`: every task on each branch.
    pub(crate) fn by_branch(&self, repo: &str) -> Result<BTreeMap<String, Vec<BranchTask>>> {
        match self.call(Request::TaskByBranch {
            repo: repo.to_owned(),
        })? {
            Response::BranchTasks { tasks } => Ok(tasks),
            other => unexpected("task.by_branch", &other),
        }
    }

    fn taken(&self, op: &str, request: Request) -> Result<bool> {
        match self.call(request)? {
            Response::Taken { taken } => Ok(taken),
            other => unexpected(op, &other),
        }
    }

    /// `task.start`.
    pub(crate) fn start(&self, ask: StartAsk) -> Result<bool> {
        self.taken("task.start", Request::TaskStart(ask))
    }

    /// `task.take`.
    pub(crate) fn take(&self, ask: TakeAsk) -> Result<bool> {
        self.taken("task.take", Request::TaskTake(ask))
    }

    /// `task.locate`: record where `owner`'s work is; `false` when `owner` no longer holds the claim.
    pub(crate) fn locate(&self, uid: &str, owner: &str, place: Place) -> Result<bool> {
        self.taken(
            "task.locate",
            Request::TaskLocate {
                uid: uid.to_owned(),
                owner: owner.to_owned(),
                place,
            },
        )
    }

    /// `task.release`: drop `owner`'s claim, and only `owner`'s.
    pub(crate) fn release(&self, uid: &str, owner: &str) -> Result<bool> {
        match self.call(Request::TaskRelease {
            uid: uid.to_owned(),
            owner: owner.to_owned(),
        })? {
            Response::Released { released } => Ok(released),
            other => unexpected("task.release", &other),
        }
    }

    /// `task.abandon`.
    pub(crate) fn abandon(&self, uid: &str, observed: Option<&str>) -> Result<Abandoned> {
        match self.call(Request::TaskAbandon {
            uid: uid.to_owned(),
            observed: observed.map(str::to_owned),
        })? {
            Response::Abandoned { abandoned } => Ok(abandoned),
            other => unexpected("task.abandon", &other),
        }
    }

    /// `repo.gate`.
    pub(crate) fn gate(&self, repo: &str) -> Result<Option<String>> {
        match self.call(Request::RepoGate {
            repo: repo.to_owned(),
        })? {
            Response::Gate { gate } => Ok(gate),
            other => unexpected("repo.gate", &other),
        }
    }

    /// `session.state`.
    pub(crate) fn session_state(&self, session: &str) -> Result<SessionStateIs> {
        match self.call(Request::SessionState {
            session: session.to_owned(),
        })? {
            Response::SessionIs { state } => Ok(state),
            other => unexpected("session.state", &other),
        }
    }
}

/// Where `task start` is being told the work is happening. Grouped so the signature stays under
/// clap's and clippy's argument-count limits.
pub(crate) struct StartWhere {
    pub(crate) branch: Option<String>,
    pub(crate) onto: Option<String>,
    pub(crate) repo: Option<String>,
    pub(crate) owner: Option<String>,
}

/// May this run take a claim somebody else holds, and if not, may it leave it in place?
///
/// **Liveness, not string equality** (D27.1). The bare `claim::claim` CAS accepts only a free
/// task or a byte-identical owner, so using its answer as a refusal meant `task start` refused
/// its own second run under a new pid, and refused after `task work` — the very sequence the
/// facet writing exists for, since a session claims as `session:<pid>[@<claude session>]:<worktree>`.
///
/// Returns whether the existing claim should be **kept**.
///
/// # Errors
/// Errors when a live owner other than this one holds the task.
fn judge_existing_claim(held: Option<&str>, owner: &str, uid: &str, cwd: &Path) -> Result<bool> {
    let Some(prev) = held else { return Ok(false) };
    // Refuse unless the holder is **proven** gone. An owner whose liveness cannot be established
    // (an externally-minted `agent:` id) keeps its claim: taking it away on an unestablished
    // answer is how a live agent's work gets started twice (design S3.2).
    if prev == owner || owner::is_alive(prev).is_no() {
        return Ok(false);
    }
    // A live session for this task that we are standing **inside** keeps its claim: replacing a
    // `session:` owner with this one-second process's `host:pid` would make the task read as
    // dead to `doctor --fix` the moment it exits, freeing a session someone is working in
    // (D36.6). Any other live owner is someone else.
    let inside = owner::session_worktree(prev).is_some_and(|w| session::is_within(cwd, &w));
    anyhow::ensure!(
        inside,
        "{uid} is already claimed by {prev}, which is still alive — nothing was changed. \
         Finish or abandon that work, or use `jkb task release {uid} --owner {prev}` if you \
         are sure it is gone."
    );
    Ok(true)
}

/// `task start` — claim the task and record where the work is happening.
///
/// Claiming and tagging together is the point: "I am starting this" and "here is the branch
/// that will finish it" are the same moment, and splitting them is how the tag ends up
/// missing on exactly the tasks that needed it.
pub(crate) fn start(kb: &Kb<'_>, uid: &str, where_: StartWhere, json: bool) -> Result<()> {
    let StartWhere {
        branch,
        onto,
        repo,
        owner,
    } = where_;
    let cwd = std::env::current_dir()?;
    let branch = match branch {
        Some(b) => b,
        None => gitrepo::current_branch(&cwd)?.context(
            "not on a branch here (detached HEAD?) — pass --branch, or run inside a git repo",
        )?,
    };
    // Through the MAIN copy, never `key(&cwd)`: inside a `jkb task work` session that is the
    // session's own directory, so the key came out as the session name — and now that these
    // facets are *set* rather than added, that replaced the real `repo=` instead of sitting
    // beside it. Every `repo=`-keyed surface (`staging ls`, In Flight, `task sessions`,
    // `batch_onto`, `task review record`) then stopped seeing the task, and `review record`
    // matching nothing is indistinguishable from a review that was never run.
    //
    // Resolved **once**. It was built three times here, each discarding the `trunk` it had just
    // spawned git to find, and two of those sat inside the write transaction — which holds the
    // single writer thread while git runs.
    let here = repo::repo_ctx().ok();
    let repo = match repo {
        Some(r) => r,
        None => here
            .as_ref()
            .context("not inside a git repo — pass --repo, or run this from the repo")?
            .key
            .clone(),
    };
    // Only *this* repo's rules may be applied to the branch, so a `--repo <other>` run skips them
    // rather than judging one repo's branch by another's trunk.
    let ours = here.as_ref().filter(|c| c.key == repo);
    let Land {
        target: land_target,
        dropped_trunk,
    } = land_target_for(ours, &branch, onto.as_deref())?;

    let facts = kb.facts(uid)?;
    let owner = owner.unwrap_or_else(owner::preferred_owner);
    let held = facts.claim.clone();
    let keep_claim = judge_existing_claim(held.as_deref(), &owner, uid, &cwd)?;
    // One op, one transaction: the claim (unless kept), the location facets through the one writer
    // `task work` also uses — additive facets here once left `task work` then `task start` with two
    // `branch=` values for one worktree — and the branch and land target as **labels on a
    // transition**, so a task told a different target later simply has a later entry.
    //
    // The claim is a compare-and-set on the owner judged above. Losing it means someone claimed the
    // task between the probe and the write, and reporting "started" while writing this session's
    // branch onto their task is exactly the confusion the liveness guard prevents.
    let taken = kb.start(StartAsk {
        uid: facts.uid.clone(),
        take: (!keep_claim).then(|| Take {
            owner: owner.clone(),
            displace: held.clone(),
        }),
        // A kept claim is kept only while it is still the one judged.
        keep: if keep_claim { held } else { None },
        place: Place {
            branch: branch.clone(),
            repo: repo.clone(),
            onto: land_target,
        },
    })?;
    anyhow::ensure!(
        taken,
        "the task was claimed by someone else while this command was checking — nothing was \
         changed; run it again"
    );
    report_started(
        kb,
        &Started {
            uid,
            branch: &branch,
            repo: &repo,
            owner: &owner,
            dropped_trunk,
        },
        json,
    )
}

/// What `task start` has just recorded, for reporting it.
struct Started<'a> {
    uid: &'a str,
    branch: &'a str,
    repo: &'a str,
    owner: &'a str,
    /// `--onto` named trunk, so it is deliberately not recorded as a land target.
    dropped_trunk: bool,
}

/// Report a `task start`, on **both** output paths.
///
/// The human note was added first and alone, which left a JSON consumer — the UI, a workflow —
/// with no way to tell that no cut point was recorded and the task will therefore never
/// auto-close. A fact worth saying out loud is worth saying to whoever is actually reading.
fn report_started(kb: &Kb<'_>, s: &Started<'_>, json: bool) -> Result<()> {
    let Started {
        uid,
        branch,
        repo,
        owner,
        dropped_trunk,
    } = *s;
    let onto = kb.facts(uid)?.land_target;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "uid": uid,
                "branch": branch,
                "repo": repo,
                "owner": owner,
                "onto": onto,
                // A dropped land target is a thing that HAPPENED, so it is on both paths. The
                // human note alone left a `--json` consumer unable to tell "trunk was named and
                // dropped" from "no target was ever given".
                "onto_dropped_trunk": dropped_trunk,
            })
        );
        return Ok(());
    }
    println!("started {uid} on {repo}@{branch} (owner {owner})");
    if dropped_trunk {
        println!(
            "  note: {branch} was cut from trunk, so trunk is not recorded as a land target — \
             a task landing on trunk would read as merged the moment anything landed."
        );
    }
    Ok(())
}

/// What `--onto` should be **recorded** as, given what it was passed — and the trunk rule.
///
/// Trunk is an unacceptable land target (D34.3): a task recorded as landing on trunk reads as
/// merged the moment anything lands there, and `jkb staging ls` would offer trunk as a batch. So
/// `--onto <trunk>` is accepted — it is an ordinary thing to say about a branch cut from trunk —
/// and simply not recorded, which the caller is told.
///
/// Refusing the flag outright was the first version, and it left the caller nothing to say about
/// a branch genuinely cut from trunk.
fn land_target_for(ctx: Option<&repo::RepoCtx>, branch: &str, onto: Option<&str>) -> Result<Land> {
    let Some(ctx) = ctx else {
        // Not in the task's repository, so neither the trunk rule nor the existence check can be
        // applied. The caller's word is taken for the record — a land target is a name, not a
        // measurement, so there is nothing here this checkout could honestly establish.
        return Ok(Land {
            target: onto.map(str::to_owned),
            dropped_trunk: false,
        });
    };
    if let Some(trunk_name) = ctx.trunk_name() {
        anyhow::ensure!(
            branch != trunk_name,
            "`{branch}` is this repo's trunk — start work on a feature branch, or the task would \
             auto-close immediately"
        );
    }
    // A land target this repository does not have is refused rather than stored. Storing it took
    // the task out of `jkb staging ls` — the one read behind the picker and In Flight — and made
    // `task land` fail later claiming the branch "no longer exists", which is not what happened.
    // `task work --onto` may name a branch that does not exist yet because it *creates* it; this
    // verb only records, so there is nothing here to make the name true.
    //
    // Asked as "is this a **branch**", not "does this revision resolve". The two come apart on
    // exactly the values that hurt: `origin/<batch>` and a tag both resolve, and both were
    // accepted and stored — the first under a key `staging ls` cannot look up (its map is keyed by
    // bare short name), so the task vanished from the listing the guard exists to keep it in, and
    // `land_preflight`'s `adopt_remote` later cut a junk local branch literally called
    // `origin/<batch>`. What is recorded is the map key, never the caller's spelling.
    //
    // **Before** the trunk comparison below, not after: that compared the caller's spelling
    // against trunk's short name and full ref, which is two spellings of one branch guessed at by
    // hand. Canonicalizing first leaves it one comparison against one name, so `--onto origin/main`
    // is recognised as trunk in a repository whose trunk ref is the bare `main` too.
    let target = match onto {
        None => None,
        Some(onto) => match gitrepo::branch_name(&ctx.root, onto)? {
            gitrepo::BranchName::Is(name) => Some(name),
            gitrepo::BranchName::Unknown => anyhow::bail!(
                "`{onto}` is not a branch in {} — a land target has to exist, or the task drops \
                 out of `jkb staging ls` and `jkb task land` fails on it later. Create it first, \
                 or name the branch {branch} was really cut from.",
                ctx.key
            ),
            gitrepo::BranchName::NotABranch => anyhow::bail!(
                "`{onto}` resolves in {} but is not a branch — a tag, an object id or `HEAD` \
                 cannot be a land target, because every reader looks one up by branch name and \
                 the task would simply disappear from `jkb staging ls`.",
                ctx.key
            ),
        },
    };
    if let Some(trunk_name) = ctx.trunk_name() {
        if target.as_deref() == Some(trunk_name) {
            return Ok(Land {
                target: None,
                dropped_trunk: true,
            });
        }
    }
    Ok(Land {
        target,
        dropped_trunk: false,
    })
}

/// What `--onto` resolved to: the land target to record, and whether trunk was dropped as one.
struct Land {
    target: Option<String>,
    /// Trunk was named, so it is measured against but not recorded. Reported on **both** output
    /// paths — a `--json` consumer could not otherwise tell a dropped target from one never given.
    dropped_trunk: bool,
}

/// `task work` — open (or return) an isolated session for a task (design D36.2).
///
/// Idempotent by construction: the task's own `branch=` tag names its session, so a second
/// invocation hands back the same worktree instead of forking the work onto a second branch.
pub(crate) fn work(
    kb: &Kb<'_>,
    db_path: &Path,
    uid: &str,
    onto: Option<&str>,
    json: bool,
) -> Result<()> {
    let ctx = repo::repo_ctx()?;
    let cwd = std::env::current_dir()?;
    let facts = kb.facts(uid)?;

    // Asked of the lifecycle rather than re-stated here: `start` has no transition out of a
    // terminal status, so this is the same refusal `jkb task start` and the In Flight row give.
    // Only the *state* half is checked now — everything a session needs is established below,
    // and asking about it before the worktree exists would be asking about facts that are not
    // yet true.
    if let Some(why) = &facts.start_refusal {
        if facts.terminal {
            anyhow::bail!("{uid}: {why}");
        }
    }

    // Session worktrees live inside the repo, so the first one must not make it dirty.
    session::ensure_excluded(&ctx.root)?;

    let sessions = session::discover(&ctx.root)?;
    let name = session_name(kb, &ctx, &facts, &sessions, uid, onto)?;
    let branch = session::branch_for(&name);
    let worktree = session::worktree_path(&ctx.root, &name);
    let onto = resolve_onto(kb, &ctx, &cwd, facts.land_target.as_deref(), onto, &name)?;

    // Claim first: if someone else is on this task, stop before making a worktree they
    // would have to clean up.
    //
    // The branch and its land target ride along **on the `start` transition**, because both are
    // already known here and `start` is the entry a reader looks at first — a history whose
    // opening line does not say where the work is sends them to the next line to find out.
    //
    // WHERE the work is (D34.1) is judged by the same write, **before** any git work, so every
    // refusal of it — a place the task's `tasks.md` line could not carry, say — comes while there is
    // nothing to undo; the write only tries it and rolls it back. It is recorded by `task.locate`
    // once the worktree exists, which also notes the branch and land target in the history. The
    // facets are *set*, not added: a second value would be a contradiction, and is how a task ends up
    // with two branches and one worktree. A **resumed** session re-asserts both, which writes nothing
    // new.
    let place = Place {
        branch: branch.clone(),
        repo: ctx.key.clone(),
        onto: Some(onto.clone()),
    };
    let owner = claim_session(kb, &facts, uid, &worktree, place.clone())?;

    // CANCELLED FIRST, and a refusal stops the verb.
    //
    // `revoke` takes the sweep lock, so a refusal means a sweep is in flight working from a
    // snapshot that still lists this worktree — and its checks all pass, because the tree is
    // registered, on the recorded HEAD and clean. Printing a note and handing the session back
    // anyway licensed that sweep to archive the checkout the operator was just told to work in
    // and force-delete its branch. `revoke`'s own doc already said the honest outcome is to
    // refuse and have the operator re-run; this makes the caller obey it.
    //
    // Before `open_worktree`, so a sweep sees either no worktree or no record — never a live
    // checkout it still holds a licence for.
    //
    // A refusal releases this run's claim, as a failed worktree add does: the verb stops, and a
    // claim on a session nobody opened is a claim nothing else would free.
    archive::revoke(db_path, &worktree)
        .inspect_err(|_| {
            let _ = kb.release(&facts.uid, &owner);
        })
        .map(|cancelled| {
            if cancelled {
                println!("cancelled the pending removal of {}", worktree.display());
            }
        })
        .with_context(|| {
            format!(
                "cannot open the session for {uid}: its pending removal could not be cancelled, \
                 and opening it anyway would let that sweep archive the checkout you were about \
                 to work in"
            )
        })?;

    let resumed = sessions.iter().any(|s| s.branch == branch);
    if !resumed {
        open_worktree(kb, &facts.uid, &owner, &ctx.root, &worktree, &branch, &onto)?;
    }
    // Where the work is, now that it is there — only while this run still holds the claim, so a
    // run displaced meanwhile does not overwrite its successor's record. The place was judged by the
    // take, so a refusal here is a change since; the claim is released, as any failure after it.
    match kb.locate(&facts.uid, &owner, place) {
        Ok(true) => {}
        Ok(false) => anyhow::bail!(
            "{uid} was claimed by someone else while its session was being opened — {} is \
             there, but the task now belongs to another run; nothing was recorded",
            worktree.display()
        ),
        // The claim is KEPT: it is what names this checkout until the location is recorded, so a
        // re-run resumes it (see the name choice above) and `abandon` can still find it. Released,
        // the checkout was left with nothing pointing at it.
        Err(e) => {
            return Err(e.context(format!(
                "{} was opened but where it is could not be recorded — run `jkb task work {uid} \
                 --onto {onto}` again to resume it, or `jkb task abandon {uid}`",
                worktree.display()
            )))
        }
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "uid": uid,
                "session": name,
                "worktree": worktree,
                "branch": branch,
                "onto": onto,
                "resumed": resumed,
                "owner": owner,
            })
        );
    } else {
        let verb = if resumed { "resumed" } else { "opened" };
        println!("{verb} session {name} for {uid}");
        println!("  worktree: {}", worktree.display());
        println!("  branch:   {branch} (lands on {onto})");
        println!("  next:     cd {} && claude", worktree.display());
        println!("  finish:   jkb task land {uid}");
    }
    Ok(())
}

/// The session `task work` opens for a task: the one it already has, or a fresh name.
///
/// Split out of [`work`] for length; every rule is as `work` states it.
fn session_name(
    kb: &Kb<'_>,
    ctx: &repo::RepoCtx,
    facts: &TaskState,
    sessions: &[session::Session],
    uid: &str,
    onto: Option<&str>,
) -> Result<String> {
    let tags = &facts.tags;
    // A session already recorded on the task keeps its name, so a second invocation returns
    // the same worktree instead of forking the work onto a second branch. A task may record
    // more than one branch (a `jkb task start` before a `task work`, or an earlier `--onto`),
    // so prefer the one that has a live worktree over merely the first that parses.
    let recorded = repo::facet_values(tags, repo::FACET_BRANCH);
    //
    // Failing that, the session the task's CLAIM names: a run stopped between taking the claim and
    // recording the location (interrupted, or refused by the record) leaves a claim on a checkout
    // and no `branch=` pointing at it, and minting a fresh name then forked the work onto a second
    // session beside a first no verb could find (stage-2 review, round 4).
    //
    // In that order: a live checkout a recorded branch names, then one only the claim names, then a
    // recorded session name with no checkout yet.
    let by_branch = kb.by_branch(&ctx.key)?;
    let live_recorded = recorded
        .iter()
        .find(|b| sessions.iter().any(|s| s.branch == **b))
        .and_then(|b| session::name_from_branch(b))
        .map(str::to_owned);
    let claimed = if live_recorded.is_none() {
        claimed_session(facts, sessions, &by_branch).map(|s| s.name.clone())
    } else {
        None
    };
    let existing = live_recorded
        .clone()
        .or_else(|| claimed.clone())
        .or_else(|| {
            recorded
                .iter()
                .find_map(|b| session::name_from_branch(b))
                .map(str::to_owned)
        });
    // A checkout found only through the claim was never recorded — the run that made it stopped
    // before its locate — so any land target on the task belongs to an earlier checkout, not this one,
    // and guessing could land its branch somewhere it was not cut from. The operator names it.
    anyhow::ensure!(
        claimed.is_none() || onto.is_some(),
        "{uid}'s checkout {} was opened but where it lands was never recorded — run `jkb task work \
         {uid} --onto <branch>` naming the branch it was cut from",
        claimed
            .as_deref()
            .map(|n| session::worktree_path(&ctx.root, n).display().to_string())
            .unwrap_or_default()
    );
    Ok(if let Some(existing) = existing {
        existing
    } else {
        let taken: std::collections::HashSet<String> =
            sessions.iter().map(|s| s.name.clone()).collect();
        session::mint_name(uid, |n| {
            taken.contains(n) || session::worktree_path(&ctx.root, n).exists()
        })
    })
}

/// Make the session's worktree, returning whether its **branch** had to be created.
///
/// Split out of `cmd_task_work` for length.
///
/// **Every** failure path releases the claim. `claim_session` has already written a
/// `session:<pid>[@<claude session>]:<worktree>` owner, and `owner::is_alive` judges one solely by whether that
/// directory exists (D36.6) — so a bail-out caused by the directory being in the way leaves a
/// claim that reads as alive forever, freed by neither `doctor --fix` nor `task reclaim`. Only the
/// `worktree_add` arm used to release, while the doc claimed all of them did.
///
/// The release is a compare-and-set on the owner this run took the claim as: a claim somebody else
/// took in the meantime is not this run's to drop.
fn open_worktree(
    kb: &Kb<'_>,
    uid: &str,
    owner: &str,
    root: &Path,
    worktree: &Path,
    branch: &str,
    onto: &str,
) -> Result<()> {
    let opened = open_worktree_inner(root, worktree, branch, onto);
    if opened.is_err() {
        let _ = kb.release(uid, owner);
    }
    opened
}

/// The fallible half of [`open_worktree`], so one `Err` covers every way it can fail.
fn open_worktree_inner(root: &Path, worktree: &Path, branch: &str, onto: &str) -> Result<()> {
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    anyhow::ensure!(
        !worktree.exists(),
        "{} exists but git does not know it as a worktree — remove it, or run \
         `git worktree prune`",
        worktree.display()
    );
    gitrepo::worktree_add(root, worktree, branch, onto)
}

/// Decide which branch this session's work will land on (design D36.3).
fn resolve_onto(
    kb: &Kb<'_>,
    ctx: &repo::RepoCtx,
    cwd: &Path,
    recorded: Option<&str>,
    flag: Option<&str>,
    session_name: &str,
) -> Result<String> {
    if let Some(flag) = flag {
        // The bare branch name the flag refers to, before anything acts on it. Unlike `task
        // start`, this verb may legitimately be handed a branch that does not exist yet — it
        // creates one — so `Unknown` is accepted and taken literally. What is refused is a name
        // that resolves to something *else*: `git branch <tag> <tag>` would otherwise cut a branch
        // named after a tag, and `--onto origin/<batch>` cut one literally called
        // `origin/<batch>`, which no reader of a land target can ever look up.
        let branch = match gitrepo::branch_name(&ctx.root, flag)? {
            gitrepo::BranchName::Is(name) => name,
            gitrepo::BranchName::Unknown => flag.to_owned(),
            gitrepo::BranchName::NotABranch => anyhow::bail!(
                "`{flag}` resolves in {} but is not a branch — a tag, an object id or `HEAD` \
                 cannot be a land target, because every reader looks one up by branch name and \
                 the session would simply disappear from `jkb staging ls`.",
                ctx.key
            ),
        };
        anyhow::ensure!(
            Some(branch.as_str()) != ctx.trunk_name(),
            "refusing to land on {branch}: it is this repo's trunk, and a task tagged with \
             it would read as merged the moment anything lands"
        );
        // Adopt an existing branch — including one that exists only on `origin/` — rather than
        // replacing it with an empty namesake cut from trunk. The start point is resolved **only**
        // if there is nothing to adopt: computing it eagerly made every `task work` fail in a repo
        // whose trunk cannot be discovered, including the `--onto` escape hatch the error
        // recommends.
        if !gitrepo::adopt_remote(&ctx.root, &branch)? {
            gitrepo::create_branch(&ctx.root, &branch, &batch_start(ctx, cwd)?)?;
        }
        return Ok(branch);
    }
    // A session that already has a target keeps it — counting the remote-tracking copy, or a
    // batch whose local ref was pruned would silently retarget the session somewhere else.
    //
    // Read from the task's own history — the last time anybody said where its work lands. The
    // session branch is minted before this runs, so on a resume the target is whatever the
    // previous run recorded, whichever branch it recorded it against.
    if let Some(branch) = recorded {
        if gitrepo::adopt_remote(&ctx.root, branch)? {
            return Ok(branch.to_owned());
        }
    }
    // Join the batch the other live sessions are landing on.
    if let Some(branch) = batch_onto(kb, ctx)? {
        return Ok(branch);
    }
    // The branch you invoked from — unless that is trunk (landing there closes tasks
    // instantly, D34.3) or another session's branch (that would stack sessions).
    if let Some(branch) = gitrepo::current_branch(cwd)? {
        if Some(branch.as_str()) != ctx.trunk_name() && session::name_from_branch(&branch).is_none()
        {
            return Ok(branch);
        }
    }
    // Cut the batch branch from trunk, named after this task — the first of the batch.
    // `create_branch`, deliberately: this is cutting a NEW batch at a known start point, so a
    // same-named branch left on the remote by an earlier, possibly already-merged batch must not
    // be adopted in its place.
    gitrepo::create_branch(&ctx.root, session_name, &batch_start(ctx, cwd)?)?;
    Ok(session_name.to_owned())
}

/// The commit a new batch branch is cut from.
///
/// The **local** trunk when that is where you are standing, and only otherwise the trunk ref
/// — which is usually `origin/main`. A local trunk ahead of its remote is the ordinary case
/// (you just merged a PR and pulled, or you commit locally first), and cutting the batch from
/// the remote ref there would silently start the work behind commits you already have, then
/// land it as if it were on top of them.
fn batch_start(ctx: &repo::RepoCtx, cwd: &Path) -> Result<String> {
    if let Some(branch) = gitrepo::current_branch(cwd)? {
        if Some(branch.as_str()) == ctx.trunk_name() {
            return Ok(branch);
        }
    }
    ctx.trunk.clone().context(
        "could not determine this repo's trunk, so there is nothing to cut a branch from \
         — pass --onto <branch> naming one that exists",
    )
}

/// The land target the repo's other live sessions share, if they agree on one.
///
/// This is what makes a second session started from trunk join the first one's batch instead
/// of cutting a branch of its own. Sessions that disagree are left alone: guessing which of
/// two batches a new task belongs to is worse than asking for `--onto`.
fn batch_onto(kb: &Kb<'_>, ctx: &repo::RepoCtx) -> Result<Option<String>> {
    let by_branch = kb.by_branch(&ctx.key)?;
    let mut found: Option<String> = None;
    for s in session::discover(&ctx.root)? {
        let Some(onto) = by_branch
            .get(&s.branch)
            .and_then(|ts| task_on(ts))
            .and_then(|t| t.onto.clone())
        else {
            continue;
        };
        // Remote-aware for the same reason: a live batch whose local ref is gone must still be
        // joinable, or the next session cuts a second batch beside it. **Materialised**, not just
        // detected — a bare branch name that only exists under `refs/remotes` is not a valid start
        // point, so merely counting it as live handed `git branch <session> <batch>` a name it
        // could not resolve and aborted the command.
        if !gitrepo::adopt_remote(&ctx.root, &onto)? {
            continue;
        }
        match &found {
            None => found = Some(onto),
            Some(f) if *f == onto => {}
            Some(_) => return Ok(None),
        }
    }
    if found.is_none() {
        // No sessions right now, but a batch checkout may survive from an earlier round —
        // you landed one task and are starting the next. Join it only while that batch is
        // still LIVE: once it has merged (or never had a commit), holding on would both
        // attract new work onto a dead branch and keep `git branch -d` from deleting it. So
        // a merged batch's checkout is released here rather than reused.
        if let Some(branch) = base_branch(ctx)? {
            if !batch_is_spent(&by_branch, &branch) {
                return Ok(Some(branch));
            }
            release_base_worktree(ctx)?;
        }
    }
    Ok(found)
}

/// The branch checked out in `.jkb/base`, if that worktree exists.
fn base_branch(ctx: &repo::RepoCtx) -> Result<Option<String>> {
    let base = session::base_worktree(&ctx.root);
    // "no branch recorded for `.jkb/base`" either way, which is what a caller does with an
    // unregistered base anyway.
    Ok(gitrepo::worktrees(&ctx.root)?
        .unwrap_or_default()
        .into_iter()
        .find(|w| session::same_path(&w.path, &base))
        .and_then(|w| w.branch))
}

/// Whether a batch branch has nothing left to give: every task recorded on it has finished,
/// one way or the other.
///
/// The same rule `staging::collect` uses to decide a batch is spent, so the picker and the
/// checkout cache cannot disagree about which batches are live. It used to ask `merge-tree`
/// whether the branch added anything to trunk, and then had to hand-correct the answer, because
/// a branch that adds nothing is either landed *or* freshly cut and still empty and refs cannot
/// tell those apart.
///
/// A batch nothing records is **not** spent: an unknown batch is more likely one this repo has
/// no tasks for than one that is finished, and losing a live batch is worse than reusing a
/// spent one.
fn batch_is_spent(by_branch: &BTreeMap<String, Vec<BranchTask>>, branch: &str) -> bool {
    let mut any = false;
    for t in by_branch.values().flatten() {
        if t.onto.as_deref() != Some(branch) {
            continue;
        }
        any = true;
        if !jkb_types::TaskStatus::is_terminal_str(Some(t.status.as_str())) {
            return false;
        }
    }
    any
}

/// Remove `.jkb/base`, freeing the branch it holds. It is only ever a checkout cache; `land`
/// makes a new one on demand.
fn release_base_worktree(ctx: &repo::RepoCtx) -> Result<()> {
    let base = session::base_worktree(&ctx.root);
    if base.exists() {
        gitrepo::worktree_remove(&ctx.root, &base, true)?;
    }
    Ok(())
}

/// Take the session's claim, taking over from this session's own previous process (a resume)
/// or from a dead owner, and refusing any other live owner **by name** (design D36.6).
///
/// **A session another Claude Code session is still running in is not taken over** (tasks S6.4,
/// decision E). Taking over the same worktree is otherwise how a session resumes, and it still is —
/// for the session that opened it, for a process with no Claude session of its own, and whenever the
/// opener has ended or is unknown to the registry. What is refused is a second Claude session starting
/// work in a checkout the registry says the first is still working in. Only a registry answer of
/// `live` refuses: unknown holds nothing back, as before.
///
/// Returns the owner the claim was taken as.
fn claim_session(
    kb: &Kb<'_>,
    facts: &TaskState,
    uid: &str,
    worktree: &Path,
    place: Place,
) -> Result<String> {
    let held = facts.claim.clone();
    if let Some(prev) = &held {
        let same_session =
            owner::session_worktree(prev).is_some_and(|w| session::same_path(&w, worktree));
        // Proven gone, or we do not take it. See `start` for why unestablished holds.
        if !same_session && !owner::is_alive(prev).is_no() {
            let where_ = owner::session_worktree(prev).map_or_else(
                || format!("owner {prev}"),
                |w| format!("a session in {}", w.display()),
            );
            anyhow::bail!(
                "{uid} is already being worked by {where_} — finish or abandon that session, \
                 or work a different task"
            );
        }
        if same_session {
            refuse_a_running_opener(kb, uid, prev, worktree)?;
        }
    }
    let opener = opener_for(kb, held.as_deref(), worktree)?;
    let owner = owner::session_owner(worktree, opener.as_deref());
    let ok = kb.take(TakeAsk {
        uid: facts.uid.clone(),
        take: Take {
            owner: owner.clone(),
            displace: held,
        },
        place,
    })?;
    anyhow::ensure!(
        ok,
        "{uid} was claimed by someone else while this command was checking — nothing was \
         changed; run it again"
    );
    Ok(owner)
}

/// Which Claude Code session a session owner records as having opened the work.
///
/// This process's session — unless it is resuming a checkout somebody else opened and is not itself
/// a running session the registry knows (a person at a terminal, a subagent): then the opener it found
/// is kept. Writing its own id there, or none, cleared the only thing that stops a second running
/// session taking over the checkout while its opener still works in it (stage-2 review, round 2).
fn opener_for(kb: &Kb<'_>, held: Option<&str>, worktree: &Path) -> Result<Option<String>> {
    let mine = owner::claude_session();
    let found = held
        .filter(|h| owner::session_worktree(h).is_some_and(|w| session::same_path(&w, worktree)))
        .and_then(|h| AgentId::parse(h).opened_by().map(str::to_owned));
    Ok(match (mine, found) {
        (Some(mine), Some(found)) if mine != found => {
            if kb.session_state(&mine)? == SessionStateIs::Live {
                Some(mine)
            } else {
                Some(found)
            }
        }
        (None, Some(found)) => Some(found),
        (mine, _) => mine,
    })
}

/// `task abandon` — drop a session without landing it (design D36.6).
pub(crate) fn abandon(
    kb: &Kb<'_>,
    db_path: &Path,
    uid: &str,
    force: bool,
    delete_branch: bool,
    json: bool,
) -> Result<()> {
    let ctx = repo::repo_ctx()?;
    let facts = kb.facts(uid)?;
    // Abandon does two separable things: it **disposes of the session**, and it **reopens the
    // task**. Only the second is wrong for a terminal task — a landed one is already merged
    // and a cancelled one was deliberately dropped, so putting either back on the ready
    // frontier (still tagged with its branch, re-dispatchable to the swarm) is the harm.
    //
    // Refusing outright was the first fix, and it stranded the session instead: `task set
    // --status cancelled` leaves the worktree, branch and claim in place, no other verb
    // removes them, and the workaround the refusal suggested — reopen, then abandon — caused
    // exactly the reopening it was guarding against. So the cleanup runs and the status is
    // left alone. The decision is taken inside the transaction below, against the status as
    // it is *then* — not against a snapshot from before a worktree removal that can take
    // long enough for a concurrent land to finish.
    let tags = &facts.tags;
    // Through the one rule (`repo::work_for`), not a third theory of it. This used to prefer the
    // session's branch and otherwise take the *first* recorded value — and `tag::applications`
    // orders by value, so a task carrying a stale `a-old` beside a live `z-live` had this command
    // delete `a-old` and forget its cut point under `--delete-branch`, while `jkb staging ls` and
    // `jkb task land` both named `z-live` as the branch the row was about. Abandon acted on a
    // branch the user was never shown, and reported it as though it were the one they clicked.
    let repo::Work {
        session: sess,
        branch,
    } = repo::work_for(&ctx, tags)?;
    // A session only the claim names — its location was never recorded — is still this task's.
    // The same rule `work` resumes by: with no checkout a recorded branch names, the one the claim
    // names — whatever branches are recorded, since a `task start` before an interrupted `task work`
    // leaves one that is not the session's (stage-2 review, round 5).
    let (sess, branch) = if let Some(s) = sess {
        (Some(s), branch)
    } else {
        let sessions = session::discover(&ctx.root)?;
        let by_branch = kb.by_branch(&ctx.key)?;
        match claimed_session(&facts, &sessions, &by_branch).cloned() {
            Some(found) => {
                let b = found.branch.clone();
                (Some(found), Some(b))
            }
            None => (None, branch),
        }
    };
    let branch = branch.with_context(|| format!("{uid} has no session"))?;

    // Abandoning is for **this** session's work. Since the swarm now records `branch=` and its
    // branch's land target so its tasks appear in the same views (D38), a task another IMPLEMENTER is
    // actively building is one right-click away — and `claim::clear` has no owner CAS, so it
    // would free a live claim and let the next SCHEDULER pass dispatch a second builder while
    // the first keeps going. Refuse a claim this session does not hold unless forced.
    let held = facts.claim.clone();
    if let Some(owner) = &held {
        let mine = sess.as_ref().is_some_and(|s| {
            owner::session_worktree(owner).is_some_and(|w| session::same_path(&w, &s.worktree))
        });
        // **Liveness**, the same rule `task work` and `task start` follow (D27.1/D36.6) —
        // not owner-string identity. Judging by name refused a claim left behind by a crashed
        // implementer or a session whose worktree was deleted by hand, so the one command that
        // exists to clean a session up was blocked by the wreckage it was there to remove, and
        // pointed the user at `jkb task release` for an owner that provably no longer exists.
        // What must be protected is work someone is *still doing*.
        if !mine && !owner::is_alive(owner).is_no() && !force {
            anyhow::bail!(
                "{uid} is claimed by {owner}, which is still alive — abandoning it would free \
                 a claim someone is working under (or is an owner whose liveness cannot be \
                 checked from here). Finish or abandon that work, use `jkb task release {uid} \
                 --owner {owner}` if you are sure it is gone, or pass --force."
            );
        }
    }

    let deferred = abandon_session(
        db_path,
        &ctx,
        sess.as_ref(),
        &branch,
        uid,
        delete_branch,
        force,
    )?;
    // Deleting the branch leaves the history alone, and that is correct: an entry says what
    // happened at a moment, and a branch deleted afterwards does not make it untrue. Nothing is
    // keyed by the name, so the next `jkb task work` cutting a **new** branch under the same
    // name cannot inherit anything from the old one — which is what the cut point this used to
    // have to forget was for.
    // NOT while the worktree still holds it. When the disposal deferred, the checkout is still
    // there with this branch checked out, and `git branch -D` refuses outright — which `?` turned
    // into a bail BEFORE the claim was released, leaving the task `in_progress`, claimed by a
    // session whose worktree still exists (so `is_alive` says Yes) and therefore reclaimable by
    // nothing. Re-running failed identically. The decision is already in the record's `Plan`, and
    // `delete_branch_if_any` applies it once the tree is out of the way.
    // `is_yes()`: delete only a branch proven to be there. An unestablished answer leaves it
    // alone, which costs a `git branch -D` and never an unrecoverable one.
    if delete_branch && deferred.is_none() && gitrepo::has_branch(&ctx.root, &branch)?.is_yes() {
        gitrepo::delete_branch(&ctx.root, &branch, true)?;
    }
    // Read from git rather than from what this function did: `dispose` deletes the branch itself
    // when it archived the tree, so a flag set here would report `false` for a branch that is
    // gone. Asked once, after everything that could have removed it.
    // Read from git rather than from what this function did — `dispose` deletes the branch itself
    // when it archived the tree — and folded into ONE value, so no two lines can disagree.
    // Three-valued, and an unestablished answer is reported as still there rather than as gone:
    // saying a branch was deleted when git could not say is what stops somebody looking for it.
    // `deferred.is_some()` is passed because "the reaper will delete it" is a claim about the
    // RECORD, not about the branch: with the tree archived (or already gone) there is no record,
    // so nothing will ever apply the plan and the operator has to do it themselves.
    let still_there = gitrepo::has_branch(&ctx.root, &branch)?;
    let branch_fate = branch_fate(
        delete_branch,
        still_there,
        deferred
            .as_ref()
            .is_some_and(archive::Deferral::will_be_swept),
    );

    // Release, and reopen unless the task is already finished (`task.abandon`).
    //
    // The land target is cleared **only** when the task is reopened, which is exactly when it stops
    // being true: an abandoned task is no longer landing on that batch. For a task that stays `done`
    // or `cancelled` it is history — which batch the work went to, or was dropped from.
    //
    // The claim released is only the one judged above, and nothing at all when there was none: it
    // was read before two git subprocesses, so a claim taken in the meantime belongs to a worker this
    // command never looked at, and the op changes nothing. The status is re-read in that transaction,
    // so a task that finished while this command was removing a worktree is not reopened by a
    // decision taken before that, and what is reported is what the transaction did. The lifecycle
    // decides the rest: a terminal task has no `abandon` transition. The checkout's cleanliness is
    // stated by this caller, which refused one it could not prove clean unless `--force` — the
    // operator supplying the fact.
    let Abandoned {
        released,
        reopened,
        status: final_status,
    } = kb.abandon(&facts.uid, held.as_deref())?;

    report_abandon(
        uid,
        &branch,
        &AbandonOutcome {
            released,
            reopened,
            final_status: &final_status,
            worktree_removed: sess.is_some() && deferred.is_none(),
            deferred: deferred.as_ref(),
            branch: branch_fate,
        },
        json,
    );
    Ok(())
}

/// Dispose of an abandoned session, if there is one, and say why not when it could not be moved.
///
/// Split out so `cmd_task_abandon` stays readable; the decision itself is the caller's and is
/// recorded in the [`archive::Plan`], which is what the sweep applies later.
fn abandon_session(
    db_path: &Path,
    ctx: &repo::RepoCtx,
    sess: Option<&session::Session>,
    branch: &str,
    uid: &str,
    delete_branch: bool,
    force: bool,
) -> Result<Option<archive::Deferral>> {
    let Some(sess) = sess else { return Ok(None) };
    // ALREADY GONE is its own outcome, not a deferral. Without this arm `dispose` reported that
    // the tree "could not be archived from in here" — true, but because there was nothing to
    // archive — and handed `--delete-branch` to a sweep with nothing to do, so the branch was
    // never deleted here and never deleted there. `land` has had this arm since D36.4.
    // PROVEN gone, not merely un-stat-able. `Path::exists()` reports `false` for any stat error,
    // so an untraversable `.jkb/work` took this shortcut for a session that was still there —
    // skipping `dispose`, so no record was written, and returning to a caller that then deletes
    // the branch on `--delete-branch`. An abandoned branch holds the only copy of its commits (as
    // the comment below says), so that pair is: commits deleted, checkout orphaned, nothing
    // tracking it. `Unknown` falls through instead, where the dirty check refuses with something
    // the operator can act on, and `--force` reaches `dispose`, which records rather than bails.
    if presence::present_under(&sess.worktree, &ctx.root)
        .fact()
        .is_no()
    {
        let _ = gitrepo::prune_worktrees(&ctx.root);
        return Ok(None);
    }
    {
        if !force {
            // `--force` is the answer to both a dirty tree and an unreadable one: it records
            // that the operator accepts whatever is in there (`Plan::accept_dirty`), which is
            // the same decision either way.
            anyhow::ensure!(
                gitrepo::is_dirty(&sess.worktree, &ctx.root)?.is_no(),
                "{} could not be established as having no uncommitted changes — commit them, \
                 or pass --force to discard whatever is there",
                sess.worktree.display()
            );
        }
        // ARCHIVED, not removed — the same rule `land` follows, through the same function.
        // `git worktree remove` unlinks the tree and stops at the first refusal, so run from
        // inside a sandboxed session this verb gutted the checkout it was asked to drop. It is
        // also the verb an operator naturally reaches for to clear the directory a deferred
        // landing leaves behind, which made it the likeliest route to that state.
        //
        // The branch is deleted only below, and only when asked: an abandoned branch holds the
        // only copy of real work, which is why `--force` here discards a dirty tree but never
        // the commits.
        if let archive::Disposed::Deferred(d) = archive::dispose(
            db_path,
            &ctx.root,
            &sess.worktree,
            branch,
            uid,
            archive::Plan {
                // THE OPERATOR'S decision, recorded so the sweep applies it rather than
                // land's. An abandoned branch holds the only copy of real work, and the
                // reaper used to force-delete it a quarter of an hour after this verb had
                // printed "branch kept".
                delete_branch,
                // `--force` means exactly "I accept what is uncommitted in there".
                // Unrecorded, the sweep's own dirty check held the record for ever over the
                // question the operator had already answered.
                accept_dirty: force,
            },
        )? {
            return Ok(Some(d));
        }
    }
    Ok(None)
}

/// What `abandon` actually did, so the report is built from outcomes rather than from the flags
/// that were asked for.
struct AbandonOutcome<'a> {
    /// The claim this command judged was released. `false` when someone else took the task while it
    /// ran — their claim is left alone, and the task is not reopened under them.
    released: bool,
    reopened: bool,
    final_status: &'a str,
    /// The checkout is no longer where it was — archived, or there was none.
    worktree_removed: bool,
    /// Why the checkout could not be archived from here, and what will become of the record —
    /// so the sentence about `jkb task reap` is derived rather than assumed.
    deferred: Option<&'a archive::Deferral>,
    /// What became of the branch. One value rather than "was it asked for" beside "did it
    /// happen", because the report printed both "branch kept — delete it with `git branch -D`"
    /// and the deferred-deletion sentence from those two, which contradict — and the first's
    /// remedy cannot work anyway while the deferred worktree still holds the branch.
    branch: BranchFate,
}

fn report_abandon(uid: &str, branch: &str, out: &AbandonOutcome<'_>, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "uid": uid, "abandoned": true, "branch": branch, "reopened": out.reopened,
                "claim_released": out.released,
                "status": out.final_status,
                // What happened, not what was asked for: `--delete-branch` on a branch that was
                // already gone deletes nothing.
                "worktree_removed": out.worktree_removed,
                "branch_deleted": out.branch == BranchFate::Deleted,
                "branch_owed_to_reaper": out.branch == BranchFate::OwedToTheReaper,
                "worktree_deferred": out.deferred.map(|d| d.why.clone()),
                "worktree_deferred_sweepable": out.deferred.map(archive::Deferral::will_be_swept),
            })
        );
    } else {
        if out.reopened {
            println!("abandoned {uid}; it is open again");
        } else if !out.released {
            println!(
                "abandoned the session for {uid}, but it was claimed by another worker meanwhile — \
                 their claim is kept, and it stays {}",
                out.final_status
            );
        } else {
            println!(
                "abandoned the session for {uid}; it stays {}",
                out.final_status
            );
        }
        // DERIVED from the verdict on the record, not from the fact that one was written. See
        // `report_landing` for the same correction: a record the sweep cannot act on was being
        // reported as work in hand.
        if let Some(d) = out.deferred {
            println!(
                "  the checkout could not be archived from in here ({}), so {}",
                d.why,
                d.outlook()
            );
        }
        // ONE line about the branch, from one value.
        match out.branch {
            BranchFate::Deleted | BranchFate::Absent => {}
            BranchFate::OwedToTheReaper => println!(
                "  branch {branch} will be deleted when `jkb task reap` archives the checkout"
            ),
            BranchFate::Kept => {
                println!("  branch {branch} kept — delete it with `git branch -D {branch}`");
            }
        }
    }
}

/// `task sessions` — what is in flight in this repo.
pub(crate) fn sessions(kb: &Kb<'_>, db_path: &Path, json: bool) -> Result<()> {
    let ctx = repo::repo_ctx()?;
    let sessions = session::discover(&ctx.root)?;
    let by_branch = kb.by_branch(&ctx.key)?;
    // Which checkouts are finished and merely waiting to be moved. In the container EVERY landing
    // produces one — a session cannot archive its own worktree — so without this the listing an
    // operator reads to find live work is mostly finished work, in rows identical to it.
    // FROM THE VERDICT, not from the existence of a record. `[awaiting archive]` means "work
    // already done that nothing has moved yet"; a record the sweep is going to HOLD means the
    // opposite, and rendering the two the same is the hand-written "a sweep will finish this"
    // claim the verdict seam exists to stop. `pending_outlook` also applies supersession, so a
    // withdrawn record does not put a badge on a live session.
    //
    // ONE read, partitioned — not two filtered reads of the same thing. Each `pending_outlook`
    // re-observes every record, which means a `git worktree list` and a `git status` per pending
    // checkout; asked twice they can disagree, and a worktree that changed underneath between the
    // two calls would land in BOTH sets or in NEITHER. Two answers to one question, which is the
    // shape `pending_outlook` was just introduced to remove from these two surfaces.
    //
    // Held records get their own set rather than being folded into the one above, so the listing
    // can say a checkout needs attention rather than silently omitting it — an absent badge reads
    // as "nothing outstanding".
    let (stuck, awaiting): (std::collections::BTreeSet<PathBuf>, _) =
        archive::pending_outlook(db_path)
            .map(|rows| {
                let (held, moving): (Vec<_>, Vec<_>) = rows
                    .into_iter()
                    .partition(|(_, v)| matches!(v, archive::Verdict::Hold(_)));
                let paths = |rs: Vec<(archive::Entry, archive::Verdict)>| {
                    rs.into_iter().map(|(e, _)| e.worktree).collect()
                };
                (paths(held), paths(moving))
            })
            .unwrap_or_default();

    let mut rows = Vec::new();
    for s in &sessions {
        let task = by_branch.get(&s.branch).and_then(|ts| task_on(ts));
        let onto = task.and_then(|t| t.onto.clone());
        // Resolved first: a recorded land target whose branch has since been deleted cannot be
        // counted against, and `ahead_count` refuses an operand it cannot resolve rather than
        // answering zero.
        let onto_ref = match &onto {
            Some(o) => gitrepo::branch_ref(&ctx.root, o, gitrepo::Prefer::Local)?,
            None => None,
        };
        // `None`, never `0`: a recorded land target that no longer exists makes the count
        // unknowable, and zero already means "nothing to land" to every other reader. This is the
        // fourth call site of `ahead_count` and the last one still folding the two together.
        let ahead = match &onto_ref {
            Some(reference) => Some(gitrepo::ahead_count(&ctx.root, reference, &s.branch)?),
            None => None,
        };
        // Deliberately no "attended" flag: nothing here can observe whether anyone is sitting
        // in a session. The owner's pid belongs to the one-second `jkb task work` process, so
        // a flag built on it reads "unattended" for the session you are working in and tells
        // you to abandon it. What IS observable — uncommitted work, commits ahead — is
        // reported instead (design D36.6).
        rows.push(serde_json::json!({
            "session": s.name,
            "worktree": s.worktree,
            "branch": s.branch,
            "onto": onto,
            "uid": task.map(|t| t.uid.clone()),
            "status": task.map(|t| t.status.clone()),
            // Three-valued, like every other reader of this: `"unknown"` is a checkout git
            // could not read, which `jkb task land` refuses and which a listing must not
            // render as the clean, landable `false`.
            "dirty": gitrepo::is_dirty(&s.worktree, &ctx.root)?.as_str(),
            "commits": ahead,
            "awaiting_archive": awaiting.iter().any(|w| session::same_path(w, &s.worktree)),
            // Distinct from the above rather than folded into it: "nothing will move this until
            // you act" is a different fact from "something will", and a consumer that saw only
            // one flag could not tell either from "there is no record at all".
            "archive_blocked": stuck.iter().any(|w| session::same_path(w, &s.worktree)),
        }));
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        println!("(no sessions in {})", ctx.key);
    } else {
        for r in &rows {
            // Three-valued, like the field it reads. `as_bool` here would have quietly dropped
            // the badge the moment `dirty` became a string, and an unreadable checkout must not
            // render as the clean, landable case — it is the one `jkb task land` refuses.
            let dirty = match r["dirty"].as_str() {
                Some("yes") => " [uncommitted]",
                Some("unknown") => " [unreadable]",
                _ => "",
            };
            // Said in the row, because it is the difference between work to pick up and work
            // already done that nothing has moved yet.
            let awaiting = if r["archive_blocked"].as_bool().unwrap_or(false) {
                " [archive blocked — see `jkb doctor`]"
            } else if r["awaiting_archive"].as_bool().unwrap_or(false) {
                " [awaiting archive]"
            } else {
                ""
            };
            // Three states, not two: a count, a target that has been deleted, and a session that
            // never recorded one. Folding the last into the second sent people looking for a
            // branch nobody had removed.
            let commits = match (r["commits"].as_u64(), r["onto"].as_str()) {
                (Some(n), _) => format!("{n} commit(s)"),
                (None, Some(onto)) => format!("commits unknown ({onto} no longer exists)"),
                (None, None) => "commits unknown (no land target recorded)".to_owned(),
            };
            println!(
                "{:<28} {} → {}  {commits}{dirty}{awaiting}",
                r["session"].as_str().unwrap_or("?"),
                r["branch"].as_str().unwrap_or("?"),
                r["onto"].as_str().unwrap_or("?"),
            );
            if let Some(uid) = r["uid"].as_str() {
                println!("  {uid} ({})", r["status"].as_str().unwrap_or("?"));
            }
        }
    }
    Ok(())
}

/// `task gate` — show the command that verifies a landing here (D36.5). Setting and clearing it are
/// host commands (decision A), done by the caller before this reports.
pub(crate) fn gate(kb: &Kb<'_>, json: bool) -> Result<()> {
    let ctx = repo::repo_ctx()?;
    let stored = kb.gate(&ctx.key)?;
    let detected = if stored.is_none() {
        session::autodetect_gate(&ctx.root)
    } else {
        None
    };
    if json {
        println!(
            "{}",
            serde_json::json!({"repo": ctx.key, "gate": stored, "would_detect": detected})
        );
    } else {
        match (&stored, &detected) {
            (Some(g), _) => println!("gate for {}: {g}", ctx.key),
            (None, Some(d)) => println!("gate for {}: (none stored; would use {d})", ctx.key),
            (None, None) => println!(
                "gate for {}: (none — landings here are UNVERIFIED; set one with \
                 `jkb task gate '<cmd>'`)",
                ctx.key
            ),
        }
    }
    Ok(())
}

/// The discovered session the task's claim names, if its claim is a session owner whose checkout is
/// one of `sessions` — how a session whose location was never recorded is still found.
///
/// **Not if another task records that session's branch.** Session names are minted from a task's
/// slug, so two tasks can mint the same name; a checkout sitting at the claimed path that another task
/// has since recorded is that task's, and resuming or abandoning it from here would take over, or
/// archive, somebody else's work (stage-2 review, round 5).
fn claimed_session<'s>(
    facts: &TaskState,
    sessions: &'s [session::Session],
    by_branch: &BTreeMap<String, Vec<BranchTask>>,
) -> Option<&'s session::Session> {
    let worktree = owner::session_worktree(facts.claim.as_deref()?)?;
    sessions
        .iter()
        .find(|s| session::same_path(&s.worktree, &worktree))
        .filter(|s| {
            by_branch
                .get(&s.branch)
                .is_none_or(|ts| ts.iter().all(|t| t.uid == facts.uid))
        })
}

/// The task a session on a branch is for, when several tasks record that branch: an unfinished one if
/// there is one, since that is the work the checkout is being used for, else the first.
pub(crate) fn task_on(tasks: &[BranchTask]) -> Option<&BranchTask> {
    tasks
        .iter()
        .find(|t| !jkb_types::TaskStatus::is_terminal_str(Some(t.status.as_str())))
        .or_else(|| tasks.first())
}

/// Refuse to take over a session worktree another, still-running Claude Code session opened (decision
/// E): **two top-level sessions in one checkout**, each overwriting the other's work.
///
/// Refused only when both are established: the opener is `live` in the registry, and so is the
/// session asking. A process with no session of its own (a person at a terminal) and a session the
/// registry does not know are let through, as before. That second case includes a subagent: it has an
/// id of its own (measured: `CLAUDE_CODE_SESSION_ID` differs from its parent's, and
/// `CLAUDE_CODE_CHILD_SESSION` is set in top-level sessions' shells too, so it tells nothing apart), and
/// refusing one would refuse its own parent's work. Whether a subagent's start ever reaches the
/// registry is not measured; if it does, a subagent resuming its parent's checkout is refused.
fn refuse_a_running_opener(kb: &Kb<'_>, uid: &str, held: &str, worktree: &Path) -> Result<()> {
    let prev = AgentId::parse(held);
    let Some(opener) = prev.opened_by() else {
        return Ok(());
    };
    let Some(mine) = owner::claude_session() else {
        return Ok(());
    };
    if mine == opener
        || kb.session_state(opener)? != SessionStateIs::Live
        || kb.session_state(&mine)? != SessionStateIs::Live
    {
        return Ok(());
    }
    anyhow::bail!(
        "{uid}'s session in {} was opened by Claude Code session {opener}, which is still running — \
         two sessions in one checkout overwrite each other's work. Continue in that session, wait \
         for it to end, or if you are sure it is gone, `jkb task release {uid} --owner {held}`.",
        worktree.display()
    )
}

#[cfg(test)]
mod tests {
    use super::{batch_is_spent, open_worktree, task_on, Kb};
    use jkb_api::sessions::BranchTask;
    use std::collections::BTreeMap;

    fn task(uid: &str, status: &str, onto: &str) -> BranchTask {
        BranchTask {
            uid: uid.into(),
            status: status.into(),
            onto: Some(onto.into()),
        }
    }

    /// A batch is spent only when every task on every branch landing on it has finished — not when the
    /// first task on a branch has; and a session is for its unfinished task.
    #[test]
    fn a_batch_with_one_open_task_among_finished_ones_is_live() {
        let mut by_branch = BTreeMap::new();
        by_branch.insert(
            "task/w".to_owned(),
            vec![
                task("task:a", "done", "batch"),
                task("task:b", "in_progress", "batch"),
            ],
        );
        assert!(!batch_is_spent(&by_branch, "batch"));
        assert_eq!(
            task_on(&by_branch["task/w"]).map(|t| t.uid.as_str()),
            Some("task:b")
        );
        by_branch.get_mut("task/w").unwrap()[1].status = "done".into();
        assert!(batch_is_spent(&by_branch, "batch"));
        assert_eq!(
            task_on(&by_branch["task/w"]).map(|t| t.uid.as_str()),
            Some("task:a")
        );
        assert!(
            !batch_is_spent(&by_branch, "other"),
            "a batch nothing records is not spent"
        );
        assert!(task_on(&[]).is_none());
    }
    use jkb_api::{Backend, LocalBackend, Request, Response};
    use jkb_core::Db;

    fn claim_of(b: &LocalBackend, uid: &str) -> Option<String> {
        match b
            .call(Request::TaskFacts {
                uid: uid.to_owned(),
            })
            .unwrap()
        {
            Response::TaskState { state } => state.claim,
            other => panic!("{other:?}"),
        }
    }

    /// **A worktree that cannot be made releases this run's claim, and only this run's.** A claim
    /// somebody else took in the meantime is theirs; the old compensation cleared whatever claim the
    /// task had.
    #[test]
    fn a_failed_worktree_releases_only_the_run_s_own_claim() {
        let b = LocalBackend::new(Db::open_in_memory().unwrap());
        let Response::Added { added } = b
            .call(
                serde_json::from_value(serde_json::json!({
                    "op": "task.add", "text": "t +tasks/x", "managed": true
                }))
                .unwrap(),
            )
            .unwrap()
        else {
            panic!("added")
        };
        let uid = added.uid;
        let tmp = tempfile::tempdir().unwrap();
        // Something is already where the worktree goes, so the add fails before git runs.
        let worktree = tmp.path().join(".jkb/work/s");
        std::fs::create_dir_all(&worktree).unwrap();
        let kb = Kb::new(&b);
        let claim = |owner: &str| {
            b.call(Request::TaskClaim {
                uid: uid.clone(),
                owner: owner.to_owned(),
            })
            .unwrap();
        };

        claim("host:someone-else");
        assert!(
            open_worktree(&kb, &uid, "session:1:~/w", tmp.path(), &worktree, "b", "o").is_err()
        );
        assert_eq!(
            claim_of(&b, &uid).as_deref(),
            Some("host:someone-else"),
            "another owner's claim survives"
        );

        b.call(Request::TaskRelease {
            uid: uid.clone(),
            owner: "host:someone-else".to_owned(),
        })
        .unwrap();
        claim("session:1:~/w");
        assert!(
            open_worktree(&kb, &uid, "session:1:~/w", tmp.path(), &worktree, "b", "o").is_err()
        );
        assert_eq!(claim_of(&b, &uid), None, "the run's own claim is released");
    }
}
