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
  readonly describe: string;
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
  /**
   * Replace a task's text, or append a note to it. A replace names the text it was made against
   * (`expected`), and jkb refuses it as `stale` when the task's text is no longer that.
   */
  edit: (uid: string, text: string, append: boolean, expected?: string) =>
    expected === undefined ? { op: "task.edit", uid, text, append } : { op: "task.edit", uid, text, append, expected },
} as const;

/**
 * The name `workflow.set` and *Play* resolve: a definition is listed as `name@version` and
 * resolved by `name` (its newest version); a preset is its name.
 */
export function strategyName(listed: string): string {
  const at = listed.lastIndexOf("@");
  return at > 0 ? listed.slice(0, at) : listed;
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
    isString(v["strategy"])
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
        (s) => isObject(s) && isString(s["name"]) && typeof s["preset"] === "boolean" && isString(s["describe"]),
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
    !transitions.every(isTransition)
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
    },
  };
}
