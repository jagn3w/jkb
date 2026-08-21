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
use jkb_core::{item, tag, Db};
use jkb_types::ItemId;

use crate::gitrepo;

/// The facet recording which branch a task is being done on, and which repo that branch is
/// in. Plain tags (design D34.1): no migration, and queryable as `tag:branch=<name>`.
pub(crate) const FACET_BRANCH: &str = "branch";
pub(crate) const FACET_REPO: &str = "repo";

/// The branch a session's work lands on used to be a facet here (`onto=`). It is now
/// a label on the task's transition history: it is a statement about a moment, so two tasks told
/// different targets are two entries with timestamps rather than one row silently keeping
/// whichever wrote last. See `jkb_core::transition::land_target`.
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
/// Where a branch lands is not a facet at all — it is a label on the task's transition history —
/// and where it was cut is not stored anywhere, so there is nothing here to collapse.
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
/// There is deliberately **no cut point** here, and no longer anywhere. It existed so that a
/// branch adding nothing to trunk could be told apart from one that never started — a question
/// only the commit-graph inference had to ask, and one a merged pull request answers directly.
#[derive(Default)]
pub(crate) struct Location<'a> {
    pub(crate) branch: Option<&'a str>,
    pub(crate) repo: Option<&'a str>,
    /// The branch this one lands on. Recorded as a **label on the transition**, not as a
    /// property of the branch kept in agreement with git.
    pub(crate) onto: Option<&'a str>,
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

/// Put `branch` on the task.
///
/// This used to do two things — write the facet **and** measure and store the branch's cut point
/// — because those two facts written apart is what every incident in this area had in common. The
/// cut point is gone: it existed only to make the commit-graph inference answerable, and that
/// inference has been replaced by a pull request lookup. So one write is all that is left, and the
/// pairing rule it enforced has nothing to pair.
///
/// # Errors
/// Returns an error if the name is not usable as a git ref, or the tag write fails.
pub(crate) fn record_branch(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    id: ItemId,
    branch: &str,
    how: BranchWrite,
) -> jkb_core::Result<()> {
    // Refuse a name git would read as an option **before it is stored**. A hostile value entered
    // the store cleanly and then poisoned every later reader — and a reader that refuses is a whole
    // `close-merged` run failing on one bad row. The store is the boundary worth defending;
    // `gitrepo::valid_ref` at the git call is the backstop for values that predate this.
    crate::gitrepo::valid_ref(branch).map_err(|e| jkb_types::Error::Validation(e.to_string()))?;
    match how {
        BranchWrite::Set => set_facet(conn, meta, id, FACET_BRANCH, branch)?,
        BranchWrite::Add => tag::apply(conn, meta, id, FACET_BRANCH, branch)?,
    }
    Ok(())
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

/// Record where a task is being worked — `task work` and `task start`.
///
/// They had a writer each: `task work` set the facets, `task start` added them with `tag::apply`.
/// A task that saw both — which the guide encourages, since `start` tags from the ambient repo —
/// ended up carrying two `branch=` values, and every reader that collapses the multi-map to one
/// then picked whichever came first.
///
/// The **land target is not written here.** It used to be a `branch_records` column that had to
/// be kept in agreement with git; it is now a label on the `start` transition, written by the
/// same call that records the transition, so there is nothing to keep in agreement and nothing
/// for two tasks on one branch to disagree about — they are two entries, with timestamps.
///
/// # Errors
/// Returns an error if a name is not usable as a git ref, or a tag write fails.
pub(crate) fn set_location_facets(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    id: ItemId,
    loc: &Location<'_>,
) -> jkb_core::Result<()> {
    if let Some(onto) = loc.onto {
        crate::gitrepo::valid_ref(onto).map_err(|e| jkb_types::Error::Validation(e.to_string()))?;
    }
    if let Some(repo) = loc.repo {
        set_facet(conn, meta, id, FACET_REPO, repo)?;
    }
    if let Some(branch) = loc.branch {
        record_branch(conn, meta, id, branch, BranchWrite::Set)?;
    }
    Ok(())
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
    let mut out = BTreeMap::new();
    for t in repo_tasks(db, repo_key)? {
        let id = t.meta.id;
        let onto = db.read(move |conn| jkb_core::transition::land_target(conn, id))?;
        for branch in facet_values(&t.tags, FACET_BRANCH) {
            out.insert(
                branch.clone(),
                SessionTask {
                    uid: t.meta.uid.clone(),
                    status: t.meta.status.clone().unwrap_or_default(),
                    onto: onto.clone(),
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
    use super::{facet_values, set_location_facets, task_tags, Location, FACET_BRANCH};
    use jkb_core::item::NewItem;
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

        let tags = task_tags(&db, id).unwrap();
        assert_eq!(facet_values(&tags, FACET_BRANCH), ["task/b".to_owned()]);
    }
}
