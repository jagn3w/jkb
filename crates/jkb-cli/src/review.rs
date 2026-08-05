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
            if status == "done" || status == "cancelled" || m.priority.unwrap_or(i64::MAX) > 1 {
                continue;
            }
            out.open_must_fix.push(OpenFinding {
                uid: m.uid.clone(),
                title: m
                    .content
                    .as_deref()
                    .and_then(|c| c.lines().find(|l| !l.trim().is_empty()))
                    .unwrap_or(&m.uid)
                    .trim()
                    .to_owned(),
            });
        }
        Ok(out)
    })?)
}

/// What a review's namespaces actually contain.
#[derive(Default)]
pub(crate) struct Findings {
    /// Every finding item found, whatever its priority or status. Zero here means the
    /// namespace resolved to nothing — **not** that the review was clean.
    pub(crate) total: usize,
    pub(crate) open_must_fix: Vec<OpenFinding>,
}

/// The open must-fix findings for one namespace — the count a staging row shows.
///
/// # Errors
/// Returns an error if the read fails.
pub(crate) fn open_must_fix(db: &Db, review_nss: &[String]) -> Result<Vec<OpenFinding>> {
    Ok(findings_in(db, review_nss)?.open_must_fix)
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
    if crate::repo::facet_one(tags, FACET_REVIEWED).is_none() {
        return Ok(GateVerdict::NeverReviewed);
    }
    let nss = crate::repo::facet_values(tags, FACET_REVIEW).to_vec();
    if nss.is_empty() {
        return Ok(GateVerdict::NoFindingsRecorded(nss));
    }
    let found = findings_in(db, &nss)?;
    if found.total == 0 {
        return Ok(GateVerdict::NoFindingsRecorded(nss));
    }
    if found.open_must_fix.is_empty() {
        Ok(GateVerdict::Passed)
    } else {
        Ok(GateVerdict::OpenFindings(found.open_must_fix))
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
/// not a task. The task is found through the existing `branch=` index, and a staging-wide
/// review tags every task on it.
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
) -> Result<Vec<Recorded>> {
    let on_branch: Vec<(ItemId, String)> = crate::repo::repo_tasks(db, repo_key)?
        .into_iter()
        .filter(|t| {
            crate::repo::facet_values(&t.tags, crate::repo::FACET_BRANCH)
                .iter()
                .any(|b| b == branch)
        })
        .map(|t| (t.meta.id, t.meta.uid.clone()))
        .collect();
    if on_branch.is_empty() {
        return Ok(Vec::new());
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

    Ok(on_branch
        .into_iter()
        .map(|(id, uid)| Recorded {
            uid,
            moved_to_review: moved_ids.contains(&id),
        })
        .collect())
}
