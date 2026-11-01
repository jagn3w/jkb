//! The request queue's rules, kept apart from React so they are tested without a window: what
//! Decline sends (never without a note), when Run may be clicked, the one-click gate both buttons
//! take synchronously, and how the history is gathered from `exec.list` pages.

import { EXEC_ENDED_STATES, decodeExecRequests, execId, type ExecView, type OpRequest, type OpResponse, type Outcome } from "@jkb/core";

import type { ExecPreview } from "../../../shared/bridge";

/** `exec.decline` for `id` with `note`, or why not: a decline always tells the session what to ask instead. */
export function declineOp(id: string, note: string): { readonly ok: true; readonly request: OpRequest } | { readonly ok: false; readonly error: string } {
  if (execId(id) === undefined) return { ok: false, error: `not a run request id: ${id}` };
  const trimmed = note.trim();
  if (trimmed === "") return { ok: false, error: "A decline needs a note for the session: say what to ask for instead." };
  return { ok: true, request: { op: "exec.decline", id, note: trimmed } };
}

/** Whether Run may be clicked for this preview, and if not, why. */
export function runBlocked(preview: ExecPreview | undefined): string | undefined {
  if (preview === undefined) return "Reading what would run…";
  if (preview.request.state !== "pending") return `${preview.request.id} is ${preview.request.state}`;
  if (preview.problem !== undefined) return preview.problem;
  if (preview.hostDir === undefined) return "The directory could not be resolved";
  return undefined;
}

/**
 * One click per request: taken synchronously on click, before any await, so a double click — or Run
 * and Decline together — sends one thing. Released only when what it started failed.
 */
export class ClickGate {
  private readonly held = new Set<string>();

  take(id: string): boolean {
    if (this.held.has(id)) return false;
    this.held.add(id);
    return true;
  }

  release(id: string): void {
    this.held.delete(id);
  }

  holds(id: string): boolean {
    return this.held.has(id);
  }
}

/** The most pages of `exec.list` the queue reads at once (100 requests a page). */
export const MAX_PAGES = 20;

/** Every request in `state` (or all), page by page, up to `MAX_PAGES`. */
export async function listAll(
  op: (request: OpRequest) => Promise<Outcome<OpResponse>>,
  state: string | undefined,
): Promise<Outcome<readonly ExecView[]>> {
  const out: ExecView[] = [];
  let after: number | undefined;
  for (let page = 0; page < MAX_PAGES; page++) {
    const r = await op({ op: "exec.list", ...(state === undefined ? {} : { state }), ...(after === undefined ? {} : { after }) });
    if (!r.ok) return r;
    const d = decodeExecRequests(r.value);
    if (!d.ok) return d;
    out.push(...d.value.requests);
    if (d.value.next === undefined) break;
    after = d.value.next;
  }
  return { ok: true, value: out };
}

/** The history: requests that left the queue for good, newest first, at most `limit`. */
export function history(all: readonly ExecView[], limit = 50): readonly ExecView[] {
  return all.filter((v) => EXEC_ENDED_STATES.includes(v.state)).reverse().slice(0, limit);
}
