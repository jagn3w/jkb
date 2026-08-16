//! Git queries backing the task/branch lifecycle (design D34.2).
//!
//! Everything here shells out to `git` in a working directory. jkb does not link a git
//! library: the authority on what merged is the user's own git, with their config, remotes
//! and refs — reimplementing that against a second implementation of the object model is
//! how the answer starts disagreeing with `git log`.

use std::collections::BTreeMap;
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
/// **Whatever comes back resolves here.** `origin/HEAD` is a symbolic ref and can dangle — the
/// remote's default branch renamed, or `origin/main` pruned — and it was the one arm that took its
/// answer on trust while the fallback arm verified. A trunk that does not resolve used to make
/// `ahead_count` quietly answer zero; it now refuses, which turned `jkb staging ls` from a listing
/// into a hard error in exactly that repo. Callers are entitled to assume this ref works, and
/// `base::measure_git` says so in as many words.
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

/// Whether a local branch named `branch` exists.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn has_branch(dir: &Path, branch: &str) -> Result<bool> {
    valid_ref(branch)?;
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

/// Every branch that exists here, **counting a remote-tracking copy**, mapped to a ref that
/// actually **resolves** to it — in one `git` call.
///
/// The batched form of the question [`branch_ref`] asks per branch, with the same
/// [`Prefer::Local`] preference. [`has_branch`] is one subprocess per question and each spawn
/// measured ~11ms here; `staging ls` redraws on every database write, so it resolves this once
/// before its loop.
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

/// Create branch `branch` at `start` if it does not already exist. Returns whether it was
/// created.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the branch.
pub fn create_branch(dir: &Path, branch: &str, start: &str) -> Result<bool> {
    valid_ref(branch)?;
    valid_ref(start)?;
    if has_branch(dir, branch)? {
        return Ok(false);
    }
    git_must(dir, &["branch", branch, start])?;
    Ok(true)
}

/// What a branch's **ref journal** (its reflog) says about the instance of the name that exists
/// here right now.
///
/// A branch name outlives the branch that held it, and nothing in git's object/ref model separates
/// a recycled name from the branch that had it before — the recorded value still resolves, still
/// differs from the new tip, and the freshly-cut guard is skipped. The checkout-local ref journal
/// does separate them, because deleting a branch destroys its log, so the recreated branch's log
/// provably starts fresh with a creation entry of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefJournal {
    /// The creation entry's `new` revision.
    pub anchor_sha: String,
    /// The creation entry's **own** timestamp. Recreating a branch from the same start point
    /// yields the same sha, so this is what separates the two instances.
    ///
    /// Deliberately not read from `git log -g --format=%ct`, which prints the *commit's* time.
    pub anchor_ts: i64,
    /// Whether every entry after the creation one is `commit`-class.
    ///
    /// The retain-license: a branch whose work was merged away looks untouched (its commits are
    /// reachable from the batch), and discarding its real fork point there costs a missed close.
    /// Its journal is creation plus commits — whereas every verb that *re-points* a branch writes
    /// a `Reset`-class entry. Unknown message classes count as **not** commit-class, so a git
    /// whose reflog vocabulary changes can only re-price the missed close, never mint a false
    /// close.
    pub only_commits: bool,
}

/// Read `branch`'s ref journal, or `None` when it cannot judge instance identity.
///
/// `None` in three cases, all of which degrade every consumer to the untouched-tip predicate
/// rather than to a judgement: no log at all (`core.logAllRefUpdates = false`, or a fresh clone
/// that never had one), a log whose oldest surviving entry is **not** a creation entry (expiry
/// removes oldest-first, so a truncated log announces its own truncation), and an unparseable
/// line.
///
/// The journal is read from git's own log file, located through `git rev-parse --git-path`, which
/// is what maps `logs/refs/heads/<branch>` to the **common** directory — so a session worktree
/// reads the same journal the main copy wrote. There is no porcelain for this: the reflog pretty
/// formats expose the new revision (`%H`) and the entry date (`%gd`) but never the `old` value,
/// and `old = zeros` is the only thing that identifies a creation entry without parsing the
/// message text, which varies (`from main`, `from HEAD`, `from main~0`).
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn ref_journal(dir: &Path, branch: &str) -> Result<Option<RefJournal>> {
    valid_ref(branch)?;
    let Some(path) = git(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            &format!("logs/refs/heads/{branch}"),
        ],
    )?
    .filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(created) = lines.next().and_then(parse_reflog_entry) else {
        return Ok(None);
    };
    // Only a creation entry has `old = zeros`. Anything else as the oldest surviving line means
    // the log has been truncated, and a truncated log cannot say which instance created the ref.
    if created.old.bytes().any(|b| b != b'0') {
        return Ok(None);
    }
    // Fails **closed** on a line this cannot read: an unparseable entry is an unknown class, and
    // an unknown class must count against retaining the record, never for it.
    let only_commits = lines.all(|l| {
        parse_reflog_entry(l)
            .is_some_and(|e| e.message.starts_with("commit:") || e.message.starts_with("commit ("))
    });
    Ok(Some(RefJournal {
        anchor_sha: created.new,
        anchor_ts: created.ts,
        only_commits,
    }))
}

/// One parsed reflog line: `<old> <new> <who> <ts> <tz>\t<message>`.
struct ReflogEntry {
    old: String,
    new: String,
    ts: i64,
    message: String,
}

/// Parse one reflog line, or `None` if it is not one.
///
/// The format is git's own and stable: two object ids, an identity, `<unix-ts> <tz>`, a TAB, then
/// the message. The identity contains spaces, so the timestamp is found from the **end** of the
/// pre-TAB half rather than by counting fields forward.
fn parse_reflog_entry(line: &str) -> Option<ReflogEntry> {
    let (head, message) = line.split_once('\t')?;
    let fields: Vec<&str> = head.split_whitespace().collect();
    // `<tz>` last, `<unix-ts>` before it — counted from the END, because the identity in the
    // middle contains spaces and a forward field count would drift with it.
    let ts = fields.get(fields.len().checked_sub(2)?)?.parse().ok()?;
    Some(ReflogEntry {
        old: (*fields.first()?).to_owned(),
        new: (*fields.get(1)?).to_owned(),
        ts,
        message: message.trim().to_owned(),
    })
}

/// Keep `branch`'s ref journal from expiring, by writing an **exact-ref** retention entry in this
/// clone's local config.
///
/// The instance anchor is only as durable as the reflog, so coverage is a condition the
/// implementation establishes rather than assumes. `gc.<pattern>.reflogExpire` accepts an exact
/// ref, which is why no branch naming scheme is needed: retention is written per recorded branch,
/// covering existing names, swarm group branches and `task/<session>` alike.
///
/// Writing `.git/config` locally is on the acceptable side of this project's decoration rule —
/// like `.git/info/exclude` it is local, unpushed and invisible in anything the user commits,
/// and unlike a `refs/jkb/*` scheme it cannot leak via push.
///
/// Failures are **not** propagated: retention is a durability improvement, not a precondition. A
/// clone that never got the entry expires on schedule and the anchor check then declines, which
/// is the same degradation as reflogs being off.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
pub fn retain_reflog(dir: &Path, branch: &str) -> Result<()> {
    valid_ref(branch)?;
    for key in reflog_retention_keys(branch) {
        let _ = git_run(dir, &["config", "--local", &key, "never"])?;
    }
    Ok(())
}

/// Drop the retention entries [`retain_reflog`] wrote, for a branch that is gone.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
pub fn release_reflog(dir: &Path, branch: &str) -> Result<()> {
    valid_ref(branch)?;
    for key in reflog_retention_keys(branch) {
        // Non-zero simply means there was nothing to unset.
        let _ = git_run(dir, &["config", "--local", "--unset-all", &key])?;
    }
    Ok(())
}

/// The two config keys retention is written under, spelled once so writing and unsetting cannot
/// drift.
fn reflog_retention_keys(branch: &str) -> [String; 2] {
    [
        format!("gc.refs/heads/{branch}.reflogExpire"),
        format!("gc.refs/heads/{branch}.reflogExpireUnreachable"),
    ]
}

/// Every branch this clone holds a reflog retention entry for.
///
/// `jkb doctor` reports the ones no branch record claims: the residue of a recorded branch that
/// was never forgotten. Inert config, but config nobody asked for, so it is surfaced rather than
/// left to accumulate silently.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn retained_reflogs(dir: &Path) -> Result<Vec<String>> {
    // `--name-only` so a value containing whitespace cannot be mistaken for part of the key. git
    // lower-cases the variable name but preserves the subsection, which is the branch.
    let Some(text) = git(
        dir,
        &[
            "config",
            "--local",
            "--name-only",
            "--get-regexp",
            r"^gc\.refs/heads/.*\.reflogexpire$",
        ],
    )?
    else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = text
        .lines()
        .filter_map(|l| {
            l.strip_prefix("gc.refs/heads/")
                .and_then(|r| r.strip_suffix(".reflogexpire"))
                .map(str::to_owned)
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
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
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn is_dirty(dir: &Path) -> Result<bool> {
    Ok(git(dir, &["status", "--porcelain"])?.is_some_and(|s| !s.is_empty()))
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

/// The commit where `a` and `b` diverged, or `None` when they share no history.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn merge_base(dir: &Path, a: &str, b: &str) -> Result<Option<String>> {
    valid_ref(a)?;
    valid_ref(b)?;
    Ok(git(dir, &["merge-base", a, b])?.filter(|s| !s.is_empty()))
}

/// Whether `reference` has any commit reachable from **no other branch**, local or remote.
///
/// "Has this branch done anything yet?", asked of git rather than inferred from a reference point
/// the caller named. Naming one is where this kept going wrong: a caller may state a grandparent
/// (`--onto main` for a branch cut from a staging branch cut from main), and every merge-base then
/// lands behind the branch's real origin, so a branch with nothing of its own reads as having
/// something. There is no reference point that is right for every caller, and this question does
/// not need one.
///
/// `branch` is the short name, excluded from the "other branches" set under both `refs/heads/` and
/// `refs/remotes/origin/` — a branch is not evidence of its own work.
///
/// Tags are deliberately not consulted: they mark commits rather than owning them, and a tagged
/// branch has still done the work.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn has_own_commits(dir: &Path, reference: &str, branch: &str) -> Result<bool> {
    valid_ref(reference)?;
    valid_ref(branch)?;
    // Excluded under EVERY remote, not just `origin`: a second remote carrying the same branch
    // name made this answer "no work of its own" for a branch full of it, and the caller then
    // recorded the tip. `*/<branch>` is matched against ref names under `refs/remotes/`, so it
    // covers `upstream/<branch>` and `fork/<branch>` alike.
    //
    // Under-exclusion is the direction that hurts, and it is worth being exact about: if the
    // pattern fails to exclude this branch, `--not` subtracts the branch from itself, no commit
    // is unique, and a branch full of work answers `false` — which is the pass-31 defect above,
    // the caller recording its tip. It is not safe by construction; it is covered by
    // `a_branch_mirrored_on_another_remote_still_has_its_own_commits`, which fails loudly if the
    // exclusion stops matching. (`base::rejected` is the backstop that keeps a wrong answer here
    // from being *stored*, but a wrong answer is still a wrong answer.)
    let remotes = format!("*/{branch}");
    Ok(git(
        dir,
        &[
            "rev-list",
            "--max-count=1",
            reference,
            "--not",
            "--exclude",
            branch,
            "--branches",
            "--exclude",
            &remotes,
            "--remotes",
        ],
    )?
    .is_some_and(|s| !s.is_empty()))
}

/// Whether `a` is an ancestor of `b` — i.e. `b` already contains it.
///
/// `false` when the two are unrelated **and** when git could not answer, which is the same
/// direction: a caller comparing two candidates keeps the one it already had rather than moving to
/// one it could not order.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn is_ancestor(dir: &Path, a: &str, b: &str) -> Result<bool> {
    valid_ref(a)?;
    valid_ref(b)?;
    Ok(git(dir, &["merge-base", "--is-ancestor", a, b])?.is_some())
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
/// have" made a fabricated sha read as a real cut point — after which `is_merged`'s tip-vs-base
/// comparison is merely *false* rather than unknown, its freshly-cut guard is skipped, and an
/// empty branch closes as merged. `--verify --quiet` with `^{commit}` is the question that
/// actually looks the object up, and it is the one every caller wanting existence must ask.
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

/// Which ref [`is_merged`] should ask about when a branch exists both locally and on the
/// remote. They can disagree, and which answer is right depends on the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefer {
    /// The remote-tracking copy, falling back to the local branch. Right for "did this work
    /// ship?": after a merged PR the local branch is often stale or gone, while
    /// `origin/<branch>` reflects what was actually merged.
    Remote,
    /// The local branch, falling back to the remote copy. Right for "is this branch spent?":
    /// a staging branch whose pushed copy merged, but which has since had another task landed
    /// onto it locally, still has commits to give. Asking `origin/` there reported it merged,
    /// hid it from the listing, and sent the next `task work` off to cut a fresh branch while
    /// the landed work sat invisible on the old one.
    Local,
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
/// `prefer` picks which ref answers the question when both a local branch and its
/// remote-tracking copy exist — see [`Prefer`].
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn is_merged(
    dir: &Path,
    branch: &str,
    trunk_ref: &str,
    base: Option<&str>,
    prefer: Prefer,
) -> Result<(MergeState, bool)> {
    valid_ref(branch)?;
    valid_ref(trunk_ref)?;
    if !exists(dir, trunk_ref)? {
        return Ok((MergeState::NoTrunk, false));
    }
    let Some(reference) = branch_ref(dir, branch, prefer)? else {
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

/// The ref that represents `branch` here — the local branch or its remote-tracking copy — or
/// `None` if neither exists. `prefer` decides which is asked for first (see [`Prefer`]).
///
/// The **one** implementation of "does this branch exist, and under what name". [`is_merged`]
/// resolves it to decide between `BranchMissing` and a real comparison; `close-merged` resolves it
/// to decide whether to tell the user a branch is gone. A second spelling of that question got
/// written as a bare `has_branch`, which only looks at `refs/heads/` — so a branch living solely
/// on the remote, the ordinary state after a local branch is deleted post-merge or on a fresh
/// clone, was reported "gone, remove the stale tag" while it still carried unmerged work.
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
/// belonged to a different branch; that is now derived in `base::ensure_recorded` from git alone
/// ("an untouched branch forked at its own tip"), because a flag has to be supplied by every
/// caller — `jkb task start` could not — and is lost by a crash between the git write and the
/// database write.
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
    if has_branch(dir, branch)? {
        return Ok(true);
    }
    let Some(remote) = branch_ref(dir, branch, Prefer::Remote)? else {
        return Ok(false);
    };
    git_must(dir, &["branch", branch, &remote])?;
    Ok(true)
}

/// Whether this git understands `merge-tree --write-tree` (2.38+). Probed by running it
/// against a ref that always exists, so we never guess from a version string.
fn supports_merge_tree(dir: &Path) -> Result<bool> {
    Ok(git(dir, &["merge-tree", "--write-tree", "HEAD", "HEAD"])?.is_some())
}

#[cfg(test)]
mod tests {
    use super::{current_branch, is_merged, key, trunk, MergeState, Prefer};
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
            let (state, fell_back) = is_merged(dir, branch, "main", None, Prefer::Remote).unwrap();
            assert_eq!(state, MergeState::Merged, "{branch} should read as merged");
            assert!(
                !fell_back,
                "{branch}: modern git should not need the fallback"
            );
        }
        // The control must NOT read as merged, or the whole thing closes everything.
        assert_eq!(
            is_merged(dir, "unmerged", "main", None, Prefer::Remote)
                .unwrap()
                .0,
            MergeState::Unmerged
        );
        // A branch that never existed is distinguished from one that is simply unmerged.
        assert_eq!(
            is_merged(dir, "no-such-branch", "main", None, Prefer::Remote)
                .unwrap()
                .0,
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
            is_merged(dir, "just-created", "main", Some("main"), Prefer::Remote)
                .unwrap()
                .0,
            MergeState::NothingToMerge
        );
        // …while a rebase-merged branch, which is ALSO byte-identical to trunk, still reads
        // as merged, because its tip has moved off the base it started from. Refs alone
        // cannot separate these two; the recorded base can.
        assert_eq!(
            is_merged(dir, "rebase", "main", Some("main~1"), Prefer::Remote)
                .unwrap()
                .0,
            MergeState::Merged
        );
        // A trunk that does not exist must not read as "nothing merged".
        assert_eq!(
            is_merged(dir, "squash", "no-such-trunk", None, Prefer::Remote)
                .unwrap()
                .0,
            MergeState::NoTrunk
        );
    }

    /// `Prefer` is the whole point of the parameter, so it needs a repo where the two refs
    /// actually disagree — a branch whose **pushed** copy merged and whose **local** copy has
    /// since moved on. That is a staging branch mid-batch: the PR landed, then another task
    /// was landed onto it locally.
    ///
    /// Asking `origin/<branch>` there reported it merged, which hid it from `staging ls` and
    /// marked it spent, so the next `task work` cut a fresh branch while the locally-landed
    /// work sat invisible on the old one. Every other test here runs against a fixture with
    /// no remote at all, where both orders resolve to the same ref and the parameter could be
    /// deleted without failing anything.
    #[test]
    fn prefer_local_sees_work_the_pushed_copy_does_not_have() {
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
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(ok.status.success(), "git {args:?}: {ok:?}");
        };
        run(tmp.path(), &["init", "-q", "--bare", "remote.git"]);
        run(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);

        // A staging branch with one task's work on it, pushed and squash-merged to trunk.
        run(&dir, &["checkout", "-q", "-b", "batch", "main"]);
        std::fs::write(dir.join("first.txt"), "first").unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-qm", "first task"]);
        run(&dir, &["push", "-q", "origin", "batch"]);
        run(&dir, &["checkout", "-q", "main"]);
        run(&dir, &["merge", "-q", "--squash", "batch"]);
        run(&dir, &["commit", "-qm", "batch (#7)"]);

        // A second task lands onto the LOCAL branch afterwards; nothing is pushed again.
        run(&dir, &["checkout", "-q", "batch"]);
        std::fs::write(dir.join("second.txt"), "second").unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-qm", "second task"]);
        run(&dir, &["checkout", "-q", "main"]);

        assert_eq!(
            is_merged(&dir, "batch", "main", None, Prefer::Remote)
                .unwrap()
                .0,
            MergeState::Merged,
            "the pushed copy really did merge — which is why `close-merged` asks it"
        );
        assert_eq!(
            is_merged(&dir, "batch", "main", None, Prefer::Local)
                .unwrap()
                .0,
            MergeState::Unmerged,
            "but the local branch still has a commit trunk does not, so the batch is live"
        );
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

    /// A branch mirrored on a **non-origin** remote has still done its own work.
    ///
    /// `has_own_commits` is the load-bearing half of the cut-point measurement — a `false` here
    /// makes the caller record the branch tip — and it excluded the branch only under `origin/`.
    /// A second remote carrying the same name (a fork, an `upstream`) therefore made a branch full
    /// of work look untouched. The pattern is `*/<branch>`, which also has to survive a nested
    /// name like `task/x`, so that is what is exercised.
    #[test]
    fn a_branch_mirrored_on_another_remote_still_has_its_own_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fixture(dir);
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
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
        // Its own commit, reachable from no other branch — pointing it at an existing branch
        // would make the answer legitimately `false` and the test vacuous.
        run(&["checkout", "-q", "-b", "task/x", "main"]);
        std::fs::write(dir.join("own.txt"), "own").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "own work"]);
        run(&["checkout", "-q", "main"]);
        // A mirror of the same branch under a remote that is not `origin`.
        run(&[
            "update-ref",
            "refs/remotes/upstream/task/x",
            "refs/heads/task/x",
        ]);
        assert!(
            super::has_own_commits(dir, "task/x", "task/x").unwrap(),
            "a branch mirrored on a non-origin remote was reported as having done nothing, so \
             its caller would record the tip as its cut point"
        );
        // The control: a branch that genuinely has nothing of its own still says so.
        run(&["branch", "empty-one", "main"]);
        assert!(!super::has_own_commits(dir, "empty-one", "empty-one").unwrap());
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
