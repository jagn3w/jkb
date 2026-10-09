//! The needs-input dot's source (D53.9): the notification records, kept current by the app's own
//! consumer group on `claude/notify`.
//
// The records (`notify.open_sessions`) are the answer; the feed only says when to read them again.
// So the order is the design session's: join the feed first, then read, and an event that arrives
// while a read is in flight asks for one more read after it — never a read whose answer is older
// than the last event. Without the feed (no `claude/notify` on this daemon, or the daemon down) the
// records are still read once and on demand, and the problem is said, not hidden.
//
// No React and no `window`: the bridge is passed in, so this is tested with a stand-in daemon.

import { needingInput, type NotifyRecord, type OpRequest, type OpResponse, type Outcome } from "@jkb/core";

import type { NotifyFeedEvent } from "../../../shared/bridge";
import { loadNotified } from "./data";

export interface WatchBridge {
  op(request: OpRequest): Promise<Outcome<OpResponse>>;
  subscribe(): Promise<Outcome<null>>;
  unsubscribe(): void;
  onEvent(listener: (event: NotifyFeedEvent) => void): () => void;
}

export class NotifyWatch {
  /** The notifications on screen, as last read. */
  records: readonly NotifyRecord[] = [];
  /** The sessions whose notification awaits the user: the dot. */
  needing: ReadonlySet<string> = new Set();
  /** Why the records may be stale: the feed is down or absent, or the last read failed. */
  problem: string | undefined;
  /** Whether a read has answered yet. */
  loaded = false;

  readonly #bridge: WatchBridge;
  readonly #listeners = new Set<() => void>();
  #unlisten: (() => void) | undefined;
  #reading: Promise<void> | undefined;
  #again = false;
  #feedProblem: string | undefined;
  #readProblem: string | undefined;
  #disposed = false;

  constructor(bridge: WatchBridge) {
    this.#bridge = bridge;
  }

  /** Hear every change of records or problem. Returns the unsubscribe. */
  onChange(listener: () => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  #changed(): void {
    this.problem = this.#readProblem ?? this.#feedProblem;
    for (const l of this.#listeners) l();
  }

  /** Join the feed, then read the records. */
  async open(): Promise<void> {
    this.#unlisten = this.#bridge.onEvent((event) => this.#onEvent(event));
    const joined = await this.#bridge.subscribe();
    if (this.#disposed) return;
    if (!joined.ok) {
      this.#feedProblem =
        joined.error.code === "no_such_topic"
          ? "claude/notify does not exist on this daemon (scripts/setup.sh creates it): the dot updates only on Refresh"
          : `live updates unavailable: ${joined.error.message}`;
      this.#changed();
    }
    await this.read();
  }

  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#unlisten?.();
    this.#bridge.unsubscribe();
    this.#listeners.clear();
  }

  #onEvent(event: NotifyFeedEvent): void {
    if (this.#disposed) return;
    switch (event.kind) {
      case "changed":
      case "gap":
        if (event.kind === "gap") this.#feedProblem = undefined;
        void this.read();
        return;
      case "error":
        this.#feedProblem = event.message;
        this.#changed();
        return;
      case "live":
        this.#feedProblem = undefined;
        // The daemon answers again: a read that failed while it did not is made now.
        if (this.#readProblem !== undefined) void this.read();
        else this.#changed();
        return;
    }
  }

  /** Read the records again; a read asked for while one is in flight runs once more after it. */
  read(): Promise<void> {
    if (this.#reading !== undefined) {
      this.#again = true;
      return this.#reading;
    }
    const run = async (): Promise<void> => {
      do {
        this.#again = false;
        const answer = await loadNotified((r) => this.#bridge.op(r));
        if (this.#disposed) return;
        if (answer.ok) {
          this.records = answer.value;
          this.needing = needingInput(answer.value);
          this.loaded = true;
          this.#readProblem = undefined;
        } else {
          this.#readProblem = `cannot read notifications: ${answer.error.message}`;
        }
        this.#changed();
      } while (this.#again && !this.#disposed);
    };
    this.#reading = run().finally(() => {
      this.#reading = undefined;
    });
    return this.#reading;
  }
}
