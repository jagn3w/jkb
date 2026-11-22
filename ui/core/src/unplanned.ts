//! The open tasks no plan step reaches, by repo, as `task.unplanned` answers them: what the Design
//! sidebar counts on each repo row and lists in its Unplanned fold, so a task that escaped the
//! write-side rule (every task is filed under a step) is shown rather than lost.
//
// The Rust side is the source of truth: `crates/jkb-api/src/tasks.rs` (`UnplannedList`), over
// `jkb_core::design::plan::unplanned` — the set `jkb doctor` and `is:unplanned` count. Which repo a
// task is in is derived there (`mount::repo_of_namespace`), never here.

import type { OpResponse, Outcome } from "./daemon.js";
import { failed } from "./daemon.js";
import { decodePlanTaskValue, type PlanTask } from "./plan.js";

/** `jkb_api::tasks::UnplannedTask`: the task in the Tasks pane's shape, and where it is placed. */
export interface UnplannedTask {
  readonly task: PlanTask;
  readonly namespace: string | null;
}

/** `jkb_api::tasks::UnplannedRepo`: one repo's unplanned tasks (`null`: in no repo). */
export interface UnplannedRepo {
  readonly repo: string | null;
  /** How many it has in all; `tasks` is the first of them. */
  readonly count: number;
  readonly tasks: readonly UnplannedTask[];
}

/** `task.unplanned`'s answer. */
export interface UnplannedList {
  readonly count: number;
  readonly repos: readonly UnplannedRepo[];
}

/** The request, as `jkb_api::Request` serializes it. */
export const unplannedOps = {
  list: (repo?: string) => (repo === undefined ? { op: "task.unplanned" } : { op: "task.unplanned", repo }),
} as const;

/** The label the sidebar gives the tasks in no repo. */
export const NO_REPO_LABEL = "Elsewhere";

/** Repo `repo`'s entry (`null`: the tasks in no repo), or `undefined` when it has none. */
export function unplannedOf(list: UnplannedList | undefined, repo: string | null): UnplannedRepo | undefined {
  return list?.repos.find((r) => r.repo === repo);
}

/** The repo's triage step, where Move to step's first option files a task (`step:inbox@<repo>`). */
export function inboxStep(repo: string): string {
  return `step:inbox@${repo}`;
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

const isCount = (v: unknown): v is number => typeof v === "number" && Number.isInteger(v) && v >= 0;

function isRepo(v: unknown): v is UnplannedRepo {
  return (
    isObject(v) &&
    (v["repo"] === null || v["repo"] === undefined || typeof v["repo"] === "string") &&
    isCount(v["count"]) &&
    Array.isArray(v["tasks"]) &&
    v["tasks"].every(
      (t) =>
        isObject(t) &&
        decodePlanTaskValue(t["task"]) !== undefined &&
        (t["namespace"] === null || t["namespace"] === undefined || typeof t["namespace"] === "string"),
    )
  );
}

/** `task.unplanned`, checked; a missing `repo` or `namespace` reads as `null`. */
export function decodeUnplanned(o: Outcome<OpResponse>): Outcome<UnplannedList> {
  if (!o.ok) return o;
  const v: unknown = o.value.result === "unplanned" ? o.value : undefined;
  if (!isObject(v) || !isCount(v["count"]) || !Array.isArray(v["repos"]) || !v["repos"].every(isRepo)) {
    return failed("internal", "jkb serve did not answer with a well-formed `unplanned`");
  }
  const repos = (v["repos"] as readonly UnplannedRepo[]).map((r) => ({
    repo: r.repo ?? null,
    count: r.count,
    tasks: r.tasks.map((t) => ({ task: t.task, namespace: t.namespace ?? null })),
  }));
  return { ok: true, value: { count: v["count"], repos } };
}
