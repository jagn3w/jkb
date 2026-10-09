//! What the Sessions tab's button does for a session (D53.9): show the app's terminal running it,
//! resume it in the container with `claude --resume=<id>`, or — for a session that ran on the host —
//! say so and nothing more. The app runs no program on the host, and offers none to run.
//
// Pure, so the rules are tested without a window. The facts that decide, and why each is trusted
// only as far as it is:
//
// - **The app's own terminals come first.** One starting or running the session owns it — the rule
//   re-attach records by (`isAttached` in `reattach.ts`) — and is shown. One whose program ended is
//   the app's own record that the session ran in the container: Resume relaunches it there, in that
//   terminal (`terminals.open` starts an ended one again in place), and no registry row can say
//   otherwise.
// - **The registry decides only for a session the app has no terminal for**, and its rows are
//   written by the session's own hooks, which a container process can forge. A holder whose `instance`
//   names a boot (`#…`, which the dev container always records) is resumed in the container, where a
//   forged row can reach nothing the container could not already. Any other is reported as having run
//   on the host, with no command and no directory: **the app runs no program on the host** (the
//   operator's rule, D53.9), and a command built from a forged row is one the operator would be
//   handed to run. A holder with no instance or no directory is refused with the reason.
// - **The id** is put on a command line only when it is a lowercase uuid (`sessionResumeSpec` refuses
//   anything else, and passes it as `--resume=<id>`).

import { isSessionUuid, type DesignPromptRecord, type SessionHolder, type SessionRow } from "@jkb/core";

import type { TerminalRoots, TerminalSpec } from "../../../shared/terminal";
import { PLAY_TASK_SCRIPT, resumedDir, sessionResumeSpec } from "../design/launch";
import { mayBeLive, type TerminalEntry } from "../terminal/state";

/** Whether a terminal's program may be running (`mayBeLive`): the only kind that owns a session. */
export function isLive(e: TerminalEntry): boolean {
  return mayBeLive(e.status);
}

/** The terminal starting or running each session, by session id. */
export function owningTerminals(entries: readonly TerminalEntry[]): Map<string, TerminalEntry> {
  const out = new Map<string, TerminalEntry>();
  for (const e of entries) if (e.spec.sessionUuid !== undefined && isLive(e)) out.set(e.spec.sessionUuid, e);
  return out;
}

/** Where a holder ran: `container` (its instance names a boot), `host`, or `undefined` when it does not say. */
export function sideOf(holder: SessionHolder): "container" | "host" | undefined {
  if (holder.instance === "") return undefined;
  return holder.instance.includes("#") ? "container" : "host";
}

/** What the button does. */
export type SessionAction =
  /** A terminal of the app is running it: bring that terminal forward. */
  | { readonly kind: "show"; readonly key: number }
  /** Resume it in the container: in `key` (an ended terminal of this session) when given, else a new terminal. */
  | { readonly kind: "resume"; readonly spec: TerminalSpec; readonly key?: number }
  /** It ran on the host: said, and nothing more. The app runs nothing on the host, and offers nothing to. */
  | { readonly kind: "host" }
  /** It cannot be resumed from here, and why. */
  | { readonly kind: "refused"; readonly why: string };

/** What the tab says for a session that ran on the host. */
export const HOST_SESSION_NOTE = "This session ran on the host; resume it from a terminal there.";

/**
 * The action for `row`. `prompt` is its design prompt, if it was launched from one: a session neither
 * the app nor the registry has a process for is resumed where its prompt was recorded, in the
 * container, exactly as the Design tab's own Resume does (`resumeSpec`).
 */
export function sessionAction(
  row: SessionRow,
  prompt: DesignPromptRecord | null,
  entries: readonly TerminalEntry[],
  roots: TerminalRoots,
  title: string,
): SessionAction {
  const notUuid: SessionAction = { kind: "refused", why: "Its id is not a lowercase session uuid." };
  if (!isSessionUuid(row.session)) return notUuid;
  const owner = owningTerminals(entries).get(row.session);
  if (owner !== undefined) return { kind: "show", key: owner.key };
  const resume = (cwd: string, key?: number): SessionAction => {
    const spec = sessionResumeSpec({ session: row.session, cwd, title }, roots);
    if (spec === undefined) return notUuid;
    return key === undefined ? { kind: "resume", spec } : { kind: "resume", spec, key };
  };
  // The holder `row.cwd` came from: the most recently seen live one, else the most recent.
  const lead = row.holders.find((h) => h.endedAt === null) ?? row.holders[0];
  const containerCwd = lead !== undefined && sideOf(lead) === "container" && lead.cwd !== "" ? lead.cwd : undefined;

  // The app's own record: an ended terminal of this session ran it in the container.
  const ended = entries.find((e) => e.spec.sessionUuid === row.session);
  if (ended !== undefined) {
    // Where it runs: a resume's own directory; a task's Play moved into its worktree after its
    // terminal opened, so only the registry (a container-side row) or its prompt can say where; any
    // other launch ran in its terminal's directory.
    const cwd =
      resumedDir(ended.spec) ??
      (ended.spec.argv[2] === PLAY_TASK_SCRIPT ? (containerCwd ?? (prompt?.cwd || undefined)) : ended.spec.cwd);
    if (cwd === undefined) return { kind: "refused", why: "Nothing records where this task's Play ran in the container." };
    return resume(cwd, ended.key);
  }

  if (lead === undefined) {
    if (prompt === null || prompt.cwd === "") {
      return { kind: "refused", why: "The registry has no process for this session, so where it ran is unknown." };
    }
    return resume(prompt.cwd);
  }
  switch (sideOf(lead)) {
    case "host":
      return { kind: "host" };
    case undefined:
      return { kind: "refused", why: "The registry does not say whether this session ran on the host or in the container." };
    case "container":
      return containerCwd === undefined
        ? { kind: "refused", why: "The registry has no directory for this session, and claude --resume finds a session by its directory." }
        : resume(containerCwd);
  }
}
