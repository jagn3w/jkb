//! The tab shell without a window: its navigation rules, and what it renders.
//
// The renderer modules are bundled with esbuild (JSX included) and rendered to a string with
// react-dom/server, so the five tabs and their panes are checked with neither Electron nor a DOM.
// Clicking through them in the real app is `smoke.test.mjs`'s job.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-shell-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));

// CommonJS, loaded with `require`: React's server build is CommonJS and requires Node builtins,
// which an ESM bundle cannot do.
const require = createRequire(import.meta.url);

async function load(entry, contents) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({
    ...(contents === undefined
      ? { entryPoints: [entry] }
      : { stdin: { contents, resolveDir: path.dirname(entry), loader: "tsx" } }),
    bundle: true,
    format: "cjs",
    platform: "node",
    jsx: "automatic",
    outfile,
    logLevel: "silent",
  });
  return require(outfile);
}

const src = path.join(here, "..", "src", "renderer", "src");
const { TABS, DEFAULT_TAB, isTabId, tabForKey } = await load(path.join(src, "tabs.ts"));

test("five tabs, in the order the design names them, the newest last", () => {
  assert.deepEqual(
    TABS.map((t) => t.label),
    ["Design", "Workflows", "Container", "Sessions", "Executions"],
  );
  assert.equal(DEFAULT_TAB, "design");
  assert.equal(isTabId("sessions"), true);
  assert.equal(isTabId("settings"), false);
  assert.equal(isTabId(null), false);
});

test("arrows wrap, Home/End jump, and other keys are not tab keys", () => {
  assert.equal(tabForKey("design", "ArrowRight"), "workflows");
  assert.equal(tabForKey("sessions", "ArrowRight"), "executions");
  assert.equal(tabForKey("executions", "ArrowRight"), "design", "wraps forward");
  assert.equal(tabForKey("design", "ArrowLeft"), "executions", "wraps back");
  assert.equal(tabForKey("container", "Home"), "design");
  assert.equal(tabForKey("workflows", "End"), "executions");
  assert.equal(tabForKey("design", "a"), undefined);
  assert.equal(tabForKey("design", "2"), undefined, "a digit alone is typing, not navigation");
});

test("Cmd/Ctrl+1…5 picks a tab by position, and nothing past the last", () => {
  assert.equal(tabForKey("design", "1", { mod: true }), "design");
  assert.equal(tabForKey("design", "4", { mod: true }), "sessions");
  assert.equal(tabForKey("design", "5", { mod: true }), "executions");
  assert.equal(tabForKey("design", "6", { mod: true }), undefined);
  assert.equal(tabForKey("design", "0", { mod: true }), undefined);
  assert.equal(tabForKey("design", "ArrowRight", { mod: true }), undefined);
});

test("the shell renders every tab and pane, with only the active pane shown", async () => {
  const { render } = await load(
    path.join(src, "render-entry.tsx"),
    `import { renderToString } from "react-dom/server";
     import { App } from "./App";
     export const render = () => renderToString(<App />);`,
  );
  // The bridge is only called from effects, which a server render does not run; stub it anyway
  // so a regression that called it during render fails here, not with "jkb is undefined".
  globalThis.window = {
    jkb: {
      hello: () => assert.fail("hello during render"),
      op: () => assert.fail("op during render"),
      info: () => assert.fail("info during render"),
      terminal: {
        open: () => assert.fail("terminal.open during render"),
        onEvent: () => assert.fail("terminal.onEvent during render"),
      },
      container: {
        status: () => assert.fail("container.status during render"),
        spec: () => assert.fail("container.spec during render"),
      },
      notify: {
        subscribe: () => assert.fail("notify.subscribe during render"),
        onEvent: () => assert.fail("notify.onEvent during render"),
      },
      sessions: {
        place: () => assert.fail("sessions.place during render"),
      },
    },
    innerHeight: 800,
    localStorage: { getItem: () => "container", setItem: () => {} },
  };
  try {
    const html = render();
    for (const id of ["design", "workflows", "container", "sessions", "executions"]) {
      assert.match(html, new RegExp(`role="tab"[^>]*id="tab-${id}"`), `tab ${id}`);
      assert.match(html, new RegExp(`id="pane-${id}"`), `pane ${id}`);
    }
    assert.match(html, /id="tab-container"[^>]*aria-selected="true"/, "the remembered tab is selected");
    assert.match(html, /id="pane-container"(?![^>]*hidden)[^>]*>/, "its pane is shown");
    assert.match(html, /id="pane-design"[^>]*hidden/, "the others are hidden");
    assert.match(html, /data-state="checking"/, "the daemon's status starts unknown");
    // The terminal drawer (D53.10): present and folded, with nothing to start a terminal in until
    // main has said where terminals run.
    assert.match(html, /class="drawer-toggle"[^>]*aria-expanded="false"/, "the drawer starts folded");
    assert.match(html, /id="terminal-drawer-body"[^>]*hidden/, "its body is hidden");
    assert.match(html, /aria-label="New terminal"[^>]*disabled/, "no new terminal before the roots are known");
    assert.doesNotMatch(html, /terminal-popover/, "no popover");
    // The Design tab (D53.4): the design tree on the left, empty until the designs are listed —
    // which the bridge is asked for from an effect, never during render.
    assert.match(html, /<nav[^>]*class="design-sidebar"[^>]*aria-label="Designs"/, "the design tree");
    assert.match(html, /<button(?=[^>]*aria-label="Refresh designs")(?=[^>]*disabled)/, "its refresh waits for the listing");
    assert.doesNotMatch(html, /aria-label="Repo"/, "no repo dropdown");
    assert.match(html, /Loading designs…/);
    // The Workflows tab (D53.7): its pickers wait for the agents and strategies, asked for from
    // effects, never during render.
    assert.match(html, /<select(?=[^>]*aria-label="Agent set")(?=[^>]*disabled)/, "the agent-set picker waits for the agents");
    assert.doesNotMatch(html, /aria-label="Workflow"/, "no control is labelled Workflow: that names a task's progress");
    assert.doesNotMatch(html, /<span>Workflow<\/span>/, "nor is the picker's visible label");
    assert.match(html, /<select(?=[^>]*aria-label="Strategy")(?=[^>]*disabled)/, "the strategy picker waits for the strategies");
    assert.match(html, /Loading agents…/);
    // The Container tab (D53.8): a button per kit run.sh flag, and the status asked for from an
    // effect, never during render.
    for (const action of ["build", "verify", "install-extensions", "stop", "remove"]) {
      assert.match(html, new RegExp(`<button[^>]*data-action="${action}"`), `the ${action} button`);
    }
    assert.match(html, /Asking the kit(?:&#x27;|')s run\.sh…/);
    // The Sessions tab (D53.9): the registry and the needs-input feed are asked for from effects,
    // never during render, and there is no dot before a record says there is one.
    assert.match(html, /Loading sessions…/);
    assert.doesNotMatch(html, /needs-dot/, "no dot before the records are read");
    assert.doesNotMatch(html, /id="tab-sessions"[^>]*aria-describedby/);
    // The Executions tab: the timeline's overview is asked for from an effect, never during render.
    assert.match(html, /<section[^>]*class="timeline-pane"[^>]*aria-label="Timeline"/);
    assert.match(html, /role="radiogroup" aria-label="Scope"/);
    assert.match(html, /Loading…/);
  } finally {
    delete globalThis.window;
  }
});

// The Executions tab's timeline, rendered from a fixture of the answers `timeline.*` gives (the
// decoders' half is ui/core's timeline.test.mjs; the joins are crates/jkb-core/src/timeline.rs's).
test("the timeline renders a session's swimlanes, a task's path and a plan's chart from a fixture", async () => {
  const { render } = await load(
    path.join(src, "timeline-entry.tsx"),
    `import { renderToString } from "react-dom/server";
     import { PlanChart, SessionLanes, SessionsOverview, TaskTimelineView } from "./executions/TimelinePane";
     export const render = (f) => ({
       overview: renderToString(<SessionsOverview overview={f.overview} onOpen={() => {}} />),
       session: renderToString(<SessionLanes timeline={f.session} nowMs={f.now} onTask={() => {}} />),
       task: renderToString(<TaskTimelineView view={f.task} />),
       plan: renderToString(<PlanChart plan={f.plan} nowMs={f.now} offsetMin={0} />),
     });`,
  );
  const H = 3_600_000;
  const T = 1_800_000_000_000;
  const ev = (over) => ({ at_ms: T, source: "workflow", id: 1, task: "task:a", event: "submit_work", from: "implement", to: "review", actor: "swarm-implementer@agent-x", role: "implementer", reason: null, detail: null, ...over });
  const summary = { session: "s-1", state: "live", cwd: "/w", started_at_ms: T - H, ended_at_ms: null, claims: 1, last_event_ms: T, stalled: true, stalled_for_ms: 31 * 60_000 };
  const state = (name, initial = false) => ({ name, initial, settled: false, awaits_input: false, next_role: null, next_step: null });
  const edge = (from, event, to) => ({ from, event, to, reconciled: false, guarded: false, planned: false, roles: [], fired_by: "implementer" });
  const graph = {
    strategy: "default:design-reviewed",
    graph: "reviewed-design",
    describe: "d",
    task: "task:a",
    phase: "review",
    status: "needs_review",
    workflow: {
      states: [state("implement", true), state("review"), state("landed")],
      transitions: [
        edge("implement", "submit_work", "review"),
        // A second row between the same two states: never taken, so never inked.
        edge("implement", "submit_systemic", "review"),
        edge("review", "request_changes", "implement"),
        edge("review", "override", null),
        // One observation from two phases: drawn as one marker on `landed`, not two arrows.
        { ...edge("implement", "observed_landed", "landed"), reconciled: true },
        { ...edge("review", "observed_landed", "landed"), reconciled: true },
      ],
    },
    lifecycle: { states: [state("open", true)], transitions: [] },
  };
  const f = {
    now: T + H,
    overview: {
      now_ms: T + H,
      sessions: [summary],
      unattributed: [{ uid: "task:b", title: "B", status: "in_progress", owner: "build-host:42", claimed_at_ms: null }],
    },
    session: {
      summary,
      tasks: [{ uid: "task:a", title: "Task A", status: "needs_review", holds_claim: true, claimed_at_ms: T - H, bound_agents: ["agent-x"] }],
      events: [
        ev({ source: "session", task: null, event: "start", at_ms: T - H, from: null, to: null }),
        ev({}),
        ev({ source: "exec_request", id: 9, at_ms: T + 1 }),
      ],
      next: null,
    },
    task: {
      timeline: {
        uid: "task:a",
        title: "Task A",
        status: "needs_review",
        strategy: null,
        claim: null,
        landing: { state: "none", branch: null, onto: null, pr: null, at_ms: null },
        visited: [
          { from: "implement", event: "submit_work", to: "review", count: 2, last_at_ms: T },
          { from: "review", event: "override", to: "design", count: 1, last_at_ms: T },
          { from: "review", event: "override", to: "implement", count: 2, last_at_ms: T },
          { from: "design", event: "approve_design", to: "implement", count: 1, last_at_ms: T },
          { from: "review", event: "observed_landed", to: "landed", count: 1, last_at_ms: T },
        ],
        visited_status: [],
        events: [ev({})],
        next: null,
      },
      graph,
    },
    plan: {
      uid: "plan:p",
      title: "The plan",
      tasks: 2,
      points: [
        { at_ms: T - 2 * H, open: 2, in_progress: 0, done: 0, cancelled: 0 },
        { at_ms: T - H, open: 1, in_progress: 1, done: 0, cancelled: 0 },
        { at_ms: T, open: 0, in_progress: 1, done: 1, cancelled: 0 },
      ],
    },
  };
  // React marks where adjacent text nodes meet; the words are what is asserted.
  const html = Object.fromEntries(Object.entries(render(f)).map(([k, v]) => [k, v.replaceAll("<!-- -->", "")]));
  // The overview: the stalled session says for how long; the process's claim is listed, not dropped.
  assert.match(html.overview, /data-stalled="true"/);
  assert.match(html.overview, /stalled 31m/);
  assert.match(html.overview, /Unattributed holders/);
  assert.match(html.overview, /build-host:42/);
  // A session: a lane for its own starts and one per task, and a tick per event.
  assert.match(html.session, /aria-label="Swimlanes"/);
  assert.equal((html.session.match(/class="lane-track"/g) ?? []).length, 2);
  assert.equal((html.session.match(/class="lane-tick"/g) ?? []).length, 3);
  // A source this app does not know (a newer jkb serve's) is drawn, neutrally.
  assert.match(html.session, /class="lane-tick" data-source="other"/);
  assert.match(html.session, /data-holds="true"/);
  // A task: its path inked and counted over its strategy's machine, and the override it took listed.
  assert.match(html.task, /class="machine-edge"[^>]*data-visited="true"/);
  assert.match(html.task, /submit_work ×2/);
  assert.match(html.task, /data-visited="true"[^>]*transform/);
  assert.match(html.task, /<th>taken<\/th>/);
  // Keyed by (from, event): the untaken row between the same states is not inked.
  assert.match(html.task, /<g class="machine-edge"><path [^>]*><\/path><text [^>]*>submit_systemic</);
  assert.match(html.task, /<tr data-visited="true"><td>implement<\/td><td>submit_work<\/td>/);
  assert.match(html.task, /<tr><td>implement<\/td><td>submit_systemic<\/td>/);
  // An override is one row whatever its destination: its count is summed.
  assert.match(html.task, /<td>review<\/td><td>override<\/td><td>\(stated\)<\/td><td>[^<]*<\/td><td>3<\/td>/);
  // A move this graph has no row for is listed, not dropped.
  assert.match(html.task, /design --approve_design--&gt; implement ×1/);
  // A step folded into an observed marker is inked on the marker, not lost from the picture.
  assert.match(html.task, /class="machine-marker" data-visited="true"/);
  assert.match(html.task, /observed_landed ← implement, review\ntaken ×1/);
  // A plan: hourly bins, stacked, through now.
  assert.match(html.plan, /aria-label="Plan progress by hour"/);
  assert.equal((html.plan.match(/<g transform="translate/g) ?? []).length, 4, "four hours, the last running to now");
  assert.match(html.plan, /data-band="done"/);
});

// Which answer may land (the Timeline pane's `asks.ts`), without a window.
test("asks are numbered per scope, a stale page is dropped, and showing the tab re-reads unless a link shows it", async () => {
  const { ask, latest, newAsks, onLinked, onShown, pageApplies } = await load(path.join(src, "executions", "asks.ts"));
  // An overview in flight, then a task asked for: the overview's answer still lands.
  const asks = newAsks();
  const overview = ask(asks, "sessions");
  const task = ask(asks, "task");
  assert.equal(latest(asks, "sessions", overview), true, "another scope's ask does not strand it");
  assert.equal(latest(asks, "task", task), true);
  assert.equal(latest(asks, "task", ask(asks, "task") - 1), false, "a newer ask in the same scope does");

  // A page asked for session A lands on A at the end it was asked from, and nowhere else.
  assert.equal(pageApplies({ id: "a", next: "c1" }, "a", "c1"), true);
  assert.equal(pageApplies({ id: "b", next: "c9" }, "a", "c1"), false, "the operator opened B meanwhile");
  assert.equal(pageApplies({ id: "a", next: "c2" }, "a", "c1"), false, "A was re-read meanwhile");
  assert.equal(pageApplies(undefined, "a", "c1"), false);

  // Hidden at start; shown: re-read. Hidden and shown again: re-read again.
  let st = { wasShown: false, linked: false };
  let r = onShown(st, false);
  assert.equal(r.reRead, false);
  r = onShown(r.next, true);
  assert.equal(r.reRead, true, "first shown");
  r = onShown(r.next, true);
  assert.equal(r.reRead, false, "still shown: no re-read");
  r = onShown(r.next, false);
  r = onShown(r.next, true);
  assert.equal(r.reRead, true, "shown again");
  // A link that shows the tab names what to read: no second read.
  st = onLinked(onShown(r.next, false).next);
  r = onShown(st, true);
  assert.equal(r.reRead, false);
  // A link followed while the tab is already shown leaves the next showing's re-read alone.
  st = onLinked(r.next);
  r = onShown(onShown(st, false).next, true);
  assert.equal(r.reRead, true);
});
