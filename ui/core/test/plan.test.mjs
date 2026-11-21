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
  strategyLabel,
  strategyName,
  strategyOption,
  strategyParts,
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
  const defined = { name: "factory@1", preset: false, describe: "d", spec: {}, defined_at: "2026-10-16T06:50:14.473Z" };
  const listed = decodeStrategies(ok({ ...strategies, strategies: [...strategies.strategies, defined] }));
  assert.equal(listed.value.strategies[1].defined_at, "2026-10-16T06:50:14.473Z");
  assert.equal(decodeStrategies(ok({ ...strategies, strategies: [{ ...defined, defined_at: 5 }] })).ok, false);
  // Each strategy carries its graph's sentence; an older daemon sends none.
  assert.equal(listed.value.strategies[0].graph, undefined);
  const graphed = decodeStrategies(ok({ ...strategies, strategies: [{ ...defined, graph: "A graph." }] }));
  assert.equal(graphed.value.strategies[0].graph, "A graph.");
  assert.equal(decodeStrategies(ok({ ...strategies, strategies: [{ ...defined, graph: 5 }] })).ok, false);

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

test("a strategy identity comes apart into name, version and whether it follows the default", () => {
  const parts = (name, version, followsDefault = false) => ({ name, version, followsDefault });
  assert.deepEqual(strategyParts("design-reviewed"), parts("design-reviewed", null));
  assert.deepEqual(strategyParts("factory@1"), parts("factory", 1));
  assert.deepEqual(strategyParts("default:factory@3"), parts("factory", 3, true));
  assert.deepEqual(strategyParts("default:design-reviewed"), parts("design-reviewed", null, true));
  assert.deepEqual(strategyParts("a-b_c@12"), parts("a-b_c", 12));
  // The operator's own `default` definition, listed and followed.
  assert.deepEqual(strategyParts("default@2"), parts("default", 2));
  assert.deepEqual(strategyParts("default:default@2"), parts("default", 2, true));
  assert.equal(strategyName("default:factory@3"), "factory");

  assert.equal(strategyLabel("factory@1"), "factory · v1");
  assert.equal(strategyLabel("design-reviewed"), "design-reviewed");
  assert.equal(strategyLabel("default:factory@1"), "factory · v1 (following default)");
  assert.equal(strategyLabel("default:design-reviewed"), "design-reviewed (following default)");
});

test("a picker option sets an operator definition apart and says when its version was defined", () => {
  const at = (iso) => `at ${iso}`;
  const preset = { name: "design-reviewed", preset: true, describe: "d" };
  const mine = { name: "factory@1", preset: false, describe: "d", defined_at: "2026-10-16T06:50:14.473Z" };
  assert.equal(strategyOption(preset, "design-reviewed", at), "design-reviewed (default)");
  assert.equal(strategyOption(preset, "factory@1", at), "design-reviewed");
  assert.equal(strategyOption(mine, "design-reviewed", at), "factory · v1 — operator-defined at 2026-10-16T06:50:14.473Z");
  assert.equal(strategyOption(mine, "factory@1", at), "factory · v1 — operator-defined at 2026-10-16T06:50:14.473Z (default)");
  // From a daemon that predates the field: still marked, with no time.
  assert.equal(strategyOption({ ...mine, defined_at: undefined }, "x", at), "factory · v1 — operator-defined");
  // The default formatter keeps text that is not a time as it is.
  assert.equal(strategyOption({ ...mine, defined_at: "whenever" }, "x"), "factory · v1 — operator-defined whenever");
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

test("a finished parent of an open subtask stays in the list, muted, so the subtask keeps its place", async () => {
  const { visibleTasks } = await import("../dist/index.js");
  const t = (uid, status, depth) => ({ uid, title: uid, status, priority: null, depth, claimed_by: null, strategy: "default:x" });
  const rows = [t("A", "in_progress", 0), t("B", "done", 0), t("B1", "open", 1), t("C", "done", 0), t("C1", "done", 1)];
  const open = (x) => x.status !== "done";
  assert.deepEqual(
    visibleTasks(rows, open).map((l) => `${l.task.uid}${l.muted ? "~" : ""}`),
    ["A", "B~", "B1"],
  );
  assert.deepEqual(visibleTasks(rows, () => true).length, 5);
  assert.deepEqual(visibleTasks([t("X", "done", 0), t("X1", "done", 1), t("X2", "open", 2)], open).map((l) => `${l.task.uid}${l.muted ? "~" : ""}`), ["X~", "X1~", "X2"]);
});

// ---- review findings under the task they were found in ----------------------------------------

const ref = (uid) => ({ uid, title: `title of ${uid}` });
const row = (uid, status, depth, foundIn) => ({
  uid,
  title: uid,
  status,
  priority: null,
  depth,
  claimed_by: null,
  strategy: "default:x",
  ...(foundIn === undefined ? {} : { found_in: foundIn.map(ref) }),
});
const drawn = (lines) =>
  lines.map((l) => `${l.task.uid}${l.muted ? "~" : ""}${l.findingOf ? `<${l.findingOf}` : ""}${l.from ? `(from ${l.from})` : ""}`);

test("nestFindings: a finding goes right after its origin's subtree, marked, with no depth of its own", async () => {
  const { nestFindings, visibleTasks } = await import("../dist/index.js");
  const rows = [row("A", "open", 0), row("A1", "open", 1), row("B", "open", 0), row("F", "open", 0, ["A"])];
  const lines = nestFindings(visibleTasks(rows, () => true));
  assert.deepEqual(drawn(lines), ["A", "A1", "F<A", "B"]);
  assert.deepEqual(
    lines.map((l) => l.task.depth),
    [0, 1, 0, 0],
    "subtask depths are untouched, and the finding keeps its own",
  );
});

test("nestFindings: a finding whose origin is elsewhere stays in place, naming its origin", async () => {
  const { nestFindings, visibleTasks } = await import("../dist/index.js");
  const rows = [row("A", "open", 0), row("F", "open", 0, ["Z"]), row("B", "open", 0)];
  assert.deepEqual(drawn(nestFindings(visibleTasks(rows, () => true))), ["A", "F(from title of Z)", "B"]);
});

test("nestFindings: with two origins in the group, the first listed wins", async () => {
  const { nestFindings, visibleTasks } = await import("../dist/index.js");
  const rows = [row("A", "open", 0), row("B", "open", 0), row("F", "open", 0, ["B", "A"]), row("G", "open", 0, ["B"])];
  assert.deepEqual(drawn(nestFindings(visibleTasks(rows, () => true))), ["A", "F<A", "B", "G<B"]);
});

test("visibleTasks keeps a finished origin muted for an open finding of it", async () => {
  const { nestFindings, visibleTasks } = await import("../dist/index.js");
  const rows = [
    row("A", "done", 0),
    row("B", "done", 0),
    row("F", "open", 0, ["A"]),
    row("G", "done", 0, ["B"]),
  ];
  const open = (t) => t.status !== "done";
  assert.deepEqual(drawn(nestFindings(visibleTasks(rows, open))), ["A~", "F<A"], "B is dropped: its finding is done too");
});

test("nestFindings: subtasks keep their depth, and a finding of a subtask follows the subtask's subtree", async () => {
  const { nestFindings, visibleTasks } = await import("../dist/index.js");
  const rows = [
    row("P", "open", 0),
    row("S", "open", 1),
    row("S1", "open", 2),
    row("T", "open", 1),
    row("F", "open", 0, ["S"]),
  ];
  const lines = nestFindings(visibleTasks(rows, () => true));
  assert.deepEqual(drawn(lines), ["P", "S", "S1", "F<S", "T"]);
  assert.deepEqual(lines.map((l) => l.task.depth), [0, 1, 2, 0, 1]);
});

test("nestFindings: two findings of each other move neither, and nothing is lost", async () => {
  const { nestFindings, visibleTasks } = await import("../dist/index.js");
  const rows = [row("F", "open", 0, ["G"]), row("G", "open", 0, ["F"]), row("A", "open", 0)];
  const lines = nestFindings(visibleTasks(rows, () => true));
  assert.deepEqual(lines.map((l) => l.task.uid).sort(), ["A", "F", "G"]);
});

test("nestFindings: a loop through a moved finding's subtask loses no line", async () => {
  const { nestFindings, visibleTasks } = await import("../dist/index.js");
  // F is found in X and carries its subtask F1; X is found in F1. The origin chain F→X→F1 never
  // revisits a member, so only block membership closes the loop.
  const rows = [row("F", "open", 0, ["X"]), row("F1", "open", 1), row("X", "open", 0, ["F1"]), row("A", "open", 0)];
  const lines = nestFindings(visibleTasks(rows, () => true));
  assert.deepEqual(lines.map((l) => l.task.uid).sort(), ["A", "F", "F1", "X"]);
  assert.equal(lines.length, 4, "no line drawn twice");
  // Each line of the loop is drawn where it was listed, unmoved and noted with its origin — not
  // after the group's last line, where F would read as A's subtask.
  assert.deepEqual(drawn(lines), ["F(from title of X)", "F1", "X(from title of F1)", "A"]);

  // Under a parent, the loop still keeps its place; a finding hanging off it (H, found in F1) is not
  // on the loop and is drawn under its origin as usual.
  const nested = [
    row("P", "open", 0),
    row("F", "open", 1, ["X"]),
    row("F1", "open", 2),
    row("X", "open", 0, ["F1"]),
    row("A", "open", 0),
    row("H", "open", 0, ["F1"]),
  ];
  assert.deepEqual(drawn(nestFindings(visibleTasks(nested, () => true))), [
    "P",
    "F(from title of X)",
    "F1",
    "H<F1",
    "X(from title of F1)",
    "A",
  ]);
});

test("design.plans and task.show decode found_in and findings, and an older daemon's answer as none", async () => {
  const { decodePlanList, decodeTaskDetail } = await import("../dist/index.js");
  const list = (tasks) => ok({ result: "design_plans", list: { uid: "design:d", plans: [], hidden: 0, tasks } });
  const withRefs = decodePlanList(list([task({ found_in: [{ uid: "task:o", title: "Origin" }] })]));
  assert.ok(withRefs.ok);
  assert.deepEqual(withRefs.value.tasks[0].found_in, [{ uid: "task:o", title: "Origin" }]);
  const old = decodePlanList(list([task()]));
  assert.ok(old.ok);
  assert.equal(old.value.tasks[0].found_in, undefined);
  assert.equal(decodePlanList(list([task({ found_in: [{ uid: 1 }] })])).ok, false);

  const item = { uid: "task:t", status: "open", priority: null, namespace: null, content: "T", tags: [] };
  const finding = {
    uid: "task:f",
    title: "F",
    status: "open",
    priority: 1,
    round: "repos/jkb/codereviews/r1",
    must_fix: true,
    step: "step:inbox@jkb",
    step_title: "Open work › Inbox",
    design: "design:open-work@jkb",
  };
  const shown = decodeTaskDetail(
    ok({
      result: "task",
      task: {
        item,
        transitions: [],
        subtasks: [],
        review: { namespaces: ["repos/jkb/codereviews/r1"] },
        findings: [finding],
        found_in: [{ uid: "task:o", title: "Origin" }],
      },
    }),
  );
  assert.ok(shown.ok);
  assert.deepEqual(shown.value.findings, [finding]);
  assert.deepEqual(shown.value.found_in, [{ uid: "task:o", title: "Origin" }]);
  assert.deepEqual(shown.value.review_namespaces, ["repos/jkb/codereviews/r1"]);
  const older = decodeTaskDetail(ok({ result: "task", task: { item, transitions: [], subtasks: [] } }));
  assert.ok(older.ok);
  assert.deepEqual([older.value.findings, older.value.found_in, older.value.review_namespaces], [[], [], []]);
  const bad = decodeTaskDetail(ok({ result: "task", task: { item, transitions: [], findings: [{ uid: "x" }] } }));
  assert.equal(bad.ok, false);
});

test("task.unplanned decodes by repo, a missing repo or namespace as null, and is refused malformed", async () => {
  const { decodeUnplanned, unplannedOps, unplannedOf, inboxStep, planOps } = await import("../dist/index.js");
  assert.deepEqual(unplannedOps.list(), { op: "task.unplanned" });
  assert.deepEqual(unplannedOps.list("jkb"), { op: "task.unplanned", repo: "jkb" });
  assert.deepEqual(planOps.move("task:a", inboxStep("jkb")), { op: "task.move", uid: "task:a", under: "step:inbox@jkb" });
  const answer = decodeUnplanned(
    ok({
      result: "unplanned",
      count: 3,
      repos: [
        { repo: "jkb", count: 2, tasks: [{ task: task(), namespace: "tasks/repos/jkb/inbox" }] },
        { repo: null, count: 1, tasks: [{ task: task({ uid: "task:b" }) }] },
      ],
    }),
  );
  assert.ok(answer.ok);
  assert.equal(answer.value.count, 3);
  assert.equal(unplannedOf(answer.value, "jkb").count, 2);
  assert.equal(unplannedOf(answer.value, null).tasks[0].namespace, null);
  assert.equal(unplannedOf(answer.value, "web"), undefined);
  assert.equal(decodeUnplanned(ok({ result: "unplanned", count: -1, repos: [] })).ok, false);
  assert.equal(decodeUnplanned(ok({ result: "unplanned", count: 1, repos: [{ repo: "x", count: 1, tasks: [{ task: {} }] }] })).ok, false);
  assert.equal(decodeUnplanned(ok({ result: "task", count: 0, repos: [] })).ok, false);
});
