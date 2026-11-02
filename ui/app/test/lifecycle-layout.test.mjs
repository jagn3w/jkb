//! The Lifecycle pane's layout without a window: `machineGeometry` over both machines of every preset
//! strategy, as `workflow.graph` answers them (fixtures/workflow-graphs.json, which
//! `graph_fixture_matches_presets` in crates/jkb-api holds to the tables), and over the edge cases.
//
// Bundled with esbuild, as in workflows.test.mjs. The same properties are checked against the real
// renderer's text metrics by lifecycle-smoke.test.mjs.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

import { intersects, overlaps } from "./lifecycle-boxes.mjs";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-lifecycle-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const require = createRequire(import.meta.url);

const dir = path.join(here, "..", "src", "renderer", "src", "workflows");
const outfile = path.join(work, "geometry.cjs");
await esbuild.build({
  stdin: { contents: 'export * from "./machineGeometry.ts"; export { machineDrawing } from "@jkb/core";', resolveDir: dir, loader: "ts" },
  bundle: true,
  format: "cjs",
  platform: "node",
  outfile,
  logLevel: "silent",
});
const { machineGeometry, machineDrawing, estimate } = require(outfile);
const paneOut = path.join(work, "pane.cjs");
await esbuild.build({ entryPoints: [path.join(dir, "LifecyclePane.tsx")], bundle: true, format: "cjs", platform: "node", jsx: "automatic", outfile: paneOut, logLevel: "silent" });
const { pathThrough } = require(paneOut);

const presets = JSON.parse(fs.readFileSync(path.join(here, "fixtures", "workflow-graphs.json"), "utf8"));
const machines = Object.entries(presets).flatMap(([name, g]) => [
  [`${name} workflow`, g.workflow],
  [`${name} lifecycle`, g.lifecycle],
]);

test("the fixture holds both machines of every preset", () => {
  assert.deepEqual(Object.keys(presets).sort(), ["autonomous", "coordinated", "design-reviewed"]);
  for (const [, m] of machines) assert.ok(m.states.length > 0 && m.transitions.length > 0);
});

const lay = (m, ...rest) => machineGeometry(machineDrawing(m), ...rest);

for (const [name, m] of machines) {
  test(`${name}: no two node boxes intersect`, () => {
    const g = lay(m);
    assert.equal(g.nodes.length, m.states.length);
    assert.deepEqual(overlaps(g.nodes, g.nodes), []);
  });

  test(`${name}: no label box intersects a node box or another label`, () => {
    const g = lay(m);
    assert.ok(g.edges.length > 0);
    const labels = g.edges.map((e) => ({ id: e.key, ...e.label }));
    assert.ok(labels.every((l) => l.w > 0 && l.h > 0));
    assert.deepEqual(overlaps(labels, g.nodes), []);
    assert.deepEqual(overlaps(labels, labels), []);
  });

  test(`${name}: no marker intersects a node or a label`, () => {
    const g = lay(m);
    const markers = g.markers.map((x) => ({ id: `marker ${x.to}`, ...x }));
    assert.equal(markers.length, machineDrawing(m).markers.length);
    const labels = g.edges.map((e) => ({ id: e.key, ...e.label }));
    assert.deepEqual(overlaps(markers, g.nodes), []);
    assert.deepEqual(overlaps(markers, labels), []);
    assert.deepEqual(overlaps(markers, markers), []);
  });

  test(`${name}: settled states rank after every live one`, () => {
    const g = lay(m);
    const settled = new Set(m.states.filter((s) => s.settled).map((s) => s.name));
    assert.ok(settled.size > 0);
    const live = g.nodes.filter((n) => !settled.has(n.id));
    for (const s of g.nodes.filter((n) => settled.has(n.id))) {
      for (const l of live) assert.ok(s.x > l.x + l.w, `${s.id} (x ${s.x}) is right of ${l.id} (right edge ${l.x + l.w})`);
    }
  });

  test(`${name}: the same table lays out identically twice`, () => {
    assert.deepEqual(lay(m), lay(structuredClone(m)));
  });

  test(`${name}: every edge runs from its source's box to its target's, inside the picture`, () => {
    const g = lay(m);
    const box = new Map(g.nodes.map((n) => [n.id, n]));
    for (const e of g.edges) {
      assert.ok(e.points.length >= 2, e.key);
      assert.ok(onBorder(box.get(e.row.from), e.points[0]), `${e.key} starts on ${e.row.from}: ${JSON.stringify(e.points[0])}`);
      assert.ok(onBorder(box.get(e.row.to), e.points.at(-1)), `${e.key} ends on ${e.row.to}: ${JSON.stringify(e.points.at(-1))}`);
      for (const p of e.points) assert.ok(p.x >= 0 && p.y >= 0 && p.x <= g.width && p.y <= g.height, `${e.key} stays in the picture`);
    }
    // dagre gives an edge's points rank by rank: a forward edge's run rightwards, a back edge's,
    // turned round, leftwards.
    for (const e of g.edges) {
      for (let i = 0; i + 1 < e.points.length; i++) {
        const [a, b] = [e.points[i], e.points[i + 1]];
        assert.ok(e.back ? b.x <= a.x + 0.01 : b.x >= a.x - 0.01, `${e.key} (${e.back ? "back" : "forward"}) runs one way: ${JSON.stringify(e.points)}`);
      }
    }
    for (const b of [...g.nodes, ...g.markers, ...g.edges.map((e) => e.label)]) {
      assert.ok(b.x >= 0 && b.y >= 0 && b.x + b.w <= g.width && b.y + b.h <= g.height, `${JSON.stringify(b)} is inside ${g.width}×${g.height}`);
    }
  });

  test(`${name}: no edge crosses a marker, but the edges of the state it sits on`, () => {
    const g = lay(m);
    const crossings = g.markers.flatMap((mk) =>
      g.edges
        .filter((e) => e.row.from !== mk.to && e.row.to !== mk.to)
        .filter((e) => e.points.some((p, i) => i > 0 && crosses(e.points[i - 1], p, mk)))
        .map((e) => [e.key, mk.to]),
    );
    assert.deepEqual(crossings, []);
  });
}

/** Whether the segment `a`–`b` passes through the inside of box `r` (Liang–Barsky). */
function crosses(a, b, r) {
  let t0 = 0;
  let t1 = 1;
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  for (const [p, q] of [
    [-dx, a.x - r.x],
    [dx, r.x + r.w - a.x],
    [-dy, a.y - r.y],
    [dy, r.y + r.h - a.y],
  ]) {
    if (p === 0) {
      if (q <= 0) return false;
    } else {
      const t = q / p;
      if (p < 0) t0 = Math.max(t0, t);
      else t1 = Math.min(t1, t);
    }
  }
  return t1 - t0 > 1e-6;
}

/** Whether `p` lies on `b`'s border. */
function onBorder(b, p) {
  const e = 0.01;
  const inX = p.x >= b.x - e && p.x <= b.x + b.w + e;
  const inY = p.y >= b.y - e && p.y <= b.y + b.h + e;
  const onX = Math.abs(p.x - b.x) < e || Math.abs(p.x - b.x - b.w) < e;
  const onY = Math.abs(p.y - b.y) < e || Math.abs(p.y - b.y - b.h) < e;
  return inX && inY && (onX || onY);
}

test("back edges are the ones running to a state no later than their source, drawn source to target", () => {
  const g = lay(presets["design-reviewed"].workflow);
  const back = g.edges.filter((e) => e.back).map((e) => e.key);
  for (const k of ["design_review|reject_design", "review|review_failed", "landable|rework", "landed|reopen", "cancelled|reopen"]) {
    assert.ok(back.includes(k), `${k} is a back edge: ${back}`);
  }
  for (const k of ["design|submit_design", "implement|submit_work", "review|review_passed"]) assert.ok(!back.includes(k), `${k} runs forward`);
  const x = new Map(g.nodes.map((n) => [n.id, n.x]));
  const reopen = g.edges.find((e) => e.key === "landed|reopen");
  assert.ok(x.get("landed") > x.get("implement"), "landed is drawn after implement, which reopen runs back to");
  assert.ok(reopen.points[0].x > reopen.points.at(-1).x, "reopen's points run from landed back to implement");
});

const st = (name, over = {}) => ({ name, initial: false, settled: false, awaits_input: false, next_role: "implementer", next_step: "s", ...over });
const tr = (from, event, to, over = {}) => ({ from, event, to, reconciled: false, guarded: false, planned: false, roles: ["operator"], fired_by: "operator", ...over });
const obs = (from, event, to) => tr(from, event, to, { reconciled: true, guarded: true, roles: [], fired_by: "observed" });

test("an empty machine lays out as an empty picture", () => {
  const g = lay({ states: [], transitions: [] });
  assert.deepEqual({ nodes: g.nodes, edges: g.edges, markers: g.markers }, { nodes: [], edges: [], markers: [] });
  assert.ok(g.width > 0 && g.height > 0);
});

test("with no initial state, the first state is the root", () => {
  const g = lay({ states: [st("b"), st("a"), st("c")], transitions: [tr("a", "go", "c"), tr("b", "go", "a")] });
  const x = new Map(g.nodes.map((n) => [n.id, n.x]));
  assert.ok(x.get("b") < x.get("a") && x.get("a") < x.get("c"), JSON.stringify([...x]));
  assert.ok(g.edges.every((e) => !e.back));
  // Rooted at the first state, `a -> b` would run forward instead; here it is the first that leads.
  const h = lay({ states: [st("a"), st("b")], transitions: [tr("b", "go", "a")] });
  assert.equal(h.edges[0].back, true, "rooted at a, the edge into it from b is a back edge");
});

test("a state the initial one cannot reach goes after the rest of the live states, and before the settled", () => {
  const m = {
    states: [st("start", { initial: true }), st("next"), st("orphan"), st("done", { settled: true, next_role: null })],
    transitions: [tr("start", "go", "next"), tr("next", "finish", "done"), tr("orphan", "finish", "done"), tr("orphan", "rejoin", "next")],
  };
  const g = lay(m);
  const x = new Map(g.nodes.map((n) => [n.id, n.x]));
  assert.ok(x.get("orphan") > x.get("next"), JSON.stringify([...x]));
  assert.ok(x.get("done") > x.get("orphan"), JSON.stringify([...x]));
  assert.equal(g.edges.find((e) => e.key === "orphan|rejoin").back, true);
});

test("two edges between the same pair are both drawn, keyed by source and event, their labels apart", () => {
  const m = { states: [st("a", { initial: true }), st("b")], transitions: [tr("a", "one", "b"), tr("a", "two", "b"), tr("b", "back", "a")] };
  const g = lay(m);
  assert.deepEqual(g.edges.map((e) => e.key), ["a|one", "a|two", "b|back"]);
  const labels = g.edges.map((e) => ({ id: e.key, ...e.label }));
  assert.deepEqual(overlaps(labels, labels), []);
  assert.deepEqual(overlaps(labels, g.nodes), []);
});

test("a state only collapsed rows reach ranks after their sources, a marker above it", () => {
  // `landed`'s only drawn edge is its outgoing `reopen`; the invisible marker rows place it.
  const m = {
    states: [st("design", { initial: true }), st("implement"), st("review"), st("landed", { settled: true, next_role: null })],
    transitions: [
      tr("design", "submit", "implement"),
      tr("implement", "submit", "review"),
      ...["design", "implement", "review"].map((p) => obs(p, "observed_landed", "landed")),
      tr("landed", "reopen", "implement"),
    ],
  };
  const d = machineDrawing(m);
  assert.equal(d.markers.length, 1);
  const g = machineGeometry(d);
  const box = new Map(g.nodes.map((n) => [n.id, n]));
  assert.ok(box.get("landed").x > box.get("review").x + box.get("review").w);
  const [marker] = g.markers;
  assert.equal(marker.to, "landed");
  assert.ok(marker.y + marker.h <= box.get("landed").y, "the marker sits above its state, not on it");
  assert.ok(marker.x >= box.get("landed").x && marker.x + marker.w <= box.get("landed").x + box.get("landed").w);
});

test("a label is as wide as its text measures, the count the pane prints included", () => {
  const m = presets["design-reviewed"].workflow;
  const plain = lay(m).edges.find((e) => e.key === "implement|submit_work");
  const counted = lay(m, estimate, (r) => (r.event === "submit_work" ? `${r.event} ×12` : r.event)).edges.find((e) => e.key === "implement|submit_work");
  assert.ok(counted.label.w >= plain.label.w + 4 * 6.1 - 1, `${counted.label.w} fits " ×12" beyond ${plain.label.w}`);
  const wide = lay(m, (t, f) => estimate(t, f) * 2);
  const labels = wide.edges.map((e) => ({ id: e.key, ...e.label }));
  assert.deepEqual(overlaps(labels, labels), [], "a wider measure still leaves no overlap");
  assert.deepEqual(overlaps(labels, wide.nodes), []);
  assert.ok(wide.nodes.some((n) => n.w > 150), "a node widens to fit its name");
});

test("intersects counts touching as apart", () => {
  assert.equal(intersects({ x: 0, y: 0, w: 10, h: 10 }, { x: 10, y: 0, w: 10, h: 10 }), false);
  assert.equal(intersects({ x: 0, y: 0, w: 10, h: 10 }, { x: 9, y: 9, w: 10, h: 10 }), true);
});

test("an edge's path passes through every point the layout gave, in order", () => {
  assert.equal(pathThrough([]), "");
  assert.equal(pathThrough([{ x: 1, y: 2 }]), "M1,2");
  const pts = [
    { x: 0, y: 0 },
    { x: 10, y: 5 },
    { x: 20, y: -5 },
    { x: 30, y: 0 },
  ];
  const d = pathThrough(pts);
  assert.match(d, /^M0,0 C/);
  const ends = [...d.matchAll(/C[^C]* (-?[\d.]+),(-?[\d.]+)/g)].map((m) => ({ x: Number(m[1]), y: Number(m[2]) }));
  assert.deepEqual(ends, pts.slice(1));
  // Straight points stay straight: the control points lie on the line.
  assert.equal(pathThrough([{ x: 0, y: 0 }, { x: 6, y: 0 }, { x: 12, y: 0 }]), "M0,0 C1,0 4,0 6,0 C8,0 11,0 12,0");
});
