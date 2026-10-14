//! The Sessions tab's reads (D53.9), over an op call so they are tested with a stand-in daemon.

import {
  decodeDesignPrompts,
  decodeDesigns,
  decodeNotified,
  decodeSessionPage,
  designOps,
  failed,
  promptOps,
  repoOf,
  sessionOps,
  type DesignPromptRecord,
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

/** Every live design's prompts, and each design's title by uid: what names a row launched from a design. */
export interface Prompts {
  readonly prompts: readonly DesignPromptRecord[];
  readonly titles: ReadonlyMap<string, string>;
}

/**
 * Every design's prompts: the designs listed once, then each repo's prompts in one read
 * (`design.prompts` by repo) — a read per repo, not per design.
 */
export async function loadPrompts(op: Op): Promise<Outcome<Prompts>> {
  const designs = decodeDesigns(await op(designOps.list()));
  if (!designs.ok) return designs;
  const repos = [...new Set(designs.value.map((d) => repoOf(d.namespace)).filter((r): r is string => r !== undefined))].sort();
  const answers = await Promise.all(repos.map(async (repo) => decodeDesignPrompts(await op(promptOps.listRepo(repo)))));
  const prompts: DesignPromptRecord[] = [];
  for (const a of answers) {
    if (!a.ok) return a;
    prompts.push(...a.value);
  }
  return { ok: true, value: { prompts, titles: new Map(designs.value.map((d) => [d.uid, d.title])) } };
}

/** How a row launched from a design is named: its design's title and its prompt's (which names the subject). */
export function promptName(prompt: DesignPromptRecord, titles: ReadonlyMap<string, string>): string {
  return `${titles.get(prompt.design) ?? prompt.design} · ${prompt.title}`;
}
