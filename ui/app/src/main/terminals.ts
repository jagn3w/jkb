//! The integrated terminal's PTYs (D53.10): main runs them, the renderer only draws them.
//
// Plain Node (no Electron), so it is tested with real PTYs and a stand-in `docker` outside the
// app. `node-pty` is injected rather than imported, for the same reason.
//
// A terminal is made from a spec the renderer sends, validated by `parseSpec` before anything
// runs. A container terminal is `docker exec -it -w <cwd> <container> <wrapper> <argv>`, the
// wrapper recording the program's process group inside the container so that closing it can end it
// there (`END_SCRIPT`), not only end the docker client. A host terminal is the login shell, which
// runs nothing until it is typed into, or a spec main itself built and issued (`issueHost`, the
// Container tab's `run.sh` actions); a host argv the renderer made up is refused. That refusal
// stops ACCIDENTAL host routing only: a host login shell takes `write`, so the renderer is trusted
// for host execution, and the defence is that the renderer is the app's own code (contextIsolation,
// sandbox, the bridge's top-frame check, a `script-src 'self'` CSP; D53.1, D53.10). A terminal's
// target is fixed when it opens. Every terminal belongs to the window that opened it: only that
// window can write to it, resize it, acknowledge its output or close it, it hears only that
// window's events, and it is ended when that window closes.

import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { realpathSync, statSync } from "node:fs";
import { homedir, userInfo } from "node:os";
import { join } from "node:path";

import {
  FLOW,
  MAX_ACK_CHARS,
  MAX_WRITE_CHARS,
  isValidSize,
  parseSpec,
  type TerminalEnd,
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
  /** Stop reading the PTY (its program blocks once the kernel's buffer fills), and start again. */
  pause(): void;
  resume(): void;
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
  /**
   * Run a short command to completion (the in-container end), with its exit code (`null` when it
   * was killed or never started) and what it printed.
   */
  run(file: string, args: readonly string[], timeoutMs: number): Promise<{ code: number | null; output: string }>;
  /** Where, inside the container, each container terminal's process is recorded. */
  readonly containerRunDir: string;
  /** Whether process `pid` (a PTY's child) still exists. */
  processAlive(pid: number): boolean;
}

/**
 * How often a paused PTY's program is checked for having exited. node-pty 1.1.0 waits at most
 * 200 ms (`DESTROY_SOCKET_TIMEOUT_MS` in its `unixTerminal.js`) after its child exits for the
 * socket to drain, then destroys it with whatever is unread; a socket left paused never drains.
 */
export const PAUSED_EXIT_CHECK_MS = 50;

/** `TerminalEnvironment.containerRunDir` on a real machine: the container's own tmp. */
export const CONTAINER_RUN_DIR = "/tmp/jkb-terminals";

/**
 * The wrapper every container program runs under: `$1` is the file to record in, the rest is the
 * program. It records its own PID and then becomes the program (`exec`), so the PID is the
 * program's. `docker exec -t` starts it as a session (and so a process group) leader, which is what
 * `END_SCRIPT` signals. If it cannot record, it refuses to run rather than start something closing
 * could not end.
 */
export const WRAPPER_SCRIPT =
  'mkdir -p "${1%/*}" && printf \'%s\\n\' "$$" >"$1" || { echo "jkb: could not record this terminal in $1; not starting it" >&2; exit 125; }; shift; exec "$@"';

/**
 * Ends a container program from a second `docker exec`: `$1` is the file the wrapper recorded in.
 * It sends the recorded process group a hangup — what closing a terminal sends, and what an
 * interactive shell (which ignores TERM) passes on to its jobs — then TERM after 2 s and KILL after
 * 4 s, and succeeds only once the recorded process is gone or a zombie (exit 0). 3: nothing
 * recorded; 4: still running after 5 s.
 */
export const END_SCRIPT = [
  'p=$(cat "$1" 2>/dev/null) || { echo "nothing recorded at $1" >&2; exit 3; }',
  'case $p in ""|*[!0-9]*) echo "not a process id in $1" >&2; exit 3;; esac',
  // Gone, or a zombie its parent has not reaped yet (it has ended either way).
  'running() { kill -0 "$p" 2>/dev/null || return 1; s=$(cat "/proc/$p/stat" 2>/dev/null) || return 0; case ${s##*") "} in Z*) return 1;; esac; return 0; }',
  'kill -s HUP -- "-$p" 2>/dev/null',
  "i=0",
  "while running; do",
  "  i=$((i+1))",
  '  [ "$i" -eq 20 ] && kill -s TERM -- "-$p" 2>/dev/null',
  '  [ "$i" -eq 40 ] && kill -s KILL -- "-$p" 2>/dev/null',
  '  [ "$i" -gt 50 ] && { echo "process $p is still running" >&2; exit 4; }',
  "  sleep 0.1",
  "done",
  'rm -f "$1"',
].join("\n");

/** How long main waits for the in-container end, and for a host program to exit after its hangup. */
export const END_TIMEOUT_MS = 10_000;

/**
 * The host end, as `END_SCRIPT` does it in the container: a hangup, TERM if the program is still
 * there after `term` ms, KILL after `kill` ms, and unconfirmed if it has not exited by `giveUp`.
 */
export const HOST_END_MS = { term: 2_000, kill: 4_000, giveUp: 5_000 } as const;

/**
 * The container `docker exec` enters: `run.sh`'s own rule, `NAME="${JKB_CONTAINER_NAME:-jkb-dev}"` --
 * unset or empty is the default, anything else is taken as it is. It trimmed, so `'jkb-alt '` sent
 * terminals into `jkb-alt` while run.sh and the tab's buttons addressed `'jkb-alt '` (review s8 round 2);
 * untrimmed, such a name fails in both places alike.
 */
export function containerName(env: Readonly<Record<string, string | undefined>>): string {
  const name = env["JKB_CONTAINER_NAME"];
  return name === undefined || name === "" ? "jkb-dev" : name;
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

/** A terminal's tag: names its record in the container. Never a path or a flag. */
const TAG = /^[A-Za-z0-9-]{1,64}$/;

/** What to spawn for a spec: the program, its arguments, and the directory to start it in. */
export interface Command {
  readonly file: string;
  readonly args: readonly string[];
  readonly cwd: string;
  /** For a container program: the second `docker exec` that ends it inside the container. */
  readonly end?: { readonly file: string; readonly args: readonly string[] };
}

/**
 * The command a spec runs. A container spec's `cwd` is the container's and goes to `docker exec
 * -w`; the docker client itself starts in the host's home, which exists. `tag` names the
 * container program's record (`WRAPPER_SCRIPT`), unique per terminal.
 */
export function commandFor(spec: TerminalSpec, env: TerminalEnvironment, tag: string): TerminalResult<Command> {
  if (spec.target === "container") {
    const docker = env.docker();
    if (docker === undefined) {
      return { ok: false, error: `docker not found (looked in ${DOCKER_CANDIDATES.join(", ")})` };
    }
    const container = env.roots.container;
    if (!CONTAINER_NAME.test(container)) return { ok: false, error: `not a container name: ${JSON.stringify(container)}` };
    if (!TAG.test(tag)) return { ok: false, error: `not a terminal tag: ${JSON.stringify(tag)}` };
    const record = `${env.containerRunDir.replace(/\/+$/, "")}/${tag}.pid`;
    const program = spec.argv.length > 0 ? spec.argv : CONTAINER_SHELL;
    return {
      ok: true,
      value: {
        file: docker,
        // Everything after the container name is the command, never docker's flags.
        args: [
          "exec", "-i", "-t", "-e", `TERM=${TERM}`, "-e", "COLORTERM=truecolor", "-w", spec.cwd, container,
          "/bin/sh", "-c", WRAPPER_SCRIPT, "jkb-terminal", record, ...program,
        ],
        cwd: env.roots.hostHome,
        end: { file: docker, args: ["exec", container, "/bin/sh", "-c", END_SCRIPT, "jkb-terminal-end", record] },
      },
    };
  }
  if (!env.isDirectory(spec.cwd)) return { ok: false, error: `no such directory on the host: ${spec.cwd}` };
  const [program, ...args] = spec.argv;
  if (program === undefined) return { ok: true, value: { file: env.hostShell, args: ["-l"], cwd: spec.cwd } };
  // Only main's own specs carry a host argv, and they name their program absolutely; nothing is
  // looked up on the app's PATH (a Dock-launched app's is launchd's, not the shell's).
  if (!program.startsWith("/")) return { ok: false, error: `a host program is named by absolute path: ${program}` };
  return { ok: true, value: { file: program, args, cwd: spec.cwd } };
}

/**
 * The environment any program main starts on the host begins from: main's own, without Electron's
 * variables (`ELECTRON_RUN_AS_NODE` would turn a child Electron into Node). The one rule, for a
 * terminal and for the container kit's `run.sh --status` alike, so the run.sh that `--status` asks and
 * the run.sh a button runs are handed the same `JKB_CONTAINER_NAME`/`JKB_CONTAINER_IMAGE`. Which
 * container a CONTAINER terminal enters is `containerName`'s, below.
 */
export function hostEnv(env: Readonly<Record<string, string | undefined>>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(env)) {
    if (v !== undefined && !k.startsWith("ELECTRON_")) out[k] = v;
  }
  return out;
}

/** The environment a terminal's process starts with: `hostEnv`, with the terminal's `TERM`. */
export function terminalEnv(env: Readonly<Record<string, string | undefined>>): Record<string, string> {
  const out = hostEnv(env);
  out["TERM"] = TERM;
  out["COLORTERM"] = "truecolor";
  return out;
}

/**
 * The ACCOUNT's home — the passwd entry, which is what `run.sh` builds its own HOME from (and so
 * where it finds the kit and mounts `~/repos` from) — rather than `$HOME`, which a launching
 * terminal can set.
 */
export function accountHome(): string {
  try {
    return userInfo().homedir || homedir();
  } catch {
    return homedir();
  }
}

/** The container's repos directory (`CTR_REPOS` in `.container/run.sh`). */
export const CONTAINER_REPOS = "/home/vscode/repos";

/**
 * Where terminals run on this machine: the container `run.sh` starts (`$JKB_CONTAINER_NAME`, the
 * variable it reads, default `jkb-dev` — read from the app's OWN environment, which for a
 * Dock-launched app is not the shell's; ui/README.md says how to set it there) and the repos mount,
 * the one directory both sides see: `~/repos` as spelled and with its links resolved
 * (`HOST_REPOS`/`HOST_REPOS_REAL`), so a path reached through either maps into the container.
 */
export function machineRoots(
  home: string,
  env: Readonly<Record<string, string | undefined>>,
  account: string = accountHome(),
): TerminalRoots {
  // `run.sh` mounts `~/repos` of the ACCOUNT's home, whatever `$HOME` the app was started with.
  const hostRepos = join(account, "repos");
  let hostReposReal = hostRepos;
  try {
    hostReposReal = realpathSync(hostRepos);
  } catch {
    // Not there yet: only the spelling can name it.
  }
  return {
    container: containerName(env),
    containerRepos: CONTAINER_REPOS,
    hostRepos,
    hostReposReal,
    hostHome: home,
  };
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
    run: runToEnd(env),
    containerRunDir: CONTAINER_RUN_DIR,
    processAlive: (pid) => {
      try {
        process.kill(pid, 0);
        return true;
      } catch (e) {
        // EPERM: it exists, as someone else's.
        return (e as { code?: unknown }).code === "EPERM";
      }
    },
  };
}

/** `TerminalEnvironment.run` with `execFile`, in `env` (without Electron's variables). */
export function runToEnd(env: Readonly<Record<string, string | undefined>>): TerminalEnvironment["run"] {
  return (file, args, timeoutMs) =>
    new Promise((resolve) => {
      execFile(file, [...args], { env: terminalEnv(env), timeout: timeoutMs, maxBuffer: 64 * 1024 }, (error, stdout, stderr) => {
        const output = `${stdout}${stderr}`.trim();
        if (error === null) return resolve({ code: 0, output });
        const code = (error as { code?: unknown }).code;
        resolve({ code: typeof code === "number" ? code : null, output: output || error.message });
      });
    });
}

interface Running {
  readonly owner: number;
  readonly info: TerminalInfo;
  readonly pty: Pty;
  /** The second docker exec that ends a container program inside the container. */
  readonly end: Command["end"];
  /** Resolves when the PTY's program has exited. */
  readonly exited: Promise<void>;
  pending: string;
  timer: ReturnType<typeof setTimeout> | undefined;
  /** Characters sent and not yet acknowledged as drawn (flow control). */
  unacked: number;
  paused: boolean;
  /** While paused: the check for the program having exited (`PAUSED_EXIT_CHECK_MS`). */
  exitCheck: ReturnType<typeof setInterval> | undefined;
  /** The program has exited: whatever is left is read through, never paused for again. */
  draining: boolean;
  /**
   * Its window closed or reloaded (`closeAll`): it is still read (and ended), but nothing more is
   * sent — a reloaded page under the same id would hold its output and exit as a terminal opening.
   */
  disowned: boolean;
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

  /** Per window: the host programs (cwd and argv) main itself built and handed that window. */
  private readonly issued = new Map<number, Set<string>>();

  constructor(
    private readonly spawn: SpawnPty,
    private readonly environment: TerminalEnvironment,
    /** Delivers an event to the window that owns the terminal. */
    private readonly emit: (owner: number, event: TerminalEvent) => void,
    private readonly newTag: () => string = () => randomUUID(),
  ) {}

  private static hostKey(spec: TerminalSpec): string {
    return JSON.stringify([spec.cwd, spec.argv]);
  }

  /**
   * Let `owner` run `spec` on the host: only for a spec main built itself from its own constants
   * (the kit's `run.sh` with an action's flag, D53.8), as it hands it out — never for one the
   * renderer sent. It lasts until that window closes or reloads.
   */
  issueHost(owner: number, spec: TerminalSpec): void {
    const set = this.issued.get(owner) ?? new Set<string>();
    set.add(TerminalHost.hostKey(spec));
    this.issued.set(owner, set);
  }

  /** Start a terminal for `owner` from an unvalidated spec, at an initial size. */
  open(owner: number, rawSpec: unknown, cols: unknown, rows: unknown): TerminalResult<TerminalInfo> {
    const parsed = parseSpec(rawSpec);
    if (!parsed.ok) return parsed;
    if (!isValidSize(cols, rows)) return { ok: false, error: "not a terminal size" };
    const spec = parsed.value;
    // The host runs the login shell, or a program main built; never argv the renderer chose.
    if (spec.target === "host" && spec.argv.length > 0 && !(this.issued.get(owner)?.has(TerminalHost.hostKey(spec)) ?? false)) {
      return { ok: false, error: "the host runs only its login shell or what the app itself built; programs run in the container" };
    }
    const command = commandFor(spec, this.environment, this.newTag());
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
    let markExited = (): void => undefined;
    const exited = new Promise<void>((resolve) => (markExited = resolve));
    const entry: Running = {
      owner,
      info: { id, spec },
      pty,
      end: command.value.end,
      exited,
      pending: "",
      timer: undefined,
      unacked: 0,
      paused: false,
      exitCheck: undefined,
      draining: false,
      disowned: false,
    };
    this.running.set(id, entry);
    pty.onData((data) => {
      entry.pending += data;
      if (entry.pending.length >= FLUSH_BYTES) this.flush(entry);
      else entry.timer ??= setTimeout(() => this.flush(entry), FLUSH_MS);
    });
    pty.onExit(({ exitCode, signal }) => {
      // Never paused again: the final flush must not start an exit check nothing would clear.
      entry.draining = true;
      this.unpause(entry);
      this.flush(entry);
      this.running.delete(id);
      markExited();
      if (!entry.disowned) this.emit(owner, { id, kind: "exit", exitCode, ...(signal ? { signal } : {}) });
    });
    return { ok: true, value: entry.info };
  }

  private flush(entry: Running): void {
    if (entry.timer !== undefined) clearTimeout(entry.timer);
    entry.timer = undefined;
    if (entry.pending === "") return;
    const data = entry.pending;
    entry.pending = "";
    entry.unacked += data.length;
    // Past the high watermark the PTY is not read until the renderer catches up: the program
    // blocks on a full buffer instead of growing the IPC queue and xterm's write buffer.
    if (!entry.paused && !entry.draining && entry.unacked > FLOW.high && this.running.has(entry.info.id)) {
      entry.paused = true;
      try {
        entry.pty.pause();
      } catch {
        // Exited: nothing to pause.
      }
      // A program that exits while its output is unread would lose that output to node-pty's
      // 200 ms destroy timer: once it is gone, the rest is read through, acknowledged or not.
      entry.exitCheck = setInterval(() => {
        if (this.environment.processAlive(entry.pty.pid)) return;
        entry.draining = true;
        this.unpause(entry);
      }, PAUSED_EXIT_CHECK_MS);
      entry.exitCheck.unref?.();
    }
    if (!entry.disowned) this.emit(entry.owner, { id: entry.info.id, kind: "data", data });
  }

  /** The renderer drew `chars` of a terminal's output. Whether it was taken. */
  ack(owner: number, id: unknown, chars: unknown): boolean {
    const entry = this.owned(owner, id);
    if (entry === undefined || !Number.isInteger(chars) || (chars as number) < 1 || (chars as number) > MAX_ACK_CHARS) return false;
    entry.unacked = Math.max(0, entry.unacked - (chars as number));
    if (entry.paused && entry.unacked < FLOW.low) this.unpause(entry);
    return true;
  }

  private unpause(entry: Running): void {
    if (entry.exitCheck !== undefined) clearInterval(entry.exitCheck);
    entry.exitCheck = undefined;
    if (!entry.paused) return;
    entry.paused = false;
    try {
      entry.pty.resume();
    } catch {
      // Gone: nothing to resume.
    }
  }

  /** Whether main has stopped reading terminal `id` until its output is drawn (for tests). */
  isPaused(id: number): boolean {
    return this.running.get(id)?.paused ?? false;
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

  /**
   * End a terminal's program, and say whether it is seen to have ended. The PTY's exit still
   * arrives as an event. A host program is sent a hangup and is confirmed by its exit. A container
   * program is ended inside the container by the end command, and confirmed only by that command's
   * success: killing the `docker exec` client is not taken to end what it runs.
   */
  async close(owner: number, id: unknown): Promise<TerminalResult<TerminalEnd>> {
    const entry = this.owned(owner, id);
    if (entry === undefined) return { ok: false, error: "no such terminal" };
    return { ok: true, value: await this.end(entry) };
  }

  private async end(entry: Running): Promise<TerminalEnd> {
    // Read through from now on: a program that writes as it handles its hangup (`make`'s
    // "*** Hangup", a cleanup message) would otherwise block on a full buffer nobody will
    // acknowledge — the renderer stopped listening when it asked for the end — and never exit.
    entry.draining = true;
    this.unpause(entry);
    if (entry.end === undefined) return this.endHost(entry);
    try {
      entry.pty.kill();
    } catch {
      // Already gone.
    }
    let result: { code: number | null; output: string };
    try {
      result = await this.environment.run(entry.end.file, entry.end.args, END_TIMEOUT_MS);
    } catch (e) {
      result = { code: null, output: e instanceof Error ? e.message : String(e) };
    }
    return result.code === 0
      ? { target: "container", confirmed: true, detail: "the program in the container ended" }
      : {
          target: "container",
          confirmed: false,
          detail: `could not confirm the program in the container ended (${result.code === null ? "the end command did not finish" : `exit ${result.code}`}${result.output ? `: ${result.output}` : ""}); it may still be running`,
        };
  }

  /** A host program: hangup, then TERM, then KILL (`HOST_END_MS`), confirmed by its exit. */
  private async endHost(entry: Running): Promise<TerminalEnd> {
    let gone = false;
    void entry.exited.then(() => (gone = true));
    const signal = (sig: string): void => {
      try {
        if (!gone) entry.pty.kill(sig);
      } catch {
        // Already gone.
      }
    };
    const wait = (ms: number): Promise<boolean> =>
      Promise.race([
        entry.exited.then(() => true),
        new Promise<boolean>((resolve) => setTimeout(() => resolve(false), ms).unref?.()),
      ]);
    signal("SIGHUP");
    if (await wait(HOST_END_MS.term)) return { target: "host", confirmed: true, detail: "the program ended" };
    signal("SIGTERM");
    if (await wait(HOST_END_MS.kill - HOST_END_MS.term)) return { target: "host", confirmed: true, detail: "the program ended" };
    signal("SIGKILL");
    if (await wait(HOST_END_MS.giveUp - HOST_END_MS.kill)) return { target: "host", confirmed: true, detail: "the program ended (killed)" };
    return { target: "host", confirmed: false, detail: `the program had not exited ${HOST_END_MS.giveUp / 1000} s after a hangup, TERM and KILL; it may still be running` };
  }

  /**
   * End every terminal `owner` runs (its window closed or reloaded), or every terminal (the app
   * quits). The ends are sent, not awaited: there is no screen left to report them on.
   */
  closeAll(owner?: number): void {
    for (const entry of [...this.running.values()]) {
      if (owner !== undefined && entry.owner !== owner) continue;
      if (entry.timer !== undefined) clearTimeout(entry.timer);
      entry.disowned = true;
      this.running.delete(entry.info.id);
      void this.end(entry);
    }
    if (owner === undefined) this.issued.clear();
    else this.issued.delete(owner);
  }

  /** The terminals `owner` runs. */
  list(owner: number): TerminalInfo[] {
    return [...this.running.values()].filter((e) => e.owner === owner).map((e) => e.info);
  }
}
