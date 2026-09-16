//! Database connection setup: open, configure PRAGMAs, and migrate.
//!
//! The single-writer *writer-actor* and the repositories are built on top of this
//! in Section 4; here we just produce a correctly-configured, migrated connection.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::{migrate, Result};

/// Open (or create) a jkb database at `path`, configure PRAGMAs, and apply any
/// pending migrations.
///
/// Refuses first, before `SQLite` creates or touches anything, when the database would live on a
/// filesystem shared with another kernel ([`crate::shared_fs`]).
///
/// The path is a PATH, never a URI: `refuse` rejects a `file:` string outright. Dropping
/// `SQLITE_OPEN_URI` from the flags is NOT enough on its own and is kept only as intent — the
/// bundled `SQLite` is compiled with `-DSQLITE_USE_URI`, and `--db file:/home/vscode/.jkb/jkb.db`
/// opened the host's database with these very flags (measured in the dev container).
///
/// # Errors
/// Returns an error if the database is on a shared filesystem, or cannot be opened, configured, or
/// migrated.
pub fn open<P: AsRef<Path>>(path: P) -> Result<Connection> {
    crate::shared_fs::refuse(path.as_ref())?;
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    configure(&conn)?;
    migrate::run(&mut conn)?;
    Ok(conn)
}

/// Open a second connection to an existing, already-migrated database, for reads only: it runs no
/// migrations, never creates the file, and has `query_only` set, so a write through it fails. What
/// lets a process serve long reads beside its writer (WAL readers do not wait on the writer) without
/// a read being able to write — `jkb serve` answers the dev container's reads on one.
///
/// # Errors
/// The shared-filesystem refusal, or a failed open or configuration.
pub fn open_reader<P: AsRef<Path>>(path: P) -> Result<Connection> {
    crate::shared_fs::refuse(path.as_ref())?;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    configure(&conn)?;
    conn.execute_batch("PRAGMA query_only = ON;")?;
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
        // V004 sync journal, V005 task claims, V006 items.status CHECK,
        // V007 memory core (items.resolution + edges.weight),
        // V008 reserved namespace types, V009 placement containment,
        // V010 items.id AUTOINCREMENT, V011 the id high-water mark V010 lost,
        // V012 sync_state.document (a file's structure leaves the namespace tree),
        // V013 branch_records (a branch is a record, not a tag value),
        // V014 the undo watermark (undo history begins after the entries written under the
        // audit-only contract), V015 the task lifecycle history,
        // V016 drops branch_records (the history holds what it held, as events),
        // V017 the message queue (mq_topics, mq_messages, mq_groups),
        // V018 the notification record (notify_sessions),
        // V019 the Claude Code session registry (claude_sessions),
        // V020 the worktree-removal records and the leases (worktree_removals, leases).
        assert_eq!(user_version, 20);

        // V008 typed the reserved system namespaces it found (design D33.4). `tasks` is
        // not seeded by a migration, so only the `_sys` markers are typed here.
        let journals: i64 = conn
            .query_row(
                "SELECT count(*) FROM namespaces
                 WHERE json_extract(metadata, '$.type') = 'journal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(journals, 3, "_sys/{{transactions,ingestions,sync}}");

        // V007's additive columns exist and default to NULL (no back-fill).
        conn.execute(
            "INSERT INTO items (uid, kind, content) VALUES ('n:res', 'note', 'body')",
            [],
        )
        .unwrap();
        let resolution: Option<String> = conn
            .query_row(
                "SELECT resolution FROM items WHERE uid = 'n:res'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(resolution, None, "resolution defaults to NULL = unresolved");

        // The CHECK constraint rejects an unknown resolution at the storage boundary.
        let bad = conn.execute(
            "UPDATE items SET resolution = 'refuted' WHERE uid = 'n:res'",
            [],
        );
        assert!(bad.is_err(), "unknown resolution must be rejected");
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

    /// `db::open` is the only place a database FILE is opened, so the shared-filesystem refusal it
    /// runs first (`shared_fs::refuse`) covers every process. A `Connection::open(path)` anywhere
    /// else in the workspace would open a database the refusal never saw — on the container kernel,
    /// the host's database, which a process on each side corrupts. In-memory opens touch no file.
    ///
    /// A string scan, not a parse: an aliased import (`use rusqlite::Connection as C`) would
    /// evade it, and is stated rather than guarded.
    #[test]
    fn no_database_file_is_opened_outside_db_rs() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = Vec::new();
        walk(&crates, &mut files);
        assert!(
            files.len() > 50,
            "scanned {} files; the walk is broken",
            files.len()
        );

        let this = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/db.rs");
        let this = std::fs::canonicalize(this).unwrap();

        // db.rs is exempt from the scan below only because its one file open is guarded — so that
        // is asserted rather than assumed: inside `pub fn open`, the refusal comes before the open.
        // Without this, deleting the refusal line left every test green.
        let own = std::fs::read_to_string(&this).unwrap();
        // Every file open in db.rs, not only the first: `open_reader` is the second.
        // At a line start, so this test's own strings are not taken for functions.
        let opens: Vec<usize> = own
            .match_indices("\npub fn open")
            .map(|(at, _)| at + 1)
            .collect();
        assert!(opens.len() >= 3, "db.rs's opens were not found: {opens:?}");
        for body_at in opens {
            let body = &own[body_at
                ..own[body_at..]
                    .find("\n}\n")
                    .map_or(own.len(), |e| body_at + e)];
            if body.starts_with("pub fn open_in_memory") {
                continue;
            }
            let refuse_at = body.find("shared_fs::refuse(").expect(
                "every db.rs open must call shared_fs::refuse — without it a database on a shared filesystem opens",
            );
            let open_at = body
                .find("Connection::open")
                .expect("a db.rs open opens a connection");
            assert!(
                refuse_at < open_at,
                "a db.rs open must refuse BEFORE it opens, or SQLite has already created files"
            );
            assert!(
                !body.contains("SQLITE_OPEN_URI"),
                "a db.rs open must not accept a URI, or `file:` paths reach a database refuse did not judge"
            );
        }
        let mut strays = Vec::new();
        for file in files {
            if std::fs::canonicalize(&file).unwrap() == this {
                continue;
            }
            let src = std::fs::read_to_string(&file).unwrap();
            for (i, line) in src.lines().enumerate() {
                let mut rest = line;
                while let Some(at) = rest.find("Connection::open") {
                    let tail = &rest[at + "Connection::open".len()..];
                    if !tail.starts_with("_in_memory") {
                        strays.push(format!("{}:{}", file.display(), i + 1));
                    }
                    rest = tail;
                }
            }
        }
        assert!(
            strays.is_empty(),
            "a database file is opened outside db::open, so the shared-filesystem refusal never \
             runs for it — route it through jkb_core::Db: {strays:?}"
        );
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
        assert_eq!(user_version, 20);
        assert_eq!(
            crate::supported_schema_version(),
            user_version,
            "a freshly migrated database is at exactly the version this build supports"
        );

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
}
