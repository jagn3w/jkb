//! The FTS5 keyword index.
//!
//! `fts_items` is an FTS5 **external-content** table over `items.content`, kept in
//! sync by the INSERT/UPDATE/DELETE trigger triad created in migration `V002`. The
//! triggers do the per-item maintenance, so [`Indexer::index`] and
//! [`Indexer::remove`] here are intentionally no-ops — this type exists to expose
//! query, integrity-check, and rebuild over that index, and to participate in the
//! [`Dispatcher`](crate::Dispatcher) uniformly.

use rusqlite::Connection;

use jkb_types::ItemId;

use crate::{IndexItem, Indexer, Result};

/// Keyword search over `items.content` via FTS5.
#[derive(Debug, Default, Clone, Copy)]
pub struct FtsIndexer;

impl FtsIndexer {
    /// Create an FTS indexer.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Full-text search for `query`, returning up to `limit` `(item_id, score)`
    /// pairs best-first. `score` is FTS5 `bm25` rank (more negative = better).
    ///
    /// The `rowid` of `fts_items` is `items.id` (external-content
    /// `content_rowid='id'`), so results are item ids directly — no join.
    ///
    /// # Errors
    /// Returns an error if the query is malformed or the statement fails.
    pub fn search(
        &self,
        conn: &Connection,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(ItemId, f64)>> {
        let limit = i64::try_from(limit)
            .map_err(|_| jkb_types::Error::Validation(format!("limit {limit} too large")))?;
        let mut stmt = conn.prepare_cached(
            "SELECT rowid, bm25(fts_items) AS score
             FROM fts_items
             WHERE fts_items MATCH ?1
             ORDER BY score
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![query, limit], |row| {
                Ok((ItemId::new(row.get(0)?), row.get::<_, f64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Run FTS5's self-consistency check against the external content.
    ///
    /// # Errors
    /// Returns an error if the index is inconsistent with `items` (or the command
    /// fails).
    pub fn integrity_check(&self, conn: &Connection) -> Result<()> {
        conn.execute_batch("INSERT INTO fts_items(fts_items) VALUES('integrity-check');")?;
        Ok(())
    }
}

impl Indexer for FtsIndexer {
    // The trait fixes the signature to `-> &str`; the literal is intentional.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "fts"
    }

    fn accepts(&self, item: &IndexItem) -> bool {
        item.content.is_some()
    }

    // Maintenance is trigger-driven (migration V002), so per-item writes are no-ops.
    fn index(&self, _conn: &Connection, _item: &IndexItem) -> Result<()> {
        Ok(())
    }

    fn remove(&self, _conn: &Connection, _id: ItemId) -> Result<()> {
        Ok(())
    }

    fn rebuild(&self, conn: &Connection) -> Result<()> {
        // FTS5's 'rebuild' re-derives the whole index from the external content.
        conn.execute_batch("INSERT INTO fts_items(fts_items) VALUES('rebuild');")?;
        Ok(())
    }
}
