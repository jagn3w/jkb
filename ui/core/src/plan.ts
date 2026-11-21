//! Execution plans and their tasks as the ops answer them (D53.6): the Design tab's Execution
//! Plan and Tasks panes, minus the window. Decoding `design.plans` / `design.prompt` (Play) /
//! `task.show` / `workflow.strategies`, the requests the panes make, and the two decisions they
//! take from the answers — which plans sit in the archive drawer, and which tasks a Play must pin to
//! the chosen strategy first.
//
// The Rust side is the source of truth: `crates/jkb-api/src/designs/plans.rs` (the shapes) and
// `crates/jkb-core/src/design/plan.rs` (the rules — archived is derived there, never here).

import type { OpRequest, OpResponse, Outcome } from "./daemon.js";
import { failed } from "./daemon.js";
import type { Span } from "./design.js";

/** `jkb_api::designs::plans::PlanTask`: a task under a plan's step, or a one-off. */
export interface PlanTask {
  readonly uid: string;
  readonly title: string;
  readonly status: string | null;
  readonly priority: number | null;
  /** 0 for a step's own task, 1 for its subtask, …. */
  readonly depth: number;
  readonly claimed_by: string | null;
  /** The workflow strategy it runs: the name it was pinned to, or `default:<name>`. */
  readonly strategy: string;
  /**
   * For a review finding, the tasks a review of which filed it (its `found_in` edges); empty or
   * absent (an older daemon) for anything else. Display only: no gate reads it.
   */
  readonly found_in?: readonly TaskRef[];
}

/** `jkb_core::design::TaskRef`: a task by uid and title. */
export interface TaskRef {
  readonly uid: string;
  readonly title: string;
}

/** `jkb_api::kb::FindingSummary`: a review finding filed about a task, as `task.show` lists it. */
export interface FindingSummary {
  readonly uid: string;
  readonly title: string;
  readonly status: string | null;
  readonly priority: number | null;
  /** Its review round's namespace. */
  readonly round: string | null;
  /** Must-fix as its recorded round held it (else priority 1 or above). */
  readonly must_fix: boolean;
  /** The plan step it sits under now, by uid. */
  readonly step: string | null;
  /** That step as `<design> › <step>`. */
  readonly step_title: string | null;
  /** The design its step belongs to. */
  readonly design: string | null;
}

/** `jkb_api::designs::plans::Step`. */
export interface PlanStep {
  readonly uid: string;
  readonly text: string;
  readonly spans: readonly Span[];
  readonly tasks: readonly PlanTask[];
}

/** `jkb_api::designs::plans::Plan`. */
export interface ExecPlan {
  readonly uid: string;
  readonly title: string;
  readonly design: string;
  /** Every task under it is terminal (and it has at least one) — derived by jkb. */
  readonly archived: boolean;
  readonly steps: readonly PlanStep[];
}

/** `jkb_api::designs::plans::PlanList`: `design.plans`' answer. */
export interface PlanList {
  readonly uid: string;
  readonly plans: readonly ExecPlan[];
  /** Archived plans the listing left out (0 when it was asked for all). */
  readonly hidden: number;
  /** Tasks directly under the design. */
  readonly tasks: readonly PlanTask[];
}

/** `jkb_api::designs::plans::WorkPrompt`: a *Play* prompt and what it was built from. */
export interface WorkPrompt {
  readonly kind: "play" | "task";
  readonly uid: string;
  readonly title: string;
  readonly design: string | null;
  /**
   * As a task pinned to it reports it (`name@version` for a definition): for a plan, the strategy the
   * operator picked, `null` when none was; for a task, the one it runs.
   */
  readonly strategy: string | null;
  readonly prompt: string;
}

/** One entry of `workflow.strategies`: a preset, or a definition named `name@version`. */
export interface Strategy {
  readonly name: string;
  readonly preset: boolean;
  /** One sentence on why to choose it (for a definition written before descriptions, a fallback naming its graph). */
  readonly describe: string;
  /** One sentence on its graph; absent from an older daemon. */
  readonly graph?: string | null;
  /** When a definition's listed version was defined; absent for a preset, and from an older daemon. */
  readonly defined_at?: string | null;
}

export interface Strategies {
  readonly strategies: readonly Strategy[];
  /** What a task with nothing pinned runs. */
  readonly default: string;
}

/** One transition, as `task.show` summarizes it. */
export interface TaskTransition {
  readonly at: string;
  readonly event: string;
  readonly to: string;
  readonly branch: string | null;
  readonly onto: string | null;
  readonly pr: number | null;
}

/** `task.show`'s answer, the parts the Tasks pane reads. */
export interface TaskDetail {
  readonly uid: string;
  readonly status: string | null;
  readonly priority: number | null;
  readonly namespace: string | null;
  /** The task's text: its title line, then its notes. */
  readonly content: string;
  readonly tags: readonly { readonly facet: string; readonly value: string }[];
  readonly transitions: readonly TaskTransition[];
  /** Its recorded review rounds' namespaces, in recording order. */
  readonly review_namespaces: readonly string[];
  /** The review findings filed about it, by round in recording order. */
  readonly findings: readonly FindingSummary[];
  /** For a finding: the tasks it was found in. */
  readonly found_in: readonly TaskRef[];
}

// ---- requests ---------------------------------------------------------------------------------

/** The requests the Execution Plan and Tasks panes make, as `jkb_api::Request` serializes them. */
export const planOps = {
  /** Always with `all`: the pane splits live from archived itself, for the drawer. */
  plans: (uid: string) => ({ op: "design.plans", uid, all: true }),
  play: (plan: string, strategy?: string) => ({
    op: "design.prompt",
    ask: strategy === undefined ? { kind: "play", plan } : { kind: "play", plan, strategy: strategyName(strategy) },
  }),
  playTask: (uid: string) => ({ op: "design.prompt", ask: { kind: "task", uid } }),
  strategies: () => ({ op: "workflow.strategies" }),
  /** Pin a task to a strategy (the operator's; the app holds the operator's token). */
  pin: (uid: string, strategy: string) => ({ op: "workflow.set", uid, strategy: strategyName(strategy) }),
  show: (uid: string) => ({ op: "task.show", uid }),
  /** Move a task under a plan step (or a task), e.g. its repo's `step:inbox@<repo>`. */
  move: (uid: string, under: string) => ({ op: "task.move", uid, under }),
  /**
   * Replace a task's text, or append a note to it. A replace names the text it was made against
   * (`expected`), and jkb refuses it as `stale` when the task's text is no longer that.
   */
  edit: (uid: string, text: string, append: boolean, expected?: string) =>
    expected === undefined ? { op: "task.edit", uid, text, append } : { op: "task.edit", uid, text, append, expected },
} as const;

/** A strategy identity, taken apart: the one parser of `name@version` and `default:<name>`. */
export interface StrategyParts {
  /** The name `workflow.set` and *Play* resolve. */
  readonly name: string;
  /** A definition's version; `null` for a preset. */
  readonly version: number | null;
  /** A task's `default:<name>`: it pinned nothing and runs whatever the default is. */
  readonly followsDefault: boolean;
}

const FOLLOWS_DEFAULT = "default:";

/**
 * Take a strategy identity apart: a listing's or a pin's `name@version` (a definition), a bare name
 * (a preset), either behind a task's `default:`. A definition name is letters, digits, `-` and `_`
 * (`store::define`), so neither `@` nor `:` occurs inside one.
 */
export function strategyParts(identity: string): StrategyParts {
  const followsDefault = identity.startsWith(FOLLOWS_DEFAULT);
  const rest = followsDefault ? identity.slice(FOLLOWS_DEFAULT.length) : identity;
  const at = rest.lastIndexOf("@");
  const version = at > 0 ? rest.slice(at + 1) : "";
  if (/^\d+$/.test(version)) {
    return { name: rest.slice(0, at), version: Number(version), followsDefault };
  }
  return { name: rest, version: null, followsDefault };
}

/**
 * The name `workflow.set` and *Play* resolve: a definition is listed as `name@version` and
 * resolved by `name` (its newest version); a preset is its name.
 */
export function strategyName(listed: string): string {
  return strategyParts(listed).name;
}

/**
 * A strategy identity as the app shows it: `factory · v1` for a definition (the agent panel's
 * `packaged · v1`), a preset by its name, and `(following default)` after a task's `default:`.
 * Display only — every comparison and request keeps the identity itself.
 */
export function strategyLabel(identity: string): string {
  const { name, version, followsDefault } = strategyParts(identity);
  return `${name}${version === null ? "" : ` · v${version}`}${followsDefault ? " (following default)" : ""}`;
}

/** Whether two identities name the same strategy (a listing's `name@version`, the default's). */
export function sameStrategy(a: string, b: string): boolean {
  return strategyName(a) === strategyName(b);
}

/** A definition time in the operator's own locale; the raw text when it is not a time. */
export function localTime(at: string): string {
  const t = new Date(at);
  return Number.isNaN(t.getTime()) ? at : t.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

/**
 * What a picker's option shows for one listed strategy: its label, then — for an operator
 * definition — the marker that sets it apart from the presets and when its version was defined,
 * then `(default)` when it is what an unpinned task runs. `time` formats the definition time.
 */
export function strategyOption(s: Strategy, defaultIdentity: string, time: (at: string) => string = localTime): string {
  const own = s.preset ? "" : ` — operator-defined${s.defined_at == null ? "" : ` ${time(s.defined_at)}`}`;
  return `${strategyLabel(s.name)}${own}${sameStrategy(s.name, defaultIdentity) ? " (default)" : ""}`;
}

// ---- decisions --------------------------------------------------------------------------------

const TERMINAL = new Set(["done", "cancelled"]);

/** Whether a task status is terminal (`TaskStatus::is_terminal_str`). */
export function isTerminal(status: string | null | undefined): boolean {
  return status !== null && status !== undefined && TERMINAL.has(status);
}

/** Live plans for the pane, archived ones for the history drawer — as jkb derived them. */
export function partitionPlans(plans: readonly ExecPlan[]): { live: ExecPlan[]; archived: ExecPlan[] } {
  const live: ExecPlan[] = [];
  const archived: ExecPlan[] = [];
  for (const p of plans) (p.archived ? archived : live).push(p);
  return { live, archived };
}

/** A task row the Tasks list draws, and whether it is there only as an ancestor of one shown. */
export interface TaskLine {
  readonly task: PlanTask;
  /** Not itself shown (finished), kept so a shown subtask is drawn under its real parent. */
  readonly muted: boolean;
}

/**
 * The rows of a depth-first task list (`depth` 0 for a step's own task) with the ones `shown` drops
 * left out, **except** each ancestor of a task that stays: it is kept, muted. Dropping it drew an
 * open subtask of a finished task indented under whichever task came before it (review round 1).
 */
export function visibleTasks(tasks: readonly PlanTask[], shown: (t: PlanTask) => boolean): TaskLine[] {
  // A finished task a shown finding was found in is kept too, muted, as an ancestor is: the finding
  // is drawn under it (`nestFindings`), and its origin is part of what it says.
  const origins = new Set<string>();
  for (const t of tasks) {
    if (!shown(t)) continue;
    const origin = originIn(t, tasks);
    if (origin !== undefined) origins.add(origin);
  }
  const kept = (t: PlanTask): boolean => shown(t) || origins.has(t.uid);
  const out: TaskLine[] = [];
  // Walked from the end: a kept row at depth d needs the nearest earlier row shallower than d.
  let want = Number.POSITIVE_INFINITY;
  for (let i = tasks.length - 1; i >= 0; i -= 1) {
    const t = tasks[i];
    if (t === undefined) continue;
    if (kept(t)) {
      out.push({ task: t, muted: !shown(t) });
      want = t.depth;
    } else if (want !== Number.POSITIVE_INFINITY && t.depth < want) {
      out.push({ task: t, muted: true });
      want = t.depth;
    }
  }
  return out.reverse();
}

/** The first of `t`'s `found_in` tasks listed in `among` (other than `t`), by `among`'s order. */
function originIn(t: PlanTask, among: readonly { readonly uid: string }[]): string | undefined {
  const named = new Set((t.found_in ?? []).map((r) => r.uid).filter((u) => u !== t.uid));
  if (named.size === 0) return undefined;
  return among.find((x) => named.has(x.uid))?.uid;
}

/** A task row with, for a review finding, where it is drawn from. */
export type FindingLine = TaskLine & {
  /** The uid of the task listed in the same group that this finding was found in: drawn under it. */
  readonly findingOf?: string;
  /** For a finding whose origin is not in the group: that origin's title, for a "from …" note. */
  readonly from?: string;
};

/**
 * A step group's rows, reordered so each review finding sits right after the task it was found in
 * (after that task's subtree), marked `findingOf` — never by depth, so it never reads as a subtask.
 * With two origins in the group the first listed wins. A finding whose origin is elsewhere (another
 * step or design, or shelved to Open work › Inbox) stays where it is, with `from` naming its origin.
 * Run on `visibleTasks`' output. Subtask depths are untouched.
 */
export function nestFindings(lines: readonly TaskLine[]): FindingLine[] {
  const tasks = lines.map((l) => l.task);
  const index = new Map(tasks.map((t, i) => [t.uid, i]));
  // The block each line heads: it and the lines after it that are deeper (its subtree).
  const blockEnd = (i: number): number => {
    const depth = tasks[i]?.depth ?? 0;
    let j = i + 1;
    while (j < tasks.length && (tasks[j]?.depth ?? 0) > depth) j += 1;
    return j;
  };
  const originOf = new Map<number, number>();
  for (let i = 0; i < tasks.length; i += 1) {
    const t = tasks[i];
    if (t === undefined) continue;
    const origin = originIn(t, tasks);
    const at = origin === undefined ? undefined : index.get(origin);
    if (at !== undefined) originOf.set(i, at);
  }
  // A chain of findings that loops back on itself moves none of its members.
  for (const start of [...originOf.keys()]) {
    const seen = new Set<number>([start]);
    let at = originOf.get(start);
    while (at !== undefined) {
      if (seen.has(at)) {
        for (const s of seen) originOf.delete(s);
        break;
      }
      seen.add(at);
      at = originOf.get(at);
    }
  }
  // A finding inside a moved block moves with it, so only a block's own head is attached.
  const attached = new Map<number, number[]>();
  const heads = new Set<number>();
  // The head of the moved block each line is in.
  const headOf = new Map<number, number>();
  for (let i = 0; i < tasks.length; i += 1) {
    if (headOf.has(i)) continue;
    const origin = originOf.get(i);
    if (origin === undefined) continue;
    const end = blockEnd(i);
    // An origin inside the finding's own block cannot hold it.
    if (origin > i && origin < end) continue;
    heads.add(i);
    for (let j = i; j < end; j += 1) headOf.set(j, i);
    attached.set(origin, [...(attached.get(origin) ?? []), i]);
  }
  // A loop the check above cannot see — one through a moved block's subtask — would draw its
  // blocks under one another and none of them under anything drawn. Each block on such a loop is
  // not moved: it stays where it was listed, noted `from` its origin like any finding whose origin
  // is not drawn here. A block hanging off the loop is then drawn under its origin as usual.
  const next = (h: number): number | undefined => {
    const origin = originOf.get(h);
    return origin === undefined ? undefined : headOf.get(origin);
  };
  const onLoop = [...heads].filter((h) => {
    let at = next(h);
    for (let steps = 0; at !== undefined && steps < heads.size; steps += 1) {
      if (at === h) return true;
      at = next(at);
    }
    return false;
  });
  for (const h of onLoop) {
    heads.delete(h);
    const origin = originOf.get(h);
    if (origin !== undefined) attached.set(origin, (attached.get(origin) ?? []).filter((f) => f !== h));
    originOf.delete(h);
  }
  const note = (line: TaskLine, k: number): FindingLine => {
    if (originOf.has(k)) return line;
    const first = (line.task.found_in ?? []).find((r) => r.uid !== line.task.uid);
    return first === undefined ? line : { ...line, from: first.title };
  };
  const out: FindingLine[] = [];
  const emitted = new Set<number>();
  // Lines `from`..`to` in order, each followed (after its own subtree) by the findings found in it.
  // A finding attached elsewhere is skipped here, with its subtree: it is drawn under its origin.
  const walk = (from: number, to: number): void => {
    let k = from;
    while (k < to) {
      const sub = blockEnd(k);
      const line = lines[k];
      if (line !== undefined && !heads.has(k) && !emitted.has(k)) {
        emitted.add(k);
        out.push(note(line, k));
        walk(k + 1, Math.min(sub, to));
        for (const f of attached.get(k) ?? []) emitFinding(f, line.task.uid);
      }
      k = sub;
    }
  };
  const emitFinding = (i: number, origin: string): void => {
    const line = lines[i];
    if (line === undefined || emitted.has(i)) return;
    emitted.add(i);
    out.push({ ...line, findingOf: origin });
    walk(i + 1, blockEnd(i));
    for (const f of attached.get(i) ?? []) emitFinding(f, line.task.uid);
  };
  walk(0, tasks.length);
  return out;
}

/** Every task of a plan, in step order. */
export function planTasks(plan: ExecPlan): PlanTask[] {
  return plan.steps.flatMap((s) => s.tasks);
}

/**
 * The tasks a *Play* under `strategy` must pin first: the open ones not already running it.
 * `strategy` is the identity a task pinned to it reports — a `workflow.strategies` listing's
 * `name@version` for a definition, the name for a preset ([`Pick.listed`]). A task already pinned to
 * it is left alone — pinning writes a row of its history — and a terminal one is not work the Play
 * starts.
 */
export function tasksToPin(tasks: readonly PlanTask[], strategy: string): PlanTask[] {
  return tasks.filter((t) => !isTerminal(t.status) && t.strategy !== strategy);
}

/**
 * An explicit strategy pick, by its one identity: `name` is what `workflow.set` and *Play* send, and
 * `listed` is what it resolves to now — what a task pinned to it reports (`mine@3`) — read from a
 * `workflow.strategies` listing. Compare tasks with `listed`; pin with `name`.
 */
export interface Pick {
  readonly name: string;
  readonly listed: string;
}

/**
 * The pick `name` (a bare strategy name) resolves to in `strategies`, the listing being each
 * definition's newest version — what `workflow.set` resolves the bare name to. `undefined` when it is
 * no longer listed.
 */
export function pickOf(strategies: Strategies | undefined, name: string | undefined): Pick | undefined {
  if (strategies === undefined || name === undefined) return undefined;
  const bare = strategyName(name);
  const s = strategies.strategies.find((x) => strategyName(x.name) === bare);
  return s === undefined ? undefined : { name: bare, listed: s.name };
}

/**
 * The pins a *Play* writes, in order, given what the operator `picked` in the strategy picker — an explicit
 * choice, never the default the picker merely shows. With nothing picked nothing is pinned: an
 * unpinned task (`default:<name>`) keeps following the default, so a later change of the default
 * still reaches it. A pick that equals the default does pin the open tasks reporting
 * `default:<name>`, since freezing them on it — off whatever the default later becomes — is what
 * an explicit choice asks for.
 */
export function playPins(tasks: readonly PlanTask[], picked: Pick | undefined): ReturnType<typeof planOps.pin>[] {
  return picked === undefined ? [] : tasksToPin(tasks, picked.listed).map((t) => planOps.pin(t.uid, picked.name));
}

// ---- editing a task's text ----------------------------------------------------------------------

/**
 * The Tasks pane's edit of a task's text: what it reads now, and the text it was started from. A
 * Save sends `base` as `task.edit`'s `expected`, so a note appended meanwhile (by the Play session's
 * Claude, say) is never erased by it.
 */
export interface Draft {
  readonly text: string;
  readonly base: string;
}

/** A Save's outcome: written; refused because the task changed under the draft; or refused. */
export type Saved =
  | { readonly kind: "saved" }
  | { readonly kind: "stale"; readonly message: string }
  | { readonly kind: "failed"; readonly message: string };

/** Save `draft` over task `uid`'s text, only while that text is still the draft's base. */
export async function saveDraft(
  op: (request: OpRequest) => Promise<Outcome<OpResponse>>,
  uid: string,
  draft: Draft,
): Promise<Saved> {
  const answer = await op(planOps.edit(uid, draft.text, false, draft.base));
  if (answer.ok) return { kind: "saved" };
  return { kind: answer.error.code === "stale" ? "stale" : "failed", message: answer.error.message };
}

/**
 * After a `stale` refusal, the operator read the task's `current` text and chose to keep their draft:
 * the draft, now based on `current`, so the next Save replaces exactly what they saw.
 */
export function rebaseDraft(draft: Draft, current: string): Draft {
  return { text: draft.text, base: current };
}

// ---- decoding answers -------------------------------------------------------------------------

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

const isString = (v: unknown): v is string => typeof v === "string";
const isNumber = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);
const isOptString = (v: unknown): v is string | null => v === null || v === undefined || isString(v);
const isOptNumber = (v: unknown): v is number | null => v === null || v === undefined || isNumber(v);

function isPlanTask(v: unknown): v is PlanTask {
  return (
    isObject(v) &&
    isString(v["uid"]) &&
    isString(v["title"]) &&
    isOptString(v["status"]) &&
    isOptNumber(v["priority"]) &&
    isNumber(v["depth"]) &&
    isOptString(v["claimed_by"]) &&
    isString(v["strategy"]) &&
    (v["found_in"] === undefined || isRefs(v["found_in"]))
  );
}

/** `v` as a `PlanTask`, or `undefined` when it is not one (for other decoders of the shape). */
export function decodePlanTaskValue(v: unknown): PlanTask | undefined {
  return isPlanTask(v) ? v : undefined;
}

function isRefs(v: unknown): v is readonly TaskRef[] {
  return Array.isArray(v) && v.every((r) => isObject(r) && isString(r["uid"]) && isString(r["title"]));
}

function isFinding(v: unknown): v is FindingSummary {
  return (
    isObject(v) &&
    isString(v["uid"]) &&
    isString(v["title"]) &&
    isOptString(v["status"]) &&
    isOptNumber(v["priority"]) &&
    isOptString(v["round"]) &&
    typeof v["must_fix"] === "boolean" &&
    isOptString(v["step"]) &&
    isOptString(v["step_title"]) &&
    isOptString(v["design"])
  );
}

function isStep(v: unknown): v is PlanStep {
  return (
    isObject(v) &&
    isString(v["uid"]) &&
    isString(v["text"]) &&
    Array.isArray(v["spans"]) &&
    v["spans"].every((s) => isObject(s) && isString(s["uid"]) && isString(s["state"])) &&
    Array.isArray(v["tasks"]) &&
    v["tasks"].every(isPlanTask)
  );
}

function isPlan(v: unknown): v is ExecPlan {
  return (
    isObject(v) &&
    isString(v["uid"]) &&
    isString(v["title"]) &&
    isString(v["design"]) &&
    typeof v["archived"] === "boolean" &&
    Array.isArray(v["steps"]) &&
    v["steps"].every(isStep)
  );
}

function field<T>(
  outcome: Outcome<OpResponse>,
  result: string,
  pick: (r: OpResponse) => unknown,
  test: (v: unknown) => v is T,
): Outcome<T> {
  if (!outcome.ok) return outcome;
  const value = outcome.value.result === result ? pick(outcome.value) : undefined;
  if (!test(value)) return failed("internal", `jkb serve did not answer with a well-formed \`${result}\``);
  return { ok: true, value };
}

export function decodePlanList(o: Outcome<OpResponse>): Outcome<PlanList> {
  return field(
    o,
    "design_plans",
    (r) => r["list"],
    (v): v is PlanList =>
      isObject(v) &&
      isString(v["uid"]) &&
      Array.isArray(v["plans"]) &&
      v["plans"].every(isPlan) &&
      isNumber(v["hidden"]) &&
      Array.isArray(v["tasks"]) &&
      v["tasks"].every(isPlanTask),
  );
}

export function decodeWorkPrompt(o: Outcome<OpResponse>): Outcome<WorkPrompt> {
  return field(
    o,
    "design_work_prompt",
    (r) => r["prompt"],
    (v): v is WorkPrompt =>
      isObject(v) &&
      (v["kind"] === "play" || v["kind"] === "task") &&
      isString(v["uid"]) &&
      isString(v["title"]) &&
      isOptString(v["design"]) &&
      isOptString(v["strategy"]) &&
      isString(v["prompt"]) &&
      v["prompt"] !== "",
  );
}

export function decodeStrategies(o: Outcome<OpResponse>): Outcome<Strategies> {
  return field(
    o,
    "strategies",
    (r) => r,
    (v): v is Strategies =>
      isObject(v) &&
      isString(v["default"]) &&
      Array.isArray(v["strategies"]) &&
      v["strategies"].every(
        (s) =>
          isObject(s) &&
          isString(s["name"]) &&
          typeof s["preset"] === "boolean" &&
          isString(s["describe"]) &&
          isOptString(s["graph"]) &&
          isOptString(s["defined_at"]),
      ),
  );
}

function isTransition(v: unknown): v is TaskTransition {
  return (
    isObject(v) &&
    isString(v["at"]) &&
    isString(v["event"]) &&
    isString(v["to"]) &&
    isOptString(v["branch"]) &&
    isOptString(v["onto"]) &&
    isOptNumber(v["pr"])
  );
}

/** `task.show`, flattened to what the Tasks pane shows. */
export function decodeTaskDetail(o: Outcome<OpResponse>): Outcome<TaskDetail> {
  if (!o.ok) return o;
  const task = o.value.result === "task" ? o.value["task"] : undefined;
  const item = isObject(task) ? task["item"] : undefined;
  const transitions = isObject(task) ? task["transitions"] : undefined;
  // Absent from an older daemon's answer, and when empty: none.
  const findings = isObject(task) ? (task["findings"] ?? []) : undefined;
  const foundIn = isObject(task) ? (task["found_in"] ?? []) : undefined;
  const review = isObject(task) ? task["review"] : undefined;
  const namespaces = isObject(review) ? (review["namespaces"] ?? []) : [];
  if (
    !isObject(item) ||
    !isString(item["uid"]) ||
    !isOptString(item["status"]) ||
    !isOptNumber(item["priority"]) ||
    !isOptString(item["namespace"]) ||
    !isOptString(item["content"]) ||
    !Array.isArray(item["tags"]) ||
    !item["tags"].every((t) => isObject(t) && isString(t["facet"]) && isString(t["value"])) ||
    !Array.isArray(transitions) ||
    !transitions.every(isTransition) ||
    !Array.isArray(findings) ||
    !findings.every(isFinding) ||
    !isRefs(foundIn) ||
    !Array.isArray(namespaces) ||
    !namespaces.every(isString)
  ) {
    return failed("internal", "jkb serve did not answer with a well-formed `task`");
  }
  return {
    ok: true,
    value: {
      uid: item["uid"],
      status: item["status"] ?? null,
      priority: item["priority"] ?? null,
      namespace: item["namespace"] ?? null,
      content: item["content"] ?? "",
      tags: item["tags"] as TaskDetail["tags"],
      transitions,
      review_namespaces: namespaces,
      findings: findings.map((f) => ({
        ...f,
        step_title: f.step_title ?? null,
        design: f.design ?? null,
      })),
      found_in: foundIn,
    },
  };
}
