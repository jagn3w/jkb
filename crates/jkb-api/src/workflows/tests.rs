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
    let (listed, roles, sets) = match ok(&coordinator, json!({ "op": "workflow.agents" })) {
        Response::WorkflowAgents {
            agents,
            roles,
            agent_sets,
        } => (agents, roles, agent_sets),
        other => panic!("{other:?}"),
    };
    // The agent sets come with their sentences, in the packaged file's order, and every template
    // belongs to one of them.
    let packaged = jkb_core::workflow::agents::agent_sets().unwrap();
    assert_eq!(sets.len(), packaged.len());
    for (s, p) in sets.iter().zip(packaged) {
        assert_eq!((&s.name, &s.describe), (&p.name, &p.describe));
    }
    assert!(listed
        .iter()
        .all(|a| sets.iter().any(|s| s.name == a.workflow)));
    let wire = serde_json::to_value(&sets[0]).unwrap();
    assert_eq!(
        wire,
        json!({ "name": "task-swarm", "describe": packaged[0].describe })
    );
    // The roles a template may take are this jkb's, so a client never keeps its own list.
    assert_eq!(
        roles,
        [
            "operator",
            "coordinator",
            "designer",
            "implementer",
            "reviewer",
            "systemic_reviewer"
        ]
    );
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
            json!({ "op": "workflow.agent", "name": "swarm-reviewer", "vars": { "branch": "x" } }),
            "no value for",
        ),
        (
            json!({ "op": "workflow.agent", "name": "swarm-reviewer", "packaged": true, "version": 1 }),
            "not both",
        ),
        (
            json!({ "op": "workflow.agent", "name": "nope" }),
            "no agent template",
        ),
        (
            json!({ "op": "workflow.agent_set", "name": "swarm-reviewer", "edit": { "describe": "d" } }),
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

/// The `autonomous` preset's machines: its `lands` toggle lets the coordinator land.
fn direct_lands() -> super::GraphView {
    let kb = Kb::new();
    match ok(
        &kb.op,
        json!({ "op": "workflow.graph", "strategy": "autonomous" }),
    ) {
        Response::WorkflowGraph { graph } => *graph,
        other => panic!("{other:?}"),
    }
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
    assert_eq!(g.graph, "design-gate");
    // The graph's sentence is `describe`; its phase path moved to `path`, both on the wire.
    let gate = jkb_core::workflow::GraphId::DesignGate;
    assert_eq!(g.describe, gate.describe());
    assert_eq!(g.path, gate.path());
    let wire = serde_json::to_value(&g).unwrap();
    assert_eq!(wire["describe"], gate.describe());
    assert_eq!(wire["path"], gate.path());
    let table = jkb_core::workflow::GraphId::DesignGate.machine().table();
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
    // Who fires each row is one answer every surface prints.
    assert_eq!(passed.fired_by, "observed");
    assert_eq!(
        edge(&coordinated, "design_review", "approve_design").fired_by,
        "operator, coordinator"
    );
    assert!(g
        .lifecycle
        .transitions
        .iter()
        .any(|t| t.fired_by == "applied"));
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

    // A task's own: its phase, its status, and the strategy pinned on it — not the default.
    kb.db
        .write_txn("test", |c, m| {
            jkb_core::task::create(c, m, &jkb_core::task::NewTask::new("task:w", "W"))
        })
        .unwrap();
    ok(
        &kb.op,
        json!({ "op": "workflow.set", "uid": "task:w", "strategy": "coordinated" }),
    );
    let t = graph(json!({ "op": "workflow.graph", "uid": "task:w" }));
    assert_eq!(t.task.as_deref(), Some("task:w"));
    assert_eq!(t.phase.as_deref(), Some("design"));
    assert_eq!(t.status.as_deref(), Some("open"));
    assert_eq!(t.strategy, "coordinated");
    assert_eq!(t.graph, "design-gate");
    assert_eq!(
        edge(&t, "design_review", "approve_design").roles,
        vec!["operator", "coordinator"]
    );
    let e = call(
        &kb.op,
        json!({ "op": "workflow.graph", "uid": "task:none" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

#[test]
fn the_graph_says_what_its_strategy_changes_from_the_default() {
    let kb = Kb::new();
    let graph = |r: Value| match ok(&kb.op, r) {
        Response::WorkflowGraph { graph } => *graph,
        other => panic!("{other:?}"),
    };
    let autonomous = graph(json!({ "op": "workflow.graph", "strategy": "autonomous" }));
    assert_eq!(autonomous.default, "design-reviewed");
    assert_eq!(
        autonomous.changes_from_default,
        vec![
            "graph: direct (default: design-gate)",
            "lands: operator, coordinator (default: operator)",
        ]
    );
    let wire = serde_json::to_value(&autonomous).unwrap();
    assert_eq!(wire["default"], "design-reviewed");
    assert_eq!(
        wire["changes_from_default"][0],
        "graph: direct (default: design-gate)"
    );
    let default = graph(json!({ "op": "workflow.graph" }));
    assert_eq!(default.changes_from_default, Vec::<String>::new());

    // The operator's `default` definition is what the diff is taken against.
    ok(
        &kb.op,
        json!({ "op": "workflow.define", "name": "default", "describe": "The coordinator approves.",
                "spec": { "graph": "design-gate",
                          "toggles": { "approves_design": ["operator", "coordinator"] } } }),
    );
    let reviewed = graph(json!({ "op": "workflow.graph", "strategy": "design-reviewed" }));
    assert_eq!(reviewed.default, "default@1");
    assert_eq!(
        reviewed.changes_from_default,
        vec!["approves designs: operator (default: operator, coordinator)"]
    );
    let default = graph(json!({ "op": "workflow.graph" }));
    assert_eq!(default.strategy, "default@1");
    assert_eq!(default.changes_from_default, Vec::<String>::new());
}

/// A stored `default` this jkb cannot read (a newer jkb's spec) leaves the diff unknown for a named
/// strategy and a pinned task, whose machines do not need it; the default's own graph is refused.
#[test]
fn an_unreadable_default_leaves_the_diff_unknown_but_draws_named_and_pinned_graphs() {
    let kb = Kb::new();
    kb.db
        .write_txn("test", |c, m| {
            jkb_core::task::create(c, m, &jkb_core::task::NewTask::new("task:p", "P"))
        })
        .unwrap();
    ok(
        &kb.op,
        json!({ "op": "workflow.set", "uid": "task:p", "strategy": "autonomous" }),
    );
    kb.db
        .write_txn("test", |c, _| {
            c.execute(
                "INSERT INTO workflow_strategies (name, version, spec, defined_at, describe)
                 VALUES ('default', 1, ?1, '2026-10-31T00:00:00Z', 'From a newer jkb.')",
                [r#"{"graph":"design-gate","a_field_from_a_newer_jkb":true}"#],
            )?;
            Ok(())
        })
        .unwrap();
    let graph = |r: Value| match ok(&kb.op, r) {
        Response::WorkflowGraph { graph } => *graph,
        other => panic!("{other:?}"),
    };
    let named = graph(json!({ "op": "workflow.graph", "strategy": "autonomous" }));
    assert_eq!(named.graph, "direct");
    assert_eq!(
        (named.default.as_str(), named.changes_from_default.len()),
        ("", 0)
    );
    let pinned = graph(json!({ "op": "workflow.graph", "uid": "task:p" }));
    assert_eq!(pinned.strategy, "autonomous");
    assert_eq!(
        (pinned.default.as_str(), pinned.changes_from_default.len()),
        ("", 0)
    );
    // The timeline draws a task's graph through the same read.
    ok(&kb.op, json!({ "op": "timeline.task", "uid": "task:p" }));
    let e = call(&kb.op, json!({ "op": "workflow.graph" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
}

/// The variable that makes [`graph_fixture_matches_presets`] rewrite the fixture instead of failing.
const WRITE_FIXTURE: &str = "JKB_WRITE_GRAPH_FIXTURE";

/// The app's layout tests and its Lifecycle smoke draw every preset's machines from
/// `ui/app/test/fixtures/workflow-graphs.json`, which must be what `workflow.graph` answers today:
/// a fixture that drifted from the tables would pin the layout of a machine jkb no longer has.
#[test]
fn graph_fixture_matches_presets() {
    let db = Db::open_in_memory().unwrap();
    let presets: Vec<&'static str> = jkb_core::workflow::strategy::PRESETS
        .iter()
        .map(|p| p.name)
        .collect();
    let answered: serde_json::Map<String, Value> = db
        .read_with(move |c| {
            presets
                .into_iter()
                .map(|name| {
                    let g = super::graph(c, None, Some(name))?;
                    Ok((name.to_owned(), serde_json::to_value(g).unwrap()))
                })
                .collect::<Result<_, ApiError>>()
        })
        .unwrap();
    let answered = Value::Object(answered);
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ui/app/test/fixtures/workflow-graphs.json");
    if std::env::var_os(WRITE_FIXTURE).is_some() {
        let text = serde_json::to_string_pretty(&answered).unwrap() + "\n";
        std::fs::write(&file, text).unwrap();
        return;
    }
    let held: Value = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    assert!(
        held == answered,
        "{} is not what workflow.graph answers for every preset; rewrite it with\n  \
         {WRITE_FIXTURE}=1 ./scripts/test.sh -p jkb-api graph_fixture_matches_presets",
        file.display()
    );
}

#[test]
fn a_copy_with_an_edit_is_one_op_and_a_refused_edit_leaves_no_copy() {
    let kb = Kb::new();
    let e = call(
        &kb.op,
        json!({ "op": "workflow.agent_copy", "from": "swarm-implementer",
                "edit": { "template": "{{ repo }}" } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    let (still, _, _) = agent(ok(
        &kb.op,
        json!({ "op": "workflow.agent", "name": "swarm-implementer" }),
    ));
    assert_eq!(
        still.source,
        jkb_core::workflow::agents::Source::Packaged,
        "nothing overrides it"
    );

    let (made, _, wrote) = agent(ok(
        &kb.op,
        json!({ "op": "workflow.agent_copy", "from": "swarm-implementer",
                "edit": { "template": "Build {{what}}.", "role": "reviewer" } }),
    ));
    assert!(wrote);
    assert_eq!(made.version, 1);
    assert_eq!(made.template, "Build {{what}}.");
    assert_eq!(made.role, "reviewer");
    assert!(!made.matches_packaged);
    assert_eq!(made.packaged_base, made.packaged_version);

    // Reverting makes it the packaged text again: nothing to contribute.
    let (back, _, _) = agent(ok(
        &kb.op,
        json!({ "op": "workflow.agent_copy", "from": "swarm-implementer", "packaged": true }),
    ));
    assert!(back.matches_packaged);
}

#[test]
fn the_lifecycle_names_who_lands_and_who_overrides() {
    let kb = Kb::new();
    let g = match ok(&kb.op, json!({ "op": "workflow.graph" })) {
        Response::WorkflowGraph { graph } => *graph,
        other => panic!("{other:?}"),
    };
    // The lifecycle's land is the strategy's landers, its override whoever may state a status;
    // every other act jkb applies inside the op that moves it.
    let lands = |g: &super::GraphView| -> Vec<String> {
        let mut by: Vec<String> = g
            .lifecycle
            .transitions
            .iter()
            .filter(|t| t.event == "land")
            .map(|t| t.fired_by.clone())
            .collect();
        by.dedup();
        by
    };
    assert_eq!(lands(&g), vec!["operator"]);
    assert_eq!(lands(&direct_lands()), vec!["operator, coordinator"]);
    for t in &g.lifecycle.transitions {
        let want = match (t.reconciled, t.event.as_str()) {
            (true, _) => "observed",
            (false, "land") => "operator",
            (false, "override") => "operator, coordinator",
            _ => "applied",
        };
        assert_eq!(t.fired_by, want, "{} --{}-->", t.from, t.event);
    }
}

#[test]
fn each_row_says_what_its_guard_checks_and_how_it_fires() {
    let kb = Kb::new();
    let g = match ok(&kb.op, json!({ "op": "workflow.graph" })) {
        Response::WorkflowGraph { graph } => *graph,
        other => panic!("{other:?}"),
    };
    // A guard is described exactly where a row has one, on both machines.
    for t in g
        .workflow
        .transitions
        .iter()
        .chain(&g.lifecycle.transitions)
    {
        assert_eq!(
            t.guard.is_some(),
            t.guarded,
            "{} --{}--> {:?}",
            t.from,
            t.event,
            t.guard
        );
    }
    let edge = |from: &str, event: &str| {
        g.workflow
            .transitions
            .iter()
            .find(|t| t.from == from && t.event == event)
            .unwrap()
            .clone()
    };
    // The review guards read the strategy's `repeated_area` (two rounds, one file, by default).
    let repeated = edge("review", "review_repeated");
    assert!(
        repeated
            .guard
            .as_deref()
            .unwrap()
            .contains("the newest 2 rounds all flagged must-fixes in the same file"),
        "{repeated:?}"
    );
    // The same event is unguarded where its row is: reopening a task already at work.
    assert_eq!(edge("implement", "reopen").guard, None);
    assert!(edge("landed", "reopen").guard.is_some());
    // An act is fired by name, an observation by observing.
    assert_eq!(
        edge("implement", "submit_work").command.as_deref(),
        Some("jkb workflow fire <uid> submit_work")
    );
    assert_eq!(
        edge("review", "review_passed").command.as_deref(),
        Some("jkb workflow observe <uid>")
    );
    assert!(g.workflow.transitions.iter().all(|t| t.command.is_some()));
    assert!(g.lifecycle.transitions.iter().all(|t| t.command.is_none()));
    // Every role's op classes, the same answer an agent template of that role carries.
    let (agents, roles) = match ok(&kb.op, json!({ "op": "workflow.agents" })) {
        Response::WorkflowAgents { agents, roles, .. } => (agents, roles),
        other => panic!("{other:?}"),
    };
    assert_eq!(g.role_ops.keys().cloned().collect::<Vec<_>>(), {
        let mut r = roles;
        r.sort();
        r
    });
    // Under the default strategy only the operator lands, so every role's classes are the table's:
    // the same set an agent template of that role carries.
    for a in &agents {
        let mut graph = g.role_ops[&a.role].clone();
        let mut agent = a.role_ops.clone();
        graph.sort();
        agent.sort();
        assert_eq!(graph, agent, "{}", a.name);
    }
    assert!(g.role_ops["operator"].iter().any(|o| o == "land"));
}

#[test]
fn a_role_may_land_where_the_strategy_lets_it() {
    let kb = Kb::new();
    let graph = |strategy: &str| match ok(
        &kb.op,
        json!({ "op": "workflow.graph", "strategy": strategy }),
    ) {
        Response::WorkflowGraph { graph } => *graph,
        other => panic!("{other:?}"),
    };
    let lands = |g: &super::GraphView, role: &str| g.role_ops[role].iter().any(|o| o == "land");
    // `autonomous` lets the coordinator land: its `landable` actor may run what its step says.
    let autonomous = graph("autonomous");
    assert!(lands(&autonomous, "coordinator"));
    let landable = autonomous
        .workflow
        .states
        .iter()
        .find(|s| s.name == "landable")
        .unwrap();
    assert_eq!(landable.next_role.as_deref(), Some("coordinator"));
    // ...and it is the same answer the lifecycle's `land` row gives.
    assert!(autonomous
        .lifecycle
        .transitions
        .iter()
        .any(|t| t.event == "land" && t.roles.iter().any(|r| r == "coordinator")));
    // `design-reviewed` does not.
    let reviewed = graph("design-reviewed");
    assert!(!lands(&reviewed, "coordinator"));
    assert!(lands(&reviewed, "operator"));
    assert!(!lands(&reviewed, "implementer"));
}

/// One swarm run's principals on one database: the main session (the container credential's ceiling,
/// the coordinator), and attested subagents of exactly the types `scripts/setup.sh` maps, read from
/// the script ([`setup_role_maps`]), so a worker the script leaves unmapped holds no role here
/// either. That every packaged subagent type is among them is pinned by
/// `setup_maps_every_subagent_type_to_its_templates_role` in `jkb-core`.
struct Swarm {
    kb: Kb,
    hook: LocalBackend,
    tool: std::cell::Cell<u32>,
    /// The run's designer budget (`--designers`).
    budget: std::cell::Cell<usize>,
    lead: String,
}

/// The path `swarm-coordinator` builds a group on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    /// The full workflow: `submit_work`, recorded rounds, `observe`, routing on the phase.
    Workflow,
    /// A task still at `design` that the run's design gate let through: routed on the verdict.
    Legacy,
    /// A task at `design` the gate holds back: a designer writes its design, and builds nothing.
    Design,
    /// `systemic_review`, left by an earlier run: a group of one, straight to a systemic reviewer.
    Systemic,
}

/// The run's design gate, as `scope_block` tells the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// Scope mode: a task at `design` must carry `design=approved`.
    On,
    /// `--no-design-gate`.
    Off,
    /// Uid mode: a deliberate hand-pick.
    HandPicked,
}

/// `swarm-coordinator` step 1's reads, in order: the phases each asks for, and its tag terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    /// A, build: every phase past design that a run drives.
    Build,
    /// R, re-design: at `design`, given a designer by this run — a design the operator rejected.
    /// At any budget: a re-design uses none.
    Redesign,
    /// B, legacy: at `design`, tagged approved, not this run's design.
    Legacy,
    /// C, design: at `design`, untagged, not this run's design — limited to the designers left.
    Design,
}

impl Read {
    const ALL: [Self; 4] = [Self::Build, Self::Redesign, Self::Legacy, Self::Design];

    /// The read's own terms, exactly as the template spells them (`{{owner}}` filled).
    fn terms(self) -> String {
        match self {
            Self::Build => "phase:implement,review,landable,systemic_review".to_owned(),
            Self::Redesign => format!("phase:design tag:swarm-designed={OWNER}"),
            Self::Legacy => format!("phase:design tag:design=approved -tag:swarm-designed={OWNER}"),
            Self::Design => {
                format!("phase:design -tag:design=approved -tag:swarm-designed={OWNER}")
            }
        }
    }
}

/// `/task-swarm`'s default designer budget.
const DEFAULT_DESIGNERS: usize = 3;

/// The terms every read adds: nothing this run settled, and no review finding.
fn every_read() -> String {
    format!("-tag:swarm-settled={OWNER} -tag:finding=review")
}

/// What step 1b reads of a task the frontier offered: its phase and its last history line (`jkb
/// workflow show`).
struct Seen {
    phase: String,
    last: String,
}

/// `swarm-coordinator` step 1b's routing table, for a task `read` offered. Pinned against the
/// template's text by `jkb-core`'s agents tests, and against what jkb lets step 3 do by
/// `step_one_admits_exactly_the_phases_step_three_can_drive`.
fn route(task: &Seen, read: Read, gate: Gate) -> Path {
    // A design turned down goes to a designer whatever the gate, the tag or a hand-pick says.
    let turned_down = ["reject_design", "systemic_redesign"]
        .iter()
        .any(|e| task.last.split_whitespace().nth(1) == Some(e));
    match (read, task.phase.as_str()) {
        (Read::Build, "systemic_review") => Path::Systemic,
        (Read::Build, _) => Path::Workflow,
        // Every task of read R had its design turned down this run.
        (Read::Redesign, _) => Path::Design,
        (Read::Legacy | Read::Design, _) if turned_down => Path::Design,
        // With the gate on, read C's tasks are the ones it holds back.
        (Read::Design, _) if gate == Gate::On => Path::Design,
        (Read::Legacy | Read::Design, _) => Path::Legacy,
    }
}

/// Every `role map <type> <role>` line of `scripts/setup.sh`, as `(type, role)`: the agent types
/// an installed jkb attests, and the roles they attest as.
fn setup_role_maps() -> Vec<(String, String)> {
    let setup = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/setup.sh"),
    )
    .unwrap();
    let maps: Vec<(String, String)> = setup
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            let mut words = l.split(" role map ").nth(1)?.split_whitespace();
            let (agent_type, role) = (words.next()?, words.next()?);
            (!agent_type.starts_with('"') && !role.starts_with('-'))
                .then(|| (agent_type.to_owned(), role.to_owned()))
        })
        .collect();
    assert!(
        maps.iter().any(|(t, _)| t == "jkb-implementer"),
        "no `role map` line read from scripts/setup.sh: {maps:?}"
    );
    maps
}

const OWNER: &str = "box:1";
const BRANCH: &str = "swarm-task/lead";
const REPO: &str = "r";

impl Swarm {
    /// A task whose workflow is at `implement`: the operator approved its design.
    fn new() -> Self {
        let s = Self::at_design();
        for event in ["submit_design", "approve_design"] {
            s.operator_fires(event, None);
        }
        s
    }

    /// The operator moves the lead's workflow; `to` is an override's phase.
    fn operator_fires(&self, event: &str, to: Option<&str>) {
        let m = Self::moved(ok(
            &self.kb.op,
            json!({ "op": "workflow.fire", "uid": self.lead, "event": event, "to": to,
                    "reason": to.map(|_| "the test puts it there") }),
        ));
        assert!(m.moved, "{event}: {m:?}");
    }

    /// The operator fires `event` on the lead with a written reason.
    fn operator_fires_with(&self, event: &str, reason: &str) {
        let m = Self::moved(ok(
            &self.kb.op,
            json!({ "op": "workflow.fire", "uid": self.lead, "event": event, "reason": reason }),
        ));
        assert!(m.moved, "{event}: {m:?}");
    }

    /// A task with no workflow history, so at `design` — where nearly every task sits, since
    /// `/design-pass` sets only the `design=approved` tag.
    fn at_design() -> Self {
        let kb = Kb::new();
        let lead = match ok(&kb.op, json!({ "op": "task.add", "text": "the lead" })) {
            Response::Added { added } => added.uid,
            other => panic!("{other:?}"),
        };
        for (agent_type, role) in setup_role_maps() {
            ok(
                &kb.op,
                json!({ "op": "role.map", "agent_type": agent_type, "role": role }),
            );
        }
        let container = match ok(&kb.op, json!({ "op": "role.rotate_container" })) {
            Response::Granted { token, .. } => token,
            other => panic!("{other:?}"),
        };
        let hook = LocalBackend::new(kb.db.clone())
            .with_tickets(Arc::clone(&kb.tickets))
            .with_caller(Caller::Token(container));
        Self {
            kb,
            hook,
            tool: std::cell::Cell::new(0),
            budget: std::cell::Cell::new(DEFAULT_DESIGNERS),
            lead,
        }
    }

    /// The backend one tool call runs as: a fresh ticket the hook mints, as the harness does per call.
    /// `None` is the main session — the coordinator.
    fn as_agent(&self, agent: Option<(&str, &str)>) -> LocalBackend {
        self.tool.set(self.tool.get() + 1);
        let token = match ok(
            &self.hook,
            json!({ "op": "attest.mint", "session": "swarm",
                    "agent_id": agent.map(|a| a.0), "agent_type": agent.map(|a| a.1),
                    "tool_use_id": format!("tool-{}", self.tool.get()) }),
        ) {
            Response::Ticket { token } => token,
            other => panic!("{other:?}"),
        };
        LocalBackend::new(self.kb.db.clone())
            .with_tickets(Arc::clone(&self.kb.tickets))
            .with_caller(Caller::Token(token))
    }

    fn coordinator(&self, r: Value) -> Response {
        ok(&self.as_agent(None), r)
    }

    fn moved(r: Response) -> crate::rbac::WorkflowMove {
        match r {
            Response::WorkflowMoved { outcome } => outcome,
            other => panic!("{other:?}"),
        }
    }

    /// The lead's phase, as `jkb workflow show` prints it to the coordinator.
    fn phase(&self) -> String {
        match self.coordinator(json!({ "op": "workflow.show", "uid": self.lead })) {
            Response::Workflow { workflow } => workflow.phase,
            other => panic!("{other:?}"),
        }
    }

    fn start_group(&self) {
        self.start_group_on(Path::Workflow);
    }

    /// What step 1b reads of `uid`, through the coordinator's own call.
    fn seen(&self, uid: &str) -> Seen {
        match self.coordinator(json!({ "op": "workflow.show", "uid": uid })) {
            Response::Workflow { workflow } => Seen {
                phase: workflow.phase,
                last: workflow.history.last().cloned().unwrap_or_default(),
            },
            other => panic!("{other:?}"),
        }
    }

    /// The uids step 1's `read` returns, in the frontier's order, as the coordinator makes it.
    fn read(&self, read: Read, limit: usize) -> Vec<String> {
        let r = self.coordinator(json!({ "op": "task.ready",
            "dsl": format!("{} {}", read.terms(), every_read()), "limit": limit }));
        let items = match r {
            Response::Items { items, .. } => items,
            other => panic!("{other:?}"),
        };
        items.into_iter().map(|i| i.uid).collect()
    }

    /// DESIGNERS LEFT: the budget less the tasks this run tagged as given a designer.
    fn designers_left(&self) -> usize {
        self.budget.get().saturating_sub(self.designed())
    }

    /// The limit step 1 reads `read` at, or `None` when it is not read: read C at DESIGNERS LEFT,
    /// and not at all at 0, with the gate on; everything else at 100.
    fn limit(&self, read: Read, gate: Gate) -> Option<usize> {
        match (read, gate) {
            (Read::Design, Gate::On) => Some(self.designers_left()).filter(|&n| n > 0),
            _ => Some(100),
        }
    }

    /// The first of step 1's reads that offers `uid` on a gated run, at the limits it reads them at.
    fn offered_by(&self, uid: &str) -> Option<Read> {
        self.offered_by_under(uid, Gate::On)
    }

    fn offered_by_under(&self, uid: &str, gate: Gate) -> Option<Read> {
        Read::ALL.into_iter().find(|&r| {
            self.limit(r, gate)
                .is_some_and(|n| self.read(r, n).iter().any(|u| u == uid))
        })
    }

    /// Step 1 and 1b for `uid`: the read that offers it, routed. `None`: no read offers it.
    fn admission(&self, uid: &str, gate: Gate) -> Option<Path> {
        let read = self.offered_by_under(uid, gate)?;
        Some(route(&self.seen(uid), read, gate))
    }

    /// Whether any of the run's reads offers the lead.
    fn in_frontier(&self) -> bool {
        self.offered_by(&self.lead).is_some()
    }

    /// `swarm-coordinator` step 1 admits the lead on `path` in a scope-mode run (the gate on), and
    /// step 2 claims it (which starts it), tags its repository and records it as the group's lead.
    fn start_group_on(&self, path: Path) {
        assert_eq!(self.admission(&self.lead, Gate::On), Some(path));
        self.coordinator(json!({ "op": "task.claim", "uid": self.lead, "owner": OWNER }));
        self.coordinator(json!({ "op": "task.tag", "uid": self.lead,
                                 "facet_value": format!("repo={REPO}"), "mode": "set" }));
        self.coordinator(json!({ "op": "task.tag", "uid": self.lead,
                                 "facet_value": format!("swarm-lead={}", self.lead), "mode": "set" }));
    }

    /// `swarm-designer`'s calls, as a `jkb-designer` subagent `agent`: bind, read, write the design
    /// into the task, submit it.
    fn designer_designs(&self, agent: &str, design: &str) {
        let des = || self.as_agent(Some((agent, "jkb-designer")));
        ok(&des(), json!({ "op": "role.bind", "uid": self.lead }));
        match ok(&des(), json!({ "op": "task.show", "uid": self.lead })) {
            Response::Task { .. } => {}
            other => panic!("{other:?}"),
        }
        ok(
            &des(),
            json!({ "op": "task.edit", "uid": self.lead, "text": design, "append": true }),
        );
        let m = Self::moved(ok(
            &des(),
            json!({ "op": "workflow.fire", "uid": self.lead, "event": "submit_design" }),
        ));
        assert!(m.moved && m.phase == "design_review", "{m:?}");
    }

    /// `swarm-coordinator`'s DESIGNING A TASK around the designer: claim, record the budget's count,
    /// then reopen and release on its report. Nothing tags the submitted design: no read offers
    /// `design_review`.
    fn coordinator_designs(&self, agent: &str, design: &str) {
        self.coordinator(json!({ "op": "task.claim", "uid": self.lead, "owner": OWNER }));
        self.coordinator(json!({ "op": "task.tag", "uid": self.lead,
                                 "facet_value": format!("swarm-designed={OWNER}"), "mode": "set" }));
        self.designer_designs(agent, design);
        self.coordinator(json!({ "op": "task.set", "uid": self.lead, "status": "open" }));
        self.coordinator(json!({ "op": "task.release", "uid": self.lead, "owner": OWNER }));
    }

    /// The lead's body.
    fn body(&self) -> String {
        match ok(&self.kb.op, json!({ "op": "task.show", "uid": self.lead })) {
            Response::Task { task, .. } => task.item.content.unwrap_or_default(),
            other => panic!("{other:?}"),
        }
    }

    /// The implementer's calls: it reads its task and design, and moves no workflow itself.
    fn implementer_works(&self) {
        let imp = self.as_agent(Some(("i1", "jkb-implementer")));
        match ok(&imp, json!({ "op": "task.show", "uid": self.lead })) {
            Response::Task { .. } => {}
            other => panic!("{other:?}"),
        }
    }

    fn fire(&self, event: &str) -> crate::rbac::WorkflowMove {
        let m = Self::moved(
            self.coordinator(json!({ "op": "workflow.fire", "uid": self.lead, "event": event })),
        );
        assert!(m.moved, "{event}: {m:?}");
        m
    }

    /// `swarm-coordinator` step 3, on `RESULT: ready`: record the branch, `needs_review`, and hand
    /// the lead to review by its phase — `submit_work` from `implement`, nothing from `review`,
    /// `rework` first from `landable`.
    fn coordinator_submits(&self) {
        self.coordinator(json!({ "op": "task.start", "uid": self.lead, "keep": OWNER,
                                 "place": { "branch": BRANCH, "repo": REPO, "onto": "fleet/main" } }));
        self.coordinator(json!({ "op": "task.set", "uid": self.lead, "status": "needs_review" }));
        match self.phase().as_str() {
            "implement" => {}
            // Already submitted: fire nothing.
            "review" => return,
            "landable" => assert_eq!(self.fire("rework").phase, "implement"),
            other => panic!("the template stalls the group at {other}"),
        }
        assert_eq!(self.fire("submit_work").phase, "review");
    }

    /// `swarm-coordinator` step 3 for a LEGACY group: record the branch and `needs_review`, and fire
    /// nothing.
    fn coordinator_submits_legacy(&self) {
        self.coordinator(json!({ "op": "task.start", "uid": self.lead, "keep": OWNER,
                                 "place": { "branch": BRANCH, "repo": REPO, "onto": "fleet/main" } }));
        self.coordinator(json!({ "op": "task.set", "uid": self.lead, "status": "needs_review" }));
    }

    /// CLOSING FINDINGS: after a clean round the coordinator closes the earlier rounds' must-fixes.
    fn coordinator_closes(&self, findings: &[String]) {
        assert!(!findings.is_empty());
        for uid in findings {
            self.coordinator(json!({ "op": "task.set", "uid": uid, "status": "done" }));
            match ok(&self.kb.op, json!({ "op": "task.show", "uid": uid })) {
                Response::Task { task, .. } => {
                    assert_eq!(task.item.status.as_deref(), Some("done"), "{uid}");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// `swarm-coordinator` step 5's eject arm: rework the `landable` lead, and reopen the work.
    fn coordinator_ejects(&self) {
        assert_eq!(self.fire("rework").phase, "implement");
        self.coordinator(json!({ "op": "task.set", "uid": self.lead, "status": "in_progress" }));
    }

    /// `swarm-reviewer`'s filing and recording, as a fresh `jkb-reviewer` subagent `agent`.
    fn reviewer_records(&self, agent: &str, pass: u32, must_fix_in: Option<&str>) -> Vec<String> {
        let rev = || self.as_agent(Some((agent, "jkb-reviewer")));
        ok(&rev(), json!({ "op": "role.bind", "uid": self.lead }));
        let ns = format!("repos/{REPO}/codereviews/pass-{pass}");
        let findings = must_fix_in.map_or_else(Vec::new, |file| {
            vec![json!({ "severity": "must-fix", "summary": "it breaks", "file": file, "line": 3 })]
        });
        let filed = match ok(
            &rev(),
            json!({ "op": "task.review_file", "ns": ns,
                    "run": { "reviewers": 1, "returned": 1 }, "findings": findings }),
        ) {
            Response::ReviewFiled { filed } => filed,
            other => panic!("{other:?}"),
        };
        match ok(
            &rev(),
            json!({ "op": "task.review_record", "repo": REPO, "branch": BRANCH, "findings": ns }),
        ) {
            Response::ReviewRecorded { recording } => {
                assert_eq!(recording.recorded.len(), 1, "{recording:?}");
                assert_eq!(recording.recorded[0].uid, self.lead);
            }
            other => panic!("{other:?}"),
        }
        filed.uids
    }

    /// `swarm-coordinator` step 4: observe the lead and route on its phase.
    fn coordinator_observes(&self) -> String {
        let m =
            Self::moved(self.coordinator(json!({ "op": "workflow.observe", "uid": self.lead })));
        assert!(m.moved, "{m:?}");
        if m.phase == "implement" {
            // Sent back: the coordinator reopens the work for the implementer.
            self.coordinator(
                json!({ "op": "task.set", "uid": self.lead, "status": "in_progress" }),
            );
        }
        m.phase
    }
}

impl Swarm {
    /// Two rounds with a must-fix in the same file: the lead is at `systemic_review`.
    fn reach_systemic_review(&self) {
        self.start_group();
        self.implementer_works();
        self.coordinator_submits();
        self.reviewer_records("r1", 1, Some("src/lib.rs"));
        assert_eq!(self.coordinator_observes(), "implement");
        self.implementer_works();
        self.coordinator_submits();
        self.reviewer_records("r2", 2, Some("src/lib.rs"));
        assert_eq!(self.coordinator_observes(), "systemic_review");
    }

    /// `swarm-systemic-reviewer`'s calls, as a `jkb-systemic-reviewer` subagent `agent`: bind, read
    /// the task, append the cause to its body, and fire `event` with the cause as its reason.
    fn systemic_reviewer_fires(&self, agent: &str, event: &str, cause: &str) -> String {
        let sys = || self.as_agent(Some((agent, "jkb-systemic-reviewer")));
        ok(&sys(), json!({ "op": "role.bind", "uid": self.lead }));
        match ok(&sys(), json!({ "op": "task.show", "uid": self.lead })) {
            Response::Task { .. } => {}
            other => panic!("{other:?}"),
        }
        ok(
            &sys(),
            json!({ "op": "task.edit", "uid": self.lead,
                    "text": format!("Systemic cause: {cause}"), "append": true }),
        );
        let m = Self::moved(ok(
            &sys(),
            json!({ "op": "workflow.fire", "uid": self.lead, "event": event, "reason": cause }),
        ));
        assert!(m.moved, "{event}: {m:?}");
        m.phase
    }

    /// The lead's last history line, which `jkb workflow show` prints.
    fn last_history(&self) -> String {
        match self.coordinator(json!({ "op": "workflow.show", "uid": self.lead })) {
            Response::Workflow { workflow } => workflow.history.last().cloned().unwrap_or_default(),
            other => panic!("{other:?}"),
        }
    }

    /// The designer budget's count: the tasks this run tagged as given a designer.
    fn designed(&self) -> usize {
        match self.coordinator(json!({ "op": "kb.query", "count": true,
            "dsl": format!("kind:task tag:swarm-designed={OWNER}") }))
        {
            Response::Count { count } => count,
            other => panic!("{other:?}"),
        }
    }

    /// Whether `owner` holds the lead's claim.
    fn claimed_by(&self, owner: &str) -> bool {
        match ok(&self.kb.op, json!({ "op": "task.claims" })) {
            Response::Claims { claims, .. } => claims
                .iter()
                .any(|c| c.uid == self.lead && c.owner == owner),
            other => panic!("{other:?}"),
        }
    }
}

/// The swarm's review loop through the workflow, with exactly the calls the `swarm-coordinator` and
/// `swarm-reviewer` templates make, each as the principal that makes it: a must-fix round sends the
/// work back to `implement`, and a second round with must-fixes in the same file goes on to a
/// systemic review rather than round the loop again.
#[test]
fn the_swarm_loop_reaches_a_systemic_review_when_must_fixes_repeat_an_area() {
    let s = Swarm::new();
    s.start_group();
    s.implementer_works();
    s.coordinator_submits();
    s.reviewer_records("r1", 1, Some("src/lib.rs"));
    assert_eq!(s.coordinator_observes(), "implement");
    s.implementer_works();
    s.coordinator_submits();
    s.reviewer_records("r2", 2, Some("src/lib.rs"));
    assert_eq!(s.coordinator_observes(), "systemic_review");
}

/// A task at `systemic_review` moves only by a systemic reviewer: the coordinator holds no systemic
/// event (jkb enforces the NEVER rule), nor can it send the work round the loop again, and neither
/// the implementer nor a fresh reviewer can move it. A swarm without a systemic reviewer stalls here.
#[test]
fn a_task_at_systemic_review_moves_only_by_a_systemic_reviewer() {
    let s = Swarm::new();
    s.reach_systemic_review();
    let stays = |b: &LocalBackend, event: &str, reason: Option<&str>| {
        let m = Swarm::moved(ok(
            b,
            json!({ "op": "workflow.fire", "uid": s.lead, "event": event, "reason": reason }),
        ));
        assert!(!m.moved && m.phase == "systemic_review", "{event}: {m:?}");
        m.refusal.unwrap_or_default()
    };
    let main = s.as_agent(None);
    for event in ["submit_systemic", "systemic_redesign"] {
        let why = stays(&main, event, Some("the coordinator decides"));
        assert!(why.contains("holds coordinator"), "{event}: {why}");
    }
    stays(&main, "submit_work", None);
    stays(&main, "rework", None);
    let imp = || s.as_agent(Some(("i1", "jkb-implementer")));
    ok(&imp(), json!({ "op": "role.bind", "uid": s.lead }));
    stays(&imp(), "submit_work", None);
    let rev = || s.as_agent(Some(("r3", "jkb-reviewer")));
    ok(&rev(), json!({ "op": "role.bind", "uid": s.lead }));
    stays(&rev(), "submit_systemic", Some("a reviewer decides"));
    let again = Swarm::moved(s.coordinator(json!({ "op": "workflow.observe", "uid": s.lead })));
    assert!(
        !again.moved && again.phase == "systemic_review",
        "{again:?}"
    );
}

/// Step 4b's code-pattern exit: the coordinator starts a `jkb-systemic-reviewer` (an agent type
/// `scripts/setup.sh` maps), which records the cause in the task and fires `submit_systemic` with
/// it as the reason. The work is back at `implement` for the same implementer, and the next clean
/// round makes it landable.
#[test]
fn a_systemic_review_sends_the_cause_back_to_the_same_implementer() {
    let s = Swarm::new();
    s.reach_systemic_review();
    let cause = "every caller re-derives the path; one helper owns it";
    assert_eq!(
        s.systemic_reviewer_fires("sr1", "submit_systemic", cause),
        "implement"
    );
    assert!(s.body().contains(&format!("Systemic cause: {cause}")));
    let last = s.last_history();
    assert!(
        last.contains("submit_systemic") && last.contains(cause),
        "{last}"
    );
    // The coordinator reopens the work for the implementer it kept, and goes back to step 3.
    s.coordinator(json!({ "op": "task.set", "uid": s.lead, "status": "in_progress" }));
    s.implementer_works();
    s.coordinator_submits();
    s.reviewer_records("r3", 3, None);
    assert_eq!(s.coordinator_observes(), "landable");
}

/// Step 4b's redesign exit: `systemic_redesign` puts the lead back at `design`, the group pauses
/// with its claim held, and a designer whose FEEDBACK is the reason writes a revised design, left at
/// `design_review` for the operator. A paused group is out of every read whatever its phase (it is
/// claimed), so no new group builds it; once the operator approves, the coordinator's check of the
/// paused lead's phase resumes the same group on its implementer.
#[test]
fn a_systemic_redesign_pauses_the_group_for_a_designer_and_resumes_it_once_approved() {
    let s = Swarm::new();
    s.reach_systemic_review();
    let cause = "the cache is per-call, not per-run: callers cannot see what it holds";
    assert_eq!(
        s.systemic_reviewer_fires("sr1", "systemic_redesign", cause),
        "design"
    );
    // DESIGNING A TASK's feedback: the last history line is the systemic redesign, with its reason.
    let last = s.last_history();
    assert!(
        last.contains("systemic_redesign") && last.contains(cause),
        "{last}"
    );
    // A redesigned task always goes to a designer: neither the gate being off, a hand-pick nor an
    // old `design=approved` tag lets its superseded design be built.
    ok(
        &s.kb.op,
        json!({ "op": "task.tag", "uid": s.lead, "facet_value": "design=approved", "mode": "set" }),
    );
    for gate in [Gate::On, Gate::Off, Gate::HandPicked] {
        for read in [Read::Legacy, Read::Design] {
            assert_eq!(
                route(&s.seen(&s.lead), read, gate),
                Path::Design,
                "{gate:?} {read:?}"
            );
        }
    }
    // The group keeps its claim through the redesign: the designer works on it as it is, and no
    // read offers it.
    assert!(s.claimed_by(OWNER));
    assert!(
        !s.in_frontier(),
        "a paused group is not offered to a new group"
    );
    // Exempt from the designer budget: claimed nothing, tagged nothing.
    let revised =
        format!("Design (revised; it replaces every earlier Design section above): {cause}");
    s.designer_designs("d1", &revised);
    // Awaiting approval: still the group's, and in no read — with no tag to say so.
    assert!(s.claimed_by(OWNER));
    assert_eq!(s.designed(), 0, "a paused group's redesign uses no budget");
    assert_eq!(s.phase(), "design_review");
    assert!(!s.in_frontier(), "still awaiting approval");
    // Approved: the check of the paused lead's phase finds it at `implement` and resumes the group —
    // reopened for the same implementer, which the coordinator continues rather than replaces.
    s.operator_fires("approve_design", None);
    assert_eq!(s.phase(), "implement");
    assert!(
        !s.in_frontier(),
        "a resumed group is still its own, not the frontier's"
    );
    s.coordinator(json!({ "op": "task.set", "uid": s.lead, "status": "in_progress" }));
    s.implementer_works();
    s.coordinator_submits();
    s.reviewer_records("r3", 3, None);
    assert_eq!(s.coordinator_observes(), "landable");
}

/// A task an earlier run left at `systemic_review` — its coordinator stopped mid-review, its claim
/// released — is offered by the next run's frontier, and step 1b sends it, as a group of one, straight
/// to a systemic reviewer. With no implementer of this run's, the `submit_systemic` exit starts a new
/// one on the task's branch, and the work goes round to `landable`.
#[test]
fn a_task_an_earlier_run_left_at_systemic_review_is_taken_up_by_the_next() {
    let s = Swarm::new();
    s.reach_systemic_review();
    // The earlier run stops: its claim is released, and nothing settles the task.
    s.coordinator(json!({ "op": "task.set", "uid": s.lead, "status": "open" }));
    s.coordinator(json!({ "op": "task.release", "uid": s.lead, "owner": OWNER }));
    // The next run's read A offers it, and step 1b routes it to step 4b.
    assert_eq!(s.offered_by(&s.lead), Some(Read::Build));
    for gate in [Gate::On, Gate::Off, Gate::HandPicked] {
        assert_eq!(s.admission(&s.lead, gate), Some(Path::Systemic), "{gate:?}");
    }
    s.start_group_on(Path::Systemic);
    assert!(s.claimed_by(OWNER));
    // The branch the systemic reviewer reads and the new implementer continues is the task's own.
    match ok(&s.kb.op, json!({ "op": "task.show", "uid": s.lead })) {
        Response::Task { task, .. } => assert!(
            task.item
                .tags
                .iter()
                .any(|t| t.facet == "branch" && t.value == BRANCH),
            "{:?}",
            task.item.tags
        ),
        other => panic!("{other:?}"),
    }
    let cause = "the parser and the renderer each own the escape rules";
    assert_eq!(
        s.systemic_reviewer_fires("sr2", "submit_systemic", cause),
        "implement"
    );
    s.coordinator(json!({ "op": "task.set", "uid": s.lead, "status": "in_progress" }));
    let imp = s.as_agent(Some(("i2", "jkb-implementer")));
    match ok(&imp, json!({ "op": "task.show", "uid": s.lead })) {
        Response::Task { .. } => {}
        other => panic!("{other:?}"),
    }
    s.coordinator_submits();
    s.reviewer_records("r3", 3, None);
    assert_eq!(s.coordinator_observes(), "landable");
}

/// A designer that cannot finish tags its task `design-failed=<run owner>` before it reports, as
/// `swarm-designer` says — a write its role may make on its bound task — so a coordinator that lost
/// the report to a compaction reads the failure from jkb rather than waiting for it.
#[test]
fn a_designer_records_its_failure_on_its_task() {
    let s = Swarm::at_design();
    s.coordinator(json!({ "op": "task.claim", "uid": s.lead, "owner": OWNER }));
    let des = || s.as_agent(Some(("d1", "jkb-designer")));
    ok(&des(), json!({ "op": "role.bind", "uid": s.lead }));
    ok(
        &des(),
        json!({ "op": "task.tag", "uid": s.lead,
                "facet_value": format!("design-failed={OWNER}"), "mode": "set" }),
    );
    // The compaction rebuild: claimed by the run, no `swarm-lead=`, still at `design`, and tagged.
    assert_eq!(s.phase(), "design");
    assert!(s.claimed_by(OWNER));
    match s.coordinator(json!({ "op": "task.show", "uid": s.lead })) {
        Response::Task { task, .. } => {
            let tags = &task.item.tags;
            assert!(tags
                .iter()
                .any(|t| t.facet == "design-failed" && t.value == OWNER));
            assert!(!tags.iter().any(|t| t.facet == "swarm-lead"));
        }
        other => panic!("{other:?}"),
    }
}

/// The same loop, with the second round clean: the work is `landable`. A merge-queue eject then
/// reworks it, and the resubmitted work goes round the loop again.
#[test]
fn the_swarm_loop_reaches_landable_on_a_clean_round() {
    let s = Swarm::new();
    s.start_group();
    s.implementer_works();
    s.coordinator_submits();
    let round_1 = s.reviewer_records("r1", 1, Some("src/lib.rs"));
    assert_eq!(s.coordinator_observes(), "implement");
    s.implementer_works();
    s.coordinator_submits();
    s.reviewer_records("r2", 2, None);
    assert_eq!(s.coordinator_observes(), "landable");
    // The clean round confirms round 1's must-fix fixed: the coordinator closes it.
    s.coordinator_closes(&round_1);
    // A coordinator observing again finds nothing new to act on.
    let again = Swarm::moved(s.coordinator(json!({ "op": "workflow.observe", "uid": s.lead })));
    assert!(!again.moved && again.phase == "landable", "{again:?}");
    // Without the rework, the resubmission is refused: the graph has no `landable` submit.
    let refused = Swarm::moved(
        s.coordinator(json!({ "op": "workflow.fire", "uid": s.lead, "event": "submit_work" })),
    );
    assert!(!refused.moved && refused.phase == "landable", "{refused:?}");
    s.coordinator_ejects();
    s.implementer_works();
    s.coordinator_submits();
    s.reviewer_records("r3", 3, None);
    assert_eq!(s.coordinator_observes(), "landable");
}

/// A coordinator that rebuilt after compaction and re-runs step 3 for a lead still at `landable`
/// reworks it first, as the phase table says, rather than being refused.
#[test]
fn a_resubmission_from_landable_reworks_first() {
    let s = Swarm::new();
    s.start_group();
    s.implementer_works();
    s.coordinator_submits();
    s.reviewer_records("r1", 1, None);
    assert_eq!(s.coordinator_observes(), "landable");
    s.coordinator(json!({ "op": "task.set", "uid": s.lead, "status": "in_progress" }));
    s.coordinator_submits();
    assert_eq!(s.phase(), "review");
}

/// Step 1 reads exactly the phases step 3 can drive — from each WORKFLOW phase the template's calls
/// reach `review` — and from every phase no read offers, and from `design`, those calls are refused.
/// So no read offers a task at `design_review`, `landed` or `cancelled`, a task at `systemic_review`
/// goes to a systemic reviewer rather than being rebuilt (or landed past its systemic review as a
/// non-lead), and a task at `design` can only go the legacy way.
#[test]
fn step_one_admits_exactly_the_phases_step_three_can_drive() {
    let refused = |s: &Swarm, event: &str| {
        let m = Swarm::moved(
            s.coordinator(json!({ "op": "workflow.fire", "uid": s.lead, "event": event })),
        );
        assert!(!m.moved, "{event} from {}: {m:?}", s.phase());
    };
    for phase in [
        "design",
        "design_review",
        "implement",
        "review",
        "landable",
        "systemic_review",
        "landed",
        "cancelled",
    ] {
        let s = Swarm::at_design();
        if phase != "design" {
            s.operator_fires("override", Some(phase));
        }
        assert_eq!(s.phase(), phase);
        // Ungated, so `design` is read at all: the reads by phase alone.
        let admitted = s.admission(&s.lead, Gate::Off);
        assert_eq!(
            admitted.is_none(),
            ["design_review", "landed", "cancelled"].contains(&phase),
            "{phase}: {admitted:?}"
        );
        match admitted {
            Some(Path::Workflow) => {
                s.start_group();
                s.coordinator_submits();
                assert_eq!(s.phase(), "review", "from {phase}");
            }
            Some(Path::Legacy | Path::Design | Path::Systemic) | None => {
                assert!([
                    "design",
                    "design_review",
                    "systemic_review",
                    "landed",
                    "cancelled"
                ]
                .contains(&phase));
                refused(&s, "submit_work");
                refused(&s, "rework");
            }
        }
    }
}

/// The design gate is one rule: approved in jkb (a phase past design, no tag needed) takes the
/// workflow path on a gated run, from read A; still at `design`, the tag (read B), a hand-pick or no
/// gate takes the legacy path, and the gate holds back the rest (read C). A review finding — at
/// `design`, tagged `finding=review` when filed — is in no read, so no run builds a must-fix
/// another implementer is fixing, nor starts a designer on one.
#[test]
fn step_one_admits_by_phase_or_tag_and_never_a_finding() {
    let gates = [Gate::On, Gate::Off, Gate::HandPicked];
    // Approved in jkb, untagged: read A offers it.
    let approved = Swarm::new();
    assert_eq!(approved.offered_by(&approved.lead), Some(Read::Build));
    for gate in gates {
        assert_eq!(
            approved.admission(&approved.lead, gate),
            Some(Path::Workflow),
            "{gate:?}"
        );
    }
    // At `design`, untagged: read C, held back by the gate only, for a designer.
    let untagged = Swarm::at_design();
    assert_eq!(untagged.offered_by(&untagged.lead), Some(Read::Design));
    assert_eq!(
        untagged.admission(&untagged.lead, Gate::On),
        Some(Path::Design)
    );
    assert_eq!(
        untagged.admission(&untagged.lead, Gate::Off),
        Some(Path::Legacy)
    );
    assert_eq!(
        untagged.admission(&untagged.lead, Gate::HandPicked),
        Some(Path::Legacy)
    );
    // ...and tagged: read B, legacy on a gated run too.
    ok(
        &untagged.kb.op,
        json!({ "op": "task.tag", "uid": untagged.lead, "facet_value": "design=approved", "mode": "set" }),
    );
    assert_eq!(untagged.offered_by(&untagged.lead), Some(Read::Legacy));
    assert_eq!(
        untagged.admission(&untagged.lead, Gate::On),
        Some(Path::Legacy)
    );

    // A finding the swarm's reviewer filed: an ordinary open task at `design`, which its filing tag
    // keeps out of every read, even tagged approved.
    let s = Swarm::new();
    s.start_group();
    s.implementer_works();
    s.coordinator_submits();
    let findings = s.reviewer_records("r1", 1, Some("src/lib.rs"));
    let finding = &findings[0];
    ok(
        &s.kb.op,
        json!({ "op": "task.tag", "uid": finding, "facet_value": "design=approved", "mode": "set" }),
    );
    assert_eq!(s.seen(finding).phase, "design");
    for gate in gates {
        assert_eq!(s.admission(finding, gate), None, "{gate:?}");
    }
    // ...which is the filing tag's doing: without it, read B would offer it.
    ok(
        &s.kb.op,
        json!({ "op": "task.tag", "uid": finding, "facet_value": "finding=review", "mode": "rm" }),
    );
    assert_eq!(s.offered_by(finding), Some(Read::Legacy));
}

/// Under a swarm a designer writes the design and the operator approves it. A task the gate holds
/// back goes to a `jkb-designer`, with exactly the calls `swarm-designer` and the coordinator's
/// DESIGNING A TASK make: it lands at `design_review`, where no read offers it — with no tag and no
/// re-check. The coordinator cannot submit a design itself, nor the designer approve one. Rejected,
/// read C offers it again for a designer, with the operator's notes last in its history; approved,
/// read A offers it and it is built on the workflow path.
#[test]
fn a_task_the_gate_holds_back_is_designed_left_to_the_operator_and_then_built() {
    let s = Swarm::at_design();
    assert_eq!(s.admission(&s.lead, Gate::On), Some(Path::Design));
    // jkb holds the coordinator to scheduling: the design is not its to submit.
    let refused = Swarm::moved(
        s.coordinator(json!({ "op": "workflow.fire", "uid": s.lead, "event": "submit_design" })),
    );
    assert!(!refused.moved && refused.phase == "design", "{refused:?}");
    let why = refused.refusal.unwrap_or_default();
    assert!(
        why.contains("holds coordinator") && why.contains("allowed: operator, designer"),
        "{why}"
    );
    s.coordinator_designs("d1", "Design: the approach");
    assert!(s.body().contains("Design: the approach"));
    match ok(&s.kb.op, json!({ "op": "task.show", "uid": s.lead })) {
        Response::Task { task, .. } => {
            assert_eq!(task.item.status.as_deref(), Some("open"));
            // The only tag the swarm wrote on it is the budget's count.
            let swarm_tags: Vec<&str> = task
                .item
                .tags
                .iter()
                .filter(|t| t.facet.starts_with("swarm-"))
                .map(|t| t.facet.as_str())
                .collect();
            assert_eq!(swarm_tags, ["swarm-designed"]);
        }
        other => panic!("{other:?}"),
    }
    // Open and unclaimed at `design_review`, yet in no read: the phase keeps it out, so tasks
    // awaiting approval never fill a read's limit.
    assert_eq!(s.phase(), "design_review");
    assert_eq!(s.admission(&s.lead, Gate::On), None);
    assert!(!s.in_frontier(), "a design awaiting approval is in no read");
    // A designer does not approve its own design.
    let refused = Swarm::moved(ok(
        &s.as_agent(Some(("d1", "jkb-designer"))),
        json!({ "op": "workflow.fire", "uid": s.lead, "event": "approve_design" }),
    ));
    assert!(
        !refused.moved && refused.phase == "design_review",
        "{refused:?}"
    );

    // Rejected with notes: back in read R at once, for a designer, the notes the last history line.
    s.operator_fires_with("reject_design", "name the files");
    let last = s.last_history();
    assert!(
        last.contains("reject_design") && last.contains("name the files"),
        "{last}"
    );
    assert_eq!(s.offered_by(&s.lead), Some(Read::Redesign));
    for gate in [Gate::On, Gate::Off, Gate::HandPicked] {
        assert_eq!(s.admission(&s.lead, gate), Some(Path::Design), "{gate:?}");
    }
    let revised =
        "Design (revised; it replaces every earlier Design section above): name the files";
    s.coordinator_designs("d2", revised);
    let body = s.body();
    assert!(
        body.find("Design: the approach").unwrap() < body.find(revised).unwrap(),
        "{body}"
    );
    assert!(!s.in_frontier());

    // Approved mid-run: read A offers it, and it is built on the workflow path, no tag needed.
    s.operator_fires("approve_design", None);
    assert_eq!(s.offered_by(&s.lead), Some(Read::Build));
    s.start_group();
    s.implementer_works();
    s.coordinator_submits();
    s.reviewer_records("r1", 1, None);
    assert_eq!(s.coordinator_observes(), "landable");
}

/// The designer budget caps the tasks one run designs, in total. A run with a budget of 1 over three
/// tasks at `design` reads C at its one designer left, designs that task, and then stops reading C:
/// the other two stay at `design`, untagged and unsettled, and the report counts them as left for
/// want of budget.
#[test]
fn a_designer_budget_of_one_designs_one_task_and_reports_the_rest() {
    let s = Swarm::at_design();
    let mut tasks = vec![s.lead.clone()];
    for text in ["the second", "the third"] {
        match ok(&s.kb.op, json!({ "op": "task.add", "text": text })) {
            Response::Added { added } => tasks.push(added.uid),
            other => panic!("{other:?}"),
        }
    }
    s.budget.set(1);
    let designers_left = || s.designers_left();
    // Reads A and B are empty; read C, at the designers left, offers exactly one.
    assert!(s.read(Read::Build, 100).is_empty());
    assert!(s.read(Read::Legacy, 100).is_empty());
    let c = s.read(Read::Design, designers_left());
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(
        c[0], s.lead,
        "the frontier's order: the oldest first among equals"
    );
    s.coordinator_designs("d1", "Design: the one the budget allows");
    assert_eq!(s.phase(), "design_review");
    // The budget is spent, by a count that survives a compaction: read C is not read again, and
    // reads A and B are empty, so the run stops.
    assert_eq!(designers_left(), 0);
    assert!(s.read(Read::Build, 100).is_empty());
    assert!(s.read(Read::Legacy, 100).is_empty());
    // The report: two at `design` left undesigned, read C's terms counted.
    let left = match s.coordinator(json!({ "op": "kb.query", "count": true,
        "dsl": format!("kind:task is:ready {} {}", Read::Design.terms(), every_read()) }))
    {
        Response::Count { count } => count,
        other => panic!("{other:?}"),
    };
    assert_eq!(left, 2);
    // Neither was written to: no settle tag, no count.
    for uid in &tasks[1..] {
        match ok(&s.kb.op, json!({ "op": "task.show", "uid": uid })) {
            Response::Task { task, .. } => assert!(
                !task.item.tags.iter().any(|t| t.facet.starts_with("swarm-")),
                "{uid}: {:?}",
                task.item.tags
            ),
            other => panic!("{other:?}"),
        }
    }
}

/// A design the operator rejects mid-run gets its free re-design even when the budget is spent: read
/// R offers it at any budget, while read C — which leaves it out — is not read at 0, so a new task at
/// `design` stays unoffered. Read C alone (capped at DESIGNERS LEFT, skipped at 0) never offered the
/// rejected task, the run stopped, and the report counted it as left for want of budget.
#[test]
fn a_design_rejected_mid_run_is_redesigned_on_a_spent_budget() {
    let s = Swarm::at_design();
    let new = match ok(
        &s.kb.op,
        json!({ "op": "task.add", "text": "a new design task" }),
    ) {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    s.budget.set(1);
    assert_eq!(
        s.read(Read::Design, s.designers_left()),
        vec![s.lead.clone()]
    );
    s.coordinator_designs("d1", "Design: the first try");
    assert_eq!(s.designers_left(), 0);
    // Spent: read C is not read, so the new task is offered by nothing.
    assert_eq!(s.limit(Read::Design, Gate::On), None);
    assert_eq!(s.offered_by(&new), None);
    assert!(
        s.read(Read::Redesign, 100).is_empty(),
        "nothing rejected yet"
    );
    // Rejected: read R offers it, at a spent budget, and step 1b sends it to a designer.
    s.operator_fires_with("reject_design", "name the files");
    assert_eq!(s.read(Read::Redesign, 100), vec![s.lead.clone()]);
    assert_eq!(s.offered_by(&s.lead), Some(Read::Redesign));
    assert_eq!(s.admission(&s.lead, Gate::On), Some(Path::Design));
    assert_eq!(
        s.offered_by(&new),
        None,
        "the budget still refuses a new design"
    );
    // Read C leaves it out at any budget, so the two reads never offer one task twice, and the
    // report's count of read C's tasks does not call it left for want of budget.
    s.budget.set(10);
    assert!(!s.read(Read::Design, 100).contains(&s.lead));
    s.budget.set(1);
    // The re-design uses no budget: the count is unchanged.
    s.coordinator_designs(
        "d2",
        "Design (revised; it replaces every earlier Design section above): files",
    );
    assert_eq!(s.designed(), 1);
    assert_eq!(s.phase(), "design_review");
    let left = match s.coordinator(json!({ "op": "kb.query", "count": true,
        "dsl": format!("kind:task is:ready {} {}", Read::Design.terms(), every_read()) }))
    {
        Response::Count { count } => count,
        other => panic!("{other:?}"),
    };
    assert_eq!(left, 1, "only the new task is left for want of budget");
}

/// A PAUSED group's redesign is exempt from the designer budget: its work is already built and
/// reviewed, and the budget is for starting new designs. With a budget of 0 — which designs nothing
/// new — a `systemic_redesign` still gets its designer, the count stays at 0, and once approved the
/// group resumes.
#[test]
fn a_paused_groups_redesign_runs_on_a_budget_of_zero() {
    let s = Swarm::new();
    s.reach_systemic_review();
    assert_eq!(
        s.systemic_reviewer_fires("sr1", "systemic_redesign", "the design is wrong"),
        "design"
    );
    s.budget.set(0);
    assert_eq!(s.designers_left(), 0, "no designer left for new designs");
    // Step 4b starts it anyway, claiming and tagging nothing.
    assert!(s.claimed_by(OWNER));
    s.designer_designs(
        "d1",
        "Design (revised; it replaces every earlier Design section above): fixed",
    );
    assert_eq!(s.phase(), "design_review");
    assert_eq!(s.designed(), 0, "the redesign did not count");
    assert!(s.claimed_by(OWNER));
    s.operator_fires("approve_design", None);
    s.coordinator(json!({ "op": "task.set", "uid": s.lead, "status": "in_progress" }));
    s.implementer_works();
    s.coordinator_submits();
    s.reviewer_records("r3", 3, None);
    assert_eq!(s.coordinator_observes(), "landable");
}

/// A task still at `design` that the design gate let through is built on the legacy path: the same
/// reviewer files and records its rounds, the coordinator routes on the verdict, and its workflow is
/// never driven. The clean round's findings are closed as on the workflow path.
#[test]
fn a_legacy_task_at_design_builds_on_the_verdict_path() {
    let s = Swarm::at_design();
    ok(
        &s.kb.op,
        json!({ "op": "task.tag", "uid": s.lead, "facet_value": "design=approved", "mode": "set" }),
    );
    s.start_group_on(Path::Legacy);
    s.implementer_works();
    s.coordinator_submits_legacy();
    let round_1 = s.reviewer_records("r1", 1, Some("src/lib.rs"));
    // `VERDICT: request_changes`: back to the implementer, with nothing for the workflow to observe.
    let m = Swarm::moved(s.coordinator(json!({ "op": "workflow.observe", "uid": s.lead })));
    assert!(!m.moved && m.phase == "design", "{m:?}");
    s.coordinator(json!({ "op": "task.set", "uid": s.lead, "status": "in_progress" }));
    s.implementer_works();
    s.coordinator_submits_legacy();
    s.reviewer_records("r2", 2, None);
    // `VERDICT: approve`: the findings it confirms fixed are closed before the merge queue.
    s.coordinator_closes(&round_1);
    assert_eq!(s.phase(), "design");
}

fn roles_under(b: &LocalBackend, strategy: Option<&str>) -> super::RolesView {
    let r = match strategy {
        Some(s) => json!({ "op": "workflow.roles", "strategy": s }),
        None => json!({ "op": "workflow.roles" }),
    };
    match ok(b, r) {
        Response::WorkflowRoles { roles } => *roles,
        other => panic!("{other:?}"),
    }
}

fn role<'a>(v: &'a super::RolesView, name: &str) -> &'a super::RoleView {
    v.roles
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no {name} in {v:?}"))
}

fn toggled(r: &super::RoleView, toggle: &str) -> Option<bool> {
    r.toggled_by
        .iter()
        .find(|t| t.toggle == toggle)
        .map(|t| t.on)
}

/// `workflow.roles` answers every role once, in `Role::ALL`'s order — those no template acts as
/// too — with what it may do under the strategy, decided by the deciders the dispatch and the
/// workflow use.
#[test]
fn every_role_is_answered_with_what_the_strategy_lets_it_do() {
    let kb = Kb::new();
    let reviewed = roles_under(&kb.op, None);
    assert_eq!(reviewed.strategy, "design-reviewed", "the default");
    let order: Vec<&str> = reviewed.roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(order, super::role_names(), "every role, once, in order");

    // A designer submits the design, and acts next where it is written.
    let designer = role(&reviewed, "designer");
    assert_eq!(designer.events, vec!["submit_design"]);
    assert_eq!(designer.acts_next_in, vec!["design"]);
    assert_eq!(designer.fires_in, vec!["design"]);
    assert_eq!(
        designer.describe,
        jkb_core::roles::Role::Designer.describe()
    );
    assert_eq!(designer.granted_by, vec!["operator", "coordinator"]);
    assert!(designer.may_grant.is_empty());
    assert!(designer.toggled_by.is_empty(), "no toggle can name it");

    // A role no agent template acts as still has its op classes, each with its sentence.
    let systemic = role(&reviewed, "systemic_reviewer");
    let ops: Vec<&str> = systemic.ops.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        ops,
        vec!["read", "hook", "task_write", "workflow", "exec_request"]
    );
    assert!(systemic.ops.iter().all(|o| !o.describe.is_empty()));
    assert_eq!(systemic.acts_next_in, vec!["systemic_review"]);
    assert_eq!(systemic.granted_by, vec!["operator"]);

    // Under the default the coordinator neither approves designs nor lands; the toggles that could
    // let it are named, off.
    let coordinator = role(&reviewed, "coordinator");
    assert!(!coordinator.events.iter().any(|e| e == "approve_design"));
    assert!(!coordinator.lands);
    assert!(!coordinator.ops.iter().any(|o| o.name == "land"));
    assert_eq!(toggled(coordinator, "approves_design"), Some(false));
    assert_eq!(toggled(coordinator, "lands"), Some(false));
    assert_eq!(coordinator.may_grant, vec!["designer", "implementer"]);
    // The operator holds every toggle, whatever a spec lists.
    let operator = role(&reviewed, "operator");
    assert_eq!(toggled(operator, "approves_design"), Some(true));
    assert_eq!(toggled(operator, "lands"), Some(true));
    assert!(operator.lands);
    assert!(operator.acts_next_in.iter().any(|p| p == "design_review"));

    // `coordinated`: the approves_design toggle gives the coordinator approve and reject.
    let coordinated = roles_under(&kb.op, Some("coordinated"));
    assert_eq!(coordinated.strategy, "coordinated");
    let coordinator = role(&coordinated, "coordinator");
    for e in ["approve_design", "reject_design"] {
        assert!(coordinator.events.iter().any(|x| x == e), "{e}");
    }
    assert_eq!(toggled(coordinator, "approves_design"), Some(true));
    assert_eq!(toggled(coordinator, "lands"), Some(false));
    assert!(coordinator
        .acts_next_in
        .iter()
        .any(|p| p == "design_review"));
    assert!(coordinator.fires_in.iter().any(|p| p == "design_review"));

    // `autonomous`: the lands toggle lets the coordinator land, as an op class too.
    let autonomous = roles_under(&kb.op, Some("autonomous"));
    let coordinator = role(&autonomous, "coordinator");
    assert!(coordinator.lands);
    assert!(coordinator.ops.iter().any(|o| o.name == "land"));
    assert_eq!(toggled(coordinator, "lands"), Some(true));
    assert!(coordinator.acts_next_in.iter().any(|p| p == "landable"));
    // The toggles, with this strategy's value.
    let lands = autonomous
        .toggles
        .iter()
        .find(|t| t.name == "lands")
        .unwrap();
    assert_eq!(lands.label, "lands");
    assert_eq!(lands.controls, vec!["land"]);
    assert_eq!(lands.domain, vec!["operator", "coordinator"]);
    assert_eq!(lands.value, vec!["operator", "coordinator"]);
    let approves = autonomous
        .toggles
        .iter()
        .find(|t| t.name == "approves_design")
        .unwrap();
    assert_eq!(approves.controls, vec!["approve_design", "reject_design"]);
    assert_eq!(approves.acts_in.as_deref(), Some("design_review"));
    assert_eq!(approves.label, "approves designs");
}

/// A role's op classes are `workflow.graph`'s `role_ops` for the same strategy: one function answers
/// both, so the Roles view and the Lifecycle panel cannot disagree.
#[test]
fn a_roles_op_classes_are_the_graphs() {
    let kb = Kb::new();
    for strategy in ["design-reviewed", "coordinated", "autonomous"] {
        let g = match ok(
            &kb.op,
            json!({ "op": "workflow.graph", "strategy": strategy }),
        ) {
            Response::WorkflowGraph { graph } => *graph,
            other => panic!("{other:?}"),
        };
        let v = roles_under(&kb.op, Some(strategy));
        for r in &v.roles {
            let ops: Vec<String> = r.ops.iter().map(|o| o.name.clone()).collect();
            assert_eq!(ops, g.role_ops[&r.name], "{strategy} {}", r.name);
            let fired: Vec<String> = {
                let mut seen = Vec::new();
                for t in &g.workflow.transitions {
                    if t.roles.contains(&r.name) && !seen.contains(&t.event) {
                        seen.push(t.event.clone());
                    }
                }
                seen
            };
            // Every act the graph lets it fire is in its events (the events cover the whole
            // event set; the graph only the rows its machine has).
            for e in &fired {
                assert!(r.events.contains(e), "{strategy} {}: {e}", r.name);
            }
        }
    }
}

/// An attested agent type mapped to a role is listed with it; a reader runs the op; an unknown
/// strategy is a named refusal.
#[test]
fn mapped_agent_types_are_listed_and_any_reader_may_ask() {
    let kb = Kb::new();
    ok(
        &kb.op,
        json!({ "op": "role.map", "agent_type": "jkb-reviewer", "role": "reviewer" }),
    );
    let v = roles_under(&kb.as_role("implementer"), None);
    assert_eq!(role(&v, "reviewer").agent_types, vec!["jkb-reviewer"]);
    assert!(role(&v, "designer").agent_types.is_empty());
    let e = call(
        &kb.op,
        json!({ "op": "workflow.roles", "strategy": "no-such-strategy" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
}

/// `ui/app/test/fixtures/workflow-roles.json` is what `workflow.roles` answers for every preset: the
/// app's decoder and Roles view are tested against it, so a wire change fails there.
#[test]
fn roles_fixture_matches_presets() {
    let db = Db::open_in_memory().unwrap();
    let presets: Vec<&'static str> = jkb_core::workflow::strategy::PRESETS
        .iter()
        .map(|p| p.name)
        .collect();
    let answered: serde_json::Map<String, Value> = db
        .read_with(move |c| {
            presets
                .into_iter()
                .map(|name| {
                    let v = super::roles(c, Some(name))?;
                    Ok((name.to_owned(), serde_json::to_value(v).unwrap()))
                })
                .collect::<Result<_, ApiError>>()
        })
        .unwrap();
    let answered = Value::Object(answered);
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ui/app/test/fixtures/workflow-roles.json");
    if std::env::var_os(WRITE_FIXTURE).is_some() {
        let text = serde_json::to_string_pretty(&answered).unwrap() + "\n";
        std::fs::write(&file, text).unwrap();
        return;
    }
    let held: Value = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    assert!(
        held == answered,
        "{} is not what workflow.roles answers for every preset; rewrite it with\n  \
         {WRITE_FIXTURE}=1 ./scripts/test.sh -p jkb-api roles_fixture_matches_presets",
        file.display()
    );
}

/// A toggle's list names roles beyond the operator, who always holds it: a strategy listing only the
/// coordinator still has the operator landing, and says so.
#[test]
fn the_operator_holds_a_toggle_its_list_leaves_out() {
    let kb = Kb::new();
    ok(
        &kb.op,
        json!({
            "op": "workflow.define",
            "name": "coord-lands",
            "describe": "The coordinator lands.",
            "spec": { "graph": "design-gate", "toggles": { "lands": ["coordinator"] } },
        }),
    );
    let v = roles_under(&kb.op, Some("coord-lands"));
    let lands = v.toggles.iter().find(|t| t.name == "lands").unwrap();
    assert_eq!(lands.value, vec!["coordinator"], "as the spec stores it");
    let operator = role(&v, "operator");
    assert!(operator.lands);
    assert_eq!(toggled(operator, "lands"), Some(true));
    assert_eq!(toggled(role(&v, "coordinator"), "lands"), Some(true));
}
