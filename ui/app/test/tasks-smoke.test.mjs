//! Code Factory shows every task, in the built app, in Electron: the Design sidebar counts each
//! repo's unplanned tasks (`task.unplanned`) and lists them in an Unplanned fold; one opens in the
//! Task pane, whose *Move to step* › Open work › Inbox sends exactly one `task.move` and re-reads the
//! count; and in an open design's Tasks tab a review finding is drawn directly under the task it was
//! found in (`found_in`), marked and at no subtask depth, with the origin's pane listing it under its
//! round.
//
// The app talks to a stand-in `jkb serve` on loopback (lifecycle-smoke.test.mjs's pattern) answering
// `hello`, `design.list`, what the Design tab sends to open a design (`design.cat`, `design.state`,
// `design.threads`, `design.prompts`, `design.plans`, `workflow.strategies`, `role.whoami` and the
// design feed's `mq.*`), `task.show`, `task.unplanned` and `task.move`; anything else is refused.
// Every op it answers is checked against `Request::OPS` in crates/jkb-api/src/lib.rs, so a renamed
// or invented op fails here rather than passing against a fiction. The skip, and where a skip is a
// failure, are electron-harness.mjs's.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as http from "node:http";
import * as os from "node:os";
import * as path from "node:path";
import test, { after, before } from "node:test";

import { _electron as electron } from "playwright-core";
import * as Y from "yjs";

import { appDir, binary, mainEntry, quit, smokeSkip } from "./electron-harness.mjs";

const skip = smokeSkip();
const TOKEN = "tasks-smoke-token";

/** The op names `jkb serve` advertises: `Request::OPS`, read from the source. */
function realOps() {
  const lib = fs.readFileSync(path.join(import.meta.dirname, "..", "..", "..", "crates", "jkb-api", "src", "lib.rs"), "utf8");
  const start = lib.indexOf("pub const OPS: &'static [&'static str] = &[");
  assert.ok(start >= 0, "Request::OPS is where this test reads it");
  const body = lib.slice(start, lib.indexOf("];", start));
  return new Set([...body.matchAll(/"([a-z_]+\.[a-z_]+)"/g)].map((m) => m[1]));
}

const DESIGN = { uid: "design:shown", title: "Shown", namespace: "designs/jkb", seq: 1, topic: "design/design:shown" };
const TEXT = "Show every task.";
const VERSION = "v1";
/** The design's text as a Yjs update, the way `design.state` sends it. */
const STATE = (() => {
  const doc = new Y.Doc();
  doc.getText("body").insert(0, TEXT);
  return Buffer.from(Y.encodeStateAsUpdate(doc)).toString("base64");
})();

const planTask = (uid, title, over = {}) => ({
  uid,
  title,
  status: "open",
  priority: null,
  depth: 0,
  claimed_by: null,
  strategy: "default:design-reviewed",
  found_in: [],
  ...over,
});
const ORIGIN = planTask("task:origin", "Build the sidebar");
const OTHER = planTask("task:other", "Write the docs");
const FINDING = planTask("task:finding", "breaks on empty repos", { priority: 1, found_in: [{ uid: ORIGIN.uid, title: ORIGIN.title }] });
const ROUND = "repos/jkb/codereviews/r1";
const PLANS = {
  uid: DESIGN.uid,
  hidden: 0,
  tasks: [],
  plans: [
    {
      uid: "plan:p",
      title: "First cut",
      design: DESIGN.uid,
      archived: false,
      // The finding listed last, as `design.plans` lists a step's tasks by position: the Tasks tab
      // draws it under its origin.
      steps: [{ uid: "step:one", text: "Show it", spans: [], tasks: [ORIGIN, OTHER, FINDING] }],
    },
  ],
};

let unplanned = [
  planTask("task:u1", "Loose one"),
  planTask("task:u2", "Loose two"),
];
const moves = [];

function show(uid) {
  const all = [ORIGIN, OTHER, FINDING, ...unplanned];
  const t = all.find((x) => x.uid === uid);
  if (t === undefined) return undefined;
  const task = {
    item: { id: 1, uid, kind: "task", status: t.status, priority: t.priority, due: null, namespace: "tasks/repos/jkb/inbox", content: t.title, tags: [] },
    transitions: [],
    subtasks: [],
  };
  if (uid === ORIGIN.uid) {
    task.review = { reviewed: "abc", namespaces: [ROUND] };
    task.findings = [
      { uid: FINDING.uid, title: FINDING.title, status: "open", priority: 1, round: ROUND, must_fix: true, step: "step:one", step_title: "Shown › Show it", design: DESIGN.uid },
    ];
  }
  if (uid === FINDING.uid) task.found_in = [{ uid: ORIGIN.uid, title: ORIGIN.title }];
  return { result: "task", task };
}

/** What the stand-in answers each op, as `jkb_api::Response` serializes it; `undefined`: refused. */
function answer(req) {
  // Only what SERVED names, which the first test holds to `Request::OPS`.
  if (!SERVED.includes(req.op)) return undefined;
  switch (req.op) {
    case "design.list":
      return { result: "designs", designs: req.retired ? [] : [DESIGN] };
    case "design.cat":
      return req.uid === DESIGN.uid
        ? { result: "design_text", design: { ...DESIGN, text: TEXT, version: VERSION, spans: [] } }
        : undefined;
    case "design.state":
      return req.uid === DESIGN.uid ? { result: "design_update", update: STATE, version: VERSION, seq: 1 } : undefined;
    case "design.threads":
      return { result: "design_thread_list", threads: [], version: VERSION };
    case "design.prompts":
      return { result: "design_prompts", prompts: [] };
    case "design.plans":
      return req.uid === DESIGN.uid ? { result: "design_plans", list: PLANS } : undefined;
    case "workflow.strategies":
      return { result: "strategies", default: "design-reviewed", strategies: [{ name: "design-reviewed", preset: true, describe: "d", defined_at: null }] };
    case "role.whoami":
      return { result: "who_am_i", whoami: { label: "operator", roles: ["operator"], task: null } };
    case "task.show":
      return show(req.uid);
    case "task.unplanned":
      return {
        result: "unplanned",
        count: unplanned.length,
        repos: unplanned.length === 0 ? [] : [{ repo: "jkb", count: unplanned.length, tasks: unplanned.map((task) => ({ task, namespace: "tasks/repos/jkb/inbox" })) }],
      };
    case "task.move":
      moves.push(req);
      unplanned = unplanned.filter((t) => t.uid !== req.uid);
      return { result: "task_moved", moved: { moved: true, under: req.under } };
    case "mq.group_create":
      return { result: "created", created: true };
    case "mq.poll":
      return { result: "messages", messages: [] };
    case "mq.group_delete":
      return { result: "group_deleted", deleted: true };
    default:
      return undefined;
  }
}

/** Every op the stand-in answers. */
const SERVED = [
  "design.list",
  "design.cat",
  "design.state",
  "design.threads",
  "design.prompts",
  "design.plans",
  "workflow.strategies",
  "role.whoami",
  "task.show",
  "task.unplanned",
  "task.move",
  "mq.group_create",
  "mq.poll",
  "mq.group_delete",
];

test("every op the stand-in answers is one jkb serve has (Request::OPS)", () => {
  const ops = realOps();
  assert.ok(ops.has("task.unplanned") && ops.has("task.move"), "the list was read");
  assert.deepEqual(
    SERVED.filter((op) => !ops.has(op)),
    [],
    "the stand-in serves an op jkb does not",
  );
});

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
        return reply(200, { protocol: 1, schema_version: 1, supported_schema: 1, ops: SERVED });
      }
      let parsed;
      try {
        parsed = JSON.parse(body);
      } catch {
        parsed = {};
      }
      const out = req.method === "POST" && req.url?.startsWith("/v1/op") ? answer(parsed) : undefined;
      const send = () => (out === undefined ? reply(400, { code: "invalid", message: `the stand-in does not serve ${parsed.op}` }) : reply(200, out));
      // A long-poll waits a little, as the daemon's does, rather than spinning the feed.
      if (parsed.op === "mq.poll") setTimeout(send, 500);
      else send();
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  home = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-tasks-smoke-"));
  const tokenDir = path.join(home, ".jkb", "daemon", String(port));
  fs.mkdirSync(tokenDir, { recursive: true });
  fs.writeFileSync(path.join(tokenDir, "token"), TOKEN, { mode: 0o600 });

  app = await electron.launch({
    executablePath: binary,
    args: [appDir],
    // JKB_APP_FROM_CHECKOUT: this is the checkout's build, which refuses to start without it (D53.3).
    env: { ...process.env, HOME: home, JKB_REMOTE: `127.0.0.1:${port}`, JKB_REMOTE_TOKEN_FILE: "", ELECTRON_RENDERER_URL: "", JKB_APP_FROM_CHECKOUT: "1" },
  });
  page = await app.firstWindow();
  await page.waitForSelector('[role="tablist"]');
  await page.locator("#tab-design").click();
  await page.locator(".design-sidebar .design-tree").waitFor();
});

after(async () => {
  if (app !== undefined) await quit(app);
  if (server !== undefined) await new Promise((r) => server.close(r));
  if (home !== undefined) fs.rmSync(home, { recursive: true, force: true });
});

const sidebar = () => page.locator(".design-sidebar");

test("the repo row counts its unplanned tasks, and the fold lists them", { skip }, async () => {
  await sidebar().locator(".tree-repo .tree-unplanned", { hasText: "unplanned 2" }).waitFor();
  await sidebar().getByRole("button", { name: "Unfold unplanned tasks of jkb" }).click();
  const fold = sidebar().getByRole("group", { name: "Unplanned tasks of jkb" });
  await fold.getByRole("button", { name: "Loose one" }).waitFor();
  await fold.getByRole("button", { name: "Loose two" }).waitFor();
});

test("an unplanned task opens in the Task pane", { skip }, async () => {
  await sidebar().getByRole("group", { name: "Unplanned tasks of jkb" }).getByRole("button", { name: "Loose one" }).click();
  await page.locator(".unplanned-view .task-pane h2", { hasText: "Loose one" }).waitFor();
});

test("Move to step › Open work › Inbox sends exactly one task.move, and the count is read again", { skip }, async () => {
  const pane = page.locator(".unplanned-view .task-pane");
  await pane.getByRole("combobox", { name: "Move to" }).selectOption("inbox");
  await pane.getByRole("button", { name: "Move", exact: true }).click();
  await sidebar().locator(".tree-repo .tree-unplanned", { hasText: "unplanned 1" }).waitFor();
  assert.deepEqual(moves, [{ op: "task.move", uid: "task:u1", under: "step:inbox@jkb" }]);
});

test("a finding sits under its origin, marked and at no subtask depth, and the origin's pane lists it", { skip }, async () => {
  await sidebar().getByRole("button", { name: DESIGN.title, exact: true }).click();
  await page.locator("#view-tasks").click();
  const rows = page.locator("#view-pane-tasks .plan-tasks > li");
  await rows.nth(2).waitFor();
  const drawn = await rows.evaluateAll((lis) =>
    lis.map((li) => {
      const b = li.querySelector("button.plan-task");
      return {
        title: li.querySelector(".task-title")?.textContent,
        finding: li.classList.contains("task-finding"),
        of: b?.getAttribute("data-finding-of") ?? null,
        marker: li.querySelector(".finding-marker") !== null,
        pad: b?.style.paddingLeft,
      };
    }),
  );
  assert.deepEqual(
    drawn.map((r) => [r.title, r.finding, r.of, r.marker]),
    [
      [ORIGIN.title, false, null, false],
      [FINDING.title, true, ORIGIN.uid, true],
      [OTHER.title, false, null, false],
    ],
  );
  assert.equal(drawn[1].pad, drawn[0].pad, "the finding is at its own depth (0), not a subtask's");

  await rows.nth(0).locator("button.plan-task").click();
  const findings = page.locator("#view-pane-tasks .task-pane .task-findings");
  await findings.waitFor();
  assert.equal(await findings.locator(".finding-round h4").first().textContent(), ROUND);
  assert.match(await findings.locator("li").first().textContent(), /must-fix.*breaks on empty repos/);
});
