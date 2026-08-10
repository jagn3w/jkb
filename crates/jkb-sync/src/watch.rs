//! Filesystem watch mode: react to changes under a mount and reconcile, debouncing
//! bursts of events (editors save via several syscalls) so one logical edit triggers
//! one reconcile. This is the only async/blocking edge in `jkb-sync`.
//!
//! Events carry the paths that changed, so a burst reconciles just those files via
//! [`crate::sync_paths`] rather than re-scanning the whole mount — important once a
//! mount backs a large tree. Only two situations fall back to a full [`crate::sync`]:
//! the initial reconcile on startup (to catch drift from while the watcher was off),
//! and a watcher error or dropped-events signal (`need_rescan`), where we can no
//! longer trust the incremental path list.
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

use jkb_core::{mount, Db};

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
    let mut watcher = notify::recommended_watcher(move |res| {
        // A closed receiver just means we're shutting down; ignore the send error.
        let _ = tx.send(res);
    })?;
    // Recursive: the OS only lets us subscribe to a directory subtree, not a glob, so
    // relevance filtering happens in `sync_paths` against the mount's include/exclude.
    watcher.watch(&dir, RecursiveMode::Recursive)?;

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
                // Coalesce: keep draining until the filesystem is quiet for `debounce`.
                while let Ok(next) = rx.recv_timeout(debounce) {
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

/// Run one reconcile pass, reporting whatever happens. **Never returns an error.**
///
/// The watcher's unit of failure is a pass, not the thread. Making the per-file reconcile
/// non-fatal was not enough: `outcome_reason`, `settle_out_of_scope` and the trailing
/// `ensure_all_mirrors` transaction still propagate out of `sync`/`sync_paths`, and a single
/// `Err` here used to exit this mount's thread for good — `watch_all` does not set `stop`, so the
/// process stayed alive joining the others, launchd never restarted it, and that mount silently
/// stopped syncing.
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
            // A pass can fail AFTER per-file transactions have committed — the trailing
            // `ensure_all_mirrors` is its own transaction — so the batch's remaining paths and
            // the mirror derivation are lost while the files read as settled. Returning `true`
            // makes the next tick a full re-scan, so the work is picked up rather than waiting
            // for someone to touch those files again.
            eprintln!("sync {mount_ns}: pass failed ({e}); re-scanning on the next event");
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
/// (`jkb sync --watch` with no namespace). Returns once all watchers have stopped;
/// if several fail, the first error is returned.
///
/// # Errors
/// Returns the first watcher **startup** error, or a validation error if a watch thread panics.
/// Reconcile failures never reach here; each thread reports its own as it goes.
pub fn watch_all(db: &Db, debounce: Duration, stop: &Arc<AtomicBool>) -> Result<()> {
    let paths = db.read(mount::all_paths)?;
    if paths.is_empty() {
        return Ok(());
    }

    let mut handles = Vec::with_capacity(paths.len());
    for path in paths {
        let db = db.clone();
        let stop = Arc::clone(stop);
        handles.push(std::thread::spawn(move || {
            let outcome = watch(&db, &path, debounce, &stop);
            if let Err(e) = &outcome {
                // Reported HERE, not at join time. A startup failure — a mount whose backing
                // directory has been moved or deleted — happens before the first pass, and
                // `watch_all` blocks joining the other threads until `stop`, which under
                // launchd is never. So without this the mount is simply never watched, prints
                // nothing, journals nothing, and `jkb doctor` says `sync journal: ok`.
                eprintln!("sync {path}: watcher stopped: {e}");
            }
            outcome
        }));
    }

    let mut result = Ok(());
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if result.is_ok() {
                    result = Err(e);
                }
            }
            Err(_) => {
                if result.is_ok() {
                    result = Err(Error::Types(jkb_types::Error::Validation(
                        "a watch thread panicked".to_owned(),
                    )));
                }
            }
        }
    }
    result
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

    /// A debt nothing owes is never due, however long the watcher idles.
    #[test]
    fn no_debt_is_never_due() {
        let debt = RetryDebt::new(false, Duration::ZERO);
        assert!(!debt.due(), "a mount with no failed pass asked for a retry");
    }
}
