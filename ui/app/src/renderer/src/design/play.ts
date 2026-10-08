//! *Play* (D53.6) as terminal specs: a Claude session in the container started with the prompt
//! `design.prompt` built — the text `jkb design prompt play|task` prints — and recorded as one of the
//! design's prompts before it starts (`launch.ts`).
//
// Pure, so the commands they run are pinned by a test. As with *Discuss*, the prompt is never
// assembled here, and everything that varies is a positional parameter, never spliced into the
// script.

import { decodeWorkPrompt, playPins } from "@jkb/core";
import type { OpRequest, OpResponse, Outcome, PlanTask, WorkPrompt } from "@jkb/core";

import type { TerminalRoots, TerminalSpec } from "../../../shared/terminal";
import { launchSpec } from "./launch";

export { PLAY_TASK_SCRIPT } from "./launch";

/** One op through the bridge. */
export type Op = (request: OpRequest) => Promise<Outcome<OpResponse>>;

/**
 * Pin `tasks` that are not yet on `strategy` (a `workflow.strategies` name) — the operator's
 * explicit choice, made before the work starts so the tasks' gates are the chosen ones — then ask
 * for the prompt. Pins one at a time and stops at the first refusal, naming the task, so nothing
 * starts under a strategy that was only half applied. With no strategy chosen (`undefined`: the
 * picker left on the default) nothing is pinned — `playPins`.
 */
export async function pinThenPrompt(
  op: Op,
  tasks: readonly PlanTask[],
  strategy: string | undefined,
  prompt: OpRequest,
): Promise<Outcome<WorkPrompt>> {
  for (const pin of playPins(tasks, strategy)) {
    const pinned = await op(pin);
    if (!pinned.ok) {
      return { ok: false, error: { ...pinned.error, message: `pinning ${pin.uid} to ${pin.strategy}: ${pinned.error.message}` } };
    }
  }
  return decodeWorkPrompt(await op(prompt));
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
