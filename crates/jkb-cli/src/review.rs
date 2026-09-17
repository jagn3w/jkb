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

pub(crate) use jkb_api::review::{FACET_REVIEW, FACET_REVIEWED};
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
/// query ([`jkb_api::sessions::review_findings`]), through whichever backend serves this command.
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
        for f in part.open_must_fix {
            if !out.open_must_fix.iter().any(|seen| seen.uid == f.uid) {
                out.open_must_fix.push(f);
            }
        }
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
    let verdict = match gate(kb, tags) {
        Ok(v) => v,
        // A review whose findings cannot be read is not a passed one — and `--no-review` is the
        // operator saying not to ask, so it is not a reason to refuse the waiver either.
        Err(e) if no_review => {
            if !json {
                eprintln!("note: the task's review could not be read ({e:#})");
            }
            GateVerdict::NeverReviewed
        }
        Err(e) => return Err(e),
    };
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

/// The reviewer workflow's result, as `jkb task review file` reads it: its `findings`, each with the
/// fields the op files and whatever else the workflow reports (`kind`, `unverified`), which are ignored.
#[derive(serde::Deserialize)]
struct WorkflowResult {
    findings: Vec<WorkflowFinding>,
}

#[derive(serde::Deserialize)]
struct WorkflowFinding {
    severity: jkb_api::review::Severity,
    summary: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    scenario: Option<String>,
    #[serde(default)]
    fix: Option<String>,
}

/// The largest workflow result `task review file` reads.
const MAX_RESULT_BYTES: u64 = 64 * 1024 * 1024;

/// `jkb task review file` — file a review's findings as tasks (design-s6-4.md F).
///
/// # Errors
/// An unreadable or malformed result, or the op's refusal.
pub(crate) fn file_cmd(
    kb: &crate::session_cli::Kb<'_>,
    findings_ns: &str,
    from: &std::path::Path,
    json: bool,
) -> Result<()> {
    use std::io::Read as _;
    let mut text = String::new();
    let what = if from.as_os_str() == "-" {
        std::io::stdin()
            .take(MAX_RESULT_BYTES)
            .read_to_string(&mut text)
            .context("reading the review result from stdin")?;
        "stdin".to_owned()
    } else {
        std::fs::File::open(from)
            .and_then(|f| f.take(MAX_RESULT_BYTES).read_to_string(&mut text))
            .with_context(|| format!("reading {}", from.display()))?;
        from.display().to_string()
    };
    let result: WorkflowResult = serde_json::from_str(&text).with_context(|| {
        format!(
            "{what} is not a review result: a JSON object with a `findings` array of {{severity              (must-fix|concern|nit), summary, file, line, scenario, fix}}"
        )
    })?;
    let filed = kb.review_file(jkb_api::review::FileAsk {
        ns: findings_ns.to_owned(),
        findings: result
            .findings
            .into_iter()
            .map(|f| jkb_api::review::Finding {
                severity: f.severity,
                summary: f.summary,
                file: f.file,
                line: f.line,
                scenario: f.scenario,
                fix: f.fix,
            })
            .collect(),
    })?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "ns": filed.ns, "uids": filed.uids, "clean": filed.clean })
        );
    } else if filed.clean {
        println!("filed a clean review under {}", filed.ns);
    } else {
        println!("filed {} finding(s) under {}", filed.uids.len(), filed.ns);
        for uid in &filed.uids {
            println!("  {uid}");
        }
    }
    Ok(())
}

/// `jkb task review record` — record that a review ran against a branch (design D38.4): the branch and
/// its HEAD resolved here with git, the rest by `task.review_record`.
///
/// # Errors
/// A git failure, or the op's refusal.
pub(crate) fn record_cmd(
    kb: &crate::session_cli::Kb<'_>,
    branch: Option<String>,
    sha: Option<String>,
    findings: &str,
    json: bool,
) -> Result<()> {
    let ctx = crate::repo::repo_ctx()?;
    let cwd = std::env::current_dir()?;
    let branch = match branch {
        Some(b) => b,
        None => crate::gitrepo::current_branch(&cwd)?
            .context("not on a branch here (detached HEAD?) — pass --branch")?,
    };
    let sha = match sha {
        Some(s) => Some(s),
        None => crate::gitrepo::rev(&ctx.root, &branch)?,
    };
    let recording = kb.review_record(jkb_api::review::RecordAsk {
        repo: ctx.key.clone(),
        branch: branch.clone(),
        sha: sha.clone(),
        findings: findings.to_owned(),
    })?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "branch": branch,
                "sha": sha,
                "findings": findings,
                "tasks": recording.recorded.iter().map(|r| serde_json::json!({
                    "uid": r.uid, "moved_to_review": r.moved_to_review,
                })).collect::<Vec<_>>(),
                "skipped_unlanded": recording.skipped_unlanded,
                "unusable": recording.unusable,
                "unwritable": recording.unwritable,
            })
        );
        return Ok(());
    }
    print_recording(&recording, &branch, sha.as_deref(), findings);
    Ok(())
}

fn print_recording(
    recording: &jkb_api::review::Recording,
    branch: &str,
    sha: Option<&str>,
    findings: &str,
) {
    let jkb_api::review::Recording {
        recorded,
        skipped_unlanded,
        unusable,
        unwritable,
    } = recording;
    if recorded.is_empty() {
        // Reviewing an arbitrary range is a legitimate thing to do, so this is a note and not an error
        // (design D38.4). But "no task records this branch" and "tasks record it and every one was
        // skipped" are different facts.
        if skipped_unlanded.is_empty() && unusable.is_empty() && unwritable.is_empty() {
            println!("no task records branch={branch} — nothing to tag (review still filed)");
        } else {
            println!(
                "nothing tagged for branch={branch} — every matching task was skipped, below                  (review still filed)"
            );
        }
    } else {
        println!(
            "recorded review of {branch}@{} -> {findings}",
            sha.unwrap_or("unknown")
        );
        for r in recorded {
            let moved = if r.moved_to_review {
                " (now needs_review)"
            } else {
                ""
            };
            println!("  {}{moved}", r.uid);
        }
    }
    // Said out loud: silence reads as "everything was tagged", and a task skipped here is one
    // `task land` will refuse as never reviewed.
    let bucket = |title: &str, uids: &[String]| {
        if !uids.is_empty() {
            println!("{title}");
            for uid in uids {
                println!("  {uid}");
            }
        }
    };
    bucket(
        "not tagged — a recorded branch cannot be handed to git at all, so nothing about them could          be checked (`jkb task tag rm <uid> branch=<value>`):",
        unusable,
    );
    bucket(
        "not tagged — filed outside the directories this client may write; record the review on the          host:",
        unwritable,
    );
    if !skipped_unlanded.is_empty() {
        bucket(
            &format!(
                "not tagged — landing on {branch}, but jkb has not grafted their work onto it yet, so                  this review did not see it:"
            ),
            skipped_unlanded,
        );
        println!("  review each in its own session (`/jkb-review-log` there), or land first.");
    }
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
        let f = super::findings_via(&crate::session_cli::Kb::new(&backend), &nss).unwrap();
        assert_eq!((f.total, f.open_count, f.open_must_fix.len()), (2, 2, 2));
    }
}
