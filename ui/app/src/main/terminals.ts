//! The integrated terminal's PTYs (D53.10): main runs them, the renderer only draws them.
//
// Plain Node (no Electron), so it is tested with real PTYs and a stand-in `docker` outside the
// app. `node-pty` is injected rather than imported, for the same reason.
//
// A terminal is made from a spec the renderer sends, validated by `parseSpec` before anything
// runs. A container terminal is `docker exec -it -w <cwd> <container> <argv>`; a host terminal runs
// `argv` itself. Every terminal belongs to the window that opened it: only that window can write
// to it, resize it or close it, it hears only that window's events, and it is killed when that
// window closes.

import { statSync } from "node:fs";

import {
  MAX_WRITE_CHARS,
  isValidSize,
  parseSpec,
  type TerminalEvent,
  type TerminalInfo,
  type TerminalResult,
  type TerminalRoots,
  type TerminalSpec,
} from "../shared/terminal";

/** The part of a `node-pty` `IPty` this module uses. */
export interface Pty {
  readonly pid: number;
  onData(listener: (data: string) => void): { dispose(): void };
  onExit(listener: (e: { exitCode: number; signal?: number }) => void): { dispose(): void };
  write(data: string): void;
  resize(cols: number, rows: number): void;
  kill(signal?: string): void;
}

/** `node-pty`'s `spawn`, narrowed to what this module passes. */
export type SpawnPty = (
  file: string,
  args: string[],
  options: { name: string; cols: number; rows: number; cwd: string; env: Record<string, string> },
) => Pty;

/** What main knows about the machine it runs on, resolved per open (docker may appear later). */
export interface TerminalEnvironment {
  readonly roots: TerminalRoots;
  /** The `docker` client, by absolute path, or `undefined` when none was found. */
  docker(): string | undefined;
  /** The host's login shell, by absolute path. */
  readonly hostShell: string;
  /** The environment a host terminal (and the docker client) starts with. */
  readonly env: Readonly<Record<string, string | undefined>>;
  /** Whether `path` is a directory on the host. */
  isDirectory(path: string): boolean;
}

/** The program a container terminal runs when its spec names none: the image's shell, by absolute path. */
export const CONTAINER_SHELL: readonly string[] = ["/bin/bash", "-l"];

/** `TERM` for every terminal: what xterm.js emulates. */
export const TERM = "xterm-256color";

/**
 * Where `docker` is looked for. Absolute paths only, never `PATH`: a GUI app's `PATH` is not the
 * shell's (on macOS it is launchd's minimal one), and the kit's rule is that what runs outside the
 * sandbox is named absolutely (`.container/README.md`). These are where Docker Desktop, Homebrew
 * and the distributions put it.
 */
export const DOCKER_CANDIDATES: readonly string[] = [
  "/usr/local/bin/docker",
  "/opt/homebrew/bin/docker",
  "/usr/bin/docker",
  "/Applications/Docker.app/Contents/Resources/bin/docker",
];

/** A container name `docker exec` will take as a name, never as a flag. */
const CONTAINER_NAME = /^[A-Za-z0-9][A-Za-z0-9_.-]*$/;

/** What to spawn for a spec: the program, its arguments, and the directory to start it in. */
export interface Command {
  readonly file: string;
  readonly args: readonly string[];
  readonly cwd: string;
}

/**
 * The command a spec runs. A container spec's `cwd` is the container's and goes to `docker exec
 * -w`; the docker client itself starts in the host's home, which exists.
 */
export function commandFor(spec: TerminalSpec, env: TerminalEnvironment): TerminalResult<Command> {
  if (spec.target === "container") {
    const docker = env.docker();
    if (docker === undefined) {
      return { ok: false, error: `docker not found (looked in ${DOCKER_CANDIDATES.join(", ")})` };
    }
    const container = env.roots.container;
    if (!CONTAINER_NAME.test(container)) return { ok: false, error: `not a container name: ${JSON.stringify(container)}` };
    const program = spec.argv.length > 0 ? spec.argv : CONTAINER_SHELL;
    return {
      ok: true,
      value: {
        file: docker,
        // Everything after the container name is the command, never docker's flags.
        args: ["exec", "-i", "-t", "-e", `TERM=${TERM}`, "-e", "COLORTERM=truecolor", "-w", spec.cwd, container, ...program],
        cwd: env.roots.hostHome,
      },
    };
  }
  if (!env.isDirectory(spec.cwd)) return { ok: false, error: `no such directory on the host: ${spec.cwd}` };
  const [file, ...args] = spec.argv.length > 0 ? spec.argv : [env.hostShell, "-l"];
  return { ok: true, value: { file: file ?? env.hostShell, args, cwd: spec.cwd } };
}

/**
 * The environment a terminal's process starts with: main's own, without Electron's variables
 * (`ELECTRON_RUN_AS_NODE` would turn a child Electron into Node), and with the terminal's `TERM`.
 */
export function terminalEnv(env: Readonly<Record<string, string | undefined>>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(env)) {
    if (v !== undefined && !k.startsWith("ELECTRON_")) out[k] = v;
  }
  out["TERM"] = TERM;
  out["COLORTERM"] = "truecolor";
  return out;
}

/** The real machine: docker by `DOCKER_CANDIDATES`, the host shell from `$SHELL`. */
export function machineEnvironment(roots: TerminalRoots, env: Readonly<Record<string, string | undefined>>, loginShell?: string): TerminalEnvironment {
  const isFile = (p: string): boolean => {
    try {
      return statSync(p).isFile();
    } catch {
      return false;
    }
  };
  const shell = [env["SHELL"], loginShell, "/bin/zsh", "/bin/bash", "/bin/sh"].find(
    (s): s is string => s !== undefined && s.startsWith("/") && isFile(s),
  );
  return {
    roots,
    docker: () => DOCKER_CANDIDATES.find(isFile),
    hostShell: shell ?? "/bin/sh",
    env,
    isDirectory: (p) => {
      try {
        return statSync(p).isDirectory();
      } catch {
        return false;
      }
    },
  };
}

interface Running {
  readonly owner: number;
  readonly info: TerminalInfo;
  readonly pty: Pty;
  pending: string;
  timer: ReturnType<typeof setTimeout> | undefined;
}

/**
 * How long output is gathered before it is sent. One IPC message per PTY read costs more than the
 * drawing it feeds when a build scrolls; a few milliseconds is below what a typist notices.
 */
export const FLUSH_MS = 4;

/** Output gathered past this is sent at once rather than waiting for the timer. */
export const FLUSH_BYTES = 256 * 1024;

/** The PTYs main runs, keyed by id, each owned by the window (`webContents` id) that opened it. */
export class TerminalHost {
  private readonly running = new Map<number, Running>();
  private nextId = 1;

  constructor(
    private readonly spawn: SpawnPty,
    private readonly environment: TerminalEnvironment,
    /** Delivers an event to the window that owns the terminal. */
    private readonly emit: (owner: number, event: TerminalEvent) => void,
  ) {}

  /** Start a terminal for `owner` from an unvalidated spec, at an initial size. */
  open(owner: number, rawSpec: unknown, cols: unknown, rows: unknown): TerminalResult<TerminalInfo> {
    const parsed = parseSpec(rawSpec);
    if (!parsed.ok) return parsed;
    if (!isValidSize(cols, rows)) return { ok: false, error: "not a terminal size" };
    const spec = parsed.value;
    const command = commandFor(spec, this.environment);
    if (!command.ok) return command;

    let pty: Pty;
    try {
      pty = this.spawn(command.value.file, [...command.value.args], {
        name: "xterm-256color",
        cols: cols as number,
        rows: rows as number,
        cwd: command.value.cwd,
        env: terminalEnv(this.environment.env),
      });
    } catch (e) {
      return { ok: false, error: `could not start ${command.value.file}: ${e instanceof Error ? e.message : String(e)}` };
    }
    const id = this.nextId++;
    const entry: Running = { owner, info: { id, spec }, pty, pending: "", timer: undefined };
    this.running.set(id, entry);
    pty.onData((data) => {
      entry.pending += data;
      if (entry.pending.length >= FLUSH_BYTES) this.flush(entry);
      else entry.timer ??= setTimeout(() => this.flush(entry), FLUSH_MS);
    });
    pty.onExit(({ exitCode, signal }) => {
      this.flush(entry);
      this.running.delete(id);
      this.emit(owner, { id, kind: "exit", exitCode, ...(signal ? { signal } : {}) });
    });
    return { ok: true, value: entry.info };
  }

  private flush(entry: Running): void {
    if (entry.timer !== undefined) clearTimeout(entry.timer);
    entry.timer = undefined;
    if (entry.pending === "") return;
    const data = entry.pending;
    entry.pending = "";
    this.emit(entry.owner, { id: entry.info.id, kind: "data", data });
  }

  /** The terminal `id`, if `owner` owns it and it is still running. */
  private owned(owner: number, id: unknown): Running | undefined {
    if (typeof id !== "number") return undefined;
    const entry = this.running.get(id);
    return entry?.owner === owner ? entry : undefined;
  }

  /** Send keystrokes (or a paste) to a terminal. Whether it was taken. */
  write(owner: number, id: unknown, data: unknown): boolean {
    const entry = this.owned(owner, id);
    if (entry === undefined || typeof data !== "string" || data.length > MAX_WRITE_CHARS) return false;
    entry.pty.write(data);
    return true;
  }

  /** Resize a terminal. Whether it was taken. */
  resize(owner: number, id: unknown, cols: unknown, rows: unknown): boolean {
    const entry = this.owned(owner, id);
    if (entry === undefined || !isValidSize(cols, rows)) return false;
    try {
      entry.pty.resize(cols as number, rows as number);
    } catch {
      return false; // Exited between the check and the call: its exit event is on its way.
    }
    return true;
  }

  /** End a terminal's process. Its exit arrives as an event, as for any other ending. */
  close(owner: number, id: unknown): boolean {
    const entry = this.owned(owner, id);
    if (entry === undefined) return false;
    entry.pty.kill();
    return true;
  }

  /** End every terminal `owner` runs (its window closed), or every terminal (the app quits). */
  closeAll(owner?: number): void {
    for (const entry of [...this.running.values()]) {
      if (owner !== undefined && entry.owner !== owner) continue;
      if (entry.timer !== undefined) clearTimeout(entry.timer);
      this.running.delete(entry.info.id);
      try {
        entry.pty.kill();
      } catch {
        // Already gone.
      }
    }
  }

  /** The terminals `owner` runs. */
  list(owner: number): TerminalInfo[] {
    return [...this.running.values()].filter((e) => e.owner === owner).map((e) => e.info);
  }
}
