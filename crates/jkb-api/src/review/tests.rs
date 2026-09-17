//! Filing and recording a review through the backend.

use serde_json::json;

use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};
use jkb_core::Db;

fn call(b: &LocalBackend, r: serde_json::Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

fn file(
    b: &LocalBackend,
    ns: &str,
    findings: &serde_json::Value,
) -> Result<super::Filed, ApiError> {
    match call(
        b,
        json!({ "op": "task.review_file", "ns": ns, "findings": findings }),
    )? {
        Response::ReviewFiled { filed } => Ok(filed),
        other => panic!("{other:?}"),
    }
}

fn record(b: &LocalBackend, branch: &str, findings: &str) -> Result<super::Recording, ApiError> {
    match call(
        b,
        json!({ "op": "task.review_record", "repo": "proj", "branch": branch,
                "sha": "abc123", "findings": findings }),
    )? {
        Response::ReviewRecorded { recording } => Ok(recording),
        other => panic!("{other:?}"),
    }
}

fn findings(b: &LocalBackend, ns: &str) -> crate::sessions::ReviewFindings {
    match call(
        b,
        json!({ "op": "task.review_findings", "namespaces": [ns] }),
    )
    .unwrap()
    {
        Response::ReviewFindings { findings } => findings,
        other => panic!("{other:?}"),
    }
}

fn show(b: &LocalBackend, uid: &str) -> serde_json::Value {
    match call(b, json!({ "op": "task.show", "uid": uid })).unwrap() {
        Response::Task { task, .. } => serde_json::to_value(task).unwrap(),
        other => panic!("{other:?}"),
    }
}

/// Each finding is a task in its severity's section, at the priority the land gate reads, with the
/// summary and location as its one-line title and the rest as its body.
#[test]
fn findings_are_filed_by_severity_where_the_gate_reads_them() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let filed = file(
        &b,
        "repos/proj/codereviews/r1",
        &json!([
            { "severity": "must-fix", "summary": "breaks\nthings", "file": "src/a.rs", "line": 7,
              "scenario": "when x", "fix": "do y" },
            { "severity": "concern", "summary": "odd" },
            { "severity": "nit", "summary": "typo", "file": "README.md" },
        ]),
    )
    .unwrap();
    assert!(!filed.clean);
    assert_eq!(filed.uids.len(), 3);
    let first = show(&b, &filed.uids[0]);
    assert_eq!(
        first["item"]["content"],
        "breaks things — src/a.rs:7\n\nwhen x\n\nFix: do y"
    );
    assert_eq!(first["item"]["priority"], 1);
    let f = findings(&b, "repos/proj/codereviews/r1");
    assert_eq!((f.total, f.open_count), (3, 1));
    assert_eq!(f.open_must_fix[0].title, "breaks things — src/a.rs:7");
    // Each section holds its own, and each is mirrored into `tasks/` like any task homed outside it.
    for (section, n) in [("must-fix", 1), ("concern", 1), ("nit", 1)] {
        let got = findings(&b, &format!("repos/proj/codereviews/r1/{section}")).total;
        assert_eq!(got, n, "{section}");
        let mirrored = findings(&b, &format!("tasks/proj/codereviews/r1/{section}")).total;
        assert_eq!(mirrored, n, "{section} mirror");
    }
}

/// A clean review still leaves something to record against, and it does not block.
#[test]
fn a_clean_review_files_one_finished_item() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let filed = file(&b, "reviews/clean", &json!([])).unwrap();
    assert!(filed.clean);
    assert_eq!(show(&b, &filed.uids[0])["item"]["status"], "done");
    let f = findings(&b, "reviews/clean");
    assert_eq!((f.total, f.open_count), (1, 0));
}

/// A review is filed once, into a namespace no mount writes to a file, and a malformed finding files
/// nothing.
#[test]
fn a_filing_is_refused_where_it_would_mix_runs_or_reach_a_file() {
    let (db, ..) = crate::tests::mutate_fixture();
    let b = LocalBackend::new(db);
    let one = json!([{ "severity": "nit", "summary": "x" }]);
    file(&b, "reviews/once", &one).unwrap();
    let e = file(&b, "reviews/once", &one).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    assert!(e.message.contains("already holds 1"), "{e:?}");

    // `repos/in` is a `tasks` mount: a finding under it would be written into its tasks.md.
    let e = file(&b, "repos/in/codereviews/r", &one).unwrap_err();
    assert!(e.message.contains("tasks.md"), "{e:?}");
    assert_eq!(findings(&b, "repos/in/codereviews").total, 0);

    for bad in [
        json!([{ "severity": "nit", "summary": " \n " }]),
        json!([{ "severity": "nit", "summary": "x".repeat(super::MAX_SUMMARY_BYTES + 1) }]),
        json!([{ "severity": "nit", "summary": "x", "fix": "y".repeat(super::MAX_DETAIL_BYTES + 1) }]),
        json!(vec![
            json!({ "severity": "nit", "summary": "x" });
            super::MAX_FINDINGS + 1
        ]),
    ] {
        let e = file(&b, "reviews/bad", &bad).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    }
    assert_eq!(findings(&b, "reviews/bad").total, 0);
    // An unknown severity, or a field the op does not take, is not a request at all.
    for bad in [
        json!({ "op": "task.review_file", "ns": "r", "findings": [{ "severity": "major", "summary": "x" }] }),
        json!({ "op": "task.review_file", "ns": "r", "findings": [{ "severity": "nit", "summary": "x", "kind": "bug" }] }),
    ] {
        assert!(serde_json::from_value::<crate::Request>(bad).is_err());
    }
}

fn started(b: &LocalBackend, text: &str, branch: &str) -> String {
    let uid = match call(
        b,
        json!({ "op": "task.add", "text": text, "managed": true }),
    )
    .unwrap()
    {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    call(
        b,
        json!({ "op": "task.start", "uid": uid,
                "take": { "owner": "host:1" },
                "place": { "branch": branch, "repo": "proj", "onto": "batch" } }),
    )
    .unwrap();
    uid
}

/// A recording tags the branch's task and moves it to review; one whose findings never arrived is
/// refused and tags nothing.
#[test]
fn a_review_is_recorded_only_against_findings_that_exist() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = started(&b, "the work", "feat");
    let e = record(&b, "feat", "reviews/missing").unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    assert!(e.message.contains("no findings found"), "{e:?}");
    assert_eq!(show(&b, &uid)["item"]["status"], "in_progress");

    file(
        &b,
        "reviews/r1",
        &json!([{ "severity": "must-fix", "summary": "x" }]),
    )
    .unwrap();
    let got = record(&b, "feat", "reviews/r1").unwrap();
    assert_eq!(
        got.recorded,
        vec![super::Recorded {
            uid: uid.clone(),
            moved_to_review: true
        }]
    );
    let shown = show(&b, &uid);
    assert_eq!(shown["item"]["status"], "needs_review");
    let tags = shown["item"]["tags"].to_string();
    assert!(
        tags.contains("abc123") && tags.contains("reviews/r1"),
        "{tags}"
    );

    // A task landing on the reviewed branch whose work has not been grafted there is reported, and
    // a branch nobody records tags nothing.
    let got = record(&b, "batch", "reviews/r1").unwrap();
    assert_eq!(got.skipped_unlanded, vec![uid]);
    assert!(got.recorded.is_empty());
    assert_eq!(
        record(&b, "main", "reviews/r1").unwrap(),
        super::Recording::default()
    );

    for (branch, sha) in [("-bad", "abc"), ("feat", "not a sha"), ("feat", "")] {
        let e = call(
            &b,
            json!({ "op": "task.review_record", "repo": "proj", "branch": branch, "sha": sha,
                    "findings": "reviews/r1" }),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{branch} {sha}: {e:?}");
    }
}

/// A client under file roots records the review on what it may write and names what it may not.
#[test]
fn a_rooted_recording_skips_the_tasks_its_client_may_not_write() {
    let (db, inside, outside, _) = crate::tests::mutate_fixture();
    let host = LocalBackend::new(db.clone());
    for uid in [&inside, &outside] {
        call(
            &host,
            json!({ "op": "task.start", "uid": uid, "take": { "owner": "host:1" },
                    "place": { "branch": "feat", "repo": "proj", "onto": "batch" } }),
        )
        .unwrap();
    }
    file(
        &host,
        "reviews/r",
        &json!([{ "severity": "nit", "summary": "x" }]),
    )
    .unwrap();
    let got = record(&crate::tests::rooted(&db), "feat", "reviews/r").unwrap();
    assert_eq!(
        got.recorded.iter().map(|r| &r.uid).collect::<Vec<_>>(),
        vec![&inside]
    );
    assert_eq!(got.unwritable, vec![outside.clone()]);
    assert_eq!(show(&host, &outside)["item"]["status"], "in_progress");
}

/// A review too large for one request is refused whole, and `fit` trims its longest texts until it
/// files — marking each cut.
#[test]
fn a_review_larger_than_one_filing_is_trimmed_to_fit() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let long = "x".repeat(7 * 1024);
    let mut findings: Vec<super::Finding> = (0..150)
        .map(|i| super::Finding {
            severity: super::Severity::Concern,
            summary: format!("finding {i}"),
            file: None,
            line: None,
            scenario: Some(long.clone()),
            fix: Some("é".repeat(10)),
        })
        .collect();
    let e = file(&b, "reviews/big", &serde_json::to_value(&findings).unwrap()).unwrap_err();
    assert!(e.message.contains("one filing takes at most"), "{e:?}");
    assert!(super::fit(&mut findings));
    assert!(serde_json::to_vec(&findings).unwrap().len() <= super::MAX_FILING_BYTES);
    let scenario = findings[0].scenario.as_deref().unwrap();
    assert!(
        scenario.ends_with(super::TRIMMED),
        "{}",
        &scenario[scenario.len() - 80..]
    );
    assert_eq!(
        findings[0].fix.as_deref(),
        Some("é".repeat(10).as_str()),
        "a short text is kept"
    );
    let filed = file(&b, "reviews/big", &serde_json::to_value(&findings).unwrap()).unwrap();
    assert_eq!(filed.uids.len(), 150);
    // A review that already fits is left alone.
    let mut small = findings[..1].to_vec();
    small[0].scenario = Some("s".to_owned());
    assert!(!super::fit(&mut small));
}
