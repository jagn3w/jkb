//! Claim-owner identity and the deterministic liveness probe (design D27.1/D27.2).
//!
//! A claim's `claimant_id` is a **liveness-checkable owner id** of the form
//! `host:pid` (the coordinator may append a run segment, `host:pid:run`; subagents
//! share their coordinator's pid, so the pid is the liveness signal). [`self_owner`]
//! mints this process's id; [`is_alive`] probes an owner by `kill -0`-ing its pid —
//! the process either exists (alive, keep the claim) or does not (reclaimable). There
//! is deliberately **no** time component: a paused-but-alive owner passes the probe.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The host segment marking a **session** owner (design D36.6): `session:<pid>:<worktree>`.
/// The pid stays in field 1 so every existing probe reads it unchanged; the worktree path
/// is what makes the claim outlive the one-second `jkb task work` process that took it.
const SESSION: &str = "session";

/// This process's owner id, `host:pid`, used as the default claim owner.
#[must_use]
pub fn self_owner() -> String {
    format!("{}:{}", hostname(), std::process::id())
}

/// The owner id for a session working in `worktree`: `session:<this pid>:<worktree>`.
///
/// The pid records who is *attending* the session right now; the worktree records that the
/// session exists at all. See [`is_alive`] for why the two are not the same question.
#[must_use]
pub fn session_owner(worktree: &Path) -> String {
    format!(
        "{SESSION}:{}:{}",
        std::process::id(),
        worktree.to_string_lossy()
    )
}

/// The worktree a session owner id points at, or [`None`] for any other owner shape.
///
/// Fields 2.. are rejoined with `:` so a path containing a colon survives the round trip.
#[must_use]
pub fn session_worktree(owner: &str) -> Option<PathBuf> {
    let mut parts = owner.split(':');
    if parts.next() != Some(SESSION) {
        return None;
    }
    parts.next()?; // the pid, read by `owner_pid`
    let rest: Vec<&str> = parts.collect();
    if rest.is_empty() {
        return None;
    }
    Some(PathBuf::from(rest.join(":")))
}

/// Whether `owner`'s process is running right now — for a session, whether anyone is sitting
/// in it. A session with no live pid is *unattended*, not finished: its branch is still
/// there and the task is still its owner's (design D36.6).
#[must_use]
pub fn is_attended(owner: &str) -> bool {
    owner_pid(owner).is_some_and(pid_exists)
}

/// Best-effort local hostname (informational; single-host — the pid is what liveness
/// keys on). Falls back to `localhost` when the environment does not expose one. Any
/// `:` in the raw value is replaced with `-` so the host segment can never absorb the
/// `:pid` field (a container/k8s host like `node:1` would otherwise make [`owner_pid`]
/// parse the wrong field).
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("HOST").ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
        .replace(':', "-")
}

/// The pid embedded in an owner id (`host:pid` / `host:pid:run`): the second
/// `:`-delimited field. Returns [`None`] if the id is not in that shape. [`self_owner`]
/// sanitizes the host segment of any `:`, so the second field is always the pid.
#[must_use]
pub fn owner_pid(owner: &str) -> Option<u32> {
    owner.split(':').nth(1).and_then(|p| p.parse::<u32>().ok())
}

/// Whether `owner` still exists — the deterministic liveness check.
///
/// Parses the pid out of the `host:pid` id and probes it with `ps -p <pid>`, which
/// exits 0 iff a process with that pid exists — **regardless of which OS user owns it**.
/// (`kill -0` was rejected here: it exits non-zero on `EPERM` for a foreign-owned but
/// live process, which would wrongly reclaim a still-running agent's claim.) An owner id
/// we cannot parse a pid from is treated as **not alive** (reclaimable) so a malformed
/// claim never wedges a task forever.
///
/// A **session** owner (`session:<pid>:<worktree>`, design D36.6) is additionally alive
/// while its worktree exists. `jkb task work` exits in under a second, so its pid would be
/// gone immediately; the thing that persists — and that means "this work is in flight" — is
/// the checkout. Freeing the claim when the terminal closes is the wrong direction of error:
/// the half-written branch is still there, and a swarm run or a second click would start the
/// same task again on a second branch.
#[must_use]
pub fn is_alive(owner: &str) -> bool {
    if owner_pid(owner).is_some_and(pid_exists) {
        return true;
    }
    session_worktree(owner).is_some_and(|w| w.exists())
}

/// Whether a process with this pid exists. `ps -p <pid> -o pid=` prints the pid and exits 0
/// if it does, 1 otherwise; it does not require ownership of the process. Dependency-free
/// and single-host.
fn pid_exists(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .output()
        .is_ok_and(|out| out.status.success())
}

#[cfg(test)]
mod tests {
    use super::{is_alive, is_attended, owner_pid, self_owner, session_owner, session_worktree};

    #[test]
    fn self_owner_is_host_colon_pid() {
        let owner = self_owner();
        let pid = owner_pid(&owner);
        assert_eq!(pid, Some(std::process::id()));
    }

    #[test]
    fn a_live_process_is_alive() {
        // This very process is, definitionally, alive.
        assert!(is_alive(&self_owner()));
    }

    #[test]
    fn a_bogus_pid_is_not_alive() {
        // pid 2^31-ish will not exist; also an unparseable id is not alive.
        assert!(!is_alive("host:4294967290"));
        assert!(!is_alive("garbage"));
    }

    #[test]
    fn a_foreign_owned_live_process_is_alive() {
        // pid 1 (launchd/init) always exists and is owned by root. `kill -0` would exit
        // EPERM (non-zero) here when we are not root; `ps -p` reports it alive regardless.
        assert!(is_alive("host:1"));
    }

    /// The claim `jkb task work` takes must survive the process that took it — otherwise
    /// `doctor --fix` frees the task while the session is still open (design D36.6).
    #[test]
    fn a_session_outlives_the_process_that_claimed_it() {
        let tmp = tempfile::tempdir().unwrap();
        let owner = session_owner(tmp.path());
        assert_eq!(owner_pid(&owner), Some(std::process::id()));
        assert_eq!(session_worktree(&owner).as_deref(), Some(tmp.path()));

        // A dead pid but a live worktree: the session is unattended, NOT reclaimable.
        let orphan = format!("session:4294967290:{}", tmp.path().display());
        assert!(is_alive(&orphan), "a live worktree keeps the claim");
        assert!(!is_attended(&orphan), "nobody is sitting in it");

        // Remove the worktree and the claim becomes reclaimable — `land`/`abandon` are the
        // only commands that do this.
        let gone = format!("session:4294967290:{}", tmp.path().join("nope").display());
        assert!(!is_alive(&gone));

        // Non-session owners are unaffected: pid alone still decides.
        assert!(session_worktree("host:123").is_none());
        assert!(!is_alive("host:4294967290"));
    }

    #[test]
    fn owner_pid_reads_the_second_field() {
        // The host segment is sanitized of `:`, so field 1 is always the pid, even with a
        // trailing run segment.
        assert_eq!(owner_pid("node-1:12345"), Some(12345));
        assert_eq!(owner_pid("host:12:run"), Some(12));
        assert_eq!(owner_pid("host"), None);
    }
}
