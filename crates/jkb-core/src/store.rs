//! The single-writer actor and the [`Db`] handle.
//!
//! `SQLite` allows many readers but only one writer. Rather than juggle locks, all
//! database access goes through one background thread that owns the write
//! `Connection` (design D8). Writes are therefore serialized — callers never see
//! `SQLITE_BUSY` — and no `unsafe` or lock management is needed.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use rusqlite::Connection;

use crate::{db, Error, Result};

/// If `path` looks like it lives inside a known cloud-sync folder (see the marker
/// list in the body), return a warning. Running a live database from such a folder
/// can corrupt it (design D23).
#[must_use]
pub fn cloud_sync_warning(path: &Path) -> Option<String> {
    const MARKERS: &[&str] = &[
        "Dropbox",
        "CloudStorage",
        "iCloud",
        "Google Drive",
        "OneDrive",
    ];
    let text = path.to_string_lossy();
    for marker in MARKERS {
        if text.contains(*marker) {
            return Some(format!(
                "database path appears to be inside a cloud-sync folder ({marker}); \
                 running a live database there can corrupt it"
            ));
        }
    }
    None
}

/// A unit of work to run on the writer thread. The result is delivered back to
/// the caller through a channel the closure captures.
type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// Metadata for a write transaction: the `txn_id` groups all changelog entries of
/// one logical change (so it can be undone as a unit) and records who made it.
pub struct WriteMeta {
    /// Monotonic id grouping this transaction's changelog entries.
    pub txn_id: i64,
    /// Who initiated the change (e.g. `"cli"`, `"mcp"`).
    pub actor: String,
}

/// Registers a statically-linked `SQLite` extension so connections opened
/// *afterwards* gain it (the shape of `sqlite3_auto_extension`).
///
/// `jkb-core` owns *when* extensions are registered — before it opens a connection —
/// because it owns the connection lifecycle (design D15: core owns
/// connection/extension/PRAGMA setup). The extension crate owns the actual (often
/// `unsafe`) FFI: e.g. pass `jkb_index::register` for `sqlite-vec`. This mirrors the
/// `Embedder` seam (trait in `jkb-types`, impls in `jkb-embed`): core defines the
/// *when*, the plugin crate provides the *what*.
pub type ExtensionRegistrar = fn();

/// A cheap-to-clone handle to the database.
///
/// Cloning clones a channel sender; every clone talks to the same writer thread.
/// The worker exits when the last `Db` clone is dropped.
#[derive(Clone)]
pub struct Db {
    tx: mpsc::Sender<Job>,
    /// The database file path, or `None` for an in-memory database.
    path: Option<PathBuf>,
}

impl Db {
    /// Open (or create) a database at `path` and start its writer thread.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened, configured, or migrated.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with(path, &[])
    }

    /// Open (or create) a database at `path`, first running each extension
    /// registrar so the connection this opens (and any opened later in the process)
    /// gains those extensions. Use this to enable `sqlite-vec`:
    /// `Db::open_with(path, &[jkb_index::register])`.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened, configured, or migrated.
    pub fn open_with<P: AsRef<Path>>(path: P, extensions: &[ExtensionRegistrar]) -> Result<Self> {
        for &register in extensions {
            register();
        }
        let path = path.as_ref().to_path_buf();
        let conn = db::open(&path)?;
        Ok(Self::from_connection(conn, Some(path)))
    }

    /// Open a fresh in-memory database with its own writer thread (for tests).
    ///
    /// # Errors
    /// Returns an error if configuration or migration fails.
    pub fn open_in_memory() -> Result<Self> {
        Self::open_in_memory_with(&[])
    }

    /// Open a fresh in-memory database, first running each extension registrar (see
    /// [`Db::open_with`]).
    ///
    /// # Errors
    /// Returns an error if configuration or migration fails.
    pub fn open_in_memory_with(extensions: &[ExtensionRegistrar]) -> Result<Self> {
        for &register in extensions {
            register();
        }
        Ok(Self::from_connection(db::open_in_memory()?, None))
    }

    fn from_connection(mut conn: Connection, path: Option<PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        thread::spawn(move || {
            // Runs until every sender (every `Db` clone) is dropped.
            while let Ok(job) = rx.recv() {
                job(&mut conn);
            }
        });
        Self { tx, path }
    }

    /// Hand `f` to the writer thread and block until it returns.
    fn submit<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Connection) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (result_tx, result_rx) = mpsc::channel();
        let job: Job = Box::new(move |conn| {
            // The receiver may be gone if the caller was cancelled; ignore that.
            let _ = result_tx.send(f(conn));
        });
        self.tx.send(job).map_err(|_| Error::WriterClosed)?;
        result_rx.recv().map_err(|_| Error::WriterClosed)
    }

    /// Run a read against the database.
    ///
    /// # Errors
    /// Propagates any error from `f`, or [`Error::WriterClosed`] if the writer
    /// thread has stopped.
    pub fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.read_with(f)
    }

    /// Like [`Db::read`] but generic over the closure's error type `E` (any error
    /// that can absorb a [`Error`]). Lets a caller crate that straddles the
    /// `jkb-core`/`jkb-index` boundary — e.g. `jkb-ingest` — use its own error type
    /// inside the closure and `?` across both.
    ///
    /// # Errors
    /// Propagates any error from `f`, or [`Error::WriterClosed`] (as `E`).
    pub fn read_with<T, E, F>(&self, f: F) -> std::result::Result<T, E>
    where
        F: FnOnce(&Connection) -> std::result::Result<T, E> + Send + 'static,
        E: From<Error> + Send + 'static,
        T: Send + 'static,
    {
        self.submit(move |conn| f(conn))?
    }

    /// Run `f` inside one write transaction. A fresh `txn_id` is allocated and
    /// passed via [`WriteMeta`]; the transaction commits iff `f` returns `Ok`.
    ///
    /// # Errors
    /// Propagates any error from `f` or the transaction machinery.
    pub fn write_txn<T, F>(&self, actor: impl Into<String>, f: F) -> Result<T>
    where
        F: FnOnce(&Connection, &WriteMeta) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.write_txn_with(actor, f)
    }

    /// Like [`Db::write_txn`] but generic over the closure's error type `E` (see
    /// [`Db::read_with`]). The transaction machinery's own errors surface as `E` via
    /// `E: From<Error>`.
    ///
    /// # Errors
    /// Propagates any error from `f` or the transaction machinery.
    pub fn write_txn_with<T, E, F>(
        &self,
        actor: impl Into<String>,
        f: F,
    ) -> std::result::Result<T, E>
    where
        F: FnOnce(&Connection, &WriteMeta) -> std::result::Result<T, E> + Send + 'static,
        E: From<Error> + Send + 'static,
        T: Send + 'static,
    {
        let actor = actor.into();
        self.submit(move |conn| -> std::result::Result<T, E> {
            // IMMEDIATE, not the default deferred. A deferred transaction whose first
            // statement is a read takes a WAL read snapshot and only upgrades on its first
            // write — and if another process committed in between, SQLite returns
            // `SQLITE_BUSY_SNAPSHOT`, for which it does **not** invoke the busy handler, so
            // `busy_timeout` never applies. Every machine `scripts/setup.sh` sets up runs a
            // `jkb sync --watch` service alongside interactive commands, so that race is
            // ordinary here — and since the sync engine now journals a failed reconcile as
            // `needs_attention`, losing it recorded transient contention as a durable per-file
            // defect. Taking the write lock up front makes contention wait instead.
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(Error::from)?;
            let txn_id: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(txn_id), 0) + 1 FROM changelog",
                    [],
                    |row| row.get(0),
                )
                .map_err(Error::from)?;
            let meta = WriteMeta { txn_id, actor };
            let conn_ref: &Connection = &tx;
            let out = f(conn_ref, &meta)?;
            tx.commit().map_err(Error::from)?;
            Ok(out)
        })?
    }

    /// Write a consistent copy of the database to `dest`, replacing it if it exists
    /// (design D8/D23). Runs on the writer thread, so no write is in flight during
    /// the copy and the destination reflects all committed state.
    ///
    /// # Errors
    /// Returns an error for an in-memory database, if `dest` resolves to the live database or
    /// one of its `-wal`/`-shm` siblings, if the vacuum fails, or if the finished backup cannot
    /// be renamed over `dest`.
    pub fn backup(&self, dest: impl AsRef<Path>) -> Result<()> {
        let Some(src) = self.path.clone() else {
            return Err(jkb_types::Error::Validation(
                "cannot back up an in-memory database".to_owned(),
            )
            .into());
        };
        let dest = dest.as_ref().to_path_buf();

        // Refuse to write over the live database, its `-wal`/`-shm` siblings included.
        //
        // `VACUUM INTO` refused any destination that already existed, which incidentally made
        // `jkb doctor --backup ~/.jkb/jkb.db` — one tab-completion away — impossible. Replacing
        // it with temp-and-rename so repeat backups work dropped that guarantee: the rename
        // would replace the open database file while this process's connection still held the
        // old inode, sending later commits to an unlinked file and leaving a foreign `-wal`
        // beside the new one, while printing "backup written" and exiting 0.
        //
        // Replacing a mechanism means inheriting the preconditions it was quietly enforcing.
        //
        // Both sides are resolved the same way — canonicalized when the file exists, raw
        // otherwise — because comparing a canonicalized path against a raw one passes for
        // exactly the case this refuses.
        let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let dest_resolved = resolve(&dest);
        for suffix in ["", "-wal", "-shm"] {
            let mut guarded = src.clone().into_os_string();
            guarded.push(suffix);
            if dest_resolved == resolve(Path::new(&guarded)) {
                return Err(jkb_types::Error::Validation(format!(
                    "refusing to back up over the live database at {} — pick another destination",
                    dest.display()
                ))
                .into());
            }
        }
        self.submit(move |conn| -> Result<()> {
            // `VACUUM INTO`, not checkpoint-then-copy.
            //
            // The old form ran `PRAGMA wal_checkpoint(TRUNCATE)` and then `fs::copy`d the main
            // database file alone. A checkpoint reports failure as a **row** with `busy = 1`
            // rather than an error, and `execute_batch` steps past rows — so a busy checkpoint
            // was silently ignored and the copy omitted every WAL frame not yet transferred.
            // The result opens cleanly and is missing recent commits, which is the worst
            // possible shape for a backup. Taking the write lock up front (IMMEDIATE) widened
            // that window, since a peer now holds it for a whole transaction.
            //
            // `VACUUM INTO` writes a complete, consistent database in one statement, reads
            // through the WAL rather than depending on it being drained, and errors instead of
            // half-succeeding.
            // `VACUUM INTO` refuses a destination that already exists, and `jkb doctor --backup
            // ~/.jkb/backup.db` — a fixed path, which is what the flag invites and what any cron
            // or pre-migration script uses — must keep working on the second run. `fs::copy`
            // overwrote, so inheriting the no-clobber rule would break every backup after the
            // first. Vacuum to a sibling temp file and rename over the destination: the rename is
            // atomic, so a failure part-way leaves the previous backup intact rather than a
            // truncated one.
            // The sibling name is UNIQUE per attempt, so two backups never write one file.
            //
            // A shared `<dest>.tmp` was tried first, guarded with `create_new` so a second
            // backup would be refused. That guard was inert: `VACUUM INTO` insists on creating
            // the file itself, so the claim had to be unlinked one line after it was taken, and
            // the `exists()` reclaim above it deleted a *live* claim rather than a stale one —
            // it excluded nothing in either direction. Exclusion was also the wrong goal.
            // Concurrent backups writing separate temp files each produce a complete, valid
            // database and the later rename simply wins; nobody can observe the half-written
            // file the shared path risked, and there is no lock to go stale and block every
            // future backup after a crash.
            //
            // The cost, stated precisely so it is not mistaken for a smaller one: a process
            // killed between the vacuum and the rename strands a full-size copy of the database,
            // and no later backup reclaims it — the fixed name was reclaimed by the next run.
            // Every other failure path cleans up (pinned by `a_failed_backup_cleans_up_its_temp
            // _file`), so the litter is bounded to SIGKILL and power loss, once per such event.
            //
            // It is deliberately not reclaimed. Every rule for deleting somebody else's temp
            // file is a liveness test — is that pid alive, is that file old enough — and the
            // fixed-name design already showed where that leads: its reclaim deleted a
            // *concurrent* backup's live file, which is the corruption this exists to prevent.
            // An age rule fails the same way on a slow vacuum over a large database. Naming it
            // `<dest>.<pid>.<n>.tmp` makes a stray recognizable and safe for a human to remove,
            // which is the most that can be done without guessing about another process.
            //
            // The pid alone is not enough: `Db` clones cheaply, so two threads of one process
            // can back up at once. An in-process counter separates those.
            //
            // APPENDED rather than `with_extension`, which replaces an extension and would
            // mangle a dated destination like `backup.db.2026-08-09`.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let mut tmp = dest.clone().into_os_string();
            tmp.push(format!(
                ".{}.{}.tmp",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let tmp = PathBuf::from(tmp);
            let tmp_sql = tmp.to_string_lossy().into_owned();
            // Cleaned up on every failure path, the rename included.
            let result = conn
                .execute("VACUUM INTO ?1", [&tmp_sql])
                .map_err(crate::Error::from)
                .and_then(|_| std::fs::rename(&tmp, &dest).map_err(crate::Error::from));
            if result.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            result
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::Db;
    use crate::item::{upsert, NewItem};

    /// A backup to the same path twice must work — `jkb doctor --backup ~/.jkb/backup.db` is a
    /// fixed path, which is what the flag invites and what any cron or pre-migration script
    /// uses. `VACUUM INTO` refuses an existing destination, so inheriting its no-clobber rule
    /// would have broken every backup after the first.
    /// Two backups to the SAME destination at once, which is the scenario the unique temp name
    /// exists for. One `Db` cannot exercise it — `backup` runs on the writer thread, so two
    /// calls through one handle serialize — so this opens two handles, as two processes would
    /// (a cron backup meeting a manual one).
    ///
    /// The previous design vacuumed both to one shared `<dest>.tmp`, and whichever finished
    /// first renamed the OTHER's half-written file over the destination and reported success.
    /// The `create_new` claim meant to stop that was inert: `VACUUM INTO` insists on creating
    /// its own file, so the claim was unlinked one line after being taken. With a unique name
    /// per attempt each writes its own complete database and the later rename simply wins, so
    /// the destination is always a whole database, never a torn one.
    #[test]
    fn two_concurrent_backups_each_leave_a_whole_database() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("jkb.db");
        let db = Db::open(&src).unwrap();
        db.write_txn("t", |conn, meta| {
            upsert(
                conn,
                meta,
                &NewItem {
                    uid: "a".to_owned(),
                    kind: "note".to_owned(),
                    content: Some("hi".to_owned()),
                    content_hash: None,
                    mime: None,
                },
            )
            .map(|_| ())
        })
        .unwrap();

        let dest = dir.path().join("backup.db");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                // A separate handle, hence a separate writer thread and connection.
                let db = Db::open(&src).unwrap();
                let dest = dest.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    db.backup(&dest)
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().expect("a concurrent backup failed");
        }

        // Whoever won, the destination is a complete, readable database carrying the row.
        let restored = Db::open(&dest).unwrap();
        let n: i64 = restored
            .read(|conn| Ok(conn.query_row("SELECT count(*) FROM items", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1, "the surviving backup is not a whole database");
        assert!(strays(dir.path()).is_empty(), "temp files were left behind");
    }

    /// A backup that fails after the vacuum must not strand its temp file. Renaming onto an
    /// existing directory is the failure the OS supplies for free; the point is the cleanup
    /// path, which is what bounds the litter to hard kills.
    #[test]
    fn a_failed_backup_cleans_up_its_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("jkb.db");
        let db = Db::open(&src).unwrap();

        // A directory cannot be replaced by a rename, so the vacuum succeeds and the rename does
        // not — exercising the arm between them.
        let dest = dir.path().join("backup.db");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("occupant"), b"x").unwrap();

        assert!(
            db.backup(&dest).is_err(),
            "backing up onto a directory should fail"
        );
        assert!(
            strays(dir.path()).is_empty(),
            "a failed backup stranded its temp file: {:?}",
            strays(dir.path())
        );
    }

    /// Every `<dest>.<pid>.<n>.tmp` sibling in `dir`. Litter is bounded to hard kills, so any
    /// stray after a completed call is a cleanup bug.
    fn strays(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| {
                let name = e.ok()?.file_name().to_string_lossy().into_owned();
                let is_temp = std::path::Path::new(&name)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("tmp"));
                (name.starts_with("backup.db.") && is_temp).then_some(name)
            })
            .collect()
    }

    #[test]
    fn backup_overwrites_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("jkb.db")).unwrap();
        db.write_txn("t", |conn, meta| {
            upsert(
                conn,
                meta,
                &NewItem {
                    uid: "n:1".to_owned(),
                    kind: "note".to_owned(),
                    content: Some("body".to_owned()),
                    content_hash: None,
                    mime: None,
                },
            )
            .map(|_| ())
        })
        .unwrap();

        let dest = dir.path().join("backup.db");
        db.backup(&dest).expect("first backup");
        db.backup(&dest).expect("second backup to the same path");

        // A leftover temp file from a killed run must never BLOCK a backup. It is deliberately
        // no longer reclaimed: temp names are unique per attempt, so nothing can safely delete
        // one (the reclaim that a fixed name allowed was deleting a *concurrent* backup's live
        // file, not a dead one). Litter is the accepted cost; a wedged backup is not.
        let stale = dir.path().join("backup.db.31337.0.tmp");
        std::fs::write(&stale, b"leftover from a killed backup").unwrap();
        db.backup(&dest)
            .expect("a stale temp file must not block a backup");

        // And a successful backup leaves no temp of its OWN behind — the property that actually
        // bounds the litter to crashes.
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| {
                let name = e.ok()?.file_name().to_string_lossy().into_owned();
                let is_temp = std::path::Path::new(&name)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("tmp"));
                (name.starts_with("backup.db.") && is_temp && name != "backup.db.31337.0.tmp")
                    .then_some(name)
            })
            .collect();
        assert!(
            strays.is_empty(),
            "a successful backup left its own temp file behind: {strays:?}"
        );

        // The backup is a real, readable database carrying the row.
        let restored = Db::open(&dest).unwrap();
        let n: i64 = restored
            .read(|conn| Ok(conn.query_row("SELECT count(*) FROM items", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1, "the backup must contain the committed row");
        assert!(
            !dir.path().join("backup.db.tmp").exists(),
            "the temp file must not survive a successful backup"
        );
    }

    /// Backing up ONTO the live database must be refused. `VACUUM INTO` used to make this
    /// impossible by refusing any existing destination; temp-and-rename removed that, so the
    /// precondition has to be stated rather than inherited.
    #[test]
    fn backup_refuses_to_overwrite_the_live_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jkb.db");
        let db = Db::open(&path).unwrap();
        for dest in [path.clone(), dir.path().join("jkb.db-wal")] {
            let err = db.backup(&dest).unwrap_err().to_string();
            assert!(
                err.contains("live database"),
                "must refuse {}: {err}",
                dest.display()
            );
        }
        assert!(path.exists(), "the database must still be there");
    }

    #[test]
    fn concurrent_writers_never_hit_sqlite_busy() {
        let db = Db::open_in_memory().unwrap();
        let mut handles = Vec::new();
        for writer in 0..8 {
            let db = db.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..50 {
                    let uid = format!("w{writer}:i{i}");
                    db.write_txn("test", move |conn, meta| {
                        upsert(
                            conn,
                            meta,
                            &NewItem {
                                uid,
                                kind: "note".to_owned(),
                                content: Some("x".to_owned()),
                                content_hash: None,
                                mime: None,
                            },
                        )
                    })
                    .expect("write should succeed via the serialized writer");
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let count = db
            .read(|conn| {
                Ok(conn.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))?)
            })
            .unwrap();
        assert_eq!(count, 400);
    }

    #[test]
    fn backup_writes_a_readable_copy() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("jkb.db");
        let db = Db::open(&src).unwrap();
        db.write_txn("t", |conn, meta| {
            upsert(
                conn,
                meta,
                &NewItem {
                    uid: "a".to_owned(),
                    kind: "note".to_owned(),
                    content: Some("hi".to_owned()),
                    content_hash: None,
                    mime: None,
                },
            )
        })
        .unwrap();

        let dest = dir.path().join("backup.db");
        db.backup(&dest).unwrap();

        let copy = Db::open(&dest).unwrap();
        let count = copy
            .read(|conn| {
                Ok(conn.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))?)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn backup_of_in_memory_is_rejected() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.backup("unused.db").is_err());
    }

    #[test]
    fn cloud_sync_warning_flags_sync_folders() {
        use std::path::Path;
        assert!(super::cloud_sync_warning(Path::new("/Users/x/Dropbox/jkb/jkb.db")).is_some());
        assert!(super::cloud_sync_warning(Path::new("/Users/x/repos/jkb/jkb.db")).is_none());
    }
}
