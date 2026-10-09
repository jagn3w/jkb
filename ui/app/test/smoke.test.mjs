//! The built app, launched in Electron and driven by Playwright: one smoke per tab (D53.2).
//
// Needs the Electron binary, which `pnpm install` downloads from GitHub releases — unreachable
// from the agent sandbox, where installs run with ELECTRON_SKIP_BINARY_DOWNLOAD=1. Without it,
// or without a display on Linux, these tests SKIP and say why. Where the smoke must run — CI sets
// JKB_REQUIRE_ELECTRON_SMOKE=1 — a reason to skip is a failure instead, so a runner that lost its
// binary or its display cannot pass green without having run any of it.
// They run against `out/`, so `pnpm run build` comes first (as in check.sh and CI).
//
// The app is pointed at a daemon that is not there (a closed loopback port, and a HOME of its
// own holding a token for that port), so it never talks to the operator's real `jkb serve`, and
// its request really goes out and is refused: the status reads unreachable for the transport,
// not for a missing token.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
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

/** Set where the smoke must run (CI): a reason to skip is then a failure. */
const REQUIRE_VAR = "JKB_REQUIRE_ELECTRON_SMOKE";

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

let app;
let page;
let home;
let port;

before(async () => {
  if (skip !== undefined) return;
  assert.ok(fs.existsSync(mainEntry), `${mainEntry} is missing: run pnpm run build first`);
  home = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-smoke-"));
  // A port nothing listens on: bind one, note it, close it.
  const probe = http.createServer();
  await new Promise((r) => probe.listen(0, "127.0.0.1", r));
  port = probe.address().port;
  await new Promise((r) => probe.close(r));
  // A token where jkb serve on that port would write one, so the hello gets as far as the socket.
  const tokenDir = path.join(home, ".jkb", "daemon", String(port));
  fs.mkdirSync(tokenDir, { recursive: true });
  fs.writeFileSync(path.join(tokenDir, "token"), "smoke-token", { mode: 0o600 });

  app = await electron.launch({
    executablePath: binary,
    args: [appDir],
    // JKB_APP_FROM_CHECKOUT: this is the checkout's build, which refuses to start without it (D53.3).
    env: { ...process.env, HOME: home, JKB_REMOTE: `127.0.0.1:${port}`, JKB_REMOTE_TOKEN_FILE: "", ELECTRON_RENDERER_URL: "", JKB_APP_FROM_CHECKOUT: "1" },
  });
  page = await app.firstWindow();
  await page.waitForSelector('[role="tablist"]');
});

after(async () => {
  await app?.close();
  if (home !== undefined) fs.rmSync(home, { recursive: true, force: true });
});

// The installed copy is what runs (D53.3): the checkout's build, started without the opt-in, says
// why and exits before it opens a window.
test("run from a checkout without JKB_APP_FROM_CHECKOUT=1, the app refuses to start", { skip }, () => {
  const env = { ...process.env, HOME: home, ELECTRON_RENDERER_URL: "" };
  delete env.JKB_APP_FROM_CHECKOUT;
  const r = spawnSync(binary, [appDir], { env, encoding: "utf8", timeout: 30_000 });
  assert.equal(r.status, 1, `exit ${r.status}, signal ${r.signal}; stderr: ${r.stderr}`);
  assert.match(r.stderr, /running from a checkout/);
  assert.match(r.stderr, /JKB_APP_FROM_CHECKOUT=1/);
});

// D53.1: an inherited ELECTRON_RENDERER_URL must not make a remote page the one main trusts with
// the bridge. Which URLs pass is pinned by dev-renderer.test.mjs; this is the refusal, in main.
test("pointed at a renderer off loopback, the app refuses to start", { skip }, () => {
  const env = { ...process.env, HOME: home, JKB_APP_FROM_CHECKOUT: "1", JKB_APP_DEV_RENDERER: "1", ELECTRON_RENDERER_URL: "http://example.net" };
  const r = spawnSync(binary, [appDir], { env, encoding: "utf8", timeout: 30_000 });
  assert.equal(r.status, 1, `exit ${r.status}, signal ${r.signal}; stderr: ${r.stderr}`);
  assert.match(r.stderr, /must be an http:\/\/ URL on loopback/);
});

// What the window was made with, from main: the renderer-side checks below cannot see `sandbox`
// (a renderer with `sandbox: false` but context isolation still shows no require or process).
test("the window's renderer is sandboxed and isolated, with no Node integration", { skip }, async () => {
  const prefs = await app.evaluate(({ BrowserWindow }) => {
    const contents = BrowserWindow.getAllWindows()[0]?.webContents;
    if (contents === undefined) return "no window";
    // Undocumented but long-standing (Electron's own lib reads it); if it goes, this fails loudly.
    if (typeof contents.getLastWebPreferences !== "function") return "no getLastWebPreferences";
    const p = contents.getLastWebPreferences();
    return { sandbox: p.sandbox, contextIsolation: p.contextIsolation, nodeIntegration: p.nodeIntegration, webviewTag: p.webviewTag };
  });
  assert.deepEqual(prefs, { sandbox: true, contextIsolation: true, nodeIntegration: false, webviewTag: false });
});

test("the renderer has no Node, only the bridge", { skip }, async () => {
  const globals = await page.evaluate(() => ({
    require: typeof globalThis.require,
    process: typeof globalThis.process,
    bridge: Object.keys(window.jkb).sort(),
  }));
  assert.deepEqual(globals, { require: "undefined", process: "undefined", bridge: ["design", "hello", "info", "op", "terminal"] });
});

test("the daemon's status is shown, and an absent daemon reads unreachable", { skip }, async () => {
  const status = page.locator(".daemon-status");
  await status.and(page.locator('[data-state="failed"]')).waitFor();
  assert.match(await status.innerText(), /unreachable/);
  // The request went out (the token was read) and nothing answered on the port.
  assert.match(await status.getAttribute("title"), new RegExp(`cannot reach jkb serve at http://127\\.0\\.0\\.1:${port}`));
});

for (const [id, label] of [
  ["design", "Design"],
  ["workflows", "Workflows"],
  ["container", "Container"],
  ["sessions", "Sessions"],
]) {
  test(`the ${label} tab opens its pane`, { skip }, async () => {
    const tab = page.getByRole("tab", { name: label, exact: true });
    await tab.click();
    const pane = page.locator(`#pane-${id}`);
    await pane.waitFor({ state: "visible" });
    assert.equal(await tab.getAttribute("aria-selected"), "true");
    assert.equal(await pane.getByRole("heading", { level: 1 }).innerText(), label);
    assert.equal(await page.locator('.panes > [role="tabpanel"]:visible').count(), 1, "only one pane is shown");
  });
}

// The Design tab (D53.4): with no daemon there are no designs to list, and the pane says why rather
// than showing an empty editor. Editing, live sync and Discuss against a real jkb are pinned by
// design.test.mjs and yjs-wire.test.mjs.
test("the Design tab says why it has no designs when the daemon is unreachable", { skip }, async () => {
  await page.getByRole("tab", { name: "Design", exact: true }).click();
  const pane = page.locator("#pane-design");
  await pane.locator(".design-empty", { hasText: "Cannot list designs" }).waitFor();
  assert.equal(await pane.getByRole("combobox", { name: "Repo" }).isDisabled(), true);
});

// The Workflows tab (D53.7): with no daemon there are no templates to draw, and the pane says why.
// The graph, the side panel's save and the lifecycle against a real jkb are pinned by the CLI and
// API tests over the same ops; Contribute's script by workflows.test.mjs.
test("the Workflows tab says why it has no agents when the daemon is unreachable", { skip }, async () => {
  await page.getByRole("tab", { name: "Workflows", exact: true }).click();
  const pane = page.locator("#pane-workflows");
  await pane.locator(".plan-hint", { hasText: "Cannot list agent templates" }).waitFor();
  assert.equal(await pane.getByRole("combobox", { name: "Workflow" }).isDisabled(), true);
});

// The Sessions tab (D53.9): with no daemon there is no registry to list and no notification to dot,
// and the pane says why. The join, the dot's rule, the claude/notify feed, the git-file reader and
// re-attach are pinned by sessions.test.mjs (app and core); the ops by the jkb-api tests.
test("the Sessions tab says why it lists nothing when the daemon is unreachable", { skip }, async () => {
  await page.getByRole("tab", { name: "Sessions", exact: true }).click();
  const pane = page.locator("#pane-sessions");
  await pane.locator(".plan-hint", { hasText: "Cannot list sessions" }).waitFor();
  await pane.locator(".design-notice", { hasText: "claude/notify" }).first().waitFor();
  assert.equal(await page.locator(".tab .needs-dot").count(), 0, "no dot without a record of one");
});

// The Container tab (D53.8): a button per kit run.sh flag, and an answer from the kit -- on a CI
// runner there is none, so it says how to install one. The kit is found under the ACCOUNT's home,
// not the smoke's HOME, so on a machine with a kit this reads that kit's (read-only) --status. What
// main runs and refuses is pinned by container.test.mjs; run.sh's side by
// scripts/tests/container-status.test.sh.
test("the Container tab offers the kit's buttons and says what the kit answered", { skip }, async () => {
  await page.getByRole("tab", { name: "Container", exact: true }).click();
  const pane = page.locator("#pane-container");
  for (const label of ["Build", "Verify", "Install extensions", "Stop", "Remove"]) {
    await pane.getByRole("button", { name: label, exact: true }).waitFor();
  }
  await pane.locator(".container-error, .container-findings").first().waitFor();
});

// The integrated terminal (D53.10). A new terminal is a container terminal, labelled so; the
// toggle moves it to the host, where it is a real shell. The container side is not exercised here
// (no dev container in CI): what it runs is pinned by terminal.test.mjs.
test("the terminal opens in the container by default, and the toggle runs it on the host", { skip }, async () => {
  page.on("dialog", (dialog) => void dialog.accept());
  await page.getByRole("button", { name: "New terminal" }).click();
  const tab = page.locator(".terminal-tab").first();
  await tab.waitFor();
  assert.equal(await tab.locator(".target-badge").innerText(), "container");
  assert.equal(await page.locator(".drawer-toggle").getAttribute("aria-expanded"), "true");

  await page.getByRole("group", { name: "Where this terminal runs" }).getByRole("button", { name: "Host" }).click();
  await tab.locator('.target-badge[data-target="host"]').waitFor();
  assert.equal(await tab.locator(".target-badge").innerText(), "host");

  const screen = page.locator("#terminal-drawer-body .terminal-panel:not([hidden]) .xterm");
  await screen.click();
  await page.keyboard.type("echo jkb-$((40 + 2))");
  await page.keyboard.press("Enter");
  await page.locator(".xterm-rows", { hasText: "jkb-42" }).waitFor({ timeout: 15_000 });

  await page.keyboard.press("Control+Backquote");
  assert.equal(await page.locator(".drawer-toggle").getAttribute("aria-expanded"), "false", "Ctrl+` folds it");
  await page.keyboard.press("Control+Backquote");
  assert.equal(await page.locator(".drawer-toggle").getAttribute("aria-expanded"), "true", "and opens it");

  await tab.getByRole("button", { name: /^Close / }).click();
  assert.equal(await page.locator(".terminal-tab").count(), 0);
});
