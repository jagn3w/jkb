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

    // It mints workers for its own task only, and never another coordinator.
    let (_, rev) = grant(&c, "reviewer", Some(&a), "rev-1");
    refused(
        &c,
        json!({ "op": "role.grant", "role": "reviewer", "task": b_task, "agent": "x" }),
    );
    refused(
        &c,
        json!({ "op": "role.grant", "role": "coordinator", "task": a, "agent": "x" }),
    );

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
            assert!(workflow.history.len() == 2, "{:?}", workflow.history);
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
