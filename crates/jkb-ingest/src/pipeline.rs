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
//!   left pending for a later [`Pipeline::index_pending`] pass.
//!
//! What is pending is derived, never marked (`V030`): an item whose vector is missing or was
//! computed from text it no longer holds ([`jkb_index::pending_predicate`]), plus the documents
//! `Pipeline::embed_document` must re-derive — re-chunked after an edit, or re-centred after a
//! chunk's vector changed.
//!
//! Idempotency is the `ingestions` row keyed `(source_hash, pipeline_version,
//! strategy, embedder_model)`: a completed run is a no-op; a captured-but-not-embedded
//! run resumes at the embed stage.

use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};

use jkb_core::item::NewItem;
use jkb_core::WriteMeta;
use jkb_core::{edge, ingestion, item, ns, placement, Db};
use jkb_index::{Indexer as _, VectorIndexer};
use jkb_types::{EdgeType, Embedder, ItemId, NamespaceId, PlacementRole};

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
    actor: &'static str,
}

/// The result of ingesting one source.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The document item id.
    pub document: ItemId,
    /// The number of chunks derived from the document.
    pub chunk_count: usize,
    /// Whether the source's items are vector-embedded and current (false if the embedder was
    /// down, or the document was edited during the run — the source is still captured and
    /// keyword-searchable, and `jkb index` finishes it).
    pub embedded: bool,
    /// True if an earlier run captured this source, so this call wrote no items — it may still have
    /// written vectors, or marked a capture embedded since complete.
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
    /// Captured (or resumed): the document and its chunks need embedding.
    Pending {
        document: ItemId,
        chunk_count: usize,
        /// Captured by an earlier run, so this one wrote no items.
        resumed: bool,
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
            actor: "ingest",
        }
    }

    /// Record the capture in the changelog as `actor`'s write (`jkb serve` records `serve`).
    #[must_use]
    pub const fn with_actor(mut self, actor: &'static str) -> Self {
        self.actor = actor;
        self
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
            self.chunking.max_chars(),
            self.chunking.overlap_chars()
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
        self.ingest(db, Some(&bytes), &parsed, namespace)
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
        self.ingest(db, Some(html.as_bytes()), &parsed, namespace)
    }

    /// Ingest a `parsed` document, with the `raw` bytes it was parsed from when the caller has them
    /// (the seam [`Pipeline::ingest_url`] and `jkb_api`'s `ingest.text` reuse).
    ///
    /// With `raw`, the source is content-addressed by those bytes and they are stored as its blob. Without
    /// — text parsed by a client that did not send its source, as the dev container's `jkb ingest`
    /// through `jkb serve` — it is addressed by [`text_address`], a hash in a domain of its own, and no
    /// blob is stored. Not by the text's plain hash: a UTF-8 file's text is its bytes, so a client that
    /// sent a host file's bytes as text took the document the host's own ingest of that file resumes
    /// into — under the client's namespace and mime, unparsed, and `already ingested` ever after.
    ///
    /// # Errors
    /// See [`Pipeline::ingest_path`].
    pub fn ingest(
        &self,
        db: &Db,
        raw: Option<&[u8]>,
        parsed: &ParsedDocument,
        namespace: &str,
    ) -> Result<Outcome> {
        let source_hash = raw.map_or_else(|| text_address(&parsed.text), blob::hash_bytes);
        let mut warnings = Vec::new();
        if parsed.text.trim().chars().count() < MIN_USABLE_CHARS {
            warnings.push(format!(
                "source yielded near-zero usable text ({} chars); a scanned PDF? OCR is v2",
                parsed.text.trim().chars().count()
            ));
        }
        let chunks = chunk::chunk_text(&parsed.text, &self.chunking);

        let capture = self.capture(db, raw, parsed, namespace, &source_hash, chunks)?;
        let (document, chunk_count, resumed) = match capture {
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
                resumed,
            } => (document, chunk_count, resumed),
        };

        // A resumed capture whose document and chunks are all current was embedded since — by `jkb
        // index`, which writes vectors but keys no ingestion. It is complete, and a repeat run says so
        // rather than resuming it forever.
        if resumed && self.document_is_current(db, document)? {
            self.mark_complete(db, &source_hash)?;
            return Ok(Outcome {
                document,
                chunk_count,
                embedded: true,
                already_ingested: true,
                warnings,
            });
        }

        // Embed stage — off the writer thread. If the embedder is down, capture still
        // stands (keyword-searchable); leave it for `index_pending` (D21).
        if let Err(e) = self.embedder.health_check() {
            warnings.push(format!(
                "embedder unavailable — captured but not embedded (run `jkb index` later): {e}"
            ));
            return Ok(Outcome {
                document,
                chunk_count,
                embedded: false,
                // Resumed and not embedded: this run wrote nothing.
                already_ingested: resumed,
                warnings,
            });
        }

        let embedded = self.embed_and_complete(db, &source_hash, document)?;
        Ok(Outcome {
            document,
            chunk_count,
            embedded,
            already_ingested: false,
            warnings,
        })
    }

    /// The capture transaction: idempotency check, then create or resume.
    fn capture(
        &self,
        db: &Db,
        raw: Option<&[u8]>,
        parsed: &ParsedDocument,
        namespace: &str,
        source_hash: &str,
        chunks: Vec<String>,
    ) -> Result<Capture> {
        let raw = raw.map(<[u8]>::to_vec);
        let text = parsed.text.clone();
        let mime = parsed.mime.clone();
        let namespace = namespace.to_owned();
        let hash = source_hash.to_owned();
        let strategy = self.strategy();
        let model = self.embedder.model().to_owned();
        let pipeline_version = self.version;

        db.write_txn_with::<Capture, Error, _>(self.actor, move |conn, meta| {
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

            // Whatever the state, a run that has the source bytes keeps them, so a blob removed out of band
            // is restored by the next ingest of its source.
            if let Some(raw) = &raw {
                blob::store(conn, &hash, raw, Some(&mime))?;
            }
            match status.as_deref() {
                Some("complete") => Ok(Capture::Complete {
                    document: document_id(conn, &hash)?,
                    chunk_count: count_chunks(conn, &hash)?,
                }),
                Some(_) => {
                    // Captured earlier but not embedded: resume at embed.
                    Ok(Capture::Pending {
                        document: document_id(conn, &hash)?,
                        chunk_count: count_chunks(conn, &hash)?,
                        resumed: true,
                    })
                }
                None => {
                    let ns_id = ns::ensure(conn, meta, &namespace)?;
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
                    write_chunks(
                        conn,
                        meta,
                        &Cut {
                            document,
                            rev: content_rev(conn, document)?,
                            uid_prefix: &format!("b3:{hash}"),
                            chunks: &chunks,
                            mime: Some(&mime),
                            namespace: Some(ns_id),
                        },
                    )?;

                    // Through the core repo, so the marker is CHANGELOGGED and `undo` takes
                    // it in the same transaction as the items it describes. Written directly
                    // here it had no changelog entry, so an undo deleted the document and its
                    // chunks while the marker survived — after which every `jkb ingest` of
                    // that file resumed into a document that no longer existed, with no CLI
                    // verb able to clear the row.
                    ingestion::record_capture(conn, meta, key)?;

                    Ok(Capture::Pending {
                        document,
                        chunk_count: chunks.len(),
                        resumed: false,
                    })
                }
            }
        })
    }

    /// Embed a captured `document` through [`Pipeline::embed_document`] and, once it and its chunks
    /// are current, mark the ingestion complete. Returns whether they are.
    ///
    /// The same function `index_pending` calls, so a document gets the same vector whichever path
    /// indexed it — including one edited between its capture and this resume, which is re-chunked
    /// first rather than given its old chunks' centroid under its new text. An embedder error fails
    /// the ingest, as it always has: the caller asked for this one source, not a backfill.
    fn embed_and_complete(&self, db: &Db, source_hash: &str, document: ItemId) -> Result<bool> {
        let mut report = IndexReport::default();
        self.embed_document(db, document, Failures::Propagate, &mut report)?;
        if !self.document_is_current(db, document)? {
            // An edit landed mid-run: what was embedded no longer describes the document, and
            // nothing was stored for it. The next run (or `jkb index`) finishes it.
            return Ok(false);
        }
        self.mark_complete(db, source_hash)?;
        Ok(true)
    }

    /// Whether `document` and every chunk of it are current: nothing to re-chunk, no chunk pending,
    /// and no vector of its own left to derive.
    fn document_is_current(&self, db: &Db, document: ItemId) -> Result<bool> {
        let vector = VectorIndexer::new(self.embedder.clone());
        db.read_with::<bool, Error, _>(move |conn| {
            Ok(stale_chunks(conn, &vector, document)?.is_empty()
                && matches!(derivation(conn, &vector, document)?, Derivation::Current))
        })
    }

    /// Mark the ingestion of `source_hash` complete.
    fn mark_complete(&self, db: &Db, source_hash: &str) -> Result<()> {
        let hash = source_hash.to_owned();
        let strategy = self.strategy();
        let model = self.embedder.model().to_owned();
        let pipeline_version = self.version;
        db.write_txn_with::<(), Error, _>("ingest-embed", move |conn, _meta| {
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

    /// The items waiting for the embedder: those [`jkb_index::pending_predicate`] names, plus the
    /// documents [`Pipeline::embed_document`] has to re-derive ([`document_rule`]).
    ///
    /// Blank text — empty or whitespace-only by `str::trim`, decided here and only here (see
    /// [`is_blank`]) — is excluded: there is nothing to embed, and asking a model to embed `""`
    /// yields a zero-length vector. A blank line preserved for round-trip by the `tasks` file
    /// serializer is such an item. The one exception is a document whose chunks were cut from text
    /// it no longer holds: blank now, it still has old chunks to remove.
    fn pending_items(&self, db: &Db) -> Result<Vec<Pending>> {
        Ok(self
            .pending_rows(db)?
            .into_iter()
            .filter(|(p, rechunk)| !is_blank(&p.content) || *rechunk)
            .map(|(p, _)| p)
            .collect())
    }

    /// Every row [`Pipeline::pending_items`] considers, with whether it is a document to re-chunk,
    /// before the blank filter.
    fn pending_rows(&self, db: &Db) -> Result<Vec<(Pending, bool)>> {
        let vector = VectorIndexer::new(self.embedder.clone());
        db.read_with::<Vec<(Pending, bool)>, Error, _>(move |conn| {
            let sql = format!(
                "SELECT i.id, i.content, i.kind, i.content_rev,
                        i.chunked_rev IS NOT NULL AND i.chunked_rev <> i.content_rev
                   FROM items i
                  WHERE ({pending})
                     OR (i.kind = '{KIND_DOCUMENT}' AND i.content IS NOT NULL AND ({rule}))",
                pending = jkb_index::pending_predicate(conn, &vector, "i")?,
                rule = document_rule("i"),
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        Pending {
                            id: ItemId::new(r.get(0)?),
                            content: r.get::<_, String>(1)?,
                            kind: r.get::<_, String>(2)?,
                            rev: r.get(3)?,
                        },
                        r.get::<_, bool>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Delete the vectors of items whose text is now blank. Their vector describes text they no
    /// longer hold — a note emptied, a document edited to nothing — so a search for that text would
    /// still find them, and there is nothing to replace it with. Returns how many were removed.
    fn drop_blank_vectors(&self, db: &Db) -> Result<usize> {
        let blank: Vec<ItemId> = self
            .pending_rows(db)?
            .into_iter()
            .filter(|(p, rechunk)| is_blank(&p.content) && !*rechunk)
            .map(|(p, _)| p.id)
            .collect();
        if blank.is_empty() {
            return Ok(0);
        }
        let vector = VectorIndexer::new(self.embedder.clone());
        db.write_txn_with::<usize, Error, _>("index-pending", move |conn, _meta| {
            let held = vector.vectors_for(conn, &blank)?;
            for id in held.keys() {
                vector.remove(conn, *id)?;
            }
            Ok(held.len())
        })
    }

    /// The number of items waiting for the embedder: a vector missing, or computed from text the
    /// item no longer holds, or a document to re-derive (excluding blank text, which is not
    /// embeddable).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn unembedded_count(&self, db: &Db) -> Result<usize> {
        Ok(self.pending_items(db)?.len())
    }

    /// Embed every item whose vector is missing or out of date — never embedded, or edited since —
    /// replacing any old vector, and re-derive every document whose chunks changed (the `jkb index`
    /// pass, D21). Returns what it wrote.
    ///
    /// Items other than documents are embedded from their own text, in committed batches. Then every
    /// document still pending goes through [`Pipeline::embed_document`] — listed again **after** that
    /// pass, because a chunk whose vector was just written has a later `seq` than its document's,
    /// which is what makes the document pending ([`document_rule`]).
    ///
    /// # Errors
    /// Returns [`jkb_types::Error::EmbedderUnavailable`] (via [`Error::Types`]) if
    /// the embedder is down, or a database error.
    pub fn index_pending(&self, db: &Db) -> Result<IndexReport> {
        let pending = self.pending_items(db)?;
        let mut report = IndexReport::default();
        if pending.is_empty() {
            self.drop_blank_vectors(db)?;
            return Ok(report);
        }
        // Actionable failure if the embedder is down (D21 / task 7.5).
        self.embedder.health_check()?;

        let direct: Vec<Pending> = pending
            .into_iter()
            .filter(|p| p.kind != KIND_DOCUMENT)
            .collect();
        for batch in direct.chunks(EMBED_BATCH) {
            let embeddings = self.embed_each(batch, Failures::Count, &mut report)?;
            report.embedded += self.store_vectors(db, embeddings)?;
        }

        let documents: Vec<ItemId> = self
            .pending_items(db)?
            .into_iter()
            .filter(|p| p.kind == KIND_DOCUMENT)
            .map(|p| p.id)
            .collect();
        for document in documents {
            self.embed_document(db, document, Failures::Count, &mut report)?;
        }
        self.drop_blank_vectors(db)?;
        Ok(report)
    }

    /// Bring `document`'s vector up to date: **the only writer of a document vector**, called by
    /// both [`Pipeline::index_pending`] and the ingest path, so the two cannot come to disagree.
    ///
    /// 1. If its `chunked_rev` is not its `content_rev`, its chunks were cut from text it no longer
    ///    holds: they are removed and new ones cut from the current text, in one write transaction
    ///    with no model call ([`Pipeline::rechunk_if_stale`]).
    /// 2. Its pending chunks are embedded.
    /// 3. If it is pending — its own vector stale, or a chunk's written after it — its vector
    ///    becomes the **centroid of its current chunks'**, or an embedding of its own text only
    ///    when it has no chunks at all.
    ///
    /// Never a truncated self-embedding. A document's content is the whole source — a transcript
    /// can be a hundred times the embedder's context — and embedding it directly embeds a prefix
    /// that claims to describe the whole, ranking against queries about its opening and missing
    /// everything after. The chunks already cover the whole text, so their centroid is both cheaper
    /// (no model call) and more faithful. The shelved `vector_stale` design got this wrong three
    /// ways, each a rule some other writer had to remember; here the rule is this function.
    ///
    /// Embedder errors are counted into `report` or propagated, per `failures`.
    ///
    /// # Errors
    /// A database error, or (with [`Failures::Propagate`]) the embedder's.
    fn embed_document(
        &self,
        db: &Db,
        document: ItemId,
        failures: Failures,
        report: &mut IndexReport,
    ) -> Result<()> {
        self.rechunk_if_stale(db, document)?;

        let vector = VectorIndexer::new(self.embedder.clone());
        let stale = {
            let vector = VectorIndexer::new(self.embedder.clone());
            db.read_with::<Vec<Pending>, Error, _>(move |conn| {
                stale_chunks(conn, &vector, document)
            })?
        };
        for batch in stale.chunks(EMBED_BATCH) {
            let embeddings = self.embed_each(batch, failures, report)?;
            report.embedded += self.store_vectors(db, embeddings)?;
        }

        let plan =
            db.read_with::<Derivation, Error, _>(move |conn| derivation(conn, &vector, document))?;
        match plan {
            // Nothing to write, or an edit raced this run and the document re-chunks next time.
            Derivation::Current | Derivation::Wait => {}
            Derivation::Centroid { rev, vectors } => {
                if let Some(c) = centroid(&vectors) {
                    report.derived += self.store_vectors(db, vec![(document, rev, c)])?;
                }
            }
            Derivation::OwnText { rev, text } => {
                let own = Pending {
                    id: document,
                    content: text,
                    kind: KIND_DOCUMENT.to_owned(),
                    rev,
                };
                let embeddings = self.embed_each(std::slice::from_ref(&own), failures, report)?;
                report.embedded += self.store_vectors(db, embeddings)?;
            }
        }
        Ok(())
    }

    /// Re-cut `document`'s chunks if they were cut from text it no longer holds — its
    /// `chunked_rev` is set and is not its `content_rev`. Returns whether it did.
    ///
    /// One write transaction with no model call: the old chunk items are removed (and their
    /// vectors with them, by the GC trigger and `vector_sources`' cascade), new ones are cut from
    /// the current text, and `chunked_rev` is set beside them. Read and cut inside the transaction,
    /// so no edit can land between the text read and the rev recorded for it.
    ///
    /// A new chunk's uid is `<document uid>:<blake3 of the text>:<idx>`. The document's uid keeps
    /// it inside the `b3:<source hash>:` prefix every lookup of a capture's chunks matches; the text
    /// hash keeps a re-cut from colliding with the captured `b3:<source hash>:<idx>` uids or with
    /// another document edited to the same text, which a bare `b3:<text hash>:<idx>` would — two
    /// documents would then share chunk items.
    ///
    /// Removing a chunk is an ordinary changelogged delete, so `jkb undo` of a re-chunk restores
    /// the old chunks and the old `chunked_rev`, which re-chunks again at the next pass: the chunks
    /// follow the text, not the undo.
    fn rechunk_if_stale(&self, db: &Db, document: ItemId) -> Result<bool> {
        let chunking = self.chunking;
        db.write_txn_with::<bool, Error, _>(self.actor, move |conn, meta| {
            let row: Option<DocumentRow> = conn
                .prepare_cached(
                    "SELECT uid, content, content_rev, chunked_rev, mime FROM items WHERE id = ?1",
                )?
                .query_row([document.get()], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .optional()?;
            let Some((uid, content, rev, Some(chunked), mime)) = row else {
                // Gone, or never cut by the chunker: nothing of its own to re-cut.
                return Ok(false);
            };
            if chunked == rev {
                return Ok(false);
            }
            for old in chunk_ids_of(conn, document)? {
                item::remove(conn, meta, old, true)?;
            }
            let text = content.unwrap_or_default();
            let chunks = chunk::chunk_text(&text, &chunking);
            let text_hash = blake3::hash(text.as_bytes()).to_hex();
            write_chunks(
                conn,
                meta,
                &Cut {
                    document,
                    rev,
                    uid_prefix: &format!("{uid}:{text_hash}"),
                    chunks: &chunks,
                    mime: mime.as_deref(),
                    namespace: primary_namespace_id(conn, document)?,
                },
            )?;
            Ok(true)
        })
    }

    /// Embed each of `items` from its own text, off the writer thread. A rejected item is counted
    /// into `report` and skipped, or fails the call, per `failures`.
    ///
    /// Counting is the backfill's policy: one bad item must not discard the whole run. A run over
    /// tens of thousands of items will meet a transient failure eventually; losing every other
    /// item's work to it makes the mop-up path unusable exactly when it is most needed. The item
    /// stays pending, so a later run retries it.
    fn embed_each(
        &self,
        items: &[Pending],
        failures: Failures,
        report: &mut IndexReport,
    ) -> Result<Vec<Embedded>> {
        let mut embeddings = Vec::with_capacity(items.len());
        for item in items {
            match self.embedder.embed(&item.content) {
                Ok(v) => embeddings.push((item.id, item.rev, v)),
                Err(e) if failures == Failures::Count => {
                    report.failed += 1;
                    report.note_failure(&e.to_string());
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(embeddings)
    }

    /// Write a batch of vectors in one transaction, returning how many landed. An item deleted or
    /// edited since it was read for embedding gets no vector (`upsert_vector` checks its rev) and
    /// is not counted; an edited one stays pending for the next run.
    ///
    /// Committing per batch rather than once at the end is what makes a long backfill
    /// resumable: interrupting it keeps everything already written, and re-running picks up
    /// only what is still missing.
    fn store_vectors(&self, db: &Db, embeddings: Vec<Embedded>) -> Result<usize> {
        if embeddings.is_empty() {
            return Ok(0);
        }
        let vector = VectorIndexer::new(self.embedder.clone());
        db.write_txn_with::<usize, Error, _>("index-pending", move |conn, _meta| {
            vector.ensure_ready(conn)?;
            let mut landed = 0;
            for (id, rev, embedding) in &embeddings {
                if vector.upsert_vector(conn, *id, *rev, embedding)? {
                    landed += 1;
                }
            }
            Ok(landed)
        })
    }
}

/// Whether `text` has nothing to embed. **The** blankness test: SQL's `trim` strips only ASCII
/// spaces, so a run of no-break spaces (as PDF extraction yields) was blank to one check and not to
/// another, and a chunk that was never embedded held its document pending forever.
fn is_blank(text: &str) -> bool {
    text.trim().is_empty()
}

/// The SQL condition under which document `alias` must be re-derived even if its own vector's
/// provenance matches: its chunks were cut from an earlier revision of its text, or a chunk's
/// vector was written after its own (`vector_sources.seq`), so its centroid predates it.
///
/// [`jkb_index::pending_predicate`] covers the document's own row; this is the rule a document
/// adds because its vector derives from its chunks rather than its text. `alias` is one of this
/// crate's identifiers, never input.
fn document_rule(alias: &str) -> String {
    format!(
        "({alias}.chunked_rev IS NOT NULL AND {alias}.chunked_rev <> {alias}.content_rev)
         OR EXISTS (SELECT 1 FROM edges e
                      JOIN items c ON c.id = e.src_item_id AND c.kind = 'chunk'
                      JOIN vector_sources cs ON cs.item_id = c.id
                     WHERE e.dst_item_id = {alias}.id AND e.type = 'derived_from'
                       AND cs.seq > (SELECT ds.seq FROM vector_sources ds
                                      WHERE ds.item_id = {alias}.id))"
    )
}

/// `document`'s chunks that [`jkb_index::pending_predicate`] names, minus blank ones.
fn stale_chunks(
    conn: &Connection,
    vector: &VectorIndexer,
    document: ItemId,
) -> Result<Vec<Pending>> {
    let sql = format!(
        "SELECT c.id, c.content, c.kind, c.content_rev FROM edges e
           JOIN items c ON c.id = e.src_item_id
          WHERE e.dst_item_id = ?1 AND e.type = 'derived_from' AND c.kind = 'chunk'
            AND {pending}
          ORDER BY c.id",
        pending = jkb_index::pending_predicate(conn, vector, "c")?
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt
        .query_map([document.get()], |r| {
            Ok(Pending {
                id: ItemId::new(r.get(0)?),
                content: r.get(1)?,
                kind: r.get(2)?,
                rev: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.retain(|p| !is_blank(&p.content));
    Ok(rows)
}

/// What [`Pipeline::embed_document`] is to write for a document, decided in one read.
enum Derivation {
    /// Its vector is current, or it has nothing to embed.
    Current,
    /// Pending, but not derivable now: its text changed since its chunks were cut (an edit raced
    /// this run), or none of its chunks has a current vector yet. It stays pending.
    Wait,
    /// The centroid of these chunk vectors, stored under content revision `rev`.
    Centroid { rev: i64, vectors: Vec<Vec<f32>> },
    /// No chunks at all: an embedding of its own `text`, stored under `rev`.
    OwnText { rev: i64, text: String },
}

/// Decide `document`'s vector: whether it is pending ([`jkb_index::pending_predicate`] or
/// [`document_rule`]), and if so from what.
fn derivation(conn: &Connection, vector: &VectorIndexer, document: ItemId) -> Result<Derivation> {
    let sql = format!(
        "SELECT d.content, d.content_rev, d.chunked_rev, ({pending}) OR ({rule})
           FROM items d WHERE d.id = ?1",
        pending = jkb_index::pending_predicate(conn, vector, "d")?,
        rule = document_rule("d"),
    );
    let row: Option<(Option<String>, i64, Option<i64>, bool)> = conn
        .prepare(&sql)?
        .query_row([document.get()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?;
    let Some((Some(text), rev, chunked, true)) = row else {
        return Ok(Derivation::Current);
    };
    if chunked.is_some_and(|c| c != rev) {
        return Ok(Derivation::Wait);
    }

    // Every non-blank chunk, and whether its vector is current.
    let sql = format!(
        "SELECT c.id, c.content, NOT ({pending}) FROM edges e
           JOIN items c ON c.id = e.src_item_id
          WHERE e.dst_item_id = ?1 AND e.type = 'derived_from' AND c.kind = 'chunk'",
        pending = jkb_index::pending_predicate(conn, vector, "c")?
    );
    let mut stmt = conn.prepare(&sql)?;
    let chunks = stmt
        .query_map([document.get()], |r| {
            Ok((
                ItemId::new(r.get(0)?),
                r.get::<_, Option<String>>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let chunks: Vec<(ItemId, bool)> = chunks
        .into_iter()
        .filter(|(_, content, _)| content.as_deref().is_some_and(|c| !is_blank(c)))
        .map(|(id, _, current)| (id, current))
        .collect();

    if chunks.is_empty() {
        // Short by construction when ingest made it — every non-empty text yields a chunk — so
        // this is a document no chunker cut (a synced file), or one with nothing to embed.
        return Ok(if is_blank(&text) {
            Derivation::Current
        } else {
            Derivation::OwnText { rev, text }
        });
    }
    // Only current vectors: a chunk the embedder rejected keeps its old one, if any, and the
    // centroid is taken without it. Its vector, when it lands, has a later `seq` than the
    // document's, so the document is re-centred then.
    let current: Vec<ItemId> = chunks
        .iter()
        .filter(|(_, current)| *current)
        .map(|(id, _)| *id)
        .collect();
    let vectors: Vec<Vec<f32>> = vector.vectors_for(conn, &current)?.into_values().collect();
    Ok(if vectors.is_empty() {
        Derivation::Wait
    } else {
        Derivation::Centroid { rev, vectors }
    })
}

/// A document as [`Pipeline::rechunk_if_stale`] reads it: uid, content, `content_rev`,
/// `chunked_rev`, mime.
type DocumentRow = (String, Option<String>, i64, Option<i64>, Option<String>);

/// One cut of a document's chunks, for [`write_chunks`].
struct Cut<'a> {
    /// The document the chunks are derived from.
    document: ItemId,
    /// The document's `content_rev` the chunks were cut from, recorded as its `chunked_rev`.
    rev: i64,
    /// Each chunk's uid is `<uid_prefix>:<idx>`.
    uid_prefix: &'a str,
    chunks: &'a [String],
    mime: Option<&'a str>,
    /// Where the chunks are placed (the document's home), if it has one.
    namespace: Option<NamespaceId>,
}

/// Write `cut`'s chunk items — `derived_from` the document, placed in its namespace by position,
/// contained by it — and record the revision they were cut from (`items.chunked_rev`) beside them.
/// The one writer of chunks, shared by capture and re-chunking, so the two cut the same shape.
fn write_chunks(conn: &Connection, meta: &WriteMeta, cut: &Cut<'_>) -> Result<()> {
    for (idx, chunk) in cut.chunks.iter().enumerate() {
        let position = i64::try_from(idx).map_err(|_| {
            Error::Types(jkb_types::Error::Validation("too many chunks".to_owned()))
        })?;
        let chunk_id = item::upsert(
            conn,
            meta,
            &NewItem {
                uid: format!("{}:{idx}", cut.uid_prefix),
                kind: "chunk".to_owned(),
                content: Some(chunk.clone()),
                content_hash: None,
                mime: cut.mime.map(str::to_owned),
            },
        )?;
        edge::link(
            conn,
            meta,
            chunk_id,
            cut.document,
            EdgeType::DerivedFrom,
            None,
        )?;
        if let Some(ns_id) = cut.namespace {
            placement::place(conn, meta, chunk_id, ns_id, PlacementRole::Chunk, position)?;
        }
        // The document CONTAINS its chunks (design D35), so they are listed
        // by expanding it rather than as flat siblings beside it.
        jkb_core::containment::contain(conn, meta, chunk_id, cut.document, position)?;
    }
    item::set_chunked_rev(conn, meta, cut.document, cut.rev)?;
    Ok(())
}

/// `item`'s current `content_rev`.
fn content_rev(conn: &Connection, item: ItemId) -> Result<i64> {
    Ok(conn
        .prepare_cached("SELECT content_rev FROM items WHERE id = ?1")?
        .query_row([item.get()], |r| r.get(0))?)
}

/// The namespace `item` is primarily placed in, if any.
fn primary_namespace_id(conn: &Connection, item: ItemId) -> Result<Option<NamespaceId>> {
    Ok(conn
        .prepare_cached(
            "SELECT namespace_id FROM placements WHERE item_id = ?1 AND role = 'primary'
              ORDER BY id LIMIT 1",
        )?
        .query_row([item.get()], |r| r.get::<_, i64>(0))
        .optional()?
        .map(NamespaceId::new))
}

/// Whether [`Pipeline::embed_document`] and [`Pipeline::embed_each`] count an embedder error and
/// carry on (a backfill) or fail with it (an ingest of one source).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Failures {
    Count,
    Propagate,
}

/// A vector ready to store: the item, the content revision it was computed from (for a document,
/// the revision its chunks were cut from), and the vector.
type Embedded = (ItemId, i64, Vec<f32>);

/// The item kind ingest stores a whole source document as. Its chunks are separate items
/// linked back to it by `derived_from`.
const KIND_DOCUMENT: &str = "document";

/// How many vectors to compute before committing. Small enough that an interrupted backfill
/// loses at most this much work, large enough that per-transaction overhead stays noise
/// against the model call that dominates each item.
const EMBED_BATCH: usize = 256;

/// One item awaiting a vector: never embedded, or edited since it was.
struct Pending {
    id: ItemId,
    content: String,
    kind: String,
    /// Its `content_rev` when `content` was read; the vector is stored only if it still is.
    rev: i64,
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

/// The address of a document known only by its `text`: blake3 in key-derivation mode under a context of
/// its own, which no plain blake3 of any bytes can equal — so a client's text and a host file are two
/// documents even when the file's text is its bytes, and a client cannot take the uid, the ingestion row
/// or the resume a host file's ingest would use. A hash of a prefix and the text was not that: a file
/// whose bytes began with the prefix hashed to the same address, and one the container wrote for the host
/// to ingest later landed in the container's capture (stage-6.3 review 2).
#[must_use]
pub fn text_address(text: &str) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("jkb 2026-09-15 ingest.text document address");
    hasher.update(text.as_bytes());
    hasher.finalize().to_hex().to_string()
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

/// The `GLOB` pattern matching every chunk uid of `hash` (`b3:<hash>:<idx>`); bind it to
/// `uid GLOB ?`.
///
/// `hash` is blake3 hex today (`blob::hash_bytes` or [`text_address`]), which holds no `GLOB`
/// metacharacter, so the escape is a no-op for every value ingest produces. It is escaped anyway,
/// through the same [`jkb_core::sql::prefix_glob`] every other prefix match uses, so that safety
/// is a property of this one function rather than of what each caller happens to pass, and these
/// sites read like their siblings instead of like the unescaped patterns that were bugs.
fn chunk_uid_glob(hash: &str) -> String {
    jkb_core::sql::prefix_glob(&format!("b3:{hash}"), ':')
}

/// Items left over from a previous ingestion of `hash` — its chunks, and the document if it
/// is somehow still there — so a re-capture can start from nothing.
fn surviving_fragments(conn: &Connection, hash: &str) -> Result<Vec<ItemId>> {
    let mut stmt =
        conn.prepare_cached("SELECT id FROM items WHERE uid = ?1 OR uid GLOB ?2 ORDER BY id DESC")?;
    let rows = stmt.query_map(
        rusqlite::params![format!("b3:{hash}"), chunk_uid_glob(hash)],
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
        .prepare_cached("SELECT count(*) FROM items WHERE uid GLOB ?1")?
        .query_row([chunk_uid_glob(hash)], |r| r.get(0))?;
    Ok(usize::try_from(n).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(uid: &str, content: &str) -> NewItem {
        NewItem {
            uid: uid.to_owned(),
            kind: "chunk".to_owned(),
            content: Some(content.to_owned()),
            content_hash: None,
            mime: None,
        }
    }

    /// A chunk-uid prefix match takes `hash` literally. Ingest only ever passes blake3 hex, so
    /// this pins the property of [`chunk_uid_glob`] rather than a reachable input: a `?` in
    /// `hash` must not act as a one-character wildcard, nor its case fold, and pull in a sibling
    /// document's chunks, which would let a re-capture delete them and a resume embed them as its
    /// own.
    #[test]
    fn chunk_uid_matches_take_the_hash_literally() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            item::upsert(conn, meta, &note("b3:a?c", "the document"))?;
            item::upsert(conn, meta, &note("b3:a?c:0", "its chunk"))?;
            item::upsert(conn, meta, &note("b3:abc", "a sibling document"))?;
            item::upsert(conn, meta, &note("b3:abc:0", "the sibling's chunk"))?;
            item::upsert(conn, meta, &note("b3:A?C:0", "a case variant's chunk"))?;
            Ok(())
        })
        .unwrap();

        db.read(|conn| {
            assert_eq!(count_chunks(conn, "a?c").unwrap(), 1);
            assert_eq!(
                surviving_fragments(conn, "a?c").unwrap().len(),
                2,
                "the document and its own chunk, not the sibling's"
            );
            Ok(())
        })
        .unwrap();
    }
}
