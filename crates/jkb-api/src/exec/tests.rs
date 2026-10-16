//! The run-request ops: who may ask (an attested session, never the operator or a grant), who may
//! answer (the operator alone), and that a session reads only its own requests.

use std::sync::Arc;

use jkb_core::Db;
use jkb_rbac::Grants as _;
use serde_json::{json, Value};

use crate::rbac::{Caller, OpPermission, Tickets, OP_GRANTS};
use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};

fn call(b: &LocalBackend, r: &Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r.clone()).expect("request parses"))
}

fn ok(b: &LocalBackend, r: &Value) -> Response {
    call(b, r).unwrap_or_else(|e| panic!("{r}: {e:?}"))
}

fn code(b: &LocalBackend, r: &Value) -> ErrorCode {
    call(b, r).expect_err(&r.to_string()).code
}

struct Kb {
    db: Db,
    op: LocalBackend,
    tickets: Arc<Tickets>,
    container: String,
}

impl Kb {
    fn new() -> Self {
        let db = Db::open_in_memory().unwrap();
        let tickets = Arc::new(Tickets::default());
        let op = LocalBackend::new(db.clone()).with_tickets(Arc::clone(&tickets));
        let container = match ok(&op, &json!({ "op": "role.rotate_container" })) {
            Response::Granted { token, .. } => token,
            other => panic!("{other:?}"),
        };
        for (t, r) in [("implementer", "implementer"), ("reviewer", "reviewer")] {
            ok(
                &op,
                &json!({ "op": "role.map", "agent_type": t, "role": r }),
            );
        }
        Self {
            db,
            op,
            tickets,
            container,
        }
    }

    fn as_token(&self, token: &str) -> LocalBackend {
        LocalBackend::new(self.db.clone())
            .with_tickets(Arc::clone(&self.tickets))
            .with_caller(Caller::Token(token.to_owned()))
    }

    /// A tool call's backend: `session`'s main session, or its subagent `agent` of type `kind`.
    fn ticket(&self, session: &str, agent: Option<(&str, &str)>, tool: &str) -> LocalBackend {
        let hook = self.as_token(&self.container);
        let token = match ok(
            &hook,
            &json!({ "op": "attest.mint", "session": session,
                     "agent_id": agent.map(|a| a.0), "agent_type": agent.map(|a| a.1),
                     "tool_use_id": tool }),
        ) {
            Response::Ticket { token } => token,
            other => panic!("{other:?}"),
        };
        self.as_token(&token)
    }
}

fn ask(argv: &[&str]) -> Value {
    json!({ "op": "exec.request", "target": "host", "dir": "/w/repo", "dir_side": "host",
            "argv": argv, "reason": "the sandbox cannot reach the host's docker",
            "pin": { "head": "ab".repeat(20), "changes": "cd".repeat(32) } })
}

fn requested(r: Response) -> (crate::exec::ExecView, bool) {
    match r {
        Response::ExecRequested { request, folded } => (request, folded),
        other => panic!("{other:?}"),
    }
}

fn view(r: Response) -> crate::exec::ExecView {
    match r {
        Response::ExecRequest { request } => request,
        other => panic!("{other:?}"),
    }
}

#[test]
fn every_role_but_the_operator_may_ask_and_none_may_answer() {
    use jkb_core::roles::Role;
    use jkb_rbac::Role as _;
    for role in Role::ALL.iter().copied().filter(|r| *r != Role::Operator) {
        assert!(
            OP_GRANTS.permits(role, OpPermission::ExecRequest),
            "{role:?}"
        );
    }
    for op in [
        json!({ "op": "exec.begin", "id": "exec-1", "runner": "r" }),
        json!({ "op": "exec.decline", "id": "exec-1", "note": "n" }),
        json!({ "op": "exec.refuse", "id": "exec-1", "reason": "dir_gone" }),
        json!({ "op": "exec.finish", "id": "exec-1", "runner": "r", "outcome": "exited",
                "exit_status": 0, "total_bytes": 0 }),
        json!({ "op": "exec.output", "id": "exec-1", "runner": "r", "chunk_b64": "eA==" }),
    ] {
        let r: crate::Request = serde_json::from_value(op).unwrap();
        assert_eq!(r.permission(), OpPermission::Admin, "{}", r.op());
    }
}

#[test]
fn an_implementer_asks_in_its_own_name_and_cannot_answer_itself() {
    let kb = Kb::new();
    let task = match ok(&kb.op, &json!({ "op": "task.add", "text": "build it" })) {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    let imp = kb.ticket("s1", Some(("ag1", "implementer")), "t1");
    ok(&imp, &json!({ "op": "role.bind", "uid": task }));
    let imp = kb.ticket("s1", Some(("ag1", "implementer")), "t2");
    let (r, folded) = requested(ok(&imp, &ask(&["./scripts/check.sh"])));
    assert!(!folded);
    assert_eq!(r.state, "pending");
    assert_eq!(r.session, "s1");
    assert_eq!(r.agent_type.as_deref(), Some("implementer"));
    assert_eq!(r.role, "implementer");
    assert_eq!(
        r.task.as_deref(),
        Some(task.as_str()),
        "the bound task is recorded"
    );
    assert_eq!(r.topic, format!("exec/{}", &r.id["exec-".len()..]));

    // Asking again folds.
    let (again, folded) = requested(ok(&imp, &ask(&["./scripts/check.sh"])));
    assert!(folded);
    assert_eq!(again.id, r.id);

    // Not one of the moves out of pending is the session's.
    let id = r.id.clone();
    for op in [
        json!({ "op": "exec.begin", "id": id, "runner": "me",
                "pin": { "head": "ab".repeat(20), "changes": "cd".repeat(32) } }),
        json!({ "op": "exec.decline", "id": id, "note": "n" }),
        json!({ "op": "exec.refuse", "id": id, "reason": "dir_gone" }),
        json!({ "op": "exec.finish", "id": id, "runner": "me", "outcome": "exited",
                "exit_status": 0, "total_bytes": 0 }),
        json!({ "op": "exec.output", "id": id, "runner": "me", "chunk_b64": "eA==" }),
    ] {
        assert_eq!(code(&imp, &op), ErrorCode::Forbidden, "{op}");
    }
    // …nor may it forge a message on its topic.
    let forged = json!({ "op": "mq.send", "topic": r.topic, "key": "k", "kind": "exec.end",
                         "payload": { "state": "finished", "exit_status": 0 }, "producer": "x" });
    assert_eq!(code(&imp, &forged), ErrorCode::Invalid);
    assert_eq!(
        view(ok(&imp, &json!({ "op": "exec.status", "id": id }))).state,
        "pending"
    );
}

#[test]
fn an_unbound_subagent_and_a_main_session_may_ask_too() {
    let kb = Kb::new();
    let rev = kb.ticket("s1", Some(("ag2", "reviewer")), "t1");
    let (r, _) = requested(ok(&rev, &ask(&["ls"])));
    assert_eq!((r.role.as_str(), r.task.as_deref()), ("reviewer", None));
    let main = kb.ticket("s1", None, "t2");
    let (m, _) = requested(ok(&main, &ask(&["ls", "-la"])));
    assert_eq!(
        m.role, "coordinator",
        "the main session holds the container's role"
    );
    assert_eq!(m.agent_type, None);
}

#[test]
fn the_operator_and_a_grant_name_no_session_and_cannot_ask() {
    let kb = Kb::new();
    let e = call(&kb.op, &ask(&["ls"])).unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden);
    assert!(e.message.contains("names no session"), "{e:?}");
    let grant = kb.as_token(&kb.container);
    assert_eq!(code(&grant, &ask(&["ls"])), ErrorCode::Forbidden);
    assert_eq!(
        code(&grant, &json!({ "op": "exec.list" })),
        ErrorCode::Forbidden
    );
}

#[test]
fn a_session_reads_only_its_own_requests_and_the_operator_reads_all() {
    let kb = Kb::new();
    let s1 = kb.ticket("s1", None, "t1");
    let s2 = kb.ticket("s2", None, "t2");
    let (a, _) = requested(ok(&s1, &ask(&["a"])));
    let (b, _) = requested(ok(&s2, &ask(&["b"])));
    assert_eq!(
        code(&s2, &json!({ "op": "exec.status", "id": a.id })),
        ErrorCode::NotFound,
        "another session's request does not exist for it"
    );
    let ids = |r: Response| match r {
        Response::ExecRequests { requests, .. } => {
            requests.into_iter().map(|v| v.id).collect::<Vec<_>>()
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(
        ids(ok(&s1, &json!({ "op": "exec.list" }))),
        vec![a.id.clone()]
    );
    assert_eq!(
        ids(ok(
            &kb.op,
            &json!({ "op": "exec.list", "state": "pending" })
        )),
        vec![a.id.clone(), b.id.clone()]
    );
    assert_eq!(
        code(&kb.op, &json!({ "op": "exec.list", "state": "approved" })),
        ErrorCode::Invalid
    );
    assert_eq!(
        code(&kb.op, &json!({ "op": "exec.status", "id": "exec-999" })),
        ErrorCode::NotFound
    );
}

#[test]
fn the_operator_runs_a_request_and_the_session_reads_how_it_ended() {
    let kb = Kb::new();
    let s1 = kb.ticket("s1", None, "t1");
    let (r, _) = requested(ok(&s1, &ask(&["make"])));
    let pin = json!({ "head": "ab".repeat(20), "changes": "cd".repeat(32) });
    match ok(
        &kb.op,
        &json!({ "op": "exec.begin", "id": r.id, "runner": "app:1", "pin": pin }),
    ) {
        Response::ExecBegun { begun, request } => {
            assert_eq!(begun, "running");
            assert_eq!(request.runner.as_deref(), Some("app:1"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        code(
            &kb.op,
            &json!({ "op": "exec.begin", "id": r.id, "runner": "app:1", "pin": pin })
        ),
        ErrorCode::Stale,
        "a request runs once"
    );
    match ok(
        &kb.op,
        &json!({ "op": "exec.output", "id": r.id, "runner": "app:1", "chunk_b64": "aGkK" }),
    ) {
        Response::ExecOutputSent { n, .. } => assert_eq!(n, 0),
        other => panic!("{other:?}"),
    }
    let done = view(ok(
        &kb.op,
        &json!({ "op": "exec.finish", "id": r.id, "runner": "app:1", "outcome": "exited",
                 "exit_status": 2, "tail_b64": "aGkK", "total_bytes": 3,
                 "log_path": "/logs/x.log" }),
    ));
    assert_eq!(
        (done.state.as_str(), done.exit_status),
        ("finished", Some(2))
    );
    let seen = view(ok(&s1, &json!({ "op": "exec.status", "id": r.id })));
    let out = seen.output.expect("the output is on the status");
    assert_eq!(out.tail_b64, "aGkK");
    assert_eq!(out.chunks, 1);
    assert_eq!(out.log_path.as_deref(), Some("/logs/x.log"));
}

#[test]
fn a_begin_with_another_pin_refuses_and_a_decline_carries_its_note() {
    let kb = Kb::new();
    let s1 = kb.ticket("s1", None, "t1");
    let (a, _) = requested(ok(&s1, &ask(&["a"])));
    match ok(
        &kb.op,
        &json!({ "op": "exec.begin", "id": a.id, "runner": "app",
                 "pin": { "head": "ab".repeat(20), "changes": "00".repeat(32) } }),
    ) {
        Response::ExecBegun { begun, request } => {
            assert_eq!(begun, "refused");
            assert_eq!(request.refuse_reason.as_deref(), Some("tree_changed"));
        }
        other => panic!("{other:?}"),
    }
    let (b, _) = requested(ok(&s1, &ask(&["b"])));
    let d = view(ok(
        &kb.op,
        &json!({ "op": "exec.decline", "id": b.id, "note": "ask for `make check`" }),
    ));
    assert_eq!(d.note.as_deref(), Some("ask for `make check`"));
    assert_eq!(
        view(ok(&s1, &json!({ "op": "exec.status", "id": b.id }))).state,
        "declined"
    );
    assert_eq!(
        code(
            &kb.op,
            &json!({ "op": "exec.refuse", "id": b.id, "reason": "nope" })
        ),
        ErrorCode::Invalid
    );
}

#[test]
fn a_malformed_request_is_invalid_and_an_unknown_field_is_refused() {
    let kb = Kb::new();
    let s1 = kb.ticket("s1", None, "t1");
    let mut bad = ask(&["--rm"]);
    assert_eq!(code(&s1, &bad), ErrorCode::Invalid);
    bad = ask(&["ls"]);
    bad["target"] = json!("moon");
    assert_eq!(code(&s1, &bad), ErrorCode::Invalid);
    bad = ask(&["ls"]);
    bad["session"] = json!("someone-else");
    assert!(
        serde_json::from_value::<crate::Request>(bad).is_err(),
        "the requester is never a request field"
    );
}
