//! The durable source of truth: the `SQLite`-backed virtual filesystem.
//!
//! All access goes through [`Db`], a cheap-to-clone handle backed by a single
//! writer thread (design D8). Repositories are plain functions over a connection
//! (see [`ns`], [`item`], [`placement`], [`edge`], [`tag`], [`binding`],
//! [`mount`], [`task`], [`undo`]) composed inside [`Db::read`] / [`Db::write_txn`].

mod changelog;
mod db;
mod error;
mod migrate;
mod store;

pub mod binding;
pub mod blob;
pub mod branch;
pub mod claim;
pub mod containment;
pub mod dsl;
pub mod edge;
pub mod ingestion;
pub mod investigation;
pub mod item;
pub mod lifecycle;
pub mod mount;
pub mod ns;
pub mod nstype;
pub mod placement;
pub mod query;
pub mod sql;
pub mod sync_state;
pub mod tag;
pub mod task;
pub mod transition;
pub mod undo;
pub mod view;

pub use error::{Error, Result};
pub use store::{cloud_sync_warning, Db, ExtensionRegistrar, WriteMeta};
