//! Comment threads on a design's text, and approving a selection, as the ops answer them. The
//! operator starts and resolves threads; Claude replies. The Rust side is the source of truth:
//! `crates/jkb-api/src/designs.rs` (the shapes) and `crates/jkb-core/src/design/threads.rs`.

import type { OpResponse, Outcome } from "./daemon.js";
import { failed } from "./daemon.js";

/** `jkb_api::designs::Comment`. */
export interface ThreadComment {
  readonly uid: string;
  /** `operator`, or the Claude principal's label. */
  readonly by: string;
  readonly at: string;
  readonly text: string;
}

/** `jkb_api::designs::Thread`: a thread as it reads now. */
export interface Thread {
  readonly uid: string;
  readonly design: string;
  /** False once its words were deleted: the thread is *detached*. */
  readonly anchored: boolean;
  /** UTF-16 offsets of its words now. */
  readonly start: number;
  readonly end: number;
  readonly quote: string;
  /** The words it was started on. */
  readonly started_on: string;
  readonly status: "open" | "resolved" | string;
  readonly comments: readonly ThreadComment[];
}

export const threadOps = {
  list: (uid: string) => ({ op: "design.threads", uid }),
  start: (uid: string, base: string, start: number, end: number, text: string) => ({
    op: "design.thread_start",
    uid,
    base,
    start,
    end,
    text,
  }),
  reply: (thread: string, text: string) => ({ op: "design.thread_reply", thread, text }),
  resolve: (thread: string) => ({ op: "design.thread_resolve", thread }),
  /** Approve exactly the words selected (the operator's alone). */
  approveSelection: (uid: string, base: string, start: number, end: number) => ({
    op: "design.approve_selection",
    uid,
    base,
    start,
    end,
  }),
};

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const isString = (v: unknown): v is string => typeof v === "string";
const isNumber = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

function isComment(v: unknown): v is ThreadComment {
  return isObject(v) && isString(v["uid"]) && isString(v["by"]) && isString(v["at"]) && isString(v["text"]);
}

function isThread(v: unknown): v is Thread {
  return (
    isObject(v) &&
    isString(v["uid"]) &&
    isString(v["design"]) &&
    typeof v["anchored"] === "boolean" &&
    isNumber(v["start"]) &&
    isNumber(v["end"]) &&
    isString(v["quote"]) &&
    isString(v["started_on"]) &&
    isString(v["status"]) &&
    Array.isArray(v["comments"]) &&
    v["comments"].every(isComment)
  );
}

/** `design.threads`' answer. */
export function decodeThreads(o: Outcome<OpResponse>): Outcome<readonly Thread[]> {
  if (!o.ok) return o;
  const threads = o.value.result === "design_thread_list" ? o.value["threads"] : undefined;
  if (!Array.isArray(threads) || !threads.every(isThread)) {
    return failed("internal", "jkb serve did not answer with a well-formed `design_thread_list`");
  }
  return { ok: true, value: threads };
}

/** A thread write's answer: the thread after it. */
export function decodeThread(o: Outcome<OpResponse>): Outcome<Thread> {
  if (!o.ok) return o;
  const thread = o.value.result === "design_thread" ? o.value["thread"] : undefined;
  if (!isThread(thread)) return failed("internal", "jkb serve did not answer with a well-formed `design_thread`");
  return { ok: true, value: thread };
}

/** A `thread` message on a design's topic: one of its threads was started, replied to or resolved. */
export interface ThreadAnnouncement {
  readonly design: string;
  readonly thread: string;
  readonly what: string;
}

export function parseThreadAnnouncement(payload: unknown): ThreadAnnouncement | undefined {
  if (!isObject(payload) || !isString(payload["design"]) || !isString(payload["thread"]) || !isString(payload["what"]))
    return undefined;
  return { design: payload["design"], thread: payload["thread"], what: payload["what"] };
}

/** Open threads first, then resolved; each in text order, detached ones last. */
export function orderThreads(threads: readonly Thread[]): Thread[] {
  const rank = (t: Thread): number => (t.status === "resolved" ? 1 : 0);
  return [...threads].sort((a, b) => rank(a) - rank(b) || Number(!a.anchored) - Number(!b.anchored) || a.start - b.start);
}

/**
 * Where each margin card goes: `want` is the vertical position level with its words, in text order,
 * `height` its measured height. Cards keep their wanted place unless the one above would overlap,
 * then sit just below it — Google Docs' rule, so a card is never hidden behind another.
 */
export function stackCards(cards: readonly { readonly want: number; readonly height: number }[], gap = 8): number[] {
  const out: number[] = [];
  let floor = Number.NEGATIVE_INFINITY;
  for (const c of cards) {
    const top = Math.max(c.want, floor);
    out.push(top);
    floor = top + c.height + gap;
  }
  return out;
}
