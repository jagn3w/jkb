//! Schema migrations.
//!
//! Migrations live as `V<n>__<name>.sql` files under `src/migrations/` and are
//! embedded into the binary at compile time by `refinery`. Refinery applies each
//! pending migration in a transaction and records it (with a checksum) in its
//! `refinery_schema_history` table — so a previously-applied migration that is
//! later edited is detected rather than silently ignored.

use rusqlite::Connection;

use crate::Result;

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
/// # Errors
/// Returns [`crate::Error::Migration`] if a migration fails to apply, or
/// [`crate::Error::Sqlite`] if the version marker cannot be written.
pub fn run(conn: &mut Connection) -> Result<()> {
    embedded::migrations::runner().run(conn)?;

    let version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM refinery_schema_history",
        [],
        |row| row.get(0),
    )?;
    conn.pragma_update(None, "user_version", version)?;
    Ok(())
}
