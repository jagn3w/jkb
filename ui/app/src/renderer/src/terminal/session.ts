//! One terminal's xterm.js instance and the PTY behind it.
//
// Lives outside React: a terminal's screen and scrollback must survive its view being unmounted
// and mounted elsewhere (a tab switch, a move from the popover to the drawer), so the xterm
// element is created once and re-parented into whichever view shows it.

import { FitAddon } from "@xterm/addon-fit";
import { Terminal, type ITheme } from "@xterm/xterm";

import { MAX_WRITE_CHARS, type TerminalEvent, type TerminalSpec } from "../../../shared/terminal";
import type { TerminalEventRouter } from "./router";
import type { TerminalStatus } from "./state";

/** The terminal's colours, from the design tokens, so it follows the app's light and dark. */
export function themeFromTokens(): ITheme {
  const css = getComputedStyle(document.documentElement);
  const token = (name: string, fallback: string): string => css.getPropertyValue(name).trim() || fallback;
  return {
    background: token("--terminal-bg", "#ffffff"),
    foreground: token("--ink", "#191919"),
    cursor: token("--ink", "#191919"),
    cursorAccent: token("--terminal-bg", "#ffffff"),
    selectionBackground: token("--terminal-selection", "#d3d3d1"),
  };
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
  private generation = 0;
  private disposed = false;

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
    for (let i = 0; i < data.length; i += MAX_WRITE_CHARS) {
      window.jkb.terminal.write(this.ptyId, data.slice(i, i + MAX_WRITE_CHARS));
    }
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

  /** Start (or start again) the program `spec` names. */
  async start(spec: TerminalSpec): Promise<void> {
    this.stop();
    const generation = ++this.generation;
    this.onStatus({ kind: "starting" });
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
        window.jkb.terminal.close(result.value.id);
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
    this.onStatus({ kind: "running" });
    this.router.claim(id, (event) => this.receive(id, event));
    // The size may have changed while the open was in flight.
    if (this.ptyId === id) window.jkb.terminal.resize(id, this.term.cols, this.term.rows);
  }

  private receive(id: number, event: TerminalEvent): void {
    if (id !== this.ptyId) return;
    if (event.kind === "data") {
      this.term.write(event.data);
      return;
    }
    this.ptyId = undefined;
    this.term.write(note(event.signal ? `[ended by signal ${event.signal}]` : `[exited with code ${event.exitCode}]`));
    this.onStatus({ kind: "exited", exitCode: event.exitCode, ...(event.signal ? { signal: event.signal } : {}) });
  }

  /** End the running program, if any, without disposing the screen. */
  private stop(): void {
    if (this.ptyId === undefined) return;
    const id = this.ptyId;
    this.ptyId = undefined;
    this.router.retire(id);
    window.jkb.terminal.close(id);
  }

  dispose(): void {
    this.disposed = true;
    this.stop();
    this.term.dispose();
    this.host.remove();
  }
}
