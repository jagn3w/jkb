//! The run-request ops: who may ask (an attested session, never the operator or a grant), who may
//! answer (the operator alone), and that a session reads only its own requests.

use std::sync::Arc;

use jkb_core::Db;
use jkb_rbac::Grants as _;
use serde_json::{json, Value};

use crate::rbac::{Caller, OpPermission, Tickets, OP_GRANTS};
use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};

fn call(b: &LocalBackend, r: &Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(crate::test_repo::in_repo(b, r.clone())).expect("request parses"))
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
                "recomputed_pin": { "head": "ab".repeat(20), "changes": "cd".repeat(32) } }),
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
        &json!({ "op": "exec.begin", "id": r.id, "runner": "app:1", "recomputed_pin": pin }),
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
            &json!({ "op": "exec.begin", "id": r.id, "runner": "app:1", "recomputed_pin": pin })
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
                 "recomputed_pin": { "head": "ab".repeat(20), "changes": "00".repeat(32) } }),
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

#[test]
fn a_request_s_topic_is_followed_by_its_requester_alone() {
    let kb = Kb::new();
    let s1 = kb.ticket("s1", None, "t1");
    let s2 = kb.ticket("s2", None, "t2");
    let (r, _) = requested(ok(&s1, &ask(&["make"])));
    ok(
        &kb.op,
        &json!({ "op": "exec.decline", "id": r.id, "note": "ask for `make check`" }),
    );
    let follow = json!({ "op": "exec.follow", "id": r.id, "max": 10 });
    match ok(&s1, &follow) {
        Response::Messages { messages } => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].payload["note"], "ask for `make check`");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(ok(&kb.op, &follow), Response::Messages { .. }));
    assert_eq!(
        code(&s2, &follow),
        ErrorCode::NotFound,
        "another session's stream"
    );
    // No queue operation reaches the topic, for a session or for the operator.
    for who in [&s1, &s2, &kb.op] {
        for op in [
            json!({ "op": "mq.tail", "topic": r.topic, "limit": 10 }),
            json!({ "op": "mq.group_create", "topic": r.topic, "group": "g" }),
            json!({ "op": "mq.poll", "topic": r.topic, "group": "g", "max": 10 }),
            json!({ "op": "mq.ack", "topic": r.topic, "group": "g", "seq": 1 }),
            json!({ "op": "mq.group_delete", "topic": r.topic, "group": "g" }),
        ] {
            assert_eq!(code(who, &op), ErrorCode::Invalid, "{op}");
        }
    }
    match ok(&kb.op, &json!({ "op": "mq.inspect" })) {
        Response::Topics { topics } => {
            assert!(topics.iter().all(|t| !t.name.starts_with("exec/")));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_session_s_end_expires_its_requests_only_from_the_harness_or_itself() {
    let kb = Kb::new();
    let end = |session: &str, pid: &str| {
        json!({ "op": "session.ended", "session": session, "reason": "exit",
                "pid": pid, "instance": "h" })
    };
    let state = |id: &str| view(ok(&kb.op, &json!({ "op": "exec.status", "id": id }))).state;
    let s1 = kb.ticket("s1", None, "t1");
    let (r, _) = requested(ok(&s1, &ask(&["make"])));
    // Another session's ticket, and an operator-minted grant that is not the container's, record an
    // end for s1 — and cost it nothing.
    ok(&kb.ticket("s2", None, "t2"), &end("s1", "10"));
    let (_, other_grant) = match ok(
        &kb.op,
        &json!({ "op": "role.grant", "role": "coordinator", "agent": "someone" }),
    ) {
        Response::Granted { grant, token } => (grant.id, token),
        other => panic!("{other:?}"),
    };
    ok(&kb.as_token(&other_grant), &end("s1", "10"));
    assert_eq!(state(&r.id), "pending");
    // The container's SessionEnd hook — the real producer, with the container credential — does.
    ok(&kb.as_token(&kb.container), &end("s1", "10"));
    assert_eq!(state(&r.id), "expired");
    // So does the session's own tool call, and the operator.
    let s3 = kb.ticket("s3", None, "t3");
    let (q, _) = requested(ok(&s3, &ask(&["make"])));
    ok(&kb.ticket("s3", None, "t4"), &end("s3", "11"));
    assert_eq!(state(&q.id), "expired");
    let s4 = kb.ticket("s4", None, "t5");
    let (w, _) = requested(ok(&s4, &ask(&["make"])));
    ok(&kb.op, &end("s4", "12"));
    assert_eq!(state(&w.id), "expired");
    // The gone-sweep's `session.gone`, from the container credential, likewise.
    let s5 = kb.ticket("s5", None, "t6");
    let (g, _) = requested(ok(&s5, &ask(&["make"])));
    ok(
        &kb.op,
        &json!({ "op": "session.started", "session": "s5", "source": "startup",
                 "pid": "13", "instance": "h" }),
    );
    ok(
        &kb.as_token(&kb.container),
        &json!({ "op": "session.gone", "session": "s5", "pid": "13", "instance": "h" }),
    );
    assert_eq!(state(&g.id), "expired");
}

/// The run-request index (`exec/requests`) is the operator's to keep a group on — the app's live
/// queue — and, under `inbox/`, the container forwarder's: a session, or the container's credential
/// for any other group, is refused every queue operation on it, as on any `exec/` topic, and the
/// hints carry no argv.
#[test]
fn the_request_index_is_read_by_the_operator_alone() {
    let kb = Kb::new();
    let s1 = kb.ticket("s1", None, "t1");
    let container = kb.as_token(&kb.container);
    let topic = jkb_core::exec::INDEX_TOPIC;
    for who in [&s1, &container] {
        for op in [
            json!({ "op": "mq.group_create", "topic": topic, "group": "code-factory" }),
            json!({ "op": "mq.poll", "topic": topic, "group": "code-factory", "max": 10 }),
            json!({ "op": "mq.ack", "topic": topic, "group": "code-factory", "seq": 1 }),
            json!({ "op": "mq.group_delete", "topic": topic, "group": "code-factory" }),
            json!({ "op": "mq.send", "topic": topic, "key": "k", "kind": "exec.changed",
                    "payload": {}, "producer": "p" }),
        ] {
            let c = code(who, &op);
            assert!(
                matches!(c, ErrorCode::Invalid | ErrorCode::Forbidden),
                "{op}: {c:?}"
            );
        }
    }
    ok(
        &kb.op,
        &json!({ "op": "mq.group_create", "topic": topic, "group": "code-factory" }),
    );
    let (r, _) = requested(ok(&s1, &ask(&["make", "secret-arg"])));
    ok(
        &kb.op,
        &json!({ "op": "exec.decline", "id": r.id, "note": "no" }),
    );
    let poll = json!({ "op": "mq.poll", "topic": topic, "group": "code-factory", "max": 10 });
    let messages = match ok(&kb.op, &poll) {
        Response::Messages { messages } => messages,
        other => panic!("{other:?}"),
    };
    let seen: Vec<(Value, Value)> = messages
        .iter()
        .map(|m| (m.payload["id"].clone(), m.payload["state"].clone()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (json!(r.id), json!("pending")),
            (json!(r.id), json!("declined"))
        ]
    );
    assert!(messages
        .iter()
        .all(|m| !m.payload.to_string().contains("secret-arg")));
    // The operator may still not SEND there: only jkb's own record does.
    assert_eq!(
        code(
            &kb.op,
            &json!({ "op": "mq.send", "topic": topic, "key": "k", "kind": "exec.changed",
                     "payload": {}, "producer": "p" })
        ),
        ErrorCode::Invalid
    );
    let seq = messages.last().unwrap().seq;
    ok(
        &kb.op,
        &json!({ "op": "mq.ack", "topic": topic, "group": "code-factory", "seq": seq }),
    );
    match ok(&kb.op, &poll) {
        Response::Messages { messages } => assert!(messages.is_empty()),
        other => panic!("{other:?}"),
    }
    match ok(
        &kb.op,
        &json!({ "op": "mq.group_delete", "topic": topic, "group": "code-factory" }),
    ) {
        Response::GroupDeleted { deleted } => assert!(deleted),
        other => panic!("{other:?}"),
    }
}

/// One hook take: what it handed over (acknowledged, as the hook does once it printed them), and
/// the open requests.
fn take(b: &LocalBackend, session: &str, agent: Option<&str>) -> (Vec<String>, Vec<String>) {
    let (lines, ids, open) = take_unacked(b, session, agent, true);
    if !ids.is_empty() {
        match ok(
            b,
            &json!({ "op": "exec.inbox.ack", "session": session, "ids": ids }),
        ) {
            Response::ExecInboxAcked { count } => assert_eq!(count, ids.len()),
            other => panic!("{other:?}"),
        }
    }
    (lines, open)
}

fn take_unacked(
    b: &LocalBackend,
    session: &str,
    agent: Option<&str>,
    open: bool,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    match ok(
        b,
        &json!({ "op": "exec.inbox.take", "session": session, "agent_id": agent, "open": open }),
    ) {
        Response::ExecInboxTaken { lines, ids, open } => (
            lines,
            ids,
            open.iter().map(crate::exec::OpenRequest::line).collect(),
        ),
        other => panic!("{other:?}"),
    }
}

fn run_to_end(kb: &Kb, id: &str, chunk: &[u8]) {
    use base64::Engine as _;
    ok(
        &kb.op,
        &json!({ "op": "exec.begin", "id": id, "runner": "app",
                        "recomputed_pin": { "head": "ab".repeat(20), "changes": "cd".repeat(32) } }),
    );
    let b64 = base64::engine::general_purpose::STANDARD.encode(chunk);
    ok(
        &kb.op,
        &json!({ "op": "exec.output", "id": id, "runner": "app", "chunk_b64": b64 }),
    );
    ok(
        &kb.op,
        &json!({ "op": "exec.finish", "id": id, "runner": "app", "outcome": "exited",
                        "exit_status": 0, "total_bytes": chunk.len() }),
    );
}

/// The hook's take is the harness's: the container credential and the operator hand a session its
/// results; a ticket — even the session's own — or another grant cannot, so no tool call can swallow a
/// result before it is heard.
#[test]
fn the_inbox_take_is_the_harness_s_alone() {
    let kb = Kb::new();
    let take_op = json!({ "op": "exec.inbox.take", "session": "s1" });
    assert_eq!(
        code(&kb.ticket("s1", None, "t1"), &take_op),
        ErrorCode::Forbidden
    );
    assert_eq!(
        code(
            &kb.ticket("s1", Some(("a1", "implementer")), "t2"),
            &take_op
        ),
        ErrorCode::Forbidden
    );
    let other_grant = match ok(
        &kb.op,
        &json!({ "op": "role.grant", "role": "coordinator", "agent": "someone" }),
    ) {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        code(&kb.as_token(&other_grant), &take_op),
        ErrorCode::Forbidden
    );
    take(&kb.as_token(&kb.container), "s1", None);
    take(&kb.op, "s1", None);
    assert_eq!(
        code(
            &kb.op,
            &json!({ "op": "exec.inbox.take", "session": "not a session" })
        ),
        ErrorCode::Invalid
    );
    assert_eq!(
        crate::Request::ExecInboxTake {
            session: "s".into(),
            agent_id: None,
            open: false
        }
        .permission(),
        OpPermission::Attest
    );
}

/// A session hears its result once: the first take hands over the message, with the warning the run
/// printed; the next finds nothing. A subagent's result waits for that subagent, until it stops.
#[test]
fn a_result_reaches_the_agent_that_filed_it_once() {
    let kb = Kb::new();
    let hook = kb.as_token(&kb.container);
    let (main_req, _) = requested(ok(&kb.ticket("s1", None, "t1"), &ask(&["land"])));
    let (sub_req, _) = requested(ok(
        &kb.ticket("s1", Some(("a1", "implementer")), "t2"),
        &ask(&["check"]),
    ));
    let (lines, open) = take(&hook, "s1", None);
    assert!(lines.is_empty());
    assert_eq!(open.len(), 2, "{open:?}");
    assert!(open[1].contains("running") || open[1].contains("pending"));

    run_to_end(&kb, &main_req.id, b"warning: 1 task is unplanned\nok\n");
    run_to_end(&kb, &sub_req.id, b"done\n");

    let (lines, open) = take(&hook, "s1", None);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with(&format!(
        "[jkb run request] {} finished exit 0",
        main_req.id
    )));
    assert!(
        lines[0].contains("\nwarning: 1 task is unplanned\n"),
        "{}",
        lines[0]
    );
    assert!(open.is_empty());
    assert!(take(&hook, "s1", None).0.is_empty(), "delivered once");
    assert!(take(&hook, "s2", None).0.is_empty(), "another session's");

    let (lines, _) = take(&hook, "s1", Some("a1"));
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("filed by implementer (a1)"),
        "{}",
        lines[0]
    );

    // A subagent that stops before its next tool call: its result becomes the main agent's.
    let (later, _) = requested(ok(
        &kb.ticket("s1", Some(("a2", "implementer")), "t3"),
        &ask(&["again"]),
    ));
    run_to_end(&kb, &later.id, b"x\n");
    assert!(take(&hook, "s1", None).0.is_empty());
    ok(
        &hook,
        &json!({ "op": "attest.release", "session": "s1", "agent_id": "a2" }),
    );
    assert_eq!(take(&hook, "s1", None).0.len(), 1);

    // `exec.status` carries the same message, and reading it delivers nothing.
    let v = view(ok(
        &kb.op,
        &json!({ "op": "exec.status", "id": main_req.id }),
    ));
    assert!(v
        .delivery_text
        .unwrap()
        .contains("warning: 1 task is unplanned"));
}

/// An inbox is honoured only from whoever may vouch for the session — never from another session's
/// ticket — and no listing carries its token.
#[test]
fn an_inbox_is_set_only_by_the_harness_or_the_session_and_never_listed() {
    let kb = Kb::new();
    let start = |session: &str, pid: &str| {
        json!({ "op": "session.started", "session": session, "source": "startup",
                "pid": pid, "instance": "h#b/ns",
                "inbox": { "socket": "/tmp/cc-socks/9.sock", "token": "sekrit-token" } })
    };
    ok(&kb.ticket("s2", None, "t1"), &start("s1", "10"));
    ok(&kb.ticket("s1", None, "t2"), &start("s1", "11"));
    ok(&kb.as_token(&kb.container), &start("s3", "12"));
    let rows = match ok(&kb.op, &json!({ "op": "session.list", "all": true })) {
        Response::ClaudeSessions { sessions, .. } => sessions,
        other => panic!("{other:?}"),
    };
    let has = |s: &str, pid: &str| {
        rows.iter()
            .find(|r| r.session == s && r.pid == pid)
            .unwrap()
            .inbox
    };
    assert!(
        !has("s1", "10"),
        "another session's ticket cannot point s1's results anywhere"
    );
    assert!(has("s1", "11"));
    assert!(has("s3", "12"));
    let listed = serde_json::to_string(&rows).unwrap();
    assert!(!listed.contains("sekrit"), "{listed}");
    let targets = kb
        .db
        .read(|c| jkb_core::claude_session::inbox_targets(c, "s1", "h#b"))
        .unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].inbox.token(), "sekrit-token");
    // A malformed inbox is dropped, and the start still recorded.
    ok(
        &kb.op,
        &json!({ "op": "session.started", "session": "s4", "source": "startup", "pid": "13",
                 "instance": "h", "inbox": { "socket": "relative", "token": "t" } }),
    );
    assert_eq!(
        kb.db
            .read(|c| jkb_core::claude_session::holders(c, "s4"))
            .unwrap()
            .iter()
            .map(|h| h.has_inbox)
            .collect::<Vec<_>>(),
        vec![false]
    );
    // A request's debug form never shows the token.
    let r: crate::Request = serde_json::from_value(start("s1", "1")).unwrap();
    assert!(!format!("{r:?}").contains("sekrit"));
}

/// A take whose reply never reached the session — the hook's client gave up after the daemon
/// committed it, or the hook died before printing — is handed over again once the hook's lease
/// lapses; without `open`, nothing but the take is computed.
#[test]
fn a_lost_take_is_handed_over_again_and_open_is_listed_only_when_asked() {
    let kb = Kb::new();
    let hook = kb.as_token(&kb.container);
    let (r, _) = requested(ok(&kb.ticket("s1", None, "t1"), &ask(&["land"])));
    let (_, _, open) = take_unacked(&hook, "s1", None, false);
    assert!(open.is_empty(), "not asked for");
    let (_, _, open) = take_unacked(&hook, "s1", None, true);
    assert_eq!(open.len(), 1);
    run_to_end(&kb, &r.id, b"ok\n");
    let (lines, ids, _) = take_unacked(&hook, "s1", None, false);
    assert_eq!((lines.len(), ids), (1, vec![r.id.clone()]));
    // The reply is lost: nothing is acknowledged. Held for the lease, then handed over again.
    assert!(take_unacked(&hook, "s1", None, false).0.is_empty());
    let n = crate::exec::id(&r.id).unwrap();
    kb.db
        .write_txn("t", move |c, _| {
            c.execute(
                "UPDATE exec_delivery SET lease_until = 0 WHERE request_id = ?1",
                [n],
            )?;
            Ok(())
        })
        .unwrap();
    let (lines, _) = take(&hook, "s1", None);
    assert_eq!(lines.len(), 1, "handed over again after the lease");
    assert!(
        take(&hook, "s1", None).0.is_empty(),
        "acknowledged: delivered"
    );
}

/// Session `s1`'s process `pid` registers its inbox in container boot `h#b`, as the managed
/// `SessionStart` hook does with the container's credential.
fn start_inbox(kb: &Kb, pid: &str, token: &str) {
    ok(
        &kb.as_token(&kb.container),
        &json!({ "op": "session.started", "session": "s1", "source": "startup", "pid": pid,
                 "instance": "h#b/pid:[1]",
                 "inbox": { "socket": format!("/tmp/cc-socks/{pid}.sock"), "token": token } }),
    );
}

fn due_ids(b: &LocalBackend, instance: &str) -> Vec<String> {
    match ok(
        b,
        &json!({ "op": "exec.inbox.due", "instance": instance, "max": 10 }),
    ) {
        Response::ExecInboxDue { ids } => ids,
        other => panic!("{other:?}"),
    }
}

/// The forwarder's ops are the harness's, as the hook's take is: a claim hands out an inbox token, so
/// no ticket — not even the session's own — and no other grant may sweep, claim or report.
#[test]
fn the_forwarder_s_ops_are_the_harness_s_alone() {
    let kb = Kb::new();
    let other_grant = match ok(
        &kb.op,
        &json!({ "op": "role.grant", "role": "coordinator", "agent": "someone" }),
    ) {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    let ops = [
        json!({ "op": "exec.inbox.due", "instance": "h#b", "max": 10 }),
        json!({ "op": "exec.inbox.claim", "id": "exec-1", "instance": "h#b", "lease_ms": 30000 }),
        json!({ "op": "exec.inbox.report", "id": "exec-1", "ok": true }),
    ];
    for op in &ops {
        for who in [
            kb.ticket("s1", None, "t1"),
            kb.ticket("s1", Some(("a1", "implementer")), "t2"),
            kb.as_token(&other_grant),
        ] {
            assert_eq!(code(&who, op), ErrorCode::Forbidden, "{op}");
        }
        let r: crate::Request = serde_json::from_value(op.clone()).unwrap();
        assert_eq!(r.permission(), OpPermission::Attest, "{op}");
    }
    assert!(due_ids(&kb.as_token(&kb.container), "h#b").is_empty());
    assert!(due_ids(&kb.op, "h#b").is_empty());
    for bad in ["h", "h/ns", "#b", "h#", "h#b/pid:[1]", "h#b#c", "h#b\n"] {
        assert_eq!(
            code(
                &kb.op,
                &json!({ "op": "exec.inbox.due", "instance": bad, "max": 1 })
            ),
            ErrorCode::Invalid,
            "{bad:?}"
        );
    }
    // A sweep is a read (it only settles expiries); a claim and a report are keyed for resend.
    let due: crate::Request = serde_json::from_value(ops[0].clone()).unwrap();
    assert!(!due.mutates());
    for op in &ops[1..] {
        let r: crate::Request = serde_json::from_value(op.clone()).unwrap();
        assert!(r.mutates(), "{op}");
    }
}

/// The forwarder's round: a due result is claimed with its message and the session's inbox targets in
/// the forwarder's boot, newest first; while the claim holds, nobody else is handed it; reported
/// posted, it is delivered, and neither the hook nor another sweep hands it over again.
#[test]
fn a_forwarder_claims_a_due_result_and_reports_it_posted() {
    let kb = Kb::new();
    let fwd = kb.as_token(&kb.container);
    let (r, _) = requested(ok(&kb.ticket("s1", None, "t1"), &ask(&["land"])));
    start_inbox(&kb, "10", "older-token");
    start_inbox(&kb, "11", "newer-token");
    assert!(due_ids(&fwd, "h#b").is_empty(), "still pending");
    run_to_end(&kb, &r.id, b"warning: 1 task is unplanned\nok\n");
    assert_eq!(due_ids(&fwd, "h#b"), vec![r.id.clone()]);
    assert!(
        due_ids(&fwd, "h#other").is_empty(),
        "another container's boot"
    );

    let claim =
        json!({ "op": "exec.inbox.claim", "id": r.id, "instance": "h#b", "lease_ms": 30000 });
    assert_eq!(
        code(
            &fwd,
            &json!({ "op": "exec.inbox.claim", "id": r.id, "instance": "h#other", "lease_ms": 30000 })
        ),
        ErrorCode::NotFound
    );
    let (text, targets) = match ok(&fwd, &claim) {
        Response::ExecInboxClaimed { text, targets } => (text, targets),
        other => panic!("{other:?}"),
    };
    assert!(
        text.starts_with(&format!("[jkb run request] {} finished exit 0", r.id)),
        "{text}"
    );
    assert!(text.contains("\nwarning: 1 task is unplanned\n"), "{text}");
    let tokens: Vec<&str> = targets.iter().map(|t| t.token.as_str()).collect();
    assert_eq!(tokens, vec!["newer-token", "older-token"]);
    assert_eq!(targets[0].socket, "/tmp/cc-socks/11.sock");
    let debug = format!(
        "{:?}",
        Response::ExecInboxClaimed {
            text: String::new(),
            targets: targets.clone()
        }
    );
    assert!(!debug.contains("newer-token"), "{debug}");

    assert_eq!(code(&fwd, &claim), ErrorCode::NotFound, "claimed already");
    assert!(
        take(&kb.as_token(&kb.container), "s1", None).0.is_empty(),
        "the hook leaves a live claim to its holder"
    );
    match ok(
        &fwd,
        &json!({ "op": "exec.inbox.report", "id": r.id, "ok": true }),
    ) {
        Response::ExecInboxReported { reported } => assert!(reported),
        other => panic!("{other:?}"),
    }
    match ok(
        &fwd,
        &json!({ "op": "exec.inbox.report", "id": r.id, "ok": true }),
    ) {
        Response::ExecInboxReported { reported } => assert!(!reported, "delivered once"),
        other => panic!("{other:?}"),
    }
    assert!(due_ids(&fwd, "h#b").is_empty());
    let n = crate::exec::id(&r.id).unwrap();
    let d = kb
        .db
        .read(move |c| jkb_core::exec_delivery::get(c, n))
        .unwrap()
        .unwrap();
    assert_eq!(d.via, Some(jkb_core::exec_delivery::Via::Socket));
}

/// A post every target refused is reported failed, with why: the row waits for the hook, or for the
/// session to come back with a newer inbox.
#[test]
fn a_failed_post_is_recorded_and_left_to_the_hook() {
    let kb = Kb::new();
    let fwd = kb.as_token(&kb.container);
    let (r, _) = requested(ok(&kb.ticket("s1", None, "t1"), &ask(&["land"])));
    start_inbox(&kb, "10", "tok");
    run_to_end(&kb, &r.id, b"ok\n");
    ok(
        &fwd,
        &json!({ "op": "exec.inbox.claim", "id": r.id, "instance": "h#b", "lease_ms": 30000 }),
    );
    match ok(
        &fwd,
        &json!({ "op": "exec.inbox.report", "id": r.id, "ok": false,
                 "error": "/tmp/cc-socks/10.sock: connection refused" }),
    ) {
        Response::ExecInboxReported { reported } => assert!(reported),
        other => panic!("{other:?}"),
    }
    let n = crate::exec::id(&r.id).unwrap();
    let d = kb
        .db
        .read(move |c| jkb_core::exec_delivery::get(c, n))
        .unwrap()
        .unwrap();
    assert_eq!(d.state, jkb_core::exec_delivery::DeliveryState::Failed);
    assert_eq!(
        d.last_error.as_deref(),
        Some("/tmp/cc-socks/10.sock: connection refused")
    );
    assert!(due_ids(&fwd, "h#b").is_empty(), "no newer inbox");
    assert_eq!(take(&fwd, "s1", None).0.len(), 1, "the hook delivers it");
}

/// The container's credential keeps its forwarder's `inbox/` group on the run-request index — and no
/// other group there, and no group on any other `exec/` topic; a ticket keeps none.
#[test]
fn the_container_keeps_only_its_inbox_groups_on_the_request_index() {
    let kb = Kb::new();
    let container = kb.as_token(&kb.container);
    let topic = jkb_core::exec::INDEX_TOPIC;
    ok(
        &container,
        &json!({ "op": "mq.group_create", "topic": topic, "group": "inbox/h" }),
    );
    let (r, _) = requested(ok(
        &kb.ticket("s1", None, "t1"),
        &ask(&["make", "secret-arg"]),
    ));
    let poll = json!({ "op": "mq.poll", "topic": topic, "group": "inbox/h", "max": 10 });
    let messages = match ok(&container, &poll) {
        Response::Messages { messages } => messages,
        other => panic!("{other:?}"),
    };
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].payload["id"], json!(r.id));
    assert!(!messages[0].payload.to_string().contains("secret-arg"));
    ok(
        &container,
        &json!({ "op": "mq.ack", "topic": topic, "group": "inbox/h", "seq": messages[0].seq }),
    );
    let s1 = kb.ticket("s1", None, "t2");
    for (who, op) in [
        (
            &s1,
            json!({ "op": "mq.group_create", "topic": topic, "group": "inbox/h" }),
        ),
        (
            &s1,
            json!({ "op": "mq.poll", "topic": topic, "group": "inbox/h", "max": 10 }),
        ),
        (
            &container,
            json!({ "op": "mq.group_create", "topic": topic, "group": "inboxes" }),
        ),
        (
            &container,
            json!({ "op": "mq.group_create", "topic": "exec/1", "group": "inbox/h" }),
        ),
        (
            &container,
            json!({ "op": "mq.poll", "topic": "exec/1", "group": "inbox/h", "max": 10 }),
        ),
        (
            &container,
            json!({ "op": "mq.send", "topic": topic, "key": "k", "kind": "exec.changed",
                    "payload": {}, "producer": "p" }),
        ),
    ] {
        let c = code(who, &op);
        assert!(
            matches!(c, ErrorCode::Invalid | ErrorCode::Forbidden),
            "{op}: {c:?}"
        );
    }
    match ok(
        &container,
        &json!({ "op": "mq.group_delete", "topic": topic, "group": "inbox/h" }),
    ) {
        Response::GroupDeleted { deleted } => assert!(deleted),
        other => panic!("{other:?}"),
    }
}
