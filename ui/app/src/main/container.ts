//! The Container tab's half in main (D53.8): the installed kit's `run.sh`, found, asked for the
//! container's status, and named in the terminal spec each button opens.
//
// Plain Node (no Electron), so it is tested against a stand-in kit outside the app. Running a
// program is injected for the same reason.
//
// Never the checkout's `run.sh`, which the agent's sandbox can write: everything that runs outside
// the sandbox comes from the kit (D53.3, `.container/README.md`). The kit is where lib.sh's
// `DC_KIT_DIR` puts it — under the ACCOUNT's home, which is also what `run.sh` builds its own HOME
// from — and its `run.sh --kit-path` must answer that same directory before anything else is run
// from it. The renderer names an action and main turns it into the kit's program and its flag, so the
// tab never spells a path; main then issues that exact spec to the window (`TerminalHost.issueHost`),
// and `terminal.open` refuses any host argv it did not issue (D53.10) -- within the limits D53.10
// states for that gate.

import { execFile } from "node:child_process";
import { realpathSync, statSync } from "node:fs";
import { join } from "node:path";

import {
  CONTAINER_ACTIONS,
  KIT_DIR_IN_HOME,
  KIT_RUN_SH,
  flagFor,
  isContainerAction,
  parseContainerStatus,
  type ContainerResult,
  type ContainerStatus,
} from "@jkb/core";

import type { TerminalSpec } from "../shared/terminal";
import { hostEnv } from "./terminals";

/** What running a program to completion answered. `code` is null when it was killed (a timeout). */
export interface RunResult {
  readonly code: number | null;
  readonly stdout: string;
  readonly stderr: string;
  /** The run hit its timeout (`code` is then null). */
  readonly timedOut?: boolean;
  /** The run was cancelled through its `AbortSignal` (`code` is then null). */
  readonly cancelled?: boolean;
  /** The signal that ended the program, when one did. */
  readonly signal?: string;
  /**
   * From a runner that stops the whole process group when the run ends abnormally (timeout,
   * cancellation, a signal): false when it confirmed the group gone, true when it could not.
   * Absent when it never had to stop it.
   */
  readonly survivors?: boolean;
}

/**
 * Run `file` with `args`, without a shell, to completion, `timeoutMs`, or `signal`'s abort; `env` is
 * added to the runner's.
 */
export type RunFile = (
  file: string,
  args: readonly string[],
  timeoutMs: number,
  env?: Readonly<Record<string, string>>,
  signal?: AbortSignal,
) => Promise<RunResult>;

/** What main knows about the machine, injected so the tests can stand a kit up anywhere. */
export interface KitEnvironment {
  /** The account's home directory (the passwd entry's, as `run.sh` reads it). */
  readonly home: string;
  readonly run: RunFile;
  isFile(path: string): boolean;
  /** `path` with every link resolved, or `undefined` when it does not exist. */
  realpath(path: string): string | undefined;
}

/** How long `--kit-path` may take: it prints a constant. */
export const KIT_PATH_TIMEOUT_MS = 10_000;

/** How long `--status` may take: a handful of `docker inspect`s and the argument assembly. */
export const STATUS_TIMEOUT_MS = 60_000;

/** `run.sh` colours its errors; a message shown in the tab is plain text. */
export function plain(text: string): string {
  return text.replace(/\x1b\[[0-9;]*m/g, "").trim();
}

/** The last `n` non-empty lines of `text`, plain: what a failed run is reported by. */
function tail(text: string, n = 6): string {
  return plain(text)
    .split("\n")
    .filter((l) => l.trim() !== "")
    .slice(-n)
    .join("\n");
}

export class ContainerKit {
  constructor(private readonly env: KitEnvironment) {}

  /** Where the kit should be: `DC_KIT_DIR`. */
  get kitDir(): string {
    return join(this.env.home, KIT_DIR_IN_HOME);
  }

  /** The kit's `run.sh`. */
  get runSh(): string {
    return join(this.kitDir, KIT_RUN_SH);
  }

  /**
   * The kit's `run.sh`, once it is there and its own `--kit-path` names the kit it is in — so a kit
   * lib.sh would put elsewhere, or one that is not a kit at all, runs nothing further.
   */
  async locate(): Promise<ContainerResult<string>> {
    const runSh = this.runSh;
    if (!this.env.isFile(runSh)) {
      return {
        ok: false,
        error: `No container kit at ${this.kitDir}. Install it from a checkout you have reviewed: <checkout>/.container/run.sh --install-kit (scripts/setup.sh does this).`,
      };
    }
    let answer: RunResult;
    try {
      answer = await this.env.run(runSh, ["--kit-path"], KIT_PATH_TIMEOUT_MS);
    } catch (e) {
      return { ok: false, error: `could not run ${runSh}: ${e instanceof Error ? e.message : String(e)}` };
    }
    if (answer.code !== 0) return { ok: false, error: `${runSh} --kit-path failed: ${tail(answer.stderr) || `exit ${answer.code}`}` };
    const named = answer.stdout.trim();
    const want = this.env.realpath(this.kitDir);
    if (named === "" || want === undefined || this.env.realpath(named) !== want) {
      return {
        ok: false,
        error: `${runSh} says the kit is at ${named || "(nothing)"}, not ${this.kitDir}; refusing to run it. Reinstall the kit: <checkout>/.container/run.sh --install-kit`,
      };
    }
    return { ok: true, value: runSh };
  }

  /** `run.sh --status`, parsed. Read-only: it inspects the container and changes nothing. */
  async status(): Promise<ContainerResult<ContainerStatus>> {
    const located = await this.locate();
    if (!located.ok) return located;
    let r: RunResult;
    try {
      r = await this.env.run(located.value, ["--status"], STATUS_TIMEOUT_MS);
    } catch (e) {
      return { ok: false, error: `could not run ${located.value}: ${e instanceof Error ? e.message : String(e)}` };
    }
    if (r.code !== 0) {
      const why = r.code === null ? `timed out after ${STATUS_TIMEOUT_MS / 1000}s` : `exit ${r.code}`;
      return { ok: false, error: `run.sh --status failed (${why})${r.stderr.trim() === "" ? "" : `:\n${tail(r.stderr)}`}` };
    }
    return parseContainerStatus(r.stdout);
  }

  /**
   * The terminal a button opens: the kit's `run.sh` with the action's one flag, on the HOST (it
   * drives docker from outside the container), so its output streams into the integrated terminal.
   * `action` comes from the renderer and is checked here; nothing else of the spec does. The caller
   * issues the answer to the window (`issueHost`), the only host argv that window may then open.
   */
  async spec(action: unknown): Promise<ContainerResult<TerminalSpec>> {
    if (!isContainerAction(action)) return { ok: false, error: `not a container action: ${JSON.stringify(action)}` };
    const located = await this.locate();
    if (!located.ok) return located;
    const label = CONTAINER_ACTIONS.find((a) => a.id === action)?.label ?? action;
    return {
      ok: true,
      value: { target: "host", cwd: this.env.home, argv: [located.value, flagFor(action)], title: `container: ${label.toLowerCase()}` },
    };
  }
}

/** The real machine. `env` is what `run.sh` is started with; it rebuilds its own from an allowlist. */
export function machineKit(home: string, env: Readonly<Record<string, string | undefined>>): KitEnvironment {
  // The terminal's rule, not a copy of it: --status here and a button's run.sh in the terminal must
  // resolve JKB_CONTAINER_NAME/IMAGE alike.
  const childEnv = hostEnv(env);
  return {
    home,
    run: (file, args, timeoutMs) =>
      new Promise((resolve) => {
        execFile(
          file,
          [...args],
          { cwd: home, env: childEnv, timeout: timeoutMs, maxBuffer: 4 * 1024 * 1024, encoding: "utf8" },
          (error, stdout, stderr) => {
            const code = error === null ? 0 : typeof error.code === "number" ? error.code : null;
            resolve({ code, stdout, stderr: error !== null && code === null && stderr === "" ? error.message : stderr });
          },
        );
      }),
    isFile: (p) => {
      try {
        return statSync(p).isFile();
      } catch {
        return false;
      }
    },
    realpath: (p) => {
      try {
        return realpathSync(p);
      } catch {
        return undefined;
      }
    },
  };
}
