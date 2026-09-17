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
            Response::Uids { uids } => Ok(uids),
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
        uid: &str,
        merged: Fact,
        pr: Option<i64>,
        dry_run: bool,
    ) -> Result<Option<String>> {
        match self.call(Request::TaskCloseMerged {
            uid: uid.to_owned(),
            merged: Merged::from(merged),
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
        anyhow::bail!(
            "this task records no branch, so there is nothing to look a pull request up by — \
             pass the number: `jkb task pr <uid> <number>`"
        );
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
    for uid in kb.open_in_repo(&repo)? {
        verdicts.push(close_one(kb, &ctx.root, &uid, dry_run)?);
    }
    report(&verdicts, dry_run, json);
    Ok(())
}

/// Decide one task, and close it if a merged pull request — or a landing jkb recorded — proves it
/// landed. The decision is the lifecycle's (`observed_landed`, whose guard requires the merge
/// **proven** and no open subtasks); this gathers the facts.
fn close_one(kb: &Kb<'_>, root: &Path, uid: &str, dry_run: bool) -> Result<CloseVerdict> {
    let facts = kb.pr_facts(uid)?;
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
                Ok(found) => found,
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
    let refusal = kb.close_merged(&facts.uid, merged, number, dry_run)?;
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
    let Some(branch) = &facts.branch else {
        return Ok(Err(
            "no branch recorded, so there is no pull request to look up — \
             `jkb task pr <uid> <number>` to name one"
                .to_owned(),
        ));
    };
    Ok(match pr::discover(root, branch) {
        pr::Discovery::One(found) => {
            kb.pr_record(&facts.uid, found.number)?;
            Ok(Some(found.number))
        }
        pr::Discovery::None => Err(format!("no pull request has `{branch}` as its head branch")),
        pr::Discovery::Ambiguous(numbers) => Err(format!(
            "`{branch}` is the head branch of {} pull requests — pick one with \
             `jkb task pr <uid> <number>`",
            numbers.len()
        )),
        pr::Discovery::Unavailable(why) => Err(why),
    })
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
