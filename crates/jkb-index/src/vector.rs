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
        // `item_id` is the rowid, so INSERT OR REPLACE is an upsert keyed on it.
        conn.prepare_cached(&format!(
            "INSERT OR REPLACE INTO {} (item_id, embedding) VALUES (?1, ?2)",
            self.table
        ))?
        .execute(params![id.get(), bytes])?;
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::{embedding_to_bytes, register};
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
