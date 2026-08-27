//! Git queries backing the task/branch lifecycle (design D34.2).
//!
//! Everything here shells out to `git` in a working directory. jkb does not link a git
//! library: the authority on what merged is the user's own git, with their config, remotes
//! and refs — reimplementing that against a second implementation of the object model is
//! how the answer starts disagreeing with `git log`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use jkb_fsm::Fact;

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
/// **Whatever comes back resolves here.** `origin/HEAD` is a symbolic ref and can dangle — the
/// remote's default branch renamed, or `origin/main` pruned — and it was the one arm that took its
/// answer on trust while the fallback arm verified. A trunk that does not resolve used to make
/// `ahead_count` quietly answer zero; it now refuses, which turned `jkb staging ls` from a listing
/// into a hard error in exactly that repo. Callers are entitled to assume this ref works.
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
            let reference = format!("origin/{branch}");
            if !branch.is_empty() && exists(dir, &reference)? {
                return Ok(Some(reference));
            }
        }
    }
    for candidate in DEFAULT_TRUNKS {
        for reference in [format!("origin/{candidate}"), (*candidate).to_owned()] {
            if exists(dir, &reference)? {
                return Ok(Some(reference));
            }
        }
    }
    Ok(None)
}

/// Check a value that will be handed to `git` as a **ref operand**, not as a flag.
///
/// `git` parses argv positionally and has no way to know that `-D` was meant as a branch name, so
/// a user-supplied ref beginning with `-` becomes an option: `jkb task work <uid> --onto=-D`
/// reached `git branch -D <trunk>` and **deleted the repository's trunk branch**. (`clap` blocks
/// the separated form `--onto -D` but passes `--onto=-D` through, which is the ordinary way to
/// give an option a hyphenated value.)
///
/// Nothing legitimate is lost: `git check-ref-format` rejects a ref name starting with `-`, so
/// such a branch cannot exist to be referred to. Empty is refused for the same reason.
///
/// Checked here, in the module every git invocation goes through, rather than at the handful of
/// CLI flags that happen to accept a branch today — `--onto`, `--branch`, `--trunk`, `task base`,
/// `task tag add branch=…`, and whatever is added next. A rule spread over entry points is the
/// defect this file has now been taught twice.
///
/// # Errors
/// Returns an error if `name` cannot be passed to git as an operand.
pub fn valid_ref(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty(),
        "an empty branch or revision name cannot be passed to git"
    );
    anyhow::ensure!(
        !name.starts_with('-'),
        "`{name}` cannot be used as a branch or revision: git reads a leading `-` as an option, \
         and no valid ref name begins with one"
    );
    Ok(())
}

/// The commit `reference` resolves to, if any.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn rev(dir: &Path, reference: &str) -> Result<Option<String>> {
    valid_ref(reference)?;
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
    valid_ref(branch)?;
    Ok(worktrees(dir)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch))
        .map(|w| w.path))
}

/// Drop git's registrations for worktrees whose directories are gone (`git worktree prune`).
///
/// Needed when something outside this process removed a session directory — `git worktree
/// list` keeps reporting it, and its branch stays locked to a checkout that is not there.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn prune_worktrees(dir: &Path) -> Result<()> {
    git_must(dir, &["worktree", "prune"])?;
    Ok(())
}

/// Add a worktree at `path` checked out to `branch`, creating that branch from `start` when
/// it does not exist yet.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the worktree.
pub fn worktree_add(dir: &Path, path: &Path, branch: &str, start: &str) -> Result<()> {
    valid_ref(branch)?;
    valid_ref(start)?;
    let path_s = path.to_string_lossy().into_owned();
    // `ensure_branch`, so a branch that exists only on the remote is checked out rather than
    // re-cut from `start`: the `-b` fallback this replaced had the same blind spot as every other
    // bare `has_branch`, and here it would silently start a session over on top of commits that
    // had already been pushed.
    let created = ensure_branch(dir, branch, start)?;
    if let Err(e) = git_must(dir, &["worktree", "add", &path_s, branch]) {
        // Undo the branch this call created: a failed `git worktree add` must not leave behind a
        // branch the user never asked for, cluttering `git branch` and the staging listing (which
        // derives its rows from branches that exist). Only the branch this call cut is removed —
        // it is seconds old, has no checkout and carries nothing, so there is nothing to lose —
        // and one that was already there is left strictly alone.
        if created {
            let _ = git_run(dir, &["branch", "-D", branch])?;
        }
        return Err(e);
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

/// Whether a local branch named `branch` exists — **three-valued**.
///
/// Asked with `for-each-ref`, not `show-ref --verify --quiet`, and the difference is the whole
/// point. `show-ref` exits non-zero both for *no such ref* and for *this repository cannot be
/// read*, and [`git`] collapses every non-zero exit to `Ok(None)` — so a corrupt `packed-refs`
/// was reported as a **deleted branch**, and `jkb task land` printed "removed its branch",
/// which is the one direction that costs something: it stops the operator looking.
///
/// `for-each-ref` exits **zero with empty output** when nothing matches (measured, along with
/// the 128 a broken repository gives), so a non-zero exit means only that git could not answer.
///
/// The match is exact. A `for-each-ref` pattern also matches deeper refs — `refs/heads/foo`
/// matches `refs/heads/foo/bar`, likewise measured — so comparing on emptiness alone would
/// report a branch that does not exist.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
pub fn has_branch(dir: &Path, branch: &str) -> Result<Fact> {
    valid_ref(branch)?;
    let want = format!("refs/heads/{branch}");
    Ok(Fact::maybe(
        git(dir, &["for-each-ref", "--format=%(refname)", &want])?
            .map(|s| s.lines().any(|l| l == want)),
    ))
}

/// Every branch that exists here, **counting a remote-tracking copy**, mapped to a ref that
/// actually **resolves** to it — in one `git` call.
///
/// The batched form of the question [`branch_ref`] asks per branch, with the same
/// [`Prefer::Local`] preference — for names that **are** branches. It is not simply `branch_ref`
/// in bulk: its keys are the set of branch names, so a tag or a raw object id is absent here and
/// resolvable there, and that difference is [`branch_name`]'s whole reason for existing.
/// [`has_branch`] is one subprocess per question and each spawn measured ~11ms here; `staging ls`
/// redraws on every database write, so it resolves this once before its loop.
///
/// It returns the **ref**, not mere membership, and that is the load-bearing part. A branch living
/// only under `refs/remotes/origin/` is live — the ordinary state after a pruned local ref — but
/// its bare short name resolves to nothing, so every git question asked with that name fails.
/// `rev-list --count` failing read as **zero commits**, so the listing admitted such a batch and
/// then told its tasks they had nothing to land, while `task work` and `task land` went on acting
/// on it. Handing callers the resolved ref means they cannot ask a question the name cannot answer.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn branch_refs(dir: &Path) -> Result<BTreeMap<String, String>> {
    // `%(refname)` decides local-vs-remote and `%(refname:short)` is what git can be handed back.
    // Classifying on the short form instead would read a local branch literally named
    // `origin/x` as a remote copy of `x`.
    let Some(text) = git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)\t%(refname:short)",
            "refs/heads",
            "refs/remotes/origin",
        ],
    )?
    else {
        return Ok(BTreeMap::new());
    };
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let Some((full, short)) = line.split_once('\t') else {
            continue;
        };
        if let Some(name) = full.strip_prefix("refs/heads/") {
            // The local ref wins, whichever order they arrive in — `Prefer::Local`.
            out.insert(name.to_owned(), short.to_owned());
        } else if let Some(name) = full.strip_prefix("refs/remotes/origin/") {
            if name != "HEAD" {
                out.entry(name.to_owned())
                    .or_insert_with(|| short.to_owned());
            }
        }
    }
    Ok(out)
}

/// What a caller-supplied name turns out to be, when the question is "is this a **branch**".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchName {
    /// It names a branch, under the bare short name [`branch_refs`] keys it by. An
    /// `origin/`-qualified spelling of a branch that exists here comes back canonicalized.
    Is(String),
    /// Nothing here goes by that name — a branch the caller may legitimately be about to create.
    Unknown,
    /// It resolves to a commit, but not through a branch: a tag, a raw object id, `HEAD`.
    NotABranch,
}

/// Which **branch** `name` refers to, as [`branch_refs`] keys them — the one answer to "is this a
/// branch, and what is it called".
///
/// Distinct from [`branch_ref`], which maps a branch name to a revision. This maps an arbitrary
/// user-supplied string to a *key*, and that is the direction every consumer of a stored branch
/// name needs: `jkb staging ls` indexes batches by bare short name, so a land target stored as
/// `origin/integration` — which `rev-parse` resolves perfectly well — matched no row and silently
/// dropped its task out of the one listing behind the branch picker and In Flight. A tag was
/// accepted the same way and vanished the task with nothing created and nothing reported.
///
/// The exact key wins over the `origin/`-stripped form, so a local branch genuinely named
/// `origin/x` is itself rather than a remote copy of `x`.
///
/// # Errors
/// Returns an error if `name` cannot be handed to git, or if `git` cannot be executed.
pub fn branch_name(dir: &Path, name: &str) -> Result<BranchName> {
    valid_ref(name)?;
    let refs = branch_refs(dir)?;
    if refs.contains_key(name) {
        return Ok(BranchName::Is(name.to_owned()));
    }
    if let Some(short) = name.strip_prefix("origin/") {
        if refs.contains_key(short) {
            return Ok(BranchName::Is(short.to_owned()));
        }
    }
    // It is not a branch. Whether it is *something* decides which of the two answers this is: a
    // name nothing here uses is a branch waiting to be cut, a name that resolves is a real object
    // the caller has mistaken for a branch, and only the second is worth refusing loudly.
    Ok(if exists(dir, name)? {
        BranchName::NotABranch
    } else {
        BranchName::Unknown
    })
}

/// Create branch `branch` at `start` if it does not already exist. Returns whether it was
/// created.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the branch.
pub fn create_branch(dir: &Path, branch: &str, start: &str) -> Result<bool> {
    valid_ref(branch)?;
    valid_ref(start)?;
    // Proven present, or attempt it: `git branch` refuses an existing branch on its own, so an
    // unestablished answer costs a clear git error rather than a silent wrong turn.
    if has_branch(dir, branch)?.is_yes() {
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
    valid_ref(branch)?;
    git_must(dir, &["branch", if force { "-D" } else { "-d" }, branch])?;
    Ok(())
}

/// Whether the working tree at `dir` has uncommitted changes (staged, unstaged, or
/// untracked). Untracked files count: a session's new module is untracked until it is
/// added, and landing without it would land a branch that does not build.
///
/// **Three-valued, and that is the whole point.** This returned `Result<bool>` and mapped a
/// `git status` that exited non-zero — an unlinked `.git`, a corrupt index, a worktree whose
/// administrative directory has been pruned — to `false`, i.e. *clean*. Every one of the eight
/// callers is a guard standing in front of something destructive (`reset --hard`, an archiving
/// rename, `git switch` across branches, the graft itself), so "git could not answer" was read
/// as "safe to proceed" at all of them.
///
/// That is the defect [`Fact`] exists to prevent, named in its own module docs — *is that
/// checkout clean* is the example given — and `CLAUDE.md` already states the rule this now
/// satisfies: **landing needs `work_dirty.is_no()`; an unreadable checkout refuses.** The
/// machine's [`jkb_core::lifecycle::TaskFacts::work_dirty`] was three-valued all along and its
/// guard asks `is_no()` correctly; the CLI simply never had an `Unknown` to give it, because the
/// collapse happened here, at the boundary, before the type could carry it.
///
/// So callers must state their polarity: a guard says `is_no()` (refuse when unreadable), a
/// report says `is_yes()` or renders all three.
///
/// `dir` must be a **worktree root** — every caller here passes a session checkout, `.jkb/base`
/// or a repo root. Handed a subdirectory this answers [`Fact::Unknown`] rather than reporting on
/// the enclosing repository, which is [`worktree_identity`]'s whole subject: a question asked
/// about one tree must not be answered by another.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all. A `git` that ran and failed is
/// [`Fact::Unknown`], not an error and emphatically not `No`.
pub fn is_dirty(dir: &Path, anchor: &Path) -> Result<Fact> {
    match worktree_identity(dir, anchor)? {
        // Nothing there IS an answer: an absent directory holds no uncommitted work. Spelling it
        // `Unknown` refused, for ever, a landing that used to complete — git keeps listing a
        // worktree whose directory was removed (`prunable`), so `session::discover` still returns
        // it, `git -C <gone>` exits 128, and `land_blocker` then blocked on a checkout that the
        // disposal step three functions later handles as `Disposal::AlreadyGone`. The remedy that
        // refusal printed could not even be run.
        WorktreeIdentity::Absent => Ok(Fact::No),
        // Git answered, but about somewhere else. Nothing it said is about this tree.
        WorktreeIdentity::Foreign => Ok(Fact::Unknown),
        WorktreeIdentity::Own => Ok(Fact::maybe(
            git(dir, &["status", "--porcelain"])?.map(|s| !s.is_empty()),
        )),
    }
}

/// Whether a per-worktree git question asked in `dir` is answered by `dir` itself.
///
/// **Git's discovery walks up**, and that quietly invalidates every question this module asks of
/// a session worktree. With `<repo>/.jkb/work/x/.git` gone, `git -C <repo>/.jkb/work/x status`
/// does not fail — it finds `<repo>/.git` and exits 0, reporting the MAIN checkout's status
/// (empty, since `.jkb/` is in `.git/info/exclude`). So the very state this three-valued rewrite
/// was written for — a worktree unlinked part-way — produced a confident `No` about a different
/// tree, and `dispose` recorded the main repo's HEAD as that session's identity, which the sweep
/// then reads as a different session reusing the name and holds for ever.
///
/// Asked here, once, so no call site has to remember that `-C` is not a fence.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
pub fn worktree_identity(dir: &Path, anchor: &Path) -> Result<WorktreeIdentity> {
    // Absence is established without asking git, and must be: git answers for the enclosing repo
    // when handed a path that is not there, which would make a removed worktree read as `Own`.
    //
    // Asked through `presence::present_under` rather than `Path::exists()`, which answers `false`
    // for ANY stat error — an untraversable parent (EACCES), ELOOP, ENAMETOOLONG — so an
    // unreadable path became `Absent`, and `Absent` is `Fact::No`, which is "proven clean": the
    // spelling this module exists to forbid, feeding `land`'s post-gate check,
    // `abandon_session`'s ensure, `land_dir_for`'s pre-`switch` check and the sweep's identity
    // guard.
    //
    // THE ANCHOR IS THE REPO ROOT, and it is the caller's to supply because only the caller knows
    // it. Anchoring on the path's own parent — which is what the owner-id probe does, having
    // nothing better — is wrong here and was a must-fix: `.jkb/` is in `.git/info/exclude`, so
    // `git clean -xdf` removes `.jkb/work` and every checkout under it while git goes on listing
    // them as prunable. Parent gone plus worktree gone reads as `Unknown`, `is_dirty` says
    // `Unknown`, and `land_blocker` then refuses for ever, printing a `git -C <worktree> status`
    // remedy about a directory that is not there. The repo root is the anchor no ordinary
    // operation removes.
    match crate::presence::present_under(dir, anchor) {
        Fact::No => return Ok(WorktreeIdentity::Absent),
        // Not established that it is there, and not established that it is gone. Nothing this
        // function could ask git afterwards would be about `dir` either.
        Fact::Unknown => return Ok(WorktreeIdentity::Foreign),
        Fact::Yes => {}
    }
    let Some(top) = git(dir, &["rev-parse", "--show-toplevel"])?.filter(|s| !s.is_empty()) else {
        return Ok(WorktreeIdentity::Foreign);
    };
    // Both sides resolved: `/tmp` and `/private/tmp` are the same directory on macOS, and
    // comparing the strings would call every worktree under one of them foreign.
    let (Ok(a), Ok(b)) = (fs::canonicalize(dir), fs::canonicalize(&top)) else {
        return Ok(WorktreeIdentity::Foreign);
    };
    Ok(if a == b {
        WorktreeIdentity::Own
    } else {
        WorktreeIdentity::Foreign
    })
}

/// Whether a directory answers git's questions about **itself** — see [`worktree_identity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeIdentity {
    /// The directory is not there. An established observation, not an unobtainable answer.
    Absent,
    /// It is there and `rev-parse --show-toplevel` is the directory itself.
    Own,
    /// It is there, but git's answers come from an enclosing repository — or it could not say.
    Foreign,
}

/// The commit `dir`'s **own** HEAD is on, or `None` if `dir` cannot answer for itself.
///
/// [`rev`] with `"HEAD"` is the wrong tool for a session worktree: git's discovery walks up, so a
/// worktree whose `.git` file has been removed cheerfully returns the enclosing repository's
/// HEAD. A session's identity is exactly what must not be borrowed from its parent.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
pub fn worktree_head(dir: &Path, anchor: &Path) -> Result<Option<String>> {
    if worktree_identity(dir, anchor)? == WorktreeIdentity::Own {
        rev(dir, "HEAD")
    } else {
        Ok(None)
    }
}

/// How many tracked files are deleted in the working tree, when that is **all** that is wrong.
///
/// `Some(n)` means: `n` tracked files are missing, nothing is staged, nothing is modified and
/// nothing is untracked — a tree somebody (or something) part-way removed, recoverable in full
/// with one `git restore .`. `None` means the dirt is real work.
///
/// This exists because of a real incident: a `git worktree remove` refused part-way through left
/// 152 deletions, and the next landing refused with "it has uncommitted changes — commit them in
/// the session first", which would have committed 62,421 deleted lines. The two states need
/// opposite advice, so they must not read the same. `jkb task land` no longer creates that state
/// — disposal is an atomic rename now — but nothing stops a stale binary, an interrupted `rm`, or
/// a hand-run `git worktree remove` from doing so.
///
/// # Errors
/// Returns an error if `git status` cannot be run here.
pub fn deletions_only(dir: &Path) -> Result<Option<usize>> {
    // Asked as four questions with no whitespace in the answers, rather than by parsing
    // `status --porcelain` — whose leading status column is exactly what a trimmed capture eats,
    // so the first entry of every listing would read as the wrong code.
    let lines = |args: &[&str]| -> Result<Option<usize>> {
        // A `git` that exits non-zero here means the question was not answered. Unknown must not
        // be spelled the same as none: it returns `None` from the caller, which is *no advice*.
        Ok(git(dir, args)?.map(|s| s.lines().filter(|l| !l.trim().is_empty()).count()))
    };
    let (Some(staged), Some(untracked), Some(unstaged), Some(deleted)) = (
        lines(&["diff", "--cached", "--name-only"])?,
        lines(&["ls-files", "--others", "--exclude-standard"])?,
        lines(&["diff", "--name-only"])?,
        lines(&["diff", "--name-only", "--diff-filter=D"])?,
    ) else {
        return Ok(None);
    };
    // A staged change of any kind is deliberate — staging a deletion included — and an untracked
    // file is work `git restore .` would not bring back and must not be advised over.
    if staged > 0 || untracked > 0 || unstaged != deleted {
        return Ok(None);
    }
    Ok((deleted > 0).then_some(deleted))
}

/// How many commits `branch` has that `onto` does not. Both must be refs this repository can
/// resolve — see [`branch_refs`].
///
/// An unresolvable operand is an **error**, not zero. It used to be zero, and zero is a load-
/// bearing answer here: `land_blocker` reads it as "nothing to land" and the listing prints it as
/// the row's commit count. So a remote-only batch, whose bare name resolves to nothing, was
/// reported as having no commits and refused a landing the command then performed. A count that
/// could not be taken must not be indistinguishable from a count of none.
///
/// # Errors
/// Returns an error if `git` cannot be executed, if either revision does not resolve here, or if
/// the count cannot be read.
pub fn ahead_count(dir: &Path, onto: &str, branch: &str) -> Result<usize> {
    valid_ref(onto)?;
    valid_ref(branch)?;
    let range = format!("{onto}..{branch}");
    let count = git(dir, &["rev-list", "--count", &range])?.with_context(|| {
        format!(
            "`git rev-list --count {range}` failed in {} — usually because one of those revisions \
             does not resolve here. A branch that exists only on the remote has to be named by \
             its `origin/` ref.",
            dir.display()
        )
    })?;
    count
        .parse()
        .with_context(|| format!("`git rev-list --count {range}` printed `{count}`"))
}

/// Check `branch` out in the working tree at `dir`.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses the switch (a dirty tree, or the
/// branch being checked out in another worktree).
pub fn switch_to(dir: &Path, branch: &str) -> Result<()> {
    valid_ref(branch)?;
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
    valid_ref(branch)?;
    valid_ref(onto)?;
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
    valid_ref(reference)?;
    git_must(dir, &["reset", "--hard", reference])?;
    Ok(())
}

/// The commit `reference` names, or `None` if this repo does not have one.
///
/// **Not [`rev`].** Plain `rev-parse` is a *parser*: handed a 40-character hex string it exits 0
/// and echoes it back whether or not the object exists, because that is already a well-formed
/// object name. So `rev` answers "is this spellable", and using it to mean "is this a commit I
/// have" once made a fabricated sha read as a real commit. `--verify --quiet` with `^{commit}` is
/// the question that actually looks the object up, and it is the one every caller wanting
/// existence must ask.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn rev_commit(dir: &Path, reference: &str) -> Result<Option<String>> {
    valid_ref(reference)?;
    Ok(git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ],
    )?
    .filter(|s| !s.is_empty()))
}

/// Whether `reference` resolves to a commit in `dir`.
fn exists(dir: &Path, reference: &str) -> Result<bool> {
    Ok(rev_commit(dir, reference)?.is_some())
}

/// Which ref to ask about when a branch exists both locally and on the remote. They can
/// disagree, and which answer is right depends on the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefer {
    /// The remote-tracking copy, falling back to the local branch. Right for "did this work
    /// ship?": after a merged pull request the local branch is often stale or gone, while
    /// `origin/<branch>` reflects what was actually merged.
    Remote,
    /// The local branch, falling back to the remote copy. Right for "is this branch spent?":
    /// a staging branch whose pushed copy merged, but which has since had another task landed
    /// onto it locally, still has commits to give.
    Local,
}

/// The ref that represents `branch` here — the local branch or its remote-tracking copy — or
/// `None` if neither exists. `prefer` decides which is asked for first (see [`Prefer`]).
///
/// The **one** implementation of "given a branch name, what ref may I hand to git for it". It
/// takes a branch name and hands back a *revision* — `close-merged` resolves one to decide whether
/// to tell the user a branch is gone, and `ahead_count` to have something it can count with. A second spelling of that question got written as a bare `has_branch`, which
/// only looks at `refs/heads/` — so a branch living solely on the remote, the ordinary state after
/// a local branch is deleted post-merge or on a fresh clone, was reported "gone, remove the stale
/// tag" while it still carried unmerged work.
///
/// It is deliberately **not** the answer to "is this string a branch". Its argument is assumed to
/// be one already, and its probe is `rev-parse`, which resolves a tag, a raw object id and
/// `origin/<b>` alike — so used as an admission check it accepts values that are not branches at
/// all. That question is [`branch_name`], and the difference between the two is what let an
/// `origin/`-qualified land target be stored under a key `staging ls` could never look up.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn branch_ref(dir: &Path, branch: &str, prefer: Prefer) -> Result<Option<String>> {
    valid_ref(branch)?;
    let candidates = match prefer {
        Prefer::Remote => [format!("origin/{branch}"), branch.to_owned()],
        Prefer::Local => [branch.to_owned(), format!("origin/{branch}")],
    };
    for candidate in &candidates {
        if exists(dir, candidate)? {
            return Ok(Some(candidate.clone()));
        }
    }
    Ok(None)
}

/// Make sure `branch` exists locally, **preferring an existing remote-tracking copy** over
/// `start`.
///
/// The distinction from [`create_branch`] is the whole point, and it is per-caller rather than
/// universal:
///
/// - *Adopt the remote* when the branch is one the caller is **referring to** — an explicit
///   `--onto <batch>`, or a session branch whose commits may already have been pushed. Cutting a
///   namesake from `start` there produces a branch carrying none of the work the name means, which
///   git accepts silently because no local ref exists, and the eventual push is rejected as
///   non-fast-forward.
/// - Use [`create_branch`] when the caller is **making a new branch** and `start` is the point it
///   must begin at. Adopting a same-named remote branch there is the opposite failure: a "fresh"
///   batch named after a task silently becomes some earlier, possibly already-merged batch.
///
/// This lived inside `create_branch` for one commit, which made that function ignore its own
/// `start` argument — a primitive whose name and signature promise something it does not do is a
/// trap for whoever calls it next.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the branch.
/// Returns whether the branch was **created here** rather than already existing.
///
/// Used only by [`worktree_add`], to undo a branch it created when the worktree add then fails.
/// It was once threaded out to the cut-point writer as evidence that a record under this name
/// belonged to a different branch — a flag every caller had to supply, and one a crash between
/// the git write and the database write loses. There is no cut point to protect any more.
pub fn ensure_branch(dir: &Path, branch: &str, start: &str) -> Result<bool> {
    // Composed from [`adopt_remote`] rather than repeating its logic: two functions that both
    // knew how to prefer a remote copy is the overlap that once made `create_branch` silently
    // ignore its own `start` argument.
    if adopt_remote(dir, branch)? {
        return Ok(false);
    }
    create_branch(dir, branch, start)
}

/// Create the local `branch` from its remote-tracking copy when that is the only place it
/// exists. Returns whether the branch is usable locally afterwards.
///
/// Separate from [`ensure_branch`] because it needs **no start point**: it either adopts what is
/// already published or reports that there is nothing to adopt. That matters at the callers that
/// resolve a land target, where computing a start point means resolving trunk — which fails in a
/// repo whose trunk cannot be discovered, and which is not needed at all when the branch exists.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the branch.
pub fn adopt_remote(dir: &Path, branch: &str) -> Result<bool> {
    valid_ref(branch)?;
    if has_branch(dir, branch)?.is_yes() {
        return Ok(true);
    }
    let Some(remote) = branch_ref(dir, branch, Prefer::Remote)? else {
        return Ok(false);
    };
    git_must(dir, &["branch", branch, &remote])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{current_branch, deletions_only, is_dirty, key, trunk};
    use jkb_fsm::Fact;
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
        // `deep/er` exists so `has_branch("deep")` has a prefix to be wrongly matched by.
        run(&["branch", "deep/er"]);
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

    /// A count that could not be taken must not be reported as a count of none.
    ///
    /// Zero is a load-bearing answer: `land_blocker` reads it as "nothing to land" and the In
    /// Flight row prints it. A remote-only branch's bare name resolves to nothing, so `rev-list`
    /// exited non-zero, the failure was mapped to zero, and the row refused a landing the command
    /// then performed. Refusing here is what makes that shape unrepresentable, rather than
    /// something each of the four call sites has to remember to avoid.
    #[test]
    fn an_unmeasurable_commit_count_is_refused_rather_than_reported_as_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fixture(dir);
        assert_eq!(
            super::ahead_count(dir, "main", "unmerged").unwrap(),
            2,
            "a measurable count is still measured"
        );
        let err = super::ahead_count(dir, "main", "no-such-branch")
            .expect_err("an unresolvable revision was answered with a number");
        assert!(
            err.to_string().contains("does not resolve"),
            "the refusal must say what could not be measured: {err}"
        );
    }

    /// `branch_refs` answers with a ref that resolves, not merely with the branch's name.
    ///
    /// A branch living only under `refs/remotes/origin/` is live — the ordinary state after a
    /// pruned local ref — and every git question asked with its bare short name fails. Returning
    /// the resolved ref is what stops a caller asking a question the name cannot answer.
    #[test]
    fn branch_refs_names_a_remote_only_branch_by_a_ref_that_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        let run = |at: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(at)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
            assert!(ok.status.success(), "git {args:?}: {ok:?}");
        };
        run(tmp.path(), &["init", "-q", "--bare", "remote.git"]);
        run(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);
        run(&dir, &["push", "-q", "origin", "unmerged"]);
        run(&dir, &["branch", "-D", "unmerged"]);
        // `main` must exist BOTH locally and on the remote, or nothing competes and the
        // local-over-remote assertion below holds whichever way the preference is written.
        run(&dir, &["push", "-q", "origin", "main"]);

        let refs = super::branch_refs(&dir).unwrap();
        assert_eq!(
            refs.get("unmerged").map(String::as_str),
            Some("origin/unmerged"),
            "a pruned branch was named by something git cannot resolve: {refs:?}"
        );
        assert_eq!(
            refs.get("main").map(String::as_str),
            Some("main"),
            "a local branch must keep its own name, not be replaced by its remote copy: {refs:?}"
        );
        // And the ref it hands back is one the counting question accepts.
        assert_eq!(
            super::ahead_count(&dir, "main", &refs["unmerged"]).unwrap(),
            2
        );
    }

    /// "Is this a branch, and what is it called" — every answer, including the two that are not
    /// branches at all.
    ///
    /// The exact key must win over the `origin/`-stripped form, or a local branch genuinely named
    /// `origin/x` would be read as a remote copy of `x` and its land target recorded against the
    /// wrong branch. And a tag has to be distinguishable from a name nothing uses: the first is a
    /// caller naming the wrong kind of thing, the second is a branch waiting to be cut, and
    /// `jkb task work --onto` may legitimately do the latter.
    #[test]
    fn branch_name_answers_which_branch_a_spelling_refers_to() {
        use super::BranchName::{Is, NotABranch, Unknown};

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        let run = |at: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(at)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
            assert!(ok.status.success(), "git {args:?}: {ok:?}");
        };
        run(tmp.path(), &["init", "-q", "--bare", "remote.git"]);
        run(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);
        run(&dir, &["push", "-q", "origin", "unmerged"]);
        run(&dir, &["branch", "-D", "unmerged"]);
        run(&dir, &["tag", "v1.0", "main"]);
        // A local branch whose name happens to start with the remote's.
        run(&dir, &["branch", "origin/decoy", "main"]);

        let name = |n: &str| super::branch_name(&dir, n).unwrap();
        assert_eq!(name("main"), Is("main".to_owned()));
        assert_eq!(
            name("unmerged"),
            Is("unmerged".to_owned()),
            "a branch that survives only on the remote is still a branch"
        );
        assert_eq!(
            name("origin/unmerged"),
            Is("unmerged".to_owned()),
            "the remote-qualified spelling must canonicalize to the key the listing uses"
        );
        assert_eq!(
            name("origin/decoy"),
            Is("origin/decoy".to_owned()),
            "a local branch named `origin/…` was read as a remote copy of something else"
        );
        assert_eq!(name("v1.0"), NotABranch, "a tag was accepted as a branch");
        assert_eq!(name("HEAD"), NotABranch);
        assert_eq!(name("no-such-thing"), Unknown);
    }

    #[test]
    fn a_part_way_removal_is_told_apart_from_work_in_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fixture(dir);
        assert_eq!(
            deletions_only(dir).unwrap(),
            None,
            "a clean tree owes no advice"
        );

        // The incident's shape: tracked files missing, nothing staged, nothing untracked.
        std::fs::remove_file(dir.join("base.txt")).unwrap();
        assert_eq!(
            deletions_only(dir).unwrap(),
            Some(1),
            "deletions alone are recoverable with `git restore .`"
        );

        // One real edit beside them and it is work again — the advice must flip back.
        std::fs::write(dir.join("new.txt"), "mine").unwrap();
        assert_eq!(
            deletions_only(dir).unwrap(),
            None,
            "an untracked file beside the deletions is work, and must not be restored over"
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

    /// A `git status` that ran and FAILED is `Unknown`, never `No`.
    ///
    /// This is the whole reason `is_dirty` is three-valued. It returned `Result<bool>` and
    /// mapped a non-zero exit to `false` — *clean* — and all eight callers are guards standing
    /// in front of something destructive: `reset --hard`, the archiving rename, `git switch`
    /// across branches, and the land graft itself. A worktree whose `.git` has been unlinked
    /// part-way (the incident `deletions_only` documents, still reachable from a stale binary
    /// or a hand-run `git worktree remove`) answered "clean" to every one of them.
    #[test]
    fn a_git_status_that_could_not_run_is_unknown_not_clean() {
        let t = tempfile::tempdir().expect("tempdir");

        // Not a git repository at all, which is what an unlinked `.git` leaves behind.
        let answer = is_dirty(t.path(), t.path()).expect("git is executable");
        assert_eq!(
            answer,
            Fact::Unknown,
            "a directory git cannot read is not a clean checkout"
        );
        assert!(
            !answer.is_no(),
            "and every guard asks `is_no()`, which must therefore be false here — this is the \
             assertion that fails if `is_dirty` ever collapses back to a bool"
        );
    }

    /// `git clean -xdf` takes `.jkb/work` with it, and a landing must still complete.
    ///
    /// THE ANCHOR IS WHY THIS PASSES. `.jkb/` is in `.git/info/exclude`, so git treats the whole
    /// directory as junk and removes it — while still listing the worktrees under it as
    /// prunable, so `session::discover` returns them and `jkb task land` asks about a checkout
    /// whose parent is gone too. Anchored on the worktree's own PARENT — which is what an owner
    /// id must do, having nothing better — that reads `Unknown`, `is_dirty` reads `Unknown`, and
    /// `land_blocker` refuses for ever while printing a `git -C <worktree> status` remedy about a
    /// directory that is not there. Anchored on the repo root, which no ordinary operation
    /// removes, the absence is established and the landing completes through
    /// `Disposal::AlreadyGone` — the behaviour `a_worktree_directory_that_is_gone_...` above
    /// records as already fixed once.
    #[test]
    fn a_session_whose_whole_work_directory_was_cleaned_is_absent_not_unknown() {
        let t = tempfile::tempdir().expect("tempdir");
        let root = t.path();
        fixture(root);
        let wt = root.join(".jkb/work/x");
        std::fs::create_dir_all(&wt).expect("mkdir");

        // What `git clean -xdf` leaves: the excluded directory and everything under it, gone.
        std::fs::remove_dir_all(root.join(".jkb")).expect("clean");
        assert!(
            !wt.parent().expect("parent").exists(),
            "the premise — the PARENT is gone too, which is the whole difficulty"
        );

        assert_eq!(
            super::worktree_identity(&wt, root).expect("identity"),
            super::WorktreeIdentity::Absent,
            "the repo root is visible, so the absence is established"
        );
        assert_eq!(
            super::is_dirty(&wt, root).expect("is_dirty"),
            Fact::No,
            "and nothing there holds uncommitted work, so the landing is not refused"
        );
    }

    /// `git -C <dir>` is NOT a fence: git's discovery walks up.
    ///
    /// A session worktree whose `.git` file is gone is answered by the enclosing repository —
    /// `status` exits 0 reporting the MAIN checkout — so the state the three-valued rewrite was
    /// written for produced a confident answer about a different tree. `is_dirty` must call that
    /// `Unknown`, and `worktree_head` must refuse to borrow the parent's HEAD as a session's
    /// identity, or the sweep reads it as a different session reusing the name and holds it.
    #[test]
    fn a_worktree_whose_git_link_is_gone_does_not_answer_with_its_parents_state() {
        let t = tempfile::tempdir().expect("tempdir");
        let root = t.path();
        fixture(root);

        // A linked worktree, then its `.git` file removed — a part-way `git worktree remove`.
        let wt = root.join(".jkb/work/x");
        std::fs::create_dir_all(wt.parent().expect("parent")).expect("mkdir");
        let add = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["worktree", "add", "-q", "--detach"])
            .arg(&wt)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git worktree add");
        assert!(add.status.success(), "{add:?}");

        assert_eq!(
            super::worktree_identity(&wt, root).expect("identity"),
            super::WorktreeIdentity::Own,
            "the intact worktree answers for itself"
        );
        let own_head = super::worktree_head(&wt, root).expect("head");
        assert!(own_head.is_some(), "and has a HEAD of its own");

        std::fs::remove_file(wt.join(".git")).expect("unlink .git");

        // The measured half: git still answers, from the enclosing repository.
        let leaked = super::git(&wt, &["rev-parse", "--show-toplevel"]).expect("git");
        assert!(
            leaked.is_some(),
            "git answers a gutted worktree from its parent — if this ever stops being true the \
             guard below is testing nothing"
        );

        assert_eq!(
            super::worktree_identity(&wt, root).expect("identity"),
            super::WorktreeIdentity::Foreign,
            "an answer sourced from the enclosing repo is not about this tree"
        );
        assert_eq!(
            super::is_dirty(&wt, root).expect("is_dirty"),
            Fact::Unknown,
            "so dirtiness is unestablished, not the parent's cleanliness"
        );
        assert_eq!(
            super::worktree_head(&wt, root).expect("head"),
            None,
            "and a session never borrows its parent's HEAD as its own identity"
        );
    }

    /// An absent directory is an ESTABLISHED observation, not an unobtainable answer.
    ///
    /// Git keeps listing a worktree whose directory was removed (`prunable`), so `session::
    /// discover` still returns it. Spelling that `Unknown` made `land_blocker` refuse for ever a
    /// landing that used to complete through `Disposal::AlreadyGone` — and the remedy it printed
    /// (`git -C <gone dir> status`) could not be run.
    #[test]
    fn a_worktree_directory_that_is_gone_is_not_dirty_rather_than_unknown() {
        let t = tempfile::tempdir().expect("tempdir");
        let gone = t.path().join("removed");
        assert_eq!(
            super::worktree_identity(&gone, t.path()).expect("identity"),
            super::WorktreeIdentity::Absent
        );
        assert_eq!(
            super::is_dirty(&gone, t.path()).expect("is_dirty"),
            Fact::No,
            "nothing there holds no uncommitted work"
        );
    }

    /// A path that cannot be stat'd is not a PROVEN absence.
    ///
    /// `Path::exists()` answers `false` for any stat error, so an untraversable parent made
    /// `worktree_identity` say `Absent`, which `is_dirty` spells `Fact::No` — "proven clean", the
    /// one answer this module exists to stop manufacturing. It then feeds `land`'s post-gate
    /// check, `abandon_session`'s ensure, `land_dir_for`'s pre-`switch` check and the sweep's
    /// identity guard, none of which can tell it from a real observation.
    #[test]
    fn a_path_that_cannot_be_stat_ed_is_not_a_proven_absence() {
        use std::os::unix::fs::PermissionsExt;

        let t = tempfile::tempdir().expect("tempdir");
        let parent = t.path().join("locked");
        let wt = parent.join("wt");
        std::fs::create_dir_all(&wt).expect("mkdir");

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");
        // Everything is measured while the parent is untraversable, then it is restored so the
        // tempdir can be cleaned up whatever the assertions do.
        let stat = std::fs::metadata(&wt).err().map(|e| e.kind());
        // Anchored on the tempdir root, which is plainly VISIBLE — so the only thing that
        // fails is the stat of `wt` itself, and the answer must still not be a proven absence.
        let identity = super::worktree_identity(&wt, t.path());
        let dirty = super::is_dirty(&wt, t.path());
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).expect("restore");

        // Asserted, not assumed: as root the chmod does not bite, and without this the test would
        // pass having observed an ordinary readable directory.
        assert_eq!(
            stat,
            Some(std::io::ErrorKind::PermissionDenied),
            "the premise — stat must actually fail, or this test is about nothing"
        );
        assert_ne!(
            identity.expect("identity"),
            super::WorktreeIdentity::Absent,
            "nobody established that it is gone; the stat never came back"
        );
        assert_eq!(
            dirty.expect("is_dirty"),
            Fact::Unknown,
            "and an unreadable path must never read as PROVEN clean"
        );
    }

    /// A branch question git could not answer is `Unknown`, never "no such branch".
    ///
    /// `show-ref --verify --quiet` exits non-zero for BOTH "no such ref" and "this repository
    /// cannot be read", and `git` collapses every non-zero exit — so an unreadable repo reported
    /// a deleted branch and `land` printed "removed its branch".
    #[test]
    fn an_unreadable_repo_does_not_report_a_branch_as_deleted() {
        let t = tempfile::tempdir().expect("tempdir");
        let root = t.path();
        fixture(root);

        assert_eq!(
            super::has_branch(root, "squash").expect("has_branch"),
            Fact::Yes
        );
        assert_eq!(
            super::has_branch(root, "no-such-branch").expect("has_branch"),
            Fact::No,
            "a branch that really is absent is PROVEN absent"
        );
        // A `for-each-ref` pattern also matches deeper refs, so an exact comparison is required.
        assert_eq!(
            super::has_branch(root, "deep").expect("has_branch"),
            Fact::No,
            "`refs/heads/deep` must not be reported present by `refs/heads/deep/er`"
        );

        // Now make the repository unreadable.
        std::fs::write(root.join(".git/HEAD"), "not a ref\n").expect("corrupt HEAD");
        std::fs::remove_dir_all(root.join(".git/refs")).expect("remove refs");
        let _ = std::fs::remove_file(root.join(".git/packed-refs"));
        assert_eq!(
            super::has_branch(root, "squash").expect("has_branch"),
            Fact::Unknown,
            "git could not answer, which is not the same as the branch being gone"
        );
    }
}
