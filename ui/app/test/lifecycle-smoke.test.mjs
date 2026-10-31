//! The Lifecycle pane in the built app, in Electron: for both machines of every preset strategy, no
//! two states overlap, no edge label covers a state or another label, and no marker covers either —
//! measured on the rendered SVG with the real fonts, where lifecycle-layout.test.mjs checks the
//! layout's own boxes against an estimate of them. Then the picture as buttons: a state, an edge or a
//! marker clicked (or picked from the keyboard) shows its rows in the side panel, and there is no
//! transitions table.
//
// The app talks to a stand-in `jkb serve` on loopback (the daemon-client.test.mjs pattern) that
// answers `hello`, `workflow.agents`, `plan.strategies`, and `workflow.graph` from
// fixtures/workflow-graphs.json, which `graph_fixture_matches_presets` in crates/jkb-api holds to
// the tables, and `workflow.roles` from fixtures/workflow-roles.json (`roles_fixture_matches_presets`);
// anything else is refused. The skip, and where a skip is a failure, are
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
const rolesByPreset = JSON.parse(fs.readFileSync(path.join(import.meta.dirname, "fixtures", "workflow-roles.json"), "utf8"));
const DEFAULT = "design-reviewed";
const TOKEN = "lifecycle-smoke-token";
/** The one agent template the stand-in serves: what a phase's panel links to. */
const IMPLEMENTER = {
  name: "swarm-implementer",
  version: 1,
  source: "packaged",
  workflow: "task-swarm",
  role: "implementer",
  role_ops: ["read"],
  fragment: false,
  describe: "Builds one piece of work.",
  template: "Build {{what}}.",
  placeholders: ["what"],
  permissions: { isolation: "worktree", model: null, writes: "code" },
  hands_off_to: [],
  based_on: null,
  defined_at: null,
  packaged_version: 1,
  overrides_packaged: false,
  behind_packaged: false,
  packaged_base: 1,
  matches_packaged: false,
};
/** Each strategy whose machines the stand-in answered, in order. */
const graphsServed = [];
/** Each strategy whose roles the stand-in answered, in order. */
const rolesServed = [];

/** What the stand-in answers each op, as `jkb_api::Response` serializes it; `undefined`: refused. */
function answer(req) {
  switch (req.op) {
    case "workflow.agents":
      return { result: "workflow_agents", agents: [IMPLEMENTER], roles: Object.keys(presets[DEFAULT].role_ops) };
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
    case "workflow.roles": {
      const v = rolesByPreset[req.strategy ?? DEFAULT];
      if (v !== undefined) rolesServed.push(req.strategy ?? DEFAULT);
      return v === undefined ? undefined : { result: "workflow_roles", roles: v };
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
        return reply(200, { protocol: 1, schema_version: 1, supported_schema: 1, ops: ["workflow.agents", "plan.strategies", "workflow.graph", "workflow.roles"] });
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

// ---- the picture as buttons, and the side panel ------------------------------------------------

const lifecycle = () => page.locator("#pane-workflows .lifecycle-pane");
const side = () => page.locator("#pane-workflows .workflows-side");
const wf = presets[DEFAULT].workflow;
const row = (from, event) => wf.transitions.find((t) => t.from === from && t.event === event);

/** The default strategy's phases, drawn. */
async function phases() {
  await show(DEFAULT);
  await drawn("workflow", wf.states.map((s) => s.name));
}

// The legend's second half: jkb's own lines on what the drawn strategy changes from the default.
test("the legend says the default is the default, and lists what another strategy changes", { skip }, async () => {
  const legend = page.locator("#pane-workflows").getByRole("region", { name: "Legend", exact: true });
  await show(DEFAULT);
  assert.equal(presets[DEFAULT].default, DEFAULT, "the fixture names the default");
  await legend.locator('[data-changes="default"]').waitFor();
  assert.equal(await legend.locator('[data-changes="default"]').innerText(), `${DEFAULT} is the default strategy.`);
  await show("autonomous");
  const changes = legend.locator('[data-changes="changes"]');
  await changes.waitFor();
  assert.equal(await changes.locator("p").innerText(), `What autonomous changes from the default (${DEFAULT}):`);
  const lines = presets.autonomous.changes_from_default;
  assert.ok(lines.length > 0, "the fixture's autonomous changes something");
  assert.deepEqual(await changes.locator("li").allInnerTexts(), lines);
});

test("there is no transitions table under the graph", { skip }, async () => {
  await phases();
  assert.equal(await lifecycle().locator("table").count(), 0, "no table in the Lifecycle pane");
  assert.equal(await page.locator("#pane-workflows .machine-table").count(), 0);
  // Every state, drawn edge and marker is a button instead.
  const buttons = lifecycle().locator("svg [role=button]");
  const marked = (await lifecycle().locator("svg .machine-marker").count()) + (await lifecycle().locator("svg .machine-edge").count());
  assert.equal(await buttons.count(), wf.states.length + marked);
});

test("clicking a state shows its next step and its undrawn override", { skip }, async () => {
  await phases();
  const implement = wf.states.find((s) => s.name === "implement");
  await lifecycle().getByRole("button", { name: "state implement", exact: true }).click();
  const panel = side().locator(".machine-panel");
  await panel.waitFor();
  assert.equal(await panel.getAttribute("aria-label"), "State implement");
  assert.equal(await lifecycle().locator('.machine-state[data-state="implement"]').getAttribute("data-selected"), "true");
  const text = await panel.innerText();
  assert.ok(text.includes(implement.next_step), `the next step, with <uid> as sent: ${text}`);
  assert.ok(text.includes(implement.next_role));
  const undrawn = panel.getByRole("region", { name: "Not drawn", exact: true });
  const override = await undrawn.innerText();
  assert.ok(override.includes("implement --override--> (stated)"), override);
  assert.ok(override.includes("the operator names the destination"), override);
  // Who acts there: the role's op classes, as jkb answered them, and its agent template.
  assert.ok(text.includes(presets[DEFAULT].role_ops.implementer.join(", ")), text);
  assert.ok(await panel.getByRole("button", { name: IMPLEMENTER.name, exact: true }).isVisible());
});

test("clicking an edge label shows who fires it", { skip }, async () => {
  await phases();
  const approve = row("design_review", "approve_design");
  await lifecycle().locator('.machine-edge[aria-label="design_review --approve_design--> implement"] .graph-edge-label').click();
  const panel = side().locator(".machine-panel");
  await panel.waitFor();
  assert.equal(await panel.getAttribute("aria-label"), "design_review --approve_design--> implement");
  const facts = await panel.locator(".machine-facts").innerText();
  assert.match(facts, new RegExp(`fired by\\s+${approve.fired_by}`), facts);
  assert.ok(facts.includes(approve.command), facts);
  // A guarded observation's guard is in jkb's words, and it is fired by observing.
  const repeated = row("review", "review_repeated");
  await lifecycle().locator('.machine-edge[aria-label="review --review_repeated--> systemic_review"] .graph-edge-label').click();
  await page.waitForFunction(() => document.querySelector("#pane-workflows .machine-panel")?.getAttribute("aria-label") === "review --review_repeated--> systemic_review");
  const guarded = await side().locator(".machine-facts").innerText();
  assert.ok(guarded.includes(repeated.guard), guarded);
  assert.ok(guarded.includes("jkb workflow observe <uid>"), guarded);
});

test("clicking beside an edge's thin line selects it", { skip }, async () => {
  await phases();
  const label = "implement --submit_work--> review";
  // A point a few units off the line, where the wide hit stroke and nothing else is under it.
  const at = await page.evaluate(() => {
    const hit = document.querySelector('#pane-workflows .machine-edge-hit[data-edge="implement|submit_work"]');
    // elementFromPoint sees only what is on screen; the column above the picture scrolls.
    hit.scrollIntoView({ block: "center", inline: "center" });
    const ctm = hit.getScreenCTM();
    const len = hit.getTotalLength();
    for (let f = 0.1; f <= 0.9; f += 0.05) {
      const p = hit.getPointAtLength(len * f);
      const q = hit.getPointAtLength(len * f + 1);
      const n = Math.hypot(q.x - p.x, q.y - p.y) || 1;
      const off = { x: p.x - ((q.y - p.y) / n) * 4, y: p.y + ((q.x - p.x) / n) * 4 };
      const x = ctm.a * off.x + ctm.c * off.y + ctm.e;
      const y = ctm.b * off.x + ctm.d * off.y + ctm.f;
      if (document.elementFromPoint(x, y) === hit) return { x, y };
    }
    return null;
  });
  assert.ok(at !== null, "some point beside the line hits the edge's wide stroke");
  await page.mouse.click(at.x, at.y);
  await page.waitForFunction((label) => document.querySelector("#pane-workflows .machine-panel")?.getAttribute("aria-label") === label, label);
});

test("clicking the landed marker lists one row per phase", { skip }, async () => {
  await phases();
  const into = wf.transitions.filter((t) => t.reconciled && t.event === "observed_landed" && t.to === "landed");
  assert.ok(into.length >= 2, "the fixture folds observed_landed into a marker");
  await lifecycle().getByRole("button", { name: `observed ×${new Set(into.map((t) => t.from)).size} into landed`, exact: true }).click();
  const panel = side().locator(".machine-panel");
  await panel.waitFor();
  const rows = panel.getByRole("region", { name: "observed_landed", exact: true }).locator(".machine-row");
  assert.equal(await rows.count(), into.length);
  assert.deepEqual(await rows.locator("button").allInnerTexts(), into.map((t) => t.from));
  assert.ok((await panel.innerText()).includes(into[0].guard), "the observation's guard, in jkb's words");
  // A source is a link to its state. Matched whole: `design` is also the start of `design_review`.
  await rows.getByRole("button", { name: into[0].from, exact: true }).click();
  await page.waitForFunction((name) => document.querySelector("#pane-workflows .machine-panel")?.getAttribute("aria-label") === `State ${name}`, into[0].from);
});

test("Escape returns to the agent panel", { skip }, async () => {
  await phases();
  // The keyboard path: focus a state, pick it with Enter, then Escape.
  const review = lifecycle().getByRole("button", { name: "state review", exact: true });
  await review.focus();
  await page.keyboard.press("Enter");
  await side().locator('.machine-panel[aria-label="State review"]').waitFor();
  await page.keyboard.press("Escape");
  await side().locator(".machine-panel").waitFor({ state: "detached" });
  assert.equal(await lifecycle().locator('.machine-state[data-selected="true"]').count(), 0);
  assert.ok(await side().locator(".workflows-pick").isVisible(), "no agent is picked yet: the panel asks for one");
  // Space picks too; switching machines clears a machine selection.
  await review.focus();
  await page.keyboard.press("Space");
  await side().locator('.machine-panel[aria-label="State review"]').waitFor();
  await lifecycle().getByRole("radio", { name: "Task status", exact: true }).click();
  await side().locator(".machine-panel").waitFor({ state: "detached" });
});

test("following a link in the panel keeps the keyboard: focus goes to its heading, then back to the picture", { skip }, async () => {
  await phases();
  await lifecycle().getByRole("button", { name: "state implement", exact: true }).click();
  const panel = side().locator(".machine-panel");
  await panel.waitFor();
  // A Leaves row is a link to its edge; the body it sat in is replaced.
  await panel.getByRole("region", { name: "Leaves", exact: true }).getByRole("button", { name: "implement --submit_work--> review", exact: true }).click();
  await page.waitForFunction(() => document.querySelector("#pane-workflows .machine-panel")?.getAttribute("aria-label") === "implement --submit_work--> review");
  assert.equal(await page.evaluate(() => document.activeElement?.closest(".machine-panel") !== null && document.activeElement?.tagName === "H2"), true, "focus is on the panel's heading");
  // Escape from there closes the panel and focus returns to the part that was selected.
  await page.keyboard.press("Escape");
  await panel.waitFor({ state: "detached" });
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute("data-part")), "edge:implement|submit_work");
  // The same through the panel's close button, then from a marker's source link.
  await page.keyboard.press("Enter");
  await panel.waitFor();
  await panel.getByRole("button", { name: "review", exact: true }).click();
  await page.waitForFunction(() => document.querySelector("#pane-workflows .machine-panel")?.getAttribute("aria-label") === "State review");
  await panel.getByRole("button", { name: "Close", exact: true }).click();
  await panel.waitFor({ state: "detached" });
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute("data-part")), "state:review");
});

test("a phase's agent template opens in the agent panel", { skip }, async () => {
  await phases();
  await lifecycle().getByRole("button", { name: "state implement", exact: true }).click();
  await side().locator(".machine-panel").getByRole("button", { name: IMPLEMENTER.name, exact: true }).click();
  await side().locator(`.agent-panel[aria-label="Agent ${IMPLEMENTER.name}"]`).waitFor();
  assert.equal(await side().locator(".machine-panel").count(), 0);
  // The selected agent's role still marks the states where it acts.
  assert.equal(await lifecycle().locator('.machine-state[data-state="implement"]').getAttribute("data-acts"), "true");
  // An unsaved edit to the template survives a look at a state and back.
  const template = side().locator(".agent-template");
  await template.fill("Build {{what}}, unsaved.");
  await lifecycle().getByRole("button", { name: "state review", exact: true }).click();
  await side().locator('.machine-panel[aria-label="State review"]').waitFor();
  assert.equal(await side().locator(".agent-panel").isVisible(), false, "the agent's panel is hidden, not shown beside it");
  await page.keyboard.press("Escape");
  await side().locator(".machine-panel").waitFor({ state: "detached" });
  assert.equal(await template.inputValue(), "Build {{what}}, unsaved.");
});

test("an agent's panel opens its role in the Roles view, which lists every role and marks where one acts", { skip }, async () => {
  await phases();
  const pane = page.locator("#pane-workflows .agents-pane");
  const views = pane.getByRole("radiogroup", { name: "Agents or roles", exact: true });
  // The agent first, from the phase it acts in; its panel names its role as a link, not a list.
  await lifecycle().getByRole("button", { name: "state implement", exact: true }).click();
  await side().locator(".machine-panel").getByRole("button", { name: IMPLEMENTER.name, exact: true }).click();
  const agentPanel = side().locator(`.agent-panel[aria-label="Agent ${IMPLEMENTER.name}"]`);
  await agentPanel.waitFor();
  assert.equal(await agentPanel.getByText("In jkb, a implementer may", { exact: false }).count(), 0);
  await agentPanel.locator(".agent-role-link").getByRole("button", { name: "implementer", exact: true }).click();

  // The Roles view, its role open beside it.
  assert.equal(await views.getByRole("radio", { name: "Roles", exact: true }).getAttribute("aria-checked"), "true");
  const rolePanel = side().locator('.role-panel[aria-label="Role implementer"]');
  await rolePanel.waitFor();
  assert.equal(await agentPanel.isVisible(), false, "the agent's panel is hidden, not shown beside it");
  const table = pane.locator(".roles-table");
  assert.deepEqual(
    await table.locator("tbody tr").evaluateAll((rows) => rows.map((r) => r.getAttribute("data-role"))),
    rolesByPreset[DEFAULT].roles.map((r) => r.name),
  );
  assert.equal(await table.locator('tr[data-role="implementer"]').getAttribute("data-selected"), "true");
  assert.ok(rolesServed.includes(DEFAULT));

  // A role no agent acts as: done by hand, and the Lifecycle pane marks its phase.
  const designer = table.locator('tr[data-role="designer"]');
  assert.equal(await designer.locator(".role-no-agent").innerText(), "no agent: done by hand");
  await designer.getByRole("button", { name: "designer", exact: true }).click();
  await side().locator('.role-panel[aria-label="Role designer"]').waitFor();
  assert.equal(await lifecycle().locator('.machine-state[data-state="design"]').getAttribute("data-acts"), "true");
  assert.equal(await lifecycle().locator('.machine-state[data-state="implement"]').getAttribute("data-acts"), null);

  // Another strategy re-reads the roles: under coordinated the coordinator approves designs.
  const asked = rolesServed.length;
  await show("coordinated");
  const deadline = Date.now() + 15_000;
  while (!rolesServed.slice(asked).includes("coordinated")) {
    assert.ok(Date.now() < deadline, `the app never asked for coordinated's roles: ${JSON.stringify(rolesServed)}`);
    await new Promise((r) => setTimeout(r, 50));
  }
  const coordinator = pane.locator('.roles-table tr[data-role="coordinator"]');
  await coordinator.locator('.role-toggle[data-on="true"]', { hasText: "approves_design" }).first().waitFor();
  assert.equal(await coordinator.locator('.role-toggle[data-on="false"]', { hasText: "lands (off in this strategy)" }).count(), 1);

  // Escape closes the role, and the agent's panel comes back.
  await side().locator(".role-panel h2").focus();
  await page.keyboard.press("Escape");
  await side().locator(".role-panel").waitFor({ state: "detached" });
  await agentPanel.waitFor();
  await views.getByRole("radio", { name: "Agents", exact: true }).click();
  await show(DEFAULT);
});
