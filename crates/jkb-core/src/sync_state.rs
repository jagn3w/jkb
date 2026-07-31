//! Sync journal repository (design D25): file-level sync state, one row per synced
//! file `uri`. Where a `binding` records where one *item*'s bytes live, `sync_state`
//! records the reconcile state of a whole *file* — which serializer produced it, its
//! last-synced hash, a content-addressed reference to the last-synced **base** bytes
//! (for three-way merge), and any unresolved `conflict` / `needs_attention` status
//! with a parse error. Surfaced read-only as the `_sys/sync` view.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use crate::store::WriteMeta;
use crate::{changelog, Result};

/// One file's row in the sync journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncState {
    /// The file's `file://<path>` uri (bare, no `#fragment`).
    pub uri: String,
    /// The serializer that last produced this file.
    pub serializer: String,
    /// `ok`, `conflict`, or `needs_attention`.
    pub status: String,
    /// blake3 of the last-synced (rendered) bytes.
    pub last_synced_hash: Option<String>,
    /// `blobs.hash` of the last-synced bytes — the three-way base.
    pub base_blob_hash: Option<String>,
    /// Actionable message when `status = 'needs_attention'`.
    pub parse_error: Option<String>,
    /// `blobs.hash` of the failing bytes stashed on quarantine.
    pub quarantine_blob_hash: Option<String>,
    /// Timestamp of the last journal write.
    pub updated_at: String,
}

/// The desired journal row for one file, written in a single [`upsert`]. Borrowed so
/// the engine can pass through the existing row's hashes when only the status changes.
#[derive(Debug, Clone, Copy)]
pub struct SyncStateWrite<'a> {
    /// The file's `file://<path>` uri (bare, no `#fragment`).
    pub uri: &'a str,
    /// The serializer that produced this file.
    pub serializer: &'a str,
    /// `ok`, `conflict`, or `needs_attention`.
    pub status: &'a str,
    /// blake3 of the last-synced (rendered) bytes.
    pub last_synced_hash: Option<&'a str>,
    /// `blobs.hash` of the last-synced bytes — the three-way base.
    pub base_blob_hash: Option<&'a str>,
    /// Actionable message when `status = 'needs_attention'`.
    pub parse_error: Option<&'a str>,
    /// `blobs.hash` of the failing bytes stashed on quarantine.
    pub quarantine_blob_hash: Option<&'a str>,
}

/// Fetch the journal row for `uri`, if one exists.
///
/// # Errors
/// Returns an error if the query fails.
pub fn get(conn: &Connection, uri: &str) -> Result<Option<SyncState>> {
    let state = conn
        .prepare_cached(
            "SELECT uri, serializer, status, last_synced_hash, base_blob_hash,
                    parse_error, quarantine_blob_hash, updated_at
             FROM sync_state WHERE uri = ?1",
        )?
        .query_row([uri], |row| {
            Ok(SyncState {
                uri: row.get(0)?,
                serializer: row.get(1)?,
                status: row.get(2)?,
                last_synced_hash: row.get(3)?,
                base_blob_hash: row.get(4)?,
                parse_error: row.get(5)?,
                quarantine_blob_hash: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })
        .optional()?;
    Ok(state)
}

/// Insert or replace the journal row for a file (the single write point), stamping
/// `updated_at` and recording the change in the changelog.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
pub fn upsert(conn: &Connection, meta: &WriteMeta, w: &SyncStateWrite) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO sync_state
             (uri, serializer, status, last_synced_hash, base_blob_hash,
              parse_error, quarantine_blob_hash, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ON CONFLICT(uri) DO UPDATE SET
             serializer = excluded.serializer, status = excluded.status,
             last_synced_hash = excluded.last_synced_hash,
             base_blob_hash = excluded.base_blob_hash,
             parse_error = excluded.parse_error,
             quarantine_blob_hash = excluded.quarantine_blob_hash,
             updated_at = excluded.updated_at",
    )?
    .execute(params![
        w.uri,
        w.serializer,
        w.status,
        w.last_synced_hash,
        w.base_blob_hash,
        w.parse_error,
        w.quarantine_blob_hash,
    ])?;
    // `base_blob_hash` is recorded here on purpose: the blob it names is the exact bytes of
    // this file at this sync, and blobs are never deleted. Journalling the hash turns the
    // blob store from an anonymous archive into a per-file **history** — "what did this file
    // look like at each sync, and which blob holds it" — which is what you need when a sync
    // has already written a wrong version over your work.
    changelog::append(
        conn,
        meta,
        "update",
        "sync_state",
        w.uri,
        None,
        Some(&json!({
            "status": w.status,
            "serializer": w.serializer,
            "base_blob_hash": w.base_blob_hash,
            "last_synced_hash": w.last_synced_hash,
        })),
    )?;
    Ok(())
}

/// Every file currently in a non-`ok` state (`conflict` / `needs_attention`), ordered
/// by uri. Backs the `jkb doctor` diagnostic.
///
/// # Errors
/// Returns an error if the query fails.
pub fn needs_attention(conn: &Connection) -> Result<Vec<SyncState>> {
    let mut stmt = conn.prepare_cached(
        "SELECT uri, serializer, status, last_synced_hash, base_blob_hash,
                parse_error, quarantine_blob_hash, updated_at
         FROM sync_state WHERE status != 'ok' ORDER BY uri",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SyncState {
            uri: row.get(0)?,
            serializer: row.get(1)?,
            status: row.get(2)?,
            last_synced_hash: row.get(3)?,
            base_blob_hash: row.get(4)?,
            parse_error: row.get(5)?,
            quarantine_blob_hash: row.get(6)?,
            updated_at: row.get(7)?,
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
    use super::{get, needs_attention, upsert, SyncStateWrite};
    use crate::Db;

    #[test]
    fn upsert_get_and_needs_attention() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            upsert(
                conn,
                meta,
                &SyncStateWrite {
                    uri: "file:///r/tasks.md",
                    serializer: "tasks",
                    status: "ok",
                    last_synced_hash: Some("h1"),
                    base_blob_hash: Some("b1"),
                    parse_error: None,
                    quarantine_blob_hash: None,
                },
            )?;
            upsert(
                conn,
                meta,
                &SyncStateWrite {
                    uri: "file:///r/broken.md",
                    serializer: "tasks",
                    status: "needs_attention",
                    last_synced_hash: Some("h2"),
                    base_blob_hash: Some("b2"),
                    parse_error: Some("bad token on line 3"),
                    quarantine_blob_hash: Some("q2"),
                },
            )
        })
        .unwrap();

        let ok = db
            .read(|conn| get(conn, "file:///r/tasks.md"))
            .unwrap()
            .unwrap();
        assert_eq!(ok.status, "ok");
        assert_eq!(ok.base_blob_hash.as_deref(), Some("b1"));

        let flagged = db.read(needs_attention).unwrap();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].uri, "file:///r/broken.md");
        assert_eq!(
            flagged[0].parse_error.as_deref(),
            Some("bad token on line 3")
        );

        // The _sys/sync view exposes the same rows.
        let via_view: i64 = db
            .read(|conn| Ok(conn.query_row("SELECT count(*) FROM sys_sync", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(via_view, 2);
    }

    #[test]
    fn upsert_replaces_on_conflict() {
        let db = Db::open_in_memory().unwrap();
        let write = |status: &'static str, hash: &'static str| {
            let db = &db;
            db.write_txn("t", move |conn, meta| {
                upsert(
                    conn,
                    meta,
                    &SyncStateWrite {
                        uri: "file:///r/a.md",
                        serializer: "document",
                        status,
                        last_synced_hash: Some(hash),
                        base_blob_hash: Some(hash),
                        parse_error: None,
                        quarantine_blob_hash: None,
                    },
                )
            })
            .unwrap();
        };
        write("ok", "h1");
        write("ok", "h2");
        let row = db
            .read(|conn| get(conn, "file:///r/a.md"))
            .unwrap()
            .unwrap();
        assert_eq!(row.last_synced_hash.as_deref(), Some("h2"));
        let count: i64 = db
            .read(|conn| Ok(conn.query_row("SELECT count(*) FROM sync_state", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 1);
    }
}
