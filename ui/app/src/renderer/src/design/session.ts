//! One open design in the renderer (D53.4): a Yjs document kept in step with jkb.
//
// The table in jkb is the truth; this document is a peer of it, like any other editor. Local edits
// leave as `design.apply` (the update bytes, base64), in order, one call at a time; updates other
// peers write — the CLI's `jkb design edit`, another window — arrive on the design's topic through
// main and are merged. Anything the feed could not carry (an update too large to announce, a gap,
// a merge waiting on something it never saw) is fetched with `design.state` from this document's
// own state vector, so the answer is exactly what it lacks.
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

export interface DesignSessionOptions {
  /** How long after a change the spans are re-read. */
  readonly spansDelayMs?: number;
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
  readonly #minBackoff: number;
  readonly #maxBackoff: number;
  readonly #listeners = new Set<() => void>();
  #outbox: Uint8Array[] = [];
  #sending = false;
  #pulling: Promise<void> | undefined;
  #pullAgain = false;
  #backoff: number;
  #retryTimer: ReturnType<typeof setTimeout> | undefined;
  #spansTimer: ReturnType<typeof setTimeout> | undefined;
  #unlisten: (() => void) | undefined;
  #idle: (() => void)[] = [];
  #disposed = false;

  constructor(bridge: SessionBridge, uid: string, topic: string, options: DesignSessionOptions = {}) {
    this.#bridge = bridge;
    this.#uid = uid;
    this.#topic = topic;
    this.#spansDelay = options.spansDelayMs ?? 250;
    this.#minBackoff = options.minBackoffMs ?? 500;
    this.#maxBackoff = options.maxBackoffMs ?? 30_000;
    this.#backoff = this.#minBackoff;
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
    const joined = await this.#bridge.subscribe(this.#topic);
    if (this.#disposed) return;
    if (!joined.ok) {
      // Without the feed the document would silently stop hearing other editors: say so, and
      // keep what can still work (load and save) working.
      this.feedProblem = `live updates unavailable: ${joined.error.message}`;
      this.#changed();
    }
    await this.pull();
  }

  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#unlisten?.();
    this.#bridge.unsubscribe(this.#topic);
    clearTimeout(this.#retryTimer);
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
        const bytes = event.update === null ? undefined : fromBase64(event.update);
        if (bytes === undefined) {
          void this.pull();
          return;
        }
        this.#merge(bytes);
        // A merge waiting on an update this document never got (the feed lost one) is completed
        // from the table.
        if (this.#hasPending()) void this.pull();
        else this.#scheduleSpans();
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
    }
  }

  #merge(bytes: Uint8Array): void {
    try {
      Y.applyUpdate(this.doc, bytes, REMOTE);
    } catch (e) {
      this.#fail(`an update from jkb does not merge: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  #hasPending(): boolean {
    return this.doc.store.pendingStructs !== null || this.doc.store.pendingDs !== null;
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
          this.#trouble(answer.error, () => void this.pull());
          return;
        }
        const bytes = fromBase64(answer.value.update);
        if (bytes === undefined) {
          this.#fail("jkb answered design.state with an update that is not base64");
          return;
        }
        this.#merge(bytes);
      } while (this.#pullAgain && !this.#disposed);
      if (this.status.kind === "connecting" || this.status.kind === "retrying") {
        this.#backoff = this.#minBackoff;
        this.#setStatus(this.#outbox.length > 0 ? { kind: "saving" } : { kind: "live" });
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
          this.#trouble(answer.error, () => void this.#flush());
          return;
        }
        this.#backoff = this.#minBackoff;
      }
      this.#setStatus({ kind: "live" });
      this.#scheduleSpans();
    } finally {
      this.#sending = false;
      if (this.#outbox.length === 0) this.#settleIdle();
    }
  }

  /** A failed call: retried later when it may pass, the session stopped when it never will. */
  #trouble(error: ApiError, again: () => void): void {
    if (!TRANSIENT.has(error.code)) {
      this.#fail(`${error.code}: ${error.message}`);
      return;
    }
    this.#setStatus({ kind: "retrying", message: error.message });
    clearTimeout(this.#retryTimer);
    this.#retryTimer = setTimeout(again, this.#backoff);
    this.#backoff = Math.min(this.#backoff * 2, this.#maxBackoff);
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
    const answer = decodeDesignDoc(await this.#bridge.op(designOps.cat(this.#uid)));
    if (this.#disposed || !answer.ok) return false;
    if (answer.value.text !== this.text.toString()) return false;
    this.current = answer.value;
    this.#changed();
    return true;
  }

  /** Resolves when every local edit has been sent (or the session has stopped). */
  idle(): Promise<void> {
    if ((this.#outbox.length === 0 && !this.#sending) || this.#disposed || this.status.kind === "failed") {
      return Promise.resolve();
    }
    return new Promise((resolve) => this.#idle.push(resolve));
  }

  /**
   * The version whose text is exactly what this document shows: every edit sent, then read back.
   * `undefined` while the text is still moving (an edit from elsewhere arrived in between).
   */
  async settledVersion(): Promise<DesignDoc | undefined> {
    await this.idle();
    if (this.#disposed || this.status.kind === "failed") return undefined;
    return (await this.refreshSpans()) ? this.current : undefined;
  }
}
