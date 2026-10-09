//! What a terminal knows about how its earlier programs ended (D53.10).
//
// Pure (no DOM, no bridge), so it is tested without a window. Every end a terminal sends is
// recorded here AS IT IS SENT, so a later start waits for it even when that start did not send it
// (a quick re-toggle, a Resume on the same tab); and an end that was not confirmed stays recorded
// until an explicit Restart overrides it, so nothing starts a second program beside one that may
// still be running.

import type { TerminalEnd } from "../../../shared/terminal";

/** An unconfirmed end if any recorded end was unconfirmed, else the latest one. */
function worst(a: TerminalEnd | undefined, b: TerminalEnd | undefined): TerminalEnd | undefined {
  if (a !== undefined && !a.confirmed) return a;
  return b ?? a;
}

export class EndRecord {
  private settledEnd: Promise<TerminalEnd | undefined> = Promise.resolve(undefined);

  /** Record an end in flight. Returns `end` itself, so a caller can still await its own. */
  record(end: Promise<TerminalEnd | undefined>): Promise<TerminalEnd | undefined> {
    const safe = end.catch((e: unknown): TerminalEnd => ({
      target: "container",
      confirmed: false,
      detail: `could not end it: ${e instanceof Error ? e.message : String(e)}`,
    }));
    const before = this.settledEnd;
    this.settledEnd = Promise.all([before, safe]).then(([a, b]) => worst(a, b));
    return safe;
  }

  /** Every end recorded so far, settled: an unconfirmed one if any was, else the latest (or none). */
  settled(): Promise<TerminalEnd | undefined> {
    return this.settledEnd;
  }

  /** Forget what was recorded: the person chose to start anyway (Restart). */
  clear(): void {
    this.settledEnd = Promise.resolve(undefined);
  }
}

/** Whether a start may go ahead after `end`: unless it was unconfirmed and nobody overrode it. */
export function mayStartAfter(end: TerminalEnd | undefined, override: boolean): boolean {
  return override || end === undefined || end.confirmed;
}
