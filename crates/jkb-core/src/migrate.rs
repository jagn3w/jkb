//! Schema migrations.
//!
//! Migrations live as `V<n>__<name>.sql` files under `src/migrations/` and are
//! embedded into the binary at compile time by `refinery`. Refinery applies each
//! pending migration in a transaction and records it (with a checksum) in its
//! `refinery_schema_history` table — so a previously-applied migration that is
//! later edited is detected rather than silently ignored.

use rusqlite::Connection;

use crate::{Error, Result};

// `embed_migrations!` generates a `migrations` module from the SQL files. It is
// machine-generated, so we relax the pedantic lints for just this module.
#[allow(clippy::pedantic)]
mod embedded {
    refinery::embed_migrations!("src/migrations");
}

/// Apply all pending migrations, then stamp `PRAGMA user_version` with the highest
/// applied version as a human-readable marker for `jkb doctor`. Refinery's history
/// table remains the authoritative record.
///
/// Migrations run with `PRAGMA foreign_keys = OFF` so a migration that rebuilds a table
/// (create-new / copy / `DROP` old / rename) is not tripped by the implicit *cascading*
/// delete that FK enforcement performs when `DROP TABLE` empties a table with
/// `ON DELETE CASCADE` children. `PRAGMA foreign_keys` is a no-op inside a transaction and
/// refinery wraps each migration in one, so the toggle must live here, outside any
/// transaction. Enforcement is restored (and a `foreign_key_check` run) afterward — the
/// SQLite-recommended way to run schema migrations.
///
/// # Errors
/// Returns [`crate::Error::Migration`] if a migration fails to apply,
/// [`crate::Error::ForeignKeyViolation`] if a migration left a dangling foreign-key
/// reference, or [`crate::Error::Sqlite`] if a PRAGMA or the version marker fails.
pub fn run(conn: &mut Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;

    let migrate_result = embedded::migrations::runner().run(conn);

    // Verify referential integrity only when the migrations applied cleanly, then restore
    // enforcement regardless of outcome so the connection is never left with FKs off.
    let violations = if migrate_result.is_ok() {
        count_foreign_key_violations(conn)?
    } else {
        0
    };
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;

    migrate_result?;
    if violations > 0 {
        return Err(Error::ForeignKeyViolation(violations));
    }

    let version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM refinery_schema_history",
        [],
        |row| row.get(0),
    )?;
    // Only stamp when it actually changed. `pragma_update` writes the database header — a
    // WAL write in WAL mode — and `run` executes on *every* `Db::open`, so stamping
    // unconditionally would make every read-only command (e.g. `jkb ls`) a writer. That
    // churns the WAL and trips file-watchers on reads (it made the VS Code explorer's live
    // refresh loop on its own queries). On a fully-migrated database this is now a no-op.
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current != version {
        conn.pragma_update(None, "user_version", version)?;
    }
    Ok(())
}

/// Count rows returned by `PRAGMA foreign_key_check` — one per dangling foreign-key
/// reference in the database (0 = clean).
fn count_foreign_key_violations(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = stmt.query([])?;
    let mut count = 0usize;
    while rows.next()?.is_some() {
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use crate::db::open_in_memory;

    #[test]
    fn status_check_allows_null_and_valid_rejects_invalid() {
        // `open_in_memory` applies every migration, including V006's rebuild + CHECK.
        let conn = open_in_memory().unwrap();

        // A non-task item with NULL status inserts.
        conn.execute(
            "INSERT INTO items (uid, kind, status) VALUES ('n1', 'text', NULL)",
            [],
        )
        .unwrap();

        // A valid task status inserts.
        conn.execute(
            "INSERT INTO items (uid, kind, status) VALUES ('t1', 'task', 'in_progress')",
            [],
        )
        .unwrap();

        // An out-of-set status is rejected by the DB (the CHECK), not just the Rust layer.
        assert!(
            conn.execute(
                "INSERT INTO items (uid, kind, status) VALUES ('t2', 'task', 'bogus')",
                [],
            )
            .is_err(),
            "invalid status should violate the CHECK constraint"
        );

        // The derived `blocked` state is never stored, so it must be rejected too.
        assert!(
            conn.execute(
                "INSERT INTO items (uid, kind, status) VALUES ('t3', 'task', 'blocked')",
                [],
            )
            .is_err(),
            "derived 'blocked' status must be rejected"
        );
    }
}
