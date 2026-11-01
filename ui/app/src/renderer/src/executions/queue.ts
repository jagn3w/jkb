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

/** The most pages of `exec.list` the queue reads at once (up to 100 requests a page). */
export const MAX_PAGES = 20;

/** Requests listed, and whether the read stopped at `MAX_PAGES` with more left unread. */
export interface Listed {
  readonly requests: readonly ExecView[];
  readonly cut: boolean;
}

/**
 * Every request in `state` (or all), page by page, up to `MAX_PAGES` — oldest first, or newest first
 * when `newest` — stopping early once `enough` says the rows read so far suffice.
 */
export async function listAll(
  op: (request: OpRequest) => Promise<Outcome<OpResponse>>,
  state: string | undefined,
  options: { readonly newest?: boolean; readonly enough?: (rows: readonly ExecView[]) => boolean } = {},
): Promise<Outcome<Listed>> {
  const out: ExecView[] = [];
  let after: number | undefined;
  for (let page = 0; page < MAX_PAGES; page++) {
    const r = await op({
      op: "exec.list",
      ...(state === undefined ? {} : { state }),
      ...(after === undefined ? {} : { after }),
      ...(options.newest === true ? { newest_first: true } : {}),
    });
    if (!r.ok) return r;
    const d = decodeExecRequests(r.value);
    if (!d.ok) return d;
    out.push(...d.value.requests);
    if (d.value.next === undefined) return { ok: true, value: { requests: out, cut: false } };
    if (options.enough?.(out) === true) return { ok: true, value: { requests: out, cut: false } };
    after = d.value.next;
  }
  return { ok: true, value: { requests: out, cut: true } };
}

/** How many requests the history shows. */
export const HISTORY_SHOWN = 50;

/** The history from a newest-first read: requests that left the queue for good, at most `limit`. */
export function history(newestFirst: readonly ExecView[], limit = HISTORY_SHOWN): readonly ExecView[] {
  return newestFirst.filter((v) => EXEC_ENDED_STATES.includes(v.state)).slice(0, limit);
}

/** Whether a newest-first read holds enough of the history to show. */
export function historyFull(rows: readonly ExecView[]): boolean {
  return history(rows).length >= HISTORY_SHOWN;
}

/**
 * How each line of a unified diff is drawn: `diff `, `index `, `---` and `+++` are a file's header
 * only between its `diff ` line and its first `@@`; inside a hunk a line starting `+++` is an added
 * `++…` line, and `---` a removed `--…` one.
 */
export function diffLineKinds(lines: readonly string[]): readonly ("meta" | "hunk" | "add" | "del" | "ctx")[] {
  let header = false;
  return lines.map((line) => {
    if (line.startsWith("diff ")) {
      header = true;
      return "meta";
    }
    if (line.startsWith("@@")) {
      header = false;
      return "hunk";
    }
    if (header) return "meta";
    if (line.startsWith("+")) return "add";
    if (line.startsWith("-")) return "del";
    return "ctx";
  });
}
