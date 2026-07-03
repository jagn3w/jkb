//! Multi-route search over the derived indexes (design D9, D20).
//!
//! Three selectable [`Route`]s rank the same substrate: [`Route::Vector`] (embed the
//! query, KNN over `sqlite-vec`), [`Route::Fts`] (FTS5 keyword relevance), and
//! [`Route::Hybrid`] (reciprocal-rank fusion of the two). There is **no LLM in the
//! search path** — the only model call is embedding the query text, done on the
//! caller's thread before touching the DB so the single writer thread never blocks
//! on network I/O.
//!
//! ## Scoping and recall (D9)
//! Every route honours the structured scope of a [`Query`] (namespace set + tags +
//! kind/status/… predicates). The vector index resolves nearest neighbours *before*
//! joins, so a naive KNN-then-filter drops recall on selective scopes. Two
//! mitigations, both in [`vector_ranked`]:
//! - **Exact scoring for a small scope** — enumerate the in-scope items and score
//!   them exactly with [`VectorIndexer::distances_for`], skipping ANN entirely.
//! - **Adaptive over-fetch for a large restricted scope** — fetch `k×N` neighbours,
//!   keep the in-scope ones, and grow `N` up to a cap until enough survive.
//!
//! The seam to *partition* the vec table by a coarse key (which would restore recall
//! without over-fetch) is noted in `jkb-index`'s `vector.rs`.
//!
//! ## Provenance ([`SearchHit`])
//! Each hit carries its identity, the route that produced it, a higher-is-better
//! score (and the raw cosine distance when a vector match contributed), its namespace
//! path, and — for a chunk — the source document it was derived from.
//!
//! ## Context-expansion ([`Searcher::get_context`], D20)
//! Given a chunk hit, return the ±N neighbouring chunks of the same source ordered by
//! `position`, with no re-embedding or new vector query.

mod error;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};

use jkb_core::query::{Query, Scope};
use jkb_core::Db;
use jkb_index::{FtsIndexer, VectorIndexer};
use jkb_types::{Embedder, ItemId};

pub use error::{Error, Result};

/// Above this many in-scope items, the vector route over-fetches ANN neighbours
/// instead of scoring the scope exactly (design D9).
const EXACT_SCORING_CAP: usize = 256;
/// Initial over-fetch multiplier: fetch `k × this` ANN neighbours before filtering.
const OVERFETCH_MULTIPLIER: usize = 8;
/// Ceiling on ANN / FTS candidates fetched while filtering to a scope.
const OVERFETCH_CAP: usize = 2048;
/// The reciprocal-rank-fusion constant (the classic default; larger = flatter).
const RRF_K: f64 = 60.0;

/// Which retrieval route produced (or fused) a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Embedding-similarity KNN.
    Vector,
    /// FTS5 keyword relevance.
    Fts,
    /// Reciprocal-rank fusion of vector and FTS.
    Hybrid,
}

impl Route {
    /// A stable lowercase name (for provenance display and `doctor`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Route::Vector => "vector",
            Route::Fts => "fts",
            Route::Hybrid => "hybrid",
        }
    }
}

/// One search result with enough provenance to locate and rank it (spec:
/// result provenance).
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// The matched item.
    pub item: ItemId,
    /// The route that produced this hit (`Hybrid` for a fused result).
    pub route: Route,
    /// Ranking score, **higher is better** across all routes (vector: negated cosine
    /// distance; FTS: negated bm25; hybrid: summed reciprocal ranks).
    pub score: f64,
    /// The raw cosine distance, when a vector match contributed (lower = closer).
    pub distance: Option<f32>,
    /// The namespace path the item is placed under, if any.
    pub namespace_path: Option<String>,
    /// For a chunk, the source document it was derived from.
    pub source_document: Option<ItemId>,
}

/// One chunk in a context-expansion window around a hit.
#[derive(Debug, Clone)]
pub struct ContextChunk {
    /// The chunk (or, for a non-chunk item, the item itself).
    pub item: ItemId,
    /// Its `position` among its source's chunks.
    pub position: i64,
    /// The chunk text.
    pub content: String,
    /// Whether this is the originally-requested hit (vs. an expanded neighbour).
    pub is_hit: bool,
}

/// Runs search routes and context-expansion over a [`Db`].
///
/// Holds the [`Embedder`] used to embed query text for the vector/hybrid routes.
pub struct Searcher {
    embedder: Arc<dyn Embedder + Send + Sync>,
}

impl Searcher {
    /// Build a searcher over `embedder` (its model/dim selects the vector table).
    #[must_use]
    pub fn new(embedder: Arc<dyn Embedder + Send + Sync>) -> Self {
        Self { embedder }
    }

    /// Search with `route`, returning up to `limit` hits best-first.
    ///
    /// The query text embedded for the vector/hybrid routes is the `~"…"` term
    /// ([`Query::vector`]) if present, else the bare FTS term; the FTS/hybrid routes
    /// key off the FTS term (falling back to the vector term). The structured part of
    /// `query` (scope/tags/kind/…) is applied as an in-scope filter on every route.
    ///
    /// # Errors
    /// Returns an error if embedding the query text fails, or a query/statement fails.
    pub fn search(
        &self,
        db: &Db,
        query: &Query,
        route: Route,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        // Embed the query text *here*, off the writer thread — the only model call in
        // the path (spec: no hidden LLM). Skipped entirely for the pure FTS route.
        let qvec = if matches!(route, Route::Vector | Route::Hybrid) {
            match vector_text(query) {
                Some(text) => Some(self.embedder.embed(text)?),
                None => None,
            }
        } else {
            None
        };

        let vector = VectorIndexer::new(self.embedder.clone());
        let fts = FtsIndexer::new();
        let scope = scope_query(query);
        let fts_text = fts_text(query).map(str::to_owned);

        db.read_with::<Vec<SearchHit>, Error, _>(move |conn| {
            // Resolve the structural candidate set once: `None` = unrestricted (all
            // items); `Some(ids)` = exactly the in-scope items to rank within.
            let scope_set: Option<Vec<ItemId>> = match &scope {
                Some(q) => Some(q.evaluate(conn)?),
                None => None,
            };
            let scope_ref = scope_set.as_deref();

            let hits = match route {
                Route::Vector => {
                    let ranked = vector_ranked(&vector, conn, qvec.as_deref(), scope_ref, limit)?;
                    ranked
                        .into_iter()
                        .map(|(id, dist)| {
                            build_hit(conn, id, Route::Vector, -f64::from(dist), Some(dist))
                        })
                        .collect::<Result<Vec<_>>>()?
                }
                Route::Fts => {
                    let ranked = fts_ranked(fts, conn, fts_text.as_deref(), scope_ref, limit)?;
                    ranked
                        .into_iter()
                        .map(|(id, bm25)| build_hit(conn, id, Route::Fts, -bm25, None))
                        .collect::<Result<Vec<_>>>()?
                }
                Route::Hybrid => {
                    // Fuse deeper than `limit` from each route so the two orderings
                    // have material to overlap on.
                    let depth = (limit * 2).clamp(20, OVERFETCH_CAP);
                    let v = vector_ranked(&vector, conn, qvec.as_deref(), scope_ref, depth)?;
                    let f = fts_ranked(fts, conn, fts_text.as_deref(), scope_ref, depth)?;
                    rrf_fuse(&v, &f, limit)
                        .into_iter()
                        .map(|(id, score, dist)| build_hit(conn, id, Route::Hybrid, score, dist))
                        .collect::<Result<Vec<_>>>()?
                }
            };
            Ok(hits)
        })
    }

    /// Expand a hit into its surrounding context: the ±`n` chunks of the same source
    /// document, ordered by `position` (design D20). Boundaries are handled naturally
    /// (fewer neighbours near the ends). No re-embedding or vector query.
    ///
    /// If `item` is not a chunk (no `derived_from` source, or no chunk placement), the
    /// item is returned as a single-element context, so callers can expand any hit
    /// uniformly.
    ///
    /// # Errors
    /// Returns an error if a statement fails.
    pub fn get_context(&self, db: &Db, item: ItemId, n: usize) -> Result<Vec<ContextChunk>> {
        db.read_with::<Vec<ContextChunk>, Error, _>(move |conn| {
            let Some(doc) = source_document(conn, item)? else {
                return single_item_context(conn, item);
            };
            let hit_pos: Option<i64> = conn
                .prepare_cached(
                    "SELECT position FROM placements WHERE item_id = ?1 AND role = 'chunk' LIMIT 1",
                )?
                .query_row([item.get()], |r| r.get(0))
                .optional()?;
            let Some(hit_pos) = hit_pos else {
                return single_item_context(conn, item);
            };

            let span = i64::try_from(n).unwrap_or(i64::MAX);
            let lo = hit_pos.saturating_sub(span);
            let hi = hit_pos.saturating_add(span);
            let mut stmt = conn.prepare_cached(
                "SELECT e.src_item_id, p.position, i.content
                 FROM edges e
                 JOIN placements p ON p.item_id = e.src_item_id AND p.role = 'chunk'
                 JOIN items i ON i.id = e.src_item_id
                 WHERE e.dst_item_id = ?1 AND e.type = 'derived_from'
                   AND p.position BETWEEN ?2 AND ?3
                 ORDER BY p.position",
            )?;
            let rows = stmt
                .query_map(params![doc.get(), lo, hi], |r| {
                    Ok((
                        ItemId::new(r.get(0)?),
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .map(|(id, position, content)| ContextChunk {
                    item: id,
                    position,
                    content,
                    is_hit: id == item,
                })
                .collect())
        })
    }
}

/// The query text for the vector/hybrid routes: the `~"…"` term, else the FTS term.
fn vector_text(q: &Query) -> Option<&str> {
    q.vector.as_deref().or(q.fts.as_deref())
}

/// The query text for the FTS/hybrid routes: the FTS term, else the `~"…"` term.
fn fts_text(q: &Query) -> Option<&str> {
    q.fts.as_deref().or(q.vector.as_deref())
}

/// The structural (scope/tags/predicate) part of `q`, with the ranking terms and
/// limit stripped, or `None` if `q` imposes no structural restriction (i.e. the
/// scope is all items — the routes then rank globally without pre-filtering).
fn scope_query(q: &Query) -> Option<Query> {
    let restricted = q.kind.is_some()
        || q.status.is_some()
        || q.priority.is_some()
        || q.due.is_some()
        || !q.tags.is_empty()
        || q.scope != Scope::All
        || q.ready
        || q.blocks.is_some();
    if !restricted {
        return None;
    }
    let mut s = q.clone();
    s.fts = None;
    s.vector = None;
    s.limit = None;
    Some(s)
}

/// Rank the vector route: unrestricted KNN, exact scoring in a small scope, or
/// adaptive over-fetch-then-filter in a large restricted scope (design D9). Returns
/// `(item, cosine_distance)` nearest-first.
fn vector_ranked(
    vector: &VectorIndexer,
    conn: &Connection,
    qvec: Option<&[f32]>,
    scope: Option<&[ItemId]>,
    k: usize,
) -> Result<Vec<(ItemId, f32)>> {
    let Some(qv) = qvec else {
        return Ok(Vec::new());
    };
    match scope {
        // Unrestricted: plain KNN over the whole table.
        None => Ok(vector.knn(conn, qv, k)?),
        Some([]) => Ok(Vec::new()),
        // Small scope: exact cosine over exactly these items — recall-preserving,
        // never approximate (spec: small scope uses exact scoring).
        Some(ids) if ids.len() <= EXACT_SCORING_CAP => {
            let mut scored = vector.distances_for(conn, qv, ids)?;
            scored.truncate(k);
            Ok(scored)
        }
        // Large restricted scope: KNN resolves globally before joins, so fetch
        // progressively more and keep the in-scope neighbours until we have `k` or hit
        // the cap (spec: selective scope still returns results via over-fetch).
        Some(ids) => {
            let set: HashSet<i64> = ids.iter().map(|id| id.get()).collect();
            let mut fetch = (k * OVERFETCH_MULTIPLIER).min(OVERFETCH_CAP);
            loop {
                let hits = vector.knn(conn, qv, fetch)?;
                let exhausted = hits.len() < fetch;
                let in_scope: Vec<(ItemId, f32)> = hits
                    .into_iter()
                    .filter(|(id, _)| set.contains(&id.get()))
                    .take(k)
                    .collect();
                if in_scope.len() >= k || fetch >= OVERFETCH_CAP || exhausted {
                    return Ok(in_scope);
                }
                fetch = (fetch * 2).min(OVERFETCH_CAP);
            }
        }
    }
}

/// Rank the FTS route, filtered to `scope`. Returns `(item, bm25)` best-first
/// (bm25 is more negative = better). FTS candidates are only the items containing the
/// term, so a generous over-fetch-then-filter preserves relevance order cheaply.
fn fts_ranked(
    fts: FtsIndexer,
    conn: &Connection,
    text: Option<&str>,
    scope: Option<&[ItemId]>,
    k: usize,
) -> Result<Vec<(ItemId, f64)>> {
    let Some(term) = text else {
        return Ok(Vec::new());
    };
    if term.trim().is_empty() {
        return Ok(Vec::new());
    }
    match scope {
        None => Ok(fts.search(conn, term, k)?),
        Some([]) => Ok(Vec::new()),
        Some(ids) => {
            let set: HashSet<i64> = ids.iter().map(|id| id.get()).collect();
            let fetched = fts.search(conn, term, OVERFETCH_CAP)?;
            Ok(fetched
                .into_iter()
                .filter(|(id, _)| set.contains(&id.get()))
                .take(k)
                .collect())
        }
    }
}

/// Reciprocal-rank fusion of the vector and FTS orderings into one ranking (a
/// no-tuning hybrid default). Each list contributes `1 / (RRF_K + rank)` per item;
/// items appearing in both accumulate both contributions. Ties break by id for a
/// deterministic order.
fn rrf_fuse(
    vector: &[(ItemId, f32)],
    fts: &[(ItemId, f64)],
    limit: usize,
) -> Vec<(ItemId, f64, Option<f32>)> {
    let mut scores: HashMap<i64, (f64, Option<f32>)> = HashMap::new();
    for (rank, (id, dist)) in vector.iter().enumerate() {
        let entry = scores.entry(id.get()).or_insert((0.0, None));
        entry.0 += recip_rank(rank);
        entry.1 = Some(*dist);
    }
    for (rank, (id, _bm25)) in fts.iter().enumerate() {
        scores.entry(id.get()).or_insert((0.0, None)).0 += recip_rank(rank);
    }
    let mut fused: Vec<(ItemId, f64, Option<f32>)> = scores
        .into_iter()
        .map(|(id, (score, dist))| (ItemId::new(id), score, dist))
        .collect();
    fused.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.get().cmp(&b.0.get()))
    });
    fused.truncate(limit);
    fused
}

/// The RRF contribution for a zero-based `rank`: `1 / (RRF_K + rank + 1)`. Ranks are
/// small (bounded by the over-fetch cap), so the `u32` conversion never saturates in
/// practice and avoids a lossy `as` cast.
fn recip_rank(rank: usize) -> f64 {
    let one_based = u32::try_from(rank + 1).unwrap_or(u32::MAX);
    1.0 / (RRF_K + f64::from(one_based))
}

/// Assemble a [`SearchHit`] for `id`, attaching provenance.
fn build_hit(
    conn: &Connection,
    id: ItemId,
    route: Route,
    score: f64,
    distance: Option<f32>,
) -> Result<SearchHit> {
    let (namespace_path, source_document) = provenance(conn, id)?;
    Ok(SearchHit {
        item: id,
        route,
        score,
        distance,
        namespace_path,
        source_document,
    })
}

/// Provenance for a hit: its namespace path (preferring a `primary` placement) and,
/// for a chunk, its source document.
fn provenance(conn: &Connection, id: ItemId) -> Result<(Option<String>, Option<ItemId>)> {
    let namespace_path: Option<String> = conn
        .prepare_cached(
            "SELECT n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
             WHERE p.item_id = ?1
             ORDER BY (p.role = 'primary') DESC, p.rowid
             LIMIT 1",
        )?
        .query_row([id.get()], |r| r.get(0))
        .optional()?;
    Ok((namespace_path, source_document(conn, id)?))
}

/// The source document a chunk was `derived_from`, if any.
fn source_document(conn: &Connection, id: ItemId) -> Result<Option<ItemId>> {
    let doc: Option<i64> = conn
        .prepare_cached(
            "SELECT dst_item_id FROM edges
             WHERE src_item_id = ?1 AND type = 'derived_from' LIMIT 1",
        )?
        .query_row([id.get()], |r| r.get(0))
        .optional()?;
    Ok(doc.map(ItemId::new))
}

/// Context for an item that has no expandable chunk neighbours: the item itself, or
/// an empty result if it does not exist.
fn single_item_context(conn: &Connection, item: ItemId) -> Result<Vec<ContextChunk>> {
    let found: Option<Option<String>> = conn
        .prepare_cached("SELECT content FROM items WHERE id = ?1")?
        .query_row([item.get()], |r| r.get::<_, Option<String>>(0))
        .optional()?;
    match found {
        Some(content) => Ok(vec![ContextChunk {
            item,
            position: 0,
            content: content.unwrap_or_default(),
            is_hit: true,
        }]),
        None => Ok(Vec::new()),
    }
}
