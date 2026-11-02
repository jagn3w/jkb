//! What every Electron smoke shares (D53.2): finding the binary, the skip and where a skip is a
//! failure, and quitting the app within a bound. smoke.test.mjs and lifecycle-smoke.test.mjs use it;
//! smoke-required.test.mjs pins the skip for each of them.
//
// The smokes need the Electron binary, which `pnpm install` downloads from GitHub releases —
// unreachable from the agent sandbox, where installs run with ELECTRON_SKIP_BINARY_DOWNLOAD=1.
// Without it, or without a display on Linux, they SKIP and say why. Where a smoke must run — CI sets
// JKB_REQUIRE_ELECTRON_SMOKE=1 — a reason to skip is a failure instead, so a runner that lost its
// binary or its display cannot pass green without having run any of it.
// They run against `out/`, so `pnpm run build` comes first (as in check.sh and CI).

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as path from "node:path";
import test from "node:test";

export const appDir = path.join(import.meta.dirname, "..");
export const mainEntry = path.join(appDir, "out", "main", "index.js");

/**
 * The Electron binary the install script fetched, found the way the `electron` package finds it
 * (`path.txt`, under `dist/` or ELECTRON_OVERRIDE_DIST_PATH) — but without `require("electron")`,
 * which, when the binary is missing, tries to download it there and then.
 */
function electronBinary() {
  const pkg = path.dirname(createRequire(import.meta.url).resolve("electron/package.json"));
  const pathFile = path.join(pkg, "path.txt");
  if (!fs.existsSync(pathFile)) return undefined;
  const dist = process.env.ELECTRON_OVERRIDE_DIST_PATH || path.join(pkg, "dist");
  const binary = path.join(dist, fs.readFileSync(pathFile, "utf8"));
  return fs.existsSync(binary) ? binary : undefined;
}

export const binary = electronBinary();

/** Why the smoke cannot run here, or `undefined` when it can. */
function skipReason() {
  if (binary === undefined) {
    return "no Electron binary; install without ELECTRON_SKIP_BINARY_DOWNLOAD to run the Electron smoke";
  }
  if (process.platform === "linux" && !process.env.DISPLAY && !process.env.WAYLAND_DISPLAY) {
    return "no display on Linux; run under xvfb-run to run the Electron smoke";
  }
  return undefined;
}

/** Set where the smoke must run (CI): a reason to skip is then a failure. */
export const REQUIRE_VAR = "JKB_REQUIRE_ELECTRON_SMOKE";

/**
 * Why this file's smoke cannot run, or `undefined` when it can: pass it as each test's `skip`.
 * Where the smoke is required, a reason is registered as a failing test; otherwise it is said.
 */
export function smokeSkip() {
  const skip = skipReason();
  if (skip !== undefined) {
    if (process.env[REQUIRE_VAR] === "1") {
      test(`the Electron smoke runs (${REQUIRE_VAR}=1)`, () => {
        assert.fail(`${REQUIRE_VAR}=1, but the Electron smoke cannot run here: ${skip}`);
      });
    } else {
      console.log(`# Electron smoke skipped: ${skip}`);
    }
  }
  return skip;
}

// How long the app may take to quit once asked. Its will-quit holds the first quit for at most
// LEAVE_WAIT_MS (1.5 s) to leave claude/notify, then quits again, so this is generous. Bounded
// because Playwright's close() waits for the process to exit with no limit of its own: an app that
// never re-quits would otherwise hold the test file open until CI's job timeout.
const QUIT_WAIT_MS = 20_000;

/** Close a launched app, failing (after killing it) when it does not quit within the bound. */
export async function quit(app) {
  const proc = app.process();
  let timer;
  const quitted = await Promise.race([
    app.close().then(() => true),
    new Promise((resolve) => {
      timer = setTimeout(() => resolve(false), QUIT_WAIT_MS);
    }),
  ]);
  clearTimeout(timer);
  if (!quitted) {
    // Say what was still running before killing it: whether Electron itself is alive (the quit
    // hold never re-quit) or has exited while something it started holds the pipes Playwright
    // waits on, and which processes those are.
    const exited = proc.exitCode !== null || proc.signalCode !== null;
    const tree = spawnSync("ps", ["-eo", "pid,ppid,pgid,stat,args", "--forest"], { encoding: "utf8" }).stdout;
    // Playwright starts Electron as its own process group: kill the group, so a child holding the
    // pipes goes too and the test file can exit, then Electron itself if the group kill failed.
    try {
      process.kill(-proc.pid, "SIGKILL");
    } catch {
      proc.kill("SIGKILL");
    }
    assert.fail(
      `the app did not quit within ${QUIT_WAIT_MS} ms of being asked to: killed it. ` +
        `Electron pid ${proc.pid} ${exited ? `had exited (code ${proc.exitCode}, signal ${proc.signalCode})` : "was still running"}.\n${tree}`,
    );
  }
}
