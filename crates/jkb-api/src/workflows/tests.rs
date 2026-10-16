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
    let (listed, roles) = match ok(&coordinator, json!({ "op": "workflow.agents" })) {
        Response::WorkflowAgents { agents, roles } => (agents, roles),
        other => panic!("{other:?}"),
    };
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

/// One swarm run's principals on one database: the main session (the container credential's ceiling,
/// the coordinator), and attested subagents of the types `scripts/setup.sh` maps — `jkb-reviewer` to
/// the reviewer, `jkb-implementer` to the implementer (pinned against the script by
/// `setup_maps_every_subagent_type_to_its_templates_role` in `jkb-core`).
struct Swarm {
    kb: Kb,
    hook: LocalBackend,
    tool: std::cell::Cell<u32>,
    lead: String,
}

/// The path `swarm-coordinator` builds a group on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    /// The full workflow: `submit_work`, recorded rounds, `observe`, routing on the phase.
    Workflow,
    /// A task still at `design` that the run's design gate let through: routed on the verdict.
    Legacy,
}

/// `swarm-coordinator` step 1's admission table, by the phase `jkb workflow show` prints. `None`
/// leaves the task out. Pinned against the template's text by `jkb-core`'s agents tests, and against
/// what jkb lets step 3 do by `step_one_admits_exactly_the_phases_step_three_can_drive`.
fn admission(phase: &str) -> Option<Path> {
    match phase {
        "implement" | "review" | "landable" => Some(Path::Workflow),
        "design" => Some(Path::Legacy),
        _ => None,
    }
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

    /// A task with no workflow history, so at `design` — where nearly every task sits, since
    /// `/design-pass` sets only the `design=approved` tag.
    fn at_design() -> Self {
        let kb = Kb::new();
        let lead = match ok(&kb.op, json!({ "op": "task.add", "text": "the lead" })) {
            Response::Added { added } => added.uid,
            other => panic!("{other:?}"),
        };
        for (agent_type, role) in [
            ("jkb-reviewer", "reviewer"),
            ("jkb-implementer", "implementer"),
        ] {
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

    /// `swarm-coordinator` step 1 admits the lead on `path`, and step 2 claims it (which starts it),
    /// tags its repository and records it as the group's lead.
    fn start_group_on(&self, path: Path) {
        assert_eq!(admission(&self.phase()), Some(path));
        self.coordinator(json!({ "op": "task.claim", "uid": self.lead, "owner": OWNER }));
        self.coordinator(json!({ "op": "task.tag", "uid": self.lead,
                                 "facet_value": format!("repo={REPO}"), "mode": "set" }));
        self.coordinator(json!({ "op": "task.tag", "uid": self.lead,
                                 "facet_value": format!("swarm-lead={}", self.lead), "mode": "set" }));
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

/// Step 1 admits exactly the phases step 3 can drive — from each WORKFLOW phase the template's calls
/// reach `review` — and from every phase it leaves out, and from `design`, those calls are refused.
/// So a task at `design_review` is excluded, a task at `systemic_review` is not rebuilt (and not
/// landed past its systemic review as a non-lead), and a task at `design` can only go the legacy way.
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
        match admission(phase) {
            Some(Path::Workflow) => {
                s.start_group();
                s.coordinator_submits();
                assert_eq!(s.phase(), "review", "from {phase}");
            }
            Some(Path::Legacy) | None => {
                refused(&s, "submit_work");
                refused(&s, "rework");
            }
        }
    }
    assert_eq!(admission("design_review"), None);
    assert_eq!(admission("systemic_review"), None);
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
