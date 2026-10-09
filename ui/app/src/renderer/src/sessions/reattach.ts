//! Re-attach after a rebuild (D53.9): before the Container tab tears the container down it records
//! the live sessions the app owns — each terminal running a Claude session in the container — and
//! after the rebuild it relaunches each as `claude --resume <uuid>` in its terminal.
//
// Pure, so the rules are tested without a window. What "the app owns" means: a terminal the app
// opened with a `sessionUuid` (every launch and resume does), on the container target, still
// running. A session another editor started is listed by the Sessions tab and can be resumed there,
// but never re-attached: the app does not own its process.

import type { ContainerAction } from "@jkb/core";

import type { TerminalRoots, TerminalSpec } from "../../../shared/terminal";
import { resumedDir, sessionResumeSpec } from "../design/launch";
import { mayBeLive, type TerminalEntry } from "../terminal/state";

/** The Container tab's actions that end the container's processes: rebuild, stop, remove. */
export const TEARS_DOWN: readonly ContainerAction[] = ["build", "stop", "remove"];

export function tearsDown(action: ContainerAction): boolean {
  return TEARS_DOWN.includes(action);
}

/** One app-owned session, as recorded before the container was torn down. */
export interface Attached {
  /** The terminal it runs in. */
  readonly key: number;
  /** Its Claude Code session id. */
  readonly session: string;
  /** Where it runs — where `claude --resume` must, since a session is found by its directory. */
  readonly cwd: string;
  readonly target: "container";
  readonly title: string;
}

/** Whether a terminal is a live app-owned container session: what a teardown ends and a rebuild re-attaches. */
function isAttached(e: TerminalEntry): e is TerminalEntry & { readonly spec: TerminalSpec & { readonly sessionUuid: string } } {
  return (
    e.spec.sessionUuid !== undefined &&
    e.spec.target === "container" &&
    mayBeLive(e.status)
  );
}

/**
 * The live app-owned sessions, recorded before a teardown. `cwdOf` is where the session really runs —
 * the session registry's directory (a task's *Play* moves into its worktree after the terminal
 * starts, so the spec's directory is not it). When the registry does not know: the directory a
 * resume moves into (its spec starts at the repos mount's root), else the terminal's own directory.
 */
export function recordAttached(
  entries: readonly TerminalEntry[],
  cwdOf: (session: string) => string | undefined,
): Attached[] {
  return entries.filter(isAttached).map((e) => ({
    key: e.key,
    session: e.spec.sessionUuid,
    cwd: cwdOf(e.spec.sessionUuid) || resumedDir(e.spec) || e.spec.cwd,
    target: "container",
    title: e.spec.title,
  }));
}

/** What to do with one recorded session once the teardown's run has ended. */
export type Reattach =
  /** Its terminal ended: relaunch it there as `claude --resume`. */
  | { readonly kind: "relaunch"; readonly key: number; readonly spec: TerminalSpec }
  /** Its terminal was closed meanwhile: open a new one for it. */
  | { readonly kind: "open"; readonly spec: TerminalSpec }
  /** Its terminal is still running (the teardown did not reach it): leave it alone. */
  | { readonly kind: "survived"; readonly key: number };

/**
 * The plan for the recorded sessions after a teardown, given the terminals as they stand and whether
 * the container is running again. `undefined` while it is not (a Stop or a Remove): the record is
 * kept for the next Build that leaves it running.
 */
export function reattachPlan(
  recorded: readonly Attached[],
  entries: readonly TerminalEntry[],
  running: boolean,
  roots: TerminalRoots,
): Reattach[] | undefined {
  if (!running) return undefined;
  return recorded.map((a) => {
    const spec = sessionResumeSpec({ session: a.session, cwd: a.cwd, title: a.title }, roots);
    const entry = entries.find((e) => e.key === a.key && e.spec.sessionUuid === a.session);
    if (entry === undefined) return { kind: "open", spec };
    if (mayBeLive(entry.status)) return { kind: "survived", key: a.key };
    return { kind: "relaunch", key: a.key, spec };
  });
}

/** Records merged: a session recorded again (a second teardown before a Build) keeps its newest record. */
export function mergeRecords(older: readonly Attached[], newer: readonly Attached[]): Attached[] {
  const seen = new Set(newer.map((a) => a.session));
  return [...older.filter((a) => !seen.has(a.session)), ...newer];
}
