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

use rusqlite::{Connection, OptionalExtension};

use jkb_core::item::NewItem;
use jkb_core::{edge, ingestion, item, ns, placement, Db};
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

        self.embed_and_complete(db, &source_hash, document, &items)?;
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
            let key = ingestion::Key {
                source_hash: &hash,
                pipeline_version,
                strategy: &strategy,
                embedder_model: &model,
            };
            // The marker is **evidence**, not proof: it says a capture happened, not that its
            // items are still there. `jkb undo` used to be the only way they could go and it
            // now takes the marker with them — but `jkb item rm <document>` reaches the same
            // state, and every resume then failed with an opaque "Query returned no rows"
            // that no CLI verb could clear, because nothing deletes an ingestion row. A marker
            // whose document is gone is treated as absent, and this capture starts fresh.
            let status = match ingestion::status(conn, key)? {
                Some(recorded) if item::id_for_uid(conn, &format!("b3:{hash}"))?.is_some() => {
                    Some(recorded)
                }
                Some(_) => {
                    // The document is gone but its CHUNKS may not be: `item rm <document>`
                    // cascades containment and edges, not the chunk items themselves. A fresh
                    // capture would then re-insert uid `b3:<hash>:0` and die on
                    // `UNIQUE constraint failed: items.uid` — swapping one permanent failure
                    // for another. Clear the fragments here, inside the same transaction, so
                    // "starts fresh" is true.
                    let ids = surviving_fragments(conn, &hash)?;
                    for id in ids {
                        item::remove(conn, meta, id, true)?;
                    }
                    // No vector sweep here. It used to be required — `item_id` is the rowid,
                    // and the fresh chunks below were about to be handed those very ids, so a
                    // leftover row made a new chunk inherit a dead embedding. Since D40
                    // (`items.id AUTOINCREMENT`) a freed id is never reissued, so the leftover
                    // rows are inert and are collected by `jkb index --sweep`. This sweep was
                    // added, correctly, one call site at a time across four review passes —
                    // which is the evidence that per-call-site cleanup was the wrong shape.
                    None
                }
                None => None,
            };

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
                        // The document CONTAINS its chunks (design D35), so they are listed
                        // by expanding it rather than as flat siblings beside it.
                        jkb_core::containment::contain(conn, meta, chunk_id, document, position)?;
                        items.push((chunk_id, chunk.clone()));
                    }

                    // Through the core repo, so the marker is CHANGELOGGED and `undo` takes
                    // it in the same transaction as the items it describes. Written directly
                    // here it had no changelog entry, so an undo deleted the document and its
                    // chunks while the marker survived — after which every `jkb ingest` of
                    // that file resumed into a document that no longer existed, with no CLI
                    // verb able to clear the row.
                    ingestion::record_capture(conn, meta, key)?;

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
    ///
    /// `document`'s own vector is **averaged from its chunks** rather than embedded from its
    /// text — see [`Pipeline::derive_document_vectors`] for why a truncated document vector
    /// is worse than none. This is the same rule the `index_pending` mop-up applies, so a
    /// document gets the same vector whichever path indexed it.
    fn embed_and_complete(
        &self,
        db: &Db,
        source_hash: &str,
        document: ItemId,
        items: &[(ItemId, String)],
    ) -> Result<()> {
        let mut embeddings = Vec::with_capacity(items.len());
        for (id, content) in items.iter().filter(|(id, _)| *id != document) {
            embeddings.push((*id, self.embedder.embed(content)?));
        }
        let chunk_vectors: Vec<Vec<f32>> = embeddings.iter().map(|(_, v)| v.clone()).collect();
        match centroid(&chunk_vectors) {
            Some(c) => embeddings.push((document, c)),
            // No chunks (a source short enough that chunking produced none): the document's
            // own text is short by construction, so embedding it directly is safe.
            None => {
                if let Some((_, text)) = items.iter().find(|(id, _)| *id == document) {
                    embeddings.push((document, self.embedder.embed(text)?));
                }
            }
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
            ingestion::mark_complete(
                conn,
                ingestion::Key {
                    source_hash: &hash,
                    pipeline_version,
                    strategy: &strategy,
                    embedder_model: &model,
                },
            )?;
            Ok(())
        })
    }

    /// The content-bearing items not yet in the vector index, **excluding** those whose
    /// content is empty or whitespace-only. A blank line preserved for round-trip (e.g.
    /// by the `tasks` file serializer) is a content item with nothing to embed, and
    /// asking the model to embed `""` yields a zero-length vector — so those are skipped
    /// here rather than sent to the embedder.
    fn pending_items(&self, db: &Db) -> Result<Vec<Pending>> {
        let table = VectorIndexer::new(self.embedder.clone())
            .table_name()
            .to_owned();
        let rows = db.read_with::<Vec<Pending>, Error, _>(move |conn| {
            let sql = pending_rows_sql(conn, &table)?;
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Pending {
                        id: ItemId::new(r.get(0)?),
                        content: r.get::<_, String>(1)?,
                        kind: r.get::<_, String>(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        Ok(rows
            .into_iter()
            .filter(|p| !p.content.trim().is_empty())
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
    pub fn index_pending(&self, db: &Db) -> Result<IndexReport> {
        let pending = self.pending_items(db)?;
        let mut report = IndexReport::default();
        if pending.is_empty() {
            return Ok(report);
        }
        // Actionable failure if the embedder is down (D21 / task 7.5).
        self.embedder.health_check()?;

        // A document is embedded from its chunks, not from its own text (see
        // `derive_document_vectors`), so it is held back until the chunks are in.
        let (documents, direct): (Vec<Pending>, Vec<Pending>) =
            pending.into_iter().partition(|p| p.kind == KIND_DOCUMENT);

        for batch in direct.chunks(EMBED_BATCH) {
            let mut embeddings = Vec::with_capacity(batch.len());
            for item in batch {
                match self.embedder.embed(&item.content) {
                    Ok(v) => embeddings.push((item.id, v)),
                    // One bad item must not discard the whole backfill. A run over tens of
                    // thousands of items will meet a transient failure eventually; losing
                    // every other item's work to it makes the mop-up path unusable exactly
                    // when it is most needed.
                    Err(e) => {
                        report.failed += 1;
                        report.note_failure(&e.to_string());
                    }
                }
            }
            report.embedded += self.store_vectors(db, embeddings)?;
        }

        report.derived = self.derive_document_vectors(db, &documents)?;
        Ok(report)
    }

    /// Write a batch of vectors in one transaction, returning how many landed.
    ///
    /// Committing per batch rather than once at the end is what makes a long backfill
    /// resumable: interrupting it keeps everything already written, and re-running picks up
    /// only what is still missing.
    fn store_vectors(&self, db: &Db, embeddings: Vec<(ItemId, Vec<f32>)>) -> Result<usize> {
        if embeddings.is_empty() {
            return Ok(0);
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

    /// Give each document a vector **averaged from its chunks** rather than embedded from
    /// its own text.
    ///
    /// A document's content is the whole source — a transcript can be a hundred times the
    /// embedder's context. Embedding it directly means embedding a truncated prefix, which
    /// does not merely lose information: it produces a vector that claims to describe the
    /// document while describing only its opening, so the document ranks against queries
    /// about its beginning and misses everything after. The chunks already cover the whole
    /// text, so their centroid is both cheaper (no model call) and more faithful.
    ///
    /// A document with no chunks (short enough that ingest made none, or created by a path
    /// that does not chunk) falls back to embedding its own content, which is safe because
    /// it is short by construction.
    fn derive_document_vectors(&self, db: &Db, documents: &[Pending]) -> Result<usize> {
        if documents.is_empty() {
            return Ok(0);
        }
        let mut derived = Vec::new();
        let mut fallback = Vec::new();
        for doc in documents {
            let id = doc.id;
            let embedder = self.embedder.clone();
            let vectors = db.read_with::<Vec<Vec<f32>>, Error, _>(move |conn| {
                let chunk_ids = chunk_ids_of(conn, id)?;
                let indexer = VectorIndexer::new(embedder);
                Ok(indexer
                    .vectors_for(conn, &chunk_ids)?
                    .into_values()
                    .collect())
            })?;
            match centroid(&vectors) {
                Some(c) => derived.push((doc.id, c)),
                None => fallback.push(doc),
            }
        }
        for doc in fallback {
            if let Ok(v) = self.embedder.embed(&doc.content) {
                derived.push((doc.id, v));
            }
        }
        let mut written = 0;
        for batch in derived.chunks(EMBED_BATCH) {
            written += self.store_vectors(db, batch.to_vec())?;
        }
        Ok(written)
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
            // The join is belt-and-braces, and provably cannot change this result set: an
            // orphan's `item_id` matches no live item, so `NOT IN` already excludes it, and
            // under id *reuse* the row names a live item so the join succeeds and the item still
            // reads as indexed. Only the high-water mark (`V011`) prevents reuse. It is kept
            // because it states the intent — "indexed" means a vector for an item that exists —
            // where a bare `NOT IN` leaves that to the reader.
            "SELECT id, content, kind FROM items
             WHERE content IS NOT NULL
               AND id NOT IN (SELECT v.item_id FROM {table} v
                              JOIN items i ON i.id = v.item_id)"
        )
    } else {
        "SELECT id, content, kind FROM items WHERE content IS NOT NULL".to_owned()
    })
}

/// The item kind ingest stores a whole source document as. Its chunks are separate items
/// linked back to it by `derived_from`.
const KIND_DOCUMENT: &str = "document";

/// How many vectors to compute before committing. Small enough that an interrupted backfill
/// loses at most this much work, large enough that per-transaction overhead stays noise
/// against the model call that dominates each item.
const EMBED_BATCH: usize = 256;

/// One un-embedded item awaiting a vector.
struct Pending {
    id: ItemId,
    content: String,
    kind: String,
}

/// What one [`Pipeline::index_pending`] run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexReport {
    /// Items embedded by calling the model.
    pub embedded: usize,
    /// Documents whose vector was averaged from their chunks (no model call).
    pub derived: usize,
    /// Items skipped because the embedder rejected them. They stay pending, so a later run
    /// retries them; the run itself still succeeds.
    pub failed: usize,
    /// The first failure's message, so a caller can say *why* without a log scrape.
    pub first_error: Option<String>,
}

impl IndexReport {
    /// Total vectors written this run.
    #[must_use]
    pub fn total(&self) -> usize {
        self.embedded + self.derived
    }

    fn note_failure(&mut self, message: &str) {
        if self.first_error.is_none() {
            self.first_error = Some(message.to_owned());
        }
    }
}

/// The chunk items derived from `document`, via the `derived_from` edge ingest writes.
fn chunk_ids_of(conn: &Connection, document: ItemId) -> rusqlite::Result<Vec<ItemId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.src_item_id FROM edges e
         JOIN items i ON i.id = e.src_item_id
         WHERE e.dst_item_id = ?1 AND e.type = 'derived_from' AND i.kind = 'chunk'",
    )?;
    let rows = stmt.query_map([document.get()], |r| Ok(ItemId::new(r.get(0)?)))?;
    rows.collect()
}

/// The mean of `vectors`, or `None` if there are none (or they disagree on length).
///
/// Deliberately *not* re-normalized: the vec table stores raw embeddings and
/// `vec_distance_cosine` normalizes at comparison time, so scaling here would be a no-op
/// that only obscured what is stored.
fn centroid(vectors: &[Vec<f32>]) -> Option<Vec<f32>> {
    let first = vectors.first()?;
    let dim = first.len();
    if dim == 0 || vectors.iter().any(|v| v.len() != dim) {
        return None;
    }
    let mut sum = vec![0.0_f64; dim];
    for v in vectors {
        for (acc, x) in sum.iter_mut().zip(v) {
            *acc += f64::from(*x);
        }
    }
    // A count that does not fit in u32 is not a real chunk count; failing the average is
    // better than silently losing precision in the divisor.
    let n = f64::from(u32::try_from(vectors.len()).ok()?);
    #[allow(clippy::cast_possible_truncation)] // f64 mean back to the f32 the table stores
    Some(sum.into_iter().map(|x| (x / n) as f32).collect())
}

/// The document item id for `hash` (uid `b3:<hash>`).
fn document_id(conn: &Connection, hash: &str) -> Result<ItemId> {
    let id: i64 = conn
        .prepare_cached("SELECT id FROM items WHERE uid = ?1")?
        .query_row([format!("b3:{hash}")], |r| r.get(0))?;
    Ok(ItemId::new(id))
}

/// Items left over from a previous ingestion of `hash` — its chunks, and the document if it
/// is somehow still there — so a re-capture can start from nothing.
fn surviving_fragments(conn: &Connection, hash: &str) -> Result<Vec<ItemId>> {
    let mut stmt =
        conn.prepare_cached("SELECT id FROM items WHERE uid = ?1 OR uid LIKE ?2 ORDER BY id DESC")?;
    let rows = stmt.query_map(
        rusqlite::params![format!("b3:{hash}"), format!("b3:{hash}:%")],
        |r| r.get::<_, i64>(0),
    )?;
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(ItemId::new)
        .collect())
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
