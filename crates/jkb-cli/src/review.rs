//! Review state and the land gate (design D38.4–D38.6).
//!
//! Whether a review has run is the one fact in the staging picture with nowhere authoritative
//! to live: git does not know, and the reviewer is a Claude workflow the CLI cannot run. So it
//! is **stored**, as facets on the task — the smallest thing that can hold it, already
//! carrying the sibling `branch=`/`onto=`/`base=` facets, and queryable for free.
//!
//! It deliberately does **not** live on the review folder's namespace: that object's metadata
//! is owned by the sync engine (`layout`, `header_line`, `position`, `prose`), and adding a
//! second writer to it is the class of bug that collapsed `openspec/`.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use jkb_core::query::{Query, Scope};
use jkb_core::{item, tag, task, Db};
use jkb_types::{ItemId, TaskStatus};

/// The branch HEAD a review ran against.
pub(crate) const FACET_REVIEWED: &str = "reviewed";
/// The review's findings namespace, so the findings are one `jkb ls` away.
pub(crate) const FACET_REVIEW: &str = "review";
/// A recorded `--no-review` override. An override nobody can see is indistinguishable from a
/// rule that does not exist.
pub(crate) const FACET_REVIEW_WAIVED: &str = "review-waived";

/// A must-fix finding that is neither `done` nor `cancelled`.
#[derive(Clone)]
pub(crate) struct OpenFinding {
    pub(crate) uid: String,
    pub(crate) title: String,
}

/// The findings of the review(s) at `review_nss`, split into open must-fix and total seen.
///
/// The scope is built as a **typed** [`Query`] rather than by interpolating the namespace
/// into the DSL. Interpolation made the namespace name re-parseable: a path containing `,`
/// (which `/review-log` does not rewrite — it only replaces `/`) split into two scopes that
/// match nothing, and any unresolvable path yields an empty candidate set. That is
/// indistinguishable from "clean" unless the caller also knows how many findings exist,
/// which is why this returns the total as well.
///
/// Terminal statuses are filtered in Rust: the DSL has `status:<s>` but no `-status:`, and
/// `is:ready` is the wrong instrument because a **blocked** must-fix finding must still block.
///
/// # Errors
/// Returns an error if the read fails.
pub(crate) fn findings_in(db: &Db, review_nss: &[String]) -> Result<Findings> {
    if review_nss.is_empty() {
        return Ok(Findings::default());
    }
    let query = Query {
        kind: Some("task".to_owned()),
        // A union over every recorded review: re-running `/review-log` must not retire the
        // previous run's still-open findings (design D38.5).
        scope: Scope::Union(review_nss.iter().cloned().map(Scope::Subtree).collect()),
        ..Query::default()
    };
    Ok(db.read(move |conn| {
        let ids = query.evaluate(conn)?;
        let metas = item::get_many(conn, &ids)?;
        let mut out = Findings {
            total: ids.len(),
            open_must_fix: Vec::new(),
        };
        for id in ids {
            let Some(m) = metas.get(&id) else { continue };
            let status = m.status.as_deref().unwrap_or("open");
            // Through the one spelling of the terminal set. This is the filter that decides
            // whether a must-fix still blocks a landing, so a divergent copy here is the most
            // expensive place to have one.
            if jkb_types::TaskStatus::is_terminal_str(Some(status))
                || m.priority.unwrap_or(i64::MAX) > 1
            {
                continue;
            }
            out.open_must_fix.push(OpenFinding {
                uid: m.uid.clone(),
                title: crate::output::title_of(m),
            });
        }
        Ok(out)
    })?)
}

/// What a review's namespaces actually contain.
#[derive(Default, Clone)]
pub(crate) struct Findings {
    /// Every finding item found, whatever its priority or status. Zero here means the
    /// namespace resolved to nothing — **not** that the review was clean.
    pub(crate) total: usize,
    pub(crate) open_must_fix: Vec<OpenFinding>,
}

/// Why a task may not land, if it may not.
pub(crate) enum GateVerdict {
    /// Reviewed, with nothing must-fix outstanding.
    Passed,
    /// No `reviewed=` facet: no review has been recorded for this task.
    NeverReviewed,
    /// A review is recorded, but its namespace(s) hold no finding items at all. That is not
    /// a clean review — it is a review whose findings never reached the KB (a quarantined
    /// `tasks.md`, a typo'd `--findings`, a namespace renamed since). Treating it as clean is
    /// the gate failing **open**, which is the one direction a safety check must not fail.
    NoFindingsRecorded(Vec<String>),
    /// Reviewed, but the review has open must-fix findings.
    OpenFindings(Vec<OpenFinding>),
}

impl GateVerdict {
    /// One line, for a listing row: why the gate would refuse, or `None` if it would pass.
    ///
    /// The long form — with the remedy for each case — is [`enforce`]'s refusal, which is what
    /// someone running `jkb task land` reads. Both spellings come from the same verdict, so a
    /// row cannot report a different rule than the command applies.
    pub(crate) fn short(&self) -> Option<String> {
        match self {
            Self::Passed => None,
            Self::NeverReviewed => Some(
                "No review has been recorded. Run /review-log in the session, or land with \
                 --no-review."
                    .to_owned(),
            ),
            Self::NoFindingsRecorded(nss) => Some(format!(
                "Its review ({}) holds no findings at all, so they never reached the KB — this \
                 is not a clean review. Re-run /review-log.",
                nss.join(", ")
            )),
            Self::OpenFindings(open) => Some(format!(
                "Its review left {} open must-fix finding(s). Fix or cancel each one, then land.",
                open.len()
            )),
        }
    }
}

/// Decide whether `tags` permit a landing (design D38.5).
///
/// Concerns and nits do not block. A gate everything trips is a gate nobody keeps: a previous
/// run put 34 of 45 findings on `concern`, and blocking on those would make `--no-review` the
/// normal path within a week.
///
/// **Every** recorded `review=` namespace is consulted, not just the newest: re-running
/// `/review-log` must not silently retire the previous run's still-open must-fix findings.
///
/// # Errors
/// Returns an error if the findings cannot be read.
pub(crate) fn gate(db: &Db, tags: &BTreeMap<String, Vec<String>>) -> Result<GateVerdict> {
    let nss = crate::repo::facet_values(tags, FACET_REVIEW).to_vec();
    Ok(gate_with(&findings_in(db, &nss)?, tags, &nss))
}

/// The gate's decision, given findings already read.
///
/// Split from [`gate`] so a caller holding the findings — `staging::collect`, which reads one
/// namespace set once for a whole branch rather than once per row — applies the same rule
/// without a second query. The rule itself lives here and nowhere else.
///
/// `nss` is the namespace set `found` was read from, and is passed rather than re-derived
/// from `tags`: the two can disagree, and the caller that substitutes an empty `Findings`
/// (for a row it does not intend to gate) would otherwise be told its intact review "holds no
/// findings at all — re-run /review-log". Taking both means the mismatch cannot be expressed.
pub(crate) fn gate_with(
    found: &Findings,
    tags: &BTreeMap<String, Vec<String>>,
    nss: &[String],
) -> GateVerdict {
    if crate::repo::facet_one(tags, FACET_REVIEWED).is_none() {
        return GateVerdict::NeverReviewed;
    }
    if nss.is_empty() || found.total == 0 {
        return GateVerdict::NoFindingsRecorded(nss.to_vec());
    }
    if found.open_must_fix.is_empty() {
        GateVerdict::Passed
    } else {
        GateVerdict::OpenFindings(found.open_must_fix.clone())
    }
}

/// Apply the land gate, or explain why the landing is refused (design D38.5).
///
/// `no_review` records a waiver instead of refusing. The waiver is *stored*, because an
/// override nobody can see is indistinguishable from a rule that does not exist.
///
/// Returns whether a waiver is **owed** — the gate did not pass and `--no-review` carried the
/// landing. The caller records it only once the landing has actually happened: writing it here
/// left a permanent waiver behind for a land that then failed on the graft or the gate build,
/// marking a task as deliberately-unreviewed for something that never occurred.
///
/// # Errors
/// Returns an error — the refusal itself — when the task has no recorded review, its review
/// namespace holds no findings at all, or its review has open must-fix findings.
pub(crate) fn enforce(
    db: &Db,
    uid: &str,
    tags: &BTreeMap<String, Vec<String>>,
    no_review: bool,
    json: bool,
) -> Result<bool> {
    let verdict = gate(db, tags)?;
    if matches!(verdict, GateVerdict::Passed) {
        return Ok(false);
    }
    if no_review {
        if !json {
            println!(
                "review: WAIVED with --no-review (recorded on the task if this land succeeds)"
            );
        }
        return Ok(true);
    }
    match verdict {
        GateVerdict::Passed => Ok(false),
        GateVerdict::NeverReviewed => anyhow::bail!(
            "{uid} has no recorded review — run `/review-log` in the session (it records the \
             review itself), or land with --no-review to record a waiver instead"
        ),
        GateVerdict::NoFindingsRecorded(nss) => anyhow::bail!(
            "{uid} records a review of {} but that namespace holds no findings at all — so \
             the review's findings never reached the KB (a quarantined tasks.md, a typo'd \
             --findings, or a namespace renamed since). Re-run `/review-log`, or land with \
             --no-review. This is NOT read as a clean review.",
            nss.join(", ")
        ),
        GateVerdict::OpenFindings(open) => {
            use std::fmt::Write as _;
            let mut msg = format!(
                "{uid} has {} open must-fix finding(s) from its review:",
                open.len()
            );
            for f in open.iter().take(10) {
                let _ = write!(msg, "\n  - {} ({})", f.title, f.uid);
            }
            msg.push_str(
                "\nfix them (or `jkb task set <uid> --status cancelled` to dismiss one), then \
                 land again; --no-review records a waiver instead",
            );
            anyhow::bail!(msg)
        }
    }
}

/// One task that a `review record` touched.
pub(crate) struct Recorded {
    pub(crate) uid: String,
    pub(crate) moved_to_review: bool,
}

/// Record that a review ran against `branch` at `sha`, producing findings under `findings_ns`.
///
/// Keyed by **branch**, because that is what a review knows: it reviewed a range on a branch,
/// not a task. Tasks are found through `branch=` (a session's own branch) **and** `onto=` (the
/// staging branch a batch lands on), so reviewing either level tags the work it covers — a
/// staging-branch review is the D38 flow, and its tasks share only `onto=`.
///
/// A branch no task claims (trunk, an ad-hoc range) matches nothing and returns an empty list
/// — a note for the caller to print, not an error, because reviewing an arbitrary range is a
/// legitimate thing to do.
///
/// Recording moves `in_progress` to `needs_review` and is the **only** author of that
/// transition (design D38.6). Any other status is left alone.
///
/// The whole branch is recorded in **one transaction**, and each task's status is re-read
/// *inside* it. Deciding the transition from a snapshot taken before the loop, then writing
/// it per-task, could resurrect a task that landed in between — `set_status` is a plain
/// `UPDATE` with no CAS, and `needs_review` is non-terminal, so a re-blocked dependent and a
/// re-offered staging branch would follow. One transaction also means a Ctrl-C halfway
/// through cannot leave half the branch tagged and half refusing to land as never reviewed.
///
/// `review=` is **added**, not set: a second run's findings do not retire the first run's
/// still-open must-fix items (the gate unions every recorded namespace). `reviewed=` is set,
/// since there is only one current HEAD.
///
/// # Errors
/// Returns an error if the database cannot be read or written.
pub(crate) fn record(
    db: &Db,
    repo_root: &std::path::Path,
    repo_key: &str,
    branch: &str,
    sha: Option<&str>,
    findings_ns: &str,
) -> Result<Recording> {
    // Matched on `branch=` — the task's own work is what was reviewed — **or** on `onto=`
    // *when that work is already in the reviewed branch*. A review of a staging branch is the
    // D38 flow, and its tasks share only `onto=<staging>`; matching `branch=` alone tagged
    // nothing and left the whole batch refused as never reviewed.
    //
    // The containment test is what keeps the gate from failing open. `onto=` says a task
    // *intends* to land on this branch, not that it has: a task still being built in its own
    // session has commits the reviewed branch has never seen, and crediting it would let
    // `jkb task land` graft never-reviewed work — the one direction a safety check must not
    // fail (see `GateVerdict::NoFindingsRecorded`).
    let mut skipped_unlanded = Vec::new();
    let mut on_branch: Vec<(ItemId, String)> = Vec::new();
    // One merge probe per distinct (work branch, base): a swarm group puts the same `branch=`
    // on every task in it, and each probe is about four git spawns.
    let mut covered: BTreeMap<(String, Option<String>), bool> = BTreeMap::new();
    for t in crate::repo::repo_tasks(db, repo_key)? {
        let names = |facet: &str| {
            crate::repo::facet_values(&t.tags, facet)
                .iter()
                .any(|b| b == branch)
        };
        // Both arms ask the same question — "is every branch of this task accounted for in the
        // reviewed one?" — and differ only in which facet made the task a candidate. Kept
        // separate because the reasons differ and are worth reading; the bodies are one call.
        if names(crate::repo::FACET_BRANCH) {
            // The reviewed branch is covered **by definition** — the review just read it — so it
            // is excluded from the probe. Probing it against itself asks `is_merged(b, b)`, and
            // a session branch with no commits yet answers `NothingToMerge`, which is not in the
            // covered set: the task was skipped, no `reviewed=` was written, and `task land`
            // then refused the branch `/review-log` had just called landable, leaving
            // `--no-review` as the remedy at hand. That normalises the override D38.5 exists to
            // prevent.
            //
            // The task's OTHER branches are still probed, which is the point of the check: a
            // task carrying a second, live session branch must not be credited by a review that
            // never saw it.
            if others_are_covered(repo_root, &t, branch, &mut covered)? {
                on_branch.push((t.meta.id, t.meta.uid.clone()));
            } else {
                skipped_unlanded.push(t.meta.uid.clone());
            }
        } else if names(crate::repo::FACET_ONTO) {
            if work_is_in(repo_root, &t, branch, &mut covered)? {
                on_branch.push((t.meta.id, t.meta.uid.clone()));
            } else {
                skipped_unlanded.push(t.meta.uid.clone());
            }
        }
    }
    if on_branch.is_empty() {
        return Ok(Recording {
            recorded: Vec::new(),
            skipped_unlanded,
        });
    }

    let (sha_owned, ns_owned) = (sha.unwrap_or("unknown").to_owned(), findings_ns.to_owned());
    let ids: Vec<ItemId> = on_branch.iter().map(|(id, _)| *id).collect();
    let moved_ids = db
        .write_txn("cli", move |conn, meta| {
            let mut moved = Vec::new();
            for id in &ids {
                crate::repo::set_facet(conn, meta, *id, FACET_REVIEWED, &sha_owned)?;
                // Additive: `review=` accumulates, so an earlier run's open findings keep
                // gating. `set_facet` here would silently un-gate them.
                tag::apply(conn, meta, *id, FACET_REVIEW, &ns_owned)?;
                // Re-read inside the transaction: the status may have changed since the
                // caller's snapshot, and only `in_progress` may become `needs_review`.
                let current = item::get(conn, *id)?.and_then(|m| m.status);
                if current.as_deref() == Some("in_progress") {
                    task::set_status(conn, meta, *id, TaskStatus::NeedsReview)?;
                    moved.push(*id);
                }
            }
            Ok(moved)
        })
        .with_context(|| format!("recording the review of {branch}"))?;

    Ok(Recording {
        recorded: on_branch
            .into_iter()
            .map(|(id, uid)| Recorded {
                uid,
                moved_to_review: moved_ids.contains(&id),
            })
            .collect(),
        skipped_unlanded,
    })
}

/// What a `review record` did, and what it deliberately did not do.
pub(crate) struct Recording {
    pub(crate) recorded: Vec<Recorded>,
    /// Tasks landing on this branch whose own work is not in it yet, so the review cannot
    /// have covered them. Reported, because silence here reads as "everything was tagged".
    pub(crate) skipped_unlanded: Vec<String>,
}

/// Whether `work`'s commits are already contained in `branch`, memoized per `(work, base)`:
/// `is_merged` is about four git spawns and a swarm group puts one branch on every task in it.
///
/// `BranchMissing` counts as contained — a branch that is gone was deleted by `land`, which only
/// happens once its commits reached the target.
fn branch_is_in(
    repo_root: &std::path::Path,
    work: &str,
    branch: &str,
    base: Option<&str>,
    covered: &mut BTreeMap<(String, Option<String>), bool>,
) -> Result<bool> {
    let key = (work.to_owned(), base.map(str::to_owned));
    if let Some(known) = covered.get(&key) {
        return Ok(*known);
    }
    // `landed_for_action`, not `is_merged`: crediting a task as reviewed is acting on the
    // answer, and a branch with no recorded base must not be credited — an empty live sibling
    // otherwise reads as covered and `reviewed=<sha>` is stamped for work no review saw.
    let (state, _) = crate::repo::landed_for_action(
        repo_root,
        work,
        branch,
        base.map(|b| vec![format!("{work}:{b}")])
            .unwrap_or_default()
            .as_slice(),
        1,
        crate::gitrepo::Prefer::Local,
    )?;
    let answer = matches!(
        state,
        crate::gitrepo::MergeState::Merged | crate::gitrepo::MergeState::BranchMissing
    );
    covered.insert(key, answer);
    Ok(answer)
}

/// Whether every `branch=` this task records **other than the reviewed one** is already
/// contained in the reviewed branch.
///
/// Split out from [`work_is_in`] because the two callers ask different questions. On the `onto=`
/// arm the task's branch is a sub-branch and containment is the whole question. On the `branch=`
/// arm the reviewed branch is the task's own, so probing it against itself is not just redundant
/// — it answers `NothingToMerge` for a session that has not committed yet, and skips the task.
fn others_are_covered(
    repo_root: &std::path::Path,
    t: &crate::repo::RepoTask,
    branch: &str,
    covered: &mut BTreeMap<(String, Option<String>), bool>,
) -> Result<bool> {
    let bases = crate::repo::facet_values(&t.tags, crate::repo::FACET_BASE);
    let works = crate::repo::facet_values(&t.tags, crate::repo::FACET_BRANCH);
    for work in works {
        if work == branch {
            continue;
        }
        // Per branch: a base describes the one branch it was cut from, and lending it to a
        // sibling disables the "nothing on it yet" guard for that sibling.
        let base = crate::repo::base_for_branch(bases, work, works.len());
        if !branch_is_in(repo_root, work, branch, base, covered)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether this task's own work is already contained in `branch` — i.e. whether a review of
/// `branch` can have seen it.
///
/// `is_merged` asks "would re-merging change anything", which is the right question however
/// the work got there: a session `land` fast-forwards and the swarm's merge queue rebases, and
/// neither preserves the original shas (D34.2).
///
/// Both degenerate answers are **not covered**, because this decides whether to open the land
/// gate and it must fail closed:
///
/// - **No `branch=` at all.** `/task-swarm` writes `onto=` at claim and `branch=` only once an
///   implementer has one, so a task mid-claim has intent and no work. Returning "covered"
///   there stamped `reviewed=` on a task that had not been written yet.
/// - **`NothingToMerge`.** A branch cut from the target with nothing committed on it re-merges
///   to the target's own tree, so on content alone it reads `Merged`. `base=` exists precisely
///   to separate that from "contributed nothing because it landed" (D34.2), so it is passed
///   here; without it, `jkb task work` followed by a staging review credited a session that
///   had no commits.
///
/// `BranchMissing` **is** covered: a branch that is gone was deleted by `land`, which happens
/// only after its commits reached the target.
///
/// **Every** recorded `branch=` is probed, and all of them must be covered. `set_location_facets`
/// makes the facet single-valued *going forward*, but it exists because `task work` followed by
/// `task start` used to leave two — and `tasks_by_branch` indexes every value for that same
/// reason. Probing only the first meant a stale, deleted branch answered `BranchMissing`,
/// stamped `reviewed=`, and opened the gate for a live sibling branch the review never saw.
///
/// Memoized per `(work branch, base)`: `is_merged` is about four git spawns, and a swarm group
/// puts the same `branch=` on every task in it.
fn work_is_in(
    repo_root: &std::path::Path,
    t: &crate::repo::RepoTask,
    branch: &str,
    covered: &mut BTreeMap<(String, Option<String>), bool>,
) -> Result<bool> {
    let branches = crate::repo::facet_values(&t.tags, crate::repo::FACET_BRANCH);
    if branches.is_empty() {
        return Ok(false);
    }
    let bases = crate::repo::facet_values(&t.tags, crate::repo::FACET_BASE);
    for work in branches {
        let base = crate::repo::base_for_branch(bases, work, branches.len());
        if !branch_is_in(repo_root, work, branch, base, covered)? {
            return Ok(false);
        }
    }
    Ok(true)
}
