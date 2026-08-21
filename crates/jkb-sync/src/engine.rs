//! The reconciliation engine: one-shot `sync` over a `file://` mount.
//!
//! Each bound file is reconciled independently inside one `write_txn` (atomic +
//! audited). Direction is decided from the **journal** (`sync_state`, one row per
//! file uri) using a **three-way** comparison against the last-synced **base** bytes:
//! `disk` vs `base` and `kb` (the current KB rendered through the serializer) vs
//! `base`, never `disk` vs `kb`. This is what lets a multi-item file distinguish "the
//! KB changed task A" from "the disk changed task B" and auto-merge disjoint edits
//! instead of declaring a whole-file conflict (design D25).
//!
//! A file's items are gathered by the bindings `file://<path>` and
//! `file://<path>#<local_id>` (design D24); the serializer maps those bytes to a
//! [`SyncDoc`] and back. On a `tasks` parse failure the file is **quarantined** — its
//! last-good items are left intact, the failing bytes are stashed, and the journal is
//! flagged `needs_attention` — rather than overwritten.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobMatcher};
use serde_json::json;
use walkdir::WalkDir;

use jkb_core::item::NewItem;
use jkb_core::{
    binding, blob, edge, item, mount, ns, placement, sync_state, tag, task, Db, WriteMeta,
};
use jkb_types::{
    ConflictPolicy, EdgeType, Error as TypeError, ItemId, NamespaceId, PlacementRole, SyncMode,
    TaskStatus,
};
use rusqlite::{Connection, OptionalExtension};

use crate::lifecycle::{FileEvent, FileState};
use crate::serializers::{resolve, SyncBlock, SyncDoc, SyncItem, SyncSection, SyncSerializer};
use crate::{Error, Result};

/// What happened when reconciling one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A new file on disk was imported for the first time (items created).
    Created,
    /// Changed disk content was imported into the KB (disk → KB).
    Imported,
    /// A changed item was rendered back to its file (KB → disk).
    Exported,
    /// Disjoint disk and KB edits were merged three-way (both sides reconciled).
    Merged,
    /// Both sides changed the same unit and the policy is `manual`: nothing modified.
    Conflict,
    /// The file failed to parse and was quarantined (last-good items kept, bytes stashed).
    Quarantined,
    /// Neither side changed in substance, but the stored bytes were not today's canonical
    /// rendering, so the file and the base were re-settled to it. Reported separately from
    /// `UpToDate` because it does write the file.
    Normalized,
    /// Neither side changed since the last sync.
    UpToDate,
    /// The mount's `sync_mode` does not permit the needed direction.
    Skipped,
    /// A both-changed file was resolved by the conflict policy in favour of **disk**; the
    /// KB side was discarded. Reported apart from `Imported` because a policy resolution
    /// throws work away, and that should never be indistinguishable from an ordinary import.
    ResolvedFromDisk,
    /// A both-changed file was resolved in favour of the **KB**; the on-disk bytes were
    /// overwritten. The losing bytes are blobbed first, so `jkb blob ls --contains` can still
    /// recover them.
    ResolvedFromKb,
    /// Exporting would have deleted item lines, so nothing was written (design D45.5).
    /// Journalled `needs_attention`.
    ///
    /// **Two causes, and they want opposite remedies** — [`FileResult::reason`] carries which one,
    /// and it must be read rather than assumed:
    ///
    /// - *Still bound, no primary placement* ([`dropped_items`]). Restore the placement
    ///   (`jkb task place <uid> <ns> --home`), or delete those lines from the file if the items
    ///   really are meant to go — an item absent from disk stops being expected and `apply_doc`
    ///   detaches it. An earlier version judged expectation from the *base* rather than from disk,
    ///   which made the refusal unclearable from the file.
    /// - *Nothing bound at all* ([`wholesale_loss`], on an export-only mount, which cannot heal
    ///   itself). Here the file is the only good copy, so deleting lines from it destroys the very
    ///   thing being protected and does not clear the refusal — the count comes from disk, so it
    ///   stays non-zero. Re-read the file instead.
    Refused,
    /// Reconciling this file errored. Reported rather than propagated, so one bad file cannot
    /// end the run — or, under the watcher, silently kill a mount's thread for good.
    Failed,
}

/// The result of reconciling one file.
#[derive(Debug, Clone)]
pub struct FileResult {
    /// The absolute file path.
    pub path: PathBuf,
    /// What happened.
    pub outcome: Outcome,
    /// Why, when the outcome alone does not say — a refusal's reason, as written to the
    /// journal.
    ///
    /// The engine computes the reason once, for the journal; the printers render it rather
    /// than restating the rule — a printer that restates it drifts from the engine, which is
    /// how a refused file came to be told to exclude a sibling that did not exist.
    pub reason: Option<String>,
}

/// The outcome of a one-shot [`sync`] over a mount.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    /// One entry per reconciled file.
    pub results: Vec<FileResult>,
}

impl SyncReport {
    /// The paths that reported a [`Outcome::Conflict`].
    #[must_use]
    pub fn conflicts(&self) -> Vec<&Path> {
        self.paths_with(Outcome::Conflict)
    }

    /// The paths that were [`Outcome::Quarantined`].
    #[must_use]
    pub fn quarantined(&self) -> Vec<&Path> {
        self.paths_with(Outcome::Quarantined)
    }

    /// The paths that were three-way [`Outcome::Merged`].
    #[must_use]
    pub fn merged(&self) -> Vec<&Path> {
        self.paths_with(Outcome::Merged)
    }

    /// The paths a conflict policy resolved, and which side won. A resolution discards the
    /// other side's edits, so it is reported per file rather than folded into the counts.
    #[must_use]
    pub fn resolved(&self) -> Vec<(&Path, &'static str)> {
        self.results
            .iter()
            .filter_map(|r| match r.outcome {
                Outcome::ResolvedFromDisk => {
                    Some((r.path.as_path(), "disk won, KB edits discarded"))
                }
                Outcome::ResolvedFromKb => Some((
                    r.path.as_path(),
                    "KB won, on-disk edits overwritten (blobbed first)",
                )),
                _ => None,
            })
            .collect()
    }

    /// Files whose reconcile **errored**, with the error text. Distinct from
    /// [`SyncReport::refused`]: a refusal is a deliberate decision not to write; a failure is
    /// something going wrong.
    #[must_use]
    pub fn failed(&self) -> Vec<(&Path, &str)> {
        self.results
            .iter()
            .filter(|r| r.outcome == Outcome::Failed)
            .map(|r| {
                (
                    r.path.as_path(),
                    r.reason.as_deref().unwrap_or("unknown error"),
                )
            })
            .collect()
    }

    /// Files an export refused because it would have deleted item lines (design D45.5). Each
    /// carries the engine's own reason in [`FileResult::reason`], and the reason matters: the two
    /// causes want opposite remedies. `dropped_items` fires when the items are **still bound** but
    /// have lost their primary placement, and points at re-homing them; `wholesale_loss` fires
    /// when the KB holds nothing for the file at all — bindings included — and points at reading
    /// the file back in. Do not paraphrase this as one or the other.
    #[must_use]
    pub fn refused(&self) -> Vec<(&Path, &str)> {
        self.results
            .iter()
            .filter(|r| r.outcome == Outcome::Refused)
            .map(|r| {
                (
                    r.path.as_path(),
                    r.reason.as_deref().unwrap_or("refused; see `jkb doctor`"),
                )
            })
            .collect()
    }

    fn paths_with(&self, outcome: Outcome) -> Vec<&Path> {
        self.results
            .iter()
            .filter(|r| r.outcome == outcome)
            .map(|r| r.path.as_path())
            .collect()
    }

    /// How many files reported `outcome`.
    #[must_use]
    pub fn count(&self, outcome: Outcome) -> usize {
        self.results.iter().filter(|r| r.outcome == outcome).count()
    }
}

/// The mount configuration needed to reconcile a file, owned so it can move into the
/// writer-thread closure.
#[derive(Debug, Clone)]
struct Ctx {
    mount_ns: String,
    dir: PathBuf,
    sync_mode: String,
    conflict_policy: String,
    serializer: String,
}

impl Ctx {
    fn imports(&self) -> bool {
        self.sync_mode == "import" || self.sync_mode == "bidirectional"
    }
    fn exports(&self) -> bool {
        self.sync_mode == "export" || self.sync_mode == "bidirectional"
    }
}

/// Run a one-shot sync over the mount at `mount_ns`.
///
/// # Errors
/// Returns an error if there is no mount at `mount_ns`, its backing uri is not a
/// `file://` path, its serializer is unknown, or a filesystem/database operation
/// fails. Conflicts and quarantines are reported in the [`SyncReport`], not as errors.
pub fn sync(db: &Db, mount_ns: &str) -> Result<SyncReport> {
    sync_with_policy(db, mount_ns, None)
}

/// Run a one-shot sync, optionally overriding the mount's conflict policy **for this run
/// only** (design D25: the policy is a mount property, but resolving one stuck file should
/// not require editing the mount).
///
/// Before this existed, the only way to resolve a conflict was to re-create the mount with a
/// different `--policy` — a write that also reset every property the caller did not restate,
/// which is how a mount's include glob was dropped and 62 files were overwritten. A per-run
/// override means the mount is never edited to get a sync unstuck.
///
/// # Errors
/// Same as [`sync`].
pub fn sync_with_policy(
    db: &Db,
    mount_ns: &str,
    policy: Option<ConflictPolicy>,
) -> Result<SyncReport> {
    let mut ctx = load_ctx(db, mount_ns)?;
    let _ = resolve(&ctx.serializer)?; // fail fast on an unknown mount serializer
    if let Some(p) = policy {
        // Typed, so there is no string to re-validate: the caller already has the enum, and
        // passing it as text meant a third hand-written spelling that could disagree.
        p.as_str().clone_into(&mut ctx.conflict_policy);
    }

    let filter = Filter::build(&read_globs(db, mount_ns)?)?;
    settle_out_of_scope(db, &ctx, &filter)?;
    let paths = discover(db, &ctx, &filter)?;
    reconcile_all(db, &ctx, paths)
}

/// Clear the flag on any file under this mount that it no longer syncs.
///
/// Driven off the **journal**, not off bindings. A file that failed to parse on its first
/// sync has a `needs_attention` row and no bindings at all — `apply_doc` never ran — so a
/// bindings-driven sweep could not see it, and once the user deleted the unparseable file
/// nothing could ever clear the flag: `jkb doctor` reported a parse failure for a file that
/// did not exist, forever. That is the stuck state this function exists to close.
///
/// A row is settled when the mount no longer selects the file **and** there is nothing left
/// on disk or in the KB for it — an excluded file, a deleted one, or both. The row is settled
/// rather than deleted so its base survives if the file comes back.
///
/// Only the full [`sync`] does this: a watch-driven `sync_paths` sees a few event paths and
/// cannot tell "out of scope" from "not in this batch".
fn settle_out_of_scope(db: &Db, ctx: &Ctx, filter: &Filter) -> Result<usize> {
    let dir = ctx.dir.clone();
    let flagged = db.read(move |conn| sync_state::flagged_under(conn, &dir))?;
    if flagged.is_empty() {
        return Ok(0);
    }
    let bound = bound_paths(db, ctx)?;
    let stale: Vec<String> = flagged
        .into_iter()
        .filter(|row| {
            let Some(path) = row.uri.strip_prefix("file://").map(PathBuf::from) else {
                return false;
            };
            // Two ways this mount is finished with a file, and only these two:
            //   - the globs no longer select it, or
            //   - it is gone from disk AND nothing in the KB is bound to it, so `discover`
            //     will never yield it again and no reconcile will ever revisit its row.
            // Anything else is still syncing, and its own reconcile owns the flag.
            //
            // Which "the globs no longer select it" means depends on whether the file is
            // bound, exactly as `discover` decides: `accepts_bound` ignores `include`, because
            // narrowing it must not orphan a file the KB already syncs — but an *unbound*
            // file has nothing to orphan, and that is the case this sweep exists for (a first
            // parse failure quarantines before `apply_doc` runs, so it has no bindings at
            // all). Using `accepts_bound` for it left a narrowed `--include` unable to clear
            // the very row it was narrowed to escape.
            let in_scope = if bound.contains(&path) {
                filter.accepts_bound(&ctx.dir, &path)
            } else {
                filter.accepts(&ctx.dir, &path)
            };
            !in_scope || (!path.exists() && !bound.contains(&path))
        })
        .map(|row| row.uri)
        .collect();
    if stale.is_empty() {
        return Ok(0);
    }
    db.write_txn_with::<usize, Error, _>("sync", move |conn, meta| {
        let mut cleared = 0;
        for uri in &stale {
            if sync_state::settle(conn, meta, uri)? {
                cleared += 1;
            }
        }
        Ok(cleared)
    })
}

/// Reconcile only the given `paths` (e.g. the paths named by filesystem-watch events),
/// deduplicated and scoped to the mount's directory and include/exclude globs.
///
/// # Errors
/// Same as [`sync`].
pub fn sync_paths(db: &Db, mount_ns: &str, paths: &[PathBuf]) -> Result<SyncReport> {
    let ctx = load_ctx(db, mount_ns)?;
    let _ = resolve(&ctx.serializer)?;

    let filter = Filter::build(&read_globs(db, mount_ns)?)?;
    let mut relevant: Vec<PathBuf> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for path in paths {
        if filter.accepts(&ctx.dir, path) && seen.insert(path.clone()) {
            relevant.push(path.clone());
        }
    }
    reconcile_all(db, &ctx, relevant)
}

/// Reconcile each path in its own audited transaction, collecting the outcomes.
fn reconcile_all(db: &Db, ctx: &Ctx, paths: Vec<PathBuf>) -> Result<SyncReport> {
    let mut results = Vec::with_capacity(paths.len());
    for path in paths {
        // ARCHIVE FIRST, in its own committed transaction, before the reconcile that may
        // overwrite the file (design D25's recovery story: "blobs are content-addressed and
        // never garbage-collected… the store is a complete history of every synced file").
        //
        // Placed here rather than at a `write_file` call site for two reasons the previous
        // hand-placed version got wrong. There are FOUR sites that overwrite a synced file
        // (`Normalized`, `finish_import`, `finish_export`, the merge arm) and only one carried
        // the rule, so three could destroy bytes no blob held. And the one that did carry it
        // ran inside the reconcile's transaction, so a later failure rolled the archive back
        // while the file stayed overwritten — losing exactly the bytes it existed to keep.
        // Content-addressed and `INSERT OR IGNORE`, so a settled file costs one hash and one
        // no-op insert.
        // A failed archive STOPS this file. Dropping the error meant the guarantee degraded to
        // best-effort exactly when the database is contended — the reconcile went on to
        // overwrite bytes that were in no blob, with nothing printed and nothing journalled.
        // Leaving the file alone is strictly better: nothing is lost, and the next pass retries.
        let archived = match archive_current_bytes(db, &path) {
            Ok(bytes) => bytes,
            Err(e) => {
                let reason = format!("could not archive the current bytes before syncing: {e}");
                let (p2, msg, ser) = (path.clone(), reason.clone(), ctx.serializer.clone());
                let _ = db.write_txn_with::<(), Error, _>("sync", move |conn, meta| {
                    let uri = file_uri(&p2);
                    let prev = sync_state::get(conn, &uri)?;
                    flag_needs_attention(conn, meta, &uri, &ser, &msg, prev.as_ref())
                });
                results.push(FileResult {
                    path,
                    outcome: Outcome::Failed,
                    reason: Some(reason),
                });
                continue;
            }
        };
        let ser_name = ctx.serializer.clone();
        let ctx = ctx.clone();
        let p = path.clone();
        // A per-file failure is a RESULT, not a run-ending error. `reconcile_all` used to
        // propagate with `?`, so a single unreadable file — a PNG dropped into a `document`
        // mount, whose serializer does not quarantine — returned `Err` out of the watcher
        // thread. `watch_all` then blocked joining the other threads until stop, launchd never
        // restarted the still-alive process, and that mount silently stopped syncing forever.
        let outcome = db.write_txn_with::<Outcome, Error, _>("sync", move |conn, meta| {
            reconcile(conn, meta, &ctx, &p, archived)
        });
        let (outcome, reason) = match outcome {
            Ok(o) => (o, outcome_reason(db, &path)?),
            Err(e) => {
                // Flag the journal in its OWN transaction: the reconcile's rolled back, so
                // without this the row keeps `status='ok'` and the failure is invisible to
                // `jkb doctor` — leaving one stderr line under the watcher as its only trace.
                let (p2, msg) = (path.clone(), e.to_string());
                let ser = ser_name.clone();
                let _ = db.write_txn_with::<(), Error, _>("sync", move |conn, meta| {
                    let uri = file_uri(&p2);
                    let prev = sync_state::get(conn, &uri)?;
                    flag_needs_attention(conn, meta, &uri, &ser, &msg, prev.as_ref())
                });
                (Outcome::Failed, Some(e.to_string()))
            }
        };
        results.push(FileResult {
            path,
            outcome,
            reason,
        });
    }
    // Any tasks just imported from `repos/<repo>/…/tasks.md` get a `tasks/…` mirror so
    // `tasks/**` stays the complete task index. Only run when a file actually changed —
    // a pure-no-op reconcile must not open a write txn, or the watcher would re-fire on
    // its own commit and spin (the file-watch feedback loop).
    let imported = results.iter().any(|r| brought_items_in(r.outcome));
    if imported {
        db.write_txn_with::<usize, Error, _>("sync", |conn, meta| {
            Ok(task::ensure_all_mirrors(conn, meta)?)
        })?;
    }
    Ok(SyncReport { results })
}

/// Store the file's current bytes in the blob archive, in their own transaction.
///
/// A **precondition**, not a safety net: if this fails the caller skips the file rather than
/// overwriting bytes no blob holds. The previous doc said the opposite — "a failure here must
/// not stop the reconcile" — which is the instruction that produced the `let _ =` a review had
/// to remove, so it is corrected rather than left to mislead the next reader.
///
/// Committed separately so it survives a rollback of the reconcile that follows.
///
/// Returns the bytes it archived, so the caller reconciles **those exact bytes** rather than
/// reading the file a second time. Two reads meant a save landing between them was overwritten
/// with only the older version archived, and the window widened when the archive moved into its
/// own transaction.
///
/// # Errors
/// Returns an error if the file cannot be read (other than not existing) or the blob write
/// fails. A missing file is `Ok(None)`: there is nothing to lose.
fn archive_current_bytes(db: &Db, path: &Path) -> Result<Option<Vec<u8>>> {
    // A file that is not there has nothing to lose. Anything ELSE that stops us reading it —
    // permissions, an I/O error — is a failure, because the reconcile is about to overwrite
    // bytes we could not copy. Swallowing every `fs::read` error put the hole back one layer
    // below where the caller just closed it.
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if bytes.is_empty() {
        return Ok(Some(bytes));
    }
    let stored = bytes.clone();
    db.write_txn_with::<(), Error, _>("sync", move |conn, _meta| {
        blob::store(conn, &blob::hash_bytes(&stored), &stored, None)?;
        Ok(())
    })?;
    Ok(Some(bytes))
}

/// Whether this outcome may have brought items **in** from disk, so the `tasks/**` mirrors
/// need re-deriving.
///
/// Written as "everything that is not one of these", so a new import-shaped variant counts by
/// default and only a deliberate addition to the list opts out. The allowlist spelling failed
/// exactly once, and silently: splitting `disk_wins` out of `Imported` into
/// `ResolvedFromDisk` — for the good reason that a policy resolution throws work away and
/// should not read as an ordinary import — dropped it from here, so a conflict-resolved task
/// was homed under `repos/<repo>/…` with no `tasks/<repo>` mirror, and `jkb task next`, which
/// scopes to `tasks/<repo>/**`, never saw it.
fn brought_items_in(outcome: Outcome) -> bool {
    match outcome {
        // Nothing was read from disk, or nothing changed at all.
        Outcome::UpToDate
        | Outcome::Skipped
        | Outcome::Conflict
        | Outcome::Quarantined
        // Nothing was written at all.
        | Outcome::Refused
        | Outcome::Failed
        // KB → disk: the items were already in the KB, and already mirrored.
        | Outcome::Exported
        | Outcome::ResolvedFromKb
        | Outcome::Normalized => false,
        Outcome::Created | Outcome::Imported | Outcome::Merged | Outcome::ResolvedFromDisk => true,
    }
}

#[cfg(test)]
mod seam_guard {
    use super::{wholesale_loss, SyncDoc, SyncItem};

    fn doc(items: usize) -> SyncDoc {
        SyncDoc {
            items: (0..items)
                .map(|i| SyncItem::new(format!("i{i}"), "task", "t"))
                .collect(),
            ..SyncDoc::default()
        }
    }

    /// The harm: the KB has lost everything bound to a file that still declares work.
    #[test]
    fn an_empty_kb_side_may_not_overwrite_a_file_that_declares_items() {
        let reason = wholesale_loss(&doc(0), &doc(3))
            .expect("an item-less render over a populated file must be refused");
        assert!(
            reason.contains('3'),
            "the reason must say how much: {reason}"
        );
    }

    /// Deleting the last task in a file is an ordinary edit, and an export-only mount is
    /// *supposed* to overwrite hand-added lines. Neither is wholesale loss, and refusing either
    /// would wedge the mount on every run.
    #[test]
    fn an_ordinary_export_is_not_refused() {
        assert!(
            wholesale_loss(&doc(2), &doc(3)).is_none(),
            "fewer items is an edit"
        );
        assert!(
            wholesale_loss(&doc(1), &doc(0)).is_none(),
            "the KB adding one is an export"
        );
        assert!(
            wholesale_loss(&doc(0), &doc(0)).is_none(),
            "both empty loses nothing"
        );
    }
}

#[cfg(test)]
mod write_seam {
    use super::write_file;

    /// The write seam refuses when the file is no longer what the pass reconciled.
    ///
    /// `reconcile` decides direction from bytes read by `archive_current_bytes`, which commits
    /// its own transaction and then queues behind the writer thread — so under load the snapshot
    /// can be seconds stale, and a save landing in that window used to be overwritten with only
    /// the older bytes archived. Driven directly here because the window is inside one `sync`
    /// call and cannot be opened deterministically from outside it.
    #[test]
    fn refuses_when_the_file_changed_since_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.md");
        std::fs::write(&path, b"v2").unwrap();

        // The pass read "v1"; the file now says "v2".
        let err = write_file(&path, b"render", Some(b"v1")).unwrap_err();
        assert!(
            err.to_string().contains("changed on disk"),
            "must refuse a stale write: {err}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"v2",
            "and must not have written anything"
        );

        // Matching snapshot: the write goes ahead.
        write_file(&path, b"render", Some(b"v2")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"render");
    }

    /// A file that was ABSENT when the pass began, and exists now, is the one overwrite that
    /// would be recoverable from nothing — the archive stored no bytes for it.
    #[test]
    fn refuses_when_an_absent_file_has_appeared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("restored.md");
        std::fs::write(&path, b"git restored me").unwrap();

        let err = write_file(&path, b"render", None).unwrap_err();
        assert!(err.to_string().contains("changed on disk"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"git restored me");

        // Still absent: writing it is the ordinary create.
        let fresh = dir.path().join("new.md");
        write_file(&fresh, b"render", None).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"render");
    }
}

#[cfg(test)]
mod mirror_predicate {
    use super::{brought_items_in, Outcome};

    /// The `match` is exhaustive, so adding an outcome stops this compiling until someone
    /// decides which side it falls on — which is the whole point: the previous shape was an
    /// allowlist a new variant could simply miss.
    #[test]
    fn every_outcome_that_reads_from_disk_triggers_the_mirror_pass() {
        for o in [
            Outcome::Created,
            Outcome::Imported,
            Outcome::Merged,
            Outcome::ResolvedFromDisk,
        ] {
            assert!(brought_items_in(o), "{o:?} imports items");
        }
        for o in [
            Outcome::Exported,
            Outcome::ResolvedFromKb,
            Outcome::Normalized,
            Outcome::UpToDate,
            Outcome::Skipped,
            Outcome::Conflict,
            Outcome::Quarantined,
            Outcome::Refused,
            Outcome::Failed,
        ] {
            assert!(!brought_items_in(o), "{o:?} does not import items");
        }
    }
}

/// The absolute backing directory of the mount at `mount_ns` (for the watcher).
///
/// # Errors
/// Returns an error if there is no mount there or its backing uri is not `file://`.
pub fn backing_dir(db: &Db, mount_ns: &str) -> Result<PathBuf> {
    Ok(load_ctx(db, mount_ns)?.dir)
}

/// If `home_ns` — or an ancestor of it — is a `tasks`-serializer `file://` mount, return the
/// bare binding uri of that mount's root tasks file (`file://<backing_dir>/tasks.md`). A task
/// homed under such a mount can bind to `<that>#<local_id>` and round-trip via [`sync`]
/// (design D26.5). Returns `None` when no `tasks` mount covers the home namespace, so the
/// caller keeps the task `managed:`. The first mount encountered while walking up stops the
/// search: a non-`tasks` mount covering the home yields `None` rather than crossing it.
///
/// # Errors
/// Returns an error if a database read fails.
pub fn tasks_mount_file(db: &Db, home_ns: &str) -> Result<Option<String>> {
    let home = home_ns.to_owned();
    let uri = db.read(move |conn| {
        let mut cur = Some(home);
        while let Some(path) = cur {
            if let Some(ns_id) = ns::get(conn, &path)? {
                if let Some(m) = mount::get(conn, ns_id)? {
                    if m.serializer == "tasks" {
                        if let Some(dir) = m.backing_uri.strip_prefix("file://") {
                            let dir = dir.trim_end_matches('/');
                            return Ok(Some(format!("file://{dir}/tasks.md")));
                        }
                    }
                    return Ok(None);
                }
            }
            cur = path.rsplit_once('/').map(|(parent, _)| parent.to_owned());
        }
        Ok(None)
    })?;
    Ok(uri)
}

/// Load the mount configuration into an owned [`Ctx`].
fn load_ctx(db: &Db, mount_ns: &str) -> Result<Ctx> {
    let path = mount_ns.to_owned();
    db.read_with::<Ctx, Error, _>(move |conn| {
        let ns_id = ns::get(conn, &path)?
            .ok_or_else(|| Error::Types(TypeError::NotFound(format!("namespace `{path}`"))))?;
        let m = mount::get(conn, ns_id)?
            .ok_or_else(|| Error::Types(TypeError::NotFound(format!("mount at `{path}`"))))?;
        let dir = m.backing_uri.strip_prefix("file://").ok_or_else(|| {
            Error::Types(TypeError::Validation(format!(
                "mount `{path}` backing uri `{}` is not a file:// path",
                m.backing_uri
            )))
        })?;
        Ok(Ctx {
            mount_ns: path.clone(),
            dir: PathBuf::from(dir),
            sync_mode: m.sync_mode,
            conflict_policy: m.conflict_policy,
            serializer: m.serializer,
        })
    })
}

/// The mount's `(include_glob, exclude_glob)`.
fn read_globs(db: &Db, mount_ns: &str) -> Result<(Option<String>, Option<String>)> {
    let path = mount_ns.to_owned();
    let m = db.read(move |conn| {
        let ns_id = ns::get(conn, &path)?;
        match ns_id {
            Some(id) => mount::get(conn, id),
            None => Ok(None),
        }
    })?;
    Ok(m.map_or((None, None), |m| (m.include_glob, m.exclude_glob)))
}

/// The mount's compiled include/exclude globs.
#[derive(Clone)]
struct Filter {
    include: Option<GlobMatcher>,
    exclude: Option<GlobMatcher>,
}

impl Filter {
    fn build(globs: &(Option<String>, Option<String>)) -> Result<Self> {
        let compile = |g: &Option<String>| -> Result<Option<GlobMatcher>> {
            Ok(g.as_ref()
                .map(|g| Glob::new(g))
                .transpose()?
                .map(|g| g.compile_matcher()))
        };
        Ok(Self {
            include: compile(&globs.0)?,
            exclude: compile(&globs.1)?,
        })
    }

    /// Whether an absolute `path` under `dir` is in scope.
    fn accepts(&self, dir: &Path, path: &Path) -> bool {
        if !path.starts_with(dir) {
            return false;
        }
        let rel = rel_str(dir, path);
        self.include.as_ref().is_none_or(|m| m.is_match(&rel))
            && !self.exclude.as_ref().is_some_and(|m| m.is_match(&rel))
    }

    /// Like [`Self::accepts`] but ignoring `include` — for already-bound files.
    fn accepts_bound(&self, dir: &Path, path: &Path) -> bool {
        path.starts_with(dir)
            && !self
                .exclude
                .as_ref()
                .is_some_and(|m| m.is_match(rel_str(dir, path)))
    }
}

/// The set of files to reconcile: those on disk matching the globs, unioned with the
/// backing files of items already bound under the mount. Binding uris may carry a
/// `#<local_id>` fragment (multi-item files), so they are stripped back to the file
/// path and deduplicated (design D24 — one file, many item bindings).
fn discover(db: &Db, ctx: &Ctx, filter: &Filter) -> Result<Vec<PathBuf>> {
    let mut set: BTreeSet<PathBuf> = BTreeSet::new();

    for entry in WalkDir::new(&ctx.dir)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if entry.file_type().is_file() && filter.accepts(&ctx.dir, entry.path()) {
            set.insert(entry.path().to_path_buf());
        }
    }

    for path in bound_paths(db, ctx)? {
        if filter.accepts_bound(&ctx.dir, &path) {
            set.insert(path);
        }
    }

    Ok(set.into_iter().collect())
}

/// Every **file** the KB already syncs under this mount, deduplicated.
///
/// The one place binding uris are turned into paths. There were verbatim copies at each call
/// site — [`discover`] and [`settle_out_of_scope`] today — each running its own read and its own
/// `strip_prefix`/`split_once` parsing, so whoever adds percent-encoding or a second fragment
/// form would have to find every one of them, and a caller left behind sees a different set of
/// synced files from the rest of the engine.
///
/// Deduplicated per file, not per binding: the `tasks` serializer binds one uri per checkbox
/// line, so a file with 262 tasks yielded 262 identical paths to whatever came next.
fn bound_paths(db: &Db, ctx: &Ctx) -> Result<BTreeSet<PathBuf>> {
    let mount_ns = ctx.mount_ns.clone();
    let uris = db.read(move |conn| binding::synced_uris_under(conn, &mount_ns))?;
    let mut out = BTreeSet::new();
    for uri in uris {
        let Some(raw) = uri.strip_prefix("file://") else {
            continue;
        };
        let bare = raw.split_once('#').map_or(raw, |(p, _)| p);
        out.insert(PathBuf::from(bare));
    }
    Ok(out)
}

/// The journal's explanation for `path`, when it has one — the reason a reconcile refused.
fn outcome_reason(db: &Db, path: &Path) -> Result<Option<String>> {
    let uri = file_uri(path);
    Ok(db
        .read(move |conn| sync_state::get(conn, &uri))?
        .filter(|j| j.status != "ok")
        .and_then(|j| j.parse_error))
}

/// Reconcile a single file within the current transaction.
// A linear pipeline — read disk, assemble KB, decide direction, act — ending in one arm per
// direction. Splitting it further would mean threading the same seven parameters through more
// helpers than it saves; the stages that carry real logic (`decide_direction`, `export_or_skip`)
// are already extracted and separately testable.
#[allow(clippy::too_many_lines)]
fn reconcile(
    conn: &Connection,
    meta: &WriteMeta,
    ctx: &Ctx,
    path: &Path,
    // The bytes the archive already read and stored, so the version reconciled is exactly the
    // version preserved. Reading the file again here left a window in which a save could land
    // between the two reads and be overwritten with only the older bytes archived.
    disk_bytes: Option<Vec<u8>>,
) -> Result<Outcome> {
    let bare_uri = file_uri(path);
    let (ser_name, serializer) = resolve_serializer(conn, ctx, &bare_uri)?;
    let journal = sync_state::get(conn, &bare_uri)?;
    // A row written before D45 has its structure in the namespace tree, not here. Populate it
    // ONCE, from the file's own base blob (or the file on disk) — both are per-file by
    // construction, unlike the pre-D39 directory namespace, which is shared by every file in the
    // directory and whose ambiguity is what seven earlier guards failed to resolve.
    //
    // Placed before the quarantine early-return below, or a file that fails to parse would never
    // be populated at all.
    let journal = populate_document(conn, meta, &bare_uri, serializer.as_ref(), path, journal)?;
    let base_hash = journal.as_ref().and_then(|j| j.last_synced_hash.clone());

    // Parse the disk side (if the file exists). A quarantining serializer turns a parse
    // failure into a journal flag instead of a hard error, protecting last-good items.
    let disk = if let Some(bytes) = disk_bytes {
        match serializer.parse(&bytes) {
            Ok(doc) => Some((bytes, doc)),
            Err(e) => {
                if serializer.quarantine_on_parse_error() {
                    return quarantine(
                        conn,
                        meta,
                        &bare_uri,
                        &ser_name,
                        &bytes,
                        &e,
                        journal.as_ref(),
                    );
                }
                return Err(e);
            }
        }
    } else {
        None
    };

    // Everything the reconcile helpers need about this file, bundled once. `snapshot` is the
    // bytes read above — the single authoritative answer to "what did this pass see on disk",
    // which the write seam re-checks before overwriting.
    let f = FileCtx {
        ctx,
        path,
        bare_uri: &bare_uri,
        ser_name: &ser_name,
        serializer: serializer.as_ref(),
        journal: journal.as_ref(),
        snapshot: disk.as_ref().map(|(bytes, _)| bytes.as_slice()),
    };

    // Assemble the KB side and render it, so we can hash it against the base.
    let kb_doc = assemble_kb_doc(conn, ctx, path, &bare_uri, journal.as_ref())?;
    let kb_bytes = serializer.render(&kb_doc)?;
    let kb_hash = hash(&kb_bytes);

    let Some((disk_bytes, disk_doc)) = disk.as_ref() else {
        return export_or_skip(conn, meta, &f, &kb_doc);
    };

    let disk_hash = hash(disk_bytes);

    // First sight of this file (no journal row yet).
    if journal.is_none() {
        if ctx.imports() {
            return finish_import(conn, meta, &f, disk_doc, Outcome::Created);
        }
        return export_or_skip(conn, meta, &f, &kb_doc);
    }

    // The KB contributes nothing to a file that still declares items. Whatever the direction
    // machinery would conclude from the hashes, the disk is the good copy — that is this guard's
    // own reasoning — so import it where the mount can, and refuse to write where it cannot.
    //
    // Decided **here, above the direction dispatch**, because every arm below gets it wrong on
    // its own and each would need its own gate:
    //
    // - `(false, true)` exports, blanking the file; on an import-only mount it instead `Skipped`
    //   and left the KB permanently empty while reporting nothing at all.
    // - The three-way arm refused *before* `apply_doc`, so the refusal blocked the very import
    //   that heals — and since a refusal never advances the base, the next sync re-entered the
    //   same arm. Refused, refused, refused, while the message recommended an edit that could not
    //   work because the edit is what routes the file into that arm.
    //
    // One condition above all of them replaces a gate in each, which is the whole point: the
    // previous placement could only ever *refuse*, and refusing is the wrong answer on two of the
    // three mount modes.
    if let Some(reason) = wholesale_loss(&kb_doc, disk_doc) {
        // ...but an empty *document* is not proof of an empty *store*. `assemble_kb_doc` also
        // omits an item that is still bound and merely lost its primary placement, so a file whose
        // items are ALL in that state renders empty and looks identical to one whose items are
        // gone. A `document` mount is one item per file, so any single dropped placement gets
        // there. The two need opposite handling and the difference is only visible in the store:
        //
        // - still bound → `jkb undo` after a re-home. The items and their edits are alive; the
        //   remedy is one command (`jkb task place <uid> <ns> --home`) and importing over them
        //   would overwrite content, status and priority from disk, destroying un-exported work.
        //   Fall through, and `finish_export`'s `dropped_items` refuses with that remedy on every
        //   mount mode — as it did before this guard was hoisted above the dispatch.
        // - nothing bound → the items really are gone, and re-reading the file is the recovery.
        //
        // So the emptiness question is asked of the store as well as of the document.
        let still_bound = dropped_items(
            conn,
            &bare_uri,
            serializer.as_ref(),
            journal.as_ref(),
            Some(disk_doc),
        )?;
        if still_bound.is_empty() {
            if ctx.imports() {
                return finish_import(conn, meta, &f, disk_doc, Outcome::Imported);
            }
            flag_refused(conn, meta, &bare_uri, &ser_name, &reason, journal.as_ref())?;
            return Ok(Outcome::Refused);
        }
    }

    let (disk_changed, kb_changed, base_doc) = decide_direction(
        conn,
        serializer.as_ref(),
        journal.as_ref(),
        base_hash.as_deref(),
        Sides {
            disk: (disk_doc, &disk_hash),
            kb: (&kb_bytes, &kb_hash),
        },
    )?;
    let was_flagged = journal.as_ref().is_some_and(|j| j.status != "ok");

    match (disk_changed, kb_changed) {
        (false, false) => {
            // Nothing changed in substance. If either side's bytes are not what today's
            // serializer renders, settle that skew ONCE rather than re-deriving it on every
            // future sync: a stale base means the byte fast path can never hit again, so an
            // upgraded serializer would leave a permanent per-file cost behind it.
            let stale = base_hash.as_deref() != Some(kb_hash.as_str())
                || base_hash.as_deref() != Some(disk_hash.as_str());
            if stale && ctx.exports() {
                // The three renderings are equal here by construction (that is what
                // `(false, false)` means), so writing `kb_bytes` cannot change the document —
                // only its formatting.
                if kb_bytes != *disk_bytes {
                    write_file(path, &kb_bytes, Some(disk_bytes))?;
                }
                mark_ok(
                    conn,
                    meta,
                    &bare_uri,
                    &ser_name,
                    &kb_hash,
                    &kb_bytes,
                    journal.as_ref().and_then(|j| j.document.as_deref()),
                )?;
                return Ok(Outcome::Normalized);
            }
            if was_flagged {
                // A previously quarantined/conflicted file is now clean again.
                mark_ok(
                    conn,
                    meta,
                    &bare_uri,
                    &ser_name,
                    &kb_hash,
                    &kb_bytes,
                    journal.as_ref().and_then(|j| j.document.as_deref()),
                )?;
            }
            Ok(Outcome::UpToDate)
        }
        (true, false) => finish_import(conn, meta, &f, disk_doc, Outcome::Imported),
        (false, true) => finish_export(conn, meta, &f, &kb_doc, Some(disk_doc)),
        (true, true) => three_way_resolve(
            conn,
            meta,
            &f,
            disk_doc,
            &kb_doc,
            &base_doc.unwrap_or_default(),
        ),
    }
}

/// Nothing can be imported (the file is gone, or the mount is export-only). If the KB still
/// has items bound to this path, export to write it — that covers a previously-synced file
/// deleted on disk and a KB-created binding not yet written (`task add --sync`). Otherwise
/// there is nothing to reconcile.
#[allow(clippy::too_many_arguments)]
/// There is no importable disk document for this file — either it is absent, or the mount does
/// not import and has no journal row yet. Export the KB side if the mount exports, else skip.
///
/// `disk` is what the pass actually read: `None` when the file is absent, `Some(bytes)` when it
/// exists but was not imported. It has to be threaded through as the export's snapshot, and
/// getting that wrong is not cosmetic — this function was once named for the absent case alone
/// and hardcoded `None`, so the export-only first sight of an EXISTING file told `write_file` to
/// expect no file at all. The write seam then refused with "changed on disk while it was being
/// synced": a failure where an export was correct, and a false account of why.
fn export_or_skip(
    conn: &Connection,
    meta: &WriteMeta,
    f: &FileCtx<'_>,
    kb_doc: &SyncDoc,
) -> Result<Outcome> {
    let FileCtx {
        ctx,
        path,
        bare_uri,
        ser_name,
        serializer,
        journal,
        snapshot,
    } = *f;
    let _ = (path, bare_uri, ser_name, serializer, journal, snapshot);
    // The emptiness test below is what makes THIS route wholesale-safe, and it is load-bearing
    // rather than an optimisation. `wholesale_loss` sits above the direction dispatch in
    // `reconcile`, which is *after* both of this function's call sites, so it does not cover them
    // — and this arm is reached only when the file is absent or has never been synced, where an
    // empty KB side is nothing to write rather than something to refuse. Without the test an
    // absent file with an empty KB side would be *created* empty by the export below.
    if ctx.exports() && !kb_doc.items.is_empty() {
        // No disk document to compare against, so expectation falls back to the base.
        return finish_export(conn, meta, f, kb_doc, None);
    }
    Ok(Outcome::Skipped)
}

/// The two sides a direction decision compares against the base.
#[derive(Clone, Copy)]
struct Sides<'a> {
    /// The parsed disk document and its raw byte hash.
    disk: (&'a SyncDoc, &'a str),
    /// The rendered KB bytes and their hash.
    kb: (&'a [u8], &'a str),
}

/// Decide which side(s) changed, and return the parsed base alongside so a three-way merge
/// does not have to load it twice.
///
/// The comparison is between **documents, not bytes**. `kb` bytes are what *today's*
/// serializer renders; the stored base is bytes some earlier version of it wrote. Comparing
/// those directly conflates "the content changed" with "the renderer changed" — and the
/// second manufactures a phantom edit on every file in a mount at once, which has silently
/// exported over real work and, where the disk had also moved, produced a wall of conflicts
/// that were not conflicts. Re-rendering the base through today's serializer puts every side
/// in the same vocabulary, so only genuine differences survive.
///
/// Byte equality stays the fast path: identical bytes cannot be a changed document, so a
/// settled file still costs two hashes and no parse.
fn decide_direction(
    conn: &Connection,
    serializer: &dyn SyncSerializer,
    journal: Option<&sync_state::SyncState>,
    base_hash: Option<&str>,
    sides: Sides<'_>,
) -> Result<(bool, bool, Option<SyncDoc>)> {
    let (disk_doc, disk_hash) = sides.disk;
    let (kb_bytes, kb_hash) = sides.kb;
    let disk_bytes_differ = base_hash != Some(disk_hash);
    let kb_bytes_differ = base_hash != Some(kb_hash);
    if !disk_bytes_differ && !kb_bytes_differ {
        return Ok((false, false, None));
    }

    let base_doc = load_base_doc(conn, journal, serializer)?;
    match &base_doc {
        Some(doc) => {
            let canonical = serializer.render(doc)?;
            Ok((
                serializer.render(disk_doc)? != canonical,
                kb_bytes != canonical,
                base_doc,
            ))
        }
        // No base document to compare against: the blob is gone (a journal predating the
        // blob store, or one whose blob was pruned). The byte comparison is then all the
        // information there is.
        None => Ok((disk_bytes_differ, kb_bytes_differ, None)),
    }
}

/// Resolve the effective serializer for a file: a per-file `bindings.serializer`
/// override if any item bound to this file carries one, else the mount's (design D24).
fn resolve_serializer(
    conn: &Connection,
    ctx: &Ctx,
    bare_uri: &str,
) -> Result<(String, Box<dyn SyncSerializer>)> {
    let mut name = ctx.serializer.clone();
    let uris = binding::synced_uris_for_file(conn, bare_uri)?;
    if let Some(first) = uris.first() {
        if let Some(id) = binding::item_for_uri(conn, first)? {
            if let Some(over) = binding::get(conn, id)?.and_then(|b| b.serializer) {
                name = over;
            }
        }
    }
    let serializer = resolve(&name)?;
    Ok((name, serializer))
}

/// Import `doc` into the KB, write the canonical rendered bytes back if they differ
/// from what is on disk (persisting minted `^ids`), and record the base + journal.
#[allow(clippy::too_many_arguments)]
fn finish_import(
    conn: &Connection,
    meta: &WriteMeta,
    f: &FileCtx<'_>,
    doc: &SyncDoc,
    outcome: Outcome,
) -> Result<Outcome> {
    let FileCtx {
        ctx,
        path,
        bare_uri,
        ser_name,
        serializer,
        journal,
        snapshot,
    } = *f;
    let disk_bytes = snapshot.unwrap_or_default();
    let _ = journal;
    if !ctx.imports() {
        return Ok(Outcome::Skipped);
    }
    let resolved = apply_doc(conn, meta, ctx, path, bare_uri, doc)?;
    let rendered = serializer.render(doc)?;
    // Persist identity / normalization back to disk (the rendered form is authoritative).
    if rendered != disk_bytes {
        // `snapshot`, not `Some(disk_bytes)`: they differ exactly when the file was absent,
        // where the first says "expect nothing there" and the second "expect an empty file".
        // Constructing a snapshot at the call site is what this refactor exists to stop.
        write_file(path, &rendered, snapshot)?;
    }
    // An import is one of the two ways a file's structure legitimately changes, so this is
    // where the journal learns it (D45.4).
    settle(
        conn,
        meta,
        bare_uri,
        ser_name,
        &rendered,
        &resolved,
        Some(&document_json(doc)),
    )?;
    Ok(outcome)
}

/// Export the KB side to the file and record the base + journal.
///
/// Takes the **document**, not the bytes, and renders it here. Every export therefore writes the
/// render of the document the guard below just judged; there is no way for a caller to have
/// judged one thing and written another. That was not hypothetical — the guard's whole job is to
/// vet what reaches `write_file`, and it was reading live bindings while the bytes came from
/// somewhere else entirely (design D45.5).
#[allow(clippy::too_many_arguments)]
fn finish_export(
    conn: &Connection,
    meta: &WriteMeta,
    f: &FileCtx<'_>,
    kb_doc: &SyncDoc,
    expected: Option<&SyncDoc>,
) -> Result<Outcome> {
    let FileCtx {
        ctx,
        path,
        bare_uri,
        ser_name,
        serializer,
        journal,
        snapshot,
    } = *f;
    if !ctx.exports() {
        return Ok(Outcome::Skipped);
    }

    // The guard lives HERE, not at the call site, because two other callers reach this function
    // — `export_or_skip` and `three_way_resolve`'s `kb_wins` — and a guard at the `(false, true)`
    // arm would let both past (design D45.5).
    if let Some(reason) = export_blocker(conn, f, expected)? {
        flag_refused(conn, meta, bare_uri, ser_name, &reason, journal)?;
        return Ok(Outcome::Refused);
    }

    let kb_bytes = &serializer.render(kb_doc)?;
    write_file(path, kb_bytes, snapshot)?;
    let resolved = current_bindings(conn, bare_uri)?;
    // An export CARRIES the structure forward rather than authoring it: `apply_doc` is the only
    // writer of a file's structure, and an export does not call it. That is the property the
    // whole model buys (D45.4).
    let document = journal.and_then(|j| j.document.clone());
    settle(
        conn,
        meta,
        bare_uri,
        ser_name,
        kb_bytes,
        &resolved,
        document.as_deref(),
    )?;
    Ok(Outcome::Exported)
}

/// Why this file must not be written from the KB side, or `None` if it may be.
///
/// Two conditions, both of which pass-10 review found the first version missing:
///
/// 1. **Items would vanish.** See [`dropped_items`].
/// 2. **The structure is unknown while the file has content.** [`dropped_items`] alone returns
///    "nothing dropped" when there is no base document, which made the guard *vacuous* on the
///    `export_or_skip` path: a file that exists on disk but has never been imported has no journal
///    document, so `assemble_kb_doc` yields no layout and no sections, and the export wrote a
///    headerless, prose-free dump over it. "No base" means unconstrained only when there is also
///    nothing on disk to lose.
///
/// The third — *the KB contributes nothing at all* — is [`wholesale_loss`], and it is deliberately
/// **not** here. Here it could only refuse, and on a mount that can import, refusing is the wrong
/// answer: it blocks the import that heals. It runs in [`reconcile`] above the direction dispatch
/// instead. It covers the arms below that dispatch, **not** [`export_or_skip`], which runs earlier
/// and carries its own emptiness test for that reason.
///
/// Both conditions here read the store, and the store is what the incidents this guards against
/// damage. That is the limitation `wholesale_loss` exists to cover, and the reason it judges
/// documents rather than bindings.
fn export_blocker(
    conn: &Connection,
    f: &FileCtx<'_>,
    disk_doc: Option<&SyncDoc>,
) -> Result<Option<String>> {
    let FileCtx {
        ctx,
        path,
        bare_uri,
        ser_name,
        serializer,
        journal,
        snapshot,
    } = *f;
    let _ = (ser_name, snapshot);
    let dropped = dropped_items(conn, bare_uri, serializer, journal, disk_doc)?;
    if !dropped.is_empty() {
        // Name the items by their real uid, LOOKED UP rather than derived. A sync-created item's
        // uid happens to be its binding uri, but one created by `jkb task add --sync` keeps its
        // `task:<slug>` uid and is merely *bound* to the file — so deriving the uri produced a
        // `jkb task place <uid>` line that fails as typed for exactly those tasks, and in the
        // file-is-gone branch that was the only remedy on offer.
        let uids = uids_of(conn, bare_uri, &dropped)?;
        // The remedy depends on whether there is a file to edit. Naming the on-disk fix for a
        // file that is gone is advice nobody can follow.
        let remedy = if path.exists() {
            "Restore their primary placements (`jkb task place <uid> <ns> --home`), or delete \
             their lines from the file if the items are meant to go."
        } else {
            "This file is gone from disk, so restore their primary placements \
             (`jkb task place <uid> <ns> --home`) — there is no line to delete."
        };
        return Ok(Some(format!(
            "{} item(s) bound to this file are missing from the assembled document ({}). \
             Exporting would delete their lines, so nothing was written. Their placements are \
             gone — `jkb undo` after a re-home does this. {remedy}",
            uids.len(),
            uids.join(", ")
        )));
    }
    // Only for a mount that can import. On an export-only mount the file is a projection of the
    // KB and there is no import to recover through, so refusing would wedge it on every run
    // while telling the user to perform an operation the mount's mode forbids.
    let structure_known = journal.and_then(|j| j.document.as_deref()).is_some();
    let disk_has_content = std::fs::read(path).is_ok_and(|b| !b.trim_ascii().is_empty());
    if ctx.imports() && !structure_known && disk_has_content {
        return Ok(Some(
            "this file has content on disk but no recorded structure, so exporting would write \
             a headerless, prose-free render over it. Nothing was written. Sync it once from \
             disk (an import) before exporting."
                .to_owned(),
        ));
    }
    Ok(None)
}

/// The seam guard: the export would delete **every** item line the file declares, because the
/// document assembled from the KB has none at all.
///
/// D45 states the single mechanism behind every sync data-loss incident this project has had — *an
/// unverified KB render reached `write_file`*. Each incident was then fixed at its route: pass 21
/// at `finish_export`'s `(false, true)` arm, pass 22 at `three_way_resolve`'s `!ctx.imports()` arm.
/// Both fixes were correct and neither was the last one, because the routes are not the cause.
///
/// This is the same question asked of the **two documents** — the KB's and the one just parsed off
/// disk — rather than of the store. That matters because the store is what the incidents damage:
/// `jkb undo` of a sync deletes the items *and* their bindings, so [`dropped_items`], which walks
/// bindings, correctly reports that nothing was dropped. There is nothing left to drop. One
/// condition covers undo, `jkb item rm`, a half-applied migration, an emptied binding table, and
/// whatever produces the same shape next.
///
/// **Detecting it is not the same as refusing it.** [`reconcile`] calls this above the direction
/// dispatch and then decides by mount mode: a mount that can import re-imports the file, because
/// the disk being the good copy is this function's own premise; only an export-only mount, which
/// has no way to read the file back, refuses. Sited inside [`export_blocker`] the answer was
/// always "refuse", which protected the file and left the KB permanently empty.
///
/// **Not** a general "fewer items than the disk" rule. On an export-only mount the file is a
/// projection and hand-added lines are legitimately removed; on any mount, deleting the last task
/// in a file is an ordinary edit. The distinguishable case is *total* loss: a file that declares
/// items against a KB side that declares none is not an edit anyone made.
///
/// **Pure, and reads nothing.** [`dropped_items`] falls back to the stored base when there is no
/// disk document; this deliberately does not. A guard whose thesis is *do not trust the store*
/// should not need the store to decide — and its one caller has a parsed disk document by
/// construction, because a file that is absent or unparsed never reaches the dispatch this sits
/// above. It took an `Option` while it lived at the export seam, where one caller had no disk
/// document; after the move that argument could only ever be `Some`, so it is gone rather than
/// left as an input no code path can produce.
fn wholesale_loss(kb_doc: &SyncDoc, disk_doc: &SyncDoc) -> Option<String> {
    if !kb_doc.items.is_empty() {
        return None;
    }
    let declared = disk_doc.items.len();
    if declared == 0 {
        return None;
    }
    // Written for the one caller that surfaces it: an export-only mount, which cannot heal
    // itself. A mount that imports never sees this text, because it re-imports instead.
    Some(format!(
        "this file declares {declared} item(s) and the KB side of it has none, so exporting would \
         delete every one of their lines. Nothing was written, and this mount cannot import, so \
         it cannot recover on its own. The file on disk is still the good copy: re-read it with \
         `jkb mount create <ns> <dir> --mode bidirectional` followed by `jkb sync <ns>`, or if \
         the items really were meant to go, delete the file."
    ))
}

/// The real `items.uid` for each `local_id`, falling back to the binding uri if the row is gone.
fn uids_of(conn: &Connection, bare_uri: &str, locals: &[String]) -> Result<Vec<String>> {
    let bound = existing_by_local(conn, bare_uri)?;
    let mut out = Vec::with_capacity(locals.len());
    for local in locals {
        let uid = match bound.get(local) {
            Some(id) => item::get(conn, *id)?.map(|m| m.uid),
            None => None,
        };
        out.push(uid.unwrap_or_else(|| item_uri(bare_uri, local)));
    }
    Ok(out)
}

/// Items the base declared, whose binding still exists, that `assemble_kb_doc` would silently
/// drop — the `local_id`s whose lines an export is about to delete.
///
/// `assemble_kb_doc` skips any bound item with no row **or no primary placement**
/// (engine.rs, `build_sync_item`'s guard), and the export arm then writes that render over the
/// file. It is reachable through a documented verb: `placement::set_primary` logs the old
/// primary's removal as `op="delete"` on `placements`, which has no inverse, while the
/// replacement is an invertible `insert` — so `jkb undo` after any re-home leaves the item with
/// no primary placement at all.
///
/// Deliberately NOT a constraint on KB-owned state: an item deleted in the KB loses its binding
/// and is legitimately absent.
fn dropped_items(
    conn: &Connection,
    bare_uri: &str,
    serializer: &dyn SyncSerializer,
    journal: Option<&sync_state::SyncState>,
    disk_doc: Option<&SyncDoc>,
) -> Result<Vec<String>> {
    // What the file is EXPECTED to contain: the disk document when we have one, and only the
    // base when the file is gone.
    //
    // Judging from the base alone made a refusal unclearable, which is the wedge this was
    // written to avoid: the message says "delete the line", the user deletes it, and every later
    // reconcile re-read the *base* — where the item still is — and refused again, so the edit
    // was never imported. An item absent from disk is one the user removed; `apply_doc` detaches
    // it, which is correct and is not a drop.
    let owned;
    let expected = match disk_doc {
        Some(doc) => doc,
        None => match load_base_doc(conn, journal, serializer)? {
            Some(doc) => {
                owned = doc;
                &owned
            }
            // Never synced and no disk document: nothing was ever declared, nothing can be lost.
            None => return Ok(Vec::new()),
        },
    };
    let bound = existing_by_local(conn, bare_uri)?;
    let declared: Vec<(String, ItemId)> = expected
        .items
        .iter()
        .filter_map(|it| bound.get(&it.local_id).map(|id| (it.local_id.clone(), *id)))
        .collect();
    if declared.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<ItemId> = declared.iter().map(|(_, id)| *id).collect();
    let rows = item_rows_for(conn, &ids)?;
    let placements = primary_placements_for(conn, &ids)?;
    Ok(declared
        .into_iter()
        .filter(|(_, id)| !rows.contains_key(id) || !placements.contains_key(id))
        .map(|(local, _)| local)
        .collect())
}

/// Fill in a journal row's `document` if it predates D45, returning the row as it now stands.
///
/// Sources, in order, both per-file: the base blob (the exact bytes this file last synced), then
/// the file on disk. Deliberately **not** the namespace metadata — the post-D39 file namespace
/// does not exist on a legacy store, and the pre-D39 directory namespace is shared, so reading it
/// is the `openspec` collapse restated as a migration.
///
/// A row with no base and no readable file is left alone: it has never synced, so there is no
/// structure to recover and the export path treats it as unconstrained.
fn populate_document(
    conn: &Connection,
    meta: &WriteMeta,
    bare_uri: &str,
    serializer: &dyn SyncSerializer,
    path: &Path,
    journal: Option<sync_state::SyncState>,
) -> Result<Option<sync_state::SyncState>> {
    let Some(row) = journal else {
        return Ok(None);
    };
    if row.document.is_some() {
        return Ok(Some(row));
    }
    let recovered = match load_base_doc(conn, Some(&row), serializer) {
        Ok(Some(doc)) => Some(doc),
        // A base that will not parse is not a reason to abort the whole run; fall through to the
        // file, and if that fails too the export guard refuses rather than writing.
        Ok(None) | Err(_) => match std::fs::read(path) {
            Ok(bytes) => serializer.parse(&bytes).ok(),
            Err(_) => None,
        },
    };
    let Some(doc) = recovered else {
        return Ok(Some(row));
    };
    let document = document_json(&doc);
    sync_state::upsert(
        conn,
        meta,
        &sync_state::SyncStateWrite {
            uri: bare_uri,
            serializer: &row.serializer,
            status: &row.status,
            last_synced_hash: row.last_synced_hash.as_deref(),
            base_blob_hash: row.base_blob_hash.as_deref(),
            parse_error: row.parse_error.as_deref(),
            quarantine_blob_hash: row.quarantine_blob_hash.as_deref(),
            document: Some(&document),
        },
    )?;
    Ok(Some(sync_state::SyncState {
        document: Some(document),
        ..row
    }))
}

/// Journal a refusal: nothing was written, and `jkb doctor` must be able to say why.
fn flag_refused(
    conn: &Connection,
    meta: &WriteMeta,
    bare_uri: &str,
    ser_name: &str,
    reason: &str,
    journal: Option<&sync_state::SyncState>,
) -> Result<()> {
    flag_needs_attention(conn, meta, bare_uri, ser_name, reason, journal)
}

/// Mark a file `needs_attention` with `reason`, carrying every other journal field forward.
///
/// The ONE spelling of this write. It restates the full [`sync_state::SyncStateWrite`] field
/// list, and that list has already been got wrong once — an earlier version of the undo snapshot
/// dropped `parse_error` and `quarantine_blob_hash` — so a second hand-written copy is a second
/// chance to drop a field.
fn flag_needs_attention(
    conn: &Connection,
    meta: &WriteMeta,
    bare_uri: &str,
    ser_name: &str,
    reason: &str,
    journal: Option<&sync_state::SyncState>,
) -> Result<()> {
    sync_state::upsert(
        conn,
        meta,
        &sync_state::SyncStateWrite {
            uri: bare_uri,
            serializer: ser_name,
            status: journal_status(conn, bare_uri, FileEvent::WriteBlocked)?,
            last_synced_hash: journal.and_then(|j| j.last_synced_hash.as_deref()),
            base_blob_hash: journal.and_then(|j| j.base_blob_hash.as_deref()),
            parse_error: Some(reason),
            quarantine_blob_hash: journal.and_then(|j| j.quarantine_blob_hash.as_deref()),
            document: journal.and_then(|j| j.document.as_deref()),
        },
    )?;
    Ok(())
}

/// Both sides changed: attempt a three-way merge of disjoint edits, else resolve by the
/// mount's `conflict_policy`.
#[allow(clippy::too_many_arguments)]
fn three_way_resolve(
    conn: &Connection,
    meta: &WriteMeta,
    f: &FileCtx<'_>,
    disk_doc: &SyncDoc,
    kb_doc: &SyncDoc,
    base_doc: &SyncDoc,
) -> Result<Outcome> {
    let FileCtx {
        ctx,
        path,
        bare_uri,
        ser_name,
        serializer,
        journal,
        snapshot,
    } = *f;
    match three_way(base_doc, disk_doc, kb_doc) {
        ThreeWay::Merged(merged) => {
            // An export-only mount never takes item edits from disk, so there is nothing to
            // merge INTO the KB — the KB is authoritative and the file is an output. Resolve it
            // the way a KB-only change resolves: export over the file. Without this the arm fell
            // through to `apply_doc` and cancelled the tasks whose lines the disk edit removed.
            if !ctx.imports() {
                return finish_export(conn, meta, f, kb_doc, Some(disk_doc));
            }
            // The merge writes the file AND cancels items the merged doc lacks, so it reaches
            // the same harm `finish_export` guards. It was left ungated on the reasoning that a
            // merge incorporates disk-side *structure* by design — true, and irrelevant: this
            // check is about **items**, which is exactly the distinction D45.5 draws.
            //
            // It mattered because the refusal's own advice is "edit the file", and that edit is
            // what routes a refused file into this arm. Following the tool's instructions
            // deleted the line the refusal had just protected.
            if let Some(reason) = export_blocker(conn, f, Some(disk_doc))? {
                flag_refused(conn, meta, bare_uri, ser_name, &reason, journal)?;
                return Ok(Outcome::Refused);
            }
            let resolved = apply_doc(conn, meta, ctx, path, bare_uri, &merged)?;
            let rendered = serializer.render(&merged)?;
            if ctx.exports() {
                write_file(path, &rendered, snapshot)?;
            }
            settle(
                conn,
                meta,
                bare_uri,
                ser_name,
                &rendered,
                &resolved,
                Some(&document_json(&merged)),
            )?;
            Ok(Outcome::Merged)
        }
        ThreeWay::Conflict => match ctx.conflict_policy.as_str() {
            "disk_wins" => finish_import(conn, meta, f, disk_doc, Outcome::ResolvedFromDisk),
            "kb_wins" => {
                if finish_export(conn, meta, f, kb_doc, Some(disk_doc))? == Outcome::Refused {
                    return Ok(Outcome::Refused);
                }
                Ok(Outcome::ResolvedFromKb)
            }
            _ => {
                // manual: overwrite neither side; flag the file so `doctor` can surface it.
                let base = journal.and_then(|j| j.base_blob_hash.clone());
                let last = journal.and_then(|j| j.last_synced_hash.clone());
                sync_state::upsert(
                    conn,
                    meta,
                    &sync_state::SyncStateWrite {
                        uri: bare_uri,
                        serializer: ser_name,
                        status: journal_status(conn, bare_uri, FileEvent::Conflicted)?,
                        last_synced_hash: last.as_deref(),
                        base_blob_hash: base.as_deref(),
                        parse_error: None,
                        quarantine_blob_hash: None,
                        // manual overwrites neither side, so neither side's structure is
                        // adopted; the journal keeps what it had.
                        document: journal.and_then(|j| j.document.as_deref()),
                    },
                )?;
                Ok(Outcome::Conflict)
            }
        },
    }
}

/// Stash the failing bytes and flag the journal `needs_attention`, keeping the KB items
/// and the existing base untouched (design D25 quarantine-don't-destroy).
fn quarantine(
    conn: &Connection,
    meta: &WriteMeta,
    bare_uri: &str,
    ser_name: &str,
    bytes: &[u8],
    err: &Error,
    journal: Option<&sync_state::SyncState>,
) -> Result<Outcome> {
    let qhash = blob::hash_bytes(bytes);
    blob::store(conn, &qhash, bytes, None)?;
    let base = journal.and_then(|j| j.base_blob_hash.clone());
    let last = journal.and_then(|j| j.last_synced_hash.clone());
    sync_state::upsert(
        conn,
        meta,
        &sync_state::SyncStateWrite {
            uri: bare_uri,
            serializer: ser_name,
            status: journal_status(conn, bare_uri, FileEvent::ParseFailed)?,
            last_synced_hash: last.as_deref(),
            base_blob_hash: base.as_deref(),
            parse_error: Some(&err.to_string()),
            quarantine_blob_hash: Some(&qhash),
            // A quarantine keeps the last-good structure exactly as it keeps the last-good
            // hashes: the file failed to parse, so nothing was learned about its shape.
            document: journal.and_then(|j| j.document.as_deref()),
        },
    )?;
    Ok(Outcome::Quarantined)
}

/// Record a clean sync: store the base blob, stamp each item's binding (back-compat),
/// and upsert the journal `ok`.
fn settle(
    conn: &Connection,
    meta: &WriteMeta,
    bare_uri: &str,
    ser_name: &str,
    base_bytes: &[u8],
    items: &[ItemId],
    document: Option<&str>,
) -> Result<()> {
    let base_hash = blob::hash_bytes(base_bytes);
    blob::store(conn, &base_hash, base_bytes, None)?;
    for id in items {
        binding::mark_synced(conn, meta, *id, &base_hash)?;
    }
    sync_state::upsert(
        conn,
        meta,
        &sync_state::SyncStateWrite {
            uri: bare_uri,
            serializer: ser_name,
            status: "ok",
            last_synced_hash: Some(&base_hash),
            base_blob_hash: Some(&base_hash),
            parse_error: None,
            quarantine_blob_hash: None,
            document,
        },
    )?;
    Ok(())
}

/// Re-affirm an `ok` journal for a file whose quarantine/conflict has cleared with no
/// remaining drift.
fn mark_ok(
    conn: &Connection,
    meta: &WriteMeta,
    bare_uri: &str,
    ser_name: &str,
    hash: &str,
    bytes: &[u8],
    document: Option<&str>,
) -> Result<()> {
    blob::store(conn, hash, bytes, None)?;
    sync_state::upsert(
        conn,
        meta,
        &sync_state::SyncStateWrite {
            uri: bare_uri,
            serializer: ser_name,
            status: journal_status(conn, bare_uri, FileEvent::Unchanged)?,
            last_synced_hash: Some(hash),
            base_blob_hash: Some(hash),
            parse_error: None,
            quarantine_blob_hash: None,
            document,
        },
    )?;
    Ok(())
}

/// The journal status this conclusion settles on, asked of `crate::lifecycle`.
///
/// The **one** place `sync_state.status` gets a value. It had four hand-written spellings, and
/// two of them were the same string for two different states — a quarantine and a blocked write
/// are both `needs_attention`, and `Outcome::Refused`'s own doc warns that the reason "must be
/// read rather than assumed". The state set says which; this maps it back to the column.
///
/// # Errors
/// Errors if the journal cannot be read, or if the conclusion is one the machine does not
/// declare from this file's current state.
fn journal_status(conn: &Connection, bare_uri: &str, event: FileEvent) -> Result<&'static str> {
    let row = sync_state::get(conn, bare_uri)?;
    let from = FileState::from_journal(
        row.as_ref().map(|r| r.status.as_str()),
        row.as_ref()
            .is_some_and(|r| r.quarantine_blob_hash.is_some()),
    );
    crate::lifecycle::status_for(from, event)
        .map_err(|e| Error::Types(TypeError::Validation(format!("{bare_uri}: {e}"))))
}

/// Everything about the file being reconciled that does not change during the pass.
///
/// These six values were threaded as positional arguments through five helpers — ten to
/// thirteen parameters each, six of them the same six every time, four of those `&str` or
/// `Option<&...>` and so interchangeable without a type error. `snapshot` is the one that
/// matters: it is the bytes this pass read from disk, which the write seam re-checks before
/// overwriting, and passing the wrong one means writing over an edit that landed mid-pass.
/// It had already been passed wrongly once — `export_or_skip` hardcoded `None` on a path
/// reachable with the file present, so an export-only mount refused its own first write and
/// blamed a disk change that never happened.
///
/// Bundling them makes that specific mistake unspellable: there is one snapshot, it belongs to
/// the file, and no call site chooses it. `Copy`, so passing it costs a pointer and nothing
/// reads as a move.
#[derive(Clone, Copy)]
struct FileCtx<'a> {
    ctx: &'a Ctx,
    path: &'a Path,
    bare_uri: &'a str,
    ser_name: &'a str,
    serializer: &'a dyn SyncSerializer,
    journal: Option<&'a sync_state::SyncState>,
    /// What this pass read from disk: `None` when the file is absent.
    snapshot: Option<&'a [u8]>,
}

// ---------------------------------------------------------------------------
// Multi-item apply (KB write side)
// ---------------------------------------------------------------------------

/// Apply `doc` to the KB (create/update items, place them under their section
/// namespaces, reconcile tags and edges, cancel removed tasks). Two passes so edges
/// resolve after every item exists. Returns the item ids the doc now maps to.
///
/// **This is the only path by which the disk side reaches the KB**, so the "does this mount
/// import?" rule lives here rather than at each caller. `finish_import` had it and the
/// three-way `Merged` arm did not, which let a hand edit to a file on an export-only mount
/// cancel and detach the KB tasks whose lines it removed — silently, and in flat contradiction
/// of the guarantee that mount makes. Two callers, one of them wrong, is the shape this
/// codebase has repeatedly paid for; a caller that reaches here without checking is now a loud
/// failure rather than a quiet deletion.
fn apply_doc(
    conn: &Connection,
    meta: &WriteMeta,
    ctx: &Ctx,
    path: &Path,
    bare_uri: &str,
    doc: &SyncDoc,
) -> Result<Vec<ItemId>> {
    if !ctx.imports() {
        return Err(Error::Types(TypeError::Validation(format!(
            "refusing to write the disk side of {} into the KB: this mount does not import. \
             This is a bug in the caller — it should have resolved the file without importing.",
            path.display()
        ))));
    }
    let file_ns_path = namespace_for(ctx, path);
    let file_ns = ns::ensure(conn, &file_ns_path)?;

    // Sections → namespaces, carrying only their header text. Their ORDER (and the file's
    // prose) lives in the layout stored on the file namespace below — one sequence, so
    // nothing can drift against anything else.
    let mut section_ns: HashMap<String, NamespaceId> = HashMap::new();
    for s in &doc.sections {
        let full = format!("{file_ns_path}/{}", s.path);
        let id = ns::ensure(conn, &full)?;
        // `sync_section` marks a namespace as this file's, so `retire_undeclared_sections` can
        // find it later. `header_line` is kept **only** as a human label in the tree — since D45
        // the authority for both the header text and the block order is the journal row, and
        // nothing reads these back to decide what the document looks like.
        //
        // MERGED, not replaced: `ns::set_metadata` writes the whole object, so assigning a fresh
        // one silently dropped every other key the namespace carried — `type` among them, which
        // is the namespace-contract mechanism (D33).
        let mut md = ns::get_metadata(conn, id)?
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| json!({}));
        if let Some(map) = md.as_object_mut() {
            map.insert("header_line".to_owned(), json!(s.header_line));
            map.insert("sync_section".to_owned(), json!(true));
        }
        ns::set_metadata(conn, meta, id, &md)?;
        section_ns.insert(s.path.clone(), id);
    }
    // A section the file no longer declares must stop being a section. Its namespace can
    // legitimately survive — it may still hold cancelled tasks, which are deliberate history
    // — but leaving `header_line` on it makes `assemble_kb_doc` re-emit a `##` header the
    // file does not have, so the KB render disagrees with the disk forever and every later
    // disk edit is resolved as a conflict (see `memory/sync-export-wins`).
    retire_undeclared_sections(conn, meta, &file_ns_path, doc)?;

    // Existing items bound to this file, by local_id.
    let existing = existing_by_local(conn, bare_uri)?;

    // Pass 1 — items only (no edges yet).
    let mut resolved: HashMap<String, ItemId> = HashMap::new();
    for it in &doc.items {
        let uri = item_uri(bare_uri, &it.local_id);
        let home = it
            .section
            .as_ref()
            .and_then(|s| section_ns.get(s).copied())
            .unwrap_or(file_ns);
        let id = match existing.get(&it.local_id) {
            Some(&id) => {
                update_item(conn, meta, id, it, home)?;
                id
            }
            None => create_item(conn, meta, ctx, it, &uri, home)?,
        };
        for m in &it.mirrors {
            let mns = ns::ensure(conn, m)?;
            placement::place(conn, meta, id, mns, PlacementRole::Reference, 0)?;
        }
        resolved.insert(it.local_id.clone(), id);
    }

    // Items that vanished from the file are detached (rebound to `managed:`) so they are
    // not re-exported, and tasks are additionally marked `cancelled` — non-destructive:
    // the item, its edges, and its history survive (design D25).
    for (lid, &id) in &existing {
        if resolved.contains_key(lid) {
            continue;
        }
        if item_kind(conn, id)?.as_deref() == Some("task") {
            // Attributed to the file, because the file is what said so: the line is gone.
            task::set_status_from_file(conn, meta, id, TaskStatus::Cancelled)?;
        }
        binding::set(conn, meta, id, "managed:", None, None)?;
    }

    // Pass 2 — reconcile edges now that every local_id resolves to an item.
    reconcile_edges(conn, meta, doc, &resolved)?;

    Ok(resolved.into_values().collect())
}

/// A file's document **structure** — block order and section headers — as the JSON stored on its
/// journal row (design D45.2).
///
/// This is the whole of what used to live in `namespaces.metadata`, moved somewhere keyed
/// one-per-file. Items are deliberately absent: they are KB-owned and come from bindings.
fn document_json(doc: &SyncDoc) -> String {
    json!({
        "layout": layout_json(doc),
        "sections": doc
            .sections
            .iter()
            .map(|s| json!({ "path": s.path, "header_line": s.header_line }))
            .collect::<Vec<_>>(),
    })
    .to_string()
}

/// Read a stored structure back. A row with no document, or unreadable JSON, yields an empty
/// structure — which the export guard (D45.5) turns into a refusal rather than a stripped file.
fn read_document(stored: Option<&str>) -> (Vec<SyncBlock>, Vec<SyncSection>) {
    let Some(value) = stored.and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok()) else {
        return (Vec::new(), Vec::new());
    };
    let layout = value.get("layout").map_or_else(Vec::new, read_layout_value);
    let sections = value
        .get("sections")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let text = |k: &str| s.get(k).and_then(serde_json::Value::as_str);
                    Some(SyncSection {
                        path: text("path")?.to_owned(),
                        header_line: text("header_line")?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (layout, sections)
}

/// Serialize a document's layout for the structure stored on the file's journal row
/// ([`document_json`], design D45.2).
fn layout_json(doc: &SyncDoc) -> serde_json::Value {
    let blocks: Vec<serde_json::Value> = doc
        .layout
        .iter()
        .map(|b| match b {
            SyncBlock::Section(path) => json!({ "section": path }),
            SyncBlock::Item(id) => json!({ "item": id }),
            SyncBlock::Prose(text) => json!({ "prose": text }),
        })
        .collect();
    serde_json::Value::Array(blocks)
}

/// Decode a layout array (the shape [`layout_json`] produces).
fn read_layout_value(value: &serde_json::Value) -> Vec<SyncBlock> {
    let Some(blocks) = value.as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|b| {
            let text = |k: &str| {
                b.get(k)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            };
            if let Some(p) = text("section") {
                Some(SyncBlock::Section(p))
            } else if let Some(i) = text("item") {
                Some(SyncBlock::Item(i))
            } else {
                text("prose").map(SyncBlock::Prose)
            }
        })
        .collect()
}

/// Clear the section metadata of every namespace under the file that the document no longer
/// declares, so it stops being rendered as a `##` header. The namespace and everything in it
/// are left alone — this only retires its *section* role.
fn retire_undeclared_sections(
    conn: &Connection,
    meta: &WriteMeta,
    file_ns_path: &str,
    doc: &SyncDoc,
) -> Result<()> {
    let declared: std::collections::HashSet<&str> =
        doc.sections.iter().map(|s| s.path.as_str()).collect();
    for (ns_id, ns_path) in ns::subtree(conn, file_ns_path)? {
        if ns_path == file_ns_path {
            continue;
        }
        if declared.contains(relative(file_ns_path, &ns_path).as_str()) {
            continue;
        }
        let Some(mut metadata) = ns::get_metadata(conn, ns_id)? else {
            continue;
        };
        let Some(map) = metadata.as_object_mut() else {
            continue;
        };
        // Keyed on `sync_section`, NOT on `header_line`. Since D45 the header text is a label
        // rather than the authority, and a `header_line` key alone would be gone from every
        // namespace the moment the structure moved — so this function would `continue` on every
        // iteration and retire nothing, forever, and invisibly, because nothing renders from
        // namespaces any more for a render test to catch.
        if map.remove("sync_section").is_none() {
            continue; // not a section to begin with
        }
        map.remove("position");
        map.remove("header_line");
        map.remove("prose");
        ns::set_metadata(conn, meta, ns_id, &metadata)?;
    }
    Ok(())
}

/// Create a new item for a [`SyncItem`], placing it under its home namespace and
/// binding it to `uri`. `content_hash` is left `None` so two identical-title tasks do
/// not dedup-collapse into one item (their `local_id`/uri is the real identity).
fn create_item(
    conn: &Connection,
    meta: &WriteMeta,
    ctx: &Ctx,
    it: &SyncItem,
    uri: &str,
    home: NamespaceId,
) -> Result<ItemId> {
    // A line deleted from the file is detached, not deleted (design D25) — and it keeps its
    // file-derived uid. Re-adding that same line mints the same uid, so a plain insert hits
    // the UNIQUE constraint and the whole sync fails. Re-attaching instead is both the fix
    // and the better semantics: deleting a line and putting it back restores the same item,
    // with its edges, tags and history intact, rather than a stranger wearing its name.
    let existing = item::id_for_uid(conn, uri)?;
    let id = match existing {
        Some(id) => id,
        None => item::upsert(
            conn,
            meta,
            &NewItem {
                uid: uri.to_owned(),
                kind: it.kind.clone(),
                content: Some(it.content.clone()),
                content_hash: None,
                mime: None,
            },
        )?,
    };
    // The binding is written **before** the columns, because it is what makes this item
    // file-backed and the status write below is attributed to the file
    // (`task::set_status_from_file`), which declines to act for a task the file does not back.
    // Written afterwards, a brand-new task's very first status came from an authority the store
    // could not yet see.
    binding::set(
        conn,
        meta,
        id,
        uri,
        Some(sync_mode_of(&ctx.sync_mode)),
        None,
    )?;
    if existing.is_some() {
        update_item(conn, meta, id, it, home)?;
    } else {
        set_task_columns(conn, meta, id, it)?;
        placement::place(conn, meta, id, home, PlacementRole::Primary, it.position)?;
    }
    // The same call the update path makes, so a file's tags reach the store by exactly one
    // route whether the line is new or re-attached. It was two — a bare `apply` loop here and a
    // reconcile there — and a rule that has to hold at both is a rule one of them will
    // eventually not have.
    tag::reconcile_tags(conn, meta, id, &it.tags)?;
    Ok(id)
}

/// Update an existing item to match a [`SyncItem`]: content, task columns, tags, and
/// primary placement — each only when it actually differs, to keep the changelog quiet.
fn update_item(
    conn: &Connection,
    meta: &WriteMeta,
    id: ItemId,
    it: &SyncItem,
    home: NamespaceId,
) -> Result<()> {
    if item::get_content(conn, id)?.as_deref() != Some(it.content.as_str()) {
        item::set_content(conn, meta, id, &it.content, None)?;
    }
    set_task_columns(conn, meta, id, it)?;
    tag::reconcile_tags(conn, meta, id, &it.tags)?;
    task::set_primary_home(conn, meta, id, home, it.position)?;
    Ok(())
}

/// Set the `status`/`priority`/`due` columns for a task item, each only when changed.
fn set_task_columns(conn: &Connection, meta: &WriteMeta, id: ItemId, it: &SyncItem) -> Result<()> {
    if it.kind != "task" {
        return Ok(());
    }
    let (status, priority, due): (Option<String>, Option<i64>, Option<String>) = conn
        .prepare_cached("SELECT status, priority, due FROM items WHERE id = ?1")?
        .query_row([id.get()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    if it.status != status {
        if let Some(s) = &it.status {
            // The file is the authority here, not an operator — so this is a reconciliation
            // with a guard on it (the file may only speak for a task it backs), and
            // `jkb task why` records it as the file's doing.
            task::set_status_str_from_file(conn, meta, id, s)?;
        }
    }
    if it.priority != priority {
        task::set_priority(conn, meta, id, it.priority)?;
    }
    if it.due != due {
        task::set_due(conn, meta, id, it.due.as_deref())?;
    }
    Ok(())
}

/// Reconcile `parent_of` and `depends_on` edges to exactly what `doc` declares.
fn reconcile_edges(
    conn: &Connection,
    meta: &WriteMeta,
    doc: &SyncDoc,
    resolved: &HashMap<String, ItemId>,
) -> Result<()> {
    let srcs: Vec<ItemId> = resolved.values().copied().collect();
    for kind in [EdgeType::ParentOf, EdgeType::DependsOn] {
        // desired src -> set(dst) from the doc, mapped to item ids.
        let mut desired: HashMap<ItemId, HashSet<ItemId>> = HashMap::new();
        for e in doc.edges.iter().filter(|e| e.edge_type == kind) {
            if let (Some(&s), Some(&d)) = (resolved.get(&e.src), resolved.get(&e.dst)) {
                if s != d {
                    desired.entry(s).or_default().insert(d);
                }
            }
        }
        // Current edges for all sources in one query, indexed by source.
        let mut current = edge::edges_from_many(conn, &srcs, kind)?;
        for &src in &srcs {
            let want = desired.get(&src).cloned().unwrap_or_default();
            let have: HashSet<ItemId> = current
                .remove(&src)
                .unwrap_or_default()
                .into_iter()
                .collect();
            for &dst in want.difference(&have) {
                edge::link(conn, meta, src, dst, kind, None)?;
            }
            for &dst in have.difference(&want) {
                edge::unlink(conn, meta, src, dst, kind)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// KB read side — assemble a SyncDoc from the items bound to a file
// ---------------------------------------------------------------------------

/// Build a [`SyncDoc`] from the current KB state of a file: its section namespaces and
/// the items bound under it. The inverse of [`apply_doc`], so `render(assemble_kb_doc)`
/// reproduces the last-synced base when the KB is unchanged.
fn assemble_kb_doc(
    conn: &Connection,
    ctx: &Ctx,
    path: &Path,
    bare_uri: &str,
    journal: Option<&sync_state::SyncState>,
) -> Result<SyncDoc> {
    let file_ns_path = namespace_for(ctx, path);
    let mut doc = SyncDoc::default();

    // **Structure comes from the file's own journal row** (design D45.2), not from the namespace
    // tree. The tree is shared, globally addressable and user-mutable — `jkb ns mv`, the VS Code
    // Rename button and `jkb ns rm` all reach it — while a file's structure is private to that
    // file and has to round-trip exactly. Reading it from a row keyed `uri PRIMARY KEY` is what
    // makes "two files share one layout" unrepresentable rather than merely guarded against.
    let (layout, sections) = read_document(journal.and_then(|j| j.document.as_deref()));
    doc.layout = layout;
    doc.sections = sections;

    // Items: everything bound to this file. Resolve every binding in one query, then
    // build both directions of the id map (uris stay ordered for a stable round-trip).
    let uris = binding::synced_uris_for_file(conn, bare_uri)?;
    let uri_ids = binding::items_for_uris(conn, &uris)?;
    let mut id_to_local: HashMap<i64, String> = HashMap::new();
    let mut resolved: Vec<(String, ItemId)> = Vec::new();
    for uri in &uris {
        let local_id = local_of(bare_uri, uri);
        if let Some(&id) = uri_ids.get(uri) {
            id_to_local.insert(id.get(), local_id.clone());
            resolved.push((local_id, id));
        }
    }

    // Batch every per-item lookup into one query each, keyed by item id, so this is a
    // constant number of round-trips instead of O(N) point queries for N items.
    let ids: Vec<ItemId> = resolved.iter().map(|(_, id)| *id).collect();
    let mut tags = tag::applications_for(conn, &ids)?;
    let mut mirrors = mirror_paths_for(conn, &ids, &file_ns_path)?;
    let parents = edge::edges_from_many(conn, &ids, EdgeType::ParentOf)?;
    let deps = edge::edges_from_many(conn, &ids, EdgeType::DependsOn)?;
    let mut item_rows = item_rows_for(conn, &ids)?;
    let placements = primary_placements_for(conn, &ids)?;

    for (local_id, id) in &resolved {
        // Skip items with no row or no primary placement (mirrors the old `load_item`).
        let (Some(row), Some(placement)) = (item_rows.remove(id), placements.get(id)) else {
            continue;
        };
        let mut item = build_sync_item(local_id, &file_ns_path, row, placement);
        item.tags = tags.remove(id).unwrap_or_default();
        item.mirrors = mirrors.remove(id).unwrap_or_default();
        doc.items.push(item);

        for (edge_type, targets) in [
            (EdgeType::ParentOf, parents.get(id)),
            (EdgeType::DependsOn, deps.get(id)),
        ] {
            for dst_id in targets.into_iter().flatten() {
                if let Some(dst) = id_to_local.get(&dst_id.get()) {
                    doc.edges.push(crate::serializers::SyncEdge {
                        src: local_id.clone(),
                        dst: dst.clone(),
                        edge_type,
                    });
                }
            }
        }
    }

    // An item's SECTION comes from the layout — the section header it sits under in the file
    // — not from the namespace it happens to be placed in. The layout is authoritative for
    // document structure, so the two must not be allowed to disagree: when they did, a
    // KB-side re-home left the assembled doc permanently different from the base, and every
    // subsequent disk edit came back as a conflict. Re-homing a file-backed item therefore
    // does not move it between sections in its file; editing the file does.
    apply_layout_sections(&mut doc);
    Ok(doc)
}

/// Set each item's `section` from its position in the layout (the nearest preceding section
/// block). Items absent from the layout keep the section derived from their placement.
fn apply_layout_sections(doc: &mut SyncDoc) {
    let mut current: Option<String> = None;
    let mut by_id: HashMap<&str, Option<String>> = HashMap::new();
    for block in &doc.layout {
        match block {
            SyncBlock::Section(path) => current = Some(path.clone()),
            SyncBlock::Item(id) => {
                by_id.insert(id.as_str(), current.clone());
            }
            SyncBlock::Prose(_) => {}
        }
    }
    for item in &mut doc.items {
        if let Some(section) = by_id.get(item.local_id.as_str()) {
            item.section.clone_from(section);
        }
    }
}

/// An item's `(kind, content, status, priority, due)` columns.
type ItemRow = (
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

/// The `(kind, content, status, priority, due)` column rows of the given items, keyed
/// by id, in one query. Items with no row are absent from the map. The batched form of
/// the per-item `items` select the old `load_item` ran.
fn item_rows_for(conn: &Connection, ids: &[ItemId]) -> Result<HashMap<ItemId, ItemRow>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT id, kind, content, status, priority, due FROM items WHERE id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(ids.iter().map(|id| id.get())),
        |r| {
            Ok((
                ItemId::new(r.get(0)?),
                (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?),
            ))
        },
    )?;
    for row in rows {
        let (id, item_row) = row?;
        out.insert(id, item_row);
    }
    Ok(out)
}

/// The primary-placement `(namespace path, position)` of the given items, keyed by id,
/// in one query. Items with no primary placement are absent from the map. The batched
/// form of the per-item primary-placement select the old `load_item` ran (an item has
/// one primary placement by construction, so first-wins is deterministic).
fn primary_placements_for(
    conn: &Connection,
    ids: &[ItemId],
) -> Result<HashMap<ItemId, (String, i64)>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT p.item_id, n.path, p.position FROM placements p
         JOIN namespaces n ON n.id = p.namespace_id
         WHERE p.role = 'primary' AND p.item_id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(ids.iter().map(|id| id.get())),
        |r| {
            Ok((
                ItemId::new(r.get(0)?),
                (r.get::<_, String>(1)?, r.get::<_, i64>(2)?),
            ))
        },
    )?;
    for row in rows {
        let (id, placement) = row?;
        out.entry(id).or_insert(placement);
    }
    Ok(out)
}

/// Shape a [`SyncItem`] from an item's already-batched row + primary placement
/// (tags/mirrors/edges are filled by the caller). The pure inverse of the per-item
/// mapping the old `load_item` did once the two rows were in hand.
fn build_sync_item(
    local_id: &str,
    file_ns_path: &str,
    row: ItemRow,
    placement: &(String, i64),
) -> SyncItem {
    let (kind, content, status, priority, due) = row;
    let (ns_path, position) = placement;
    let section = if ns_path == file_ns_path {
        None
    } else {
        Some(relative(file_ns_path, ns_path))
    };

    let mut item = SyncItem::new(local_id.to_owned(), &kind, content.unwrap_or_default());
    item.section = section;
    item.position = *position;
    item.status = status;
    item.priority = priority;
    item.due = due;
    item
}

/// The reference-placement namespace paths of the given items outside their file
/// namespace subtree, as `+ns` mirrors, keyed by item id in one query. Each item's
/// paths stay ordered (`ORDER BY item_id, n.path`) for a stable round-trip. Items
/// with no mirrors are absent from the map.
fn mirror_paths_for(
    conn: &Connection,
    ids: &[ItemId],
    file_ns_path: &str,
) -> Result<HashMap<ItemId, Vec<String>>> {
    let mut out: HashMap<ItemId, Vec<String>> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    // Escape LIKE metacharacters in the path (namespace paths contain `_`), so the subtree
    // exclusion is literal and doesn't spuriously match a sibling namespace.
    let subtree_like = format!("{}/%", jkb_core::sql::like_escape(file_ns_path));
    let placeholders = vec!["?"; ids.len()].join(", ");
    // `tasks/**` reference placements are the internal task index (auto-mirrored by
    // `task::ensure_task_mirror`), not user-authored `+ns` mirrors — never serialize
    // them back into the file, or they'd leak in as `+tasks/…` and break byte-stability.
    let sql = format!(
        "SELECT p.item_id, n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
         WHERE p.role = 'reference' AND n.path != ? AND n.path NOT LIKE ? ESCAPE '\\'
           AND n.path != 'tasks' AND n.path NOT LIKE 'tasks/%'
           AND p.item_id IN ({placeholders})
         ORDER BY p.item_id, n.path"
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(ids.len() + 2);
    params.push(rusqlite::types::Value::Text(file_ns_path.to_owned()));
    params.push(rusqlite::types::Value::Text(subtree_like));
    params.extend(
        ids.iter()
            .map(|id| rusqlite::types::Value::Integer(id.get())),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        Ok((ItemId::new(r.get::<_, i64>(0)?), r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, path) = row?;
        out.entry(id).or_default().push(path);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Three-way merge
// ---------------------------------------------------------------------------

/// The outcome of a per-item three-way comparison.
enum ThreeWay {
    /// Disjoint edits — a merged document combining both sides.
    Merged(SyncDoc),
    /// The same item changed incompatibly on both sides.
    Conflict,
}

/// A semantic signature of an item, used to detect per-item changes (ignores position,
/// which is presentation, not meaning).
#[derive(PartialEq, Eq)]
struct Sig {
    content: String,
    status: Option<String>,
    priority: Option<i64>,
    due: Option<String>,
    section: Option<String>,
    /// The item's parent `local_id` (its `parent_of` incoming edge). Part of the
    /// signature so a re-parenting/indentation change on either side is detected —
    /// without it a nesting edit has an identical `Sig` and is silently reverted.
    parent: Option<String>,
    tags: Vec<(String, String)>,
    mirrors: Vec<String>,
    deps: Vec<String>,
}

/// Signatures of every item in `doc`, keyed by `local_id`.
fn sigs(doc: &SyncDoc) -> HashMap<String, Sig> {
    let mut deps: HashMap<&str, Vec<String>> = HashMap::new();
    // `parent_of` edges run parent(src) -> child(dst); key the child's parent by dst so
    // a re-parenting shows up in the child's signature (the edge's authoritative form).
    let mut parent_of: HashMap<&str, String> = HashMap::new();
    for e in &doc.edges {
        match e.edge_type {
            EdgeType::DependsOn => deps.entry(&e.src).or_default().push(e.dst.clone()),
            EdgeType::ParentOf => {
                parent_of.insert(&e.dst, e.src.clone());
            }
            _ => {}
        }
    }
    doc.items
        .iter()
        .map(|i| {
            let mut tags = i.tags.clone();
            tags.sort();
            let mut mirrors = i.mirrors.clone();
            mirrors.sort();
            let mut d = deps.get(i.local_id.as_str()).cloned().unwrap_or_default();
            d.sort();
            (
                i.local_id.clone(),
                Sig {
                    content: i.content.clone(),
                    status: i.status.clone(),
                    priority: i.priority,
                    due: i.due.clone(),
                    section: i.section.clone(),
                    parent: parent_of.get(i.local_id.as_str()).cloned(),
                    tags,
                    mirrors,
                    deps: d,
                },
            )
        })
        .collect()
}

/// Merge disjoint disk and KB edits against a common base. Returns [`ThreeWay::Conflict`]
/// if any single item changed incompatibly on both sides.
fn three_way(base: &SyncDoc, disk: &SyncDoc, kb: &SyncDoc) -> ThreeWay {
    let (bs, ds, ks) = (sigs(base), sigs(disk), sigs(kb));
    let changed = |a: &HashMap<String, Sig>, b: &HashMap<String, Sig>| -> HashSet<String> {
        let mut out = HashSet::new();
        for id in a.keys().chain(b.keys()) {
            if a.get(id) != b.get(id) {
                out.insert(id.clone());
            }
        }
        out
    };
    let changed_disk = changed(&bs, &ds);
    let changed_kb = changed(&bs, &ks);

    for id in changed_disk.intersection(&changed_kb) {
        if ds.get(id) != ks.get(id) {
            return ThreeWay::Conflict;
        }
    }

    // Disjoint: for each id take the side that changed it (else base). `disk` is the
    // structural skeleton; kb-only changes are overlaid.
    let mut merged = SyncDoc::default();
    let mut seen: HashSet<String> = HashSet::new();
    for s in disk.sections.iter().chain(kb.sections.iter()) {
        if seen.insert(s.path.clone()) {
            merged.sections.push(s.clone());
        }
    }

    let mut ids: Vec<String> = bs
        .keys()
        .chain(ds.keys())
        .chain(ks.keys())
        .cloned()
        .collect();
    ids.sort();
    ids.dedup();

    let chosen_side = |id: &str| -> &SyncDoc {
        if changed_disk.contains(id) {
            disk
        } else if changed_kb.contains(id) {
            kb
        } else {
            base
        }
    };

    let mut present: HashSet<String> = HashSet::new();
    for id in &ids {
        let side = chosen_side(id);
        if let Some(item) = side.items.iter().find(|i| &i.local_id == id) {
            merged.items.push(item.clone());
            present.insert(id.clone());
        }
    }

    // Emit each edge from its *owner's* chosen side, so a per-item edit picks up its own
    // edges: `depends_on` is owned by its `src` (the dependent), `parent_of` by its `dst`
    // (the child — its indentation). Taking every edge by `src` alone drops a re-parented
    // child's incoming edge when only the child changed (its parent item stays on `base`).
    for id in &ids {
        let side = chosen_side(id);
        for e in &side.edges {
            let owner = match e.edge_type {
                EdgeType::ParentOf => &e.dst,
                _ => &e.src,
            };
            if owner == id {
                merged.edges.push(e.clone());
            }
        }
    }
    merged
        .edges
        .retain(|e| present.contains(&e.src) && present.contains(&e.dst));

    // Take the LAYOUT (block order + prose) wholesale from the disk side, which is the
    // structural skeleton this merge is built on and which *is* the file's own text. Merging
    // ordinals from three different parses is exactly what used to put a `##` header in the
    // middle of an item; one side's layout is coherent by construction. Blocks naming items
    // that did not survive the merge are dropped, and `render` appends anything the layout
    // does not mention, so a KB-only item is never lost.
    let source = if disk.layout.is_empty() { kb } else { disk };
    merged.layout = source
        .layout
        .iter()
        .filter(|b| match b {
            SyncBlock::Item(id) => present.contains(id),
            _ => true,
        })
        .cloned()
        .collect();
    ThreeWay::Merged(merged)
}

/// Parse the last-synced base bytes back into a document for three-way merge; an empty
/// document if the base blob is missing.
fn load_base_doc(
    conn: &Connection,
    journal: Option<&sync_state::SyncState>,
    serializer: &dyn SyncSerializer,
) -> Result<Option<SyncDoc>> {
    let Some(hash) = journal.and_then(|j| j.base_blob_hash.as_deref()) else {
        return Ok(None);
    };
    match blob::load(conn, hash)? {
        Some(bytes) => Ok(Some(serializer.parse(&bytes)?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// The current `local_id -> ItemId` map for a file. Resolves every binding in one query.
fn existing_by_local(conn: &Connection, bare_uri: &str) -> Result<HashMap<String, ItemId>> {
    let uris = binding::synced_uris_for_file(conn, bare_uri)?;
    let uri_ids = binding::items_for_uris(conn, &uris)?;
    let mut out = HashMap::new();
    for uri in &uris {
        if let Some(&id) = uri_ids.get(uri) {
            out.insert(local_of(bare_uri, uri), id);
        }
    }
    Ok(out)
}

/// The item ids currently bound to a file (for stamping on export). Resolves every
/// binding in one query; ids stay in the `synced_uris_for_file` order.
fn current_bindings(conn: &Connection, bare_uri: &str) -> Result<Vec<ItemId>> {
    let uris = binding::synced_uris_for_file(conn, bare_uri)?;
    let uri_ids = binding::items_for_uris(conn, &uris)?;
    let mut out = Vec::new();
    for uri in &uris {
        if let Some(&id) = uri_ids.get(uri) {
            out.push(id);
        }
    }
    Ok(out)
}

/// The `kind` of an item, if it exists.
fn item_kind(conn: &Connection, id: ItemId) -> Result<Option<String>> {
    let kind: Option<String> = conn
        .prepare_cached("SELECT kind FROM items WHERE id = ?1")?
        .query_row([id.get()], |r| r.get(0))
        .optional()?;
    Ok(kind)
}

/// The binding uri for an item: `file://<path>` for the document single item (empty
/// `local_id`), else `file://<path>#<local_id>`.
fn item_uri(bare_uri: &str, local_id: &str) -> String {
    if local_id.is_empty() {
        bare_uri.to_owned()
    } else {
        format!("{bare_uri}#{local_id}")
    }
}

/// The `local_id` encoded in a binding `uri` relative to its file's `bare_uri`.
fn local_of(bare_uri: &str, uri: &str) -> String {
    uri.strip_prefix(bare_uri)
        .and_then(|rest| rest.strip_prefix('#'))
        .unwrap_or("")
        .to_owned()
}

/// The mirror namespace for a file: the mount namespace, the file's parent directories, and
/// **the filename itself** as the final segment (design D39.1).
///
/// One namespace, one file. Since D45 a file's structure lives on its **journal row**, not here,
/// so this no longer decides what a document looks like — but it still bounds
/// `retire_undeclared_sections`, which walks a namespace subtree: without the filename segment
/// that subtree is the whole directory, so importing one file retires a neighbouring file's
/// sections (and at the mount root, the whole mount's).
///
/// It was introduced for a stronger reason that no longer applies: every file in a directory
/// shared one namespace and therefore one `layout`, so whichever synced last owned it and the
/// next export of any sibling rendered from the wrong one. That is the `openspec` collapse —
/// 62 of 63 files left byte-identical to a neighbour — and D45 removed its cause rather than
/// its symptom.
///
/// It cost seven guards across eight review passes to keep answering *whose layout is this?*,
/// and each answer was a proxy — did it sync cleanly, is a sibling bound, is there a journal
/// row — that was true in some case where the real answer was the other file. The question has
/// no instances now.
///
/// **The filename keeps its extension.** `tasks` reads better than `tasks.md`, but then
/// `tasks.md` and `tasks.txt` in one directory would collide again — the same defect in a
/// rarer form, which is worse. The segment is the filename because the filename is what is
/// unique within a directory.
fn namespace_for(ctx: &Ctx, path: &Path) -> String {
    let rel = path.strip_prefix(&ctx.dir).unwrap_or(path);
    let mut parts = vec![ctx.mount_ns.clone()];
    for comp in rel.components() {
        if let Component::Normal(seg) = comp {
            parts.push(seg.to_string_lossy().into_owned());
        }
    }
    parts.join("/")
}

/// `ns_path` relative to `base` (drops the `base/` prefix).
fn relative(base: &str, ns_path: &str) -> String {
    ns_path
        .strip_prefix(base)
        .and_then(|r| r.strip_prefix('/'))
        .unwrap_or(ns_path)
        .to_owned()
}

/// `path` relative to `dir` as a forward-slash string, for glob matching.
fn rel_str(dir: &Path, path: &Path) -> String {
    path.strip_prefix(dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A `file://<absolute path>` uri.
/// The journal/binding key for a file: `file://<absolute path>`.
///
/// Public because the CLI needs to match journal rows against reconciled paths, and a
/// hand-rebuilt copy over there made the sync exit code depend on a string convention with no
/// owner. One spelling, exported.
#[must_use]
pub fn file_uri(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy())
}

/// blake3 of `bytes` as lowercase hex (the sync hash; same scheme as the blob store).
fn hash(bytes: &[u8]) -> String {
    blob::hash_bytes(bytes)
}

/// Write `bytes` to `path`, creating parent directories as needed.
fn write_file(path: &Path, bytes: &[u8], snapshot: Option<&[u8]>) -> Result<()> {
    // REFUSE to write if the file is no longer what this pass reconciled.
    //
    // `reconcile` decides direction from bytes read by `archive_current_bytes`, which commits
    // its own transaction and then queues behind the writer thread — seconds under load. A save
    // landing in that window used to be caught because the old code re-read the file inside the
    // reconcile transaction; passing one snapshot down closed a double-read race and opened a
    // wider one. So the snapshot is re-validated here, at the single seam every write goes
    // through, immediately before the bytes hit the disk.
    //
    // The two cases are deliberately asymmetric. `Some(bytes)` means those bytes are in the
    // blob archive, so a mismatch costs a retry. `None` means the file was ABSENT when this pass
    // started and nothing was preserved — so a file that has appeared since (a `git restore`,
    // an editor writing late) is the one overwrite that would be recoverable from nothing.
    let current = match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if current.as_deref() != snapshot {
        return Err(Error::Types(TypeError::Validation(format!(
            "{} changed on disk while it was being synced; nothing was written. It will be \
             reconciled on the next pass.",
            path.display()
        ))));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Map a mount `sync_mode` string to a [`SyncMode`] (unknown → bidirectional).
fn sync_mode_of(s: &str) -> SyncMode {
    match s {
        "import" => SyncMode::Import,
        "export" => SyncMode::Export,
        _ => SyncMode::Bidirectional,
    }
}
