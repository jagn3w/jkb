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

/// One command run: `(success, stdout, stderr)`, or `None` when the program could not be launched.
///
/// A run that hit [`DOCKER_TIMEOUT`] answers with [`TIMED_OUT`] as its stderr, which is a sentinel
/// rather than a message so no caller has to match Docker's wording for it.
pub type Runner<'a> = &'a dyn Fn(&[&str], Option<&str>) -> Option<(bool, String, String)>;

/// The stderr a [`Runner`] reports when its command ran out of time.
pub const TIMED_OUT: &str = "\u{0}jkb:timed-out";

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
const NOTHING_TO_DO: [&str; 2] = ["nothing to archive", "no transcripts"];

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
    // `None` (no docker on this host) and a failed inspect (no such container) are one answer:
    // there is nothing here to sweep. A TIMEOUT is not one of them.
    let inspect = run(&["inspect", "-f", "{{.State.Running}}", name], None);
    if matches!(&inspect, Some((_, _, err)) if err == TIMED_OUT) {
        return Sweep::Unreachable(format!("docker inspect {name} did not answer"));
    }
    let Some((true, running, _)) = inspect else {
        return Sweep::Absent;
    };
    if running.trim() != "true" {
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
        return Sweep::Absent;
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
        let mut child = Command::new("docker")
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        // WRITTEN FROM A THREAD, because the script is larger than a pipe buffer (74KB against
        // 64KB) and `bash -s` executes as it reads: a blocking write from here would deadlock the
        // moment the child stopped reading to run a `find`. The handle is moved in, so the pipe is
        // closed when the thread ends and `bash` sees EOF.
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
        let deadline = Instant::now() + DOCKER_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
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
        let out = child.wait_with_output().ok()?;
        Some((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        sweep_with, Sweep, DEV_CONTAINER_NAME, KEEP_SESSIONS_VAR, NOTHING_TO_DO, SWEEP_SCRIPT,
        TIMED_OUT,
    };
    use std::cell::RefCell;

    type Reply = Option<(bool, &'static str, &'static str)>;
    type Seen = RefCell<Vec<(Vec<String>, bool)>>;

    /// A runner that answers a fixed script of replies and records what it was asked, and whether
    /// anything was handed to the command on stdin.
    fn runner(
        replies: Vec<Reply>,
        seen: &Seen,
    ) -> impl Fn(&[&str], Option<&str>) -> Option<(bool, String, String)> + '_ {
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

    #[test]
    fn no_docker_at_all_is_absent_and_pokes_nothing_further() {
        let seen = Seen::default();
        assert_eq!(
            sweep_with("jkb-dev", &[], &runner(vec![None], &seen)),
            Sweep::Absent
        );
        assert_eq!(seen.borrow().len(), 1, "it must not try to exec after that");
    }

    #[test]
    fn a_container_that_is_not_running_is_absent() {
        let seen = Seen::default();
        let r = runner(vec![Some((true, "false\n", ""))], &seen);
        assert_eq!(sweep_with("jkb-dev", &[], &r), Sweep::Absent);
        assert_eq!(seen.borrow().len(), 1);
    }

    #[test]
    fn a_missing_container_is_absent_rather_than_a_failure() {
        let seen = Seen::default();
        let r = runner(
            vec![Some((false, "", "Error: No such object: jkb-dev\n"))],
            &seen,
        );
        assert_eq!(sweep_with("jkb-dev", &[], &r), Sweep::Absent);
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
            vec![Some((true, "true\n", "")), Some((false, "", TIMED_OUT))],
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
                Some((true, "true\n", "")),
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
            vec!["inspect", "-f", "{{.State.Running}}", "jkb-dev"]
        );
        assert!(!calls[0].1, "inspect is asked nothing on stdin");
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
                Some((true, "true\n", "")),
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
                Some((true, "true\n", "")),
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
                Some((true, "true\n", "")),
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
                Some((true, "true\n", "")),
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
            vec![Some((true, "true\n", "")), Some((false, "", ""))],
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
        assert_eq!(NOTHING_TO_DO, ["nothing to archive", "no transcripts"]);
    }
}
