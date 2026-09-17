//! The session ops through the backend, as the CLI drives them: each write is a compare-and-set on the
//! owner the caller judged, and a client under file roots is held to them like every task write.

use serde_json::json;

use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response, SessionStateIs};
use jkb_core::Db;

fn call(b: &LocalBackend, r: serde_json::Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

fn add(b: &LocalBackend, text: &str) -> String {
    match call(
        b,
        json!({ "op": "task.add", "text": text, "managed": true }),
    )
    .unwrap()
    {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    }
}

fn facts(b: &LocalBackend, uid: &str) -> super::TaskState {
    match call(b, json!({ "op": "task.facts", "uid": uid })).unwrap() {
        Response::TaskState { state } => state,
        other => panic!("{other:?}"),
    }
}

fn taken(r: Response) -> bool {
    match r {
        Response::Taken { taken } => taken,
        other => panic!("{other:?}"),
    }
}

fn start_on(
    b: &LocalBackend,
    uid: &str,
    owner: &str,
    displace: Option<&str>,
    branch: &str,
    onto: &str,
) -> Result<bool, ApiError> {
    call(
        b,
        json!({ "op": "task.start", "uid": uid,
                "take": { "owner": owner, "displace": displace },
                "place": { "branch": branch, "repo": "proj", "onto": onto } }),
    )
    .map(taken)
}

fn start(
    b: &LocalBackend,
    uid: &str,
    owner: &str,
    displace: Option<&str>,
) -> Result<bool, ApiError> {
    start_on(b, uid, owner, displace, "feat", "batch")
}

/// `task.start` takes the claim, sets the location facets and records the branch and land target in
/// the history — and `task.facts` reads all of it back in one answer.
#[test]
fn a_start_claims_and_records_where_the_work_is() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "do the thing +tasks/x");
    let before = facts(&b, &uid);
    assert_eq!(
        (before.status.as_str(), before.claim.as_deref()),
        ("open", None)
    );
    assert_eq!(before.start_refusal, None);

    assert!(start(&b, &uid, "host:1", None).unwrap());
    let after = facts(&b, &uid);
    assert_eq!(after.claim.as_deref(), Some("host:1"));
    assert_eq!(after.status, "in_progress");
    assert_eq!(after.tags["branch"], ["feat"]);
    assert_eq!(after.tags["repo"], ["proj"]);
    assert_eq!(after.land_target.as_deref(), Some("batch"));
    assert_eq!(
        after.start_refusal, None,
        "an unfinished task reports no refusal: whether it can be started depends on who asks"
    );

    let Response::BranchTasks { tasks } =
        call(&b, json!({ "op": "task.by_branch", "repo": "proj" })).unwrap()
    else {
        panic!("expected tasks")
    };
    let t = &tasks["feat"];
    assert_eq!(t.len(), 1);
    assert_eq!(
        (t[0].uid.as_str(), t[0].onto.as_deref()),
        (uid.as_str(), Some("batch"))
    );
    // A second task on the same branch is listed beside the first, in id order, not in its place.
    let second = add(&b, "same branch +tasks/x");
    assert!(start(&b, &second, "host:2", None).unwrap());
    let Response::BranchTasks { tasks } =
        call(&b, json!({ "op": "task.by_branch", "repo": "proj" })).unwrap()
    else {
        panic!("expected tasks")
    };
    let uids: Vec<&str> = tasks["feat"].iter().map(|t| t.uid.as_str()).collect();
    assert_eq!(uids, [uid.as_str(), second.as_str()]);
    let Response::BranchTasks { tasks } =
        call(&b, json!({ "op": "task.by_branch", "repo": "other" })).unwrap()
    else {
        panic!("expected tasks")
    };
    assert!(tasks.is_empty());
}

/// **The compare-and-set.** A takeover names the owner the caller judged; if the claim moved since —
/// or appeared where the caller saw none — nothing is written: not the claim, not the facets, not the
/// land target.
#[test]
fn a_takeover_of_an_owner_that_changed_writes_nothing() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "contested +tasks/x");
    assert!(start(&b, &uid, "host:1", None).unwrap());
    // The caller judged `host:9` gone, but the claim is `host:1`'s.
    assert!(!start_on(&b, &uid, "host:2", Some("host:9"), "other", "elsewhere").unwrap());
    // The caller saw no claim, and there is one now.
    assert!(!start_on(&b, &uid, "host:2", None, "other", "elsewhere").unwrap());
    let f = facts(&b, &uid);
    assert_eq!(f.claim.as_deref(), Some("host:1"));
    assert_eq!(f.tags["branch"], ["feat"]);
    assert_eq!(f.land_target.as_deref(), Some("batch"));

    // A takeover of the owner actually there succeeds.
    assert!(start(&b, &uid, "host:2", Some("host:1")).unwrap());
    assert_eq!(facts(&b, &uid).claim.as_deref(), Some("host:2"));

    // Kept: no claim change, only the location — and only while the claim is still the one kept.
    let keep = |kept: &str, branch: &str| {
        call(
            &b,
            json!({ "op": "task.start", "uid": uid, "keep": kept,
                    "place": { "branch": branch, "repo": "proj" } }),
        )
        .map(taken)
        .unwrap()
    };
    assert!(!keep("host:1", "stale"), "the kept claim moved on");
    assert_eq!(facts(&b, &uid).tags["branch"], ["feat"]);
    assert!(keep("host:2", "feat2"));
    let f = facts(&b, &uid);
    assert_eq!(f.claim.as_deref(), Some("host:2"));
    assert_eq!(f.tags["branch"], ["feat2"]);

    // Exactly one of take and keep.
    for bad in [
        json!({ "op": "task.start", "uid": uid, "place": { "branch": "x", "repo": "proj" } }),
        json!({ "op": "task.start", "uid": uid, "keep": "host:2", "take": { "owner": "host:2" },
                "place": { "branch": "x", "repo": "proj" } }),
    ] {
        let e = call(&b, bad.clone()).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{bad}: {e:?}");
    }
    assert_eq!(facts(&b, &uid).tags["branch"], ["feat2"]);
}

/// `task.take` is `task work`'s claim: its start transition carries no branch or land target, a
/// same-owner retake is not an error — and it writes no location. `task.locate` does, once the
/// worktree exists, noting the branch and land target, and only for the owner that holds the claim,
/// so a displaced run's late write changes nothing.
#[test]
fn a_take_claims_and_a_locate_records_the_holder_s_place() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "work it +tasks/x");
    let take = |owner: &str, displace: Option<&str>, branch: &str| {
        call(
            &b,
            json!({ "op": "task.take", "uid": uid,
                    "take": { "owner": owner, "displace": displace },
                    "place": { "branch": branch, "repo": "proj", "onto": "batch" } }),
        )
        .map(taken)
    };
    let locate = |owner: &str, branch: &str| {
        call(
            &b,
            json!({ "op": "task.locate", "uid": uid, "owner": owner,
                    "place": { "branch": branch, "repo": "proj", "onto": "batch" } }),
        )
        .map(taken)
        .unwrap()
    };
    let history = || match call(&b, json!({ "op": "task.why", "uid": uid })).unwrap() {
        Response::History { entries, .. } => entries,
        other => panic!("{other:?}"),
    };
    assert!(take("session:1:~/w", None, "task/w").unwrap());
    let f = facts(&b, &uid);
    assert_eq!(
        (f.claim.as_deref(), f.land_target),
        (Some("session:1:~/w"), None),
        "a take names no branch and no land target: nothing is there yet"
    );
    assert!(!f.tags.contains_key("branch"), "a take writes no location");
    assert!(history()
        .iter()
        .all(|e| e.branch.is_none() && e.onto.is_none()));
    assert!(locate("session:1:~/w", "task/w"));
    let f = facts(&b, &uid);
    assert_eq!(f.tags["branch"], ["task/w"]);
    assert_eq!(f.tags["repo"], ["proj"]);
    assert_eq!(
        f.land_target.as_deref(),
        Some("batch"),
        "the locate labels the history"
    );
    let labelled = history().len();
    assert!(locate("session:1:~/w", "task/w"));
    assert_eq!(
        history().len(),
        labelled,
        "an unchanged place adds no entry"
    );
    assert!(
        take("session:1:~/w", Some("session:1:~/w"), "task/w").unwrap(),
        "a resume re-takes its own claim"
    );
    // Another run took over and recorded its place; the displaced one's late locate changes nothing.
    assert!(take("session:2:~/v", Some("session:1:~/w"), "task/v").unwrap());
    assert!(locate("session:2:~/v", "task/v"));
    assert!(!locate("session:1:~/w", "task/w"));
    assert_eq!(facts(&b, &uid).tags["branch"], ["task/v"]);
    // A session's claim says where it lands.
    let e = call(
        &b,
        json!({ "op": "task.take", "uid": uid, "take": { "owner": "session:2:~/v" },
                "place": { "branch": "task/v", "repo": "proj" } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}

/// **A place the task's `tasks.md` line cannot carry is refused by the take**, before any git work, and
/// the trial leaves nothing behind: no claim, no facet, no history entry for it.
#[test]
fn a_take_refuses_a_place_the_task_s_line_cannot_carry_and_writes_nothing() {
    use jkb_core::{mount, ns};
    use jkb_types::{ConflictPolicy, SyncMode};
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |c, m| {
        let id = ns::ensure(c, "repos/in")?;
        mount::create(
            c,
            m,
            id,
            "file:///Users/u/repos/in",
            SyncMode::Bidirectional,
            "tasks",
            None,
            None,
            ConflictPolicy::Manual,
        )?;
        Ok(())
    })
    .unwrap();
    let b = LocalBackend::new(db);
    let uid = match call(&b, json!({ "op": "task.add", "text": "filed +repos/in" })).unwrap() {
        Response::Added { added } => {
            assert!(added.binding.is_some(), "file-backed");
            added.uid
        }
        other => panic!("{other:?}"),
    };
    let history = |b: &LocalBackend| match call(b, json!({ "op": "task.why", "uid": uid })).unwrap()
    {
        Response::History { entries, .. } => entries.len(),
        other => panic!("{other:?}"),
    };
    let before = history(&b);
    // A repo key with a space: the line would not read back (tasks F4).
    let e = call(
        &b,
        json!({ "op": "task.take", "uid": uid, "take": { "owner": "session:1:~/w" },
                "place": { "branch": "task/w", "repo": "My App", "onto": "batch" } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    let f = facts(&b, &uid);
    assert_eq!((f.claim, f.land_target), (None, None));
    assert!(
        !f.tags.contains_key("branch") && !f.tags.contains_key("repo"),
        "{:?}",
        f.tags
    );
    assert_eq!(history(&b), before, "no transition recorded");
    // The same take with a place the line can carry succeeds, and writes no location yet.
    assert!(call(
        &b,
        json!({ "op": "task.take", "uid": uid, "take": { "owner": "session:1:~/w" },
                "place": { "branch": "task/w", "repo": "proj", "onto": "batch" } }),
    )
    .map(taken)
    .unwrap());
    assert!(!facts(&b, &uid).tags.contains_key("branch"));
}

/// A terminal task cannot be started: the lifecycle's refusal is what `task.facts` reports and what a
/// write returns.
#[test]
fn a_finished_task_is_not_started() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "finished +tasks/x");
    call(
        &b,
        json!({ "op": "task.set", "uid": uid, "status": "done" }),
    )
    .unwrap();
    let f = facts(&b, &uid);
    assert!(f.terminal);
    assert!(f.start_refusal.is_some());
    let e = start(&b, &uid, "host:1", None).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    assert_eq!(facts(&b, &uid).tags.get("branch"), None, "nothing written");
}

fn abandon(b: &LocalBackend, uid: &str, observed: Option<&str>) -> super::Abandoned {
    match call(
        b,
        json!({ "op": "task.abandon", "uid": uid, "observed": observed }),
    )
    .unwrap()
    {
        Response::Abandoned { abandoned } => abandoned,
        other => panic!("{other:?}"),
    }
}

/// `task.abandon` releases the claim it was told about and reopens — and changes nothing when the
/// claim is not the one the caller judged, including a claim that appeared after it looked.
#[test]
fn an_abandon_releases_only_the_judged_claim() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "drop it +tasks/x");
    assert!(start(&b, &uid, "host:1", None).unwrap());

    let a = abandon(&b, &uid, Some("host:9"));
    assert_eq!(
        (a.released, a.reopened, a.status.as_str()),
        (false, false, "in_progress")
    );
    assert_eq!(facts(&b, &uid).claim.as_deref(), Some("host:1"));

    let a = abandon(&b, &uid, None);
    assert!(!a.released && !a.reopened, "a claim nobody judged stays");

    // The judged claim went, and nobody holds the task: it is reopened as asked.
    call(
        &b,
        json!({ "op": "task.release", "uid": uid, "owner": "host:1" }),
    )
    .unwrap();
    let a = abandon(&b, &uid, Some("host:1"));
    assert_eq!(
        (a.released, a.reopened, a.status.as_str()),
        (true, true, "open")
    );
    assert!(start(&b, &uid, "host:1", None).unwrap());

    let a = abandon(&b, &uid, Some("host:1"));
    assert_eq!(
        (a.released, a.reopened, a.status.as_str()),
        (true, true, "open")
    );
    let f = facts(&b, &uid);
    assert_eq!((f.claim, f.land_target), (None, None));

    // A finished task is left finished.
    assert!(start(&b, &uid, "host:1", None).unwrap());
    call(
        &b,
        json!({ "op": "task.set", "uid": uid, "status": "done" }),
    )
    .unwrap();
    let a = abandon(&b, &uid, facts(&b, &uid).claim.as_deref());
    assert_eq!(
        (a.released, a.reopened, a.status.as_str()),
        (true, false, "done")
    );
}

/// The gate is read through the daemon and never written: there is no op that stores one.
#[test]
fn the_gate_is_read_only() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    let gate = |repo: &str| match call(&b, json!({ "op": "repo.gate", "repo": repo })).unwrap() {
        Response::Gate { gate } => gate,
        other => panic!("{other:?}"),
    };
    assert_eq!(gate("proj"), None);
    db.write_txn("t", |c, m| {
        let id = jkb_core::ns::ensure(c, "repos/proj")?;
        jkb_core::ns::set_metadata(c, m, id, &json!({ "gate": "make test", "type": "repo" }))
    })
    .unwrap();
    assert_eq!(gate("proj").as_deref(), Some("make test"));
    assert!(
        crate::Request::OPS
            .iter()
            .all(|op| !op.contains("gate") || *op == "repo.gate"),
        "no op stores a gate"
    );
}

/// Every name a session op stores is bounded, free of control characters, and — for a branch and a
/// land target — not one git would read as an option; an owner is held to the claim ops' own limit. A refused write
/// changes nothing.
#[test]
fn malformed_session_fields_are_refused() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "bounded +tasks/x");
    let long = "b".repeat(super::MAX_NAME_BYTES + 1);
    for place in [
        json!({ "branch": "", "repo": "r", "onto": "o" }),
        json!({ "branch": long, "repo": "r", "onto": "o" }),
        json!({ "branch": "b\nx", "repo": "r", "onto": "o" }),
        json!({ "branch": "b", "repo": "r", "onto": "" }),
        json!({ "branch": "b", "repo": "", "onto": "o" }),
        // Names git would read as options (`location::valid_ref`, the rule every land-target writer
        // applies).
        json!({ "branch": "b", "repo": "r", "onto": "--upload-pack=x" }),
        json!({ "branch": "-b", "repo": "r", "onto": "o" }),
    ] {
        for op in ["task.take", "task.start"] {
            let e = call(
                &b,
                json!({ "op": op, "uid": uid, "take": { "owner": "host:1" }, "place": place }),
            )
            .unwrap_err();
            assert_eq!(e.code, ErrorCode::Invalid, "{op} {place}: {e:?}");
        }
    }
    let e = call(&b, json!({ "op": "task.by_branch", "repo": "" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    let e = start(
        &b,
        &uid,
        &"o".repeat(crate::tasks::MAX_OWNER_BYTES + 1),
        None,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    let f = facts(&b, &uid);
    assert!(!f.tags.contains_key("branch"));
    assert_eq!(f.claim, None);
    let e = call(&b, json!({ "op": "task.facts", "uid": "task:nope" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

/// `session.state` answers from the registry: unknown, live, ended — a closed set on the wire, so a
/// state a newer daemon added fails to decode rather than reading as one of these.
#[test]
fn a_session_state_is_read_from_the_registry() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let state = || match call(&b, json!({ "op": "session.state", "session": "s1" })).unwrap() {
        Response::SessionIs { state } => state,
        other => panic!("{other:?}"),
    };
    assert_eq!(state(), SessionStateIs::Unknown);
    call(
        &b,
        json!({ "op": "session.started", "session": "s1", "source": "startup",
                "pid": "1", "instance": "h" }),
    )
    .unwrap();
    assert_eq!(state(), SessionStateIs::Live);
    call(
        &b,
        json!({ "op": "session.ended", "session": "s1", "reason": "other",
                "pid": "1", "instance": "h" }),
    )
    .unwrap();
    assert_eq!(state(), SessionStateIs::Ended);
    assert_eq!(
        serde_json::to_value(SessionStateIs::Ended).unwrap(),
        json!("ended")
    );
    assert!(serde_json::from_value::<Response>(
        json!({ "result": "session_is", "state": "suspended" })
    )
    .is_err());
    for bad in ["", "a b", "a:b"] {
        let e = call(&b, json!({ "op": "session.state", "session": bad })).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{bad:?}");
    }
}

/// **A claim is compared as stored.** An owner written in a spelling `AgentId` would render differently
/// (`agent:a:b` renders `agent:a-b`) is reported verbatim by `task.facts`, so sending it back as
/// `displace` or `observed` matches it — a re-rendered owner never would, and the verb would loop on
/// "run it again" for ever.
#[test]
fn a_claim_in_a_non_canonical_spelling_is_compared_as_stored() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    let uid = add(&b, "legacy claim +tasks/x");
    // Written below the ops, as an older binary or a hand edit could have left it.
    let plant = |db: &Db| {
        let uid = uid.clone();
        db.write_txn("t", move |c, m| {
            let id = jkb_core::task::resolve_ref(c, &uid)?.expect("task");
            jkb_core::claim::claim(c, m, id, "agent:reviewer:G3")?;
            Ok(())
        })
        .unwrap();
    };
    plant(&db);
    let held = facts(&b, &uid).claim.unwrap();
    assert_eq!(held, "agent:reviewer:G3");
    assert!(start(&b, &uid, "host:1", Some(&held)).unwrap());
    assert_eq!(facts(&b, &uid).claim.as_deref(), Some("host:1"));

    call(
        &b,
        json!({ "op": "task.release", "uid": uid, "owner": "host:1" }),
    )
    .unwrap();
    plant(&db);
    let a = abandon(&b, &uid, Some("agent:reviewer:G3"));
    assert!(a.reopened, "the stored claim was released: {a:?}");
    assert_eq!(facts(&b, &uid).claim, None);
}

/// **A claim is taken only in the spelling it is stored in.** `agent:reviewer:G3` is stored
/// `agent:reviewer-G3`, so a worker that took it under the first spelling could never release or
/// compare-and-set its own claim again. Each op that takes a claim refuses it, before writing.
#[test]
fn a_claim_is_taken_only_in_its_stored_spelling() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "spelled +tasks/x");
    let odd = "agent:reviewer:G3";
    let refused = |r: Result<Response, ApiError>| {
        let e = r.expect_err("a non-canonical owner is refused");
        assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    };
    refused(call(
        &b,
        json!({ "op": "task.claim", "uid": uid, "owner": odd }),
    ));
    refused(call(
        &b,
        json!({ "op": "task.start", "uid": uid, "take": { "owner": odd },
                "place": { "branch": "feat", "repo": "proj", "onto": "batch" } }),
    ));
    refused(call(
        &b,
        json!({ "op": "task.take", "uid": uid, "take": { "owner": odd },
                "place": { "branch": "feat", "repo": "proj", "onto": "batch" } }),
    ));
    assert_eq!(facts(&b, &uid).claim, None, "nothing was taken");
    assert!(start(&b, &uid, "agent:reviewer-G3", None).unwrap());
}

/// **A claim held in an older spelling is refused with the way out, not reported as a race.** Its holder,
/// sending the spelling it is now required to, would otherwise pass the lifecycle (which compares parsed
/// owners) and fail the claim's byte compare-and-set.
#[test]
fn a_claim_held_in_an_older_spelling_is_refused_with_the_way_out() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    let uid = add(&b, "respelled +tasks/x");
    let u = uid.clone();
    db.write_txn("t", move |c, m| {
        let id = jkb_core::task::resolve_ref(c, &u)?.expect("task");
        jkb_core::claim::claim(c, m, id, "agent:reviewer:G3")?;
        Ok(())
    })
    .unwrap();
    match call(
        &b,
        json!({ "op": "task.claim", "uid": uid, "owner": "agent:reviewer-G3" }),
    )
    .unwrap()
    {
        Response::Claimed { claimed } => {
            assert!(!claimed.acquired);
            let why = claimed.refusal.unwrap();
            assert!(why.contains("--owner 'agent:reviewer:G3'"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    call(
        &b,
        json!({ "op": "task.release", "uid": uid, "owner": "agent:reviewer:G3" }),
    )
    .unwrap();
    match call(
        &b,
        json!({ "op": "task.claim", "uid": uid, "owner": "agent:reviewer-G3" }),
    )
    .unwrap()
    {
        Response::Claimed { claimed } => assert!(claimed.acquired, "{claimed:?}"),
        other => panic!("{other:?}"),
    }
}

fn landing(r: Result<Response, ApiError>) -> Result<super::Landing, ApiError> {
    match r? {
        Response::Landing { landing } => Ok(landing),
        other => panic!("{other:?}"),
    }
}

/// `task.land` finishes a task and frees its claim, labelled with where it landed; a task cancelled
/// meanwhile stays cancelled, and says why; a head that is not a commit is refused.
#[test]
fn a_land_finishes_the_task_unless_it_was_cancelled_meanwhile() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let uid = add(&b, "land it +tasks/x");
    assert!(start(&b, &uid, "host:1", None).unwrap());
    let land = |uid: &str, head: Option<&str>| {
        landing(call(
            &b,
            json!({ "op": "task.land", "uid": uid,
                    "landed": { "branch": "feat", "onto": "batch", "head": head } }),
        ))
    };
    let e = land(&uid, Some("not-a-commit")).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);

    let l = land(&uid, Some("abcd1234")).unwrap();
    assert_eq!((l.moved, l.status.as_str()), (true, "done"), "{l:?}");
    assert_eq!(facts(&b, &uid).claim, None);
    match call(&b, json!({ "op": "task.why", "uid": uid })).unwrap() {
        Response::History { entries, .. } => {
            let last = entries.last().unwrap();
            assert_eq!(last.event, "land", "{entries:?}");
            assert_eq!(last.onto.as_deref(), Some("batch"));
        }
        other => panic!("{other:?}"),
    }

    let other = add(&b, "dropped meanwhile +tasks/x");
    assert!(start(&b, &other, "host:1", None).unwrap());
    call(
        &b,
        json!({ "op": "task.set", "uid": other, "status": "cancelled" }),
    )
    .unwrap();
    let l = land(&other, None).unwrap();
    assert!(!l.moved && l.refusal.is_some(), "{l:?}");
    assert_eq!(l.status, "cancelled");
}

/// `task.landed` records the queue's graft: it finishes a task in progress, records a landing a guard
/// held back without moving the task, and records nothing for a task the event does not apply to.
#[test]
fn a_queue_landing_is_recorded_as_the_lifecycle_allows() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let landed = |uid: &str| {
        landing(call(
            &b,
            json!({ "op": "task.landed", "uid": uid,
                    "landed": { "branch": "feat", "onto": "batch" } }),
        ))
        .unwrap()
    };
    let events = |uid: &str| match call(&b, json!({ "op": "task.why", "uid": uid })).unwrap() {
        Response::History { entries, .. } => {
            entries.into_iter().map(|e| e.event).collect::<Vec<_>>()
        }
        other => panic!("{other:?}"),
    };

    let working = add(&b, "in flight +tasks/x");
    assert!(start(&b, &working, "host:1", None).unwrap());
    let l = landed(&working);
    assert_eq!((l.moved, l.status.as_str()), (true, "done"), "{l:?}");

    // A parent with an open subtask is held — and the landing is still in its history.
    let parent = add(&b, "parent +tasks/x");
    assert!(start(&b, &parent, "host:1", None).unwrap());
    call(
        &b,
        json!({ "op": "task.add", "text": "child", "managed": true, "under": parent }),
    )
    .unwrap();
    let l = landed(&parent);
    assert!(!l.moved && l.refusal.is_some(), "{l:?}");
    assert_eq!(l.status, "in_progress");
    assert_eq!(
        events(&parent).last().map(String::as_str),
        Some("observed_landed")
    );

    // An open task: the event does not apply, and nothing is written.
    let open = add(&b, "never started +tasks/x");
    let before = events(&open);
    let l = landed(&open);
    assert!(!l.moved, "{l:?}");
    assert_eq!(events(&open), before, "nothing recorded");
}

/// `task.review_findings` counts every finding, and the open must-fix ones apart, across every named
/// review; a finished or lower-priority finding does not block.
#[test]
fn review_findings_count_what_blocks_a_landing() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    add(&b, "must fix !p1 +reviews/one");
    add(&b, "must fix too !p0 +reviews/two");
    add(&b, "a concern !p2 +reviews/one");
    let done = add(&b, "fixed !p1 +reviews/two");
    call(
        &b,
        json!({ "op": "task.set", "uid": done, "status": "done" }),
    )
    .unwrap();
    let ask = |nss: serde_json::Value| match call(
        &b,
        json!({ "op": "task.review_findings", "namespaces": nss }),
    ) {
        Ok(Response::ReviewFindings { findings }) => Ok(findings),
        Ok(other) => panic!("{other:?}"),
        Err(e) => Err(e),
    };
    let f = ask(json!(["reviews/one", "reviews/two"])).unwrap();
    assert_eq!(
        (f.total, f.open_count, f.open_must_fix.len()),
        (4, 2, 2),
        "{f:?}"
    );
    let f = ask(json!(["reviews/nowhere"])).unwrap();
    assert_eq!((f.total, f.open_count), (0, 0));
    let many: Vec<String> = (0..=super::MAX_REVIEW_NAMESPACES)
        .map(|i| format!("r/{i}"))
        .collect();
    assert_eq!(ask(json!(many)).unwrap_err().code, ErrorCode::Invalid);
}

/// A finding's title is cut to its cap, and a namespace set holding more tasks than a review can is
/// refused with the remedy.
#[test]
fn review_findings_are_bounded() {
    let db = Db::open_in_memory().unwrap();
    let b = LocalBackend::new(db.clone());
    let long = "x".repeat(super::MAX_FINDING_TITLE_CHARS + 50);
    add(&b, &format!("{long} !p1 +reviews/long"));
    let ask = |nss: serde_json::Value| {
        call(
            &b,
            json!({ "op": "task.review_findings", "namespaces": nss }),
        )
    };
    match ask(json!(["reviews/long"])).unwrap() {
        Response::ReviewFindings { findings } => assert_eq!(
            findings.open_must_fix[0].title.chars().count(),
            super::MAX_FINDING_TITLE_CHARS
        ),
        other => panic!("{other:?}"),
    }
    db.write_txn("t", |c, m| {
        for i in 0..=super::MAX_EXAMINED_FINDINGS {
            let mut t = jkb_core::task::NewTask::new(format!("task:huge-{i}"), format!("t{i}"));
            "reviews/huge".clone_into(&mut t.home);
            jkb_core::task::create(c, m, &t)?;
        }
        Ok(())
    })
    .unwrap();
    let e = ask(json!(["reviews/huge"])).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    assert!(e.message.contains("review="), "{e:?}");
}

/// `task.staging` answers only the repo's tasks that have somewhere to land, with what a row needs.
#[test]
fn the_staging_read_lists_the_repo_s_landing_tasks() {
    let b = LocalBackend::new(Db::open_in_memory().unwrap());
    let landing = add(&b, "landing work");
    let parent_sub = add(&b, "a sub");
    let elsewhere = add(&b, "other repo");
    let _idle = add(&b, "never started");
    assert!(start_on(&b, &landing, "host:1", None, "feat", "batch").unwrap());
    call(
        &b,
        json!({ "op": "task.add", "text": "child", "under": landing, "managed": true }),
    )
    .unwrap();
    assert!(start_on(&b, &parent_sub, "host:1", None, "f2", "batch").unwrap());
    call(
        &b,
        json!({ "op": "task.start", "uid": elsewhere, "take": { "owner": "host:1" },
                "place": { "branch": "x", "repo": "other", "onto": "batch" } }),
    )
    .unwrap();
    let Response::StagingTasks { tasks, truncated } = call(
        &b,
        json!({ "op": "task.staging", "repo": "proj", "all": true }),
    )
    .unwrap() else {
        panic!("staging")
    };
    assert!(!truncated);
    let rows: Vec<(&str, &str, &str, bool)> = tasks
        .iter()
        .map(|t| {
            (
                t.uid.as_str(),
                t.title.as_str(),
                t.land_target.as_str(),
                t.open_subtasks,
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (landing.as_str(), "landing work", "batch", true),
            (parent_sub.as_str(), "a sub", "batch", false),
        ]
    );
    assert_eq!(tasks[0].tags["branch"], vec!["feat".to_owned()]);
    assert_eq!(tasks[0].status, "in_progress");
    let e = call(&b, json!({ "op": "task.staging", "repo": "" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);

    // A spent batch — every task on it finished — is left out unless asked for.
    let old = add(&b, "shipped long ago");
    assert!(start_on(&b, &old, "host:1", None, "f3", "old-batch").unwrap());
    call(
        &b,
        json!({ "op": "task.set", "uid": old, "status": "done" }),
    )
    .unwrap();
    let targets = |all: bool| match call(
        &b,
        json!({ "op": "task.staging", "repo": "proj", "all": all }),
    )
    .unwrap()
    {
        Response::StagingTasks { tasks, .. } => tasks
            .into_iter()
            .map(|t| t.land_target)
            .collect::<std::collections::BTreeSet<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(!targets(false).contains("old-batch"));
    assert!(targets(true).contains("old-batch"));
    assert!(targets(false).contains("batch"), "a live batch is listed");
}
