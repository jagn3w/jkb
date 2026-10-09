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

/// The newest schema version this build's migrations produce.
///
/// Compared with [`applied_version`] — refinery's highest applied migration — by [`refuse_newer`],
/// which every write transaction asks: a newer binary may have migrated the database underneath a
/// long-running process, and writing with this code's queries is how it would put rows a newer schema
/// does not expect.
#[must_use]
pub fn supported_version() -> i64 {
    static SUPPORTED: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        embedded::migrations::runner()
            .get_migrations()
            .iter()
            .map(|m| i64::from(m.version()))
            .max()
            .unwrap_or(0)
    })
}

/// The newest migration applied to this database, from refinery's history table — which each
/// migration writes inside its own transaction, unlike `PRAGMA user_version`, stamped only after all
/// of them (and the foreign-key check) finish. 0 for a database never migrated.
///
/// # Errors
/// [`crate::Error::Sqlite`] if the history cannot be read.
pub fn applied_version(conn: &Connection) -> Result<i64> {
    let has_history: bool = conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' \
             AND name = 'refinery_schema_history')",
        )?
        .query_row([], |row| row.get(0))?;
    if !has_history {
        return Ok(0);
    }
    Ok(conn
        .prepare_cached("SELECT COALESCE(MAX(version), 0) FROM refinery_schema_history")?
        .query_row([], |row| row.get(0))?)
}

/// Refuse a database a newer jkb has migrated. [`crate::Db::write_txn`] asks this inside its
/// IMMEDIATE transaction, so no migration can commit between the question and the write — a
/// long-lived process (the daemon, the sync watcher) must not write this build's row shapes into a
/// schema it does not know.
///
/// # Errors
/// [`crate::Error::SchemaNewer`], or [`crate::Error::Sqlite`] if the history cannot be read.
pub fn refuse_newer(conn: &Connection) -> Result<()> {
    let (found, supported) = (applied_version(conn)?, supported_version());
    if found > supported {
        return Err(Error::SchemaNewer { found, supported });
    }
    Ok(())
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
/// Returns [`crate::Error::SchemaNewer`] for a database already past this build's migrations,
/// [`crate::Error::Migration`] if a migration fails to apply,
/// [`crate::Error::ForeignKeyViolation`] if a migration left a dangling foreign-key
/// reference, or [`crate::Error::Sqlite`] if a PRAGMA or the version marker fails.
pub fn run(conn: &mut Connection) -> Result<()> {
    refuse_newer(conn)?;
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

        // 1. Migrate to V009 — BEFORE the AUTOINCREMENT rebuild. This ordering is the whole
        //    fixture: V010 reseeds `sqlite_sequence` from the *surviving* maximum, so the ids
        //    have to already exist and already be deleted when it runs. Populating after V010
        //    instead sets the sequence via the explicit-id inserts themselves and masks the bug
        //    entirely — which is exactly what an earlier version of this test did, leaving the
        //    only regression pin for an id-reuse corruption passing against a no-op migration.
        embedded::migrations::runner()
            .set_target(Target::Version(9))
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

        // 3. Now V010 runs on that database and does its damage: it reseeds the counter to the
        //    surviving maximum (3) and leaves a duplicate row behind.
        embedded::migrations::runner()
            .set_target(Target::Version(10))
            .run(&mut conn)
            .unwrap();
        let seq_after_v010: i64 = conn
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'items' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            seq_after_v010, 3,
            "the fixture must actually reproduce V010's damage, or this test pins nothing"
        );

        // 4. Apply V011.
        embedded::migrations::runner().run(&mut conn).unwrap();

        // 5. The HARM first, so a regression reports the thing that matters rather than a
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

#[cfg(test)]
mod reviews_migration_tests {
    use super::embedded;
    use refinery::Target;
    use rusqlite::Connection;

    /// V023 moves the land gate's review facts out of tags (design D52.7). Migrated in two steps, like
    /// the V011 test beside it, so the tags exist in the shapes a real store holds before V023 runs:
    /// several rounds with one head, a head with no round, a waiver, and an unrelated tag to leave.
    #[test]
    #[allow(clippy::too_many_lines)] // one migration scenario, seeded and read back top to bottom
    fn v023_moves_review_facets_into_reviews_and_leaves_other_tags() {
        let mut conn = Connection::open_in_memory().unwrap();
        embedded::migrations::runner()
            .set_target(Target::Version(22))
            .run(&mut conn)
            .unwrap();
        for (id, uid, priority) in [
            (1, "task:a", None),
            (2, "task:b", None),
            (3, "task:c", None),
            (4, "task:backlog", Some(2)),
            (5, "task:f-old", Some(1)),
            (6, "task:f-new", Some(3)),
        ] {
            conn.execute(
                "INSERT INTO items (id, uid, kind, priority) VALUES (?1, ?2, 'task', ?3)",
                rusqlite::params![id, uid, priority],
            )
            .unwrap();
        }
        // Each round's findings: the older round's is a must-fix.
        for (ns_id, path, item) in [
            (101, "codereviews/20260820-1/must-fix", 5),
            (102, "codereviews/20260821-2/nit", 6),
        ] {
            conn.execute(
                "INSERT INTO namespaces (id, path, kind) VALUES (?1, ?2, 'logical')",
                rusqlite::params![ns_id, path],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO placements (item_id, namespace_id, role) VALUES (?1, ?2, 'home')",
                rusqlite::params![item, ns_id],
            )
            .unwrap();
        }
        for (item, facet, value) in [
            (1, "review", "codereviews/20260821-2"),
            (1, "review", "codereviews/20260820-1"),
            (1, "reviewed", "51c459a"),
            (1, "repo", "jkb"),
            (2, "reviewed", "abc"),
            (3, "review-waived", "def"),
            // A backlog finding's trail back to the review that found it: no `reviewed=` beside
            // it, so it records no review — and it stays, as the ordinary tag it always was.
            (4, "review", "codereviews/20260821-2"),
        ] {
            conn.execute(
                "INSERT INTO tag_applications (item_id, facet, value) VALUES (?1, ?2, ?3)",
                rusqlite::params![item, facet, value],
            )
            .unwrap();
        }
        embedded::migrations::runner().run(&mut conn).unwrap();

        let rows: Vec<(i64, String, Option<String>, String)> = conn
            .prepare("SELECT item_id, kind, ns, sha FROM reviews ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let s = |v: &str| v.to_owned();
        assert_eq!(
            rows,
            vec![
                (
                    1,
                    s("recorded"),
                    Some(s("codereviews/20260820-1")),
                    s("51c459a")
                ),
                (
                    1,
                    s("recorded"),
                    Some(s("codereviews/20260821-2")),
                    s("51c459a")
                ),
                (2, s("recorded"), None, s("abc")),
                (3, s("waived"), None, s("def")),
            ],
            "rounds in name (date) order, a bare head kept, the waiver kept"
        );
        let left: Vec<String> = conn
            .prepare("SELECT facet FROM tag_applications ORDER BY facet")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            left,
            vec!["repo".to_owned(), "review".to_owned()],
            "only the migrated review facets go; the backlog trail stays"
        );
        let trail: i64 = conn
            .query_row(
                "SELECT count(*) FROM tag_applications WHERE item_id = 4 AND facet = 'review'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(trail, 1);

        let rounds: Vec<(String, i64)> = conn
            .prepare(
                "SELECT r.ns, coalesce(sum(f.must_fix), 0) FROM review_rounds r
                 LEFT JOIN review_round_findings f ON f.round_id = r.id
                 GROUP BY r.id ORDER BY r.id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rounds,
            vec![
                (s("codereviews/20260820-1"), 1),
                (s("codereviews/20260821-2"), 0)
            ],
            "every migrated round snapshotted, in filing order, with its must-fix count"
        );
    }
}

#[cfg(test)]
mod design_exports_migration_tests {
    use super::embedded;
    use refinery::Target;
    use rusqlite::{params, Connection};

    /// V026 moves a design's `doc_target` and `sources` metadata keys into rows of their own and
    /// strips them from the metadata, leaving other keys and other kinds alone. And undo
    /// history ends at the newest transaction that wrote those keys, so none of them can be undone
    /// into a blob nothing reads, while later work stays undoable.
    #[test]
    #[allow(clippy::too_many_lines)] // one migration scenario, seeded and read back top to bottom
    fn v026_moves_doc_targets_and_sources_out_of_design_metadata() {
        let mut conn = Connection::open_in_memory().unwrap();
        embedded::migrations::runner()
            .set_target(Target::Version(25))
            .run(&mut conn)
            .unwrap();
        let h = "a".repeat(64);
        for (id, uid, kind, metadata, ns) in [
            (
                1,
                "design:a",
                "design",
                format!(
                    r#"{{"doc_target":"docs/a.md","sources":[{{"path":"README.md","blake3":"{h}"}}],"keep":1}}"#
                ),
                Some("designs/jkb"),
            ),
            (
                2,
                "design:b",
                "design",
                r#"{"doc_target":"docs/a.md"}"#.to_owned(),
                Some("designs/jkb"),
            ),
            (
                3,
                "design:c",
                "design",
                r#"{"doc_target":"docs/a.md"}"#.to_owned(),
                Some("designs/web/sub"),
            ),
            (
                4,
                "note:x",
                "note",
                r#"{"doc_target":"docs/x.md"}"#.to_owned(),
                None,
            ),
        ] {
            conn.execute(
                "INSERT INTO items (id, uid, kind, metadata) VALUES (?1, ?2, ?3, ?4)",
                params![id, uid, kind, metadata],
            )
            .unwrap();
            if let Some(ns) = ns {
                conn.execute(
                    "INSERT OR IGNORE INTO namespaces (path, kind) VALUES (?1, 'logical')",
                    [ns],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO placements (item_id, namespace_id, role)
                     SELECT ?1, id, 'primary' FROM namespaces WHERE path = ?2",
                    params![id, ns],
                )
                .unwrap();
            }
        }
        // The 872aec5 build logged its target write as a design metadata update (txn 7); txn 9 is
        // later, unrelated work; txn 8 writes another item's metadata with the same key.
        let meta_entry = |before: &str, after: &str| serde_json::json!({ "b": { "metadata": before }, "a": { "metadata": after } });
        let target = meta_entry("{}", r#"{"doc_target":"docs/a.md"}"#);
        let note = meta_entry("{}", r#"{"doc_target":"docs/x.md"}"#);
        for (txn, entity, before, after) in [
            (7, "1", target["b"].to_string(), target["a"].to_string()),
            (8, "4", note["b"].to_string(), note["a"].to_string()),
            (
                9,
                "1",
                r#"{"content":"x"}"#.to_owned(),
                r#"{"content":"y"}"#.to_owned(),
            ),
        ] {
            conn.execute(
                "INSERT INTO changelog (txn_id, op, entity_type, entity_id, before, after)
                 VALUES (?1, 'update', 'items', ?2, ?3, ?4)",
                params![txn, entity, before, after],
            )
            .unwrap();
        }
        embedded::migrations::runner().run(&mut conn).unwrap();

        let targets: Vec<(i64, String)> = conn
            .prepare("SELECT design_id, path FROM design_doc_targets ORDER BY design_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        // Every design's target moves; two of one repo on one file is `exports`' refusal to make.
        assert_eq!(
            targets,
            vec![
                (1, "docs/a.md".to_owned()),
                (2, "docs/a.md".to_owned()),
                (3, "docs/a.md".to_owned()),
            ]
        );
        let sources: Vec<(i64, String, String)> = conn
            .prepare("SELECT design_id, path, blake3 FROM design_sources")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(sources, vec![(1, "README.md".to_owned(), h)]);
        let metadata = |id: i64| -> String {
            conn.query_row("SELECT metadata FROM items WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(metadata(1), r#"{"keep":1}"#);
        assert_eq!(metadata(2), "{}");
        assert_eq!(metadata(4), r#"{"doc_target":"docs/x.md"}"#, "not a design");
        let watermark: i64 = conn
            .query_row("SELECT from_txn FROM undo_watermark", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            watermark, 7,
            "history ends at the design's key write, not the note's or later work"
        );
    }

    /// A database that never wrote those keys keeps its watermark.
    #[test]
    fn v026_leaves_the_watermark_of_a_database_without_the_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        embedded::migrations::runner()
            .set_target(Target::Version(25))
            .run(&mut conn)
            .unwrap();
        conn.execute(
            "INSERT INTO changelog (txn_id, op, entity_type, entity_id, before, after)
             VALUES (4, 'update', 'items', '1', '{\"content\":\"x\"}', 'not json')",
            [],
        )
        .unwrap();
        embedded::migrations::runner().run(&mut conn).unwrap();
        let watermark: i64 = conn
            .query_row("SELECT from_txn FROM undo_watermark", [], |r| r.get(0))
            .unwrap();
        assert_eq!(watermark, 0);
    }
}
