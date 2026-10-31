//! The Workflows tab's legend (`legend.ts`): the terms it defines, each on one line, and what it says
//! about the drawn strategy against the default. Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import { WORKFLOW_TERMS, decodeWorkflowGraph, strategyChanges } from "../dist/index.js";

/** The longest definition the strip shows on one line at the tab's usual width. */
const ONE_LINE = 140;

test("the legend defines role, agent, agent set, graph and strategy, each in one line", () => {
  assert.deepEqual(
    WORKFLOW_TERMS.map((t) => t.term),
    ["Role", "Agent", "Agent set", "Graph", "Strategy"],
  );
  for (const { term, line } of WORKFLOW_TERMS) {
    assert.ok(line.trim().length > 0, `${term} has a definition`);
    assert.ok(!/[\r\n]/.test(line), `${term}'s definition has no line break`);
    assert.ok(line.length <= ONE_LINE, `${term}'s definition fits on one line (${line.length} > ${ONE_LINE})`);
  }
});

const machine = { states: [], transitions: [] };
const graph = (over) => {
  const g = decodeWorkflowGraph({
    ok: true,
    value: { result: "workflow_graph", graph: { strategy: "autonomous", graph: "direct", describe: "d", workflow: machine, lifecycle: machine, ...over } },
  });
  assert.equal(g.ok, true, JSON.stringify(g));
  return g.value;
};

test("an older daemon's graph has unknown changes, and the legend hides them", () => {
  const g = graph({});
  assert.equal(g.default, null);
  assert.equal(g.changes_from_default, null);
  assert.deepEqual(strategyChanges(g), { kind: "unknown" });
  // One field without the other is not an answer the legend can word.
  assert.deepEqual(strategyChanges(graph({ default: "design-reviewed" })), { kind: "unknown" });
  assert.deepEqual(strategyChanges(graph({ changes_from_default: [] })), { kind: "unknown" });
});

test("a malformed changes list is refused", () => {
  const g = decodeWorkflowGraph({
    ok: true,
    value: {
      result: "workflow_graph",
      graph: { strategy: "x", graph: "g", describe: "d", workflow: machine, lifecycle: machine, default: "d", changes_from_default: [3] },
    },
  });
  assert.equal(g.ok, false);
});

test("the legend says the drawn strategy is the default, the same as it, or what it changes", () => {
  const lines = ["graph: direct (default: design-gate)", "lands: operator, coordinator (default: operator)"];
  assert.deepEqual(strategyChanges(graph({ default: "design-reviewed", changes_from_default: lines })), {
    kind: "changes",
    label: "autonomous",
    defaultLabel: "design-reviewed",
    lines,
  });
  assert.deepEqual(strategyChanges(graph({ strategy: "design-reviewed", default: "design-reviewed", changes_from_default: [] })), {
    kind: "default",
    label: "design-reviewed",
  });
  // An operator definition is labelled as the pickers label it, and matched by name.
  assert.deepEqual(strategyChanges(graph({ strategy: "default@2", default: "default@2", changes_from_default: [] })), {
    kind: "default",
    label: "default · v2",
  });
  assert.deepEqual(strategyChanges(graph({ strategy: "mine@1", default: "default@2", changes_from_default: [] })), {
    kind: "same",
    label: "mine · v1",
    defaultLabel: "default · v2",
  });
});
