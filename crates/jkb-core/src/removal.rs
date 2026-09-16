//! The worktree-removal records, in the database (tasks S6.4, decision B; the records themselves are
//! design D49's, `jkb-cli`'s `archive` module).
//!
//! A row is what a record file was: a session worktree a verb could not archive where it ran, or one it
//! archived, which the reap service finishes and later deletes. This module stores and hands them back
//! and decides nothing about them — which paths a record may name, and what a sweep does with one, stay
//! with `archive`, which reads every row as untrusted input exactly as it read the files.
//!
//! Paths are strings as the writer sent them: absolute, or `~/`-relative when they lie under the
//! writer's home, so the host and the dev container resolve one row to the same checkout.

use rusqlite::{params, Connection, OptionalExtension};

use crate::mq::QueueError;
use crate::{Result, WriteMeta};

/// The longest path a record stores. Bounded so a page of [`PAGE`] rows fits one `jkb serve` answer
/// (1 MiB) with room to spare, whatever the rows hold.
pub const MAX_PATH_BYTES: usize = 1024;

/// The longest branch, uid, head or `written_via`.
pub const MAX_FIELD_BYTES: usize = 256;

/// The most rows one [`list`] call returns. A caller reads the store a page at a time, to the end: a
/// sweep, or `task work` cancelling its checkout's record, that stopped at a cap would miss a record.
pub const PAGE: usize = 64;

/// One record, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalRow {
    /// The row id — the record's identity, and creation order.
    pub id: i64,
    /// The removal as written.
    pub removal: Removal,
    /// The backend that wrote it (`serve` for a client of the daemon).
    pub written_via: String,
}

/// A record's fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Removal {
    /// The session worktree.
    pub worktree: String,
    /// Its repository's root.
    pub repo_root: String,
    /// Its branch; empty if unknown.
    pub branch: String,
    /// The task it was for.
    pub uid: String,
    /// Delete the branch once the tree is out of the way.
    pub delete_branch: bool,
    /// The operator accepted whatever is uncommitted in it.
    pub accept_dirty: bool,
    /// When it was recorded (Unix seconds).
    pub recorded_at: i64,
    /// The commit the worktree was on — the record's instance identity.
    pub head: Option<String>,
    /// Where the tree was moved to, once it has been.
    pub archive: Option<String>,
    /// When it was moved (Unix seconds).
    pub archived_at: Option<i64>,
}

fn invalid(what: &'static str, why: String) -> crate::Error {
    QueueError::Invalid { what, why }.into()
}

fn check_text(what: &'static str, value: &str, max: usize, empty_ok: bool) -> Result<()> {
    if (!empty_ok && value.is_empty()) || value.len() > max || value.chars().any(char::is_control) {
        return Err(invalid(
            what,
            format!("at most {max} bytes with no control characters"),
        ));
    }
    Ok(())
}

impl Removal {
    fn check(&self) -> Result<()> {
        check_text("worktree", &self.worktree, MAX_PATH_BYTES, false)?;
        check_text("repo_root", &self.repo_root, MAX_PATH_BYTES, false)?;
        check_text("branch", &self.branch, MAX_FIELD_BYTES, true)?;
        check_text("uid", &self.uid, MAX_FIELD_BYTES, true)?;
        if let Some(h) = &self.head {
            check_text("head", h, MAX_FIELD_BYTES, false)?;
        }
        if let Some(a) = &self.archive {
            check_text("archive", a, MAX_PATH_BYTES, false)?;
        }
        if self.archive.is_some() != self.archived_at.is_some() {
            return Err(invalid(
                "archive",
                "an archive and the time it was made go together".to_owned(),
            ));
        }
        Ok(())
    }
}

const COLUMNS: &str = "id, worktree, repo_root, branch, uid, delete_branch, accept_dirty, \
                       recorded_at, head, archive, archived_at, written_via";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RemovalRow> {
    Ok(RemovalRow {
        id: r.get(0)?,
        removal: Removal {
            worktree: r.get(1)?,
            repo_root: r.get(2)?,
            branch: r.get(3)?,
            uid: r.get(4)?,
            delete_branch: r.get(5)?,
            accept_dirty: r.get(6)?,
            recorded_at: r.get(7)?,
            head: r.get(8)?,
            archive: r.get(9)?,
            archived_at: r.get(10)?,
        },
        written_via: r.get(11)?,
    })
}

/// Record a disposal. Always a new row: one disposal, one record.
///
/// # Errors
/// A malformed field, or a database error.
pub fn add(conn: &Connection, _meta: &WriteMeta, removal: &Removal, via: &str) -> Result<i64> {
    removal.check()?;
    check_text("written_via", via, MAX_FIELD_BYTES, false)?;
    Ok(conn
        .prepare_cached(
            "INSERT INTO worktree_removals (worktree, repo_root, branch, uid, delete_branch, \
                 accept_dirty, recorded_at, head, archive, archived_at, written_via) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) RETURNING id",
        )?
        .query_row(
            params![
                removal.worktree,
                removal.repo_root,
                removal.branch,
                removal.uid,
                removal.delete_branch,
                removal.accept_dirty,
                removal.recorded_at,
                removal.head,
                removal.archive,
                removal.archived_at,
                via
            ],
            |r| r.get(0),
        )?)
}

/// One record.
///
/// # Errors
/// A database error.
pub fn get(conn: &Connection, id: i64) -> Result<Option<RemovalRow>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM worktree_removals WHERE id = ?1"
        ))?
        .query_row([id], row)
        .optional()?)
}

/// The records after row `after` (from the start with `None`), oldest first, at most [`PAGE`]; and the
/// `after` of the next page, `None` when this one is the last.
///
/// # Errors
/// A database error.
pub fn list(conn: &Connection, after: Option<i64>) -> Result<(Vec<RemovalRow>, Option<i64>)> {
    let fetch = i64::try_from(PAGE + 1).unwrap_or(i64::MAX);
    let mut rows: Vec<RemovalRow> = conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM worktree_removals WHERE id > ?1 ORDER BY id LIMIT ?2"
        ))?
        .query_map(params![after.unwrap_or(i64::MIN), fetch], row)?
        .collect::<rusqlite::Result<_>>()?;
    let next = if rows.len() > PAGE {
        rows.truncate(PAGE);
        rows.last().map(|r| r.id)
    } else {
        None
    };
    Ok((rows, next))
}

/// Every record, oldest first, read a page at a time — for a caller in the same process as the database.
///
/// # Errors
/// A database error.
pub fn list_all(conn: &Connection) -> Result<Vec<RemovalRow>> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let (rows, next) = list(conn, after)?;
        out.extend(rows);
        match next {
            Some(n) => after = Some(n),
            None => return Ok(out),
        }
    }
}

/// Record that the pending record `id` was archived to `archive` at `at`. Only a pending record moves:
/// returns `false`, changing nothing, for one already archived or gone.
///
/// # Errors
/// A malformed archive path, or a database error.
pub fn archived(
    conn: &Connection,
    _meta: &WriteMeta,
    id: i64,
    archive: &str,
    at: i64,
) -> Result<bool> {
    check_text("archive", archive, MAX_PATH_BYTES, false)?;
    Ok(conn
        .prepare_cached(
            "UPDATE worktree_removals SET archive = ?1, archived_at = ?2 \
             WHERE id = ?3 AND archive IS NULL",
        )?
        .execute(params![archive, at, id])?
        > 0)
}

/// Drop the record `id` if it is still pending — a cancelled disposal. Returns whether it did.
///
/// # Errors
/// A database error.
pub fn cancel_pending(conn: &Connection, _meta: &WriteMeta, id: i64) -> Result<bool> {
    Ok(conn
        .prepare_cached("DELETE FROM worktree_removals WHERE id = ?1 AND archive IS NULL")?
        .execute([id])?
        > 0)
}

/// Drop the record `id`. Returns whether it was there.
///
/// # Errors
/// A database error.
pub fn remove(conn: &Connection, _meta: &WriteMeta, id: i64) -> Result<bool> {
    Ok(conn
        .prepare_cached("DELETE FROM worktree_removals WHERE id = ?1")?
        .execute([id])?
        > 0)
}

#[cfg(test)]
mod tests {
    use super::{add, archived, get, list, list_all, remove, Removal, MAX_PATH_BYTES, PAGE};
    use crate::mq::QueueError;
    use crate::{Db, Error};

    fn pending(worktree: &str) -> Removal {
        Removal {
            worktree: worktree.to_owned(),
            repo_root: "~/repos/p".to_owned(),
            branch: "task/s".to_owned(),
            uid: "task:t".to_owned(),
            recorded_at: 10,
            head: Some("abc".to_owned()),
            ..Removal::default()
        }
    }

    /// A record is added, archived once, and dropped; every row reads back as written.
    #[test]
    fn a_record_is_added_archived_once_and_dropped() {
        let db = Db::open_in_memory().unwrap();
        let r = pending("~/repos/p/.jkb/work/s");
        let r2 = r.clone();
        let id = db
            .write_txn("t", move |c, m| add(c, m, &r2, "serve"))
            .unwrap();
        let got = db.read(move |c| get(c, id)).unwrap().unwrap();
        assert_eq!(
            (got.removal.clone(), got.written_via.as_str()),
            (r, "serve")
        );

        assert!(db
            .write_txn("t", move |c, m| archived(
                c,
                m,
                id,
                "~/repos/p/.jkb/archive/s-1",
                20
            ))
            .unwrap());
        assert!(
            !db.write_txn("t", move |c, m| archived(c, m, id, "~/x", 30))
                .unwrap(),
            "an archived record is not re-archived"
        );
        let got = db.read(move |c| get(c, id)).unwrap().unwrap();
        assert_eq!(
            (got.removal.archive.as_deref(), got.removal.archived_at),
            (Some("~/repos/p/.jkb/archive/s-1"), Some(20))
        );
        assert!(db.write_txn("t", move |c, m| remove(c, m, id)).unwrap());
        assert!(!db.write_txn("t", move |c, m| remove(c, m, id)).unwrap());
        assert!(!db
            .write_txn("t", move |c, m| archived(c, m, id, "~/y", 1))
            .unwrap());
    }

    /// One disposal, one row — two disposals of one worktree are two records, in creation order.
    #[test]
    fn every_disposal_is_its_own_record_listed_oldest_first() {
        let db = Db::open_in_memory().unwrap();
        for _ in 0..2 {
            db.write_txn("t", |c, m| add(c, m, &pending("~/w"), "cli"))
                .unwrap();
        }
        let (rows, next) = db.read(|c| list(c, None)).unwrap();
        assert_eq!(next, None);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].id < rows[1].id);
    }

    /// The listing is paged, every page is full but the last, and the pages together are every row once.
    #[test]
    fn the_listing_pages_through_every_record_once() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |c, m| {
            for _ in 0..=2 * PAGE {
                add(c, m, &pending("~/w"), "cli")?;
            }
            Ok(())
        })
        .unwrap();
        let (first, next) = db.read(|c| list(c, None)).unwrap();
        assert_eq!((first.len(), next), (PAGE, Some(first[PAGE - 1].id)));
        let (second, next) = db.read(move |c| list(c, next)).unwrap();
        assert_eq!(second.len(), PAGE);
        let (third, next) = db.read(move |c| list(c, next)).unwrap();
        assert_eq!((third.len(), next), (1, None));
        let all = db.read(list_all).unwrap();
        assert_eq!(all.len(), 2 * PAGE + 1);
        assert!(all.windows(2).all(|w| w[0].id < w[1].id));
    }

    #[test]
    fn malformed_records_are_refused() {
        let db = Db::open_in_memory().unwrap();
        for (bad, why) in [
            (pending(""), "no worktree"),
            (pending("~/w\n"), "a control character"),
            (pending(&"w".repeat(MAX_PATH_BYTES + 1)), "a path too long"),
            (
                Removal {
                    repo_root: String::new(),
                    ..pending("~/w")
                },
                "no repo root",
            ),
            (
                Removal {
                    archive: Some("~/a".to_owned()),
                    ..pending("~/w")
                },
                "an archive with no time",
            ),
            (
                Removal {
                    head: Some(String::new()),
                    ..pending("~/w")
                },
                "an empty head",
            ),
        ] {
            let err = db
                .write_txn("t", move |c, m| add(c, m, &bad, "cli"))
                .unwrap_err();
            assert!(
                matches!(err, Error::Queue(QueueError::Invalid { .. })),
                "{why}: {err}"
            );
        }
        assert!(db.read(list_all).unwrap().is_empty());
    }
}
