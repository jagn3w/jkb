//! `jkb task pr` and `jkb task close-merged` as clients of the ops (tasks S6.4 stage 5): the pull request
//! is looked up here, with `gh` and the checkout, and the history is read and written through
//! `task.pr_facts`, `task.pr_record` and `task.close_merged`.

use std::path::Path;

use anyhow::Result;
use jkb_api::prs::{Merged, PrAbsent, PrFacts};
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

    /// `task.pr_absent`.
    fn pr_absent(&self, uid: &str, branch: &str, remote_head: Option<&str>) -> Result<()> {
        match self.call(Request::TaskPrAbsent {
            uid: uid.to_owned(),
            branch: branch.to_owned(),
            remote_head: remote_head.map(str::to_owned),
        })? {
            Response::Applied {} => Ok(()),
            other => unexpected("task.pr_absent", &other),
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
        None => discover(kb, &facts)?,
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
///
/// Always asks `gh`, whatever an earlier answer said: this verb is the remedy a held task's reason
/// names when that answer may have gone stale. A *none* it hears is recorded, as `close-merged`
/// records one.
fn discover(kb: &Kb<'_>, facts: &PrFacts) -> Result<Option<i64>> {
    let Ok(ctx) = repo::repo_ctx() else {
        anyhow::bail!(
            "not in a git repository, so there is no branch to look a pull request up by"
        );
    };
    let Some(branch) = &facts.branch else {
        anyhow::bail!("{}", no_branch_reason(facts));
    };
    let found = match look_up(kb, &Checkout(&ctx.root), facts, branch, Ask::Always) {
        Lookup::Asked(found) => found,
        // Not answered under `Ask::Always`; were it, it would still mean none.
        Lookup::StillAbsent => pr::Discovery::None,
    };
    match found {
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
    let (verdicts, failed) = decide_all(kb, &Checkout(&ctx.root), &repo, dry_run)?;
    report(&verdicts, dry_run, json);
    anyhow::ensure!(
        failed == 0,
        "{failed} task(s) could not be decided (listed above); the rest were"
    );
    Ok(())
}

/// Every unfinished task in `repo`, decided: the verdicts, and how many could not be decided.
fn decide_all(
    kb: &Kb<'_>,
    gh: &dyn Github,
    repo: &str,
    dry_run: bool,
) -> Result<(Vec<CloseVerdict>, usize)> {
    // Finished tasks are left out by the op, in one read: the `post-merge` hook runs this on every
    // `git pull`, over every task the repo has ever had.
    let mut verdicts = Vec::new();
    let mut failed = 0;
    for uid in kb.open_in_repo(repo)? {
        // One task's failure — deleted while `gh` ran for the others, say — is that task's verdict,
        // and the rest still close. The run still fails at the end, so a failure that was the run's
        // (the daemon gone) is not reported as a quiet hold.
        let verdict = close_one(kb, gh, &uid, dry_run).unwrap_or_else(|e| {
            failed += 1;
            CloseVerdict {
                uid: uid.clone(),
                pr: None,
                held: Some(format!("could not be decided: {e:#}")),
            }
        });
        verdicts.push(verdict);
    }
    Ok((verdicts, failed))
}

/// What `close-merged` and `task pr` ask outside the knowledge base — `gh`, and git for a branch's
/// remote-tracking tip. [`Checkout`] asks them in the checkout; a test answers instead, so the
/// number of times `gh` is asked is something a test can count.
trait Github {
    /// The pull requests headed by `branch` ([`pr::discover`]).
    fn discover(&self, branch: &str) -> pr::Discovery;
    /// Whether pull request `number` proves the work landed ([`pr::merged_fact`]).
    fn merged(&self, number: Option<i64>, resumed_at: Option<&str>) -> (Fact, Option<String>);
    /// The commit `branch`'s remote-tracking ref names, `None` when there is no such ref; `Err` when
    /// git could not say.
    fn remote_head(&self, branch: &str) -> Result<Option<String>>;
}

/// [`Github`] in the checkout at this root.
struct Checkout<'a>(&'a Path);

impl Github for Checkout<'_> {
    fn discover(&self, branch: &str) -> pr::Discovery {
        pr::discover(self.0, branch)
    }

    fn merged(&self, number: Option<i64>, resumed_at: Option<&str>) -> (Fact, Option<String>) {
        pr::merged_fact(self.0, number, resumed_at)
    }

    fn remote_head(&self, branch: &str) -> Result<Option<String>> {
        // `origin`, as `gitrepo::branch_ref` reads it, spelled in full so a local branch named
        // `origin/<branch>` cannot answer for it.
        crate::gitrepo::rev_commit(self.0, &format!("refs/remotes/origin/{branch}"))
    }
}

/// Whether a lookup may be answered by a recorded *none*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// Only when nothing it depends on has moved — `close-merged`, run on every `git pull`.
    UnlessStillAbsent,
    /// Every time — `task pr`, the remedy for a recorded answer gone stale.
    Always,
}

/// What a branch lookup came to.
#[derive(Debug)]
enum Lookup {
    /// `gh` was asked, and said this.
    Asked(pr::Discovery),
    /// `gh` was not asked: it answered *none* for this branch before, and the branch has not moved on
    /// the remote since.
    StillAbsent,
}

/// Look up the pull request headed by `branch`, asking `gh` only when its last answer may no longer
/// hold, and recording a *none* it hears so the next run need not ask.
///
/// **Why a *none* is kept, and keyed by the remote tip.** The `post-merge` hook runs `close-merged`
/// over every unfinished task on every `git pull`, and a task in flight with no pull request is the
/// common case — so without this, every pull asked GitHub the same question once per such task. A
/// pull request is opened from a pushed branch and a merge that deletes the remote branch removes
/// the tracking ref at the next pruning fetch, so the remote-tracking tip is what moves when the
/// answer can have changed. Keyed by the branch too, so a task re-worked on another branch asks.
///
/// What it misses, stated rather than covered over: a pull request opened from a tip already asked
/// about — pushed, then a `git pull` here, then opened without pushing again. That costs a missed
/// close, never a wrong one (a recorded *none* cannot close anything), and the held task's reason
/// names the remedy, `jkb task pr <uid>`, which always asks.
///
/// An unreadable tip is never a key: the cached answer is not used and a *none* is not recorded.
fn look_up(kb: &Kb<'_>, gh: &dyn Github, facts: &PrFacts, branch: &str, ask: Ask) -> Lookup {
    let tip = gh.remote_head(branch).ok();
    if ask == Ask::UnlessStillAbsent
        && tip
            .as_ref()
            .is_some_and(|tip| still_absent(facts.pr_absent.as_ref(), branch, tip.as_deref()))
    {
        return Lookup::StillAbsent;
    }
    let found = gh.discover(branch);
    if let (pr::Discovery::None, Some(tip)) = (&found, &tip) {
        // Best effort, and deliberately so: this only spares the next run a question. Failing here
        // — a daemon older than the op, say — means the next run asks again, which is what every
        // run did before; failing the task's verdict over it would hold work this run can decide.
        if let Err(e) = kb.pr_absent(&facts.uid, branch, tip.as_deref()) {
            eprintln!(
                "{}: could not record that no pull request has `{branch}` as its head ({e:#}); \
                 the next run will ask again",
                facts.uid
            );
        }
    }
    Lookup::Asked(found)
}

/// Whether a recorded *none* still answers for `branch` at remote-tracking tip `tip` (`None`: the
/// branch has no remote-tracking ref). Pure, so the staleness rule is checked without `gh` or git.
fn still_absent(recorded: Option<&PrAbsent>, branch: &str, tip: Option<&str>) -> bool {
    recorded.is_some_and(|r| r.branch == branch && r.remote_head.as_deref() == tip)
}

/// Decide one task, and close it if a merged pull request — or a landing jkb recorded — proves it
/// landed. The decision is the lifecycle's (`observed_landed`, whose guard requires the merge
/// **proven** and no open subtasks); this gathers the facts.
fn close_one(kb: &Kb<'_>, gh: &dyn Github, uid: &str, dry_run: bool) -> Result<CloseVerdict> {
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
            None => match discover_quietly(kb, gh, &facts)? {
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
        let (merged, why) = gh.merged(number, facts.resumed_at.as_deref());
        (number, merged, why.map(with_context))
    };
    // **The op is asked only where it can act.** `observed_landed`'s guard requires the merge
    // proven (`jkb_core::lifecycle`), so with anything short of `Yes` the op could only refuse and
    // write nothing — and it is a write, served on the writer, once per unfinished task per
    // `git pull`. The reason is the one the lookup gave, which says what was not established.
    if merged != Fact::Yes {
        return Ok(CloseVerdict {
            uid: facts.uid,
            pr: number,
            held: Some(why.unwrap_or_else(|| {
                with_context("that its work merged is not established".to_owned())
            })),
        });
    }
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
    gh: &dyn Github,
    facts: &PrFacts,
) -> Result<Result<Option<i64>, String>> {
    Ok(
        match quiet_verdict(facts, |branch| {
            look_up(kb, gh, facts, branch, Ask::UnlessStillAbsent)
        }) {
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
fn quiet_verdict(facts: &PrFacts, look: impl FnOnce(&str) -> Lookup) -> Result<i64, String> {
    let Some(branch) = facts.branch.as_deref() else {
        return Err(no_branch_reason(facts));
    };
    let found = match look(branch) {
        Lookup::Asked(found) => found,
        Lookup::StillAbsent => {
            return Err(format!(
                "no pull request had `{branch}` as its head branch when last asked, and the branch \
                 has not moved on the remote since — `jkb task pr {}` asks again",
                facts.uid
            ))
        }
    };
    match found {
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

    use std::cell::{Cell, RefCell};

    use jkb_api::prs::PrAbsent;
    use jkb_api::{ApiError, Backend, LocalBackend, Request, Response};
    use jkb_fsm::Fact;

    use super::{
        decide_all, look_up, quiet_verdict, still_absent, Ask, CloseVerdict, Github, Lookup,
    };
    use crate::pr::{Discovery, PullRequest};
    use crate::session_cli::Kb;

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
            pr_absent: None,
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
        let no_lookup = |_: &str| -> Lookup { unreachable!("no branch, no lookup") };
        let mut reasons = vec![
            quiet_verdict(&facts(None, None), no_lookup),
            quiet_verdict(&facts(None, Some("task/x")), no_lookup),
        ];
        for d in every_discovery() {
            reasons.push(quiet_verdict(&facts(Some("task/x"), None), move |_| {
                Lookup::Asked(d)
            }));
        }
        reasons.push(quiet_verdict(&facts(Some("task/x"), None), |_| {
            Lookup::StillAbsent
        }));
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
            held, 6,
            "no branch (with and without a facet), none, ambiguous, unavailable and a none still \
             standing each hold the task"
        );
    }

    /// Exactly one pull request is the only outcome that does not hold the task.
    #[test]
    fn one_pull_request_is_found_not_held() {
        let found = every_discovery()
            .into_iter()
            .map(|d| quiet_verdict(&facts(Some("task/x"), None), move |_| Lookup::Asked(d)))
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        assert_eq!(found, vec![31]);
    }

    /// A recorded *none* answers only for the branch it was asked about, at the remote tip it was
    /// asked at — a push, a first push, and the tracking ref's removal after a merge each re-ask.
    #[test]
    fn a_recorded_none_stands_only_while_the_remote_tip_has_not_moved() {
        let none_at = |branch: &str, tip: Option<&str>| PrAbsent {
            branch: branch.to_owned(),
            remote_head: tip.map(str::to_owned),
        };
        let at_a = none_at("feat", Some("aa"));
        assert!(still_absent(Some(&at_a), "feat", Some("aa")));
        assert!(!still_absent(Some(&at_a), "feat", Some("bb")), "pushed");
        assert!(
            !still_absent(Some(&at_a), "feat", None),
            "tracking ref gone"
        );
        assert!(
            !still_absent(Some(&at_a), "other", Some("aa")),
            "another branch"
        );
        let unpushed = none_at("feat", None);
        assert!(still_absent(Some(&unpushed), "feat", None));
        assert!(
            !still_absent(Some(&unpushed), "feat", Some("aa")),
            "first push"
        );
        assert!(!still_absent(None, "feat", None), "never asked");
    }

    /// A backend that remembers every op it served, in order.
    struct Counting {
        inner: LocalBackend,
        ops: RefCell<Vec<Request>>,
    }

    impl Backend for Counting {
        fn call(&self, request: Request) -> Result<Response, ApiError> {
            self.ops.borrow_mut().push(request.clone());
            self.inner.call(request)
        }
    }

    impl Counting {
        fn ops_named(&self, op: &str) -> usize {
            self.ops.borrow().iter().filter(|r| r.op() == op).count()
        }

        /// The ops served on the writer: `LocalBackend` serves every other op on its reader.
        fn writes(&self) -> Vec<&'static str> {
            self.ops
                .borrow()
                .iter()
                .filter(|r| !r.is_agent_read())
                .map(Request::op)
                .collect()
        }

        fn forget(&self) {
            self.ops.borrow_mut().clear();
        }
    }

    /// `gh` and git, answered by the test, counting each question.
    struct FakeGh {
        discovery: RefCell<Discovery>,
        merged: RefCell<(Fact, Option<String>)>,
        tip: RefCell<Option<String>>,
        discovered: Cell<usize>,
        merged_asked: Cell<usize>,
    }

    impl FakeGh {
        fn new() -> Self {
            Self {
                discovery: RefCell::new(Discovery::None),
                merged: RefCell::new((Fact::Unknown, Some("not asked".to_owned()))),
                tip: RefCell::new(None),
                discovered: Cell::new(0),
                merged_asked: Cell::new(0),
            }
        }
    }

    impl Github for FakeGh {
        fn discover(&self, _branch: &str) -> Discovery {
            self.discovered.set(self.discovered.get() + 1);
            self.discovery.borrow().clone()
        }

        fn merged(
            &self,
            _number: Option<i64>,
            _resumed_at: Option<&str>,
        ) -> (Fact, Option<String>) {
            self.merged_asked.set(self.merged_asked.get() + 1);
            self.merged.borrow().clone()
        }

        fn remote_head(&self, _branch: &str) -> anyhow::Result<Option<String>> {
            Ok(self.tip.borrow().clone())
        }
    }

    fn op(b: &LocalBackend, r: serde_json::Value) -> Response {
        b.call(serde_json::from_value::<Request>(r).unwrap())
            .unwrap()
    }

    /// A task started on `branch` in repo `proj`, through the ops; its uid.
    fn started(b: &LocalBackend, title: &str, branch: &str) -> String {
        let uid = match op(
            b,
            serde_json::json!({ "op": "task.add", "text": title, "managed": true }),
        ) {
            Response::Added { added } => added.uid,
            other => panic!("{other:?}"),
        };
        op(
            b,
            serde_json::json!({ "op": "task.start", "uid": uid, "take": { "owner": "box:1" },
                                "place": { "branch": branch, "repo": "proj" } }),
        );
        uid
    }

    fn status(b: &LocalBackend, uid: &str) -> String {
        match op(b, serde_json::json!({ "op": "task.show", "uid": uid })) {
            Response::Task { task, .. } => task.item.status.unwrap_or_default(),
            other => panic!("{other:?}"),
        }
    }

    fn one(verdicts: (Vec<CloseVerdict>, usize)) -> CloseVerdict {
        let (mut verdicts, failed) = verdicts;
        assert_eq!(failed, 0);
        assert_eq!(verdicts.len(), 1);
        verdicts.remove(0)
    }

    /// **`gh` is not asked again for a branch it said has no pull request until that branch moves on
    /// the remote** — the `post-merge` hook runs `close-merged` on every `git pull`, and a task in
    /// flight with no pull request is the common case. Then a pull request found closes the task.
    #[test]
    fn close_merged_asks_gh_about_a_branch_with_no_pull_request_once_per_remote_tip() {
        let db = jkb_core::Db::open_in_memory().unwrap();
        let b = Counting {
            inner: LocalBackend::new(db),
            ops: RefCell::new(Vec::new()),
        };
        let kb = Kb::new(&b);
        let uid = started(&b.inner, "in flight", "feat");
        let gh = FakeGh::new();
        *gh.tip.borrow_mut() = Some("aa".to_owned());

        let first = one(decide_all(&kb, &gh, "proj", false).unwrap());
        assert_eq!(gh.discovered.get(), 1);
        assert!(first.held.unwrap().contains("no pull request has `feat`"));
        assert_eq!(b.writes(), ["task.pr_absent"], "the none is recorded");

        // The next pull: nothing moved, so `gh` is not asked, and nothing is written.
        b.forget();
        let again = one(decide_all(&kb, &gh, "proj", false).unwrap());
        assert_eq!(gh.discovered.get(), 1, "asked again with nothing moved");
        let why = again.held.unwrap();
        assert!(why.contains("when last asked"), "{why}");
        assert!(why.contains(&format!("jkb task pr {uid}")), "{why}");
        assert!(b.writes().is_empty(), "{:?}", b.writes());

        // `task pr`'s lookup always asks, whatever was recorded.
        let facts = kb.pr_facts(&uid).unwrap();
        assert!(matches!(
            look_up(&kb, &gh, &facts, "feat", Ask::Always),
            Lookup::Asked(Discovery::None)
        ));
        assert_eq!(gh.discovered.get(), 2);

        // Pushed: the tip moved, so it asks — and a pull request has been opened and merged.
        *gh.tip.borrow_mut() = Some("bb".to_owned());
        *gh.discovery.borrow_mut() = Discovery::One(Box::new(PullRequest {
            number: 31,
            state: Some("MERGED".to_owned()),
            merged_at: None,
            base_ref_name: Some("main".to_owned()),
            head_ref_name: Some("feat".to_owned()),
        }));
        *gh.merged.borrow_mut() = (Fact::Yes, None);
        let closed = one(decide_all(&kb, &gh, "proj", false).unwrap());
        assert_eq!(gh.discovered.get(), 3);
        assert_eq!((closed.pr, closed.held), (Some(31), None));
        assert_eq!(status(&b.inner, &uid), "done");
    }

    /// The tracking ref going away — a merged pull request's branch deleted, then a pruning fetch —
    /// is a move, and so is the first push of a branch that had none.
    #[test]
    fn a_tracking_ref_that_appears_or_vanishes_asks_again() {
        let db = jkb_core::Db::open_in_memory().unwrap();
        let b = Counting {
            inner: LocalBackend::new(db),
            ops: RefCell::new(Vec::new()),
        };
        let kb = Kb::new(&b);
        started(&b.inner, "in flight", "feat");
        let gh = FakeGh::new();
        for (tip, asked) in [
            (None, 1),
            (None, 1),
            (Some("aa"), 2),
            (Some("aa"), 2),
            (None, 3),
        ] {
            *gh.tip.borrow_mut() = tip.map(str::to_owned);
            one(decide_all(&kb, &gh, "proj", false).unwrap());
            assert_eq!(gh.discovered.get(), asked, "at tip {tip:?}");
        }
    }

    /// **`close-merged` writes only where it acts.** Its unfinished tasks come from one read whatever
    /// the repo has ever held, and a task whose merge is not proven costs no write: the op could only
    /// refuse it.
    #[test]
    fn close_merged_writes_only_where_a_merge_is_proven() {
        let db = jkb_core::Db::open_in_memory().unwrap();
        let b = Counting {
            inner: LocalBackend::new(db),
            ops: RefCell::new(Vec::new()),
        };
        let kb = Kb::new(&b);
        let uid = started(&b.inner, "in review", "feat");
        op(
            &b.inner,
            serde_json::json!({ "op": "task.pr_record", "uid": uid, "number": 31 }),
        );
        // Finished tasks the repo has held, which are not visited one by one.
        for n in 0..3 {
            let done = started(&b.inner, &format!("finished {n}"), &format!("old-{n}"));
            op(
                &b.inner,
                serde_json::json!({ "op": "task.set", "uid": done, "status": "done" }),
            );
        }
        let gh = FakeGh::new();
        for (merged, why) in [
            (Fact::No, "pull request #31 is OPEN"),
            (Fact::Unknown, "`gh` is not installed"),
        ] {
            b.forget();
            *gh.merged.borrow_mut() = (merged, Some(why.to_owned()));
            let held = one(decide_all(&kb, &gh, "proj", false).unwrap());
            assert_eq!(held.held.as_deref(), Some(why));
            assert!(b.writes().is_empty(), "{merged:?}: {:?}", b.writes());
            assert_eq!(b.ops_named("task.open_in_repo"), 1);
            assert_eq!(b.ops_named("task.pr_facts"), 1, "only the unfinished task");
        }
        assert_eq!(
            gh.discovered.get(),
            0,
            "a recorded number is not rediscovered"
        );
        assert_eq!(status(&b.inner, &uid), "in_progress");

        // A dry run of a proven merge asks the op, which judges without writing.
        b.forget();
        *gh.merged.borrow_mut() = (Fact::Yes, None);
        let would = one(decide_all(&kb, &gh, "proj", true).unwrap());
        assert_eq!(would.held, None);
        assert_eq!(b.ops_named("task.close_merged"), 1);
        assert_eq!(status(&b.inner, &uid), "in_progress");

        b.forget();
        let closed = one(decide_all(&kb, &gh, "proj", false).unwrap());
        assert_eq!((closed.pr, closed.held), (Some(31), None));
        assert_eq!(b.writes(), ["task.close_merged"]);
        assert_eq!(status(&b.inner, &uid), "done");
    }
}
