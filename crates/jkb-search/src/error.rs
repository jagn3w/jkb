//! `jkb-search`'s error type — the crate where `jkb-core`'s query engine and
//! `jkb-index`'s indexers meet.
//!
//! Search evaluates a candidate set via `jkb-core` and ranks it via `jkb-index`
//! (plus its own provenance/context SQL), so its error absorbs both (there is no
//! cross-`From` between those two crates). It also wraps `jkb_types` (embedder
//! failures on the query text) and `rusqlite` (its own read SQL). Mirrors
//! `jkb-ingest`'s bridging error.

use thiserror::Error;

/// Errors surfaced by `jkb-search`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A `jkb-core` failure (query evaluation, namespace/placement reads).
    #[error(transparent)]
    Core(#[from] jkb_core::Error),

    /// A `jkb-index` failure (vector KNN / exact scoring / FTS search).
    #[error(transparent)]
    Index(#[from] jkb_index::Error),

    /// A shared-vocabulary error (e.g. embedding the query text failed).
    #[error(transparent)]
    Types(#[from] jkb_types::Error),

    /// A `SQLite` failure from search's own SQL (provenance, context-expansion).
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Convenience alias: `Result<T>` is `Result<T, jkb_search::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
