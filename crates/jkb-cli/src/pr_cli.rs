//! `jkb task pr` and `jkb task close-merged` as clients of the ops (tasks S6.4 stage 5): the pull request
//! is looked up here, with `gh` and the checkout, and the history is read and written through
//! `task.pr_facts`, `task.pr_record` and `task.close_merged`.

use std::path::Path;

use anyhow::Result;
use jkb_api::prs::{Merged, PrFacts};
use jkb_api::{Request, Response};
use jkb_fsm::Fact;

use crate::ops_cli::unexpected;
use crate::session_cli::Kb;
use crate::{pr, repo};

impl Kb<'_> {
    /// `task.pr_facts`.
    pub(crate) fn pr_facts(&self, uid: &str) -> Result<PrFacts> {
        match self.call(Request::TaskPrFacts {
            uid: uid.to_owned(),
        })? {
            Response::PrFacts { facts } => Ok(facts),
            other => unexpected("task.pr_facts", &other),
        }
    }

    /// `task.open_in_repo`.
    fn open_in_repo(&self, repo: &str) -> Result<Vec<String>> {
        match self.call(Request::TaskOpenInRepo {
            repo: repo.to_owned(),
        })? {
            Response::Uids { uids, truncated } => {
                anyhow::ensure!(
                    !truncated,
                    "{repo} has more unfinished tasks than one answer through `jkb serve` carries; \
                     run `jkb task close-merged` on the host"
                );
                Ok(uids)
            }
            other => unexpected("task.open_in_repo", &other),
        }
    }

    /// `task.pr_record`.
    fn pr_record(&self, uid: &str, number: i64) -> Result<()> {
        match self.call(Request::TaskPrRecord {
            uid: uid.to_owned(),
            number,
        })? {
            Response::Applied {} => Ok(()),
            other => unexpected("task.pr_record", &other),
        }
    }

    /// `task.close_merged`: why the task was held, if it was.
    fn close_merged(
        &self,
        facts: &PrFacts,
        merged: Fact,
        pr: Option<i64>,
        dry_run: bool,
    ) -> Result<Option<String>> {
        match self.call(Request::TaskCloseMerged {
            uid: facts.uid.clone(),
            merged: Merged::from(merged),
            observed: jkb_api::prs::Observed {
                live_landing: facts.live_landing,
                resumed_at: facts.resumed_at.clone(),
                pr: facts.pr,
            },
            pr,
            dry_run,
        })? {
            Response::Closed { refusal } => Ok(refusal),
            other => unexpected("task.close_merged", &other),
        }
    }
}

/// `task pr <uid> [number]` — show, or record, the pull request that proves this work landed.
///
/// With a number, records it. Without, discovers it from the task's recorded branch and records what
/// it finds — **once**. After that the number is what is consulted, and the branch name never is: a
/// number is minted by GitHub and never reused, so a branch deleted, renamed or reused afterwards
/// cannot change the answer.
///
/// # Errors
/// An unknown task, a failed lookup, or the op's refusal.
pub(crate) fn pr(kb: &Kb<'_>, uid: &str, number: Option<i64>, json: bool) -> Result<()> {
    let facts = kb.pr_facts(uid)?;
    let number = match number {
        Some(n) => Some(n),
        None if facts.pr.is_some() => facts.pr,
        None => discover(&facts)?,
    };
    let Some(number) = number else {
        if json {
            println!("{}", serde_json::json!({"uid": uid, "pr": null}));
        }
        return Ok(());
    };
    if facts.pr != Some(number) {
        kb.pr_record(&facts.uid, number)?;
    }
    let ctx = repo::repo_ctx().ok();
    let (merged, why) = ctx.as_ref().map_or_else(
        || (Fact::Unknown, Some("not in a git repository".to_owned())),
        // `None`: this verb reports a fact about the pull request — *did it merge* — and is not
        // deciding whether to close anything.
        |c| pr::merged_fact(&c.root, Some(number), None),
    );
    if json {
        println!(
            "{}",
            serde_json::json!({"uid": uid, "pr": number, "merged": merged.as_str(), "why": why})
        );
    } else {
        println!(
            "{uid}: pull request #{number} — merged: {}",
            merged.as_str()
        );
        if let Some(why) = why {
            println!("  {why}");
        }
    }
    Ok(())
}

/// Find the pull request for a task's recorded branch, refusing to guess when a reused branch name
/// matches more than one.
fn discover(facts: &PrFacts) -> Result<Option<i64>> {
    let Ok(ctx) = repo::repo_ctx() else {
        anyhow::bail!(
            "not in a git repository, so there is no branch to look a pull request up by"
        );
    };
    let Some(branch) = &facts.branch else {
        anyhow::bail!("{}", no_branch_reason(facts));
    };
    match pr::discover(&ctx.root, branch) {
        pr::Discovery::One(found) => Ok(Some(found.number)),
        pr::Discovery::None => {
            println!("no pull request has `{branch}` as its head branch");
            Ok(None)
        }
        // The recycled-name case, reported rather than guessed.
        pr::Discovery::Ambiguous(numbers) => anyhow::bail!(
            "`{branch}` is the head branch of more than one pull request ({}) — that branch name \
             has been reused, so which one is this task's work is not something to guess. Pass \
             the number: `jkb task pr <uid> <number>`",
            numbers
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        pr::Discovery::Unavailable(why) => anyhow::bail!(
            "{why}\n  ...or name it directly: `jkb task pr <uid> <number>`, which needs no `gh`."
        ),
    }
}

/// One task's verdict in a `close-merged` run.
struct CloseVerdict {
    uid: String,
    /// The pull request consulted, when there was one.
    pr: Option<i64>,
    /// Why it was **not** closed, or `None` if it was.
    held: Option<String>,
}

/// `task close-merged`: close every task in this repo whose pull request has merged.
///
/// **A lookup, not an inference.** A pull request number is minted by GitHub and never reused, so there
/// is nothing to disambiguate. It closes nothing it cannot prove: no number recorded, no `gh`, no
/// network, a branch name that matches two pull requests — every one of those is [`Fact::Unknown`],
/// the lifecycle holds the task, and the reason is printed (design D34.4).
///
/// # Errors
/// Errors if the repo cannot be resolved or an op fails.
pub(crate) fn close_merged(
    kb: &Kb<'_>,
    repo: Option<String>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let ctx = repo::repo_ctx().map_err(|e| anyhow::anyhow!("{e}"))?;
    let repo = repo.unwrap_or_else(|| ctx.key.clone());
    // **Refused when `--repo` names somewhere else.** Pull request numbers are per-repository and low
    // ones collide by construction, so resolving another repo's task against *this* checkout closes on
    // an unrelated merge.
    anyhow::ensure!(
        repo == ctx.key,
        "`--repo {repo}` names a different repository from this checkout ({}), and pull request \
         numbers are per-repository — asking `gh` here would resolve {}'s numbers against {}'s. \
         Run it from {repo}'s checkout.",
        ctx.key,
        repo,
        ctx.key,
    );
    // Finished tasks are left out by the op: the `post-merge` hook runs this on every `git pull`.
    let mut verdicts = Vec::new();
    let mut failed = 0;
    for uid in kb.open_in_repo(&repo)? {
        // One task's failure — deleted while `gh` ran for the others, say — is that task's verdict,
        // and the rest still close. The run still fails at the end, so a failure that was the run's
        // (the daemon gone) is not reported as a quiet hold.
        let verdict = close_one(kb, &ctx.root, &uid, dry_run).unwrap_or_else(|e| {
            failed += 1;
            CloseVerdict {
                uid: uid.clone(),
                pr: None,
                held: Some(format!("could not be decided: {e:#}")),
            }
        });
        verdicts.push(verdict);
    }
    report(&verdicts, dry_run, json);
    anyhow::ensure!(
        failed == 0,
        "{failed} task(s) could not be decided (listed above); the rest were"
    );
    Ok(())
}

/// Decide one task, and close it if a merged pull request — or a landing jkb recorded — proves it
/// landed. The decision is the lifecycle's (`observed_landed`, whose guard requires the merge
/// **proven** and no open subtasks); this gathers the facts.
fn close_one(kb: &Kb<'_>, root: &Path, uid: &str, dry_run: bool) -> Result<CloseVerdict> {
    let mut facts = kb.pr_facts(uid)?;
    // Held, not failed: one task this client may not write must not stop the rest closing.
    if !facts.writable {
        return Ok(CloseVerdict {
            uid: facts.uid,
            pr: facts.pr,
            held: Some(
                "filed outside the directories this client may write; run `jkb task close-merged` \
                 on the host"
                    .to_owned(),
            ),
        });
    }
    // **A superseded landing is context, never a verdict**: it says the local graft is stale, not
    // whether the work reached its destination another way, so it only colours a hold's reason.
    let superseded = facts.superseded.as_ref().map(|(onto, event, at)| {
        format!(
            "its earlier landing onto {} was superseded when the task went back to work ({event} at \
             {at})",
            onto.as_deref().unwrap_or("its target"),
        )
    });
    let with_context = |why: String| match &superseded {
        Some(note) => format!("{why}; {note}"),
        None => why,
    };

    // **A landing jkb itself recorded is asked about first**: when the merge queue grafted locally it
    // is the only evidence there is, and no pull request number is credited for it.
    let (number, merged, why) = if facts.live_landing {
        (None, Fact::Yes, None)
    } else {
        // Discover once, from the branch, and record what is found.
        let number = match facts.pr {
            Some(n) => Some(n),
            None => match discover_quietly(kb, root, &facts)? {
                Ok(found) => {
                    // Recorded just now, so the history the op compares against says so.
                    facts.pr = found;
                    found
                }
                Err(why) => {
                    return Ok(CloseVerdict {
                        uid: facts.uid,
                        pr: None,
                        held: Some(with_context(why)),
                    })
                }
            },
        };
        // A merge older than the last resumption is not proof about the work in flight.
        let (merged, why) = pr::merged_fact(root, number, facts.resumed_at.as_deref());
        (number, merged, why.map(with_context))
    };
    // The status is read in the op's transaction, so a task cancelled while `gh` ran is not closed.
    let refusal = kb.close_merged(&facts, merged, number, dry_run)?;
    let held = refusal.map(|r| match &why {
        Some(w) => format!("{r} ({w})"),
        None => r,
    });
    Ok(CloseVerdict {
        uid: facts.uid,
        pr: number,
        held,
    })
}

/// Try to find a task's pull request from its recorded branch, without failing the run: `Err` is a
/// *reason to hold this task*, because one task with no branch, an ambiguous branch name or no `gh`
/// must not stop the rest from closing.
fn discover_quietly(
    kb: &Kb<'_>,
    root: &Path,
    facts: &PrFacts,
) -> Result<Result<Option<i64>, String>> {
    Ok(
        match quiet_verdict(facts, |branch| pr::discover(root, branch)) {
            Ok(number) => {
                kb.pr_record(&facts.uid, number)?;
                Ok(Some(number))
            }
            Err(why) => Err(why),
        },
    )
}

/// What a branch lookup means for `close-merged`: the one pull request's number, or the reason
/// printed on the task's line of the hold report. Pure apart from `look`, so every reason it can
/// render is reachable from a test.
fn quiet_verdict(facts: &PrFacts, look: impl FnOnce(&str) -> pr::Discovery) -> Result<i64, String> {
    let Some(branch) = facts.branch.as_deref() else {
        return Err(no_branch_reason(facts));
    };
    match look(branch) {
        pr::Discovery::One(found) => Ok(found.number),
        pr::Discovery::None => Err(format!("no pull request has `{branch}` as its head branch")),
        pr::Discovery::Ambiguous(numbers) => Err(format!(
            "`{branch}` is the head branch of {} pull requests — pick one with \
             `jkb task pr <uid> <number>`",
            numbers.len()
        )),
        pr::Discovery::Unavailable(why) => Err(why),
    }
}

/// Why a task whose history names no branch has no pull request to look up — and the remedy for
/// the cause it actually has, which is one of two.
///
/// One sentence used to serve both, and named the wrong one for every task started before the
/// transition log existed: those carry a `branch=` facet, so "no branch recorded" was false, and
/// its remedy (`jkb task pr`) did not touch the cause. What such a task lacks is a session in its
/// history saying where that branch lands, which `jkb task work --onto` records. A hold whose
/// reason misidentifies the cause sends its reader to a verb that will not help, which is worse
/// than a vague one. Shared by `task pr` and `close-merged`, so the two cannot drift apart again.
fn no_branch_reason(facts: &PrFacts) -> String {
    let uid = &facts.uid;
    match &facts.branch_facet {
        None => format!(
            "this task records no branch, so there is no pull request to look up — name one with \
             `jkb task pr {uid} <number>`"
        ),
        Some(branch) => format!(
            "this task's `branch=` facet says it is on `{branch}`, but no session in its history \
             names that branch or where it lands (it was started before jkb recorded sessions as \
             transitions), so there is no branch to look a pull request up by — record where it \
             lands with `jkb task work {uid} --onto <land target>`"
        ),
    }
}

/// Print what a `close-merged` run decided: closed, and held with the guard's reason.
fn report(verdicts: &[CloseVerdict], dry_run: bool, json: bool) {
    let (closed, held): (Vec<_>, Vec<_>) = verdicts.iter().partition(|v| v.held.is_none());
    if json {
        println!(
            "{}",
            serde_json::json!({
                "dry_run": dry_run,
                "closed": closed.iter().map(|v| serde_json::json!({"uid": v.uid, "pr": v.pr}))
                    .collect::<Vec<_>>(),
                "held": held.iter().map(|v| serde_json::json!({
                    "uid": v.uid, "pr": v.pr, "reason": v.held
                })).collect::<Vec<_>>(),
            })
        );
        return;
    }
    let verb = if dry_run { "would close" } else { "closed" };
    println!("{verb} {} task(s)", closed.len());
    for v in &closed {
        match v.pr {
            Some(n) => println!("  {} (pull request #{n})", v.uid),
            None => println!("  {}", v.uid),
        }
    }
    if !held.is_empty() {
        println!("held {}:", held.len());
        for v in &held {
            println!("  {} — {}", v.uid, v.held.as_deref().unwrap_or(""));
        }
    }
}

#[cfg(test)]
mod tests {
    use jkb_api::prs::PrFacts;

    use super::quiet_verdict;
    use crate::pr::{Discovery, PullRequest};

    /// One of each [`Discovery`] outcome. The `match` is exhaustive with no wildcard, so a variant
    /// added later fails to compile here until it is given a sample — the set stays closed.
    fn every_discovery() -> Vec<Discovery> {
        let one = Discovery::One(Box::new(PullRequest {
            number: 31,
            state: Some("MERGED".to_owned()),
            merged_at: None,
            base_ref_name: Some("main".to_owned()),
            head_ref_name: Some("task/x".to_owned()),
        }));
        let all = vec![
            one,
            Discovery::None,
            Discovery::Ambiguous(vec![31, 44]),
            Discovery::Unavailable("`gh` is not installed".to_owned()),
        ];
        for d in &all {
            match d {
                Discovery::One(_)
                | Discovery::None
                | Discovery::Ambiguous(_)
                | Discovery::Unavailable(_) => {}
            }
        }
        all
    }

    /// The task facts a lookup sees: `branch` from the session history, `branch_facet` from the
    /// task's `branch=` tag.
    fn facts(branch: Option<&str>, branch_facet: Option<&str>) -> PrFacts {
        PrFacts {
            uid: "task:x".to_owned(),
            pr: None,
            branch: branch.map(str::to_owned),
            branch_facet: branch_facet.map(str::to_owned),
            live_landing: false,
            superseded: None,
            resumed_at: None,
            writable: true,
        }
    }

    /// Every hold reason `close-merged` can print for a branch lookup reads as one sentence.
    ///
    /// Two of these once rendered fourteen spaces mid-sentence: a `\`-continued literal whose
    /// continuation had been lost, leaving the next line's indentation inside the string — the
    /// defect `archive.rs`'s `every_remedy_reads_as_a_sentence` catches for `Remedy::advice`.
    /// Asserted over the closed set — both no-branch causes, then every [`Discovery`] outcome —
    /// on the run of whitespace the defect produces, not on a substring it leaves intact.
    #[test]
    fn every_hold_reason_reads_as_a_sentence() {
        let no_lookup = |_: &str| -> Discovery { unreachable!("no branch, no lookup") };
        let mut reasons = vec![
            quiet_verdict(&facts(None, None), no_lookup),
            quiet_verdict(&facts(None, Some("task/x")), no_lookup),
        ];
        for d in every_discovery() {
            reasons.push(quiet_verdict(&facts(Some("task/x"), None), move |_| d));
        }
        let mut held = 0;
        for reason in reasons {
            let Err(reason) = reason else { continue };
            held += 1;
            assert!(!reason.trim().is_empty(), "a hold says nothing");
            assert!(
                !reason.contains("  "),
                "a hold reason renders a run of whitespace, so a line continuation was lost: \
                 {reason:?}"
            );
            assert!(
                !reason.contains('\n'),
                "a hold reason spans lines, which the one-task-per-line report cannot show: \
                 {reason:?}"
            );
        }
        assert_eq!(
            held, 5,
            "no branch (with and without a facet), none, ambiguous and unavailable each hold the task"
        );
    }

    /// Exactly one pull request is the only outcome that does not hold the task.
    #[test]
    fn one_pull_request_is_found_not_held() {
        let found = every_discovery()
            .into_iter()
            .map(|d| quiet_verdict(&facts(Some("task/x"), None), move |_| d))
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        assert_eq!(found, vec![31]);
    }
}
