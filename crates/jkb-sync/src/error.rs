//! `jkb-sync`'s error type.
//!
//! Bridges the failures the sync engine touches — `jkb-core` (DB/repos), the shared
//! [`jkb_types::Error`], filesystem I/O, glob compilation, and the file watcher.
//! Conflicts are **not** errors: they are reported as an [`crate::Outcome`].

use thiserror::Error;

/// Errors surfaced by `jkb-sync`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A failure from `jkb-core` (database, repositories, transactions).
    #[error(transparent)]
    Core(#[from] jkb_core::Error),

    /// A shared-vocabulary error (validation, not-found, …).
    #[error(transparent)]
    Types(#[from] jkb_types::Error),

    /// A direct `SQLite` error from the engine's inline reconciliation queries.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A filesystem read/write failure while syncing a file.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    /// An include/exclude glob failed to compile.
    #[error("glob: {0}")]
    Glob(#[from] globset::Error),

    /// The filesystem watcher failed.
    #[error("watch: {0}")]
    Watch(#[from] notify::Error),
}

/// Convenience alias: `Result<T>` is `Result<T, jkb_sync::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
