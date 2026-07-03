//! Staged, idempotent ingestion (design D7, D18, D21).
//!
//! [`Pipeline`] drives `parse → chunk → embed`, keyed by the `ingestions`
//! idempotency row: a completed run is a no-op, a captured-but-un-embedded run
//! resumes at the embed stage. Capture writes items (FTS-indexed immediately by
//! triggers) in one transaction; embedding is a separate, resumable stage that
//! never blocks capture ([`Pipeline::index_pending`] mops up un-embedded items).
//!
//! Raw sources are content-addressed in the `blobs` store ([`blob`]); source bytes
//! become a [`adapter::ParsedDocument`] via a [`adapter::SourceAdapter`]. This is
//! the crate where `jkb-core` (items) and `jkb-index` (vectors) meet, so its
//! [`Error`] absorbs both.
//!
//! Open the database with `sqlite-vec` registered so the embed stage can write
//! vectors: `Db::open_with(path, &[jkb_index::register])`.

pub mod adapter;
pub mod blob;
pub mod chunk;
mod error;
mod fetch;
mod pipeline;

pub use error::{Error, Result};
pub use pipeline::{Outcome, Pipeline};
