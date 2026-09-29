//! Telling the dev container to bound its own Bash-sandbox deny list.
//!
//! **Why this lives on the host.** Claude Code's Bash sandbox enumerates every session transcript
//! into one argv, Linux caps one argument at `MAX_ARG_STRLEN`, and a dev container over that limit
//! fails *every* Bash tool call at spawn with `E2BIG` — total, from the first call, with nothing in
//! the message naming transcripts (measured 2026-09-28; `.container/sweep-transcripts.sh` carries
//! the numbers). `.container/run.sh` sweeps on every container start, and that was the only trigger
//! there was: the container that produced the failure had reached 1,182 transcripts *without being
//! recreated*, because the documented workflow is `run.sh` once and then attach and keep working.
//!
//! **Why the reaper.** It is the one long-lived thing on the host already sweeping on a timer for
//! this project, and this is the same kind of job: something only an outside process is placed to
//! do. Rejected: a loop inside the container and a timer unit beside this one, both of which are a
//! second scheduler to reason about for one sweep.
//!
//! **Why `docker exec`, and why on STDIN.** `~/.claude-state` is a named Docker **volume**
//! (`jkb-claude-state`), not a host bind, so there is no host path to walk: the work can only happen
//! inside, and `docker exec` is the only way in. What it runs, though, was very nearly wrong. The
//! first version baked the script into the image and exec'd `/usr/local/bin/sweep-transcripts.sh` —
//! which **no already-running container has**, because only a rebuilt image carries it and nothing
//! forces a rebuild. On every live container that exec would have exited 127, been reported once
//! into `reap.log`, and deduped for ever, with every gate green. So the script is embedded in this
//! binary at compile time and fed to `bash -s` on stdin. There is then no second copy to drift, no
//! rebuild to require, and no path to agree about: the reaper runs exactly the sweep the `jkb` that
//! `setup.sh` installed was built from.
//!
//! **It is never fatal, and silent unless something is wrong.** A host with no Docker, or no such
//! container, or a stopped one, is the ordinary case for anyone not using the dev container and says
//! nothing at all. A Docker that will not *answer* is not that case, and does say so.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The dev container's name — `.container/run.sh` spells it `${JKB_CONTAINER_NAME:-jkb-dev}`.
///
/// Held to that default by `check-config.sh`, which reads both: a reaper poking a container name
/// nothing creates would be silent for ever, which is the failure this whole module exists to end.
pub const DEV_CONTAINER_NAME: &str = "jkb-dev";

/// The environment variable `run.sh` takes the container's name from.
pub const CONTAINER_NAME_VAR: &str = "JKB_CONTAINER_NAME";

/// The container this host should sweep: what `run.sh` would have named, the same way it names it.
///
/// **Honoured for two reasons, and the second is why it is not just tidiness.** An operator who
/// sets `JKB_CONTAINER_NAME` creates a container this reaper would otherwise never find — a trigger
/// silently dead for ever, which is the exact failure the name guard exists to prevent. And it is
/// the seam that keeps a TEST SUITE from reaching a real container: `cargo test` spawns
/// `task reap --watch` as a real child with the developer's own environment, so without an override
/// the first tick `docker exec`s the sweep into the running `jkb-dev` and archives the developer's
/// live transcripts. A name nothing can create is the fixture's answer.
#[must_use]
pub fn dev_container_name() -> String {
    chosen_container_name(std::env::var(CONTAINER_NAME_VAR).ok())
}

/// The name an override does or does not supply — the decision, without the environment.
///
/// Separated because a test that reached for `set_var` would be setting a process-wide variable in a
/// binary whose other tests concurrently fork `git`, which is the one rule this crate wrote down for
/// itself (`gitrepo.rs`). The environment read is one line above and has nothing to decide.
fn chosen_container_name(from_env: Option<String>) -> String {
    from_env
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEV_CONTAINER_NAME.to_owned())
}

/// The sweep itself, embedded at compile time.
///
/// `include_str!` rather than a file the container carries, for the reason in the module docs: a
/// copy in the image is a copy that drifts and that an existing container does not have. The crate
/// already reaches out of itself this way for `.claude/commands/*.md`.
const SWEEP_SCRIPT: &str = include_str!("../../../.container/sweep-transcripts.sh");

/// How long one Docker call may take before it is abandoned.
///
/// **The reaper is not only doing this.** The same tick finishes every deferred worktree landing on
/// the machine, so a Docker daemon that never answers — a wedged containerd, a storage-starved host,
/// a `DOCKER_HOST` pointing somewhere unreachable — would stop that too, for ever, on an unbounded
/// wait. A sweep that does not happen costs a long deny list; a reaper that never returns costs
/// every landing on the machine.
///
/// Generous rather than tight: `docker exec` on a loaded host is slow before it is broken, and a
/// timeout that fired on slowness would report an unreachable daemon every tick on a busy machine.
const DOCKER_TIMEOUT: Duration = Duration::from_mins(1);

/// What one attempt came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sweep {
    /// No Docker, or no container of that name running.
    ///
    /// This is the answer on a host with no Docker at all — a plain cloud instance, a CI runner, a
    /// laptop that only ever runs `jkb` — and on one where Docker is installed but this container
    /// was never created. Both are somebody working normally, and neither wants a line in a log.
    Absent,
    /// Docker is there and would not answer inside [`DOCKER_TIMEOUT`].
    ///
    /// SAID, unlike [`Sweep::Absent`], because it is the difference between "there is nothing to
    /// sweep here" and "there may be something to sweep and I could not find out" — and silence on
    /// the second is the green-log-over-a-broken-container failure this subsystem exists to end.
    Unreachable(String),
    /// It ran and had nothing to say.
    Quiet,
    /// It ran and said something worth a line in the log.
    Said(String),
    /// It ran and reported a problem — the deny list is over budget, or it could not archive.
    Failed(String),
}

/// One command run: `(success, stdout, stderr)`, or `None` when the program is not installed.
///
/// `None` means exactly one thing — **no `docker` binary on this host** — because it is the only
/// answer a caller is allowed to keep quiet about. A run that hit [`DOCKER_TIMEOUT`] answers with
/// [`TIMED_OUT`] as its stderr, and one that had a `docker` to launch and still could not launch it
/// answers with [`LAUNCH_FAILED`]; both are sentinels rather than messages so no caller has to match
/// Docker's wording for them.
pub type Runner<'a> = &'a dyn Fn(&[&str], Option<&str>) -> Option<(bool, String, String)>;

/// The stderr a [`Runner`] reports when its command ran out of time.
pub const TIMED_OUT: &str = "\u{0}jkb:timed-out";

/// The stderr prefix a [`Runner`] reports when `docker` exists but could not be launched.
///
/// A PREFIX, not an exact sentinel like [`TIMED_OUT`], because the OS error is the whole diagnostic
/// value here — a permission denied on the binary, an exec format error, a full process table —
/// and the reason is appended to it.
pub const LAUNCH_FAILED: &str = "\u{0}jkb:launch-failed ";

/// The environment variable the sweep reads its never-archive list from.
///
/// Spelled once here and once in `.container/sweep-transcripts.sh`; `check-config.sh` requires the
/// two to agree, because a keep-list the sweep does not read is a keep-list that protects nothing
/// while every gate reports the property.
pub const KEEP_SESSIONS_VAR: &str = "JKB_KEEP_SESSIONS";

/// What the sweep prints when it ran and there was nothing to do.
///
/// These are the sweep's words, and `check-config.sh` requires each to be text the sweep actually
/// emits — the same guard it applies to `verify.sh`'s classifiers, and for the same reason: a
/// phrase living in two files with nothing comparing them silently reclassifies every future run
/// the day one end is reworded.
const NOTHING_TO_DO: [&str; 3] = [
    "nothing to archive",
    "no transcripts",
    // The sweep exits 0 for a root that is not there too, and this one was missing: a container
    // whose transcript root is absent logged one identical line every quarter hour for ever,
    // because `Said` is printed unconditionally and only `Failed`/`Unreachable` are deduped.
    // check-config.sh derives the sweep's own quiet exits and requires each to be declared here,
    // so a fourth one is a red gate rather than a new log line.
    "does not exist",
];

/// How recently a session must have been SEEN to count as live.
///
/// **Absence of an end record is not life, and treating it as life caused the failure this whole
/// feature prevents.** A container session ends without its `SessionEnd` hook whenever the container
/// is stopped — every rebuild, and the documented E2BIG recovery, which is `run.sh` recreating it.
/// Those rows can never be closed afterwards: the liveness probe needs the same instance, and a
/// recreated container is a different one, so every orphan stays open until the 90-day prune. Handed
/// to the sweep, each shields the whole subtree under it (`<slug>/<id>/subagents/**` — the bulk of
/// the population), and three or four orphaned swarm sessions exceed the entire budget on their own:
/// the sweep reclaims nothing and every Bash call goes on dying at spawn.
///
/// Six refresh windows, derived from the registry's own cadence rather than picked: a row is
/// refreshed at most once an hour, so six hours of silence is not evidence of life. Over-keeping is
/// only the safe direction while it LAPSES.
pub const LIVE_SEEN_WITHIN_MS: i64 = jkb_core::claude_session::SEEN_REFRESH_MS * 6;

/// The session ids a sweep must not plan, from `(session, seen_at)` pairs.
///
/// **One rule, two callers, because there are two triggers.** The reaper reads the registry from the
/// database; `run.sh` asks the daemon for it on the host. Those are different access paths and will
/// stay different — but "which sessions count" is one question, and the round that first answered it
/// answered it in only one of the two places: the staleness fix above landed on the reaper while
/// `run.sh` went on passing ids from rows that had been open for months. A rule every call site must
/// separately remember is the defect this repository keeps rediscovering.
pub fn live_ids<I: IntoIterator<Item = (String, i64)>>(rows: I, now: i64) -> Vec<String> {
    let cutoff = now.saturating_sub(LIVE_SEEN_WITHIN_MS);
    let mut ids: Vec<String> = rows
        .into_iter()
        .filter(|(_, seen_at)| *seen_at >= cutoff)
        .map(|(session, _)| session)
        .collect();
    // One id per session however many processes hold it, and a stable order so two runs that see the
    // same sessions produce the same string.
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The verdict of a multi-line report is its last non-empty line; it says several things on the way
/// to one. The sweep's own `--self-test` and `.container/verify.sh` read it the same way.
fn last_line(s: &str) -> String {
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// Sweep the dev container's transcripts, driving Docker through `run`.
///
/// Separated from [`sweep_dev_container`] so the decisions here are testable without Docker: what
/// counts as absent, what counts as unreachable, what counts as quiet, and what reaches the log.
pub fn sweep_with(name: &str, live: &[String], run: Runner<'_>) -> Sweep {
    // ASKED BEFORE POKED, so "there is no such container" is a fact rather than an error message
    // parsed out of a failed exec. `docker exec` against a missing container and against a broken
    // one both exit non-zero with prose, and telling those apart by their wording is a guess that
    // goes stale with the next Docker release.
    //
    // `docker ps --filter` RATHER THAN `docker inspect`, because the two questions have to be told
    // apart and only this one separates them by EXIT CODE. `inspect` on a missing container exits
    // non-zero — the same as a daemon that cannot be reached, a socket the user may not open, or a
    // `DOCKER_HOST` pointing nowhere — so every one of those became `Absent`, which is reported by
    // saying nothing. A trigger dead for ever, never saying it could not find out. `ps --filter`
    // exits 0 with EMPTY output when the daemon answered and there is no such container, so
    // silence is earned rather than assumed, and it lists only running containers, which is the
    // question anyway.
    // THE FILTER ONLY NARROWS; the decision is the exact match below. `--filter name=` is a
    // REGEX on the daemon's side, so an anchored `^…$` is the one part of this that could fail
    // CLOSED — silently listing nothing, which reads as "no such container" — if a future Docker
    // treats the anchors differently or a name ever carries a metacharacter. Unanchored it can
    // only over-list, and over-listing is what the exact match is for.
    let probe = run(
        &[
            "ps",
            "--filter",
            &format!("name={name}"),
            "--format",
            "{{.Names}}",
        ],
        None,
    );
    let Some((ok, names, err)) = probe else {
        // The BINARY is not there. A laptop or a cloud instance that never runs containers is
        // somebody working normally, and it is the one case that stays silent — and it is the ONLY
        // one, which is why `None` is narrowed to `ErrorKind::NotFound` at the spawn rather than
        // meaning "could not launch". Every other spawn failure — `docker` present but not
        // executable by this user, an exec format error, a process table with no room left — is a
        // host that MEANT to have containers, and answering `Absent` retired the trigger on it
        // permanently without ever saying so.
        return Sweep::Absent;
    };
    if let Some(why) = err.strip_prefix(LAUNCH_FAILED) {
        return Sweep::Unreachable(format!("docker is installed but could not be run: {why}"));
    }
    if err == TIMED_OUT {
        return Sweep::Unreachable(format!("docker did not answer when asked about {name}"));
    }
    if !ok {
        // Docker is installed and would not answer: a daemon that is down, a socket this user may
        // not open, a context or DOCKER_HOST pointing somewhere unreachable. Any of those leaves
        // the container unswept, and none of them is a reason to be quiet about it.
        return Sweep::Unreachable(format!(
            "docker is installed but could not be asked about {name}: {}",
            last_line(&err)
        ));
    }
    if !names.lines().any(|l| l.trim() == name) {
        // The daemon answered and there is no such container running. Earned silence.
        return Sweep::Absent;
    }
    // THE SESSIONS THIS MACHINE KNOWS ARE LIVE, so the sweep never plans one. `KEEP_NEWEST`'s whole
    // argument was "the live session is writing one of them right now", and it was written for a
    // sweep that ran at container START, when nothing is open. On this timer it runs mid-flight, and
    // during a swarm more than 32 transcripts are touched inside one window — at which point the
    // newest-32 floor stops being a statement about live sessions and a running one can be archived
    // out from under itself. A transcript is named for its session, so the ids are the answer.
    //
    // Passed as ONE environment variable rather than arguments: `docker exec -e` leaves the script's
    // own argument dispatch alone, and an id list is data, not a flag.
    let keep = format!("{KEEP_SESSIONS_VAR}={}", live.join(" "));
    let Some((ok, out, err)) = run(
        &["exec", "-i", "-e", &keep, name, "bash", "-s"],
        Some(SWEEP_SCRIPT),
    ) else {
        // NOT `Absent`. The probe just answered, so Docker is there and the container is running —
        // a spawn that fails now is the client vanishing between two calls, not a host without
        // containers, and `Absent` would lose the tick in silence. The round that stopped the
        // probe arm absorbing this left the exec arm doing it.
        return Sweep::Unreachable(format!(
            "docker answered about {name} and then could not be run"
        ));
    };
    if err == TIMED_OUT {
        return Sweep::Unreachable(format!("the sweep in {name} did not finish"));
    }
    if !ok {
        let why = last_line(&err);
        let why = if why.is_empty() { last_line(&out) } else { why };
        // NEVER EMPTY. `last_transcript_failure` starts as the empty string and dedups on equality,
        // so a failure with nothing on either stream — an exec killed by a daemon restart, rc 137
        // with no output — would be suppressed on its first occurrence and every one after, in the
        // one state this module exists to report.
        return Sweep::Failed(if why.is_empty() {
            format!("the sweep in {name} failed and printed nothing")
        } else {
            why
        });
    }
    let said = last_line(&out);
    // A tick that archived nothing is the steady state — 96 times a day, for ever. The reaper's own
    // rule for its log applies: say what CHANGED, not that a timer fired.
    if said.is_empty() || NOTHING_TO_DO.iter().any(|p| said.contains(p)) {
        Sweep::Quiet
    } else {
        Sweep::Said(said)
    }
}

/// Sweep the dev container's transcripts, if there is one, never touching a live session's.
#[must_use]
pub fn sweep_dev_container(name: &str, live: &[String]) -> Sweep {
    sweep_with(name, live, &|args, stdin| {
        // Spawned rather than `output()`ed, so the wait has a deadline.
        let spawned = Command::new("docker")
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        // ONLY a missing binary is `None`. See `Runner`: `None` is the one answer callers report by
        // staying silent, so every other spawn error has to arrive as a message instead.
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => return Some((false, String::new(), format!("{LAUNCH_FAILED}{e}"))),
        };
        // EVERY PIPE GETS A THREAD, and all three for the same reason: a pipe nobody is moving
        // blocks whoever is on the other end of it.
        //
        // The WRITER, because the script is larger than a pipe buffer (74KB against 64KB) and
        // `bash -s` executes as it reads, so a blocking write from here deadlocks the moment the
        // child pauses to run a `find`. The handle is moved in, so the pipe closes when the thread
        // ends and `bash` sees EOF.
        //
        // The READERS, because the child can outrun a 64KB buffer too, and the case where it does
        // is the one that matters: an archive that has gone read-only or full makes `mkdir -p`
        // fail for every planned file, ~100 bytes of stderr each, ~100KB across a thousand. Left
        // unread until `wait_with_output` after the loop, the child blocked on write, `try_wait`
        // never returned `Some`, and at sixty seconds this reported `Unreachable` — telling the
        // operator the daemon would not answer when the daemon was fine and the disk was full,
        // and burning the whole timeout before `reap_once` on every tick.
        let writer = stdin.map(|s| {
            let mut pipe = child.stdin.take();
            let body = s.to_owned();
            std::thread::spawn(move || {
                if let Some(p) = pipe.as_mut() {
                    let _ = p.write_all(body.as_bytes());
                }
                drop(pipe);
            })
        });
        // Spelled twice rather than shared: the two handles are different types, and a generic
        // helper for six lines reads worse than the six lines.
        let out_pipe = child.stdout.take();
        let out_t = std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(mut p) = out_pipe {
                let _ = std::io::Read::read_to_string(&mut p, &mut buf);
            }
            buf
        });
        let err_pipe = child.stderr.take();
        let err_t = std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(mut p) = err_pipe {
                let _ = std::io::Read::read_to_string(&mut p, &mut buf);
            }
            buf
        });
        let deadline = Instant::now() + DOCKER_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    // This kills the local `docker exec` CLIENT. The sweep itself is a `bash` inside
                    // the container and keeps running: `exec` does not signal the remote process
                    // when its client dies. That is deliberate — a half-finished archive is worse
                    // than a slow one, and the sweep is written to be interrupted safely at a file
                    // boundary, not mid-`mv`. What it costs is that a container slow enough to time
                    // out once will usually do it again, and each tick leaves another `bash`
                    // running; they do not serialise, and the message they produce dedups to one
                    // constant key, so the stacking is silent. The reason that is tolerable and not
                    // a leak: `DOCKER_TIMEOUT` is a minute against a sweep measured in seconds, so
                    // reaching it at all means the container is wedged, which is the state the
                    // `Unreachable` below exists to report.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Some((false, String::new(), TIMED_OUT.to_owned()));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
        if let Some(w) = writer {
            let _ = w.join();
        }
        // Joined AFTER the child is gone or killed, so both pipes have reached EOF and neither
        // join can outlive the deadline the loop above enforces.
        let status = child.wait().ok()?;
        let stdout = out_t.join().unwrap_or_default();
        let stderr = err_t.join().unwrap_or_default();
        Some((status.success(), stdout, stderr))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        sweep_with, Sweep, DEV_CONTAINER_NAME, KEEP_SESSIONS_VAR, LAUNCH_FAILED, NOTHING_TO_DO,
        SWEEP_SCRIPT, TIMED_OUT,
    };
    use std::cell::RefCell;

    // Borrowed rather than `'static`: one reply is built at runtime (the launch-failure sentinel
    // plus the OS reason), and spelling that sentinel as a literal here would be a second copy of
    // a constant whose whole job is to be compared against.
    type Reply<'a> = Option<(bool, &'a str, &'a str)>;
    type Seen = RefCell<Vec<(Vec<String>, bool)>>;

    /// A runner that answers a fixed script of replies and records what it was asked, and whether
    /// anything was handed to the command on stdin.
    fn runner<'a>(
        replies: Vec<Reply<'a>>,
        seen: &'a Seen,
    ) -> impl Fn(&[&str], Option<&str>) -> Option<(bool, String, String)> + 'a {
        let replies = RefCell::new(replies.into_iter());
        move |args: &[&str], stdin: Option<&str>| {
            seen.borrow_mut().push((
                args.iter().map(|a| (*a).to_owned()).collect(),
                stdin.is_some(),
            ));
            replies
                .borrow_mut()
                .next()
                .flatten()
                .map(|(ok, o, e)| (ok, o.to_owned(), e.to_owned()))
        }
    }

    /// A ROW WITH NO END RECORD IS NOT A LIVE SESSION. A container session ends without its
    /// `SessionEnd` hook whenever the container is stopped — every rebuild, and the documented
    /// E2BIG recovery — and those rows can never be closed afterwards, so they sat open for the
    /// 90-day prune. The sweep protects the whole subtree under each id, and three or four orphaned
    /// swarm sessions exceed the entire budget: the sweep then reclaimed nothing and every Bash call
    /// went on dying at spawn, reached by the recovery step the record tells the operator to run.
    #[test]
    fn a_session_nobody_has_seen_for_hours_stops_holding_its_transcripts() {
        // `(session, seen_at)` pairs, which is all the rule is about — the two callers map
        // their own row types to this, and neither type belongs in the rule.
        let row = |session: &str, seen_at: i64| (session.to_owned(), seen_at);
        let now = 1_000_000_000_000;
        let hour = jkb_core::claude_session::SEEN_REFRESH_MS;
        let got = super::live_ids(
            vec![
                row("fresh", now - 60_000),
                row("an-hour-idle", now - hour),
                // Orphaned by a container recreate: open for ever, and the shape that caused it.
                row("orphaned-weeks-ago", now - hour * 24 * 14),
                // One session, two processes holding it: one id out.
                row("fresh", now - 120_000),
            ],
            now,
        );
        assert_eq!(got, vec!["an-hour-idle", "fresh"], "sorted and deduped");
        assert!(
            !got.iter().any(|id| id == "orphaned-weeks-ago"),
            "an unclosable row must stop protecting, or the sweep can never reclaim"
        );
        // ...and the boundary is the cutoff itself, not something either side of it.
        let edge = super::live_ids(
            vec![
                row("just-inside", now - super::LIVE_SEEN_WITHIN_MS),
                row("just-outside", now - super::LIVE_SEEN_WITHIN_MS - 1),
            ],
            now,
        );
        assert_eq!(edge, vec!["just-inside"]);
    }

    #[test]
    fn no_docker_at_all_is_absent_and_pokes_nothing_further() {
        let seen = Seen::default();
        assert_eq!(
            sweep_with("jkb-dev", &[], &runner(vec![None], &seen)),
            Sweep::Absent
        );
        assert_eq!(seen.borrow().len(), 1, "it must not try to exec after that");
    }

    /// The daemon ANSWERED and listed nothing: there is no such container running. Silence here is
    /// earned, which is the whole difference from the case below.
    #[test]
    fn a_container_that_is_not_running_is_absent() {
        let seen = Seen::default();
        let r = runner(vec![Some((true, "", ""))], &seen);
        assert_eq!(sweep_with("jkb-dev", &[], &r), Sweep::Absent);
        assert_eq!(seen.borrow().len(), 1);
    }

    /// A name that merely CONTAINS ours is not ours. `--filter name=` is a regex on the daemon's
    /// side and is passed UNANCHORED on purpose — anchors are the one place the probe could fail
    /// closed and silently — so it can only over-list, and this exact match is the whole decision.
    #[test]
    fn a_container_whose_name_merely_contains_ours_is_not_ours() {
        let seen = Seen::default();
        let r = runner(vec![Some((true, "jkb-dev-scratch\n", ""))], &seen);
        assert_eq!(sweep_with("jkb-dev", &[], &r), Sweep::Absent);
        assert_eq!(seen.borrow().len(), 1, "and nothing is exec'd into it");
    }

    /// Installed, and would not answer. Every one of these used to be `Absent` — reported by
    /// saying nothing — so the trigger could be dead for ever on a host that has the container:
    /// a launchd agent whose PATH omits Docker Desktop, a socket this user may not open, a
    /// `DOCKER_HOST` pointing nowhere.
    #[test]
    fn a_docker_that_will_not_answer_is_said_not_swallowed() {
        let seen = Seen::default();
        let r = runner(
            vec![Some((
                false,
                "",
                "Cannot connect to the Docker daemon at unix:///var/run/docker.sock\n",
            ))],
            &seen,
        );
        match sweep_with("jkb-dev", &[], &r) {
            Sweep::Unreachable(why) => assert!(why.contains("could not be asked"), "{why}"),
            other => panic!("wanted Unreachable, got {other:?}"),
        }
        assert_eq!(seen.borrow().len(), 1, "it must not exec through that");
    }

    /// Present, and could not be launched. Distinct from a host with no `docker` at all, which is
    /// the ONE silent answer — and the distinction is the whole point: a `docker` the service user
    /// may not execute, or an exec that fails for want of a process slot, is a host that meant to
    /// have containers. Absorbed into `Absent` it retired the trigger for ever without a word, on
    /// the very configuration the record names (a launchd agent with its own PATH and user).
    #[test]
    fn a_docker_that_cannot_be_launched_is_said_not_swallowed() {
        let seen = Seen::default();
        let stderr = format!("{LAUNCH_FAILED}permission denied (os error 13)");
        let r = runner(vec![Some((false, "", &stderr))], &seen);
        match sweep_with("jkb-dev", &[], &r) {
            Sweep::Unreachable(why) => {
                assert!(why.contains("could not be run"), "{why}");
                assert!(
                    why.contains("permission denied"),
                    "the OS reason survives: {why}"
                );
                assert!(
                    !why.contains('\u{0}'),
                    "the sentinel itself never reaches a human: {why}"
                );
            }
            other => panic!("wanted Unreachable, got {other:?}"),
        }
        assert_eq!(seen.borrow().len(), 1, "it must not exec through that");
    }

    #[test]
    fn a_wedged_daemon_is_said_rather_than_swallowed() {
        let seen = Seen::default();
        let r = runner(vec![Some((false, "", TIMED_OUT))], &seen);
        match sweep_with("jkb-dev", &[], &r) {
            Sweep::Unreachable(why) => assert!(why.contains("did not answer"), "{why}"),
            other => panic!("wanted Unreachable, got {other:?}"),
        }
        assert_eq!(
            seen.borrow().len(),
            1,
            "it must not exec through a daemon that is not answering"
        );
    }

    #[test]
    fn a_sweep_that_never_finishes_is_said_too() {
        let seen = Seen::default();
        let r = runner(
            vec![Some((true, "jkb-dev\n", "")), Some((false, "", TIMED_OUT))],
            &seen,
        );
        match sweep_with("jkb-dev", &[], &r) {
            Sweep::Unreachable(why) => assert!(why.contains("did not finish"), "{why}"),
            other => panic!("wanted Unreachable, got {other:?}"),
        }
    }

    /// THE SCRIPT GOES IN ON STDIN, and nothing is asked of the container's filesystem. The version
    /// this replaced exec'd `/usr/local/bin/sweep-transcripts.sh`, which only a REBUILT image
    /// carries — so on every already-running container it exited 127, was reported once into
    /// reap.log and deduped for ever, with every gate green.
    ///
    /// The expected argv is written out as its own literal rather than built from the production
    /// constants: a test that reads the same source production reads cannot fail when that source
    /// is wrong.
    #[test]
    fn the_sweep_is_fed_to_bash_on_stdin_and_asks_the_image_for_nothing() {
        let seen = Seen::default();
        let r = runner(
            vec![
                Some((true, "jkb-dev\n", "")),
                Some((
                    true,
                    "transcript sweep: 10 deny bytes projected, budget 65536 — nothing to archive\n",
                    "",
                )),
            ],
            &seen,
        );
        assert_eq!(sweep_with("jkb-dev", &[], &r), Sweep::Quiet);
        let calls = seen.borrow();
        assert_eq!(
            calls[0].0,
            vec!["ps", "--filter", "name=jkb-dev", "--format", "{{.Names}}"]
        );
        assert!(!calls[0].1, "the probe is asked nothing on stdin");
        assert_eq!(
            calls[1].0,
            vec![
                "exec",
                "-i",
                "-e",
                "JKB_KEEP_SESSIONS=",
                "jkb-dev",
                "bash",
                "-s"
            ]
        );
        assert!(calls[1].1, "the sweep must arrive on stdin");
    }

    /// The ids of every session this machine knows to be live are handed to the sweep, which never
    /// plans one — the protection `KEEP_NEWEST` stopped providing when the sweep gained a timer.
    #[test]
    fn live_sessions_are_named_to_the_sweep_so_it_cannot_plan_them() {
        let seen = Seen::default();
        let live = ["aaaa-1111".to_owned(), "bbbb-2222".to_owned()];
        let r = runner(
            vec![
                Some((true, "jkb-dev\n", "")),
                Some((true, "transcript sweep: 10 deny bytes projected, budget 65536 — nothing to archive\n", "")),
            ],
            &seen,
        );
        assert_eq!(sweep_with("jkb-dev", &live, &r), Sweep::Quiet);
        let calls = seen.borrow();
        assert_eq!(
            calls[1].0,
            vec![
                "exec",
                "-i",
                "-e",
                "JKB_KEEP_SESSIONS=aaaa-1111 bbbb-2222",
                "jkb-dev",
                "bash",
                "-s"
            ],
            "space-separated, in one variable, as the sweep splits them"
        );
        assert_eq!(KEEP_SESSIONS_VAR, "JKB_KEEP_SESSIONS");
    }

    /// The name is `run.sh`'s, honoured the way `run.sh` honours it — and the override is what
    /// stops `cargo test` reaching a real container, since the suite spawns `task reap --watch` as
    /// a real child with the developer's own environment.
    ///
    /// Driven by VALUES, not by `set_var`: this binary's other tests fork `git` concurrently, and a
    /// process-wide variable set mid-run is the hazard this crate wrote a rule about for itself.
    #[test]
    fn the_container_name_follows_run_sh_and_can_be_overridden() {
        let chosen = super::chosen_container_name;
        assert_eq!(chosen(None), "jkb-dev");
        assert_eq!(
            chosen(Some("jkb-somewhere-else".to_owned())),
            "jkb-somewhere-else"
        );
        // Empty is not a name. Set-but-blank would otherwise point every exec at "".
        assert_eq!(chosen(Some(String::new())), "jkb-dev");
        assert_eq!(chosen(Some("   ".to_owned())), "jkb-dev");
    }

    #[test]
    fn the_embedded_script_is_the_sweep_and_carries_its_own_dispatch() {
        assert!(
            SWEEP_SCRIPT.contains("sweep_transcripts()"),
            "the embedded script must be the sweep"
        );
        assert!(
            SWEEP_SCRIPT.contains("--self-test"),
            "…the whole of it, dispatch included, since bash -s reads one stream"
        );
        assert_eq!(DEV_CONTAINER_NAME, "jkb-dev");
    }

    #[test]
    fn a_sweep_that_archived_something_is_worth_a_line() {
        let seen = Seen::default();
        let r = runner(
            vec![
                Some((true, "jkb-dev\n", "")),
                Some((true, "transcript sweep: archived 7 file(s) to /x (70000 -> 62000 deny bytes, budget 65536)\n", "")),
            ],
            &seen,
        );
        match sweep_with("jkb-dev", &[], &r) {
            Sweep::Said(line) => assert!(line.contains("archived 7 file(s)"), "{line}"),
            other => panic!("wanted Said, got {other:?}"),
        }
    }

    /// The state the whole sweep exists to end. It must reach the log by its own words, not as a
    /// bare exit code — the operator has no other way to learn that Bash is about to stop working.
    #[test]
    fn an_over_budget_tree_is_a_failure_carrying_the_reason() {
        let seen = Seen::default();
        let r = runner(
            vec![
                Some((true, "jkb-dev\n", "")),
                Some((
                    false,
                    "transcript sweep: archived 0 file(s)\n",
                    "transcript sweep: 80092 deny bytes remain after this sweep, over the 65536 byte budget — Bash may still fail at spawn with E2BIG\n",
                )),
            ],
            &seen,
        );
        match sweep_with("jkb-dev", &[], &r) {
            Sweep::Failed(why) => assert!(why.contains("E2BIG"), "{why}"),
            other => panic!("wanted Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_with_a_silent_stderr_falls_back_to_stdout() {
        let seen = Seen::default();
        let r = runner(
            vec![
                Some((true, "jkb-dev\n", "")),
                Some((
                    false,
                    "transcript sweep: could not create /x — nothing archived\n",
                    "",
                )),
            ],
            &seen,
        );
        match sweep_with("jkb-dev", &[], &r) {
            Sweep::Failed(why) => assert!(why.contains("could not create"), "{why}"),
            other => panic!("wanted Failed, got {other:?}"),
        }
    }

    /// A failure that printed NOTHING must still say something: the reaper dedups on equality
    /// against a state that starts empty, so an empty message is suppressed for ever — on its first
    /// occurrence and every one after — in the one state worth reporting.
    #[test]
    fn a_failure_that_printed_nothing_still_reaches_the_log() {
        let seen = Seen::default();
        let r = runner(
            vec![Some((true, "jkb-dev\n", "")), Some((false, "", ""))],
            &seen,
        );
        match sweep_with("jkb-dev", &[], &r) {
            Sweep::Failed(why) => assert!(!why.is_empty(), "an empty failure is never reported"),
            other => panic!("wanted Failed, got {other:?}"),
        }
    }

    /// The phrases that decide Quiet are the SWEEP'S words. Written here as literals so this test
    /// fails if the constant changes; check-config.sh separately requires each to be text the sweep
    /// actually prints, which is the half that catches the sweep's end being reworded.
    #[test]
    fn nothing_to_do_is_recognised_by_the_sweeps_own_wording() {
        assert_eq!(
            NOTHING_TO_DO,
            ["nothing to archive", "no transcripts", "does not exist"]
        );
    }
}
