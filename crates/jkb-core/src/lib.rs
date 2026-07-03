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
pub mod dsl;
pub mod edge;
pub mod item;
pub mod mount;
pub mod ns;
pub mod placement;
pub mod query;
pub mod sync_state;
pub mod tag;
pub mod task;
pub mod undo;
pub mod view;

pub use error::{Error, Result};
pub use store::{cloud_sync_warning, Db, ExtensionRegistrar, WriteMeta};
