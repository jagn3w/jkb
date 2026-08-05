//! Bidirectional `file://` sync (design D3/D24/D25, Section 11).
//!
//! Reconciles bound files with items through a pluggable [`SyncSerializer`] (v1 ships
//! [`DocumentSerializer`] = one file ⇄ one item). [`sync`] is a one-shot reconcile
//! over a mount; [`watch`] reacts to filesystem changes with debounce. Direction
//! (import / export / conflict) is decided per file by comparing the disk hash and
//! the KB-render hash against the binding's `last_synced_hash`, honouring the mount's
//! `sync_mode` and `conflict_policy`. Every reconcile is one audited transaction.
//!
//! Open the database with `jkb-index`'s registrar as usual
//! (`Db::open_with(path, &[jkb_index::register])`); sync itself needs no extensions.

mod engine;
mod error;
mod serializers;
mod watch;

pub use engine::{
    backing_dir, sync, sync_paths, sync_with_policy, tasks_mount_file, FileResult, Outcome,
    SyncReport,
};
pub use error::{Error, Result};
pub use serializers::{
    resolve, DocumentSerializer, SyncDoc, SyncEdge, SyncItem, SyncSection, SyncSerializer,
    TasksSerializer, AVAILABLE,
};
pub use watch::{watch, watch_all};
