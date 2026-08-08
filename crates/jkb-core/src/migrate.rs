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

#[cfg(test)]
mod high_water_tests {
    use super::embedded;
    use refinery::Target;
    use rusqlite::Connection;

    /// V010 claimed to stop `items.id` reuse and did not; V011 is the fix (design D42.1).
    ///
    /// This has to migrate a database **in two steps** — up to V010, populate it the way a real
    /// store gets populated, then apply V011 — so it cannot use `Db::open`, which runs every
    /// migration at once. `mod migrate` and `mod db` are private, so it also cannot live in
    /// `tests/`; it belongs here, beside the runner it drives.
    ///
    /// Asserts the harm, not a counter: an id that was issued and freed must never be handed to
    /// a later item, because a `vec_items_<dim>` row outlives its item and the new item would
    /// inherit its embedding.
    #[test]
    fn v011_restores_the_high_water_mark_v010_lost() {
        let mut conn = Connection::open_in_memory().unwrap();
        // The harness owns this toggle because `PRAGMA foreign_keys` is a no-op inside the
        // transaction refinery wraps each migration in, and V010 rebuilds a table with
        // `ON DELETE CASCADE` children.
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();

        // 1. Migrate to V010 exactly — the state every existing database is in.
        embedded::migrations::runner()
            .set_target(Target::Version(10))
            .run(&mut conn)
            .unwrap();

        // 2. Five items, then delete the top two — what `jkb ingest` + `jkb undo` leaves behind,
        //    with vector rows for 4 and 5 still in `vec_items_<dim>`.
        for i in 1..=5 {
            conn.execute(
                "INSERT INTO items (id, uid, kind) VALUES (?1, ?2, 'chunk')",
                rusqlite::params![i, format!("u{i}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO changelog (txn_id, actor, op, entity_type, entity_id)
                 VALUES (1, 't', 'insert', 'items', ?1)",
                [i.to_string()],
            )
            .unwrap();
        }
        conn.execute("DELETE FROM items WHERE id IN (4, 5)", [])
            .unwrap();

        // Reproduce V010's own damage: it seeded from the *surviving* max and left two rows.
        let rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_sequence WHERE name = 'items'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(rows >= 1, "V010 seeded sqlite_sequence");

        // 3. Apply V011.
        embedded::migrations::runner().run(&mut conn).unwrap();

        // 4. The HARM first, so a regression reports the thing that matters rather than a
        //    bookkeeping detail: a new item must not land on 4 or 5.
        conn.execute(
            "INSERT INTO items (uid, kind) VALUES ('fresh', 'chunk')",
            [],
        )
        .unwrap();
        let fresh: i64 = conn
            .query_row("SELECT id FROM items WHERE uid = 'fresh'", [], |r| r.get(0))
            .unwrap();
        assert!(
            fresh > 5,
            "id {fresh} was reissued after 4 and 5 were freed — a new item would inherit a \
             deleted item's embedding"
        );

        // Then the hygiene: exactly one sequence row, so V010's duplicate is gone.
        let seq_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_sequence WHERE name = 'items'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            seq_rows, 1,
            "V011 must leave exactly one row, not add a third"
        );
    }
}
