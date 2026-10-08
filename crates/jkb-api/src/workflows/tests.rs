//! `workflow.agents`/`agent`/`agent_copy`/`agent_set` and `workflow.graph` through the backend: the
//! wire shapes, who may write, and that the graph is the compiled table.

use std::sync::Arc;

use jkb_core::Db;
use serde_json::{json, Value};

use crate::rbac::{Caller, Tickets};
use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};

fn call(b: &LocalBackend, r: Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

#[allow(clippy::needless_pass_by_value)] // every call site builds its request inline
fn ok(b: &LocalBackend, r: Value) -> Response {
    call(b, r.clone()).unwrap_or_else(|e| panic!("{r}: {e:?}"))
}

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

    fn as_role(&self, role: &str) -> LocalBackend {
        let token = match ok(
            &self.op,
            json!({ "op": "role.grant", "role": role, "agent": format!("{role}-agent") }),
        ) {
            Response::Granted { token, .. } => token,
            other => panic!("{other:?}"),
        };
        LocalBackend::new(self.db.clone())
            .with_tickets(Arc::clone(&self.tickets))
            .with_caller(Caller::Token(token))
    }
}

fn agent(r: Response) -> (super::AgentView, Option<String>, bool) {
    match r {
        Response::WorkflowAgent {
            agent,
            rendered,
            wrote,
        } => (*agent, rendered, wrote),
        other => panic!("{other:?}"),
    }
}

#[test]
fn every_role_reads_templates_and_only_the_operator_writes_them() {
    let kb = Kb::new();
    let coordinator = kb.as_role("coordinator");
    let listed = match ok(&coordinator, json!({ "op": "workflow.agents" })) {
        Response::WorkflowAgents { agents } => agents,
        other => panic!("{other:?}"),
    };
    let imp = listed
        .iter()
        .find(|a| a.name == "swarm-implementer")
        .unwrap();
    assert_eq!(imp.source, jkb_core::workflow::agents::Source::Packaged);
    assert_eq!(imp.role, "implementer");
    assert!(imp.role_ops.contains(&"task_claim".to_owned()));
    assert!(!imp.role_ops.contains(&"admin".to_owned()));
    assert!(imp.placeholders.contains(&"task_list".to_owned()));

    for r in [
        json!({ "op": "workflow.agent_copy", "from": "swarm-implementer" }),
        json!({ "op": "workflow.agent_set", "name": "swarm-implementer",
                "edit": { "describe": "mine" } }),
    ] {
        let e = call(&coordinator, r).unwrap_err();
        assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
    }

    let (copied, _, wrote) = agent(ok(
        &kb.op,
        json!({ "op": "workflow.agent_copy", "from": "swarm-implementer" }),
    ));
    assert!(wrote);
    assert!(copied.overrides_packaged);
    assert_eq!(copied.version, 1);
    let (edited, _, wrote) = agent(ok(
        &kb.op,
        json!({ "op": "workflow.agent_set", "name": "swarm-implementer",
                "edit": { "template": "Build {{what}} in {{repo}}.", "role": "implementer",
                          "permissions": { "isolation": "worktree", "model": null, "writes": "code" } } }),
    ));
    assert!(wrote);
    assert_eq!(edited.version, 2);
    assert_eq!(edited.placeholders, vec!["repo", "what"]);

    // The same edit again writes nothing.
    let (_, _, wrote) = agent(ok(
        &kb.op,
        json!({ "op": "workflow.agent_set", "name": "swarm-implementer",
                "edit": { "template": "Build {{what}} in {{repo}}." } }),
    ));
    assert!(!wrote);

    // A script reads it by name, filled in.
    let (shown, rendered, _) = agent(ok(
        &coordinator,
        json!({ "op": "workflow.agent", "name": "swarm-implementer",
                "vars": { "what": "it", "repo": "/r" } }),
    ));
    assert_eq!(shown.version, 2);
    assert_eq!(rendered.as_deref(), Some("Build it in /r."));
    // ... and the packaged text is still there.
    let (pkg, _, _) = agent(ok(
        &coordinator,
        json!({ "op": "workflow.agent", "name": "swarm-implementer", "packaged": true }),
    ));
    assert!(pkg.template.starts_with("You are the IMPLEMENTER"));
}

#[test]
fn bad_asks_are_named_refusals() {
    let kb = Kb::new();
    for (r, needle) in [
        (
            json!({ "op": "workflow.agent", "name": "swarm-status", "vars": { "status": "x" } }),
            "no value for",
        ),
        (
            json!({ "op": "workflow.agent", "name": "swarm-status", "packaged": true, "version": 1 }),
            "not both",
        ),
        (
            json!({ "op": "workflow.agent", "name": "nope" }),
            "no agent template",
        ),
        (
            json!({ "op": "workflow.agent_set", "name": "swarm-status", "edit": { "describe": "d" } }),
            "read-only",
        ),
        (
            json!({ "op": "workflow.graph", "uid": "task:x", "strategy": "autonomous" }),
            "not both",
        ),
    ] {
        let e = call(&kb.op, r.clone()).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{r}: {e:?}");
        assert!(e.message.contains(needle), "{r}: {}", e.message);
    }
    // An edit carrying a field the API does not know is refused at parse, not ignored.
    assert!(serde_json::from_value::<crate::Request>(json!({
        "op": "workflow.agent_set", "name": "x", "edit": { "colour": "red" }
    }))
    .is_err());
}

#[test]
fn the_graph_is_the_compiled_table_with_the_strategys_permissions() {
    let kb = Kb::new();
    let graph = |r: Value| match ok(&kb.op, r) {
        Response::WorkflowGraph { graph } => *graph,
        other => panic!("{other:?}"),
    };
    let g = graph(json!({ "op": "workflow.graph" }));
    assert_eq!(g.strategy, "design-reviewed", "the default");
    assert_eq!(g.graph, "reviewed-design");
    let table = jkb_core::workflow::GraphId::ReviewedDesign
        .machine()
        .table();
    assert_eq!(g.workflow.transitions.len(), table.transitions.len());
    assert_eq!(g.workflow.states.len(), table.states.len());
    let design = &g.workflow.states[0];
    assert!(design.initial);
    assert_eq!(design.next_role.as_deref(), Some("designer"));
    let edge = |g: &super::GraphView, from: &str, event: &str| {
        g.workflow
            .transitions
            .iter()
            .find(|t| t.from == from && t.event == event)
            .unwrap()
            .clone()
    };
    // Who may approve a design is the strategy's toggle.
    assert_eq!(
        edge(&g, "design_review", "approve_design").roles,
        vec!["operator"]
    );
    let coordinated = graph(json!({ "op": "workflow.graph", "strategy": "coordinated" }));
    assert_eq!(
        edge(&coordinated, "design_review", "approve_design").roles,
        vec!["operator", "coordinator"]
    );
    // An observation is anyone's; an override names its own destination.
    let passed = edge(&g, "review", "review_passed");
    assert!(passed.reconciled && passed.guarded && passed.roles.is_empty());
    assert_eq!(edge(&g, "design", "override").to, None);
    // The direct graph skips design review.
    let direct = graph(json!({ "op": "workflow.graph", "strategy": "autonomous" }));
    assert_eq!(
        edge(&direct, "design", "submit_design").to.as_deref(),
        Some("implement")
    );
    // The lifecycle machine comes with it.
    assert_eq!(
        g.lifecycle.states.len(),
        jkb_core::lifecycle::machine().table().states.len()
    );
    assert!(g.lifecycle.states.iter().any(|s| s.name == "in_progress"));

    // A task's own: its phase and status.
    kb.db
        .write_txn("test", |c, m| {
            jkb_core::task::create(c, m, &jkb_core::task::NewTask::new("task:w", "W"))
        })
        .unwrap();
    let t = graph(json!({ "op": "workflow.graph", "uid": "task:w" }));
    assert_eq!(t.task.as_deref(), Some("task:w"));
    assert_eq!(t.phase.as_deref(), Some("design"));
    assert_eq!(t.status.as_deref(), Some("open"));
    let e = call(
        &kb.op,
        json!({ "op": "workflow.graph", "uid": "task:none" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}
