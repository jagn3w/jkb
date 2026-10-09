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

import { execFile } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

import {
  APP_BUILDER_IN_SRC,
  APP_HOME_IN_HOME,
  APP_LOG_IN_HOME,
  APP_SRC_IN_HOME,
  APP_STAMP_IN_HOME,
  COMMIT_LOG_FORMAT,
  UPDATE_REF,
  UPDATE_REFSPEC,
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

type Result<T> = ContainerResult<T>;

/** The last `n` non-empty lines of `text`, plain: what a failure is reported by. */
function tail(text: string, n = 12): string {
  return plain(text)
    .split("\n")
    .filter((l) => l.trim() !== "")
    .slice(-n)
    .join("\n");
}

export class AppUpdater {
  /** Whether an update is building now: the menu refuses a second. */
  private building = false;

  /** `home` is the account's home; `run` runs a program to completion, without a shell. */
  constructor(
    private readonly home: string,
    private readonly run: RunFile,
  ) {}

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
    if (!existsSync(join(this.src, ".git"))) {
      return { ok: false, error: `No clean clone at ${this.src}. scripts/setup.sh creates it, from a checkout you have reviewed.` };
    }
    const fetched = await this.gitOut(["fetch", "--quiet", "--no-tags", "origin", UPDATE_REFSPEC], "fetch origin main", FETCH_TIMEOUT_MS);
    if (!fetched.ok) return fetched;
    const tip = await this.gitOut(["rev-parse", "--verify", "--quiet", `${UPDATE_REF}^{commit}`], "rev-parse origin/main");
    if (!tip.ok) return tip;
    const target = tip.value.trim();
    if (!isCommitId(target)) return { ok: false, error: `origin/main is not a commit: ${JSON.stringify(target)}` };
    const installed = this.installed();
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
   * shown — and run the clone's builder. The build's output goes to `logFile`.
   */
  async apply(target: unknown): Promise<Result<string>> {
    if (this.building) return { ok: false, error: "An update is already building." };
    if (!isCommitId(target)) return { ok: false, error: `not a commit id: ${JSON.stringify(target)}` };
    this.building = true;
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
        r = await this.run("/bin/bash", [builder, "--app-home", this.appHome], BUILD_TIMEOUT_MS);
      } catch (e) {
        return { ok: false, error: `could not run ${builder}: ${e instanceof Error ? e.message : String(e)}` };
      }
      this.writeLog(`$ ${builder} --app-home ${this.appHome}\n${r.stdout}${r.stderr === "" ? "" : `\n--- stderr ---\n${r.stderr}`}\n`);
      if (r.code !== 0) {
        const why = r.code === null ? `timed out after ${BUILD_TIMEOUT_MS / 60_000} minutes` : `exit ${r.code}`;
        return { ok: false, error: `The build failed (${why}); the installed app is unchanged. Its output is in ${this.logFile}.\n\n${tail(`${r.stdout}\n${r.stderr}`)}` };
      }
      if (this.installed() !== target) {
        return { ok: false, error: `The build finished but did not record ${target.slice(0, 12)} as installed; see ${this.logFile}.` };
      }
      return { ok: true, value: target };
    } finally {
      this.building = false;
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


/**
 * Run programs on the real machine from `home`, with `env` minus Electron's own variables (which
 * would make a child Electron run as Node) and minus git's repository selection, and with `HOME`
 * set to `home` — the account's — so the builder's defaults (where the app is installed, pnpm's
 * home) agree with the `--app-home` it is given, whatever HOME the app was launched with.
 */
export function machineRunner(home: string, env: Readonly<Record<string, string | undefined>>): RunFile {
  const childEnv: Record<string, string> = {};
  for (const [k, v] of Object.entries(env)) {
    if (v !== undefined && !k.startsWith("ELECTRON_") && !GIT_SELECTION.includes(k)) childEnv[k] = v;
  }
  childEnv["HOME"] = home;
  return (file, args, timeoutMs) =>
    new Promise((resolve) => {
      execFile(
        file,
        [...args],
        { cwd: home, env: childEnv, timeout: timeoutMs, maxBuffer: 64 * 1024 * 1024, encoding: "utf8" },
        (error, stdout, stderr) => {
          const code = error === null ? 0 : typeof error.code === "number" ? error.code : null;
          resolve({ code, stdout, stderr: error !== null && code === null && stderr === "" ? error.message : stderr });
        },
      );
    });
}
