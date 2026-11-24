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
pub mod claude_session;
pub mod containment;
pub mod design;
pub mod dsl;
pub mod edge;
pub mod exec;
pub mod host;
pub mod ingestion;
pub mod investigation;
pub mod item;
pub mod lease;
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
pub mod removal;
pub mod reviews;
pub mod roles;
pub mod sql;
pub mod sync_state;
pub mod tag;
pub mod task;
pub mod timeline;
pub mod transition;
pub mod undo;
pub mod view;
pub mod workflow;

pub use error::{Error, Result};
pub use migrate::{
    applied_version as applied_schema_version, refuse_newer as refuse_newer_schema,
    supported_version as supported_schema_version,
};
pub use store::{cloud_sync_warning, Db, ExtensionRegistrar, WriteMeta};

/// This crate's directory, read at run time from the `CARGO_MANIFEST_DIR` that `cargo test` sets.
///
/// Not the compile-time `env!` form: the build directory is shared across checkouts, so a test
/// binary compiled in one worktree is reused by another, and a baked path keeps naming the first
/// one — after that worktree is deleted, every test reading a repo file through it fails `NotFound`.
#[cfg(test)]
pub(crate) fn manifest_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo test sets CARGO_MANIFEST_DIR"),
    )
}

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

/// No test in the workspace reads a repo file through the compile-time manifest directory. Each
/// crate's tests call its `manifest_dir()`; this keeps a new test from reaching for `env!` instead,
/// which passes in the checkout that compiled it and fails only in the land gate of another.
#[cfg(test)]
mod manifest_dir_tests {
    #[test]
    fn no_source_bakes_in_the_manifest_directory() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(&super::manifest_dir().join(".."), &mut files);
        assert!(
            files.len() > 50,
            "scanned {} files; the walk is broken",
            files.len()
        );
        // Assembled, so this file does not match its own needle.
        let needle = format!("env!(\"{}\")", "CARGO_MANIFEST_DIR");
        let baked: Vec<_> = files
            .iter()
            .filter(|f| std::fs::read_to_string(f).unwrap().contains(&needle))
            .collect();
        assert!(
            baked.is_empty(),
            "read the run-time CARGO_MANIFEST_DIR (the crate's `manifest_dir()`), not {needle}: {baked:?}"
        );
    }
}
