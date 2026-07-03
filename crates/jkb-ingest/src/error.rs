//! `jkb-ingest`'s error type — the crate where `jkb-core` and `jkb-index` meet.
//!
//! Ingestion writes items via `jkb-core` repos and vectors via `jkb-index`, so its
//! error absorbs both (there is no cross-`From` between those two crates). It also
//! wraps `rusqlite` (its own idempotency/blob SQL), `jkb_types` (embedder failures),
//! and I/O (reading source files).

use thiserror::Error;

/// Errors surfaced by `jkb-ingest`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A `jkb-core` failure (repos, transaction machinery).
    #[error(transparent)]
    Core(#[from] jkb_core::Error),

    /// A `jkb-index` failure (vector index / catalog).
    #[error(transparent)]
    Index(#[from] jkb_index::Error),

    /// A shared-vocabulary error (e.g. an embedder failure or validation).
    #[error(transparent)]
    Types(#[from] jkb_types::Error),

    /// A `SQLite` failure from ingest's own SQL (`ingestions`, `blobs`).
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Reading a source file failed.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    /// No adapter handles the given source.
    #[error("unsupported source: {0}")]
    Unsupported(String),

    /// Fetching/rendering a URL failed (headless browser launch, navigation, or
    /// content read).
    #[error("fetch: {0}")]
    Fetch(String),
}

/// Convenience alias: `Result<T>` is `Result<T, jkb_ingest::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
