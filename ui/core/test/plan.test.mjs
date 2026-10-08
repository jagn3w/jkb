//! Execution plans as `@jkb/core` reads them (D53.6): the answers' shapes, the requests, the archive
//! split and which tasks a Play pins first. Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import {
  decodePlanList,
  decodeStrategies,
  decodeTaskDetail,
  decodeWorkPrompt,
  isTerminal,
  partitionPlans,
  planOps,
  planTasks,
  strategyName,
  playPins,
  tasksToPin,
} from "../dist/index.js";

const ok = (value) => ({ ok: true, value });

const task = (over = {}) => ({
  uid: "task:a",
  title: "Build it",
  status: "open",
  priority: null,
  depth: 0,
  claimed_by: null,
  strategy: "default:design-reviewed",
  ...over,
});

const plan = (over = {}) => ({
  uid: "plan:x",
  title: "First cut",
  design: "design:d",
  archived: false,
  steps: [{ uid: "step:s", text: "scaffold", spans: [{ uid: "span:a", state: "STAGED" }], tasks: [task()] }],
  ...over,
});

test("a plan listing decodes by its tag, and a malformed one is an internal failure", () => {
  const list = { uid: "design:d", plans: [plan()], hidden: 0, tasks: [task({ uid: "task:one-off" })] };
  assert.deepEqual(decodePlanList(ok({ result: "design_plans", list })), ok(list));
  for (const bad of [
    { ...list, plans: [{ ...plan(), archived: "no" }] },
    { ...list, tasks: [{ ...task(), depth: "0" }] },
    { ...list, hidden: undefined },
  ]) {
    const r = decodePlanList(ok({ result: "design_plans", list: bad }));
    assert.equal(r.ok, false);
    assert.equal(r.error.code, "internal");
  }
  const refused = { ok: false, error: { code: "not_found", message: "no design" } };
  assert.equal(decodePlanList(refused), refused);
});

test("a work prompt, the strategies and a task decode", () => {
  const prompt = { kind: "play", uid: "plan:x", title: "First cut", design: "design:d", strategy: "coordinated", prompt: "p" };
  assert.deepEqual(decodeWorkPrompt(ok({ result: "design_work_prompt", prompt })), ok(prompt));
  assert.equal(decodeWorkPrompt(ok({ result: "design_work_prompt", prompt: { ...prompt, prompt: "" } })).ok, false);
  assert.equal(decodeWorkPrompt(ok({ result: "design_prompt", prompt })).ok, false, "a Discuss answer is not a Play one");

  const strategies = { result: "strategies", default: "design-reviewed", strategies: [{ name: "coordinated", preset: true, describe: "d", spec: {} }] };
  assert.equal(decodeStrategies(ok(strategies)).value.default, "design-reviewed");
  assert.equal(decodeStrategies(ok({ ...strategies, strategies: [{ name: 1 }] })).ok, false);

  const shown = {
    result: "task",
    task: {
      item: { id: 1, uid: "task:a", kind: "task", status: "open", priority: 2, due: null, namespace: "tasks/x", content: "Build it\n\nnotes", tags: [] },
      transitions: [{ at: "t", event: "start", to: "in_progress", branch: null, onto: null, pr: null }],
      subtasks: [],
    },
  };
  const detail = decodeTaskDetail(ok(shown));
  assert.equal(detail.ok, true);
  assert.equal(detail.value.content, "Build it\n\nnotes");
  assert.equal(detail.value.transitions.length, 1);
  assert.equal(decodeTaskDetail(ok({ ...shown, task: { ...shown.task, transitions: [{}] } })).ok, false);
  assert.equal(decodeTaskDetail(ok({ result: "item", item: {} })).ok, false);
});

test("requests are the ops' own shapes, a definition resolved by its name", () => {
  assert.deepEqual(planOps.plans("design:d"), { op: "design.plans", uid: "design:d", all: true });
  assert.deepEqual(planOps.play("plan:x"), { op: "design.prompt", ask: { kind: "play", plan: "plan:x" } });
  assert.deepEqual(planOps.play("plan:x", "mine@3"), {
    op: "design.prompt",
    ask: { kind: "play", plan: "plan:x", strategy: "mine" },
  });
  assert.deepEqual(planOps.playTask("task:a"), { op: "design.prompt", ask: { kind: "task", uid: "task:a" } });
  assert.deepEqual(planOps.pin("task:a", "coordinated"), { op: "workflow.set", uid: "task:a", strategy: "coordinated" });
  assert.deepEqual(planOps.edit("task:a", "note", true), { op: "task.edit", uid: "task:a", text: "note", append: true });
  assert.equal(strategyName("mine@12"), "mine");
  assert.equal(strategyName("autonomous"), "autonomous");
});

test("archived plans go to the drawer, and a Play pins only open tasks not already on its strategy", () => {
  const live = plan();
  const old = plan({ uid: "plan:old", archived: true });
  assert.deepEqual(partitionPlans([old, live]), { live: [live], archived: [old] });

  const tasks = [
    task({ uid: "task:open" }),
    task({ uid: "task:pinned", strategy: "coordinated" }),
    task({ uid: "task:done", status: "done" }),
    task({ uid: "task:cancelled", status: "cancelled" }),
    task({ uid: "task:review", status: "needs_review", strategy: "autonomous" }),
  ];
  assert.deepEqual(
    tasksToPin(tasks, "coordinated").map((t) => t.uid),
    ["task:open", "task:review"],
  );
  assert.equal(isTerminal("done"), true);
  assert.equal(isTerminal("in_progress"), false);
  assert.equal(isTerminal(null), false);
  assert.deepEqual(
    planTasks(plan({ steps: [...live.steps, { uid: "step:t", text: "t", spans: [], tasks: [task({ uid: "task:b" })] }] })).map((t) => t.uid),
    ["task:a", "task:b"],
  );
});

test("a Play with no strategy picked pins nothing, so unpinned tasks keep following the default", () => {
  const tasks = [
    task({ uid: "task:unpinned" }),
    task({ uid: "task:also", status: "in_progress" }),
    task({ uid: "task:pinned", strategy: "coordinated" }),
  ];
  // The picker shows the default (`design-reviewed`) but the operator never touched it.
  assert.deepEqual(playPins(tasks, undefined), []);
  // An explicit pick equal to the default is a choice: it pins the open tasks only defaulting to
  // it, freezing them there, and leaves a task already pinned to it alone.
  assert.deepEqual(playPins([...tasks, task({ uid: "task:on-it", strategy: "design-reviewed" })], "design-reviewed"), [
    { op: "workflow.set", uid: "task:unpinned", strategy: "design-reviewed" },
    { op: "workflow.set", uid: "task:also", strategy: "design-reviewed" },
    { op: "workflow.set", uid: "task:pinned", strategy: "design-reviewed" },
  ]);
  // A listed definition is pinned by its name.
  assert.deepEqual(playPins([task()], "mine@2"), [{ op: "workflow.set", uid: "task:a", strategy: "mine" }]);
  // The plan's prompt request names a strategy only when one was picked.
  assert.deepEqual(planOps.play("plan:x", undefined), { op: "design.prompt", ask: { kind: "play", plan: "plan:x" } });
});
