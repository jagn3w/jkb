// The selection actions' pure steps — the version a selection is offsets into, a request whose
// answer is noticed when it fails, and Approve itself — so each way they can end is tested without
// React. Every path that ends without the action done says why: an Approve that showed nothing was
// indistinguishable from one that was never clicked.

import { decodeDesignSpan, threadOps, type OpRequest, type OpResponse, type Outcome, type Span } from "@jkb/core";

import type { DesignSession } from "./session";

/**
 * How long a selection action waits for the session to settle (every edit sent, then read back)
 * before saying the edits are still being saved. A session whose saves keep failing transiently
 * retries for as long as it is shown, so without a bound the action would wait with nothing said.
 */
export const IDLE_TIMEOUT_MS = 10_000;

/** What an action is, in the two ways a notice says it. */
export interface Doing {
  /** "while <during>": e.g. "the approval was being sent". */
  readonly during: string;
  /** "Cannot <verb>: …": e.g. "approve". */
  readonly verb: string;
}

export type Notice = (message: string) => void;

/** What a selection action needs of the session: only that it can settle. */
export type Settling = Pick<DesignSession, "settledVersion">;

/** The op seam: `window.jkb.op` in the app, which rejects when the main process's handler throws. */
export type Op = (request: OpRequest) => Promise<Outcome<OpResponse>>;

const message = (e: unknown): string => (e instanceof Error ? e.message : String(e));

/**
 * A request's value, or `undefined` with one notice: its refusal's message, or, when it rejected
 * (an IPC failure), `Cannot <verb>: <message>` — never a rejection left to escape unhandled.
 */
export async function settle<T>(request: () => Promise<Outcome<T>>, doing: Doing, onNotice: Notice): Promise<T | undefined> {
  let answer: Outcome<T>;
  try {
    answer = await request();
  } catch (e) {
    onNotice(`Cannot ${doing.verb}: ${message(e)}`);
    return undefined;
  }
  if (!answer.ok) {
    onNotice(answer.error.message);
    return undefined;
  }
  return answer.value;
}

export interface VersionOf {
  readonly session: Settling | undefined;
  /** The text the selection's offsets are into. */
  readonly shown: string;
  readonly doing: Doing;
  readonly onNotice: Notice;
  /** Defaults to [`IDLE_TIMEOUT_MS`]; tests pass a few milliseconds. */
  readonly idleTimeoutMs?: number;
}

const TIMED_OUT = Symbol("timed out");

/**
 * The version whose text is `shown`, or `undefined` with exactly one notice saying why not — each
 * way a session can fail to settle said as *Discuss* says it, never all as "the text changed".
 */
export async function versionOf({ session, shown, doing, onNotice, idleTimeoutMs = IDLE_TIMEOUT_MS }: VersionOf): Promise<string | undefined> {
  if (session === undefined) {
    onNotice("The design is still opening — select the text again.");
    return undefined;
  }
  let timer: ReturnType<typeof setTimeout> | undefined;
  let settled: Awaited<ReturnType<Settling["settledVersion"]>> | typeof TIMED_OUT;
  try {
    // Abandoned on timeout, not cancelled: `race` subscribes to it, so a later rejection is handled.
    settled = await Promise.race([
      session.settledVersion(),
      new Promise<typeof TIMED_OUT>((resolve) => {
        timer = setTimeout(() => resolve(TIMED_OUT), idleTimeoutMs);
      }),
    ]);
  } catch (e) {
    onNotice(`Cannot ${doing.verb}: ${message(e)}`);
    return undefined;
  } finally {
    clearTimeout(timer);
  }
  if (settled === TIMED_OUT) {
    onNotice("Edits are still being saved — try again in a moment.");
    return undefined;
  }
  if (!settled.ok) {
    const why = settled.why;
    switch (why.kind) {
      case "closed":
        onNotice(`The design was left before its edits were saved, while ${doing.during} — select the text again.`);
        return undefined;
      case "failed":
        onNotice(`${why.message} — reopen the design.`);
        return undefined;
      case "unread":
        onNotice(`Cannot read the design: ${why.message}`);
        return undefined;
      case "moved":
        break;
    }
  }
  if (!settled.ok || settled.doc.text !== shown) {
    onNotice(`The text changed while ${doing.during} — select it again.`);
    return undefined;
  }
  return settled.doc.version;
}

export const APPROVING: Doing = { during: "the approval was being sent", verb: "approve" };

export interface ApproveSelection {
  readonly design: string;
  readonly session: Settling | undefined;
  readonly from: number;
  readonly to: number;
  readonly shown: string;
  readonly op: Op;
  readonly onNotice: Notice;
  /** Told when the request is in flight, and when it is over. */
  readonly onBusy?: (busy: boolean) => void;
  readonly idleTimeoutMs?: number;
}

/**
 * Approve exactly the words selected, against the version whose text is the one on screen. Ends
 * with exactly one notice whichever way it goes, success included, so "nothing happened" is never
 * an outcome: the span approved, or why not.
 */
export async function approveSelection(args: ApproveSelection): Promise<Span | undefined> {
  const { design, session, from, to, shown, op, onNotice, onBusy, idleTimeoutMs } = args;
  const base = await versionOf({ session, shown, doing: APPROVING, onNotice, idleTimeoutMs });
  if (base === undefined) return undefined;
  onBusy?.(true);
  let span: Span | undefined;
  try {
    span = await settle(async () => decodeDesignSpan(await op(threadOps.approveSelection(design, base, from, to))), APPROVING, onNotice);
  } finally {
    onBusy?.(false);
  }
  if (span === undefined) return undefined;
  onNotice(`Approved ${span.text.length} characters (${span.uid}).`);
  return span;
}
