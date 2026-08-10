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
/// The trunk commit the branch was cut from, recorded at `task start`. Without it a
/// rebase-merged branch — which GitHub fast-forwards, leaving it byte-identical to trunk —
/// cannot be told apart from a branch that was just created and never touched.
pub(crate) const FACET_BASE: &str = "base";

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

/// The single value of a facet that should only ever have one (`onto`, `repo`, `base`).
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
#[derive(Default)]
pub(crate) struct Location<'a> {
    pub(crate) branch: Option<&'a str>,
    pub(crate) repo: Option<&'a str>,
    pub(crate) base: Option<&'a str>,
    pub(crate) onto: Option<&'a str>,
}

/// Record where a task is being worked — the **one** writer of the location facets.
///
/// They had two: `task work` set them, `task start` added them with `tag::apply`. A task that
/// saw both — which the guide encourages, since `start` tags from the ambient repo — ended up
/// carrying two `branch=` values, and every reader that collapses the multi-map to one
/// (`close-merged`'s merge probe, the staging rows) then picked whichever came first.
pub(crate) fn set_location_facets(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    id: ItemId,
    loc: &Location<'_>,
) -> jkb_core::Result<()> {
    // `base=` is qualified by the branch it was recorded for: `<branch>:<sha>`. A base is the
    // trunk tip a PARTICULAR branch was cut from, so it is meaningless applied to any other —
    // and it was being applied to all of them, which disabled the "freshly cut, nothing on it
    // yet" guard for every branch but one and let an empty live branch read as merged.
    // Git forbids `:` in a ref name, so splitting on the first one is unambiguous.
    let qualified;
    let base = match (loc.base, loc.branch) {
        (Some(base), Some(branch)) => {
            qualified = format!("{branch}:{base}");
            Some(qualified.as_str())
        }
        // No branch to attribute it to: store it bare, and let `base_for_branch` decide whether
        // it is safe to use.
        (base, _) => base,
    };
    for (facet, value) in [
        (FACET_BRANCH, loc.branch),
        (FACET_REPO, loc.repo),
        (FACET_BASE, base),
        (FACET_ONTO, loc.onto),
    ] {
        if let Some(value) = value {
            set_facet(conn, meta, id, facet, value)?;
        }
    }
    Ok(())
}

/// The base recorded for `branch` specifically, or `None` if this task records none for it.
///
/// `None` is the conservative answer and callers must treat it as such: without a base,
/// `is_merged` cannot tell a rebase-merged branch from one just cut, and closing a task whose
/// work is still in flight buries it. A missed auto-close costs one command (design D34.4).
///
/// A bare (unqualified) value is a pre-qualification record. It is honoured **only** when the
/// task records exactly one branch — the case it was written for and the case it is correct
/// for. With several branches there is no way to tell which one it describes, and guessing is
/// what this function exists to stop.
pub(crate) fn base_for_branch<'a>(
    bases: &'a [String],
    branch: &str,
    branch_count: usize,
) -> Option<&'a str> {
    for v in bases {
        if let Some((b, sha)) = v.split_once(':') {
            if b == branch {
                return Some(sha);
            }
        }
    }
    if branch_count == 1 {
        return bases.iter().map(String::as_str).find(|v| !v.contains(':'));
    }
    None
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
    use super::base_for_branch;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    /// The regression. A task recording two branches has one base, cut for one of them. Lending
    /// it to the other disabled `is_merged`'s "freshly cut, nothing on it yet" guard for that
    /// branch, so an empty live session read as merged and `close-merged` marked the task done
    /// while the work was still uncommitted.
    #[test]
    fn a_base_is_never_lent_to_a_branch_it_was_not_cut_for() {
        let bases = v(&["task/a:abc123"]);
        assert_eq!(base_for_branch(&bases, "task/a", 2), Some("abc123"));
        assert_eq!(
            base_for_branch(&bases, "task/b", 2),
            None,
            "task/b was handed task/a's base, which disables the empty-branch guard for it"
        );
    }

    /// Each branch gets its own once both are recorded.
    #[test]
    fn each_branch_resolves_its_own_base() {
        let bases = v(&["task/a:aaa", "task/b:bbb"]);
        assert_eq!(base_for_branch(&bases, "task/a", 2), Some("aaa"));
        assert_eq!(base_for_branch(&bases, "task/b", 2), Some("bbb"));
    }

    /// A pre-qualification record is honoured for the single-branch case it was written for —
    /// otherwise every existing task would lose its guard on upgrade.
    #[test]
    fn a_bare_legacy_base_still_applies_to_a_lone_branch() {
        let bases = v(&["deadbeef"]);
        assert_eq!(base_for_branch(&bases, "task/a", 1), Some("deadbeef"));
    }

    /// But not when there are several branches: nothing says which one it describes, and
    /// guessing is what closes a task whose work is still in flight.
    #[test]
    fn a_bare_legacy_base_is_refused_when_several_branches_exist() {
        let bases = v(&["deadbeef"]);
        assert_eq!(
            base_for_branch(&bases, "task/a", 2),
            None,
            "an unattributable legacy base was applied to one of several branches"
        );
    }
}
