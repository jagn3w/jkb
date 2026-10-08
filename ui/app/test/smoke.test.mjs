//! The built app, launched in Electron and driven by Playwright: one smoke per tab (D53.2).
//
// Needs the Electron binary, which `pnpm install` downloads from GitHub releases — unreachable
// from the agent sandbox, where installs run with ELECTRON_SKIP_BINARY_DOWNLOAD=1. Without it,
// or without a display on Linux, these tests SKIP and say why; they are never reported green.
// They run against `out/`, so `pnpm run build` comes first (as in check.sh and CI).
//
// The app is pointed at a daemon that is not there (a closed loopback port, and a HOME of its
// own), so it never talks to the operator's real `jkb serve` and the status reads unreachable.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as http from "node:http";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after, before } from "node:test";

import { _electron as electron } from "playwright-core";

const here = import.meta.dirname;
const appDir = path.join(here, "..");
const mainEntry = path.join(appDir, "out", "main", "index.js");

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

const binary = electronBinary();

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

const skip = skipReason();
if (skip !== undefined) console.log(`# Electron smoke skipped: ${skip}`);

let app;
let page;
let home;

before(async () => {
  if (skip !== undefined) return;
  assert.ok(fs.existsSync(mainEntry), `${mainEntry} is missing: run pnpm run build first`);
  home = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-smoke-"));
  // A port nothing listens on: bind one, note it, close it.
  const probe = http.createServer();
  await new Promise((r) => probe.listen(0, "127.0.0.1", r));
  const port = probe.address().port;
  await new Promise((r) => probe.close(r));

  app = await electron.launch({
    executablePath: binary,
    args: [appDir],
    env: { ...process.env, HOME: home, JKB_REMOTE: `127.0.0.1:${port}`, JKB_REMOTE_TOKEN_FILE: "", ELECTRON_RENDERER_URL: "" },
  });
  page = await app.firstWindow();
  await page.waitForSelector('[role="tablist"]');
});

after(async () => {
  await app?.close();
  if (home !== undefined) fs.rmSync(home, { recursive: true, force: true });
});

test("the renderer has no Node, only the bridge", { skip }, async () => {
  const globals = await page.evaluate(() => ({
    require: typeof globalThis.require,
    process: typeof globalThis.process,
    bridge: Object.keys(window.jkb).sort(),
  }));
  assert.deepEqual(globals, { require: "undefined", process: "undefined", bridge: ["hello", "info", "op"] });
});

test("the daemon's status is shown, and an absent daemon reads unreachable", { skip }, async () => {
  const status = page.locator(".daemon-status");
  await status.and(page.locator('[data-state="failed"]')).waitFor();
  assert.match(await status.innerText(), /unreachable/);
});

for (const [id, label] of [
  ["design", "Design"],
  ["workflows", "Workflows"],
  ["container", "Container"],
  ["sessions", "Sessions"],
]) {
  test(`the ${label} tab opens its pane`, { skip }, async () => {
    await page.getByRole("tab", { name: label }).click();
    const pane = page.locator(`#pane-${id}`);
    await pane.waitFor({ state: "visible" });
    assert.equal(await page.getByRole("tab", { name: label }).getAttribute("aria-selected"), "true");
    assert.equal(await pane.getByRole("heading", { level: 1 }).innerText(), label);
    assert.equal(await page.locator('[role="tabpanel"]:visible').count(), 1, "only one pane is shown");
  });
}
