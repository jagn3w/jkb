//! The vector index — the ONLY module that touches `sqlite-vec` (design D9).
//!
//! All `sqlite-vec` SQL and the single unavoidable `unsafe` FFI call are isolated
//! here, so the rest of the workspace stays unsafe-free and the highest-churn
//! dependency has one blast radius.
//!
//! Each embedding dimension gets its own `vec_items_<dim>` `vec0` table with
//! `item_id` as the `INTEGER PRIMARY KEY` (the vec0 rowid), so KNN returns
//! `item_id` directly with no hot-path join, and upsert/delete key on it cleanly.
//! `embeddings_meta` is the catalog of which such tables exist. v1 uses exactly one
//! model/dim; the per-dim design merely permits more later.

use std::sync::{Arc, Once};

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};

use jkb_types::{CatalogIdentity, Embedder, Error as TypesError, ItemId};

use crate::{IndexItem, Indexer, Result};

/// Delete vector rows whose item no longer exists, across **every** `vec_items_<dim>` table.
///
/// The vec tables are derived indexes (D9) but cannot carry a foreign key — they are virtual
/// tables, so `ON DELETE CASCADE` is not available to them — which means a deleted item
/// leaves its vector behind. That is not merely stale: `item_id` IS the rowid, `SQLite` hands a
/// freed rowid to the next item created, and the orphan then collides with it. Concretely,
/// `jkb ingest` -> `jkb undo` -> `jkb ingest` failed on the second ingest and on every ingest
/// into that database afterwards, while a vector search for the deleted text returned the new
/// document's chunks.
///
/// A free function taking only a connection, because the caller that needs it — `jkb undo` —
/// has no embedder and must work offline; and it sweeps every dimension's table, because a
/// database whose embedding model changed has more than one.
///
/// # Errors
/// Returns an error if a statement fails.
pub fn drop_orphan_vectors(conn: &Connection) -> Result<usize> {
    let mut dropped = 0;
    for table in vector_tables(conn)? {
        // The name comes from `sqlite_master`, filtered to our own prefix — never from input.
        dropped += conn
            .prepare_cached(&format!(
                "DELETE FROM {table} WHERE item_id NOT IN (SELECT id FROM items)"
            ))?
            .execute([])?;
    }
    Ok(dropped)
}

/// How many orphaned vector rows exist, without deleting any.
///
/// The read-only half of [`drop_orphan_vectors`] — the same predicate, minus the delete — so
/// `jkb doctor` can report what `jkb doctor --fix` would remove and stay read-only. It lives
/// here beside the delete because the two must agree on what counts as an orphan and on which
/// tables are ours; the CLI previously carried its own copy of both queries, including the
/// `vec0` shadow-table filter that is the non-obvious part.
///
/// # Errors
/// Returns an error if a statement fails.
pub fn count_orphan_vectors(conn: &Connection) -> Result<i64> {
    let mut total = 0;
    for table in vector_tables(conn)? {
        total += conn
            .prepare_cached(&format!(
                "SELECT count(*) FROM {table} WHERE item_id NOT IN (SELECT id FROM items)"
            ))?
            .query_row([], |row| row.get::<_, i64>(0))?;
    }
    Ok(total)
}

/// Every `vec_items_<dim>` table in this database — one per embedding dimension the store has
/// ever used, so a database whose model changed has more than one.
///
/// `USING vec0` is what distinguishes the virtual table from the shadow tables vec0 creates
/// beside it (`..._info`, `..._chunks`, `..._rowids`), which share the name prefix, have no
/// `item_id` column, and must never be written to directly.
///
/// # Errors
/// Returns an error if the query fails.
pub fn vector_tables(conn: &Connection) -> Result<Vec<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT name FROM sqlite_master
              WHERE type = 'table' AND name LIKE 'vec_items_%' AND sql LIKE '%vec0%'
           ORDER BY name",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Registers the statically-linked `sqlite-vec` extension so every `SQLite`
/// connection opened *afterwards* in this process gains the `vec0` virtual table
/// and vector functions.
///
/// This is the `sqlite-vec` implementation of `jkb_core::ExtensionRegistrar`. Rather
/// than calling it directly, hand it to core so it sequences the registration before
/// opening the connection: `Db::open_with(path, &[jkb_index::register])`. Idempotent
/// and thread-safe — the registration runs at most once per process.
pub fn register() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        // SAFETY: `sqlite3_auto_extension` records a C function pointer that SQLite
        // invokes on each new connection. `sqlite3_vec_init` is exactly such an
        // init function; the transmute only erases its concrete signature to the
        // generic entry-point type SQLite expects. Both come from the pinned,
        // statically-linked `sqlite-vec` crate — no extension is loaded from disk
        // (design: no arbitrary `load_extension`). This is the one FFI call the
        // workspace's `deny(unsafe_code)` allows, scoped to this block.
        #[allow(unsafe_code, clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// Vector (KNN) index over item embeddings, backed by one `vec_items_<dim>` `vec0`
/// table. Holds the [`Embedder`] so [`VectorIndexer::rebuild`] can re-derive vectors
/// from item content (the source of truth stores text, not vectors).
pub struct VectorIndexer {
    embedder: Arc<dyn Embedder + Send + Sync>,
    dim: usize,
    table: String,
}

impl VectorIndexer {
    /// Build a vector indexer for `embedder`'s model/dim. The target table is
    /// `vec_items_<dim>`.
    #[must_use]
    pub fn new(embedder: Arc<dyn Embedder + Send + Sync>) -> Self {
        let dim = embedder.dim();
        Self {
            embedder,
            dim,
            table: format!("vec_items_{dim}"),
        }
    }

    /// The name of the backing `vec_items_<dim>` table.
    #[must_use]
    pub fn table_name(&self) -> &str {
        &self.table
    }

    /// `self.dim` as an `i64` for binding, refusing absurd dims rather than wrapping.
    fn dim_i64(&self) -> Result<i64> {
        i64::try_from(self.dim).map_err(|_| {
            TypesError::Validation(format!("embedding dim {} too large", self.dim)).into()
        })
    }

    /// Create the vec table if absent and reconcile the `embeddings_meta` catalog:
    /// on first use record `(model, dim, table, model_version)`; thereafter refuse a
    /// model/dim that doesn't match what the table was populated with.
    ///
    /// # Errors
    /// Returns an error if the DDL fails or the active embedder is incompatible with
    /// the recorded catalog entry.
    pub fn ensure_ready(&self, conn: &Connection) -> Result<()> {
        // `dim` is our own `usize` (never user input), so interpolating it into the
        // DDL — where a bound parameter is not allowed anyway — is injection-safe.
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {table} USING vec0(
                 item_id INTEGER PRIMARY KEY,
                 embedding float[{dim}]
             );",
            table = self.table,
            dim = self.dim
        ))?;

        let existing: Option<(String, i64)> = conn
            .prepare_cached("SELECT model, dim FROM embeddings_meta WHERE table_name = ?1")?
            .query_row([&self.table], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;

        if let Some((model, dim)) = existing {
            let dim = usize::try_from(dim)
                .map_err(|_| TypesError::Validation(format!("catalog dim {dim} is invalid")))?;
            jkb_types::ensure_compatible(CatalogIdentity { model: &model, dim }, &*self.embedder)?;
        } else {
            // Best-effort: record the resolved version so `:latest` drift is
            // detectable later, but don't fail table setup if the backend is down.
            let version = self.embedder.resolved_version().ok().flatten();
            conn.prepare_cached(
                "INSERT INTO embeddings_meta(model, dim, table_name, model_version)
                 VALUES(?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                self.embedder.model(),
                self.dim_i64()?,
                self.table,
                version,
            ])?;
        }
        Ok(())
    }

    /// Add or replace the vector for `id`.
    ///
    /// # Errors
    /// Returns [`crate::Error`] if `embedding`'s length is not the table dim, or a
    /// statement fails.
    pub fn upsert_vector(&self, conn: &Connection, id: ItemId, embedding: &[f32]) -> Result<()> {
        if embedding.len() != self.dim {
            return Err(TypesError::Validation(format!(
                "embedding length {} does not match vec table dim {}",
                embedding.len(),
                self.dim
            ))
            .into());
        }
        let bytes = embedding_to_bytes(embedding);
        // Delete then insert, rather than `INSERT OR REPLACE`: vec0 is a virtual table and
        // does **not** honour the conflict clause, so re-inserting an existing `item_id`
        // raises `UNIQUE constraint failed` instead of replacing. The comment here used to
        // claim otherwise, and the failure only surfaced when an id was reused — a rowid
        // freed by `undo` and handed to the next ingest, which then failed permanently.
        conn.prepare_cached(&format!("DELETE FROM {} WHERE item_id = ?1", self.table))?
            .execute([id.get()])?;
        conn.prepare_cached(&format!(
            "INSERT INTO {} (item_id, embedding) VALUES (?1, ?2)",
            self.table
        ))?
        .execute(params![id.get(), bytes])?;
        Ok(())
    }

    /// Delete vector rows whose item no longer exists, returning how many went.
    ///
    /// The vec table is a **derived** index (D9) but has no foreign key to `items` — it is a
    /// virtual table, so `ON DELETE CASCADE` is not available — which means a deleted item
    /// leaves its vector behind. That is not merely stale: `item_id` is the rowid, `SQLite`
    /// hands a freed rowid to the next inserted item, and the orphan then collides with it.
    /// Concretely, `jkb ingest` + `jkb undo` + `jkb ingest` failed on the second ingest and
    /// every ingest after it, while a vector search for the deleted text returned the *new*
    /// document's chunks.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub fn drop_orphans(&self, conn: &Connection) -> Result<usize> {
        if !self.table_exists(conn)? {
            return Ok(0);
        }
        Ok(conn
            .prepare_cached(&format!(
                "DELETE FROM {} WHERE item_id NOT IN (SELECT id FROM items)",
                self.table
            ))?
            .execute([])?)
    }

    /// The stored vectors for `ids`, as an `item_id -> embedding` map. Ids with no vector
    /// are absent. Returns empty if no vec table exists yet.
    ///
    /// Reading vectors back out is what lets a parent item derive its vector from its
    /// children instead of from a truncated copy of its own text — `jkb-ingest` averages a
    /// document's chunk vectors rather than embedding the first few thousand characters of
    /// a document that may be a hundred times that long.
    ///
    /// # Errors
    /// Returns [`crate::Error`] if a statement fails or a stored blob is not a whole
    /// number of `f32`s of the table's dimension.
    pub fn vectors_for(
        &self,
        conn: &Connection,
        ids: &[ItemId],
    ) -> Result<std::collections::HashMap<ItemId, Vec<f32>>> {
        let mut out = std::collections::HashMap::new();
        if ids.is_empty() || !self.table_exists(conn)? {
            return Ok(out);
        }
        // Placeholders are generated from a count; the ids themselves are bound.
        let placeholders = (1..=ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT item_id, embedding FROM {} WHERE item_id IN ({placeholders})",
            self.table
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(ids.iter().map(|i| i.get())),
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )?;
        for row in rows {
            let (id, bytes) = row?;
            out.insert(ItemId::new(id), bytes_to_embedding(&bytes, self.dim)?);
        }
        Ok(out)
    }

    /// K-nearest-neighbour search for `query`, returning up to `k` `(item_id,
    /// distance)` pairs nearest-first. Returns empty if no vec table exists yet.
    ///
    /// # Errors
    /// Returns [`crate::Error`] if `query`'s length is not the table dim, or a
    /// statement fails.
    pub fn knn(&self, conn: &Connection, query: &[f32], k: usize) -> Result<Vec<(ItemId, f32)>> {
        if query.len() != self.dim {
            return Err(TypesError::Validation(format!(
                "query length {} does not match vec table dim {}",
                query.len(),
                self.dim
            ))
            .into());
        }
        if !self.table_exists(conn)? {
            return Ok(Vec::new());
        }
        let k = i64::try_from(k).map_err(|_| TypesError::Validation(format!("k {k} too large")))?;
        let bytes = embedding_to_bytes(query);
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT item_id, distance FROM {}
             WHERE embedding MATCH ?1 AND k = ?2
             ORDER BY distance",
            self.table
        ))?;
        let rows = stmt
            .query_map(params![bytes, k], |row| {
                Ok((ItemId::new(row.get(0)?), row.get::<_, f32>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Exact cosine distances from `query` to a specific, enumerated set of items —
    /// the recall-preserving path for a *small* scope (design D9). Rather than ask
    /// the ANN index for the global nearest neighbours and hope the in-scope ones
    /// survive, this scores exactly the `ids` given, using `sqlite-vec`'s
    /// `vec_distance_cosine` scalar over the stored vectors. Returns `(item_id,
    /// distance)` nearest-first; items in `ids` that have no stored vector are simply
    /// absent from the result.
    ///
    /// Partition seam (D9): when the vec table is later partitioned by a coarse key,
    /// this exact path is unaffected (it addresses rows by `item_id`); only the
    /// approximate [`Self::knn`] path needs to learn the partition column.
    ///
    /// # Errors
    /// Returns [`crate::Error`] if `query`'s length is not the table dim, or a
    /// statement fails.
    pub fn distances_for(
        &self,
        conn: &Connection,
        query: &[f32],
        ids: &[ItemId],
    ) -> Result<Vec<(ItemId, f32)>> {
        if query.len() != self.dim {
            return Err(TypesError::Validation(format!(
                "query length {} does not match vec table dim {}",
                query.len(),
                self.dim
            ))
            .into());
        }
        if ids.is_empty() || !self.table_exists(conn)? {
            return Ok(Vec::new());
        }
        // `?1` is the query vector; the remaining placeholders are the item ids.
        let placeholders = vec!["?"; ids.len()].join(", ");
        let mut params: Vec<Value> = Vec::with_capacity(ids.len() + 1);
        params.push(Value::Blob(embedding_to_bytes(query)));
        params.extend(ids.iter().map(|id| Value::Integer(id.get())));
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT item_id, vec_distance_cosine(embedding, ?1) AS distance
             FROM {} WHERE item_id IN ({placeholders})
             ORDER BY distance",
            self.table
        ))?;
        let rows = stmt
            .query_map(params_from_iter(params.iter()), |row| {
                Ok((ItemId::new(row.get(0)?), row.get::<_, f32>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Whether the backing vec table has been created yet.
    fn table_exists(&self, conn: &Connection) -> Result<bool> {
        let found: Option<i64> = conn
            .prepare_cached("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?
            .query_row([&self.table], |row| row.get(0))
            .optional()?;
        Ok(found.is_some())
    }
}

impl Indexer for VectorIndexer {
    // The trait fixes the signature to `-> &str`; the literal is intentional.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "vector"
    }

    fn accepts(&self, item: &IndexItem) -> bool {
        item.content.is_some()
    }

    fn index(&self, conn: &Connection, item: &IndexItem) -> Result<()> {
        // Capture must not block on the embedder (design D21): if no embedding is
        // supplied, leave the item un-vectorized for a later embed pass.
        if let Some(embedding) = item.embedding {
            self.ensure_ready(conn)?;
            self.upsert_vector(conn, item.id, embedding)?;
        }
        Ok(())
    }

    fn remove(&self, conn: &Connection, id: ItemId) -> Result<()> {
        if self.table_exists(conn)? {
            conn.prepare_cached(&format!("DELETE FROM {} WHERE item_id = ?1", self.table))?
                .execute([id.get()])?;
        }
        Ok(())
    }

    fn rebuild(&self, conn: &Connection) -> Result<()> {
        self.ensure_ready(conn)?;
        conn.execute_batch(&format!("DELETE FROM {};", self.table))?;

        // Re-derive every vector from item content (the source of truth). Collect
        // first so the read statement is dropped before we borrow `conn` to write.
        let rows: Vec<(i64, String)> = {
            let mut stmt =
                conn.prepare_cached("SELECT id, content FROM items WHERE content IS NOT NULL")?;
            let collected = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            collected
        };
        for (id, content) in rows {
            let embedding = self.embedder.embed(&content)?;
            self.upsert_vector(conn, ItemId::new(id), &embedding)?;
        }
        Ok(())
    }
}

/// Serialize a vector to `sqlite-vec`'s little-endian float32 byte layout.
fn embedding_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for f in v {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    bytes
}

/// Decode a little-endian `f32` blob written by [`embedding_to_bytes`], validating that it
/// is a whole number of floats of the expected dimension — a short or misaligned blob means
/// the table and the catalog disagree, which must surface rather than yield a silent
/// half-vector.
fn bytes_to_embedding(bytes: &[u8], dim: usize) -> Result<Vec<f32>> {
    if bytes.len() != dim * 4 {
        return Err(TypesError::Validation(format!(
            "stored embedding is {} bytes, expected {} ({dim} x f32)",
            bytes.len(),
            dim * 4
        ))
        .into());
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{bytes_to_embedding, embedding_to_bytes, register};

    #[test]
    fn embedding_bytes_round_trip_and_reject_a_wrong_length_blob() {
        let v = vec![0.5_f32, -1.25, 0.0, 3.5];
        let bytes = embedding_to_bytes(&v);
        assert_eq!(bytes_to_embedding(&bytes, 4).unwrap(), v);
        // A blob that is not exactly dim x f32 means the table and catalog disagree; that
        // must surface rather than yield a silent half-vector.
        assert!(bytes_to_embedding(&bytes, 8).is_err());
        assert!(bytes_to_embedding(&bytes[..7], 4).is_err());
    }

    use rusqlite::Connection;

    #[test]
    fn vec0_knn_returns_item_id_directly_after_registration() {
        register();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE vt USING vec0(item_id INTEGER PRIMARY KEY, embedding float[3]);",
        )
        .unwrap();
        for (id, v) in [(101i64, [1.0f32, 0.0, 0.0]), (102, [0.0, 1.0, 0.0])] {
            conn.execute(
                "INSERT INTO vt (item_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![id, embedding_to_bytes(&v)],
            )
            .unwrap();
        }

        let (item_id, dist): (i64, f32) = conn
            .query_row(
                "SELECT item_id, distance FROM vt
                 WHERE embedding MATCH ?1 AND k = 1
                 ORDER BY distance",
                [embedding_to_bytes(&[1.0, 0.0, 0.0])],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(item_id, 101);
        assert!(dist.abs() < 1e-6);
    }

    #[test]
    fn distances_for_scores_only_the_requested_ids_exactly() {
        use jkb_types::ItemId;

        // A tiny stand-in embedder (dim 3) so `VectorIndexer::new` has something to
        // hold; the vectors are hand-seeded, so `embed` is never exercised.
        struct E;
        impl jkb_types::Embedder for E {
            #[allow(clippy::unnecessary_literal_bound)]
            fn model(&self) -> &str {
                "e"
            }
            fn dim(&self) -> usize {
                3
            }
            fn embed(&self, _t: &str) -> jkb_types::Result<Vec<f32>> {
                Ok(vec![0.0; 3])
            }
            fn health_check(&self) -> jkb_types::Result<()> {
                Ok(())
            }
        }

        register();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE vec_items_3 USING vec0(item_id INTEGER PRIMARY KEY, embedding float[3]);",
        )
        .unwrap();
        for (id, v) in [
            (1i64, [1.0f32, 0.0, 0.0]),
            (2, [0.0, 1.0, 0.0]),
            (3, [0.0, 0.0, 1.0]),
        ] {
            conn.execute(
                "INSERT INTO vec_items_3 (item_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![id, embedding_to_bytes(&v)],
            )
            .unwrap();
        }

        let indexer = super::VectorIndexer::new(std::sync::Arc::new(E));

        // Query nearest [1,0,0]; ask only for ids {2, 3} — id 1 must be excluded even
        // though it is the true nearest, proving the scope is respected.
        let out = indexer
            .distances_for(&conn, &[1.0, 0.0, 0.0], &[ItemId::new(2), ItemId::new(3)])
            .unwrap();
        let ids: Vec<i64> = out.iter().map(|(id, _)| id.get()).collect();
        assert!(!ids.contains(&1), "id 1 is out of the requested set");
        assert_eq!(ids.len(), 2);
        // Both are orthogonal to the query => equal cosine distance ~1.0.
        for (_, d) in &out {
            assert!((d - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn embedding_bytes_are_little_endian_f32() {
        assert_eq!(embedding_to_bytes(&[1.0]), 1.0f32.to_le_bytes().to_vec());
        assert_eq!(embedding_to_bytes(&[]), Vec::<u8>::new());
    }
}
