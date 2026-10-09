//! *jkb ▸ Update from main…* (D53.3) in main, without a window: fetch `main` into the app's clean
//! clone, say what an update would take, and build exactly that commit with the clone's own builder.
//
// Plain Node (no Electron), so it is tested against a real git repository standing in for the
// clone. Running a program is injected for the same reason.
//
// What runs is never a checkout's. The clone lives under the account's home, where no agent writes
// (the dev container does not mount it), and is moved only to `origin/main` — code that passed review
// and landed. The builder is that clone's `scripts/build-app.sh`, which refuses any tree that is not
// the clone at `origin/main`, installs with the frozen lockfile, swaps the packaged app in, and stamps
// the commit it installed. Main then relaunches.
//
// Only the installed copy updates: the builder installs at `installedExecutable`'s place and main
// relaunches what is running, so a copy anywhere else would be "updated" without changing. And one
// install at a time: the clone, the build and the swap are shared with setup.sh's install, so both
// hold the same lock (`APP_LOCK_IN_HOME`, scripts/lib.sh's `app_lock`).

import { spawn, type ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, realpathSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

import {
  APP_BUILDER_IN_SRC,
  APP_HOME_IN_HOME,
  APP_LOCK_IN_HOME,
  APP_LOCK_TOKEN_VAR,
  APP_LOG_IN_HOME,
  APP_SRC_IN_HOME,
  APP_STAMP_IN_HOME,
  BUILT_COMMIT_IN_OUT,
  COMMIT_LOG_FORMAT,
  UPDATE_REF,
  UPDATE_REFSPEC,
  installedExecutable,
  isCommitId,
  parseCommitLog,
  parseInstalledStamp,
  type ContainerResult,
  type UpdatePlan,
} from "@jkb/core";

import { GIT_SELECTION } from "../shared/gitEnv";
import { plain, type RunFile, type RunResult } from "./container";

/** How long fetching `main` may take. */
export const FETCH_TIMEOUT_MS = 5 * 60_000;

/** How long a local git query may take. */
export const GIT_TIMEOUT_MS = 30_000;

/** How long the build may take: a frozen install, two type-checks, a bundle and a package. */
export const BUILD_TIMEOUT_MS = 30 * 60_000;

/** How long a timed-out run's process group gets after SIGTERM, and then after SIGKILL, to be gone. */
export const KILL_GRACE_MS = 5_000;

/** The most output a run keeps, from each stream: the tail, which is what a failure is read from. */
const MAX_OUTPUT = 16 * 1024 * 1024;

type Result<T> = ContainerResult<T>;

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

/** The commit `build-app.sh` built into the app's `out/` directory, or `undefined`. */
export function builtCommit(outDir: string): string | undefined {
  try {
    const text = readFileSync(join(outDir, BUILT_COMMIT_IN_OUT), "utf8").trim();
    return isCommitId(text) ? text : undefined;
  } catch {
    return undefined;
  }
}

/** Whether a process `pid` exists (EPERM: it does, as someone else's). */
function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
}

function readTrimmed(path: string): string | undefined {
  try {
    return readFileSync(path, "utf8").trim();
  } catch {
    return undefined;
  }
}

/**
 * Take the install lock at `dir` — lib.sh's `app_lock`, the same way: `mkdir`, then `pid` and
 * `token` inside. A lock whose holder is gone is moved aside under a name of its own and checked to
 * be that holder's before it is removed. Answers the token, or why it is busy.
 */
export function takeAppLock(dir: string): Result<string> {
  const busy = (why: string): Result<string> => ({ ok: false, error: `Another install or update is running (${why}). If none is, remove ${dir}.` });
  try {
    mkdirSync(dirname(dir), { recursive: true });
    try {
      mkdirSync(dir);
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code !== "EEXIST") throw e;
      const pid = readTrimmed(join(dir, "pid")) ?? "";
      if (!/^[0-9]+$/.test(pid)) return busy(`${dir} names no holder yet`);
      if (alive(Number(pid))) return busy(`pid ${pid} holds ${dir}`);
      const aside = `${dir}.stale.${process.pid}.${randomBytes(4).toString("hex")}`;
      try {
        renameSync(dir, aside);
      } catch {
        return busy(`another run is breaking ${dir}`);
      }
      if (readTrimmed(join(aside, "pid")) !== pid) return busy(`another run took ${dir}`);
      rmSync(aside, { recursive: true, force: true });
      try {
        mkdirSync(dir);
      } catch {
        return busy(`another run took ${dir}`);
      }
    }
    const token = `${process.pid}.${randomBytes(12).toString("hex")}`;
    writeFileSync(join(dir, "token"), `${token}\n`);
    writeFileSync(join(dir, "pid"), `${process.pid}\n`);
    return { ok: true, value: token };
  } catch (e) {
    return { ok: false, error: `could not take the install lock ${dir}: ${e instanceof Error ? e.message : String(e)}` };
  }
}

/** Release the lock at `dir` if it is still `token`'s. */
export function releaseAppLock(dir: string, token: string): void {
  if (readTrimmed(join(dir, "token")) !== token) return;
  try {
    rmSync(dir, { recursive: true, force: true });
  } catch {
    // Left for the next run, which finds its holder gone and breaks it.
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
   * `home` is the account's home; `run` runs a program to completion, without a shell; `running` is
   * this process — an update runs only when it is the installed copy.
   */
  constructor(
    private readonly home: string,
    private readonly run: RunFile,
    private readonly running: RunningCopy,
    options: UpdaterOptions = {},
  ) {
    this.buildTimeoutMs = options.buildTimeoutMs ?? BUILD_TIMEOUT_MS;
  }

  get lockDir(): string {
    return join(this.home, APP_LOCK_IN_HOME);
  }

  /**
   * Why this copy may not update itself, or `undefined`. The builder installs at the installed
   * copy's place and main relaunches what is running: from anywhere else, an update would rebuild
   * that place and relaunch this copy unchanged.
   */
  private notInstalled(): string | undefined {
    const installed = installedExecutable(this.running.platform, this.home);
    if (isInstalledCopy(this.running.exe, this.running.platform, this.home)) return undefined;
    return (
      `This copy (${this.running.exe}) is not the installed one (${installed}). Update from main installs at ` +
      `${installed} and relaunches what is running, so it cannot update this copy. Run the installed copy, ` +
      `or update this one by other means.`
    );
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

  get busy(): boolean {
    return this.building;
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
      const why = r.code === null ? `timed out after ${Math.round(timeoutMs / 1000)}s` : `exit ${r.code}`;
      return { ok: false, error: `git ${what} failed (${why})${r.stderr.trim() === "" ? "" : `:\n${tail(r.stderr)}`}` };
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

  /** Fetch `main` into the clone and say what an update would take. Builds and moves nothing. */
  async plan(): Promise<Result<UpdatePlan>> {
    const refused = this.notInstalled();
    if (refused !== undefined) return { ok: false, error: refused };
    if (!existsSync(join(this.src, ".git"))) {
      return { ok: false, error: `No clean clone at ${this.src}. scripts/setup.sh creates it, from a checkout you have reviewed.` };
    }
    const fetched = await this.gitOut(["fetch", "--quiet", "--no-tags", "origin", UPDATE_REFSPEC], "fetch origin main", FETCH_TIMEOUT_MS);
    if (!fetched.ok) return fetched;
    const tip = await this.gitOut(["rev-parse", "--verify", "--quiet", `${UPDATE_REF}^{commit}`], "rev-parse origin/main");
    if (!tip.ok) return tip;
    const target = tip.value.trim();
    if (!isCommitId(target)) return { ok: false, error: `origin/main is not a commit: ${JSON.stringify(target)}` };
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
   * Move the clone to `target` — which must still be `origin/main`'s tip, the commit the user was
   * shown — and run the clone's builder, under the install lock. The build's output goes to `logFile`.
   */
  async apply(target: unknown): Promise<Result<string>> {
    if (this.building) return { ok: false, error: "An update is already building." };
    if (!isCommitId(target)) return { ok: false, error: `not a commit id: ${JSON.stringify(target)}` };
    const refused = this.notInstalled();
    if (refused !== undefined) return { ok: false, error: refused };
    const lock = takeAppLock(this.lockDir);
    if (!lock.ok) return lock;
    this.building = true;
    // Kept when a timed-out build's processes could not be confirmed gone: they may still swap and
    // stamp, and a second install must not start under them. A later run breaks it once this
    // process (its holder) has exited.
    let keepLock = false;
    try {
      const tip = await this.gitOut(["rev-parse", "--verify", "--quiet", `${UPDATE_REF}^{commit}`], "rev-parse origin/main");
      if (!tip.ok) return tip;
      if (tip.value.trim() !== target) {
        return { ok: false, error: `origin/main moved to ${tip.value.trim().slice(0, 12)} since it was shown; check again.` };
      }
      const moved = await this.gitOut(["checkout", "--quiet", "--detach", "--force", target], "checkout");
      if (!moved.ok) return moved;
      // Untracked files go; ignored ones (node_modules, the last build's out/) stay, so a rebuild
      // does not start from nothing. Nothing but this app writes in the clone.
      const cleaned = await this.gitOut(["clean", "-ffdq"], "clean");
      if (!cleaned.ok) return cleaned;

      const builder = join(this.src, APP_BUILDER_IN_SRC);
      if (!existsSync(builder)) return { ok: false, error: `main at ${target.slice(0, 12)} has no ${APP_BUILDER_IN_SRC}.` };
      let r: RunResult;
      try {
        r = await this.run("/bin/bash", [builder, "--app-home", this.appHome], this.buildTimeoutMs, { [APP_LOCK_TOKEN_VAR]: lock.value });
      } catch (e) {
        return { ok: false, error: `could not run ${builder}: ${e instanceof Error ? e.message : String(e)}` };
      }
      this.writeLog(`$ ${builder} --app-home ${this.appHome}\n${r.stdout}${r.stderr === "" ? "" : `\n--- stderr ---\n${r.stderr}`}\n`);
      const output = tail(`${r.stdout}\n${r.stderr}`);
      if (r.code === null) {
        const after = `timed out after ${Math.round(this.buildTimeoutMs / 1000)}s`;
        if (r.survivors === true) {
          keepLock = true;
          return {
            ok: false,
            error:
              `The build ${after}, and some of its processes could not be stopped: the installed app may still ` +
              `change. The install lock (${this.lockDir}) stays held until this app quits. Its output is in ${this.logFile}.\n\n${output}`,
          };
        }
        return {
          ok: false,
          error:
            `The build ${after} and was stopped, every process it started with it. It was not recorded as installed; if it ` +
            `was stopped while swapping, the app it replaced is in ${join(this.appHome, "previous")}. Its output is in ${this.logFile}.\n\n${output}`,
        };
      }
      if (r.code !== 0) {
        return { ok: false, error: `The build failed (exit ${r.code}); the installed app is unchanged. Its output is in ${this.logFile}.\n\n${output}` };
      }
      if (this.installed() !== target) {
        return { ok: false, error: `The build finished but did not record ${target.slice(0, 12)} as installed; see ${this.logFile}.` };
      }
      return { ok: true, value: target };
    } finally {
      this.building = false;
      if (!keepLock) releaseAppLock(this.lockDir, lock.value);
    }
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


/** Whether any process in group `pgid` exists. */
function groupAlive(pgid: number): boolean {
  try {
    process.kill(-pgid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
}

function signalGroup(pgid: number, signal: NodeJS.Signals): void {
  try {
    process.kill(-pgid, signal);
  } catch {
    // Gone already.
  }
}

/** Whether group `pgid` is gone within `ms`, polling (the event loop reaps the leader meanwhile). */
async function groupGone(pgid: number, ms: number): Promise<boolean> {
  const end = Date.now() + ms;
  while (groupAlive(pgid)) {
    if (Date.now() >= end) return false;
    await new Promise((r) => setTimeout(r, 50));
  }
  return true;
}

/** SIGTERM the group, then SIGKILL it; whether it is confirmed gone. */
async function stopGroup(pgid: number | undefined, graceMs: number): Promise<boolean> {
  if (pgid === undefined) return true;
  signalGroup(pgid, "SIGTERM");
  if (await groupGone(pgid, graceMs)) return true;
  signalGroup(pgid, "SIGKILL");
  return groupGone(pgid, graceMs);
}

/**
 * Run programs on the real machine from `home`, with `env` minus Electron's own variables (which
 * would make a child Electron run as Node) and minus git's repository selection, with
 * `GIT_TERMINAL_PROMPT=0` (as lib.sh's fetch of the same refspec has: a credential prompt on the
 * terminal the app was started from would hang the update rather than fail it), and with `HOME` set
 * to `home` — the account's — so the builder's defaults (where the app is installed, pnpm's home)
 * agree with the `--app-home` it is given, whatever HOME the app was launched with.
 *
 * Each run is its own process group, and a timeout stops the WHOLE group — pnpm, electron-builder,
 * the swap — not just the program started: killing only that would leave its children running on to
 * swap and stamp after the update had said it failed. `survivors` says the group could not be
 * confirmed gone.
 */
export function machineRunner(home: string, env: Readonly<Record<string, string | undefined>>, graceMs = KILL_GRACE_MS): RunFile {
  const childEnv: Record<string, string> = {};
  for (const [k, v] of Object.entries(env)) {
    if (v !== undefined && !k.startsWith("ELECTRON_") && !GIT_SELECTION.includes(k)) childEnv[k] = v;
  }
  childEnv["HOME"] = home;
  childEnv["GIT_TERMINAL_PROMPT"] = "0";
  return (file, args, timeoutMs, extraEnv = {}) =>
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
      try {
        child = spawn(file, [...args], { cwd: home, env: { ...childEnv, ...extraEnv }, detached: true, stdio: ["ignore", "pipe", "pipe"] });
      } catch (e) {
        finish({ code: null, stdout: "", stderr: e instanceof Error ? e.message : String(e) });
        return;
      }
      child.stdout?.setEncoding("utf8").on("data", (c: string) => (stdout = keep(stdout, c)));
      child.stderr?.setEncoding("utf8").on("data", (c: string) => (stderr = keep(stderr, c)));
      let timedOut = false;
      const timer = setTimeout(() => {
        timedOut = true;
        void stopGroup(child.pid, graceMs).then((gone) => {
          child.stdout?.destroy();
          child.stderr?.destroy();
          finish({ code: null, stdout, stderr: stderr === "" ? `timed out after ${timeoutMs}ms` : stderr, survivors: !gone });
        });
      }, timeoutMs);
      child.on("error", (e) => {
        clearTimeout(timer);
        finish({ code: null, stdout, stderr: stderr === "" ? e.message : stderr });
      });
      child.on("close", (code) => {
        if (timedOut) return;
        clearTimeout(timer);
        finish({ code: code ?? null, stdout, stderr });
      });
    });
}
