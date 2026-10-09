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
import { TerminalRun } from "./run";
import { MIN_CONTRAST_RATIO, themeFrom } from "./theme";

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
  private disposed = false;
  /** When its programs start and end (`run.ts`); this class only draws them. */
  private readonly run: TerminalRun;
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
      minimumContrastRatio: MIN_CONTRAST_RATIO,
      theme: themeFromTokens(),
    });
    this.term.loadAddon(this.fitAddon);
    // Ctrl+` belongs to the app (it toggles the drawer), not to the program in the terminal.
    this.term.attachCustomKeyEventHandler((e) => !(e.ctrlKey && e.key === "`"));
    this.term.onData((data) => this.send(data));
    this.term.onResize(({ cols, rows }) => {
      const id = this.run.id;
      if (id !== undefined) window.jkb.terminal.resize(id, cols, rows);
    });
    this.host = document.createElement("div");
    this.host.className = "terminal-host";
    this.run = new TerminalRun(
      { open: (spec, cols, rows) => window.jkb.terminal.open(spec, cols, rows), close: (id) => window.jkb.terminal.close(id) },
      {
        size: () => {
          this.fit();
          return { cols: this.term.cols, rows: this.term.rows };
        },
        note: (text) => this.writeNote(text),
        status: (status) => this.onStatus(status),
        attach: (id) => {
          this.router.claim(id, (event) => this.receive(id, event));
          // The size may have changed while the open was in flight.
          window.jkb.terminal.resize(id, this.term.cols, this.term.rows);
        },
        detach: (id) => this.router.retire(id),
      },
    );
  }

  private send(data: string): void {
    const id = this.run.id;
    if (id === undefined) return;
    for (const chunk of chunkWrite(data)) window.jkb.terminal.write(id, chunk);
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
   * Start (or start again) the program `spec` names, ending the one running first (`TerminalRun`):
   * not while the last end is unconfirmed, unless `override` (the person's Restart).
   */
  start(spec: TerminalSpec, options: { readonly override?: boolean } = {}): Promise<void> {
    return this.run.start(spec, options);
  }

  /** End the program (the tab is closing), and answer how that went; the screen stays and says so. */
  async end(): Promise<TerminalEnd | undefined> {
    const end = await this.run.stop();
    if (end !== undefined && !end.confirmed) this.writeNote(`[${end.detail}] Close the tab again to forget it.`);
    return end;
  }

  private receive(id: number, event: TerminalEvent): void {
    if (id !== this.run.id) return;
    if (event.kind === "data") {
      const chars = event.data.length;
      this.term.write(event.data, () => this.drew(id, chars));
      return;
    }
    this.run.exited(id);
    this.term.write(note(event.signal ? `[ended by signal ${event.signal}]` : `[exited with code ${event.exitCode}]`));
    this.onStatus({ kind: "exited", exitCode: event.exitCode, ...(event.signal ? { signal: event.signal } : {}) });
  }

  /** Dispose of the screen and end the program; answers how the end went. */
  dispose(): Promise<TerminalEnd | undefined> {
    this.disposed = true;
    const end = this.run.dispose();
    this.term.dispose();
    this.host.remove();
    return end;
  }
}
