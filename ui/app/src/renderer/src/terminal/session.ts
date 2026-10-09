//! One terminal's xterm.js instance and the PTY behind it.
//
// Lives outside React: a terminal's screen and scrollback must survive its view being unmounted
// and mounted elsewhere (a tab switch, a move from the popover to the drawer), so the xterm
// element is created once and re-parented into whichever view shows it.

import { FitAddon } from "@xterm/addon-fit";
import { Terminal, type ITheme } from "@xterm/xterm";

import { FLOW, chunkWrite, type TerminalEnd, type TerminalEvent, type TerminalSpec } from "../../../shared/terminal";
import type { TerminalEventRouter } from "./router";
import type { TerminalStatus } from "./state";
import { themeFrom } from "./theme";

/** The terminal's colours, from the design tokens, so it follows the app's light and dark. */
export function themeFromTokens(): ITheme {
  const css = getComputedStyle(document.documentElement);
  return themeFrom((name) => css.getPropertyValue(name));
}

function monoFont(): string {
  return getComputedStyle(document.documentElement).getPropertyValue("--font-mono").trim() || "monospace";
}

/** Resolves once the terminal's font has loaded (or failed to: the fallback is drawn then). */
function fontLoaded(): Promise<void> {
  try {
    return document.fonts.load(`13px ${monoFont()}`).then(
      () => undefined,
      () => undefined,
    );
  } catch {
    return Promise.resolve();
  }
}

/** Dim text, for the lines the terminal itself writes (an exit, a failure to start). */
const note = (text: string): string => `\r\n\x1b[2m${text}\x1b[22m\r\n`;

export class TerminalSession {
  readonly term: Terminal;
  private readonly fitAddon = new FitAddon();
  private readonly host: HTMLDivElement;
  private opened = false;
  private opening: Promise<void> | undefined;
  private ptyId: number | undefined;
  private ptyTarget: TerminalSpec["target"] = "container";
  private generation = 0;
  private disposed = false;
  /** Drawn output not yet acknowledged to main, and the PTY it came from. */
  private drawn = { id: -1, chars: 0 };

  constructor(
    private readonly router: TerminalEventRouter,
    private readonly onStatus: (status: TerminalStatus) => void,
  ) {
    this.term = new Terminal({
      fontFamily: monoFont(),
      fontSize: 13,
      lineHeight: 1.2,
      cursorBlink: true,
      scrollback: 10_000,
      allowProposedApi: false,
      theme: themeFromTokens(),
    });
    this.term.loadAddon(this.fitAddon);
    // Ctrl+` belongs to the app (it toggles the drawer), not to the program in the terminal.
    this.term.attachCustomKeyEventHandler((e) => !(e.ctrlKey && e.key === "`"));
    this.term.onData((data) => this.send(data));
    this.term.onResize(({ cols, rows }) => {
      if (this.ptyId !== undefined) window.jkb.terminal.resize(this.ptyId, cols, rows);
    });
    this.host = document.createElement("div");
    this.host.className = "terminal-host";
  }

  private send(data: string): void {
    if (this.ptyId === undefined) return;
    for (const chunk of chunkWrite(data)) window.jkb.terminal.write(this.ptyId, chunk);
  }

  /** xterm drew `chars` of `id`'s output: tell main in batches, so it reads the PTY again (`FLOW`). */
  private drew(id: number, chars: number): void {
    if (this.drawn.id !== id) this.drawn = { id, chars: 0 };
    this.drawn.chars += chars;
    if (this.drawn.chars >= FLOW.ackBatch) {
      window.jkb.terminal.ack(id, this.drawn.chars);
      this.drawn.chars = 0;
    }
  }

  private writeNote(text: string): void {
    if (!this.disposed) this.term.write(note(text));
  }

  /**
   * Show the terminal in `container` (moving it from wherever it was). The first time, xterm is
   * opened only once the monospace font has loaded: it measures its cell from the font when it
   * opens and never again on its own, so a terminal opened on the fallback font keeps the
   * fallback's cell size and draws the real glyphs into it.
   */
  attach(container: HTMLElement): void {
    container.appendChild(this.host);
    if (this.opened) {
      this.fit();
      return;
    }
    this.opening ??= fontLoaded().then(() => {
      if (this.disposed) return;
      this.term.open(this.host);
      this.opened = true;
      this.fit();
      if (this.host.clientWidth > 0) this.term.focus();
    });
  }

  /** Fit the terminal to its view. A hidden view has no size, and is left alone. */
  fit(): void {
    if (!this.opened || this.host.clientWidth === 0 || this.host.clientHeight === 0) return;
    try {
      this.fitAddon.fit();
    } catch {
      // Not yet measurable (fonts loading); the next resize fits it.
    }
  }

  focus(): void {
    this.term.focus();
  }

  setTheme(theme: ITheme): void {
    this.term.options.theme = theme;
  }

  /**
   * Start (or start again) the program `spec` names, ending the one running first. A program on the
   * host runs only once main's own dialog confirmed it (`confirmHost`; it does not ask again for one
   * already confirmed in this window). With `requireEnded` (the target toggle), a container program
   * that could not be confirmed ended keeps the new one from starting: two programs on one session
   * is worse than none.
   */
  async start(spec: TerminalSpec, options: { readonly requireEnded?: boolean } = {}): Promise<void> {
    const generation = ++this.generation;
    this.onStatus({ kind: "starting" });
    if (spec.target === "host" && spec.argv.length > 0) {
      let confirmed;
      try {
        confirmed = await window.jkb.terminal.confirmHost(spec);
      } catch (e) {
        confirmed = { ok: false as const, error: e instanceof Error ? e.message : String(e) };
      }
      if (this.disposed || generation !== this.generation) return;
      if (!confirmed.ok || !confirmed.value) {
        const why = confirmed.ok ? "running it on the host was not confirmed" : confirmed.error;
        this.writeNote(`not started: ${why}`);
        this.onStatus({ kind: "failed", error: why });
        return;
      }
    }
    const ended = await this.stop();
    if (this.disposed || generation !== this.generation) return;
    if (ended !== undefined) {
      this.writeNote(`[${ended.detail}]`);
      if (options.requireEnded === true && !ended.confirmed) {
        const why = `not started on the ${spec.target}: ${ended.detail}. Restart runs it anyway.`;
        this.writeNote(why);
        this.onStatus({ kind: "failed", error: why });
        return;
      }
    }
    this.fit();
    let result;
    try {
      result = await window.jkb.terminal.open(spec, this.term.cols, this.term.rows);
    } catch (e) {
      result = { ok: false as const, error: e instanceof Error ? e.message : String(e) };
    }
    if (this.disposed || generation !== this.generation) {
      // Closed or restarted while it was starting: end the one that just started.
      if (result.ok) {
        this.router.retire(result.value.id);
        void window.jkb.terminal.close(result.value.id);
      }
      return;
    }
    if (!result.ok) {
      this.term.write(note(`could not start: ${result.error}`));
      this.onStatus({ kind: "failed", error: result.error });
      return;
    }
    const id = result.value.id;
    this.ptyId = id;
    this.ptyTarget = spec.target;
    this.onStatus({ kind: "running" });
    this.router.claim(id, (event) => this.receive(id, event));
    // The size may have changed while the open was in flight.
    if (this.ptyId === id) window.jkb.terminal.resize(id, this.term.cols, this.term.rows);
  }

  private receive(id: number, event: TerminalEvent): void {
    if (id !== this.ptyId) return;
    if (event.kind === "data") {
      const chars = event.data.length;
      this.term.write(event.data, () => this.drew(id, chars));
      return;
    }
    this.ptyId = undefined;
    this.term.write(note(event.signal ? `[ended by signal ${event.signal}]` : `[exited with code ${event.exitCode}]`));
    this.onStatus({ kind: "exited", exitCode: event.exitCode, ...(event.signal ? { signal: event.signal } : {}) });
  }

  /**
   * End the running program, if any, without disposing the screen: what main saw of its ending, or
   * `undefined` when nothing was running.
   */
  private async stop(): Promise<TerminalEnd | undefined> {
    if (this.ptyId === undefined) return undefined;
    const id = this.ptyId;
    this.ptyId = undefined;
    this.router.retire(id);
    try {
      const result = await window.jkb.terminal.close(id);
      // Gone already (it exited as it was closed): its exit was the ending.
      return result.ok ? result.value : undefined;
    } catch (e) {
      return { target: this.ptyTarget, confirmed: false, detail: `could not end it: ${e instanceof Error ? e.message : String(e)}` };
    }
  }

  dispose(): void {
    this.disposed = true;
    void this.stop();
    this.term.dispose();
    this.host.remove();
  }
}
