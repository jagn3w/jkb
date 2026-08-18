//! Bidirectional `file://` sync (design D3/D24/D25, Section 11).
//!
//! Reconciles bound files with items through a pluggable [`SyncSerializer`]. Two ship:
//! [`DocumentSerializer`] (one file ⇄ one item) and [`TasksSerializer`] (one `tasks.md`
//! ⇄ many `task` items). [`sync`] is a one-shot reconcile over a mount; [`watch`] reacts
//! to filesystem changes with debounce. Direction (import / export / merge / conflict) is
//! decided per file from its `_sys/sync` journal row, by comparing the disk bytes and the
//! KB render each against the last-settled **base** — never against each other — and
//! honouring the mount's `sync_mode` and `conflict_policy`. Every reconcile is one
//! audited transaction.
//!
//! Open the database with `jkb-index`'s registrar as usual
//! (`Db::open_with(path, &[jkb_index::register])`); sync itself needs no extensions.

mod engine;
mod error;
mod serializers;
mod watch;

pub use engine::{
    backing_dir, file_uri, sync, sync_paths, sync_with_policy, tasks_mount_file, FileResult,
    Outcome, SyncReport,
};
pub use error::{Error, Result};
pub use serializers::{
    resolve, DocumentSerializer, SyncDoc, SyncEdge, SyncItem, SyncSection, SyncSerializer,
    TasksSerializer, AVAILABLE,
};
pub use watch::{watch, watch_all};
