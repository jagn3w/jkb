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
use std::time::Duration;

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
/// Returns an error if the watcher cannot be created/armed, or a reconcile fails.
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

    // Reconcile drift accumulated while not watching.
    report_refusals(mount_ns, &engine::sync(db, mount_ns)?);

    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(debounce) {
            Ok(first) => {
                let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
                let mut rescan = collect(first, &mut paths);
                // Coalesce: keep draining until the filesystem is quiet for `debounce`.
                while let Ok(next) = rx.recv_timeout(debounce) {
                    rescan |= collect(next, &mut paths);
                }
                let report = if rescan {
                    // Fell behind (watcher error / dropped events): re-scan everything.
                    Some(engine::sync(db, mount_ns)?)
                } else if paths.is_empty() {
                    None
                } else {
                    let paths: Vec<PathBuf> = paths.into_iter().collect();
                    Some(engine::sync_paths(db, mount_ns, &paths)?)
                };
                if let Some(report) = report {
                    report_refusals(mount_ns, &report);
                }
            }
            Err(RecvTimeoutError::Timeout) => {} // idle tick — re-check the stop flag
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

/// Say, on stderr, which files this reconcile refused — **or silently resolved against**.
///
/// The watcher used to discard every `SyncReport`, so a refusal it could not journal was
/// invisible on every surface at once. A collision between files that have never synced
/// writes no journal row on purpose — inventing one would give a file a base-less entry that
/// `reconcile` then reads as "both sides changed" — so `jkb doctor`, which reads the journal,
/// says the mount is healthy while none of its files is syncing. The report is the only
/// remaining place that fact exists; printing it is what makes it observable.
///
/// stderr, because this is a long-running service whose stdout is a log file: a refusal is a
/// diagnostic, not output.
fn report_refusals(mount_ns: &str, report: &engine::SyncReport) {
    // A `disk_wins`/`kb_wins` resolution DISCARDS one side's edits. The one-shot CLI prints
    // it per file precisely so that is visible when it happens; under the installed watcher
    // service the report was dropped, and `jkb doctor` cannot help — a resolution settles the
    // journal to `ok`, so `needs_attention` returns nothing. Every surface was silent about
    // the one outcome that throws work away.
    for (path, how) in report.resolved() {
        eprintln!("jkb sync [{mount_ns}]: RESOLVED {} — {how}", path.display());
    }
    // The engine's own reason, not a restatement: `Collided` covers both "a live sibling
    // shares this namespace" (exclude one) and "the namespace's layout belongs to a file that
    // is gone" (edit this one), and a fixed string told half of them to exclude a file that
    // does not exist.
    for (path, reason) in report.refusals() {
        eprintln!(
            "jkb sync [{mount_ns}]: REFUSED {} — {reason}",
            path.display()
        );
    }
    for path in report.quarantined() {
        eprintln!(
            "jkb sync [{mount_ns}]: needs attention (parse failed) {}",
            path.display()
        );
    }
    for path in report.conflicts() {
        eprintln!(
            "jkb sync [{mount_ns}]: CONFLICT {} — both sides changed the same item and the \
             policy is `manual`; nothing was written",
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
/// Returns the first watcher/reconcile error, or a validation error if a watch thread
/// panics.
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
            watch(&db, &path, debounce, &stop)
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
