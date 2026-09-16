//! Where a task is being worked — which repo, which branch — as the facet tags every reader of it
//! shares (design D34.1/D36), and the one check a ref name must pass before it is stored.
//!
//! Moved here from `jkb-cli`'s `repo.rs` when the task-mutate set became typed operations (tasks
//! S6.2): `jkb task tag branch=…` and `jkb task add #branch=…` write through these, and an op served
//! by `jkb serve` cannot call into the CLI binary. `jkb-cli` re-exports them, so its call sites are
//! unchanged and there is still one copy.

use jkb_types::ItemId;
use rusqlite::Connection;

use crate::{tag, Result};

/// Why `name` cannot be handed to `git` as a ref operand, or `None` when it can.
///
/// `git` parses argv positionally, so a ref beginning with `-` becomes an option: `jkb task work
/// <uid> --onto=-D` reached `git branch -D <trunk>` and deleted the repository's trunk branch. Nothing
/// legitimate is lost: `git check-ref-format` rejects a name starting with `-`. Empty is refused for
/// the same reason. The sentence is shared by the store's refusal ([`valid_ref`]) and the CLI's check
/// at every git call, so the two cannot word one rule differently.
#[must_use]
pub fn ref_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("an empty branch or revision name cannot be passed to git".to_owned());
    }
    if name.starts_with('-') {
        return Some(format!(
            "`{name}` cannot be used as a branch or revision: git reads a leading `-` as an option, \
             and no valid ref name begins with one"
        ));
    }
    None
}

/// [`ref_problem`] as a refusal, for a value about to be stored.
///
/// # Errors
/// A validation error naming the problem.
pub fn valid_ref(name: &str) -> Result<()> {
    match ref_problem(name) {
        Some(why) => Err(jkb_types::Error::Validation(why).into()),
        None => Ok(()),
    }
}

/// Every task tagged `repo=<repo_key>`, as a **typed** query — the one definition of "this repo's
/// tasks", which the session ops (`jkb_api::sessions`) and the staging surfaces both ask, so they cannot
/// disagree about which batches are live.
///
/// Built rather than parsed from `format!("kind:task tag:repo={key}")`: the key is a directory
/// basename, so a repo cloned into `~/dev/my project` produced `tag:repo=my` plus a bare FTS term,
/// which matches nothing.
#[must_use]
pub fn tasks_in_repo(repo_key: &str) -> crate::query::Query {
    use crate::query::{CmpOp, Query, TagPred};
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

/// The facet recording which branch a task is being done on, and which repo that branch is
/// in. Plain tags (design D34.1): no migration, and queryable as `tag:branch=<name>`.
pub const FACET_BRANCH: &str = "branch";
pub const FACET_REPO: &str = "repo";

/// Set a facet to exactly one value, removing any others it already had.
///
/// `tag::apply` is additive, which is right for open-ended facets and wrong for the ones that
/// answer "where is this being worked" — a second value there is not extra information, it is
/// a contradiction the readers have to guess their way through.
///
/// # Errors
/// Returns an error if a tag read or write fails.
pub fn set_facet(
    conn: &Connection,
    meta: &crate::WriteMeta,
    id: ItemId,
    facet: &str,
    value: &str,
) -> crate::Result<()> {
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
pub struct Location<'a> {
    /// The branch the work is on.
    pub branch: Option<&'a str>,
    /// The repo key.
    pub repo: Option<&'a str>,
    /// The branch this one lands on. Recorded as a **label on the transition**, not as a
    /// property of the branch kept in agreement with git.
    pub onto: Option<&'a str>,
}

/// Whether a branch value joins the ones a task already records, or replaces them.
#[derive(Clone, Copy)]
pub enum BranchWrite {
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
pub fn record_branch(
    conn: &Connection,
    meta: &crate::WriteMeta,
    id: ItemId,
    branch: &str,
    how: BranchWrite,
) -> crate::Result<()> {
    // Refuse a name git would read as an option **before it is stored**. A hostile value entered
    // the store cleanly and then poisoned every later reader — and a reader that refuses is a whole
    // `close-merged` run failing on one bad row. The store is the boundary worth defending;
    // `jkb-cli`'s `gitrepo::valid_ref` at the git call is the backstop for values that predate this.
    valid_ref(branch)?;
    match how {
        BranchWrite::Set => set_facet(conn, meta, id, FACET_BRANCH, branch)?,
        BranchWrite::Add => tag::apply(conn, meta, id, FACET_BRANCH, branch)?,
    }
    Ok(())
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
pub fn set_location_facets(
    conn: &Connection,
    meta: &crate::WriteMeta,
    id: ItemId,
    loc: &Location<'_>,
) -> crate::Result<()> {
    if let Some(onto) = loc.onto {
        valid_ref(onto)?;
    }
    if let Some(repo) = loc.repo {
        set_facet(conn, meta, id, FACET_REPO, repo)?;
    }
    if let Some(branch) = loc.branch {
        record_branch(conn, meta, id, branch, BranchWrite::Set)?;
    }
    Ok(())
}
