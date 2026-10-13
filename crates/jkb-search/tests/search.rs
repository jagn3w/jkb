//! End-to-end search tests over a real migrated DB with `sqlite-vec` registered.
//!
//! A deterministic offline embedder (dim 16, unit-normalized — the pattern from
//! `jkb-ingest`'s pipeline tests) stands in for ollama, so route selection, scoping,
//! recall-preserving over-fetch, hybrid fusion, and context-expansion are all
//! exercised without a network.

use std::sync::Arc;

use jkb_core::item::NewItem;
use jkb_core::query::{Query, Scope};
use jkb_core::{item, ns, placement, Db};
use jkb_index::VectorIndexer;
use jkb_ingest::adapter::ParsedDocument;
use jkb_ingest::chunk::ChunkConfig;
use jkb_ingest::Pipeline;
use jkb_search::{Route, Searcher};
use jkb_types::{Embedder, ItemId, PlacementRole, Result as TypesResult};

/// Deterministic offline embedder: dim 16, byte-histogram then unit-normalized, so
/// similar text lands near in cosine space and the query vector is reproducible.
struct FakeEmbedder;

impl FakeEmbedder {
    fn arc() -> Arc<dyn Embedder + Send + Sync> {
        Arc::new(Self)
    }
}

impl Embedder for FakeEmbedder {
    #[allow(clippy::unnecessary_literal_bound)]
    fn model(&self) -> &str {
        "fake"
    }
    fn dim(&self) -> usize {
        16
    }
    fn embed(&self, text: &str) -> TypesResult<Vec<f32>> {
        Ok(embed16(text))
    }
    fn health_check(&self) -> TypesResult<()> {
        Ok(())
    }
    fn resolved_version(&self) -> TypesResult<Option<String>> {
        Ok(Some("fake:v1".to_owned()))
    }
}

/// The same embedding function, usable directly in tests (to compute a query vector
/// or hand-seed vectors).
fn embed16(text: &str) -> Vec<f32> {
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
    v
}

fn open_db() -> Db {
    Db::open_in_memory_with(&[jkb_index::register]).unwrap()
}

fn pipeline() -> Pipeline {
    // Small windows so ordinary test text yields several chunks.
    Pipeline::new(FakeEmbedder::arc())
        .with_chunking(ChunkConfig::new(40, 10, 10).expect("valid chunk config"))
}

fn doc(text: &str) -> ParsedDocument {
    ParsedDocument {
        title: None,
        text: text.to_owned(),
        mime: "text/plain".to_owned(),
    }
}

fn ingest(db: &Db, text: &str, namespace: &str) -> ItemId {
    pipeline()
        .ingest(db, Some(text.as_bytes()), &doc(text), namespace)
        .unwrap()
        .document
}

fn searcher() -> Searcher {
    Searcher::new(FakeEmbedder::arc())
}

/// The chunk items of `document`, as `(id, position)` ordered by position.
fn chunks_of(db: &Db, document: ItemId) -> Vec<(ItemId, i64)> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT e.src_item_id, p.position
             FROM edges e
             JOIN placements p ON p.item_id = e.src_item_id AND p.role = 'chunk'
             WHERE e.dst_item_id = ?1 AND e.type = 'derived_from'
             ORDER BY p.position",
        )?;
        let rows = stmt
            .query_map([document.get()], |r| {
                Ok((ItemId::new(r.get(0)?), r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .unwrap()
}

#[test]
fn vector_route_ranks_by_similarity_with_distances() {
    let db = open_db();
    ingest(
        &db,
        "The Rust programming language emphasizes memory safety and performance \
         in systems software without a garbage collector.",
        "docs",
    );

    let q = Query {
        vector: Some("safety and performance".to_owned()),
        ..Query::default()
    };
    let hits = searcher().search(&db, &q, Route::Vector, 5).unwrap().hits;

    assert!(!hits.is_empty(), "vector route finds embedded items");
    assert!(hits.iter().all(|h| h.route == Route::Vector));
    assert!(
        hits.iter().all(|h| h.distance.is_some()),
        "vector hits carry a cosine distance"
    );
    // Best-first: score (negated distance) is non-increasing.
    for pair in hits.windows(2) {
        assert!(pair[0].score >= pair[1].score);
    }
}

#[test]
fn fts_route_excludes_items_lacking_the_term() {
    let db = open_db();
    ingest(
        &db,
        "Photosynthesis converts light energy into chemical energy in chloroplasts.",
        "docs",
    );
    let other = ingest(
        &db,
        "Distributed systems reason about partial failure and network partitions.",
        "docs",
    );

    let q = Query {
        fts: Some("photosynthesis".to_owned()),
        ..Query::default()
    };
    let hits = searcher().search(&db, &q, Route::Fts, 10).unwrap().hits;

    assert!(!hits.is_empty(), "keyword route returns matches");
    assert!(hits.iter().all(|h| h.route == Route::Fts));
    assert!(
        hits.iter().all(|h| h.item != other),
        "an item lacking the term is excluded"
    );
}

#[test]
fn scoped_vector_returns_only_in_scope_items() {
    let db = open_db();
    let in_scope = ingest(
        &db,
        "Ecology studies the relationships between organisms and their environment.",
        "books/biology",
    );
    ingest(
        &db,
        "Ecology studies the relationships between organisms and their environment.",
        "notes/misc",
    );

    let q = Query {
        vector: Some("organisms and environment".to_owned()),
        scope: Scope::Subtree("books".to_owned()),
        ..Query::default()
    };
    let hits = searcher().search(&db, &q, Route::Vector, 10).unwrap().hits;

    assert!(!hits.is_empty(), "scoped vector search returns results");
    for hit in &hits {
        let path = hit.namespace_path.as_deref().unwrap_or("");
        assert!(
            path == "books" || path.starts_with("books/"),
            "hit {:?} is out of scope (path {path})",
            hit.item
        );
    }
    // The in-scope document (or its chunks) is present; the out-of-scope copy is not
    // (deduped items aside — the two texts differ only by namespace here, and the
    // document item is shared by content hash, so assert we at least got the in-scope
    // placement's path).
    assert!(hits
        .iter()
        .any(|h| h.item == in_scope || h.source_document == Some(in_scope)));
}

#[test]
fn selective_large_scope_returns_results_via_overfetch() {
    // Cross the exact-scoring threshold (256) so the adaptive over-fetch branch runs,
    // and pollute the global nearest-neighbours with out-of-scope decoys so the first
    // batch misses the scope entirely (spec: selective scope still returns results).
    let db = open_db();
    let query_text = "needle in a haystack";
    let q_vec = embed16(query_text);
    // A distinct in-scope vector, far from the query in cosine space.
    let in_vec = {
        let mut v = vec![0.0f32; 16];
        v[0] = 1.0;
        v
    };

    let vector = VectorIndexer::new(FakeEmbedder::arc());
    db.write_txn_with::<(), jkb_search::Error, _>("seed", move |conn, meta| {
        vector.ensure_ready(conn)?;
        let out_ns = ns::ensure(conn, meta, "out")?;
        let in_ns = ns::ensure(conn, meta, "in")?;
        // 60 out-of-scope decoys sitting exactly on the query (distance ~0): far more
        // than the initial over-fetch batch, so growth is forced.
        for i in 0..60 {
            let id = item::upsert(
                conn,
                meta,
                &NewItem {
                    uid: format!("out:{i}"),
                    kind: "note".to_owned(),
                    content: None,
                    content_hash: None,
                    mime: None,
                },
            )?;
            placement::place(conn, meta, id, out_ns, PlacementRole::Primary, 0)?;
            vector.upsert_vector(conn, id, 0, &q_vec)?;
        }
        // 300 in-scope items (> 256) at a farther, uniform distance.
        for i in 0..300 {
            let id = item::upsert(
                conn,
                meta,
                &NewItem {
                    uid: format!("in:{i}"),
                    kind: "note".to_owned(),
                    content: None,
                    content_hash: None,
                    mime: None,
                },
            )?;
            placement::place(conn, meta, id, in_ns, PlacementRole::Primary, 0)?;
            vector.upsert_vector(conn, id, 0, &in_vec)?;
        }
        Ok(())
    })
    .unwrap();

    let q = Query {
        vector: Some(query_text.to_owned()),
        scope: Scope::Subtree("in".to_owned()),
        ..Query::default()
    };
    let hits = searcher().search(&db, &q, Route::Vector, 5).unwrap().hits;

    assert_eq!(
        hits.len(),
        5,
        "over-fetch surfaces in-scope hits despite decoys"
    );
    for hit in &hits {
        assert_eq!(hit.namespace_path.as_deref(), Some("in"));
    }
}

#[test]
fn hybrid_fuses_vector_and_fts_orderings() {
    let db = open_db();
    ingest(
        &db,
        "Concurrency in Rust prevents data races at compile time via ownership.",
        "docs",
    );
    ingest(
        &db,
        "Ownership and borrowing give Rust memory safety without a runtime.",
        "docs",
    );

    let q = Query {
        fts: Some("ownership".to_owned()),
        vector: Some("memory safety and ownership".to_owned()),
        ..Query::default()
    };

    let vector = searcher().search(&db, &q, Route::Vector, 10).unwrap().hits;
    let fts = searcher().search(&db, &q, Route::Fts, 10).unwrap().hits;
    let hybrid = searcher().search(&db, &q, Route::Hybrid, 10).unwrap().hits;

    assert!(!hybrid.is_empty(), "hybrid returns fused results");
    assert!(hybrid.iter().all(|h| h.route == Route::Hybrid));
    // Best-first by fused score.
    for pair in hybrid.windows(2) {
        assert!(pair[0].score >= pair[1].score);
    }
    // Fusion draws from both orderings: the hybrid set is a subset of their union.
    let union: std::collections::HashSet<i64> = vector
        .iter()
        .chain(fts.iter())
        .map(|h| h.item.get())
        .collect();
    assert!(hybrid.iter().all(|h| union.contains(&h.item.get())));
    // An item present in *both* single-route rankings must appear in the fused result
    // (it accumulates two reciprocal-rank contributions).
    let in_vector: std::collections::HashSet<i64> = vector.iter().map(|h| h.item.get()).collect();
    let both: Vec<i64> = fts
        .iter()
        .map(|h| h.item.get())
        .filter(|id| in_vector.contains(id))
        .collect();
    for id in both {
        assert!(
            hybrid.iter().any(|h| h.item.get() == id),
            "item {id} ranked by both routes should survive fusion"
        );
    }
}

#[test]
fn context_expansion_returns_ordered_neighbors_and_handles_boundaries() {
    let db = open_db();
    // Long text => many small chunks with sequential positions 0..n.
    let mut text = String::new();
    for i in 0..12 {
        use std::fmt::Write as _;
        write!(
            text,
            "Sentence number {i} about knowledge bases and retrieval. "
        )
        .unwrap();
    }
    let document = ingest(&db, &text, "docs");
    let chunks = chunks_of(&db, document);
    assert!(chunks.len() >= 5, "need several chunks to expand around");

    // A middle chunk: ±2 yields exactly 5 neighbours in order, centred on the hit.
    let mid = chunks[chunks.len() / 2];
    let ctx = searcher().get_context(&db, mid.0, 2).unwrap();
    assert_eq!(ctx.len(), 5);
    let positions: Vec<i64> = ctx.iter().map(|c| c.position).collect();
    let mut sorted = positions.clone();
    sorted.sort_unstable();
    assert_eq!(positions, sorted, "neighbours are ordered by position");
    assert_eq!(ctx.iter().filter(|c| c.is_hit).count(), 1);
    assert!(ctx.iter().find(|c| c.is_hit).unwrap().item == mid.0);

    // Boundary: the first chunk has no predecessors, so only it + followers come back.
    let first = chunks[0];
    let ctx = searcher().get_context(&db, first.0, 2).unwrap();
    assert_eq!(
        ctx.first().unwrap().position,
        first.1,
        "no chunk before the first"
    );
    assert!(ctx.first().unwrap().is_hit);
    assert!(ctx.len() <= 3, "first ± 2 = itself + up to two followers");
}

#[test]
fn context_expansion_of_a_non_chunk_returns_the_item_itself() {
    let db = open_db();
    let id = db
        .write_txn("seed", |conn, meta| {
            item::upsert(
                conn,
                meta,
                &NewItem {
                    uid: "note:1".to_owned(),
                    kind: "note".to_owned(),
                    content: Some("a standalone note".to_owned()),
                    content_hash: None,
                    mime: None,
                },
            )
        })
        .unwrap();

    let ctx = searcher().get_context(&db, id, 3).unwrap();
    assert_eq!(ctx.len(), 1);
    assert!(ctx[0].is_hit);
    assert_eq!(ctx[0].item, id);
    assert_eq!(ctx[0].content, "a standalone note");
}

/// Plant `n` orphan vector rows — ids no item has, as a database that predates the vector GC
/// trigger carries — each exactly the embedding of `text`, so they rank ahead of every live row.
fn plant_orphans(db: &Db, text: &str, n: i64) {
    let bytes: Vec<u8> = embed16(text).iter().flat_map(|f| f.to_le_bytes()).collect();
    db.write_txn("test", move |conn, _meta| {
        let mut insert =
            conn.prepare_cached("INSERT INTO vec_items_16(item_id, embedding) VALUES (?1, ?2)")?;
        for orphan in 900_000..900_000 + n {
            insert.execute(rusqlite::params![orphan, bytes])?;
        }
        Ok(())
    })
    .unwrap();
}

/// A search reports the orphan vector rows its liveness filter dropped, on every route that runs
/// a KNN, so the caller can point the user at `jkb index --sweep` — and reports none on a clean
/// table, so the hint never fires there.
#[test]
fn search_reports_the_orphan_vectors_it_filtered_out() {
    let db = open_db();
    ingest(
        &db,
        "The Rust programming language emphasizes memory safety and performance \
         in systems software without a garbage collector.",
        "docs",
    );
    let q = Query {
        vector: Some("safety and performance".to_owned()),
        ..Query::default()
    };

    for route in [Route::Vector, Route::Hybrid] {
        let clean = searcher().search(&db, &q, route, 5).unwrap();
        assert!(!clean.hits.is_empty(), "{route:?}");
        assert_eq!(clean.orphans.dropped, 0, "clean table, {route:?}");
        assert!(clean.orphans.scanned > 0, "the KNN ran, {route:?}");
    }

    plant_orphans(&db, "safety and performance", 40);
    for route in [Route::Vector, Route::Hybrid] {
        let dirty = searcher().search(&db, &q, route, 5).unwrap();
        assert!(!dirty.hits.is_empty(), "live hits still found, {route:?}");
        assert!(
            dirty.hits.iter().all(|h| h.item.get() < 900_000),
            "no orphan reaches the hits, {route:?}"
        );
        assert_eq!(
            dirty.orphans.dropped, 40,
            "every orphan ranks first, so the last page held all of them, {route:?}: {:?}",
            dirty.orphans
        );
        assert!(dirty.orphans.scanned > dirty.orphans.dropped);
    }

    // FTS never touches the vector table.
    let q = Query {
        fts: Some("safety".to_owned()),
        ..Query::default()
    };
    let fts = searcher().search(&db, &q, Route::Fts, 5).unwrap();
    assert_eq!(fts.orphans, jkb_search::OrphanVectors::default());
}
