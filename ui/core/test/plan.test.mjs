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
  pickOf,
  playPins,
  rebaseDraft,
  saveDraft,
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
  // A plan's Play with nothing picked reports no strategy, not the default.
  const unpicked = { ...prompt, strategy: null };
  assert.deepEqual(decodeWorkPrompt(ok({ result: "design_work_prompt", prompt: unpicked })), ok(unpicked));

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
  assert.deepEqual(planOps.edit("task:a", "new", false, "old"), {
    op: "task.edit",
    uid: "task:a",
    text: "new",
    append: false,
    expected: "old",
  });
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
  const listing = {
    default: "design-reviewed",
    strategies: [
      { name: "design-reviewed", preset: true, describe: "" },
      { name: "mine@3", preset: false, describe: "" },
    ],
  };
  const preset = pickOf(listing, "design-reviewed");
  // An explicit pick equal to the default is a choice: it pins the open tasks only defaulting to
  // it, freezing them there, and leaves a task already pinned to it alone.
  assert.deepEqual(playPins([...tasks, task({ uid: "task:on-it", strategy: "design-reviewed" })], preset), [
    { op: "workflow.set", uid: "task:unpinned", strategy: "design-reviewed" },
    { op: "workflow.set", uid: "task:also", strategy: "design-reviewed" },
    { op: "workflow.set", uid: "task:pinned", strategy: "design-reviewed" },
  ]);
  // A definition is one identity: pinned by its bare name, compared by what that name resolves to
  // now (its newest listed version), so a task on an older version is repinned and one on the
  // newest is left alone — whichever spelling the pick was made with.
  for (const picked of ["mine", "mine@2", "mine@3"]) {
    assert.deepEqual(pickOf(listing, picked), { name: "mine", listed: "mine@3" });
  }
  const mine = pickOf(listing, "mine");
  assert.deepEqual(playPins([task({ uid: "task:old", strategy: "mine@2" }), task({ uid: "task:new", strategy: "mine@3" })], mine), [
    { op: "workflow.set", uid: "task:old", strategy: "mine" },
  ]);
  assert.equal(pickOf(listing, "gone"), undefined, "a pick no longer listed resolves to nothing");
  assert.equal(pickOf(undefined, "mine"), undefined);
  // The plan's prompt request names a strategy only when one was picked.
  assert.deepEqual(planOps.play("plan:x", undefined), { op: "design.prompt", ask: { kind: "play", plan: "plan:x" } });
});

test("a Save names the text its draft started from; a stale one is told apart and keeps the draft", async () => {
  const calls = [];
  let current = "Build it";
  const op = async (request) => {
    calls.push(request);
    if (request.expected !== undefined && request.expected !== current) {
      return { ok: false, error: { code: "stale", message: "the item's text changed" } };
    }
    current = request.text;
    return ok({ result: "edited", file_backed: false });
  };
  const draft = { text: "Build it well", base: "Build it" };
  // The Play session's Claude appends a note after the draft opened.
  current = "Build it\n\nblocked on X";
  assert.deepEqual(await saveDraft(op, "task:a", draft), { kind: "stale", message: "the item's text changed" });
  assert.equal(current, "Build it\n\nblocked on X", "the note survives");
  assert.deepEqual(calls[0], { op: "task.edit", uid: "task:a", text: "Build it well", append: false, expected: "Build it" });
  // Kept over the text the operator was shown, the next Save replaces exactly that.
  const kept = rebaseDraft(draft, current);
  assert.deepEqual(kept, { text: "Build it well", base: "Build it\n\nblocked on X" });
  assert.deepEqual(await saveDraft(op, "task:a", kept), { kind: "saved" });
  assert.equal(current, "Build it well");
  const refused = async () => ({ ok: false, error: { code: "invalid", message: "would not round-trip" } });
  assert.deepEqual(await saveDraft(refused, "task:a", kept), { kind: "failed", message: "would not round-trip" });
});
