//! *Discuss* (D53.5) as a terminal spec: a Claude session in the container, in the design's repo,
//! started with the prompt `design.prompt` built — the same text `jkb design prompt discuss` prints —
//! and recorded as one of the design's prompts before it starts (`launch.ts`).
//
// Pure, so the command it runs is pinned by a test. The prompt is never assembled here: the app
// only carries it (D53.5, "the prompt is CLI output, not app string-building").

import type { DesignPrompt } from "@jkb/core";

import type { TerminalRoots, TerminalSpec } from "../../../shared/terminal";
import { launchSpec } from "./launch";

export { repoDir } from "./launch";

/**
 * The terminal a *Discuss* opens. `sessionUuid` is minted by the caller and handed to Claude as its
 * session id, so the terminal and the session are the same thing to every later lookup — and a
 * second *Discuss* of the same id shows this terminal instead of starting another.
 */
export function discussSpec(prompt: DesignPrompt, repo: string, roots: TerminalRoots, sessionUuid: string): TerminalSpec {
  return launchSpec(
    { design: prompt.uid, launch: "discuss", label: "Discuss", title: prompt.title, prompt: prompt.prompt },
    repo,
    roots,
    sessionUuid,
  );
}

/**
 * `run`, one at a time: a call made while the last is still pending is ignored, not queued. A
 * *Discuss* waits for every edit to be saved and the design read back — seconds, or as long as the
 * daemon is away — and a second click in that window would start a second Claude on one selection.
 */
export function exclusive<A extends unknown[]>(run: (...args: A) => Promise<void>): (...args: A) => Promise<void> {
  let pending = false;
  return async (...args: A): Promise<void> => {
    if (pending) return;
    pending = true;
    try {
      await run(...args);
    } finally {
      pending = false;
    }
  };
}
