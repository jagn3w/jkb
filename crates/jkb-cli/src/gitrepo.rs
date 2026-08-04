//! Git queries backing the task/branch lifecycle (design D34.2).
//!
//! Everything here shells out to `git` in a working directory. jkb does not link a git
//! library: the authority on what merged is the user's own git, with their config, remotes
//! and refs — reimplementing that against a second implementation of the object model is
//! how the answer starts disagreeing with `git log`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Branch names tried, in order, when a repo does not say which branch is its trunk.
const DEFAULT_TRUNKS: &[&str] = &["main", "master", "trunk", "develop"];

/// Run `git` in `dir`, returning trimmed stdout. `Ok(None)` when git exits non-zero — the
/// common "this ref does not exist" case, which is a fact rather than a failure.
fn git(dir: &Path, args: &[&str]) -> Result<Option<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}

/// The repository root containing `dir`, or `None` if it is not inside a work tree.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
pub fn root(dir: &Path) -> Result<Option<PathBuf>> {
    Ok(git(dir, &["rev-parse", "--show-toplevel"])?
        .filter(|s| !s.is_empty())
        .map(PathBuf::from))
}

/// The **main** working copy's root, even when `dir` is inside a linked worktree.
///
/// [`root`] answers "which checkout am I in", which is the wrong question for anything that
/// belongs to the repository as a whole: session worktrees and the land lock all live under
/// the main copy's `.jkb/`, or a session that ran `jkb` from inside another session would
/// nest its own `.jkb/work` inside a checkout.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn main_root(dir: &Path) -> Result<Option<PathBuf>> {
    let Some(here) = root(dir)? else {
        return Ok(None);
    };
    // `--path-format=absolute` needs git 2.31; without it the answer would be relative to
    // git's cwd, which is not `here` when `dir` is a subdirectory. Falling back to this
    // checkout is right for the overwhelmingly common case of not being in a worktree.
    let Some(common) = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?
    .filter(|s| !s.is_empty()) else {
        return Ok(Some(here));
    };
    Ok(Some(
        PathBuf::from(common)
            .parent()
            .map_or(here, Path::to_path_buf),
    ))
}

/// The repo's short name — the basename of its root. This is the `repo=` tag value and
/// mirrors the `repos/<repo>` / `tasks/<repo>` namespace key (design D26/D32).
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn key(dir: &Path) -> Result<Option<String>> {
    Ok(root(dir)?.and_then(|r| {
        r.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
    }))
}

/// The currently checked-out branch, or `None` when detached.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn current_branch(dir: &Path) -> Result<Option<String>> {
    Ok(git(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])?.filter(|s| !s.is_empty()))
}

/// The repo's trunk branch: whatever `origin/HEAD` points at, else the first of
/// [`DEFAULT_TRUNKS`] that exists.
///
/// Asking the remote first matters for the "works across a variety of repos" case — a repo
/// whose default is `master` or `develop` must not be silently measured against a `main`
/// that does not exist, because "no such ref" and "nothing merged" would look identical.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn trunk(dir: &Path) -> Result<Option<String>> {
    // `origin/HEAD` -> `origin/main`; take the part after the remote name.
    if let Some(sym) = git(
        dir,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )? {
        if let Some((_, branch)) = sym.split_once('/') {
            if !branch.is_empty() {
                return Ok(Some(format!("origin/{branch}")));
            }
        }
    }
    for candidate in DEFAULT_TRUNKS {
        for reference in [format!("origin/{candidate}"), (*candidate).to_owned()] {
            if git(
                dir,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{reference}^{{commit}}"),
                ],
            )?
            .is_some_and(|s| !s.is_empty())
            {
                return Ok(Some(reference));
            }
        }
    }
    Ok(None)
}

/// The commit `reference` resolves to, if any.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn rev(dir: &Path, reference: &str) -> Result<Option<String>> {
    Ok(git(dir, &["rev-parse", reference])?.filter(|s| !s.is_empty()))
}

/// Run `git` in `dir` for its exit status, returning `(ok, combined output)`. Used by the
/// mutating half of this module (worktrees, branches, rebase), where the failure text is
/// what the user needs to see and a non-zero exit is not "this ref does not exist".
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
fn git_run(dir: &Path, args: &[&str]) -> Result<(bool, String)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text.trim().to_owned()))
}

/// Run `git` in `dir`, turning a non-zero exit into an error carrying git's own message.
fn git_must(dir: &Path, args: &[&str]) -> Result<String> {
    let (ok, text) = git_run(dir, args)?;
    anyhow::ensure!(ok, "git {}: {text}", args.join(" "));
    Ok(text)
}

/// One entry of `git worktree list` — a checkout of this repository.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// The worktree's absolute path.
    pub path: PathBuf,
    /// The branch checked out there, or `None` when detached.
    pub branch: Option<String>,
}

/// Every worktree of the repository containing `dir`, main copy included.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn worktrees(dir: &Path) -> Result<Vec<Worktree>> {
    let Some(text) = git(dir, &["worktree", "list", "--porcelain"])? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            // A blank line separates records, but the last one may have none — flush on the
            // next header instead of relying on the trailing separator.
            if let Some(prev) = path.take() {
                out.push(Worktree {
                    path: prev,
                    branch: branch.take(),
                });
            }
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.trim_start_matches("refs/heads/").to_owned());
        }
    }
    if let Some(p) = path {
        out.push(Worktree { path: p, branch });
    }
    Ok(out)
}

/// The worktree in which `branch` is currently checked out, if any. `git` refuses to check
/// one branch out twice, so this is what decides whether a land can borrow an existing
/// checkout or must make its own.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn worktree_for_branch(dir: &Path, branch: &str) -> Result<Option<PathBuf>> {
    Ok(worktrees(dir)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch))
        .map(|w| w.path))
}

/// Add a worktree at `path` checked out to `branch`, creating that branch from `start` when
/// it does not exist yet.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the worktree.
pub fn worktree_add(dir: &Path, path: &Path, branch: &str, start: &str) -> Result<()> {
    let path_s = path.to_string_lossy().into_owned();
    if has_branch(dir, branch)? {
        git_must(dir, &["worktree", "add", &path_s, branch])?;
    } else {
        git_must(dir, &["worktree", "add", "-b", branch, &path_s, start])?;
    }
    Ok(())
}

/// Remove the worktree at `path` and prune the administrative entry. `force` discards
/// uncommitted changes; without it git refuses a dirty worktree, which is the check the
/// caller wants.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to remove the worktree.
pub fn worktree_remove(dir: &Path, path: &Path, force: bool) -> Result<()> {
    let path_s = path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_s);
    git_must(dir, &args)?;
    let _ = git_run(dir, &["worktree", "prune"])?;
    Ok(())
}

/// Whether a local branch named `branch` exists.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn has_branch(dir: &Path, branch: &str) -> Result<bool> {
    Ok(git(
        dir,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )?
    .is_some())
}

/// Create branch `branch` at `start` if it does not already exist. Returns whether it was
/// created.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the branch.
pub fn create_branch(dir: &Path, branch: &str, start: &str) -> Result<bool> {
    if has_branch(dir, branch)? {
        return Ok(false);
    }
    git_must(dir, &["branch", branch, start])?;
    Ok(true)
}

/// Delete branch `branch`, discarding unmerged commits when `force`.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to delete the branch.
pub fn delete_branch(dir: &Path, branch: &str, force: bool) -> Result<()> {
    git_must(dir, &["branch", if force { "-D" } else { "-d" }, branch])?;
    Ok(())
}

/// Whether the working tree at `dir` has uncommitted changes (staged, unstaged, or
/// untracked). Untracked files count: a session's new module is untracked until it is
/// added, and landing without it would land a branch that does not build.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn is_dirty(dir: &Path) -> Result<bool> {
    Ok(git(dir, &["status", "--porcelain"])?.is_some_and(|s| !s.is_empty()))
}

/// How many commits `branch` has that `onto` does not.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn ahead_count(dir: &Path, onto: &str, branch: &str) -> Result<usize> {
    Ok(
        git(dir, &["rev-list", "--count", &format!("{onto}..{branch}")])?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    )
}

/// Check `branch` out in the working tree at `dir`.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses the switch (a dirty tree, or the
/// branch being checked out in another worktree).
pub fn switch_to(dir: &Path, branch: &str) -> Result<()> {
    git_must(dir, &["switch", branch])?;
    Ok(())
}

/// The outcome of [`graft`]: what happened to the target branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Graft {
    /// `onto` was fast-forwarded to `branch`'s commits, rebased onto its live tip. Carries
    /// the commit the rebase produced, so the caller can point the branch at what actually
    /// landed instead of leaving it on its pre-rebase commits.
    Landed { grafted: String },
    /// The rebase hit a conflict; nothing changed. The branch's author must rebase it.
    Conflict,
}

/// Rebase `branch` onto the live tip of `onto` and fast-forward `onto` to the result, in the
/// working tree at `dir` (which must be checked out to `onto`). Returns the pre-graft tip of
/// `onto` alongside the outcome, so a caller whose gate goes red can roll back to it.
///
/// The rebase runs on a **detached HEAD** at `branch` rather than via `git rebase <onto>
/// <branch>`, because that form checks `branch` out first and git refuses while the session
/// worktree holds it (design D36.4). Detaching does not claim the ref — which also means it
/// does not *move* it: `branch` still points at its pre-rebase commits when this returns.
///
/// # Errors
/// Returns an error if `git` cannot be executed, or if the working tree cannot be put on
/// `onto` to begin with.
pub fn graft(dir: &Path, branch: &str, onto: &str) -> Result<(Graft, String)> {
    git_must(dir, &["switch", onto])?;
    let pre = rev(dir, "HEAD")?.context("target branch has no commits to graft onto")?;

    if !git_run(dir, &["checkout", "--detach", branch])?.0 {
        git_must(dir, &["switch", onto])?;
        return Ok((Graft::Conflict, pre));
    }
    if !git_run(dir, &["rebase", onto])?.0 {
        let _ = git_run(dir, &["rebase", "--abort"])?;
        git_must(dir, &["switch", onto])?;
        return Ok((Graft::Conflict, pre));
    }
    let grafted = rev(dir, "HEAD")?.context("rebase produced no commit")?;
    git_must(dir, &["switch", onto])?;
    if !git_run(dir, &["merge", "--ff-only", &grafted])?.0 {
        reset_hard(dir, &pre)?;
        return Ok((Graft::Conflict, pre));
    }
    Ok((Graft::Landed { grafted }, pre))
}

/// Hard-reset the working tree at `dir` to `reference` — the rollback after a red gate.
///
/// # Errors
/// Returns an error if `git` cannot be executed or the reset fails.
pub fn reset_hard(dir: &Path, reference: &str) -> Result<()> {
    git_must(dir, &["reset", "--hard", reference])?;
    Ok(())
}

/// Whether `reference` resolves to a commit in `dir`.
fn exists(dir: &Path, reference: &str) -> Result<bool> {
    Ok(git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ],
    )?
    .is_some_and(|s| !s.is_empty()))
}

/// Why [`is_merged`] answered the way it did — surfaced so a "not merged" that is really
/// "that branch is gone" does not read as "still in progress".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeState {
    /// The branch contributes nothing to trunk: already merged, by any strategy.
    Merged,
    /// The branch exists and still carries changes trunk does not have.
    Unmerged,
    /// No such branch locally or on the remote. Common after a merged PR whose branch was
    /// deleted — but *also* what a typo looks like, so it is reported, never auto-closed.
    BranchMissing,
    /// The trunk branch could not be determined, so nothing can be compared against it.
    NoTrunk,
    /// The branch is identical to trunk — it has no commits of its own yet. "Re-merging
    /// changes nothing" is true here for the opposite reason to [`MergeState::Merged`]:
    /// the work has not started, not that it landed.
    NothingToMerge,
}

/// Whether `branch` is already merged into `trunk_ref` in the repo at `dir`.
///
/// Uses `git merge-tree --write-tree`, which answers "would merging this change anything?"
/// rather than "are these commits present". That distinction is the whole point: squash
/// merges rewrite the branch into one new commit and rebase merges rewrite every SHA, so
/// commit-identity checks (`--is-ancestor`, `git cherry`) report *not merged* for two of
/// the three strategies GitHub offers. Re-merging is strategy-agnostic — if the result is
/// trunk's own tree, the branch adds nothing however it landed.
///
/// Falls back to `--is-ancestor` on git older than 2.38, which lacks
/// `merge-tree --write-tree`. That fallback misses squash merges, so the caller is told
/// (via `fell_back`) rather than being handed a confident wrong answer.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn is_merged(
    dir: &Path,
    branch: &str,
    trunk_ref: &str,
    base: Option<&str>,
) -> Result<(MergeState, bool)> {
    if !exists(dir, trunk_ref)? {
        return Ok((MergeState::NoTrunk, false));
    }
    // Prefer the remote-tracking branch: after a merged PR the local branch is often stale
    // or gone, while `origin/<branch>` reflects what was actually merged.
    let candidates = [format!("origin/{branch}"), branch.to_owned()];
    let Some(reference) = candidates
        .iter()
        .find(|r| exists(dir, r).unwrap_or(false))
        .cloned()
    else {
        return Ok((MergeState::BranchMissing, false));
    };

    let Some(trunk_tree) = git(dir, &["rev-parse", &format!("{trunk_ref}^{{tree}}")])? else {
        return Ok((MergeState::NoTrunk, false));
    };

    // A branch that has not moved since work started contributes nothing to trunk — but
    // because there is no work yet, not because it landed. Left unguarded, `jkb task start`
    // on a freshly-cut branch closes the task on the very next `close-merged`.
    //
    // Refs alone CANNOT tell the two apart: GitHub's "Rebase and merge" fast-forwards trunk
    // to the branch tip, so a rebase-merged branch is byte-identical to trunk — exactly like
    // a branch just cut from it. The discriminator is the trunk tip recorded when work
    // started: if the branch still sits on it, nothing has been written. A caller with no
    // recorded base skips this and falls through to the merge check, which is what keeps
    // rebase-merge working for a hand-tagged branch.
    if let Some(base) = base {
        let tip = git(dir, &["rev-parse", &reference])?;
        if tip.is_some() && tip == git(dir, &["rev-parse", base])? {
            return Ok((MergeState::NothingToMerge, false));
        }
    }

    let Some(tree) = git(dir, &["merge-tree", "--write-tree", trunk_ref, &reference])? else {
        // Non-zero exit means either a merge conflict (definitely not merged) or a git too
        // old for `--write-tree`. They mean opposite things, so probe rather than guess.
        if supports_merge_tree(dir)? {
            return Ok((MergeState::Unmerged, false));
        }
        let merged = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["merge-base", "--is-ancestor", &reference, trunk_ref])
            .status()
            .with_context(|| "running `git merge-base --is-ancestor`")?
            .success();
        let state = if merged {
            MergeState::Merged
        } else {
            MergeState::Unmerged
        };
        return Ok((state, true));
    };

    // A clean re-merge producing trunk's own tree means the branch contributes nothing.
    let state = if tree.lines().next().unwrap_or_default().trim() == trunk_tree {
        MergeState::Merged
    } else {
        MergeState::Unmerged
    };
    Ok((state, false))
}

/// Whether this git understands `merge-tree --write-tree` (2.38+). Probed by running it
/// against a ref that always exists, so we never guess from a version string.
fn supports_merge_tree(dir: &Path) -> Result<bool> {
    Ok(git(dir, &["merge-tree", "--write-tree", "HEAD", "HEAD"])?.is_some())
}

#[cfg(test)]
mod tests {
    use super::{current_branch, is_merged, key, trunk, MergeState};
    use std::path::Path;
    use std::process::Command;

    /// Build a throwaway repo exercising all three GitHub merge strategies plus an
    /// unmerged control. Each branch touches its own file so the merges do not conflict.
    fn fixture(dir: &Path) {
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                // Neutralize the developer's git config wholesale. This machine sets
                // core.hooksPath globally and signs commits; either would make the fixture
                // fail for reasons that have nothing to do with merge detection.
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
        for branch in ["mergecommit", "squash", "rebase", "unmerged"] {
            run(&["checkout", "-q", "-b", branch, "main"]);
            std::fs::write(dir.join(format!("{branch}.txt")), "one").unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-qm", &format!("{branch} c1")]);
            std::fs::write(dir.join(format!("{branch}.txt")), "one\ntwo").unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-qm", &format!("{branch} c2")]);
            run(&["checkout", "-q", "main"]);
        }
        run(&[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge pull request #1",
            "mergecommit",
        ]);
        run(&["merge", "-q", "--squash", "squash"]);
        run(&["commit", "-qm", "squashed change (#2)"]);
        run(&["checkout", "-q", "rebase"]);
        run(&["rebase", "-q", "main"]);
        run(&["checkout", "-q", "main"]);
        run(&["merge", "-q", "--ff-only", "rebase"]);
    }

    /// The regression that motivates using `merge-tree` at all: commit-identity checks
    /// report "not merged" for squash, which is GitHub's most popular strategy.
    #[test]
    fn merge_detection_survives_every_github_merge_strategy() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fixture(dir);

        for branch in ["mergecommit", "squash", "rebase"] {
            let (state, fell_back) = is_merged(dir, branch, "main", None).unwrap();
            assert_eq!(state, MergeState::Merged, "{branch} should read as merged");
            assert!(
                !fell_back,
                "{branch}: modern git should not need the fallback"
            );
        }
        // The control must NOT read as merged, or the whole thing closes everything.
        assert_eq!(
            is_merged(dir, "unmerged", "main", None).unwrap().0,
            MergeState::Unmerged
        );
        // A branch that never existed is distinguished from one that is simply unmerged.
        assert_eq!(
            is_merged(dir, "no-such-branch", "main", None).unwrap().0,
            MergeState::BranchMissing
        );
        // A freshly-cut branch with no commits contributes nothing to trunk, but because
        // the work has not started -- NOT because it landed. Without this the task closes
        // on the first `close-merged` after `task start`, which is the exact inverse of
        // what the command is for. (Caught by an end-to-end run, not by unit tests.)
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap()
        };
        run(&["branch", "just-created", "main"]);
        assert_eq!(
            is_merged(dir, "just-created", "main", Some("main"))
                .unwrap()
                .0,
            MergeState::NothingToMerge
        );
        // …while a rebase-merged branch, which is ALSO byte-identical to trunk, still reads
        // as merged, because its tip has moved off the base it started from. Refs alone
        // cannot separate these two; the recorded base can.
        assert_eq!(
            is_merged(dir, "rebase", "main", Some("main~1")).unwrap().0,
            MergeState::Merged
        );
        // A trunk that does not exist must not read as "nothing merged".
        assert_eq!(
            is_merged(dir, "squash", "no-such-trunk", None).unwrap().0,
            MergeState::NoTrunk
        );
    }

    #[test]
    fn repo_key_branch_and_trunk_are_discovered() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fixture(dir);
        assert_eq!(current_branch(dir).unwrap().as_deref(), Some("main"));
        // No remote here, so trunk falls through to the local `main`.
        assert_eq!(trunk(dir).unwrap().as_deref(), Some("main"));
        assert!(key(dir).unwrap().is_some());
    }
}
