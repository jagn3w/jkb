//! End-to-end ingestion tests over a real migrated DB with `sqlite-vec` registered.
//!
//! A deterministic fake embedder (optionally "down") stands in for ollama, so the
//! staging, idempotency, resume, and non-blocking-capture behaviours are exercised
//! without a network.

use std::sync::Arc;

use jkb_core::Db;
use jkb_index::{FtsIndexer, VectorIndexer};
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
    Pipeline::new(FakeEmbedder::arc(healthy)).with_chunking(ChunkConfig {
        max_chars: 40,
        overlap_chars: 10,
        min_chars: 10,
    })
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
        .ingest(&db, text.as_bytes(), &doc(text), "docs")
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
        .ingest(&db, text.as_bytes(), &doc(text), "docs")
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
        .ingest(&db, text.as_bytes(), &doc(text), "docs")
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
        .ingest(&db, text.as_bytes(), &doc(text), "docs")
        .unwrap();
    assert!(!down.embedded);

    // Re-ingest with a healthy embedder: resumes at the embed stage (not a no-op).
    let up = pipeline(true)
        .ingest(&db, text.as_bytes(), &doc(text), "docs")
        .unwrap();
    assert!(up.embedded);
    assert!(!up.already_ingested, "was resumed, not already complete");

    // A third ingest is now a true no-op.
    let again = pipeline(true)
        .ingest(&db, text.as_bytes(), &doc(text), "docs")
        .unwrap();
    assert!(again.already_ingested);
    assert_eq!(pipeline(true).unembedded_count(&db).unwrap(), 0);
}

#[test]
fn duplicate_source_is_stored_once() {
    let db = open_db();
    let text = "Idempotent ingestion means re-running is a no-op.";
    pipeline(true)
        .ingest(&db, text.as_bytes(), &doc(text), "docs")
        .unwrap();
    let items_after_first = item_count(&db);

    // Same bytes again (as if from a different path): no new items, one blob.
    let second = pipeline(true)
        .ingest(&db, text.as_bytes(), &doc(text), "docs")
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

    let result = pipeline(true).ingest(&db, text.as_bytes(), &doc(&text), "docs");
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
        let first = pipeline(true).ingest(&db, body.as_bytes(), &doc(&body), "docs").unwrap();
        prop_assert!(!first.already_ingested);
        let count_after_first = item_count(&db);

        let second = pipeline(true).ingest(&db, body.as_bytes(), &doc(&body), "docs").unwrap();
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
        .ingest(&db, text.as_bytes(), &doc(&text), "docs")
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
        let ns = jkb_core::ns::ensure(conn, "docs")?;
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

/// A document's vector is the mean of its chunks', not an embedding of its truncated text:
/// a vector claiming to describe a document while describing only its opening ranks against
/// queries about the opening and misses everything after it.
#[test]
fn a_documents_vector_is_the_centroid_of_its_chunks() {
    let db = open_db();
    let pipe = pipeline(true);
    let text = "one two three four five six seven eight nine ten eleven twelve ".repeat(6);
    let outcome = pipe
        .ingest(&db, text.as_bytes(), &doc(&text), "docs")
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
