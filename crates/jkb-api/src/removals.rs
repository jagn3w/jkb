//! The worktree-removal records and the leases, as ops (tasks S6.4 stage 3, decisions B and D).
//!
//! The records are the session verbs' promises to the reap service ([`jkb_core::removal`]); the leases
//! are the removal sweep's lock and, from stage 4, `jkb task land`'s per-repo lock
//! ([`jkb_core::lease`]). Both were files beside the database or in the repo, which a client of
//! `jkb serve` has no path to — and which a process on another kernel cannot lock safely anyway.
//!
//! **What a client may name.** A record is acted on by the host's reap service, which renames and later
//! deletes the directories it names. So under `jkb serve`'s [`FileRoots`] every path a client writes
//! must be `~/`-relative and resolve, against this host's home, to somewhere under those roots — the
//! directories the dev container sees ([`FileRoots::admits_home_path`]). That is a check of spelling,
//! and the container can plant links in those directories, so the reader confines as well: a record
//! written through `jkb serve` is acted on only while its repo root resolves under `~/repos`, and
//! through no link (`jkb-cli`'s `archive::beneath`, `jkb_core::nofollow`). A record a client did not
//! write is out of its reach: archiving or dropping one is refused unless the record lies under the
//! roots.
//!
//! **Departure from decision B as written**, which named a record by *(repo key, slug)*: there is no
//! registry from a repo key to its root on either side, and the owners of stage 2 already name a
//! worktree `~/repos/…` for the same reason (design-s6-4.md, "As built").

use jkb_core::{lease, removal, WriteMeta};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::tasks::FileRoots;
use crate::{ApiError, ErrorCode};

/// The removal sweep's lease: one sweep at a time, across the host and every client.
pub const SWEEP_LEASE: &str = "removal-sweep";

/// The `written_via` of a record imported from the old file store (`removal.add` with `legacy`).
pub const LEGACY_WRITER: &str = "legacy";

/// The prefix of a repo's land lease (`land:<repo key>`).
pub const LAND_LEASE_PREFIX: &str = "land:";

/// A record as a request carries it and a listing answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Removal {
    /// The session worktree: absolute, or `~/`-relative.
    pub worktree: String,
    /// Its repository's root.
    pub repo_root: String,
    /// Its branch; empty if unknown.
    #[serde(default)]
    pub branch: String,
    /// The task it was for.
    #[serde(default)]
    pub uid: String,
    /// Delete the branch once the tree is out of the way.
    #[serde(default)]
    pub delete_branch: bool,
    /// The operator accepted whatever is uncommitted in it.
    #[serde(default)]
    pub accept_dirty: bool,
    /// When it was recorded (Unix seconds).
    pub recorded_at: i64,
    /// The commit the worktree was on.
    #[serde(default)]
    pub head: Option<String>,
    /// Where the tree was moved to, once it has been.
    #[serde(default)]
    pub archive: Option<String>,
    /// When it was moved (Unix seconds).
    #[serde(default)]
    pub archived_at: Option<i64>,
}

impl From<Removal> for removal::Removal {
    fn from(r: Removal) -> Self {
        Self {
            worktree: r.worktree,
            repo_root: r.repo_root,
            branch: r.branch,
            uid: r.uid,
            delete_branch: r.delete_branch,
            accept_dirty: r.accept_dirty,
            recorded_at: r.recorded_at,
            head: r.head,
            archive: r.archive,
            archived_at: r.archived_at,
        }
    }
}

impl From<removal::Removal> for Removal {
    fn from(r: removal::Removal) -> Self {
        Self {
            worktree: r.worktree,
            repo_root: r.repo_root,
            branch: r.branch,
            uid: r.uid,
            delete_branch: r.delete_branch,
            accept_dirty: r.accept_dirty,
            recorded_at: r.recorded_at,
            head: r.head,
            archive: r.archive,
            archived_at: r.archived_at,
        }
    }
}

/// A stored record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalRecord {
    /// Its identity, and its place in creation order.
    pub id: i64,
    /// What it says.
    #[serde(flatten)]
    pub removal: Removal,
    /// The backend that wrote it (`serve` for a client of the daemon).
    pub written_via: String,
}

fn forbidden(why: String) -> ApiError {
    ApiError::with_code(ErrorCode::Forbidden, why)
}

fn check_paths<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    roots: Option<&FileRoots>,
) -> Result<(), ApiError> {
    let Some(roots) = roots else { return Ok(()) };
    for p in paths {
        if !roots.admits_home_path(p) {
            return Err(forbidden(format!(
                "{p} is not a `~/` path under the directories this client may name — the host's reap \
                 service acts on the paths a removal record names"
            )));
        }
    }
    Ok(())
}

fn record_paths(r: &removal::Removal) -> impl Iterator<Item = &str> {
    [r.worktree.as_str(), r.repo_root.as_str()]
        .into_iter()
        .chain(r.archive.as_deref())
}

/// `removal.add`: record a disposal, as a new record.
///
/// # Errors
/// [`ErrorCode::Forbidden`] for a path outside `roots`, [`ErrorCode::Invalid`] for a malformed field,
/// or a failed write.
pub fn add(
    conn: &Connection,
    meta: &WriteMeta,
    ask: Removal,
    via: &str,
    roots: Option<&FileRoots>,
) -> Result<i64, ApiError> {
    let r: removal::Removal = ask.into();
    check_paths(record_paths(&r), roots)?;
    Ok(removal::add(conn, meta, &r, via)?)
}

/// `removal.list`: the records after `after`, oldest first, a page at a time; and where the next page
/// starts.
///
/// # Errors
/// A failed read.
pub fn list(
    conn: &Connection,
    after: Option<i64>,
) -> Result<(Vec<RemovalRecord>, Option<i64>), ApiError> {
    let (rows, next) = removal::list(conn, after)?;
    Ok((
        rows.into_iter()
            .map(|r| RemovalRecord {
                id: r.id,
                removal: r.removal.into(),
                written_via: r.written_via,
            })
            .collect(),
        next,
    ))
}

/// Whether record `id` exists — and, under `roots`, lies under them.
fn reachable(conn: &Connection, id: i64, roots: Option<&FileRoots>) -> Result<bool, ApiError> {
    let Some(row) = removal::get(conn, id)? else {
        return Ok(false);
    };
    check_paths(record_paths(&row.removal), roots)?;
    Ok(true)
}

/// `removal.archived`: the pending record `id` was archived to `archive` at `at`. `false` for a record
/// already archived or gone.
///
/// # Errors
/// [`ErrorCode::Forbidden`] for a record or an archive outside `roots`, or a failed write.
pub fn archived(
    conn: &Connection,
    meta: &WriteMeta,
    id: i64,
    archive: &str,
    at: i64,
    roots: Option<&FileRoots>,
) -> Result<bool, ApiError> {
    check_paths([archive], roots)?;
    if !reachable(conn, id, roots)? {
        return Ok(false);
    }
    Ok(removal::archived(conn, meta, id, archive, at)?)
}

/// `removal.drop`: forget the record `id`. `false` when it was not there.
///
/// # Errors
/// [`ErrorCode::Forbidden`] for a record outside `roots`, or a failed write.
pub fn drop_record(
    conn: &Connection,
    meta: &WriteMeta,
    id: i64,
    roots: Option<&FileRoots>,
) -> Result<bool, ApiError> {
    if !reachable(conn, id, roots)? {
        return Ok(false);
    }
    Ok(removal::remove(conn, meta, id)?)
}

/// The most records one `removal.cancel` names.
pub const MAX_CANCEL: usize = 256;

/// What `removal.cancel` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cancelled {
    /// How many pending records it dropped.
    pub cancelled: usize,
    /// The sweep holding its lease, when the cancel was refused for it — nothing was dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep_holder: Option<String>,
    /// Records named that this client may not name, left as they are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<i64>,
}

/// `removal.cancel`: drop the pending records `ids` — `jkb task work` handing a checkout back — unless a
/// sweep is in flight, in one transaction.
///
/// **Without taking the sweep's lease.** A sweep reads the records and then acts on them, so a
/// cancellation while one runs is refused: the sweep may archive the checkout being handed back. But
/// the check and the delete need no lease of their own — they are one write — and taking one would
/// let a `task work` killed in between leave the lease to a holder the host cannot probe, stopping
/// the host's reap service until an operator broke it. A record already archived, or gone, is left.
///
/// # Errors
/// [`ErrorCode::Invalid`] for too many ids, or a failed write. A record outside `roots` is skipped.
pub fn cancel(
    conn: &Connection,
    meta: &WriteMeta,
    ids: &[i64],
    roots: Option<&FileRoots>,
) -> Result<Cancelled, ApiError> {
    if ids.len() > MAX_CANCEL {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            format!("at most {MAX_CANCEL} records per cancel"),
        ));
    }
    if let Some(held) = lease::get(conn, SWEEP_LEASE)? {
        return Ok(Cancelled {
            cancelled: 0,
            sweep_holder: Some(held.owner().to_owned()),
            skipped: Vec::new(),
        });
    }
    // A record this client may not name is skipped and reported, not a refusal of the whole cancel:
    // the caller decides what a skipped record naming its checkout means.
    let mut cancelled = 0;
    let mut skipped = Vec::new();
    for &id in ids {
        let mayname = match reachable(conn, id, roots) {
            Ok(found) => found,
            Err(e) if e.code == ErrorCode::Forbidden => {
                skipped.push(id);
                false
            }
            Err(e) => return Err(e),
        };
        if mayname && removal::cancel_pending(conn, meta, id)? {
            cancelled += 1;
        }
    }
    Ok(Cancelled {
        cancelled,
        sweep_holder: None,
        skipped,
    })
}

/// A lease as `lease.get` answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseHeld {
    /// `<owner id> <nonce>`, as its taker wrote it.
    pub holder: String,
    /// When it was taken (Unix ms).
    pub taken_at: i64,
}

/// Only the leases jkb takes: the sweep's, and a repo's land lease. A client naming anything else would
/// be using the table as storage.
fn check_lease_name(name: &str) -> Result<(), ApiError> {
    let known = name == SWEEP_LEASE
        || name
            .strip_prefix(LAND_LEASE_PREFIX)
            .is_some_and(|repo| !repo.is_empty());
    if known {
        Ok(())
    } else {
        Err(ApiError::with_code(
            ErrorCode::Invalid,
            format!(
                "{name:?} is not a lease jkb takes (`{SWEEP_LEASE}`, `{LAND_LEASE_PREFIX}<repo>`)"
            ),
        ))
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// `lease.get`.
///
/// # Errors
/// A malformed name, or a failed read.
pub fn lease_get(conn: &Connection, name: &str) -> Result<Option<LeaseHeld>, ApiError> {
    check_lease_name(name)?;
    Ok(lease::get(conn, name)?.map(|l| LeaseHeld {
        holder: l.holder,
        taken_at: l.taken_at,
    }))
}

/// `lease.take`: take `name` for `holder` when it is free, or — with `displace` — while it is still held
/// by exactly the holder the caller judged gone. `false` changes nothing.
///
/// # Errors
/// A malformed name or holder, or a failed write.
pub fn lease_take(
    conn: &Connection,
    meta: &WriteMeta,
    name: &str,
    holder: &str,
    displace: Option<&str>,
) -> Result<bool, ApiError> {
    check_lease_name(name)?;
    Ok(lease::take(conn, meta, name, holder, displace, now_ms())?)
}

/// `lease.release`: drop `name` if `holder` still holds it.
///
/// # Errors
/// A malformed name or holder, or a failed write.
pub fn lease_release(
    conn: &Connection,
    meta: &WriteMeta,
    name: &str,
    holder: &str,
) -> Result<bool, ApiError> {
    check_lease_name(name)?;
    Ok(lease::release(conn, meta, name, holder)?)
}

/// `lease.break`: drop `name` whoever holds it — the operator's escape, `jkb task reap --break-lock`.
/// **Host only**: under `roots` it is refused, as `jkb task reap` is. A client that judged a holder gone
/// takes over with `lease.take`'s `displace` instead.
///
/// # Errors
/// [`ErrorCode::Forbidden`] under `roots`, a malformed name, or a failed write.
pub fn lease_break(
    conn: &Connection,
    meta: &WriteMeta,
    name: &str,
    roots: Option<&FileRoots>,
) -> Result<Option<String>, ApiError> {
    if roots.is_some() {
        return Err(forbidden(
            "breaking a lease is the host operator's escape — run `jkb task reap --break-lock` on the \
             host"
                .to_owned(),
        ));
    }
    check_lease_name(name)?;
    Ok(lease::break_lease(conn, meta, name)?)
}

#[cfg(test)]
mod tests;
