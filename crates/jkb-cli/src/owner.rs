//! Probing a claim owner's liveness (design D27.1/D27.2, S3.1/S3.2).
//!
//! The *identity* is [`jkb_types::AgentId`] — a parsed type with a closed set of shapes, each
//! declaring what would prove it via [`Liveness`]. This module is the other half: the probe, at
//! the edge, where forking `ps` and touching the filesystem belong.
//!
//! The probe answers a [`Fact`], not a `bool`. That is the load-bearing change: an owner whose
//! liveness cannot be established — an externally-minted `agent:` id, or a `claimant_id` in a
//! shape we cannot read — is **unestablished**, never *dead*. Reclaiming on an unestablished
//! answer frees a live agent's task silently; holding it reports a claim a person clears with
//! one command. Of the two ways to be wrong, the recoverable one wins (D34.4).
//!
//! There is still deliberately **no time component**: no TTL, no heartbeat, so an agent paused
//! on a permission prompt keeps its claim.

use std::path::{Path, PathBuf};

use jkb_fsm::Fact;
use jkb_types::{AgentId, Liveness};
use rustix::io::Errno;
use rustix::process::{self, Pid};

/// This process's owner id, `host:pid`, used as the default claim owner.
#[must_use]
pub fn self_owner() -> String {
    AgentId::this_process(&hostname(), std::process::id()).as_str()
}

/// The owner id for a session working in `worktree`: `session:<this pid>:<worktree>`.
///
/// The pid is **provenance** — which `jkb task work` process opened the session — and is
/// deliberately *not* a liveness signal: that process exits within a second, long before anyone
/// reads the claim. Liveness is the worktree; see [`is_alive`].
#[must_use]
pub fn session_owner(worktree: &Path) -> String {
    AgentId::session(std::process::id(), worktree).as_str()
}

/// An externally-minted agent owner, from `JKB_AGENT_ID` when the environment sets one.
///
/// For a caller whose process and checkout are not the thing that persists — a subagent, a
/// resumed session, a cloud run. jkb cannot probe such an owner, and says so
/// ([`Fact::Unknown`]) rather than guessing; the consequence is that its claim is never
/// auto-reclaimed, which is exactly the property that makes an opaque id usable here.
#[must_use]
pub fn env_agent() -> Option<String> {
    std::env::var("JKB_AGENT_ID")
        .ok()
        .filter(|id| !id.trim().is_empty())
        .map(|id| AgentId::agent(id.trim()).as_str())
}

/// The claim owner this process should use: its agent id when one is set, else `host:pid`.
#[must_use]
pub fn preferred_owner() -> String {
    env_agent().unwrap_or_else(self_owner)
}

/// The worktree a session owner id points at, or [`None`] for any other owner shape.
#[must_use]
pub fn session_worktree(owner: &str) -> Option<PathBuf> {
    AgentId::parse(owner).worktree().map(Path::to_path_buf)
}

/// Whether a process with this pid exists — the raw probe, for callers that hold a pid rather
/// than an owner id (the land lock's stale-holder check).
///
/// `Unknown` when `ps` could not be run at all. The land lock reads this to decide whether a
/// holder is stale, and treating "could not ask" as "gone" there breaks a live lock.
#[must_use]
pub fn pid_alive(pid: u32) -> Fact {
    pid_exists(pid)
}

/// Best-effort local hostname (informational; single-host — the pid is what liveness keys on).
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("HOST").ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

/// Whether `owner` still exists — proven, disproven, or unestablished.
///
/// The match is over [`Liveness`], a closed enum, so a new owner shape cannot be added without
/// the compiler demanding a probe for it. Per shape:
///
/// * a **process** is probed with `ps -p <pid>`, which exits 0 iff a process with that pid
///   exists **regardless of which OS user owns it**. (`kill -0` was rejected here: it exits
///   non-zero on `EPERM` for a foreign-owned but live process, which would wrongly reclaim a
///   still-running agent's claim.)
/// * a **session** is judged **only** by its worktree (design D36.6). `jkb task work` exits in
///   under a second, so its pid is gone before anyone reads the claim; the thing that persists
///   — and that means "this work is in flight" — is the checkout. The pid is ignored rather
///   than consulted as a fallback, so a *recycled* pid cannot keep a removed session's claim
///   alive after `land`/`abandon` took its worktree away.
/// * an **external** agent, and any id we cannot read, is [`Fact::Unknown`]: nothing here can
///   say. That is not "dead" — see the module docs.
#[must_use]
pub fn is_alive(owner: &str) -> Fact {
    match AgentId::parse(owner).liveness() {
        Liveness::Process(pid) => pid_exists(pid),
        // `Path::exists` is itself lossy — it answers `false` for a permission error as well as
        // for a missing directory — so it is asked through `try_exists`, which separates them.
        Liveness::Worktree(dir) => Fact::observed(dir.try_exists()),
        Liveness::External => Fact::Unknown,
    }
}

/// Whether a process with this pid exists, asked of the kernel rather than of a program:
/// `kill(pid, 0)` runs the existence and permission checks and sends no signal.
///
/// **`EPERM` means alive**, and that is the whole reason this is a syscall. The kernel refuses
/// because the process is *there* and belongs to someone else, so the error is positive evidence
/// of existence. The shell's `kill -0` throws that away — it collapses `EPERM` and `ESRCH` into
/// one non-zero exit, which would read a running agent's claim as dead and free it (D27.2) — so
/// `ps -p` was used instead, because it reports processes it does not own. But `ps` is
/// setuid-root on macOS, and a sandboxed process cannot exec a setuid binary at all: under the
/// D48 posture the probe could never run, and every `host:pid` owner became [`Fact::Unknown`]
/// (D48.10). Asking the kernel keeps what `ps` was chosen for and needs no subprocess, no `PATH`,
/// and no setuid binary.
///
/// A pid that cannot be represented is [`Fact::No`], not `Unknown`: no process can carry an id
/// outside `pid_t`, so its absence is established rather than merely unobserved.
fn pid_exists(pid: u32) -> Fact {
    let Ok(raw) = i32::try_from(pid) else {
        return Fact::No;
    };
    let Some(pid) = Pid::from_raw(raw) else {
        return Fact::No;
    };
    liveness_from(process::test_kill_process(pid))
}

/// The result-to-fact mapping, kept pure so every arm is reachable from a test — including errnos
/// that cannot be provoked on demand, which is what the old subprocess seam existed to reach and
/// could only do by breaking `PATH` for the whole test binary.
fn liveness_from(probe: Result<(), Errno>) -> Fact {
    match probe {
        // Exists. `Ok` is ours to signal; `EPERM` is someone else's — the kernel refused
        // *because* the process is there, which is the distinction `kill -0` loses.
        Ok(()) | Err(Errno::PERM) => Fact::Yes,
        // No such process.
        Err(Errno::SRCH) => Fact::No,
        // Anything else was not established, and a probe that could not answer must never be
        // read as "dead" — one `doctor --fix` would free every live claim.
        Err(_) => Fact::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::{is_alive, self_owner, session_owner, session_worktree};
    use jkb_fsm::Fact;
    use jkb_types::AgentId;

    /// The pid an owner id carries, for the tests that assert what this module mints.
    fn owner_pid(owner: &str) -> Option<u32> {
        match AgentId::parse(owner) {
            AgentId::Process { pid, .. } | AgentId::Session { pid, .. } => Some(pid),
            AgentId::Agent { .. } | AgentId::Unrecognized { .. } => None,
        }
    }

    #[test]
    fn self_owner_is_host_colon_pid() {
        let owner = self_owner();
        assert_eq!(owner_pid(&owner), Some(std::process::id()));
    }

    #[test]
    fn a_live_process_is_alive() {
        assert_eq!(is_alive(&self_owner()), Fact::Yes);
    }

    /// A pid we can probe and find nothing for is **proven** dead; a shape we cannot read is
    /// *unestablished*, which is a different answer and must not free the task.
    #[test]
    fn a_dead_pid_is_no_and_an_unreadable_owner_is_unknown() {
        assert_eq!(is_alive("host:4294967290"), Fact::No);
        assert_eq!(is_alive("garbage"), Fact::Unknown);
        assert_eq!(is_alive("agent:01JBX7Q4"), Fact::Unknown);
    }

    /// A probe that **could not answer** is `Unknown`, never `No`.
    ///
    /// This is the defect the whole `Fact` type exists to prevent, sitting in the one probe that
    /// protects every claim: fold "could not establish" into "that process is gone" and one
    /// `doctor --fix` frees every live `host:pid` claim in the database.
    ///
    /// It is reached directly now. The previous version had to make a spawn fail for real — an
    /// absolute path to a program that is not there — because the only way in was through the
    /// subprocess. (An earlier version emptied `PATH`, which is process-global while `cargo test`
    /// runs this binary on a thread pool, and reddened the shared gate about one run in six in
    /// tests with no connection to the change.) With the probe a syscall, the mapping is a pure
    /// function and every arm is an ordinary assertion.
    #[test]
    fn a_probe_that_could_not_answer_is_unknown_not_dead() {
        use rustix::io::Errno;
        assert_eq!(super::liveness_from(Err(Errno::NOMEM)), Fact::Unknown);
        assert_eq!(super::liveness_from(Err(Errno::INVAL)), Fact::Unknown);
    }

    /// `EPERM` is the distinction the shell's `kill -0` loses, and the reason `ps` was reached
    /// for in the first place: the kernel refuses because the process is **there** and is not
    /// ours. Reading it as dead reclaims a running agent's work (D27.2).
    #[test]
    fn eperm_means_alive_and_esrch_means_dead() {
        use rustix::io::Errno;
        assert_eq!(super::liveness_from(Err(Errno::PERM)), Fact::Yes);
        assert_eq!(super::liveness_from(Err(Errno::SRCH)), Fact::No);
        assert_eq!(super::liveness_from(Ok(())), Fact::Yes);
    }

    #[test]
    fn a_foreign_owned_live_process_is_alive() {
        // pid 1 (launchd/init) always exists and is owned by root. `kill -0` would exit
        // EPERM (non-zero) here when we are not root; `ps -p` reports it alive regardless.
        assert_eq!(is_alive("host:1"), Fact::Yes);
    }

    /// The claim `jkb task work` takes must survive the process that took it — otherwise
    /// `doctor --fix` frees the task while the session is still open (design D36.6).
    #[test]
    fn a_session_outlives_the_process_that_claimed_it() {
        let tmp = tempfile::tempdir().unwrap();
        let owner = session_owner(tmp.path());
        assert_eq!(owner_pid(&owner), Some(std::process::id()));
        assert_eq!(session_worktree(&owner).as_deref(), Some(tmp.path()));

        // A dead pid but a live worktree: still claimed. The pid is provenance, not liveness.
        let orphan = format!("session:4294967290:{}", tmp.path().display());
        assert_eq!(
            is_alive(&orphan),
            Fact::Yes,
            "a live worktree keeps the claim"
        );

        // Remove the worktree and the claim becomes reclaimable — `land`/`abandon` are the
        // only commands that do this. A live pid must NOT keep it alive: pids are recycled,
        // and this one belongs to a process that exited long ago.
        let gone = format!(
            "session:{}:{}",
            std::process::id(),
            tmp.path().join("nope").display()
        );
        assert_eq!(is_alive(&gone), Fact::No, "the worktree alone decides");

        assert!(session_worktree("host:123").is_none());
    }

    #[test]
    fn owner_pid_reads_the_second_field() {
        assert_eq!(owner_pid("node-1:12345"), Some(12345));
        assert_eq!(owner_pid("host:12:run"), Some(12));
        assert_eq!(owner_pid("host"), None);
    }

    /// An externally-minted id is preferred when the environment names one, so a subagent's
    /// claim outlives the process that took it and is never auto-reclaimed.
    #[test]
    fn an_env_agent_id_becomes_the_owner() {
        // `env_agent` reads the process environment, so this asserts the shape it produces
        // rather than mutating the environment out from under a parallel test.
        let id = AgentId::agent("run-7").as_str();
        assert_eq!(id, "agent:run-7");
        assert_eq!(is_alive(&id), Fact::Unknown);
    }
}
