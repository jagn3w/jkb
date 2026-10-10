use std::sync::Arc;

use jkb_core::Db;
use jkb_rbac::Grants as _;
use serde_json::json;

use super::{Caller, OpPermission, Tickets, OP_GRANTS};
use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};

fn call(b: &LocalBackend, r: serde_json::Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

#[allow(clippy::needless_pass_by_value)] // every call site builds its request inline
fn ok(b: &LocalBackend, r: serde_json::Value) -> Response {
    call(b, r.clone()).unwrap_or_else(|e| panic!("{r}: {e:?}"))
}

#[allow(clippy::needless_pass_by_value)] // every call site builds its request inline
fn refused(b: &LocalBackend, r: serde_json::Value) -> ApiError {
    let e = call(b, r.clone()).expect_err(&r.to_string());
    assert_eq!(e.code, ErrorCode::Forbidden, "{r}: {e:?}");
    e
}

fn add(b: &LocalBackend, text: &str) -> String {
    match ok(b, json!({ "op": "task.add", "text": text })) {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    }
}

fn grant(b: &LocalBackend, role: &str, task: Option<&str>, agent: &str) -> (i64, String) {
    match ok(
        b,
        json!({ "op": "role.grant", "role": role, "task": task, "agent": agent }),
    ) {
        Response::Granted { grant, token } => (grant.id, token),
        other => panic!("{other:?}"),
    }
}

/// An operator backend, and a way to make one serving a token on the same database.
struct Kb {
    db: Db,
    op: LocalBackend,
    tickets: Arc<Tickets>,
}

impl Kb {
    fn new() -> Self {
        let db = Db::open_in_memory().unwrap();
        let tickets = Arc::new(Tickets::default());
        Self {
            op: LocalBackend::new(db.clone()).with_tickets(Arc::clone(&tickets)),
            db,
            tickets,
        }
    }

    fn as_token(&self, token: &str) -> LocalBackend {
        LocalBackend::new(self.db.clone())
            .with_tickets(Arc::clone(&self.tickets))
            .with_caller(Caller::Token(token.to_owned()))
    }
}

#[test]
fn the_op_table_is_sound_and_only_the_operator_administers() {
    assert_eq!(OP_GRANTS.check(), vec![]);
    for role in [
        jkb_core::roles::Role::Coordinator,
        jkb_core::roles::Role::Designer,
        jkb_core::roles::Role::Implementer,
        jkb_core::roles::Role::Reviewer,
        jkb_core::roles::Role::SystemicReviewer,
    ] {
        assert!(!OP_GRANTS.permits(role, OpPermission::Admin), "{role:?}");
        assert!(!OP_GRANTS.permits(role, OpPermission::Land), "{role:?}");
        assert!(!OP_GRANTS.permits(role, OpPermission::Attest), "{role:?}");
    }
}

#[test]
fn an_unknown_token_is_unauthorized_not_the_operator() {
    let kb = Kb::new();
    let e = call(&kb.as_token("nope"), json!({ "op": "kb.ls" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthorized);
    let e = call(&kb.as_token("t_nope"), json!({ "op": "kb.ls" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthorized, "an unknown ticket too");
}

#[test]
fn a_scoped_coordinator_writes_its_task_and_nothing_else() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let b_task = add(&kb.op, "task b");
    let (_, token) = grant(&kb.op, "coordinator", Some(&a), "coord");
    let c = kb.as_token(&token);

    ok(&c, json!({ "op": "task.set", "uid": a, "priority": 2 }));
    let e = refused(
        &c,
        json!({ "op": "task.set", "uid": b_task, "priority": 2 }),
    );
    assert!(e.message.contains("scoped to another task"), "{e:?}");
    ok(&c, json!({ "op": "kb.ls" }));
    let e = refused(&c, json!({ "op": "item.rm", "uid": a }));
    assert!(
        e.message.contains("operator"),
        "the refusal names who may: {e:?}"
    );

    // It mints workers for its own task only, and never another coordinator — nor a reviewer, whose
    // token it would hold, and so review its own work with.
    grant(&c, "implementer", Some(&a), "impl-1");
    refused(
        &c,
        json!({ "op": "role.grant", "role": "implementer", "task": b_task, "agent": "x" }),
    );
    refused(
        &c,
        json!({ "op": "role.grant", "role": "coordinator", "task": a, "agent": "x" }),
    );
    for role in ["reviewer", "systemic_reviewer"] {
        refused(
            &c,
            json!({ "op": "role.grant", "role": role, "task": a, "agent": "x" }),
        );
    }
    let (_, rev) = grant(&kb.op, "reviewer", Some(&a), "rev-1");

    // A reviewer reviews; it does not edit or set status.
    let r = kb.as_token(&rev);
    refused(
        &r,
        json!({ "op": "task.edit", "uid": a, "text": "x", "append": true }),
    );
    refused(&r, json!({ "op": "task.set", "uid": a, "status": "done" }));
    match ok(&r, json!({ "op": "role.whoami" })) {
        Response::WhoAmI { whoami } => {
            assert_eq!(whoami.roles, vec!["reviewer"]);
            assert_eq!(whoami.task.as_deref(), Some(a.as_str()));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn revoking_a_coordinator_locks_out_every_worker_it_minted() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let (cid, token) = grant(&kb.op, "coordinator", Some(&a), "coord");
    let (_, worker) = grant(&kb.as_token(&token), "implementer", Some(&a), "w");
    match ok(&kb.op, json!({ "op": "role.revoke", "id": cid })) {
        Response::Revoked { count } => assert_eq!(count, 2),
        other => panic!("{other:?}"),
    }
    let e = call(&kb.as_token(&worker), json!({ "op": "kb.ls" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthorized);
}

#[test]
fn who_lands_is_the_task_strategys_lands_toggle() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let (_, token) = grant(&kb.op, "coordinator", Some(&a), "coord");
    let c = kb.as_token(&token);
    let landed = json!({ "op": "task.landed", "uid": a,
                         "landed": { "branch": "b", "onto": "o", "head": "abcd" } });
    let e = refused(&c, landed.clone());
    assert!(e.message.contains("does not let this caller land"), "{e:?}");
    // The coordinator cannot pick the laxer strategy for itself; the operator can.
    refused(
        &c,
        json!({ "op": "workflow.set", "uid": a, "strategy": "autonomous" }),
    );
    ok(
        &kb.op,
        json!({ "op": "workflow.set", "uid": a, "strategy": "autonomous" }),
    );
    // Now permitted — whatever the op itself then answers, it is not a refusal by role.
    if let Err(e) = call(&c, landed) {
        assert_ne!(e.code, ErrorCode::Forbidden, "{e:?}");
    }
    // And waiving the review gate stays the operator's under every strategy.
    refused(
        &c,
        json!({ "op": "task.review_waive", "uid": a, "sha": "abc" }),
    );
}

#[test]
fn each_role_fires_only_its_own_workflow_step() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let (_, designer) = grant(&kb.op, "designer", Some(&a), "d");
    let (_, coord) = grant(&kb.op, "coordinator", Some(&a), "c");
    let fire = |b: &LocalBackend, event: &str| match ok(
        b,
        json!({ "op": "workflow.fire", "uid": a, "event": event }),
    ) {
        Response::WorkflowMoved { outcome } => outcome,
        other => panic!("{other:?}"),
    };
    let d = kb.as_token(&designer);
    let moved = fire(&d, "submit_design");
    assert!(moved.moved, "{moved:?}");
    assert_eq!(moved.phase, "design_review");
    let c = kb.as_token(&coord);
    let no = fire(&c, "approve_design");
    assert!(!no.moved);
    assert!(
        no.refusal.as_deref().unwrap_or("").contains("operator"),
        "{no:?}"
    );
    let yes = fire(&kb.op, "approve_design");
    assert_eq!(yes.phase, "implement");
    match ok(&c, json!({ "op": "workflow.show", "uid": a })) {
        Response::Workflow { workflow } => {
            assert_eq!(workflow.phase, "implement");
            assert_eq!(workflow.next_role, "implementer");
            assert!(
                !workflow.caller_acts_next,
                "the coordinator spawns, it does not implement"
            );
            assert!(workflow.may_fire.contains(&"submit_work".to_owned()));
            // The pin row the first move writes, then the two moves.
            assert!(workflow.history.len() == 3, "{:?}", workflow.history);
            assert!(
                workflow.history[0].contains("pin_strategy"),
                "{:?}",
                workflow.history
            );
        }
        other => panic!("{other:?}"),
    }
}

/// The dev container's credential, the tickets it mints for the hook, and what each ticket holds.
#[test]
fn harness_tickets_carry_the_attested_agent_and_bind_it_to_one_task() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let b_task = add(&kb.op, "task b");
    let container = match ok(&kb.op, json!({ "op": "role.rotate_container" })) {
        Response::Granted { token, grant } => {
            assert_eq!(grant.role, "coordinator");
            token
        }
        other => panic!("{other:?}"),
    };
    ok(
        &kb.op,
        json!({ "op": "role.map", "agent_type": "reviewer", "role": "reviewer" }),
    );
    let hook = kb.as_token(&container);
    let mint = |agent_id: Option<&str>, agent_type: Option<&str>, tool: &str| match ok(
        &hook,
        json!({ "op": "attest.mint", "session": "s1", "agent_id": agent_id,
                "agent_type": agent_type, "tool_use_id": tool }),
    ) {
        Response::Ticket { token } => token,
        other => panic!("{other:?}"),
    };

    // The main session holds the container's ceiling — and cannot mint tickets itself.
    let main = kb.as_token(&mint(None, None, "tool-1"));
    ok(&main, json!({ "op": "task.set", "uid": a, "priority": 3 }));
    refused(
        &main,
        json!({ "op": "attest.mint", "session": "s1", "agent_id": "x", "agent_type": "reviewer",
                "tool_use_id": "t" }),
    );

    // A reviewer subagent: bound to the first task it writes, and held there.
    let rev = kb.as_token(&mint(Some("ag1"), Some("reviewer"), "tool-2"));
    refused(
        &rev,
        json!({ "op": "task.edit", "uid": a, "text": "x", "append": true }),
    );
    match ok(&rev, json!({ "op": "workflow.observe", "uid": a })) {
        Response::WorkflowMoved { .. } => {}
        other => panic!("{other:?}"),
    }
    let e = refused(&rev, json!({ "op": "workflow.observe", "uid": b_task }));
    assert!(e.message.contains("scoped to another task"), "{e:?}");

    // An untyped subagent holds no role, and is told how to fix that.
    let gp = kb.as_token(&mint(Some("ag2"), Some("general-purpose"), "tool-3"));
    let e = refused(&gp, json!({ "op": "kb.ls" }));
    assert!(e.message.contains("jkb role map"), "{e:?}");

    // Released at PostToolUse: the ticket stops resolving.
    let tool4 = mint(Some("ag1"), Some("reviewer"), "tool-4");
    match ok(
        &hook,
        json!({ "op": "attest.release", "session": "s1", "tool_use_id": "tool-4" }),
    ) {
        Response::TicketsReleased { count } => assert_eq!(count, 1),
        other => panic!("{other:?}"),
    }
    let e = call(&kb.as_token(&tool4), json!({ "op": "kb.ls" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthorized);

    // SubagentStop releases every ticket that subagent holds.
    let t5 = mint(Some("ag1"), Some("reviewer"), "tool-5");
    let _t6 = mint(Some("ag1"), Some("reviewer"), "tool-6");
    match ok(
        &hook,
        json!({ "op": "attest.release", "session": "s1", "agent_id": "ag1" }),
    ) {
        Response::TicketsReleased { count } => assert!(count >= 2, "{count}"),
        other => panic!("{other:?}"),
    }
    assert!(call(&kb.as_token(&t5), json!({ "op": "kb.ls" })).is_err());

    // No other grant — not even an operator-minted coordinator — mints tickets.
    let (_, other) = grant(&kb.op, "coordinator", None, "someone");
    refused(
        &kb.as_token(&other),
        json!({ "op": "attest.mint", "session": "s1", "tool_use_id": "t" }),
    );
    // And no agent type may be mapped to operator.
    let e = call(
        &kb.op,
        json!({ "op": "role.map", "agent_type": "evil", "role": "operator" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}

#[test]
fn strategies_list_presets_then_definitions_and_name_the_default() {
    let kb = Kb::new();
    ok(
        &kb.op,
        json!({ "op": "workflow.define", "name": "mine",
                "spec": { "graph": "direct", "toggles": { "lands": ["operator", "coordinator"] } } }),
    );
    match ok(&kb.op, json!({ "op": "workflow.strategies" })) {
        Response::Strategies {
            strategies,
            default,
        } => {
            assert_eq!(default, "design-reviewed");
            let names: Vec<_> = strategies.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(
                names,
                vec!["design-reviewed", "coordinated", "autonomous", "mine@1"]
            );
        }
        other => panic!("{other:?}"),
    }
    let e = call(
        &kb.op,
        json!({ "op": "workflow.define", "name": "bad",
                "spec": { "graph": "direct", "toggles": { "lands": ["implementer"] } } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}

#[test]
fn rotating_with_keep_keeps_a_live_credential_and_replaces_anything_else() {
    let kb = Kb::new();
    let rotate = |keep: Option<&str>| match ok(
        &kb.op,
        json!({ "op": "role.rotate_container", "keep": keep }),
    ) {
        Response::Granted { grant, token } => (grant.id, token),
        other => panic!("{other:?}"),
    };
    let (id, token) = rotate(None);
    assert_eq!(
        rotate(Some(&token)),
        (id, token.clone()),
        "live: kept as it is"
    );
    let (id2, token2) = rotate(Some("not-a-token"));
    assert_ne!(token2, token, "unknown: rotated");
    assert!(
        call(&kb.as_token(&token), json!({ "op": "kb.ls" })).is_err(),
        "and the old one revoked"
    );
    let (_, token3) = rotate(Some(&token));
    assert_ne!(token3, token, "a revoked one is not kept");
    assert_ne!(id2, 0);
    // A live grant that is not the container's is never kept in its place.
    let a = add(&kb.op, "a");
    let (_, other) = grant(&kb.op, "coordinator", Some(&a), "someone");
    assert_ne!(rotate(Some(&other)).1, other);
}

#[test]
fn a_scoped_caller_adds_only_under_its_task_and_records_only_its_own_review() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let (_, coord) = grant(&kb.op, "coordinator", Some(&a), "c");
    let c = kb.as_token(&coord);
    let e = refused(&c, json!({ "op": "task.add", "text": "stray" }));
    assert!(e.message.contains("only under it"), "{e:?}");
    ok(
        &c,
        json!({ "op": "task.add", "text": "a subtask", "under": a }),
    );

    // An attested reviewer that has not bound yet may not record a review, and binding is explicit.
    let container = match ok(&kb.op, json!({ "op": "role.rotate_container" })) {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    ok(
        &kb.op,
        json!({ "op": "role.map", "agent_type": "reviewer", "role": "reviewer" }),
    );
    let ticket = match ok(
        &kb.as_token(&container),
        json!({ "op": "attest.mint", "session": "s", "agent_id": "r1",
                "agent_type": "reviewer", "tool_use_id": "t" }),
    ) {
        Response::Ticket { token } => token,
        other => panic!("{other:?}"),
    };
    let rev = kb.as_token(&ticket);
    let record = json!({ "op": "task.review_record", "repo": "r", "branch": "b",
                         "findings": "reviews/x" });
    let e = refused(&rev, record);
    assert!(e.message.contains("jkb role bind"), "{e:?}");
    let e = refused(
        &rev,
        json!({ "op": "task.review_file", "run": { "reviewers": 1, "returned": 1 },
                "ns": "repos/r/codereviews/x", "findings": [] }),
    );
    assert!(e.message.contains("jkb role bind"), "a filing too: {e:?}");
    ok(&rev, json!({ "op": "role.bind", "uid": a }));
    match ok(&rev, json!({ "op": "role.whoami" })) {
        Response::WhoAmI { whoami } => assert_eq!(whoami.task.as_deref(), Some(a.as_str())),
        other => panic!("{other:?}"),
    }
    // ...and once bound it files the round, as `/jkb-review`'s reviewer does after `jkb role bind`.
    // Under its repository's codereviews, which the task names: `jkb task start` records `repo=`.
    ok(
        &kb.op,
        json!({ "op": "task.tag", "uid": a, "facet_value": "repo=r", "mode": "set" }),
    );
    match ok(
        &rev,
        json!({ "op": "task.review_file", "run": { "reviewers": 1, "returned": 1 },
                "ns": "repos/r/codereviews/x",
                "findings": [{ "severity": "nit", "summary": "a nit" }] }),
    ) {
        Response::ReviewFiled { filed } => assert_eq!(filed.uids.len(), 1),
        other => panic!("{other:?}"),
    }
    let other = add(&kb.op, "task b");
    refused(&rev, json!({ "op": "role.bind", "uid": other }));
}

#[test]
fn a_scoped_caller_writes_no_shared_state_and_places_only_beside_its_task() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let (_, coord) = grant(&kb.op, "coordinator", Some(&a), "c");
    let c = kb.as_token(&coord);
    for shared in [
        json!({ "op": "lease.take", "name": "land:jkb", "holder": "x 1", "displace": "q 2" }),
        json!({ "op": "removal.add", "removal": { "worktree": "/w/b", "repo_root": "/r",
                "branch": "b", "uid": a, "recorded_at": 0, "accept_dirty": true,
                "delete_branch": true } }),
    ] {
        let e = refused(&c, shared);
        assert!(e.message.contains("no one task owns"), "{e:?}");
    }
    // Only where its own task is: never a review round's namespace, its own or another's.
    let e = refused(
        &c,
        json!({ "op": "task.add", "text": "late !p3", "under": a,
                "home": "repos/p/codereviews/r1/nit" }),
    );
    assert!(e.message.contains("only where that task is"), "{e:?}");
    let e = refused(
        &c,
        json!({ "op": "task.add", "text": "x +repos/p/codereviews/r2", "under": a }),
    );
    assert!(e.message.contains("only where that task is"), "{e:?}");
    ok(
        &c,
        json!({ "op": "task.add", "text": "a subtask", "under": a }),
    );
    let e = refused(
        &c,
        json!({ "op": "task.place", "uid": a, "ns": "repos/p/codereviews/r1/must-fix" }),
    );
    assert!(e.message.contains("only where that task is"), "{e:?}");
}

#[test]
fn a_refused_first_op_leaves_the_worker_unbound_and_a_label_is_not_the_credential() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let b_task = add(&kb.op, "task b");
    let container = match ok(&kb.op, json!({ "op": "role.rotate_container" })) {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    ok(
        &kb.op,
        json!({ "op": "role.map", "agent_type": "reviewer", "role": "reviewer" }),
    );
    let ticket = match ok(
        &kb.as_token(&container),
        json!({ "op": "attest.mint", "session": "s", "agent_id": "r1",
                "agent_type": "reviewer", "tool_use_id": "t" }),
    ) {
        Response::Ticket { token } => token,
        other => panic!("{other:?}"),
    };
    let rev = kb.as_token(&ticket);
    // Admitted — so bound — then refused by the op itself.
    let e = call(
        &rev,
        json!({ "op": "workflow.fire", "uid": a, "event": "no_such_event" }),
    )
    .unwrap_err();
    assert_ne!(e.code, ErrorCode::Forbidden, "{e:?}");
    match ok(&rev, json!({ "op": "role.whoami" })) {
        Response::WhoAmI { whoami } => assert_eq!(whoami.task, None, "the binding was undone"),
        other => panic!("{other:?}"),
    }
    ok(&rev, json!({ "op": "role.bind", "uid": b_task }));

    // A grant the operator labelled `container` is not the container credential.
    let (_, labelled) = grant(&kb.op, "implementer", Some(&a), "container");
    let e = refused(
        &kb.as_token(&labelled),
        json!({ "op": "attest.mint", "session": "s", "tool_use_id": "t2" }),
    );
    assert!(e.message.contains("container's own credential"), "{e:?}");
}

#[test]
fn a_session_holds_a_bounded_number_of_live_tickets() {
    let kb = Kb::new();
    let container = match ok(&kb.op, json!({ "op": "role.rotate_container" })) {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    let hook = kb.as_token(&container);
    let mint = |session: &str, i: usize| {
        call(
            &hook,
            json!({ "op": "attest.mint", "session": session, "tool_use_id": format!("t{i}") }),
        )
    };
    for i in 0..super::MAX_SESSION_TICKETS {
        mint("greedy", i).unwrap();
    }
    let e = mint("greedy", usize::MAX).unwrap_err();
    assert_eq!(e.code, ErrorCode::Busy, "{e:?}");
    mint("another", 0).expect("another session is not held to it");
    // Releasing makes room again.
    ok(
        &hook,
        json!({ "op": "attest.release", "session": "greedy", "tool_use_id": "t0" }),
    );
    mint("greedy", usize::MAX).unwrap();
}

/// An attested subagent's FIRST write binds it, and is held to the scope that binding makes: placing
/// its task into another task's review round is refused even then.
#[test]
fn a_first_write_that_binds_is_held_to_the_scope_it_binds() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let container = match ok(&kb.op, json!({ "op": "role.rotate_container" })) {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    ok(
        &kb.op,
        json!({ "op": "role.map", "agent_type": "implementer", "role": "implementer" }),
    );
    let ticket = match ok(
        &kb.as_token(&container),
        json!({ "op": "attest.mint", "session": "s", "agent_id": "i1",
                "agent_type": "implementer", "tool_use_id": "t" }),
    ) {
        Response::Ticket { token } => token,
        other => panic!("{other:?}"),
    };
    let e = refused(
        &kb.as_token(&ticket),
        json!({ "op": "task.place", "uid": a, "ns": "repos/p/codereviews/other/must-fix" }),
    );
    assert!(e.message.contains("only where that task is"), "{e:?}");
}

/// Every call of one attested subagent takes the same lock — two of its tickets included — so a
/// binding its failed call undoes cannot be one a concurrent call of it relied on. The main session
/// takes none, and a stopped subagent's lock goes with it.
#[test]
fn one_subagent_s_calls_share_one_lock_until_it_stops() {
    let kb = Kb::new();
    let container = match ok(&kb.op, json!({ "op": "role.rotate_container" })) {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    let hook = kb.as_token(&container);
    let mint = |agent: Option<&str>, tool: &str| match ok(
        &hook,
        json!({ "op": "attest.mint", "session": "s", "agent_id": agent, "tool_use_id": tool }),
    ) {
        Response::Ticket { token } => token,
        other => panic!("{other:?}"),
    };
    let (t1, t2, main) = (
        mint(Some("a1"), "x1"),
        mint(Some("a1"), "x2"),
        mint(None, "x3"),
    );
    let l1 = kb.tickets.agent_lock(&t1).unwrap().unwrap();
    let l2 = kb.tickets.agent_lock(&t2).unwrap().unwrap();
    assert!(
        Arc::ptr_eq(&l1, &l2),
        "one lock per subagent, whatever the ticket"
    );
    assert!(kb.tickets.agent_lock(&main).unwrap().is_none());
    ok(
        &hook,
        json!({ "op": "attest.release", "session": "s", "agent_id": "a1" }),
    );
    let t3 = mint(Some("a1"), "x4");
    let l3 = kb.tickets.agent_lock(&t3).unwrap().unwrap();
    assert!(!Arc::ptr_eq(&l1, &l3), "released with the subagent");
}

/// A call of an attested subagent waits while another call of the same subagent holds its lock — the
/// serialization the binding undo relies on, pinned by watching a call wait.
#[test]
fn a_subagent_s_call_waits_for_its_other_call() {
    let kb = Kb::new();
    let container = match ok(&kb.op, json!({ "op": "role.rotate_container" })) {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    ok(
        &kb.op,
        json!({ "op": "role.map", "agent_type": "reviewer", "role": "reviewer" }),
    );
    let ticket = match ok(
        &kb.as_token(&container),
        json!({ "op": "attest.mint", "session": "s", "agent_id": "a1",
                "agent_type": "reviewer", "tool_use_id": "t" }),
    ) {
        Response::Ticket { token } => token,
        other => panic!("{other:?}"),
    };
    let lock = kb.tickets.agent_lock(&ticket).unwrap().unwrap();
    let held = lock.lock().unwrap();
    let backend = kb.as_token(&ticket);
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let r = call(&backend, json!({ "op": "kb.ls" }));
        let _ = tx.send(r.is_ok());
    });
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        "the call ran while the subagent's other call held its lock"
    );
    drop(held);
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(10)),
        Ok(true)
    );
    worker.join().unwrap();
}

/// The lock map does not outgrow the subagents holding live tickets.
#[test]
fn the_subagent_lock_map_is_bounded_by_live_subagents() {
    let kb = Kb::new();
    let container = match ok(&kb.op, json!({ "op": "role.rotate_container" })) {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    let hook = kb.as_token(&container);
    let n = 2 * super::MAX_SESSION_TICKETS + 20;
    for i in 0..n {
        let session = format!("s{i}");
        let token = match ok(
            &hook,
            json!({ "op": "attest.mint", "session": session, "agent_id": "a", "tool_use_id": "t" }),
        ) {
            Response::Ticket { token } => token,
            other => panic!("{other:?}"),
        };
        drop(kb.tickets.agent_lock(&token).unwrap());
        // Its ticket released per tool call, as PostToolUse does, leaving the lock entry behind.
        ok(
            &hook,
            json!({ "op": "attest.release", "session": session, "tool_use_id": "t" }),
        );
    }
    assert!(
        kb.tickets.agent_locks() <= 2 * super::MAX_SESSION_TICKETS + 1,
        "{} locks for no live subagent",
        kb.tickets.agent_locks()
    );
}

/// A task whose workflow is parked at `landed` is not landed again by anyone but the operator, even
/// under a strategy whose `lands` toggle names the coordinator and with the task reopened — and the
/// workflow says the operator's `reopen` is next.
#[test]
fn a_parked_workflow_is_landed_again_only_after_the_operator_reopens_it() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    // Unscoped, as the container's own credential is: settling a task revokes grants scoped to it.
    let (_, token) = grant(&kb.op, "coordinator", None, "coord");
    let c = kb.as_token(&token);
    ok(
        &kb.op,
        json!({ "op": "workflow.set", "uid": a, "strategy": "autonomous" }),
    );
    ok(
        &kb.op,
        json!({ "op": "workflow.fire", "uid": a, "event": "override", "to": "landable",
                "reason": "reviewed" }),
    );
    // Really landed, then the task put back to work: its workflow stays parked at `landed`.
    ok(
        &kb.op,
        json!({ "op": "task.set", "uid": a, "status": "in_progress" }),
    );
    let landed = json!({ "op": "task.landed", "uid": a,
                         "landed": { "branch": "b", "onto": "o", "head": "abcd" } });
    match ok(&kb.op, landed.clone()) {
        Response::Landing { landing } => assert!(landing.moved, "{landing:?}"),
        other => panic!("{other:?}"),
    }
    // The queue re-running the branch is told the task landed, not that it is held.
    match ok(&c, landed.clone()) {
        Response::Landing { landing } => assert!(landing.refusal.is_none(), "{landing:?}"),
        other => panic!("{other:?}"),
    }
    // Nor new commits on the same branch: that is new work.
    let e = refused(
        &c,
        json!({ "op": "task.landed", "uid": a,
                "landed": { "branch": "b", "onto": "o", "head": "ffff" } }),
    );
    assert!(e.message.contains("parked at `landed`"), "{e:?}");
    // Not a landing somewhere else, though: that records a landing it never had.
    let e = refused(
        &c,
        json!({ "op": "task.landed", "uid": a,
                "landed": { "branch": "b", "onto": "elsewhere", "head": "abcd" } }),
    );
    assert!(e.message.contains("parked at `landed`"), "{e:?}");
    ok(
        &kb.op,
        json!({ "op": "task.set", "uid": a, "status": "open" }),
    );
    // The coordinator picks the task back up and lands it again: refused, the workflow is parked.
    ok(
        &c,
        json!({ "op": "task.set", "uid": a, "status": "in_progress" }),
    );
    let e = refused(&c, landed.clone());
    assert!(e.message.contains("parked at `landed`"), "{e:?}");
    match ok(&kb.op, json!({ "op": "workflow.show", "uid": a })) {
        Response::Workflow { workflow } => {
            assert_eq!(workflow.next_role, "operator");
            assert!(
                workflow.next_step.contains("reopen"),
                "{}",
                workflow.next_step
            );
        }
        other => panic!("{other:?}"),
    }
    // The operator reopens it; the coordinator's strategy then lets it land.
    ok(
        &kb.op,
        json!({ "op": "workflow.fire", "uid": a, "event": "reopen" }),
    );
    if let Err(e) = call(&c, landed) {
        assert_ne!(e.code, ErrorCode::Forbidden, "{e:?}");
    }
}

/// A caller held to a review finding lives inside the round, and still places nothing there: not a
/// subtask beside its finding, not a mirror of it into the round.
#[test]
fn a_caller_held_to_a_finding_places_nothing_in_its_round() {
    let kb = Kb::new();
    let filed = match ok(
        &kb.op,
        json!({ "op": "task.review_file", "run": { "reviewers": 1, "returned": 1 },
                "ns": "repos/p/codereviews/r7",
                "findings": [{ "severity": "must-fix", "summary": "bad" }] }),
    ) {
        Response::ReviewFiled { filed } => filed,
        other => panic!("{other:?}"),
    };
    let finding = filed.uids[0].clone();
    let (_, token) = grant(&kb.op, "implementer", Some(&finding), "impl");
    let c = kb.as_token(&token);
    let e = refused(
        &c,
        json!({ "op": "task.add", "text": "x !p0", "under": finding }),
    );
    assert!(e.message.contains("never in a review round"), "{e:?}");
    let e = refused(
        &c,
        json!({ "op": "task.place", "uid": finding, "ns": "repos/p/codereviews/r7/nit" }),
    );
    assert!(e.message.contains("never in a review round"), "{e:?}");
}

/// A subtask of something inside a review round, asked for by a caller held to a task outside it,
/// lives at the caller's own task's home — the nearest place it may write — rather than being refused.
#[test]
fn a_subtask_under_a_round_homes_at_the_callers_own_task() {
    let kb = Kb::new();
    ok(
        &kb.op,
        json!({ "op": "task.review_file", "run": { "reviewers": 1, "returned": 1 },
                "ns": "repos/p/codereviews/r8",
                "findings": [{ "severity": "nit", "summary": "meh" }] }),
    );
    let (task, home) = match ok(&kb.op, json!({ "op": "task.add", "text": "own task" })) {
        Response::Added { added } => (added.uid, added.home),
        other => panic!("{other:?}"),
    };
    assert!(!home.contains("codereviews"), "{home}");
    let inside = match ok(
        &kb.op,
        json!({ "op": "task.add", "text": "inside", "under": task,
                "home": "repos/p/codereviews/r8/nit" }),
    ) {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    let (_, token) = grant(&kb.op, "implementer", Some(&task), "impl");
    let c = kb.as_token(&token);
    match ok(
        &c,
        json!({ "op": "task.add", "text": "follow-up", "under": inside }),
    ) {
        Response::Added { added } => assert_eq!(added.home, home),
        other => panic!("{other:?}"),
    }
}

/// A round `/review-log` mounted and nobody has recorded yet is neither filed nor recorded, and is
/// still a review round: a worker held to one of its findings places nothing there, or the recording
/// would later snapshot that line as the round's.
#[test]
fn an_unrecorded_mounted_round_is_still_a_review_round() {
    let kb = Kb::new();
    let finding = match ok(
        &kb.op,
        json!({ "op": "task.add", "text": "mounted finding !p1",
                "home": "repos/p/codereviews/m1/must-fix" }),
    ) {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    let (_, token) = grant(&kb.op, "implementer", Some(&finding), "impl");
    let c = kb.as_token(&token);
    let e = refused(
        &c,
        json!({ "op": "task.add", "text": "x !p0", "under": finding }),
    );
    assert!(e.message.contains("never in a review round"), "{e:?}");
    let e = refused(
        &c,
        json!({ "op": "task.place", "uid": finding, "ns": "repos/p/codereviews/m1/nit" }),
    );
    assert!(e.message.contains("never in a review round"), "{e:?}");
    // Nor through the round's `tasks/` mirror, even where the finding is placed there too.
    ok(
        &kb.op,
        json!({ "op": "task.place", "uid": finding, "ns": "tasks/p/codereviews/m1/must-fix" }),
    );
    let e = refused(
        &c,
        json!({ "op": "task.add", "text": "y !p0", "under": finding,
                "home": "tasks/p/codereviews/m1/must-fix" }),
    );
    assert!(e.message.contains("never in a review round"), "{e:?}");
}

/// A task whose workflow is parked at `cancelled` is not landed by ticking it `done` first: only a
/// repeat of a live landing gets past a parked workflow (review round 6).
#[test]
fn a_cancelled_workflow_is_not_landed_by_ticking_it_done() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let (_, token) = grant(&kb.op, "coordinator", None, "coord");
    let c = kb.as_token(&token);
    ok(
        &kb.op,
        json!({ "op": "workflow.set", "uid": a, "strategy": "autonomous" }),
    );
    ok(
        &kb.op,
        json!({ "op": "task.set", "uid": a, "status": "cancelled" }),
    );
    ok(&kb.op, json!({ "op": "workflow.observe", "uid": a }));
    ok(&c, json!({ "op": "task.set", "uid": a, "status": "done" }));
    let e = refused(
        &c,
        json!({ "op": "task.landed", "uid": a,
                "landed": { "branch": "b", "onto": "o", "head": "abcd" } }),
    );
    assert!(e.message.contains("parked at `cancelled`"), "{e:?}");
}

/// A live landing is not proof a workflow is parked at `landed`: one parked at `cancelled` that the
/// operator then landed keeps its phase and gains a live landing, and a repeat of that landing is still
/// the operator's to allow (review round 7). (A landing the guard held keeps the workflow from parking
/// at `cancelled` at all: `observed_cancelled` needs no live landing.)
#[test]
fn a_live_landing_is_not_repeated_past_a_cancelled_workflow() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let (_, token) = grant(&kb.op, "coordinator", None, "coord");
    let c = kb.as_token(&token);
    ok(
        &kb.op,
        json!({ "op": "workflow.set", "uid": a, "strategy": "autonomous" }),
    );
    ok(
        &kb.op,
        json!({ "op": "task.set", "uid": a, "status": "cancelled" }),
    );
    ok(&kb.op, json!({ "op": "workflow.observe", "uid": a }));
    ok(
        &kb.op,
        json!({ "op": "task.set", "uid": a, "status": "in_progress" }),
    );
    let landed = json!({ "op": "task.landed", "uid": a,
                         "landed": { "branch": "b", "onto": "o", "head": "abcd" } });
    match ok(&kb.op, landed.clone()) {
        Response::Landing { landing } => assert!(landing.moved, "{landing:?}"),
        other => panic!("{other:?}"),
    }
    match ok(&kb.op, json!({ "op": "workflow.show", "uid": a })) {
        Response::Workflow { workflow } => assert_eq!(workflow.phase, "cancelled"),
        other => panic!("{other:?}"),
    }
    let e = refused(&c, landed);
    assert!(e.message.contains("parked at `cancelled`"), "{e:?}");
}

/// A grant its minter may no longer grant is listed only with the revoked ones, and marked there: it
/// never authenticates, and the operator should see which to revoke (review rounds 6–7).
#[test]
fn a_grant_no_longer_grantable_is_marked_in_the_full_listing() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let (_, coord) = grant(&kb.op, "coordinator", None, "coord");
    let (id, _) = grant(&kb.as_token(&coord), "implementer", Some(&a), "w");
    // As a build from before the table was tightened minted it.
    kb.db
        .write_txn("test", move |c, _| {
            c.execute(
                "UPDATE role_grants SET role = 'reviewer' WHERE id = ?1",
                [id],
            )?;
            Ok(())
        })
        .unwrap();
    let listed = |all: bool| match ok(&kb.op, json!({ "op": "role.list", "all": all })) {
        Response::Grants { listing } => listing.grants,
        other => panic!("{other:?}"),
    };
    assert!(!listed(false).iter().any(|g| g.id == id));
    let stale = listed(true)
        .into_iter()
        .find(|g| g.id == id)
        .expect("listed");
    assert!(!stale.grantable, "{stale:?}");
    assert!(listed(true)
        .iter()
        .filter(|g| g.id != id)
        .all(|g| g.grantable));
}

/// `task.move` is granted as `task.place` is — the same permission, held to the moved task — and a
/// scoped caller is held to the new parent too, as `task.add --under` is: a subtask moved under an
/// unrelated task would hold that task off the frontier and its land gate.
#[test]
fn task_move_is_granted_as_task_place_and_held_to_the_parent_s_scope() {
    let parse = |r: serde_json::Value| -> crate::Request { serde_json::from_value(r).unwrap() };
    let place = parse(json!({ "op": "task.place", "uid": "u", "ns": "n" }));
    let moved = parse(json!({ "op": "task.move", "uid": "u", "under": "p" }));
    assert_eq!(moved.permission(), place.permission());
    assert_eq!(moved.target(), place.target());

    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let unrelated = add(&kb.op, "unrelated");
    let under_a = |text: &str| match ok(
        &kb.op,
        json!({ "op": "task.add", "text": text, "under": a }),
    ) {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    let (sub, sibling) = (under_a("sub"), under_a("sibling"));
    let (_, token) = grant(&kb.op, "coordinator", Some(&a), "coord");
    let c = kb.as_token(&token);

    let e = refused(
        &c,
        json!({ "op": "task.move", "uid": sub, "under": unrelated }),
    );
    assert!(
        e.message.contains("moves tasks only under that task"),
        "{e:?}"
    );
    match ok(
        &c,
        json!({ "op": "task.move", "uid": sub, "under": sibling }),
    ) {
        Response::TaskMoved { moved } => assert!(moved.moved, "{moved:?}"),
        other => panic!("{other:?}"),
    }
    let e = refused(
        &c,
        json!({ "op": "task.move", "uid": unrelated, "under": a }),
    );
    assert!(e.message.contains("scoped to another task"), "{e:?}");
    let (_, rev) = grant(&kb.op, "reviewer", Some(&a), "rev-1");
    refused(
        &kb.as_token(&rev),
        json!({ "op": "task.move", "uid": sub, "under": a }),
    );
}

/// An attested subagent's first write binds it to the task that write names; a `task.move` first
/// would bind it to the moved task and then refuse every parent outside it. Refused up front, with
/// how to bind, and the subagent left unbound.
#[test]
fn an_attested_subagent_s_first_write_cannot_be_a_move() {
    let kb = Kb::new();
    let a = add(&kb.op, "task a");
    let under_a = |text: &str| match ok(
        &kb.op,
        json!({ "op": "task.add", "text": text, "under": a }),
    ) {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    let (sub, sibling) = (under_a("sub"), under_a("sibling"));
    let container = match ok(&kb.op, json!({ "op": "role.rotate_container" })) {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    ok(
        &kb.op,
        json!({ "op": "role.map", "agent_type": "implementer", "role": "implementer" }),
    );
    let hook = kb.as_token(&container);
    let ticket = |tool: &str| match ok(
        &hook,
        json!({ "op": "attest.mint", "session": "s1", "agent_id": "ag1",
                "agent_type": "implementer", "tool_use_id": tool }),
    ) {
        Response::Ticket { token } => kb.as_token(&token),
        other => panic!("{other:?}"),
    };
    let e = refused(
        &ticket("tool-1"),
        json!({ "op": "task.move", "uid": sub, "under": sibling }),
    );
    assert!(e.message.contains("jkb role bind"), "{e:?}");
    // Still unbound: it binds to `a` and then moves within it.
    ok(&ticket("tool-2"), json!({ "op": "role.bind", "uid": a }));
    match ok(
        &ticket("tool-3"),
        json!({ "op": "task.move", "uid": sub, "under": sibling }),
    ) {
        Response::TaskMoved { moved } => assert!(moved.moved, "{moved:?}"),
        other => panic!("{other:?}"),
    }
}
