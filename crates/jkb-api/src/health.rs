//! `kb.health`: the database side of `jkb doctor` (tasks S6.4 stage 5, design-s6-4.md I).
//!
//! What only the database can answer — its schema version, its full-text index, its sync journal, its
//! derived vector rows — in one op, so `jkb doctor` reports the same thing on the host and in the dev
//! container. What only the host can answer (the embedder, the old worktree-removal store, the
//! database's folder) and what only the client can answer (whether a claim's owner is alive) are not
//! here; `--fix` and `--backup`, which change the host, stay host-only.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::ApiError;

/// A synced file the journal flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flagged {
    /// The file's uri.
    pub uri: String,
    /// `conflict` or `needs_attention`.
    pub status: String,
    /// What is wrong, when the journal says.
    pub detail: Option<String>,
}

/// The most flagged files one answer lists.
pub const MAX_FLAGGED: usize = 200;

/// What `kb.health` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    /// The schema version (`PRAGMA user_version`).
    pub schema_version: i64,
    /// The full-text index passed its integrity check.
    pub fts_ok: bool,
    /// Files the sync journal flags, the first [`MAX_FLAGGED`].
    pub flagged: Vec<Flagged>,
    /// How many files it flags in all.
    pub flagged_count: usize,
    /// How many vector tables there are.
    pub vector_tables: usize,
    /// Vector rows whose item is gone.
    pub stale_vectors: usize,
}

/// `kb.health`. Not an agent read: FTS5's integrity check is spelled as an `INSERT`, which the
/// daemon's `query_only` reader refuses.
///
/// # Errors
/// A failed read.
pub fn health(conn: &Connection) -> Result<Health, ApiError> {
    let schema_version = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(jkb_core::Error::from)?;
    let fts_ok = jkb_index::FtsIndexer::new().integrity_check(conn).is_ok();
    let all = jkb_core::sync_state::needs_attention(conn)?;
    let flagged_count = all.len();
    let flagged = all
        .into_iter()
        .take(MAX_FLAGGED)
        .map(|s| Flagged {
            uri: s.uri,
            status: s.status,
            detail: s.parse_error,
        })
        .collect();
    let vector_tables = jkb_index::vector_tables(conn)
        .map_err(|e| index_error(&e))?
        .len();
    let stale_vectors = if vector_tables == 0 {
        0
    } else {
        jkb_index::count_stale(conn)
            .map_err(|e| index_error(&e))?
            .vectors
    };
    Ok(Health {
        schema_version,
        fts_ok,
        flagged,
        flagged_count,
        vector_tables,
        stale_vectors,
    })
}

fn index_error(e: &jkb_index::Error) -> ApiError {
    ApiError::with_code(crate::ErrorCode::Internal, e.to_string())
}

#[cfg(test)]
mod tests {
    use jkb_core::Db;

    /// Past [`super::MAX_FLAGGED`] the list is cut and the count is not.
    #[test]
    fn the_flagged_files_are_counted_past_the_listed_ones() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |c, m| {
            for i in 0..=super::MAX_FLAGGED {
                let uri = format!("file:///r/f{i}.md");
                jkb_core::sync_state::upsert(
                    c,
                    m,
                    &jkb_core::sync_state::SyncStateWrite {
                        uri: &uri,
                        serializer: "document",
                        status: "needs_attention",
                        last_synced_hash: None,
                        base_blob_hash: None,
                        parse_error: Some("bad"),
                        quarantine_blob_hash: None,
                        document: None,
                    },
                )?;
            }
            Ok(())
        })
        .unwrap();
        let h = db.read_with(super::health).unwrap();
        assert_eq!(h.flagged.len(), super::MAX_FLAGGED);
        assert_eq!(h.flagged_count, super::MAX_FLAGGED + 1);
        assert!(h.fts_ok);
        assert_eq!((h.vector_tables, h.stale_vectors), (0, 0));
        assert_eq!(h.flagged[0].detail.as_deref(), Some("bad"));
    }
}
