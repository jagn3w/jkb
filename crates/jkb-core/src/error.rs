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

    /// The background writer thread has stopped, so the request cannot be served.
    #[error("database writer has stopped")]
    WriterClosed,
}

/// Convenience alias: `Result<T>` is `Result<T, jkb_core::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
