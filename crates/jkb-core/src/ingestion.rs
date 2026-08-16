//! The ingestion marker: the row that makes `jkb ingest` idempotent (design D21).
//!
//! One row per `(source_hash, pipeline_version, strategy, embedder_model)` records how far
//! that combination got — `captured` (items exist, vectors do not) or `complete`. Re-running
//! ingest reads it and resumes rather than duplicating the document.
//!
//! It lives in **core**, beside the `blobs` repo and for the same reason: core owns the
//! schema and the changelog, and a marker written without a changelog entry is a row `undo`
//! cannot see. That mattered concretely — an `undo` after an ingest deleted the document and
//! its chunks while the marker survived, after which every later `jkb ingest` of that file
//! took the "already captured" branch, failed to find the document, and errored. No CLI verb
//! deletes an ingestion row, so the file could never be ingested into that database again.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use crate::changelog::Entity;
use crate::store::WriteMeta;
use crate::{changelog, Result};

/// What identifies one ingestion attempt: the same key the table is unique on.
#[derive(Debug, Clone, Copy)]
pub struct Key<'a> {
    /// blake3 of the source bytes.
    pub source_hash: &'a str,
    /// The pipeline's version, so a changed chunker re-ingests.
    pub pipeline_version: i64,
    /// The chunking strategy.
    pub strategy: &'a str,
    /// The embedder model, so a changed model re-embeds.
    pub embedder_model: &'a str,
}

/// The recorded status for `key` (`captured` / `complete`), or `None` if never ingested.
///
/// # Errors
/// Returns an error if the query fails.
pub fn status(conn: &Connection, key: Key<'_>) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT status FROM ingestions
             WHERE source_hash = ?1 AND pipeline_version = ?2
               AND strategy = ?3 AND embedder_model = ?4",
        )?
        .query_row(
            params![
                key.source_hash,
                key.pipeline_version,
                key.strategy,
                key.embedder_model
            ],
            |r| r.get(0),
        )
        .optional()?)
}

/// Record that the capture stage finished: items exist, embedding is still to do.
///
/// Changelogged, so `undo` removes the marker in the same transaction as the items it
/// describes. `ingestions` is on `undo`'s table allowlist; it simply never had an entry.
///
/// # Errors
/// Returns an error if the insert or the changelog append fails.
pub fn record_capture(conn: &Connection, meta: &WriteMeta, key: Key<'_>) -> Result<()> {
    // Whether this is an insert is established BEFORE writing, the same rule `mount::create`
    // and `containment::contain` follow — a re-capture (the caller found a marker whose
    // document had been deleted) reuses the existing row, and logging that as an `insert`
    // would have `undo` delete a row that predates the transaction.
    let existing = status(conn, key)?;
    let rowid: i64 = conn
        .prepare_cached(
            "INSERT INTO ingestions
                 (source_hash, pipeline_version, strategy, embedder_model,
                  stage, status, blob_hash, started_at)
             VALUES (?1, ?2, ?3, ?4, 'embed', 'captured', ?1,
                  strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(source_hash, pipeline_version, strategy, embedder_model)
                 DO UPDATE SET stage = 'embed', status = 'captured', completed_at = NULL,
                     started_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             RETURNING rowid",
        )?
        .query_row(
            params![
                key.source_hash,
                key.pipeline_version,
                key.strategy,
                key.embedder_model
            ],
            |r| r.get(0),
        )?;
    changelog::append(
        conn,
        meta,
        if existing.is_some() {
            "update"
        } else {
            "insert"
        },
        Entity::Ingestions,
        &rowid.to_string(),
        existing.map(|status| json!({ "status": status })).as_ref(),
        Some(&json!({
            "source_hash": key.source_hash,
            "pipeline_version": key.pipeline_version,
            "strategy": key.strategy,
            "embedder_model": key.embedder_model,
            "status": "captured",
        })),
    )?;
    Ok(())
}

/// Mark the ingestion complete (vectors written).
///
/// Deliberately **not** changelogged: `undo` inverts inserts and a small set of updates, and
/// an undone completion would leave a `captured` marker describing items that still have
/// vectors. The marker's whole row is removed by undoing the capture that created it.
///
/// # Errors
/// Returns an error if the update fails.
pub fn mark_complete(conn: &Connection, key: Key<'_>) -> Result<()> {
    conn.prepare_cached(
        "UPDATE ingestions
            SET status = 'complete',
                completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
          WHERE source_hash = ?1 AND pipeline_version = ?2
            AND strategy = ?3 AND embedder_model = ?4",
    )?
    .execute(params![
        key.source_hash,
        key.pipeline_version,
        key.strategy,
        key.embedder_model
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{mark_complete, record_capture, status, Key};
    use crate::Db;

    /// The marker must go with the items it describes. It did not: `undo` deleted the
    /// document and left the marker, and ingest then refused that file forever.
    #[test]
    fn undoing_a_capture_removes_its_marker() {
        use crate::{item, undo};

        let db = Db::open_in_memory().unwrap();
        let key = Key {
            source_hash: "b3:abc",
            pipeline_version: 1,
            strategy: "chars",
            embedder_model: "m",
        };
        db.write_txn("t", move |c, m| {
            item::upsert(
                c,
                m,
                &item::NewItem {
                    uid: "doc".to_owned(),
                    kind: "document".to_owned(),
                    content: Some("body".to_owned()),
                    content_hash: None,
                    mime: None,
                },
            )?;
            record_capture(c, m, key)
        })
        .unwrap();
        assert_eq!(
            db.read(move |c| status(c, key)).unwrap().as_deref(),
            Some("captured")
        );

        db.write_txn("t", undo::undo_last).unwrap();
        assert!(
            db.read(move |c| status(c, key)).unwrap().is_none(),
            "the marker must not outlive the items it says were captured"
        );
    }

    #[test]
    fn completing_moves_the_status() {
        let db = Db::open_in_memory().unwrap();
        let key = Key {
            source_hash: "b3:def",
            pipeline_version: 1,
            strategy: "chars",
            embedder_model: "m",
        };
        db.write_txn("t", move |c, m| record_capture(c, m, key))
            .unwrap();
        db.write_txn("t", move |c, _m| mark_complete(c, key))
            .unwrap();
        assert_eq!(
            db.read(move |c| status(c, key)).unwrap().as_deref(),
            Some("complete")
        );
    }
}
