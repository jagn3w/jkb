//! The Requests pane of the Executions tab in the built app, in Electron (the Executions design): a
//! pending run request renders with its argv escaped, its reason, and Run and Decline; Run stays
//! disabled while main cannot say what would run (here: the directory is not on this machine); Decline
//! asks for a note before it sends anything; the history lists a finished request.
//
// The app talks to a stand-in `jkb serve` on loopback (lifecycle-smoke.test.mjs's pattern) answering
// `hello`, `exec.list`, `exec.decline` and the feed's `mq.*` on `exec/requests`; anything else is
// refused. The skip, and where a skip is a failure, are electron-harness.mjs's.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as http from "node:http";
import * as os from "node:os";
import * as path from "node:path";
import test, { after, before } from "node:test";

import { _electron as electron } from "playwright-core";

import { appDir, binary, mainEntry, quit, smokeSkip } from "./electron-harness.mjs";

const skip = smokeSkip();
const TOKEN = "requests-smoke-token";
const PIN = { head: "a".repeat(40), changes: "b".repeat(64) };
const base = {
  topic: "exec/1", target: "host", dir_side: "host", session: "sess-1", role: "implementer", agent_type: "implementer",
  timeout_ms: 900000, created_at: Date.now(), expires_at: Date.now() + 3_600_000, pin: PIN,
};
const PENDING = { ...base, id: "exec-1", state: "pending", dir: "/nowhere/repo", argv: ["rm", "-rf‮", "x "], reason: "the sandbox cannot reach docker" };
const FINISHED = { ...base, id: "exec-2", topic: "exec/2", state: "finished", dir: "/nowhere/repo", argv: ["make"], reason: "build", outcome: "exited", exit_status: 0 };
const declines = [];

function answer(req) {
  switch (req.op) {
    case "exec.list": {
      const all = [PENDING, FINISHED];
      return { result: "exec_requests", requests: req.state === undefined ? all : all.filter((r) => r.state === req.state) };
    }
    case "exec.decline":
      declines.push(req);
      return { result: "exec_request", request: { ...PENDING, state: "declined", note: req.note } };
    case "mq.group_create":
      return req.topic === "exec/requests" ? { result: "created", created: true } : undefined;
    case "mq.poll":
      return req.topic === "exec/requests" ? { result: "messages", messages: [] } : undefined;
    case "mq.group_delete":
      return { result: "group_deleted", deleted: true };
    default:
      return undefined;
  }
}

let server;
let app;
let page;
let home;

before(async () => {
  if (skip !== undefined) return;
  assert.ok(fs.existsSync(mainEntry), `${mainEntry} is missing: run pnpm run build first`);
  server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const reply = (status, value) => {
        res.writeHead(status, { "content-type": "application/json" });
        res.end(JSON.stringify(value));
      };
      if (req.headers.authorization !== `Bearer ${TOKEN}`) return reply(401, { code: "unauthorized", message: "bad token" });
      if (req.method === "GET" && req.url === "/v1/hello") {
        return reply(200, { protocol: 1, schema_version: 1, supported_schema: 1, ops: ["exec.list", "exec.decline"] });
      }
      let parsed;
      try {
        parsed = JSON.parse(body);
      } catch {
        parsed = {};
      }
      const out = req.method === "POST" && req.url?.startsWith("/v1/op") ? answer(parsed) : undefined;
      // A long-poll waits a little, as the daemon's does, rather than spinning the feed.
      const send = () => (out === undefined ? reply(400, { code: "invalid", message: `the stand-in does not serve ${parsed.op}` }) : reply(200, out));
      if (parsed.op === "mq.poll") setTimeout(send, 500);
      else send();
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  home = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-requests-smoke-"));
  const tokenDir = path.join(home, ".jkb", "daemon", String(port));
  fs.mkdirSync(tokenDir, { recursive: true });
  fs.writeFileSync(path.join(tokenDir, "token"), TOKEN, { mode: 0o600 });
  app = await electron.launch({
    executablePath: binary,
    args: [appDir],
    env: { ...process.env, HOME: home, JKB_REMOTE: `127.0.0.1:${port}`, JKB_REMOTE_TOKEN_FILE: "", ELECTRON_RENDERER_URL: "", JKB_APP_FROM_CHECKOUT: "1" },
  });
  page = await app.firstWindow();
  await page.waitForSelector('[role="tablist"]');
  await page.getByRole("tab", { name: "Executions", exact: true }).click();
});

after(async () => {
  if (app !== undefined) await quit(app);
  if (server !== undefined) await new Promise((r) => server.close(r));
  if (home !== undefined) fs.rmSync(home, { recursive: true, force: true });
});

test("a pending request renders escaped, with Run and Decline; Decline needs a note", { skip }, async () => {
  const card = page.locator("#pane-executions").getByRole("article", { name: "Run request exec-1" });
  await card.waitFor();
  const argv = card.getByRole("list", { name: "argv" }).locator("li");
  assert.deepEqual(await argv.locator("code").allTextContents(), ["rm", "-rf\\u{202e}", "x␠"]);
  assert.match(await card.textContent(), /the sandbox cannot reach docker/);
  assert.equal(await card.locator('.target-badge[data-target="host"]').count(), 1);
  const run = card.getByRole("button", { name: "Run", exact: true });
  await run.waitFor();
  // The directory is not on this machine: main says so, and Run stays disabled.
  await page.waitForFunction(() => !document.body.textContent?.includes("Pinning the tree"));
  assert.equal(await run.isDisabled(), true);
  await card.getByRole("button", { name: "Decline…" }).click();
  const submit = card.getByRole("button", { name: "Decline", exact: true });
  assert.equal(await submit.isDisabled(), true, "no note, no decline");
  await card.getByRole("textbox").fill("ask for make check");
  await submit.click();
  for (let i = 0; i < 50 && declines.length === 0; i++) await new Promise((r) => setTimeout(r, 100));
  assert.deepEqual(declines.map((d) => [d.id, d.note]), [["exec-1", "ask for make check"]]);
});

test("the history lists a finished request with its outcome", { skip }, async () => {
  const row = page.locator("#pane-executions .history-row").filter({ hasText: "exec-2" });
  await row.waitFor();
  assert.match(await row.textContent(), /exited 0/);
});
