//! `jkb-mcp`'s error type: bridges the errors the tools meet. The server maps these into MCP
//! `ErrorData` (a request the tool could not serve as asked → `invalid_params`, everything else →
//! `internal_error`).

use thiserror::Error;

/// Errors surfaced by the MCP tool logic.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// An operation's refusal or failure, from whichever backend serves the tools.
    #[error("{}", .0.message)]
    Api(jkb_api::ApiError),

    /// Reading a source to ingest.
    #[error(transparent)]
    Ingest(#[from] jkb_ingest::Error),

    /// A shared-vocabulary error (validation, not-found, …).
    #[error(transparent)]
    Types(#[from] jkb_types::Error),

    /// A source of the wrong kind for the tool asked.
    #[error("{0}")]
    Source(String),

    /// An answer this build did not expect.
    #[error("internal: {0}")]
    Unexpected(String),
}

impl From<jkb_api::ApiError> for Error {
    fn from(e: jkb_api::ApiError) -> Self {
        Self::Api(e)
    }
}

impl Error {
    /// Whether this is a client-input error, so the server can report `invalid_params` rather than
    /// `internal_error`.
    #[must_use]
    pub fn is_user_error(&self) -> bool {
        use jkb_api::ErrorCode;
        match self {
            Self::Api(e) => matches!(
                e.code,
                ErrorCode::Invalid
                    | ErrorCode::BadRequest
                    | ErrorCode::NotFound
                    | ErrorCode::Forbidden
                    | ErrorCode::Unsupported
                    | ErrorCode::TooLarge
            ),
            Self::Types(jkb_types::Error::Validation(_) | jkb_types::Error::NotFound(_))
            | Self::Source(_) => true,
            // The source named: missing, unreadable as asked, of no kind jkb reads, or a page that would
            // not load.
            Self::Ingest(e) => {
                matches!(
                    e,
                    jkb_ingest::Error::Unsupported(_)
                        | jkb_ingest::Error::Fetch(_)
                        | jkb_ingest::Error::Types(jkb_types::Error::Validation(_))
                ) || matches!(e, jkb_ingest::Error::Io(io) if matches!(
                    io.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::InvalidInput
                        | std::io::ErrorKind::InvalidData
                        | std::io::ErrorKind::IsADirectory
                ))
            }
            _ => false,
        }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;
