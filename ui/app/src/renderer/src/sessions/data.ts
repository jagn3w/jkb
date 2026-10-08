//! The Sessions tab's reads (D53.9), over an op call so they are tested with a stand-in daemon.

import {
  decodeNotified,
  decodeSessionPage,
  failed,
  sessionOps,
  type NotifyRecord,
  type OpRequest,
  type OpResponse,
  type Outcome,
  type SessionHolder,
} from "@jkb/core";

export type Op = (request: OpRequest) => Promise<Outcome<OpResponse>>;

/** At most this many `session.list` pages are read for one listing. */
export const MAX_PAGES = 10;

/** The registry's rows: every live one, or (`all`) the most recently seen — and whether more were left unread. */
export interface Holders {
  readonly holders: readonly SessionHolder[];
  readonly truncated: boolean;
}

export async function loadHolders(op: Op, all: boolean): Promise<Outcome<Holders>> {
  const holders: SessionHolder[] = [];
  let after: string | undefined;
  for (let page = 0; page < MAX_PAGES; page++) {
    const answer = decodeSessionPage(await op(sessionOps.list(all, after)));
    if (!answer.ok) return answer;
    holders.push(...answer.value.holders);
    if (answer.value.next === null) return { ok: true, value: { holders, truncated: false } };
    // A page that ends where it began would loop forever; the daemon never sends one, but say so.
    if (answer.value.next === after) return failed("internal", "session.list repeated a page");
    after = answer.value.next;
  }
  return { ok: true, value: { holders, truncated: true } };
}

export async function loadNotified(op: Op): Promise<Outcome<readonly NotifyRecord[]>> {
  return decodeNotified(await op(sessionOps.notified()));
}

/**
 * Where each live session runs, by session id: its most recently seen live process's directory, as
 * its hooks reported it. What re-attach resumes in.
 */
export function liveCwds(holders: readonly SessionHolder[]): Map<string, string> {
  const out = new Map<string, { cwd: string; seenAt: number }>();
  for (const h of holders) {
    if (h.endedAt !== null || h.cwd === "") continue;
    const had = out.get(h.session);
    if (had === undefined || h.seenAt > had.seenAt) out.set(h.session, { cwd: h.cwd, seenAt: h.seenAt });
  }
  return new Map([...out].map(([s, v]) => [s, v.cwd]));
}
