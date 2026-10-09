//! *jkb ▸ Update from main…* (D53.3) in main, without a window: fetch `main` to show what an update
//! would take, have the clone's own builder build and STAGE exactly that commit, and, as the app
//! quits, start the clone's install step to swap the staged copy in once nothing runs.
//
// Plain Node (no Electron), so it is tested against a real git repository standing in for the
// clone. Running a program is injected for the same reason.
//
// What runs is never a checkout's. The clone lives under the account's home, where no agent writes
// (the dev container does not mount it), and is moved only to `origin/main` — code that passed review
// and landed. The builder is that clone's `scripts/build-app.sh`, which moves the clone to the commit
// shown (refusing if `main` moved since), refuses any tree that is not the clone at `origin/main`,
// installs with the frozen lockfile and stages the packaged app. It never touches the installed app.
// `scripts/install-app.sh` is the one step that does, and only once no copy of the app is running:
// a running Electron app loads its helpers from the bundle by path, so it is never swapped under.

import { spawn, type ChildProcess } from "node:child_process";
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, readSync, realpathSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

import {
  APP_BUILDER_IN_SRC,
  APP_HOME_IN_HOME,
  APP_INSTALLER_IN_SRC,
  APP_INSTALL_LOG_IN_HOME,
  APP_INSTALL_RESULT_IN_HOME,
  APP_LOCK_IN_HOME,
  APP_LOG_IN_HOME,
  APP_SRC_IN_HOME,
  APP_STAGED_IN_HOME,
  APP_STAMP_IN_HOME,
  BUILD_EXIT,
  BUILT_COMMIT_IN_OUT,
  COMMIT_LOG_FORMAT,
  UPDATE_SHOWN_REF,
  UPDATE_SHOWN_REFSPEC,
  checkoutRefusal,
  installedExecutable,
  isCommitId,
  parseCommitLog,
  parseInstallResult,
  parseInstalledStamp,
  type ContainerResult,
  type UpdatePlan,
} from "@jkb/core";

import { GIT_SELECTION } from "../shared/gitEnv";

// The list scripts/lib.sh's APP_GIT_SELECTION mirrors (a test holds them equal), re-exported for it.
export { GIT_SELECTION };
import { plain } from "./container";

/** How long fetching `main` may take. */
export const FETCH_TIMEOUT_MS = 5 * 60_000;

/** How long a local git query may take. */
export const GIT_TIMEOUT_MS = 30_000;

/** How long the build may take: a fetch, a frozen install, two type-checks, a bundle and a package. */
export const BUILD_TIMEOUT_MS = 30 * 60_000;

/** How long a timed-out run's process group gets after SIGTERM before SIGKILL. */
export const KILL_GRACE_MS = 5_000;

/** The most output a run keeps, from each stream: the tail, which is what a failure is read from. */
const MAX_OUTPUT = 16 * 1024 * 1024;

type Result<T> = ContainerResult<T>;

/** What running a program to completion answered. */
export interface RunResult {
  /** The exit status, or null when it did not exit normally (a timeout, a signal, never started). */
  readonly code: number | null;
  readonly stdout: string;
  readonly stderr: string;
  /** The run hit its timeout, and its process group was stopped. */
  readonly timedOut?: boolean;
  /** The timeout needed SIGKILL: the program may not have run its own exit handling (a lock's release). */
  readonly forced?: boolean;
  /** The signal that ended the program, when one did. */
  readonly signal?: string;
}

/**
 * Run `file` with `args`, without a shell, to completion or `timeoutMs`. With `logFile`, its output
 * is appended to that file instead of piped back (and `stdout` is the file's tail): what a run that
 * must outlive this process needs, since a pipe into an exited process kills the writer.
 */
export type RunFile = (file: string, args: readonly string[], timeoutMs: number, logFile?: string) => Promise<RunResult>;

/** Start `file` with `args` detached, outliving this process, its output appended to `logFile`. */
export type StartDetached = (file: string, args: readonly string[], logFile: string) => void;

/** How a run that did not exit 0 ended, in words: what every failure here is reported by. */
export function describeEnd(r: RunResult, timeoutMs: number): string {
  if (r.timedOut === true) return `timed out after ${Math.round(timeoutMs / 1000)}s and was stopped`;
  if (r.signal !== undefined) return `was killed (${r.signal})`;
  if (r.code === null) return "could not be run";
  return `failed (exit ${r.code})`;
}

/** The last `n` non-empty lines of `text`, plain: what a failure is reported by. */
function tail(text: string, n = 12): string {
  return plain(text)
    .split("\n")
    .filter((l) => l.trim() !== "")
    .slice(-n)
    .join("\n");
}

/** What is running: its executable and platform (main's `app.getPath("exe")`), and its built-in commit. */
export interface RunningCopy {
  readonly exe: string;
  readonly platform: string;
  /** The commit built into this copy (`builtCommit`), or `undefined` for a build that predates it. */
  readonly commit: string | undefined;
}

/** Whether `exe`, with every link resolved, is the installed copy's executable under `home`. */
export function isInstalledCopy(exe: string, platform: string, home: string): boolean {
  try {
    return realpathSync(exe) === realpathSync(installedExecutable(platform, home));
  } catch {
    return false;
  }
}

/** What main knows at startup about the process it is. */
export interface StartupFacts {
  readonly isPackaged: boolean;
  readonly exe: string;
  readonly platform: string;
  /** The account's home. */
  readonly home: string;
  readonly env: Readonly<Record<string, string | undefined>>;
}

/**
 * Why this process may not run, or `undefined`: only the installed copy runs by default — packaged,
 * AND its executable (links resolved) is the one install-app.sh installs. A package left in a
 * checkout's `dist/`, a copy moved elsewhere, or a checkout's `out/` runs only with
 * `JKB_APP_FROM_CHECKOUT=1`.
 */
export function startupRefusal(f: StartupFacts): string | undefined {
  return checkoutRefusal(
    f.isPackaged && isInstalledCopy(f.exe, f.platform, f.home),
    f.env,
    installedExecutable(f.platform, f.home),
  );
}

/** The commit `build-app.sh` built into the app's `out/` directory, or `undefined`. */
export function builtCommit(outDir: string): string | undefined {
  try {
    const text = readFileSync(join(outDir, BUILT_COMMIT_IN_OUT), "utf8").trim();
    return isCommitId(text) ? text : undefined;
  } catch {
    return undefined;
  }
}

export interface UpdaterOptions {
  /** How long the build may take (`BUILD_TIMEOUT_MS`). */
  readonly buildTimeoutMs?: number;
}

export class AppUpdater {
  /** Whether an update is building now: the menu refuses a second. */
  private building = false;

  private readonly buildTimeoutMs: number;

  /**
   * `home` is the account's home; `run` runs a program to completion, without a shell; `start` starts
   * the install step detached; `running` is this process — an update runs only as the installed copy.
   */
  constructor(
    private readonly home: string,
    private readonly run: RunFile,
    private readonly start: StartDetached,
    private readonly running: RunningCopy,
    options: UpdaterOptions = {},
  ) {
    this.buildTimeoutMs = options.buildTimeoutMs ?? BUILD_TIMEOUT_MS;
  }

  get appHome(): string {
    return join(this.home, APP_HOME_IN_HOME);
  }

  get src(): string {
    return join(this.home, APP_SRC_IN_HOME);
  }

  get logFile(): string {
    return join(this.home, APP_LOG_IN_HOME);
  }

  get installLogFile(): string {
    return join(this.home, APP_INSTALL_LOG_IN_HOME);
  }

  get busy(): boolean {
    return this.building;
  }

  /**
   * Why this copy may not update itself, or `undefined`. The install step installs at the installed
   * copy's place: from anywhere else, an update would install there and relaunch that, leaving this
   * copy as it is and reporting it "up to date" for good.
   */
  private notInstalled(): string | undefined {
    const installed = installedExecutable(this.running.platform, this.home);
    if (isInstalledCopy(this.running.exe, this.running.platform, this.home)) return undefined;
    return (
      `This copy (${this.running.exe}) is not the installed one (${installed}). Update from main installs at ` +
      `${installed}, so it cannot update this copy. Run the installed copy, or update this one by other means.`
    );
  }

  private git(args: readonly string[], timeoutMs = GIT_TIMEOUT_MS): Promise<RunResult> {
    return this.run("git", ["-C", this.src, ...args], timeoutMs);
  }

  private async gitOut(args: readonly string[], what: string, timeoutMs = GIT_TIMEOUT_MS): Promise<Result<string>> {
    let r: RunResult;
    try {
      r = await this.git(args, timeoutMs);
    } catch (e) {
      return { ok: false, error: `could not run git (${what}): ${e instanceof Error ? e.message : String(e)}` };
    }
    if (r.code !== 0) {
      return { ok: false, error: `git ${what} ${describeEnd(r, timeoutMs)}${r.stderr.trim() === "" ? "" : `:\n${tail(r.stderr)}`}` };
    }
    return { ok: true, value: r.stdout };
  }

  /** The commit the stamp says is installed, or `undefined`. */
  installed(): string | undefined {
    try {
      return parseInstalledStamp(readFileSync(join(this.home, APP_STAMP_IN_HOME), "utf8"));
    } catch {
      return undefined;
    }
  }

  /** The commit staged for install (`build-app.sh`'s `staged/commit`, with its `app/`), or `undefined`. */
  stagedCommit(): string | undefined {
    const staged = join(this.home, APP_STAGED_IN_HOME);
    if (!existsSync(join(staged, "app"))) return undefined;
    try {
      const c = readFileSync(join(staged, "commit"), "utf8").trim();
      return isCommitId(c) ? c : undefined;
    } catch {
      return undefined;
    }
  }

  /**
   * A staged build the last install step did not install, and why, or `undefined`: something is
   * staged, it is not what runs, and `install.result` says the step ran for it and did not succeed
   * (75: another build or install held the lock; 76: a copy was running; else a failure). What the
   * app tells the user at startup, since the step ran after it quit.
   */
  pendingInstall(): { commit: string; status: number } | undefined {
    const staged = this.stagedCommit();
    if (staged === undefined || staged === this.running.commit) return undefined;
    let result: ReturnType<typeof parseInstallResult>;
    try {
      result = parseInstallResult(readFileSync(join(this.home, APP_INSTALL_RESULT_IN_HOME), "utf8"));
    } catch {
      return undefined;
    }
    if (result === undefined || result.status === 0) return undefined;
    if (result.commit !== undefined && result.commit !== staged) return undefined;
    return { commit: staged, status: result.status };
  }

  get lockDir(): string {
    return join(this.home, APP_LOCK_IN_HOME);
  }

  /**
   * Fetch `main` and say what an update would take. Builds and moves nothing; the fetch goes to
   * `UPDATE_SHOWN_REF`, which no build or install reads, so it needs no lock.
   */
  async plan(): Promise<Result<UpdatePlan>> {
    const refused = this.notInstalled();
    if (refused !== undefined) return { ok: false, error: refused };
    if (!existsSync(join(this.src, ".git"))) {
      return { ok: false, error: `No clean clone at ${this.src}. scripts/setup.sh creates it, from a checkout you have reviewed.` };
    }
    const fetched = await this.gitOut(
      // `--refmap=`: without it git also updates the configured remote-tracking ref (origin/main)
      // "opportunistically" — exactly the ref this fetch must leave to the build.
      ["fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--refmap=", "origin", UPDATE_SHOWN_REFSPEC],
      "fetch origin main",
      FETCH_TIMEOUT_MS,
    );
    if (!fetched.ok) return fetched;
    const tip = await this.gitOut(["rev-parse", "--verify", "--quiet", `${UPDATE_SHOWN_REF}^{commit}`], "rev-parse main");
    if (!tip.ok) return tip;
    const target = tip.value.trim();
    if (!isCommitId(target)) return { ok: false, error: `main is not a commit: ${JSON.stringify(target)}` };
    // What is running, when the build says; the stamp only says what was installed last.
    const installed = this.running.commit ?? this.installed();
    if (installed === undefined || installed === target) return { ok: true, value: { installed, target, commits: [], diverged: false } };
    let diverged = true;
    try {
      diverged = (await this.git(["merge-base", "--is-ancestor", installed, target])).code !== 0;
    } catch {
      diverged = true;
    }
    // `target --not installed`: the commits the update takes. A stamp naming a commit this clone
    // does not have lists nothing, and says so through `diverged`.
    const log = await this.gitOut(["log", `--format=${COMMIT_LOG_FORMAT}`, target, "--not", installed, "--"], "log");
    return { ok: true, value: { installed, target, commits: log.ok ? parseCommitLog(log.value) : [], diverged } };
  }

  /**
   * Build and stage `target` — which must still be `main`'s tip, the commit the user was shown — with
   * the clone's builder (`--update-to`, which checks that under its lock). Installs nothing. A build
   * of `target` already staged (an install that did not go through) is not built again. The build's
   * output goes straight to `logFile`, not through this process, so a quit mid-build leaves it to
   * finish staging. Answers the commit staged.
   */
  async apply(target: unknown): Promise<Result<string>> {
    if (this.building) return { ok: false, error: "An update is already building." };
    if (!isCommitId(target)) return { ok: false, error: `not a commit id: ${JSON.stringify(target)}` };
    const refused = this.notInstalled();
    if (refused !== undefined) return { ok: false, error: refused };
    const builder = join(this.src, APP_BUILDER_IN_SRC);
    if (!existsSync(builder)) return { ok: false, error: `The clone at ${this.src} has no ${APP_BUILDER_IN_SRC}; run scripts/setup.sh.` };
    if (this.stagedCommit() === target) return { ok: true, value: target };
    this.building = true;
    try {
      const argv = [builder, "--update-to", target];
      this.writeLog(`$ ${argv.join(" ")}\n`);
      let r: RunResult;
      try {
        r = await this.run("/bin/bash", argv, this.buildTimeoutMs, this.logFile);
      } catch (e) {
        return { ok: false, error: `could not run ${builder}: ${e instanceof Error ? e.message : String(e)}` };
      }
      const output = tail(`${r.stdout}\n${r.stderr}`);
      const lockAdvice = `If no build or install of Code Factory is running, remove ${this.lockDir}.`;
      if (r.code === BUILD_EXIT.busy) {
        return { ok: false, error: `Another build or install of Code Factory holds its lock (${this.lockDir}). ${lockAdvice}\n\n${output}` };
      }
      if (r.code !== 0) {
        const forced = r.forced === true ? ` It had to be killed, so it may have left its lock behind: ${lockAdvice}` : "";
        return {
          ok: false,
          error: `The build ${describeEnd(r, this.buildTimeoutMs)}. Nothing was installed (a build only stages).${forced} Its output is in ${this.logFile}.\n\n${output}`,
        };
      }
      const staged = this.stagedCommit();
      if (staged !== target) {
        return { ok: false, error: `The build finished but did not stage ${target.slice(0, 12)}; see ${this.logFile}.` };
      }
      return { ok: true, value: staged };
    } finally {
      this.building = false;
    }
  }

  /**
   * Start the clone's install step detached, for this process (`pid`) on its way out: it waits for
   * `pid` and every copy of the app to exit, swaps the staged copy in, stamps it, and relaunches the
   * app when `relaunch` (the old one, when nothing could be installed; none while a copy runs). Its
   * output goes to `installLogFile`. Nothing staged, nothing started.
   */
  startInstaller(pid: number, relaunch: boolean): Result<string> {
    const staged = this.stagedCommit();
    if (staged === undefined) return { ok: false, error: "Nothing is staged to install." };
    const installer = join(this.src, APP_INSTALLER_IN_SRC);
    if (!existsSync(installer)) return { ok: false, error: `The clone at ${this.src} has no ${APP_INSTALLER_IN_SRC}.` };
    try {
      this.start("/bin/bash", [installer, "--wait-pid", String(pid), ...(relaunch ? ["--relaunch"] : [])], this.installLogFile);
    } catch (e) {
      return { ok: false, error: `could not start ${installer}: ${e instanceof Error ? e.message : String(e)}` };
    }
    return { ok: true, value: staged };
  }

  private writeLog(text: string): void {
    try {
      mkdirSync(dirname(this.logFile), { recursive: true });
      writeFileSync(this.logFile, text);
    } catch {
      // The log is a convenience; the result carries the tail either way.
    }
  }
}


/**
 * The environment programs run with on the real machine: `env` minus Electron's own variables (which
 * would make a child Electron run as Node) and git's repository selection, with
 * `GIT_TERMINAL_PROMPT=0` (as lib.sh's fetches have: a credential prompt on the terminal the app was
 * started from would hang the update rather than fail it), and with `HOME` set to `home` — the
 * account's — so the scripts' defaults (the app home, where the app is installed, pnpm's home) are
 * the ones the app identifies itself by, whatever HOME the app was launched with.
 */
export function machineEnv(home: string, env: Readonly<Record<string, string | undefined>>): Record<string, string> {
  const childEnv: Record<string, string> = {};
  for (const [k, v] of Object.entries(env)) {
    if (v !== undefined && !k.startsWith("ELECTRON_") && !GIT_SELECTION.includes(k)) childEnv[k] = v;
  }
  childEnv["HOME"] = home;
  childEnv["GIT_TERMINAL_PROMPT"] = "0";
  return childEnv;
}

/** Whether any process in group `pgid` exists. */
function groupAlive(pgid: number): boolean {
  try {
    process.kill(-pgid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
}

/** The last `max` bytes of `file`, as text, or "". */
function fileTail(file: string, max = 256 * 1024): string {
  try {
    const size = statSync(file).size;
    const fd = openSync(file, "r");
    try {
      const len = Math.min(size, max);
      const buf = Buffer.alloc(len);
      readSync(fd, buf, 0, len, size - len);
      return buf.toString("utf8");
    } finally {
      closeSync(fd);
    }
  } catch {
    return "";
  }
}

function signalGroup(pgid: number, signal: NodeJS.Signals): void {
  try {
    process.kill(-pgid, signal);
  } catch {
    // Gone already.
  }
}

/**
 * Run programs on the real machine from `home`, in `machineEnv`. Each run is its own process group,
 * and a timeout stops the WHOLE group (SIGTERM, then SIGKILL after `graceMs` if any of it is left,
 * which `forced` reports) — pnpm and electron-builder, not just the bash that started them. With a
 * `logFile` the run's output goes there, not through a pipe into this process.
 */
export function machineRunner(home: string, env: Readonly<Record<string, string | undefined>>, graceMs = KILL_GRACE_MS): RunFile {
  const childEnv = machineEnv(home, env);
  return (file, args, timeoutMs, logFile) =>
    new Promise((resolve) => {
      let settled = false;
      const finish = (r: RunResult): void => {
        if (!settled) {
          settled = true;
          resolve(r);
        }
      };
      let stdout = "";
      let stderr = "";
      const keep = (acc: string, chunk: string): string => {
        const all = acc + chunk;
        return all.length > MAX_OUTPUT ? all.slice(-MAX_OUTPUT) : all;
      };
      let child: ChildProcess;
      let fd: number | undefined;
      try {
        if (logFile !== undefined) {
          mkdirSync(dirname(logFile), { recursive: true });
          fd = openSync(logFile, "a");
        }
        child = spawn(file, [...args], { cwd: home, env: childEnv, detached: true, stdio: ["ignore", fd ?? "pipe", fd ?? "pipe"] });
      } catch (e) {
        finish({ code: null, stdout: "", stderr: e instanceof Error ? e.message : String(e) });
        return;
      } finally {
        if (fd !== undefined) closeSync(fd);
      }
      // With a log file the output is read back from it, once the run is over.
      const out = (): string => (logFile === undefined ? stdout : fileTail(logFile));
      child.stdout?.setEncoding("utf8").on("data", (c: string) => (stdout = keep(stdout, c)));
      child.stderr?.setEncoding("utf8").on("data", (c: string) => (stderr = keep(stderr, c)));
      let timedOut = false;
      let forced = false;
      const timer = setTimeout(() => {
        timedOut = true;
        const pid = child.pid;
        if (pid !== undefined) signalGroup(pid, "SIGTERM");
        setTimeout(() => {
          if (pid !== undefined && groupAlive(pid)) {
            signalGroup(pid, "SIGKILL");
            forced = true;
          }
          // Not waiting on 'close': a member of the group holding the pipes would hold it open.
          setTimeout(() => {
            child.stdout?.destroy();
            child.stderr?.destroy();
            finish({ code: null, stdout: out(), stderr, timedOut: true, forced });
          }, 200);
        }, graceMs);
      }, timeoutMs);
      child.on("error", (e) => {
        clearTimeout(timer);
        finish({ code: null, stdout: out(), stderr: stderr === "" ? e.message : stderr });
      });
      child.on("close", (code, signal) => {
        clearTimeout(timer);
        if (timedOut) {
          // The leader went on SIGTERM; the grace timer may still find the rest of the group.
          if (!forced && child.pid !== undefined && groupAlive(child.pid)) return;
          finish({ code: null, stdout: out(), stderr, timedOut: true, forced });
        } else finish({ code: code ?? null, stdout: out(), stderr, ...(signal === null ? {} : { signal }) });
      });
    });
}

/** Start programs detached on the real machine, in `machineEnv`, their output appended to a log. */
export function machineStarter(home: string, env: Readonly<Record<string, string | undefined>>): StartDetached {
  const childEnv = machineEnv(home, env);
  return (file, args, logFile) => {
    mkdirSync(dirname(logFile), { recursive: true });
    const fd = openSync(logFile, "a");
    try {
      const child = spawn(file, [...args], { cwd: home, env: childEnv, detached: true, stdio: ["ignore", fd, fd] });
      child.unref();
    } finally {
      closeSync(fd);
    }
  };
}
