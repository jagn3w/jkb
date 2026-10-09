//! The order in which one terminal starts and ends its programs (D53.10).
//
// Pure (the bridge and the screen are passed in), so the ordering is tested with a fake bridge
// rather than only through its parts. `session.ts` wraps it in xterm.
//
// Rules:
// - An end is awaited before anything else starts, and an open still in flight is awaited before
//   an end: a PTY whose open lands after its start was superseded (or its tab closed) is ended
//   like any other, not dropped.
// - The last end is remembered. If it was not confirmed, nothing starts here until an explicit
//   override (Restart): the program may still be running, and two programs on one Claude session
//   is worse than none. This one field is all the end tracking a fixed-target terminal needs: its
//   only stop-then-start paths are this terminal's own.

import type { TerminalEnd, TerminalInfo, TerminalResult, TerminalSpec } from "../../../shared/terminal";
import type { TerminalStatus } from "./state";

/** The part of `window.jkb.terminal` a run uses. */
export interface RunBridge {
  open(spec: TerminalSpec, cols: number, rows: number): Promise<TerminalResult<TerminalInfo>>;
  close(id: number): Promise<TerminalResult<TerminalEnd>>;
}

/** What a run tells its screen. */
export interface RunScreen {
  size(): { cols: number; rows: number };
  note(text: string): void;
  status(status: TerminalStatus): void;
  /** Start showing PTY `id`'s output (it is now this terminal's). */
  attach(id: number): void;
  /** Stop showing PTY `id`'s output (it is being ended). */
  detach(id: number): void;
}

const failure = (e: unknown): string => (e instanceof Error ? e.message : String(e));

export class TerminalRun {
  private ptyId: number | undefined;
  private target: TerminalSpec["target"] = "container";
  private generation = 0;
  private disposed = false;
  /** The open in flight, if any; settles once its PTY (if one started) is recorded in `ptyId`. */
  private inflight: Promise<void> = Promise.resolve();
  /** The last end sent, settled or not. */
  private lastEnd: Promise<TerminalEnd | undefined> = Promise.resolve(undefined);

  constructor(
    private readonly bridge: RunBridge,
    private readonly screen: RunScreen,
  ) {}

  /** The PTY this terminal shows now. */
  get id(): number | undefined {
    return this.ptyId;
  }

  /** PTY `id` exited by itself: there is nothing left to end. */
  exited(id: number): void {
    if (this.ptyId === id) this.ptyId = undefined;
  }

  /**
   * Start `spec`, ending what runs first. It does not start while the last end is unconfirmed,
   * unless `override` (the person's Restart).
   */
  async start(spec: TerminalSpec, options: { readonly override?: boolean } = {}): Promise<void> {
    const generation = ++this.generation;
    this.screen.status({ kind: "starting" });
    const ended = await this.stop();
    if (this.disposed || generation !== this.generation) return;
    if (ended !== undefined && !ended.confirmed) {
      if (options.override !== true) {
        const why = `not started: the program before it may still be running (${ended.detail}). Restart runs it anyway.`;
        this.screen.note(why);
        this.screen.status({ kind: "failed", error: why, mayBeRunning: true });
        return;
      }
      this.lastEnd = Promise.resolve(undefined);
    }
    const { cols, rows } = this.screen.size();
    const opening = this.bridge.open(spec, cols, rows).catch((e: unknown) => ({ ok: false as const, error: failure(e) }));
    // Recorded whatever happens to this start, so a stop that comes meanwhile ends it.
    this.inflight = opening.then((result) => {
      if (result.ok) {
        this.ptyId = result.value.id;
        this.target = spec.target;
      }
    });
    const result = await opening;
    await this.inflight;
    if (this.disposed || generation !== this.generation) return; // Its stop ends what just opened.
    if (!result.ok) {
      this.screen.note(`could not start: ${result.error}`);
      this.screen.status({ kind: "failed", error: result.error });
      return;
    }
    this.screen.status({ kind: "running" });
    this.screen.attach(result.value.id);
  }

  /**
   * End what runs (after any open in flight), and answer the last end: this one, or — when
   * nothing runs — the one before, so an unconfirmed end keeps answering until overridden.
   */
  stop(): Promise<TerminalEnd | undefined> {
    const before = this.lastEnd;
    const end = this.inflight.then(() => {
      const id = this.ptyId;
      if (id === undefined) return before;
      this.ptyId = undefined;
      this.screen.detach(id);
      const target = this.target;
      return this.bridge.close(id).then(
        // Gone already (it exited as it was closed): its exit was the ending.
        (r): TerminalEnd | undefined => (r.ok ? r.value : undefined),
        (e: unknown): TerminalEnd => ({ target, confirmed: false, detail: `could not end it: ${failure(e)}` }),
      );
    });
    this.lastEnd = end;
    return end;
  }

  /** No more starts; end what runs (including an open still in flight). */
  dispose(): Promise<TerminalEnd | undefined> {
    this.disposed = true;
    return this.stop();
  }
}
