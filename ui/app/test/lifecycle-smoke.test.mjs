//! The Lifecycle pane in the built app, in Electron: for both machines of every preset strategy, no
//! two states overlap, no edge label covers a state or another label, and no marker covers either —
//! measured on the rendered SVG with the real fonts, where lifecycle-layout.test.mjs checks the
//! layout's own boxes against an estimate of them.
//
// The app talks to a stand-in `jkb serve` on loopback (the daemon-client.test.mjs pattern) that
// answers `hello`, `workflow.agents`, `plan.strategies`, and `workflow.graph` from
// fixtures/workflow-graphs.json, which `graph_fixture_matches_presets` in crates/jkb-api holds to
// the tables; anything else is refused. The skip, and where a skip is a failure, are
// electron-harness.mjs's.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as http from "node:http";
import * as os from "node:os";
import * as path from "node:path";
import test, { after, before } from "node:test";

import { _electron as electron } from "playwright-core";

import { appDir, binary, mainEntry, quit, smokeSkip } from "./electron-harness.mjs";
import { overlaps } from "./lifecycle-boxes.mjs";

const skip = smokeSkip();

const presets = JSON.parse(fs.readFileSync(path.join(import.meta.dirname, "fixtures", "workflow-graphs.json"), "utf8"));
const DEFAULT = "design-reviewed";
const TOKEN = "lifecycle-smoke-token";
/** Each strategy whose machines the stand-in answered, in order. */
const graphsServed = [];

/** What the stand-in answers each op, as `jkb_api::Response` serializes it; `undefined`: refused. */
function answer(req) {
  switch (req.op) {
    case "workflow.agents":
      return { result: "workflow_agents", agents: [], roles: [] };
    case "plan.strategies":
      return {
        result: "strategies",
        default: DEFAULT,
        strategies: Object.entries(presets).map(([name, g]) => ({ name, preset: true, describe: g.describe, defined_at: null })),
      };
    case "workflow.graph": {
      const g = presets[req.strategy ?? DEFAULT];
      if (g !== undefined) graphsServed.push(req.strategy ?? DEFAULT);
      return g === undefined ? undefined : { result: "workflow_graph", graph: g };
    }
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
        return reply(200, { protocol: 1, schema_version: 1, supported_schema: 1, ops: ["workflow.agents", "plan.strategies", "workflow.graph"] });
      }
      let parsed;
      try {
        parsed = JSON.parse(body);
      } catch {
        parsed = {};
      }
      const out = req.method === "POST" && req.url?.startsWith("/v1/op") ? answer(parsed) : undefined;
      return out === undefined ? reply(400, { code: "invalid", message: `the stand-in does not serve ${parsed.op}` }) : reply(200, out);
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  home = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-lifecycle-smoke-"));
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
  await page.getByRole("tab", { name: "Workflows", exact: true }).click();
  await page.locator("#pane-workflows .lifecycle-pane svg").waitFor();
  await page.evaluate(() => document.fonts.ready);
});

after(async () => {
  if (app !== undefined) await quit(app);
  if (server !== undefined) await new Promise((r) => server.close(r));
  if (home !== undefined) fs.rmSync(home, { recursive: true, force: true });
});

/** The rendered boxes (`getBBox`, in the SVG's own units) of what the pane drew, once it drew `states`. */
async function drawn(which, states) {
  const pane = page.locator("#pane-workflows .lifecycle-pane");
  await pane.getByRole("radio", { name: which === "workflow" ? /^Phases/ : "Task status" }).click();
  await page.waitForFunction(
    ({ which, states }) => {
      const svg = document.querySelector(`#pane-workflows .lifecycle-pane svg[aria-label="The ${which} state machine"]`);
      const names = [...(svg?.querySelectorAll(".machine-state") ?? [])].map((g) => g.getAttribute("data-state"));
      return names.length === states.length && names.every((n, i) => n === states[i]);
    },
    { which, states },
  );
  return page.evaluate(() => {
    const svg = document.querySelector("#pane-workflows .lifecycle-pane svg");
    const box = (el, id) => {
      const b = el.getBBox();
      return { id, x: b.x, y: b.y, w: b.width, h: b.height };
    };
    return {
      nodes: [...svg.querySelectorAll(".machine-state")].map((g) => box(g.querySelector("rect"), g.getAttribute("data-state"))),
      labels: [...svg.querySelectorAll(".machine-edge .graph-edge-label")].map((t, i) => box(t, `${i}:${t.textContent}`)),
      markers: [...svg.querySelectorAll(".machine-marker rect")].map((r, i) => box(r, `marker ${i}`)),
    };
  });
}

/**
 * Pick `strategy`, and wait for its machines to be drawn: the pane is replaced while they load, so
 * a check made before the stand-in answered could read the previous strategy's picture.
 */
async function show(strategy) {
  const picker = page.locator("#pane-workflows").getByRole("combobox", { name: "Strategy" });
  await picker.and(page.locator(":enabled")).waitFor();
  if ((await picker.inputValue()) === strategy && graphsServed.length > 0) return;
  const asked = graphsServed.length;
  await picker.selectOption(strategy);
  const deadline = Date.now() + 15_000;
  while (!graphsServed.slice(asked).includes(strategy)) {
    assert.ok(Date.now() < deadline, `the app never asked for ${strategy}'s machines: ${JSON.stringify(graphsServed)}`);
    await new Promise((r) => setTimeout(r, 50));
  }
  await page.locator("#pane-workflows .lifecycle-pane svg").waitFor();
}

for (const [strategy, graph] of Object.entries(presets)) {
  for (const which of ["workflow", "lifecycle"]) {
    test(`${strategy} ${which}: no states, labels or markers overlap as rendered`, { skip }, async () => {
      await show(strategy);
      const b = await drawn(which, graph[which].states.map((s) => s.name));
      assert.ok(b.labels.length > 0, "edges are labelled");
      assert.ok(b.labels.every((l) => l.w > 0 && l.h > 0), `every label has text: ${JSON.stringify(b.labels)}`);
      assert.deepEqual(overlaps(b.nodes, b.nodes), [], "no two node boxes intersect");
      assert.deepEqual(overlaps(b.labels, b.nodes), [], "no label box intersects a node box");
      assert.deepEqual(overlaps(b.labels, b.labels), [], "no label box intersects another label");
      assert.deepEqual(overlaps(b.markers, b.nodes), [], "no marker intersects a node");
      assert.deepEqual(overlaps(b.markers, b.labels), [], "no marker intersects a label");
    });
  }
}
