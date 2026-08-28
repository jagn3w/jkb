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
use jkb_fsm::Fact;
use serde::{Deserialize, Serialize};

use crate::gitrepo;
use crate::presence;

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
    /// `(uid, why)` for a `delete_branch` plan that was NOT carried out because its premise had
    /// stopped holding. Reported rather than silent: a plan the operator asked for and that jkb
    /// declined to apply is exactly the thing they must be told about, and the alternative — a
    /// silently surviving branch — reads as a jkb bug rather than as a deliberate refusal.
    pub kept_branches: Vec<(String, String)>,
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

/// A disposal nobody could carry out here, and what will become of the record it left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deferral {
    /// Why this process could not move the tree.
    pub why: String,
    /// What a later sweep will do with the record — the SAME answer `jkb task reap` executes.
    pub verdict: Verdict,
}

impl Deferral {
    /// The sentence to print after "it could not be archived from here": either the promise that
    /// something else will finish it, or the reason nothing will and what to do instead.
    #[must_use]
    pub fn outlook(&self) -> String {
        match &self.verdict {
            Verdict::Stow => {
                "it is recorded for `jkb task reap`, which the watcher service runs".to_owned()
            }
            Verdict::DropRecord => "the checkout is already gone, so nothing is owed".to_owned(),
            // NEVER a promise here. This is the case the reports used to describe as work in
            // hand, while the sweep could not touch it.
            Verdict::Hold(b) => format!(
                "it is recorded, but `jkb task reap` will not be able to finish it: {} — {}",
                b.reason,
                b.remedy.advice()
            ),
        }
    }

    /// Whether a later sweep will actually act on the record — what a promise may be made about.
    #[must_use]
    pub fn will_be_swept(&self) -> bool {
        matches!(self.verdict, Verdict::Stow | Verdict::DropRecord)
    }
}

/// What became of a worktree this process tried to dispose of.
pub enum Disposed {
    /// Moved into the repo's archive.
    Archived(PathBuf),
    /// Nothing moved — this process may not unlink the tree — and a record was left for the
    /// reaper. Carries the refusal, because "it did not work" without the reason is what makes an
    /// operator go looking in the wrong place.
    Deferred(Deferral),
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
    let head = gitrepo::worktree_head(worktree, repo_root)
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
            // A MISSING IDENTITY IS RECORDED, NOT REFUSED — and the reasoning that made this a
            // refusal for one round was right about the cost and wrong about who pays it.
            //
            // It is true that a record with no head cannot be re-identified, so the sweep will
            // hold it. But refusing does not avoid that: it leaves the same unarchivable tree on
            // disk with no record at all, AND takes the caller's plan down with it. `abandon`
            // calls this through `?`, so the bail returned before the `write_txn` that releases
            // the claim — leaving the task `in_progress`, held by a session whose worktree still
            // exists, so `owner::is_alive` says Yes and `doctor --fix` will not reclaim it, and
            // every re-run of the printed remedy hit the same bail. That is the exact wedge the
            // comment above says the requirement was moved out of the preamble to avoid; moving
            // it here fixed the archived path and left it on the deferred one, which in the
            // container is the ORDINARY path (D49: a session cannot archive its own checkout).
            //
            // Recording is strictly better in every direction: the tree is remembered rather than
            // forgotten, the task is freed, and the state terminates — `still_the_recorded_session`
            // names the one action that resolves it, and once the operator removes the directory
            // the sweep's absent-worktree arm prunes git's registration, applies `plan`'s branch
            // deletion and drops the record. A refusal must leave something worth preserving, and
            // here there is nothing: the checkout can no longer answer git for itself, and the
            // operator has already said `--force`.
            if let Err(rec) = record(db, &entry) {
                eprintln!(
                    "note: {} could not be archived from here ({e}) and the removal record could \
                     not be written either ({rec}) — remove the directory by hand",
                    worktree.display()
                );
            }
            // THE VERDICT ON THE RECORD JUST WRITTEN, so the caller's report is derived rather
            // than assumed. Every verb used to print that `jkb task reap` would finish this —
            // and for a tree that could not answer git for itself, no sweep could. A promise
            // about another process's future behaviour has to come from the thing that decides
            // it.
            let verdict = pending_verdict(&entry);
            Ok(Disposed::Deferred(Deferral {
                why: e.to_string(),
                verdict,
            }))
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
    /// So a per-acquisition **nonce** goes in the file beside the owner id, and `content` is the
    /// whole line as it was read back off disk.
    ///
    /// NOT an `Option`. It was one — `None` when the read-back failed — and that spelled "this
    /// lock has no release identity", which `Drop` could only honour by leaving the file behind
    /// at the end of a sweep that had run perfectly. The next acquisition then reads an owner id
    /// of `""`, which `owner::is_alive` cannot recognise and so answers `Fact::Unknown`, which is
    /// never `is_no()` — so the lock is held for ever by nobody and every later sweep no-ops,
    /// breakable only by a person running `--break-lock`. A lock that cannot say what it wrote is
    /// not a lock, so `acquire` removes the file and refuses instead of returning one.
    content: String,
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
                    let Some(content) = content else {
                        // Removing it is safe for the reason that makes this arm reachable at
                        // all: `create_new` succeeded, so this acquisition is the only party
                        // that can be holding this file, and the identity never landed, so
                        // nothing else could recognise it either. Leaving it is the one option
                        // that costs something — see the field's doc.
                        let _ = fs::remove_file(&path);
                        anyhow::bail!(
                            "could not establish a sweep lock at {} — it was created, but what \
                             was written to it could not be read back, so releasing it later \
                             could not be told from releasing whoever holds it by then. The file \
                             has been removed and nothing was swept; try again.",
                            path.display()
                        );
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
                // THE THIRD READER, and it was the one not routed through the parser. `holder_of`
                // says "ONE parser, because there are two readers and they must not disagree";
                // there were three. `--dry-run --break-lock` goes through `lock_holder` and
                // prints a clean owner id, while `--break-lock` came through here and printed the
                // same identity with the nonce glued on — two spellings of one id in adjacent
                // lines of one command, and the second is not something
                // `jkb task release --owner` will match.
                Ok(Some(holder_of(&holder).to_owned()))
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
        let mine = self.content.as_str();
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

/// How the tree at `entry.worktree` compares with the identity the record carries.
///
/// A path and a branch are reusable names; establishing that the tree is the one the record
/// describes is what makes acting on it safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityMatch {
    /// The tree answers for itself and is on the commit the record names.
    Matches,
    /// The tree is there and cannot answer git for itself: a part-way `git worktree remove`, the
    /// failure this module exists to replace.
    ///
    /// **Independent of whether the record carries a head**, which is the correction. `head: None`
    /// is not a missing field — it is the observation *when I saw this tree it could not say what
    /// it was on* — but a record made BEFORE the tree broke carries a head and describes the same
    /// thing today. Deciding by which field the record happens to hold gave the better-informed
    /// record the worse outcome.
    ///
    /// It is stowable. Identity can never be established here, so any hold is permanent, and the
    /// alternative on offer was telling an operator to delete a checkout that may hold work.
    Wreck,
    /// The tree answers, and says something else — a different session reusing the name, or a
    /// wreck somebody repaired.
    DifferentSession,
    /// Nothing could be established either way.
    Unestablished,
}

/// What the sweep will do with one PENDING record — the one answer, executed by the sweep and
/// rendered by every verb that promises something about it.
///
/// The choke point exists because the promises and the behaviour diverged: `report_landing`,
/// `report_abandon`, `BranchFate::OwedToTheReaper` and `jkb doctor` each hand-wrote a claim that
/// `jkb task reap` would finish the job, while the sweep bailed before it could. That is the
/// disease `staging::land_blocker` was written to cure one area over — "**THE** rule, in one
/// place, so the command and the row cannot describe different rules". Same cure here.
///
/// Deliberately only about a pending record. An archived one is a different and much simpler
/// question — containment plus age — and no verb makes a promise about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Move it into the archive: a lossless rename, reversible for the retention window.
    Stow,
    /// Nothing is owed. Prune git's registration, apply the branch plan, stop tracking it.
    DropRecord,
    /// Held, with the reason and the action that changes it.
    Hold(Blocked),
}

/// Why a record is held, and the one thing that unblocks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// What was observed, in a sentence.
    pub reason: String,
    /// What to do about it. A CLOSED set, so a hold cannot be reported with advice nobody
    /// checked — the failure D48 records as "a printed remedy whose obvious argument froze the
    /// task permanently", found twice, one message apart.
    pub remedy: Remedy,
}

/// Declares [`Remedy`] and, beside each variant, one sample of it.
///
/// **The closed-set claim, made true instead of asserted.** The audits below walk a list of every
/// remedy, and that list was written by hand and checked against a second hand-written list of
/// names — so adding a variant satisfied both (neither is tied to the enum) and skipped the audit
/// entirely. Which is exactly what happened the round before this one, in the helper whose doc
/// claimed the opposite. Here the sample is part of the declaration, so a variant cannot exist
/// without one; the precedent is `jkb_core::changelog::Entity`, whose variants and `Entity::ALL`
/// come out of one macro for the same reason.
macro_rules! remedies {
    (
        $(#[$emeta:meta])*
        $name:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident $({ $($field:ident : $ty:ty),* $(,)? })? = $sample:expr
            ),* $(,)?
        }
    ) => {
        $(#[$emeta])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum $name {
            $( $(#[$vmeta])* $variant $({ $($field: $ty),* })? ),*
        }

        #[cfg(test)]
        impl $name {
            /// One of every variant, derived from the declaration — never a second list.
            fn every() -> Vec<$name> {
                vec![ $($sample),* ]
            }
        }
    };
}

remedies! {
/// The actions that resolve a hold. Rendered in exactly one place, [`Remedy::advice`].
Remedy {
    /// Another process finishes it: the watcher service, or `jkb task reap` run from a terminal
    /// outside the session that made the record.
    RunReap = Remedy::RunReap,
    /// The tree holds work nobody has committed.
    CommitOrForce { uid: String } = Remedy::CommitOrForce { uid: "task:t".to_owned() },
    /// The record cannot be checked against the tree. Disposing again writes a fresh record with
    /// an identity that can be, and `governing_pending` makes the newer one authoritative — so
    /// this supersedes rather than accumulating.
    ///
    /// **Only offered for a tree git still registers.** `abandon` reaches a session through
    /// [`crate::session::discover`], which walks `git worktree list` — so for an unregistered
    /// directory it finds nothing, writes no record, and this advice changes nothing at all. That
    /// is [`RemoveByHand`](Self::RemoveByHand)'s case.
    Redispose { uid: String } = Remedy::Redispose { uid: "task:t".to_owned() },
    /// A part-way removal, recoverable in full.
    RestoreTree { worktree: PathBuf } = Remedy::RestoreTree {
        worktree: PathBuf::from("/repo/.jkb/work/s"),
    },
    /// The tree cannot be identified AND git no longer registers it, so no jkb verb can reach it:
    /// `abandon` discovers sessions from `git worktree list` and would find nothing to re-record.
    /// The operator's own look at the directory is the only thing that ends this.
    ///
    /// Advising removal is the thing D34.4 warns against, and it is right only here — where the
    /// tree cannot be shown to be ours, so the reversible alternative (`stow`) is not on offer.
    /// Everywhere it is on offer, it wins.
    RemoveByHand { worktree: PathBuf } = Remedy::RemoveByHand {
        worktree: PathBuf::from("/repo/.jkb/work/s"),
    },
    /// This machine cannot stat the directory — EACCES on a parent component is the usual cause,
    /// and a uid-mismatched `.jkb/work` across the container bind is how it arises here.
    FixPermissions { worktree: PathBuf } = Remedy::FixPermissions {
        worktree: PathBuf::from("/repo/.jkb/work/s"),
    },
    /// `git` did not answer in this repo at all, so nothing about the tree is established.
    FixGitAccess { repo_root: PathBuf } = Remedy::FixGitAccess {
        repo_root: PathBuf::from("/repo"),
    },
    /// Git answered ABOUT THE REPO and said this path is not one of its worktrees, and the
    /// directory will not answer for itself either — so nothing here can say what it is, and no
    /// jkb verb can reach it (`abandon` discovers sessions from `git worktree list`).
    ///
    /// Distinct from [`FixGitAccess`](Self::FixGitAccess), which claims git is silent: here git
    /// spoke, and saying otherwise sent the operator to check a command that had already answered.
    /// Distinct from [`RemoveByHand`](Self::RemoveByHand) too — that one is licensed by the tree
    /// having ANSWERED and named itself a stranger. The way out here is `git worktree repair`,
    /// which is non-destructive and re-links a checkout whose administrative file was lost.
    InspectByHand { worktree: PathBuf } = Remedy::InspectByHand {
        worktree: PathBuf::from("/repo/.jkb/work/s"),
    },
    /// Nothing on this machine can act: the repo is not reachable from here.
    NoActionFromHere = Remedy::NoActionFromHere,
}
}

impl Remedy {
    /// The advice, in one place, so two verbs cannot word one action differently.
    #[must_use]
    pub fn advice(&self) -> String {
        match self {
            Self::RunReap => {
                "`jkb task reap` from a terminal outside the session will finish it".to_owned()
            }
            Self::CommitOrForce { uid } => format!(
                "commit them in the session, or run `jkb task abandon {uid} --force`, which \
                 records that you accept them"
            ),
            Self::Redispose { uid } => format!(
                "run `jkb task abandon {uid} --force` — that records the tree afresh, with an \
                 identity the sweep can check, and supersedes this record"
            ),
            Self::RestoreTree { worktree } => format!(
                "`git -C {} restore .` puts them all back",
                worktree.display()
            ),
            Self::RemoveByHand { worktree } => format!(
                "git no longer registers it, so no jkb verb can reach it — look at {} and remove \
                 it yourself; the next sweep then finishes the disposal it is owed",
                worktree.display()
            ),
            Self::FixPermissions { worktree } => format!(
                "this machine cannot read {} — check the permissions on it and its parents, then \
                 re-run",
                worktree.display()
            ),
            Self::FixGitAccess { repo_root } => format!(
                "git did not answer in {} — check `git -C {} worktree list`; the record is kept \
                 until it can",
                repo_root.display(),
                repo_root.display()
            ),
            Self::InspectByHand { worktree } => format!(
                "git lists no worktree at {} and the directory will not answer for itself, \
                 so nothing here can say what it is — `git -C {} worktree repair` re-links \
                 a checkout whose administrative file was lost; if it is not a checkout you \
                 want, move it out of `.jkb/work` yourself. The record is kept until it \
                 changes.",
                worktree.display(),
                worktree.display()
            ),
            Self::NoActionFromHere => {
                "nothing here can act on it; run it where that repo is checked out".to_owned()
            }
        }
    }

    /// Does following this advice destroy something?
    ///
    /// Exactly one does, and it earns its own predicate because **the exit audit cannot police
    /// this**. That audit asks whether a hold ESCAPES; an operator deleting a live checkout is a
    /// perfectly good escape by its lights, so a destructive remedy offered on a guess would pass
    /// it. D34.4 — of the two ways to be wrong, the one that costs a command wins — has to be
    /// asserted separately, over what was PROVEN rather than over what changes.
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        match self {
            Self::RemoveByHand { .. } => true,
            Self::RunReap
            | Self::CommitOrForce { .. }
            | Self::Redispose { .. }
            | Self::RestoreTree { .. }
            | Self::FixPermissions { .. }
            | Self::FixGitAccess { .. }
            // Its primary advice is `git worktree repair`, which destroys nothing; moving a
            // directory aside is offered as the operator's own judgement after looking, which is
            // not the same as jkb telling them to delete a checkout it cannot identify.
            | Self::InspectByHand { .. }
            | Self::NoActionFromHere => false,
        }
    }
}

/// Everything the verdict is decided from — gathered once, so the decision is a pure function of
/// what was seen and can be exercised over its whole product in a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// Is the worktree there, anchored on the repo root (see [`crate::presence`])? Carries the
    /// CAUSE of an unestablished answer, because the two causes have opposite remedies.
    pub present: presence::Presence,
    /// Does git still register that path as a worktree of this repo? A [`Fact`], because
    /// `git worktree list` can fail to answer — and `false` is the value that selects the one
    /// destructive remedy in the set.
    pub registered: Fact,
    /// How the tree compares with the recorded identity.
    pub identity: IdentityMatch,
    /// Uncommitted work in the tree.
    pub dirty: Fact,
    /// What the dirt is made of, when there is any.
    pub deletions: gitrepo::Deletions,
}

/// Look at the tree a pending record names.
///
/// **Every field is as fine-grained as the remedies keyed on it.** That is the rule this function
/// kept breaking: `verdict_pending` is pure and its whole product is walked by an audit, so any
/// two world-states that need different advice must be *distinguishable in `Observed`* — a
/// distinction collapsed here is one the audit is constitutionally unable to see. Three separate
/// must-fixes were one collapse each: `worktree_head` spelling wreck and absent alike, `worktrees`
/// spelling a failed git as "not registered", and `Presence` spelling two causes as one `Unknown`.
fn observe_pending(entry: &Entry) -> Observed {
    let present = presence::present_under(&entry.worktree, &entry.repo_root);
    let registered = match gitrepo::worktrees(&entry.repo_root) {
        // Canonical comparison: git reports `/private/var/...` where the record holds
        // `/var/...` on macOS, and a raw equality answers "not registered" for the very
        // directory you are standing in.
        Ok(Some(ws)) => Fact::from(
            ws.iter()
                .any(|w| crate::session::same_path(&w.path, &entry.worktree)),
        ),
        // Git did not answer — never `No`, which is what licenses telling somebody to delete a
        // checkout.
        Ok(None) | Err(_) => Fact::Unknown,
    };
    // ONE question, asked once: what is this tree? HEAD is consulted only where it can mean
    // anything — `worktree_identity == Own`, i.e. the tree answers for itself. Asking
    // `worktree_head` first (which it did, whenever the record carried a head) threw the
    // `Foreign` answer away, because that function returns `Ok(None)` for a wreck AND for a
    // vanished tree: the record carrying MORE information got the WORSE verdict, and a wreck that
    // had also been unregistered by a routine `git worktree prune` was met with "remove it
    // yourself" where the head-less record for the identical state was stowed reversibly.
    match gitrepo::worktree_identity(&entry.worktree, &entry.repo_root) {
        Ok(gitrepo::WorktreeIdentity::Own) => {
            // `rev(dir, "HEAD")` is not usable here and `worktree_head` is: git's discovery walks
            // up, so for a tree that does not answer for itself `rev` reports the MAIN checkout's
            // HEAD. A field's reader and its writer must answer about the same tree.
            let head = gitrepo::worktree_head(&entry.worktree, &entry.repo_root);
            let identity = match (entry.head.as_deref(), head) {
                (Some(want), Ok(Some(h))) if h == want => IdentityMatch::Matches,
                (_, Ok(Some(_))) => IdentityMatch::DifferentSession,
                (_, Ok(None) | Err(_)) => IdentityMatch::Unestablished,
            };
            finish_observation(entry, present, registered, identity)
        }
        // A WRECK, whether or not the record carries a head. A tree that cannot answer git can
        // neither prove nor refute an identity, ever — so holding for one is a permanent hold,
        // and `stow` is a lossless rename within our own `.jkb/` (`Record::parse` has already
        // constrained the path to `<repo>/.jkb/{work,archive}`).
        //
        // ESTABLISHED, and only established: `Foreign` now means git answered and named an
        // enclosing repo. It used to also cover "git would not answer", which is how a repo under
        // `fatal: detected dubious ownership` had every deferred checkout beneath it renamed away
        // with the dirty check waived. That answer is `Unestablished` below, where it holds.
        Ok(gitrepo::WorktreeIdentity::Foreign) => {
            finish_observation(entry, present, registered, IdentityMatch::Wreck)
        }
        // `Absent` is `present`'s business, not identity's; `Unestablished` is git declining to
        // say; `Err` is git failing to run at all.
        Ok(gitrepo::WorktreeIdentity::Absent | gitrepo::WorktreeIdentity::Unestablished)
        | Err(_) => finish_observation(entry, present, registered, IdentityMatch::Unestablished),
    }
}

/// The rest of the observation — split out only so the identity arms above each end in one call
/// rather than each repeating the dirt questions.
fn finish_observation(
    entry: &Entry,
    present: presence::Presence,
    registered: Fact,
    identity: IdentityMatch,
) -> Observed {
    let dirty = gitrepo::is_dirty(&entry.worktree, &entry.repo_root).unwrap_or(Fact::Unknown);
    // `Unknown` — NOT `NotOnly` — when the question was never asked. Nothing reads this outside
    // the `dirty.is_yes()` arm today, so the difference is inert; it is written the honest way
    // anyway because `NotOnly` claims something was ESTABLISHED, and the whole defect this
    // observation seam keeps producing is an unasked question wearing an answer's spelling. An
    // arm added later that reads `deletions` without checking `dirty` first would be told a clean
    // tree holds real work.
    let deletions = if dirty.is_yes() {
        gitrepo::deletions_only(&entry.worktree, &entry.repo_root)
            .unwrap_or(gitrepo::Deletions::Unknown)
    } else {
        gitrepo::Deletions::Unknown
    };
    Observed {
        present,
        registered,
        identity,
        dirty,
        deletions,
    }
}

/// What to do with a pending record, from observations alone.
///
/// **The identity bar scales with how destructive the act is**, which is the rule that unwedged
/// this. `stow` is an atomic rename into `.jkb/archive` that preserves every byte and stays
/// reversible for the retention window, so it needs only that the tree is still plausibly the
/// one the record describes. Nothing here licenses an irreversible act on a weaker identity —
/// and the previous behaviour, holding for ever while advising the operator to delete the
/// directory by hand, had it exactly backwards: it refused the reversible action and recommended
/// the irreversible one (D34.4).
fn verdict_pending(entry: &Entry, obs: &Observed) -> Verdict {
    match obs.present {
        // Gone already. Whoever removed it did the sweep's work; the branch plan still applies.
        presence::Presence::Gone => return Verdict::DropRecord,
        // TWO CAUSES, TWO REMEDIES. Collapsed to one `Unknown` this printed "run it where that
        // repo is checked out" for both — advice that is right for the container bind and, for a
        // directory this machine cannot stat, wrong in the one case the sweep reaches most: the
        // repo IS checked out here, `reap` proved the root readable above the dispatch, so the
        // operator re-runs in the same place and observes the same thing for ever.
        presence::Presence::Unreadable => {
            return Verdict::Hold(Blocked {
                reason: format!("{} could not be read", entry.worktree.display()),
                remedy: Remedy::FixPermissions {
                    worktree: entry.worktree.clone(),
                },
            })
        }
        presence::Presence::AnchorInvisible => {
            return Verdict::Hold(Blocked {
                reason: format!(
                    "neither {} nor the repo around it is visible from here",
                    entry.worktree.display()
                ),
                remedy: Remedy::NoActionFromHere,
            })
        }
        presence::Presence::Here => {}
    }
    // REGISTRATION IS NOT EVIDENCE ABOUT IDENTITY, and vetoing on it here was a dead end. A
    // commit is not reused, so a tree sitting on the commit the record names is the recorded
    // session whatever `git worktree list` currently says — and `git worktree prune` unregisters
    // exactly the broken trees these records are written for, so requiring registration required
    // the absence of a routine cleanup. Worse, the remedy offered was `Redispose`, which cannot
    // be carried out for an unregistered tree at all (see [`Remedy::Redispose`]): the hold was
    // permanent and its advice was a sentence.
    //
    // What registration really governs is which REMEDY is followable, so it is read below, in the
    // arms that have one to offer.
    match obs.identity {
        IdentityMatch::Matches => {}
        // THE WRECK IS STOWABLE, and this is the finding that produced this whole seam. A tree
        // that cannot answer git for itself cannot answer `is_dirty` either, so requiring it
        // clean would rebuild the wedge one column over. The dirty check is waived HERE, for a
        // reason written down rather than by omission: the question is unanswerable in principle
        // for a wreck, and `stow` loses nothing — where the alternative on offer was telling the
        // operator to delete the directory.
        IdentityMatch::Wreck => return Verdict::Stow,
        IdentityMatch::DifferentSession => {
            return Verdict::Hold(Blocked {
                reason: format!(
                    "{} is not the tree the record describes",
                    entry.worktree.display()
                ),
                remedy: unidentified_remedy(entry, obs),
            })
        }
        IdentityMatch::Unestablished => {
            return Verdict::Hold(Blocked {
                reason: format!(
                    "{} could not be shown to be the tree the record describes",
                    entry.worktree.display()
                ),
                remedy: unidentified_remedy(entry, obs),
            })
        }
    }
    if entry.plan.accept_dirty {
        // The operator passed `--force`: they have already decided about whatever is in there,
        // and asking again would hold this record for ever over the very thing they answered.
        return Verdict::Stow;
    }
    match obs.dirty {
        // Proven clean, and nothing weaker. `is_dirty` used to spell "git ran and failed" as
        // `false`, so a worktree whose `.git` had been unlinked part-way read as clean and was
        // archived on the strength of it.
        f if f.is_no() => Verdict::Stow,
        // THE REASON COMES FROM ONE RENDERER (`Deletions::caveat`), shared with `jkb task land`,
        // which explains the same observation to the same person and had drifted to wording an
        // unanswered probe exactly like ordinary work. Only the REMEDY is chosen here, because
        // only that genuinely differs: a part-way removal is put back, anything else is committed
        // or forced.
        f if f.is_yes() => {
            let reason = match obs.deletions.caveat() {
                Some(c) => format!("it has uncommitted changes — {c}"),
                None => "it has uncommitted changes".to_owned(),
            };
            Verdict::Hold(Blocked {
                reason,
                // A tree that is only MISSING files is a part-way removal, not work, and the two
                // want opposite advice — the rule that was still telling an operator to commit
                // 62,000 deleted lines. Anything else, INCLUDING an unanswered probe, gets the
                // conservative remedy; what keeps the two apart for the reader is the caveat
                // above, since `Unknown` must not silently borrow "restore it all".
                remedy: match obs.deletions {
                    gitrepo::Deletions::Only(_) => Remedy::RestoreTree {
                        worktree: entry.worktree.clone(),
                    },
                    gitrepo::Deletions::NotOnly | gitrepo::Deletions::Unknown => {
                        Remedy::CommitOrForce {
                            uid: entry.uid.clone(),
                        }
                    }
                },
            })
        }
        // NOT `Redispose`, which the audit below caught: the tree is identified — a fresh
        // record would carry the same identity and the same unreadable status, so the advice
        // led straight back here. What resolves it is making the tree readable, or saying the
        // dirt is accepted, which is what this remedy offers.
        _ => Verdict::Hold(Blocked {
            reason: format!(
                "git could not say whether {} has uncommitted changes",
                entry.worktree.display()
            ),
            remedy: Remedy::CommitOrForce {
                uid: entry.uid.clone(),
            },
        }),
    }
}

/// The way out of a hold where the tree in front of us is not the one the record describes.
///
/// Both such arms ask this rather than each naming a remedy, because the choice turns on one fact
/// neither of them is about: **can any jkb verb still reach this tree?** `abandon` — the verb that
/// would write a fresh, checkable record — discovers sessions from `git worktree list`, so for a
/// directory git no longer registers it finds nothing and does nothing. Offering `Redispose`
/// there is offering advice that cannot be followed, which is the whole failure this closed set
/// exists to make impossible.
fn unidentified_remedy(entry: &Entry, obs: &Observed) -> Remedy {
    match obs.registered {
        // Still a worktree git knows about, so `abandon` can find it and re-record it.
        f if f.is_yes() => Remedy::Redispose {
            uid: entry.uid.clone(),
        },
        // PROVEN unregistered, and only proven: git answered, and said this path is not one of
        // its worktrees. That is the whole licence for the one destructive remedy in the set —
        // `registered` was a `bool`, so a `git worktree list` that failed to run spelled itself
        // `false` and this arm told the operator to delete a checkout on the strength of a
        // question nobody answered.
        f if f.is_no() && obs.identity == IdentityMatch::DifferentSession => Remedy::RemoveByHand {
            worktree: entry.worktree.clone(),
        },
        // Git ANSWERED about the repo — this path is not one of its worktrees — and the tree still
        // could not say what it is. Saying "git did not answer" here sent the operator to check a
        // command that already had, and left the hold permanent: the honest audit found it the
        // moment `applied` stopped choosing which of git's two answers to model.
        f if f.is_no() => Remedy::InspectByHand {
            worktree: entry.worktree.clone(),
        },
        // Git could not answer at all. Nothing about the tree OR its registration is established,
        // and neither licenses destroying anything.
        _ => Remedy::FixGitAccess {
            repo_root: entry.repo_root.clone(),
        },
    }
}

/// The verdict for a pending record, observations and all — the one entry point.
#[must_use]
pub fn pending_verdict(entry: &Entry) -> Verdict {
    verdict_pending(entry, &observe_pending(entry))
}

/// The pending records that GOVERN, each with the verdict the sweep will execute on it.
///
/// **The one read behind every report about a deferred disposal.** Two surfaces were deriving
/// this for themselves and each got a different half wrong: `jkb task sessions` printed
/// `[awaiting archive]` from the mere EXISTENCE of a record — so a record nothing will ever act
/// on read as work already in hand, which is the exact claim the `Verdict` seam was introduced to
/// stop — and `jkb doctor` asked [`pending_verdict`] per record without applying supersession, so
/// a withdrawn record still got a vote and re-printed advice the operator had already taken. The
/// sweep applies both rules, in this order; anything that describes the sweep must apply the same
/// two.
///
/// Ordered by worktree, and at most one entry per worktree by construction.
///
/// # Errors
/// Returns an error if the record store cannot be read.
pub fn pending_outlook(db: &Path) -> Result<Vec<(Entry, Verdict)>> {
    Ok(pending_outlook_in(&entries(db)?))
}

/// [`pending_outlook`] over a store already in hand.
///
/// Exists so a caller that has read the store can ask about the pending half **without reading it
/// a second time**. `jkb doctor` did read it twice — once for the archived count, once through
/// `pending_outlook` — and the two counts were then taken from two different reads of one file: a
/// record archived by a concurrent sweep between them is pending in neither answer and archived in
/// neither, so it is reported nowhere. The second read's error was swallowed too, which turned a
/// store full of deferred checkouts into "0 awaiting archive" with no next step offered.
#[must_use]
pub fn pending_outlook_in(store: &Store) -> Vec<(Entry, Verdict)> {
    let governing = governing_pending(&store.records);
    let mut out: Vec<(Entry, Verdict)> = store
        .records
        .iter()
        .filter(|(marker, r)| {
            r.archive.is_none() && governing.get(&r.worktree).is_some_and(|w| w == marker)
        })
        .map(|(_, r)| {
            let entry = (**r).clone();
            let verdict = pending_verdict(&entry);
            (entry, verdict)
        })
        .collect();
    out.sort_by(|a, b| a.0.worktree.cmp(&b.0.worktree));
    out
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
        // THE ANCHOR ITSELF, asked the same way as everything it licenses. It was a bare
        // `exists()` — safe, because both `No` and `Unknown` land in this held arm, but a third
        // spelling of the question in the very function that states the rule, and it is what
        // every absence below hangs from.
        if !matches!(entry.repo_root.try_exists(), Ok(true)) {
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

        // Not archived yet: this is the deferred half of a landing.
        sweep_pending(&mut entry, &marker, now, dry_run, &mut report);
    }

    Ok(report)
}

/// The pending half of one record: the landing this process could not finish, finished here.
///
/// Sibling of [`sweep_archived`], and split out for the same reason — the reachability check that
/// licenses reading an absence at all runs in the caller, above both.
fn sweep_pending(entry: &mut Entry, marker: &Path, now: u64, dry_run: bool, report: &mut Report) {
    // ONE decision, and it is not made here. The sweep EXECUTES the verdict that
    // `report_landing`, `report_abandon` and `jkb doctor` all render, so a verb can no longer
    // promise something the sweep will not do — which it did, for every record whose tree could
    // not answer git for itself.
    match pending_verdict(entry) {
        Verdict::DropRecord => {
            if !dry_run {
                let _ = gitrepo::prune_worktrees(&entry.repo_root);
                delete_branch_if_any(entry, report);
            }
            drop_marker(marker, dry_run, report, &entry.uid);
            return;
        }
        Verdict::Hold(blocked) => {
            report.held.push((
                entry.uid.clone(),
                format!("{} — {}", blocked.reason, blocked.remedy.advice()),
            ));
            return;
        }
        Verdict::Stow => {}
    }
    if dry_run {
        report
            .archived
            .push((entry.uid.clone(), archive_root(&entry.repo_root)));
        return;
    }
    match stow(&entry.repo_root, &entry.worktree, now) {
        Ok(dest) => {
            entry.archive = Some(dest.clone());
            entry.archived_at = Some(now);
            // THE RECORD IS WRITTEN FIRST, before the two git commands below, because those
            // can fail and this is the only thing that says where the tree went. Written back
            // to the SAME file it was read from, so no second copy can appear.
            if let Err(e) = record_at(marker, entry) {
                report.held.push((
                    entry.uid.clone(),
                    format!(
                        "archived to {} but the record could not be updated: {e}",
                        dest.display()
                    ),
                ));
                return;
            }
            let _ = gitrepo::prune_worktrees(&entry.repo_root);
            delete_branch_if_any(entry, report);
            report.archived.push((entry.uid.clone(), dest));
        }
        // The verdict said stow and the rename refused — which is ORDINARY rather than an error:
        // a session cannot unlink its own checkout, so a sweep run from inside one lands here for
        // its own tree while every other process moves it freely.
        Err(e) => report.held.push((
            entry.uid.clone(),
            format!(
                "{} could not be moved: {e} — {}",
                entry.worktree.display(),
                Remedy::RunReap.advice()
            ),
        )),
    }
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
    // MATCHED ON `Presence`, not collapsed with `.fact()`. The collapse was justified by "this
    // arm offers no remedy" — but it prints a sentence, and a sentence IS the remedy here: for an
    // unreadable archive `chmod` on the directory is exactly what ends the hold, while for one on
    // the far side of the container bind nothing on this machine can. Worded identically, a
    // multi-gigabyte checkout is kept for ever with no action to take, and the justifying comment
    // was contradicted by `removable()` twenty lines below, which produces a permission-shaped
    // hold of its own.
    match presence::present_under(dir, &entry.repo_root) {
        // Somebody removed it by hand. Nothing owed, so stop tracking it.
        presence::Presence::Gone => {
            drop_marker(marker, dry_run, report, &entry.uid);
            return;
        }
        // Dropping the record here is destructive in the quiet way: it is the only thing that
        // names a multi-gigabyte archive, so a stat error that read as "removed by hand" left the
        // directory referenced by nothing and never deleted.
        cause @ (presence::Presence::Unreadable | presence::Presence::AnchorInvisible) => {
            let why = if cause == presence::Presence::Unreadable {
                "this machine cannot read it — check the permissions on it and its parents, then \
                 re-run"
            } else {
                "neither it nor the repo around it is visible from here; run this where that repo \
                 is checked out"
            };
            report.held.push((
                entry.uid.clone(),
                format!(
                    "{} could not be read, so whether it is still there is unestablished — the \
                     record is kept. {why}",
                    dir.display()
                ),
            ));
            return;
        }
        presence::Presence::Here => {}
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

/// Deleting the branch is tidiness, not correctness — **while the premise that makes it tidiness
/// still holds**, which is what this now checks instead of assuming.
///
/// The premise is that the branch's commits are already in the target, and `entry.head` is the
/// evidence: it is where the branch was when the plan was made. If the branch has moved since,
/// the commits past that point are in no target and this is the only copy, so a forced
/// `git branch -D` destroys work. A deferred record makes that ordinary rather than exotic — the
/// checkout is left in place for days and can be committed in, which is precisely the state the
/// container produces on every landing.
///
/// **Checked here, in the callee, and not at the one verdict that provoked the finding.** The
/// review found it on the `Wreck` path, where an unidentified tree reaches `Stow`; but
/// `DropRecord` deletes branches too, and the next acting verdict would have to remember the rule
/// as well. A rule every call site must remember is the defect (D40/D45).
///
/// **Its domain is the DEFERRED application of a plan**, and that is why two sibling sites
/// legitimately do not call it: [`dispose`]'s own delete, and `jkb task abandon`'s when nothing
/// was deferred. There the operator asks and jkb acts in one moment, so there is no window in
/// which the branch can move between the decision and the act — the premise cannot have stopped
/// holding, and re-checking it would only add a way to refuse what was just requested. What makes
/// this function's check necessary is elapsed time, not the verb.
///
/// The comparison is positive-only: a `rev` that answers nothing is git failing OR the branch
/// being gone already, and neither licenses a delete, so anything but a proven match keeps it.
/// A branch already gone is silently fine — there is nothing to keep and nothing to report.
fn delete_branch_if_any(entry: &Entry, report: &mut Report) {
    if !entry.plan.delete_branch || entry.branch.is_empty() {
        return;
    }
    let tip = gitrepo::rev(&entry.repo_root, &entry.branch).ok().flatten();
    match (&tip, &entry.head) {
        (Some(now), Some(planned)) if now == planned => {
            // A branch somebody checked out elsewhere is not an error worth stopping an
            // unattended sweep for.
            let _ = gitrepo::delete_branch(&entry.repo_root, &entry.branch, true);
        }
        // Gone already, or git will not name it: nothing to delete, nothing to warn about.
        (None, _) => {}
        (Some(now), planned) => report.kept_branches.push((
            entry.uid.clone(),
            format!(
                "{} was kept: the disposal planned to delete it at {}, and it is now at {} — \
                 whether anything since reached a target is not something this can establish, \
                 and a forced `git branch -D` on the wrong side of that is the only copy's last \
                 reader. Delete it yourself once you have looked.",
                entry.branch,
                planned
                    .as_deref()
                    .unwrap_or("a commit the record could not observe"),
                now
            ),
        )),
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

    /// An archive this machine cannot stat is held with the fix that ends it, not with the
    /// sentence for one on the other side of the container bind.
    ///
    /// The archived arm collapsed `Presence` with `.fact()`, justified by "this arm offers no
    /// remedy" — but it prints a sentence, and for an unreadable directory `chmod` is exactly the
    /// remedy. Worded identically to the unreachable case, a multi-gigabyte checkout is kept for
    /// ever with nothing an operator can do about it.
    #[test]
    fn an_unreadable_archive_is_held_with_the_fix_that_ends_it() {
        use std::os::unix::fs::PermissionsExt;

        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");
        let dir = archive_root(&repo).join("sess-19700101T000000Z");
        fs::create_dir_all(&dir).expect("mk");
        fs::create_dir_all(store_dir(&db)).expect("mk");
        let e = Entry {
            archive: Some(dir.clone()),
            archived_at: Some(0),
            ..session_entry(&repo, &wt, &branch, &head)
        };
        fs::write(
            store_dir(&db).join("m.json"),
            serde_json::to_vec(&e).expect("json"),
        )
        .expect("write");

        // EACCES on the parent, so the stat of `dir` itself fails — `Presence::Unreadable`, which
        // is what `.fact()` used to flatten into the same `Unknown` as an invisible anchor.
        let parent = archive_root(&repo);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).expect("chmod");
        let stat_failed = dir.try_exists().is_err();
        let r = reap(&db, RETAIN_DAYS, false);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).expect("restore");

        assert!(
            stat_failed,
            "the premise — the stat must actually fail, or this test is about nothing"
        );
        let r = r.expect("reap");
        assert_eq!(r.held.len(), 1, "the record is kept, never dropped: {r:?}");
        assert!(
            r.held[0].1.contains("permissions"),
            "and it must say what ends it, rather than sending the operator to another \
             machine: {}",
            r.held[0].1
        );
    }

    /// A `delete_branch` plan is applied only while the branch is still where the plan was made
    /// about it — otherwise the forced delete is the last reader of the only copy.
    ///
    /// A deferred record is exactly the state that makes this ordinary rather than exotic: the
    /// checkout is left in place for days, and in the container EVERY landing produces one. Commit
    /// in it and the branch moves past the recorded head; those commits are in no target, so
    /// `git branch -D` destroys them. Both halves are asserted from ONE fixture, so what differs
    /// between them is only whether the branch moved.
    #[test]
    fn a_branch_that_moved_since_the_plan_is_kept_and_reported() {
        for moved in [false, true] {
            let t = tempfile::tempdir().expect("tempdir");
            let db = t.path().join("jkb.db");
            let repo = t.path().join("repo");
            let (wt, branch, head) = session(&repo, "sess");
            fs::create_dir_all(store_dir(&db)).expect("mk");
            let e = Entry {
                plan: Plan {
                    delete_branch: true,
                    accept_dirty: false,
                },
                ..session_entry(&repo, &wt, &branch, &head)
            };
            fs::write(
                store_dir(&db).join("m.json"),
                serde_json::to_vec(&e).expect("json"),
            )
            .expect("write");

            if moved {
                // Work done in the session AFTER the disposal was recorded — the commits that
                // exist nowhere else.
                fs::write(wt.join("late.txt"), "after the plan").expect("write");
                git(&wt, &["add", "-A"]);
                git(&wt, &["commit", "-qm", "late work"]);
                assert_ne!(
                    git(&repo, &["rev-parse", &branch]),
                    head,
                    "the premise: the branch really did move"
                );
            }
            // BOTH runs go through the wreck path, so the only difference between them is where
            // the branch points. Found by running it: a moved branch on a HEALTHY tree gives
            // `DifferentSession` and is held, so the sweep never reaches a branch delete at all —
            // the reachable route to this guard is the one the review described, a tree whose git
            // linkage broke after the disposal was recorded.
            fs::remove_file(wt.join(".git")).expect("unlink the worktree's .git");
            assert_eq!(
                gitrepo::worktree_identity(&wt, &repo).expect("identity"),
                gitrepo::WorktreeIdentity::Foreign,
                "the premise: git answers about the enclosing repo, so this is a proven wreck"
            );

            let r = reap(&db, RETAIN_DAYS, false).expect("reap");
            assert_eq!(r.archived.len(), 1, "the tree is stowed either way: {r:?}");

            // `branch --list`, not `rev-parse --verify`: the fixture's `git` asserts success, and
            // rev-parse exits non-zero for the very state half this test is about.
            let survives = !git(&repo, &["branch", "--list", &branch]).is_empty();
            if moved {
                assert!(survives, "the only copy of `late work` must not be deleted");
                assert_eq!(
                    r.kept_branches.len(),
                    1,
                    "and the refusal is REPORTED — a silently surviving branch reads as a bug \
                     rather than as a decision: {r:?}"
                );
                assert!(
                    r.kept_branches[0].1.contains(&branch),
                    "naming the branch: {r:?}"
                );
            } else {
                assert!(
                    !survives,
                    "an unmoved branch is tidied away as the plan asked"
                );
                assert!(r.kept_branches.is_empty(), "nothing to report: {r:?}");
            }
        }
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
            gitrepo::worktree_head(&wt, &repo).expect("head"),
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

    /// A record the sweep will hold is never reported as work in hand.
    ///
    /// The second half of the must-fix, and the reason `Deferral` carries a verdict at all.
    /// `report_landing`, `report_abandon`, `branch_fate` and `jkb doctor` each hand-wrote "`jkb
    /// task reap` will finish this" from the mere fact that a record had been written — so an
    /// operator was sent, repeatedly, to run a command that would report the same hold for ever.
    #[test]
    fn a_deferral_promises_a_sweep_only_when_one_will_happen() {
        let held = Deferral {
            why: "permission denied".to_owned(),
            verdict: Verdict::Hold(Blocked {
                reason: "it is not the tree the record describes".to_owned(),
                remedy: Remedy::Redispose {
                    uid: "task:t".to_owned(),
                },
            }),
        };
        assert!(!held.will_be_swept(), "nothing will act on it");
        let outlook = held.outlook();
        assert!(
            !outlook.contains("which the watcher service runs"),
            "and it must not say something will: {outlook}"
        );
        assert!(
            outlook.contains("will not be able to finish it")
                && outlook.contains("jkb task abandon"),
            "it says so, and what to do instead: {outlook}"
        );

        let owed = Deferral {
            why: "permission denied".to_owned(),
            verdict: Verdict::Stow,
        };
        assert!(owed.will_be_swept(), "this one really is in hand");
        assert!(
            owed.outlook().contains("jkb task reap"),
            "so it may say so: {}",
            owed.outlook()
        );
    }

    /// Git's registration decides which remedy is followable — never whether the tree is ours.
    ///
    /// Two halves of one correction, and they pull in opposite directions, so both are asserted.
    /// A commit id is never reused, so a tree sitting on the recorded commit IS the recorded
    /// session and registration adds nothing — while `git worktree prune` unregisters precisely
    /// the broken trees these records get written for, so vetoing on it required the absence of a
    /// routine cleanup. And where the tree genuinely cannot be identified, registration is what
    /// decides whether `abandon` can even see it: [`crate::session::discover`] walks
    /// `git worktree list`, so on an unregistered directory it writes no record and `Redispose`
    /// is advice that cannot be followed.
    #[test]
    fn registration_gates_the_remedy_not_the_identity() {
        let entry = entry(Path::new("/repo/.jkb/work/s"), Path::new("/repo"));
        let unregistered = Observed {
            present: presence::Presence::Here,
            registered: Fact::No,
            identity: IdentityMatch::Matches,
            dirty: Fact::No,
            deletions: gitrepo::Deletions::NotOnly,
        };
        assert_eq!(
            verdict_pending(&entry, &unregistered),
            Verdict::Stow,
            "the commit matches, so `git worktree list` has nothing to add — and a prune of a \
             broken tree must not freeze its own record"
        );

        let stranger = Observed {
            identity: IdentityMatch::DifferentSession,
            ..unregistered.clone()
        };
        let Verdict::Hold(blocked) = verdict_pending(&entry, &stranger) else {
            panic!("a tree that is not ours is never moved")
        };
        assert!(
            matches!(blocked.remedy, Remedy::RemoveByHand { .. }),
            "and the way out cannot be `jkb task abandon`, which discovers sessions from git's \
             registration and would find nothing here: {blocked:?}"
        );

        let stranger_registered = Observed {
            registered: Fact::Yes,
            ..stranger
        };
        let Verdict::Hold(blocked) = verdict_pending(&entry, &stranger_registered) else {
            panic!("still not ours")
        };
        assert!(
            matches!(blocked.remedy, Remedy::Redispose { .. }),
            "but with the tree still registered, re-recording it is reachable: {blocked:?}"
        );
    }

    /// Every remedy is a sentence somebody can read — checked over the CLOSED set, so a variant
    /// added later cannot skip it.
    ///
    /// The self-review found two of these rendering `which` + eighteen spaces + `records`: a
    /// `\`-continued literal whose continuation had been lost, leaving the run of indentation
    /// inside the string. Nothing caught it, and the reason is the shape this repo keeps meeting —
    /// [`a_deferral_promises_a_sweep_only_when_one_will_happen`] asserts
    /// `outlook.contains("jkb task abandon")`, which sits BEFORE the damage, so the assertion was
    /// satisfied by a mangled string exactly as by a good one. Assert on something the defect
    /// changes.
    #[test]
    fn every_remedy_reads_as_a_sentence() {
        for remedy in Remedy::every() {
            let advice = remedy.advice();
            assert!(!advice.trim().is_empty(), "{remedy:?} says nothing");
            assert!(
                !advice.contains("  "),
                "{remedy:?} renders a run of whitespace, so a line continuation was lost: {advice:?}"
            );
            assert!(
                !advice.contains('\n'),
                "{remedy:?} spans lines, which the one-line hold reports cannot show: {advice:?}"
            );
        }
    }

    /// EVERY HOLD HAS AN EXIT — the property that makes a closed `Remedy` worth having.
    ///
    /// This is `Machine::audit`'s `UnreachableRemedy`/`DeadEnd` check, hand-rolled for one
    /// function because the disposal record does not (yet) earn a lifecycle table. D48 records
    /// the failure it prevents: "passes 31 and 32 are the same finding one message apart — a
    /// printed remedy whose obvious argument froze the task permanently".
    ///
    /// Walk the product of what can be observed and, for every `Hold`, require that doing what the
    /// remedy says moves the verdict — **for every outcome the advice can have**, not for one
    /// hand-picked happy one. That last clause is the round-11 correction: `applied` used to
    /// settle each fact optimistically, so a remedy whose success was not even guaranteed by its
    /// own mechanism was certified as an exit.
    #[test]
    fn every_hold_names_a_remedy_that_actually_leads_out() {
        let mut holds = 0;
        for (entry, obs) in world() {
            let Verdict::Hold(blocked) = verdict_pending(&entry, &obs) else {
                continue;
            };
            holds += 1;
            assert!(
                !blocked.remedy.advice().is_empty(),
                "a hold must actually say something"
            );
            assert!(
                escapes(&entry, &obs, &blocked.remedy),
                "doing what it says leaves the verdict where it was, so this hold is permanent: \
                 {obs:?} -> {blocked:?}"
            );
        }
        assert!(holds > 0, "the walk must actually reach some holds");
    }

    /// A `git worktree list` that did not run is not a report that the tree is unregistered.
    ///
    /// The OBSERVATION half of that rule, and it needed its own test: the product walks supply
    /// `Observed` directly, so they exercise `verdict_pending` and never `observe_pending` —
    /// spelling a failed git as `Fact::No` there passed every one of them while handing the
    /// verdict the one value that licenses telling somebody to delete a checkout.
    #[test]
    fn a_git_that_cannot_answer_leaves_registration_unestablished() {
        let t = tempfile::tempdir().expect("tempdir");
        // A directory that is emphatically not a git repository, so `git worktree list` exits
        // non-zero — which `worktrees` used to report as an empty list, i.e. "not registered".
        let root = t.path().join("not-a-repo");
        let wt = root.join(".jkb/work/s");
        fs::create_dir_all(&wt).expect("mkdir");
        assert!(
            gitrepo::worktrees(&root)
                .expect("no spawn failure")
                .is_none(),
            "the premise: git really does decline to answer here"
        );

        let obs = observe_pending(&entry(&wt, &root));
        assert_eq!(
            obs.registered,
            Fact::Unknown,
            "unasked is not answered — and `No` is what selects the irreversible remedy"
        );
        // GIT CANNOT SAY WHAT THIS TREE IS EITHER, so nothing about it is established. This used
        // to read `Wreck` — `worktree_identity` spelled "would not answer" and "answered, and it
        // is a wreck" the same way — and the whole assertion below then sat behind an `if let`
        // that never bound.
        assert_eq!(
            obs.identity,
            IdentityMatch::Unestablished,
            "a `rev-parse` that exited non-zero establishes nothing about the tree: {obs:?}"
        );

        // ON THE VERDICT ITSELF, not on one shape of it. Behind `if let Verdict::Hold(b)` this
        // was dead code for its own fixture — the sweep had decided to MOVE a directory git said
        // nothing about, and the test passed. An assertion that any non-`Hold` answer satisfies
        // silently is the shape this file has now met four times.
        match verdict_pending(&entry(&wt, &root), &obs) {
            Verdict::Hold(b) => assert!(
                !b.remedy.is_destructive(),
                "nothing may be destroyed on the strength of a question nobody answered: {b:?}"
            ),
            acted => panic!(
                "a tree git will not speak for is HELD; anything else acts on an observation \
                 nobody made: {acted:?}"
            ),
        }
    }

    /// A dirty tree whose dirt could not be characterised is not reported as real work.
    ///
    /// The verdict half of the same rule. Both cases end in `CommitOrForce`, so the remedy alone
    /// cannot distinguish them — what must differ is the REASON, because "it has uncommitted
    /// changes" asserts something about a probe that never answered, and it is the sentence an
    /// operator acts on.
    #[test]
    fn dirt_that_could_not_be_characterised_is_not_called_work() {
        let entry = entry(Path::new("/repo/.jkb/work/s"), Path::new("/repo"));
        let reason_for = |d| match verdict_pending(
            &entry,
            &Observed {
                present: presence::Presence::Here,
                registered: Fact::Yes,
                identity: IdentityMatch::Matches,
                dirty: Fact::Yes,
                deletions: d,
            },
        ) {
            Verdict::Hold(b) => b.reason,
            other => panic!("a dirty tree is held: {other:?}"),
        };
        assert_ne!(
            reason_for(gitrepo::Deletions::Unknown),
            reason_for(gitrepo::Deletions::NotOnly),
            "an unanswered probe must not be reported in the same words as an answered one"
        );
        assert!(
            reason_for(gitrepo::Deletions::Unknown).contains("unestablished"),
            "and it must say so: {}",
            reason_for(gitrepo::Deletions::Unknown)
        );
    }

    /// The two ways of failing to establish presence get the two different remedies.
    ///
    /// Neither audit can catch this, which is why it is asserted directly: both causes model the
    /// same outcomes (the stat comes back, either way), so the exit walk is satisfied by either
    /// remedy and the proof rule never sees them — the remedies are both non-destructive. What is
    /// wrong when they are merged is the ADVICE, and advice correctness is not a property either
    /// walk can express. Merged, the sweep printed "run it where that repo is checked out" for a
    /// directory on a repo `reap` had just proved readable — an operator could follow it for ever.
    #[test]
    fn an_unreadable_path_and_an_unreachable_repo_get_different_advice() {
        let entry = entry(Path::new("/repo/.jkb/work/s"), Path::new("/repo"));
        let base = Observed {
            present: presence::Presence::Here,
            registered: Fact::Yes,
            identity: IdentityMatch::Matches,
            dirty: Fact::No,
            deletions: gitrepo::Deletions::NotOnly,
        };
        let remedy_for = |p| match verdict_pending(
            &entry,
            &Observed {
                present: p,
                ..base.clone()
            },
        ) {
            Verdict::Hold(b) => b.remedy,
            other => panic!("{p:?} must hold: {other:?}"),
        };
        assert!(
            matches!(
                remedy_for(presence::Presence::Unreadable),
                Remedy::FixPermissions { .. }
            ),
            "this machine cannot stat it — the fix is here"
        );
        assert!(
            matches!(
                remedy_for(presence::Presence::AnchorInvisible),
                Remedy::NoActionFromHere
            ),
            "the repo is not on this filesystem — nothing here can act"
        );
    }

    /// A withdrawn record gets no vote in what is reported — the sweep's FIRST rule, which the
    /// reports were skipping.
    ///
    /// `jkb doctor` asked `pending_verdict` per record, so `abandon`, change your mind, `abandon
    /// --force` left two records for one worktree: the count said "2 awaiting archive" for one
    /// checkout and the older, withdrawn plan re-printed the advice the operator had just taken.
    #[test]
    fn only_the_governing_record_is_reported() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, _) = session(&repo, "sess");
        fs::write(repo.join(".jkb/archive"), b"in the way").expect("write");

        for accept_dirty in [false, true] {
            let out = dispose(
                &db,
                &repo,
                &wt,
                &branch,
                "task:t",
                Plan {
                    delete_branch: false,
                    accept_dirty,
                },
            )
            .expect("dispose");
            assert!(matches!(out, Disposed::Deferred(_)));
        }
        assert_eq!(
            entries(&db).expect("entries").records.len(),
            2,
            "the premise: two disposals of one worktree really did leave two records"
        );

        let outlook = pending_outlook(&db).expect("outlook");
        assert_eq!(
            outlook.len(),
            1,
            "but only one of them governs: {outlook:?}"
        );
        assert!(
            outlook[0].0.plan.accept_dirty,
            "and it is the LATER one, whose plan is the one the operator last chose"
        );
    }

    /// A DESTRUCTIVE remedy is offered only where every fact licensing it was PROVEN.
    ///
    /// Separate from the exit audit above, and it has to be, because that audit cannot see this:
    /// an operator deleting a live checkout is a perfectly good "exit" — `present` becomes `Gone`
    /// and the verdict moves to `DropRecord`. So the exit walk would certify `RemoveByHand`
    /// offered on a pure guess. D34.4 is about which way to be wrong, not about whether the state
    /// changes, and it needs its own assertion over what was ESTABLISHED.
    #[test]
    fn nothing_irreversible_is_advised_on_an_unproven_observation() {
        let mut destructive = 0;
        for (entry, obs) in world() {
            let Verdict::Hold(blocked) = verdict_pending(&entry, &obs) else {
                continue;
            };
            if !blocked.remedy.is_destructive() {
                continue;
            }
            destructive += 1;
            assert_eq!(
                obs.present,
                presence::Presence::Here,
                "advising removal of a directory nobody established is there: {obs:?}"
            );
            assert!(
                obs.registered.is_no(),
                "`registered` must be PROVEN false — git answering nothing is what used to \
                 select this: {obs:?}"
            );
            assert_eq!(
                obs.identity,
                IdentityMatch::DifferentSession,
                "and the tree must have ANSWERED and said it is not ours; a tree that cannot \
                 answer is a wreck, which is stowed reversibly instead: {obs:?}"
            );
        }
        assert!(
            destructive > 0,
            "the walk must reach the destructive remedy, or this asserts nothing"
        );
    }

    /// **What jkb DOES, not what it says** — the third walk, and the one the other two could not
    /// have been.
    ///
    /// Both audits above `continue` on any non-`Hold` verdict, so the entire `world()` guarantee
    /// was about ADVICE. Meanwhile `Verdict::Stow` is strictly more destructive than the remedy
    /// those audits police: `RemoveByHand` is a sentence printed to a person who will look before
    /// acting, while `Stow` renames a live checkout out from under whoever is in it, force-deletes
    /// its branch, and hands the tree to `remove_dir_all` after the retention window — unattended,
    /// from a service. **The stronger act had the weaker check**, which is D34.4 read backwards.
    ///
    /// The cost was paid in the very round that added those two audits: widening `Foreign` into an
    /// unconditional `Wreck => Stow` put the module's most destructive outcome behind an
    /// observation nobody had established, and neither walk looked at it.
    ///
    /// So: an ACTING verdict requires that the tree was proven to be there, and that its identity
    /// was either proven (`Matches`) or is unprovable-in-principle for a reason that is itself an
    /// established observation (`Wreck` — git ANSWERED and named an enclosing repo).
    /// `Unestablished` licenses nothing, whichever way it arose.
    #[test]
    fn nothing_is_acted_on_without_the_facts_that_license_it() {
        let mut acted = 0;
        for (entry, obs) in world() {
            match verdict_pending(&entry, &obs) {
                // Advice, which the two audits above are about. This one is about acts.
                Verdict::Hold(_) => {}
                // `DropRecord` is reached only from `Presence::Gone`, which is itself the proof:
                // the tree is not there, so there is nothing left to act on. Asserted rather than
                // assumed, because that is one `match` arm away from being untrue.
                Verdict::DropRecord => {
                    assert_eq!(
                        obs.present,
                        presence::Presence::Gone,
                        "a record is dropped only where the tree is PROVEN gone: {obs:?}"
                    );
                }
                Verdict::Stow => {
                    acted += 1;
                    assert_eq!(
                        obs.present,
                        presence::Presence::Here,
                        "moving a directory nobody established is there: {obs:?}"
                    );
                    assert!(
                        matches!(obs.identity, IdentityMatch::Matches | IdentityMatch::Wreck),
                        "stowing a tree whose identity was never established — the widening this \
                         walk exists to have caught: {obs:?}"
                    );
                }
            }
        }
        assert!(acted > 0, "the walk must reach an acting verdict");
    }

    /// The matcher can fail — the D48.13 rule, for the two audits above.
    ///
    /// A harness that judges other guards needs a negative control, or "every hold escapes" is a
    /// sentence that has never been observed to be false. `RunReap` is the honest control: it is
    /// a real variant with no modelled effect (it is the executor's, for a rename this process
    /// may not perform), so offering it for a reachable hold must be reported as a dead end.
    #[test]
    fn the_exit_audit_reports_a_remedy_that_does_nothing() {
        let entry = entry(Path::new("/repo/.jkb/work/s"), Path::new("/repo"));
        let obs = Observed {
            present: presence::Presence::Here,
            registered: Fact::Yes,
            identity: IdentityMatch::Matches,
            dirty: Fact::Yes,
            deletions: gitrepo::Deletions::NotOnly,
        };
        let Verdict::Hold(real) = verdict_pending(&entry, &obs) else {
            panic!("a dirty identified tree is held")
        };
        assert!(
            escapes(&entry, &obs, &real.remedy),
            "the real remedy is an exit"
        );

        assert!(
            !escapes(&entry, &obs, &Remedy::RunReap),
            "a remedy with no effect must be REPORTED as a dead end, or the audits above are \
             matching nothing"
        );
    }

    /// Every observation the world can present. Both audits walk this, so neither can be passing
    /// because it looked at a smaller set than the other.
    fn world() -> Vec<(Entry, Observed)> {
        let facts = [Fact::Yes, Fact::No, Fact::Unknown];
        let presences = [
            presence::Presence::Here,
            presence::Presence::Gone,
            presence::Presence::Unreadable,
            presence::Presence::AnchorInvisible,
        ];
        let identities = [
            IdentityMatch::Matches,
            IdentityMatch::Wreck,
            IdentityMatch::DifferentSession,
            IdentityMatch::Unestablished,
        ];
        let deletions = [
            gitrepo::Deletions::Only(3),
            gitrepo::Deletions::NotOnly,
            gitrepo::Deletions::Unknown,
        ];
        let mut out = Vec::new();
        for present in presences {
            for registered in facts {
                for identity in identities {
                    for dirty in facts {
                        for d in deletions {
                            for accept_dirty in [true, false] {
                                out.push((
                                    Entry {
                                        plan: Plan {
                                            delete_branch: false,
                                            accept_dirty,
                                        },
                                        ..entry(Path::new("/repo/.jkb/work/s"), Path::new("/repo"))
                                    },
                                    Observed {
                                        present,
                                        registered,
                                        identity,
                                        dirty,
                                        deletions: d,
                                    },
                                ));
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// Does following `remedy` move the verdict off where it is now, WHATEVER the advice turns
    /// out to achieve? Factored out so the negative control can assert it answers `false`.
    ///
    /// Compared against the verdict THIS observation produces, never against a `Blocked` handed
    /// in — which is what the first version did, and the negative control caught it immediately:
    /// substituting an inert remedy changed the `Blocked` it was compared with, so the comparison
    /// found a difference and reported an escape. A harness that judges other guards has to be
    /// watched failing, or it is a sentence nobody has tested.
    fn escapes(entry: &Entry, obs: &Observed, remedy: &Remedy) -> bool {
        let before = verdict_pending(entry, obs);
        let outcomes = applied(remedy, obs);
        assert!(!outcomes.is_empty(), "a remedy models at least one outcome");
        outcomes
            .into_iter()
            .all(|after| verdict_pending(entry, &after) != before)
    }

    /// What the world can look like once the operator has done what the remedy says — **every**
    /// outcome, not the convenient one.
    ///
    /// Deliberately a MODEL rather than a re-run of the code: "the operator removes a directory"
    /// cannot execute in a unit audit, so some model is unavoidable. Two rules keep it honest,
    /// both learned by getting it wrong:
    ///
    /// 1. **An arm may settle a fact only where that fact IS the remedy's success criterion.**
    ///    `RemoveByHand` -> `Gone` is legitimate: removal is literally what the advice asks.
    ///    Everything else may only turn one unknown into the SET of answers it might come back
    ///    as. `NoActionFromHere` used to settle `present: Yes` — but "take the stat somewhere it
    ///    can be taken" can come back either way, and picking the branch that escapes is how a
    ///    remedy gets credit it has not earned.
    /// 2. **Each arm names the mechanism that delivers the effect** — `governing_pending`,
    ///    `session::discover` — so a claim about another command is checkable rather than assumed.
    fn applied(remedy: &Remedy, obs: &Observed) -> Vec<Observed> {
        let with = |f: &dyn Fn(&mut Observed)| {
            let mut o = obs.clone();
            f(&mut o);
            o
        };
        match remedy {
            // `abandon` re-records the tree and `governing_pending` makes the newer record
            // authoritative — but what the fresh record OBSERVES is not ours to choose: a healthy
            // tree gives a head that matches, a wreck gives `head: None` and is seen as a wreck.
            // Both must lead out.
            Remedy::Redispose { .. } if obs.registered.is_yes() => vec![
                with(&|o| o.identity = IdentityMatch::Matches),
                with(&|o| o.identity = IdentityMatch::Wreck),
            ],
            // The operator looked and removed it, which is exactly what this asks — so settling
            // the fact is legitimate here and nowhere else.
            Remedy::RemoveByHand { .. } => vec![with(&|o| o.present = presence::Presence::Gone)],
            // Both leave the tree with nothing uncommitted in it — one by committing (or by
            // `--force`, which records that the dirt is accepted, honoured through
            // `plan.accept_dirty`), the other by putting a part-way removal back.
            Remedy::CommitOrForce { .. } | Remedy::RestoreTree { .. } => vec![with(&|o| {
                o.dirty = Fact::No;
                o.deletions = gitrepo::Deletions::NotOnly;
            })],
            // The stat is taken somewhere it can be taken, or the permissions are fixed. It comes
            // back EITHER WAY, and both answers must lead out of this hold.
            Remedy::NoActionFromHere | Remedy::FixPermissions { .. } => vec![
                with(&|o| o.present = presence::Presence::Here),
                with(&|o| o.present = presence::Presence::Gone),
            ],
            // git answers again — and that is ALL this remedy's criterion is. What git then says
            // about registration is one answer and what it says about identity is a second,
            // independent one, so the arm yields the whole product rather than two convenient
            // pairings. Yielding only `(Yes, Matches)` and `(No, DifferentSession)` omitted
            // `(No, Unestablished)`, which reproduces the hold exactly — so a permanent hold was
            // certified as escapable by the audit whose entire job is to find those.
            Remedy::FixGitAccess { .. } => {
                let mut out = Vec::new();
                for registered in [Fact::Yes, Fact::No] {
                    for identity in [
                        IdentityMatch::Matches,
                        IdentityMatch::Wreck,
                        IdentityMatch::DifferentSession,
                        IdentityMatch::Unestablished,
                    ] {
                        out.push(with(&|o| {
                            o.registered = registered;
                            o.identity = identity;
                        }));
                    }
                }
                out
            }
            // NOTHING CHANGES, for two different reasons — and both deserve to be read as a dead
            // end. `RunReap` is not reachable from `verdict_pending` at all (it is the executor's,
            // for a rename this process may not perform), which is what makes it the negative
            // control above. A `Redispose` that reaches here is one offered for an UNREGISTERED
            // tree, which `unidentified_remedy` will not do — `abandon` discovers sessions from
            // `git worktree list` and would find nothing to re-record.
            // `git worktree repair` re-links a checkout whose administrative file was lost, after
            // which the tree answers for itself — what it then SAYS is not ours to choose, so
            // every identity it might report is yielded. The operator moving it aside instead is
            // the other outcome the advice names.
            Remedy::InspectByHand { .. } => vec![
                with(&|o| o.identity = IdentityMatch::Matches),
                with(&|o| o.identity = IdentityMatch::Wreck),
                with(&|o| o.identity = IdentityMatch::DifferentSession),
                with(&|o| o.present = presence::Presence::Gone),
            ],
            Remedy::RunReap | Remedy::Redispose { .. } => vec![obs.clone()],
        }
    }

    /// A deferred tree whose HEAD cannot be read is RECORDED, not refused — and the state it
    /// leaves terminates.
    ///
    /// Refusing here was the wedge, not the cure. `abandon` calls `dispose` through `?`, so the
    /// bail returned before the `write_txn` that releases the claim: the task stayed
    /// `in_progress` under a session owner `is_alive` still says Yes to, `doctor --fix` would not
    /// reclaim it because the worktree still exists, and re-running hit the same bail. In the
    /// container this is the ORDINARY path, since a session cannot archive its own checkout.
    ///
    /// Recording costs nothing that refusing saved: the same unarchivable tree is on disk either
    /// way, and only one of the two remembers it.
    #[test]
    fn a_deferred_disposal_whose_head_cannot_be_read_is_recorded_rather_than_refused() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, _) = session(&repo, "sess");

        // Nothing can be moved...
        fs::write(repo.join(".jkb/archive"), b"in the way").expect("write");
        // ...and the tree can no longer say what it is on. A part-way `git worktree remove`.
        fs::remove_file(wt.join(".git")).expect("unlink .git");
        assert_eq!(
            gitrepo::worktree_head(&wt, &repo).expect("head"),
            None,
            "the premise: no identity is obtainable from the tree itself"
        );

        let out = dispose(
            &db,
            &repo,
            &wt,
            &branch,
            "task:t",
            Plan {
                delete_branch: true,
                accept_dirty: true,
            },
        )
        .expect("recorded, not refused — the caller's claim release is behind this `?`");
        assert!(matches!(out, Disposed::Deferred(_)));

        let records = entries(&db).expect("entries").records;
        assert_eq!(records.len(), 1, "the tree is remembered, not forgotten");
        assert!(
            records[0].1.head.is_none(),
            "and honestly: no identity was obtainable, so none is claimed"
        );

        // AND THE SWEEP CAN FINISH IT. `head: None` is the observation "this tree could not say
        // what it was on", not a blank field, and the tree is still exactly that — so the record
        // is verifiable and `stow`, which loses nothing, is what happens. Read as a missing
        // field it bailed before every later check, so no sweep could ever act while `land`,
        // `abandon`, `branch_fate` and `doctor` all promised one would; the remedy it printed
        // was to delete the directory by hand, which is strictly more destructive than the
        // rename it was refusing (D34.4, inverted).
        assert_eq!(
            pending_verdict(&records[0].1),
            Verdict::Stow,
            "a wreck the record describes as a wreck is still identifiable"
        );
    }

    /// The sweep must not identify a worktree by its PARENT's HEAD.
    ///
    /// `still_the_recorded_session` asked `rev(&entry.worktree, "HEAD")` while `dispose` wrote
    /// the field with `worktree_head` — and git's discovery walks up, so once the session's
    /// `.git` file is unlinked `rev` answers from the main checkout. The dangerous half is the
    /// SILENT one, built here: when the main checkout happens to sit on the same commit — the
    /// ordinary state right after a land fast-forwards it — the identity check passed on a
    /// borrowed value and the sweep went on to move a tree it had never identified.
    #[test]
    fn the_sweep_does_not_identify_a_worktree_by_its_parents_head() {
        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, _) = session(&repo, "sess");

        fs::write(repo.join(".jkb/archive"), b"in the way").expect("write");
        let out = dispose(
            &db,
            &repo,
            &wt,
            &branch,
            "task:t",
            Plan {
                delete_branch: false,
                accept_dirty: true,
            },
        )
        .expect("dispose");
        assert!(matches!(out, Disposed::Deferred(_)));
        let entry = entries(&db).expect("entries").records.remove(0).1;
        let want = entry
            .head
            .clone()
            .expect("the session's own tip was recorded");

        // The main checkout is moved onto the very commit the record names — what a land leaves
        // behind — and then the session's link is broken.
        git(&repo, &["reset", "--hard", &want]);
        fs::remove_file(wt.join(".git")).expect("unlink .git");
        assert_eq!(
            gitrepo::rev(&wt, "HEAD").expect("rev").as_deref(),
            Some(want.as_str()),
            "the premise: the OLD reader would now be handed the recorded commit, from the wrong \
             tree, and would call the identity established"
        );

        // ASSERTED ON THE IDENTITY, not on the verdict — and that is the whole discrimination
        // here. This record's plan carries `accept_dirty`, so a borrowed `Matches` would reach
        // `Stow` too: a test asserting only the verdict would pass whether or not the tree's
        // identity had been taken from the enclosing repo, which is the one thing it exists to
        // forbid. `Wreck` is reachable only through `worktree_identity`, which compares
        // `--show-toplevel` against the directory itself.
        assert_eq!(
            observe_pending(&entry).identity,
            IdentityMatch::Wreck,
            "the tree is judged on what IT says, so a commit that happens to match the enclosing \
             repo's HEAD establishes nothing"
        );
        // And a wreck is stowed. This used to be a hold, and the review that changed it is the
        // reason: `worktree_head` answers `Ok(None)` for a wreck AND for a vanished tree, so a
        // record carrying a head could never be told the two apart and was held — while the
        // head-LESS record for this identical state was stowed. The better-informed record got
        // the worse outcome, and once a routine `git worktree prune` had unregistered the broken
        // tree the hold's advice became "remove it yourself". `stow` is a rename inside our own
        // `.jkb/`, reversible for the retention window; deleting a checkout is not.
        assert_eq!(
            pending_verdict(&entry),
            Verdict::Stow,
            "a wreck can never prove its identity, so holding for one is a permanent hold"
        );
    }

    /// A session the sweep cannot STAT is not one somebody removed.
    ///
    /// The absence arm is the destructive pair: it prunes git's registration, applies the
    /// record's branch deletion — forced, and after `abandon --delete-branch` that branch is the
    /// only copy of the session's commits — and drops the record, so nothing tracks the checkout
    /// afterwards. `Path::exists()` answers `false` for ANY stat error, so an untraversable
    /// `.jkb/work` drove all three against a session that was still there. Same defect as
    /// `worktree_identity`'s, at a site that fix did not reach.
    #[test]
    fn a_session_the_sweep_cannot_stat_is_not_one_somebody_removed() {
        use std::os::unix::fs::PermissionsExt;

        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, branch, head) = session(&repo, "sess");

        record(
            &db,
            &Entry {
                head: Some(head),
                branch: branch.clone(),
                plan: Plan {
                    delete_branch: true,
                    accept_dirty: false,
                },
                ..entry(&wt, &repo)
            },
        )
        .expect("record");

        let work = repo.join(".jkb/work");
        fs::set_permissions(&work, fs::Permissions::from_mode(0o000)).expect("chmod 000");
        let stat = fs::metadata(&wt).err().map(|e| e.kind());
        let swept = reap(&db, 0, false);
        fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).expect("restore");

        assert_eq!(
            stat,
            Some(io::ErrorKind::PermissionDenied),
            "the premise — the stat must actually fail, or this test is about nothing"
        );
        let r = swept.expect("reap");
        assert!(
            r.cleared.is_empty(),
            "nobody established the session was removed, so its record stands: {r:?}"
        );
        assert_eq!(
            r.held.len(),
            1,
            "and it is held, with a reason: {:?}",
            r.held
        );
        assert!(
            !git(&repo, &["branch", "--list", &branch]).is_empty(),
            "and the branch is still there"
        );
    }

    /// Nor is an archive it cannot stat.
    ///
    /// Quieter and worse: the record is the only thing that names a multi-gigabyte archive, so
    /// dropping it on an unproven absence leaves the directory referenced by nothing and never
    /// deleted — the same harm the repo-root check above the dispatch exists to prevent, one
    /// level down.
    #[test]
    fn an_archive_the_sweep_cannot_stat_is_not_one_somebody_removed() {
        use std::os::unix::fs::PermissionsExt;

        let t = tempfile::tempdir().expect("tempdir");
        let db = t.path().join("jkb.db");
        let repo = t.path().join("repo");
        let (wt, _, head) = session(&repo, "sess");
        let root = archive_root(&repo);
        let archived = root.join("sess-20260101T000000Z");
        fs::create_dir_all(&archived).expect("mk");
        fs::write(archived.join("f"), b"x").expect("write");

        record(
            &db,
            &Entry {
                head: Some(head),
                archive: Some(archived.clone()),
                archived_at: Some(0),
                ..entry(&wt, &repo)
            },
        )
        .expect("record");

        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).expect("chmod 000");
        let stat = fs::metadata(&archived).err().map(|e| e.kind());
        // Retention 0, so a sweep that got past the guard would go on to delete it.
        let swept = reap(&db, 0, false);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("restore");

        assert_eq!(
            stat,
            Some(io::ErrorKind::PermissionDenied),
            "the premise — the stat must actually fail, or this test is about nothing"
        );
        let r = swept.expect("reap");
        assert!(
            r.cleared.is_empty(),
            "the only thing that names the archive is not thrown away on a stat error: {r:?}"
        );
        assert_eq!(r.held.len(), 1, "it is held, with a reason: {:?}", r.held);
        assert!(archived.exists(), "and the archive is untouched");
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
        // ITS RETURN VALUE, not merely that there was one. `break_held` read the file directly
        // instead of going through `holder_of`, so `--break-lock` printed the owner id with the
        // release nonce glued on while `--dry-run --break-lock`, which goes through
        // `lock_holder`, printed it clean — one identity rendered two ways by adjacent lines of
        // one command, and the noisy one is not something `jkb task release --owner` matches.
        assert_eq!(
            break_lock(&db).expect("break").as_deref(),
            Some(crate::owner::self_owner().as_str()),
            "every reader of the lock file goes through the one parser"
        );
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
        let one = first.content.clone();
        break_lock(&db).expect("break").expect("there was a holder");
        let Ok(second) = SweepLock::acquire(&db).expect("acquire") else {
            panic!("free again once broken")
        };
        let two = second.content.clone();

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
