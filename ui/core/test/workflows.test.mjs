//! The Workflows tab's data as `@jkb/core` reads it (D53.7): the answers' shapes, the requests, what
//! a save sends, and the two layouts. Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import {
  agentGraph,
  decodeAgent,
  decodeAgents,
  decodeWorkflowGraph,
  draftOf,
  editBetween,
  isAgentName,
  layered,
  machineLayout,
  placeholdersOf,
  workflowOps,
  workflowsOf,
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
  assert.deepEqual(workflowOps.set("x", { describe: "d" }), { op: "workflow.agent_set", name: "x", edit: { describe: "d" } });
  assert.deepEqual(workflowOps.graph(), { op: "workflow.graph" });
  assert.deepEqual(workflowOps.graph("coordinated"), { op: "workflow.graph", strategy: "coordinated" });
});

test("agents decode, and a malformed answer is a named failure", () => {
  const listed = decodeAgents(ok({ result: "workflow_agents", agents: [agent({ packaged_version: undefined, based_on: undefined })] }));
  assert.equal(listed.ok, true);
  assert.equal(listed.value[0].packaged_version, null);
  assert.equal(listed.value[0].based_on, null);
  assert.equal(decodeAgents(ok({ result: "workflow_agents", agents: [{ name: "x" }] })).ok, false);
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

test("names and placeholders follow jkb's rules", () => {
  assert.equal(isAgentName("strict-reviewer"), true);
  for (const bad of ["", "Strict", "1x", "a_b", "x".repeat(65)]) assert.equal(isAgentName(bad), false, bad);
  assert.deepEqual(placeholdersOf("{{b}} {{a}} {{b}} {{Not}}"), ["a", "b"]);
});

test("the agent graph is a workflow's agents and their hand-offs, fragments left out", () => {
  const agents = [
    agent({ name: "a", hands_off_to: ["b", "c"] }),
    agent({ name: "b", hands_off_to: ["c", "gone"] }),
    agent({ name: "c", hands_off_to: ["b"] }),
    agent({ name: "frag", fragment: true }),
    agent({ name: "other", workflow: "code-review" }),
  ];
  assert.deepEqual(workflowsOf(agents), ["task-swarm", "code-review"]);
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
    { from: "design", event: "submit_design", to: "implement", reconciled: false, guarded: false, planned: false, roles: ["operator"] },
    { from: "implement", event: "observed_landed", to: "landed", reconciled: true, guarded: true, planned: false, roles: [] },
    { from: "implement", event: "reopen", to: "implement", reconciled: false, guarded: false, planned: false, roles: [] },
    { from: "design", event: "override", to: null, reconciled: false, guarded: false, planned: false, roles: ["operator"] },
  ],
};

test("a machine is laid out from its table, self-loops and overrides left to be listed", () => {
  const l = machineLayout(machine);
  assert.deepEqual(
    l.nodes.map((n) => [n.id, n.layer]),
    [
      ["design", 0],
      ["implement", 1],
      ["landed", 2],
    ],
  );
  assert.deepEqual(
    l.links.map((x) => x.label),
    ["submit_design", "observed_landed"],
  );
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
  assert.equal(decodeWorkflowGraph(ok({ result: "workflow_graph", graph: { strategy: "x" } })).ok, false);
});
