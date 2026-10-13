//! End-to-end ingestion tests over a real migrated DB with `sqlite-vec` registered.
//!
//! A deterministic fake embedder (optionally "down") stands in for ollama, so the
//! staging, idempotency, resume, and non-blocking-capture behaviours are exercised
//! without a network.

use std::sync::Arc;

use jkb_core::Db;
use jkb_index::{FtsIndexer, StaleRows, VectorIndexer};
use jkb_ingest::adapter::ParsedDocument;
use jkb_ingest::chunk::ChunkConfig;
use jkb_ingest::{blob, Pipeline};
use jkb_types::{Embedder, Result as TypesResult};
use proptest::prelude::*;

/// Deterministic offline embedder; `healthy=false` makes `health_check` fail so we
/// can exercise the "embedder down" path.
struct FakeEmbedder {
    healthy: bool,
    /// Reject any input longer than this, as ollama does past its context window. `None`
    /// accepts anything.
    max_chars: Option<usize>,
}

impl FakeEmbedder {
    fn arc(healthy: bool) -> Arc<dyn Embedder + Send + Sync> {
        Arc::new(Self {
            healthy,
            max_chars: None,
        })
    }

    /// An embedder that refuses over-long input, standing in for ollama's
    /// "the input length exceeds the context length".
    fn arc_with_limit(max_chars: usize) -> Arc<dyn Embedder + Send + Sync> {
        Arc::new(Self {
            healthy: true,
            max_chars: Some(max_chars),
        })
    }
}

impl Embedder for FakeEmbedder {
    #[allow(clippy::unnecessary_literal_bound)] // trait fixes the `-> &str` signature
    fn model(&self) -> &str {
        "fake"
    }
    fn dim(&self) -> usize {
        16
    }
    fn embed(&self, text: &str) -> TypesResult<Vec<f32>> {
        if self.max_chars.is_some_and(|m| text.chars().count() > m) {
            return Err(jkb_types::Error::EmbedderUnavailable(
                "test: the input length exceeds the context length".to_owned(),
            ));
        }
        let mut v = vec![0.0f32; 16];
        for (i, b) in text.bytes().enumerate() {
            v[i % 16] += f32::from(b);
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        Ok(v)
    }
    fn health_check(&self) -> TypesResult<()> {
        if self.healthy {
            Ok(())
        } else {
            Err(jkb_types::Error::EmbedderUnavailable(
                "test: down".to_owned(),
            ))
        }
    }
    fn resolved_version(&self) -> TypesResult<Option<String>> {
        Ok(Some("fake:v1".to_owned()))
    }
}

fn open_db() -> Db {
    Db::open_in_memory_with(&[jkb_index::register]).unwrap()
}

fn pipeline(healthy: bool) -> Pipeline {
    // Small windows so ordinary test text produces multiple chunks.
    Pipeline::new(FakeEmbedder::arc(healthy))
        .with_chunking(ChunkConfig::new(40, 10, 10).expect("valid chunk config"))
}

fn doc(text: &str) -> ParsedDocument {
    ParsedDocument {
        title: None,
        text: text.to_owned(),
        mime: "text/plain".to_owned(),
    }
}

/// Live URL ingestion: renders a page in a headless browser and captures its text.
/// Ignored by default — needs a network connection and a local Chrome/Chromium.
#[test]
#[ignore = "requires a network connection and a local Chrome/Chromium"]
fn ingest_url_renders_and_captures() {
    let db = open_db();
    let outcome = Pipeline::new(FakeEmbedder::arc(true))
        .ingest_url(&db, "https://example.com/", "web")
        .unwrap();
    assert!(!outcome.already_ingested);
    assert!(outcome.chunk_count >= 1);
    // example.com renders the phrase "Example Domain".
    let hits: i64 = db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM fts_items WHERE fts_items MATCH 'Example'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(hits >= 1);
}

fn item_count(db: &Db) -> i64 {
    db.read(|conn| Ok(conn.query_row("SELECT count(*) FROM items", [], |r| r.get(0))?))
        .unwrap()
}

fn fts_hits(db: &Db, term: &str) -> usize {
    let term = term.to_owned();
    db.read(move |conn| Ok(FtsIndexer::new().search(conn, &term, 50).unwrap()))
        .unwrap()
        .len()
}

#[test]
fn ingest_captures_embeds_and_is_searchable() {
    let db = open_db();
    let text = "The Rust programming language emphasizes safety and performance \
                across systems software without a garbage collector.";
    let outcome = pipeline(true)
        .ingest(&db, Some(text.as_bytes()), &doc(text), "docs")
        .unwrap();

    assert!(outcome.embedded);
    assert!(!outcome.already_ingested);
    assert!(outcome.chunk_count >= 2, "expected multiple chunks");
    assert!(
        fts_hits(&db, "garbage") >= 1,
        "keyword-searchable after capture"
    );

    // Vector search returns item ids (document + chunks were embedded).
    let embedder = FakeEmbedder::arc(true);
    let hits = db
        .read(move |conn| {
            let q = embedder.embed("safety and performance").unwrap();
            Ok(VectorIndexer::new(embedder.clone())
                .knn(conn, &q, 3)
                .unwrap())
        })
        .unwrap();
    assert!(!hits.is_empty(), "vector search finds embedded items");
}

#[test]
fn capture_survives_embedder_down_and_is_keyword_searchable() {
    let db = open_db();
    let text = "Photosynthesis converts light energy into chemical energy in plant chloroplasts.";
    let outcome = pipeline(false)
        .ingest(&db, Some(text.as_bytes()), &doc(text), "docs")
        .unwrap();

    assert!(!outcome.embedded, "embedder down => not embedded");
    assert!(!outcome.already_ingested);
    assert!(
        !outcome.warnings.is_empty(),
        "should warn about the down embedder"
    );
    assert!(
        fts_hits(&db, "chloroplasts") >= 1,
        "captured => keyword-searchable at once"
    );
    assert!(
        pipeline(false).unembedded_count(&db).unwrap() >= 1,
        "un-embedded items are reported"
    );
}

#[test]
fn index_pending_embeds_captured_items() {
    let db = open_db();
    let text = "Distributed systems must reason about partial failure and network partitions.";
    pipeline(false)
        .ingest(&db, Some(text.as_bytes()), &doc(text), "docs")
        .unwrap();
    let before = pipeline(true).unembedded_count(&db).unwrap();
    assert!(before >= 1);

    let report = pipeline(true).index_pending(&db).unwrap();
    assert_eq!(report.total(), before, "all pending items get embedded");
    assert_eq!(pipeline(true).unembedded_count(&db).unwrap(), 0);
}

#[test]
fn resume_embeds_after_embedder_recovers() {
    let db = open_db();
    let text = "Tail latency dominates user-perceived performance in fan-out request services.";

    // First ingest with the embedder down: captured, not embedded.
    let down = pipeline(false)
        .ingest(&db, Some(text.as_bytes()), &doc(text), "docs")
        .unwrap();
    assert!(!down.embedded);

    // Re-ingest with a healthy embedder: resumes at the embed stage (not a no-op).
    let up = pipeline(true)
        .ingest(&db, Some(text.as_bytes()), &doc(text), "docs")
        .unwrap();
    assert!(up.embedded);
    assert!(!up.already_ingested, "was resumed, not already complete");

    // A third ingest is now a true no-op.
    let again = pipeline(true)
        .ingest(&db, Some(text.as_bytes()), &doc(text), "docs")
        .unwrap();
    assert!(again.already_ingested);
    assert_eq!(pipeline(true).unembedded_count(&db).unwrap(), 0);
}

#[test]
fn duplicate_source_is_stored_once() {
    let db = open_db();
    let text = "Idempotent ingestion means re-running is a no-op.";
    pipeline(true)
        .ingest(&db, Some(text.as_bytes()), &doc(text), "docs")
        .unwrap();
    let items_after_first = item_count(&db);

    // Same bytes again (as if from a different path): no new items, one blob.
    let second = pipeline(true)
        .ingest(&db, Some(text.as_bytes()), &doc(text), "docs")
        .unwrap();
    assert!(second.already_ingested);
    assert_eq!(item_count(&db), items_after_first, "no duplicate items");

    let blobs: i64 = db
        .read(|conn| Ok(conn.query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(blobs, 1, "identical raw source stored once");
}

#[test]
fn capture_failure_rolls_back_leaving_no_partial_items() {
    let db = open_db();
    let text = "abcdefghij ".repeat(12); // > max_chars => multiple chunks
    let hash = blob::hash_bytes(text.as_bytes());

    // Pre-insert an item colliding with the SECOND chunk uid, so capture fails only
    // after the document and first chunk have been created within the transaction.
    let collider = format!("b3:{hash}:1");
    db.write_txn("seed", {
        let collider = collider.clone();
        move |conn, meta| {
            jkb_core::item::upsert(
                conn,
                meta,
                &jkb_core::item::NewItem {
                    uid: collider,
                    kind: "note".to_owned(),
                    content: Some("pre-existing".to_owned()),
                    content_hash: None,
                    mime: None,
                },
            )?;
            Ok(())
        }
    })
    .unwrap();
    let before = item_count(&db);

    let result = pipeline(true).ingest(&db, Some(text.as_bytes()), &doc(&text), "docs");
    assert!(result.is_err(), "colliding chunk uid must fail the capture");

    // Nothing partial: the document was rolled back, only the seeded collider remains.
    assert_eq!(item_count(&db), before, "capture rolled back cleanly");
    let doc_exists: Option<i64> = db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT id FROM items WHERE uid = ?1",
                    [format!("b3:{hash}")],
                    |r| r.get(0),
                )
                .ok())
        })
        .unwrap();
    assert!(
        doc_exists.is_none(),
        "no document item survived the rollback"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Ingesting the same bytes twice is a no-op the second time, with no new items.
    #[test]
    fn reingest_is_a_noop(body in "[a-zA-Z0-9 ]{20,200}") {
        let db = open_db();
        let first = pipeline(true).ingest(&db, Some(body.as_bytes()), &doc(&body), "docs").unwrap();
        prop_assert!(!first.already_ingested);
        let count_after_first = item_count(&db);

        let second = pipeline(true).ingest(&db, Some(body.as_bytes()), &doc(&body), "docs").unwrap();
        prop_assert!(second.already_ingested);
        prop_assert_eq!(item_count(&db), count_after_first);
    }
}

#[test]
fn empty_and_whitespace_items_are_excluded_from_the_embed_pass() {
    use jkb_core::item::{upsert, NewItem};

    // A file serializer (e.g. `tasks`) preserves blank lines as empty-content items.
    // Those are not embeddable — asking a real model to embed "" yields a zero-length
    // vector — so the embed pass must skip them.
    let db = open_db();
    let mk = |uid: &str, content: &str| NewItem {
        uid: uid.to_owned(),
        kind: "text".to_owned(),
        content: Some(content.to_owned()),
        content_hash: None,
        mime: None,
    };
    db.write_txn("t", move |conn, meta| {
        upsert(conn, meta, &mk("real", "real content to embed"))?;
        upsert(conn, meta, &mk("blank", ""))?;
        upsert(conn, meta, &mk("whitespace", "  \n\t "))?;
        Ok(())
    })
    .unwrap();

    let p = pipeline(true);
    // Only the one real-content item counts as pending.
    assert_eq!(p.unembedded_count(&db).unwrap(), 1);
    // The embed pass embeds exactly that one (never sending empty content to the model).
    assert_eq!(p.index_pending(&db).unwrap().total(), 1);
    assert_eq!(p.unembedded_count(&db).unwrap(), 0);
}

/// The regression this whole change exists for: one item the embedder rejects must not
/// discard the whole backfill. Before, `index_pending` propagated the first error, so a
/// single oversized item threw away every other item's vector.
#[test]
fn index_pending_skips_a_rejected_item_and_keeps_the_rest() {
    let db = open_db();
    // Capture with the embedder down so everything lands in the pending set.
    let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu ".repeat(4);
    pipeline(false)
        .ingest(&db, Some(text.as_bytes()), &doc(&text), "docs")
        .unwrap();
    // An oversized non-document item, which has no chunks to fall back on.
    let huge = "x".repeat(500);
    db.write_txn("t", move |conn, meta| {
        let id = jkb_core::item::upsert(
            conn,
            meta,
            &jkb_core::item::NewItem {
                uid: "note:huge".to_owned(),
                kind: "note".to_owned(),
                content: Some(huge.clone()),
                content_hash: None,
                mime: None,
            },
        )?;
        let ns = jkb_core::ns::ensure(conn, meta, "docs")?;
        jkb_core::placement::place(conn, meta, id, ns, jkb_types::PlacementRole::Primary, 0)
    })
    .unwrap();

    let pipe = Pipeline::new(FakeEmbedder::arc_with_limit(120));
    let report = pipe.index_pending(&db).unwrap();

    // The run SUCCEEDED, skipped only the offender, and still wrote everything else.
    assert_eq!(report.failed, 1, "{report:?}");
    assert!(report.embedded > 0, "{report:?}");
    assert_eq!(
        report.derived, 1,
        "the document is derived, not embedded: {report:?}"
    );
    assert!(
        report
            .first_error
            .as_deref()
            .unwrap_or("")
            .contains("context length"),
        "{report:?}"
    );
    // Only the rejected item is still pending — everything else was committed.
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 1);
}

/// An embedder that deletes `victim` the moment it is asked to embed `trigger`: an item removed
/// while a backfill is between reading it and storing its vector.
struct DeletingEmbedder {
    db: Db,
    trigger: &'static str,
    victim: &'static str,
}

impl Embedder for DeletingEmbedder {
    #[allow(clippy::unnecessary_literal_bound)] // trait fixes the `-> &str` signature
    fn model(&self) -> &str {
        "fake"
    }
    fn dim(&self) -> usize {
        16
    }
    fn embed(&self, text: &str) -> TypesResult<Vec<f32>> {
        if text == self.trigger {
            let victim = self.victim;
            self.db
                .write_txn("t", move |conn, meta| {
                    let id = jkb_core::item::id_for_uid(conn, victim)?
                        .expect("setup: the victim exists");
                    jkb_core::item::remove(conn, meta, id, true)
                })
                .unwrap();
        }
        let mut v = vec![0.0f32; 16];
        v[text.len() % 16] = 1.0;
        Ok(v)
    }
    fn health_check(&self) -> TypesResult<()> {
        Ok(())
    }
    fn resolved_version(&self) -> TypesResult<Option<String>> {
        Ok(Some("fake:v1".to_owned()))
    }
}

/// An item deleted mid-backfill gets no vector. Embedding runs outside any transaction, so the
/// item is gone by the time its batch is stored, after the GC trigger has already fired; writing
/// the vector then recreated exactly the orphan the trigger exists to prevent.
#[test]
fn index_pending_writes_no_vector_for_an_item_deleted_mid_run() {
    let db = open_db();
    let mk = |uid: &str, content: &str| jkb_core::item::NewItem {
        uid: uid.to_owned(),
        kind: "note".to_owned(),
        content: Some(content.to_owned()),
        content_hash: None,
        mime: None,
    };
    db.write_txn("t", move |conn, meta| {
        jkb_core::item::upsert(conn, meta, &mk("note:keep", "kept content"))?;
        jkb_core::item::upsert(conn, meta, &mk("note:doomed", "doomed content"))?;
        Ok(())
    })
    .unwrap();

    let pipe = Pipeline::new(Arc::new(DeletingEmbedder {
        db: db.clone(),
        trigger: "doomed content",
        victim: "note:doomed",
    }));
    let report = pipe.index_pending(&db).unwrap();

    assert_eq!(
        db.read(|conn| {
            let StaleRows { vectors } = jkb_index::count_stale(conn).unwrap();
            Ok(vectors)
        })
        .unwrap(),
        0,
        "a vector was written for the deleted item"
    );
    assert_eq!(
        report.embedded, 1,
        "only the surviving item's vector landed: {report:?}"
    );
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 0);
}

/// A vector row whose item is gone leaves the pending set exactly as if it were not there: the
/// candidates come from `items`, so an orphan's id names no candidate. Built on a store whose GC
/// trigger has been dropped, which is how a pre-trigger database comes to hold such a row.
#[test]
fn an_orphan_vector_row_neither_hides_nor_adds_a_pending_item() {
    let db = open_db();
    let mk = |uid: &str, content: &str| jkb_core::item::NewItem {
        uid: uid.to_owned(),
        kind: "note".to_owned(),
        content: Some(content.to_owned()),
        content_hash: None,
        mime: None,
    };
    db.write_txn("t", move |conn, meta| {
        jkb_core::item::upsert(conn, meta, &mk("note:kept", "kept content"))?;
        jkb_core::item::upsert(conn, meta, &mk("note:gone", "gone content"))?;
        Ok(())
    })
    .unwrap();
    let pipe = pipeline(true);
    assert_eq!(pipe.index_pending(&db).unwrap().total(), 2);

    db.write_txn("t", move |conn, meta| {
        conn.execute_batch("DROP TRIGGER vec_items_16_gc")?;
        let gone = jkb_core::item::id_for_uid(conn, "note:gone")?.expect("setup: item exists");
        jkb_core::item::remove(conn, meta, gone, true)?;
        jkb_core::item::upsert(conn, meta, &mk("note:new", "new content"))?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        db.read(|conn| {
            let StaleRows { vectors } = jkb_index::count_stale(conn).unwrap();
            Ok(vectors)
        })
        .unwrap(),
        1,
        "setup: the deleted item's vector is left behind"
    );

    assert_eq!(
        pipe.unembedded_count(&db).unwrap(),
        1,
        "only the new item is pending"
    );
    assert_eq!(pipe.index_pending(&db).unwrap().embedded, 1);
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 0);
}

/// A document's vector is the mean of its chunks', not an embedding of its truncated text:
/// a vector claiming to describe a document while describing only its opening ranks against
/// queries about the opening and misses everything after it.
#[test]
fn a_documents_vector_is_the_centroid_of_its_chunks() {
    let db = open_db();
    let pipe = pipeline(true);
    let text = "one two three four five six seven eight nine ten eleven twelve ".repeat(6);
    let outcome = pipe
        .ingest(&db, Some(text.as_bytes()), &doc(&text), "docs")
        .unwrap();
    assert!(outcome.chunk_count > 1, "need several chunks to average");

    let indexer = VectorIndexer::new(FakeEmbedder::arc(true));
    let doc_id = outcome.document;
    let (doc_vec, chunk_vecs) = db
        .read(move |conn| {
            let chunk_ids: Vec<_> = {
                let mut stmt = conn.prepare(
                    "SELECT src_item_id FROM edges WHERE dst_item_id = ?1 AND type = 'derived_from'",
                )?;
                let rows = stmt.query_map([doc_id.get()], |r| {
                    Ok(jkb_types::ItemId::new(r.get::<_, i64>(0)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let chunks = indexer.vectors_for(conn, &chunk_ids).unwrap();
            let d = indexer.vectors_for(conn, &[doc_id]).unwrap();
            Ok((
                d.get(&doc_id).cloned().expect("document has a vector"),
                chunks.into_values().collect::<Vec<_>>(),
            ))
        })
        .unwrap();

    assert_eq!(chunk_vecs.len(), outcome.chunk_count);
    #[allow(clippy::cast_precision_loss)]
    let n = chunk_vecs.len() as f32;
    for i in 0..doc_vec.len() {
        let mean: f32 = chunk_vecs.iter().map(|v| v[i]).sum::<f32>() / n;
        assert!(
            (doc_vec[i] - mean).abs() < 1e-5,
            "component {i}: {} != mean {mean}",
            doc_vec[i]
        );
    }
}

/// A capture no model embedded, whose vectors `index_pending` wrote later, is complete: a repeat run
/// answers already ingested and embedded, and marks the ingestion so, rather than resuming it forever.
/// Text with no source bytes is addressed apart from any file's bytes, and a run that has the bytes
/// keeps them as the blob even when it only resumes.
#[test]
fn a_capture_embedded_later_completes_and_text_is_addressed_apart_from_bytes() {
    let db = open_db();
    let text = "no model saw this text when it was captured, but the index did later";
    let first = pipeline(false)
        .ingest(&db, None, &doc(text), "docs")
        .unwrap();
    assert!(!first.embedded && !first.already_ingested);
    let again = pipeline(false)
        .ingest(&db, None, &doc(text), "docs")
        .unwrap();
    assert!(
        again.already_ingested && !again.embedded,
        "a resume with no model wrote nothing: {again:?}"
    );
    pipeline(true).index_pending(&db).unwrap();
    let complete = pipeline(false)
        .ingest(&db, None, &doc(text), "docs")
        .unwrap();
    assert!(
        complete.already_ingested && complete.embedded,
        "{complete:?}"
    );
    let status: String = db
        .read(|c| Ok(c.query_row("SELECT status FROM ingestions", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(status, "complete");

    // The same characters as a file's bytes are another document, and that ingest stores its blob.
    let before = item_count(&db);
    let from_file = pipeline(false)
        .ingest(&db, Some(text.as_bytes()), &doc(text), "docs")
        .unwrap();
    assert_ne!(from_file.document, first.document);
    assert!(item_count(&db) > before);
    let resumed = pipeline(false)
        .ingest(&db, Some(text.as_bytes()), &doc(text), "docs")
        .unwrap();
    assert_eq!(resumed.document, from_file.document);
    db.write_txn("t", |c, _| {
        c.execute("DELETE FROM blobs", [])?;
        Ok(())
    })
    .unwrap();
    pipeline(false)
        .ingest(&db, Some(text.as_bytes()), &doc(text), "docs")
        .unwrap();
    let blobs: i64 = db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(blobs, 1, "a resume with the bytes keeps them");
}

// ---- vector provenance (V030): an edit makes an item pending; a document re-derives ----------

/// The stored vector for item `id`, if it has one.
fn vector_by_id(db: &Db, id: jkb_types::ItemId) -> Option<Vec<f32>> {
    let indexer = VectorIndexer::new(FakeEmbedder::arc(true));
    db.read(move |conn| Ok(indexer.vectors_for(conn, &[id]).unwrap().remove(&id)))
        .unwrap()
}

/// Write `content` over item `id` through the content repo every editor uses, returning the
/// transaction it was written in.
fn edit(db: &Db, id: jkb_types::ItemId, content: &str) -> i64 {
    let content = content.to_owned();
    db.write_txn("t", move |conn, meta| {
        jkb_core::item::set_content(conn, meta, id, &content, None)?;
        Ok(meta.txn_id)
    })
    .unwrap()
}

fn new_item(db: &Db, uid: &str, kind: &str, content: &str) -> jkb_types::ItemId {
    let item = jkb_core::item::NewItem {
        uid: uid.to_owned(),
        kind: kind.to_owned(),
        content: Some(content.to_owned()),
        content_hash: None,
        mime: None,
    };
    db.write_txn("t", move |conn, meta| {
        jkb_core::item::upsert(conn, meta, &item)
    })
    .unwrap()
}

/// `document`'s chunks as `(id, uid, content)`, in position order.
fn chunks_of(db: &Db, document: jkb_types::ItemId) -> Vec<(jkb_types::ItemId, String, String)> {
    db.read(move |conn| {
        Ok(jkb_core::item::derived_children(conn, document, "chunk")?
            .into_iter()
            .map(|m| (m.id, m.uid, m.content.unwrap_or_default()))
            .collect())
    })
    .unwrap()
}

/// Whether item `id` still exists.
fn exists(db: &Db, id: jkb_types::ItemId) -> bool {
    db.read(move |conn| Ok(jkb_core::item::get(conn, id)?.is_some()))
        .unwrap()
}

/// `(content_rev, chunked_rev)` of item `id`.
fn revs(db: &Db, id: jkb_types::ItemId) -> (i64, Option<i64>) {
    db.read(move |conn| {
        Ok(conn.query_row(
            "SELECT content_rev, chunked_rev FROM items WHERE id = ?1",
            [id.get()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    })
    .unwrap()
}

/// Assert `document`'s stored vector is the mean of its current chunks' stored vectors.
fn assert_is_centroid_of_its_chunks(db: &Db, document: jkb_types::ItemId) {
    let chunk_ids: Vec<_> = chunks_of(db, document).into_iter().map(|c| c.0).collect();
    assert!(!chunk_ids.is_empty(), "the document has no chunks");
    let indexer = VectorIndexer::new(FakeEmbedder::arc(true));
    let ids = chunk_ids.clone();
    let chunk_vecs = db
        .read(move |conn| Ok(indexer.vectors_for(conn, &ids).unwrap()))
        .unwrap();
    assert_eq!(
        chunk_vecs.len(),
        chunk_ids.len(),
        "a current chunk has no vector"
    );
    let doc_vec = vector_by_id(db, document).expect("the document has a vector");
    #[allow(clippy::cast_precision_loss)]
    let n = chunk_vecs.len() as f32;
    for (i, component) in doc_vec.iter().enumerate() {
        let mean: f32 = chunk_vecs.values().map(|v| v[i]).sum::<f32>() / n;
        assert!(
            (component - mean).abs() < 1e-5,
            "component {i}: {component} is not the mean {mean} of the current chunk vectors"
        );
    }
}

/// The fake embedder, recording every text it is asked to embed and optionally failing every call
/// after the first `budget`.
struct RecordingEmbedder {
    seen: std::sync::Mutex<Vec<String>>,
    budget: Option<usize>,
}

impl RecordingEmbedder {
    fn new(budget: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            seen: std::sync::Mutex::new(Vec::new()),
            budget,
        })
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl Embedder for RecordingEmbedder {
    #[allow(clippy::unnecessary_literal_bound)] // trait fixes the `-> &str` signature
    fn model(&self) -> &str {
        "fake"
    }
    fn dim(&self) -> usize {
        16
    }
    fn embed(&self, text: &str) -> TypesResult<Vec<f32>> {
        let mut seen = self.seen.lock().unwrap();
        if self.budget.is_some_and(|b| seen.len() >= b) {
            return Err(jkb_types::Error::EmbedderUnavailable(
                "test: interrupted".to_owned(),
            ));
        }
        seen.push(text.to_owned());
        FakeEmbedder::arc(true).embed(text)
    }
    fn health_check(&self) -> TypesResult<()> {
        Ok(())
    }
    fn resolved_version(&self) -> TypesResult<Option<String>> {
        Ok(Some("fake:v1".to_owned()))
    }
}

fn recording_pipeline(embedder: &Arc<RecordingEmbedder>) -> Pipeline {
    Pipeline::new(embedder.clone())
        .with_chunking(ChunkConfig::new(40, 10, 10).expect("valid chunk config"))
}

const OLD_TEXT: &str = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu \
                        alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
const NEW_TEXT: &str = "zebra crossings need fresh paint every spring and the council \
                        argues about the budget for zebra crossings every autumn";

/// (a) Editing an item's content makes it pending, and the next pass replaces its vector — judged by
/// what a search finds, not by a count. Before, "pending" meant only "has no vector", so an edited
/// task kept the vector of its first text for ever: an exact query for the old text still found it,
/// one for the new text did not, while FTS (kept current by triggers) matched the new.
#[test]
fn an_edited_task_is_re_embedded_and_found_by_its_new_text_not_its_old() {
    let db = open_db();
    let task = new_item(&db, "task:t", "task", "fix the leaking boiler valve");
    let other = new_item(&db, "note:o", "note", "a note that never changes");
    let pipe = pipeline(true);
    assert_eq!(pipe.index_pending(&db).unwrap().embedded, 2);
    let untouched = vector_by_id(&db, other);

    edit(&db, task, "repaint the zebra crossing");
    assert_eq!(
        pipe.unembedded_count(&db).unwrap(),
        1,
        "the edit is pending"
    );

    let nearest = |text: &str| {
        let q = FakeEmbedder::arc(true).embed(text).unwrap();
        let indexer = VectorIndexer::new(FakeEmbedder::arc(true));
        db.read(move |conn| Ok(indexer.knn(conn, &q, 1).unwrap()))
            .unwrap()
            .into_iter()
            .next()
            .expect("a hit")
    };
    // The harm, before the pass: the old text still finds the task exactly; the new does not.
    assert_eq!(nearest("fix the leaking boiler valve").0, task);
    assert!(nearest("fix the leaking boiler valve").1 < 1e-5);

    let report = pipe.index_pending(&db).unwrap();
    assert_eq!(report.embedded, 1, "only the edited task: {report:?}");
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 0);
    let (hit, distance) = nearest("repaint the zebra crossing");
    assert_eq!(hit, task, "the new text does not find the task");
    assert!(distance < 1e-5);
    let (hit, distance) = nearest("fix the leaking boiler valve");
    assert!(
        hit != task || distance > 1e-3,
        "the old text still finds the task exactly"
    );
    assert_eq!(
        vector_by_id(&db, other),
        untouched,
        "an untouched item was re-embedded"
    );
    assert_eq!(fts_hits(&db, "zebra"), 1);
    assert_eq!(fts_hits(&db, "boiler"), 0);
}

/// Writing an item's content back unchanged leaves it current: a revision counts a change of text,
/// not a write.
#[test]
fn rewriting_identical_content_leaves_the_item_current() {
    let db = open_db();
    let id = new_item(&db, "note:same", "note", "steady text");
    let pipe = pipeline(true);
    pipe.index_pending(&db).unwrap();
    edit(&db, id, "steady text");
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 0);
}

/// (b) An edited document is re-chunked from its new text — the old chunk items are gone, the new
/// ones cut from what it says now — and its vector is the centroid of the new chunks'. Its own text
/// is never handed to the model: a document's vector derives from its chunks, and its whole text
/// embedded is a truncated prefix claiming to describe all of it.
#[test]
fn an_edited_document_is_re_chunked_and_re_centred_without_embedding_its_own_text() {
    let db = open_db();
    let outcome = pipeline(true)
        .ingest(&db, Some(OLD_TEXT.as_bytes()), &doc(OLD_TEXT), "docs")
        .unwrap();
    let document = outcome.document;
    let old_chunks = chunks_of(&db, document);
    assert!(old_chunks.len() > 1, "need several chunks to replace");
    assert_eq!(revs(&db, document), (0, Some(0)));

    edit(&db, document, NEW_TEXT);
    assert_eq!(revs(&db, document), (1, Some(0)));
    assert_eq!(pipeline(true).unembedded_count(&db).unwrap(), 1);

    let recorder = RecordingEmbedder::new(None);
    let report = recording_pipeline(&recorder).index_pending(&db).unwrap();
    assert_eq!(report.derived, 1, "{report:?}");

    for (id, _, _) in &old_chunks {
        assert!(!exists(&db, *id), "an old chunk survived the re-chunk");
        assert!(
            vector_by_id(&db, *id).is_none(),
            "an old chunk kept its vector"
        );
    }
    let new_chunks = chunks_of(&db, document);
    let expected = jkb_ingest::chunk::chunk_text(
        NEW_TEXT,
        &ChunkConfig::new(40, 10, 10).expect("valid chunk config"),
    );
    assert_eq!(
        new_chunks.iter().map(|c| c.2.clone()).collect::<Vec<_>>(),
        expected,
        "the new chunks are not cut from the new text"
    );
    let prefix = format!("b3:{}:", blob::hash_bytes(OLD_TEXT.as_bytes()));
    assert!(
        new_chunks.iter().all(|c| c.1.starts_with(&prefix)),
        "a re-cut chunk left its capture's uid prefix: {new_chunks:?}"
    );
    assert_eq!(revs(&db, document), (1, Some(1)));

    let seen = recorder.seen();
    assert!(
        !seen.iter().any(|t| t == NEW_TEXT),
        "the document's own text was embedded"
    );
    assert_eq!(
        seen, expected,
        "the model was asked for something other than the new chunks"
    );
    assert_is_centroid_of_its_chunks(&db, document);
    assert_eq!(pipeline(true).unembedded_count(&db).unwrap(), 0);
    assert_eq!(
        fts_hits(&db, "lambda"),
        0,
        "an old chunk's text is still indexed"
    );
}

/// (c) The resumed-ingest path takes the same route: a document edited between a capture with no
/// embedder and the resume is re-chunked, and gets its new chunks' centroid — not its old chunks'
/// under its new text, and not its own text.
#[test]
fn a_resumed_ingest_re_chunks_a_document_edited_since_its_capture() {
    let db = open_db();
    let first = pipeline(false)
        .ingest(&db, Some(OLD_TEXT.as_bytes()), &doc(OLD_TEXT), "docs")
        .unwrap();
    assert!(!first.embedded && first.chunk_count > 1);
    let document = first.document;
    let old_chunks = chunks_of(&db, document);
    edit(&db, document, NEW_TEXT);

    let recorder = RecordingEmbedder::new(None);
    let resumed = recording_pipeline(&recorder)
        .ingest(&db, Some(OLD_TEXT.as_bytes()), &doc(OLD_TEXT), "docs")
        .unwrap();
    assert!(resumed.embedded, "{resumed:?}");

    for (id, _, _) in &old_chunks {
        assert!(!exists(&db, *id), "an old chunk survived the resume");
    }
    assert!(
        !recorder
            .seen()
            .iter()
            .any(|t| t == NEW_TEXT || t == OLD_TEXT),
        "a whole document text was embedded"
    );
    assert_is_centroid_of_its_chunks(&db, document);
    assert_eq!(pipeline(true).unembedded_count(&db).unwrap(), 0);

    // And it is complete: a repeat run writes nothing.
    let again = pipeline(true)
        .ingest(&db, Some(OLD_TEXT.as_bytes()), &doc(OLD_TEXT), "docs")
        .unwrap();
    assert!(again.already_ingested && again.embedded, "{again:?}");
}

/// (d) `jkb index --all` forgets every vector's provenance and re-embeds the store, and every
/// document comes out as the centroid of its chunks — never an embedding of its own, truncated,
/// text, which is what the shelved stale-mark design did to every multi-chunk document.
#[test]
fn index_all_leaves_every_document_vector_the_centroid_of_its_chunks() {
    let db = open_db();
    let pipe = pipeline(true);
    let a = pipe
        .ingest(&db, Some(OLD_TEXT.as_bytes()), &doc(OLD_TEXT), "docs")
        .unwrap()
        .document;
    let b = pipe
        .ingest(&db, Some(NEW_TEXT.as_bytes()), &doc(NEW_TEXT), "docs")
        .unwrap()
        .document;
    new_item(&db, "note:n", "note", "a standalone note");
    pipe.index_pending(&db).unwrap();
    let total = pipe.unembedded_count(&db).unwrap();
    assert_eq!(total, 0);

    let forgotten = db
        .write_txn_with::<_, jkb_ingest::Error, _>("t", |conn, _| {
            Ok(jkb_index::forget_sources(conn)?)
        })
        .unwrap();
    assert!(forgotten > 0);
    assert!(vector_by_id(&db, a).is_some(), "--all deleted a vector");
    assert_eq!(pipe.unembedded_count(&db).unwrap(), forgotten);

    let recorder = RecordingEmbedder::new(None);
    recording_pipeline(&recorder).index_pending(&db).unwrap();
    assert!(
        !recorder
            .seen()
            .iter()
            .any(|t| t == OLD_TEXT || t == NEW_TEXT),
        "a document was embedded from its own text"
    );
    assert_is_centroid_of_its_chunks(&db, a);
    assert_is_centroid_of_its_chunks(&db, b);
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 0);
}

/// (e) A document's vector is the centroid of its chunks', so a chunk re-embedded after an edit
/// re-centres it: the chunk's new vector has a later `seq` than the document's.
#[test]
fn an_edited_chunk_re_centres_its_document() {
    let db = open_db();
    let pipe = pipeline(true);
    let document = pipe
        .ingest(&db, Some(OLD_TEXT.as_bytes()), &doc(OLD_TEXT), "docs")
        .unwrap()
        .document;
    let first = chunks_of(&db, document)[0].0;
    let before = vector_by_id(&db, document);
    edit(&db, first, "an entirely different opening");
    assert_eq!(
        pipe.unembedded_count(&db).unwrap(),
        1,
        "the chunk is pending"
    );

    let report = pipe.index_pending(&db).unwrap();
    assert_eq!((report.embedded, report.derived), (1, 1), "{report:?}");
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 0);
    assert_eq!(
        vector_by_id(&db, first).unwrap(),
        FakeEmbedder::arc(true)
            .embed("an entirely different opening")
            .unwrap()
    );
    assert_ne!(
        vector_by_id(&db, document),
        before,
        "the document was not re-centred"
    );
    assert_is_centroid_of_its_chunks(&db, document);
    // The chunk's edit is its own, not the document's: nothing was re-cut.
    assert_eq!(revs(&db, document), (0, Some(0)));
}

/// (f) Undo restores content through its generic column restore, not through `set_content`, and
/// that write counts too — the revision is kept by the schema. Undoing a document's edit re-chunks
/// it back from the restored text.
#[test]
fn undoing_an_edit_re_embeds_and_re_chunks_from_the_restored_text() {
    let db = open_db();
    let note = new_item(&db, "note:undone", "note", "first draft");
    let pipe = pipeline(true);
    let document = pipe
        .ingest(&db, Some(OLD_TEXT.as_bytes()), &doc(OLD_TEXT), "docs")
        .unwrap()
        .document;
    let note_edit = edit(&db, note, "second draft");
    let doc_edit = edit(&db, document, NEW_TEXT);
    pipe.index_pending(&db).unwrap();
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 0);

    for txn in [doc_edit, note_edit] {
        db.write_txn("t", move |conn, meta| jkb_core::undo::undo(conn, meta, txn))
            .unwrap();
    }
    assert_eq!(
        revs(&db, note).0,
        2,
        "the undo's restore did not count as a change"
    );
    assert_eq!(revs(&db, document).0, 2);
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 2);

    let recorder = RecordingEmbedder::new(None);
    recording_pipeline(&recorder).index_pending(&db).unwrap();
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 0);
    assert_eq!(
        vector_by_id(&db, note).unwrap(),
        FakeEmbedder::arc(true).embed("first draft").unwrap()
    );
    let expected = jkb_ingest::chunk::chunk_text(
        OLD_TEXT,
        &ChunkConfig::new(40, 10, 10).expect("valid chunk config"),
    );
    assert_eq!(
        chunks_of(&db, document)
            .into_iter()
            .map(|c| c.2)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(!recorder.seen().iter().any(|t| t == OLD_TEXT));
    assert_is_centroid_of_its_chunks(&db, document);
}

/// Edits item `victim` to `replacement` the moment it is asked to embed `trigger` — an edit landing
/// while a pass is between reading an item and storing its vector.
struct EditingEmbedder {
    db: Db,
    trigger: &'static str,
    victim: jkb_types::ItemId,
    replacement: &'static str,
}

impl Embedder for EditingEmbedder {
    #[allow(clippy::unnecessary_literal_bound)] // trait fixes the `-> &str` signature
    fn model(&self) -> &str {
        "fake"
    }
    fn dim(&self) -> usize {
        16
    }
    fn embed(&self, text: &str) -> TypesResult<Vec<f32>> {
        if text == self.trigger {
            edit(&self.db, self.victim, self.replacement);
        }
        FakeEmbedder::arc(true).embed(text)
    }
    fn health_check(&self) -> TypesResult<()> {
        Ok(())
    }
    fn resolved_version(&self) -> TypesResult<Option<String>> {
        Ok(Some("fake:v1".to_owned()))
    }
}

/// (g) An item edited between being read for embedding and its vector being stored gets no vector:
/// storing the old text's vector under the new revision would make the edit look embedded for good.
/// It stays pending, and the next pass embeds what it says now. The same for a document edited
/// while its chunks are being embedded: its centroid is not taken over chunks of its old text.
#[test]
fn an_edit_during_the_pass_leaves_the_item_pending() {
    let db = open_db();
    let note = new_item(&db, "note:raced", "note", "text before the race");
    let racing = Pipeline::new(Arc::new(EditingEmbedder {
        db: db.clone(),
        trigger: "text before the race",
        victim: note,
        replacement: "the edit that lands mid-run",
    }));
    let report = racing.index_pending(&db).unwrap();
    assert_eq!(
        report.embedded, 0,
        "the superseded text's vector landed: {report:?}"
    );
    let plain = pipeline(true);
    assert_eq!(plain.unembedded_count(&db).unwrap(), 1, "the edit was lost");
    plain.index_pending(&db).unwrap();
    assert_eq!(
        vector_by_id(&db, note).unwrap(),
        FakeEmbedder::arc(true)
            .embed("the edit that lands mid-run")
            .unwrap()
    );

    // The document: captured with no embedder, then resumed by a run during which it is edited
    // as its first chunk is embedded. The chunks of its old text land (each is still at the
    // revision it was read at), but its centroid is not taken over them.
    let first = pipeline(false)
        .ingest(&db, Some(OLD_TEXT.as_bytes()), &doc(OLD_TEXT), "docs")
        .unwrap();
    let document = first.document;
    let first_chunk: &'static str =
        Box::leak(chunks_of(&db, document)[0].2.clone().into_boxed_str());
    let racing = Pipeline::new(Arc::new(EditingEmbedder {
        db: db.clone(),
        trigger: first_chunk,
        victim: document,
        replacement: NEW_TEXT,
    }))
    .with_chunking(ChunkConfig::new(40, 10, 10).expect("valid chunk config"));
    let resumed = racing
        .ingest(&db, Some(OLD_TEXT.as_bytes()), &doc(OLD_TEXT), "docs")
        .unwrap();
    assert!(
        !resumed.embedded,
        "the raced resume reported the document embedded: {resumed:?}"
    );
    assert!(
        vector_by_id(&db, document).is_none(),
        "a centroid of the old text's chunks landed under the new text"
    );
    assert_eq!(
        plain.unembedded_count(&db).unwrap(),
        1,
        "the document's edit was lost"
    );

    plain.index_pending(&db).unwrap();
    assert_eq!(plain.unembedded_count(&db).unwrap(), 0);
    assert_eq!(revs(&db, document), (1, Some(1)));
    assert_is_centroid_of_its_chunks(&db, document);
}

/// (h) A store upgraded from V029 holds vectors with no provenance: every one reads as pending, keeps
/// serving until replaced, and the next `jkb index` re-embeds them — resumably, so an interrupted
/// pass keeps what it wrote and a plain re-run finishes.
#[test]
fn pre_upgrade_vectors_are_pending_and_re_embedded_resumably() {
    let db = open_db();
    let pipe = pipeline(true);
    let document = pipe
        .ingest(&db, Some(OLD_TEXT.as_bytes()), &doc(OLD_TEXT), "docs")
        .unwrap()
        .document;
    for i in 0..6 {
        new_item(
            &db,
            &format!("note:{i}"),
            "note",
            &format!("note number {i}"),
        );
    }
    pipe.index_pending(&db).unwrap();
    let items: usize = db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM items WHERE content IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .map(|n| usize::try_from(n).unwrap())
        .unwrap();

    // What V030 leaves a V029 store with: vectors, and no provenance for any of them.
    db.write_txn("t", |c, _| {
        c.execute("DELETE FROM vector_sources", [])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(pipe.unembedded_count(&db).unwrap(), items);
    assert!(
        vector_by_id(&db, document).is_some(),
        "the old vectors stopped serving"
    );

    // Interrupted after three embeds: what landed stays, the rest is still pending.
    let interrupted = RecordingEmbedder::new(Some(3));
    let report = recording_pipeline(&interrupted).index_pending(&db).unwrap();
    assert_eq!(report.embedded, 3, "{report:?}");
    assert!(report.failed > 0, "{report:?}");
    // The document took the centroid of the three chunk vectors that landed; the rest of its
    // chunks, landing later, have later `seq`s and re-centre it.
    assert_eq!(report.derived, 1, "{report:?}");
    assert_eq!(pipe.unembedded_count(&db).unwrap(), items - report.total());

    pipe.index_pending(&db).unwrap();
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 0);
    assert_is_centroid_of_its_chunks(&db, document);
}

/// A chunk that is blank by `str::trim` — a run of no-break spaces, as PDF extraction yields — is
/// never embedded, so it must not hold its document pending either: blankness has one definition.
#[test]
fn a_chunk_of_unicode_whitespace_does_not_hold_its_document_pending() {
    let db = open_db();
    let text = format!("real words here {}", "\u{a0}".repeat(4000));
    let outcome = pipeline(false)
        .ingest(&db, Some(text.as_bytes()), &doc(&text), "docs")
        .unwrap();
    assert!(!outcome.embedded && outcome.chunk_count > 1);
    let pipe = pipeline(true);
    pipe.index_pending(&db).unwrap();
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 0);
    assert!(vector_by_id(&db, outcome.document).is_some());
}

/// An item edited to nothing loses its vector — a search for the text it no longer holds must not
/// find it — and is not pending, since there is nothing to embed. A document edited to nothing also
/// loses its chunks.
#[test]
fn an_item_edited_to_blank_loses_its_vector_and_a_document_its_chunks() {
    let db = open_db();
    let pipe = pipeline(true);
    let note = new_item(&db, "note:b", "note", "words that will go");
    let document = pipe
        .ingest(&db, Some(OLD_TEXT.as_bytes()), &doc(OLD_TEXT), "docs")
        .unwrap()
        .document;
    pipe.index_pending(&db).unwrap();
    let old_chunks = chunks_of(&db, document);

    edit(&db, note, "  ");
    edit(&db, document, "");
    assert_eq!(
        pipe.unembedded_count(&db).unwrap(),
        1,
        "the document still has chunks to drop"
    );
    pipe.index_pending(&db).unwrap();

    assert!(
        vector_by_id(&db, note).is_none(),
        "the blanked note kept its old vector"
    );
    assert!(
        vector_by_id(&db, document).is_none(),
        "the blanked document kept its old vector"
    );
    assert!(chunks_of(&db, document).is_empty());
    for (id, _, _) in old_chunks {
        assert!(!exists(&db, id));
    }
    assert_eq!(pipe.unembedded_count(&db).unwrap(), 0);
}
