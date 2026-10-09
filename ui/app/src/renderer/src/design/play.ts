//! *Play* (D53.6) as terminal specs: a Claude session in the container started with the prompt
//! `design.prompt` built — the text `jkb design prompt play|task` prints — and recorded as one of the
//! design's prompts before it starts (`launch.ts`).
//
// Pure, so the commands they run are pinned by a test. As with *Discuss*, the prompt is never
// assembled here, and everything that varies is a positional parameter, never spliced into the
// script.

import { decodePlanList, decodeStrategies, decodeWorkPrompt, pickOf, planOps, playPins } from "@jkb/core";
import type { OpRequest, OpResponse, Outcome, PlanList, PlanTask, WorkPrompt } from "@jkb/core";

import type { TerminalRoots, TerminalSpec } from "../../../shared/terminal";
import { launchSpec } from "./launch";

export { PLAY_TASK_SCRIPT } from "./launch";

/** One op through the bridge. */
export type Op = (request: OpRequest) => Promise<Outcome<OpResponse>>;

/** What a *Play* works: the design it is in, and which of its tasks (from a fresh listing) it starts. */
export interface PlayTarget {
  readonly design: string;
  /** The tasks the Play starts, picked out of a `design.plans` answer; `undefined` when gone. */
  readonly tasks: (list: PlanList) => readonly PlanTask[] | undefined;
  /** What to call it in a refusal. */
  readonly what: string;
}

/**
 * Pin the target's open tasks that are not yet on `picked` (a bare strategy name: the operator's
 * explicit choice), then ask for the prompt — so the tasks' gates are the chosen ones before the work
 * starts. Both what the tasks run and what the name resolves to are read fresh, right before pinning:
 * a pane's listing is not live (D53.6), and a task Claude added since, or a definition redefined
 * since, would otherwise be skipped or repinned. Pins one at a time and stops at the first refusal,
 * naming the task, so nothing starts under a strategy that was only half applied. With nothing picked
 * (`undefined`: the picker left on "each task's own") nothing is read or pinned — `playPins`.
 */
export async function pinThenPrompt(
  op: Op,
  target: PlayTarget,
  picked: string | undefined,
  prompt: OpRequest,
): Promise<Outcome<WorkPrompt>> {
  if (picked !== undefined) {
    const strategies = decodeStrategies(await op(planOps.strategies()));
    if (!strategies.ok) return strategies;
    const pick = pickOf(strategies.value, picked);
    if (pick === undefined) {
      return { ok: false, error: { code: "invalid", message: `the strategy ${picked} is no longer listed; pick again` } };
    }
    const listed = decodePlanList(await op(planOps.plans(target.design)));
    if (!listed.ok) return listed;
    const tasks = target.tasks(listed.value);
    if (tasks === undefined) {
      return { ok: false, error: { code: "not_found", message: `${target.what} is no longer in the design; refresh` } };
    }
    for (const pin of playPins(tasks, pick)) {
      const pinned = await op(pin);
      if (!pinned.ok) {
        return { ok: false, error: { ...pinned.error, message: `pinning ${pin.uid} to ${pin.strategy}: ${pinned.error.message}` } };
      }
    }
  }
  return decodeWorkPrompt(await op(prompt));
}

/** A plan's *Play* target: its tasks, in step order. */
export function planTarget(design: string, plan: string): PlayTarget {
  return {
    design,
    what: plan,
    tasks: (list) => list.plans.find((p) => p.uid === plan)?.steps.flatMap((s) => s.tasks),
  };
}

/** A task's *Play* target: that task, under a plan's step or a one-off. */
export function taskTarget(design: string, uid: string): PlayTarget {
  return {
    design,
    what: uid,
    tasks: (list) => {
      const t = [...list.plans.flatMap((p) => p.steps.flatMap((s) => s.tasks)), ...list.tasks].find((x) => x.uid === uid);
      return t === undefined ? undefined : [t];
    },
  };
}

/** The terminal a plan's *Play* opens: Claude in the design's repo, with the plan's prompt. */
export function playPlanSpec(
  prompt: WorkPrompt,
  design: string,
  repo: string,
  roots: TerminalRoots,
  sessionUuid: string,
): TerminalSpec {
  return launchSpec(
    { design, launch: "play", subject: prompt.uid, label: "Play", title: prompt.title, prompt: prompt.prompt },
    repo,
    roots,
    sessionUuid,
  );
}

/**
 * The terminal a task's *Play* opens: its session worktree (`jkb task work`), then Claude in it with
 * the task's prompt. `design` is the design whose Tasks pane played it: a session the Design tab
 * starts is always one of its design's prompts, whatever the task's own place.
 */
export function playTaskSpec(
  prompt: WorkPrompt,
  design: string,
  repo: string,
  roots: TerminalRoots,
  sessionUuid: string,
): TerminalSpec {
  return launchSpec(
    { design, launch: "task", subject: prompt.uid, label: "Play", title: prompt.title, prompt: prompt.prompt },
    repo,
    roots,
    sessionUuid,
  );
}
