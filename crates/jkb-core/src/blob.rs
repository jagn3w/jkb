//! Content-addressed blob store: raw bytes keyed by their blake3 hash.
//!
//! The `blobs` table lives in this crate's schema (`V001`), so the store is owned
//! here and shared by every crate that needs it: `jkb-ingest` stores raw sources
//! (design D7), and `jkb-sync` stores last-synced base bytes and quarantined bytes
//! (design D25). Blobs are immutable and content-addressed, so storing is a dedupe
//! `INSERT OR IGNORE` and is deliberately **not** changelogged (there is nothing to
//! undo — the same hash always maps to the same bytes).

use rusqlite::{params, Connection, OptionalExtension};

use jkb_types::Error as TypeError;

use crate::{Error, Result};

/// The blake3 hash of `bytes`, as a lowercase hex string. This is the blob key and
/// the same scheme used for `content_hash` and the sync hashes.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Store `bytes` under `hash` (content-addressed), deduping identical content.
/// Idempotent: re-storing the same hash is a no-op.
///
/// # Errors
/// Returns an error if `bytes` is too large to record its size, or the insert fails.
pub fn store(conn: &Connection, hash: &str, bytes: &[u8], mime: Option<&str>) -> Result<()> {
    let size = i64::try_from(bytes.len())
        .map_err(|_| Error::Types(TypeError::Validation("blob too large".to_owned())))?;
    conn.prepare_cached(
        "INSERT OR IGNORE INTO blobs (hash, bytes, mime, size) VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute(params![hash, bytes, mime, size])?;
    Ok(())
}

/// Load the bytes stored under `hash`, if present.
///
/// # Errors
/// Returns an error if the query fails.
pub fn load(conn: &Connection, hash: &str) -> Result<Option<Vec<u8>>> {
    let bytes: Option<Vec<u8>> = conn
        .prepare_cached("SELECT bytes FROM blobs WHERE hash = ?1")?
        .query_row([hash], |row| row.get(0))
        .optional()?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{hash_bytes, load, store};
    use crate::Db;

    #[test]
    fn hash_is_deterministic_and_hex() {
        let a = hash_bytes(b"hello");
        assert_eq!(a, hash_bytes(b"hello"));
        assert_ne!(a, hash_bytes(b"world"));
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn store_then_load_round_trips_and_dedups() {
        let db = Db::open_in_memory().unwrap();
        let hash = hash_bytes(b"payload");
        db.write_txn("t", {
            let hash = hash.clone();
            move |conn, _meta| {
                store(conn, &hash, b"payload", Some("text/plain"))?;
                store(conn, &hash, b"payload", Some("text/plain"))?; // idempotent
                Ok(())
            }
        })
        .unwrap();

        let got = db.read(move |conn| load(conn, &hash)).unwrap();
        assert_eq!(got.as_deref(), Some(&b"payload"[..]));

        let count: i64 = db
            .read(|conn| Ok(conn.query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn load_missing_is_none() {
        let db = Db::open_in_memory().unwrap();
        let got = db.read(|conn| load(conn, "deadbeef")).unwrap();
        assert!(got.is_none());
    }
}
