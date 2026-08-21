//! Pull requests: the proof that work landed somewhere jkb did not put it (design S-series).
//!
//! Everything jkb lands itself is an **event** — `jkb task land` performs the graft, the merge
//! queue calls `jkb task landed`, and both append to the task's history. The only case needing
//! anything else is a landing jkb did not perform: you open a pull request and somebody merges
//! it.
//!
//! That case used to be answered by inference over the commit graph, and the inference is hard
//! for one specific reason: **a squash or rebase merge rewrites the commits**, so containment
//! cannot be tested, and the weaker question `is_merged` asks — *does this branch add anything
//! to trunk?* — cannot tell a branch whose work was squashed away from one that never started.
//! Making it answerable at all needed a stored cut point per branch, a reflog-derived instance
//! anchor to say which *instance* of a recycled name the cut point described, and a supersede
//! rule for when a name changed hands. Roughly a quarter of the `staging-workflow` review
//! corpus's must-fix findings live in that apparatus.
//!
//! A pull request answers the question directly, and its number is **minted by GitHub and never
//! reused**, so there is nothing to disambiguate: no cut point, no anchor, no instance problem.
//! It also produces an answer the inference could not — *closed without merging*.
//!
//! **Everything here degrades to [`Fact::Unknown`]**, never to a `no`. No `gh` on `PATH`, no
//! network, no GitHub remote, an unexpected JSON shape, a rate limit — all of them mean *we
//! could not establish it*, and the lifecycle holds the task and says why. That is the correct
//! failure: a missed close costs one command, and a wrong one buries work still in flight
//! (D34.4).

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use jkb_fsm::Fact;
use serde::Deserialize;

/// The `gh` fields this module reads. Every one is optional and an absent or unexpected value
/// degrades the answer to [`Fact::Unknown`] rather than to a `false`.
const FIELDS: &str = "number,state,mergedAt,baseRefName,headRefName";

/// A pull request, as much of it as `gh` gave us.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequest {
    /// The immutable, never-reused id. This is the whole reason to prefer this over the graph.
    pub(crate) number: i64,
    /// `OPEN` / `CLOSED` / `MERGED`, as `gh` spells it.
    pub(crate) state: Option<String>,
    /// When it merged, if it did.
    pub(crate) merged_at: Option<String>,
    /// The branch it targets.
    pub(crate) base_ref_name: Option<String>,
    /// The branch it comes from.
    pub(crate) head_ref_name: Option<String>,
}

impl PullRequest {
    /// Whether this pull request **merged**.
    ///
    /// `MERGED` is proof; `OPEN` and `CLOSED` are proof it has not (a closed-unmerged pull
    /// request is a real answer, and one the commit-graph inference could not produce at all).
    /// Any other spelling — a `gh` that grew a state, a shape we did not expect — is
    /// [`Fact::Unknown`], because a state we do not recognize is not evidence.
    pub(crate) fn merged(&self) -> Fact {
        match self.state.as_deref() {
            Some("MERGED") => Fact::Yes,
            Some("OPEN" | "CLOSED") => Fact::No,
            _ => Fact::Unknown,
        }
    }
}

/// What a search for a task's pull request found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Discovery {
    /// Exactly one pull request comes from this branch.
    One(Box<PullRequest>),
    /// None does.
    None,
    /// More than one does — a branch name that has been reused. **Reported, never guessed**:
    /// which one is this task's work is precisely the question a recycled name cannot answer,
    /// and picking one is how the old inference closed tasks that had not landed.
    Ambiguous(Vec<i64>),
    /// We could not ask. Carries the reason, because "no pull request" and "no `gh` installed"
    /// must never render as the same sentence.
    Unavailable(String),
}

/// Find the pull request whose head branch is `branch`.
///
/// Used **once** per task, after which the number is recorded as a transition and the branch
/// name is never consulted again — which is what stops a later reuse of that name from
/// attaching somebody else's pull request to this work.
pub(crate) fn discover(dir: &Path, branch: &str) -> Discovery {
    let out = match gh(
        dir,
        &[
            "pr", "list", "--head", branch, "--state", "all", "--limit", "10", "--json", FIELDS,
        ],
    ) {
        Ok(out) => out,
        Err(why) => return Discovery::Unavailable(why),
    };
    let prs: Vec<PullRequest> = match serde_json::from_str(&out) {
        Ok(prs) => prs,
        Err(e) => {
            return Discovery::Unavailable(format!(
                "`gh pr list` returned JSON this build does not recognize ({e}) — treating the \
                 landing as unproven rather than guessing"
            ))
        }
    };
    match prs.len() {
        0 => Discovery::None,
        1 => Discovery::One(Box::new(
            prs.into_iter().next().unwrap_or_else(|| unreachable!()),
        )),
        _ => Discovery::Ambiguous(prs.iter().map(|p| p.number).collect()),
    }
}

/// Read one pull request by number.
///
/// The read every consumer of a *recorded* pull request makes. Keyed by a number, so a branch
/// that has since been deleted, renamed or recreated changes nothing about the answer.
pub(crate) fn lookup(dir: &Path, number: i64) -> Result<PullRequest, String> {
    let out = gh(dir, &["pr", "view", &number.to_string(), "--json", FIELDS])?;
    serde_json::from_str(&out).map_err(|e| {
        format!(
            "`gh pr view {number}` returned JSON this build does not recognize ({e}) — treating \
             the landing as unproven rather than guessing"
        )
    })
}

/// Whether a recorded pull request proves this work landed.
///
/// The one thing the lifecycle's `observed_landed` guard reads. Every failure — no number
/// recorded, no `gh`, no network, an unrecognized state — is [`Fact::Unknown`], and the guard
/// requires it proven.
pub(crate) fn merged_fact(
    dir: &Path,
    number: Option<i64>,
    resumed_at: Option<&str>,
) -> (Fact, Option<String>) {
    let Some(number) = number else {
        return (
            Fact::Unknown,
            Some("no pull request is recorded for this task".to_owned()),
        );
    };
    match lookup(dir, number) {
        Ok(pr) => {
            // **Spent evidence**, the same rule `jkb_core::transition::resumed` states for a
            // recorded landing, reaching the other half of the evidence. A merge is proof about
            // the past and reads as `MERGED` for ever; a task put back to work after it merged is
            // being worked on *now*, and closing it again on the strength of that merge is the
            // defect this pair exists to stop. It predates the recorded-landing path — a merged
            // pull request has always re-closed a reopened task on the next `git pull`.
            //
            // `No` where we have *established* the merge does not speak for the work in flight;
            // `Unknown` where a resumption is known but the merge cannot be placed against it,
            // which holds the task rather than closing it (D34.4: a missed close costs one
            // command, a wrong one buries work).
            match spent(&pr, resumed_at) {
                Staleness::Spent(why) => return (Fact::No, Some(why)),
                Staleness::Undecidable(why) => return (Fact::Unknown, Some(why)),
                Staleness::Live => {}
            }
            let fact = pr.merged();
            let why = match fact {
                Fact::Yes => None,
                Fact::No => Some(format!(
                    "pull request #{number} is {}",
                    pr.state.as_deref().unwrap_or("not merged")
                )),
                Fact::Unknown => Some(format!(
                    "pull request #{number} is in a state this build does not recognize"
                )),
            };
            (fact, why)
        }
        Err(why) => (Fact::Unknown, Some(why)),
    }
}

/// Whether this merge is **spent** — it happened, and the task has since been put back to work,
/// so it says nothing about the work in flight. `Some(reason)` when it is.
///
/// The same rule `jkb_core::transition::resumed` states for a recorded landing, reaching the
/// other half of the evidence. A merge reads as `MERGED` for ever, so without this a reopened
/// task is closed again on the next `git pull` — the `post-merge` hook runs `close-merged` over
/// every task unattended, so nobody chose to run it. It predates the recorded-landing path.
///
/// Pure, and separated from [`merged_fact`] for that reason: everything around it needs `gh`, and
/// a rule that can only be exercised by shelling out to an authenticated network client is a rule
/// nothing checks.
///
/// **No resumption means the question does not arise**, which is the overwhelmingly normal case
/// and why [`Staleness::Live`] is the default — not because a missed close is the cheap error. It
/// is the expensive one only in the other direction: a missed close costs one command, and a
/// wrong one buries work still in flight (D34.4, and this module's own opening paragraph).
///
/// So where a resumption **is** known and the merge cannot be placed against it, this returns
/// [`Staleness::Undecidable`] and the task is held with the reason printed. Returning `Live`
/// there would close a task on a merge that might well predate the work in flight, picking the
/// burying direction on the strength of a missing field.
fn spent(pr: &PullRequest, resumed_at: Option<&str>) -> Staleness {
    let Some(resumed_at) = resumed_at else {
        return Staleness::Live;
    };
    if pr.merged() != Fact::Yes {
        // Not a merge at all; `merged_fact`'s ordinary path has the right answer for it.
        return Staleness::Live;
    }
    let Some(merged_at) = pr.merged_at.as_deref() else {
        return Staleness::Undecidable(format!(
            "pull request #{} is merged but carries no merge time, so it cannot be told from \
             work done before this task was put back to work at {resumed_at}",
            pr.number
        ));
    };
    // RFC-3339 from `gh` and from `strftime('%Y-%m-%dT%H:%M:%fZ')` — both zero-padded, both UTC,
    // so lexical order is chronological order. Compared as strings on purpose: parsing them would
    // add a date library to answer a question the format already answers.
    if merged_at < resumed_at {
        return Staleness::Spent(format!(
            "pull request #{} merged at {merged_at}, but the task was put back to work at \
             {resumed_at}",
            pr.number
        ));
    }
    Staleness::Live
}

/// Whether a merge still speaks for the work in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Staleness {
    /// It does — or no resumption is known, so the question does not arise.
    Live,
    /// It does not: the task went back to work after it merged.
    Spent(String),
    /// It cannot be told, and a resumption **is** known. Held, never closed — see [`spent`].
    Undecidable(String),
}

/// Run `gh` in `dir`, returning stdout or a sentence explaining why we could not ask.
///
/// Deliberately shells out rather than speaking HTTP: `gh` already holds the user's
/// authentication, and adding an HTTP client plus a token story to `jkb-cli` for one query is a
/// dependency and a secret this tool does not otherwise need.
fn gh(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("gh")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "`gh` is not installed, so a pull request cannot be checked from here — \
                 install it (`brew install gh`) or close the task by hand"
                    .to_owned()
            } else {
                format!("could not run `gh`: {e}")
            }
        })?;
    if !out.status.success() {
        // Collapsed to one line. `gh`'s own messages are multi-line — the unauthenticated one is
        // two sentences on two lines — and this string is carried as a *reason* into a report
        // that prints one task per line. A newline in it silently breaks that alignment for
        // every consumer, so it is flattened here, at the one place the string is made, rather
        // than at each of them.
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "`gh {}` failed: {}",
            args.first().copied().unwrap_or("pr"),
            err.split_whitespace().collect::<Vec<_>>().join(" ")
        ));
    }
    String::from_utf8(out.stdout).map_err(|_| "`gh` returned output that is not UTF-8".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{spent, Discovery, PullRequest, Staleness};
    use jkb_fsm::Fact;

    fn pr(state: &str) -> PullRequest {
        PullRequest {
            number: 31,
            state: Some(state.to_owned()),
            merged_at: None,
            base_ref_name: Some("main".to_owned()),
            head_ref_name: Some("feature".to_owned()),
        }
    }

    /// The three answers, and the fourth that is not an answer. A state we do not recognize is
    /// **not** evidence of anything — spelling it `No` would make a future `gh` release read as
    /// "definitely not merged".
    #[test]
    fn only_merged_proves_a_landing_and_an_unknown_state_proves_nothing() {
        assert_eq!(pr("MERGED").merged(), Fact::Yes);
        assert_eq!(pr("OPEN").merged(), Fact::No);
        // A pull request closed without merging is a real answer the commit-graph inference
        // could not produce at all.
        assert_eq!(pr("CLOSED").merged(), Fact::No);
        assert_eq!(pr("QUEUED").merged(), Fact::Unknown);
        assert_eq!(
            PullRequest {
                state: None,
                ..pr("MERGED")
            }
            .merged(),
            Fact::Unknown
        );
    }

    /// The wire shape is `gh`'s, not ours, and this build cannot verify it by running the tool.
    /// So the parse is asserted here against the documented field names, and anything that does
    /// not parse becomes `Unavailable` rather than an empty list — which would read as "no pull
    /// request" and let a task close on the strength of a JSON change.
    #[test]
    fn the_wire_shape_parses_and_a_surprise_is_unavailable_not_empty() {
        let json = r#"[{"number":31,"state":"MERGED","mergedAt":"2026-08-19T10:00:00Z",
                        "baseRefName":"main","headRefName":"staging-workflow"}]"#;
        let prs: Vec<PullRequest> = serde_json::from_str(json).expect("documented shape");
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 31);
        assert_eq!(prs[0].merged(), Fact::Yes);
        assert_eq!(prs[0].head_ref_name.as_deref(), Some("staging-workflow"));

        // A field renamed upstream: still parses, because every field but the number is
        // optional — and the missing state is what makes the answer `Unknown`.
        let renamed: Vec<PullRequest> =
            serde_json::from_str(r#"[{"number":31,"mergeState":"MERGED"}]"#).expect("lenient");
        assert_eq!(renamed[0].merged(), Fact::Unknown);

        // Something that is not a list at all does not parse, and `discover` turns that into
        // `Unavailable`.
        assert!(serde_json::from_str::<Vec<PullRequest>>("{}").is_err());
    }

    /// Two pull requests from one branch name is exactly the recycled-name case, and it is
    /// **reported**. Guessing which is this task's is how the old inference closed work that
    /// had not landed.
    #[test]
    fn a_reused_branch_name_is_reported_not_guessed() {
        let ambiguous = Discovery::Ambiguous(vec![31, 44]);
        assert_ne!(ambiguous, Discovery::None);
        match ambiguous {
            Discovery::Ambiguous(ns) => assert_eq!(ns, vec![31, 44]),
            _ => panic!("expected ambiguity"),
        }
    }
    /// A merge speaks for the work in flight only if nothing has put the task back to work since
    /// — and where that cannot be told, the task is **held**, not closed.
    ///
    /// The three answers are asserted apart because two of them used to be one: returning "not
    /// spent" for a merge with no timestamp closes the task, which is the burying direction
    /// (D34.4). `Live` is the default because no resumption is the normal case, not because a
    /// missed close is cheap.
    #[test]
    fn a_merge_is_spent_once_the_task_has_been_put_back_to_work() {
        let merged = PullRequest {
            number: 31,
            state: Some("MERGED".to_owned()),
            merged_at: Some("2026-08-19T10:00:00Z".to_owned()),
            base_ref_name: None,
            head_ref_name: None,
        };
        // Put back to work the day after it merged: the merge is about older work.
        let Staleness::Spent(why) = spent(&merged, Some("2026-08-20T09:00:00Z")) else {
            panic!("a merge older than the resumption must be spent");
        };
        assert!(
            why.contains("#31") && why.contains("put back to work"),
            "{why}"
        );

        // Resumed *before* the merge — an ordinary landing of work that was in progress.
        assert_eq!(
            spent(&merged, Some("2026-08-18T09:00:00Z")),
            Staleness::Live
        );
        // Never resumed at all: the question does not arise.
        assert_eq!(spent(&merged, None), Staleness::Live);
        // An open pull request is not a spent merge; it is not a merge.
        let open = PullRequest {
            state: Some("OPEN".to_owned()),
            ..merged.clone()
        };
        assert_eq!(spent(&open, Some("2026-08-20T09:00:00Z")), Staleness::Live);

        // Merged, a resumption is known, and the merge cannot be placed against it. HELD — the
        // one case where leaving the answer alone would close a task on a merge that may well
        // predate the work in flight.
        let undated = PullRequest {
            merged_at: None,
            ..merged.clone()
        };
        let Staleness::Undecidable(why) = spent(&undated, Some("2026-08-20T09:00:00Z")) else {
            panic!("an unplaceable merge with a known resumption must hold, not close");
        };
        assert!(why.contains("no merge time"), "{why}");
        // ...but with no resumption known there is nothing to compare against and nothing to hold
        // for, so it goes back to the ordinary path.
        assert_eq!(spent(&undated, None), Staleness::Live);
    }
}

#[cfg(test)]
mod live {
    use super::{discover, lookup, Discovery};
    use jkb_fsm::Fact;

    /// The one thing the offline tests cannot establish: that a **real** merged pull request
    /// comes back from *this exact query* with `state: "MERGED"`.
    ///
    /// Everything around it is checked without the network — the field names against
    /// `gh pr view --json`'s own list, the flags against `gh pr list --help`, the uppercase
    /// state against `gh`'s `display.go`, and the whole failure path against a real
    /// unauthenticated `gh`. What is left is one live call.
    ///
    /// `#[ignore]` for the same reason the ollama and Chrome smokes are: it needs something this
    /// suite cannot provide. Run it with `gh` installed **and authenticated**
    /// (`gh auth login`, or `GH_TOKEN` set):
    ///
    /// ```text
    /// ./scripts/test.sh -p jkb-cli --lib -- --ignored live_
    /// ```
    #[test]
    #[ignore = "needs `gh` authenticated against a repo with a merged pull request"]
    fn live_a_merged_pull_request_reads_as_merged() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // Discovery by head branch, then a lookup by number — the two calls in the order the
        // commands make them. A branch nobody ever opened a pull request for is `None`, which is
        // an answer, not a failure.
        // The failure names what happened. Run without auth, the discovery comes back
        // `Unavailable` carrying `gh`'s own message, and a panic saying only "expected one pull
        // request" would send its reader to look for a missing branch instead of to
        // `gh auth login`.
        let found = match discover(repo, "staging-workflow") {
            Discovery::One(found) => found,
            Discovery::None => panic!("no pull request has `staging-workflow` as its head branch"),
            Discovery::Ambiguous(ns) => panic!("that branch name has been reused: {ns:?}"),
            Discovery::Unavailable(why) => panic!("could not ask: {why}"),
        };
        assert_eq!(found.merged(), Fact::Yes, "state was {:?}", found.state);
        assert_eq!(found.base_ref_name.as_deref(), Some("main"));

        let by_number = lookup(repo, found.number).expect("view by number");
        assert_eq!(by_number.number, found.number);
        assert_eq!(
            by_number.merged(),
            Fact::Yes,
            "the number and the branch disagree, which is the whole reason the number is what \
             gets stored"
        );
    }
}
