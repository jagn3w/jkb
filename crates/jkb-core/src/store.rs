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

    /// Checkpoint the WAL (`TRUNCATE`) and copy the database file to `dest`
    /// (design D8/D23). Runs on the writer thread, so no write is in flight during
    /// the copy and the destination reflects all committed state.
    ///
    /// # Errors
    /// Returns an error for an in-memory database, or if the checkpoint or copy
    /// fails.
    pub fn backup(&self, dest: impl AsRef<Path>) -> Result<()> {
        let src = self.path.clone().ok_or_else(|| {
            jkb_types::Error::Validation("cannot back up an in-memory database".to_owned())
        })?;
        let dest = dest.as_ref().to_path_buf();
        self.submit(move |conn| -> Result<()> {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            std::fs::copy(&src, &dest)?;
            Ok(())
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::Db;
    use crate::item::{upsert, NewItem};

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
