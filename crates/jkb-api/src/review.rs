//! A review's database steps (tasks S6.4 stage 5, design-s6-4.md F and G): filing its findings and
//! recording it against a branch.
//!
//! `jkb task review file` and `jkb task review record` resolve the repo, branch and HEAD with git
//! where they run and hand the rest to these ops, so a review logged in the dev container and one
//! logged on the host are the same rows. Pure database work (design H4), like [`crate::sessions`].
//!
//! **Findings are filed as `managed:` tasks, never through a mount.** A mount has the host's sync read
//! and write files at a path the client chose; filing through an op writes nothing but rows.

use std::fmt::Write as _;

use jkb_core::location::{set_facet, valid_ref, FACET_BRANCH};
use jkb_core::query::{Query, Scope};
use jkb_core::{item, mount, ns, tag, task, transition, WriteMeta};
use jkb_types::{ItemId, TaskStatus};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::sessions::{check_name, review_findings};
use crate::tasks::{check_line, line_problem, writable, FileRoots};
use crate::{ApiError, ErrorCode};

/// The branch HEAD a review ran against.
pub const FACET_REVIEWED: &str = "reviewed";
/// The review's findings namespace, so the findings are one `jkb ls` away.
pub const FACET_REVIEW: &str = "review";

/// The most findings one `task.review_file` files. A review has tens; a reviewer that returned
/// thousands is broken, and each finding is a task, a mirror and a changelog entry.
pub const MAX_FINDINGS: usize = 1000;

/// The longest one-line summary a finding may have, in bytes.
pub const MAX_SUMMARY_BYTES: usize = 2048;

/// The longest scenario or fix a finding may carry, in bytes.
pub const MAX_DETAIL_BYTES: usize = 32 * 1024;

/// The most one filing's findings may take, serialized. It is one request, and a client of `jkb serve`
/// sends at most a mebibyte; a filing cannot be split, because a review is filed once. [`fit`] trims a
/// review's longest texts until it fits.
pub const MAX_FILING_BYTES: usize = 768 * 1024;

/// The shortest a scenario or fix is trimmed to by [`fit`].
const MIN_TRIMMED_BYTES: usize = 256;

/// What [`fit`] appends to a text it cut.
pub const TRIMMED: &str = " … (cut to fit one filing; the reviewer's result holds the rest)";

/// Cut `text` to at most `max` bytes, marked with [`TRIMMED`]; whether it was cut.
fn trim(text: &mut String, max: usize) -> bool {
    if text.len() <= max {
        return false;
    }
    let mut end = max.saturating_sub(TRIMMED.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(TRIMMED);
    true
}

/// Fit `findings` to one filing: every summary and file to [`MAX_SUMMARY_BYTES`] and every scenario and fix to
/// [`MAX_DETAIL_BYTES`], then the scenarios and fixes further, halving their allowance each round down
/// to [`MIN_TRIMMED_BYTES`], until the whole serializes within [`MAX_FILING_BYTES`]. Returns whether
/// anything was cut. A review that still does not fit — a thousand long summaries — is left for
/// [`file`] to refuse.
pub fn fit(findings: &mut [Finding]) -> bool {
    let size = |f: &[Finding]| serde_json::to_vec(f).map_or(usize::MAX, |v| v.len());
    let mut cut = false;
    for f in findings.iter_mut() {
        cut |= trim(&mut f.summary, MAX_SUMMARY_BYTES);
        if let Some(file) = &mut f.file {
            cut |= trim(file, MAX_SUMMARY_BYTES);
        }
        for text in [&mut f.scenario, &mut f.fix].into_iter().flatten() {
            cut |= trim(text, MAX_DETAIL_BYTES);
        }
    }
    let mut allowance = MAX_DETAIL_BYTES;
    while size(findings) > MAX_FILING_BYTES && allowance > MIN_TRIMMED_BYTES {
        allowance /= 2;
        for f in findings.iter_mut() {
            for text in [&mut f.scenario, &mut f.fix].into_iter().flatten() {
                cut |= trim(text, allowance);
            }
        }
    }
    cut
}

/// How serious a finding is: where it is filed, and at what priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    /// Blocks landing: priority 1.
    #[serde(rename = "must-fix")]
    MustFix,
    /// Priority 2.
    #[serde(rename = "concern")]
    Concern,
    /// Priority 3.
    #[serde(rename = "nit")]
    Nit,
}

impl Severity {
    /// The section it is filed under, below the review's namespace.
    #[must_use]
    pub const fn section(self) -> &'static str {
        match self {
            Self::MustFix => "must-fix",
            Self::Concern => "concern",
            Self::Nit => "nit",
        }
    }

    /// Its priority. The land gate counts priority 1 and above as must-fix
    /// ([`review_findings`]), so this is what makes a must-fix finding block.
    #[must_use]
    pub const fn priority(self) -> i64 {
        match self {
            Self::MustFix => 1,
            Self::Concern => 2,
            Self::Nit => 3,
        }
    }
}

/// One finding, as the reviewer workflow reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    /// How serious it is.
    pub severity: Severity,
    /// One line saying what is wrong. Line breaks are folded to spaces: it is the task's title.
    pub summary: String,
    /// The file it is in.
    #[serde(default)]
    pub file: Option<String>,
    /// The line it is at.
    #[serde(default)]
    pub line: Option<u64>,
    /// How it goes wrong.
    #[serde(default)]
    pub scenario: Option<String>,
    /// What to do about it.
    #[serde(default)]
    pub fix: Option<String>,
}

impl Finding {
    /// The task body: the summary and location on the first line, then the scenario and the fix.
    fn body(&self) -> String {
        let mut title = fold(&self.summary);
        match (&self.file, self.line) {
            (Some(file), Some(line)) => {
                let _ = write!(title, " — {}:{line}", fold(file));
            }
            (Some(file), None) => {
                let _ = write!(title, " — {}", fold(file));
            }
            _ => {}
        }
        let mut body = title;
        if let Some(s) = self.scenario.as_deref().filter(|s| !s.trim().is_empty()) {
            body.push_str("\n\n");
            body.push_str(s.trim());
        }
        if let Some(f) = self.fix.as_deref().filter(|s| !s.trim().is_empty()) {
            body.push_str("\n\nFix: ");
            body.push_str(f.trim());
        }
        body
    }
}

/// `text` on one line: a line break in a title would make the rest of it the body.
fn fold(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How the review ran, as the reviewer workflow reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRun {
    /// How many reviewers were launched.
    pub reviewers: u64,
    /// How many of them came back.
    pub returned: u64,
    /// Why the review did not run, when it did not.
    #[serde(default)]
    pub error: Option<String>,
}

impl ReviewRun {
    /// Why this run is not a review the land gate may rely on: it reported an error, no reviewer ran,
    /// or not every reviewer came back — a partly read change with no findings reads as clean, and one
    /// with findings as complete.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(e.clone());
        }
        if self.reviewers == 0 {
            return Some("no reviewer read the change".to_owned());
        }
        (self.returned != self.reviewers).then(|| {
            format!(
                "{} of {} reviewers came back, so part of the change was not read",
                self.returned, self.reviewers
            )
        })
    }
}

/// `task.review_file`'s request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAsk {
    /// The review's namespace, e.g. `repos/<repo>/codereviews/<folder>`. Must hold nothing yet.
    pub ns: String,
    /// How the review ran. A review that did not run fully is refused: filed, it would let
    /// `task land` pass work nobody read.
    pub run: ReviewRun,
    /// What the reviewer found. Empty files one finished "clean review" item, so the review is on
    /// record: `task.review_record` refuses a findings namespace that holds nothing.
    pub findings: Vec<Finding>,
}

/// What `task.review_file` filed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filed {
    /// The review's namespace, normalized.
    pub ns: String,
    /// Each finding's task uid, in the order given.
    pub uids: Vec<String>,
    /// No findings were given, so one finished "clean review" item was filed instead.
    pub clean: bool,
}

/// The title of the item a clean review files.
pub const CLEAN_REVIEW: &str = "No findings — clean review";

fn invalid(why: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::Invalid, why)
}

fn check_bytes(what: &str, value: &str, max: usize) -> Result<(), ApiError> {
    if value.len() > max {
        return Err(invalid(format!(
            "a finding's {what} of at most {max} bytes ({} given)",
            value.len()
        )));
    }
    Ok(())
}

/// `task.review_file`: file a review's findings as `managed:` tasks under `ask.ns`, one section per
/// severity (`must-fix`, `concern`, `nit`), each mirrored into `tasks/` like any task homed outside it.
///
/// Refused when the review did not run fully ([`ReviewRun::refusal`]), when the namespace already
/// holds anything — a second filing into one review would mix two runs, and the gate reads them as
/// one — and when a `tasks` mount covers a section, where the host's sync would write the findings
/// into a file.
///
/// # Errors
/// [`ErrorCode::Invalid`] for a malformed or occupied namespace, a covered section, or an oversized
/// finding; or a failed write.
pub fn file(conn: &Connection, meta: &WriteMeta, ask: &FileAsk) -> Result<Filed, ApiError> {
    if let Some(why) = ask.run.refusal() {
        return Err(invalid(format!(
            "this review did not run fully ({why}) — nothing was filed, because it would read as a \
             review of the whole change. Re-run the review."
        )));
    }
    let root = ns::normalize(&ask.ns)?;
    if ask.findings.len() > MAX_FINDINGS {
        return Err(invalid(format!(
            "at most {MAX_FINDINGS} findings in one review ({} given)",
            ask.findings.len()
        )));
    }
    for f in &ask.findings {
        if fold(&f.summary).is_empty() {
            return Err(invalid("a finding with an empty summary"));
        }
        check_bytes("summary", &f.summary, MAX_SUMMARY_BYTES)?;
        check_bytes("file", f.file.as_deref().unwrap_or(""), MAX_SUMMARY_BYTES)?;
        check_bytes(
            "scenario",
            f.scenario.as_deref().unwrap_or(""),
            MAX_DETAIL_BYTES,
        )?;
        check_bytes("fix", f.fix.as_deref().unwrap_or(""), MAX_DETAIL_BYTES)?;
    }
    let size = serde_json::to_vec(&ask.findings).map_or(usize::MAX, |v| v.len());
    if size > MAX_FILING_BYTES {
        return Err(invalid(format!(
            "these findings take {size} bytes and one filing takes at most {MAX_FILING_BYTES}, \
             even with their scenarios and fixes cut short; file fewer findings, or shorter summaries"
        )));
    }
    let held = Query {
        scope: Scope::Subtree(root.clone()),
        ..Query::default()
    }
    .evaluate(conn)?;
    if !held.is_empty() {
        return Err(invalid(format!(
            "`{root}` already holds {} item(s): a review's findings are filed once, into a namespace \
             of their own",
            held.len()
        )));
    }

    let clean = ask.findings.is_empty();
    let entries: Vec<(String, i64, String)> = if clean {
        vec![("summary".to_owned(), 3, CLEAN_REVIEW.to_owned())]
    } else {
        ask.findings
            .iter()
            .map(|f| {
                (
                    f.severity.section().to_owned(),
                    f.severity.priority(),
                    f.body(),
                )
            })
            .collect()
    };
    let mut uids = Vec::with_capacity(entries.len());
    for (i, (section, priority, body)) in entries.into_iter().enumerate() {
        let home = format!("{root}/{section}");
        if let Some(file) = mount::tasks_file_for(conn, &home)? {
            return Err(invalid(format!(
                "`{home}` is written to {file} by a `tasks` mount; file the review into a namespace \
                 no mount covers"
            )));
        }
        let title = body.lines().next().unwrap_or_default().to_owned();
        let uid = format!("{}-{i}", task::mint_uid(&title));
        let mut spec = task::NewTask::new(uid.clone(), body);
        spec.priority = Some(priority);
        spec.home = home;
        let id = task::create(conn, meta, &spec)?;
        if clean {
            task::set_status(conn, meta, id, TaskStatus::Done)?;
        }
        uids.push(uid);
    }
    Ok(Filed {
        ns: root,
        uids,
        clean,
    })
}

/// `task.review_record`'s request: a review of `branch` at `sha`, whose findings are under `findings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordAsk {
    /// The repo key (`repo=`).
    pub repo: String,
    /// The reviewed branch.
    pub branch: String,
    /// The reviewed HEAD, when the client could resolve it.
    #[serde(default)]
    pub sha: Option<String>,
    /// The namespace holding the review's findings.
    pub findings: String,
}

/// One task a `task.review_record` tagged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recorded {
    /// Its uid.
    pub uid: String,
    /// It moved from `in_progress` to `needs_review`.
    pub moved_to_review: bool,
}

/// What a `task.review_record` did, and what it deliberately did not do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recording {
    /// The tasks tagged.
    pub recorded: Vec<Recorded>,
    /// Tasks whose land target is this branch but whose work has not been grafted onto it yet, so the
    /// review cannot have covered them. Reported, because silence here reads as "everything was
    /// tagged".
    pub skipped_unlanded: Vec<String>,
    /// Skipped because a recorded branch value cannot be handed to git at all. Reported, not fatal:
    /// one malformed tag must not stop the whole branch being credited.
    pub unusable: Vec<String>,
    /// Skipped because this client may not write them (filed outside `jkb serve`'s file roots).
    #[serde(default)]
    pub unwritable: Vec<String>,
}

/// The longest SHA a review may record.
const MAX_SHA_BYTES: usize = 64;

/// `task.review_record`: record that a review ran against `ask.branch`, producing findings under
/// `ask.findings` (design D38.4).
///
/// Keyed by **branch**, because that is what a review knows: it reviewed a range on a branch, not a
/// task. Tasks are found through `branch=` (a session's own branch) **and** their branches' recorded
/// land target (the staging branch a batch lands on), so reviewing either level tags the work it
/// covers — see [`credited_by`]. A branch no task claims (trunk, an ad-hoc range) matches nothing and
/// returns an empty recording: reviewing an arbitrary range is a legitimate thing to do.
///
/// **Refused when the findings namespace holds nothing.** A review whose findings never reached the
/// knowledge base — a typo, a namespace renamed since — must never read as a clean review; this is the
/// moment the caller can still fix it, and the gate refuses the same thing later.
///
/// Recording moves `in_progress` to `needs_review` and is the **only** author of that transition
/// (design D38.6). It all happens in the one transaction the op runs in, each task's status read inside
/// it, so a task that landed meanwhile is not moved back, and an interrupted run tags nothing.
///
/// `review=` is **added**, not set: a second run's findings do not retire the first run's still-open
/// must-fix items (the gate unions every recorded namespace). `reviewed=` is set, since there is only
/// one current HEAD.
///
/// # Errors
/// [`ErrorCode::Invalid`] for a malformed ask or an empty findings namespace, a tagged task whose
/// tasks.md line the tags would break, or a failed write.
pub fn record(
    conn: &Connection,
    meta: &WriteMeta,
    ask: &RecordAsk,
    roots: Option<&FileRoots>,
) -> Result<Recording, ApiError> {
    check_name("repo key", &ask.repo)?;
    check_name("branch", &ask.branch)?;
    valid_ref(&ask.branch)?;
    check_name("findings namespace", &ask.findings)?;
    let findings = ns::normalize(&ask.findings)?;
    let sha = ask.sha.as_deref().unwrap_or("unknown");
    if sha.is_empty()
        || sha.len() > MAX_SHA_BYTES
        || !sha.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Err(invalid(format!(
            "a reviewed SHA of 1 to {MAX_SHA_BYTES} letters and digits"
        )));
    }
    if review_findings(conn, std::slice::from_ref(&findings))?.total == 0 {
        return Err(invalid(format!(
            "no findings found under `{findings}` — nothing was recorded. A review whose findings \
             never reached the KB must not be recorded as one: check the namespace exists (`jkb ls \
             {findings}`) and that `jkb task review file` filed the review into it."
        )));
    }

    let mut out = Recording::default();
    let ids = jkb_core::location::tasks_in_repo(&ask.repo).evaluate(conn)?;
    let metas = item::get_many(conn, &ids)?;
    for id in ids {
        let Some(m) = metas.get(&id) else { continue };
        let branches: Vec<String> = tag::applications(conn, id)?
            .into_iter()
            .filter(|(f, _)| f == FACET_BRANCH)
            .map(|(_, v)| v)
            .collect();
        // A branch value git cannot be handed at all costs its own row and no more: failing on one
        // recorded `reviewed=` for NO task, so one malformed tag anywhere in the repo turned every
        // landing in the batch into "never reviewed".
        if branches.iter().any(|b| valid_ref(b).is_err()) {
            out.unusable.push(m.uid.clone());
            continue;
        }
        match credited_by(conn, id, &branches, &ask.branch)? {
            Credit::OwnBranch | Credit::Grafted => {}
            Credit::LandsHereButHasNot => {
                out.skipped_unlanded.push(m.uid.clone());
                continue;
            }
            // Dropped, and that is right: this walks **every** task in the repo, so `Unrelated` is
            // overwhelmingly "records a different branch".
            Credit::Unrelated => continue,
        }
        match writable(conn, &m.uid, roots) {
            Ok(_) => {}
            Err(e) if e.code == ErrorCode::Forbidden => {
                out.unwritable.push(m.uid.clone());
                continue;
            }
            Err(e) => return Err(e),
        }
        let before = line_problem(conn, &m.uid)?;
        set_facet(conn, meta, id, FACET_REVIEWED, sha)?;
        tag::apply(conn, meta, id, FACET_REVIEW, &findings)?;
        let moved = item::get(conn, id)?.and_then(|m| m.status).as_deref() == Some("in_progress");
        if moved {
            task::set_status(conn, meta, id, TaskStatus::NeedsReview)?;
        }
        check_line(conn, &m.uid, before.as_deref())?;
        out.recorded.push(Recorded {
            uid: m.uid.clone(),
            moved_to_review: moved,
        });
    }
    Ok(out)
}

/// Why a review of one branch does — or does not — cover a task.
enum Credit {
    /// The reviewed branch **is** this task's work branch. Covered by definition: the review read it.
    OwnBranch,
    /// jkb itself grafted this task's work onto the reviewed branch, and recorded that it did.
    Grafted,
    /// The task means to land here and has not yet, so the review saw none of its work.
    LandsHereButHasNot,
    /// Nothing to do with this branch.
    Unrelated,
}

/// Whether a review of `branch` covers task `id`, whose recorded branches are `branches`.
///
/// A **recorded event** rather than an inference from the commit graph: jkb performs the graft, so a
/// `land` transition onto the reviewed branch is the answer. A containment probe could not tell a
/// rebased or squashed landing from a branch with no commits at all, and read an empty session as
/// covered.
fn credited_by(
    conn: &Connection,
    id: ItemId,
    branches: &[String],
    branch: &str,
) -> Result<Credit, ApiError> {
    if branches.iter().any(|b| b == branch) {
        return Ok(Credit::OwnBranch);
    }
    let landing = transition::landing(conn, id)?;
    let onto_is_branch = |r: Option<&transition::TransitionRow>| -> bool {
        r.and_then(|r| r.labels.onto.as_deref())
            .is_some_and(|onto| onto == branch)
    };
    // **Present tense first, and it is the one that can credit.** A landing that still speaks for the
    // work means this branch holds what the task is doing now.
    if onto_is_branch(landing.live()) {
        return Ok(Credit::Grafted);
    }
    // **Then the question the present tense cannot answer.** A task still aimed here has work coming
    // that this review has not seen, whatever it grafted before: a task that landed, was reopened for
    // a must-fix, and had its fix committed in a session this branch has never seen must not be
    // stamped `reviewed=` for work nobody reviewed.
    let target = transition::land_target(conn, id)?;
    if target.as_deref() == Some(branch) {
        return Ok(Credit::LandsHereButHasNot);
    }
    // **Only now the historical question**, and only because the task aims nowhere — what `abandon`
    // leaves. A graft does not un-happen, so a session abandoned after its work reached this branch is
    // still covered by a review of it.
    if target.is_none() && onto_is_branch(landing.recorded()) {
        return Ok(Credit::Grafted);
    }
    Ok(Credit::Unrelated)
}

#[cfg(test)]
mod tests;
