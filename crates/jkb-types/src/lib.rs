//! Shared vocabulary for jkb: newtype IDs, enums, the core trait seams, and the
//! shared error type.
//!
//! This crate is deliberately dependency-light (only `serde` + `thiserror`) so
//! every other crate can share one set of types without pulling in heavy
//! implementations.

mod catalog;
mod enums;
mod error;
mod id;
mod traits;

pub use catalog::{check_version_drift, ensure_compatible, CatalogIdentity, VersionDrift};
pub use enums::{
    ConflictPolicy, EdgeType, NamespaceKind, PlacementRole, Resolution, SyncMode, TaskStatus,
};
pub use error::{Error, Result};
pub use id::{EdgeId, ItemId, NamespaceId, Uid};
pub use traits::Embedder;
