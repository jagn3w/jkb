//! The commit a branch was cut from — the **one** module that knows how it is measured.
//!
//! ## Where the fact lives, and why that changed
//!
//! "Branch X was cut from commit Y" is a fact about a *branch*. It used to be stored as a tag
//! application on whichever tasks happened to name the branch, and tag applications are
//! item-keyed, multi-valued, untyped and writable from any route. Each of those four properties
//! produced its own family of defects — a `<branch>:<sha>` encoding that leaked to a dozen sites
//! with their own attribution rules, a documented repair that deleted *other* branches' records, a
//! symbolic revision stored verbatim, and five separate write routes taught the rule one at a
//! time.
//!
//! It is now a row in `branch_records`, keyed `(repo, branch)` — see [`jkb_core::branch`]. The
//! encoding, the attribution rules, and the question "which branch does this value belong to?" no
//! longer exist. What is left here is the half that cannot live in core, because core does not
//! shell out to git: **measurement**.
//!
//! ## The measurement rules, unchanged
//!
//! - **The tip is a measurement result under exactly one condition, and never a fallback.** A
//!   branch with no commits of its own forked at its own tip, provably; [`untouched_tip`] is the
//!   one place that is turned into a value. Everywhere else, failing to measure records *nothing*
//!   and reports why ([`Missing`]) — nothing is repairable and reported, a wrong value is silent
//!   and permanent.
//! - **What is measured is a merge-base, not a tip** ([`measure`]). A merge-base is the same
//!   commit whenever it is taken, so there is no right moment to call this; `/task-swarm` can only
//!   name a group's branch after an implementer has committed on it.
//! - **The parent is what the caller states in this call**, never the stored land target, which
//!   records an earlier moment and may name a batch this branch has nothing to do with.
//! - **`has_own_commits` is asked of git**, so a stale, wrong, unresolvable or *grandparent*
//!   parent cannot change the one thing readers ask of the record: whether it equals the tip. And
//!   it may answer *neither* — a broken ref anywhere fails the traversal — which is
//!   [`Missing::CannotAsk`, not "untouched"](Untouched), because "untouched" is the answer that
//!   makes the tip storable.
//!
//! ## The staleness rule is the write's shape, not a step in it
//!
//! A branch name outlives the branch that held it. The rule — a recorded value on an untouched
//! branch that is not its tip belongs to whatever held the name before — is **not** a discard
//! sequenced before an insert here. It is the `WHERE` clause of
//! [`jkb_core::branch::record_cut_point`]'s single statement, so a port cannot drop it by
//! omission and cannot mis-sequence it. What this module contributes is the *evidence*: a
//! [`Cut::UntouchedTip`] versus a [`Cut::Fork`], and the two anchor questions below.
//!
//! ## The instance anchor
//!
//! The one read-time check that is sound. A branch's creation reflog entry identifies *this*
//! instance of the name — written once per instance, destroyed by the deletion that ends it,
//! forged by no verb (`branch -f` and `checkout -B` append `Reset`-class entries). So:
//!
//! - a **mismatch** is positive proof of recycling, and supersedes the record (writer side) or
//!   refuses to act on it (reader side, [`stale_instance`]);
//! - a **match plus a `commit`-class-only journal** licenses *retaining* a record on an untouched
//!   branch — the merged-away case, whose fork point discard-and-hold would otherwise throw away;
//! - **absent or truncated** declines, degrading to the untouched-tip predicate. Every failure
//!   mode lands on today's behaviour, never on a new false close.

use jkb_core::{
    branch::{self, Cut, Supersede},
    Db, WriteMeta,
};
use rusqlite::Connection;

use crate::gitrepo;

/// The verb that drops a cut point, named in every refusal that has a repair path.
///
/// There is deliberately **no** verb anywhere that accepts a commit id. The sha nearest a user's
/// hand is the branch tip, and a cut point equal to the tip reads as "nothing has happened here"
/// forever — never creditable, never landable, never corrected. Three findings across three review
/// passes were the same shape, each fixed by rewording a message; there are only so many messages.
pub(crate) const FORGET_VERB: &str = "jkb task base --forget <branch>";

/// The verb that *measures* a cut point, spelled **once** and **private to this module**.
///
/// Nothing outside `base` may name it. That is not tidiness: a remedy naming this verb is only
/// safe when whoever prints it knows the branch's state, and three surfaces in a row printed it at
/// a moment when running it was actively harmful. The general rule, which this constant's privacy
/// is the enforcement of:
///
/// > **No message may name a measuring verb at a moment when measuring is harmful.**
///
/// Measuring is harmful exactly when the branch has **no commits of its own right now**, because
/// then the only value that can be measured is its tip — and `cut_point == tip` reads as "nothing
/// has happened on this branch" for ever: never creditable, never landable, and never corrected,
/// since `ensure_recorded` does not overwrite. A branch that has just been fast-forwarded into its
/// target is in exactly that state, which is why `jkb task landed`'s refusal — printed at the one
/// moment the work is provably gone from the branch — was the third instance of the family.
///
/// So the sentence is not something a surface writes. It is something a surface *asks for*, from
/// [`advice`], which puts the question to git. The one exception is [`Missing::remedy`], and it is
/// an exception by construction rather than by permission: the variants that name the verb are
/// only ever produced past `measure`'s untouched check, i.e. only for a branch that provably has
/// commits of its own.
const MEASURE_VERB: &str = "jkb task start <uid> --branch <branch> --onto <parent>";

/// [`MEASURE_VERB`] with the task and branch filled in, where the caller knows them — a
/// substitution rather than a second `format!`, so the verb really is spelled once.
fn measure_verb(uid: &str, branch: &str) -> String {
    MEASURE_VERB
        .replace("<uid>", uid)
        .replace("<branch>", branch)
}

/// What to tell a reader whose branch has **no usable cut point** — decided by asking git about the
/// branch, never written out by the surface that prints it.
///
/// See [`MEASURE_VERB`] for why this exists at all. The variants are the states that change the
/// answer, and only one of them may name a measuring verb.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Advice {
    /// A fork point can honestly be measured right now — either the branch has commits of its own
    /// (so its fork point is a real commit behind them), or it has never moved since it was cut
    /// (so its tip provably *is* its fork point).
    Measure,
    /// The branch has no commits of its own and has **moved** since it was cut, so its work was
    /// carried away — fast-forwarded into a batch, squashed into trunk, or rebased. Anything
    /// measurable here is its tip, and `cut_point == tip` freezes the task for good.
    TooLate,
    /// The branch has no commits of its own and the **ref journal cannot say** whether it was ever
    /// started — so which of [`Self::Measure`] and [`Self::TooLate`] applies is unknown.
    ///
    /// Its own variant because the two answers have opposite consequences and the difference is
    /// visible to the reader. Folded into `TooLate`, this state told the reader as **fact** that
    /// the branch "has moved since it was cut … so its work was carried away", and offered closing
    /// the task — about a branch that may be untouched and unstarted. Reached by
    /// `core.logAllRefUpdates=false`, reflog expiry, a reftable repository, and a branch that
    /// exists only as `origin/<b>` (the ordinary state after a merged PR deletes the local copy),
    /// so it is not a corner.
    ///
    /// It still withholds the measuring verb, and that is deliberate rather than an oversight:
    /// with the journal unreadable, "never started" and "merged away" present one identical
    /// signature (D34.4), and measuring an untouched-*looking* branch records its tip. Not
    /// knowing is a reason to say so, not a reason to act.
    Unverifiable,
    /// The branch does not exist in this repository.
    Gone,
    /// Git could not walk the repository's refs, so nothing about the branch can be established.
    CannotAsk,
}

impl Advice {
    /// The sentence to print, with no leading capital and no trailing stop, so it composes into a
    /// caller's own message the way [`Missing::remedy`] does.
    ///
    /// `uid` may be a placeholder (`<uid>`) where the surface is branch-keyed rather than
    /// task-keyed — `jkb task landed` knows a branch and no task.
    pub(crate) fn sentence(self, uid: &str, branch: &str) -> String {
        match self {
            Self::Measure => format!("measure one with `{}`", measure_verb(uid, branch)),
            // Deliberately names **no** runnable command for this branch. Everything a reader
            // could copy from here would record the tip.
            Self::TooLate => format!(
                "nothing here can measure it: {branch} has moved since it was cut and now has no \
                 commits of its own, so its work was carried away and the only value obtainable \
                 is its tip — which reads as \"nothing has happened on this branch\" for ever. A \
                 cut point has to be recorded while the branch is being cut, before its work \
                 lands. If this work did land, close the task directly (`jkb task set <uid> \
                 --status done`)"
            ),
            // Names no verb either — but says what is unknown rather than asserting the worse of
            // the two possibilities as fact.
            Self::Unverifiable => format!(
                "nothing here can measure it: {branch} has no commits of its own, and this \
                 repository's ref journal for it cannot be read (reflogs disabled, expired, or a \
                 branch that exists only on the remote), so whether it was never started or its \
                 work was already carried away cannot be told apart here. Both look identical to \
                 git, and measuring on the wrong one records the tip — which reads as \"nothing \
                 has happened on this branch\" for ever. Check what landed before deciding"
            ),
            Self::Gone => format!(
                "{branch} does not exist in this repository, so nothing here can measure it — \
                 remove the stale record (`jkb task tag rm <uid> branch={branch}`), or close the \
                 task directly if its work landed"
            ),
            Self::CannotAsk => format!(
                "git could not walk this repository's refs, so whether {branch} has any commits of \
                 its own is unknown — repair the repository (`git fsck` names a broken ref) and \
                 run it again"
            ),
        }
    }
}

impl Advice {
    /// How much this state argues for *not* acting, so a message about several branches can be
    /// governed by the most restrictive of them.
    ///
    /// A bucket naming two branches must not advise measuring merely because one of them could
    /// be: the reader runs the command once, against whichever name is at hand.
    fn caution(self) -> u8 {
        match self {
            Self::Measure => 0,
            Self::CannotAsk => 1,
            // Above `CannotAsk` (which is a repository fault, not a fact about this branch) and
            // below the two that state something definite: given a choice of sentences, one that
            // names what happened beats one that says it cannot be told.
            Self::Unverifiable => 2,
            Self::Gone => 3,
            Self::TooLate => 4,
        }
    }
}

/// The sentence for a bucket that names several branches — the most cautious of their answers,
/// spoken about the branch that produced it.
///
/// # Errors
/// Returns an error if git cannot be run at all.
pub(crate) fn advice_for_any(
    repo_root: &std::path::Path,
    uid: &str,
    branches: &[String],
) -> jkb_core::Result<String> {
    let mut worst: Option<(Advice, &String)> = None;
    for b in branches {
        let a = advice(repo_root, b)?;
        if worst.is_none_or(|(held, _)| a.caution() > held.caution()) {
            worst = Some((a, b));
        }
    }
    Ok(match worst {
        Some((a, b)) => a.sentence(uid, b),
        // No branch to ask about: the caller has nothing to advise, and inventing a verb here is
        // precisely what this module refuses to do.
        None => "no branch is recorded for it, so there is nothing to look at".to_owned(),
    })
}

/// Ask git what a reader should do about `branch`'s missing cut point.
///
/// The seam behind [`MEASURE_VERB`]'s rule. Every surface that refuses, holds or reports for want
/// of a cut point routes through here, so none of them has to know — or remember — that measuring
/// is only safe on a branch that has commits of its own.
///
/// # Errors
/// Returns an error if git cannot be run at all.
pub(crate) fn advice(repo_root: &std::path::Path, branch: &str) -> jkb_core::Result<Advice> {
    use crate::gitrepo::{branch_ref, Prefer};
    if git(branch_ref(repo_root, branch, Prefer::Local))?.is_none() {
        return Ok(Advice::Gone);
    }
    Ok(match untouched_tip(repo_root, branch)? {
        Untouched::No => Advice::Measure,
        // "No commits of its own" is two states, and only one of them is a problem (D34.4): a
        // branch that was never started, and one whose work was carried away. Refs cannot tell
        // them apart — but the **ref journal** can, by the same fact the retain-license uses: a
        // branch that has never moved since it was cut still points at its creation entry's
        // revision, so its tip provably *is* its fork point and recording it is right. A branch
        // that has moved and yet has nothing of its own had its work taken elsewhere.
        //
        // Absent or truncated journal → withhold the verb, the conservative side: withholding it
        // costs a sentence, naming it on a merged-away branch costs the task permanently.
        //
        // But "the journal says it moved" and "the journal could not be read" are two states, and
        // they were spelled the same. Only the first is evidence that the work was carried away;
        // saying so about the second told a reader with reflogs disabled — or with a branch that
        // exists only as `origin/<b>`, which never has a local journal — that their untouched
        // branch's work had already landed, and offered to close the task.
        Untouched::At(tip) => match git(gitrepo::ref_journal(repo_root, branch))? {
            Some(journal) if journal.anchor_sha == tip => Advice::Measure,
            Some(_) => Advice::TooLate,
            None => Advice::Unverifiable,
        },
        Untouched::Unknown => Advice::CannotAsk,
    })
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
///
/// **This is not the guard on the write.** `branch_records`' CHECK is, and it is the only one:
/// nothing calls this before a cut point is stored, so a symbolic revision fails as a constraint
/// violation. What this is for is the two questions git cannot be asked — whether a value
/// *recorded before that CHECK existed* still has the right form ([`crate::repo::base_is_usable`]),
/// and whether a landing's `landed_head` is a commit id at all, since one that is not can never be
/// credited and recording it would mean "never credited" while reading as "recorded".
pub(crate) fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Why no cut point could be recorded — carried out to the caller so the surface that *reports* it
/// states the reason the writer actually had, rather than re-deriving one from a proxy.
///
/// Every variant means "nothing was recorded", which both readers treat as *do not act*. That is
/// deliberately distinct from recording a plausible-looking value: a task with no cut point is
/// listed by `close-merged` as undecidable, names a remedy, and is repaired by the next run that
/// can measure. A task holding a wrong one is silent and permanent. Recording a guess is the worse
/// of the two, always.
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
    /// Git could not say whether the branch has commits of its own, so which of the two admissible
    /// values applies is unknown. A repository-level fault — a broken ref under `refs/heads` or
    /// `refs/remotes` fails the whole traversal — not anything about this branch.
    ///
    /// Its own variant rather than folded into the others because "untouched" is the answer that
    /// makes the tip admissible, and guessing it is how a task acquires a cut point equal to its
    /// tip and never closes again.
    CannotAsk,
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
            Self::CannotAsk => "git-could-not-answer-here",
        }
    }

    /// What the reader should do about it. Never "pass a sha by hand": the sha nearest to hand is
    /// the branch tip, and a cut point equal to the tip reads as "nothing has happened here"
    /// forever, which is how a task becomes permanently unlandable.
    ///
    /// This is the one place outside [`advice`] that names [`MEASURE_VERB`], and it is safe **by
    /// construction**, not by permission: the four variants below that name it are produced only
    /// past `measure`'s untouched check, i.e. only for a branch that provably has commits of its
    /// own — which is exactly [`Advice::Measure`]'s condition. A new variant reachable on an
    /// untouched branch must not join that arm; call [`advice`] instead.
    pub(crate) fn remedy(&self, uid: &str, branch: &str) -> String {
        match self {
            Self::NotThisRepo(wanted) => {
                format!("run it again from {wanted}, the repository {branch} lives in")
            }
            Self::NoSuchBranch => {
                format!("run it again once {branch} exists")
            }
            // Nothing about the task or the branch is wrong, so no jkb verb repairs it: the
            // repository itself cannot answer, and the next run after it can will measure.
            Self::CannotAsk => format!(
                "git could not walk this repository's refs, so whether {branch} has any commits \
                 of its own is unknown — repair the repository (`git fsck` names a broken ref) \
                 and run it again"
            ),
            Self::NoParentNamed
            | Self::ParentNotFound
            | Self::NoCommonHistory
            | Self::NotTheForkPoint => format!(
                "name the branch {branch} was cut from: `{}`",
                measure_verb(uid, branch)
            ),
        }
    }
}

/// Where a branch forked, or why that could not be established.
enum Measurement {
    At(Cut),
    Missing(Missing),
}

/// Measure `branch`'s cut point and record it — the one **measurer**, and the only caller of
/// [`jkb_core::branch::record_cut_point`] in this crate.
///
/// Callers pass no value. Three of them once did, and grew three theories of what a cut point is:
/// trunk's tip, the merge-base with trunk, then the land target's tip. The last was wrong in this
/// project's own primary flow — a task branch cut from a *staging* branch is ahead of its
/// merge-base with trunk before any work happens, so the freshly-cut guard never fired and an
/// empty task closed as merged. So there is nothing left to pass.
///
/// An existing record still always wins, except against the two proofs that it describes a
/// *different branch of the same name* ([`Supersede`]); that decision is made inside the one
/// statement rather than here, so it cannot be dropped or mis-ordered by a caller.
///
/// `cut_from` is the branch this one forked off, **as the caller states it in this call** —
/// never read back from the stored land target, which records an earlier moment. It is
/// deliberately not called `onto`: inside this module `onto` would mean the measurement parent
/// while everywhere else it means the *land target*, and the two come apart at trunk, which is a
/// fine parent and an unacceptable land target (D34.3). A task carrying a land
/// target from a previous batch, given a new branch cut from trunk, would otherwise measure their
/// merge-base — well behind the new branch's tip — and an empty branch identical to trunk would
/// then skip the freshly-cut guard and close as merged. Naming the parent is a fact the caller
/// has; *computing a commit id* is the judgement that went wrong four times, and callers still
/// cannot do that.
///
/// # Errors
/// Returns an error if git cannot be run or the record cannot be written.
pub(crate) fn ensure_recorded(
    conn: &Connection,
    meta: &WriteMeta,
    repo_root: &std::path::Path,
    repo: &str,
    branch: &str,
    cut_from: Option<&str>,
) -> jkb_core::Result<Option<Missing>> {
    let root = repo_root;
    let stored = branch::get(conn, repo, branch)?;
    // One reflog read, feeding both anchor questions. `None` — reflogs off, log expired, a branch
    // that only exists on the remote — judges nothing, so both fall back to the tip predicate.
    let journal = git(gitrepo::ref_journal(root, branch))?;
    let stored_anchor = stored.as_ref().and_then(|r| r.anchor.as_ref());
    let anchor_mismatch = match (stored_anchor, journal.as_ref()) {
        (Some(a), Some(j)) => a.sha != j.anchor_sha || a.ts != j.anchor_ts,
        _ => false,
    };
    let fresh_anchor = journal.as_ref().map(|j| branch::Anchor {
        sha: j.anchor_sha.clone(),
        ts: j.anchor_ts,
    });
    let had_record = stored.as_ref().is_some_and(|r| r.cut_point.is_some());

    let why = match measure(root, branch, cut_from)? {
        Measurement::At(cut) => {
            // **The retain-license.** "No commits of its own" is also true of a branch whose work
            // was *merged away* — fast-forwarded into its batch, or carried into trunk by a merge
            // commit — and discarding its real fork point there costs a missed close. That branch
            // was never deleted, so its creation entry matches, and its tip was reached only by
            // `commit`-class entries; every verb that re-points a branch writes a `Reset`-class
            // one. Where the anchor cannot verify, this does not fire and the record is discarded
            // and the task held, which is today's behaviour. The degradation direction is always
            // discard-and-hold: a git that changes its reflog vocabulary can re-price the missed
            // close, never mint a false one.
            let retain = matches!(cut, Cut::UntouchedTip(_))
                && stored_anchor.is_some()
                && !anchor_mismatch
                && journal.as_ref().is_some_and(|j| j.only_commits);
            let refused = rejected(Some(root), branch, cut.sha())?;
            if refused.is_none() {
                branch::record_cut_point(
                    conn,
                    meta,
                    repo,
                    branch,
                    &cut,
                    fresh_anchor.as_ref(),
                    Supersede {
                        untouched: matches!(cut, Cut::UntouchedTip(_)) && !retain,
                        anchor_mismatch,
                    },
                )?;
            }
            refused
        }
        // Nothing recorded, and the reason travels with it. Writing *something* here — the tip is
        // the tempting value — is the failure this module keeps having: it reads as a real
        // measurement, is never reported, and can never be corrected.
        Measurement::Missing(why) => Some(why),
    };
    // A record that already stood is not a problem to report, whatever this run could measure.
    let recorded = why.is_none() || had_record;
    if recorded {
        // The anchor is only as durable as the reflog, so coverage is established rather than
        // assumed. Best-effort by design: a clone that never got the entry expires on schedule and
        // the anchor checks then decline, which is the same degradation as reflogs being off.
        git(gitrepo::retain_reflog(root, branch))?;
    }
    Ok(if had_record { None } else { why })
}

/// Fill in a cut point for a branch this call merely **refers to** — someone else's land target.
///
/// Distinct from [`ensure_recorded`] because the two answer different questions, and using the
/// measuring one here destroyed live records. `jkb task start B --onto integration` is a statement
/// about **B**; whatever it can observe about `integration` is not evidence about `integration`'s
/// identity. But the merge queue fast-forwards a batch branch, which leaves it truthfully reading
/// as *untouched* — so `ensure_recorded`'s supersede arm fired on the batch, replaced the real cut
/// point X with the batch's current tip T1, and froze every task already landed on it at
/// `NothingToMerge` **permanently**: `is_merged` answers that whenever `cut_point == tip`, and
/// `jkb task base --forget` cannot repair it because [`advice`] then answers [`Advice::TooLate`].
///
/// So this may only ever **fill**, in two independent ways: it returns before measuring if a cut
/// point is already recorded, and it passes [`Supersede::default()`](Supersede), under which
/// `record_cut_point`'s statement is `WHERE cut_point IS NULL`. Staleness evidence belongs to the
/// branch being recorded, not to the branch it names as a parent.
///
/// And filling is itself gated on [`advice`] — the same question every *message* asks before
/// naming a measuring verb. If it would be harmful to tell a reader to measure this branch now, it
/// is harmful to measure it now: on a branch that has moved and has no commits of its own the only
/// obtainable value is its tip, which is the frozen state above arrived at through the other arm.
/// Recording nothing is reported and repairable; a tip is silent and permanent.
///
/// # Errors
/// Returns an error if git cannot be run or the record cannot be written.
pub(crate) fn fill_for_reference(
    conn: &Connection,
    meta: &WriteMeta,
    repo_root: &std::path::Path,
    repo: &str,
    branch: &str,
    cut_from: Option<&str>,
) -> jkb_core::Result<()> {
    if branch::get(conn, repo, branch)?.is_some_and(|r| r.cut_point.is_some()) {
        return Ok(());
    }
    if advice(repo_root, branch)? != Advice::Measure {
        return Ok(());
    }
    let Measurement::At(cut) = measure(repo_root, branch, cut_from)? else {
        return Ok(());
    };
    if rejected(Some(repo_root), branch, cut.sha())?.is_some() {
        return Ok(());
    }
    let anchor = git(gitrepo::ref_journal(repo_root, branch))?.map(|j| branch::Anchor {
        sha: j.anchor_sha,
        ts: j.anchor_ts,
    });
    branch::record_cut_point(
        conn,
        meta,
        repo,
        branch,
        &cut,
        anchor.as_ref(),
        Supersede::default(),
    )?;
    // The anchor is only as durable as the reflog, so coverage is established rather than assumed
    // — the same best-effort licence `ensure_recorded` takes.
    git(gitrepo::retain_reflog(repo_root, branch))?;
    Ok(())
}

/// Whether `record`'s branch is a **different branch of the same name** — verified, positive proof
/// of recycling, read at the moment a reader is about to act on the record.
///
/// The one read-time staleness check that is sound, and it is sound precisely because it is not a
/// *signature*. Three states present one identical observable signature — no commits of its own, a
/// record that is not the tip, adds nothing to trunk: a branch rebase-ff-merged externally, one
/// merge-commit-merged externally, and a recycled name. D34.2 exists to close the first and D34.4
/// forbids closing the last, so no signature predicate evaluated at read time can be right. An
/// anchor mismatch separates them: the two externally-merged branches were never deleted, so their
/// creation entries still match.
///
/// Acts **only toward hold**: a mismatch means do not act, and an absent or truncated journal, or
/// a record with no anchor, proceeds exactly as before. One priced consequence — a second clone
/// has its own creation entries, so a record made in one checkout and read in another mismatches
/// and is held. A missed close, the accepted direction.
///
/// # Errors
/// Returns an error if git cannot be run.
pub(crate) fn stale_instance(
    repo_root: &std::path::Path,
    branch: &str,
    record: &branch::BranchRecord,
) -> anyhow::Result<bool> {
    let Some(anchor) = record.anchor.as_ref() else {
        return Ok(false);
    };
    let Some(journal) = gitrepo::ref_journal(repo_root, branch)? else {
        return Ok(false);
    };
    Ok(anchor.sha != journal.anchor_sha || anchor.ts != journal.anchor_ts)
}

/// Drop `branch`'s recorded **cut point**, so the next `task start` / `task work` measures again —
/// `jkb task base --forget`.
///
/// The branch still exists, so only the measurement is dropped: where it lands, and whether jkb
/// landed it, are separate facts and taking them out here would drop the task out of
/// `jkb staging ls` as a side effect of repairing a commit id. The reflog retention entry stays
/// for the same reason — the branch is still one we record.
///
/// # Errors
/// Returns an error if the write or the changelog append fails.
pub(crate) fn forget_cut_point(
    conn: &Connection,
    meta: &WriteMeta,
    repo: &str,
    branch: &str,
) -> jkb_core::Result<bool> {
    branch::forget_cut_point(conn, meta, repo, branch)
}

/// Drop `branch`'s record, and the reflog retention entry that went with it — because the branch
/// itself is gone.
///
/// `jkb task abandon --delete-branch` frees the branch *name* while leaving the task live, so the
/// next `jkb task work` cuts a **new** branch under it. The old record still resolved and still
/// differed from the new tip, so `is_merged` skipped its freshly-cut guard and `close-merged`
/// marked the task done with nothing written on it.
///
/// Never a step in a write. It is `abandon --delete-branch`'s verb and `jkb task base --forget`'s,
/// and nothing else calls it.
///
/// # Errors
/// Returns an error if the delete or the changelog append fails.
pub(crate) fn forget(
    conn: &Connection,
    meta: &WriteMeta,
    repo_root: Option<&std::path::Path>,
    repo: &str,
    branch: &str,
) -> jkb_core::Result<bool> {
    let dropped = branch::forget(conn, meta, repo, branch)?;
    if let Some(root) = repo_root {
        git(gitrepo::release_reflog(root, branch))?;
    }
    Ok(dropped)
}

/// The cut point recorded for `branch`, if any — so a command can report it rather than leaving a
/// later reader to decline silently.
///
/// Returns the value, not a boolean: the human path only needs to know whether there is one, but
/// the JSON path should carry what was recorded, and two accessors would let those answers drift.
///
/// # Errors
/// Returns an error if the record cannot be read.
pub(crate) fn recorded_for(db: &Db, repo: &str, branch: &str) -> anyhow::Result<Option<String>> {
    let (repo, branch) = (repo.to_owned(), branch.to_owned());
    Ok(db
        .read(move |conn| branch::get(conn, &repo, &branch))?
        .and_then(|r| r.cut_point))
}

/// Whether a branch is provably untouched, and at which tip.
///
/// Three states, not two, because the third is the one that costs: "git could not tell us" spelled
/// as `No` merely declines, but spelled as `At(tip)` records the tip of a branch full of work and
/// freezes its task for good. Both readers below fail closed on [`Untouched::Unknown`].
enum Untouched {
    /// No commits of its own, so this tip **is** its fork point.
    At(String),
    /// It has commits of its own, or does not exist here.
    No,
    /// Git could not answer — a broken ref anywhere fails the traversal.
    Unknown,
}

/// The branch's tip **if the branch has no commits of its own**, in which case that tip is
/// provably its fork point.
///
/// The one place "an untouched branch forked at its tip" is turned into a value, so the
/// admissibility rule and the measurement cannot disagree about what untouched means.
fn untouched_tip(repo_root: &std::path::Path, branch: &str) -> jkb_core::Result<Untouched> {
    git(untouched_tip_git(repo_root, branch))
}

fn untouched_tip_git(repo_root: &std::path::Path, branch: &str) -> anyhow::Result<Untouched> {
    use crate::gitrepo::{branch_ref, has_own_commits, rev_commit, Prefer};
    let Some(here) = branch_ref(repo_root, branch, Prefer::Local)? else {
        return Ok(Untouched::No);
    };
    match has_own_commits(repo_root, &here, branch)? {
        None => return Ok(Untouched::Unknown),
        Some(true) => return Ok(Untouched::No),
        Some(false) => {}
    }
    Ok(match rev_commit(repo_root, &here)? {
        Some(tip) => Untouched::At(tip),
        None => Untouched::No,
    })
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
/// `NothingToMerge`, no review can credit the task, and `task land` refuses it. Three separate
/// fallbacks here used to return the tip, and once the untouched case was hoisted above them every
/// one of them was reachable *only* when the branch had work — i.e. only when the tip was the worst
/// available answer.
///
/// **Why the caller cannot get this wrong.** `has_own_commits` asks git — is any commit here
/// reachable from no other branch — and needs no reference point, so a stale parent, a wrong
/// parent, an unresolvable one and a *grandparent* all fail to change the one thing readers ask of
/// the record: whether it equals the tip. The reference points below only pick which meaningful
/// commit to store for a branch that has genuinely diverged.
fn measure(
    repo_root: &std::path::Path,
    branch: &str,
    cut_from: Option<&str>,
) -> jkb_core::Result<Measurement> {
    git(measure_git(repo_root, branch, cut_from))
}

/// [`measure`] in `anyhow`'s error type, so the git calls compose with `?`.
fn measure_git(
    repo_root: &std::path::Path,
    branch: &str,
    cut_from: Option<&str>,
) -> anyhow::Result<Measurement> {
    use crate::gitrepo::{branch_ref, is_ancestor, merge_base, trunk, Prefer};
    let Some(here) = branch_ref(repo_root, branch, Prefer::Local)? else {
        return Ok(Measurement::Missing(Missing::NoSuchBranch));
    };
    // The one case where the tip is the answer, and it is an answer rather than a guess.
    match untouched_tip_git(repo_root, branch)? {
        Untouched::At(tip) => return Ok(Measurement::At(Cut::UntouchedTip(tip))),
        // Not knowing whether the branch is untouched means not knowing which of the two
        // admissible values applies, and the tempting guess — "untouched" — is the one that makes
        // the tip storable. Record nothing and say so.
        Untouched::Unknown => return Ok(Measurement::Missing(Missing::CannotAsk)),
        Untouched::No => {}
    }

    // From here the branch has commits of its own, so its tip is certainly NOT its fork point and
    // must never be recorded. Either a reference point yields one, or nothing is recorded.
    let Some(cut_from) = cut_from else {
        return Ok(Measurement::Missing(Missing::NoParentNamed));
    };
    let Some(target) = branch_ref(repo_root, cut_from, Prefer::Local)? else {
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
        Some(fork) => Measurement::At(Cut::Fork(fork)),
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
///    that tip — see the caveat below, which is a deliberate cost rather than an oversight;
///  * a branch **with** commits of its own certainly did not fork at its tip, so its cut point
///    must be anything else.
///
/// Violating the second is the defect this area has now had three times, by three different
/// routes: a fallback that returned the tip, an adopted legacy value that beat the measurement,
/// and a caller naming the branch as its own parent so the merge-base came back as the tip. The
/// first two are gone with the encoding; the third is still reachable — `--onto <this branch>`
/// makes `merge_base(here, here)` the tip — which is why this survives as a check on the
/// measurement rather than as a doc comment.
///
/// **"No commits of its own" describes two states, and this refuses both the same way.** A branch
/// that was never started, and one whose commits were fast-forwarded away into its batch or trunk,
/// are the same to git. That is the D34.4 trade taken deliberately: the alternative admits an older
/// value on a never-started branch, which skips the freshly-cut guard and closes a task with
/// nothing on it. Where the instance anchor verifies, the merged-away branch keeps its record via
/// the retain-license in [`ensure_recorded`]; where it cannot, the remedy for a task that genuinely
/// landed is `jkb task set <uid> --status done`, not a cut point.
///
/// `None` for `repo_root` means nothing can be verified here, and then nothing is.
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
        // Which of the two rules applies is unknown, so neither can be checked. Written as the
        // fail-closed *default* rather than as a live guard: `measure` asks the same question
        // first and stops at `Missing::CannotAsk`, so nothing reaches here today. It matters
        // because the alternative default is `None` — admit the value — and this function's whole
        // job is to be the arm that cannot admit a tip by accident.
        (Untouched::Unknown, _) => Some(Missing::CannotAsk),
        // Untouched: the tip is the only admissible value.
        (Untouched::At(untouched), _) if sha != untouched => Some(Missing::NotTheForkPoint),
        // Has work: the tip is the one inadmissible value.
        (Untouched::No, Some(tip)) if sha == tip => Some(Missing::NotTheForkPoint),
        _ => None,
    })
}

/// The branch's tip, or `None` if this repository does not have the branch.
fn tip_of(repo_root: &std::path::Path, branch: &str) -> jkb_core::Result<Option<String>> {
    git(tip_of_git(repo_root, branch))
}

fn tip_of_git(repo_root: &std::path::Path, branch: &str) -> anyhow::Result<Option<String>> {
    use crate::gitrepo::{branch_ref, rev_commit, Prefer};
    match branch_ref(repo_root, branch, Prefer::Local)? {
        Some(here) => rev_commit(repo_root, &here),
        None => Ok(None),
    }
}

/// Carry an `anyhow` git failure into `jkb_core`'s error type — spelled once, because this module
/// crosses that boundary in six places and each spelling was a chance to lose the message.
fn git<T>(result: anyhow::Result<T>) -> jkb_core::Result<T> {
    result.map_err(|e| jkb_types::Error::Validation(e.to_string()).into())
}

#[cfg(test)]
mod tests {
    /// **Only `base.rs` may tell a reader to measure a cut point.**
    ///
    /// The rule this enforces is stated at [`super::MEASURE_VERB`]: a remedy naming a measuring
    /// verb is only safe when whoever prints it knows the branch's state, and three surfaces in a
    /// row printed one at a moment when running it was harmful. The constant is private to this
    /// module so no other file can *format* the verb, and [`super::advice`] is how a surface gets
    /// the sentence instead — but privacy cannot stop someone typing the words out, so this reads
    /// the crate's own sources back.
    ///
    /// The predicate is narrow on purpose, and it reads **string literals**, not lines. `--onto`
    /// appears legitimately in the agent guide and in clap's help, which *list* a command's syntax
    /// rather than prescribing it, and `measure_root_for` is an ordinary identifier; what no
    /// legitimate site does is put `--onto` and a measuring word in one **message**.
    ///
    /// What it cannot catch is a remedy that names the verb without either word. That is a real
    /// limit, and the reason it is worth having anyway is that every one of the four historical
    /// instances said "measure".
    #[test]
    fn only_this_module_tells_a_reader_to_measure_a_cut_point() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        // Read the directory rather than listing the files: a list would be one more thing to
        // remember, which is the shape of defect this whole test exists to close.
        for entry in std::fs::read_dir(&dir).expect("crate sources") {
            let path = entry.expect("dir entry").path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            if path.file_name().is_some_and(|f| f == "base.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source file");
            let code: String = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            for literal in string_literals(&code) {
                let names_verb = literal.contains("--onto");
                let tells_you_to_measure = literal.contains("measur") || literal.contains("Measur");
                if names_verb && tells_you_to_measure {
                    offenders.push(format!(
                        "{}: {literal}",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                    ));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a surface outside `base.rs` names a measuring verb in a message. It cannot know \
             whether measuring is safe for that branch — call `base::advice(root, branch)` and \
             print its sentence. Offenders:\n{}",
            offenders.join("\n---\n")
        );
    }

    /// Every double-quoted literal in `code`, escapes skipped. Crude by design — it exists to feed
    /// the scan above, not to parse Rust — and it errs towards *including* text, which can only
    /// make the guard stricter.
    fn string_literals(code: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut chars = code.chars();
        while let Some(c) = chars.next() {
            if c != '"' {
                continue;
            }
            let mut lit = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        // The escaped character is consumed, so an embedded `\"` cannot end the
                        // literal early. `jkb doctor` prints a message containing one; without
                        // this it would split into pieces that each look innocent.
                        if let Some(next) = chars.next() {
                            lit.push(next);
                        }
                    }
                    '"' => break,
                    _ => lit.push(c),
                }
            }
            out.push(lit);
        }
        out
    }

    /// The scanner reads **messages**, not identifiers, and an escaped quote inside a message does
    /// not end it early.
    ///
    /// Both halves are load-bearing for the guard above. `measure_root_for` is an ordinary
    /// function name and must not be mistaken for a sentence, or the guard fires on code that
    /// tells nobody anything. And a message containing `\"` — `jkb doctor` prints one — must stay
    /// whole, or it splits into fragments that each look innocent while the sentence they form
    /// does not.
    #[test]
    fn the_literal_scanner_reads_messages_not_identifiers_and_keeps_an_escaped_quote() {
        let code = "let x = measure_root_for(a);\nbail!(\"say \\\"--onto\\\" and measure\");";
        let lits = string_literals(code);
        assert_eq!(
            lits,
            vec!["say \"--onto\" and measure".to_owned()],
            "an escaped quote split the message, or an identifier was read as one"
        );
    }
}
