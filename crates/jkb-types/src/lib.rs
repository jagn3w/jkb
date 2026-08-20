//! Shared vocabulary for jkb: newtype IDs, enums, the core trait seams, and the
//! shared error type.
//!
//! This crate is deliberately dependency-light (`serde`, `thiserror`, and the dependency-free
//! `jkb-fsm`) so every other crate can share one set of types without pulling in heavy
//! implementations. `jkb-fsm` is here because [`TaskStatus`] *is* a lifecycle state and
//! implementing [`jkb_fsm::State`] for it must live beside the enum — a parallel state enum in
//! `jkb-core` would be a fourth copy of the same five strings.

mod agent;
mod catalog;
mod enums;
mod error;
mod id;
mod traits;

pub use agent::{AgentId, Liveness};
pub use catalog::{check_version_drift, ensure_compatible, CatalogIdentity, VersionDrift};
pub use enums::{
    ConflictPolicy, EdgeType, NamespaceKind, PlacementRole, Resolution, SyncMode, TaskStatus,
};
pub use error::{Error, Result};
pub use id::{EdgeId, ItemId, NamespaceId, Uid};
pub use traits::Embedder;
