//! The Workflows tab's data as `@jkb/core` reads it (D53.7): the answers' shapes, the requests, what
//! a save sends, and the two layouts. Runs against `dist/`.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import test from "node:test";

import {
  agentGraph,
  agentSetsOf,
  canContribute,
  decodeAgent,
  decodeAgents,
  decodeWorkflowGraph,
  draftOf,
  editBetween,
  eventsFiredBy,
  isAgentName,
  layered,
  machineDrawing,
  markerSources,
  placeholdersOf,
  rowKey,
  sameSelection,
  saveRequest,
  selectionIn,
  stateRows,
  takenByRow,
  workflowOps,
} from "../dist/index.js";

const ok = (value) => ({ ok: true, value });

const agent = (over = {}) => ({
  name: "swarm-implementer",
  version: 1,
  source: "packaged",
  workflow: "task-swarm",
  role: "implementer",
  role_ops: ["read", "task_write"],
  fragment: false,
  describe: "Builds it.",
  template: "Build {{what}} in {{repo}}.",
  placeholders: ["repo", "what"],
  permissions: { isolation: "worktree", model: null, writes: "code" },
  hands_off_to: [],
  based_on: null,
  defined_at: null,
  packaged_version: 1,
  overrides_packaged: false,
  behind_packaged: false,
  packaged_base: 1,
  matches_packaged: false,
  ...over,
});

test("the requests are the ops jkb serve takes", () => {
  assert.deepEqual(workflowOps.agents(), { op: "workflow.agents" });
  assert.deepEqual(workflowOps.agent("x", true), { op: "workflow.agent", name: "x", packaged: true });
  assert.deepEqual(workflowOps.copy("x"), { op: "workflow.agent_copy", from: "x", packaged: false });
  assert.deepEqual(workflowOps.copy("x", { as: "y", packaged: true }), {
    op: "workflow.agent_copy",
    from: "x",
    packaged: true,
    as: "y",
  });
  assert.deepEqual(workflowOps.copy("x", { edit: { describe: "d" } }), {
    op: "workflow.agent_copy",
    from: "x",
    packaged: false,
    edit: { describe: "d" },
  });
  assert.deepEqual(workflowOps.set("x", { describe: "d" }), { op: "workflow.agent_set", name: "x", edit: { describe: "d" } });
  assert.deepEqual(workflowOps.graph(), { op: "workflow.graph" });
  assert.deepEqual(workflowOps.graph("coordinated"), { op: "workflow.graph", strategy: "coordinated" });
});

test("agents decode, and a malformed answer is a named failure", () => {
  const listed = decodeAgents(
    ok({
      result: "workflow_agents",
      agents: [agent({ packaged_version: undefined, based_on: undefined, packaged_base: undefined, matches_packaged: undefined })],
      roles: ["operator", "auditor"],
    }),
  );
  assert.equal(listed.ok, true);
  assert.equal(listed.value.agents[0].packaged_version, null);
  assert.equal(listed.value.agents[0].based_on, null);
  assert.equal(listed.value.agents[0].packaged_base, null);
  assert.equal(listed.value.agents[0].matches_packaged, false);
  // The roles are the server's, whatever they are: a role added there is offered here.
  assert.deepEqual(listed.value.roles, ["operator", "auditor"]);
  assert.equal(decodeAgents(ok({ result: "workflow_agents", agents: [agent()] })).ok, false, "no roles");
  assert.equal(decodeAgents(ok({ result: "workflow_agents", agents: [{ name: "x" }], roles: [] })).ok, false);
  assert.equal(decodeAgents(ok({ result: "strategies" })).ok, false);
  const one = decodeAgent(ok({ result: "workflow_agent", agent: agent(), wrote: true }));
  assert.equal(one.ok && one.value.wrote, true);
  assert.equal(decodeAgent(ok({ result: "workflow_agent", agent: agent({ permissions: { isolation: "vm", writes: "code" } }) })).ok, false);
  const refused = { ok: false, error: { code: "forbidden", message: "no" } };
  assert.deepEqual(decodeAgent(refused), refused);
});

test("a save sends only what changed", () => {
  const a = agent();
  assert.deepEqual(editBetween(a, draftOf(a)), {});
  const draft = { ...draftOf(a), template: "Build it.", permissions: { ...a.permissions, model: "haiku" } };
  assert.deepEqual(editBetween(a, draft), { template: "Build it.", permissions: { isolation: "worktree", model: "haiku", writes: "code" } });
  assert.deepEqual(editBetween(a, { ...draftOf(a), hands_off_to: ["x"] }), { hands_off_to: ["x"] });
});

test("a save is one request: a packaged template's is a copy carrying the edit", () => {
  const p = agent();
  // Unchanged, it is a plain copy; edited, the edit rides in the same op, so a refused edit leaves no copy.
  assert.deepEqual(saveRequest(p, draftOf(p)), { op: "workflow.agent_copy", from: "swarm-implementer", packaged: false });
  assert.deepEqual(saveRequest(p, { ...draftOf(p), template: "{{ repo }}" }), {
    op: "workflow.agent_copy",
    from: "swarm-implementer",
    packaged: false,
    edit: { template: "{{ repo }}" },
  });
  const c = agent({ source: "operator", overrides_packaged: true });
  assert.equal(saveRequest(c, draftOf(c)), undefined, "nothing to save");
  assert.deepEqual(saveRequest(c, { ...draftOf(c), describe: "d" }), {
    op: "workflow.agent_set",
    name: "swarm-implementer",
    edit: { describe: "d" },
  });
});

test("Contribute is offered only for a copy that differs from the packaged text", () => {
  assert.equal(canContribute(agent()), false, "a packaged template");
  assert.equal(canContribute(agent({ source: "operator", overrides_packaged: true, matches_packaged: true })), false, "reverted");
  assert.equal(canContribute(agent({ source: "operator", overrides_packaged: true })), true);
  assert.equal(canContribute(agent({ source: "operator", packaged_version: null, packaged_base: null })), true, "the operator's own");
});

test("names and placeholders follow jkb's rules", () => {
  assert.equal(isAgentName("strict-reviewer"), true);
  for (const bad of ["", "Strict", "1x", "a_b", "x".repeat(65)]) assert.equal(isAgentName(bad), false, bad);
  assert.deepEqual(placeholdersOf("{{b}} {{a}} {{b}} {{Not}}"), ["a", "b"]);
});

test("the agent graph is an agent set's agents and their hand-offs, fragments left out", () => {
  const agents = [
    agent({ name: "a", hands_off_to: ["b", "c"] }),
    agent({ name: "b", hands_off_to: ["c", "gone"] }),
    agent({ name: "c", hands_off_to: ["b"] }),
    agent({ name: "frag", fragment: true }),
    agent({ name: "other", workflow: "code-review" }),
  ];
  assert.deepEqual(agentSetsOf(agents), ["task-swarm", "code-review"]);
  const g = agentGraph(agents, "task-swarm");
  assert.deepEqual(
    g.nodes.map((n) => [n.id, n.layer, n.row]),
    [
      ["a", 0, 0],
      ["b", 1, 0],
      ["c", 1, 1],
    ],
  );
  assert.deepEqual(g.dangling, [{ from: "b", to: "gone" }]);
  // b -> c stays within a layer, c -> b runs back: both drawn as back edges.
  assert.deepEqual(
    g.links.map((l) => [l.from, l.to, l.back]),
    [
      ["a", "b", false],
      ["a", "c", false],
      ["b", "c", true],
      ["c", "b", true],
    ],
  );
  assert.equal(g.layers, 2);
  assert.equal(g.rows, 2);
});

test("a cycle with no root still draws every node", () => {
  const l = layered(["x", "y"], [{ from: "x", to: "y", label: "" }, { from: "y", to: "x", label: "" }], []);
  assert.deepEqual(
    l.nodes.map((n) => n.layer),
    [0, 1],
  );
});

const machine = {
  states: [
    { name: "design", initial: true, settled: false, awaits_input: false, next_role: "designer", next_step: "s" },
    { name: "implement", initial: false, settled: false, awaits_input: false, next_role: "implementer", next_step: "s" },
    { name: "landed", initial: false, settled: true, awaits_input: false, next_role: null, next_step: null },
  ],
  transitions: [
    { from: "design", event: "submit_design", to: "implement", reconciled: false, guarded: false, planned: false, roles: ["operator"], fired_by: "operator" },
    { from: "implement", event: "observed_landed", to: "landed", reconciled: true, guarded: true, planned: false, roles: [], fired_by: "observed" },
    { from: "implement", event: "reopen", to: "implement", reconciled: false, guarded: false, planned: false, roles: [], fired_by: "applied" },
    { from: "design", event: "override", to: null, reconciled: false, guarded: false, planned: false, roles: ["operator"], fired_by: "operator" },
  ],
};

test("a machine's drawing keeps its states and edges, self-loops and overrides left to be listed", () => {
  const d = machineDrawing(machine);
  assert.deepEqual(
    d.states.map((s) => s.name),
    ["design", "implement", "landed"],
  );
  assert.deepEqual(
    d.edges.map((x) => x.event),
    ["submit_design", "observed_landed"],
  );
  assert.deepEqual(
    d.listed.map((t) => t.event),
    ["reopen", "override"],
  );
});

const st = (name, over = {}) => ({ name, initial: false, settled: false, awaits_input: false, next_role: "implementer", next_step: "s", ...over });
const tr = (from, event, to, over = {}) => ({ from, event, to, reconciled: false, guarded: false, planned: false, roles: ["operator"], fired_by: "operator", ...over });
const obs = (from, event, to, over = {}) => tr(from, event, to, { reconciled: true, guarded: true, roles: [], fired_by: "observed", ...over });

// Shaped like `design-reviewed`: six live phases, each with an observed landing and cancelling.
const PHASES = ["design", "design_review", "implement", "review", "landable", "changes_requested"];
const sixPhase = {
  states: [
    ...PHASES.map((p, i) => st(p, { initial: i === 0 })),
    st("landed", { settled: true, next_role: null, next_step: null }),
    st("cancelled", { settled: true, next_role: null, next_step: null }),
  ],
  transitions: [
    tr("design", "submit_design", "design_review"),
    tr("design_review", "approve_design", "implement"),
    tr("implement", "submit_work", "review"),
    obs("review", "review_passed", "landable"),
    tr("review", "request_changes", "changes_requested"),
    tr("changes_requested", "submit_work", "review"),
    ...PHASES.map((p) => obs(p, "observed_landed", "landed", { guarded: p !== "landable" })),
    ...PHASES.map((p) => obs(p, "observed_cancelled", "cancelled")),
    ...PHASES.map((p) => tr(p, "override", null)),
    tr("implement", "note", "implement", { roles: [], fired_by: "applied" }),
  ],
};

test("observations from two or more phases collapse into one marker per target", () => {
  const d = machineDrawing(sixPhase);
  assert.deepEqual(
    d.markers.map((m) => [m.to, m.groups.map((g) => [g.event, g.rows.map((r) => r.from)])]),
    [
      ["landed", [["observed_landed", PHASES]]],
      ["cancelled", [["observed_cancelled", PHASES]]],
    ],
  );
  assert.equal(markerSources(d.markers[0]).length, 6, "every live phase is still counted, not hidden as 'from any'");
  assert.deepEqual(
    d.markers[0].groups[0].rows.map((r) => r.guarded),
    PHASES.map((p) => p !== "landable"),
    "a guarded and an unguarded row of one (to, event) share a group, each keeping its own flag",
  );
  assert.ok(d.edges.every((e) => e.event !== "observed_landed" && e.event !== "observed_cancelled"));
  assert.deepEqual(
    d.edges.map((x) => x.event),
    ["submit_design", "approve_design", "submit_work", "review_passed", "request_changes", "submit_work"],
    "the graph draws only the real phase-to-phase edges",
  );
  assert.ok(d.states.some((s) => s.name === "landed") && d.states.some((s) => s.name === "cancelled"), "a state only markers reach is still drawn");

  // Two collapsed events reaching one target: one marker, two groups.
  const two = {
    ...sixPhase,
    transitions: [...sixPhase.transitions, obs("design", "observed_merged", "landed"), obs("review", "observed_merged", "landed")],
  };
  const m = machineDrawing(two).markers.filter((x) => x.to === "landed");
  assert.equal(m.length, 1);
  assert.deepEqual(
    m[0].groups.map((g) => g.event),
    ["observed_landed", "observed_merged"],
  );
  assert.equal(markerSources(m[0]).length, 6, "N counts distinct source states, not rows");
});

test("a single-source observation stays an edge", () => {
  const d = machineDrawing(sixPhase);
  assert.ok(d.edges.some((e) => e.from === "review" && e.event === "review_passed" && e.to === "landable"));
  assert.ok(!d.markers.some((m) => m.to === "landable"));
  // One live phase collapses nothing.
  const one = {
    states: [st("implement", { initial: true }), st("landed", { settled: true })],
    transitions: [obs("implement", "observed_landed", "landed"), tr("implement", "override", null)],
  };
  const o = machineDrawing(one);
  assert.deepEqual(o.markers, []);
  assert.deepEqual(
    o.edges.map((e) => e.event),
    ["observed_landed"],
  );
  // The same event from two states, but not observed: arrows, never a marker.
  const fired = { ...one, states: [...one.states, st("review")], transitions: [tr("implement", "land", "landed"), tr("review", "land", "landed")] };
  assert.deepEqual(machineDrawing(fired).markers, []);
});

test("every row is drawn, collapsed or listed, exactly once", () => {
  for (const m of [machine, sixPhase]) {
    const d = machineDrawing(m);
    const seen = [...d.edges, ...d.markers.flatMap((x) => x.groups.flatMap((g) => g.rows)), ...d.listed];
    assert.equal(seen.length, m.transitions.length);
    assert.deepEqual(new Set(seen), new Set(m.transitions), "each row is the table's own object, none invented or lost");
    assert.equal(new Set(d.markers.map((x) => x.to)).size, d.markers.length, "one marker per target");
    assert.ok(d.listed.every((t) => t.to === null || t.to === t.from));
    assert.ok(d.edges.every((t) => t.to !== null && t.to !== t.from));
    assert.equal(d.states, m.states);
  }
});

test("the graph answer decodes, with absent optionals as null", () => {
  const g = decodeWorkflowGraph(
    ok({
      result: "workflow_graph",
      graph: { strategy: "autonomous", graph: "direct", describe: "d", spec: {}, workflow: machine, lifecycle: machine },
    }),
  );
  assert.equal(g.ok, true);
  assert.equal(g.value.task, null);
  assert.equal(g.value.workflow.transitions[3].to, null);
  assert.equal(g.value.lifecycle.transitions[2].fired_by, "applied", "who fires a row is jkb's word, not redecided here");
  const unlabelled = { ...machine, transitions: machine.transitions.map(({ fired_by: _, ...t }) => t) };
  assert.equal(
    decodeWorkflowGraph(ok({ result: "workflow_graph", graph: { strategy: "x", graph: "g", describe: "d", workflow: unlabelled, lifecycle: machine } })).ok,
    false,
  );
  assert.equal(decodeWorkflowGraph(ok({ result: "workflow_graph", graph: { strategy: "x" } })).ok, false);
});

test("a state's rows are what leaves it, what arrives, and what is not drawn", () => {
  const d = machineDrawing(sixPhase);
  const ev = (rows) => rows.map((t) => `${t.from}|${t.event}|${t.to}`);
  const implement = stateRows(d, "implement");
  assert.deepEqual(ev(implement.leaves), ["implement|submit_work|review"]);
  assert.deepEqual(ev(implement.arrives), ["design_review|approve_design|implement"]);
  assert.deepEqual(ev(implement.listed), ["implement|override|null", "implement|note|implement"], "its override and its self-loop, in table order");
  // A marker's rows arrive at its target, after the drawn edges into it.
  const landed = stateRows(d, "landed");
  assert.deepEqual(landed.leaves, []);
  assert.deepEqual(ev(landed.arrives), PHASES.map((p) => `${p}|observed_landed|landed`));
  assert.deepEqual(landed.listed, []);
  // Each row is the table's own object.
  assert.ok(implement.leaves.every((t) => sixPhase.transitions.includes(t)));
});

test("every row of every machine is reachable from some state's panel", () => {
  const fixture = JSON.parse(fs.readFileSync(path.join(import.meta.dirname, "..", "..", "app", "test", "fixtures", "workflow-graphs.json"), "utf8"));
  const machines = Object.values(fixture).flatMap((g) => [g.workflow, g.lifecycle]);
  assert.equal(machines.length, 6);
  for (const m of [...machines, machine, sixPhase]) {
    const d = machineDrawing(m);
    const reached = new Set(
      m.states.flatMap((s) => {
        const r = stateRows(d, s.name);
        return [...r.leaves, ...r.arrives, ...r.listed];
      }),
    );
    for (const t of m.transitions) assert.ok(reached.has(t), `${t.from} --${t.event}--> ${t.to} is in no state's panel`);
  }
});

test("a selection survives a strategy switch only if it still names something drawn", () => {
  const d = machineDrawing(sixPhase);
  assert.equal(selectionIn(d, { kind: "state", machine: "workflow", name: "review" }), true);
  assert.equal(selectionIn(d, { kind: "state", machine: "workflow", name: "systemic_review" }), false);
  assert.equal(selectionIn(d, { kind: "edge", machine: "workflow", from: "review", event: "review_passed" }), true);
  // A row that is listed or collapsed is not an edge of the picture.
  assert.equal(selectionIn(d, { kind: "edge", machine: "workflow", from: "implement", event: "override" }), false);
  assert.equal(selectionIn(d, { kind: "edge", machine: "workflow", from: "implement", event: "observed_landed" }), false);
  assert.equal(selectionIn(d, { kind: "marker", machine: "workflow", to: "landed" }), true);
  assert.equal(selectionIn(d, { kind: "marker", machine: "workflow", to: "landable" }), false);
});

test("two selections are the same only for the same part of the same machine", () => {
  const s = { kind: "state", machine: "workflow", name: "review" };
  assert.equal(sameSelection(s, { name: "review", machine: "workflow", kind: "state" }), true, "key order does not matter");
  assert.equal(sameSelection(s, { ...s, machine: "lifecycle" }), false);
  assert.equal(sameSelection(s, { kind: "marker", machine: "workflow", to: "review" }), false);
  assert.equal(sameSelection({ kind: "edge", machine: "workflow", from: "a", event: "x" }, { kind: "edge", machine: "workflow", from: "a", event: "y" }), false);
  assert.equal(sameSelection(undefined, undefined), true);
  assert.equal(sameSelection(s, undefined), false);
});

test("a role's events are those the strategy lets it fire, once each", () => {
  assert.deepEqual(eventsFiredBy(sixPhase, "operator"), ["submit_design", "approve_design", "submit_work", "request_changes", "override"]);
  assert.deepEqual(eventsFiredBy(sixPhase, "designer"), []);
});

test("a task's moves count per row, an override's destinations summed", () => {
  const taken = takenByRow([
    { from: "review", event: "override", to: "design", count: 1 },
    { from: "review", event: "override", to: "implement", count: 2 },
    { from: "implement", event: "submit_work", to: "review", count: 2 },
  ]);
  assert.equal(taken.get(rowKey("review", "override")), 3);
  assert.equal(taken.get(rowKey("implement", "submit_work")), 2);
  assert.equal(taken.size, 2);
});

test("a row's guard and command are jkb's words, absent ones null; every role's op classes come with the graph", () => {
  const worded = {
    ...machine,
    transitions: machine.transitions.map((t, i) => (i === 1 ? { ...t, guard: "a landing is recorded", command: "jkb workflow observe <uid>" } : t)),
  };
  const g = decodeWorkflowGraph(
    ok({ result: "workflow_graph", graph: { strategy: "x", graph: "g", describe: "d", workflow: worded, lifecycle: machine, role_ops: { operator: ["read"] } } }),
  );
  assert.equal(g.ok, true);
  assert.equal(g.value.workflow.transitions[1].guard, "a landing is recorded");
  assert.equal(g.value.workflow.transitions[1].command, "jkb workflow observe <uid>");
  assert.equal(g.value.workflow.transitions[0].guard, null, "an older jkb's row has no guard words");
  assert.equal(g.value.workflow.transitions[0].command, null);
  assert.deepEqual(g.value.role_ops, { operator: ["read"] });
  const older = decodeWorkflowGraph(ok({ result: "workflow_graph", graph: { strategy: "x", graph: "g", describe: "d", workflow: machine, lifecycle: machine } }));
  assert.deepEqual(older.value.role_ops, {});
  const bad = (graph) => decodeWorkflowGraph(ok({ result: "workflow_graph", graph: { strategy: "x", graph: "g", describe: "d", workflow: machine, lifecycle: machine, ...graph } })).ok;
  assert.equal(bad({ role_ops: { operator: "read" } }), false);
  assert.equal(bad({ workflow: { ...machine, transitions: [{ ...machine.transitions[0], guard: 3 }] } }), false);
});
