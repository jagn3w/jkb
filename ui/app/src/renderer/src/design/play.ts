//! *Play* (D53.6) as terminal specs: a Claude session in the container started with the prompt
//! `design.prompt` built — the text `jkb design prompt play|task` prints.
//
// Pure, so the commands they run are pinned by a test. As with *Discuss*, the prompt is never
// assembled here, and everything that varies is a positional parameter, never spliced into the
// script.

import { decodeWorkPrompt, playPins } from "@jkb/core";
import type { OpRequest, OpResponse, Outcome, PlanTask, WorkPrompt } from "@jkb/core";

import type { TerminalRoots, TerminalSpec } from "../../../shared/terminal";
import { DISCUSS_SCRIPT, repoDir } from "./discuss";

/**
 * *Play* on a task: `jkb task work` opens (or resumes) the task's own worktree and claims it, and
 * Claude starts there. `jq` reads the worktree from the JSON answer (it is in the image), skipping
 * any line that is not JSON — `task work` prints a note before its answer when it cancels a pending
 * removal. A refusal, or an answer naming no worktree, stops the script before Claude starts, its
 * message left on the terminal (`pipefail`: `jkb`'s own failure, not only `jq`'s).
 */
export const PLAY_TASK_SCRIPT =
  'set -euo pipefail; dir=$(jkb --json task work "$1" | jq -Rer \'fromjson? | objects | .worktree | strings\'); cd "$dir"; exec claude --session-id "$2" "$3"';

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

function titled(label: string, title: string): string {
  const t = `${label} · ${title}`;
  return t.length > 200 ? `${t.slice(0, 199)}…` : t;
}

/** The terminal a plan's *Play* opens: Claude in the design's repo, with the plan's prompt. */
export function playPlanSpec(prompt: WorkPrompt, repo: string, roots: TerminalRoots, sessionUuid: string): TerminalSpec {
  return {
    target: "container",
    cwd: repoDir(roots, repo),
    argv: ["/bin/bash", "-lc", DISCUSS_SCRIPT, "claude", sessionUuid, prompt.prompt],
    title: titled("Play", prompt.title),
    sessionUuid,
  };
}

/** The terminal a task's *Play* opens: its session worktree, then Claude in it with the task's prompt. */
export function playTaskSpec(prompt: WorkPrompt, repo: string, roots: TerminalRoots, sessionUuid: string): TerminalSpec {
  return {
    target: "container",
    cwd: repoDir(roots, repo),
    argv: ["/bin/bash", "-lc", PLAY_TASK_SCRIPT, "play", prompt.uid, sessionUuid, prompt.prompt],
    title: titled("Play", prompt.title),
    sessionUuid,
  };
}
