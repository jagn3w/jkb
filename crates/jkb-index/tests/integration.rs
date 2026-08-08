//! End-to-end index tests over a real migrated database (via `jkb_core::Db`).
//!
//! The DB is opened via `Db::open_in_memory_with(&[jkb_index::register])`, so core
//! sequences the `sqlite-vec` registration before opening the writer connection. A
//! deterministic fake embedder makes rebuilds reproducible without a live ollama.

use std::sync::Arc;

use jkb_core::item::{upsert, NewItem};
use jkb_core::Db;
use jkb_index::{Dispatcher, FtsIndexer, IndexItem, Indexer, VectorIndexer};
use jkb_types::{Embedder, ItemId, Result as TypesResult};

/// A deterministic, offline embedder: content maps to a fixed unit vector, so the
/// same text always yields the same embedding (making rebuild reproducible).
struct FakeEmbedder {
    model: String,
    dim: usize,
}

impl FakeEmbedder {
    fn arc(model: &str, dim: usize) -> Arc<dyn Embedder + Send + Sync> {
        Arc::new(Self {
            model: model.to_owned(),
            dim,
        })
    }
}

impl Embedder for FakeEmbedder {
    fn model(&self) -> &str {
        &self.model
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> TypesResult<Vec<f32>> {
        let mut v = vec![0.0f32; self.dim];
        for (i, b) in text.bytes().enumerate() {
            v[i % self.dim] += f32::from(b);
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
        Ok(())
    }
    fn resolved_version(&self) -> TypesResult<Option<String>> {
        Ok(Some(format!("fake:{}", self.model)))
    }
}

/// Open an in-memory DB with `sqlite-vec` registered through core's extension seam
/// (the pattern the CLI/MCP will use).
fn open_db() -> Db {
    Db::open_in_memory_with(&[jkb_index::register]).unwrap()
}

/// Insert an item and return its id.
fn add_item(
    conn: &rusqlite::Connection,
    meta: &jkb_core::WriteMeta,
    uid: &str,
    content: &str,
) -> ItemId {
    upsert(
        conn,
        meta,
        &NewItem {
            uid: uid.to_owned(),
            kind: "note".to_owned(),
            content: Some(content.to_owned()),
            content_hash: None,
            mime: None,
        },
    )
    .unwrap()
}

#[test]
fn dispatcher_indexes_and_knn_returns_item_ids_without_a_join() {
    let db = open_db();
    let embedder = FakeEmbedder::arc("fake", 16);

    let apple_id = db
        .write_txn("test", {
            let embedder = embedder.clone();
            move |conn, meta| {
                let dispatcher = Dispatcher::new(vec![
                    Box::new(FtsIndexer::new()),
                    Box::new(VectorIndexer::new(embedder.clone())),
                ]);
                let items = [
                    ("n:apple", "apple orchard fruit harvest"),
                    ("n:rocket", "rocket engine thrust orbit"),
                    ("n:ocean", "ocean tide wave current"),
                ];
                let mut apple_id = ItemId::new(0);
                for (uid, content) in items {
                    let id = add_item(conn, meta, uid, content);
                    if uid == "n:apple" {
                        apple_id = id;
                    }
                    let embedding = embedder.embed(content).unwrap();
                    dispatcher
                        .on_upsert(
                            conn,
                            &IndexItem {
                                id,
                                kind: "note",
                                mime: None,
                                content: Some(content),
                                embedding: Some(&embedding),
                            },
                        )
                        .unwrap();
                }
                Ok(apple_id)
            }
        })
        .unwrap();

    // Query near the apple item; nearest neighbour should be the apple item id.
    let nearest = db
        .read({
            let embedder = embedder.clone();
            move |conn| {
                let vector = VectorIndexer::new(embedder.clone());
                let q = embedder.embed("apple fruit orchard").unwrap();
                let hits = vector.knn(conn, &q, 3).unwrap();
                Ok(hits)
            }
        })
        .unwrap();

    assert_eq!(nearest.first().map(|(id, _)| *id), Some(apple_id));
    assert_eq!(nearest.len(), 3);
}

#[test]
fn fts_reflects_inserts_and_updates() {
    let db = open_db();

    let id = db
        .write_txn("test", |conn, meta| {
            Ok(add_item(conn, meta, "n:1", "the quick brown fox"))
        })
        .unwrap();

    let hits_brown = db
        .read(|conn| Ok(FtsIndexer::new().search(conn, "brown", 10).unwrap()))
        .unwrap();
    assert_eq!(hits_brown.len(), 1);
    assert_eq!(hits_brown[0].0, id);

    // Update the content; the FTS triggers must reflect it.
    db.write_txn("test", move |conn, _meta| {
        conn.execute(
            "UPDATE items SET content = 'lazy sleeping dog' WHERE id = ?1",
            [id.get()],
        )?;
        Ok(())
    })
    .unwrap();

    let (still_brown, now_dog) = db
        .read(|conn| {
            let fts = FtsIndexer::new();
            Ok((
                fts.search(conn, "brown", 10).unwrap().len(),
                fts.search(conn, "dog", 10).unwrap().len(),
            ))
        })
        .unwrap();
    assert_eq!(still_brown, 0, "stale term should be gone after update");
    assert_eq!(now_dog, 1, "new term should be searchable after update");
}

#[test]
fn drop_and_rebuild_reproduces_vector_results() {
    let db = open_db();
    let embedder = FakeEmbedder::arc("fake", 16);

    let query_text = "rocket thrust";
    // Build the index, capture KNN ordering.
    let before = db
        .write_txn("test", {
            let embedder = embedder.clone();
            move |conn, meta| {
                let vector = VectorIndexer::new(embedder.clone());
                vector.ensure_ready(conn).unwrap();
                for (uid, content) in [
                    ("n:apple", "apple orchard fruit"),
                    ("n:rocket", "rocket engine thrust orbit"),
                    ("n:ocean", "ocean tide wave"),
                ] {
                    let id = add_item(conn, meta, uid, content);
                    let e = embedder.embed(content).unwrap();
                    vector.upsert_vector(conn, id, &e).unwrap();
                }
                let q = embedder.embed(query_text).unwrap();
                Ok(vector.knn(conn, &q, 3).unwrap())
            }
        })
        .unwrap();

    // Drop all vectors, then rebuild from item content (re-embedding).
    let after = db
        .write_txn("test", {
            let embedder = embedder.clone();
            move |conn, _meta| {
                let vector = VectorIndexer::new(embedder.clone());
                // A real drop: remove the whole vec table, then rebuild re-creates
                // it and re-embeds from item content (the source of truth).
                conn.execute_batch(&format!("DROP TABLE {};", vector.table_name()))?;
                vector.rebuild(conn).unwrap();
                let q = embedder.embed(query_text).unwrap();
                Ok(vector.knn(conn, &q, 3).unwrap())
            }
        })
        .unwrap();

    assert_eq!(
        before, after,
        "rebuild must reproduce identical KNN results"
    );
    assert_eq!(after.len(), 3);
}

#[test]
fn catalog_refuses_a_different_model_at_the_same_dim() {
    let db = open_db();

    // First model populates the catalog for vec_items_8.
    db.write_txn("test", |conn, meta| {
        let vector = VectorIndexer::new(FakeEmbedder::arc("model-a", 8));
        let id = add_item(conn, meta, "n:1", "hello world");
        let e = FakeEmbedder::arc("model-a", 8)
            .embed("hello world")
            .unwrap();
        vector.ensure_ready(conn).unwrap();
        vector.upsert_vector(conn, id, &e).unwrap();
        Ok(())
    })
    .unwrap();

    // A different model at the same dim (same vec_items_8 table) must be refused.
    // Return the error message (if any) through the jkb_core::Result boundary.
    let mismatch = db
        .write_txn("test", |conn, _meta| {
            let vector = VectorIndexer::new(FakeEmbedder::arc("model-b", 8));
            Ok(vector.ensure_ready(conn).err().map(|e| e.to_string()))
        })
        .unwrap();
    let msg = mismatch.expect("model mismatch must be refused");
    assert!(msg.contains("model"), "unexpected error: {msg}");
}

#[test]
fn fts_rebuild_and_integrity_check_pass() {
    let db = open_db();
    db.write_txn("test", |conn, meta| {
        add_item(conn, meta, "n:1", "alpha beta gamma");
        Ok(())
    })
    .unwrap();

    db.write_txn("test", |conn, _meta| {
        let fts = FtsIndexer::new();
        fts.rebuild(conn).unwrap();
        fts.integrity_check(conn).unwrap();
        assert_eq!(fts.search(conn, "beta", 10).unwrap().len(), 1);
        Ok(())
    })
    .unwrap();
}

/// A deleted item's vector must be sweepable, re-embedding an id must replace rather than
/// collide, and — since D40 — a new item must **not** be able to land on a dead vector's id.
///
/// The vec table is a virtual table, so it can carry no foreign key and nothing cascades. Its
/// `item_id` is the rowid, and while `items.id` was a plain rowid alias `SQLite` handed a
/// deleted item's id to the next item created, after which the leftover row **collided** with
/// it. Observed end to end: `jkb ingest` → `jkb undo` → `jkb ingest` failed with `UNIQUE
/// constraint failed` and kept failing for every later ingest into that database, while a
/// vector search for the deleted text returned the new document's chunks. `INSERT OR REPLACE`
/// did not save it: vec0 ignores the conflict clause.
///
/// `AUTOINCREMENT` (V010) removed the reuse, so the last assertion is now the inverse of what
/// this test originally pinned: the successor gets a **fresh** id and the stale row cannot be
/// adopted. The sweep is still asserted, because a monotonically growing index still wants
/// collecting — it is just housekeeping now rather than the thing standing between the store
/// and corruption.
#[test]
fn a_deleted_items_vector_is_swept_and_a_successor_gets_a_fresh_id() {
    let db = open_db();
    let embedder = FakeEmbedder::arc("fake", 16);

    let id = db
        .write_txn("test", {
            let embedder = embedder.clone();
            move |conn, meta| {
                let vector = VectorIndexer::new(embedder.clone());
                vector.ensure_ready(conn).unwrap();
                let id = add_item(conn, meta, "n:doomed", "doomed content");
                vector
                    .upsert_vector(conn, id, &embedder.embed("doomed content").unwrap())
                    .unwrap();
                Ok(id)
            }
        })
        .unwrap();

    // Re-embedding the SAME id must replace, not raise `UNIQUE constraint failed`.
    db.write_txn("test", {
        let embedder = embedder.clone();
        move |conn, _meta| {
            VectorIndexer::new(embedder.clone())
                .upsert_vector(conn, id, &embedder.embed("second pass").unwrap())
                .unwrap();
            Ok(())
        }
    })
    .unwrap();

    // Delete the item the way `undo` does — straight out of `items`, touching no jkb code.
    // The GC trigger (D42.2) removes the vector, so the sweep afterwards finds NOTHING to do:
    // that is the whole point of moving the obligation into the schema.
    let dropped = db
        .write_txn("test", move |conn, _meta| {
            conn.execute("DELETE FROM items WHERE id = ?1", [id.get()])?;
            let remaining: i64 =
                conn.query_row("SELECT count(*) FROM vec_items_16", [], |r| r.get(0))?;
            assert_eq!(
                remaining, 0,
                "the trigger must remove the vector with its item, without jkb's help"
            );
            Ok(jkb_index::drop_orphan_vectors(conn).unwrap())
        })
        .unwrap();
    assert_eq!(
        dropped, 0,
        "nothing left for the sweep — it is repair for pre-trigger rows, not the guarantee"
    );

    // The successor must NOT take the freed id — that is what makes a missed sweep harmless.
    db.write_txn("test", {
        let embedder = embedder.clone();
        move |conn, meta| {
            let new_id = add_item(conn, meta, "n:successor", "successor content");
            assert!(
                new_id.get() > id.get(),
                "id {} was reissued after {} was deleted — a stale vector row could be adopted",
                new_id.get(),
                id.get()
            );
            VectorIndexer::new(embedder.clone())
                .upsert_vector(conn, new_id, &embedder.embed("successor content").unwrap())
                .unwrap();
            Ok(())
        }
    })
    .unwrap();
}

/// D42.2 probe: can a `DELETE` trigger on `items` remove the row from a `vec0` **virtual**
/// table? The whole schema-enforced-invariant argument rests on this, and a trigger
/// referencing a virtual table is not obviously legal — so it is tested, not assumed.
#[test]
fn a_delete_trigger_can_reach_a_vec0_virtual_table() {
    let db = open_db();
    let embedder = FakeEmbedder::arc("fake", 16);
    let id = db
        .write_txn("test", {
            let embedder = embedder.clone();
            move |conn, meta| {
                let vector = VectorIndexer::new(embedder.clone());
                vector.ensure_ready(conn).unwrap();
                let id = add_item(conn, meta, "n:trig", "body");
                vector
                    .upsert_vector(conn, id, &embedder.embed("body").unwrap())
                    .unwrap();
                // No trigger created here: `ensure_ready` above installs it (D42.2). This test
                // exists to prove a trigger can reach a vec0 **virtual** table at all, which is
                // the assumption the whole schema-enforced argument rests on.
                Ok(id)
            }
        })
        .unwrap();

    let before: i64 = db
        .read(|conn| Ok(conn.query_row("SELECT count(*) FROM vec_items_16", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(before, 1, "the vector was written");

    db.write_txn("test", move |conn, _m| {
        conn.execute("DELETE FROM items WHERE id = ?1", [id.get()])?;
        Ok(())
    })
    .expect("DELETE on items with the trigger present");

    let after: i64 = db
        .read(|conn| Ok(conn.query_row("SELECT count(*) FROM vec_items_16", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(after, 0, "the trigger must have removed the orphan");
}

/// A search must never return an item for a *deleted* item's text (design D42.5).
///
/// Every other vector test here asserts a count or non-emptiness, and all of them stay green
/// while retrieval returns the wrong row — which is exactly the shape of the bug that shipped:
/// a reused id pointed a live item at a dead embedding, so the count was right and the answer
/// was wrong. This asserts the harm.
#[test]
fn a_deleted_items_text_does_not_retrieve_its_successor() {
    let db = open_db();
    let embedder = FakeEmbedder::arc("fake", 16);
    let doomed_vec = embedder.embed("the doomed document about badgers").unwrap();

    let doomed = db
        .write_txn("test", {
            let embedder = embedder.clone();
            let v = doomed_vec.clone();
            move |conn, meta| {
                let vector = VectorIndexer::new(embedder.clone());
                vector.ensure_ready(conn).unwrap();
                let id = add_item(conn, meta, "n:doomed", "the doomed document about badgers");
                vector.upsert_vector(conn, id, &v).unwrap();
                Ok(id)
            }
        })
        .unwrap();

    // Delete it the way `undo` does — straight out of `items`.
    db.write_txn("test", move |conn, _m| {
        conn.execute("DELETE FROM items WHERE id = ?1", [doomed.get()])?;
        Ok(())
    })
    .unwrap();

    // A successor, with unrelated content and NO vector of its own.
    let successor = db
        .write_txn("test", |conn, meta| {
            Ok(add_item(
                conn,
                meta,
                "n:successor",
                "unrelated content about tax law",
            ))
        })
        .unwrap();

    // Query with the DELETED item's own embedding: the strongest possible pull toward its row.
    let hits = db
        .read({
            let embedder = embedder.clone();
            move |conn| {
                Ok(VectorIndexer::new(embedder.clone())
                    .knn(conn, &doomed_vec, 10)
                    .unwrap())
            }
        })
        .unwrap();

    assert!(
        !hits.iter().any(|(id, _)| *id == successor),
        "the successor was returned for the deleted item's text — it inherited a dead embedding"
    );
    assert!(
        !hits.iter().any(|(id, _)| *id == doomed),
        "a deleted item must not be returned at all"
    );
}
