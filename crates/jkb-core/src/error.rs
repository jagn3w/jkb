//! `jkb-core`'s error type.
//!
//! Wraps the external failures core touches (`rusqlite`, `refinery`) and the
//! shared [`jkb_types::Error`]. Each library crate owns an error like this; the
//! binary edge collapses them into user-facing messages with `anyhow`.

use thiserror::Error;

/// Errors surfaced by `jkb-core`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A `SQLite`/`rusqlite` failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A schema migration failed to apply.
    #[error("migration: {0}")]
    Migration(#[from] refinery::Error),

    /// A shared-vocabulary error from [`jkb_types`].
    #[error(transparent)]
    Types(#[from] jkb_types::Error),

    /// A filesystem error (e.g. during backup).
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    /// A message-queue operation was refused ([`crate::mq`]).
    #[error(transparent)]
    Queue(#[from] crate::mq::QueueError),

    /// The background writer thread has stopped, so the request cannot be served.
    #[error("database writer has stopped")]
    WriterClosed,

    /// The database would live on a filesystem shared with another kernel, where `SQLite`'s locks
    /// and wal-index do not hold (design r3.2 H1).
    #[error(
        "refusing to open a database: {} is on a {kind} filesystem shared with another kernel, \
         where SQLite's locks and WAL index do not work: measured, a process on each side \
         corrupted a database within 38 commits (.container/sqlite-share-probe.py). Use a database \
         on a local disk (set JKB_DB or --db), or reach the host's knowledge base through its daemon",
        path.display()
    )]
    SharedFilesystem {
        /// The path whose filesystem was judged: the directory that would hold the database, or
        /// the database file or one of its `-wal`/`-shm`/`-journal` siblings.
        path: std::path::PathBuf,
        /// Which shared filesystem it is.
        kind: &'static str,
    },

    /// The database path is a `file:` URI, which the bundled `SQLite` parses as a URI whatever the
    /// open's flags — so no path guard can judge what it names (design r3.2 H1).
    #[error(
        "refusing to open {}: jkb opens database PATHS, never `file:` URIs (SQLite would parse it as \
         a URI and open whatever it names, past the shared-filesystem guard)",
        path.display()
    )]
    UriPath {
        /// The rejected string.
        path: std::path::PathBuf,
    },

    /// Whether the database's directory is on a shared filesystem could not be established.
    #[error("cannot tell what filesystem {} is on ({reason}); refusing to open a database there", path.display())]
    FilesystemUnknown {
        /// The path that was asked about: the database path as given, or one resolved from it.
        path: std::path::PathBuf,
        /// Why `statfs` failed.
        reason: String,
    },

    /// A schema migration left the database with dangling foreign-key references
    /// (detected by the post-migration `foreign_key_check`).
    #[error("migration left {0} foreign-key violation(s); database integrity check failed")]
    ForeignKeyViolation(usize),
}

/// Convenience alias: `Result<T>` is `Result<T, jkb_core::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
