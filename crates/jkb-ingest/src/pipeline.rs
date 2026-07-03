//! The staged, idempotent ingestion driver (design D7, D21).
//!
//! Two stages with different durability:
//!
//! - **Capture** (parse → chunk → create items/placements/edges + store blob) runs
//!   in one transaction that commits on its own. Items are FTS-indexed immediately
//!   by the `V002` triggers, so a captured source is keyword-searchable at once and
//!   capture never blocks on the embedder (D21). A mid-capture failure rolls the
//!   whole thing back — no partial items.
//! - **Embed** is a separate, resumable stage. Embeddings are computed off the
//!   writer thread (no DB lock held during network I/O), then written in a second
//!   transaction. If the embedder is down, capture still succeeds and the source is
//!   left un-embedded for a later [`Pipeline::index_pending`] pass.
//!
//! Idempotency is the `ingestions` row keyed `(source_hash, pipeline_version,
//! strategy, embedder_model)`: a completed run is a no-op; a captured-but-not-embedded
//! run resumes at the embed stage.

use std::path::Path;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};

use jkb_core::item::NewItem;
use jkb_core::{edge, item, ns, placement, Db};
use jkb_index::VectorIndexer;
use jkb_types::{EdgeType, Embedder, ItemId, PlacementRole};

use crate::adapter::{self, ParsedDocument, SourceAdapter};
use crate::chunk::{self, ChunkConfig};
use crate::{blob, Error, Result};

/// Sources yielding fewer usable characters than this trigger an empty-extraction
/// warning (e.g. a scanned PDF; OCR is v2).
const MIN_USABLE_CHARS: usize = 8;

/// The ingestion pipeline: chunking config + the embedder used for the embed stage.
pub struct Pipeline {
    embedder: Arc<dyn Embedder + Send + Sync>,
    chunking: ChunkConfig,
    version: i64,
}

/// The result of ingesting one source.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The document item id.
    pub document: ItemId,
    /// The number of chunks derived from the document.
    pub chunk_count: usize,
    /// Whether the source's items are vector-embedded (false if the embedder was
    /// down — the source is still captured and keyword-searchable).
    pub embedded: bool,
    /// True if this source was already fully ingested (the call was a no-op).
    pub already_ingested: bool,
    /// Non-fatal warnings (e.g. near-empty extraction, embedder unavailable).
    pub warnings: Vec<String>,
}

/// What the capture stage did, handed back so the caller can run the embed stage.
enum Capture {
    /// Already fully ingested — nothing to do.
    Complete {
        document: ItemId,
        chunk_count: usize,
    },
    /// Captured (or resumed): these `(item, content)` pairs need embedding.
    Pending {
        document: ItemId,
        chunk_count: usize,
        items: Vec<(ItemId, String)>,
    },
}

impl Pipeline {
    /// A pipeline with default chunking.
    #[must_use]
    pub fn new(embedder: Arc<dyn Embedder + Send + Sync>) -> Self {
        Self {
            embedder,
            chunking: ChunkConfig::default(),
            version: 1,
        }
    }

    /// Override the chunking configuration.
    #[must_use]
    pub fn with_chunking(mut self, chunking: ChunkConfig) -> Self {
        self.chunking = chunking;
        self
    }

    /// A string describing the chunking strategy, part of the idempotency key.
    fn strategy(&self) -> String {
        format!(
            "char:{}:{}",
            self.chunking.max_chars, self.chunking.overlap_chars
        )
    }

    /// Ingest the file at `path`, placing the document under `namespace`.
    ///
    /// # Errors
    /// Returns an error if the file can't be read/parsed, capture fails (rolled
    /// back), or the embed stage fails for a reason other than the embedder being
    /// unavailable (which is reported non-fatally in [`Outcome::embedded`]).
    pub fn ingest_path(&self, db: &Db, path: &Path, namespace: &str) -> Result<Outcome> {
        let bytes = std::fs::read(path)?;
        let parsed = adapter::parse(path, &bytes)?;
        self.ingest(db, &bytes, &parsed, namespace)
    }

    /// Ingest a URL (design D18): render it in a headless browser (so client-side
    /// JavaScript runs), extract text from the resulting DOM via
    /// [`adapter::HtmlAdapter`], then run the normal capture→embed pipeline. The
    /// rendered HTML is the content-addressed source, so re-ingesting an unchanged
    /// page is a no-op.
    ///
    /// # Errors
    /// Returns [`Error::Fetch`] if the page can't be rendered (e.g. Chrome not
    /// installed), or the errors of [`Pipeline::ingest`].
    pub fn ingest_url(&self, db: &Db, url: &str, namespace: &str) -> Result<Outcome> {
        let html = crate::fetch::render_url(url)?;
        let parsed = adapter::HtmlAdapter.parse(html.as_bytes())?;
        self.ingest(db, html.as_bytes(), &parsed, namespace)
    }

    /// Ingest already-read `raw` bytes with their `parsed` document (the seam
    /// [`Pipeline::ingest_url`] reuses: fetch → parse → `ingest`).
    ///
    /// # Errors
    /// See [`Pipeline::ingest_path`].
    pub fn ingest(
        &self,
        db: &Db,
        raw: &[u8],
        parsed: &ParsedDocument,
        namespace: &str,
    ) -> Result<Outcome> {
        let source_hash = blob::hash_bytes(raw);
        let mut warnings = Vec::new();
        if parsed.text.trim().chars().count() < MIN_USABLE_CHARS {
            warnings.push(format!(
                "source yielded near-zero usable text ({} chars); a scanned PDF? OCR is v2",
                parsed.text.trim().chars().count()
            ));
        }
        let chunks = chunk::chunk_text(&parsed.text, &self.chunking);

        let capture = self.capture(db, raw, parsed, namespace, &source_hash, chunks)?;
        let (document, chunk_count, items) = match capture {
            Capture::Complete {
                document,
                chunk_count,
            } => {
                return Ok(Outcome {
                    document,
                    chunk_count,
                    embedded: true,
                    already_ingested: true,
                    warnings,
                });
            }
            Capture::Pending {
                document,
                chunk_count,
                items,
            } => (document, chunk_count, items),
        };

        // Embed stage — off the writer thread. If the embedder is down, capture still
        // stands (keyword-searchable); leave it for `index_pending` (D21).
        if let Err(e) = self.embedder.health_check() {
            warnings.push(format!(
                "embedder unavailable — captured but not embedded (run index --pending later): {e}"
            ));
            return Ok(Outcome {
                document,
                chunk_count,
                embedded: false,
                already_ingested: false,
                warnings,
            });
        }

        self.embed_and_complete(db, &source_hash, &items)?;
        Ok(Outcome {
            document,
            chunk_count,
            embedded: true,
            already_ingested: false,
            warnings,
        })
    }

    /// The capture transaction: idempotency check, then create or resume.
    fn capture(
        &self,
        db: &Db,
        raw: &[u8],
        parsed: &ParsedDocument,
        namespace: &str,
        source_hash: &str,
        chunks: Vec<String>,
    ) -> Result<Capture> {
        let raw = raw.to_vec();
        let text = parsed.text.clone();
        let mime = parsed.mime.clone();
        let namespace = namespace.to_owned();
        let hash = source_hash.to_owned();
        let strategy = self.strategy();
        let model = self.embedder.model().to_owned();
        let pipeline_version = self.version;

        db.write_txn_with::<Capture, Error, _>("ingest", move |conn, meta| {
            let status: Option<String> = conn
                .prepare_cached(
                    "SELECT status FROM ingestions
                     WHERE source_hash = ?1 AND pipeline_version = ?2
                       AND strategy = ?3 AND embedder_model = ?4",
                )?
                .query_row(params![hash, pipeline_version, strategy, model], |r| {
                    r.get(0)
                })
                .optional()?;

            match status.as_deref() {
                Some("complete") => Ok(Capture::Complete {
                    document: document_id(conn, &hash)?,
                    chunk_count: count_chunks(conn, &hash)?,
                }),
                Some(_) => {
                    // Captured earlier but not embedded: resume at embed.
                    let (document, items) = load_items(conn, &hash)?;
                    Ok(Capture::Pending {
                        document,
                        chunk_count: items.len().saturating_sub(1),
                        items,
                    })
                }
                None => {
                    blob::store(conn, &hash, &raw, Some(&mime))?;
                    let ns_id = ns::ensure(conn, &namespace)?;
                    let document = item::upsert(
                        conn,
                        meta,
                        &NewItem {
                            uid: format!("b3:{hash}"),
                            kind: "document".to_owned(),
                            content: Some(text.clone()),
                            content_hash: Some(hash.clone()),
                            mime: Some(mime.clone()),
                        },
                    )?;
                    placement::place(conn, meta, document, ns_id, PlacementRole::Primary, 0)?;

                    let mut items = vec![(document, text)];
                    for (idx, chunk) in chunks.iter().enumerate() {
                        let position = i64::try_from(idx).map_err(|_| {
                            Error::Types(jkb_types::Error::Validation("too many chunks".to_owned()))
                        })?;
                        let chunk_id = item::upsert(
                            conn,
                            meta,
                            &NewItem {
                                uid: format!("b3:{hash}:{idx}"),
                                kind: "chunk".to_owned(),
                                content: Some(chunk.clone()),
                                content_hash: None,
                                mime: Some(mime.clone()),
                            },
                        )?;
                        edge::link(conn, meta, chunk_id, document, EdgeType::DerivedFrom, None)?;
                        placement::place(
                            conn,
                            meta,
                            chunk_id,
                            ns_id,
                            PlacementRole::Chunk,
                            position,
                        )?;
                        items.push((chunk_id, chunk.clone()));
                    }

                    conn.prepare_cached(
                        "INSERT INTO ingestions
                             (source_hash, pipeline_version, strategy, embedder_model,
                              stage, status, blob_hash, started_at)
                         VALUES (?1, ?2, ?3, ?4, 'embed', 'captured', ?1,
                              strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                    )?
                    .execute(params![
                        hash,
                        pipeline_version,
                        strategy,
                        model
                    ])?;

                    let chunk_count = chunks.len();
                    Ok(Capture::Pending {
                        document,
                        chunk_count,
                        items,
                    })
                }
            }
        })
    }

    /// Embed `items` (off-thread), write the vectors, and mark the ingestion complete.
    fn embed_and_complete(
        &self,
        db: &Db,
        source_hash: &str,
        items: &[(ItemId, String)],
    ) -> Result<()> {
        let mut embeddings = Vec::with_capacity(items.len());
        for (id, content) in items {
            embeddings.push((*id, self.embedder.embed(content)?));
        }

        let vector = VectorIndexer::new(self.embedder.clone());
        let hash = source_hash.to_owned();
        let strategy = self.strategy();
        let model = self.embedder.model().to_owned();
        let pipeline_version = self.version;

        db.write_txn_with::<(), Error, _>("ingest-embed", move |conn, _meta| {
            vector.ensure_ready(conn)?;
            for (id, embedding) in &embeddings {
                vector.upsert_vector(conn, *id, embedding)?;
            }
            conn.prepare_cached(
                "UPDATE ingestions
                    SET status = 'complete',
                        completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                  WHERE source_hash = ?1 AND pipeline_version = ?2
                    AND strategy = ?3 AND embedder_model = ?4",
            )?
            .execute(params![hash, pipeline_version, strategy, model])?;
            Ok(())
        })
    }

    /// The content-bearing items not yet in the vector index, **excluding** those whose
    /// content is empty or whitespace-only. A blank line preserved for round-trip (e.g.
    /// by the `tasks` file serializer) is a content item with nothing to embed, and
    /// asking the model to embed `""` yields a zero-length vector — so those are skipped
    /// here rather than sent to the embedder.
    fn pending_items(&self, db: &Db) -> Result<Vec<(ItemId, String)>> {
        let table = VectorIndexer::new(self.embedder.clone())
            .table_name()
            .to_owned();
        let rows = db.read_with::<Vec<(ItemId, String)>, Error, _>(move |conn| {
            let sql = pending_rows_sql(conn, &table)?;
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map([], |r| Ok((ItemId::new(r.get(0)?), r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        Ok(rows
            .into_iter()
            .filter(|(_, content)| !content.trim().is_empty())
            .collect())
    }

    /// The number of content-bearing items not yet in the vector index (excluding
    /// empty/whitespace-only content, which is not embeddable).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn unembedded_count(&self, db: &Db) -> Result<usize> {
        Ok(self.pending_items(db)?.len())
    }

    /// Embed every content-bearing item that isn't in the vector index yet (the
    /// `index --pending` pass, D21). Returns the number embedded.
    ///
    /// # Errors
    /// Returns [`jkb_types::Error::EmbedderUnavailable`] (via [`Error::Types`]) if
    /// the embedder is down, or a database error.
    pub fn index_pending(&self, db: &Db) -> Result<usize> {
        let pending = self.pending_items(db)?;
        if pending.is_empty() {
            return Ok(0);
        }
        // Actionable failure if the embedder is down (D21 / task 7.5).
        self.embedder.health_check()?;

        let mut embeddings = Vec::with_capacity(pending.len());
        for (id, content) in &pending {
            embeddings.push((*id, self.embedder.embed(content)?));
        }
        let count = embeddings.len();
        let vector = VectorIndexer::new(self.embedder.clone());
        db.write_txn_with::<(), Error, _>("index-pending", move |conn, _meta| {
            vector.ensure_ready(conn)?;
            for (id, embedding) in &embeddings {
                vector.upsert_vector(conn, *id, embedding)?;
            }
            Ok(())
        })?;
        Ok(count)
    }
}

/// Whether a table named `table` exists.
fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let found: Option<i64> = conn
        .prepare_cached("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?
        .query_row([table], |r| r.get(0))
        .optional()?;
    Ok(found.is_some())
}

/// `SELECT count(*)` of un-embedded content items (all of them if no vec table yet).
/// `SELECT id, content` of un-embedded content items.
fn pending_rows_sql(conn: &Connection, table: &str) -> rusqlite::Result<String> {
    Ok(if table_exists(conn, table)? {
        format!(
            "SELECT id, content FROM items
             WHERE content IS NOT NULL AND id NOT IN (SELECT item_id FROM {table})"
        )
    } else {
        "SELECT id, content FROM items WHERE content IS NOT NULL".to_owned()
    })
}

/// The document item id for `hash` (uid `b3:<hash>`).
fn document_id(conn: &Connection, hash: &str) -> Result<ItemId> {
    let id: i64 = conn
        .prepare_cached("SELECT id FROM items WHERE uid = ?1")?
        .query_row([format!("b3:{hash}")], |r| r.get(0))?;
    Ok(ItemId::new(id))
}

/// The number of chunk items for `hash` (uid `b3:<hash>:<idx>`).
fn count_chunks(conn: &Connection, hash: &str) -> Result<usize> {
    let n: i64 = conn
        .prepare_cached("SELECT count(*) FROM items WHERE uid LIKE ?1")?
        .query_row([format!("b3:{hash}:%")], |r| r.get(0))?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// The document item plus its chunks, as `(id, content)` pairs, for the embed stage.
fn load_items(conn: &Connection, hash: &str) -> Result<(ItemId, Vec<(ItemId, String)>)> {
    let document = document_id(conn, hash)?;
    let doc_content: String = conn
        .prepare_cached("SELECT content FROM items WHERE id = ?1")?
        .query_row([document.get()], |r| r.get(0))?;
    let mut items = vec![(document, doc_content)];

    let mut stmt = conn.prepare_cached(
        "SELECT id, content FROM items WHERE uid LIKE ?1 AND content IS NOT NULL ORDER BY id",
    )?;
    let chunks = stmt
        .query_map([format!("b3:{hash}:%")], |r| {
            Ok((ItemId::new(r.get(0)?), r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    items.extend(chunks);
    Ok((document, items))
}
