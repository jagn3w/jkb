//! Running one of the Container tab's actions (D53.8), minus React: the confirmation, main's terminal
//! spec, the re-attach record, and the terminal — with one action at a time, from the click until its
//! terminal ends.
//
// Pure apart from what is injected, so the order and the gate are tested without a window.

import { CONTAINER_ACTIONS, type ContainerAction, type ContainerResult } from "@jkb/core";

import type { TerminalSpec } from "../../../shared/terminal";

/** What running an action reaches outside itself. */
export interface ActionRunDeps {
  confirm(message: string): boolean;
  /** Main's terminal spec for the action (the kit's run.sh and its flag). */
  spec(action: ContainerAction): Promise<ContainerResult<TerminalSpec>>;
  /** Called after the spec and before the terminal opens: the re-attach record, for an action that tears down. */
  beforeOpen(action: ContainerAction): Promise<void>;
  /** Open the terminal (to run once); its key. */
  open(spec: TerminalSpec): number;
  notice(message: string | undefined): void;
}

/**
 * One action at a time. Taken SYNCHRONOUSLY, before the first await: the tab's `running` state is set
 * only after main has answered and the sessions were read, so a second click on Build in between saw
 * nothing running and started a second run.sh against the same container, whose end then went
 * unnoticed (review s8 round 2). Held from the click until the tab releases it when the run's terminal
 * ends; released at once by every path that opens no terminal.
 */
export class ActionGate {
  private held = false;

  get busy(): boolean {
    return this.held;
  }

  take(): boolean {
    if (this.held) return false;
    this.held = true;
    return true;
  }

  release(): void {
    this.held = false;
  }
}

/** Run `action`: the terminal's key, or `undefined` when nothing was opened (busy, declined, refused). */
export async function runAction(
  action: ContainerAction,
  gate: ActionGate,
  deps: ActionRunDeps,
): Promise<{ readonly key: number; readonly action: ContainerAction } | undefined> {
  const spec = CONTAINER_ACTIONS.find((a) => a.id === action);
  if (spec === undefined || !gate.take()) return undefined;
  let opened = false;
  try {
    if (spec.ends && !deps.confirm(`${spec.label} the container? ${spec.summary}`)) return undefined;
    deps.notice(undefined);
    const answer = await deps.spec(action);
    if (!answer.ok) {
      deps.notice(answer.error);
      return undefined;
    }
    await deps.beforeOpen(action);
    const key = deps.open(answer.value);
    opened = true;
    return { key, action };
  } finally {
    if (!opened) gate.release();
  }
}
