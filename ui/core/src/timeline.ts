//! The Executions tab's lifecycle timeline as the `timeline.*` ops answer it, minus the window: the
//! requests, the decoders, a session's swimlanes, and a plan's change points binned by hour or day.
//
// The Rust side is the source of truth: `crates/jkb-core/src/timeline.rs` does every join (who a
// claim or a transition speaks for, the merge order, stalled, the visited edges, the change points)
// and `crates/jkb-api/src/timeline.rs` carries it. Nothing here re-decides any of it: the renderer
// draws what was derived once, in Rust.
//
// The decoders follow the wire's version-skew rule: response fields are only ever added, so a field
// this app does not know is ignored, and a source it does not know is kept as an opaque name and
// drawn neutrally. A newer `jkb serve` never blanks the timeline. The strategy's graph is
// `workflow.graph`'s answer, decoded by its own decoder.

import type { OpResponse, Outcome } from "./daemon.js";
import { failed } from "./daemon.js";
import { decodeWorkflowGraph, type WorkflowGraph } from "./workflows.js";

/** `jkb_core::timeline::Source`s this app knows how to draw; any other is drawn neutrally. */
export const TIMELINE_SOURCES = ["session", "claim", "status", "workflow", "review"] as const;

/** Which record an event came from: one of [`TIMELINE_SOURCES`], or a newer one kept by name. */
export type TimelineSource = string;

/** Whether this app draws `source` as its own kind, rather than neutrally. */
export function knownSource(source: string): source is (typeof TIMELINE_SOURCES)[number] {
  return (TIMELINE_SOURCES as readonly string[]).includes(source);
}

/** `jkb_core::timeline::Event`. */
export interface TimelineEvent {
  readonly at_ms: number;
  readonly source: TimelineSource;
  readonly id: number;
  readonly task: string | null;
  readonly event: string;
  readonly from: string | null;
  readonly to: string | null;
  readonly actor: string | null;
  readonly role: string | null;
  readonly reason: string | null;
  readonly detail: string | null;
}

/** `jkb_core::timeline::SessionSummary`. */
export interface SessionSummary {
  readonly session: string;
  /** `live`, `ended` or `unknown`. */
  readonly state: string;
  readonly cwd: string | null;
  readonly started_at_ms: number | null;
  readonly ended_at_ms: number | null;
  readonly claims: number;
  readonly last_event_ms: number | null;
  /** Holds a claim, has not ended, and has fired nothing for 30 minutes — decided in Rust. */
  readonly stalled: boolean;
  readonly stalled_for_ms: number | null;
}

/** `jkb_core::timeline::HeldClaim`: a claim no session is attributed. */
export interface HeldClaim {
  readonly uid: string;
  readonly title: string;
  readonly status: string | null;
  readonly owner: string;
  readonly claimed_at_ms: number | null;
}

/** `timeline.sessions`' answer. */
export interface TimelineOverview {
  readonly now_ms: number;
  readonly sessions: readonly SessionSummary[];
  readonly unattributed: readonly HeldClaim[];
}

/** `jkb_core::timeline::SessionTask`: one swimlane. */
export interface SessionTask {
  readonly uid: string;
  readonly title: string;
  readonly status: string | null;
  readonly holds_claim: boolean;
  readonly claimed_at_ms: number | null;
  readonly bound_agents: readonly string[];
}

/** `timeline.session`'s answer: one page. */
export interface SessionTimeline {
  readonly summary: SessionSummary;
  readonly tasks: readonly SessionTask[];
  readonly events: readonly TimelineEvent[];
  readonly next: string | null;
}

/** `jkb_core::timeline::Visited`: a transition a task's history took — a row of its machine, keyed
 * as the table keys rows, by `from` and `event`. */
export interface VisitedEdge {
  readonly from: string;
  readonly event: string;
  readonly to: string;
  readonly count: number;
  readonly last_at_ms: number;
}

/** `jkb_core::timeline::LandingView`. */
export interface TaskLanding {
  /** `landed`, `superseded` or `none`. */
  readonly state: string;
  readonly branch: string | null;
  readonly onto: string | null;
  readonly pr: number | null;
  readonly at_ms: number | null;
}

/** `jkb_core::timeline::ClaimNow`. */
export interface TaskClaim {
  readonly owner: string;
  readonly claimed_at_ms: number | null;
}

/** `jkb_core::timeline::TaskTimeline`. */
export interface TaskTimeline {
  readonly uid: string;
  readonly title: string;
  readonly status: string | null;
  readonly strategy: string | null;
  readonly claim: TaskClaim | null;
  readonly landing: TaskLanding;
  readonly visited: readonly VisitedEdge[];
  readonly visited_status: readonly VisitedEdge[];
  readonly events: readonly TimelineEvent[];
  readonly next: string | null;
}

/** `timeline.task`'s answer: the timeline and the strategy's machines to draw it over. */
export interface TaskView {
  readonly timeline: TaskTimeline;
  readonly graph: WorkflowGraph;
}

/** `jkb_core::timeline::PlanPoint`: the counts from `at_ms` on. */
export interface PlanPoint {
  readonly at_ms: number;
  readonly open: number;
  readonly in_progress: number;
  readonly done: number;
  readonly cancelled: number;
}

/** `timeline.plan`'s answer. */
export interface PlanTimeline {
  readonly uid: string;
  readonly title: string;
  readonly tasks: number;
  readonly points: readonly PlanPoint[];
}

// ---- requests ---------------------------------------------------------------------------------

/** The requests the timeline makes, as `jkb_api::Request` serializes them. */
export const timelineOps = {
  sessions: () => ({ op: "timeline.sessions" }),
  session: (session: string, after?: string) =>
    after === undefined ? { op: "timeline.session", session } : { op: "timeline.session", session, after },
  task: (uid: string, after?: string) => (after === undefined ? { op: "timeline.task", uid } : { op: "timeline.task", uid, after }),
  plan: (uid: string) => ({ op: "timeline.plan", uid }),
} as const;

// ---- decoding answers -------------------------------------------------------------------------

type Rec = Record<string, unknown>;

function isObject(v: unknown): v is Rec {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}
const isString = (v: unknown): v is string => typeof v === "string";
const isNumber = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);
const isBool = (v: unknown): v is boolean => typeof v === "boolean";
const optString = (v: unknown): v is string | null | undefined => v === null || v === undefined || isString(v);
const optNumber = (v: unknown): v is number | null | undefined => v === null || v === undefined || isNumber(v);
const nul = <T>(v: T | null | undefined): T | null => (v === undefined ? null : v);

function event(v: unknown): TimelineEvent | undefined {
  if (
    !isObject(v) ||
    !isNumber(v["at_ms"]) ||
    !isString(v["source"]) ||
    !isNumber(v["id"]) ||
    !isString(v["event"]) ||
    !["task", "from", "to", "actor", "role", "reason", "detail"].every((k) => optString(v[k]))
  ) {
    return undefined;
  }
  return {
    at_ms: v["at_ms"],
    source: v["source"],
    id: v["id"],
    task: nul(v["task"] as string | null | undefined),
    event: v["event"],
    from: nul(v["from"] as string | null | undefined),
    to: nul(v["to"] as string | null | undefined),
    actor: nul(v["actor"] as string | null | undefined),
    role: nul(v["role"] as string | null | undefined),
    reason: nul(v["reason"] as string | null | undefined),
    detail: nul(v["detail"] as string | null | undefined),
  };
}

/** Every element decoded, or `undefined` when any is not. */
function all<T>(v: unknown, one: (x: unknown) => T | undefined): T[] | undefined {
  if (!Array.isArray(v)) return undefined;
  const out: T[] = [];
  for (const x of v) {
    const d = one(x);
    if (d === undefined) return undefined;
    out.push(d);
  }
  return out;
}

function summary(v: unknown): SessionSummary | undefined {
  if (
    !isObject(v) ||
    !isString(v["session"]) ||
    !isString(v["state"]) ||
    !optString(v["cwd"]) ||
    !optNumber(v["started_at_ms"]) ||
    !optNumber(v["ended_at_ms"]) ||
    !isNumber(v["claims"]) ||
    !optNumber(v["last_event_ms"]) ||
    !isBool(v["stalled"]) ||
    !optNumber(v["stalled_for_ms"])
  ) {
    return undefined;
  }
  return {
    session: v["session"],
    state: v["state"],
    cwd: nul(v["cwd"]),
    started_at_ms: nul(v["started_at_ms"]),
    ended_at_ms: nul(v["ended_at_ms"]),
    claims: v["claims"],
    last_event_ms: nul(v["last_event_ms"]),
    stalled: v["stalled"],
    stalled_for_ms: nul(v["stalled_for_ms"]),
  };
}

function heldClaim(v: unknown): HeldClaim | undefined {
  if (
    !isObject(v) ||
    !isString(v["uid"]) ||
    !isString(v["title"]) ||
    !optString(v["status"]) ||
    !isString(v["owner"]) ||
    !optNumber(v["claimed_at_ms"])
  ) {
    return undefined;
  }
  return { uid: v["uid"], title: v["title"], status: nul(v["status"]), owner: v["owner"], claimed_at_ms: nul(v["claimed_at_ms"]) };
}

function sessionTask(v: unknown): SessionTask | undefined {
  if (
    !isObject(v) ||
    !isString(v["uid"]) ||
    !isString(v["title"]) ||
    !optString(v["status"]) ||
    !isBool(v["holds_claim"]) ||
    !optNumber(v["claimed_at_ms"]) ||
    !Array.isArray(v["bound_agents"]) ||
    !v["bound_agents"].every(isString)
  ) {
    return undefined;
  }
  return {
    uid: v["uid"],
    title: v["title"],
    status: nul(v["status"]),
    holds_claim: v["holds_claim"],
    claimed_at_ms: nul(v["claimed_at_ms"]),
    bound_agents: v["bound_agents"] as string[],
  };
}

function visited(v: unknown): VisitedEdge | undefined {
  if (
    !isObject(v) ||
    !isString(v["from"]) ||
    !isString(v["event"]) ||
    !isString(v["to"]) ||
    !isNumber(v["count"]) ||
    !isNumber(v["last_at_ms"])
  ) {
    return undefined;
  }
  return { from: v["from"], event: v["event"], to: v["to"], count: v["count"], last_at_ms: v["last_at_ms"] };
}

function landing(v: unknown): TaskLanding | undefined {
  if (
    !isObject(v) ||
    !isString(v["state"]) ||
    !optString(v["branch"]) ||
    !optString(v["onto"]) ||
    !optNumber(v["pr"]) ||
    !optNumber(v["at_ms"])
  ) {
    return undefined;
  }
  return { state: v["state"], branch: nul(v["branch"]), onto: nul(v["onto"]), pr: nul(v["pr"]), at_ms: nul(v["at_ms"]) };
}

function taskClaim(v: unknown): TaskClaim | null | undefined {
  if (v === null || v === undefined) return null;
  if (!isObject(v) || !isString(v["owner"]) || !optNumber(v["claimed_at_ms"])) {
    return undefined;
  }
  return { owner: v["owner"], claimed_at_ms: nul(v["claimed_at_ms"]) };
}

function planPoint(v: unknown): PlanPoint | undefined {
  if (
    !isObject(v) ||
    !isNumber(v["at_ms"]) ||
    !isNumber(v["open"]) ||
    !isNumber(v["in_progress"]) ||
    !isNumber(v["done"]) ||
    !isNumber(v["cancelled"])
  ) {
    return undefined;
  }
  return { at_ms: v["at_ms"], open: v["open"], in_progress: v["in_progress"], done: v["done"], cancelled: v["cancelled"] };
}

/** The `field` of a `result` answer, or `undefined` when the answer is another. */
function field(o: OpResponse, result: string, name: string): unknown {
  if (o.result !== result) return undefined;
  return o[name];
}

const malformed = <T>(result: string): Outcome<T> => failed("internal", `jkb serve did not answer with a well-formed \`${result}\``);

/** `timeline.sessions`' answer. */
export function decodeTimelineOverview(o: Outcome<OpResponse>): Outcome<TimelineOverview> {
  if (!o.ok) return o;
  const v = field(o.value, "timeline_sessions", "overview");
  if (!isObject(v) || !isNumber(v["now_ms"])) {
    return malformed("timeline_sessions");
  }
  const sessions = all(v["sessions"], summary);
  const unattributed = all(v["unattributed"], heldClaim);
  if (sessions === undefined || unattributed === undefined) return malformed("timeline_sessions");
  return { ok: true, value: { now_ms: v["now_ms"], sessions, unattributed } };
}

/** `timeline.session`'s answer. */
export function decodeSessionTimeline(o: Outcome<OpResponse>): Outcome<SessionTimeline> {
  if (!o.ok) return o;
  const v = field(o.value, "timeline_session", "timeline");
  if (!isObject(v) || !optString(v["next"])) {
    return malformed("timeline_session");
  }
  const s = summary(v["summary"]);
  const tasks = all(v["tasks"], sessionTask);
  const events = all(v["events"], event);
  if (s === undefined || tasks === undefined || events === undefined) return malformed("timeline_session");
  return { ok: true, value: { summary: s, tasks, events, next: nul(v["next"]) } };
}

/** `timeline.task`'s answer. */
export function decodeTaskTimeline(o: Outcome<OpResponse>): Outcome<TaskView> {
  if (!o.ok) return o;
  const view = field(o.value, "timeline_task", "task");
  if (!isObject(view)) return malformed("timeline_task");
  const t = view["timeline"];
  if (
    !isObject(t) ||
    !isString(t["uid"]) ||
    !isString(t["title"]) ||
    !optString(t["status"]) ||
    !optString(t["strategy"]) ||
    !optString(t["next"])
  ) {
    return malformed("timeline_task");
  }
  const claim = taskClaim(t["claim"]);
  const land = landing(t["landing"]);
  const phases = all(t["visited"], visited);
  const statuses = all(t["visited_status"], visited);
  const events = all(t["events"], event);
  const graph = decodeWorkflowGraph({ ok: true, value: { result: "workflow_graph", graph: view["graph"] } });
  if (claim === undefined || land === undefined || phases === undefined || statuses === undefined || events === undefined || !graph.ok) {
    return malformed("timeline_task");
  }
  return {
    ok: true,
    value: {
      timeline: {
        uid: t["uid"],
        title: t["title"],
        status: nul(t["status"]),
        strategy: nul(t["strategy"]),
        claim,
        landing: land,
        visited: phases,
        visited_status: statuses,
        events,
        next: nul(t["next"]),
      },
      graph: graph.value,
    },
  };
}

/** `timeline.plan`'s answer. */
export function decodePlanTimeline(o: Outcome<OpResponse>): Outcome<PlanTimeline> {
  if (!o.ok) return o;
  const v = field(o.value, "timeline_plan", "plan");
  if (!isObject(v) || !isString(v["uid"]) || !isString(v["title"]) || !isNumber(v["tasks"])) {
    return malformed("timeline_plan");
  }
  const points = all(v["points"], planPoint);
  if (points === undefined) return malformed("timeline_plan");
  return { ok: true, value: { uid: v["uid"], title: v["title"], tasks: v["tasks"], points } };
}

// ---- a session's swimlanes --------------------------------------------------------------------

/** One swimlane: a task, and the events about it. */
export interface Swimlane {
  readonly task: SessionTask;
  readonly events: readonly TimelineEvent[];
}

/** A session's timeline laid out: a lane per task in jkb's order, and the session's own events. */
export interface Swimlanes {
  readonly lanes: readonly Swimlane[];
  /** Events about no task: the session's starts and ends. */
  readonly session: readonly TimelineEvent[];
  readonly start_ms: number;
  readonly end_ms: number;
}

/**
 * The swimlanes of a session's page, to `now_ms` (the timeline runs to now, so a stalled session's
 * silence shows as empty track). An event about a task the lanes do not list gets a lane of its own
 * at the end rather than vanishing.
 */
export function swimlanes(t: SessionTimeline, now_ms: number): Swimlanes {
  const byTask = new Map<string, TimelineEvent[]>();
  const session: TimelineEvent[] = [];
  for (const e of t.events) {
    if (e.task === null) session.push(e);
    else byTask.set(e.task, [...(byTask.get(e.task) ?? []), e]);
  }
  const lanes: Swimlane[] = t.tasks.map((task) => ({ task, events: byTask.get(task.uid) ?? [] }));
  const listed = new Set(t.tasks.map((x) => x.uid));
  for (const [uid, events] of byTask) {
    if (!listed.has(uid)) {
      lanes.push({ task: { uid, title: uid, status: null, holds_claim: false, claimed_at_ms: null, bound_agents: [] }, events });
    }
  }
  const times = t.events.map((e) => e.at_ms);
  const start_ms = times.length > 0 ? Math.min(...times) : now_ms;
  return { lanes, session, start_ms, end_ms: Math.max(now_ms, start_ms + 1) };
}

// ---- a plan's change points, binned -----------------------------------------------------------

export type BinUnit = "hour" | "day";

const UNIT_MS: Record<BinUnit, number> = { hour: 3_600_000, day: 86_400_000 };

/** The counts as of the end of one bin. */
export interface PlanBin extends Omit<PlanPoint, "at_ms"> {
  readonly start_ms: number;
  readonly end_ms: number;
}

/** The most bins one chart draws; a longer span is drawn at its newest bins. */
export const MAX_BINS = 2000;

/** Hours for a plan that spans up to three days, days past that. */
export function binUnitFor(points: readonly PlanPoint[], until_ms: number): BinUnit {
  const first = points[0];
  return first === undefined || until_ms - first.at_ms <= 3 * UNIT_MS.day ? "hour" : "day";
}

/**
 * The change points as bins of `unit`, each holding the counts as they stood at its end, from the
 * first point's bin through the one holding `until_ms`. Bins are aligned to the operator's clock:
 * `offset_min` is the local offset from UTC in minutes (east positive, `-Date#getTimezoneOffset()`),
 * so a day bin runs local midnight to midnight.
 */
export function binPlan(points: readonly PlanPoint[], unit: BinUnit, until_ms: number, offset_min = 0): PlanBin[] {
  const first = points[0];
  if (first === undefined) return [];
  const size = UNIT_MS[unit];
  const off = offset_min * 60_000;
  const align = (t: number): number => Math.floor((t + off) / size) * size - off;
  const last = Math.max(until_ms, (points[points.length - 1] as PlanPoint).at_ms);
  let start = Math.max(align(first.at_ms), align(last) - (MAX_BINS - 1) * size);
  const out: PlanBin[] = [];
  let i = 0;
  let now: PlanPoint = { at_ms: 0, open: 0, in_progress: 0, done: 0, cancelled: 0 };
  for (; start <= last; start += size) {
    const end = start + size;
    while (i < points.length && (points[i] as PlanPoint).at_ms < end) {
      now = points[i] as PlanPoint;
      i += 1;
    }
    out.push({ start_ms: start, end_ms: end, open: now.open, in_progress: now.in_progress, done: now.done, cancelled: now.cancelled });
  }
  return out;
}

// ---- words ------------------------------------------------------------------------------------

/** A duration in ms, coarsely, as `jkb timeline` prints it: `45s`, `12m`, `3h05m`, `2d04h`. */
export function spanText(ms: number): string {
  const s = Math.floor(Math.max(0, ms) / 1000);
  const pad = (n: number): string => String(n).padStart(2, "0");
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86_400) return `${Math.floor(s / 3600)}h${pad(Math.floor((s % 3600) / 60))}m`;
  return `${Math.floor(s / 86_400)}d${pad(Math.floor((s % 86_400) / 3600))}h`;
}
