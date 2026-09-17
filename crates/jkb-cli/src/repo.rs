//! The repo + facet vocabulary the session, staging and review surfaces share.
//!
//! Where a task is being worked — which repo, which branch — is recorded as plain facet tags
//! (design D34.1/D36), and several modules need to read and write them. (Where that branch
//! lands is a **label on the task's transition history**, written by the same call that records
//! the transition — see `jkb_core::transition::land_target`.) They lived in `main.rs` beside the
//! clap surface, which made
//! `staging.rs` and
//! `review.rs` depend *inward* on the binary root while every other module depends sideways,
//! and forced eleven items to be `pub(crate)` in an already very large file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::gitrepo;

pub(crate) use jkb_core::location::FACET_BRANCH;

/// The values recorded for one facet.
pub(crate) fn facet_values<'a>(
    tags: &'a BTreeMap<String, Vec<String>>,
    facet: &str,
) -> &'a [String] {
    tags.get(facet).map_or(&[], Vec::as_slice)
}

/// The single value of a facet that should only ever have one (`repo`).
///
/// Where a branch lands is not a facet at all — it is a label on the task's transition history —
/// and where it was cut is not stored anywhere, so there is nothing here to collapse.
pub(crate) fn facet_one<'a>(
    tags: &'a BTreeMap<String, Vec<String>>,
    facet: &str,
) -> Option<&'a String> {
    facet_values(tags, facet).first()
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

#[cfg(test)]
mod tests {
    use super::{facet_values, FACET_BRANCH};
    use jkb_core::item::NewItem;
    use jkb_core::location::{set_location_facets, Location};
    use jkb_core::Db;

    /// `branch=` is *set*, not added, by this writer: a second value is a contradiction rather
    /// than extra information, and a reader that collapses the multi-map picks one at random
    /// (design D36.6).
    ///
    /// The land target is not a facet at all — it is a label on the task's transition history,
    /// so "two tasks on one branch disagree about where it lands" is two entries with timestamps
    /// rather than one row silently keeping whichever wrote last.
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

        let mut tags = std::collections::BTreeMap::<String, Vec<String>>::new();
        for (facet, value) in db
            .read(move |conn| jkb_core::tag::applications(conn, id))
            .unwrap()
        {
            tags.entry(facet).or_default().push(value);
        }
        assert_eq!(facet_values(&tags, FACET_BRANCH), ["task/b".to_owned()]);
    }
}
