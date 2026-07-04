//! Claim-owner identity and the deterministic liveness probe (design D27.1/D27.2).
//!
//! A claim's `claimant_id` is a **liveness-checkable owner id** of the form
//! `host:pid` (the coordinator may append a run segment, `host:pid:run`; subagents
//! share their coordinator's pid, so the pid is the liveness signal). [`self_owner`]
//! mints this process's id; [`is_alive`] probes an owner by `kill -0`-ing its pid —
//! the process either exists (alive, keep the claim) or does not (reclaimable). There
//! is deliberately **no** time component: a paused-but-alive owner passes the probe.

use std::process::Command;

/// This process's owner id, `host:pid`, used as the default claim owner.
#[must_use]
pub fn self_owner() -> String {
    format!("{}:{}", hostname(), std::process::id())
}

/// Best-effort local hostname (informational; single-host — the pid is what liveness
/// keys on). Falls back to `localhost` when the environment does not expose one.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("HOST").ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

/// The pid embedded in an owner id (`host:pid` / `host:pid:run`): the second
/// `:`-delimited field. Returns [`None`] if the id is not in that shape.
#[must_use]
pub fn owner_pid(owner: &str) -> Option<u32> {
    owner.split(':').nth(1).and_then(|p| p.parse::<u32>().ok())
}

/// Whether `owner`'s process still exists — the deterministic liveness check.
///
/// Parses the pid out of the `host:pid` id and probes it with `kill -0`, which
/// succeeds iff the process exists (even a permission error means it exists). An owner
/// id we cannot parse a pid from is treated as **not alive** (reclaimable) so a
/// malformed claim never wedges a task forever.
#[must_use]
pub fn is_alive(owner: &str) -> bool {
    let Some(pid) = owner_pid(owner) else {
        return false;
    };
    // `kill -0 <pid>` exits 0 if the process exists. Dependency-free and single-host.
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::{is_alive, owner_pid, self_owner};

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
}
