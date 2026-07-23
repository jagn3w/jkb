//! Database connection setup: open, configure PRAGMAs, and migrate.
//!
//! The single-writer *writer-actor* and the repositories are built on top of this
//! in Section 4; here we just produce a correctly-configured, migrated connection.

use std::path::Path;

use rusqlite::Connection;

use crate::{migrate, Result};

/// Open (or create) a jkb database at `path`, configure PRAGMAs, and apply any
/// pending migrations.
///
/// # Errors
/// Returns an error if the database cannot be opened, configured, or migrated.
pub fn open<P: AsRef<Path>>(path: P) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    configure(&conn)?;
    migrate::run(&mut conn)?;
    Ok(conn)
}

/// Open a fresh in-memory database, configured and migrated. Intended for tests.
///
/// # Errors
/// Returns an error if configuration or migration fails.
pub fn open_in_memory() -> Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate::run(&mut conn)?;
    Ok(conn)
}

/// Apply the connection-level PRAGMAs jkb relies on (design D8):
/// WAL for concurrent reads, enforced foreign keys, `NORMAL` sync (WAL-safe), and
/// a busy timeout so brief lock contention waits instead of erroring.
fn configure(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{open, open_in_memory};

    #[test]
    fn migrations_create_tables_and_seed_sys_namespaces() {
        let conn = open_in_memory().unwrap();

        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN (
                     'namespaces', 'items', 'bindings', 'mounts', 'placements',
                     'edges', 'tag_defs', 'tag_applications', 'blobs',
                     'ingestions', 'embeddings_meta', 'changelog'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 12);

        let sys_namespaces: i64 = conn
            .query_row(
                "SELECT count(*) FROM namespaces WHERE path LIKE '_sys%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // _sys, _sys/transactions, _sys/ingestions (V001) + _sys/sync (V004).
        assert_eq!(sys_namespaces, 4);

        let user_version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        // Bumped with each migration: V001 init, V002 fts, V003 embeddings_meta version,
        // V004 sync journal, V005 task claims, V006 items.status CHECK.
        assert_eq!(user_version, 6);
    }

    #[test]
    fn fts_trigger_indexes_content_and_integrity_holds() {
        let conn = open_in_memory().unwrap();

        conn.execute(
            "INSERT INTO items (uid, kind, content) VALUES ('n:1', 'note', 'the quick brown fox')",
            [],
        )
        .unwrap();

        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM fts_items WHERE fts_items MATCH 'brown'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);

        // External-content FTS5 self-consistency check must pass.
        conn.execute_batch("INSERT INTO fts_items(fts_items) VALUES('integrity-check');")
            .unwrap();
    }

    #[test]
    fn reopening_a_file_db_is_idempotent_and_uses_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jkb.db");

        drop(open(&path).unwrap()); // first open: applies migrations
        let conn = open(&path).unwrap(); // second open: migrations are a no-op

        let user_version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(user_version, 6);

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
}
