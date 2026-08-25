//! A session worktree is **archived**, never deleted (design D49).
//!
//! TWO INCIDENTS, ONE MECHANISM. `jkb task land` used to finish with `git worktree remove`, and
//! `git`'s implementation of that is *unlink the tree recursively, and stop at the first
//! refusal*. Landing from inside a sandboxed agent session, the refusal comes at
//! `<worktree>/.claude/settings.json` — Claude Code protects a project's policy files from the
//! agent whose policy they are — by which point 152 files were already gone. The verb reported an
//! error about the *directory*, said nothing about the 62,421 lines it had already removed, and
//! the next attempt refused with "it has uncommitted changes" because the dirty-tree guard read
//! those deletions as work in progress. Nothing was lost only because everything was committed.
//!
//! So disposal is a **rename**, not a walk. `fs::rename` is atomic: it either moves the whole
//! tree into `<repo>/.jkb/archive/<session>-<stamp>` or it changes nothing at all, and there is
//! no state in between for a failure to leave behind. That removes partial destruction as a
//! representable outcome rather than guarding against it, and it also means a worktree removed by
//! mistake is still sitting there to be moved back.
//!
//! WHAT IS DENIED IS SCOPED TO THE SESSION ITSELF, which is what makes deferral work. Measured
//! across five live worktrees, only the session's *own* tree refuses (`rmdir` answers `EPERM`
//! where every other session answers `ENOTEMPTY`), because the protected paths are registered for
//! the session's own working directories. So any other process — the watcher service, another
//! session, a plain terminal — can archive it. `land` therefore never blocks on this: it grafts,
//! records what it could not move here, applies its plan, and the reaper finishes the job.
//!
//! DELETION IS A SEPARATE, LATER DECISION. An archive is removed once it is older than the
//! retention window, and that is the only deletion in this module. It is probed first with
//! `remove_dir`, whose `EPERM`-versus-`ENOTEMPTY` answer is exactly the discrimination above, so
//! the sweep never starts a walk it cannot finish either.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::gitrepo;

/// How long an archived worktree is kept before the sweep deletes it.
pub const RETAIN_DAYS: u64 = 30;

const SECS_PER_DAY: u64 = 86_400;

/// One worktree the system owes an archive, and then a deletion.
///
/// The same record covers both halves of the life cycle, so there is no second index to keep in
/// agreement with this one: `archive` is `None` while the tree is still in place and `Some` once
/// it has been moved, and the sweep works from the same directory the deferral wrote into.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    /// Where the session worktree is (or was, once `archive` is set).
    pub worktree: PathBuf,
    /// The repo whose `.jkb/archive` receives it, and whose git metadata is pruned.
    pub repo_root: PathBuf,
    /// The session branch, deleted once the tree is out of the way. Empty if unknown.
    pub branch: String,
    /// The task this session was for — reported, never re-read as authority.
    pub uid: String,
    /// When the landing recorded this, in seconds since the Unix epoch.
    pub recorded_at: u64,
    /// The commit the worktree was on when the record was written.
    ///
    /// This is the record's **instance identity**, and it is why the sweep can be trusted to act
    /// on a path. A path and a branch name are both reusable: remove a deferred worktree by hand,
    /// reopen the task and `jkb task work` recreates a session at the same path on the same
    /// branch — and a sweep keyed on those two would archive that live tree and force-delete its
    /// branch. A commit is not reused, so a tree that is not still sitting on the one the landing
    /// recorded is not the tree this record is about. `None` only for a record written before this
    /// field existed, which is held rather than acted on for the same reason.
    #[serde(default)]
    pub head: Option<String>,
    /// Where the tree was moved to, once it has been.
    #[serde(default)]
    pub archive: Option<PathBuf>,
    /// When it was moved, which is what the retention window is measured from.
    #[serde(default)]
    pub archived_at: Option<u64>,
}

/// What one sweep did. Every field is something that happened, never something intended.
#[derive(Default, Debug)]
pub struct Report {
    /// `(uid, archive path)` for trees moved out of the way by this sweep.
    pub archived: Vec<(String, PathBuf)>,
    /// `(uid, why)` for trees this process could not move — the session's own, typically.
    pub held: Vec<(String, String)>,
    /// Archives deleted because they are past the retention window.
    pub deleted: Vec<PathBuf>,
    /// Records dropped because there was nothing left to act on.
    pub cleared: Vec<String>,
    /// Archives still inside the retention window.
    pub retained: usize,
    /// What those archives occupy, in bytes.
    ///
    /// Reported because the alternative signal is a full disk. A session worktree carries the
    /// repo's build output — on this repo, gigabytes — and archiving keeps it for the retention
    /// window. Deliberately REPORTED rather than pruned: `git clean -X` would delete exactly the
    /// regenerable files and would also delete a gitignored `.env`, and this whole mechanism
    /// exists because a delete that was not asked for is the failure being designed against.
    /// Shorten the window with `--retain-days` if the size is what matters more.
    pub retained_bytes: u64,
    /// Marker files that could not be read. Reported rather than removed: a file we cannot parse
    /// may be a torn write, and deleting it would discard the only record of a live worktree.
    pub unreadable: Vec<PathBuf>,
}

impl Report {
    /// Whether the sweep did anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.archived.is_empty()
            && self.held.is_empty()
            && self.deleted.is_empty()
            && self.cleared.is_empty()
            && self.unreadable.is_empty()
    }
}

/// Seconds since the Unix epoch. A clock that cannot be read reads as 0, which makes everything
/// look ancient — so the sweep is written to compare with a saturating subtraction and a 0 `now`
/// simply retains everything rather than deleting it all.
#[must_use]
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Where the removal records live: beside the database, so the reaper needs no repo context and
/// one service can sweep every repo the machine has.
#[must_use]
pub fn store_dir(db: &Path) -> PathBuf {
    db.parent()
        .unwrap_or_else(|| Path::new("."))
        .join("worktree-removals")
}

/// Where a repo keeps its archived worktrees: beside `.jkb/work`, on the same filesystem, so the
/// move is a rename rather than a copy. `.jkb/` is already in the repo's `.git/info/exclude`.
#[must_use]
pub fn archive_root(repo_root: &Path) -> PathBuf {
    repo_root.join(".jkb").join("archive")
}

/// One record per file, named for the worktree it is about. Per-entry files rather than one
/// document holding a map: two processes sweeping at once then cannot lose each other's updates,
/// and a take is one `remove_file` touching nothing else.
///
/// The readable part is a slug, and the slug alone is **not** an identity: mapping every
/// non-alphanumeric character to `-` makes `~/repos/a-b/.jkb/work/s` and `~/repos/a/b/.jkb/work/s`
/// the same name, and the second record would then overwrite the first — silently leaving one
/// landed worktree that nothing ever archives. The hash is what makes the name unique; the slug
/// is there so a person can tell the files apart.
fn marker_path(db: &Path, worktree: &Path) -> PathBuf {
    let raw = worktree.to_string_lossy();
    let slug: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let digest = jkb_core::blob::hash_bytes(raw.as_bytes());
    store_dir(db).join(format!("{slug}-{}.json", &digest[..8]))
}

/// Write (or replace) the record for one worktree.
///
/// # Errors
/// Returns an error if the store cannot be created or the record cannot be written.
pub fn record(db: &Path, entry: &Entry) -> Result<()> {
    record_at(&marker_path(db, &entry.worktree), entry)
}

/// Write one record to a marker file **already chosen**.
///
/// `reap` updates through this rather than through [`record`], so an entry is written back to the
/// file it was read from. Recomputing the name would leave the original beside the update the
/// moment the two disagree — a record read from a hand-written or older-scheme marker then
/// survives as a second, stale copy of the same worktree, which the next sweep reads as live.
///
/// # Errors
/// Returns an error if the store cannot be created or the record cannot be written.
pub fn record_at(path: &Path, entry: &Entry) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let body = serde_json::to_vec_pretty(entry).context("encoding the worktree removal record")?;
    // Written beside and renamed into place, so a sweep running concurrently reads either the old
    // record or the new one and never half of either.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &body).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// What the record store holds: the records that parsed, and the files that did not.
#[derive(Default, Debug)]
pub struct Store {
    /// `(marker file, record)`, ordered by marker path so a sweep is deterministic.
    pub records: Vec<(PathBuf, Entry)>,
    /// Marker files that could not be read or parsed.
    pub unreadable: Vec<PathBuf>,
}

/// Every record currently in the store, with the marker file each came from.
///
/// # Errors
/// Returns an error only if the store directory exists and cannot be listed; an absent store is
/// the ordinary state and reads as empty.
pub fn entries(db: &Path) -> Result<Store> {
    let dir = store_dir(db);
    let mut store = Store::default();
    let listing = match fs::read_dir(&dir) {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(store),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    for item in listing {
        let path = item
            .with_context(|| format!("listing {}", dir.display()))?
            .path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        match fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<Entry>(&b).map_err(|e| e.to_string()))
        {
            Ok(entry) => store.records.push((path, entry)),
            Err(_) => store.unreadable.push(path),
        }
    }
    store.records.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(store)
}

/// Move a worktree into its repo's archive. **Atomic**: on any error nothing has moved.
///
/// # Errors
/// Returns the underlying I/O error, including the `PermissionDenied` that a session attempting
/// to archive its own worktree gets.
pub fn stow(repo_root: &Path, worktree: &Path, at: u64) -> io::Result<PathBuf> {
    let root = archive_root(repo_root);
    fs::create_dir_all(&root)?;
    let name = worktree.file_name().map_or_else(
        || "session".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let stamp = stamp(at);
    let mut dest = root.join(format!("{name}-{stamp}"));
    // Two landings in the same second, or a re-archive of a recreated session name.
    let mut n = 2;
    while dest.exists() {
        dest = root.join(format!("{name}-{stamp}-{n}"));
        n += 1;
    }
    fs::rename(worktree, &dest)?;
    Ok(dest)
}

/// What became of a worktree this process tried to dispose of.
pub enum Disposed {
    /// Moved into the repo's archive.
    Archived(PathBuf),
    /// Nothing moved — this process may not unlink the tree — and a record was left for the
    /// reaper. Carries the refusal, because "it did not work" without the reason is what makes an
    /// operator go looking in the wrong place.
    Deferred(String),
}

/// Dispose of one session worktree, and leave a record either way.
///
/// THE ONE DISPOSAL every verb calls. `land` and `abandon` both end a session, and having them
/// end it two different ways is how `abandon` kept the recursive `git worktree remove` this
/// module exists to replace — so from inside a sandboxed session the verb an operator reaches
/// for to clear a leftover directory was the one that gutted it. A rule each call site has to
/// remember is the defect; this is the callee that remembers it.
///
/// `delete_branch` is the caller's, because the two verbs genuinely differ: a landing's branch is
/// a duplicate of commits now in the target, while an abandoned branch holds the only copy of
/// real work and is kept unless the operator says otherwise.
///
/// # Errors
/// Returns an error only if the worktree's HEAD cannot be read, which is the one thing this
/// cannot proceed without: it is the identity the sweep later checks before acting on the path.
pub fn dispose(
    db: &Path,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
    uid: &str,
    delete_branch: bool,
) -> Result<Disposed> {
    let head = gitrepo::rev(worktree, "HEAD")
        .with_context(|| format!("reading HEAD in {}", worktree.display()))?;
    let mut entry = Entry {
        worktree: worktree.to_path_buf(),
        repo_root: repo_root.to_path_buf(),
        branch: branch.to_owned(),
        uid: uid.to_owned(),
        recorded_at: now_secs(),
        head,
        archive: None,
        archived_at: None,
    };
    match stow(repo_root, worktree, entry.recorded_at) {
        Ok(dest) => {
            entry.archive = Some(dest.clone());
            entry.archived_at = Some(entry.recorded_at);
            // Written BEFORE the two git commands, which can fail: this is the only thing that
            // says where the tree went, and an archive nothing records is never swept. A failure
            // here is reported rather than returned — the tree is safely out of the way and the
            // caller's work succeeded, so the cost is disk, and bailing would cost the landing.
            if let Err(e) = record(db, &entry) {
                eprintln!(
                    "note: archived to {} but the removal record could not be written ({e}) — it \
                     will not be swept automatically; delete it by hand when you no longer want it",
                    dest.display()
                );
            }
            let _ = gitrepo::prune_worktrees(repo_root);
            if delete_branch {
                if let Err(e) = gitrepo::delete_branch(repo_root, branch, true) {
                    eprintln!("note: could not delete {branch}: {e}");
                }
            }
            Ok(Disposed::Archived(dest))
        }
        // A SESSION CANNOT ARCHIVE ITSELF, and that is the ordinary case rather than an error:
        // the refusal covers the session's own working directories, and every other process — the
        // watcher service, another session, a terminal — moves it freely (measured across five
        // live worktrees). The rename changed nothing, so the caller's work is simply completed
        // and the tree is handed to `jkb task reap`.
        Err(e) => {
            if let Err(rec) = record(db, &entry) {
                eprintln!(
                    "note: {} could not be archived from here ({e}) and the removal record could \
                     not be written either ({rec}) — remove the directory by hand",
                    worktree.display()
                );
            }
            Ok(Disposed::Deferred(e.to_string()))
        }
    }
}

/// Whether this process may unlink `path` — asked of the kernel, not inferred.
///
/// `remove_dir` on a non-empty directory is the exact probe: it cannot succeed, so it destroys
/// nothing, and its two failure modes are the discrimination we need — `PermissionDenied` means
/// the unlink is refused (for the tree, not just the directory: the refusal propagates up from a
/// protected descendant, measured), `DirectoryNotEmpty` means it is permitted. Anything else is
/// unknown, and unknown holds rather than deletes.
fn removable(path: &Path) -> Result<(), String> {
    match fs::remove_dir(path) {
        // It was empty and is now gone. Nothing to walk, and nothing lost.
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Exclusive access to the record store for the duration of one sweep.
///
/// Two sweeps racing do lose each other's work, and not hypothetically: the service runs one every
/// quarter hour and `jkb doctor --fix` runs another on demand. Both read the same pending record;
/// the first archives it and writes the record back; the second finds the worktree gone, takes the
/// "nothing left to act on" arm and **deletes the record the first just wrote** — leaving an
/// archive directory nothing tracks and nothing will ever delete.
///
/// Same shape as `session::LandLock`, including its rule: a lock is stale only when its holder is
/// **proven** gone. An unparseable pid and a probe that could not run are both unestablished, and
/// breaking a live sweep's lock on an unestablished answer is what the lock exists to prevent.
struct SweepLock {
    path: PathBuf,
}

impl SweepLock {
    fn acquire(db: &Path) -> Result<Option<Self>> {
        let dir = store_dir(db);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(".sweep.lock");
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = write!(f, "{}", std::process::id());
                    return Ok(Some(Self { path }));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let holder = fs::read_to_string(&path).unwrap_or_default();
                    let dead = holder
                        .trim()
                        .parse::<u32>()
                        .map_or(jkb_fsm::Fact::Unknown, crate::owner::pid_alive)
                        .is_no();
                    if !dead {
                        // Not an error: the other sweep is doing this sweep's work. A service
                        // tick that says "someone else is on it" must not read as a failure.
                        return Ok(None);
                    }
                    let _ = fs::remove_file(&path);
                }
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        Ok(None)
    }
}

impl Drop for SweepLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Bytes under `dir`. Best-effort: anything unreadable simply does not count, because this is a
/// number printed for a person and never a value anything decides on.
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

/// Whether the tree at `entry.worktree` is still the session this record is about.
///
/// Three questions, all of which must answer yes, and any of which failing HOLDS: git still
/// registers that path as a worktree of this repo, it is still sitting on the commit the landing
/// recorded, and it has nothing uncommitted. The first two establish identity — a path and a
/// branch name are reusable, a commit is not — and the third is the ordinary safety rule: work
/// nobody has committed is not something to move out from under whoever is writing it.
fn still_the_recorded_session(entry: &Entry) -> Result<(), String> {
    let Some(want) = entry.head.as_deref() else {
        return Err(
            "the record predates instance identity, so the tree cannot be shown to be \
                    the one it describes"
                .to_owned(),
        );
    };
    let registered = gitrepo::worktrees(&entry.repo_root)
        .map_err(|e| format!("git could not list this repo's worktrees: {e}"))?
        .into_iter()
        // Canonical comparison: git reports `/private/var/...` where the record holds
        // `/var/...` on macOS, and a raw equality answers "not registered" for the very
        // directory you are standing in.
        .any(|w| crate::session::same_path(&w.path, &entry.worktree));
    if !registered {
        return Err(format!(
            "{} is no longer a registered worktree of this repo",
            entry.worktree.display()
        ));
    }
    match gitrepo::rev(&entry.worktree, "HEAD") {
        Ok(Some(head)) if head == want => {}
        Ok(Some(head)) => {
            return Err(format!(
            "it is on {} now, not the {want} the landing recorded — this is a different session \
                 reusing the name",
            &head[..head.len().min(8)]
        ))
        }
        Ok(None) => return Err("its HEAD does not resolve".to_owned()),
        Err(e) => return Err(format!("git could not read its HEAD: {e}")),
    }
    match gitrepo::is_dirty(&entry.worktree) {
        Ok(false) => Ok(()),
        Ok(true) => Err("it has uncommitted changes".to_owned()),
        Err(e) => Err(format!("git could not check it for changes: {e}")),
    }
}

/// Archive what is owed, then delete archives past `retain_days`.
///
/// Never fails as a whole for one bad record: a repo that has moved, a worktree somebody removed
/// by hand and a marker we cannot parse are all reported and stepped over, because this runs
/// unattended from a service and one wedged entry must not stop the rest.
///
/// # Errors
/// Returns an error only if the record store itself cannot be listed.
pub fn reap(db: &Path, retain_days: u64, dry_run: bool) -> Result<Report> {
    // One sweep at a time, held across the reads as well as the writes: what races here is the
    // DECISION as much as the write, because two sweeps that both read one pending record both
    // act on it. A sweep that finds the lock held reports nothing and returns — the other process
    // is doing this one's work, which is not a failure.
    let Some(_lock) = SweepLock::acquire(db)? else {
        return Ok(Report::default());
    };
    let now = now_secs();
    let store = entries(db)?;
    let mut report = Report {
        unreadable: store.unreadable,
        ..Report::default()
    };

    for (marker, mut entry) in store.records {
        // Already archived, so the only question left is whether it is old enough to delete.
        if let Some(dir) = entry.archive.clone() {
            if !dir.exists() {
                // Somebody removed it by hand. Nothing owed, so stop tracking it.
                drop_marker(&marker, dry_run, &mut report, &entry.uid);
                continue;
            }
            let age = now.saturating_sub(entry.archived_at.unwrap_or(now));
            if age < retain_days.saturating_mul(SECS_PER_DAY) {
                report.retained += 1;
                report.retained_bytes += dir_size(&dir);
                continue;
            }
            if let Err(why) = removable(&dir) {
                report.held.push((
                    entry.uid.clone(),
                    format!("{} cannot be deleted from here: {why}", dir.display()),
                ));
            } else if dry_run {
                report.deleted.push(dir);
            } else if let Err(e) = fs::remove_dir_all(&dir) {
                // The probe proved the unlink permitted, but a walk can still fail — a file
                // appearing mid-sweep, a device error. The record stays and the next sweep
                // tries again.
                report.held.push((
                    entry.uid.clone(),
                    format!("{} was not fully deleted: {e}", dir.display()),
                ));
            } else {
                report.deleted.push(dir);
                drop_marker(&marker, dry_run, &mut report, "");
            }
            continue;
        }

        // Not archived yet: this is the deferred half of a landing.
        //
        // A REPO WE CANNOT SEE IS UNKNOWN, NOT SETTLED. This used to clear the record, and the
        // case is ordinary rather than exotic: `~/.jkb` is bind-mounted into the dev container, so
        // both sides share one store while the same repo is `/home/vscode/repos/jkb` there and
        // `~/repos/jkb` here. A container-recorded landing swept on the host would have had its
        // record deleted while the worktree sat untouched — never archived, never reported, and
        // listed as in flight for ever. Same for a repo on a drive that is not mounted today.
        // Holding costs a line of output; clearing costs the only record of a live worktree.
        if !entry.repo_root.exists() {
            report.held.push((
                entry.uid.clone(),
                format!(
                    "{} is not reachable from here, so whether anything is owed cannot be \
                     established — the record is kept",
                    entry.repo_root.display()
                ),
            ));
            continue;
        }
        if !entry.worktree.exists() {
            // Gone already. Tidy git's registration and the branch, then stop tracking.
            if !dry_run {
                let _ = gitrepo::prune_worktrees(&entry.repo_root);
                delete_branch_if_any(&entry.repo_root, &entry.branch);
            }
            drop_marker(&marker, dry_run, &mut report, &entry.uid);
            continue;
        }
        // Identity, before anything destructive. A record names a path and a branch, and both are
        // reusable names; establishing that the tree is still the one it describes is what makes
        // acting on them safe.
        if let Err(why) = still_the_recorded_session(&entry) {
            report.held.push((
                entry.uid.clone(),
                format!("{}: {why}", entry.worktree.display()),
            ));
            continue;
        }
        if dry_run {
            report
                .archived
                .push((entry.uid.clone(), archive_root(&entry.repo_root)));
            continue;
        }
        match stow(&entry.repo_root, &entry.worktree, now) {
            Ok(dest) => {
                entry.archive = Some(dest.clone());
                entry.archived_at = Some(now);
                // THE RECORD IS WRITTEN FIRST, before the two git commands below, because those
                // can fail and this is the only thing that says where the tree went. Written back
                // to the SAME file it was read from, so no second copy can appear.
                if let Err(e) = record_at(&marker, &entry) {
                    report.held.push((
                        entry.uid.clone(),
                        format!(
                            "archived to {} but the record could not be updated: {e}",
                            dest.display()
                        ),
                    ));
                    continue;
                }
                let _ = gitrepo::prune_worktrees(&entry.repo_root);
                delete_branch_if_any(&entry.repo_root, &entry.branch);
                report.archived.push((entry.uid.clone(), dest));
            }
            Err(e) => report.held.push((
                entry.uid.clone(),
                format!("{} could not be moved: {e}", entry.worktree.display()),
            )),
        }
    }

    Ok(report)
}

fn drop_marker(marker: &Path, dry_run: bool, report: &mut Report, uid: &str) {
    if !dry_run {
        let _ = fs::remove_file(marker);
    }
    if !uid.is_empty() {
        report.cleared.push(uid.to_owned());
    }
}

/// Deleting the branch is tidiness, not correctness: its commits are in the target by the time
/// anything here runs. A branch somebody checked out elsewhere, or already deleted, is not an
/// error worth stopping an unattended sweep for.
fn delete_branch_if_any(repo_root: &Path, branch: &str) {
    if !branch.is_empty() {
        let _ = gitrepo::delete_branch(repo_root, branch, true);
    }
}

/// `YYYYmmddTHHMMSSZ`, so an archive directory says when it was made without anything having to
/// parse it. The record carries the epoch seconds; this is only ever read by a person.
#[must_use]
pub fn stamp(secs: u64) -> String {
    let days = i64::try_from(secs / SECS_PER_DAY).unwrap_or(0);
    let rem = secs % SECS_PER_DAY;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{h:02}{mi:02}{s:02}Z",
        h = rem / 3600,
        mi = (rem % 3600) / 60,
        s = rem % 60,
    )
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's `civil_from_days`). Written out rather
/// than taking a date dependency for one cosmetic directory name.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(worktree: &Path, repo: &Path) -> Entry {
        Entry {
            worktree: worktree.to_path_buf(),
            repo_root: repo.to_path_buf(),
            branch: String::new(),
            uid: "task:t".into(),
            recorded_at: 0,
            head: None,
            archive: None,
            archived_at: None,
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            // The developer's global config signs commits and sets core.hooksPath; either would
            // fail this fixture for reasons that have nothing to do with archiving.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A repo with one real session worktree, which is what the sweep now requires before it will
    /// act: the identity check asks git whether the path is a registered worktree and whether it
    /// is still on the commit the record names.
    fn session(repo: &Path, name: &str) -> (PathBuf, String, String) {
        std::fs::create_dir_all(repo).expect("mk");
        git(repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("base.txt"), "base").expect("write");
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-qm", "base"]);
        let wt = repo.join(".jkb/work").join(name);
        let branch = format!("task/{name}");
        git(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                &wt.to_string_lossy(),
                "main",
            ],
        );
        let head = git(&wt, &["rev-parse", "HEAD"]);
        (wt, branch, head)
    }

    fn session_entry(repo: &Path, wt: &Path, branch: &str, head: &str) -> Entry {
        Entry {
            head: Some(head.to_owned()),
            branch: branch.to_owned(),
            ..entry(wt, repo)
        }
    }

    #[test]
    fn two_worktrees_whose_slugs_collide_keep_separate_records() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        // Identical once every non-alphanumeric character becomes `-`, which is why the name
        // cannot be the slug alone.
        let a = Path::new("/r/a-b/.jkb/work/s");
        let b = Path::new("/r/a/b/.jkb/work/s");
        record(&db, &entry(a, Path::new("/r/a-b"))).expect("a");
        record(&db, &entry(b, Path::new("/r/a/b"))).expect("b");

        let records = entries(&db).expect("entries").records;
        assert_eq!(records.len(), 2, "neither record overwrote the other");
        let mut seen: Vec<&Path> = records.iter().map(|(_, e)| e.worktree.as_path()).collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![b, a]);
    }

    #[test]
    fn the_stamp_is_a_readable_utc_date() {
        // 2026-08-25T15:30:00Z. Pinned against a value computed elsewhere, so the calendar
        // arithmetic is checked rather than asserted to equal itself.
        assert_eq!(stamp(1_787_671_800), "20260825T153000Z");
        assert_eq!(stamp(0), "19700101T000000Z");
        // A leap day, which is what the era arithmetic is for.
        assert_eq!(stamp(1_709_164_800), "20240229T000000Z");
    }

    #[test]
    fn stowing_moves_the_whole_tree_and_leaves_nothing_behind() {
        let t = tempfile::tempdir().expect("tempdir");
        let repo = t.path();
        let wt = repo.join(".jkb/work/sess");
        fs::create_dir_all(wt.join("crates")).expect("mk");
        fs::write(wt.join("crates/a.rs"), b"fn main() {}").expect("write");

        let dest = stow(repo, &wt, 0).expect("stow");
        assert!(!wt.exists(), "the worktree is gone from .jkb/work");
        assert_eq!(
            fs::read(dest.join("crates/a.rs")).expect("read"),
            b"fn main() {}",
            "and every file came with it"
        );
        assert!(dest.starts_with(archive_root(repo)));
    }

    #[test]
    fn a_second_stow_in_the_same_second_does_not_collide() {
        let t = tempfile::tempdir().expect("tempdir");
        let repo = t.path();
        for _ in 0..2 {
            let wt = repo.join(".jkb/work/sess");
            fs::create_dir_all(&wt).expect("mk");
            fs::write(wt.join("f"), b"x").expect("write");
            stow(repo, &wt, 0).expect("stow");
        }
        let n = fs::read_dir(archive_root(repo)).expect("ls").count();
        assert_eq!(
            n, 2,
            "both archives survive; the second did not overwrite the first"
        );
    }

    #[test]
    fn a_pending_record_is_archived_and_then_swept_when_it_ages_out() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");
        record(&db, &session_entry(&repo, &wt, &branch, &head)).expect("record");

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(
            r.archived.len(),
            1,
            "the pending tree is archived: {:?}",
            r.held
        );
        assert!(!wt.exists());
        let dest = r.archived[0].1.clone();
        assert!(dest.exists(), "and the archive is where the report says");

        // Still inside the window: the sweep must NOT delete it.
        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(r.retained, 1);
        assert!(dest.exists(), "a fresh archive is kept");
        assert!(r.deleted.is_empty());

        // Past it.
        let r = reap(&db, 0, false).expect("reap");
        assert_eq!(r.deleted, vec![dest.clone()]);
        assert!(!dest.exists(), "an aged-out archive is deleted");
        let left = entries(&db).expect("entries").records;
        assert!(left.is_empty(), "and its record goes with it");
    }

    #[test]
    fn archiving_updates_the_marker_it_read_rather_than_writing_a_second_one() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");
        // A marker under a name `marker_path` would not choose — an older scheme, or one written
        // by hand. The update must land HERE, not beside it.
        fs::create_dir_all(store_dir(&db)).expect("mk");
        let marker = store_dir(&db).join("legacy-name.json");
        let e = session_entry(&repo, &wt, &branch, &head);
        fs::write(&marker, serde_json::to_vec(&e).expect("json")).expect("write");

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(r.archived.len(), 1);
        let left = entries(&db).expect("entries").records;
        assert_eq!(left.len(), 1, "one record, not the original plus an update");
        assert_eq!(left[0].0, marker, "and it is the file the sweep read");
        assert!(left[0].1.archive.is_some(), "carrying where the tree went");
    }

    #[test]
    fn a_session_reusing_the_recorded_name_is_held_rather_than_archived() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");
        record(&db, &session_entry(&repo, &wt, &branch, &head)).expect("record");

        // The deferred worktree is removed by hand and a NEW session is opened at the same path
        // on the same branch — which is what reopening the task and running `jkb task work` does.
        // A sweep keyed on path and branch alone would archive this live tree and force-delete
        // its branch; the commit is what tells them apart.
        git(
            &repo,
            &["worktree", "remove", "--force", &wt.to_string_lossy()],
        );
        git(&repo, &["branch", "-D", &branch]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                &wt.to_string_lossy(),
                "main",
            ],
        );
        std::fs::write(wt.join("new-work.txt"), "in progress").expect("write");
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "different work"]);

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert!(r.archived.is_empty(), "the live session was not archived");
        assert_eq!(r.held.len(), 1, "it is held, with the reason: {:?}", r.held);
        assert!(
            wt.join("new-work.txt").exists(),
            "and its work is untouched"
        );
        assert!(
            gitrepo::rev(&repo, &branch).expect("rev").is_some(),
            "and its branch survives"
        );
    }

    #[test]
    fn uncommitted_work_in_a_recorded_session_holds_the_sweep() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");
        record(&db, &session_entry(&repo, &wt, &branch, &head)).expect("record");
        std::fs::write(wt.join("scratch.txt"), "unsaved").expect("write");

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert!(r.archived.is_empty(), "nothing is moved out from under it");
        assert_eq!(r.held.len(), 1, "{:?}", r.held);
        assert!(wt.join("scratch.txt").exists());
    }

    #[test]
    fn a_repo_this_process_cannot_see_holds_its_record_instead_of_clearing_it() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        // The ordinary cross-boundary case: a landing recorded inside the dev container names
        // /home/vscode/repos/jkb, and the host sweeps the same shared store.
        let e = Entry {
            head: Some("deadbeef".into()),
            ..entry(
                Path::new("/home/vscode/repos/jkb/.jkb/work/s"),
                Path::new("/home/vscode/repos/jkb"),
            )
        };
        record(&db, &e).expect("record");

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert!(r.cleared.is_empty(), "an unreachable repo settles nothing");
        assert_eq!(r.held.len(), 1, "{:?}", r.held);
        assert_eq!(
            entries(&db).expect("entries").records.len(),
            1,
            "the record survives for whichever side can see the repo"
        );
    }

    #[test]
    fn a_dry_run_reports_without_moving_or_deleting_anything() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");
        record(&db, &session_entry(&repo, &wt, &branch, &head)).expect("record");

        let r = reap(&db, RETAIN_DAYS, true).expect("reap");
        assert_eq!(
            r.archived.len(),
            1,
            "it says what it would do: {:?}",
            r.held
        );
        assert!(wt.exists(), "and the tree has not moved");
        let left = entries(&db).expect("entries").records;
        assert_eq!(left.len(), 1, "and the record survives for the real run");
    }

    #[test]
    fn a_record_whose_worktree_vanished_is_cleared_rather_than_held_for_ever() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        fs::create_dir_all(&repo).expect("mk");
        record(&db, &entry(&repo.join(".jkb/work/gone"), &repo)).expect("record");

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(r.cleared.len(), 1);
        let left = entries(&db).expect("entries").records;
        assert!(left.is_empty());
    }

    #[test]
    fn an_unparseable_record_is_reported_and_kept() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        fs::create_dir_all(store_dir(&db)).expect("mk");
        let junk = store_dir(&db).join("torn.json");
        fs::write(&junk, b"{not json").expect("write");

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(r.unreadable, vec![junk.clone()]);
        assert!(
            junk.exists(),
            "it may be a torn write for a live worktree, so it stays"
        );
    }

    #[test]
    fn removable_says_yes_for_an_ordinary_non_empty_directory() {
        let t = tempfile::tempdir().expect("tempdir");
        let d = t.path().join("d");
        fs::create_dir_all(d.join("inner")).expect("mk");
        assert!(
            removable(&d).is_ok(),
            "ENOTEMPTY means the unlink is permitted"
        );
        assert!(d.exists(), "and the probe destroyed nothing");
    }
}
