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

/// One stored blob's metadata (without its bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobInfo {
    /// The blake3 hex key.
    pub hash: String,
    /// Size in bytes.
    pub size: i64,
    /// MIME type, if the writer recorded one.
    pub mime: Option<String>,
    /// When it was first stored (ISO).
    pub created_at: String,
}

/// List stored blobs, newest first, optionally restricted to those **containing**
/// `contains` as a byte substring.
///
/// This is the read side of the archive nothing else exposes. File sync stores the bytes
/// of every version it settles, and blobs are never deleted — so the store already holds a
/// complete history of every synced file, and until now there was no way to look at it. When
/// a sync writes a wrong version over a file, searching the blobs for a line you remember is
/// the recovery path.
///
/// # Errors
/// Returns an error if the query fails.
pub fn list(conn: &Connection, contains: Option<&[u8]>, limit: usize) -> Result<Vec<BlobInfo>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    // `instr` over two BLOBs is a byte-substring test, so this needs no lossy text
    // conversion and works on non-UTF-8 content.
    let mut stmt = conn.prepare_cached(
        "SELECT hash, size, mime, created_at FROM blobs
         WHERE ?1 IS NULL OR instr(bytes, ?1) > 0
         ORDER BY created_at DESC, hash
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![contains, limit], |r| {
        Ok(BlobInfo {
            hash: r.get(0)?,
            size: r.get(1)?,
            mime: r.get(2)?,
            created_at: r.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{hash_bytes, list, load, store};
    use crate::Db;

    #[test]
    fn list_finds_a_blob_by_its_content() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, _m| {
            for body in [
                b"alpha version one".as_slice(),
                b"beta version two".as_slice(),
            ] {
                store(conn, &hash_bytes(body), body, Some("text/plain"))?;
            }
            Ok(())
        })
        .unwrap();

        // Unfiltered lists everything.
        assert_eq!(db.read(|c| list(c, None, 100)).unwrap().len(), 2);

        // A content search finds the version that carries a remembered line — the recovery
        // path when a bad write has already landed on disk.
        let hits = db.read(|c| list(c, Some(b"beta".as_slice()), 100)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            db.read({
                let hash = hits[0].hash.clone();
                move |c| load(c, &hash)
            })
            .unwrap()
            .unwrap(),
            b"beta version two"
        );
        assert_eq!(hits[0].size, 16);

        // A miss is empty, not an error.
        assert!(db
            .read(|c| list(c, Some(b"gamma".as_slice()), 100))
            .unwrap()
            .is_empty());
    }

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
