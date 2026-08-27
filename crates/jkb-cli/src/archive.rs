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

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::gitrepo;

/// How long an archived worktree is kept before the sweep deletes it.
pub const RETAIN_DAYS: u64 = 30;

const SECS_PER_DAY: u64 = 86_400;

/// What the verb that ended a session decided to do with it.
///
/// THE RECORD CARRIES THE DECISION THAT PRODUCED IT. Without this the sweep applied `land`'s
/// defaults to every record, and `jkb task abandon` — which keeps the branch, because an
/// abandoned branch holds the only copy of real work — printed "branch kept" and then had that
/// branch force-deleted by the reaper a quarter of an hour later. The same for `--force`, whose
/// whole meaning is "I accept the uncommitted work in there": unrecorded, the sweep's own dirty
/// check held the record for ever instead.
///
/// Both default to the safe answer on a record written before they existed: keep the branch, and
/// do not touch a dirty tree.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct Plan {
    /// Delete the branch once the tree is out of the way — now if the disposal archives it, or
    /// later when the sweep does.
    #[serde(default)]
    pub delete_branch: bool,
    /// The operator has already accepted whatever is uncommitted in the tree, so the sweep's
    /// dirty check must not re-litigate a decision a person took.
    #[serde(default)]
    pub accept_dirty: bool,
}

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
    /// What the verb decided. See [`Plan`].
    #[serde(default, flatten)]
    pub plan: Plan,
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
    /// Pending records replaced by a later disposal of the same worktree. Their decision was
    /// superseded, so applying it would carry out an instruction the operator withdrew.
    pub superseded: Vec<String>,
    /// Archives still inside the retention window.
    ///
    /// The PATHS, not a byte count: the count is only ever printed, and computing it in the sweep
    /// meant a full stat-walk of every archive every fifteen minutes for a number the watch loop
    /// then discarded unprinted. Whoever prints it walks it (`archive::dir_size`).
    ///
    /// Size is reported at all because the alternative signal is a full disk — a session worktree
    /// carries the repo's build output, gigabytes on this repo, kept for the retention window.
    /// Deliberately reported rather than pruned: `git clean -X` deletes exactly the regenerable
    /// files and also deletes a gitignored `.env`, and unrequested deletion is the failure this
    /// whole mechanism is designed against. Shorten `--retain-days` if size matters more.
    pub retained: Vec<PathBuf>,
    /// Marker files that could not be read. Reported rather than removed: a file we cannot parse
    /// may be a torn write, and deleting it would discard the only record of a live worktree.
    pub unreadable: Vec<PathBuf>,
    /// Another sweep held the lock, so this one looked at nothing — and WHO holds it, because
    /// that is the only thing an operator can act on.
    ///
    /// Distinct from an empty sweep for the reason every other unestablished answer here is:
    /// "there was nothing to do" and "I did not look" are different facts, and printing the
    /// first for the second is what this module keeps being corrected for.
    pub skipped: Option<Held>,
}

impl Report {
    /// What this sweep OBSERVED but could not act on, as a stable fingerprint.
    ///
    /// A watcher prints an observation when it CHANGES, not every interval: a record whose repo
    /// is on the other side of the container bind is permanently unreachable and permanently
    /// held, and counting it as activity made the service re-report and re-walk every retained
    /// archive every interval, for ever, about something no sweep will ever change. Silencing it
    /// outright would be the other error — a torn record needs saying once.
    #[must_use]
    pub fn observed(&self) -> String {
        let mut lines: Vec<String> = self
            .held
            .iter()
            .map(|(uid, why)| format!("h {uid} {why}"))
            .chain(self.unreadable.iter().map(|p| format!("u {}", p.display())))
            // A lock nothing can break is an observation too, and the one this type was added to
            // make visible: `Held`'s own doc says the escape "needs the file and the holder
            // printed". Omitted here, the watcher went permanently silent about exactly it —
            // every deferred landing on the machine stopped completing with no log line anywhere.
            .chain(
                self.skipped
                    .iter()
                    .map(|h| format!("l {} {}", h.path.display(), h.holder)),
            )
            .collect();
        lines.sort();
        lines.join("\n")
    }

    /// Whether the sweep did anything at all — ACTED, not merely observed.
    ///
    /// `held` is deliberately not counted. A held record is the steady state on a machine that
    /// shares `~/.jkb` across the container bind: the other side's records are permanently
    /// unreachable and permanently held, so counting them made the `--watch` service re-report
    /// and re-walk every retained archive every interval, for ever, about something no sweep will
    /// ever change. A skipped sweep is likewise empty: whoever holds the lock is doing this
    /// sweep's work, and the one-shot caller says so explicitly instead.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.archived.is_empty()
            && self.superseded.is_empty()
            && self.deleted.is_empty()
            && self.cleared.is_empty()
    }
}

/// A sweep lock somebody else holds.
///
/// Named in full because there is no automatic way out of one: an owner on another host is
/// `Fact::Unknown` and unknown never frees anything, so a lock left by a container that has since
/// been rebuilt — its hostname gone with it — is respected by every later sweep, on both sides,
/// permanently. That is the right default (breaking a live sweeper's lock is what the lock exists
/// to prevent) but it needs an escape a person can take, and an escape needs the file and the
/// holder printed.
#[derive(Debug, Clone)]
pub struct Held {
    /// The lock file, so `--break-lock` and an operator's `rm` have something to name.
    pub path: PathBuf,
    /// Its holder, verbatim. Empty when the lock could not be read.
    pub holder: String,
}

/// Who holds the sweep lock, if anyone — without touching it.
///
/// # Errors
/// Returns an error if the lock exists and cannot be read.
pub fn lock_holder(db: &Path) -> Result<Option<String>> {
    let path = store_dir(db).join(".sweep.lock");
    match fs::read_to_string(&path) {
        // The file is `<owner> <nonce>` — the nonce is `SweepLock`'s release identity and is no
        // part of who holds it. Taking the first field keeps every consumer unchanged: this is
        // what `jkb task reap`'s refusal prints and what `owner::is_alive` is asked about, and a
        // nonce appended to the id would make the liveness probe unable to parse it, so every
        // holder would read `Unrecognized` -> `Unknown` and no stale lock could ever be broken.
        Ok(holder) => Ok(Some(holder_of(&holder).to_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Remove the sweep lock, whoever holds it. Returns the holder it displaced, if any.
///
/// # Errors
/// Returns an error if the lock exists and cannot be removed.
pub fn break_lock(db: &Path) -> Result<Option<String>> {
    SweepLock::break_held(db)
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
fn marker_stem(db: &Path, worktree: &Path) -> PathBuf {
    let raw = worktree.to_string_lossy();
    let slug: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let digest = jkb_core::blob::hash_bytes(raw.as_bytes());
    store_dir(db).join(format!("{slug}-{}", &digest[..8]))
}

/// A marker file no existing record occupies.
///
/// ONE DISPOSAL, ONE RECORD. The name used to be a pure function of the worktree path — and a
/// session name is reused: abandon a session (archived to `sess-<stamp>`, recorded), reopen the
/// task, `jkb task work` mints the same name at the same path, and the next disposal wrote its
/// record over the first. The first archive was then referenced by nothing: no sweep saw it, no
/// `doctor` listed it, and a checkout with its build output sat there permanently — defeating the
/// retention window whose whole job is keeping the disk from filling. `Entry.head` exists because
/// a path and a branch are both reusable names; the record's own identity was still the path.
fn fresh_marker(db: &Path, worktree: &Path) -> PathBuf {
    let stem = marker_stem(db, worktree);
    // EVERY file carries the counter, zero-padded, so lexical order IS creation order. The first
    // version left the first file bare and suffixed the rest — and `slug-hash-2.json` sorts BELOW
    // `slug-hash.json`, because `-` precedes `.`. `governing_pending` breaks a `recorded_at` tie
    // on the marker, so two disposals in the same second gave the older, withdrawn plan the vote:
    // the inverse of the rule that function exists for. Making the name order the truth costs one
    // format string; reasoning about `-` versus `.` at the comparison site does not survive.
    let mut n = 1;
    loop {
        let path = PathBuf::from(format!("{}-{n:04}.json", stem.display()));
        if !path.exists() {
            return path;
        }
        n += 1;
    }
}

/// Write (or replace) the record for one worktree.
///
/// # Errors
/// Returns an error if the store cannot be created or the record cannot be written.
pub fn record(db: &Path, entry: &Entry) -> Result<()> {
    // A FRESH file every time. This is only ever called to record a NEW disposal; the sweep
    // updates an existing record through `record_at`, with the marker it read.
    fs::create_dir_all(store_dir(db))
        .with_context(|| format!("creating {}", store_dir(db).display()))?;
    record_at(&fresh_marker(db, &entry.worktree), entry)
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
    ///
    /// Only records that [`Record::parse`] accepted. The sweep receives no others, which is why
    /// no arm of it can forget to ask.
    pub records: Vec<(PathBuf, Record)>,
    /// Marker files that could not be read or parsed as JSON at all.
    pub unreadable: Vec<PathBuf>,
    /// Records that parsed as JSON but describe paths this mechanism does not own. Reported and
    /// kept, never deleted: a refusal is not a licence to act on the file.
    pub rejected: Vec<Rejected>,
}

/// Cancel the pending removal of `worktree`, if one is recorded.
///
/// A RECORD IS A PROMISE WITH A CANCEL. Without this the only way a record ended was something
/// destructive happening, and a session legitimately brought back to life still had one: `jkb
/// task abandon` from inside a session defers, the task reopens, `jkb task work` hands the same
/// directory back — and the sweep then either archives the checkout the operator is sitting in
/// and force-deletes its branch, or, once they commit, holds it for ever insisting it is a
/// different session reusing the name. Neither is recoverable by anything the operator is told.
///
/// Only a record that has not been archived yet can be cancelled: once the tree has been moved
/// the record is what says where it went, and the retention sweep owns it.
///
/// # Errors
/// Returns an error if the store cannot be read or the record cannot be removed.
pub fn revoke(db: &Path, worktree: &Path) -> Result<bool> {
    // UNDER THE SWEEP LOCK. Without it a sweep already in flight is working from a snapshot taken
    // before this removal: it archives the checkout `task work` is at that moment handing back,
    // and re-writes the record this deleted. Refusing is the honest outcome — the caller says so
    // and the operator re-runs — because a cancellation that silently lost the race is worse than
    // one that admits it.
    let _lock = SweepLock::acquire(db)?.map_err(|held| {
        anyhow::anyhow!(
            "a sweep is running ({}), so the pending removal could not be cancelled — re-run \
             this in a moment, or clear a stale lock with `jkb task reap --break-lock`",
            if held.holder.is_empty() {
                "holder unknown"
            } else {
                &held.holder
            }
        )
    })?;
    // Found by ASKING the store rather than by computing a filename: one worktree can have
    // several records now (one per disposal), and only the pending ones are cancellable.
    let mut cancelled = false;
    for (path, record) in entries(db)?.records {
        if record.archive.is_some() || record.worktree != worktree {
            continue;
        }
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        cancelled = true;
    }
    Ok(cancelled)
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
            Ok(entry) => {
                let uid = entry.uid.clone();
                match Record::parse(entry) {
                    Ok(record) => store.records.push((path, record)),
                    Err(why) => store.rejected.push(Rejected {
                        marker: path,
                        uid,
                        why,
                    }),
                }
            }
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
    plan: Plan,
) -> Result<Disposed> {
    // The worktree's OWN head — `gitrepo::rev(dir, "HEAD")` is not that. Git's discovery walks
    // up, so a session whose `.git` file has been removed returns the *enclosing repository's*
    // HEAD, and recording that as this session's identity is worse than recording none: the
    // sweep compares it against the tree and concludes a different session is reusing the name.
    //
    // NOT a precondition of disposal. Requiring it before `stow` refused a disposal that would
    // have succeeded outright — and in `abandon` the `?` returned before the claim was released,
    // leaving the task `in_progress` held by a session whose worktree still exists, so
    // `owner::is_alive` says Yes and `doctor --fix` will not reclaim it. That is the exact wedge
    // this function's caller documents having already fixed once. `head` is read by the sweep
    // only to re-identify a DEFERRED tree; an archived record names its archive and needs no
    // identity at all, so the requirement belongs in that arm and nowhere else.
    let head = gitrepo::worktree_head(worktree)
        .with_context(|| format!("reading HEAD in {}", worktree.display()))?;
    let mut entry = Entry {
        worktree: worktree.to_path_buf(),
        repo_root: repo_root.to_path_buf(),
        branch: branch.to_owned(),
        uid: uid.to_owned(),
        recorded_at: now_secs(),
        plan,
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
            if plan.delete_branch {
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
            // HERE is where the identity is load-bearing: the sweep will come back to a tree
            // nobody moved, and `still_the_recorded_session` checks this commit before it touches
            // the path. Without it every sweep takes the "no HEAD was recorded" arm — which names
            // no action — and the directory sits there for ever. The tree is untouched at this
            // point (the rename changed nothing), so refusing costs the caller nothing it has not
            // already done, and the operator still has the directory in front of them.
            if entry.head.is_none() {
                anyhow::bail!(
                    "{} could not be archived from here ({e}), and its HEAD cannot be read from \
                     the worktree itself — so there is nothing to identify it by when the sweep \
                     comes back, and it would be held for ever. `git -C {} status` will say what \
                     git makes of the directory; if it has been partly removed, \
                     `git -C {} restore .` puts it back.",
                    worktree.display(),
                    worktree.display(),
                    worktree.display()
                );
            }
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
    /// EXACTLY WHAT THIS ACQUISITION WROTE. `Drop` unlinks only while the file still contains
    /// it, for the reason `acquire` states about the stale-lock path: releasing a lock that is no
    /// longer yours takes its current holder's with it.
    ///
    /// The identity has to be something neither the owner id nor the filesystem can hand back:
    ///
    /// * the **owner id alone** cannot tell an acquisition from its successor — `host:pid` is the
    ///   same string for both when one process reacquires, which is what the test for this does;
    /// * the **inode** was the first fix and is not safe either. `break_lock` unlinks the file,
    ///   freeing its inode, and the successor creates a new one in the same directory an instant
    ///   later — on ext4 the just-freed bit in the parent group's bitmap is a prime candidate, so
    ///   the displaced sweeper's `Drop` would delete the successor's lock. APFS hands out
    ///   monotonic inode numbers, which is the only reason the test passed on this machine.
    ///
    /// So a per-acquisition **nonce** goes in the file beside the owner id. `content` is the whole
    /// line; `None` if it could not be read back, which disables the release rather than guessing.
    content: Option<String>,
}

/// The owner id out of a lock file's `<owner> <nonce>` contents.
///
/// ONE parser, because there are two readers and they must not disagree: `lock_holder` renders
/// this to the operator, and `SweepLock::acquire` hands it to `owner::is_alive`. Left as the raw
/// contents, the appended nonce would make every id `Unrecognized` — hence `Fact::Unknown`, hence
/// never `is_no()` — and no stale lock could ever be broken again.
fn holder_of(contents: &str) -> &str {
    // No `.trim()`: `split_whitespace` already skips leading whitespace, and clippy is right
    // that chaining them is redundant.
    contents.split_whitespace().next().unwrap_or_default()
}

/// A value no other acquisition of this lock will write. Not security, just distinctness: pid and
/// a monotonic-ish clock reading, which cannot repeat within one process and cannot collide
/// across processes.
fn lock_nonce() -> String {
    format!(
        "{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    )
}

impl SweepLock {
    fn acquire(db: &Path) -> Result<Result<Self, Held>> {
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
                    // `<owner> <nonce>`. `lock_holder` reports the first field, so what an
                    // operator is shown is unchanged.
                    let line = format!("{} {}", crate::owner::self_owner(), lock_nonce());
                    let wrote = write!(f, "{line}").is_ok();
                    drop(f);
                    // Read back rather than assumed: `Drop` compares against what is ON DISK, so
                    // an identity that never reached it would make the release a silent no-op and
                    // wedge the lock for every later sweep.
                    let content = if wrote {
                        fs::read_to_string(&path).ok().filter(|c| c.trim() == line)
                    } else {
                        None
                    };
                    return Ok(Ok(Self { path, content }));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let contents = fs::read_to_string(&path).unwrap_or_default();
                    let holder = holder_of(&contents);
                    // `host:pid`, judged by `owner::is_alive`, which answers `Fact`. A BARE pid
                    // was wrong here in a way it is not for `LandLock`: this store is shared
                    // across the host/container bind on purpose, and pid 812 in the container is
                    // a different process from pid 812 on the host — so the lock excluded nothing
                    // at exactly the boundary where two sweepers meet, and could declare a live
                    // one dead. An owner on another host is `Unknown`, and unknown respects.
                    let dead = crate::owner::is_alive(holder).is_no();
                    if !dead {
                        // Not an error: the other sweep is doing this sweep's work. A service
                        // tick that says "someone else is on it" must not read as a failure.
                        return Ok(Err(Held {
                            path,
                            holder: holder.to_owned(),
                        }));
                    }
                    // RE-READ BEFORE UNLINKING, which is the half of `LandLock`'s shape this
                    // dropped: two sweeps that both judge one stale lock would otherwise both
                    // remove it, and the second's remove would take the first's fresh lock with
                    // it. Removing only while the content is still what was judged makes that a
                    // race one of them loses instead of one they both win.
                    // Against the WHOLE contents, not the owner id parsed out of it: the file is
                    // `<owner> <nonce>`, so comparing the first field alone would match a
                    // successor written by the same host and pid, and comparing to the parsed id
                    // would never match at all — which would leave the stale lock in place and
                    // fail every later acquisition.
                    let still = fs::read_to_string(&path).unwrap_or_default();
                    if still.trim() == contents.trim() {
                        let _ = fs::remove_file(&path);
                    }
                }
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        Ok(Err(Held {
            path,
            holder: String::new(),
        }))
    }

    /// Remove a lock whoever holds it — the operator's escape.
    ///
    /// There is no automatic one and there should not be: a holder on another host is `Unknown`,
    /// and breaking a live sweeper's lock on an unestablished answer is exactly what the lock
    /// prevents. But a container that was killed mid-sweep and then rebuilt leaves a holder whose
    /// hostname no longer exists anywhere, and without this every sweep on both sides no-ops for
    /// ever with nothing to do about it.
    ///
    /// # Errors
    /// Returns an error if the lock file exists and cannot be removed.
    fn break_held(db: &Path) -> Result<Option<String>> {
        let path = store_dir(db).join(".sweep.lock");
        match fs::read_to_string(&path) {
            Ok(holder) => {
                fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
                Ok(Some(holder.trim().to_owned()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

impl Drop for SweepLock {
    /// Release, but only what is still ours.
    ///
    /// An unconditional unlink here undid the rule `acquire` states above it, and `--break-lock`
    /// is what makes that reachable rather than theoretical: across the host/container bind a
    /// foreign holder is `Unknown` by construction, so breaking the lock is the ORDINARY remedy
    /// there, not an escape from a wedge. Break a live sweeper's lock, acquire a fresh one, and
    /// the displaced sweeper's `Drop` removes the successor's — after which two sweeps run
    /// concurrently, which is the whole thing this type exists to prevent.
    ///
    /// Re-reading is not atomic with the unlink and does not need to be: it converts a race both
    /// parties win into one a party can only lose by being displaced in the instant between, and
    /// the displacing party is a person running `--break-lock`.
    fn drop(&mut self) {
        let Some(mine) = self.content.as_deref() else {
            return;
        };
        if fs::read_to_string(&self.path).is_ok_and(|c| c.trim() == mine) {
            let _ = fs::remove_file(&self.path);
        }
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

/// A record the sweep is allowed to act on: read from the store and **proven** to describe paths
/// this mechanism owns.
///
/// THE STORE IS UNTRUSTED INPUT, so it gets a parser rather than a checklist. It is a directory of
/// JSON files inside `~/.jkb` — bind-mounted into the dev container, granted in the posture's
/// `allowWrite`, and read by a launchd agent that runs outside every sandbox — and what the sweep
/// does with a path in it is `remove_dir_all`. The first attempt at this was a `paths_are_ours`
/// function the sweep called before dispatching, and it was defeated by `..`: `Path::starts_with`
/// compares components without interpreting them, so `<repo>/.jkb/archive/../../../Documents`
/// "starts with" the archive root while naming something else entirely. A check the caller
/// remembers to run, over paths nobody normalized, is two mistakes; this is one type.
///
/// [`Entry`] remains the wire form and is trusted for nothing. The only way to obtain a `Record`
/// is [`Record::parse`], so a value of this type IS the evidence, and no sweep arm can be written
/// that skips it.
#[derive(Debug, Clone)]
pub struct Record(Entry);

impl std::ops::Deref for Record {
    type Target = Entry;
    fn deref(&self) -> &Entry {
        &self.0
    }
}

/// A record that parsed but was refused, with enough to report it.
///
/// The uid is carried even though nothing trusts it, because it is what a person recognises. It
/// used to be dropped and the marker path put in the `uid` field of the report instead — a field
/// holding something other than what its name says, which is the shape this branch has spent four
/// rounds removing from everywhere else.
#[derive(Debug, Clone)]
pub struct Rejected {
    /// The marker file, so it can be found and looked at.
    pub marker: PathBuf,
    /// The uid the record claims. A label, never authority.
    pub uid: String,
    /// Why it was refused.
    pub why: String,
}

/// A path this mechanism could have written: absolute, and naming only ordinary components.
///
/// `..` is REFUSED rather than resolved. Resolving would be sound too, but nothing here ever
/// writes one, so a record containing one is either corrupt or hostile and neither deserves a
/// best-effort interpretation. `.` goes with it for the same reason. Purely lexical, so a symlink
/// planted afterwards cannot change the answer, and a path that does not exist is judged the same
/// as one that does.
fn plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|c| {
            matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
}

impl Record {
    /// The wire record inside, for a sweep that needs to update and re-write it.
    #[must_use]
    pub fn clone_entry(&self) -> Entry {
        self.0.clone()
    }

    /// Read one wire record, or say why it cannot be acted on.
    ///
    /// # Errors
    /// Returns the reason the record is refused: a path that is not plain and absolute, or one
    /// that names something outside `<repo_root>/.jkb/{work,archive}`.
    pub fn parse(entry: Entry) -> Result<Self, String> {
        for (what, path) in [
            ("repo_root", entry.repo_root.as_path()),
            ("worktree", entry.worktree.as_path()),
        ]
        .into_iter()
        .chain(entry.archive.as_deref().map(|a| ("archive", a)))
        {
            if !plain_absolute(path) {
                return Err(format!(
                    "its {what} ({}) is not an absolute path of ordinary components",
                    path.display()
                ));
            }
        }
        let work = entry.repo_root.join(".jkb").join("work");
        if !entry.worktree.starts_with(&work) || entry.worktree == work {
            return Err(format!(
                "{} is not a session worktree under {}",
                entry.worktree.display(),
                work.display()
            ));
        }
        if let Some(dir) = &entry.archive {
            let root = archive_root(&entry.repo_root);
            if !dir.starts_with(&root) || *dir == root {
                return Err(format!(
                    "its archive {} is not under {}",
                    dir.display(),
                    root.display()
                ));
            }
        }
        Ok(Self(entry))
    }
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
            "no HEAD was recorded for it — an older record, or a checkout whose HEAD did not \
                    resolve when it was written — so it cannot be shown to be the tree the record \
                    describes"
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
    if entry.plan.accept_dirty {
        // The operator passed `--force`: they have already decided about whatever is in there,
        // and asking again would hold this record for ever over the very thing they answered.
        return Ok(());
    }
    match gitrepo::is_dirty(&entry.worktree) {
        // Proven clean, and nothing weaker. `is_dirty` used to spell "git ran and failed" as
        // `false`, so a worktree whose `.git` had been unlinked part-way — the very incident
        // this module exists to make unrepresentable — read as clean and was archived on the
        // strength of it. The arm below already refused when git could not be *executed*; this
        // is the same refusal for a git that executed and could not answer.
        Ok(f) if f.is_no() => Ok(()),
        Ok(f) if f.is_unknown() => Err(format!(
            "git could not say whether it has uncommitted changes — `git -C {} status` will \
             say what it makes of the directory",
            entry.worktree.display()
        )),
        Ok(_) => Err(match gitrepo::deletions_only(&entry.worktree) {
            // The third site of this rule, and the one that was still telling an operator to
            // commit 62,000 deleted lines. A tree that is only MISSING files is a part-way
            // removal, not work, and the two want opposite advice.
            Ok(Some(n)) => format!(
                "its {n} change(s) are deletions of tracked files and nothing else — a part-way \
                 removal, not work. `git -C {} restore .` puts them all back",
                entry.worktree.display()
            ),
            _ => "it has uncommitted changes — commit them, or drop the session with \
                  `jkb task abandon <uid> --force`, which records that you accept them"
                .to_owned(),
        }),
        Err(e) => Err(format!("git could not check it for changes: {e}")),
    }
}

/// Which pending record governs each worktree: the newest, by `recorded_at`, ties broken by
/// marker path so the choice is deterministic rather than whichever the directory listing yielded.
///
/// ONE DISPOSAL'S DECISION APPLIES. Giving each disposal its own marker fixed an archived record
/// being overwritten, and opened this: `abandon --delete-branch`, change your mind, `abandon`
/// again without it — two pending records for one tree, each carrying its own `Plan`, and the
/// older one still force-deletes the branch the later run printed "kept" for. That is exactly the
/// regression `Plan` was added to prevent, arriving by a different route.
///
/// ARCHIVED records are never superseded: each names a distinct archive that still has to be
/// swept, so they are a set of facts rather than competing instructions.
fn governing_pending(records: &[(PathBuf, Record)]) -> BTreeMap<PathBuf, PathBuf> {
    let mut best: BTreeMap<PathBuf, (u64, PathBuf)> = BTreeMap::new();
    for (marker, record) in records {
        if record.archive.is_some() {
            continue;
        }
        // Marker names sort in creation order (see `fresh_marker`), so the larger pair is the
        // later disposal even when the timestamps tie.
        let candidate = (record.recorded_at, marker.clone());
        match best.get(&record.worktree) {
            Some(current) if *current >= candidate => {}
            _ => {
                best.insert(record.worktree.clone(), candidate);
            }
        }
    }
    best.into_iter().map(|(k, (_, m))| (k, m)).collect()
}

/// Drop a pending record a later disposal of the same worktree replaced. Returns whether it did.
fn superseded(
    governing: &BTreeMap<PathBuf, PathBuf>,
    entry: &Entry,
    marker: &Path,
    dry_run: bool,
    report: &mut Report,
) -> bool {
    if entry.archive.is_some() {
        return false;
    }
    if governing
        .get(&entry.worktree)
        .is_none_or(|winner| winner == marker)
    {
        return false;
    }
    if !dry_run {
        let _ = fs::remove_file(marker);
    }
    report.superseded.push(entry.uid.clone());
    true
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
    let _lock = match SweepLock::acquire(db)? {
        Ok(lock) => lock,
        Err(held) => {
            return Ok(Report {
                skipped: Some(held),
                ..Report::default()
            })
        }
    };
    let now = now_secs();
    let store = entries(db)?;
    let mut report = Report {
        unreadable: store.unreadable,
        ..Report::default()
    };
    for r in store.rejected {
        report.held.push((
            r.uid,
            format!(
                "{} — refused; this record does not describe anything this sweep owns ({})",
                r.why,
                r.marker.display()
            ),
        ));
    }

    let governing = governing_pending(&store.records);

    for (marker, record) in store.records {
        let mut entry = record.clone_entry();
        if superseded(&governing, &entry, &marker, dry_run, &mut report) {
            continue;
        }
        // CAN THIS PROCESS SEE THE REPO AT ALL, before either arm looks at the record. Above the
        // dispatch because the condition dominates both — the pending arm had it and the archived
        // arm did not, so each side of the container bind destroyed the other's archived records:
        // `/Users/...` is not reachable in the container, the archived arm read that as "somebody
        // removed it by hand" and dropped the record, and the multi-gigabyte checkout it named was
        // then referenced by nothing and never deleted. An absent directory is evidence of removal
        // only when the repo it lives under is reachable in the first place (D45.5).
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
        // Already archived, so the only question left is whether it is old enough to delete.
        if let Some(dir) = entry.archive.clone() {
            sweep_archived(
                &dir,
                &entry,
                &marker,
                retain_days,
                now,
                dry_run,
                &mut report,
            );
            continue;
        }

        // Not archived yet: this is the deferred half of a landing. The repo is reachable by the
        // time we get here, so an absent worktree really is one somebody removed.
        if !entry.worktree.exists() {
            // Gone already. Tidy git's registration and the branch, then stop tracking.
            if !dry_run {
                let _ = gitrepo::prune_worktrees(&entry.repo_root);
                delete_branch_if_any(&entry);
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
                delete_branch_if_any(&entry);
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

/// The archived half of one record: keep it, or delete it once it is past the window.
///
/// Split from [`reap`] only for length; the containment check that makes acting on `dir` safe at
/// all runs in the caller, above both arms.
#[allow(clippy::too_many_arguments)]
fn sweep_archived(
    dir: &Path,
    entry: &Entry,
    marker: &Path,
    retain_days: u64,
    now: u64,
    dry_run: bool,
    report: &mut Report,
) {
    if !dir.exists() {
        // Somebody removed it by hand. Nothing owed, so stop tracking it.
        drop_marker(marker, dry_run, report, &entry.uid);
        return;
    }
    let age = now.saturating_sub(entry.archived_at.unwrap_or(now));
    if age < retain_days.saturating_mul(SECS_PER_DAY) {
        report.retained.push(dir.to_path_buf());
        return;
    }
    if dry_run {
        // Reported without probing: `removable` works by attempting `remove_dir`, which
        // succeeds on an empty directory — so asking the question during a dry run
        // deleted the very thing it was about to say it *would* delete.
        report.deleted.push(dir.to_path_buf());
        return;
    }
    if let Err(why) = removable(dir) {
        report.held.push((
            entry.uid.clone(),
            format!("{} cannot be deleted from here: {why}", dir.display()),
        ));
    } else if let Err(e) = fs::remove_dir_all(dir) {
        // The probe proved the unlink permitted, but a walk can still fail — a file
        // appearing mid-sweep, a device error. The record stays and the next sweep
        // tries again.
        report.held.push((
            entry.uid.clone(),
            format!("{} was not fully deleted: {e}", dir.display()),
        ));
    } else {
        report.deleted.push(dir.to_path_buf());
        drop_marker(marker, dry_run, report, "");
    }
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
fn delete_branch_if_any(entry: &Entry) {
    if entry.plan.delete_branch && !entry.branch.is_empty() {
        let _ = gitrepo::delete_branch(&entry.repo_root, &entry.branch, true);
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
            plan: Plan::default(),
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

    /// A repo root that cannot exist anywhere, for the cross-boundary cases.
    ///
    /// These used to name `/home/vscode/repos/jkb` — "a path the host cannot see" — which is
    /// exactly the bind target this change adds, so inside the container it EXISTS: the tests took
    /// the opposite arm, the gate was red in the environment the change exists to introduce, and
    /// two of them ran `git worktree prune` against the developer's real checkout. Unreachability
    /// has to be a property of the fixture, not an assumption about the machine.
    fn unreachable_repo(t: &tempfile::TempDir) -> PathBuf {
        t.path().join("never-created")
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
        assert_eq!(r.retained.len(), 1);
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

    /// A session that CAN be archived is archived, even when its HEAD cannot be read.
    ///
    /// The HEAD requirement was a precondition of the whole function, checked before `stow` was
    /// even attempted — so a disposal that would have succeeded outright was refused instead, and
    /// `jkb task abandon --force`'s `?` returned before the claim was released. That left the
    /// task `in_progress`, held by a session whose worktree still exists — so `owner::is_alive`
    /// says Yes, `doctor --fix` will not reclaim it, and re-running fails identically. `head` is
    /// read only to re-identify a DEFERRED tree; an archived record names its archive.
    #[test]
    fn a_session_whose_head_cannot_be_read_is_still_archived() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, _) = session(&repo, "sess");

        // A part-way `git worktree remove`: the directory is intact, its `.git` link is not.
        fs::remove_file(wt.join(".git")).expect("unlink .git");
        assert_eq!(
            gitrepo::worktree_head(&wt).expect("head"),
            None,
            "and the tree can no longer say what it is on"
        );

        let out = dispose(
            &db,
            &repo,
            &wt,
            &branch,
            "task:t",
            Plan {
                delete_branch: true,
                accept_dirty: false,
            },
        )
        .expect("an archivable session is archived, not refused");
        let Disposed::Archived(dest) = out else {
            panic!("stow can succeed here, so it must have")
        };
        assert!(dest.exists(), "the tree really moved");
        assert!(!wt.exists(), "and is no longer where it was");
    }

    #[test]
    fn a_deferred_disposal_records_the_head_the_worktree_is_actually_on() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, _) = session(&repo, "sess");

        // Force `stow` to fail without needing a sandbox: a regular file where the archive
        // directory must be created. Nothing moves, which is the deferred arm's whole premise.
        fs::write(repo.join(".jkb/archive"), b"in the way").expect("write");

        // The caller moves the branch before disposing — `land` resets it to what actually
        // landed. The record must name the commit the tree is on AFTERWARDS: recorded before,
        // the reaper finds a tree that is not on the commit the record names, concludes it is a
        // different session reusing the name, and holds it for ever.
        fs::write(wt.join("more.txt"), "landed").expect("write");
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "what landed"]);
        let after = git(&wt, &["rev-parse", "HEAD"]);

        let out = dispose(
            &db,
            &repo,
            &wt,
            &branch,
            "task:t",
            Plan {
                delete_branch: true,
                accept_dirty: false,
            },
        )
        .expect("dispose");
        assert!(
            matches!(out, Disposed::Deferred(_)),
            "nothing could be moved"
        );
        assert!(wt.exists(), "and nothing was");

        let records = entries(&db).expect("entries").records;
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].1.head.as_deref(),
            Some(after.as_str()),
            "the record names the commit the tree is on now"
        );

        // ...which is what lets the reaper act on it once the obstruction is gone.
        fs::remove_file(repo.join(".jkb/archive")).expect("rm");
        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(r.archived.len(), 1, "held for no reason: {:?}", r.held);
    }

    /// A sweep that only OBSERVED is empty, and what it observed is a stable fingerprint.
    ///
    /// The `--watch` service prints when `is_empty()` is false or the observation changed. A held
    /// record whose repo is on the other side of the container bind is permanently unreachable and
    /// permanently held, so counting it as activity made the service re-report and re-walk every
    /// retained archive every interval, for ever, about something no sweep will ever change.
    #[test]
    fn observing_is_not_acting_and_an_unchanged_observation_is_one_fingerprint() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        // A container-written record: this machine cannot reach the repo, so it is held for ever.
        let repo = unreachable_repo(&t);
        let e = Entry {
            head: Some("abc".into()),
            ..entry(&repo.join(".jkb/work/s"), &repo)
        };
        record(&db, &e).expect("record");

        let first = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(first.held.len(), 1, "it is held: {:?}", first.held);
        assert!(
            first.is_empty(),
            "but nothing was DONE, so the watcher has no action to report"
        );

        let second = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(
            first.observed(),
            second.observed(),
            "and an unchanged observation reads the same, so it is said once rather than hourly"
        );

        // A new thing to observe changes the fingerprint, so it is not silence either.
        fs::write(store_dir(&db).join("torn.json"), b"{not json").expect("write");
        let third = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_ne!(
            second.observed(),
            third.observed(),
            "something new to say must break the silence"
        );
    }

    #[test]
    fn releasing_the_lock_never_removes_a_successors() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let path = store_dir(&db).join(".sweep.lock");

        let Ok(displaced) = SweepLock::acquire(&db).expect("acquire") else {
            panic!("the lock is free to begin with")
        };

        // What `--break-lock` does. NOT an exotic path across the host/container bind: a foreign
        // holder is `Unknown` there by construction, so `jkb task reap` advises breaking it and
        // the operator is invited to do exactly this against a live sweeper.
        break_lock(&db).expect("break").expect("there was a holder");
        let Ok(successor) = SweepLock::acquire(&db).expect("acquire") else {
            panic!("the lock is free again once it has been broken")
        };

        // The displaced sweeper finishes and releases. `Drop` used to unlink unconditionally,
        // taking the successor's lock with it — after which two sweeps run at once, which is the
        // one thing this type exists to prevent.
        drop(displaced);
        assert!(
            path.exists(),
            "the successor still holds the lock after the displaced sweeper released"
        );
        assert_eq!(
            lock_holder(&db).expect("holder").as_deref(),
            Some(crate::owner::self_owner().as_str()),
            "and it is still the successor's"
        );

        drop(successor);
        assert!(!path.exists(), "the holder's own release does remove it");
    }

    /// Two acquisitions never share a release identity, whatever the filesystem does.
    ///
    /// The identity was the inode, and that is not safe: `break_lock` unlinks the file, freeing
    /// its inode, and on ext4 the just-freed bit in the parent group's bitmap is a prime
    /// candidate for the successor created an instant later — so the displaced sweeper's `Drop`
    /// would delete the successor's lock and two sweeps would run at once. APFS hands out
    /// monotonic inode numbers, which is the only reason the sibling test above passes here; a
    /// test asserting a property the local filesystem happens to provide is not a test of the
    /// code. The nonce is what makes it hold on both.
    #[test]
    fn two_acquisitions_never_share_a_release_identity() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");

        let Ok(first) = SweepLock::acquire(&db).expect("acquire") else {
            panic!("free to begin with")
        };
        let one = first
            .content
            .clone()
            .expect("the lock records what it wrote");
        break_lock(&db).expect("break").expect("there was a holder");
        let Ok(second) = SweepLock::acquire(&db).expect("acquire") else {
            panic!("free again once broken")
        };
        let two = second.content.clone().expect("and so does its successor");

        assert_ne!(
            one, two,
            "same process, same host, same pid — so an identity built from the owner id alone \
             cannot tell these apart, and one built from the inode cannot on a filesystem that \
             reuses them"
        );
        // The owner id is still what an operator is shown, nonce and all kept out of it.
        assert_eq!(
            lock_holder(&db).expect("holder").as_deref(),
            Some(crate::owner::self_owner().as_str()),
            "the nonce is a release identity, not part of who holds the lock"
        );
        drop(first);
        assert!(
            store_dir(&db).join(".sweep.lock").exists(),
            "and the displaced sweeper still does not take its successor's lock"
        );
        drop(second);
    }

    #[test]
    fn a_sweep_that_could_not_take_the_lock_says_so_rather_than_reporting_nothing_to_do() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        // A live holder, written the way `acquire` writes it. A BARE pid would pass this test
        // for the wrong reason: `owner::is_alive` cannot parse one, so it answers `Unknown` and
        // the lock is respected — which a garbage string achieves equally well. The id has to be
        // one the liveness probe actually recognises, or the test is about parsing, not liveness.
        fs::create_dir_all(store_dir(&db)).expect("mk");
        let lock = store_dir(&db).join(".sweep.lock");
        fs::write(&lock, crate::owner::self_owner()).expect("write");
        assert!(
            crate::owner::is_alive(&crate::owner::self_owner()).is_yes(),
            "the fixture's holder must be recognised as ALIVE, or this asserts nothing"
        );

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        let held = r
            .skipped
            .expect("it did not look, and that is not finding nothing");
        assert_eq!(
            held.holder,
            crate::owner::self_owner(),
            "and it names the holder, which is the only thing an operator can act on"
        );

        // ...and a holder PROVEN gone is taken over, or one crashed sweep wedges the machine's
        // reaper for ever. THIS host — a pid on another one is `Unknown`, which is respected, so
        // a foreign hostname here would assert the opposite of what this line says.
        let dead_holder = format!("{}:4294967290", crate::owner::hostname_for_test());
        assert!(
            crate::owner::is_alive(&dead_holder).is_no(),
            "the fixture's holder must be PROVEN gone, or this asserts nothing"
        );
        fs::write(&lock, &dead_holder).expect("write");
        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert!(r.skipped.is_none(), "a dead holder's lock is taken over");
    }

    #[test]
    fn a_record_written_before_the_plan_existed_still_reads_and_keeps_the_branch() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        fs::create_dir_all(store_dir(&db)).expect("mk");
        // Exactly the shape this shipped with before `Plan`: no `delete_branch`, no
        // `accept_dirty`. It has to parse, and it has to default to the SAFE answers — keep the
        // branch, do not touch a dirty tree — because the record cannot say what was decided.
        fs::write(
            store_dir(&db).join("legacy.json"),
            br#"{"worktree":"/r/.jkb/work/s","repo_root":"/r","branch":"task/s",
                 "uid":"task:t","recorded_at":1,"head":"abc","archive":null,"archived_at":null}"#,
        )
        .expect("write");

        let store = entries(&db).expect("entries");
        assert!(store.unreadable.is_empty(), "an older record still parses");
        assert_eq!(store.records.len(), 1);
        let plan = store.records[0].1.plan;
        assert!(
            !plan.delete_branch,
            "a record that cannot say defaults to keeping the branch"
        );
        assert!(!plan.accept_dirty, "and to not touching uncommitted work");
    }

    #[test]
    fn a_record_round_trips_through_the_store() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let e = Entry {
            plan: Plan {
                delete_branch: true,
                accept_dirty: true,
            },
            head: Some("cafe".into()),
            ..entry(Path::new("/r/.jkb/work/s"), Path::new("/r"))
        };
        record(&db, &e).expect("record");
        let back = entries(&db).expect("entries").records.pop().expect("one").1;
        assert!(
            back.plan.delete_branch && back.plan.accept_dirty,
            "the plan survives the store"
        );
        assert_eq!(back.head.as_deref(), Some("cafe"));
        assert_eq!(back.worktree, e.worktree);
    }

    #[test]
    fn the_sweep_applies_the_plan_the_verb_recorded_not_lands_defaults() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");
        // `jkb task abandon` without --delete-branch: the branch holds the only copy of the work,
        // and the verb tells the operator it kept it. The sweep must not contradict that.
        let e = Entry {
            plan: Plan {
                delete_branch: false,
                accept_dirty: false,
            },
            ..session_entry(&repo, &wt, &branch, &head)
        };
        record(&db, &e).expect("record");

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(
            r.archived.len(),
            1,
            "the tree is still archived: {:?}",
            r.held
        );
        assert!(
            gitrepo::rev(&repo, &branch).expect("rev").is_some(),
            "but the branch the verb said it kept is still there"
        );
    }

    #[test]
    fn an_accepted_dirty_tree_does_not_hold_the_sweep_for_ever() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");
        // `--force` is the operator saying they accept what is uncommitted in there. Asking again
        // would hold this record for ever over the very question they answered.
        let e = Entry {
            plan: Plan {
                delete_branch: true,
                accept_dirty: true,
            },
            ..session_entry(&repo, &wt, &branch, &head)
        };
        record(&db, &e).expect("record");
        fs::write(wt.join("scratch.txt"), "unsaved").expect("write");

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(r.archived.len(), 1, "held anyway: {:?}", r.held);
        assert!(
            r.archived[0].1.join("scratch.txt").exists(),
            "and the uncommitted work went into the archive rather than being destroyed"
        );
    }

    #[test]
    fn a_record_can_be_cancelled_while_it_is_still_pending() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");
        record(&db, &session_entry(&repo, &wt, &branch, &head)).expect("record");

        assert!(
            revoke(&db, &wt).expect("revoke"),
            "a pending record is cancellable"
        );
        assert!(entries(&db).expect("entries").records.is_empty());
        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert!(r.archived.is_empty(), "and the live session is left alone");
        assert!(wt.exists());

        // ...but once the tree has been moved the record is what says where it went, so it is
        // the retention sweep's and cancelling it would strand the archive.
        record(&db, &session_entry(&repo, &wt, &branch, &head)).expect("record");
        reap(&db, RETAIN_DAYS, false).expect("reap");
        assert!(
            !revoke(&db, &wt).expect("revoke"),
            "an archived record is not cancellable"
        );
        assert_eq!(entries(&db).expect("entries").records.len(), 1);
    }

    #[test]
    fn a_traversal_out_of_the_archive_root_is_refused_not_followed() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        fs::create_dir_all(&repo).expect("mk");
        let victim = t.path().join("Documents");
        fs::create_dir_all(victim.join("taxes")).expect("mk");
        fs::write(victim.join("taxes/2025.pdf"), b"important").expect("write");

        // `Path::starts_with` compares components WITHOUT interpreting them, so this "starts
        // with" the archive root while naming somewhere else entirely. The first version of the
        // containment guard accepted it.
        let traversal = archive_root(&repo)
            .join("..")
            .join("..")
            .join("..")
            .join("Documents");
        let e = Entry {
            archive: Some(traversal),
            archived_at: Some(1),
            head: Some("abc".into()),
            ..entry(&repo.join(".jkb/work/s"), &repo)
        };
        fs::create_dir_all(store_dir(&db)).expect("mk");
        fs::write(
            store_dir(&db).join("hostile.json"),
            serde_json::to_vec(&e).expect("json"),
        )
        .expect("write");

        let store = entries(&db).expect("entries");
        assert!(
            store.records.is_empty(),
            "the sweep is never handed a record it has not proven"
        );
        assert_eq!(store.rejected.len(), 1, "it is refused, with the reason");

        let r = reap(&db, 0, false).expect("reap");
        assert!(r.deleted.is_empty());
        assert!(
            victim.join("taxes/2025.pdf").exists(),
            "and the directory it aimed at is untouched"
        );
        assert_eq!(
            entries(&db).expect("entries").rejected.len(),
            1,
            "a refusal is not a licence to delete the file either"
        );
    }

    #[test]
    fn an_unreachable_repo_holds_an_archived_record_too() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        // One side writes a record the other cannot reach — the container's `/home/vscode/…` swept
        // on the host, or the reverse. The archived arm used to read "not visible from here" as
        // "somebody removed it by hand" and drop the record, so each side destroyed the other's and
        // the checkout it named was referenced by nothing and never deleted.
        let repo = unreachable_repo(&t);
        let e = Entry {
            archive: Some(repo.join(".jkb/archive/s-19700101T000000Z")),
            archived_at: Some(1),
            head: Some("abc".into()),
            ..entry(&repo.join(".jkb/work/s"), &repo)
        };
        record(&db, &e).expect("record");

        let r = reap(&db, 0, false).expect("reap");
        assert!(r.cleared.is_empty(), "an unreachable repo settles nothing");
        assert_eq!(r.held.len(), 1, "{:?}", r.held);
        assert_eq!(
            entries(&db).expect("entries").records.len(),
            1,
            "the record survives for whichever side can see the repo"
        );
    }

    /// The tie the first version got backwards: two disposals in the SAME second.
    ///
    /// `governing_pending` breaks a `recorded_at` tie on the marker, and the first naming scheme
    /// left the first file bare while suffixing the rest — so `slug-hash-2.json` sorted below
    /// `slug-hash.json` and the older, withdrawn plan won the vote. Marker names now sort in
    /// creation order, which is what makes the comparison mean what it says.
    #[test]
    fn a_same_second_redisposal_is_still_governed_by_the_later_one() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");

        let stamp = now_secs();
        let mut first = session_entry(&repo, &wt, &branch, &head);
        first.plan.delete_branch = true;
        first.recorded_at = stamp;
        record(&db, &first).expect("first");
        let mut second = session_entry(&repo, &wt, &branch, &head);
        second.plan.delete_branch = false;
        second.recorded_at = stamp; // the same second, so only the marker order can decide
        record(&db, &second).expect("second");

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(r.superseded.len(), 1, "one of them is withdrawn: {r:?}");
        assert!(
            gitrepo::rev(&repo, &branch).expect("rev").is_some(),
            "and it is the FIRST — the later disposal said it kept the branch"
        );
    }

    #[test]
    fn a_withdrawn_decision_does_not_still_delete_the_branch() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");

        // `abandon --delete-branch` defers, then the operator changes their mind and abandons
        // again without it. Two pending records for one tree, each carrying its own Plan — and
        // the older one would still force-delete the branch the later run printed "kept" for.
        let mut first = session_entry(&repo, &wt, &branch, &head);
        first.plan.delete_branch = true;
        first.recorded_at = 1;
        record(&db, &first).expect("first");
        let mut second = session_entry(&repo, &wt, &branch, &head);
        second.plan.delete_branch = false;
        second.recorded_at = 2;
        record(&db, &second).expect("second");

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert_eq!(
            r.superseded.len(),
            1,
            "the withdrawn decision is dropped: {r:?}"
        );
        assert_eq!(r.archived.len(), 1, "and the tree is archived once");
        assert!(
            gitrepo::rev(&repo, &branch).expect("rev").is_some(),
            "the branch the LATER disposal said it kept is still there"
        );
        assert_eq!(
            entries(&db).expect("entries").records.len(),
            1,
            "one record survives, and it is the archived one"
        );
    }

    #[test]
    fn two_archived_records_for_one_worktree_are_both_kept() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let wt = repo.join(".jkb/work/sess");
        // Archived records are facts, not competing instructions: each names a distinct archive
        // that still has to be swept, so superseding one would strand it for ever.
        // Fresh, or the retention arm deletes them and there is nothing left to be retained —
        // which is a true outcome for a 1970 timestamp and not what this test is about.
        let fresh = now_secs();
        for (n, stamp) in [(0u64, fresh), (1, fresh + 1)] {
            let e = Entry {
                archive: Some(archive_root(&repo).join(format!("sess-{n}"))),
                archived_at: Some(stamp),
                head: Some("abc".into()),
                recorded_at: stamp,
                ..entry(&wt, &repo)
            };
            fs::create_dir_all(e.archive.as_ref().expect("set")).expect("mk");
            record(&db, &e).expect("record");
        }

        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert!(r.superseded.is_empty(), "neither archive is superseded");
        assert_eq!(r.retained.len(), 2, "both are tracked to their retention");
    }

    #[test]
    fn a_second_disposal_of_one_session_does_not_overwrite_the_first_record() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let wt = repo.join(".jkb/work/sess");
        // A session name is REUSED: abandon, reopen the task, `task work` mints the same name at
        // the same path. Keyed on that path, the second disposal's record landed on the first's
        // and the first archive became unreferenced — never swept, never reported, permanent.
        let first = Entry {
            archive: Some(archive_root(&repo).join("sess-19700101T000000Z")),
            archived_at: Some(1),
            head: Some("aaa".into()),
            ..entry(&wt, &repo)
        };
        let second = Entry {
            archive: Some(archive_root(&repo).join("sess-19700101T000001Z")),
            archived_at: Some(2),
            head: Some("bbb".into()),
            ..entry(&wt, &repo)
        };
        record(&db, &first).expect("first");
        record(&db, &second).expect("second");

        let kept = entries(&db).expect("entries").records;
        assert_eq!(kept.len(), 2, "both disposals are tracked");
        let mut archives: Vec<String> = kept
            .iter()
            .filter_map(|(_, r)| r.archive.as_ref().map(|a| a.display().to_string()))
            .collect();
        archives.sort();
        assert!(
            archives[0].ends_with("sess-19700101T000000Z")
                && archives[1].ends_with("sess-19700101T000001Z"),
            "neither archive is orphaned: {archives:?}"
        );
    }

    #[test]
    fn a_record_pointing_outside_the_repo_deletes_nothing() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        fs::create_dir_all(&repo).expect("mk");
        // Somebody's documents. The record store is inside the `~/.jkb` bind, which the container
        // can write and the posture grants — and the host's reaper runs outside every sandbox.
        let victim = t.path().join("Documents");
        fs::create_dir_all(victim.join("taxes")).expect("mk");
        fs::write(victim.join("taxes/2025.pdf"), b"important").expect("write");

        let e = Entry {
            archive: Some(victim.clone()),
            // Old enough that the retention window is long past.
            archived_at: Some(1),
            head: Some("abc".into()),
            ..entry(&repo.join(".jkb/work/s"), &repo)
        };
        record(&db, &e).expect("record");

        let r = reap(&db, 0, false).expect("reap");
        assert!(r.deleted.is_empty(), "nothing outside the repo is deleted");
        assert_eq!(r.held.len(), 1, "it is held, with the reason: {:?}", r.held);
        assert!(
            victim.join("taxes/2025.pdf").exists(),
            "and the directory it named is untouched"
        );
    }

    #[test]
    fn a_record_naming_a_worktree_outside_jkb_work_is_refused() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        fs::create_dir_all(&repo).expect("mk");
        let elsewhere = t.path().join("not-a-session");
        fs::create_dir_all(&elsewhere).expect("mk");
        fs::write(elsewhere.join("f"), b"x").expect("write");

        let e = Entry {
            head: Some("abc".into()),
            ..entry(&elsewhere, &repo)
        };
        record(&db, &e).expect("record");

        // REFUSED BY `Record::parse`, and the assertion says so — this used to check only that
        // exactly one record was held, which `still_the_recorded_session` satisfies on its own
        // (a path outside `.jkb/work` is no registered worktree either), so the one test pinning
        // the bound on what `stow`'s `fs::rename` may pick up passed with the containment half of
        // the parser deleted. The refusal has its own wording; ask for it.
        let r = reap(&db, RETAIN_DAYS, false).expect("reap");
        assert!(r.archived.is_empty(), "and nothing is moved either");
        assert_eq!(r.held.len(), 1, "{:?}", r.held);
        assert!(
            r.held[0]
                .1
                .contains("does not describe anything this sweep owns"),
            "held for containment, not for some other reason: {:?}",
            r.held[0]
        );
        assert_eq!(
            entries(&db).expect("entries").rejected.len(),
            1,
            "and it never becomes a Record at all"
        );
        assert!(elsewhere.join("f").exists());
    }

    #[test]
    fn a_dry_run_does_not_delete_an_empty_archive_through_its_own_probe() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        // `removable` works by ATTEMPTING `remove_dir`, which succeeds on an empty directory — so
        // asking the question during a dry run deleted the thing it was about to say it *would*.
        let dir = archive_root(&repo).join("s-19700101T000000Z");
        fs::create_dir_all(&dir).expect("mk");
        let e = Entry {
            archive: Some(dir.clone()),
            archived_at: Some(1),
            head: Some("abc".into()),
            ..entry(&repo.join(".jkb/work/s"), &repo)
        };
        record(&db, &e).expect("record");

        let r = reap(&db, 0, true).expect("reap");
        assert_eq!(r.deleted, vec![dir.clone()], "it says what it would delete");
        assert!(dir.exists(), "and a dry run deleted nothing");
    }

    #[test]
    fn cancelling_refuses_while_a_sweep_holds_the_lock() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");
        record(&db, &session_entry(&repo, &wt, &branch, &head)).expect("record");

        // A sweep already in flight is working from a snapshot taken before this cancellation:
        // it would archive the checkout `task work` is at that moment handing back, and re-write
        // the record this deleted. Refusing is the honest outcome — the caller says so and the
        // operator re-runs — because a cancellation that silently lost the race is worse.
        fs::write(
            store_dir(&db).join(".sweep.lock"),
            crate::owner::self_owner(),
        )
        .expect("write");

        assert!(
            revoke(&db, &wt).is_err(),
            "a cancellation that cannot exclude the sweep must say so, not report success"
        );
        assert_eq!(
            entries(&db).expect("entries").records.len(),
            1,
            "and the record it could not safely remove is still there"
        );
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
        // The ordinary cross-boundary case: a landing recorded on one side of the container bind,
        // swept from the other, where the repo it names is not reachable.
        let repo = unreachable_repo(&t);
        let e = Entry {
            head: Some("deadbeef".into()),
            ..entry(&repo.join(".jkb/work/s"), &repo)
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
