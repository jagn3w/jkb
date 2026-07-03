//! The shared error type.
//!
//! Library crates return [`Error`] (built with `thiserror`); the binary edge
//! (`jkb-cli`) is where these are turned into user-facing messages with `anyhow`.

use thiserror::Error;

/// Errors surfaced by jkb library crates.
///
/// `#[non_exhaustive]` lets us add variants later without it being a breaking
/// change for downstream `match`es (they must include a `_ =>` arm).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Input failed validation (e.g. a malformed namespace path).
    #[error("validation: {0}")]
    Validation(String),

    /// A requested entity does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// The configured embedder backend is unreachable or missing its model.
    #[error("embedder unavailable: {0}")]
    EmbedderUnavailable(String),

    /// An underlying I/O failure.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience alias so crates write `Result<T>` instead of `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn display_is_human_readable() {
        let err = Error::Validation("empty segment".to_owned());
        assert_eq!(err.to_string(), "validation: empty segment");
    }

    #[test]
    fn io_error_converts_via_from() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        let err: Error = io.into();
        assert!(matches!(err, Error::Io(_)));
    }
}
