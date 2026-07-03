//! Core trait seams.
//!
//! [`Embedder`] is defined here because its currency is just text and vectors, so
//! `jkb-ingest` and `jkb-search` can depend on the trait without pulling a heavy
//! ONNX/HTTP implementation (design D12/D15).
//!
//! `Indexer` is the exception: it operates on a `rusqlite::Connection`, so it lives
//! in `jkb-index` rather than here — pulling `SQLite` into this dependency-light
//! crate would drag it into `jkb-embed` too. The remaining seams — `SourceAdapter`
//! and `SyncSerializer` — will live here, added alongside the domain value types
//! they exchange (parsed documents, serialized units) in Sections 7/11; defining
//! their signatures now would mean inventing those types speculatively.

use crate::Result;

/// Produces embedding vectors for text.
///
/// Implementations may call a local server (ollama) or run in-process (fastembed);
/// callers depend only on this trait, chosen by configuration.
pub trait Embedder {
    /// The model identifier (e.g. `"nomic-embed-text"`).
    fn model(&self) -> &str;

    /// The dimensionality of the vectors this embedder produces.
    fn dim(&self) -> usize;

    /// Embed `text` into a vector of length [`Embedder::dim`].
    ///
    /// # Errors
    /// Returns [`crate::Error::EmbedderUnavailable`] if the backend is unreachable
    /// or the configured model is missing.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Verify the backend is reachable and the model is available.
    ///
    /// # Errors
    /// Returns [`crate::Error::EmbedderUnavailable`] if the check fails.
    fn health_check(&self) -> Result<()>;

    /// A stable fingerprint of the *resolved* model — e.g. ollama's content digest
    /// — used to detect silent drift when a mutable tag like `:latest` is
    /// re-pointed at new weights. [`Embedder::model`] returns the configured name,
    /// which cannot see such drift; this can. May perform I/O to query the backend.
    /// Returns `None` when the backend exposes no version handle.
    ///
    /// The default returns `None`, so backends opt in.
    ///
    /// # Errors
    /// Returns [`crate::Error::EmbedderUnavailable`] if the backend must be queried
    /// and is unreachable.
    fn resolved_version(&self) -> Result<Option<String>> {
        Ok(None)
    }
}
