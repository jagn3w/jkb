//! Content-addressed blob store.
//!
//! The `blobs` table and its store live in `jkb-core` (it owns the schema and is now
//! shared by `jkb-sync` for base/quarantine bytes too). This module just re-exports
//! that single implementation so ingestion keeps calling `blob::hash_bytes` /
//! `blob::store` unchanged; `store` returns a `jkb_core::Result`, which the ingest
//! pipeline's `?` bridges via `Error::Core`.

pub use jkb_core::blob::{hash_bytes, store};
