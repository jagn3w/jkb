//! One subscription to main's terminal events for the whole window, routed to each terminal by id.
//
// Pure (no DOM, no bridge: the subscription is passed in), so it is tested without a window.
//
// Output can arrive for an id the renderer has not claimed yet: main starts the PTY before the
// open call's answer is read, and a fast program writes in between. Such events are held until
// the id is claimed. An id the renderer closed is retired, so its last output and its exit — which
// arrive after the close — are dropped rather than held for a claim that will never come.

import type { TerminalEvent } from "../../../shared/terminal";

/** What is held for unclaimed ids, in all, before the oldest is dropped. */
export const HELD_LIMIT_CHARS = 1024 * 1024;

export class TerminalEventRouter {
  private readonly handlers = new Map<number, (event: TerminalEvent) => void>();
  private readonly held = new Map<number, TerminalEvent[]>();
  private readonly retired = new Set<number>();
  private heldChars = 0;
  private readonly unsubscribe: () => void;

  constructor(
    subscribe: (listener: (event: TerminalEvent) => void) => () => void,
    /**
     * Held output dropped unseen, so its characters can still be acknowledged to main: main counts
     * them as outstanding, and a terminal whose output was never acknowledged stays paused (`FLOW`).
     */
    private readonly onDropped: (id: number, chars: number) => void = () => undefined,
  ) {
    this.unsubscribe = subscribe((event) => this.deliver(event));
  }

  private deliver(event: TerminalEvent): void {
    const handler = this.handlers.get(event.id);
    if (handler !== undefined) {
      handler(event);
      if (event.kind === "exit") this.handlers.delete(event.id);
      return;
    }
    if (this.retired.has(event.id)) {
      if (event.kind === "exit") this.retired.delete(event.id);
      return;
    }
    const list = this.held.get(event.id) ?? [];
    list.push(event);
    this.held.set(event.id, list);
    this.heldChars += event.kind === "data" ? event.data.length : 0;
    this.trim();
  }

  private trim(): void {
    for (const [id, list] of this.held) {
      if (this.heldChars <= HELD_LIMIT_CHARS) return;
      this.held.delete(id);
      let chars = 0;
      for (const e of list) chars += e.kind === "data" ? e.data.length : 0;
      this.heldChars -= chars;
      if (chars > 0) this.onDropped(id, chars);
    }
  }

  /** Route `id`'s events to `handler`, starting with any already held for it. */
  claim(id: number, handler: (event: TerminalEvent) => void): void {
    const held = this.held.get(id) ?? [];
    this.held.delete(id);
    let exited = false;
    for (const e of held) {
      this.heldChars -= e.kind === "data" ? e.data.length : 0;
      handler(e);
      exited ||= e.kind === "exit";
    }
    if (!exited) this.handlers.set(id, handler);
  }

  /**
   * Stop routing `id` (its terminal is being closed, claimed or not); whatever still arrives for
   * it is dropped. Call it only for an id whose exit has not been delivered: the retirement lasts
   * until that exit arrives.
   */
  retire(id: number): void {
    this.handlers.delete(id);
    const held = this.held.get(id) ?? [];
    this.held.delete(id);
    for (const e of held) this.heldChars -= e.kind === "data" ? e.data.length : 0;
    if (!held.some((e) => e.kind === "exit")) this.retired.add(id);
  }

  dispose(): void {
    this.unsubscribe();
    this.handlers.clear();
    this.held.clear();
  }
}
