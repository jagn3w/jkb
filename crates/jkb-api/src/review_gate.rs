//! The land gate's review rule (design D38.5, D52.6), stated once for every caller that asks it.
//!
//! It lived in the CLI alone, and the daemon trusted whatever the CLI's gate had decided: `task.land`
//! states the review as waived, and `task.landed` states that the caller "performed and gated the
//! graft". For the operator that is the design — the operator may waive the gate — but a swarm
//! coordinator holds the same ops, and the only thing holding it to a reviewer's verdict was a line in
//! its prompt. [`crate::rbac::authorize`] now asks [`landing`] of every landing a non-operator asks
//! for, and the CLI's `jkb task land` and `jkb task landed --check` read [`gate_with`] and
//! [`landing_with`], so the rule a refusal names is the rule that was applied.

use jkb_core::reviews::ReviewState;
use rusqlite::Connection;

use crate::sessions::{review_findings, ReviewFinding, ReviewFindings, MAX_REVIEW_NAMESPACES};
use crate::ApiError;

/// Why a task's review record does not let it land, or [`GateVerdict::Passed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Reviewed, with nothing must-fix outstanding.
    Passed,
    /// No review has been recorded for this task.
    NeverReviewed,
    /// A review is recorded, but its namespace(s) hold no finding items at all. That is not a clean
    /// review — it is a review whose findings never reached the KB (a quarantined `tasks.md`, a
    /// typo'd `--findings`, a namespace renamed since). Treating it as clean is the gate failing
    /// **open**, which is the one direction a safety check must not fail.
    NoFindingsRecorded(Vec<String>),
    /// Reviewed, but the review has open must-fix findings: how many, and the first of them.
    OpenFindings(usize, Vec<ReviewFinding>),
    /// Nothing is open, but the **newest** round found must-fixes (D52.6): fixing them is not a clean
    /// review of the fix. Another round has to come back with none.
    LastRoundNotClean {
        /// That round's namespace.
        ns: String,
        /// How many must-fixes it found.
        must_fix: usize,
    },
    /// The daemon answering is too old to report rounds, so the last-round clause cannot be asked.
    /// Refused rather than skipped: skipping is the gate failing open.
    RoundsUnknown,
    /// The newest recorded review ran against another commit than the one being landed: commits made
    /// after the review are commits no reviewer saw. Only [`landing_with`] asks this.
    StaleHead {
        /// The head the newest recorded review ran against.
        reviewed: String,
        /// The head being landed, when the caller could name it.
        head: Option<String>,
    },
}

impl GateVerdict {
    /// One line: why the gate refuses, or `None` if it passes.
    #[must_use]
    pub fn short(&self) -> Option<String> {
        match self {
            Self::Passed => None,
            Self::NeverReviewed => Some(
                "No review has been recorded. Run /jkb-review in the session, or land with \
                 --no-review."
                    .to_owned(),
            ),
            Self::NoFindingsRecorded(nss) => Some(format!(
                "Its review ({}) holds no findings at all, so they never reached the KB — this \
                 is not a clean review. Re-run /jkb-review.",
                nss.join(", ")
            )),
            Self::OpenFindings(count, _) => Some(format!(
                "Its review left {count} open must-fix finding(s). Fix or cancel each one, then land."
            )),
            Self::LastRoundNotClean { ns, must_fix } => Some(format!(
                "Its newest review round ({ns}) found {must_fix} must-fix finding(s). Fixing them is \
                 not a review of the fix: run another round, which must come back with none."
            )),
            Self::RoundsUnknown => Some(
                "The jkb serve answering is too old to report review rounds, so the last-round \
                 check cannot run. Restart it on this jkb (./scripts/setup.sh on the host)."
                    .to_owned(),
            ),
            Self::StaleHead { reviewed, head } => Some(match head {
                Some(head) => format!(
                    "Its newest review ran against {reviewed}, and the head being landed is {head}: \
                     no reviewer saw the commits since. Review this head."
                ),
                None => format!(
                    "Its newest review ran against {reviewed}, and the head being landed could not \
                     be named, so nothing shows a reviewer saw it."
                ),
            }),
        }
    }
}

/// Decide whether a task's review record permits a landing (design D38.5, D52.6).
///
/// Concerns and nits do not block. A gate everything trips is a gate nobody keeps: a previous run
/// put 34 of 45 findings on `concern`, and blocking on those would make `--no-review` the normal
/// path within a week.
///
/// **Every** recorded round is consulted for open must-fixes, not just the newest: re-running
/// `/review-log` must not silently retire the previous run's still-open must-fix findings. And the
/// **newest** round must itself have found none (D52.6): a round that found must-fixes is not made
/// clean by fixing them — the fix has not been reviewed.
///
/// `found` must have been read from exactly `review.namespaces`.
#[must_use]
pub fn gate_with(found: &ReviewFindings, review: &ReviewState) -> GateVerdict {
    if review.reviewed.is_none() {
        return GateVerdict::NeverReviewed;
    }
    let nss = &review.namespaces;
    if nss.is_empty() || found.total == 0 {
        return GateVerdict::NoFindingsRecorded(nss.clone());
    }
    if found.open_count > 0 {
        return GateVerdict::OpenFindings(found.open_count, found.open_must_fix.clone());
    }
    if !found.rounds_reported {
        return GateVerdict::RoundsUnknown;
    }
    match found.rounds.last() {
        Some(last) if last.must_fix > 0 => GateVerdict::LastRoundNotClean {
            ns: last.ns.clone(),
            must_fix: last.must_fix,
        },
        _ => GateVerdict::Passed,
    }
}

/// [`gate_with`] for a landing of `head` somebody else grafts — the merge queue's: the review must
/// also have run against `head` itself. An operator's waiver of exactly `head` passes it.
#[must_use]
pub fn landing_with(
    found: &ReviewFindings,
    review: &ReviewState,
    head: Option<&str>,
) -> GateVerdict {
    if head.is_some() && review.waived.as_deref() == head {
        return GateVerdict::Passed;
    }
    match gate_with(found, review) {
        GateVerdict::Passed => {}
        refused => return refused,
    }
    match (&review.reviewed, head) {
        (Some(reviewed), Some(head)) if reviewed == head => GateVerdict::Passed,
        (Some(reviewed), head) => GateVerdict::StaleHead {
            reviewed: reviewed.clone(),
            head: head.map(str::to_owned),
        },
        // `gate_with` refused this already.
        (None, _) => GateVerdict::NeverReviewed,
    }
}

/// Add up `parts`, each one answer of [`review_findings`] over a piece of a namespace set no larger
/// than [`MAX_REVIEW_NAMESPACES`]. A long-lived branch's task outgrows one piece; a land refused for
/// that would be a wedge.
#[must_use]
pub fn merge(parts: impl IntoIterator<Item = ReviewFindings>) -> ReviewFindings {
    let mut out = ReviewFindings {
        rounds_reported: true,
        ..ReviewFindings::default()
    };
    for part in parts {
        out.total += part.total;
        out.open_count += part.open_count;
        out.rounds_reported &= part.rounds_reported;
        out.rounds.extend(part.rounds);
        for f in part.open_must_fix {
            if !out.open_must_fix.iter().any(|seen| seen.uid == f.uid) {
                out.open_must_fix.push(f);
            }
        }
    }
    out.rounds.sort_by_key(|r| r.filed);
    out
}

/// [`landing_with`], read from the database: what a landing of `head` for `task` by anyone but the
/// operator must pass.
///
/// # Errors
/// A database error, or the findings cannot be read.
pub fn landing(
    conn: &Connection,
    task: jkb_types::ItemId,
    head: Option<&str>,
) -> Result<GateVerdict, ApiError> {
    let review = jkb_core::reviews::state(conn, task)?;
    let parts = review
        .namespaces
        .chunks(MAX_REVIEW_NAMESPACES)
        .map(|nss| review_findings(conn, nss))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(landing_with(&merge(parts), &review, head))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::ReviewRound;

    fn reviewed(sha: &str) -> ReviewState {
        ReviewState {
            reviewed: Some(sha.to_owned()),
            namespaces: vec!["r/1".to_owned()],
            ..ReviewState::default()
        }
    }

    fn clean() -> ReviewFindings {
        ReviewFindings {
            total: 1,
            rounds: vec![ReviewRound {
                ns: "r/1".to_owned(),
                filed: 1,
                must_fix: 0,
                areas: Vec::new(),
            }],
            rounds_reported: true,
            ..ReviewFindings::default()
        }
    }

    #[test]
    fn a_landing_needs_a_clean_review_of_the_very_head_it_lands() {
        let found = clean();
        assert_eq!(
            landing_with(&found, &reviewed("aaa"), Some("aaa")),
            GateVerdict::Passed
        );
        assert!(matches!(
            landing_with(&found, &reviewed("aaa"), Some("bbb")),
            GateVerdict::StaleHead { .. }
        ));
        assert!(matches!(
            landing_with(&found, &reviewed("aaa"), None),
            GateVerdict::StaleHead { head: None, .. }
        ));
        assert_eq!(
            landing_with(
                &ReviewFindings::default(),
                &ReviewState::default(),
                Some("aaa")
            ),
            GateVerdict::NeverReviewed
        );
        // The review rule itself still applies first: a head-matching round with must-fixes is no
        // approval.
        let mut dirty = clean();
        dirty.rounds[0].must_fix = 2;
        assert!(matches!(
            landing_with(&dirty, &reviewed("aaa"), Some("aaa")),
            GateVerdict::LastRoundNotClean { .. }
        ));
    }

    #[test]
    fn an_operator_waiver_passes_only_the_head_it_names() {
        let waived = ReviewState {
            waived: Some("aaa".to_owned()),
            ..ReviewState::default()
        };
        let none = ReviewFindings::default();
        assert_eq!(
            landing_with(&none, &waived, Some("aaa")),
            GateVerdict::Passed
        );
        assert_eq!(
            landing_with(&none, &waived, Some("bbb")),
            GateVerdict::NeverReviewed
        );
        assert_eq!(
            landing_with(&none, &waived, None),
            GateVerdict::NeverReviewed
        );
    }
}
