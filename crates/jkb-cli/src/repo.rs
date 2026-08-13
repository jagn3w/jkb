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

/// The branch a session's work lands on (design D36.3). Recorded at `task work` so a resumed
/// session — and `task land` itself — target the branch the batch was always going to, not
/// whatever happens to be checked out later.
pub(crate) const FACET_ONTO: &str = "onto";

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

/// The single value of a facet that should only ever have one (`onto`, `repo`).
///
/// **Not the cut point.** That one is per-branch multi-valued and belongs to [`crate::base`],
/// which is the only module allowed to read or write it.
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

/// Remove every value of a facet, leaving the task carrying none.
pub(crate) fn clear_facet(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    id: ItemId,
    facet: &str,
) -> jkb_core::Result<()> {
    for (f, v) in tag::applications(conn, id)? {
        if f == facet {
            tag::remove(conn, meta, id, &f, &v)?;
        }
    }
    Ok(())
}

/// Where a task is being worked. Every field is single-valued by nature: a second `branch=`
/// is a contradiction, not extra information (design D36.6).
///
/// `cut_from` is **not a location facet** and is not stored as one — it is where the caller
/// believes `branch` began, handed to [`crate::base::ensure_recorded`], which decides whether to
/// record it. It rides along because the two writes must happen in one order and neither caller
/// should have to know that (see [`set_location_facets`]).
#[derive(Default)]
pub(crate) struct Location<'a> {
    pub(crate) branch: Option<&'a str>,
    pub(crate) repo: Option<&'a str>,
    pub(crate) onto: Option<&'a str>,
    pub(crate) cut_from: Option<&'a str>,
}

/// Record where a task is being worked — the **one** writer of the location facets.
///
/// They had two: `task work` set them, `task start` added them with `tag::apply`. A task that
/// saw both — which the guide encourages, since `start` tags from the ambient repo — ended up
/// carrying two `branch=` values, and every reader that collapses the multi-map to one
/// (`close-merged`'s merge probe, the staging rows) then picked whichever came first.
///
/// It is also the one place the cut point is recorded, and it does that **first** — before
/// `branch=` is rewritten. [`crate::base::ensure_recorded`] decides whether a pre-qualification
/// value belongs to this branch by asking what other branch the task names, and after the rewrite
/// the answer is always "none", so every stale value would look adoptable. Ordering that
/// correctly is exactly the kind of thing two call sites get wrong one at a time, so neither call
/// site is told about it.
pub(crate) fn set_location_facets(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    id: ItemId,
    loc: &Location<'_>,
) -> jkb_core::Result<()> {
    // Refuse a name git would read as an option **before it is stored**. `task start` records
    // branch facets without touching git at all, so a hostile value entered the store cleanly and
    // then poisoned every later reader — and a reader that refuses is a whole `close-merged` run
    // failing on one bad row. The store is the boundary worth defending; `gitrepo::valid_ref` at
    // the git call is the backstop for values that predate this.
    for name in [loc.branch, loc.onto].into_iter().flatten() {
        crate::gitrepo::valid_ref(name).map_err(|e| jkb_types::Error::Validation(e.to_string()))?;
    }
    if let Some(branch) = loc.branch {
        crate::base::ensure_recorded(conn, meta, id, branch, loc.cut_from)?;
    }
    for (facet, value) in [
        (FACET_BRANCH, loc.branch),
        (FACET_REPO, loc.repo),
        (FACET_ONTO, loc.onto),
    ] {
        if let Some(value) = value {
            set_facet(conn, meta, id, facet, value)?;
        }
    }
    Ok(())
}

/// Does `branch` count as landed *for the purpose of acting on the task*?
///
/// The one place that turns a git fact into a decision, and the only thing the three readers —
/// `close-merged`, `review::others_are_covered`, `review::work_is_in` — may ask.
///
/// The cut point is resolved **here**, from the task's tags, rather than being passed in. It was
/// a parameter, and every caller therefore had to remember to resolve it per branch; two of them
/// forgot in different ways. A reader now supplies the task's tags and the branch it is asking
/// about, which are the two things it certainly knows.
///
/// [`gitrepo::is_merged`] answers a narrower, purely factual question: does this branch add
/// anything to trunk? It deliberately falls through its freshly-cut guard when given no base,
/// which keeps merge-commit and rebase detection working for a hand-tagged branch. That is right
/// for a git query and wrong as a licence to act: `merge-tree` on a branch with no commits yields
/// trunk's own tree, so an empty live session read as `Merged` and `close-merged` marked its task
/// done with the work still uncommitted.
///
/// So the policy lives here. **No base recorded for this branch means we do not act.** Without
/// one, "cut and not started" and "landed and cleaned up" are indistinguishable — that ambiguity
/// is the entire reason `base=` exists — and of the two ways to be wrong, a missed auto-close
/// costs one command while a wrong one buries work still in flight (design D34.4).
///
/// The cost, accepted: a branch tagged by hand with no `base=` no longer auto-closes. `jkb task
/// start` and `jkb task work` both record one, so this only affects facets written by hand.
///
/// # Errors
/// Returns an error if git cannot be run.
pub(crate) fn landed_for_action(
    cwd: &std::path::Path,
    branch: &str,
    trunk_ref: &str,
    tags: &BTreeMap<String, Vec<String>>,
    prefer: crate::gitrepo::Prefer,
) -> anyhow::Result<(crate::gitrepo::MergeState, bool)> {
    landed_with_base(
        cwd,
        branch,
        trunk_ref,
        crate::base::resolve(tags, branch),
        prefer,
    )
}

/// [`landed_for_action`] for a caller that has already resolved the base for this branch.
///
/// The policy — **no base, do not act** — lives here so both entry points share it. A caller
/// holding a resolved base previously had to re-qualify it into a `<branch>:<sha>` string purely
/// so the other entry point could take it apart again, which is the kind of round trip that
/// invites someone to skip the helper and call `is_merged` directly.
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
    // question and deliberately falls through when it has no usable base. Here is also where it
    // catches every route a bad value can arrive by — a mistyped `jkb task base`, a `#base=`
    // quick-add modifier, a hand-edited tag — rather than one of them.
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
        // Checked here, on the reader's side, so it holds however the value reached the store —
        // `base::write` refuses to record one, but a legacy or hand-edited tag never passed it.
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
        for branch in facet_values(&t.tags, FACET_BRANCH) {
            out.insert(branch.clone(), t.session_task());
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
    pub(crate) fn session_task(&self) -> SessionTask {
        SessionTask {
            uid: self.meta.uid.clone(),
            status: self.meta.status.clone().unwrap_or_default(),
            onto: facet_one(&self.tags, FACET_ONTO).cloned(),
        }
    }

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
    use super::{facet_values, set_location_facets, task_tags, Location, FACET_BRANCH, FACET_ONTO};
    use jkb_core::{item::NewItem, Db};

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

    /// The three location facets are *set*, not added: a second `branch=` is a contradiction, and
    /// a reader that collapses the multi-map picks one at random (design D36.6).
    #[test]
    fn the_location_facets_are_single_valued() {
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
        assert_eq!(facet_values(&tags, FACET_ONTO), ["batch/two".to_owned()]);
    }
}
