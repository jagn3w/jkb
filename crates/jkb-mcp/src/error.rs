//! `jkb-mcp`'s error type: bridges the library errors the tools touch. The server
//! maps these into MCP `ErrorData` (user-input errors → `invalid_params`, everything
//! else → `internal_error`).

use thiserror::Error;

/// Errors surfaced by the MCP tool logic.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A `jkb-core` failure (database, repositories, transactions).
    #[error(transparent)]
    Core(#[from] jkb_core::Error),

    /// A search failure.
    #[error(transparent)]
    Search(#[from] jkb_search::Error),

    /// An ingestion failure.
    #[error(transparent)]
    Ingest(#[from] jkb_ingest::Error),

    /// A shared-vocabulary error (validation, not-found, …).
    #[error(transparent)]
    Types(#[from] jkb_types::Error),

    /// A JSON (de)serialization failure.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Whether this is a client-input error (validation / not-found), so the server
    /// can report `invalid_params` rather than `internal_error`.
    #[must_use]
    pub fn is_user_error(&self) -> bool {
        matches!(
            self,
            Error::Types(jkb_types::Error::Validation(_) | jkb_types::Error::NotFound(_))
                | Error::Core(jkb_core::Error::Types(
                    jkb_types::Error::Validation(_) | jkb_types::Error::NotFound(_)
                ))
        )
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;
