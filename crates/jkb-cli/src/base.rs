//! The commit a branch was cut from — the **one** module that knows how it is stored.
//!
//! ## Why this module exists
//!
//! "Branch X was cut from commit Y" is a fact about a *branch*, but the store keys tags by
//! `(item, facet)`, so the branch has to be encoded into the value: `base=<branch>:<sha>`. That
//! encoding leaked to roughly a dozen call sites, and four consecutive review passes each found a
//! different one holding a different theory of it:
//!
//! - `jkb task work` asked `resumed` — whether a *worktree* existed — and so overwrote a real cut
//!   point with the land target's current tip whenever a branch was re-worked after `abandon`.
//! - `jkb task start` treated a pre-qualification bare value as "not recorded", wrote today's
//!   trunk tip over it, and lost the actual cut point.
//! - `jkb task tag set base=…` went through the generic single-value setter, which clears the
//!   facet's other values — so recording one branch's base deleted every sibling's.
//!
//! None of those were reader bugs: given a correctly recorded base, both readers
//! (`task close-merged` and `task review record`) have always agreed. They were three writers
//! each answering *"is a base already recorded for this branch?"* from a different proxy.
//!
//! So that question has exactly one implementation here ([`ensure_recorded`]), the reader's
//! question has exactly one ([`resolve`]), they sit next to each other so the deliberate
//! asymmetry between them is visible in one screen, and [`FACET`] is **private**: no other module
//! can spell the facet, format a `<branch>:<sha>` value, or take one apart.
//!
//! There are three more things anyone can want of a cut point, and each has one implementation
//! here too: **measure** it ([`ensure_recorded`], which is why callers pass no value —
//! `/task-swarm` computed its own three times and got a different wrong answer each time),
//! **replace** one by hand ([`write`], reached as `jkb task base`), and **drop** one because the
//! branch it describes is gone ([`forget`]).
//!
//! ## Why a git ref was rejected
//!
//! `refs/jkb/base/<branch>` would make per-branch keying structural and put the fact where
//! branches live. It was rejected deliberately: jkb runs inside other people's professional
//! repositories, and decorating them with refs the user never asked for is a side effect the tool
//! has no licence to take. Coordination stays in the store; git is used for branches and commits.

use std::collections::BTreeMap;

use jkb_core::{tag, WriteMeta};
use jkb_types::ItemId;
use rusqlite::Connection;

use crate::repo::{facet_values, FACET_BRANCH};

/// The facet the cut point is stored under. **Private on purpose** — see the module docs.
const FACET: &str = "base";

/// The verb that writes a cut point, named in every refusal so the remedy is always reachable.
pub(crate) const VERB: &str = "jkb task base <uid> <branch> <sha>";

/// Whether `facet` is this module's, and therefore off-limits to the generic tag commands.
///
/// `jkb task tag set base=…` is not merely discouraged: [`crate::repo::set_facet`] clears the
/// facet's other values, and for this facet those are *other branches' cut points*. The remedy
/// string in an earlier error message named that exact command, and `/task-swarm` was changed to
/// run it — so the tool was instructing its users to destroy the records it then refused to act
/// without.
pub(crate) fn is_reserved_facet(facet: &str) -> bool {
    facet == FACET
}

/// Whether `value` is a full object id — the only form a cut point may be **stored** in.
///
/// A cut point has to mean the same commit in every clone, and only a full object id does.
/// Symbolic revisions resolve *everywhere* and to something different in each repository: storing
/// the literal `HEAD` and re-resolving it later yields whatever that repo is pointed at now, which
/// is precisely the unrelated commit that makes `is_merged` skip its freshly-cut guard and close a
/// task whose work never landed. A *foreign* full sha is harmless by comparison — it simply does
/// not resolve in the task's repo, so the readers decline to act. The dangerous values are the
/// ones that always resolve.
///
/// 40 hex digits for sha-1, 64 for a sha-256 repository.
pub(crate) fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// The cut point recorded for `branch`, as a **reader** should resolve it.
///
/// Two forms are accepted:
///
/// 1. `<branch>:<sha>` — qualified, the only form written since. Matched exactly, so a base is
///    never lent to a branch it was not cut for. Git forbids `:` in a ref name, so splitting on
///    the first one is unambiguous.
/// 2. A bare `<sha>` — written before bases were qualified. Honoured **only** when the task
///    records no branch other than this one ([`is_the_only_branch`]), which is the case it was
///    written for and the only case it can be attributed to. With several branches nothing says
///    which one it describes, and guessing is what closes a task whose work is still in flight.
///
/// `None` means "no cut point is recorded for this branch" — nothing more. The decision of what
/// to *do* with that lives in [`crate::repo::landed_with_base`], which refuses to act, so this
/// function never has to be read as an opinion.
pub(crate) fn resolve<'a>(
    tags: &'a BTreeMap<String, Vec<String>>,
    branch: &str,
) -> Option<&'a str> {
    if let Some(sha) = qualified(tags, branch) {
        return Some(sha);
    }
    if is_the_only_branch(tags, branch) {
        return facet_values(tags, FACET)
            .iter()
            .map(String::as_str)
            .find(|v| !v.contains(':'));
    }
    None
}

/// The cut point recorded for `branch` in its **qualified** form, ignoring legacy values.
///
/// The writer's "is one already recorded for this branch?" and the reader's first arm are the same
/// question, so they are the same function.
fn qualified<'a>(tags: &'a BTreeMap<String, Vec<String>>, branch: &str) -> Option<&'a str> {
    let prefix = format!("{branch}:");
    facet_values(tags, FACET)
        .iter()
        .find_map(|v| v.strip_prefix(&prefix))
}

/// Whether `branch` is the only branch this task names — the sole condition under which an
/// unqualified legacy value can be attributed to it.
///
/// Phrased as "no *other* branch" rather than "at most one branch recorded", which is what the
/// reader used while the writer already asked it this way. They come apart when the branch being
/// asked about is not one the task records at all, and there the count answered yes and lent the
/// value out.
fn is_the_only_branch(tags: &BTreeMap<String, Vec<String>>, branch: &str) -> bool {
    !facet_values(tags, FACET_BRANCH)
        .iter()
        .any(|b| b.as_str() != branch)
}

/// Why no cut point could be recorded — carried out to the caller so the surface that *reports* it
/// states the reason the writer actually had, rather than re-deriving one from a proxy.
///
/// Every variant means "nothing was recorded", which both readers treat as *do not act*. That is
/// deliberately distinct from recording a plausible-looking value: a task with no cut point is
/// listed by `close-merged` as undecidable, names a remedy, and is repaired by the next run that
/// can measure. A task holding a wrong one is silent and permanent, because [`ensure_recorded`]
/// never overwrites. Recording a guess is the worse of the two, always.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Missing {
    /// Not standing in the task's own repository, so nothing here could honestly measure it.
    /// Carries the repository it wanted, so no caller can name the wrong one in the remedy — one
    /// of the two did, telling the user to re-run from the checkout that had just refused them.
    NotThisRepo(String),
    /// The branch does not exist in this repository yet.
    NoSuchBranch,
    /// The branch already has commits of its own and the caller named no parent, so there is
    /// nothing to measure the fork point against.
    NoParentNamed,
    /// The caller named a parent this repository does not have.
    ParentNotFound,
    /// The branch and everything it could be measured against share no history.
    NoCommonHistory,
    /// The value is not this branch's fork point: it equals the tip of a branch that has done
    /// work, or differs from the tip of one that has not. See [`rejected`].
    NotTheForkPoint,
}

impl Missing {
    /// The stable, machine-readable form, for `--json` consumers.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::NotThisRepo(_) => "not-this-repos-checkout",
            Self::NoSuchBranch => "branch-does-not-exist-here",
            Self::NoParentNamed => "no-parent-named-and-branch-has-commits",
            Self::ParentNotFound => "named-parent-does-not-exist-here",
            Self::NoCommonHistory => "no-shared-history-to-measure-against",
            Self::NotTheForkPoint => "not-this-branchs-fork-point",
        }
    }

    /// What the reader should do about it. Never "pass a sha by hand": the sha nearest to hand is
    /// the branch tip, and a cut point equal to the tip reads as "nothing has happened here"
    /// forever, which is how a task becomes permanently unlandable.
    pub(crate) fn remedy(&self, uid: &str, branch: &str, _repo: &str) -> String {
        match self {
            Self::NotThisRepo(wanted) => {
                format!("run it again from {wanted}, the repository {branch} lives in")
            }
            Self::NoSuchBranch => {
                format!("run it again once {branch} exists")
            }
            Self::NoParentNamed
            | Self::ParentNotFound
            | Self::NoCommonHistory
            | Self::NotTheForkPoint => format!(
                "name the branch {branch} was cut from: \
                 `jkb task start {uid} --branch {branch} --onto <parent>`"
            ),
        }
    }
}

/// Where a branch forked, or why that could not be established.
enum Measurement {
    At(String),
    Missing(Missing),
}

/// Record the cut point for `branch` **if one is not already recorded for it** — the one writer,
/// and the one **measurer**.
///
/// It used to take the value from the caller, and three callers grew three theories of what a cut
/// point is: trunk's tip, then the merge-base with trunk, then the land target's tip. Each was
/// wrong in a different situation, and the last was wrong in this project's own primary flow — a
/// task branch cut from a *staging* branch is ahead of its merge-base with trunk before any work
/// happens, so the freshly-cut guard never fired and an empty task closed as merged. So there is
/// nothing left to pass.
///
/// **What is measured** is where `branch` diverged from the branch it lands on — their merge-base
/// ([`measure`]) — falling back to the branch's own tip when no land target is known.
///
/// The merge-base is what makes the answer independent of *when* it is taken, and that is the
/// point. The tip is only the cut point at one instant, the moment the branch is created; a writer
/// that ran later recorded `base == tip` on a branch full of work, `is_merged` then answered
/// `NothingToMerge` forever, and the task could neither be credited by a review nor land.
/// `/task-swarm` hits exactly that — it can only name a group's branch *after* the implementer has
/// committed on it. A merge-base taken at any point in the branch's life gives the same commit, so
/// there is no longer a right moment to call this.
///
/// An existing record still always wins: only the first observation can know a cut point a later
/// rebase has moved past, and re-measuring is what overwrote real ones.
///
/// The failure mode stays safe in the direction D34.4 requires. With no land target and
/// pre-existing commits, `base == tip` reads as "nothing to merge" and the task is *held* rather
/// than closed — a missed auto-close costs one command; a false one buries work.
///
/// This runs `git` inside the write transaction, and — since the staleness check below — it does
/// so even when a cut point is already recorded: three or four processes on the writer thread
/// before the early return. That is the price of the check, which is what stops a record
/// outliving the branch it describes, and it is paid on `task work` / `task start` / `task tag`
/// rather than anywhere hot. Do not "optimise" it back to an early return on
/// `qualified(..).is_some()`; that is the bug it replaced.
///
/// A bare pre-qualification value is **adopted** (re-written as `<branch>:<sha>`) when this task
/// records no branch other than `branch`, since then it can only have been cut for this one. That
/// is strictly better than discarding it and substituting a fresh measurement: the bare value is
/// the real cut point, and anything measured now is a guess about the past.
///
/// Any bare value that is *not* adopted is removed. Leaving it would put the task back in the
/// single-branch case above the moment the branch facet is rewritten, and the reader would then
/// lend one branch's cut point to another — which is what disables the "freshly cut, nothing on
/// it yet" guard and closes a task with its work still uncommitted.
///
/// **Attribution reads the branch facet as it stands right now**, which is why
/// [`crate::repo::set_location_facets`] calls this *before* it rewrites `branch=` and is the only
/// caller. Run afterwards, the task always names exactly the branch being recorded, every bare
/// value looks attributable, and a cut point from the branch the task was on last week is adopted
/// for the one it is on today. Sequencing that correctly is not something two call sites should
/// each have to remember — this whole module exists because that kind of remembering failed four
/// times running.
///
/// `repo_root` is `None` when the caller is **not standing in the task's own repository**, and
/// then nothing is measured. A namesake branch in whatever checkout the cwd happens to be is not
/// this task's branch, and recording its tip as a verified cut point is worse than recording none.
///
/// `onto` is the branch this one was cut from, **as the caller states it in this call** — never
/// read back from the task's `onto=` facet, even though the caller is usually writing exactly that
/// value beside it.
///
/// That distinction is the point. Which branch a task's work was cut from is something the caller
/// knows *now*, in this moment; the stored facet is a record of some earlier moment, and a stale
/// one is worse than none here. A task carrying `onto=` from a previous batch, given a new branch
/// cut from trunk, would measure their merge-base — a commit well behind the new branch's tip — and
/// an empty branch identical to trunk would then skip the freshly-cut guard and close as merged.
/// Naming the parent branch is not the kind of judgement that went wrong four times; *computing a
/// commit id* was, and callers still cannot do that.
///
/// With no `onto` the fallback is the branch's own tip, which is the pre-existing rule and errs
/// towards holding (D34.4).
///
/// The `has_own_commits` gate in [`measure`] is what makes all of that safe rather than merely
/// careful: a caller can name the wrong parent, an unresolvable one, or a *grandparent*, and none
/// of it can change whether the recorded value equals the branch tip — which is the only thing any
/// reader asks of it.
///
/// # Errors
/// Returns an error if a tag read or write fails.
pub(crate) fn ensure_recorded(
    conn: &Connection,
    meta: &WriteMeta,
    id: ItemId,
    repo_root: Option<&std::path::Path>,
    branch: &str,
    onto: Option<&str>,
) -> jkb_core::Result<Option<Missing>> {
    let Some(root) = repo_root else {
        return Ok(Some(Missing::NotThisRepo(
            crate::repo::facet_one(&read_tags(conn, id)?, crate::repo::FACET_REPO)
                .cloned()
                .unwrap_or_else(|| "its repo".to_owned()),
        )));
    };
    let mut tags = read_tags(conn, id)?;

    // A record that cannot describe *this* branch is discarded before it can block a measurement.
    //
    // A cut point is keyed by branch NAME, and a name outlives the branch that held it: delete a
    // branch, cut a fresh one under the same name, and the old value still resolves. There is no
    // git fact distinguishing the two branches — but there is one that makes the record provably
    // wrong. **A branch with no commits of its own forked at its own tip**, by definition. So if
    // the branch is untouched and the record says anything else, the record belongs to whatever
    // held the name before.
    //
    // That is the whole guard, and it needs no flag threaded from the caller. An earlier version
    // carried "did `worktree_add` just create this branch?" as a bool through three signatures,
    // which `jkb task start` could not supply at all and a crash between the git write and the
    // database write silently lost.
    //
    // Nothing else needs checking, because a stale record can only do harm in this one case: with
    // commits of its own, the branch's mergedness is decided by `merge-tree` on content and the
    // recorded value is not consulted beyond being unequal to the tip.
    //
    // **The known false positive, and why it is the accepted direction.** "No commits of its own"
    // is also true of a branch whose work has been *merged away* — fast-forwarded into its batch,
    // or carried into trunk by a merge commit. Git cannot distinguish that from a recycled name:
    // both are a branch with no unique commits and a record that is not its tip. So this discards
    // a correct fork point in that case, re-measures, and records the tip — after which the task
    // is **held** rather than auto-closed. That is a missed close, which costs one command; the
    // alternative (not discarding) is a *false* close on a recycled name, which buries work
    // (D34.4). Squash and rebase merges rewrite the shas, so the branch keeps unique commits and
    // is unaffected — and `/task-swarm` records before its merge queue runs, never after. Pinned
    // by `a_branch_whose_work_was_merged_away_is_held_never_closed`, so that this is not
    // "corrected" into the unsafe direction.
    if let Some(recorded) = qualified(&tags, branch) {
        let stale = match untouched_tip(root, branch)? {
            Some(tip) => tip != recorded,
            None => false,
        };
        if !stale {
            return Ok(None);
        }
        forget(conn, meta, id, branch)?;
        tags = read_tags(conn, id)?;
    }

    match measure(root, branch, onto)? {
        Measurement::At(cut) => Ok(record_if_absent(
            conn,
            meta,
            id,
            Some(root),
            &tags,
            branch,
            Some(&cut),
        )?),
        // Nothing recorded, and the reason travels with it. Writing *something* here — the tip
        // was the tempting value — is the failure this module keeps having: it reads as a real
        // measurement, is never reported, and can never be corrected.
        Measurement::Missing(why) => {
            record_if_absent(conn, meta, id, Some(root), &tags, branch, None)?;
            Ok(if qualified(&read_tags(conn, id)?, branch).is_some() {
                None // an attributable legacy value was adopted after all
            } else {
                Some(why)
            })
        }
    }
}

/// The branch's tip **if the branch has no commits of its own**, in which case that tip is
/// provably its fork point. `None` when it has done work, or does not exist here.
///
/// The one place "an untouched branch forked at its tip" is turned into a value, so the writer's
/// staleness check and the measurement cannot disagree about what untouched means.
fn untouched_tip(repo_root: &std::path::Path, branch: &str) -> jkb_core::Result<Option<String>> {
    untouched_tip_git(repo_root, branch)
        .map_err(|e| jkb_types::Error::Validation(e.to_string()).into())
}

fn untouched_tip_git(repo_root: &std::path::Path, branch: &str) -> anyhow::Result<Option<String>> {
    use crate::gitrepo::{branch_ref, has_own_commits, rev_commit, Prefer};
    let Some(here) = branch_ref(repo_root, branch, Prefer::Local)? else {
        return Ok(None);
    };
    if has_own_commits(repo_root, &here, branch)? {
        return Ok(None);
    }
    rev_commit(repo_root, &here)
}

/// A task's facet tags as a multi-map, read inside a transaction.
fn read_tags(conn: &Connection, id: ItemId) -> jkb_core::Result<BTreeMap<String, Vec<String>>> {
    let mut tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (facet, value) in tag::applications(conn, id)? {
        tags.entry(facet).or_default().push(value);
    }
    Ok(tags)
}

/// The decision half of [`ensure_recorded`]: given a candidate, decide whether to record it.
///
/// Private, and split out only so the adoption states can be unit-tested without a git repo.
/// Callers outside this module reach [`ensure_recorded`], which measures — the candidate is not
/// something any of them may choose.
fn record_if_absent(
    conn: &Connection,
    meta: &WriteMeta,
    id: ItemId,
    repo_root: Option<&std::path::Path>,
    tags: &BTreeMap<String, Vec<String>>,
    branch: &str,
    cut: Option<&str>,
) -> jkb_core::Result<Option<Missing>> {
    if qualified(tags, branch).is_some() {
        return Ok(None);
    }

    let bare: Vec<String> = facet_values(tags, FACET)
        .iter()
        .filter(|v| !v.contains(':'))
        .cloned()
        .collect();
    // Only an object id may be adopted. A bare value that is not one arrived by a route that
    // never validated it — `jkb task add "… #base=HEAD"` reaches `tag::apply` directly — and
    // promoting it to this branch's qualified cut point would launder an unchecked string into the
    // record every landing decision reads.
    let adopted = if is_the_only_branch(tags, branch) {
        bare.iter().map(String::as_str).find(|v| is_object_id(v))
    } else {
        None
    };

    // An attributable pre-qualification value wins over the measurement: it is the real cut
    // point, and anything measured now is a guess about the past.
    //
    // …unless it is not a cut point at all. Adoption used to beat the measurement unconditionally,
    // including the one measurement that is a *proof* — an untouched branch forked at its own tip
    // — so a planted `#base=<older sha>` was preferred over it and the branch then read as having
    // moved. `rejected` is consulted rather than re-implemented, so the graceful path here and the
    // hard refusal in `write` cannot disagree about what is admissible.
    let mut refused = None;
    if let Some(sha) = adopted.or(cut) {
        match rejected(repo_root, branch, sha)? {
            None => write(conn, meta, id, repo_root, branch, sha)?,
            Some(why) => refused = Some(why),
        }
    }
    // A bare value is removed only when it can never legitimately serve **any** branch of this
    // task — which is exactly two cases:
    //
    //  * it was just adopted, so it now exists in qualified form and the bare copy would double
    //    the record; or
    //  * it is not a full object id, so `is_object_id` bars it from ever being adopted and
    //    `base_is_usable` bars any reader from acting on it. `jkb task add "… #base=HEAD"` plants
    //    exactly this, and leaving it lets a later run launder an unchecked string into the record
    //    every landing decision reads.
    //
    // Everything else stays. Removing *every* bare value was the previous rule, and it destroyed
    // the cut point the reader was still lending to a branch the task already had, the moment a
    // second branch was added — `jkb task tag add <uid> branch=<second>` reaches this directly.
    // An object id left in place is inert while the task names several branches (`resolve` lends
    // one only to a task naming a single branch), and becomes lendable again if the other branch
    // is removed, which is correct: it was that branch's cut point all along.
    for value in &bare {
        if Some(value.as_str()) == adopted || !is_object_id(value) {
            tag::remove(conn, meta, id, FACET, value)?;
        }
    }
    Ok(refused)
}

/// Drop every cut point [`resolve`] would hand `branch` — because the branch itself is gone.
///
/// `jkb task abandon --delete-branch` frees the branch *name* while leaving the task live, so the
/// next `jkb task work` cuts a **new** branch under it. The old record still resolved and still
/// differed from the new tip, so `is_merged` skipped its freshly-cut guard and `close-merged`
/// marked the task done with nothing written on it.
///
/// Unqualified legacy values go too, but only when this is the task's sole branch — which is
/// exactly when [`resolve`] would lend one to it. Removing them otherwise deletes a sibling's
/// record.
///
/// # Errors
/// Returns an error if a tag read or write fails.
pub(crate) fn forget(
    conn: &Connection,
    meta: &WriteMeta,
    id: ItemId,
    branch: &str,
) -> jkb_core::Result<()> {
    let tags = read_tags(conn, id)?;
    let lone = is_the_only_branch(&tags, branch);
    let prefix = format!("{branch}:");
    for value in facet_values(&tags, FACET) {
        if value.starts_with(&prefix) || (lone && !value.contains(':')) {
            tag::remove(conn, meta, id, FACET, value)?;
        }
    }
    Ok(())
}

/// The cut point now recorded for `branch`, if any — so a command can report it rather than
/// leaving a later reader to decline silently.
///
/// Returns the value, not a boolean: the human path only needs to know whether there is one, but
/// the JSON path should carry what was recorded, and two accessors would let those two answers
/// drift.
///
/// # Errors
/// Returns an error if the tags cannot be read.
pub(crate) fn recorded_for(
    db: &jkb_core::Db,
    id: ItemId,
    branch: &str,
) -> anyhow::Result<Option<String>> {
    Ok(resolve(&crate::repo::task_tags(db, id)?, branch).map(str::to_owned))
}

/// The latest commit on `branch` that `branch` did not itself create — its fork point.
///
/// **The tip is a measurement result under exactly one condition, and never a fallback.** A branch
/// with no commits of its own forked at its own tip, provably; that is the only case where the tip
/// is returned. Everywhere else, failing to measure yields [`Missing`] and *nothing is recorded*.
///
/// That distinction is the whole shape of this function, and getting it wrong is the defect it has
/// had twice. `base == tip` reads as "nothing has happened on this branch" forever — correct for an
/// untouched branch and catastrophic for one full of work, because `is_merged` then answers
/// `NothingToMerge`, no review can credit the task, `task land` refuses it, and `ensure_recorded`
/// will never overwrite. Three separate fallbacks here used to return the tip, and once the
/// untouched case was hoisted above them every one of them was reachable *only* when the branch had
/// work — i.e. only when the tip was the worst available answer.
///
/// **Why the caller cannot get this wrong.** `has_own_commits` asks git — is any commit here
/// reachable from no other branch — and needs no reference point, so a stale parent, a wrong
/// parent, an unresolvable one and a *grandparent* all fail to change the one thing readers ask of
/// the record: whether it equals the tip. The reference points below only pick which meaningful
/// commit to store for a branch that has genuinely diverged.
fn measure(
    repo_root: &std::path::Path,
    branch: &str,
    onto: Option<&str>,
) -> jkb_core::Result<Measurement> {
    measure_git(repo_root, branch, onto)
        .map_err(|e| jkb_types::Error::Validation(e.to_string()).into())
}

/// [`measure`] in `anyhow`'s error type, so the git calls compose with `?`.
fn measure_git(
    repo_root: &std::path::Path,
    branch: &str,
    onto: Option<&str>,
) -> anyhow::Result<Measurement> {
    use crate::gitrepo::{branch_ref, is_ancestor, merge_base, trunk, Prefer};
    let Some(here) = branch_ref(repo_root, branch, Prefer::Local)? else {
        return Ok(Measurement::Missing(Missing::NoSuchBranch));
    };
    // The one case where the tip is the answer, and it is an answer rather than a guess.
    if let Some(tip) = untouched_tip_git(repo_root, branch)? {
        return Ok(Measurement::At(tip));
    }

    // From here the branch has commits of its own, so its tip is certainly NOT its fork point and
    // must never be recorded. Either a reference point yields one, or nothing is recorded.
    let Some(onto) = onto else {
        return Ok(Measurement::Missing(Missing::NoParentNamed));
    };
    let Some(target) = branch_ref(repo_root, onto, Prefer::Local)? else {
        return Ok(Measurement::Missing(Missing::ParentNotFound));
    };

    // Everything this branch inherited rather than wrote: from the parent the caller named, and
    // from trunk. Taking the later of the two can only move the fork point forward, and only a
    // fork point that reaches the tip could mislead — which the untouched check above has already
    // excluded.
    let against: Vec<String> = std::iter::once(target).chain(trunk(repo_root)?).collect();
    let mut fork: Option<String> = None;
    for reference in against {
        let Some(candidate) = merge_base(repo_root, &here, &reference)? else {
            continue;
        };
        // Both candidates are merge-bases with the same branch, so each is an ancestor of it and
        // the two are ordered. Keep whichever is later; on an unorderable answer keep what we had.
        fork = match fork {
            Some(held) if !is_ancestor(repo_root, &held, &candidate)? => Some(held),
            _ => Some(candidate),
        };
    }
    Ok(match fork {
        Some(fork) => Measurement::At(fork),
        None => Measurement::Missing(Missing::NoCommonHistory),
    })
}

/// Whether `sha` may be stored as `branch`'s cut point, or why not.
///
/// **The one rule**, and the reason it is a function rather than a comment: the readers ask a cut
/// point exactly one question — does it equal the branch tip — so there are only two admissible
/// values, and which one applies is decided by git, not by whoever is writing.
///
///  * a branch with **no commits of its own** forked at its own tip, so its cut point *must* be
///    that tip;
///  * a branch **with** commits of its own certainly did not fork at its tip, so its cut point
///    must be anything else.
///
/// Violating the second is the defect this module has now had three times, by three different
/// routes: a fallback that returned the tip, an adopted legacy value that beat the measurement,
/// and a caller naming the branch as its own parent so the merge-base came back as the tip. Each
/// was fixed where it happened. This is the check that makes a fourth route impossible — and it
/// binds `jkb task base` too, where the sha nearest a user's hand is `git rev-parse <branch>`.
///
/// `None` for `repo_root` means nothing can be verified here (see [`crate::repo::measure_root_for`]),
/// and then nothing is: an unverifiable value is stored as given, exactly as it always was.
///
/// **Cost, accepted.** The same two git questions — does this branch exist, does it have commits
/// of its own — are asked here, by the staleness check in [`ensure_recorded`], and inside
/// [`measure`]: roughly a dozen short-lived `git` processes on the writer thread for one
/// `jkb task work`. Threading a precomputed answer between them was the obvious economy and is
/// deliberately not taken: carrying derived state between these three is exactly how the
/// created-ness flag went wrong, and this runs on three interactive commands rather than in any
/// loop. Measure before trading that back.
fn rejected(
    repo_root: Option<&std::path::Path>,
    branch: &str,
    sha: &str,
) -> jkb_core::Result<Option<Missing>> {
    let Some(root) = repo_root else {
        return Ok(None);
    };
    let tip = tip_of(root, branch)?;
    Ok(match (untouched_tip(root, branch)?, tip) {
        // Untouched: the tip is the only admissible value.
        (Some(untouched), _) if sha != untouched => Some(Missing::NotTheForkPoint),
        // Has work: the tip is the one inadmissible value.
        (None, Some(tip)) if sha == tip => Some(Missing::NotTheForkPoint),
        _ => None,
    })
}

/// The branch's tip, or `None` if this repository does not have the branch.
fn tip_of(repo_root: &std::path::Path, branch: &str) -> jkb_core::Result<Option<String>> {
    tip_of_git(repo_root, branch).map_err(|e| jkb_types::Error::Validation(e.to_string()).into())
}

fn tip_of_git(repo_root: &std::path::Path, branch: &str) -> anyhow::Result<Option<String>> {
    use crate::gitrepo::{branch_ref, rev_commit, Prefer};
    match branch_ref(repo_root, branch, Prefer::Local)? {
        Some(here) => rev_commit(repo_root, &here),
        None => Ok(None),
    }
}

/// Record the cut point for `branch`, replacing whatever this branch had — `jkb task base`.
///
/// Deliberately **not** [`crate::repo::set_facet`]: that clears the facet's other values, which
/// here are other branches' cut points. Only the entry whose `<branch>:` prefix matches is
/// replaced. `tag::apply` alone is wrong too — it is additive, so re-recording would accumulate
/// stale entries and [`resolve`] would return whichever came first.
///
/// # Errors
/// Returns an error if a tag write fails.
pub(crate) fn write(
    conn: &Connection,
    meta: &WriteMeta,
    id: ItemId,
    repo_root: Option<&std::path::Path>,
    branch: &str,
    sha: &str,
) -> jkb_core::Result<()> {
    // Enforced HERE, not at the callers, because the callers are the problem: three of them have
    // now stored a tip on a branch full of work, each by a different route. `record_if_absent`
    // consults the same predicate first so it can decline gracefully rather than error; this is
    // what makes sure it — and everything added later — cannot simply forget to.
    if rejected(repo_root, branch, sha)?.is_some() {
        // Worded for the surface that actually hits this — `jkb task base`, repairing a record by
        // hand — rather than through `Missing::remedy`, which is written for the commands that
        // *measure* and would tell someone already using the right verb to go and use another
        // one. Which half of the rule was broken is derivable from the branch, so the message says
        // both and lets the reader see which applies.
        return Err(jkb_types::Error::Validation(format!(
            "`{sha}` is not where {branch} forked. A branch with commits of its own did not \
             fork at its own tip — pass the fork point, e.g. `git merge-base <parent> \
             {branch}` — and a branch with no commits of its own forked at exactly its tip, so \
             that is the only value it can take. `jkb task start <uid> --branch {branch} --onto \
             <parent>` works this out for you."
        ))
        .into());
    }
    // The form check lives HERE rather than at the CLI verb, because the verb is not the only way
    // in: `ensure_recorded` writes too, and a caller-side rule is one more thing every present and
    // future writer has to remember — the failure this module exists to end.
    if !is_object_id(sha) {
        return Err(jkb_types::Error::Validation(format!(
            "`{sha}` is not a full commit id, and a cut point must be one: a symbolic revision \
             resolves to a different commit in every clone, so recording one silently closes the \
             task against work it never named. Pass the full 40-character id."
        ))
        .into());
    }
    let qualified = format!("{branch}:{sha}");
    let prefix = format!("{branch}:");
    for (f, v) in tag::applications(conn, id)? {
        if f == FACET && v != qualified && v.starts_with(&prefix) {
            tag::remove(conn, meta, id, &f, &v)?;
        }
    }
    tag::apply(conn, meta, id, FACET, &qualified)
}

#[cfg(test)]
mod tests {
    use super::{read_tags, record_if_absent, resolve, write, FACET};
    use crate::repo::{task_tags, FACET_BRANCH};
    use jkb_core::{item::NewItem, tag, Db};
    use jkb_types::ItemId;
    use std::collections::BTreeMap;

    fn tags(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (f, v) in pairs {
            out.entry((*f).to_owned())
                .or_default()
                .push((*v).to_owned());
        }
        out
    }

    fn a_task(db: &Db, pairs: &[(&str, &str)]) -> ItemId {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(f, v)| ((*f).to_owned(), (*v).to_owned()))
            .collect();
        db.write_txn("t", move |conn, meta| {
            let id = jkb_core::item::upsert(
                conn,
                meta,
                &NewItem {
                    uid: "task:t".to_owned(),
                    kind: "task".to_owned(),
                    content: None,
                    content_hash: None,
                    mime: None,
                },
            )?;
            for (f, v) in &pairs {
                tag::apply(conn, meta, id, f, v)?;
            }
            Ok(id)
        })
        .unwrap()
    }

    /// Record a cut point the way both writers do — through `set_location_facets`, which is the
    /// only caller and which sequences this before it rewrites `branch=`.
    fn ensure(db: &Db, id: ItemId, branch: &str, cut: Option<&str>) {
        let (branch, cut) = (branch.to_owned(), cut.map(str::to_owned));
        db.write_txn("t", move |conn, meta| {
            let tags = read_tags(conn, id)?;
            record_if_absent(conn, meta, id, None, &tags, &branch, cut.as_deref())
        })
        .unwrap();
    }

    /// Deleting a branch takes its cut point with it. Left behind, the record survives into the
    /// **next** branch to take that name — `jkb task work` after `abandon --delete-branch` cuts a
    /// fresh one — where it still resolves, still differs from the new tip, and so disables the
    /// freshly-cut guard that is all that stops an empty branch closing as merged.
    #[test]
    fn forgetting_a_branch_drops_the_cut_point_it_would_be_lent() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET_BRANCH, "task/b"),
                (FACET, "task/a:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                (FACET, "task/b:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            ],
        );
        db.write_txn("t", move |conn, meta| {
            super::forget(conn, meta, id, "task/a")
        })
        .unwrap();
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/a"),
            None,
            "the deleted branch's record survived it"
        );
        assert_eq!(
            resolve(&t, "task/b"),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            "a sibling branch's record was taken with it"
        );
    }

    /// An unqualified legacy value goes too — but only when it is *this* branch the reader would
    /// lend it to, which is the same attribution rule [`resolve`] applies. Otherwise a `forget`
    /// leaves behind exactly the value that will be handed to the branch's replacement.
    #[test]
    fn forgetting_a_lone_branch_drops_an_unqualified_legacy_value() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET, "1111111111111111111111111111111111111111"),
            ],
        );
        db.write_txn("t", move |conn, meta| {
            super::forget(conn, meta, id, "task/a")
        })
        .unwrap();
        assert!(
            super::facet_values(&task_tags(&db, id).unwrap(), FACET).is_empty(),
            "a legacy value the reader lends to this lone branch outlived the branch"
        );
    }

    // ---- the reader's four states (see the module docs) ----

    #[test]
    fn no_base_recorded_resolves_to_none() {
        assert_eq!(resolve(&tags(&[(FACET_BRANCH, "task/a")]), "task/a"), None);
    }

    #[test]
    fn a_qualified_base_resolves_for_its_own_branch() {
        let t = tags(&[
            (FACET_BRANCH, "task/a"),
            (FACET, "task/a:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ]);
        assert_eq!(
            resolve(&t, "task/a"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    /// The regression that closed a live task: lending one branch's cut point to another disables
    /// `is_merged`'s "freshly cut, nothing on it yet" guard for the branch that borrowed it.
    #[test]
    fn a_base_is_never_lent_to_a_branch_it_was_not_cut_for() {
        let t = tags(&[
            (FACET_BRANCH, "task/a"),
            (FACET_BRANCH, "task/b"),
            (FACET, "task/a:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ]);
        assert_eq!(
            resolve(&t, "task/a"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            resolve(&t, "task/b"),
            None,
            "task/b was handed task/a's cut point"
        );
    }

    #[test]
    fn a_bare_legacy_base_applies_to_a_lone_branch() {
        let t = tags(&[
            (FACET_BRANCH, "task/a"),
            (FACET, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
        ]);
        assert_eq!(
            resolve(&t, "task/a"),
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
    }

    #[test]
    fn a_bare_legacy_base_is_refused_when_several_branches_exist() {
        let t = tags(&[
            (FACET_BRANCH, "task/a"),
            (FACET_BRANCH, "task/b"),
            (FACET, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
        ]);
        assert_eq!(
            resolve(&t, "task/a"),
            None,
            "an unattributable legacy base was applied to one of several branches"
        );
    }

    // ---- the writer's four states ----

    #[test]
    fn a_missing_base_is_recorded_from_the_cut() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a")]);
        ensure(
            &db,
            id,
            "task/a",
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        );
        assert_eq!(
            resolve(&task_tags(&db, id).unwrap(), "task/a"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    /// The must-fix `task work` kept re-introducing: a later run must never recompute a cut point
    /// the branch has long since moved past.
    #[test]
    fn an_existing_base_is_never_overwritten() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET, "task/a:4444444444444444444444444444444444444444"),
            ],
        );
        ensure(
            &db,
            id,
            "task/a",
            Some("2222222222222222222222222222222222222222"),
        );
        assert_eq!(
            resolve(&task_tags(&db, id).unwrap(), "task/a"),
            Some("4444444444444444444444444444444444444444"),
            "a live branch's cut point was replaced with the land target's current tip"
        );
    }

    /// A base recorded for a *sibling* says nothing about this branch, so this branch must still
    /// get one — treating the task as "already recorded" left the new branch with none at all.
    #[test]
    fn a_siblings_base_does_not_count_as_recorded_for_this_branch() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET_BRANCH, "task/b"),
                (FACET, "task/a:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ],
        );
        ensure(
            &db,
            id,
            "task/b",
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        );
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/a"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "task/a's base was lost"
        );
        assert_eq!(
            resolve(&t, "task/b"),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    /// The pass-22 concern. A bare value is the branch's *real* cut point; the caller's `cut` is
    /// today's trunk tip. Substituting the guess for the fact broke the empty-branch guard.
    #[test]
    fn a_bare_legacy_base_is_adopted_rather_than_replaced() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET, "1111111111111111111111111111111111111111"),
            ],
        );
        ensure(
            &db,
            id,
            "task/a",
            Some("2222222222222222222222222222222222222222"),
        );
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/a"),
            Some("1111111111111111111111111111111111111111"),
            "the actual cut point was discarded in favour of today's tip"
        );
        assert_eq!(
            super::facet_values(&t, FACET).len(),
            1,
            "the bare value survived alongside its qualified form"
        );
    }

    /// But a bare value cannot be attributed once the task names another branch, and leaving it
    /// there would let the reader lend it out as soon as the branch facet is rewritten.
    #[test]
    fn an_unattributable_bare_base_is_dropped_not_adopted() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET_BRANCH, "task/b"),
                (FACET, "3333333333333333333333333333333333333333"),
            ],
        );
        ensure(
            &db,
            id,
            "task/b",
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        );
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/b"),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(
            resolve(&t, "task/a"),
            None,
            "an unattributable legacy value was left where it could be lent out"
        );
    }

    /// The explicit verb replaces one branch's record and leaves the others alone. `set_facet`
    /// here — the command the old error message recommended — deleted every sibling's.
    #[test]
    fn the_explicit_verb_replaces_only_its_own_branch() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET_BRANCH, "task/b"),
                (FACET, "task/a:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                (FACET, "task/b:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            ],
        );
        db.write_txn("t", move |conn, meta| {
            write(
                conn,
                meta,
                id,
                None,
                "task/b",
                "cccccccccccccccccccccccccccccccccccccccc",
            )
        })
        .unwrap();
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/a"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "a sibling was clobbered"
        );
        assert_eq!(
            resolve(&t, "task/b"),
            Some("cccccccccccccccccccccccccccccccccccccccc")
        );
        assert_eq!(
            super::facet_values(&t, FACET).len(),
            2,
            "a stale entry survived"
        );
    }

    /// A symbolic revision must never be stored. It resolves in every clone, to whatever that
    /// repository is pointed at now, so `is_merged` compares the branch tip against an unrelated
    /// commit, skips its freshly-cut guard, and closes a task whose work never landed. A foreign
    /// *object id* is harmless by comparison — it simply does not resolve, and the readers decline
    /// to act — so form, not reachability, is what has to be refused at the write.
    #[test]
    fn a_symbolic_revision_is_never_stored_as_a_cut_point() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a")]);
        for symbolic in ["HEAD", "main", "@", "origin/main", "1111111"] {
            let sym = symbolic.to_owned();
            let err = db
                .write_txn("t", move |conn, meta| {
                    write(conn, meta, id, None, "task/a", &sym)
                })
                .expect_err("a symbolic revision must be refused, not recorded");
            assert!(
                err.to_string().contains("full commit id"),
                "the refusal must say why: {err}"
            );
        }
        assert_eq!(resolve(&task_tags(&db, id).unwrap(), "task/a"), None);
    }

    /// And it is not laundered in through adoption either — `jkb task add "… #base=HEAD"` reaches
    /// `tag::apply` without passing any check, so the bare value must be dropped rather than
    /// promoted to this branch's qualified cut point.
    #[test]
    fn a_bare_value_that_is_not_an_object_id_is_dropped_not_adopted() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a"), (FACET, "HEAD")]);
        ensure(&db, id, "task/a", None);
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/a"),
            None,
            "`HEAD` was adopted as a cut point"
        );
        assert!(
            super::facet_values(&t, FACET).is_empty(),
            "the unusable bare value was left where a later run could adopt it"
        );
    }

    /// With no cut available and nothing on record, nothing is written — and the readers then
    /// refuse to act, which is the safe direction.
    #[test]
    fn no_cut_and_no_record_writes_nothing() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a")]);
        ensure(&db, id, "task/a", None);
        assert_eq!(resolve(&task_tags(&db, id).unwrap(), "task/a"), None);
    }
}
