//! Crash recovery for task claims through the ops (tasks S6.4 stage 5, design-s6-4.md H):
//! `task.claims` lists what is held, and `task.reclaim` frees the claims whose owner the client proved
//! gone.
//!
//! **The client probes; the op only compares.** Whether a process or a checkout still exists can be
//! asked only where it lives (design H4), so `jkb task reclaim` and `jkb doctor` probe each owner where
//! they run and send the owners they proved gone. The op frees, in its own transaction, the claims still
//! held by exactly one of those strings. That string is the compare-and-set: a new process, or a session
//! resumed in the same checkout, claims under a different one, so a probe made before the transaction
//! cannot free a claim taken after it.

use std::collections::BTreeSet;

use jkb_core::{transition, WriteMeta};
use jkb_fsm::Fact;
use jkb_types::{AgentId, Liveness};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::tasks::{check_line, check_owner, line_problem, writable, FileRoots};
use crate::{ApiError, ErrorCode};

/// One held claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// The claimed task's uid.
    pub uid: String,
    /// Who holds it.
    pub owner: String,
}

impl From<jkb_core::claim::ClaimInfo> for Claim {
    fn from(c: jkb_core::claim::ClaimInfo) -> Self {
        Self {
            uid: c.uid,
            owner: c.owner,
        }
    }
}

/// The most claims one `task.claims` page lists.
pub const CLAIMS_PAGE: usize = 1000;

/// The most owners one `task.reclaim` names.
pub const MAX_DEAD_OWNERS: usize = 1000;

/// `task.claims`: the held claims after cursor `after`, in task order, at most [`CLAIMS_PAGE`], and the
/// cursor of the next page when there is one — sent back as it came.
///
/// # Errors
/// A failed read.
pub fn claims(conn: &Connection, after: Option<i64>) -> Result<(Vec<Claim>, Option<i64>), ApiError> {
    let mut held: Vec<jkb_core::claim::ClaimInfo> = jkb_core::claim::claimed(conn)?
        .into_iter()
        .filter(|c| after.is_none_or(|a| c.id.get() > a))
        .collect();
    let next = (held.len() > CLAIMS_PAGE).then(|| held[CLAIMS_PAGE - 1].id.get());
    held.truncate(CLAIMS_PAGE);
    Ok((held.into_iter().map(Claim::from).collect(), next))
}

/// What `task.reclaim` did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reclaimed {
    /// The claims it freed.
    pub cleared: Vec<Claim>,
    /// Owners it would not take as gone on this client's word, with why.
    pub refused: Vec<Refusal>,
    /// Claims held by one of the owners, left alone because this client may not write the task.
    #[serde(default)]
    pub unwritable: Vec<Claim>,
}

/// An owner `task.reclaim` would not free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// The owner.
    pub owner: String,
    /// Why.
    pub reason: String,
}

/// Where a `task.reclaim` comes from — what decides which owners it may say are gone.
#[derive(Debug, Clone, Copy)]
pub enum Asker<'a> {
    /// A process on this host, in-process: it probed where the owners live.
    Local,
    /// A client of `jkb serve` on the host named `server_host`, which may write tasks only under
    /// `roots`.
    Remote {
        /// The daemon's host.
        server_host: &'a str,
        /// Where its task writes may reach.
        roots: &'a FileRoots,
    },
}

/// Why `asker` cannot have proved `owner` gone, if it cannot.
///
/// Nobody can prove an `agent:` owner or an unreadable one gone ([`Liveness::External`]), so neither is
/// ever taken, from anyone. A client of the daemon cannot have probed a process of the daemon's own host
/// either: its pid namespace is its own. A session is judged by its checkout, which a client sees only
/// in the directory it shares with the host — a `~/`-relative worktree its file roots admit
/// ([`FileRoots::admits_home_path`], which also refuses a `..`), the one form both sides resolve.
fn unprovable(asker: Asker<'_>, owner: &str) -> Option<String> {
    match AgentId::parse(owner).liveness() {
        Liveness::External => Some(
            "nothing can prove this owner gone; `jkb task release <uid> --owner <owner>` once you know"
                .to_owned(),
        ),
        Liveness::Process { host, .. } => match asker {
            Asker::Remote { server_host, .. } if host == server_host => Some(format!(
                "a process of {host}, where jkb serve runs; only a reclaim run there can probe it"
            )),
            _ => None,
        },
        Liveness::Worktree(dir) => match asker {
            Asker::Remote { roots, .. }
                if !dir.to_str().is_some_and(|d| roots.admits_home_path(d)) =>
            {
                Some(
                    "a session checkout outside the directory this client shares with the host, \
                     which it cannot see"
                        .to_owned(),
                )
            }
            _ => None,
        },
    }
}

/// `task.reclaim`: free every claim held by one of `dead`, which the client proved gone, unless the
/// asker cannot have proved it ([`unprovable`]) or may not write the task. Through the lifecycle's
/// `ObservedOwnerGone`, in one transaction.
///
/// This gives a client no power it lacked: `task.release` already frees any claim whose owner string
/// the client names. What it adds is the lifecycle move and the history entry a crash recovery records.
///
/// # Errors
/// [`ErrorCode::Invalid`] for too many or malformed owners, or a failed write.
pub fn reclaim(
    conn: &Connection,
    meta: &WriteMeta,
    dead: &[String],
    asker: Asker<'_>,
) -> Result<Reclaimed, ApiError> {
    if dead.len() > MAX_DEAD_OWNERS {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            format!("at most {MAX_DEAD_OWNERS} owners in one reclaim"),
        ));
    }
    let mut out = Reclaimed::default();
    let mut gone = BTreeSet::new();
    for owner in dead {
        check_owner(owner)?;
        match unprovable(asker, owner) {
            Some(reason) => out.refused.push(Refusal {
                owner: owner.clone(),
                reason,
            }),
            None => {
                gone.insert(owner.as_str());
            }
        }
    }
    let roots = match asker {
        Asker::Local => None,
        Asker::Remote { roots, .. } => Some(roots),
    };
    let mut failure = None;
    let mut unwritable = Vec::new();
    // Each freed task's tasks.md line is held to its round trip, as every task write is.
    let mut lines = std::collections::BTreeMap::new();
    let found = transition::reclaim_judged(conn, meta, |c| {
        if !gone.contains(c.owner.as_str()) {
            return Fact::Yes;
        }
        match writable(conn, &c.uid, roots).and_then(|_| line_problem(conn, &c.uid)) {
            Ok(before) => {
                lines.insert(c.uid.clone(), before);
                Fact::No
            }
            Err(e) if e.code == ErrorCode::Forbidden => {
                unwritable.push(Claim::from(c.clone()));
                Fact::Yes
            }
            Err(e) => {
                failure.get_or_insert(e);
                Fact::Yes
            }
        }
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    for c in &found.cleared {
        check_line(conn, &c.uid, lines.get(&c.uid).and_then(Option::as_deref))?;
    }
    out.cleared = found.cleared.into_iter().map(Claim::from).collect();
    out.unwritable = unwritable;
    Ok(out)
}

#[cfg(test)]
mod tests;
