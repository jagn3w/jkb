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
mod shared_fs;
mod store;

pub mod binding;
pub mod blob;
pub mod claim;
pub mod containment;
pub mod dsl;
pub mod edge;
pub mod ingestion;
pub mod investigation;
pub mod item;
pub mod lifecycle;
pub mod location;
pub mod mount;
pub mod mq;
pub mod nofollow;
pub mod notify;
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
pub use migrate::{
    applied_version as applied_schema_version, refuse_newer as refuse_newer_schema,
    supported_version as supported_schema_version,
};
pub use store::{cloud_sync_warning, Db, ExtensionRegistrar, WriteMeta};

/// Refuse a path on a filesystem shared with another kernel — the rule `Db::open` applies to a
/// database, offered for the other file whose writer must be the host: `jkb serve`'s token, which
/// the dev container sees through the `~/.jkb` bind. A daemon started in the container would
/// otherwise overwrite the host daemon's live token and lock every client out.
///
/// # Errors
/// As the database refusal: [`Error::UriPath`], [`Error::SharedFilesystem`], or
/// [`Error::FilesystemUnknown`] when the filesystem cannot be established.
pub fn refuse_shared_filesystem(path: &std::path::Path) -> Result<()> {
    shared_fs::refuse(path)
}
