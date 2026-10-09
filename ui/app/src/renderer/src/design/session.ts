//! One open design in the renderer (D53.4): a Yjs document kept in step with jkb.
//
// The table in jkb is the truth; this document is a peer of it, like any other editor. Local edits
// leave as `design.apply` (the update bytes, base64), in order, one call at a time. What other peers
// write — the CLI's `jkb design edit`, another window — is announced on the design's topic, and each
// announcement is only a hint: the session fetches with `design.state` from this document's own state
// vector, so what it merges is exactly what it lacks, read from the table. The bytes an announcement
// carries are never merged: anyone who may send to the queue could put any bytes there (D53.4).
//
// Span states come from `design.cat`, whose text is compared with this document's before its spans
// are drawn: an answer read while an edit was in flight describes other text, and is dropped for the
// next one rather than drawn in the wrong place. The version of a matching answer is the one a
// *Discuss* names — the version whose text is what the operator is looking at.
//
// No React and no `window`: the bridge is passed in, so this is tested with a stand-in daemon.

import * as Y from "yjs";

import {
  decodeDesignDoc,
  decodeDesignUpdate,
  decodeDesignWritten,
  designOps,
  fromBase64,
  toBase64,
  type ApiError,
  type DesignDoc,
  type OpRequest,
  type OpResponse,
  type Outcome,
} from "@jkb/core";

import type { DesignFeedEvent } from "../../../shared/bridge";

/** What a session needs from the bridge. */
export interface SessionBridge {
  op(request: OpRequest): Promise<Outcome<OpResponse>>;
  subscribe(topic: string): Promise<Outcome<null>>;
  unsubscribe(topic: string): void;
  onEvent(listener: (event: DesignFeedEvent) => void): () => void;
}

/** Where the session is. */
export type SyncStatus =
  | { readonly kind: "connecting" }
  | { readonly kind: "live" }
  | { readonly kind: "saving" }
  /** Retrying: the daemon is unreachable or busy. Edits are kept and sent when it answers. */
  | { readonly kind: "retrying"; readonly message: string }
  /** The daemon refused an edit or the design: nothing more is sent until the design is reopened. */
  | { readonly kind: "failed"; readonly message: string };

/** The origin of every change merged from jkb, so it is never sent back. */
export const REMOTE = Symbol("jkb");

/** Codes that mean "try again later", as opposed to a refusal. */
const TRANSIENT = new Set(["unavailable", "busy", "internal", "unknown"]);

/** Why there is no settled version to send a *Discuss* with. */
export type Unsettled =
  /** The session stopped (a refused edit or design): nothing it shows can be named. */
  | { readonly kind: "failed"; readonly message: string }
  /** The pane left the design while waiting (the session is detached or disposed). */
  | { readonly kind: "closed" }
  /** `design.cat` could not be read. */
  | { readonly kind: "unread"; readonly message: string }
  /** The text changed while the selection was being sent: an edit from elsewhere arrived. */
  | { readonly kind: "moved" };

/** A `design.cat` read: the answer kept, or why none was. */
type SpansRead = { readonly kept: true } | { readonly kept: false; readonly unread?: string };

/** One retried call's timer and backoff. Each call that retries has its own, so one never cancels another's. */
class Retry {
  #timer: ReturnType<typeof setTimeout> | undefined;
  #backoff: number;
  /** Attempts scheduled since the last success, or since `recount`. */
  attempts = 0;
  constructor(
    readonly min: number,
    readonly max: number,
  ) {
    this.#backoff = min;
  }
  schedule(again: () => void): void {
    clearTimeout(this.#timer);
    this.#timer = setTimeout(() => {
      this.#timer = undefined;
      again();
    }, this.#backoff);
    this.#backoff = Math.min(this.#backoff * 2, this.max);
    this.attempts += 1;
  }
  reset(): void {
    this.#backoff = this.min;
    this.attempts = 0;
  }
  recount(): void {
    this.attempts = 0;
  }
  cancel(): void {
    clearTimeout(this.#timer);
    this.#timer = undefined;
  }
}

export interface DesignSessionOptions {
  /** How long after a change the spans are re-read. */
  readonly spansDelayMs?: number;
  /**
   * How many times a detached session (no pane shows it) retries sending its edits before it gives
   * up and stops, which its registry reports. An attached session retries for as long as it is shown.
   */
  readonly detachedRetries?: number;
  readonly minBackoffMs?: number;
  readonly maxBackoffMs?: number;
}

export class DesignSession {
  readonly doc: Y.Doc;
  readonly text: Y.Text;
  /** The latest `design.cat` answer whose text was this document's, or `undefined` before one. */
  current: DesignDoc | undefined;
  status: SyncStatus = { kind: "connecting" };
  /**
   * Why other editors' changes are not arriving, when they are not: the feed is down (and retrying)
   * or could not be joined. Saving is unaffected; what others write shows on the next read.
   */
  feedProblem: string | undefined;

  readonly #bridge: SessionBridge;
  readonly #uid: string;
  readonly #topic: string;
  readonly #spansDelay: number;
  readonly #listeners = new Set<() => void>();
  #outbox: Uint8Array[] = [];
  #sending = false;
  #pulling: Promise<void> | undefined;
  #pullAgain = false;
  /** Whether a load has merged: until then the document is empty, not the design. */
  #loaded = false;
  /** The feed could not be joined because the design has no topic yet: joined again later. */
  #joinLater = false;
  readonly #flushRetry: Retry;
  readonly #pullRetry: Retry;
  readonly #joinRetry: Retry;
  #spansTimer: ReturnType<typeof setTimeout> | undefined;
  #unlisten: (() => void) | undefined;
  #idle: (() => void)[] = [];
  #disposed = false;
  #attached = true;
  readonly #detachedRetries: number;

  constructor(bridge: SessionBridge, uid: string, topic: string, options: DesignSessionOptions = {}) {
    this.#bridge = bridge;
    this.#uid = uid;
    this.#topic = topic;
    this.#spansDelay = options.spansDelayMs ?? 250;
    this.#detachedRetries = options.detachedRetries ?? 10;
    const min = options.minBackoffMs ?? 500;
    const max = options.maxBackoffMs ?? 30_000;
    this.#flushRetry = new Retry(min, max);
    this.#pullRetry = new Retry(min, max);
    this.#joinRetry = new Retry(min, max);
    // Garbage collection off, as the engine's documents: a deleted run stays addressable, which an
    // undo of it (a forward update from jkb) relies on.
    this.doc = new Y.Doc({ gc: false });
    this.text = this.doc.getText("body");
    this.doc.getMap("spans");
    this.doc.on("update", (update: Uint8Array, origin: unknown) => {
      if (origin === REMOTE || this.#disposed) return;
      this.#outbox.push(update);
      void this.#flush();
    });
  }

  get uid(): string {
    return this.#uid;
  }

  /**
   * Whether the editor may take typing: only once a load has merged, and never after a refusal.
   * Text typed into the empty document before the load would be sent as a real edit, and land at
   * one end of the design's text.
   */
  get editable(): boolean {
    return this.#loaded && this.status.kind !== "failed";
  }

  /** Whether local edits are waiting to be sent (or one is in flight). */
  get unsent(): boolean {
    return this.#outbox.length > 0 || this.#sending;
  }

  /** Hear every change of status or spans. Returns the unsubscribe. */
  onChange(listener: () => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  #changed(): void {
    for (const l of this.#listeners) l();
  }

  #setStatus(status: SyncStatus): void {
    if (this.status.kind === "failed") return;
    this.status = status;
    this.#changed();
  }

  /**
   * Join the design's feed, then load it. The order matters: once main's group is on the topic, an
   * update written after the load is delivered, and one written before it is in the load.
   */
  async open(): Promise<void> {
    this.#unlisten = this.#bridge.onEvent((event) => this.#onEvent(event));
    await this.#join();
    if (this.#disposed) return;
    await this.pull();
  }

  /**
   * Join the design's topic. A design made before every design had a topic from creation has none
   * until its first update, and the join is refused with `no_such_topic`: it is tried again — on a
   * timer, and as soon as this session's own edit has created the topic — and the design re-read
   * once joined, for whatever was written while it was not.
   */
  async #join(): Promise<boolean> {
    this.#joinRetry.cancel();
    const joined = await this.#bridge.subscribe(this.#topic);
    if (this.#disposed) return false;
    if (joined.ok) {
      if (this.#joinLater) {
        this.#joinLater = false;
        this.feedProblem = undefined;
        this.#changed();
      }
      return true;
    }
    // Without the feed the document would silently stop hearing other editors: say so, and keep
    // what can still work (load and save) working.
    this.feedProblem = `live updates unavailable: ${joined.error.message}`;
    this.#joinLater = joined.error.code === "no_such_topic";
    if (this.#joinLater) this.#joinRetry.schedule(() => void this.#rejoin());
    this.#changed();
    return false;
  }

  async #rejoin(): Promise<void> {
    if (await this.#join()) await this.pull();
  }

  /** Whether a pane shows this session. */
  get attached(): boolean {
    return this.#attached;
  }

  /**
   * Shown again, or no longer shown (`registry.ts` decides). Detached, the session keeps sending what
   * it holds — with bounded retries — and every wait on it ends: a *Discuss* pending on a design the
   * pane has left resolves as `closed` rather than holding the one-at-a-time guard.
   */
  setAttached(attached: boolean): void {
    if (this.#attached === attached || this.#disposed) return;
    this.#attached = attached;
    this.#flushRetry.recount();
    if (!attached) this.#settleIdle();
  }

  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#unlisten?.();
    this.#bridge.unsubscribe(this.#topic);
    this.#flushRetry.cancel();
    this.#pullRetry.cancel();
    this.#joinRetry.cancel();
    clearTimeout(this.#spansTimer);
    this.#listeners.clear();
    for (const w of this.#idle) w();
    this.doc.destroy();
  }

  #onEvent(event: DesignFeedEvent): void {
    if (event.topic !== this.#topic || this.#disposed) return;
    switch (event.kind) {
      case "update": {
        if (this.feedProblem !== undefined) {
          this.feedProblem = undefined;
          this.#changed();
        }
        // A hint, never the bytes to merge: what the table holds is fetched (see the header).
        void this.pull();
        return;
      }
      case "gap":
        this.feedProblem = undefined;
        void this.pull();
        return;
      case "live":
        this.feedProblem = undefined;
        this.#changed();
        return;
      case "error":
        this.feedProblem = event.message;
        this.#changed();
        return;
      case "prompt":
        // The Prompts pane's, not the document's.
        return;
    }
  }

  #merge(bytes: Uint8Array): void {
    try {
      Y.applyUpdate(this.doc, bytes, REMOTE);
    } catch (e) {
      this.#fail(`an update from jkb does not merge: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  /** Fetch what this document lacks (`design.state` since its state vector) and merge it. */
  pull(): Promise<void> {
    if (this.#pulling !== undefined) {
      this.#pullAgain = true;
      return this.#pulling;
    }
    const run = async (): Promise<void> => {
      do {
        this.#pullAgain = false;
        const since = toBase64(Y.encodeStateVector(this.doc));
        const answer = decodeDesignUpdate(await this.#bridge.op(designOps.state(this.#uid, since)));
        if (this.#disposed) return;
        if (!answer.ok) {
          this.#trouble(answer.error, this.#pullRetry, () => void this.pull());
          return;
        }
        const bytes = fromBase64(answer.value.update);
        if (bytes === undefined) {
          this.#fail("jkb answered design.state with an update that is not base64");
          return;
        }
        this.#merge(bytes);
      } while (this.#pullAgain && !this.#disposed);
      if (this.#disposed) return;
      this.#pullRetry.reset();
      this.#pullRetry.cancel();
      this.#loaded = true;
      if (this.status.kind === "connecting" || this.status.kind === "retrying") {
        // An edit waiting on its own retry is still unsent: "saving" until that one is sent.
        this.#setStatus(this.unsent ? { kind: "saving" } : { kind: "live" });
      } else {
        this.#changed();
      }
      this.#scheduleSpans();
    };
    this.#pulling = run().finally(() => {
      this.#pulling = undefined;
    });
    return this.#pulling;
  }

  /** Send the outbox, one `design.apply` at a time, as one merged update. */
  async #flush(): Promise<void> {
    if (this.#sending || this.#disposed || this.status.kind === "failed") return;
    this.#sending = true;
    try {
      while (this.#outbox.length > 0 && !this.#disposed) {
        this.#setStatus({ kind: "saving" });
        // Taken whole: edits made while this one is in flight queue behind it in a new outbox.
        const batch = this.#outbox;
        this.#outbox = [];
        const update = batch.length === 1 ? (batch[0] as Uint8Array) : Y.mergeUpdates(batch);
        const answer = decodeDesignWritten(await this.#bridge.op(designOps.apply(this.#uid, toBase64(update))));
        if (this.#disposed) return;
        if (!answer.ok) {
          // Not sent: back at the head of the queue, ahead of anything written since.
          this.#outbox = [...batch, ...this.#outbox];
          if (!this.#attached && this.#flushRetry.attempts >= this.#detachedRetries) {
            this.#fail(`not saved after ${this.#detachedRetries} retries: ${answer.error.message}`);
            return;
          }
          this.#trouble(answer.error, this.#flushRetry, () => void this.#flush());
          return;
        }
        // Sent: a resend still scheduled from an earlier failure has nothing left to do.
        this.#flushRetry.reset();
        this.#flushRetry.cancel();
        // This session's edit has created a topic the design lacked: join it now.
        if (this.#joinLater) void this.#rejoin();
      }
      this.#setStatus({ kind: "live" });
      this.#scheduleSpans();
    } finally {
      this.#sending = false;
      if (this.#outbox.length === 0 && !this.#disposed) {
        this.#settleIdle();
        // Now `unsent` is false: a registry waiting to dispose a detached session hears it.
        this.#changed();
      }
    }
  }

  /** A failed call: retried later when it may pass, the session stopped when it never will. */
  #trouble(error: ApiError, retry: Retry, again: () => void): void {
    if (!TRANSIENT.has(error.code)) {
      this.#fail(`${error.code}: ${error.message}`);
      return;
    }
    this.#setStatus({ kind: "retrying", message: error.message });
    retry.schedule(again);
  }

  #fail(message: string): void {
    this.status = { kind: "failed", message };
    this.#settleIdle();
    this.#changed();
  }

  #settleIdle(): void {
    const waiting = this.#idle;
    this.#idle = [];
    for (const w of waiting) w();
  }

  #scheduleSpans(): void {
    if (this.#disposed) return;
    clearTimeout(this.#spansTimer);
    this.#spansTimer = setTimeout(() => void this.refreshSpans(), this.#spansDelay);
  }

  /**
   * Re-read the design (`design.cat`) and keep the answer when its text is this document's. Returns
   * whether it was kept.
   */
  async refreshSpans(): Promise<boolean> {
    return (await this.#readSpans()).kept;
  }

  async #readSpans(): Promise<SpansRead> {
    const answer = decodeDesignDoc(await this.#bridge.op(designOps.cat(this.#uid)));
    if (this.#disposed) return { kept: false };
    if (!answer.ok) return { kept: false, unread: answer.error.message };
    if (answer.value.text !== this.text.toString()) return { kept: false };
    this.current = answer.value;
    this.#changed();
    return { kept: true };
  }

  /** Resolves when every local edit has been sent (or the session has stopped). */
  idle(): Promise<void> {
    if (!this.unsent || this.#disposed || !this.#attached || this.status.kind === "failed") {
      return Promise.resolve();
    }
    return new Promise((resolve) => this.#idle.push(resolve));
  }

  /**
   * The version whose text is exactly what this document shows: every edit sent, then read back.
   * Otherwise why there is none — the session stopped or closed, the read failed, or the text moved
   * (an edit from elsewhere arrived in between) — each its own reason, so each is said as it is.
   */
  async settledVersion(): Promise<{ readonly ok: true; readonly doc: DesignDoc } | { readonly ok: false; readonly why: Unsettled }> {
    await this.idle();
    const stopped = (): Unsettled | undefined =>
      this.#disposed || !this.#attached
        ? { kind: "closed" }
        : this.status.kind === "failed"
          ? { kind: "failed", message: this.status.message }
          : undefined;
    const before = stopped();
    if (before !== undefined) return { ok: false, why: before };
    const read = await this.#readSpans();
    const after = stopped();
    if (after !== undefined) return { ok: false, why: after };
    if (read.kept && this.current !== undefined) return { ok: true, doc: this.current };
    return { ok: false, why: read.kept || read.unread === undefined ? { kind: "moved" } : { kind: "unread", message: read.unread } };
  }
}
