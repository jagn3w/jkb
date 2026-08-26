//! Probing a claim owner's liveness (design D27.1/D27.2, S3.1/S3.2).
//!
//! The *identity* is [`jkb_types::AgentId`] — a parsed type with a closed set of shapes, each
//! declaring what would prove it via [`Liveness`]. This module is the other half: the probe, at
//! the edge, where probing processes and touching the filesystem belong.
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
/// `Unknown` when liveness could not be established at all. The land lock reads this to decide whether a
/// holder is stale, and treating "could not ask" as "gone" there breaks a live lock.
#[must_use]
pub fn pid_alive(pid: u32) -> Fact {
    pid_exists(pid)
}

/// This machine's name, for a test that must build an owner id this host will actually probe.
#[cfg(test)]
pub fn hostname_for_test() -> String {
    hostname()
}

/// This machine's name — asked of the kernel when the environment does not say.
///
/// The environment comes first so a test (and an operator) can pin it. What matters is the
/// fallback: it used to be the literal `"localhost"`, which both a host and the dev container
/// running on it would answer, so `host:pid` owner ids from either side compared EQUAL and the
/// container's pid namespace was probed as if it were this one. A rule whose two sides answer the
/// same name is not a rule, so the last resort is `uname`, which names the machine.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("HOST").ok())
        .filter(|h| !h.is_empty())
        .or_else(|| {
            let uts = rustix::system::uname();
            let node = uts.nodename().to_string_lossy().into_owned();
            (!node.is_empty()).then_some(node)
        })
        .unwrap_or_else(|| "localhost".to_owned())
}

/// Whether `owner` still exists — proven, disproven, or unestablished.
///
/// The match is over [`Liveness`], a closed enum, so a new owner shape cannot be added without
/// the compiler demanding a probe for it. Per shape:
///
/// * a **process** is probed with `kill(pid, 0)`, which reports existence **regardless of which
///   OS user owns it**: `EPERM` means the process is there and is not ours, which is as good an
///   answer as `Ok`. (The shell's `kill -0` is what was rejected, and rightly — it collapses
///   `EPERM` and `ESRCH` into one non-zero exit. `pid_exists` below has the full history.)
/// * a **session** is judged **only** by its worktree (design D36.6). `jkb task work` exits in
///   under a second, so its pid is gone before anyone reads the claim; the thing that persists
///   — and that means "this work is in flight" — is the checkout. The pid is ignored rather
///   than consulted as a fallback, so a *recycled* pid cannot keep a removed session's claim
///   alive after `land`/`abandon` took its worktree away.
/// * an **external** agent, an owner naming **another host**, and any id we cannot read are all
///   [`Fact::Unknown`]: nothing here can say. That is not "dead" — see the module docs.
#[must_use]
pub fn is_alive(owner: &str) -> Fact {
    match AgentId::parse(owner).liveness() {
        // A pid is only meaningful on the host that issued it. `~/.jkb` is bind-mounted into the
        // dev container on purpose, so a claim — or a sweep lock — written in there names a pid in
        // the container's namespace, and probing it here answers about whichever local process
        // holds that number: a live owner reported dead, or a dead one alive. Unknown is the only
        // honest answer for another host, and unknown never frees anything (D48.10).
        Liveness::Process { host, pid } if host == hostname() => pid_exists(pid),
        // `Path::exists` is itself lossy — it answers `false` for a permission error as well as
        // for a missing directory — so it is asked through `try_exists`, which separates them.
        // And an absence is only proof where the place it would be is visible; see `absent_here`.
        Liveness::Worktree(dir) => present_here(&dir),
        // An owner on another host and an external agent are the same answer for the same
        // reason: nothing here can establish it. Never "dead" — see the module docs.
        Liveness::Process { .. } | Liveness::External => Fact::Unknown,
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
/// Whether `path` is there, as far as this machine can establish.
///
/// AN ABSENCE IS ONLY PROOF WHERE YOU CAN SEE THE PLACE IT WOULD BE — the same rule the archive
/// sweep applies to a record whose repo it cannot reach. `Liveness::Process` was host-qualified so
/// a container's pid is not probed against this machine's process table; a filesystem path is no
/// more portable across that boundary, and it was left un-qualified. A host session claims as
/// `session:<pid>:/Users/…/.jkb/work/sess`; inside the container that path does not exist, so a
/// bare `try_exists` answered `false`, `Fact::No`, and `reclaim_dead` freed the claim of a session
/// running on the host — with `abandon` and `task work` then acting against it.
///
/// The session owner id carries no host, so the question is asked of the filesystem instead: the
/// directory that would CONTAIN it must be visible. On the host `…/.jkb/work` exists and a missing
/// `sess` is real; in the container it does not, so nothing is established. That is sound in both
/// directions and needs no change to an id format already written into databases.
///
/// Deliberately conservative at the edge: if the parent is gone too — someone removed `.jkb/work`
/// wholesale — this answers `Unknown` and the claim is reported rather than freed. Of the two ways
/// to be wrong, the one that costs a command wins (D34.4).
/// Answers the question the caller asks, rather than its inverse: the first version returned
/// "is it absent" and every arm was then flipped at the call site, which is one edit away from
/// reading backwards in the probe that decides whether a claim may be freed.
fn present_here(path: &Path) -> Fact {
    match path.try_exists() {
        Ok(true) => Fact::Yes,
        Err(_) => Fact::Unknown,
        Ok(false) => match path.parent() {
            // A path with no parent is `/`, whose absence is not a thing to reason about.
            None => Fact::Unknown,
            // The place it would be is visible, so it really is gone.
            Some(parent) => match parent.try_exists() {
                Ok(true) => Fact::No,
                _ => Fact::Unknown,
            },
        },
    }
}

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
    use super::{hostname, is_alive, self_owner, session_owner, session_worktree};
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
        assert_eq!(is_alive(&format!("{}:4294967290", hostname())), Fact::No);
        assert_eq!(is_alive("garbage"), Fact::Unknown);
        assert_eq!(is_alive("agent:01JBX7Q4"), Fact::Unknown);
    }

    /// A pid is only meaningful on the host that issued it, and `~/.jkb` is shared across exactly
    /// that boundary — the dev container's pid 1 is not this machine's pid 1. Probing a foreign
    /// owner's pid locally answers about whichever process holds that number here: a live owner
    /// reported dead (and its claim freed), or a dead one reported alive.
    /// The same rule as the pid one, for the shape it was not applied to.
    ///
    /// A session's claim is its checkout, and a checkout on another machine is not absent — it is
    /// unobservable. Probing it as if it were local frees a live session's claim, which is what
    /// `abandon` and `task work` then act on.
    #[test]
    fn a_session_worktree_this_machine_cannot_see_is_unknown_not_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join(".jkb/work");
        std::fs::create_dir_all(&work).unwrap();

        // Alive: the checkout is there.
        let live = work.join("live");
        std::fs::create_dir(&live).unwrap();
        assert_eq!(is_alive(&session_owner(&live)), Fact::Yes);

        // Provably gone: this machine can see the directory it would be in.
        let gone = work.join("gone");
        assert_eq!(
            is_alive(&session_owner(&gone)),
            Fact::No,
            "an absence IS proof where the place it would be is visible"
        );

        // Another machine's layout: neither the checkout nor the tree it lives in is here. This
        // is the container looking at a host session, and it must not read as gone.
        assert_eq!(
            is_alive("session:1:/not-a-path-on-this-machine/repos/jkb/.jkb/work/sess"),
            Fact::Unknown,
            "a path from a filesystem this kernel does not have establishes nothing"
        );
    }

    #[test]
    fn an_owner_on_another_host_is_unknown_rather_than_probed_locally() {
        // pid 1 exists on every machine, so a local probe would answer `Yes` for this.
        assert_eq!(
            is_alive("some-other-machine:1"),
            Fact::Unknown,
            "another host's pid is not this host's to probe"
        );
        // ...and the same id on THIS host still answers from the kernel.
        assert_eq!(is_alive(&format!("{}:1", hostname())), Fact::Yes);
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

    /// A pid that really is gone, driven through the syscall.
    ///
    /// Every other dead-pid fixture here is `4294967290`, which is not representable as `pid_t`
    /// and short-circuits before `kill` is ever called — so `ESRCH` -> [`Fact::No`], the one
    /// verdict that frees another agent's claim, had no coverage at all. A child that has been
    /// spawned and reaped gives a pid that was real a moment ago and certainly is not now.
    #[test]
    fn a_reaped_child_is_established_dead() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn a short-lived child");
        let pid = child.id();
        child
            .wait()
            .expect("reap it, so it is gone rather than a zombie");
        assert_eq!(
            is_alive(&format!("{}:{pid}", hostname())),
            Fact::No,
            "a reaped pid must reach the kernel and come back ESRCH"
        );
    }

    #[test]
    fn a_foreign_owned_live_process_is_alive() {
        // pid 1 (launchd/init) always exists and is owned by root, so unless we ARE root the
        // kernel answers `EPERM` — which is the whole point: the refusal is evidence the process
        // exists. This is the case the shell's `kill -0` gets wrong by reading its non-zero exit
        // as "gone", and the case `ps` was originally brought in to recover.
        assert_eq!(is_alive(&format!("{}:1", hostname())), Fact::Yes);
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
