//! Filing and recording a review through the backend.

use serde_json::json;

use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};
use jkb_core::Db;

fn facts(b: &LocalBackend, uid: &str) -> crate::sessions::TaskState {
    match call(b, json!({ "op": "task.facts", "uid": uid })).unwrap() {
        Response::TaskState { state } => state,
        other => panic!("{other:?}"),
    }
}

fn call(b: &LocalBackend, r: serde_json::Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(crate::test_repo::in_repo(b, r)).expect("request parses"))
}

fn file(
    b: &LocalBackend,
    ns: &str,
    findings: &serde_json::Value,
) -> Result<super::Filed, ApiError> {
    match call(
        b,
        json!({ "op": "task.review_file", "run": { "reviewers": 1, "returned": 1 }, "ns": ns, "findings": findings }),
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
    crate::test_repo::know(&b.db, &["proj"]);
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

/// Every item a filing makes says it is a review finding (`finding=review`), so a frontier query
/// leaves findings out by `-tag:` wherever they are placed or mirrored, and keeps every other task.
#[test]
fn every_filed_finding_is_tagged_so_a_frontier_query_leaves_it_out() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    crate::test_repo::know(&b.db, &["proj"]);
    let work = match call(&b, json!({ "op": "task.add", "text": "ordinary work" })).unwrap() {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    let filed = file(
        &b,
        "repos/proj/codereviews/r1",
        &json!([
            { "severity": "must-fix", "summary": "breaks", "file": "src/a.rs" },
            { "severity": "concern", "summary": "odd" },
            { "severity": "nit", "summary": "typo" },
        ]),
    )
    .unwrap();
    let clean = file(&b, "repos/proj/codereviews/r2", &json!([])).unwrap();
    let (facet, value) = jkb_core::reviews::FINDING_TAG;
    for uid in filed.uids.iter().chain(&clean.uids) {
        let tags = &show(&b, uid)["item"]["tags"];
        assert!(
            tags.as_array()
                .unwrap()
                .iter()
                .any(|t| t["facet"] == facet && t["value"] == value),
            "{uid}: {tags}"
        );
    }
    let ready = |dsl: &str| {
        serde_json::to_string(
            &call(&b, json!({ "op": "task.ready", "dsl": dsl, "limit": 100 })).unwrap(),
        )
        .unwrap()
    };
    // The mirror under `tasks/` offers them, as a swarm's scope does, until the query leaves them out.
    let all = ready("ns:tasks/**");
    assert!(filed.uids.iter().all(|u| all.contains(u)), "{all}");
    let frontier = ready(&format!("ns:tasks/** -tag:{facet}={value}"));
    assert!(frontier.contains(&work), "{frontier}");
    assert!(
        !filed.uids.iter().any(|u| frontier.contains(u)),
        "{frontier}"
    );
}

/// Only a review every reviewer came back from is filed.
#[test]
fn only_a_review_that_ran_fully_is_filed() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    for (i, run) in [
        json!({ "reviewers": 3, "returned": 2 }),
        json!({ "reviewers": 0, "returned": 0 }),
        json!({ "reviewers": 1, "returned": 1, "error": "survey failed" }),
    ]
    .into_iter()
    .enumerate()
    {
        let ns = format!("repos/t/codereviews/partial{i}");
        let e = call(
            &b,
            json!({ "op": "task.review_file", "run": run, "ns": ns,
                    "findings": [{ "severity": "must-fix", "summary": "x" }] }),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{run}: {e:?}");
        assert!(e.message.contains("did not run fully"), "{e:?}");
        assert_eq!(findings(&b, &ns).total, 0);
    }
    // A request that does not say how the review ran is not a request at all.
    assert!(serde_json::from_value::<crate::Request>(
        json!({ "op": "task.review_file", "ns": "r", "findings": [] })
    )
    .is_err());
}

/// A clean review still leaves something to record against, and it does not block.
#[test]
fn a_clean_review_files_one_finished_item() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    crate::test_repo::know(&b.db, &["t"]);
    let filed = file(&b, "repos/t/codereviews/clean", &json!([])).unwrap();
    assert!(filed.clean);
    assert_eq!(show(&b, &filed.uids[0])["item"]["status"], "done");
    let f = findings(&b, "repos/t/codereviews/clean");
    assert_eq!((f.total, f.open_count), (1, 0));
}

/// A review is filed once, into a namespace no mount writes to a file, and a malformed finding files
/// nothing.
#[test]
fn a_filing_is_refused_where_it_would_mix_runs_or_reach_a_file() {
    let (db, ..) = crate::tests::mutate_fixture();
    let b = LocalBackend::new(db);
    let one = json!([{ "severity": "nit", "summary": "x" }]);
    file(&b, "repos/t/codereviews/once", &one).unwrap();
    let e = file(&b, "repos/t/codereviews/once", &one).unwrap_err();
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
        let e = file(&b, "repos/t/codereviews/bad", &bad).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    }
    assert_eq!(findings(&b, "repos/t/codereviews/bad").total, 0);
    // An unknown severity, or a field the op does not take, is not a request at all.
    for bad in [
        json!({ "op": "task.review_file", "run": { "reviewers": 1, "returned": 1 }, "ns": "r", "findings": [{ "severity": "major", "summary": "x" }] }),
        json!({ "op": "task.review_file", "run": { "reviewers": 1, "returned": 1 }, "ns": "r", "findings": [{ "severity": "nit", "summary": "x", "kind": "bug" }] }),
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
    let e = record(&b, "feat", "repos/t/codereviews/missing").unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    assert!(e.message.contains("no findings found"), "{e:?}");
    assert_eq!(show(&b, &uid)["item"]["status"], "in_progress");

    file(
        &b,
        "repos/t/codereviews/r1",
        &json!([{ "severity": "must-fix", "summary": "x" }]),
    )
    .unwrap();
    let got = record(&b, "feat", "repos/t/codereviews/r1").unwrap();
    assert_eq!(
        got.recorded,
        vec![super::Recorded {
            uid: uid.clone(),
            moved_to_review: true
        }]
    );
    let shown = show(&b, &uid);
    assert_eq!(shown["item"]["status"], "needs_review");
    let review = facts(&b, &uid).review;
    assert_eq!(review.reviewed.as_deref(), Some("abc123"));
    assert_eq!(review.namespaces, vec!["repos/t/codereviews/r1".to_owned()]);
    let tags = shown["item"]["tags"].to_string();
    assert!(
        !tags.contains("abc123") && !tags.contains("repos/t/codereviews/r1"),
        "the review record is not a tag (D52.7): {tags}"
    );

    // A tag spelled like the old record is ordinary content: it waives nothing, reviews nothing.
    call(
        &b,
        json!({ "op": "task.tag", "uid": uid, "facet_value": "review-waived=forged", "mode": "add" }),
    )
    .unwrap();
    assert_eq!(facts(&b, &uid).review.waived, None);
    call(
        &b,
        json!({ "op": "task.review_waive", "uid": uid, "sha": "def456" }),
    )
    .unwrap();
    assert_eq!(facts(&b, &uid).review.waived.as_deref(), Some("def456"));

    // A task landing on the reviewed branch whose work has not been grafted there is reported, and
    // a branch nobody records tags nothing.
    let got = record(&b, "batch", "repos/t/codereviews/r1").unwrap();
    assert_eq!(got.skipped_unlanded, vec![uid]);
    assert!(got.recorded.is_empty());
    assert_eq!(
        record(&b, "main", "repos/t/codereviews/r1").unwrap(),
        super::Recording::default()
    );

    for (branch, sha) in [("-bad", "abc"), ("feat", "not a sha"), ("feat", "")] {
        let e = call(
            &b,
            json!({ "op": "task.review_record", "repo": "proj", "branch": branch, "sha": sha,
                    "findings": "repos/t/codereviews/r1" }),
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
        "repos/t/codereviews/r",
        &json!([{ "severity": "nit", "summary": "x" }]),
    )
    .unwrap();
    let got = record(&crate::tests::rooted(&db), "feat", "repos/t/codereviews/r").unwrap();
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
    crate::test_repo::know(&b.db, &["t"]);
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
    let e = file(
        &b,
        "repos/t/codereviews/big",
        &serde_json::to_value(&findings).unwrap(),
    )
    .unwrap_err();
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
    let filed = file(
        &b,
        "repos/t/codereviews/big",
        &serde_json::to_value(&findings).unwrap(),
    )
    .unwrap();
    assert_eq!(filed.uids.len(), 150);
    // A single field past its own cap is cut to it, however small the whole review.
    let mut one = vec![super::Finding {
        severity: super::Severity::Nit,
        summary: "s".repeat(super::MAX_SUMMARY_BYTES + 10),
        file: Some("f".repeat(super::MAX_SUMMARY_BYTES + 10)),
        line: None,
        scenario: Some("z".repeat(super::MAX_DETAIL_BYTES + 10)),
        fix: None,
    }];
    assert!(super::fit(&mut one));
    assert!(one[0].summary.len() <= super::MAX_SUMMARY_BYTES);
    assert!(one[0].scenario.as_ref().unwrap().len() <= super::MAX_DETAIL_BYTES);
    file(
        &b,
        "repos/t/codereviews/one",
        &serde_json::to_value(&one).unwrap(),
    )
    .unwrap();
    // A review that already fits is left alone.
    let mut small = findings[..1].to_vec();
    small[0].scenario = Some("s".to_owned());
    assert!(!super::fit(&mut small));
}

/// A caller held to one task records a review for that task only, even when another task records the
/// same branch (D52.4): a round recorded against someone else's work is a review it did not get.
#[test]
fn a_scoped_caller_s_review_credits_only_its_own_task() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&b.db, &["proj"]);
    let mine = started(&b, "mine", "shared");
    let theirs = started(&b, "theirs", "shared");
    let token = match call(
        &b,
        json!({ "op": "role.grant", "role": "reviewer", "task": mine, "agent": "r" }),
    )
    .unwrap()
    {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    let scoped = LocalBackend::new(db).with_caller(crate::rbac::Caller::Token(token));
    file(
        &scoped,
        "repos/proj/codereviews/s1",
        &json!([{ "severity": "nit", "summary": "x" }]),
    )
    .unwrap();
    let got = record(&scoped, "shared", "repos/proj/codereviews/s1").unwrap();
    assert_eq!(
        got.recorded
            .iter()
            .map(|r| r.uid.as_str())
            .collect::<Vec<_>>(),
        vec![mine.as_str()]
    );
    assert_eq!(got.unwritable, vec![theirs], "skipped, and said so");
}

/// A non-operator records only a namespace a filing made, and the gate counts the round as it was
/// recorded: lowering a must-fix's priority afterwards does not clear it.
#[test]
fn a_non_operator_records_only_a_filing_and_the_gate_reads_the_recorded_round() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&b.db, &["t", "proj"]);
    let mine = started(&b, "mine", "feat");
    call(
        &b,
        json!({ "op": "task.add", "text": "by hand !p1", "home": "repos/t/codereviews/hand", "managed": true }),
    )
    .unwrap();
    let token = match call(
        &b,
        json!({ "op": "role.grant", "role": "reviewer", "task": mine, "agent": "r" }),
    )
    .unwrap()
    {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    // A coordinator drives the work; it neither files nor records the review of it.
    let coord = match call(
        &b,
        json!({ "op": "role.grant", "role": "coordinator", "agent": "c" }),
    )
    .unwrap()
    {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    let coordinator = LocalBackend::new(db.clone()).with_caller(crate::rbac::Caller::Token(coord));
    let e = file(&coordinator, "repos/proj/codereviews/self", &json!([])).unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
    assert!(e.message.contains("reviewer"), "{e:?}");
    let scoped = LocalBackend::new(db).with_caller(crate::rbac::Caller::Token(token));
    let e = record(&scoped, "feat", "repos/t/codereviews/hand").unwrap_err();
    assert!(e.message.contains("not a namespace"), "{e:?}");
    // The operator's `/review-log` path (a mount) still records any namespace.
    record(&b, "feat", "repos/t/codereviews/hand").unwrap();

    // Someone else's filing — the operator's here — is not this caller's to record.
    file(
        &b,
        "repos/t/codereviews/op",
        &json!([{ "severity": "nit", "summary": "theirs" }]),
    )
    .unwrap();
    let e = record(&scoped, "feat", "repos/t/codereviews/op").unwrap_err();
    assert!(e.message.contains("this caller filed"), "{e:?}");
    // It files only under its repository's codereviews.
    let e = file(
        &scoped,
        "tasks/elsewhere",
        &json!([{ "severity": "must-fix", "summary": "do this" }]),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");

    let ns = "repos/proj/codereviews/f";
    let filed = file(
        &scoped,
        ns,
        &json!([{ "severity": "must-fix", "summary": "bad", "file": "src/a.rs" }]),
    )
    .unwrap();
    record(&scoped, "feat", ns).unwrap();
    call(
        &b,
        json!({ "op": "task.set", "uid": filed.uids[0], "priority": 3 }),
    )
    .unwrap();
    let f = findings(&b, ns);
    assert_eq!(f.open_count, 1, "still the must-fix it was recorded as");
    assert_eq!(f.rounds[0].must_fix, 1);
    // Nothing added to a recorded round afterwards counts, and nothing may be filed inside it.
    call(
        &b,
        json!({ "op": "task.add", "text": "late !p0", "home": format!("{ns}/must-fix"), "managed": true }),
    )
    .unwrap();
    assert_eq!(
        findings(&b, ns).open_count,
        1,
        "a late line is not the round's"
    );
    let e = file(
        &b,
        &format!("{ns}/more"),
        &json!([{ "severity": "must-fix", "summary": "inside" }]),
    )
    .unwrap_err();
    assert!(
        e.message.contains("overlaps the recorded review round"),
        "{e:?}"
    );
}

// ---- findings filed beside the reviewed task, under its plan step ----

use jkb_core::design::plan;
use jkb_core::{containment, item};
use jkb_types::ItemId;

/// A design in repo `proj` with a one-step plan: the step's `(uid, id)`.
fn a_step(db: &Db, title: &str) -> (String, ItemId) {
    let title = title.to_owned();
    db.write_txn("t", move |c, m| {
        let d = jkb_core::design::create(c, m, "proj", &title, "Build it.")?.uid;
        let p = plan::create(c, m, &d, &title, &["one".to_owned()])?;
        let uid = p.steps[0].uid.clone();
        let id = item::id_for_uid(c, &uid)?.unwrap();
        Ok((uid, id))
    })
    .unwrap()
}

/// A task added under `under`, recording `repo=proj` as a started task does.
fn added_under(b: &LocalBackend, text: &str, under: &str) -> String {
    let uid = match call(b, json!({ "op": "task.add", "text": text, "under": under })).unwrap() {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    call(
        b,
        json!({ "op": "task.tag", "uid": uid, "facet_value": "repo=proj", "mode": "add" }),
    )
    .unwrap();
    uid
}

/// A backend held to task `uid`, as a bound reviewer is.
fn reviewer_of(db: &Db, uid: &str) -> LocalBackend {
    let b = LocalBackend::new(db.clone());
    let token = match call(
        &b,
        json!({ "op": "role.grant", "role": "reviewer", "task": uid, "agent": "r" }),
    )
    .unwrap()
    {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    LocalBackend::new(db.clone()).with_caller(crate::rbac::Caller::Token(token))
}

fn id_of(db: &Db, uid: &str) -> ItemId {
    let uid = uid.to_owned();
    db.read(move |c| item::id_for_uid(c, &uid))
        .unwrap()
        .unwrap()
}

fn container(db: &Db, uid: &str) -> Option<ItemId> {
    let id = id_of(db, uid);
    db.read(move |c| containment::parent(c, id)).unwrap()
}

fn inbox(db: &Db) -> ItemId {
    id_of(db, &plan::triage_step_uid("proj"))
}

/// The bodies of a nit, a concern and a must-fix.
fn three() -> serde_json::Value {
    json!([
        { "severity": "nit", "summary": "typo" },
        { "severity": "concern", "summary": "odd" },
        { "severity": "must-fix", "summary": "breaks", "file": "src/a.rs" },
    ])
}

/// Test 1: a reviewer bound to T, under step S, files its findings under S — beside T — and says so;
/// nothing it filed is unplanned.
#[test]
fn a_bound_reviewer_files_under_the_reviewed_tasks_step() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&db, &["proj"]);
    let (step_uid, step) = a_step(&db, "Planned");
    let t = added_under(&b, "the work", &step_uid);
    let filed = file(&reviewer_of(&db, &t), "repos/proj/codereviews/r1", &three()).unwrap();
    assert_eq!(filed.step, step_uid);
    for uid in &filed.uids {
        assert_eq!(container(&db, uid), Some(step), "{uid}");
    }
    assert!(db.read(plan::unplanned).unwrap().is_empty());
}

/// Test 2: reviewing a subtask T of task P, the findings go under the step above both — never under P
/// or T, where they would hold either off the frontier and its landing.
#[test]
fn a_subtasks_findings_go_under_the_step_never_under_a_task() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&db, &["proj"]);
    let (step_uid, step) = a_step(&db, "Nested");
    let p = added_under(&b, "the parent", &step_uid);
    let t = added_under(&b, "the subtask", &p);
    let (pid, tid) = (id_of(&db, &p), id_of(&db, &t));
    let holds = |db: &Db| {
        db.read(move |c| {
            let ready = jkb_core::task::ready_ids(c, jkb_core::query::Query::default(), None)?;
            Ok((
                jkb_core::task::subtasks_all_terminal(c, pid)?,
                jkb_core::task::subtasks_all_terminal(c, tid)?,
                ready.contains(&pid),
                ready.contains(&tid),
            ))
        })
        .unwrap()
    };
    let before = holds(&db);
    assert!(
        before.1 && before.3,
        "T has no subtasks and is ready: {before:?}"
    );
    let filed = file(&reviewer_of(&db, &t), "repos/proj/codereviews/r1", &three()).unwrap();
    for uid in &filed.uids {
        assert_eq!(container(&db, uid), Some(step), "{uid}");
    }
    assert_eq!(holds(&db), before, "the findings hold neither P nor T");
}

/// Test 3: a reviewed task under no step (a loose task from before every task had one) sends its
/// findings to the repo's Open work › Inbox.
#[test]
fn a_loose_reviewed_tasks_findings_go_to_the_inbox() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&db, &["proj"]);
    let t = started(&b, "loose work", "feat");
    let tid = id_of(&db, &t);
    db.write_txn("t", move |c, _| {
        c.execute(
            "DELETE FROM containment WHERE child_item_id = ?1",
            [tid.get()],
        )?;
        c.execute(
            "DELETE FROM edges WHERE dst_item_id = ?1 AND type = 'parent_of'",
            [tid.get()],
        )?;
        Ok(())
    })
    .unwrap();
    let filed = file(&reviewer_of(&db, &t), "repos/proj/codereviews/r1", &three()).unwrap();
    assert_eq!(filed.step, plan::triage_step_uid("proj"));
    let inbox = inbox(&db);
    for uid in &filed.uids {
        assert_eq!(container(&db, uid), Some(inbox), "{uid}");
    }
}

fn file_for(
    b: &LocalBackend,
    ns: &str,
    task: &str,
    findings: &serde_json::Value,
) -> Result<super::Filed, ApiError> {
    match call(
        b,
        json!({ "op": "task.review_file", "run": { "reviewers": 1, "returned": 1 }, "ns": ns,
                "findings": findings, "task": task }),
    )? {
        Response::ReviewFiled { filed } => Ok(filed),
        other => panic!("{other:?}"),
    }
}

fn changelog_rows(db: &Db) -> i64 {
    db.read(|c| Ok(c.query_row("SELECT count(*) FROM changelog", [], |r| r.get(0))?))
        .unwrap()
}

/// Test 4: an unbound filing names its task with `task`; without one it goes to the Inbox; an unknown
/// task files nothing; and a caller held to one task cannot name another.
#[test]
fn an_unbound_filing_names_its_task_and_a_bound_one_cannot_name_another() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&db, &["proj"]);
    let (step_uid, step) = a_step(&db, "Named");
    let t = added_under(&b, "the work", &step_uid);
    let u = added_under(&b, "other work", &step_uid);

    let named = file_for(&b, "repos/proj/codereviews/named", &t, &three()).unwrap();
    assert_eq!(named.step, step_uid);
    assert!(named.uids.iter().all(|u| container(&db, u) == Some(step)));

    let unnamed = file(&b, "repos/proj/codereviews/unnamed", &three()).unwrap();
    assert_eq!(unnamed.step, plan::triage_step_uid("proj"));
    let inbox = inbox(&db);
    assert!(unnamed
        .uids
        .iter()
        .all(|u| container(&db, u) == Some(inbox)));

    let rows = changelog_rows(&db);
    let e = file_for(&b, "repos/proj/codereviews/unknown", "task:nope", &three()).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    assert_eq!(findings(&b, "repos/proj/codereviews/unknown").total, 0);
    assert_eq!(changelog_rows(&db), rows, "nothing was written");
    let e = file_for(&b, "repos/proj/codereviews/step", &step_uid, &three()).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "a step is not a task: {e:?}");

    let e = file_for(
        &reviewer_of(&db, &t),
        "repos/proj/codereviews/other",
        &u,
        &three(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
    assert!(e.message.contains(&t), "{e:?}");
    assert_eq!(findings(&b, "repos/proj/codereviews/other").total, 0);
    // Naming its own task is what binding already says.
    file_for(
        &reviewer_of(&db, &t),
        "repos/proj/codereviews/own",
        &t,
        &three(),
    )
    .unwrap();
}

/// Test 5: beside the task, the gate still reads the round's snapshot: open nits and concerns with the
/// must-fix closed admit; an open must-fix refuses, wherever it has been moved.
#[test]
fn the_gate_reads_the_round_not_where_its_findings_sit() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&db, &["proj"]);
    let (step_uid, _) = a_step(&db, "Gated");
    let t = added_under(&b, "the work", &step_uid);
    call(
        &b,
        json!({ "op": "task.start", "uid": t, "take": { "owner": "host:1" },
                "place": { "branch": "feat", "repo": "proj", "onto": "batch" } }),
    )
    .unwrap();
    let scoped = reviewer_of(&db, &t);
    let ns = "repos/proj/codereviews/r1";
    let filed = file(&scoped, ns, &three()).unwrap();
    record(&scoped, "feat", ns).unwrap();
    call(
        &b,
        json!({ "op": "task.set", "uid": filed.uids[2], "status": "done" }),
    )
    .unwrap();
    let f = findings(&b, ns);
    assert!(f.open_must_fix.is_empty(), "{f:?}");
    for uid in &filed.uids[..2] {
        assert_eq!(
            show(&b, uid)["item"]["status"],
            "open",
            "the nit and the concern stay open"
        );
    }
    let open = facts(&b, &t).open_subtasks;
    assert!(!open, "the findings are no subtasks of the task");

    let ns = "repos/proj/codereviews/r2";
    let filed = file(&scoped, ns, &three()).unwrap();
    record(&scoped, "feat", ns).unwrap();
    assert_eq!(findings(&b, ns).open_must_fix.len(), 1);
    db.write_txn("t", |c, m| plan::triage_step(c, m, "proj"))
        .unwrap();
    call(
        &b,
        json!({ "op": "task.move", "uid": filed.uids[2], "under": plan::triage_step_uid("proj") }),
    )
    .unwrap();
    assert_eq!(container(&db, &filed.uids[2]), Some(inbox(&db)));
    assert_eq!(
        findings(&b, ns).open_must_fix.len(),
        1,
        "moved, it still blocks"
    );
}

// ---- `found_in`: each finding names the task it was found in, for display only ----

/// A design in repo `proj` with a one-step plan: `(design uid, step uid)`.
fn a_design(db: &Db, title: &str) -> (String, String) {
    let title = title.to_owned();
    db.write_txn("t", move |c, m| {
        let d = jkb_core::design::create(c, m, "proj", &title, "Build it.")?.uid;
        let p = plan::create(c, m, &d, &title, &["one".to_owned()])?;
        Ok((d, p.steps[0].uid.clone()))
    })
    .unwrap()
}

/// The uids of the tasks `uid` was found in.
fn found_in_of(db: &Db, uid: &str) -> Vec<String> {
    let id = id_of(db, uid);
    db.read(move |c| {
        let dsts = jkb_core::edge::edges_from(c, id, jkb_types::EdgeType::FoundIn)?;
        let metas = item::get_many(c, &dsts)?;
        Ok(dsts.iter().map(|d| metas[d].uid.clone()).collect())
    })
    .unwrap()
}

fn found_in_count(db: &Db) -> i64 {
    db.read(|c| {
        Ok(c.query_row(
            "SELECT count(*) FROM edges WHERE type = 'found_in'",
            [],
            |r| r.get(0),
        )?)
    })
    .unwrap()
}

/// Test 1: a filing links every item it files `found_in` the scoped task, or the task the ask names
/// when unscoped — the clean-review item too; an unbound filing links nothing; and an undo of the
/// filing removes the findings with their edges.
#[test]
fn a_filing_links_its_findings_to_the_reviewed_task() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&db, &["proj"]);
    let (_, step_uid) = a_design(&db, "Linked");
    let t = added_under(&b, "the work", &step_uid);
    let u = added_under(&b, "other work", &step_uid);

    let scoped = file(&reviewer_of(&db, &t), "repos/proj/codereviews/r1", &three()).unwrap();
    for f in &scoped.uids {
        assert_eq!(found_in_of(&db, f), [t.as_str()], "{f}");
    }
    let named = file_for(&b, "repos/proj/codereviews/r2", &u, &three()).unwrap();
    for f in &named.uids {
        assert_eq!(found_in_of(&db, f), [u.as_str()], "{f}");
    }
    let clean = file_for(&b, "repos/proj/codereviews/r3", &u, &json!([])).unwrap();
    assert!(clean.clean);
    assert_eq!(
        found_in_of(&db, &clean.uids[0]),
        [u.as_str()],
        "the clean item"
    );

    let before = found_in_count(&db);
    let unbound = file(&b, "repos/proj/codereviews/r4", &three()).unwrap();
    assert_eq!(
        found_in_count(&db),
        before,
        "an unbound filing links nothing"
    );
    assert!(found_in_of(&db, &unbound.uids[0]).is_empty());

    let named = file_for(&b, "repos/proj/codereviews/r5", &t, &three()).unwrap();
    assert_eq!(found_in_count(&db), before + 3);
    let txn: i64 = db
        .read(|c| Ok(c.query_row("SELECT max(txn_id) FROM changelog", [], |r| r.get(0))?))
        .unwrap();
    db.write_txn("t", move |c, m| jkb_core::undo::undo(c, m, txn))
        .unwrap();
    assert_eq!(found_in_count(&db), before, "the undo took the edges");
    for f in &named.uids {
        let f = f.clone();
        assert!(
            db.read(move |c| item::id_for_uid(c, &f)).unwrap().is_none(),
            "and the findings"
        );
    }
}

/// Test 3 at the op level, the load-bearing guarantee: a task with an open must-fix and an open nit
/// linked `found_in` it — filed beside it and not recorded — is on the frontier, unblocked, passes
/// `task.land_check`, and `task.land` admits it: no open-subtask refusal. The must-fix only refuses
/// through the round snapshot once recorded, which an unrecorded filing has none of.
#[test]
fn open_findings_found_in_a_task_leave_it_landable() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&db, &["proj"]);
    let (_, step_uid) = a_design(&db, "Landable");
    let t = added_under(&b, "the work", &step_uid);
    let filed = file_for(
        &b,
        "repos/proj/codereviews/r1",
        &t,
        &json!([
            { "severity": "must-fix", "summary": "breaks" },
            { "severity": "nit", "summary": "typo" },
        ]),
    )
    .unwrap();
    for f in &filed.uids {
        assert_eq!(found_in_of(&db, f), [t.as_str()]);
        assert_eq!(show(&b, f)["item"]["status"], "open");
    }
    let tid = id_of(&db, &t);
    let (blocked, ready) = db
        .read(move |c| {
            Ok((
                jkb_core::task::is_blocked(c, tid)?,
                jkb_core::task::ready_ids(c, jkb_core::query::Query::default(), None)?
                    .contains(&tid),
            ))
        })
        .unwrap();
    assert!(!blocked, "is_blocked");
    assert!(ready, "on the frontier");
    call(
        &b,
        json!({ "op": "task.start", "uid": t, "take": { "owner": "host:1" },
                "place": { "branch": "feat", "repo": "proj", "onto": "batch" } }),
    )
    .unwrap();
    assert!(!facts(&b, &t).open_subtasks, "open_subtasks");
    assert!(
        show(&b, &t)["review"]["namespaces"].is_null(),
        "unrecorded: no round the gate could read"
    );
    call(
        &b,
        json!({ "op": "task.land_check", "uid": t, "landed": { "branch": "feat", "onto": "batch" } }),
    )
    .expect("land_check admits it");
    let landed = match call(
        &b,
        json!({ "op": "task.land", "uid": t, "landed": { "branch": "feat", "onto": "batch" } }),
    )
    .unwrap()
    {
        Response::Landing { landing } => landing,
        other => panic!("{other:?}"),
    };
    assert_eq!(landed.refusal, None, "no open-subtask refusal");
    assert_eq!(landed.status, "done");
}

/// The must-fix refusal comes only from the round snapshot: recorded, the round reports its open
/// must-fix; the `found_in` edge plays no part in it.
#[test]
fn the_must_fix_refusal_comes_from_the_recorded_round_not_the_edge() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&db, &["proj"]);
    let (_, step_uid) = a_design(&db, "Recorded");
    let t = added_under(&b, "the work", &step_uid);
    call(
        &b,
        json!({ "op": "task.start", "uid": t, "take": { "owner": "host:1" },
                "place": { "branch": "feat", "repo": "proj", "onto": "batch" } }),
    )
    .unwrap();
    let ns = "repos/proj/codereviews/r1";
    let scoped = reviewer_of(&db, &t);
    let filed = file(
        &scoped,
        ns,
        &json!([{ "severity": "must-fix", "summary": "b" }]),
    )
    .unwrap();
    record(&scoped, "feat", ns).unwrap();
    assert_eq!(findings(&b, ns).open_must_fix.len(), 1);
    // Dropping the edge changes nothing the gate reads.
    let fid = id_of(&db, &filed.uids[0]);
    let tid = id_of(&db, &t);
    db.write_txn("t", move |c, m| {
        jkb_core::edge::unlink(c, m, fid, tid, jkb_types::EdgeType::FoundIn)
    })
    .unwrap();
    assert_eq!(findings(&b, ns).open_must_fix.len(), 1);
}

/// Test 7: `task.show` lists a task's findings by round with their current step and must-fix, and a
/// finding's page names the task it was found in; `design.plans` carries `found_in`; and an answer
/// without the fields (an older daemon's) decodes as empty.
#[test]
fn task_show_and_design_plans_carry_findings_and_found_in() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&db, &["proj"]);
    let (design, step_uid) = a_design(&db, "Shown");
    let t = added_under(&b, "the work", &step_uid);
    call(
        &b,
        json!({ "op": "task.start", "uid": t, "take": { "owner": "host:1" },
                "place": { "branch": "feat", "repo": "proj", "onto": "batch" } }),
    )
    .unwrap();
    let scoped = reviewer_of(&db, &t);
    let r1 = file(&scoped, "repos/proj/codereviews/r1", &three()).unwrap();
    record(&scoped, "feat", "repos/proj/codereviews/r1").unwrap();
    let r2 = file_for(
        &b,
        "repos/proj/codereviews/r2",
        &t,
        &json!([{ "severity": "must-fix", "summary": "late" }]),
    )
    .unwrap();
    db.write_txn("t", |c, m| plan::triage_step(c, m, "proj"))
        .unwrap();
    call(
        &b,
        json!({ "op": "task.move", "uid": r1.uids[0], "under": plan::triage_step_uid("proj") }),
    )
    .unwrap();

    let shown = show(&b, &t);
    let rows = shown["findings"].as_array().unwrap();
    let got: Vec<(&str, &str, bool, &str)> = rows
        .iter()
        .map(|f| {
            (
                f["uid"].as_str().unwrap(),
                f["round"].as_str().unwrap(),
                f["must_fix"].as_bool().unwrap(),
                f["step"].as_str().unwrap(),
            )
        })
        .collect();
    let inbox = plan::triage_step_uid("proj");
    assert_eq!(
        got,
        [
            (
                r1.uids[0].as_str(),
                "repos/proj/codereviews/r1",
                false,
                inbox.as_str()
            ),
            (
                r1.uids[1].as_str(),
                "repos/proj/codereviews/r1",
                false,
                step_uid.as_str()
            ),
            (
                r1.uids[2].as_str(),
                "repos/proj/codereviews/r1",
                true,
                step_uid.as_str()
            ),
            (
                r2.uids[0].as_str(),
                "repos/proj/codereviews/r2",
                true,
                step_uid.as_str()
            ),
        ],
        "the recorded round first, then the unrecorded one; must-fix by snapshot, else priority"
    );
    assert_eq!(rows[0]["step_title"], "Open work \u{203a} Inbox");
    assert!(shown["found_in"].is_null(), "a task found in nothing");
    let finding = show(&b, &r1.uids[2]);
    assert_eq!(finding["found_in"][0]["uid"], t.as_str());
    assert_eq!(finding["found_in"][0]["title"], "the work");

    let plans = match call(&b, json!({ "op": "design.plans", "uid": design })).unwrap() {
        Response::DesignPlans { list } => serde_json::to_value(list).unwrap(),
        other => panic!("{other:?}"),
    };
    let tasks = plans["plans"][0]["steps"][0]["tasks"].as_array().unwrap();
    let of = |uid: &str| tasks.iter().find(|x| x["uid"] == uid).unwrap().clone();
    assert_eq!(of(&t)["found_in"], json!([]));
    assert_eq!(of(&r1.uids[2])["found_in"][0]["uid"], t.as_str());

    // An older daemon's answers, without the fields.
    let mut old = of(&t);
    old.as_object_mut().unwrap().remove("found_in");
    let decoded: crate::designs::plans::PlanTask = serde_json::from_value(old).unwrap();
    assert!(decoded.found_in.is_empty());
    let mut old = shown.clone();
    let o = old.as_object_mut().unwrap();
    o.remove("findings");
    o.remove("found_in");
    let decoded: crate::kb::TaskDetail = serde_json::from_value(old).unwrap();
    assert!(decoded.findings.is_empty() && decoded.found_in.is_empty());
}

/// A task left under no step, homed at `home`, as a build before the guard could leave one.
fn loose_at(db: &Db, uid: &str, home: &str) -> ItemId {
    let (uid, home) = (uid.to_owned(), home.to_owned());
    db.write_txn("t", move |c, m| {
        let mut spec = jkb_core::task::NewTask::new(
            uid,
            "loose work",
            jkb_core::task::Under::inbox_of("proj"),
        );
        spec.home = home;
        let id = jkb_core::task::create(c, m, &spec)?;
        c.execute(
            "DELETE FROM containment WHERE child_item_id = ?1",
            [id.get()],
        )?;
        c.execute(
            "DELETE FROM edges WHERE dst_item_id = ?1 AND type = 'parent_of'",
            [id.get()],
        )?;
        Ok(id)
    })
    .unwrap()
}

fn unplanned(b: &LocalBackend, ask: &serde_json::Value) -> crate::tasks::UnplannedList {
    let mut req = json!({ "op": "task.unplanned" });
    req.as_object_mut()
        .unwrap()
        .extend(ask.as_object().unwrap().clone());
    match call(b, req).unwrap() {
        Response::Unplanned { list } => list,
        other => panic!("{other:?}"),
    }
}

/// Test 8: `task.unplanned` groups the unplanned set by repo — a task in no repo last, a review
/// finding homed under `repos/<repo>/codereviews/…` in its repo — counts each repo past its listing
/// cap, leaves terminal tasks out, and totals what `plan::unplanned` and `kb.health` count.
#[test]
fn task_unplanned_groups_the_unplanned_set_by_repo() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    crate::test_repo::know(&db, &["proj", "app"]);
    a_design(&db, "Planned work");
    for i in 0..3 {
        loose_at(&db, &format!("task:p{i}"), "tasks/repos/proj/inbox");
    }
    loose_at(&db, "task:a0", "repos/app/notes");
    loose_at(&db, "task:none", "notes/elsewhere");
    let done = loose_at(&db, "task:finished", "tasks/repos/proj/inbox");
    db.write_txn("t", move |c, m| {
        jkb_core::task::set_status(c, m, done, jkb_types::TaskStatus::Done)
    })
    .unwrap();
    let filed = file(
        &b,
        "repos/proj/codereviews/r1",
        &json!([{ "severity": "nit", "summary": "x" }]),
    )
    .unwrap();
    let fid = id_of(&db, &filed.uids[0]);
    db.write_txn("t", move |c, _| {
        c.execute(
            "DELETE FROM containment WHERE child_item_id = ?1",
            [fid.get()],
        )?;
        Ok(())
    })
    .unwrap();

    let all = unplanned(&b, &json!({}));
    let shape: Vec<(Option<&str>, usize, usize)> = all
        .repos
        .iter()
        .map(|r| (r.repo.as_deref(), r.count, r.tasks.len()))
        .collect();
    assert_eq!(
        shape,
        [(Some("app"), 1, 1), (Some("proj"), 4, 4), (None, 1, 1)]
    );
    let proj = &all.repos[1];
    assert!(proj.tasks.iter().any(|t| t.task.uid == filed.uids[0]
        && t.namespace
            .as_deref()
            .is_some_and(|n| n.starts_with("repos/proj/codereviews/"))));
    assert!(all
        .repos
        .iter()
        .flat_map(|r| &r.tasks)
        .all(|t| t.task.uid != "task:finished"));
    assert_eq!(all.count, 6);
    assert_eq!(all.count, db.read(plan::unplanned).unwrap().len());
    let health = match call(&b, json!({ "op": "kb.health" })).unwrap() {
        Response::Health { health } => health,
        other => panic!("{other:?}"),
    };
    assert_eq!(health.unplanned_count, Some(all.count));

    let capped = unplanned(&b, &json!({ "repo": "proj", "limit": 2 }));
    assert_eq!(capped.count, 4);
    assert_eq!(capped.repos.len(), 1);
    assert_eq!((capped.repos[0].count, capped.repos[0].tasks.len()), (4, 2));
}
