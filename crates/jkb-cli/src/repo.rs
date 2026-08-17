//! The repo + facet vocabulary the session, staging and review surfaces share.
//!
//! Where a task is being worked — which repo, which branch, which staging branch it lands on
//! — is recorded as plain facet tags (design D34.1/D36), and several modules need to read and
//! write them. They lived in `main.rs` beside the clap surface, which made `staging.rs` and
//! `review.rs` depend *inward* on the binary root while every other module depends sideways,
//! and forced eleven items to be `pub(crate)` in an already very large file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use jkb_core::{item, tag, Db};
use jkb_types::ItemId;

use crate::gitrepo;

/// The facet recording which branch a task is being done on, and which repo that branch is
/// in. Plain tags (design D34.1): no migration, and queryable as `tag:branch=<name>`.
pub(crate) const FACET_BRANCH: &str = "branch";
pub(crate) const FACET_REPO: &str = "repo";

/// The branch a session's work lands on used to be a facet here (`onto=`). It is now
/// `branch_records.land_target`, because it was branch-keyed by accident of having exactly one
/// writer: two tasks on one branch could record different land targets, and `None` on a facet
/// could not tell "lands on trunk" from "never recorded". See `jkb_core::branch::BranchRecord`.
/// A task's facet tags, **every** value per facet.
///
/// Tags are a multi-map: `tag::apply` adds, so a task can legitimately carry two `branch=`
/// values — one from `jkb task start` (D34) and one from `task work`. Collapsing them to a
/// single value silently picks one, and picking the wrong `branch=` makes `task work` mint a
/// second session for a task that already has one.
pub(crate) fn task_tags(db: &Db, id: ItemId) -> Result<BTreeMap<String, Vec<String>>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (facet, value) in db.read(move |conn| tag::applications(conn, id))? {
        out.entry(facet).or_default().push(value);
    }
    Ok(out)
}

/// The values recorded for one facet.
pub(crate) fn facet_values<'a>(
    tags: &'a BTreeMap<String, Vec<String>>,
    facet: &str,
) -> &'a [String] {
    tags.get(facet).map_or(&[], Vec::as_slice)
}

/// The single value of a facet that should only ever have one (`repo`).
///
/// The two facts that are per-*branch* rather than per-task — where a branch was cut and where it
/// lands — are not facets at all any more; they are `branch_records` columns, keyed
/// `(repo, branch)`, so there is nothing here to collapse.
pub(crate) fn facet_one<'a>(
    tags: &'a BTreeMap<String, Vec<String>>,
    facet: &str,
) -> Option<&'a String> {
    facet_values(tags, facet).first()
}

/// Set a facet to exactly one value, removing any others it already had.
///
/// `tag::apply` is additive, which is right for open-ended facets and wrong for the ones that
/// answer "where is this being worked" — a second value there is not extra information, it is
/// a contradiction the readers have to guess their way through.
pub(crate) fn set_facet(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    id: ItemId,
    facet: &str,
    value: &str,
) -> jkb_core::Result<()> {
    for (f, v) in tag::applications(conn, id)? {
        if f == facet && v != value {
            tag::remove(conn, meta, id, &f, &v)?;
        }
    }
    tag::apply(conn, meta, id, facet, value)
}

/// Where a task is being worked. Every field is single-valued by nature: a second `branch=`
/// is a contradiction, not extra information (design D36.6).
///
/// There is deliberately no cut-point *value* here. It used to ride along as a commit the caller
/// had computed, and three callers grew three theories of what it was;
/// [`crate::base::ensure_recorded`] measures it instead. What a caller may still state is
/// `cut_from` — which *branch* this one forked off — because that is a fact the caller has rather
/// than a judgement it has to make.
#[derive(Default)]
pub(crate) struct Location<'a> {
    pub(crate) branch: Option<&'a str>,
    pub(crate) repo: Option<&'a str>,
    pub(crate) onto: Option<&'a str>,
    /// The branch `branch` was **cut from**, for measuring the cut point. Usually the same as
    /// `onto`, and deliberately a separate field because the two come apart at trunk: a branch cut
    /// from trunk has a perfectly good measurement reference and must not record trunk as a land
    /// target, or it reads as merged the moment anything lands (D34.3). `None` falls back to
    /// `onto`, so the ordinary caller states it once.
    pub(crate) cut_from: Option<&'a str>,
}

/// Whether a branch value joins the ones a task already records, or replaces them.
#[derive(Clone, Copy)]
pub(crate) enum BranchWrite {
    /// The task is being *moved* to this branch — `task work`, `task start`. A second `branch=`
    /// there is a contradiction (D36.6).
    Set,
    /// This branch is *additional*. A task can legitimately record two, and every reader indexes
    /// both, because deciding a task has landed on the strength of one while the other is live is
    /// how work gets buried.
    Add,
}

/// Put `branch` on the task **and** record where it was cut, in one call. The only way either
/// happens in this crate.
///
/// The pairing is the whole architecture of this area, and every incident in it has been the same
/// two facts written apart: `/task-swarm` wrote `branch=` and no cut point, so once the readers
/// began refusing to act without one every swarm task became undecidable; `jkb task tag set
/// branch=` — which the guide recommended, and which the swarm therefore used — did the same and
/// went on doing it after the swarm was fixed; `task work` re-stamped a cut point for a branch it
/// was not re-cutting. Two independent writes that must agree will eventually not, so there is one
/// write.
///
/// **There is no ordering constraint between the two halves any more.** There used to be — the cut
/// point had to be written before `branch=`, because attribution of an unqualified legacy value
/// asked what other branch the task named, and after the rewrite the answer was always "none". The
/// record is keyed `(repo, branch)`, so nothing about it is attributed from the task's facets and
/// nothing about the order matters.
///
/// The repository the record is keyed under is the task's own `repo=` when it has one, else the
/// checkout we are standing in. Those two agree by construction wherever both exist:
/// [`measure_root_for`] hands back a root only when they match.
///
/// # Errors
/// Returns an error if the name is not usable as a git ref, or a tag or record write fails.
pub(crate) fn record_branch(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    id: ItemId,
    repo_root: Option<&std::path::Path>,
    branch: &str,
    // Named `cut_from`, not `onto`, because it is only the **measurement parent** here — where
    // `branch` forked. `Location` carries both, and they are the same branch for every caller but
    // one: a branch cut from trunk has trunk as its parent and must not record trunk as a land
    // target, or the task reads as merged the moment anything lands (D34.3). Calling both `onto`
    // let a plausible "why are these two fields the same?" simplification reintroduce that.
    cut_from: Option<&str>,
    how: BranchWrite,
) -> jkb_core::Result<Option<crate::base::Missing>> {
    // Refuse a name git would read as an option **before it is stored**. A hostile value entered
    // the store cleanly and then poisoned every later reader — and a reader that refuses is a whole
    // `close-merged` run failing on one bad row. The store is the boundary worth defending;
    // `gitrepo::valid_ref` at the git call is the backstop for values that predate this.
    crate::gitrepo::valid_ref(branch).map_err(|e| jkb_types::Error::Validation(e.to_string()))?;
    let missing = match repo_key_for(conn, id, repo_root)? {
        Some((root, repo)) => {
            crate::base::ensure_recorded(conn, meta, &root, &repo, branch, cut_from)?
        }
        // Not standing in the task's own repository, so nothing here could honestly measure it: a
        // namesake branch in whatever checkout the cwd happens to be is not this task's branch.
        None => Some(crate::base::Missing::NotThisRepo(
            facet_one(&read_tags(conn, id)?, FACET_REPO)
                .cloned()
                .unwrap_or_else(|| "its repo".to_owned()),
        )),
    };
    match how {
        BranchWrite::Set => set_facet(conn, meta, id, FACET_BRANCH, branch)?,
        BranchWrite::Add => tag::apply(conn, meta, id, FACET_BRANCH, branch)?,
    }
    Ok(missing)
}

/// The `(checkout root, repo key)` a branch record for this task may be keyed and measured under,
/// or `None` when there is none.
///
/// The key is the task's `repo=` when it has one — the value every `repo=`-scoped surface uses —
/// and otherwise this checkout's own key, which is the case of a task that has not been told where
/// it lives yet. It is never *guessed* from a checkout that disagrees with the task: `repo_root` is
/// already `None` there (see [`measure_root_for`]), which is what stops a namesake branch in a
/// sibling checkout being recorded as this task's verified cut point.
fn repo_key_for(
    conn: &rusqlite::Connection,
    id: ItemId,
    repo_root: Option<&std::path::Path>,
) -> jkb_core::Result<Option<(PathBuf, String)>> {
    let Some(root) = repo_root else {
        return Ok(None);
    };
    let stated = facet_one(&read_tags(conn, id)?, FACET_REPO).cloned();
    let key = match stated {
        Some(key) => Some(key),
        None => {
            crate::gitrepo::key(root).map_err(|e| jkb_types::Error::Validation(e.to_string()))?
        }
    };
    Ok(key.map(|k| (root.to_path_buf(), k)))
}

/// The repository key this write is *about*, with no checkout involved.
///
/// The caller's `--repo` wins — `task start` is stating where the work is — and otherwise the
/// task's own `repo=`. Distinct from [`repo_key_for`], which additionally demands a checkout to
/// **measure** in; a land target needs no measurement, so demanding one there silently dropped the
/// flag.
fn stated_repo(
    conn: &rusqlite::Connection,
    id: ItemId,
    loc: &Location<'_>,
) -> jkb_core::Result<Option<String>> {
    Ok(match loc.repo {
        Some(repo) => Some(repo.to_owned()),
        None => facet_one(&read_tags(conn, id)?, FACET_REPO).cloned(),
    })
}

/// A task's facet tags as a multi-map, read inside a transaction.
fn read_tags(
    conn: &rusqlite::Connection,
    id: ItemId,
) -> jkb_core::Result<BTreeMap<String, Vec<String>>> {
    let mut tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (facet, value) in tag::applications(conn, id)? {
        tags.entry(facet).or_default().push(value);
    }
    Ok(tags)
}

/// Record which branch `branch` lands on, and make sure that target has a record of its own.
///
/// The **ensure-on-reference** is what makes `close-merged`'s landing path mean anything: a
/// landing event says the work is in `S`, and the next question is whether `S` reached trunk —
/// which needs `S`'s own cut point. Doing it here rather than at the sites that *cut* a batch
/// covers the swarm too, whose integration branch is cut by a prompt.
///
/// `S` is measured against **trunk**, its parent by construction (design D38): a batch branch is
/// cut from trunk, and when it is freshly cut and untouched that measurement is the one provable
/// case — its own tip. The measuring is [`crate::base::fill_for_reference`]'s, which may only fill
/// a gap: this call is about `branch`, and what it can observe about `target` is not evidence
/// about `target`.
///
/// **The stored value is a bare branch name**, canonicalized here rather than at the flags that
/// feed it. `jkb staging ls` keys batches by the short names `gitrepo::branch_refs` returns, so a
/// target stored as the caller spelled it — `origin/integration`, which `rev-parse` resolves
/// perfectly well — matched no row and silently dropped its task out of the one listing behind the
/// branch picker and In Flight. Doing it at the single writer is what stops the next flag that
/// accepts a branch from having to remember; the CLI verbs also ask
/// [`crate::gitrepo::branch_name`], but for the sake of a better sentence, not for the rule.
///
/// # Errors
/// Returns an error if git cannot be run, if `target` is not a branch in this repository, or if a
/// record write fails.
pub(crate) fn record_land_target(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    repo_root: Option<&std::path::Path>,
    repo: &str,
    branch: &str,
    target: Option<&str>,
) -> jkb_core::Result<()> {
    // With no root nothing here can be checked — the caller is recording for a repository it is
    // not standing in — so the word is taken as given, exactly as the cut point is simply not
    // measured there.
    let canonical = match (repo_root, target) {
        (Some(root), Some(target)) => Some(canonical_branch(root, target)?),
        _ => None,
    };
    let target = canonical.as_deref().or(target);
    jkb_core::branch::set_land_target(conn, meta, repo, branch, target)?;
    // A branch is never its own land target in any real flow, and measuring one against trunk here
    // would be a **second** measurement of the same branch with a different parent than
    // `record_branch` just used — two answers to one question, which is the shape this whole area
    // exists to remove. (`--onto <this branch>` is refused as a measurement parent by `rejected`;
    // this makes sure the ensure-on-reference does not quietly record what that refusal declined.)
    if let (Some(root), Some(target)) = (repo_root, target.filter(|t| *t != branch)) {
        let trunk =
            crate::gitrepo::trunk(root).map_err(|e| jkb_types::Error::Validation(e.to_string()))?;
        // `fill_for_reference`, never `ensure_recorded`: the target is not the branch this call is
        // recording, so nothing observed about it here is evidence about *its* identity. The batch
        // a merge queue has just fast-forwarded reads as untouched, and the measuring entry point
        // took that as proof of recycling and replaced the batch's real cut point with its current
        // tip — freezing every task already landed on it, permanently. See `base::fill_for_reference`.
        crate::base::fill_for_reference(conn, meta, root, repo, target, trunk.as_deref())?;
    }
    Ok(())
}

/// Record that `branch`'s work reached `onto`, with `head` the tip that landed — the **one**
/// writer of a landing event.
///
/// It is a separate function for the reason the land target is: both columns hold a *branch name*
/// that a reader looks a record up by, and a spelling no reader resolves is silent. Round 3 fixed
/// exactly that for `land_target` and its commit called [`record_land_target`] "the single
/// writer"; it was not — `jkb task landed` writes `landed_onto` and was never routed through it,
/// so `--onto batch-tag` (a tag on the batch commit, which `is_merged` resolves perfectly well)
/// stored a key `branch_record` misses, holding every task on that branch for ever with no repair
/// verb. Now `jkb_core::branch::record_landing` has exactly one caller outside its own tests.
///
/// Both names are canonicalized, not just the target: `branch` is what the record is keyed by, so
/// storing `origin/grp` there would make the row unreachable from the task that names `grp`.
///
/// # Errors
/// Returns an error if git cannot be run, if either name is not a branch in this repository, or if
/// the write fails.
pub(crate) fn record_landing(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    repo_root: &std::path::Path,
    repo: &str,
    branch: &str,
    onto: &str,
    head: &str,
) -> jkb_core::Result<()> {
    let branch = canonical_branch(repo_root, branch)?;
    let onto = canonical_branch(repo_root, onto)?;
    jkb_core::branch::record_landing(conn, meta, repo, &branch, &onto, head)
}

/// The bare branch name `target` refers to here, or a refusal naming what it actually is.
///
/// The refusal is the point: a value that is not a branch cannot be *stored* in a column readers
/// look branches up by — a land target or either half of a landing event — whichever flag it
/// arrived through, because a miss there is silent.
fn canonical_branch(root: &std::path::Path, target: &str) -> jkb_core::Result<String> {
    let refuse = |what: &str| {
        Err(jkb_types::Error::Validation(format!(
            "`{target}` {what}, so it cannot be recorded as a branch — `jkb staging ls` and \
             `jkb task close-merged` look branches up by name, and a task whose record names \
             something else simply disappears from them"
        ))
        .into())
    };
    match crate::gitrepo::branch_name(root, target)
        .map_err(|e| jkb_types::Error::Validation(e.to_string()))?
    {
        crate::gitrepo::BranchName::Is(name) => Ok(name),
        crate::gitrepo::BranchName::Unknown => refuse("is not a branch in this repository"),
        crate::gitrepo::BranchName::NotABranch => {
            refuse("resolves to a commit but is not a branch (a tag, an object id, or `HEAD`)")
        }
    }
}

/// Which of a task's recorded branches its work is on — the **one** rule, shared by the In Flight
/// row and `jkb task land`.
///
/// Both had already been given the same existence *predicate*, and still disagreed, because they
/// chose which branch to ask about differently: the row preferred a recorded branch that resolves,
/// the command took whichever `tag::applications` returned first (lexicographically smallest). A
/// task carrying a stale `a-gone` beside a live `z-live` therefore got two opposite explanations
/// from the one shared blocker — and `land`'s advice for the branch it picked is to run
/// `jkb task work`, which cuts a *second* branch and detaches the task from its batch.
///
/// A live session wins outright: that is the branch with a checkout on disk, whatever the tags say
/// (D36.2). Otherwise prefer one that exists, and fall back to the first recorded so a task whose
/// branches have all been deleted still names one to report about.
pub(crate) fn work_branch(
    session: Option<&str>,
    branches: &[String],
    refs: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(s) = session {
        return Some(s.to_owned());
    }
    branches
        .iter()
        .find(|b| refs.contains_key(*b))
        .or_else(|| branches.first())
        .cloned()
}

/// A task's live session, if it has one, and the branch its work is on.
pub(crate) struct Work {
    /// The `.jkb/work` session checked out on one of the task's recorded branches.
    pub(crate) session: Option<crate::session::Session>,
    /// The branch [`work_branch`] chose. `None` only when the task records no branch at all.
    pub(crate) branch: Option<String>,
}

/// Everything "where is this task's work" means, resolved **once** — the entry point for a caller
/// that has a task's tags and nothing else.
///
/// The session and the branch are returned together because they are one answer. Handing back only
/// the session left each caller to pick a branch for itself, and `jkb task abandon` picked
/// differently from `jkb staging ls` and `jkb task land`: it took the first `branch=` value, which
/// `tag::applications` orders lexicographically, so a task carrying a stale `a-old` beside a live
/// `z-live` had `--delete-branch` destroy `a-old` and forget its cut point while the row the user
/// clicked Abandon on named `z-live` — the third consumer of a rule two of them already shared.
///
/// The batched surface ([`crate::staging`]) still calls [`work_branch`] directly with the sessions
/// and refs it has already read once for the whole listing — the same rule, not a second one.
///
/// # Errors
/// Returns an error if git cannot be run.
pub(crate) fn work_for(ctx: &RepoCtx, tags: &BTreeMap<String, Vec<String>>) -> Result<Work> {
    let branches = facet_values(tags, FACET_BRANCH);
    // Match by worktree rather than by "the task's branch tag": a task that picked up a second
    // `branch=` still resolves to the session that actually exists on disk (D36.2).
    let session = crate::session::discover(&ctx.root)?
        .into_iter()
        .find(|s| branches.contains(&s.branch));
    let refs = gitrepo::branch_refs(&ctx.root)?;
    let branch = work_branch(session.as_ref().map(|s| s.branch.as_str()), branches, &refs);
    Ok(Work { session, branch })
}

/// The repository a cut point for this task may honestly be measured in, or `None`.
///
/// The **one** answer to that question. It had three — `task base` compared the task's `repo=`
/// against the checkout it was standing in, `task start` compared the repo it was about to record,
/// and `task tag set branch=` never asked at all — and the one that guessed recorded a namesake
/// branch's commit from a sibling checkout as this task's verified cut point. The database is
/// global across repos (D32), so every one of these commands legitimately runs from anywhere; what
/// none of them may do is measure a branch that merely shares a name.
///
/// `intended_repo` is the key the caller is about to record, for `task start`, which is stating
/// where the work is rather than reading it back. Everyone else passes `None` and the task's own
/// `repo=` answers. A task with no `repo=` recorded says nothing either way, so the checkout we are
/// in is accepted — that is the pre-existing behaviour of the only command that asked.
///
/// # Errors
/// Returns an error if the task's tags cannot be read.
pub(crate) fn measure_root_for(
    db: &Db,
    id: ItemId,
    intended_repo: Option<&str>,
) -> Result<Option<PathBuf>> {
    let Some(here) = repo_ctx().ok() else {
        return Ok(None);
    };
    let want = match intended_repo {
        Some(r) => Some(r.to_owned()),
        None => facet_one(&task_tags(db, id)?, FACET_REPO).cloned(),
    };
    Ok(match want {
        Some(want) if want != here.key => None,
        _ => Some(here.root),
    })
}

/// Record where a task is being worked — `task work` and `task start`.
///
/// They had a writer each: `task work` set the facets, `task start` added them with `tag::apply`.
/// A task that saw both — which the guide encourages, since `start` tags from the ambient repo —
/// ended up carrying two `branch=` values, and every reader that collapses the multi-map to one
/// (`close-merged`'s merge probe, the staging rows) then picked whichever came first.
///
/// The branch goes through [`record_branch`], which is what pairs it with its cut point; this
/// function adds the two facets that carry no such obligation.
///
/// **`record_branch` runs last, after the context facets are in place.** [`crate::base::Missing`]
/// names the repository a cut point would have to be measured in, and it reads that from the
/// task's `repo=`. Written afterwards, that read saw the *previous* repository — or, on a task's
/// first `jkb task start --repo <other>`, no repository at all, so the one run that was handed the
/// answer as an argument printed a placeholder. Recording the context first makes
/// `ensure_recorded` name the same repository [`measure_root_for`] compared against, by
/// construction rather than by two call sites agreeing.
pub(crate) fn set_location_facets(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    id: ItemId,
    repo_root: Option<&std::path::Path>,
    loc: &Location<'_>,
) -> jkb_core::Result<Option<crate::base::Missing>> {
    if let Some(onto) = loc.onto {
        crate::gitrepo::valid_ref(onto).map_err(|e| jkb_types::Error::Validation(e.to_string()))?;
    }
    if let Some(repo) = loc.repo {
        set_facet(conn, meta, id, FACET_REPO, repo)?;
    }
    // Why no cut point was recorded, when none was — carried out so the command that reports it
    // states the reason the writer actually had. Deriving it at the reporting site from a proxy
    // ("were we in the right repo?") is how it came to claim a branch did not exist when the real
    // reason was that the caller had named no parent.
    let mut missing = None;
    if let Some(branch) = loc.branch {
        // `loc.onto`, the caller's statement of what this branch was cut from — never the stored
        // facet, which records an earlier moment and may name a batch this branch has nothing to
        // do with. See `base::ensure_recorded`.
        missing = record_branch(
            conn,
            meta,
            id,
            repo_root,
            branch,
            loc.cut_from.or(loc.onto),
            BranchWrite::Set,
        )?;
        // The land target is a fact about the BRANCH, so it is recorded against it rather than
        // against the task — which is what stops two tasks on one branch recording different
        // targets, and what makes "lands on trunk" (an explicit `None`) distinguishable from
        // "never recorded" (no row).
        //
        // Only when the caller states one: `task start` without `--onto` says nothing about where
        // the branch lands, and writing `None` there would clear a target the branch already has.
        //
        // **Keyed by the task's repository, not by the checkout we are standing in.** This used to
        // go through `repo_key_for`, which needs a *root* — so `jkb task start --repo <other>
        // --onto <batch>`, which legitimately runs from anywhere, dropped `--onto` in silence: the
        // task never appeared in `jkb staging ls` and `jkb task land` later failed saying it
        // recorded no land target, with nothing having said why. `record_land_target`'s own
        // no-root arm ("nothing here can be checked, so the word is taken as given") was thereby
        // unreachable. A land target is a name, not a measurement: unlike the cut point there is
        // nothing about it this checkout could honestly establish, so there is nothing to withhold.
        if let Some(onto) = loc.onto {
            match repo_key_for(conn, id, repo_root)? {
                Some((root, repo)) => {
                    record_land_target(conn, meta, Some(&root), &repo, branch, Some(onto))?;
                }
                None => {
                    if let Some(repo) = stated_repo(conn, id, loc)? {
                        record_land_target(conn, meta, None, &repo, branch, Some(onto))?;
                    }
                }
            }
        }
    }
    Ok(missing)
}

/// A land target left in place because the branch is not this task's alone.
pub(crate) struct SharedBranch {
    /// The branch whose land target was kept.
    pub(crate) branch: String,
    /// The uids of the other live tasks recorded on it.
    pub(crate) tasks: Vec<String>,
}

/// Clear the land target of every branch this task records **and no other live task does** —
/// `jkb task abandon`, and a `--onto <trunk>` that says the branch is on no batch.
///
/// Every such branch, not the one a chooser picks: leaving a stale target on a sibling keeps the
/// task rendering as live `implementing` work and keeps that batch classified unmerged and offered
/// as a land target long after it is spent (design D36.3).
///
/// **But a land target is a property of the branch, not of the task**, and `/task-swarm` puts up
/// to four tasks on one group branch. Under the old item-keyed `onto=` facet this command could
/// only reach its own task; keyed `(repo, branch)` it reaches every task on the branch, and
/// abandoning one of a group silently dropped its three siblings out of `jkb staging ls` and out
/// of `jkb task land` — whose advice for a task with no target is `jkb task work`, which cuts a
/// *second* branch and detaches it from the batch. So a shared branch keeps its target and is
/// returned, for the caller to say so.
///
/// # Errors
/// Returns an error if a tag read or a record write fails.
pub(crate) fn clear_land_targets(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    id: ItemId,
    repo_root: Option<&std::path::Path>,
) -> jkb_core::Result<Vec<SharedBranch>> {
    let Some((_, repo)) = repo_key_for(conn, id, repo_root)? else {
        return Ok(Vec::new());
    };
    let mut shared = Vec::new();
    for branch in facet_values(&read_tags(conn, id)?, FACET_BRANCH) {
        let others = live_tasks_on_branch(conn, id, &repo, branch)?;
        if others.is_empty() {
            jkb_core::branch::set_land_target(conn, meta, &repo, branch, None)?;
        } else {
            shared.push(SharedBranch {
                branch: branch.clone(),
                tasks: others,
            });
        }
    }
    Ok(shared)
}

/// The uids of the **other** non-terminal tasks recorded on `branch` in `repo`.
///
/// A task with no `repo=` of its own counts: this command's own repo comes from the checkout when
/// the facet is absent, so an unstated repo is the same repo far more often than not, and of the
/// two ways to be wrong here only clearing a live sibling's target does damage.
fn live_tasks_on_branch(
    conn: &rusqlite::Connection,
    id: ItemId,
    repo: &str,
    branch: &str,
) -> jkb_core::Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.uid, i.status FROM items i
           JOIN tag_applications t
             ON t.item_id = i.id AND t.facet = ?1 AND t.value = ?2
          WHERE i.id <> ?3
            AND NOT EXISTS (
                SELECT 1 FROM tag_applications r
                 WHERE r.item_id = i.id AND r.facet = ?4 AND r.value <> ?5
            )
          ORDER BY i.uid",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![FACET_BRANCH, branch, id.get(), FACET_REPO, repo],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (uid, status) = row?;
        // Terminal statuses filtered here rather than in SQL, so the one definition of "terminal"
        // (`TaskStatus`) stays the only one.
        if !jkb_types::TaskStatus::is_terminal_str(status.as_deref()) {
            out.push(uid);
        }
    }
    Ok(out)
}

/// Does `branch` count as landed *for the purpose of acting on the task*?
///
/// The one place that turns a git fact into a decision, and the only thing the three readers —
/// `close-merged`, `review::others_are_covered`, `review::work_is_in` — may ask.
///
/// The cut point is resolved **here**, from `branch`'s own record, rather than being passed in. It
/// was a parameter, and every caller therefore had to remember to resolve it per branch; two of
/// them forgot in different ways. A reader now supplies the repo and the branch it is asking
/// about, which are the two things it certainly knows.
///
/// Three things are asked, in this order, and the first two exist to keep the third from acting on
/// a record that is not this branch's:
///
/// 1. **Is the record's branch the same instance of the name?** A verified anchor mismatch is
///    positive proof the branch was deleted and recreated, so nothing here may act — see
///    [`crate::base::stale_instance`]. Absent or unverifiable, this declines and the read proceeds
///    exactly as it did before.
/// 2. **Did jkb itself land this branch?** Where it did, the work is in `landed_onto` and the
///    remaining question is whether *that* branch reached trunk — one branch per batch instead of
///    one per task, and the batch is the case where the cut point is provable rather than measured
///    against a moving parent. The event is credited **only** while the branch still points at
///    `landed_head` or is gone: the row is keyed by name, and a name outlives its branch.
/// 3. Otherwise the ordinary inference, unchanged.
///
/// [`gitrepo::is_merged`] answers a narrower, purely factual question: does this branch add
/// anything to trunk? It deliberately falls through its freshly-cut guard when given no base,
/// which keeps merge-commit and rebase detection working for a hand-tagged branch. That is right
/// for a git query and wrong as a licence to act: `merge-tree` on a branch with no commits yields
/// trunk's own tree, so an empty live session read as `Merged` and `close-merged` marked its task
/// done with the work still uncommitted.
///
/// So the policy lives here. **No cut point recorded for this branch means we do not act.** Without
/// one, "cut and not started" and "landed and cleaned up" are indistinguishable — that ambiguity is
/// the entire reason a cut point is stored — and of the two ways to be wrong, a missed auto-close
/// costs one command while a wrong one buries work still in flight (design D34.4).
///
/// # Errors
/// Returns an error if git or the database cannot be read.
pub(crate) fn landed_for_action(
    db: &Db,
    cwd: &std::path::Path,
    repo: &str,
    branch: &str,
    trunk_ref: &str,
    prefer: crate::gitrepo::Prefer,
) -> anyhow::Result<(crate::gitrepo::MergeState, bool)> {
    // Bounded, because a landing event names another branch whose own record is consulted next: a
    // batch landed onto a batch is legitimate, a cycle is not, and following one forever is how a
    // `post-merge` hook stops returning.
    let mut asking = branch.to_owned();
    for _ in 0..8 {
        let record = branch_record(db, repo, &asking)?;
        let Some(record) = record else {
            return landed_with_base(cwd, &asking, trunk_ref, None, prefer);
        };
        if crate::base::stale_instance(cwd, &asking, &record)? {
            // Proof that this record describes a branch that no longer exists. Acting on it is the
            // silent, permanent failure D34.4 forbids, so we hold.
            //
            // The repair is not automatic and this comment used to say it was: nothing here
            // rewrites the row, and the hold lasts until a **writer** runs on that branch — the
            // next `jkb task start` / `jkb task work`, whose supersede arm fires on exactly this,
            // or `jkb task base --forget <branch>` by hand. That arm additionally required the
            // freshly measured cut point to *differ*, so a branch recycled at the same commit
            // (trunk had not moved) could never be repaired at all; see
            // `branch::record_cut_point`'s `SUPERSEDED`.
            return Ok((crate::gitrepo::MergeState::NothingToMerge, false));
        }
        let Some(landing) = record.landed.as_ref().filter(|l| credited(cwd, &asking, l)) else {
            return landed_with_base(cwd, &asking, trunk_ref, record.cut_point.as_deref(), prefer);
        };
        // The event *is* the answer when it names the very branch containment is being asked
        // against. Walking on to ask "and is `S` contained in `S`?" needs `S`'s own cut point,
        // which the review path frequently has no reason to hold — `jkb task review record`
        // passes the reviewed branch here, and a batch measured in a repository with no
        // discoverable trunk records none — and the reader then declined to credit work jkb had
        // itself just grafted onto that branch. Before the event existed this asked about the
        // task's own branch and answered `Merged`, so following the event must not be the thing
        // that makes the answer worse. (This is not the 6.8 state: there the target is a
        // *different* branch from the one asked about, and whether it in turn reached trunk is a
        // question the record genuinely cannot answer.)
        if landing.onto == trunk_ref {
            return Ok((crate::gitrepo::MergeState::Merged, false));
        }
        // FOLLOWING THE HOP MUST NOT MAKE THE ANSWER WORSE. Deleting a batch branch once it has
        // landed is ordinary cleanup — `batch_is_spent`/`release_base_worktree` exist to do it —
        // and asking `is_merged` about a branch that is gone answers `BranchMissing`. That state
        // then travelled back to `close-merged` as if it were about one of the *task's own*
        // branches: it held the task for ever and printed `jkb task tag rm <uid> branch=<name>`,
        // naming the live branch, which is the only record of the work that actually landed. With
        // no landing event at all and the identical git state, the same task closes.
        //
        // So a target that no longer resolves means the event **cannot decide**, and we fall back
        // to the inference for the branch we are standing on — which is what answered before the
        // event existed. That is the conservative direction in both cases: a batch deleted after
        // reaching trunk leaves this branch's commits in trunk (closes, correctly), and one
        // deleted while abandoned leaves them out of it (holds, correctly).
        match crate::gitrepo::branch_ref(cwd, &landing.onto, prefer) {
            Ok(Some(_)) => {}
            // Gone, or git could not say — neither is a licence to attribute the target's state
            // to this branch.
            Ok(None) | Err(_) => {
                return landed_with_base(
                    cwd,
                    &asking,
                    trunk_ref,
                    record.cut_point.as_deref(),
                    prefer,
                )
            }
        }
        asking = landing.onto.clone();
    }
    Ok((crate::gitrepo::MergeState::NothingToMerge, false))
}

/// Whether a recorded landing still describes the branch that carries the name now.
///
/// A land does not move the branch ref — the graft rebases detached and fast-forwards the target —
/// so the branch still points at `landed_head` afterwards, until something re-points it. A gone
/// branch counts too: deletion after landing is ordinary cleanup, and `is_merged` already treats a
/// missing branch as contained on the review side.
///
/// Without this the event re-creates the exact staleness the record is keyed by name to avoid: a
/// namesake recreated after a jkb landing would present its predecessor's, and close a task with
/// nothing on it through the *trusted* path.
fn credited(cwd: &std::path::Path, branch: &str, landing: &jkb_core::branch::Landing) -> bool {
    match crate::gitrepo::branch_ref(cwd, branch, crate::gitrepo::Prefer::Local) {
        Ok(None) => true,
        Ok(Some(reference)) => {
            crate::gitrepo::rev_commit(cwd, &reference).ok().flatten() == Some(landing.head.clone())
        }
        // A git failure is not evidence the event applies. Falling through to the inference path
        // holds the task, which is the safe direction.
        Err(_) => false,
    }
}

/// One branch's record, read through the writer-actor.
///
/// # Errors
/// Returns an error if the query fails.
pub(crate) fn branch_record(
    db: &Db,
    repo: &str,
    branch: &str,
) -> Result<Option<jkb_core::branch::BranchRecord>> {
    let (repo, branch) = (repo.to_owned(), branch.to_owned());
    Ok(db.read(move |conn| jkb_core::branch::get(conn, &repo, &branch))?)
}

/// Every branch record in a repo, keyed by branch — **one** read, for the surfaces that need many.
///
/// The staging view redraws on every database write and holds a row per task, so a lookup per task
/// there is the N+1 shape `repo_tasks` exists to avoid (design risk 2).
///
/// # Errors
/// Returns an error if the query fails.
pub(crate) fn branch_records(
    db: &Db,
    repo: &str,
) -> Result<BTreeMap<String, jkb_core::branch::BranchRecord>> {
    let repo = repo.to_owned();
    Ok(db.read(move |conn| jkb_core::branch::for_repo(conn, &repo))?)
}

/// [`landed_for_action`] for a caller that has already resolved the base for this branch.
///
/// The policy — **no cut point, do not act** — lives here so both entry points share it. Anything
/// that already holds a branch's cut point (the review pass, which reads every record in the repo
/// at once) comes through this door rather than calling `is_merged` directly, which would skip the
/// policy.
///
/// # Errors
/// Returns an error if git cannot be run.
pub(crate) fn landed_with_base(
    cwd: &std::path::Path,
    branch: &str,
    trunk_ref: &str,
    base: Option<&str>,
    prefer: crate::gitrepo::Prefer,
) -> anyhow::Result<(crate::gitrepo::MergeState, bool)> {
    let Some(base) = base else {
        return Ok((crate::gitrepo::MergeState::NothingToMerge, false));
    };
    // A cut point git cannot resolve is **worse than none**, so it is treated as none here.
    //
    // `is_merged` decides "freshly cut, nothing on it yet" by comparing the branch tip against
    // `rev-parse <base>`. When the base does not resolve that right-hand side is `None`, the
    // comparison is simply false, and the guard is skipped rather than applied — so an empty
    // branch falls through to `merge-tree`, which yields trunk's own tree, and reads as merged.
    // A missing base refuses to act; a garbage one closed the task.
    //
    // The check belongs at the policy layer, not in `is_merged`: that function answers a factual
    // question and deliberately falls through when it has no usable base. The routes it once
    // caught are gone — no verb takes a commit id, and `base=` is an ordinary facet nothing reads
    // — but a value can still be unresolvable *here*: a record made in another clone of the
    // repository, or one whose commit has since been garbage-collected. The value's **form** is
    // governed by the `branch_records` CHECK, which is the only thing enforcing it — nothing in
    // Rust checks before the write; only git can answer whether this checkout has the object.
    if !base_is_usable(cwd, Some(base))? {
        return Ok((crate::gitrepo::MergeState::NothingToMerge, false));
    }
    crate::gitrepo::is_merged(cwd, branch, trunk_ref, Some(base), prefer)
}

/// Can this cut point actually be used to decide anything — is one recorded, and does git resolve
/// it here?
///
/// The **one** implementation of that question. [`landed_with_base`] gates on it to decide whether
/// to act, and `close-merged` asks it again to explain *why* it did not — reporting "no usable cut
/// point" separately from "still in flight", since only the first has a remedy. Those two must
/// agree: a second spelling drifts, and then the report explains a decision the policy did not
/// make. The first version of that report had its own inline copy.
///
/// # Errors
/// Returns an error if `git` cannot be run.
pub(crate) fn base_is_usable(cwd: &std::path::Path, base: Option<&str>) -> anyhow::Result<bool> {
    match base {
        None => Ok(false),
        // Two questions, and both must hold. **Form first**: a symbolic revision like `HEAD`
        // resolves in every repository, to whatever that one is pointed at now, so a stored
        // `HEAD` would pass the git check and mean something different every time it is read.
        // Checked here, on the reader's side, so it holds however the value reached the store.
        // The schema now refuses one at the write (`branch_records`' CHECK), but a value recorded
        // before that CHECK existed never passed it.
        //
        // **Then existence**, via `rev_commit` rather than `rev`: plain `rev-parse` parses rather
        // than looks up, so it accepts any 40-character hex string and a fabricated sha read as a
        // usable cut point.
        Some(base) if !crate::base::is_object_id(base) => Ok(false),
        Some(base) => Ok(crate::gitrepo::rev_commit(cwd, base)?.is_some()),
    }
}

/// What the session commands need to know about the repo they are running in.
pub(crate) struct RepoCtx {
    /// The **main** copy's root — where `.jkb/` lives, even when invoked from inside a
    /// session worktree.
    pub(crate) root: PathBuf,
    /// The repo key, matching the `repo=` tag and the `repos/<repo>` namespace (D26/D32).
    pub(crate) key: String,
    /// The trunk ref (`origin/main`, `main`, …), if this repo has a discoverable one.
    pub(crate) trunk: Option<String>,
}

impl RepoCtx {
    /// The trunk's short branch name (`origin/main` → `main`), for comparing against a
    /// checked-out branch and for cutting new branches.
    pub(crate) fn trunk_name(&self) -> Option<&str> {
        self.trunk
            .as_deref()
            .map(|t| t.rsplit('/').next().unwrap_or(t))
    }
}

/// Resolve the repo the current directory belongs to.
pub(crate) fn repo_ctx() -> Result<RepoCtx> {
    let cwd = std::env::current_dir()?;
    let root = gitrepo::main_root(&cwd)?.context(
        "not inside a git repo — a task session is a git worktree, so run this from the repo",
    )?;
    let key = gitrepo::key(&root)?.context("could not determine this repo's name")?;
    let trunk = gitrepo::trunk(&root)?;
    Ok(RepoCtx { root, key, trunk })
}

/// One task, as far as the session commands care.
pub(crate) struct SessionTask {
    pub(crate) uid: String,
    pub(crate) status: String,
    /// Where **this** branch lands, from its own record.
    ///
    /// Per branch rather than per task, which is what it always was in substance: a task carrying
    /// two branches had one `onto=` facet, so whichever branch you looked up got the other's
    /// answer.
    pub(crate) onto: Option<String>,
}

/// This repo's tasks indexed by **every** branch each records.
///
/// `branch=` is the only link from a worktree back to its task — there is deliberately no
/// session state file to fall out of step with git (design D36.2). A task carrying two of
/// them is indexed under both, so a worktree is found whichever one names it.
pub(crate) fn tasks_by_branch(db: &Db, repo_key: &str) -> Result<BTreeMap<String, SessionTask>> {
    // One read for every branch record in the repo, joined in memory — never a lookup per task.
    let records = branch_records(db, repo_key)?;
    let mut out = BTreeMap::new();
    for t in repo_tasks(db, repo_key)? {
        for branch in facet_values(&t.tags, FACET_BRANCH) {
            out.insert(
                branch.clone(),
                SessionTask {
                    uid: t.meta.uid.clone(),
                    status: t.meta.status.clone().unwrap_or_default(),
                    onto: records.get(branch).and_then(|r| r.land_target.clone()),
                },
            );
        }
    }
    Ok(out)
}

/// Every task tagged `repo=<repo_key>`, as a **typed** query.
///
/// Built rather than parsed from `format!("kind:task tag:repo={key}")`: the key is a
/// directory basename, so a repo cloned into `~/dev/my project` produced `tag:repo=my` plus
/// a bare FTS term, which matches nothing. Every staging surface then reported empty — no
/// staging branches, no batch to join, no task recording the branch a review ran on — while
/// the tags themselves were stored correctly and nothing errored. Same reasoning as
/// `review::findings_in`, which was fixed for the namespace and left here.
pub(crate) fn tasks_in_repo(repo_key: &str) -> jkb_core::query::Query {
    use jkb_core::query::{CmpOp, Query, TagPred};
    Query {
        kind: Some("task".to_owned()),
        tags: vec![TagPred {
            facet: FACET_REPO.to_owned(),
            op: CmpOp::Eq,
            value: repo_key.to_owned(),
        }],
        ..Query::default()
    }
}

/// One of this repo's tasks with everything the session and staging reads need.
pub(crate) struct RepoTask {
    pub(crate) meta: jkb_core::item::ItemMeta,
    pub(crate) tags: BTreeMap<String, Vec<String>>,
}

impl RepoTask {
    /// The first line of the task's body — what a human calls the task.
    pub(crate) fn title(&self) -> String {
        crate::output::title_of(&self.meta)
    }
}

/// Every task tagged `repo=<repo_key>`, with its rows and tags, in **one** database read.
///
/// The previous shape issued the query, then a `tag::applications` *and* an `item::get` per
/// task — each a round-trip serialized on the writer thread, over a set that grows with every
/// task ever worked in this repo. That is fine for `task sessions` at three sessions and not
/// fine for a view that redraws on every database write (design D38.2).
pub(crate) fn repo_tasks(db: &Db, repo_key: &str) -> Result<Vec<RepoTask>> {
    let query = tasks_in_repo(repo_key);
    Ok(db.read(move |conn| {
        let ids = query.evaluate(conn)?;
        let metas = item::get_many(conn, &ids)?;
        let tags = tag::applications_for(conn, &ids)?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(meta) = metas.get(&id) else { continue };
            let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (facet, value) in tags.get(&id).cloned().unwrap_or_default() {
                grouped.entry(facet).or_default().push(value);
            }
            out.push(RepoTask {
                meta: meta.clone(),
                tags: grouped,
            });
        }
        Ok(out)
    })?)
}

#[cfg(test)]
mod tests {
    use super::{
        facet_values, record_land_target, set_location_facets, task_tags, Location, FACET_BRANCH,
    };
    use jkb_core::{item::NewItem, Db};

    /// A scratch repository with one commit, a branch, and a tag on the same commit.
    fn repo_with_a_branch_and_a_tag(dir: &std::path::Path) {
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                // The developer's global config sets `core.hooksPath` and signs commits; either
                // would fail this fixture for reasons unrelated to what it tests.
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(ok.status.success(), "git {args:?}: {ok:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("base.txt"), "base").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "base"]);
        run(&["branch", "batch", "main"]);
        run(&["tag", "v1.0", "main"]);
    }

    /// The land-target rule lives at the **writer**, not at the flags that feed it.
    ///
    /// Both CLI verbs ask `gitrepo::branch_name` first, for the sake of a sentence a user can act
    /// on — but that is two sites, and this area's whole history is a rule taught to call sites one
    /// at a time. What makes the third one safe is that nothing can put a value in
    /// `branch_records.land_target` that `gitrepo::branch_refs` does not key on, whatever it was
    /// spelled as: every reader looks a target up by branch name, and a miss there is silent.
    #[test]
    fn a_land_target_is_stored_only_as_a_name_the_readers_key_on() {
        let tmp = tempfile::tempdir().unwrap();
        repo_with_a_branch_and_a_tag(tmp.path());
        // Owned, because the writer-actor's closure must be `'static`.
        let root = tmp.path().to_path_buf();
        let db = Db::open_in_memory().unwrap();

        // A tag resolves perfectly well and is not a branch. Refused rather than stored.
        let at = root.clone();
        let refused = db.write_txn("t", move |conn, meta| {
            record_land_target(conn, meta, Some(&at), "proj", "feat", Some("v1.0"))
        });
        assert!(
            refused.is_err(),
            "a tag was stored as a land target, which drops its task out of `staging ls`"
        );

        // A branch is stored, and under the bare name the listing keys on.
        db.write_txn("t", move |conn, meta| {
            record_land_target(conn, meta, Some(&root), "proj", "feat", Some("batch"))
        })
        .unwrap();
        let stored = db
            .read(|conn| jkb_core::branch::get(conn, "proj", "feat"))
            .unwrap()
            .and_then(|r| r.land_target);
        assert_eq!(stored.as_deref(), Some("batch"));
    }

    /// The policy both entry points share: with no base recorded for this branch we do not act,
    /// whatever git would say. `is_merged` deliberately falls through its freshly-cut guard when
    /// given no base — right for a factual query, and a licence to close a live task if used as
    /// one, since `merge-tree` on a branch with no commits yields trunk's own tree.
    ///
    /// Asserted without a git repo: reaching git at all would mean the refusal did not happen.
    #[test]
    fn no_recorded_base_means_do_not_act() {
        let (state, fell_back) = super::landed_with_base(
            std::path::Path::new("/nonexistent-so-git-would-fail"),
            "task/a",
            "main",
            None,
            crate::gitrepo::Prefer::Local,
        )
        .expect("a missing base must be answered without consulting git");
        assert_eq!(
            state,
            crate::gitrepo::MergeState::NothingToMerge,
            "a branch with no recorded base was treated as landed"
        );
        assert!(!fell_back);
    }

    /// `branch=` is *set*, not added, by this writer: a second value is a contradiction rather
    /// than extra information, and a reader that collapses the multi-map picks one at random
    /// (design D36.6).
    ///
    /// The land target is no longer a facet at all — it is `branch_records.land_target`, keyed by
    /// branch, so "two tasks on one branch disagree about where it lands" is unrepresentable
    /// rather than merely discouraged. Retargeting through the CLI is covered end to end by
    /// `retargeting_a_session_replaces_the_facets_it_records`, which needs a real repository.
    #[test]
    fn the_branch_facet_is_single_valued() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .write_txn("t", |conn, meta| {
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
                for (branch, onto) in [("task/a", "batch/one"), ("task/b", "batch/two")] {
                    set_location_facets(
                        conn,
                        meta,
                        id,
                        Some(std::path::Path::new("/nonexistent-so-nothing-is-measured")),
                        &Location {
                            branch: Some(branch),
                            onto: Some(onto),
                            ..Location::default()
                        },
                    )?;
                }
                Ok(id)
            })
            .unwrap();

        let tags = task_tags(&db, id).unwrap();
        assert_eq!(facet_values(&tags, FACET_BRANCH), ["task/b".to_owned()]);
    }
}
