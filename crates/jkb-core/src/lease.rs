//! Named, exclusive holds in the database (tasks S6.4, decisions B and D).
//!
//! The removal sweep's lock and `jkb task land`'s per-repo lock were files — `<store>/.sweep.lock` and
//! `<repo>/.jkb/land.lock` — each holding the owner that took it. A file a process on another kernel
//! can see is not a lock a process on another kernel can take safely, and a client of `jkb serve` has
//! no path to the store at all. A lease row is the same thing, taken through the writer.
//!
//! **Nothing here judges a holder.** Whether the process named in `holder` is gone is a question only
//! its own machine can answer, so the caller asks it, and a takeover is a compare-and-set on the exact
//! holder it judged: [`take`] with `displace` replaces a lease only while it still names that holder,
//! and [`release`] drops one only while it names the releaser. A holder is `<owner id> <nonce>`: the
//! owner id is what is judged and reported, and the nonce tells one acquisition from the next by the
//! same process — the reason the file lock grew one.

use rusqlite::{params, Connection, OptionalExtension};

use crate::mq::QueueError;
use crate::{Result, WriteMeta};

/// The longest lease name.
pub const MAX_NAME_BYTES: usize = 300;

/// The longest holder.
pub const MAX_HOLDER_BYTES: usize = 600;

/// A lease somebody holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// `<owner id> <nonce>`, as the taker wrote it.
    pub holder: String,
    /// When it was taken (Unix ms).
    pub taken_at: i64,
}

impl Lease {
    /// The owner id part of the holder — what is judged and what is shown.
    #[must_use]
    pub fn owner(&self) -> &str {
        owner_of(&self.holder)
    }
}

/// The owner id out of a `<owner id> <nonce> [more…]` holder — the first field, whatever follows (the
/// land lease appends the Claude Code session it was taken in).
#[must_use]
pub fn owner_of(holder: &str) -> &str {
    holder.split_whitespace().next().unwrap_or_default()
}

fn invalid(what: &'static str, why: String) -> crate::Error {
    QueueError::Invalid { what, why }.into()
}

fn check(name: &str, holder: Option<&str>) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(invalid(
            "lease",
            format!("a lease name is 1..={MAX_NAME_BYTES} bytes with no control characters"),
        ));
    }
    if let Some(h) = holder {
        if owner_of(h).is_empty() || h.len() > MAX_HOLDER_BYTES || h.chars().any(char::is_control) {
            return Err(invalid(
                "holder",
                format!(
                    "a holder is `<owner> <nonce>`, 1..={MAX_HOLDER_BYTES} bytes with no control \
                     characters"
                ),
            ));
        }
    }
    Ok(())
}

/// Who holds `name`, if anyone.
///
/// # Errors
/// A malformed name, or a database error.
pub fn get(conn: &Connection, name: &str) -> Result<Option<Lease>> {
    check(name, None)?;
    Ok(conn
        .prepare_cached("SELECT holder, taken_at FROM leases WHERE name = ?1")?
        .query_row([name], |r| {
            Ok(Lease {
                holder: r.get(0)?,
                taken_at: r.get(1)?,
            })
        })
        .optional()?)
}

/// Take `name` for `holder`: when nobody holds it, or — with `displace` — when it is still held by
/// exactly `displace`, the holder the caller judged gone. Returns whether `holder` now holds it; `false`
/// changes nothing.
///
/// # Errors
/// A malformed name or holder, or a database error.
pub fn take(
    conn: &Connection,
    _meta: &WriteMeta,
    name: &str,
    holder: &str,
    displace: Option<&str>,
    now: i64,
) -> Result<bool> {
    check(name, Some(holder))?;
    if let Some(d) = displace {
        check(name, Some(d))?;
        let replaced = conn
            .prepare_cached(
                "UPDATE leases SET holder = ?1, taken_at = ?2 WHERE name = ?3 AND holder = ?4",
            )?
            .execute(params![holder, now, name, d])?;
        return Ok(replaced > 0);
    }
    let inserted = conn
        .prepare_cached(
            "INSERT INTO leases (name, holder, taken_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(name) DO NOTHING",
        )?
        .execute(params![name, holder, now])?;
    Ok(inserted > 0)
}

/// Release `name` if `holder` still holds it. Returns whether it did.
///
/// # Errors
/// A malformed name or holder, or a database error.
pub fn release(conn: &Connection, _meta: &WriteMeta, name: &str, holder: &str) -> Result<bool> {
    check(name, Some(holder))?;
    Ok(conn
        .prepare_cached("DELETE FROM leases WHERE name = ?1 AND holder = ?2")?
        .execute(params![name, holder])?
        > 0)
}

/// Remove `name` whoever holds it — the operator's escape from a holder nothing can prove gone.
/// Returns the holder it displaced.
///
/// # Errors
/// A malformed name, or a database error.
pub fn break_lease(conn: &Connection, _meta: &WriteMeta, name: &str) -> Result<Option<String>> {
    check(name, None)?;
    Ok(conn
        .prepare_cached("DELETE FROM leases WHERE name = ?1 RETURNING holder")?
        .query_row([name], |r| r.get(0))
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::{break_lease, get, owner_of, release, take};
    use crate::mq::QueueError;
    use crate::{Db, Error};

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn take_(db: &Db, holder: &str, displace: Option<&str>) -> bool {
        let (h, d) = (holder.to_owned(), displace.map(str::to_owned));
        db.write_txn("t", move |c, m| take(c, m, "l", &h, d.as_deref(), 5))
            .unwrap()
    }

    /// One holder at a time; a takeover must name the holder it judged, and a release only drops the
    /// releaser's own lease.
    #[test]
    fn a_lease_is_exclusive_and_every_change_is_a_compare_and_set() {
        let db = db();
        assert!(take_(&db, "host:1 n1", None));
        assert!(!take_(&db, "host:2 n2", None), "held");
        assert!(
            !take_(&db, "host:2 n2", Some("host:1 other")),
            "a takeover of a holder that is not there changes nothing"
        );
        let l = db.read(|c| get(c, "l")).unwrap().unwrap();
        assert_eq!(
            (l.holder.as_str(), l.owner(), l.taken_at),
            ("host:1 n1", "host:1", 5)
        );

        // Same process, next acquisition: the nonce keeps the two apart.
        assert!(!db
            .write_txn("t", |c, m| release(c, m, "l", "host:1 n0"))
            .unwrap());
        assert!(take_(&db, "host:2 n2", Some("host:1 n1")), "judged gone");
        assert!(
            !db.write_txn("t", |c, m| release(c, m, "l", "host:1 n1"))
                .unwrap(),
            "the displaced holder cannot release its successor"
        );
        assert!(db
            .write_txn("t", |c, m| release(c, m, "l", "host:2 n2"))
            .unwrap());
        assert_eq!(db.read(|c| get(c, "l")).unwrap(), None);

        assert!(take_(&db, "host:3 n3", None));
        assert_eq!(
            db.write_txn("t", |c, m| break_lease(c, m, "l"))
                .unwrap()
                .as_deref(),
            Some("host:3 n3")
        );
        assert_eq!(
            db.write_txn("t", |c, m| break_lease(c, m, "l")).unwrap(),
            None
        );
    }

    #[test]
    fn malformed_names_and_holders_are_refused() {
        let db = db();
        for (name, holder) in [
            ("", "host:1 n"),
            ("l\n", "host:1 n"),
            ("l", ""),
            ("l", " "),
            ("l", "host:1\nn"),
        ] {
            let (n, h) = (name.to_owned(), holder.to_owned());
            let err = db
                .write_txn("t", move |c, m| take(c, m, &n, &h, None, 0))
                .unwrap_err();
            assert!(
                matches!(err, Error::Queue(QueueError::Invalid { .. })),
                "{name:?} {holder:?}: {err}"
            );
        }
        assert_eq!(owner_of("host:1 nonce"), "host:1");
        assert_eq!(owner_of(""), "");
    }
}
