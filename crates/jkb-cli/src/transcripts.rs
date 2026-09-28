//! Telling the dev container to bound its own Bash-sandbox deny list.
//!
//! **Why this lives on the host.** Claude Code's Bash sandbox enumerates every session transcript
//! into one argv, Linux caps one argument at `MAX_ARG_STRLEN`, and a dev container over that limit
//! fails *every* Bash tool call at spawn with `E2BIG` — total, from the first call, with nothing in
//! the message naming transcripts (measured 2026-09-28; `.container/sweep-transcripts.sh` carries
//! the numbers). `.container/run.sh` sweeps on every container start, and that is the only trigger
//! there was: the container that produced the failure had reached 1,182 transcripts *without being
//! recreated*, because the documented workflow is `run.sh` once and then attach and keep working.
//! A start-only trigger bounds the deny list at a rate with nothing to do with the rate transcripts
//! are created.
//!
//! **Why the reaper.** It is the one long-lived thing on the host already sweeping on a timer for
//! this project, and this is the same kind of job: something only an outside process is placed to
//! do. The alternative considered and rejected was a second scheduler — a loop inside the container
//! or a timer unit beside this one — which is two schedulers to reason about for two sweeps.
//!
//! **Why `docker exec` and not a path.** `~/.claude-state` is a named Docker **volume**
//! (`jkb-claude-state`), not a host bind, so there is no host path to walk: the work can only happen
//! inside. And the reaper does not know where any checkout is — it knows a database path — so it
//! runs the copy the IMAGE carries at [`SWEEP_IN_IMAGE`] rather than one from a working tree. That
//! copy is root-owned, which is a small second gain: the host-triggered sweep runs a script the
//! agent inside the container cannot rewrite, the same argument the firewall script is installed by.
//!
//! **It is never fatal and usually silent.** A host with no Docker, or no such container, or a
//! stopped one, is the ordinary case for anyone not using the dev container, and it says nothing at
//! all. `.container/check-config.sh` holds [`DEV_CONTAINER_NAME`] to `run.sh`'s default and
//! [`SWEEP_IN_IMAGE`] to the Dockerfile's destination, so neither can drift from the container this
//! is aimed at.

use std::process::Command;

/// The dev container's name — `.container/run.sh` spells it `${JKB_CONTAINER_NAME:-jkb-dev}`.
///
/// Held to that default by `check-config.sh`, which reads both: a reaper poking a container name
/// nothing creates would be silent for ever, which is the failure this whole module exists to end.
pub const DEV_CONTAINER_NAME: &str = "jkb-dev";

/// Where `.container/Dockerfile` installs the sweep inside the image.
///
/// Also held to the Dockerfile by `check-config.sh`. Run by absolute path rather than by name, so
/// this does not depend on what `PATH` a non-interactive `docker exec` happens to get.
pub const SWEEP_IN_IMAGE: &str = "/usr/local/bin/sweep-transcripts.sh";

/// What one attempt came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sweep {
    /// No Docker, or no container of that name running. The ordinary case off the dev container,
    /// and nothing to report.
    Absent,
    /// It ran and had nothing to say, or said only that the tree is already under budget.
    Quiet,
    /// It ran and said something worth a line in the log.
    Said(String),
    /// It ran and reported a problem — the deny list is over budget, or it could not archive.
    Failed(String),
}

/// One command run: `(success, stdout, stderr)`, or `None` when the program could not be launched.
pub type Runner<'a> = &'a dyn Fn(&[&str]) -> Option<(bool, String, String)>;

/// Sweep the dev container's transcripts, driving Docker through `run`.
///
/// Separated from [`sweep_dev_container`] so the decisions here are testable without Docker: what
/// counts as absent, what counts as quiet, and what reaches the log.
pub fn sweep_with(name: &str, run: Runner<'_>) -> Sweep {
    // ASKED BEFORE POKED, so "there is no such container" is a fact rather than an error message
    // parsed out of a failed exec. `docker exec` against a missing container and against a broken
    // one both exit non-zero with prose, and telling those apart by their wording is a guess that
    // goes stale with the next Docker release.
    // `None` (no docker on this host) and a failed inspect (no such container) are one answer:
    // there is nothing here to sweep. Written as one arm because they are one fact, not two.
    let Some((true, running, _)) = run(&["inspect", "-f", "{{.State.Running}}", name]) else {
        return Sweep::Absent;
    };
    if running.trim() != "true" {
        return Sweep::Absent;
    }
    let Some((ok, out, err)) = run(&["exec", name, "bash", SWEEP_IN_IMAGE]) else {
        return Sweep::Absent;
    };
    // The sweep's verdict is the LAST line it printed, on either stream — it says several things on
    // the way to one. Its own `--self-test` and `.container/verify.sh` read it the same way.
    let last = |s: &str| {
        s.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .to_owned()
    };
    if !ok {
        let why = last(&err);
        return Sweep::Failed(if why.is_empty() { last(&out) } else { why });
    }
    let said = last(&out);
    // A start that archived nothing is the steady state — 96 times a day, for ever. The reaper's own
    // rule for its log applies: say what CHANGED, not that a timer fired.
    if said.is_empty() || said.contains("nothing to archive") || said.contains("no transcripts") {
        Sweep::Quiet
    } else {
        Sweep::Said(said)
    }
}

/// Sweep the dev container's transcripts, if there is one.
#[must_use]
pub fn sweep_dev_container(name: &str) -> Sweep {
    sweep_with(name, &|args| {
        let out = Command::new("docker").args(args).output().ok()?;
        Some((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::{sweep_with, Sweep, SWEEP_IN_IMAGE};
    use std::cell::RefCell;

    /// A runner that answers a fixed script of replies and records what it was asked.
    fn runner<'a>(
        replies: Vec<Option<(bool, &'static str, &'static str)>>,
        seen: &'a RefCell<Vec<Vec<String>>>,
    ) -> impl Fn(&[&str]) -> Option<(bool, String, String)> + 'a {
        let replies = RefCell::new(replies.into_iter());
        move |args: &[&str]| {
            seen.borrow_mut()
                .push(args.iter().map(|a| (*a).to_owned()).collect());
            replies
                .borrow_mut()
                .next()
                .flatten()
                .map(|(ok, o, e)| (ok, o.to_owned(), e.to_owned()))
        }
    }

    #[test]
    fn no_docker_at_all_is_absent_and_pokes_nothing_further() {
        let seen = RefCell::new(Vec::new());
        assert_eq!(
            sweep_with("jkb-dev", &runner(vec![None], &seen)),
            Sweep::Absent
        );
        assert_eq!(seen.borrow().len(), 1, "it must not try to exec after that");
    }

    #[test]
    fn a_container_that_is_not_running_is_absent() {
        let seen = RefCell::new(Vec::new());
        let r = runner(vec![Some((true, "false\n", ""))], &seen);
        assert_eq!(sweep_with("jkb-dev", &r), Sweep::Absent);
        assert_eq!(seen.borrow().len(), 1);
    }

    #[test]
    fn a_missing_container_is_absent_rather_than_a_failure() {
        let seen = RefCell::new(Vec::new());
        let r = runner(
            vec![Some((false, "", "Error: No such object: jkb-dev\n"))],
            &seen,
        );
        assert_eq!(sweep_with("jkb-dev", &r), Sweep::Absent);
    }

    /// The steady state, 96 times a day: it ran and there was nothing to do. A log that says so
    /// every quarter hour is a log nobody reads the rest of.
    #[test]
    fn a_sweep_with_nothing_to_do_says_nothing() {
        let seen = RefCell::new(Vec::new());
        let r = runner(
            vec![
                Some((true, "true\n", "")),
                Some((
                    true,
                    "transcript sweep: 4100 deny bytes projected, budget 65536 — nothing to archive\n",
                    "",
                )),
            ],
            &seen,
        );
        assert_eq!(sweep_with("jkb-dev", &r), Sweep::Quiet);
        assert_eq!(
            seen.borrow()[1],
            vec!["exec", "jkb-dev", "bash", SWEEP_IN_IMAGE],
            "the image's copy, by absolute path"
        );
    }

    #[test]
    fn a_sweep_that_archived_something_is_worth_a_line() {
        let seen = RefCell::new(Vec::new());
        let r = runner(
            vec![
                Some((true, "true\n", "")),
                Some((true, "transcript sweep: archived 7 file(s) to /x (70000 -> 62000 deny bytes, budget 65536)\n", "")),
            ],
            &seen,
        );
        match sweep_with("jkb-dev", &r) {
            Sweep::Said(line) => assert!(line.contains("archived 7 file(s)"), "{line}"),
            other => panic!("wanted Said, got {other:?}"),
        }
    }

    /// The state the whole sweep exists to end. It must reach the log by its own words, not as a
    /// bare exit code — the operator has no other way to learn that Bash is about to stop working.
    #[test]
    fn an_over_budget_tree_is_a_failure_carrying_the_reason() {
        let seen = RefCell::new(Vec::new());
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
        match sweep_with("jkb-dev", &r) {
            Sweep::Failed(why) => assert!(why.contains("E2BIG"), "{why}"),
            other => panic!("wanted Failed, got {other:?}"),
        }
    }

    /// A failure with nothing on stderr must still carry something: the empty-string report is the
    /// one an operator cannot act on at all.
    #[test]
    fn a_failure_with_a_silent_stderr_falls_back_to_stdout() {
        let seen = RefCell::new(Vec::new());
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
        match sweep_with("jkb-dev", &r) {
            Sweep::Failed(why) => assert!(why.contains("could not create"), "{why}"),
            other => panic!("wanted Failed, got {other:?}"),
        }
    }
}
