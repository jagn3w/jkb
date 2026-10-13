//! Filesystem watch mode: react to changes under a mount and reconcile, debouncing
//! bursts of events (editors save via several syscalls) so one logical edit triggers
//! one reconcile. This is the only async/blocking edge in `jkb-sync`.
//!
//! Events carry the paths that changed, so a burst reconciles just those files via
//! [`crate::sync_paths`] rather than re-scanning the whole mount — important once a
//! mount backs a large tree. A change made in the **database** raises no filesystem event, so after
//! each iteration the watcher also asks the changelog whether anyone but sync has written since its last
//! look (at most once per debounce), and if so reconciles the bound files whose knowledge-base side
//! changed ([`crate::sync_kb_changes`]), at most once per three debounces.
//! Three situations fall back to a full [`crate::sync`]: the initial reconcile on startup (to
//! catch drift from while the watcher was off); a watcher error or dropped-events signal
//! (`need_rescan`), where the incremental path list can no longer be trusted, which runs at once;
//! and the retry a failed pass owes ([`RetryDebt`]), which runs once its backoff has elapsed —
//! on the next event or idle tick, whichever comes first.
//!
//! Stopping is via a shared `Arc<AtomicBool>` flag (checked each idle tick), so one
//! signal (e.g. the CLI's Ctrl-C handler) can stop a single [`watch`] or the
//! all-mounts [`watch_all`] uniformly.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::{Event, RecursiveMode, Watcher};

use jkb_core::{mount, sync_state, Db};

use crate::{engine, Error, Result};

/// Watch the mount at `mount_ns` and reconcile on change until `stop` is set. A
/// one-shot full [`crate::sync`] runs first; thereafter each debounced burst of events
/// reconciles only the paths those events named (see the module docs).
///
/// `debounce` is both the quiet-period that coalesces a burst and the poll interval
/// at which `stop` is checked while idle.
///
/// # Errors
/// Returns an error if the watcher cannot be created/armed. A **reconcile failure is reported,
/// not returned** (see `run_pass`) — the loop has to outlive one bad pass, or a mount stops
/// syncing for the life of the service.
pub fn watch(db: &Db, mount_ns: &str, debounce: Duration, stop: &Arc<AtomicBool>) -> Result<()> {
    let dir = engine::backing_dir(db, mount_ns)?;

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    // Links are not followed: a directory link planted inside a mount — a dev container can write
    // inside the directories it binds — made the recursive watch walk and subscribe to the host
    // directories it pointed at, and turned their activity into events under the mount.
    let mut watcher = notify::RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            // A read changes nothing, and is dropped here rather than in the loop: counted there,
            // any process reading a file under the mount — an editor, a grep, this watcher's own
            // reconcile — kept the debounce from ever going quiet, so the idle tick that asks about
            // database changes never came.
            if res.as_ref().is_ok_and(|event| is_read_only(event.kind)) {
                return;
            }
            // A closed receiver just means we're shutting down; ignore the send error.
            let _ = tx.send(res);
        },
        notify::Config::default().with_follow_symlinks(false),
    )?;
    // Recursive: the OS only lets us subscribe to a directory subtree, not a glob, so
    // relevance filtering happens in `sync_paths` against the mount's include/exclude.
    watcher.watch(&dir, RecursiveMode::Recursive)?;

    // Read before the first pass, so a write that lands during it is still seen afterwards.
    let mut database = DatabaseWrites::new(db.read(sync_state::latest_write)?);
    let mut debt = RetryDebt::new(
        run_pass(mount_ns, || engine::sync(db, mount_ns)),
        debounce.saturating_mul(RETRY_TICKS),
    );

    while !stop.load(Ordering::Relaxed) {
        // What this iteration owes: a full re-scan, or a targeted pass over these paths.
        let work = match rx.recv_timeout(debounce) {
            Ok(first) => {
                let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
                let mut rescan = collect(first, &mut paths);
                // Coalesce: keep draining until the filesystem is quiet for `debounce` — or for at
                // most a burst's worth of it. Under churn that never pauses (a build, git, a task
                // worktree writing under a repo mount) an unbounded drain never returned, so neither
                // the paths it had gathered nor anything after this loop ever ran.
                let burst = Instant::now();
                while burst.elapsed() < debounce.saturating_mul(BURST_TICKS) {
                    let Ok(next) = rx.recv_timeout(debounce) else {
                        break;
                    };
                    rescan |= collect(next, &mut paths);
                }
                // A dropped-event rescan is immediate; a RETRY waits for the backoff. Without
                // the interval here `retry_owed` latched on any deterministically-failing file
                // — a PNG caught by a `document` glob — and turned every debounced save into a
                // whole-mount re-walk on the single writer thread, forever. The debt stays owed
                // either way; the idle arm settles it.
                if rescan || debt.due() {
                    Some(Work::Full)
                } else if paths.is_empty() {
                    None
                } else {
                    Some(Work::Paths(paths.into_iter().collect()))
                }
            }
            // An idle tick still settles a debt. Without this the retry only ever fired if a
            // file happened to change, which is the one condition a failed pass cannot rely on.
            Err(RecvTimeoutError::Timeout) => debt.due().then_some(Work::Full),
            Err(RecvTimeoutError::Disconnected) => break,
        };

        match work {
            Some(Work::Full) => {
                debt.full_pass(run_pass(mount_ns, || engine::sync(db, mount_ns)));
            }
            Some(Work::Paths(paths)) => {
                debt.targeted_pass(run_pass(mount_ns, || {
                    engine::sync_paths(db, mount_ns, &paths)
                }));
            }
            None => {}
        }

        // Then the database, whatever this iteration did: nothing on the filesystem reports a task
        // edited through `jkb`, and asked only on an idle tick it waited out any file churn.
        database.poll(db, mount_ns, debounce);
        if database.pass_due(debounce.saturating_mul(EXPORT_TICKS)) {
            let judged = &mut database.judged;
            debt.targeted_pass(run_pass(mount_ns, || {
                engine::sync_kb_changes(db, mount_ns, judged)
            }));
            database.passed();
        }
    }
    Ok(())
}

/// The retry a failed pass owes, plus the backoff that spaces retries out.
///
/// This is a type rather than three variables in `watch` because the two kinds of pass update it
/// **asymmetrically**, and written inline that asymmetry is one character away from vanishing —
/// and did vanish twice. A full pass re-examined every file, so its result *replaces* the debt.
/// A targeted pass looked at a handful of event paths and knows nothing about the rest of the
/// mount, so it can only ever *add* to it. Assigning there let one unrelated successful save
/// discharge the debt of a failed full pass, which was then never retried at all — the exact
/// failure the debt exists to prevent. `targeted_pass` has no way to express "replace".
struct RetryDebt {
    owed: bool,
    /// The interval a fresh debt waits, before any backoff doubling.
    base: Duration,
    after: Duration,
    /// When the last **full** pass finished. Measured from the finish, not the start: a pass
    /// slower than the interval would otherwise have already "waited" by the time it returned,
    /// collapsing the spacing to zero and running back-to-back full syncs on the writer thread.
    last: Instant,
}

impl RetryDebt {
    fn new(owed: bool, base: Duration) -> Self {
        Self {
            owed,
            base,
            after: base,
            last: Instant::now(),
        }
    }

    /// Is a retry both owed and due? A dropped-event rescan ignores this and runs immediately;
    /// only the retry waits.
    fn due(&self) -> bool {
        self.owed && self.last.elapsed() >= self.after
    }

    /// A full pass's result is the whole truth about this mount, so it replaces the debt and
    /// restarts the clock. The interval doubles while it keeps failing, so a permanently broken
    /// mount — its namespace deleted, say — does not log and re-run forever at a fixed rate.
    fn full_pass(&mut self, failed: bool) {
        self.owed = failed;
        self.last = Instant::now();
        self.after = if failed {
            (self.after * 2).min(MAX_RETRY)
        } else {
            self.base
        };
    }

    /// A targeted pass can only add to the debt, and deliberately does **not** touch the clock:
    /// the backoff measures quiet time between *full* passes, so a steady stream of ordinary
    /// saves would otherwise hold a pending retry off forever.
    fn targeted_pass(&mut self, failed: bool) {
        self.owed |= failed;
    }
}

/// What a watcher knows about writes to the database that no filesystem event reports.
///
/// The changelog is asked at most once per debounce and a pass is run at most once per spacing, with a
/// write seen in between kept owed rather than dropped: without the spacing a fleet writing to any part
/// of the knowledge base had every mount render every bound file on each tick, on the single writer
/// thread. A changelog that cannot be read is said once per failing stretch, not on every tick.
struct DatabaseWrites {
    /// The newest changelog id looked at.
    seen: i64,
    /// Something other than sync wrote since the last pass.
    owed: bool,
    polled: Instant,
    passed: Option<Instant>,
    failing: bool,
    /// What each flagged bound file was last judged on, so it is reconciled once per change.
    judged: engine::FlaggedJudgements,
}

impl DatabaseWrites {
    fn new(seen: i64) -> Self {
        Self {
            seen,
            owed: false,
            polled: Instant::now(),
            passed: None,
            failing: false,
            judged: engine::FlaggedJudgements::default(),
        }
    }

    /// Ask the changelog what was written since the last look, if a debounce has passed.
    fn poll(&mut self, db: &Db, mount_ns: &str, every: Duration) {
        if self.polled.elapsed() < every {
            return;
        }
        self.polled = Instant::now();
        let after = self.seen;
        match db.read(move |conn| sync_state::writes_since(conn, after)) {
            Ok(writes) => {
                self.seen = writes.latest;
                self.owed |= writes.by_others;
                self.failing = false;
            }
            Err(e) => {
                if !self.failing {
                    eprintln!("sync {mount_ns}: cannot read the changelog ({e}); retrying");
                }
                self.failing = true;
            }
        }
    }

    /// Whether a pass over database changes is owed and `spacing` has passed since the last one ended.
    fn pass_due(&self, spacing: Duration) -> bool {
        self.owed && self.passed.is_none_or(|at| at.elapsed() >= spacing)
    }

    /// A pass has run: the debt is discharged — a write during it is owed again by the next poll, which
    /// reads past the mark taken before it — and the spacing is measured from **now**, when it ended.
    /// From its start, a pass longer than the spacing was followed at once by the next, the unspaced
    /// cost the spacing exists to prevent ([`RetryDebt`] measures from the finish for the same reason).
    fn passed(&mut self) {
        self.owed = false;
        self.passed = Some(Instant::now());
    }
}

/// How many debounce intervals one burst of filesystem events is coalesced for, at most.
const BURST_TICKS: u32 = 10;
/// How many debounce intervals apart passes over database changes run, at least.
const EXPORT_TICKS: u32 = 3;

/// How many debounce intervals to wait before retrying a failed pass.
const RETRY_TICKS: u32 = 10;
/// Ceiling for the backoff, so a permanently failing mount settles at one attempt a minute
/// rather than filling an unrotated log.
const MAX_RETRY: Duration = Duration::from_mins(1);

/// What one iteration of the watch loop owes.
enum Work {
    /// Re-scan the whole mount: events were dropped, or a previous pass failed.
    Full,
    /// Reconcile exactly these paths.
    Paths(Vec<PathBuf>),
}

/// Run one reconcile pass, reporting whatever happens. **Never returns an error.** Answers whether
/// the pass owes a retry: `true` when it failed outright or any file in it failed, which the caller
/// records as [`RetryDebt`] and settles with a full pass once the backoff allows.
///
/// The watcher's unit of failure is a pass, not the thread. Making the per-file reconcile
/// non-fatal was not enough: `outcome_reason` and `settle_out_of_scope` still propagate out of
/// `sync`/`sync_paths`, and a single `Err` here used to exit this mount's thread for good —
/// `watch_all` does not set `stop`, so the process stayed alive joining the others, launchd never
/// restarted it, and that mount silently stopped syncing.
fn run_pass<F>(mount_ns: &str, pass: F) -> bool
where
    F: FnOnce() -> crate::Result<engine::SyncReport>,
{
    match pass() {
        Ok(report) => {
            report_notable(mount_ns, &report);
            // A per-file failure owes a retry just as much as a pass-level one — since a lost
            // write-lock race is recorded rather than raised, returning `false` here meant the
            // failure mode that actually happens was the one never retried.
            //
            // The debt is never *abandoned*. An earlier version dropped it once the failing set
            // repeated, to stop a deterministically-unreadable file forcing a full re-walk on
            // every event — but that also gave up on transient contention after two attempts,
            // which is the case the retry exists for. The cost it was avoiding is handled by the
            // backoff instead: the caller escalates to a full pass only once the interval has
            // elapsed.
            !report.failed().is_empty()
        }
        Err(e) => {
            // A pass can fail AFTER per-file transactions have committed — `outcome_reason` reads
            // the journal after each file's — so the batch's remaining paths are lost while the
            // files before them read as settled. Returning `true` owes a full re-scan, run once
            // the retry backoff has elapsed, so the work is picked up rather than waiting for
            // someone to touch those files again.
            eprintln!(
                "sync {mount_ns}: pass failed ({e}); re-scanning the mount after the retry backoff"
            );
            // A pass-level error says nothing about which files are healthy, so the debt is
            // owed unconditionally. The caller decides *when* to settle it: a dropped-event
            // rescan runs immediately, a retry waits for the backoff.
            true
        }
    }
}

/// Say anything a person would want to know about a reconcile, on stderr.
///
/// The watcher is how `jkb` runs in practice — `jkb service install` puts it under
/// launchd/systemd — and it had **no** output at all: not one `print` or `eprint` in the file.
/// The sharpest case is `resolved()`. A `disk_wins`/`kb_wins` resolution throws one side's edits
/// away and then settles the journal `ok`, so it is invisible to `jkb doctor` too: without this,
/// a destructive resolution left no trace on any surface in the system.
fn report_notable(mount_ns: &str, report: &engine::SyncReport) {
    for (path, how) in report.resolved() {
        eprintln!("sync {mount_ns}: RESOLVED {} — {how}", path.display());
    }
    for (path, err) in report.failed() {
        eprintln!("sync {mount_ns}: FAILED {}: {err}", path.display());
    }
    for (path, reason) in report.refused() {
        eprintln!("sync {mount_ns}: REFUSED {}: {reason}", path.display());
    }
    for path in report.conflicts() {
        eprintln!("sync {mount_ns}: conflict {}", path.display());
    }
    for path in report.quarantined() {
        eprintln!(
            "sync {mount_ns}: needs attention (parse failed) {}",
            path.display()
        );
    }
}

/// Watch **every** configured mount concurrently (one thread each), reconciling each
/// on change until `stop` is set. This is the persistent-daemon entry point
/// (`jkb sync --watch` with no namespace). Returns once every watcher has stopped.
///
/// The set of watched mounts is **not** fixed at startup. Every [`MOUNT_SCAN_TICKS`] debounces the
/// mount list is re-read ([`Watchers::tick`]): a mount created since is armed, a mount deleted
/// since has its watcher stopped, and a watcher that exited — a mount whose backing directory was
/// missing when it tried to arm, say — is re-armed after a backoff. Reading the list once made a
/// mount created while the service ran unwatched until the service restarted, which under launchd
/// is the next login; and a mount whose watcher failed to arm stayed unwatched even once its
/// directory came back, with nothing on any surface saying so.
///
/// # Errors
/// Returns an error only if the mount list cannot be read at startup. A watcher that fails to arm,
/// exits or panics is reported on stderr and re-armed; reconcile failures never reach here, each
/// thread reports its own as it goes.
pub fn watch_all(db: &Db, debounce: Duration, stop: &Arc<AtomicBool>) -> Result<()> {
    let mut mounts: BTreeSet<String> = db.read(mount::all_paths)?.into_iter().collect();
    let mut watchers = Watchers::new(debounce.saturating_mul(RETRY_TICKS));
    let mut arm = |path: &str| ThreadWatcher::spawn(db, path, debounce);

    // Armed before `stop` is first read, so every mount gets its initial reconcile even when the
    // caller stops at once.
    watchers.tick(&mounts, Instant::now(), &mut arm);
    let mut ticks: u32 = 0;
    let mut listing_failed = false;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(debounce);
        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(MOUNT_SCAN_TICKS) {
            match db.read(mount::all_paths) {
                Ok(paths) => {
                    mounts = paths.into_iter().collect();
                    listing_failed = false;
                }
                // Keep the last list: a transient read failure must not stop every watcher.
                Err(e) => {
                    if !listing_failed {
                        eprintln!("sync: cannot re-read the mount list ({e}); retrying");
                    }
                    listing_failed = true;
                }
            }
        }
        watchers.tick(&mounts, Instant::now(), &mut arm);
    }
    watchers.shutdown();
    Ok(())
}

/// How many debounce intervals apart `watch_all` re-reads the mount list.
const MOUNT_SCAN_TICKS: u32 = 10;

/// One running per-mount watcher, as [`Watchers`] sees it. A trait so the supervision rules can
/// be tested with watchers whose lifetime the test decides, rather than with threads and sleeps.
trait Running {
    /// Whether the watcher has exited (and [`Running::join`] will not block).
    fn finished(&self) -> bool;
    /// Ask the watcher to stop; it exits at its next idle tick.
    fn stop(&self);
    /// Wait for the watcher to exit; `Err` names why it stopped, a panic included.
    fn join(self) -> Result<()>;
}

/// A [`watch`] on its own thread with its own stop flag, so a deleted mount's watcher can be
/// stopped without stopping the others.
struct ThreadWatcher {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<Result<()>>,
}

impl ThreadWatcher {
    fn spawn(db: &Db, path: &str, debounce: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let db = db.clone();
        let path = path.to_owned();
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || watch(&db, &path, debounce, &thread_stop));
        Self { stop, handle }
    }
}

impl Running for ThreadWatcher {
    fn finished(&self) -> bool {
        self.handle.is_finished()
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn join(self) -> Result<()> {
        self.handle.join().unwrap_or_else(|_| {
            Err(Error::Types(jkb_types::Error::Validation(
                "the watch thread panicked".to_owned(),
            )))
        })
    }
}

/// One mount's place in [`Watchers`]: its watcher if one is running, and when to re-arm it if not.
struct Slot<H> {
    running: Option<H>,
    /// When a mount with no running watcher may be armed again.
    retry_at: Instant,
    /// When the running watcher was armed.
    armed_at: Instant,
    /// The wait before the next re-arm, doubling while the watcher keeps exiting.
    backoff: Duration,
}

/// The per-mount watchers `watch_all` keeps running, reconciled against the mount list.
///
/// Exactly one slot per mount path, so a mount is never watched twice however often it is listed:
/// a duplicate watcher would reconcile every file twice, racing itself on the write seam.
struct Watchers<H: Running> {
    slots: std::collections::BTreeMap<String, Slot<H>>,
    /// Watchers of deleted mounts, stopped and waiting to exit.
    retiring: Vec<H>,
    base: Duration,
}

impl<H: Running> Watchers<H> {
    fn new(base: Duration) -> Self {
        Self {
            slots: std::collections::BTreeMap::new(),
            retiring: Vec::new(),
            base,
        }
    }

    /// Bring the running watchers in line with `mounts`: stop those of deleted mounts, collect
    /// any that exited (reporting why, and scheduling a re-arm after a backoff that doubles while
    /// it keeps exiting), and arm every listed mount that has no watcher and is due one.
    fn tick(&mut self, mounts: &BTreeSet<String>, now: Instant, arm: &mut impl FnMut(&str) -> H) {
        self.slots.retain(|path, slot| {
            if mounts.contains(path) {
                return true;
            }
            if let Some(running) = slot.running.take() {
                running.stop();
                self.retiring.push(running);
            }
            false
        });
        let (done, still): (Vec<H>, Vec<H>) = self.retiring.drain(..).partition(Running::finished);
        self.retiring = still;
        for running in done {
            // A deleted mount's watcher: why it stopped no longer matters.
            let _ = running.join();
        }

        for path in mounts {
            let slot = self.slots.entry(path.clone()).or_insert_with(|| Slot {
                running: None,
                retry_at: now,
                armed_at: now,
                backoff: self.base,
            });
            match slot.running.take() {
                Some(running) if running.finished() => {
                    let why = match running.join() {
                        Ok(()) => "it exited".to_owned(),
                        Err(e) => e.to_string(),
                    };
                    // Reported HERE, as it happens: under launchd `watch_all` never returns, so a
                    // watcher that stopped silently is a mount that silently stopped syncing.
                    eprintln!(
                        "sync {path}: watcher stopped: {why}; re-arming in {}s",
                        slot.backoff.as_secs_f64()
                    );
                    slot.retry_at = now + slot.backoff;
                    slot.backoff = (slot.backoff * 2).min(MAX_RETRY);
                }
                Some(running) => {
                    // Up for a whole base interval: it armed and is working, so a later exit
                    // starts again from the base backoff rather than wherever a past failing
                    // stretch left it.
                    if now.saturating_duration_since(slot.armed_at) >= self.base {
                        slot.backoff = self.base;
                    }
                    slot.running = Some(running);
                }
                None if now >= slot.retry_at => {
                    slot.armed_at = now;
                    slot.running = Some(arm(path));
                }
                None => {}
            }
        }
    }

    /// Stop every watcher and wait for each to exit.
    fn shutdown(self) {
        let running: Vec<H> = self
            .slots
            .into_values()
            .filter_map(|slot| slot.running)
            .chain(self.retiring)
            .collect();
        for r in &running {
            r.stop();
        }
        for r in running {
            if let Err(e) = r.join() {
                eprintln!("sync: a watcher stopped with an error: {e}");
            }
        }
    }
}

/// Accumulate an event's changed paths into `paths`. Returns `true` if a full rescan
/// is needed — a watcher error, or the OS signalled it dropped events — because the
/// incremental path list can no longer be trusted.
fn collect(res: notify::Result<Event>, paths: &mut BTreeSet<PathBuf>) -> bool {
    match res {
        Ok(event) if event.need_rescan() => true,
        Ok(event) => {
            paths.extend(event.paths);
            false
        }
        Err(_) => true,
    }
}

/// Whether `kind` is an access that cannot have changed the file: an open, a read, a close after
/// reading. A close after writing is not — it ends a write.
///
/// Linux's inotify backend subscribes to opens, so the reconcile's own read of a file raised an event
/// asking for another reconcile of it, at every debounce, for as long as the watcher ran. That loop
/// re-reconciled every recently read file, which hid on Linux that nothing exported a change made in
/// the database; on macOS, whose events carry no opens, the edit simply never reached the file.
fn is_read_only(kind: notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode};
    matches!(
        kind,
        notify::EventKind::Access(access)
            if !matches!(access, AccessKind::Close(AccessMode::Write))
    )
}

#[cfg(test)]
mod tests {
    use super::{RetryDebt, MAX_RETRY};
    use std::time::Duration;

    const BASE: Duration = Duration::from_millis(100);

    /// The regression this type exists for. A targeted pass sees only the paths a file event
    /// named; a successful one is not evidence that the mount-wide failure a full pass recorded
    /// has gone away. When the targeted arm assigned instead of OR-ing, any unrelated save
    /// cleared the debt and the failed full pass was never retried — the watcher went quiet on a
    /// broken mount, which is the one thing it must not do.
    #[test]
    fn a_successful_targeted_pass_does_not_discharge_a_full_passs_debt() {
        let mut debt = RetryDebt::new(true, BASE);
        debt.targeted_pass(false);
        assert!(
            debt.owed,
            "a successful targeted pass discharged the retry owed by a failed full pass; \
             that full pass will now never be retried"
        );
    }

    /// The clock belongs to full passes. Stamping it here meant a steady stream of ordinary
    /// saves kept pushing the deadline out, so a due retry never came due.
    #[test]
    fn a_targeted_pass_does_not_push_out_a_pending_retry() {
        let mut debt = RetryDebt::new(true, BASE);
        let deadline = debt.last;
        debt.targeted_pass(true);
        assert_eq!(
            debt.last, deadline,
            "a targeted pass moved the backoff clock, delaying a retry that was already owed"
        );
    }

    /// The other half of the asymmetry: a full pass re-examined everything, so a clean one is
    /// entitled to clear the debt and reset the backoff.
    #[test]
    fn a_full_pass_replaces_the_debt_in_both_directions() {
        let mut debt = RetryDebt::new(false, BASE);
        debt.full_pass(true);
        assert!(debt.owed, "a failed full pass recorded no debt");
        assert_eq!(debt.after, BASE * 2, "the retry interval did not back off");

        debt.full_pass(false);
        assert!(!debt.owed, "a clean full pass left the debt owed");
        assert_eq!(
            debt.after, BASE,
            "the retry interval did not reset after a clean pass"
        );
    }

    /// Backoff doubles but is capped, so a permanently broken mount settles at a slow retry
    /// rather than growing an interval that overflows or effectively stops retrying.
    #[test]
    fn backoff_doubles_up_to_the_cap_and_stops() {
        let mut debt = RetryDebt::new(false, BASE);
        for _ in 0..100 {
            debt.full_pass(true);
        }
        assert_eq!(debt.after, MAX_RETRY, "backoff did not settle at the cap");
    }

    /// Opening or reading a file is not a change to it; closing it after a write is.
    #[test]
    fn a_read_only_access_is_not_a_change() {
        use notify::event::{AccessKind, AccessMode, CreateKind, EventKind};
        assert!(super::is_read_only(EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(super::is_read_only(EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(!super::is_read_only(EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(!super::is_read_only(EventKind::Create(CreateKind::File)));
    }

    /// A database write is owed until a pass takes it, and passes are spaced: a write inside the spacing
    /// is kept for the next one, not dropped.
    #[test]
    fn database_passes_are_spaced_and_a_write_between_them_is_kept() {
        let mut writes = super::DatabaseWrites::new(0);
        assert!(!writes.pass_due(BASE), "nothing written, nothing owed");
        writes.owed = true;
        assert!(writes.pass_due(BASE), "the first owed pass runs at once");
        // A pass that takes longer than the spacing: the next is measured from its end.
        std::thread::sleep(BASE * 2);
        writes.passed();
        writes.owed = true;
        assert!(
            !writes.pass_due(BASE),
            "a second inside the spacing after the last pass ENDED waits"
        );
        assert!(writes.owed, "and stays owed");
        std::thread::sleep(BASE);
        assert!(writes.pass_due(BASE), "then runs");
        writes.passed();
        assert!(!writes.owed);
        writes.owed = true;
        assert!(
            !writes.pass_due(BASE),
            "every pass restarts the spacing, not only the first"
        );
    }

    /// A watcher whose lifetime the test decides.
    #[derive(Default)]
    struct FakeState {
        finished: std::cell::Cell<bool>,
        stopped: std::cell::Cell<bool>,
        joined: std::cell::Cell<bool>,
    }

    struct Fake(std::rc::Rc<FakeState>);

    impl super::Running for Fake {
        fn finished(&self) -> bool {
            self.0.finished.get()
        }
        fn stop(&self) {
            self.0.stopped.set(true);
        }
        fn join(self) -> crate::Result<()> {
            self.0.joined.set(true);
            Err(crate::Error::Types(jkb_types::Error::Validation(
                "backing directory missing".to_owned(),
            )))
        }
    }

    /// Records every arm, keeping a handle on each fake so the test can end or inspect it.
    #[derive(Default)]
    struct Arms(Vec<(String, std::rc::Rc<FakeState>)>);

    impl Arms {
        fn arm(&mut self, path: &str) -> Fake {
            let state = std::rc::Rc::new(FakeState::default());
            self.0.push((path.to_owned(), std::rc::Rc::clone(&state)));
            Fake(state)
        }
        fn count(&self, path: &str) -> usize {
            self.0.iter().filter(|(p, _)| p == path).count()
        }
        fn last(&self, path: &str) -> std::rc::Rc<FakeState> {
            let found = self.0.iter().rev().find(|(p, _)| p == path);
            std::rc::Rc::clone(&found.expect("armed").1)
        }
    }

    fn mounts(paths: &[&str]) -> std::collections::BTreeSet<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    /// A mount created after the watchers started is armed by the next tick, and a mount already
    /// watched is never armed a second time however often it is listed.
    #[test]
    fn a_mount_created_later_is_armed_once() {
        let now = std::time::Instant::now();
        let mut arms = Arms::default();
        let mut w = super::Watchers::new(BASE);
        w.tick(&mounts(&["docs/a"]), now, &mut |p| arms.arm(p));
        assert_eq!(arms.count("docs/a"), 1);

        w.tick(&mounts(&["docs/a", "docs/b"]), now, &mut |p| arms.arm(p));
        w.tick(&mounts(&["docs/a", "docs/b"]), now + BASE, &mut |p| {
            arms.arm(p)
        });
        assert_eq!(arms.count("docs/b"), 1, "the new mount is armed");
        assert_eq!(
            arms.count("docs/a"),
            1,
            "a watched mount is never armed twice"
        );
    }

    /// A watcher that exited — one that failed to arm — is re-armed once its backoff elapses and
    /// not before, and the backoff doubles while it keeps failing.
    #[test]
    fn a_watcher_that_exited_is_re_armed_after_a_backoff() {
        let now = std::time::Instant::now();
        let list = mounts(&["docs/a"]);
        let mut arms = Arms::default();
        let mut w = super::Watchers::new(BASE);
        w.tick(&list, now, &mut |p| arms.arm(p));
        arms.last("docs/a").finished.set(true);

        w.tick(&list, now, &mut |p| arms.arm(p));
        assert!(arms.last("docs/a").joined.get(), "the exit is collected");
        assert_eq!(arms.count("docs/a"), 1, "not re-armed before the backoff");
        w.tick(&list, now + BASE, &mut |p| arms.arm(p));
        assert_eq!(arms.count("docs/a"), 2, "re-armed once the backoff elapsed");

        // Fails again at once: the next wait is doubled.
        arms.last("docs/a").finished.set(true);
        w.tick(&list, now + BASE, &mut |p| arms.arm(p));
        w.tick(&list, now + BASE * 2, &mut |p| arms.arm(p));
        assert_eq!(arms.count("docs/a"), 2, "the second wait is longer");
        w.tick(&list, now + BASE * 3, &mut |p| arms.arm(p));
        assert_eq!(arms.count("docs/a"), 3);
    }

    /// A deleted mount's watcher is stopped, and collected once it exits.
    #[test]
    fn a_deleted_mount_s_watcher_is_stopped() {
        let now = std::time::Instant::now();
        let mut arms = Arms::default();
        let mut w = super::Watchers::new(BASE);
        w.tick(&mounts(&["docs/a", "docs/b"]), now, &mut |p| arms.arm(p));
        w.tick(&mounts(&["docs/a"]), now, &mut |p| arms.arm(p));
        let b = arms.last("docs/b");
        assert!(
            b.stopped.get(),
            "the deleted mount's watcher was not stopped"
        );
        assert!(!arms.last("docs/a").stopped.get());

        b.finished.set(true);
        w.tick(&mounts(&["docs/a"]), now, &mut |p| arms.arm(p));
        assert!(
            b.joined.get(),
            "the stopped watcher is collected once it exits"
        );
        assert_eq!(arms.count("docs/b"), 1, "and is not re-armed");
    }

    /// Shutdown stops and collects every watcher.
    #[test]
    fn shutdown_stops_every_watcher() {
        let now = std::time::Instant::now();
        let mut arms = Arms::default();
        let mut w = super::Watchers::new(BASE);
        w.tick(&mounts(&["docs/a", "docs/b"]), now, &mut |p| arms.arm(p));
        for (_, state) in &arms.0 {
            state.finished.set(true);
        }
        w.shutdown();
        assert!(arms
            .0
            .iter()
            .all(|(_, s)| s.stopped.get() && s.joined.get()));
    }

    /// A pass that fails outright is turned into a retry the watcher owes, never an error that
    /// leaves the loop: `run_pass` has no way to return one, and its answer must be "owed".
    #[test]
    fn a_pass_that_fails_outright_owes_a_retry() {
        let owed = super::run_pass("docs/a", || {
            Err(crate::Error::Types(jkb_types::Error::Validation(
                "unknown serializer".to_owned(),
            )))
        });
        assert!(
            owed,
            "a failed pass owed no retry; that mount would never be re-scanned"
        );
    }

    /// A debt nothing owes is never due, however long the watcher idles.
    #[test]
    fn no_debt_is_never_due() {
        let debt = RetryDebt::new(false, Duration::ZERO);
        assert!(!debt.due(), "a mount with no failed pass asked for a retry");
    }
}
