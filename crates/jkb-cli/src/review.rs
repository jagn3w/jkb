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
use jkb_core::{item, task, Db};
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

/// The open must-fix findings of the review at `review_ns`.
///
/// Counted with `kind:task ns:<review_ns>/** priority<=1`, filtering terminal statuses in
/// Rust: the DSL has `status:<s>` but no `-status:`, and `is:ready` is the wrong instrument
/// because a **blocked** must-fix finding must still block landing.
///
/// # Errors
/// Returns an error if the query or the read fails.
pub(crate) fn open_must_fix(db: &Db, review_ns: &str) -> Result<Vec<OpenFinding>> {
    let query = jkb_core::query::parse(&format!("kind:task ns:{review_ns}/** priority<=1"))?;
    Ok(db.read(move |conn| {
        let ids = query.evaluate(conn)?;
        let metas = item::get_many(conn, &ids)?;
        let mut out = Vec::new();
        for id in ids {
            let Some(m) = metas.get(&id) else { continue };
            let status = m.status.as_deref().unwrap_or("open");
            if status == "done" || status == "cancelled" {
                continue;
            }
            out.push(OpenFinding {
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

/// Why a task may not land, if it may not.
pub(crate) enum GateVerdict {
    /// Reviewed, with nothing must-fix outstanding.
    Passed,
    /// No `reviewed=` facet: no review has been recorded for this task.
    NeverReviewed,
    /// Reviewed, but the review has open must-fix findings.
    OpenFindings(Vec<OpenFinding>),
}

/// Decide whether `tags` permit a landing (design D38.5).
///
/// Concerns and nits do not block. A gate everything trips is a gate nobody keeps: a previous
/// run put 34 of 45 findings on `concern`, and blocking on those would make `--no-review` the
/// normal path within a week.
///
/// # Errors
/// Returns an error if the findings cannot be read.
pub(crate) fn gate(db: &Db, tags: &BTreeMap<String, Vec<String>>) -> Result<GateVerdict> {
    if crate::facet_one(tags, FACET_REVIEWED).is_none() {
        return Ok(GateVerdict::NeverReviewed);
    }
    let Some(ns) = crate::facet_one(tags, FACET_REVIEW) else {
        // Reviewed but no findings namespace recorded: nothing to check against.
        return Ok(GateVerdict::Passed);
    };
    let open = open_must_fix(db, ns)?;
    if open.is_empty() {
        Ok(GateVerdict::Passed)
    } else {
        Ok(GateVerdict::OpenFindings(open))
    }
}

/// Apply the land gate, or explain why the landing is refused (design D38.5).
///
/// `no_review` records a waiver instead of refusing. The waiver is *stored*, because an
/// override nobody can see is indistinguishable from a rule that does not exist.
///
/// # Errors
/// Returns an error — the refusal itself — when the task has no recorded review, or its
/// review has open must-fix findings.
pub(crate) fn enforce(
    db: &Db,
    uid: &str,
    id: ItemId,
    tags: &BTreeMap<String, Vec<String>>,
    head: &str,
    no_review: bool,
    json: bool,
) -> Result<()> {
    let verdict = gate(db, tags)?;
    if matches!(verdict, GateVerdict::Passed) {
        return Ok(());
    }
    if no_review {
        waive(db, id, head)?;
        if !json {
            println!("review: WAIVED with --no-review (recorded as review-waived={head})");
        }
        return Ok(());
    }
    match verdict {
        GateVerdict::Passed => Ok(()),
        GateVerdict::NeverReviewed => anyhow::bail!(
            "{uid} has no recorded review — run `/review-log` in the session (it records the \
             review itself), or land with --no-review to record a waiver instead"
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
/// # Errors
/// Returns an error if the database cannot be read or written.
pub(crate) fn record(
    db: &Db,
    repo_key: &str,
    branch: &str,
    sha: Option<&str>,
    findings_ns: &str,
) -> Result<Vec<Recorded>> {
    let tasks = crate::repo_tasks(db, repo_key)?;
    let mut out = Vec::new();
    for t in tasks {
        if !crate::facet_values(&t.tags, crate::FACET_BRANCH)
            .iter()
            .any(|b| b == branch)
        {
            continue;
        }
        let id = t.meta.id;
        let status = t.meta.status.clone().unwrap_or_default();
        let moved = status == "in_progress";
        let (sha_owned, ns_owned) = (sha.unwrap_or("unknown").to_owned(), findings_ns.to_owned());
        db.write_txn("cli", move |conn, meta| {
            crate::set_facet(conn, meta, id, FACET_REVIEWED, &sha_owned)?;
            crate::set_facet(conn, meta, id, FACET_REVIEW, &ns_owned)?;
            if moved {
                task::set_status(conn, meta, id, TaskStatus::NeedsReview)?;
            }
            Ok(())
        })
        .with_context(|| format!("recording the review on {}", t.meta.uid))?;
        out.push(Recorded {
            uid: t.meta.uid.clone(),
            moved_to_review: moved,
        });
    }
    Ok(out)
}

/// Record a `--no-review` waiver on `id`.
///
/// # Errors
/// Returns an error if the write fails.
pub(crate) fn waive(db: &Db, id: ItemId, sha: &str) -> Result<()> {
    let sha = sha.to_owned();
    db.write_txn("cli", move |conn, meta| {
        crate::set_facet(conn, meta, id, FACET_REVIEW_WAIVED, &sha)
    })?;
    Ok(())
}
