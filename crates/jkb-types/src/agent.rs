//! [`AgentId`]: who is holding a task, as a parsed type rather than a string with a parser
//! scattered across a module (design S3.1).
//!
//! A claim's owner was always an owner id — it was never keyed by branch — in one of two ad-hoc
//! shapes read with `split(':').nth(1)`. The model underneath is sound, and its two tempting
//! alternatives stay rejected: there is **no TTL and no heartbeat**, so an agent paused on a
//! permission prompt keeps its claim (design D27.1). What was wrong with it is the *string*:
//!
//! * the shapes were not enumerable, so nothing could check that every shape has a liveness
//!   rule — see [`Liveness`], which is a closed enum precisely so adding a shape breaks the
//!   probe until it is taught;
//! * there was no room for a third shape, which is what an agent that is neither a process we
//!   can see nor a worktree on this disk needs.
//!
//! The probe itself stays at the edge (`jkb-cli`'s `owner` module): this crate says what
//! *would* prove an owner alive, not how to look.

use std::fmt;
use std::path::{Path, PathBuf};

/// The host segment marking a **session** owner: `session:<pid>:<worktree>`.
const SESSION: &str = "session";

/// The host segment marking an **agent** owner: `agent:<id>`.
const AGENT: &str = "agent";

/// Host segments that mean something, and so may never be produced as an actual hostname.
const RESERVED_HOSTS: &[&str] = &[SESSION, AGENT];

/// Who holds a claim.
///
/// Round-trips through [`AgentId::as_str`]/[`AgentId::parse`] to the string stored in
/// `items.claimant_id`, so existing claims keep their meaning.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentId {
    /// A process on this machine: `host:pid`, or `host:pid:run` for a coordinator that wants to
    /// distinguish its runs. Subagents share their coordinator's pid, so the pid is the signal.
    Process {
        /// WHICH MACHINE the pid belongs to, and therefore whether it may be probed at all.
        ///
        /// Not informational — it decides. `~/.jkb` is bind-mounted into the dev container on
        /// purpose, so pid 812 written in there is a different process from pid 812 out here;
        /// probing one against the other reports a live owner dead (and frees its claim) or a
        /// dead one alive. `Liveness::Process` carries it, and the caller — which is the only
        /// party that knows this host's name — compares.
        host: String,
        /// The process to probe.
        pid: u32,
        /// An optional run discriminator, kept so two runs of one coordinator are distinct ids.
        run: Option<String>,
    },
    /// A `jkb task work` session: `session:<pid>:<worktree>` (design D36.6).
    ///
    /// The pid is **provenance** — which process opened the session — and is deliberately never
    /// consulted for liveness: `jkb task work` exits within a second, long before anybody reads
    /// the claim. The worktree is what persists and what means "this work is in flight".
    Session {
        /// Which process opened it. Recorded, never probed.
        pid: u32,
        /// The checkout whose existence is the claim.
        worktree: PathBuf,
    },
    /// An externally-minted agent identity: `agent:<id>`.
    ///
    /// For a caller that knows who it is and whose process and checkout are not the thing that
    /// persists — a subagent, a resumed session, a cloud run. Its liveness is
    /// [`Liveness::External`]: **nothing local can establish it**, which is a real answer rather
    /// than a missing one, and the machinery that consumes it treats an unestablished liveness
    /// as *do not reclaim* (design S3.2).
    ///
    /// The old objection — "session ids will not work, subagents are not resumable and there is
    /// no way to message them" — answered a different question, namely whether jkb could go and
    /// *ask* an agent something. A claim needs only a value that is stable for the life of the
    /// work; it does not need to be reachable.
    Agent {
        /// The opaque identity, as minted by whoever runs the agent.
        id: String,
    },
    /// An id in none of the shapes above, kept verbatim.
    ///
    /// Not an error: `items.claimant_id` is a plain column that older binaries and hand edits
    /// can write, and a value we cannot read is emphatically not a licence to free the task
    /// (see [`Liveness::External`]).
    Unrecognized {
        /// The stored value, unchanged, so it round-trips.
        raw: String,
    },
}

/// What would prove an owner is still there.
///
/// A closed enum on purpose: the probe at the edge matches exhaustively, so a new owner shape
/// cannot be added without the compiler demanding a liveness rule for it. That is the property
/// a string with an ad-hoc parser could not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// A process with this pid exists **on this host**.
    ///
    /// The host is carried because a pid means nothing without one: pid 812 in a dev container
    /// is a different process from pid 812 on the machine hosting it, and jkb deliberately shares
    /// `~/.jkb` across that boundary. Discarding it here made the probe answer about whichever
    /// process happened to hold that number locally — reporting a live owner dead, or a dead one
    /// alive, at exactly the boundary the claim model exists to be careful about. Only the caller
    /// knows this host's name, so the comparison is theirs; this type's job is to refuse to hand
    /// over a pid without the host it belongs to.
    Process { host: String, pid: u32 },
    /// This directory exists.
    Worktree(PathBuf),
    /// Nothing on this machine can say. Not "dead" — *unestablished*.
    External,
}

impl AgentId {
    /// This process's owner id, `host:pid`.
    #[must_use]
    pub fn this_process(host: &str, pid: u32) -> Self {
        Self::Process {
            host: sanitize_host(host),
            pid,
            run: None,
        }
    }

    /// The owner id for a session working in `worktree`.
    #[must_use]
    pub fn session(pid: u32, worktree: &Path) -> Self {
        Self::Session {
            pid,
            worktree: worktree.to_path_buf(),
        }
    }

    /// An externally-minted agent identity.
    ///
    /// The id is stored verbatim except that `:` is replaced with `-`, so it cannot absorb a
    /// field boundary and re-parse as something else.
    #[must_use]
    pub fn agent(id: &str) -> Self {
        Self::Agent {
            id: id.replace(':', "-"),
        }
    }

    /// Read a stored `claimant_id`.
    ///
    /// Total: every string parses, because a value we cannot recognize must round-trip rather
    /// than be dropped or treated as free.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let mut parts = raw.split(':');
        let head = parts.next().unwrap_or_default();
        let rest: Vec<&str> = parts.collect();
        match head {
            SESSION => {
                // `session:<pid>:<worktree>`; fields 2.. are rejoined so a path containing a
                // colon survives the round trip.
                match rest.split_first() {
                    Some((pid, tail)) if !tail.is_empty() => match pid.parse::<u32>() {
                        Ok(pid) => Self::Session {
                            pid,
                            worktree: PathBuf::from(tail.join(":")),
                        },
                        Err(_) => Self::Unrecognized {
                            raw: raw.to_owned(),
                        },
                    },
                    _ => Self::Unrecognized {
                        raw: raw.to_owned(),
                    },
                }
            }
            AGENT if !rest.is_empty() => Self::Agent { id: rest.join("-") },
            _ => match rest.split_first() {
                Some((pid, tail)) => match pid.parse::<u32>() {
                    Ok(pid) => Self::Process {
                        host: head.to_owned(),
                        pid,
                        run: (!tail.is_empty()).then(|| tail.join(":")),
                    },
                    Err(_) => Self::Unrecognized {
                        raw: raw.to_owned(),
                    },
                },
                None => Self::Unrecognized {
                    raw: raw.to_owned(),
                },
            },
        }
    }

    /// The stored spelling. Inverse of [`AgentId::parse`].
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Self::Process { host, pid, run } => match run {
                Some(run) => format!("{host}:{pid}:{run}"),
                None => format!("{host}:{pid}"),
            },
            Self::Session { pid, worktree } => {
                format!("{SESSION}:{pid}:{}", worktree.to_string_lossy())
            }
            Self::Agent { id } => format!("{AGENT}:{id}"),
            Self::Unrecognized { raw } => raw.clone(),
        }
    }

    /// What would prove this owner is still there. See [`Liveness`].
    ///
    /// An [`AgentId::Unrecognized`] id is [`Liveness::External`], **not** dead. Treating an
    /// unreadable owner as reclaimable is the one direction of error that frees a live agent's
    /// task; the other direction reports a claim a person can clear with one command
    /// (design S3.2).
    #[must_use]
    pub fn liveness(&self) -> Liveness {
        match self {
            Self::Process { host, pid, .. } => Liveness::Process {
                host: host.clone(),
                pid: *pid,
            },
            Self::Session { worktree, .. } => Liveness::Worktree(worktree.clone()),
            Self::Agent { .. } | Self::Unrecognized { .. } => Liveness::External,
        }
    }

    /// The session worktree this owner names, if it is a session owner.
    #[must_use]
    pub fn worktree(&self) -> Option<&Path> {
        match self {
            Self::Session { worktree, .. } => Some(worktree),
            _ => None,
        }
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// A host segment that cannot be mistaken for a shape marker or absorb a field boundary.
///
/// `:` becomes `-` (a container host like `node:1` would otherwise make the pid parse read the
/// wrong field), and a host that spells a reserved marker is suffixed rather than being allowed
/// to claim that shape's meaning.
fn sanitize_host(host: &str) -> String {
    let host = host.replace(':', "-");
    if RESERVED_HOSTS.contains(&host.as_str()) {
        format!("{host}-host")
    } else {
        host
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentId, Liveness};
    use std::path::{Path, PathBuf};

    /// Every shape round-trips through the stored string, including one we cannot read.
    #[test]
    fn every_shape_round_trips() {
        let cases = [
            "host:123",
            "host:123:run-7",
            "session:99:/tmp/work/a-task",
            "agent:01JBX7Q4",
            "garbage",
            "",
        ];
        for raw in cases {
            assert_eq!(AgentId::parse(raw).as_str(), raw, "round trip of `{raw}`");
        }
    }

    #[test]
    fn a_path_with_a_colon_survives() {
        let odd = Path::new("/tmp/we:ird/work");
        let id = AgentId::session(7, odd);
        assert_eq!(AgentId::parse(&id.as_str()), id);
        assert_eq!(id.worktree(), Some(odd));
    }

    /// The liveness basis is the whole point of the type: each shape declares what would prove
    /// it, and the probe at the edge matches this exhaustively.
    #[test]
    fn each_shape_declares_what_would_prove_it() {
        assert_eq!(
            AgentId::parse("host:42").liveness(),
            Liveness::Process {
                host: "host".into(),
                pid: 42
            },
            "a process is proven by its pid ON ITS OWN HOST — a pid without one names nothing"
        );
        assert_eq!(
            AgentId::parse("session:1:/tmp/w").liveness(),
            Liveness::Worktree(PathBuf::from("/tmp/w")),
            "a session is proven by its checkout, never by the pid that opened it"
        );
        assert_eq!(
            AgentId::parse("agent:abc").liveness(),
            Liveness::External,
            "nothing local can establish an external agent"
        );
    }

    /// An id we cannot read is **not** dead. Reading it as dead is the error that frees a live
    /// agent's task; reading it as unestablished holds the claim and reports it.
    #[test]
    fn an_unreadable_owner_is_unestablished_not_dead() {
        assert_eq!(AgentId::parse("garbage").liveness(), Liveness::External);
        assert_eq!(
            AgentId::parse("host:not-a-pid").liveness(),
            Liveness::External
        );
    }

    /// A host that spells a shape marker must not be able to claim that shape's meaning.
    #[test]
    fn a_reserved_host_name_cannot_impersonate_a_shape() {
        let id = AgentId::this_process("session", 5);
        assert_eq!(id.as_str(), "session-host:5");
        assert_eq!(
            id.liveness(),
            Liveness::Process {
                host: "session-host".into(),
                pid: 5
            }
        );
        let id = AgentId::this_process("node:1", 5);
        assert_eq!(
            id.as_str(),
            "node-1:5",
            "a colon in the host is neutralized"
        );
    }

    /// A colon inside an externally-minted id cannot re-parse as another field.
    #[test]
    fn an_agent_id_cannot_absorb_a_field_boundary() {
        let id = AgentId::agent("run:7");
        assert_eq!(id.as_str(), "agent:run-7");
        assert_eq!(AgentId::parse(&id.as_str()), id);
    }
}
