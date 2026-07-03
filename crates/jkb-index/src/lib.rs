//! Derived, rebuildable indexes over the virtual filesystem (design D1).
//!
//! Every index here is *derived* from the source-of-truth `items` table and can be
//! dropped and rebuilt. Two v1 engines implement [`Indexer`]:
//! [`fts::FtsIndexer`] (FTS5 keyword) and [`vector::VectorIndexer`] (`sqlite-vec`
//! vectors). A [`Dispatcher`] routes each item to the indexers that accept it.
//!
//! ## Layering note
//! `Indexer` lives here (not in `jkb-types`) because it operates on a
//! `rusqlite::Connection` — keeping `SQLite` out of the dependency-light vocabulary
//! crate (and thus out of `jkb-embed`). The pure `(model, dim)` catalog checks it
//! relies on *do* live in `jkb-types` ([`jkb_types::ensure_compatible`]).
//!
//! ## `sqlite-vec` registration
//! [`vector::register`] must be called once, before opening the database whose
//! connections will use vectors (before `jkb_core::Db::open`). It is the only
//! `unsafe` in the workspace and is isolated to [`vector`].

mod error;
pub mod fts;
pub mod vector;

use rusqlite::Connection;

use jkb_types::ItemId;

pub use error::{Error, Result};
pub use fts::FtsIndexer;
pub use vector::{register, VectorIndexer};

/// The item fields an [`Indexer`] needs to (re)index a single item.
///
/// Borrows rather than owns so the dispatcher can hand out a view over a row it
/// just read. `embedding` is `Some` only when a caller (the ingest embed stage)
/// has already computed it; capture leaves it `None` so indexing never blocks on
/// the embedder (design D21).
#[derive(Debug, Clone, Copy)]
pub struct IndexItem<'a> {
    /// The item's rowid identity.
    pub id: ItemId,
    /// The item kind (e.g. `document`, `chunk`, `note`, `task`).
    pub kind: &'a str,
    /// The MIME type, if known.
    pub mime: Option<&'a str>,
    /// The item's text content, if any.
    pub content: Option<&'a str>,
    /// A precomputed embedding, if the caller already has one.
    pub embedding: Option<&'a [f32]>,
}

/// A derived index over the VFS. Implementations maintain their own tables and can
/// rebuild them from the source of truth.
pub trait Indexer {
    /// A stable short name (for logging and `doctor`).
    fn name(&self) -> &str;

    /// Whether this indexer handles `item`.
    fn accepts(&self, item: &IndexItem) -> bool;

    /// Add or update `item` in the index.
    ///
    /// # Errors
    /// Returns an error if the underlying index write fails.
    fn index(&self, conn: &Connection, item: &IndexItem) -> Result<()>;

    /// Remove the item with `id` from the index.
    ///
    /// # Errors
    /// Returns an error if the underlying index delete fails.
    fn remove(&self, conn: &Connection, id: ItemId) -> Result<()>;

    /// Rebuild the entire index from the source-of-truth tables.
    ///
    /// # Errors
    /// Returns an error if the rebuild fails.
    fn rebuild(&self, conn: &Connection) -> Result<()>;
}

/// Routes item lifecycle events to the indexers that accept them (design D1).
pub struct Dispatcher {
    indexers: Vec<Box<dyn Indexer>>,
}

impl Dispatcher {
    /// Build a dispatcher over `indexers`.
    #[must_use]
    pub fn new(indexers: Vec<Box<dyn Indexer>>) -> Self {
        Self { indexers }
    }

    /// Index `item` in every accepting indexer.
    ///
    /// # Errors
    /// Returns the first indexer error (leaving earlier indexers applied; the caller
    /// runs this inside a transaction so a failure rolls the whole set back).
    pub fn on_upsert(&self, conn: &Connection, item: &IndexItem) -> Result<()> {
        for indexer in &self.indexers {
            if indexer.accepts(item) {
                indexer.index(conn, item)?;
            }
        }
        Ok(())
    }

    /// Remove the item with `id` from every indexer.
    ///
    /// # Errors
    /// Returns the first indexer error.
    pub fn on_delete(&self, conn: &Connection, id: ItemId) -> Result<()> {
        for indexer in &self.indexers {
            indexer.remove(conn, id)?;
        }
        Ok(())
    }

    /// Rebuild every index from the source of truth.
    ///
    /// # Errors
    /// Returns the first indexer error.
    pub fn rebuild_all(&self, conn: &Connection) -> Result<()> {
        for indexer in &self.indexers {
            indexer.rebuild(conn)?;
        }
        Ok(())
    }

    /// The indexers' names, in dispatch order (for `doctor`).
    #[must_use]
    pub fn indexer_names(&self) -> Vec<&str> {
        self.indexers.iter().map(|i| i.name()).collect()
    }
}
