//! The Executions tab's timeline as `@jkb/core` reads it: the requests, the decoders (which refuse a
//! field they do not know), a session's swimlanes, and a plan's change points binned by hour or day.
//! Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import {
  binPlan,
  binUnitFor,
  decodePlanTimeline,
  decodeSessionTimeline,
  decodeTaskTimeline,
  decodeTimelineOverview,
  spanText,
  swimlanes,
  timelineOps,
} from "../dist/index.js";

const ok = (value) => ({ ok: true, value });
const H = 3_600_000;
const D = 24 * H;
/** 2027-01-15T00:00:00Z. */
const MIDNIGHT = 1_800_000_000_000 - 8 * H;

/** An event as `jkb_core::timeline::Event` serializes it: every field, nulls included. */
const ev = (over = {}) => ({
  at_ms: MIDNIGHT,
  source: "workflow",
  id: 1,
  task: "task:a",
  event: "submit_work",
  from: "implement",
  to: "review",
  actor: "swarm-implementer@agent-x",
  role: "implementer",
  reason: null,
  detail: null,
  ...over,
});

const summary = (over = {}) => ({
  session: "s-1",
  state: "live",
  cwd: "/w",
  started_at_ms: MIDNIGHT - H,
  ended_at_ms: null,
  claims: 1,
  last_event_ms: MIDNIGHT,
  stalled: true,
  stalled_for_ms: 31 * 60_000,
  ...over,
});

const graph = {
  strategy: "default:design-reviewed",
  graph: "reviewed-design",
  describe: "d",
  spec: {},
  task: "task:a",
  phase: "review",
  status: "needs_review",
  workflow: { states: [], transitions: [] },
  lifecycle: { states: [], transitions: [] },
};

const taskTimeline = (over = {}) => ({
  uid: "task:a",
  title: "A",
  status: "needs_review",
  strategy: null,
  claim: { owner: "session:1@s-1:/w", claimed_at_ms: MIDNIGHT },
  landing: { state: "none", branch: null, onto: null, pr: null, at_ms: null },
  visited: [{ from: "implement", to: "review", count: 2, last_at_ms: MIDNIGHT }],
  visited_status: [],
  events: [ev()],
  next: null,
  ...over,
});

test("the requests are the ops jkb serve takes", () => {
  assert.deepEqual(timelineOps.sessions(), { op: "timeline.sessions" });
  assert.deepEqual(timelineOps.session("s-1"), { op: "timeline.session", session: "s-1" });
  assert.deepEqual(timelineOps.session("s-1", "1.status.2"), { op: "timeline.session", session: "s-1", after: "1.status.2" });
  assert.deepEqual(timelineOps.task("task:a"), { op: "timeline.task", uid: "task:a" });
  assert.deepEqual(timelineOps.task("task:a", "c"), { op: "timeline.task", uid: "task:a", after: "c" });
  assert.deepEqual(timelineOps.plan("plan:p"), { op: "timeline.plan", uid: "plan:p" });
});

test("the overview decodes as the daemon writes it", () => {
  const wire = {
    result: "timeline_sessions",
    overview: {
      now_ms: MIDNIGHT + H,
      sessions: [summary()],
      unattributed: [{ uid: "task:b", title: "B", status: "in_progress", owner: "host:42", claimed_at_ms: null }],
    },
  };
  const got = decodeTimelineOverview(ok(wire));
  assert.equal(got.ok, true);
  assert.equal(got.value.sessions[0].stalled_for_ms, 31 * 60_000);
  assert.equal(got.value.unattributed[0].owner, "host:42");
  // A failure passes through untouched.
  const refused = { ok: false, error: { code: "unauthorized", message: "no" } };
  assert.deepEqual(decodeTimelineOverview(refused), refused);
  // Another op's answer is not this one.
  assert.equal(decodeTimelineOverview(ok({ result: "workflow_graph", graph })).ok, false);
});

test("a field the decoder does not know refuses the answer, at every level", () => {
  const session = (timeline) => decodeSessionTimeline(ok({ result: "timeline_session", timeline }));
  const page = { summary: summary(), tasks: [], events: [ev()], next: null };
  assert.equal(session(page).ok, true);
  assert.equal(session({ ...page, extra: 1 }).ok, false, "on the page");
  assert.equal(session({ ...page, summary: { ...summary(), extra: 1 } }).ok, false, "on the summary");
  assert.equal(session({ ...page, events: [{ ...ev(), extra: 1 }] }).ok, false, "on an event");
  assert.equal(session({ ...page, events: [ev({ source: "telepathy" })] }).ok, false, "an unknown source");
  assert.equal(decodeSessionTimeline(ok({ result: "timeline_session", timeline: page, extra: 1 })).ok, false, "on the answer");

  const task = (timeline) => decodeTaskTimeline(ok({ result: "timeline_task", task: { timeline, graph } }));
  assert.equal(task(taskTimeline()).ok, true);
  assert.equal(task({ ...taskTimeline(), extra: 1 }).ok, false);
  assert.equal(task(taskTimeline({ landing: { state: "none", branch: null, onto: null, pr: null, at_ms: null, extra: 1 } })).ok, false);
  assert.equal(task(taskTimeline({ visited: [{ from: "a", to: "b", count: 1, last_at_ms: 0, extra: 1 }] })).ok, false);
  assert.equal(task(taskTimeline({ claim: { owner: "x", claimed_at_ms: null, extra: 1 } })).ok, false);

  const plan = (p) => decodePlanTimeline(ok({ result: "timeline_plan", plan: p }));
  const p = { uid: "plan:p", title: "P", tasks: 1, points: [{ at_ms: 0, open: 1, in_progress: 0, done: 0, cancelled: 0 }] };
  assert.equal(plan(p).ok, true);
  assert.equal(plan({ ...p, points: [{ ...p.points[0], extra: 1 }] }).ok, false);
  assert.equal(plan({ ...p, extra: 1 }).ok, false);
});

test("a task's timeline carries its strategy's graph, decoded by workflow.graph's own decoder", () => {
  const got = decodeTaskTimeline(ok({ result: "timeline_task", task: { timeline: taskTimeline(), graph } }));
  assert.equal(got.ok, true);
  assert.equal(got.value.graph.phase, "review");
  assert.equal(got.value.timeline.visited[0].count, 2);
  assert.equal(got.value.timeline.claim.owner, "session:1@s-1:/w");
  assert.equal(decodeTaskTimeline(ok({ result: "timeline_task", task: { timeline: taskTimeline(), graph: {} } })).ok, false);
  // An absent optional field reads as null, as serde writes it when skipped.
  const bare = taskTimeline();
  delete bare.strategy;
  assert.equal(decodeTaskTimeline(ok({ result: "timeline_task", task: { timeline: bare, graph } })).value.timeline.strategy, null);
});

test("swimlanes keep jkb's lane order, give a stray task a lane, and run to now", () => {
  const t = {
    summary: summary(),
    tasks: [
      { uid: "task:a", title: "A", status: "in_progress", holds_claim: true, claimed_at_ms: MIDNIGHT, bound_agents: [] },
      { uid: "task:b", title: "B", status: "open", holds_claim: false, claimed_at_ms: null, bound_agents: ["agent-x"] },
    ],
    events: [
      ev({ source: "session", task: null, at_ms: MIDNIGHT - H, event: "start" }),
      ev({ id: 2, task: "task:a" }),
      ev({ id: 3, task: "task:z", at_ms: MIDNIGHT + 1 }),
    ],
    next: null,
  };
  const s = swimlanes(t, MIDNIGHT + 2 * H);
  assert.deepEqual(
    s.lanes.map((l) => [l.task.uid, l.events.length]),
    [
      ["task:a", 1],
      ["task:b", 0],
      ["task:z", 1],
    ],
  );
  assert.equal(s.session.length, 1);
  assert.equal(s.start_ms, MIDNIGHT - H);
  assert.equal(s.end_ms, MIDNIGHT + 2 * H);
});

test("binning carries each bin's end-state across a day boundary", () => {
  const points = [
    { at_ms: MIDNIGHT - 2 * H + 5, open: 2, in_progress: 0, done: 0, cancelled: 0 },
    { at_ms: MIDNIGHT - 30 * 60_000, open: 1, in_progress: 1, done: 0, cancelled: 0 },
    { at_ms: MIDNIGHT + 10, open: 1, in_progress: 0, done: 1, cancelled: 0 },
  ];
  const hours = binPlan(points, "hour", MIDNIGHT + H + 1);
  assert.deepEqual(
    hours.map((b) => [b.start_ms - MIDNIGHT, b.open, b.in_progress, b.done]),
    [
      [-2 * H, 2, 0, 0],
      [-H, 1, 1, 0],
      [0, 1, 0, 1],
      [H, 1, 0, 1],
    ],
  );
  // By UTC day: the day before midnight ends with one in progress; the day after with one done.
  const days = binPlan(points, "day", MIDNIGHT + H);
  assert.deepEqual(
    days.map((b) => [b.start_ms - MIDNIGHT, b.in_progress, b.done]),
    [
      [-D, 1, 0],
      [0, 0, 1],
    ],
  );
  // On a clock three hours ahead of UTC, local midnight is 21:00Z: every point is in one local day.
  const local = binPlan(points, "day", MIDNIGHT + H, 180);
  assert.deepEqual(
    local.map((b) => [b.start_ms - MIDNIGHT, b.open, b.done]),
    [[-3 * H, 1, 1]],
  );
  assert.deepEqual(binPlan([], "hour", MIDNIGHT), []);
  assert.equal(binUnitFor(points, MIDNIGHT + 2 * D), "hour");
  assert.equal(binUnitFor(points, MIDNIGHT + 5 * D), "day");
});

test("durations read as jkb timeline prints them", () => {
  assert.equal(spanText(45_000), "45s");
  assert.equal(spanText(31 * 60_000), "31m");
  assert.equal(spanText(3_900_000), "1h05m");
  assert.equal(spanText(2 * D + 4 * H), "2d04h");
});
