//! Review state and the land gate (design D38.4–D38.6).
//!
//! Whether a review has run is the one fact in the staging picture with nowhere authoritative
//! to live: git does not know, and the reviewer is a Claude workflow the CLI cannot run. So it
//! is **stored**, as facets on the task — the smallest thing that can hold it, already
//! carrying the sibling `branch=`/`repo=` facets, and queryable for free. (Whether the review
//! *saw* a task's work is a different question, and no longer stored at all: jkb performs the
//! graft, so a `land` transition onto the reviewed branch is the answer — see `credited_by`.)
//!
//! It deliberately does **not** live on the review folder's namespace: that object's metadata
//! is owned by the sync engine (`header_line`, `sync_section`), and adding a
//! second writer to it is the class of bug that collapsed `openspec/`.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
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

/// The findings of the review(s) at `review_nss`, split into open must-fix and total seen — the one
/// query ([`jkb_api::sessions::review_findings`]), read in this process.
///
/// # Errors
/// Returns an error if the read fails.
pub(crate) fn findings_in(db: &Db, review_nss: &[String]) -> Result<Findings> {
    chunked(review_nss, |nss| {
        let nss = nss.to_vec();
        db.read_with(move |conn| jkb_api::sessions::review_findings(conn, &nss))
            .map_err(|e| anyhow::anyhow!(e.message))
    })
}

/// [`findings_in`], through whichever backend serves this command.
///
/// # Errors
/// Returns an error if the op fails.
pub(crate) fn findings_via(
    kb: &crate::session_cli::Kb<'_>,
    review_nss: &[String],
) -> Result<Findings> {
    chunked(review_nss, |nss| kb.review_findings(nss))
}

/// Ask `read` about `review_nss` in pieces the query accepts, and add the answers up. Every
/// `/review-log` pass adds a `review=` value, so a long-lived branch's task outgrows one piece; a land
/// refused for that — even with `--no-review` — would be a wedge.
fn chunked(
    review_nss: &[String],
    read: impl Fn(&[String]) -> Result<jkb_api::sessions::ReviewFindings>,
) -> Result<Findings> {
    let mut out = Findings::default();
    for nss in review_nss.chunks(jkb_api::sessions::MAX_REVIEW_NAMESPACES) {
        let part: Findings = read(nss)?.into();
        out.total += part.total;
        out.open_count += part.open_count;
        out.open_must_fix.extend(part.open_must_fix);
    }
    Ok(out)
}

/// What a review's namespaces actually contain.
#[derive(Default, Clone)]
pub(crate) struct Findings {
    /// Every finding item found, whatever its priority or status. Zero here means the
    /// namespace resolved to nothing — **not** that the review was clean.
    pub(crate) total: usize,
    /// How many are open and must-fix.
    pub(crate) open_count: usize,
    /// The first of them, for a refusal to name.
    pub(crate) open_must_fix: Vec<OpenFinding>,
}

impl From<jkb_api::sessions::ReviewFindings> for Findings {
    fn from(f: jkb_api::sessions::ReviewFindings) -> Self {
        Self {
            total: f.total,
            open_count: f.open_count,
            open_must_fix: f
                .open_must_fix
                .into_iter()
                .map(|o| OpenFinding {
                    uid: o.uid,
                    title: o.title,
                })
                .collect(),
        }
    }
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
    /// Reviewed, but the review has open must-fix findings: how many, and the first of them.
    OpenFindings(usize, Vec<OpenFinding>),
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
                "No review has been recorded. Run /jkb-review-log in the session, or land with \
                 --no-review."
                    .to_owned(),
            ),
            Self::NoFindingsRecorded(nss) => Some(format!(
                "Its review ({}) holds no findings at all, so they never reached the KB — this \
                 is not a clean review. Re-run /jkb-review-log.",
                nss.join(", ")
            )),
            Self::OpenFindings(count, _) => Some(format!(
                "Its review left {count} open must-fix finding(s). Fix or cancel each one, then land."
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
pub(crate) fn gate(
    kb: &crate::session_cli::Kb<'_>,
    tags: &BTreeMap<String, Vec<String>>,
) -> Result<GateVerdict> {
    let nss = crate::repo::facet_values(tags, FACET_REVIEW).to_vec();
    Ok(gate_with(&findings_via(kb, &nss)?, tags, &nss))
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
    if found.open_count == 0 {
        GateVerdict::Passed
    } else {
        GateVerdict::OpenFindings(found.open_count, found.open_must_fix.clone())
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
    kb: &crate::session_cli::Kb<'_>,
    uid: &str,
    tags: &BTreeMap<String, Vec<String>>,
    no_review: bool,
    json: bool,
) -> Result<bool> {
    let verdict = gate(kb, tags)?;
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
            "{uid} has no recorded review — run `/jkb-review-log` in the session (it records the \
             review itself), or land with --no-review to record a waiver instead"
        ),
        GateVerdict::NoFindingsRecorded(nss) => anyhow::bail!(
            "{uid} records a review of {} but that namespace holds no findings at all — so \
             the review's findings never reached the KB (a quarantined tasks.md, a typo'd \
             --findings, or a namespace renamed since). Re-run `/jkb-review-log`, or land with \
             --no-review. This is NOT read as a clean review.",
            nss.join(", ")
        ),
        GateVerdict::OpenFindings(count, open) => {
            use std::fmt::Write as _;
            let mut msg = format!("{uid} has {count} open must-fix finding(s) from its review:");
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
/// not a task. Tasks are found through `branch=` (a session's own branch) **and** their branches'
/// recorded land target (the
/// staging branch a batch lands on), so reviewing either level tags the work it covers — a
/// staging-branch review is the D38 flow, and its tasks share only the land target.
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
    repo_key: &str,
    branch: &str,
    sha: Option<&str>,
    findings_ns: &str,
) -> Result<Recording> {
    // Matched on `branch=` — the task's own work is what was reviewed — **or** on the land target
    // *when that work is already in the reviewed branch*. A review of a staging branch is the
    // D38 flow, and its tasks share only the land target; matching `branch=` alone tagged
    // nothing and left the whole batch refused as never reviewed.
    //
    // The containment test is what keeps the gate from failing open. A land target says a task
    // *intends* to land on this branch, not that it has: a task still being built in its own
    // session has commits the reviewed branch has never seen, and crediting it would let
    // `jkb task land` graft never-reviewed work — the one direction a safety check must not
    // fail (see `GateVerdict::NoFindingsRecorded`).
    // Tasks that intend to land here but whose work is not on this branch yet, so the review
    // cannot have seen it. Reported, because silence here reads as "everything was tagged".
    let mut skipped_unlanded = Vec::new();
    let mut on_branch: Vec<(ItemId, String)> = Vec::new();
    let mut unusable = Vec::new();
    for t in crate::repo::repo_tasks(db, repo_key)? {
        // A branch value git cannot be handed at all costs its own row and no more. `?`-ing on one
        // aborted the entire run, which records `reviewed=` for NO task — so one malformed tag
        // anywhere in the repo silently turned every landing in the batch into "never reviewed".
        if crate::repo::facet_values(&t.tags, crate::repo::FACET_BRANCH)
            .iter()
            .any(|b| crate::gitrepo::valid_ref(b).is_err())
        {
            unusable.push(t.meta.uid.clone());
            continue;
        }
        match credited_by(db, &t, branch)? {
            Credit::OwnBranch | Credit::Grafted => on_branch.push((t.meta.id, t.meta.uid.clone())),
            Credit::LandsHereButHasNot => skipped_unlanded.push(t.meta.uid.clone()),
            // Dropped, and that is right: this loop walks **every** task in the repo, so
            // `Unrelated` is overwhelmingly "records a different branch" — listing those would
            // report most of the backlog on every run. The case that must not land here is a task
            // whose work jkb really did graft onto this branch and which was then abandoned; that
            // is answered by `credited_by` asking the *historical* question, above, not by a
            // bucket here.
            Credit::Unrelated => {}
        }
    }
    if on_branch.is_empty() {
        return Ok(Recording {
            recorded: Vec::new(),
            skipped_unlanded,
            unusable,
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
        unusable,
    })
}

/// What a `review record` did, and what it deliberately did not do.
pub(crate) struct Recording {
    pub(crate) recorded: Vec<Recorded>,
    /// Tasks whose land target is this branch but whose work has not been grafted onto it yet,
    /// so the review cannot have covered them. Reported, because silence here reads as
    /// "everything was tagged".
    pub(crate) skipped_unlanded: Vec<String>,
    /// Skipped because a recorded branch value cannot be handed to git at all. Reported, not
    /// fatal: one malformed tag must not stop the whole branch being credited.
    pub(crate) unusable: Vec<String>,
}

/// Why a review of one branch does — or does not — cover a task.
enum Credit {
    /// The reviewed branch **is** this task's work branch. Covered by definition: the review
    /// read it.
    OwnBranch,
    /// jkb itself grafted this task's work onto the reviewed branch, and recorded that it did.
    Grafted,
    /// The task means to land here and has not yet, so the review saw none of its work.
    LandsHereButHasNot,
    /// Nothing to do with this branch.
    Unrelated,
}

/// Whether a review of `branch` covers this task's work.
///
/// This replaced a containment probe over the commit graph, and the replacement is not a cheaper
/// version of the same question — it is a **recorded event** instead of an inference. The probe
/// had to ask "are this task's commits already in the reviewed branch?", which after a rebase or
/// squash cannot be answered from the commits, so it fell back to "does this branch add anything
/// to the target?" — which a branch with *no commits at all* also answers no to. Separating those
/// two needed a stored cut point per branch, and every degenerate case had to be pinned down by
/// hand: an empty session read as covered and stamped `reviewed=` on work nobody had written.
///
/// jkb performs the graft, so it knows. A `land` transition onto this branch is the answer, and
/// a task that has not landed yet simply has no such entry — which is the same conservative
/// direction the probe was straining for, without the machinery.
///
/// # Errors
/// Returns an error if the history cannot be read.
fn credited_by(db: &Db, t: &crate::repo::RepoTask, branch: &str) -> Result<Credit> {
    if crate::repo::facet_values(&t.tags, crate::repo::FACET_BRANCH)
        .iter()
        .any(|b| b == branch)
    {
        return Ok(Credit::OwnBranch);
    }
    let id = t.meta.id;
    let landing = db.read(move |conn| jkb_core::transition::landing(conn, id))?;
    let onto_is_branch = |r: Option<&jkb_core::transition::TransitionRow>| -> bool {
        r.and_then(|r| r.labels.onto.as_deref())
            .is_some_and(|onto| onto == branch)
    };

    // **Present tense first, and it is the one that can credit.** A landing that still speaks for
    // the work means this branch holds what the task is doing now, so the review read it.
    if onto_is_branch(landing.live()) {
        return Ok(Credit::Grafted);
    }

    // **Then the question the present tense cannot answer.** A task still aimed here has work
    // coming that this review has not seen, whatever it grafted before — so it is reported, never
    // credited. Asking the historical question ahead of this credited a task that landed, was
    // reopened for a must-fix, and had its fix committed in a session this branch has never seen:
    // `reviewed=` was stamped for work this review never read, and the task was moved to
    // `needs_review` under somebody's feet. Recording a false statement is the harm; that it also
    // unblocks the gate is true only where the task had never been reviewed before, since
    // `gate_with` asks whether a `reviewed=` exists and not whether it is current (D38 declines
    // to enforce staleness deliberately).
    let target = db.read(move |conn| jkb_core::transition::land_target(conn, id))?;
    if target.as_deref() == Some(branch) {
        return Ok(Credit::LandsHereButHasNot);
    }

    // **Only now the historical question**, and only because nothing present-tense applies: the
    // task aims nowhere. That is what `abandon` leaves — it retires the land target — and a graft
    // does not un-happen, so a session abandoned after its work reached this branch is still
    // covered by a review of it. Without this the task fell to `Unrelated`, which this loop drops,
    // so `review record` said nothing about it and `land` refused it much later for want of a
    // review nobody knew was missing.
    if target.is_none() && onto_is_branch(landing.recorded()) {
        return Ok(Credit::Grafted);
    }

    Ok(Credit::Unrelated)
}

#[cfg(test)]
mod tests {
    use jkb_core::Db;

    /// A task whose reviews outnumber what one query takes is still gated by all of them — asked in
    /// pieces, added up — rather than refused outright, which blocked even `--no-review`.
    #[test]
    fn findings_across_more_reviews_than_one_query_takes_are_added_up() {
        let db = Db::open_in_memory().unwrap();
        let backend = jkb_api::LocalBackend::new(db.clone());
        let n = jkb_api::sessions::MAX_REVIEW_NAMESPACES + 1;
        let nss: Vec<String> = (0..n).map(|i| format!("reviews/r{i}")).collect();
        for ns in [&nss[0], &nss[n - 1]] {
            jkb_api::Backend::call(
                &backend,
                serde_json::from_value(serde_json::json!({
                    "op": "task.add", "text": format!("must fix !p1 +{ns}"), "managed": true
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let local = super::findings_in(&db, &nss).unwrap();
        let via = super::findings_via(&crate::session_cli::Kb::new(&backend), &nss).unwrap();
        for f in [local, via] {
            assert_eq!((f.total, f.open_count, f.open_must_fix.len()), (2, 2, 2));
        }
    }
}
