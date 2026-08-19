//! Parallel task sessions: the layout, naming, landing lock and gate a hand-driven task
//! session needs (design D36).
//!
//! A session is a git worktree holding one task's work on one branch, so N terminals can
//! drive N tasks without sharing a checkout. Everything a session needs to be found again
//! lives in git (the worktree and its branch) and the KB (the task's `branch=` tag and claim,
//! and that branch's own record) — there is no session state file to drift.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jkb_core::{ns, Db};
use serde_json::{json, Value};

use crate::gitrepo;

/// Repo-local directory holding every session artifact. Git-ignored.
const JKB_DIR: &str = ".jkb";
/// Where session worktrees live: `<repo>/.jkb/work/<session>`.
const WORK_DIR: &str = "work";
/// A checkout of the land target, made only when it is checked out nowhere else.
const BASE_DIR: &str = "base";
/// Serializes landing, so two sessions cannot graft onto the same branch at once.
const LOCK_FILE: &str = "land.lock";
/// The branch prefix for a session's own branch: `task/<session>`.
pub const BRANCH_PREFIX: &str = "task/";
/// Longest session name minted from a task uid — long enough to stay readable in
/// `git branch` and a directory listing, short enough not to bury the path.
const MAX_NAME: usize = 40;

/// `<repo>/.jkb`.
#[must_use]
pub fn jkb_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(JKB_DIR)
}

/// `<repo>/.jkb/work/<session>` — where a session's checkout lives.
#[must_use]
pub fn worktree_path(repo_root: &Path, name: &str) -> PathBuf {
    jkb_dir(repo_root).join(WORK_DIR).join(name)
}

/// `<repo>/.jkb/base` — the land target's checkout, when it has none of its own.
#[must_use]
pub fn base_worktree(repo_root: &Path) -> PathBuf {
    jkb_dir(repo_root).join(BASE_DIR)
}

/// The branch a session works on.
#[must_use]
pub fn branch_for(name: &str) -> String {
    format!("{BRANCH_PREFIX}{name}")
}

/// The session name encoded in a session branch, if this is one.
#[must_use]
pub fn name_from_branch(branch: &str) -> Option<&str> {
    branch.strip_prefix(BRANCH_PREFIX)
}

/// Mint a session name from a task uid: readable, path-safe, and stable for the same task.
///
/// The trailing hash a managed uid carries (`…-18c7815f3e2bad28`) is dropped — it makes
/// every directory and branch name unreadable and buys nothing here, because `taken`
/// disambiguates the rare collision. `taken` reports whether a candidate name is already
/// used by a *different* session.
pub fn mint_name(uid: &str, taken: impl Fn(&str) -> bool) -> String {
    // `task:<slug>` for a managed task, `file://<path>#<local_id>` for a file-backed one.
    let tail = uid
        .rsplit_once('#')
        .map_or_else(|| uid.rsplit(':').next().unwrap_or(uid), |(_, frag)| frag);
    let mut slug: String = tail
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Drop the uid's disambiguating hash suffix (12+ hex chars), not a real word.
    if let Some((head, last)) = slug.rsplit_once('-') {
        if last.len() >= 12 && last.chars().all(|c| c.is_ascii_hexdigit()) && !head.is_empty() {
            slug = head.to_owned();
        }
    }
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let base = slug
        .trim_matches('-')
        .chars()
        .take(MAX_NAME)
        .collect::<String>();
    let base = base.trim_matches('-');
    let base = if base.is_empty() { "task" } else { base };

    if !taken(base) {
        return base.to_owned();
    }
    (2..1000)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| format!("{base}-{}", std::process::id()))
}

/// A live session: a worktree, the branch checked out in it, and the task it is for.
#[derive(Debug, Clone)]
pub struct Session {
    /// The session name (also the worktree's directory name).
    pub name: String,
    /// The session's checkout.
    pub worktree: PathBuf,
    /// The branch checked out there (`task/<name>`).
    pub branch: String,
}

/// Make git ignore `.jkb/` in this repo, locally.
///
/// Session worktrees live inside the repo, so without this the very first session makes the
/// working tree dirty — and `land` refuses a dirty target, because a red gate rolls it back.
/// The exclusion goes in `.git/info/exclude` rather than `.gitignore`: it is local, needs no
/// commit, and jkb has no business editing a tracked file in someone else's repo.
///
/// This **appends** and never rewrites. Read the file to decide whether the entry is already
/// there, by all means — but a read that failed must not become an empty `current` that a
/// whole-file write then makes true, destroying ignore rules jkb did not author. Appending
/// makes the worst case a duplicate line, which git ignores.
///
/// # Errors
/// Returns an error if the entry cannot be appended.
pub fn ensure_excluded(repo_root: &Path) -> Result<()> {
    let exclude = repo_root.join(".git").join("info").join("exclude");
    let Some(parent) = exclude.parent() else {
        return Ok(());
    };
    // A linked worktree's `.git` is a file, so this only applies in the main copy — which is
    // the only place `repo_root` ever points (see `gitrepo::main_root`).
    if !parent.exists() {
        return Ok(());
    }
    // Bytes, not `read_to_string`: an exclude file in some other encoding is still a file
    // whose contents matter, and it must not be a reason to fail or to overwrite.
    let current = fs::read(&exclude).unwrap_or_default();
    let text = String::from_utf8_lossy(&current);
    let entry = format!("/{JKB_DIR}/");
    if text.lines().any(|l| l.trim() == entry) {
        return Ok(());
    }
    let sep = if current.is_empty() || current.ends_with(b"\n") {
        ""
    } else {
        "\n"
    };
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)
        .with_context(|| format!("opening {}", exclude.display()))?;
    write!(file, "{sep}# jkb task sessions (git worktrees)\n{entry}\n")
        .with_context(|| format!("appending to {}", exclude.display()))?;
    Ok(())
}

/// Resolve symlinks for comparison, falling back to the path as given when it does not
/// exist yet. Git reports canonical paths (`/private/var/…` on macOS, where `/tmp` and
/// `/var` are symlinks), so comparing them to a path we composed ourselves needs this or
/// every session looks like it belongs to a different repo.
#[must_use]
pub fn same_path(a: &Path, b: &Path) -> bool {
    canonical(a) == canonical(b)
}

/// Whether `inner` is `outer` or lives beneath it, comparing canonical paths.
///
/// Canonical because a session worktree under `/var/...` is reached as `/private/var/...` on
/// macOS, so a raw `starts_with` answers "no" for a directory you are standing in.
#[must_use]
pub fn is_within(inner: &Path, outer: &Path) -> bool {
    canonical(inner).starts_with(canonical(outer))
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Every session worktree of the repo at `repo_root`, discovered from git rather than from a
/// state file — a worktree removed by hand simply stops being a session.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn discover(repo_root: &Path) -> Result<Vec<Session>> {
    let work_root = canonical(&jkb_dir(repo_root).join(WORK_DIR));
    let mut out = Vec::new();
    for wt in gitrepo::worktrees(repo_root)? {
        let Some(branch) = wt.branch.clone() else {
            continue;
        };
        let Some(name) = name_from_branch(&branch) else {
            continue;
        };
        // Only worktrees under `.jkb/work` are sessions; a `task/*` branch someone checked
        // out elsewhere is their business.
        if !canonical(&wt.path).starts_with(&work_root) {
            continue;
        }
        out.push(Session {
            name: name.to_owned(),
            worktree: wt.path,
            branch,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// An exclusive hold on this repo's landing, released on drop.
///
/// Landing must be serial: two sessions grafting at once would each run the gate against a
/// tree the other is about to change, which is exactly the "green alone, red together" case
/// the gate exists to catch (design D36.4).
pub struct LandLock {
    path: PathBuf,
}

impl LandLock {
    /// Take the lock, or fail naming the pid that holds it. A lock whose holder no longer
    /// exists is stale — a crashed land must not wedge the repo — and is taken over.
    ///
    /// # Errors
    /// Returns an error if another **live** land holds the lock, or if the lock file cannot
    /// be created.
    pub fn acquire(repo_root: &Path) -> Result<Self> {
        let dir = jkb_dir(repo_root);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(LOCK_FILE);
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = write!(f, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = fs::read_to_string(&path).unwrap_or_default();
                    let alive = holder
                        .trim()
                        .parse::<u32>()
                        .is_ok_and(crate::owner::pid_alive);
                    anyhow::ensure!(
                        !alive,
                        "another `jkb task land` is running here (pid {}) — landing is serial \
                         so its gate result stays meaningful; wait for it to finish",
                        holder.trim()
                    );
                    // Remove the stale lock only while it is still the one we judged. Two lands
                    // seeing the same dead pid both reached this line, and an unconditional
                    // unlink let the second delete the lock the FIRST had just created — leaving
                    // both believing they held it, which is the one thing this file exists to
                    // prevent. Re-reading narrows that to the instant between this check and the
                    // unlink; it cannot close it without a real file lock, and the honest note is
                    // that landing is serialised against ordinary use, not against a race.
                    if fs::read_to_string(&path).unwrap_or_default() == holder {
                        let _ = fs::remove_file(&path);
                    }
                }
                Err(e) => return Err(e).context("taking the land lock"),
            }
        }
        anyhow::bail!("could not take the land lock at {}", path.display())
    }
}

impl Drop for LandLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// The gate (design D36.5)
// ---------------------------------------------------------------------------

/// Commands probed, in order, when a repo has no stored gate. The first whose file exists is
/// used *and stored*, so the guess is made once and is inspectable afterwards.
const GATE_CANDIDATES: &[(&str, &str)] = &[
    ("scripts/check.sh", "./scripts/check.sh"),
    ("scripts/test.sh", "./scripts/test.sh"),
    ("Makefile", "make test"),
];

/// The namespace a repo's settings hang off (design D32: `repos/<repo>`).
fn repo_ns(repo_key: &str) -> String {
    format!("repos/{repo_key}")
}

/// The gate command remembered for `repo_key`, if any.
///
/// # Errors
/// Returns an error if the database read fails.
pub fn stored_gate(db: &Db, repo_key: &str) -> Result<Option<String>> {
    let path = repo_ns(repo_key);
    Ok(db.read(move |conn| {
        let Some(id) = ns::get(conn, &path)? else {
            return Ok(None);
        };
        Ok(ns::get_metadata(conn, id)?
            .and_then(|m| m.get("gate").and_then(Value::as_str).map(str::to_owned)))
    })?)
}

/// Remember `cmd` as `repo_key`'s gate (or forget it when `cmd` is [`None`]).
///
/// The write **merges** into the namespace's existing metadata rather than replacing it: a
/// repo namespace may already carry a type or file-sync bookkeeping, and clobbering that to
/// store a build command would be a silent data loss.
///
/// # Errors
/// Returns an error if the database write fails.
pub fn set_gate(db: &Db, repo_key: &str, cmd: Option<&str>) -> Result<()> {
    let path = repo_ns(repo_key);
    let cmd = cmd.map(str::to_owned);
    db.write_txn("cli", move |conn, meta| {
        let id = ns::ensure(conn, &path)?;
        let mut metadata = ns::get_metadata(conn, id)?.unwrap_or_else(|| json!({}));
        if !metadata.is_object() {
            metadata = json!({});
        }
        if let Some(obj) = metadata.as_object_mut() {
            match &cmd {
                Some(c) => {
                    obj.insert("gate".to_owned(), Value::String(c.clone()));
                }
                None => {
                    obj.remove("gate");
                }
            }
        }
        ns::set_metadata(conn, meta, id, &metadata)
    })?;
    Ok(())
}

/// The first [`GATE_CANDIDATES`] entry whose marker file exists in `repo_root`.
#[must_use]
pub fn autodetect_gate(repo_root: &Path) -> Option<String> {
    GATE_CANDIDATES
        .iter()
        .find(|(marker, _)| repo_root.join(marker).exists())
        .map(|(_, cmd)| (*cmd).to_owned())
}

/// Where a resolved gate command came from — printed so a landing never *reads* as verified
/// when nothing ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateSource {
    /// `--no-gate`: deliberately unverified.
    Skipped,
    /// Given on the command line (and remembered for the repo).
    Flag,
    /// Remembered for this repo.
    Stored,
    /// Detected from the repo's layout, and remembered.
    Detected,
    /// Nothing given, stored, or detected.
    None,
}

impl GateSource {
    /// A short human tag for the "gate: …" line.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Skipped => "skipped (--no-gate)",
            Self::Flag => "from --gate, remembered for this repo",
            Self::Stored => "remembered for this repo",
            Self::Detected => "autodetected, remembered for this repo",
            Self::None => "none found — landing UNVERIFIED",
        }
    }
}

/// Resolve the gate command for this land: flag, then stored, then autodetect (design D36.5).
/// A flag or a detection is remembered, so the answer is decided once per repo.
///
/// # Errors
/// Returns an error if reading or writing the stored gate fails.
pub fn resolve_gate(
    db: &Db,
    repo_root: &Path,
    repo_key: &str,
    flag: Option<&str>,
    no_gate: bool,
) -> Result<(Option<String>, GateSource)> {
    if no_gate {
        return Ok((None, GateSource::Skipped));
    }
    if let Some(cmd) = flag {
        set_gate(db, repo_key, Some(cmd))?;
        return Ok((Some(cmd.to_owned()), GateSource::Flag));
    }
    if let Some(cmd) = stored_gate(db, repo_key)? {
        return Ok((Some(cmd), GateSource::Stored));
    }
    match autodetect_gate(repo_root) {
        Some(cmd) => {
            set_gate(db, repo_key, Some(&cmd))?;
            Ok((Some(cmd), GateSource::Detected))
        }
        None => Ok((None, GateSource::None)),
    }
}

/// Run `cmd` in `dir` through the user's shell. Returns whether it passed, and its combined
/// output when `capture` — which `--json` needs, since a build streaming to stdout would
/// otherwise be interleaved into the JSON document.
///
/// # Errors
/// Returns an error if the shell cannot be executed at all.
pub fn run_gate(dir: &Path, cmd: &str, capture: bool) -> Result<(bool, Option<String>)> {
    let mut command = std::process::Command::new("sh");
    command.arg("-c").arg(cmd).current_dir(dir);
    if capture {
        let out = command
            .output()
            .with_context(|| format!("running the gate `{cmd}`"))?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        return Ok((out.status.success(), Some(text)));
    }
    let status = command
        .status()
        .with_context(|| format!("running the gate `{cmd}`"))?;
    Ok((status.success(), None))
}

#[cfg(test)]
mod tests {
    use super::{branch_for, mint_name, name_from_branch, LandLock};

    #[test]
    fn a_session_name_is_the_task_made_readable() {
        // The uid's disambiguating hash is noise in a branch name and a directory listing.
        assert_eq!(
            mint_name("task:subtasks-for-a-task-18c7815f3e2bad28", |_| false),
            "subtasks-for-a-task"
        );
        // A file-backed task is named by its `^local_id`, which is already the readable part.
        assert_eq!(
            mint_name("file:///repo/tasks.md#fix-ls-counts", |_| false),
            "fix-ls-counts"
        );
        // A word that merely looks hex-ish is not a hash: too short to be one.
        assert_eq!(mint_name("task:add-cafe", |_| false), "add-cafe");
        // Collisions get a counter rather than silently reusing another task's worktree.
        assert_eq!(mint_name("task:fix-ls", |n| n == "fix-ls"), "fix-ls-2");
        // A uid with nothing usable in it still yields a valid path segment.
        assert_eq!(mint_name("task:___", |_| false), "task");
    }

    #[test]
    fn branch_names_round_trip() {
        assert_eq!(branch_for("fix-ls"), "task/fix-ls");
        assert_eq!(name_from_branch("task/fix-ls"), Some("fix-ls"));
        assert_eq!(name_from_branch("main"), None);
    }

    /// Landing is serial (D36.4), but a crashed land must not wedge the repo forever.
    #[test]
    fn the_land_lock_is_exclusive_but_not_permanent() {
        let tmp = tempfile::tempdir().unwrap();
        let held = LandLock::acquire(tmp.path()).unwrap();
        assert!(
            LandLock::acquire(tmp.path()).is_err(),
            "a live land must block a second one"
        );
        drop(held);
        let after = LandLock::acquire(tmp.path()).unwrap();
        drop(after);

        // A lock left behind by a process that no longer exists is stale, not fatal.
        let lock = super::jkb_dir(tmp.path()).join(super::LOCK_FILE);
        std::fs::write(&lock, "4294967290").unwrap();
        let taken = LandLock::acquire(tmp.path()).unwrap();
        drop(taken);
    }
}
