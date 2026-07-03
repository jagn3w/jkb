//! `jkb-index`'s error type.
//!
//! Wraps `rusqlite` failures and the shared [`jkb_types::Error`] (which carries
//! embedder/validation errors surfaced through the `Embedder` trait during a
//! rebuild). The binary edge collapses these into user-facing messages.

use thiserror::Error;

/// Errors surfaced by `jkb-index`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A `SQLite`/`rusqlite` failure (including `sqlite-vec` SQL).
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A shared-vocabulary error from [`jkb_types`] (e.g. an embedder failure or a
    /// dim/model-compatibility rejection).
    #[error(transparent)]
    Types(#[from] jkb_types::Error),
}

/// Convenience alias: `Result<T>` is `Result<T, jkb_index::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
