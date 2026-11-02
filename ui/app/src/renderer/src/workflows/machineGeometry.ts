//! Where the Lifecycle pane draws a machine: every state, edge, edge label and marker placed by a
//! layered layout (`@dagrejs/dagre`, left to right), so no label sits on a node or another label.
//! Pure and synchronous: the same drawing and measure always give the same geometry.
//
// dagre places each edge label as a node of its own in the rank between the edge's ends, which is
// what keeps labels off nodes. The ranks come from the table, not dagre's own cycle breaking (below).

import { Graph, layout, type EdgeLabel, type GraphLabel, type NodeLabel } from "@dagrejs/dagre";
import { markerSources, type GraphEdge, type GraphState, type MachineDrawing, type ObservedMarker } from "@jkb/core";

/** A rectangle by its top-left corner. */
export interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Point {
  x: number;
  y: number;
}

export interface MachineGeometry {
  width: number;
  height: number;
  /** One per state, in table order: the node's drawn box (a marker's reserved band excluded). */
  nodes: (Box & { id: string })[];
  /** One per drawn edge, in the drawing's order; `points` run from `row.from` to `row.to`. */
  edges: { key: string; row: GraphEdge; points: Point[]; label: Box; back: boolean }[];
  /** One per marker, in the drawing's order, sitting above its state. */
  markers: (Box & { to: string })[];
}

/** The text styles a measure is asked about: a node's name, its meta line, an edge or marker label. */
export type Font = "name" | "meta" | "label";
export type Measure = (text: string, font: Font) => number;

/** The node's drawn height, its least width, and the room around its text. */
const NODE_H = 46;
const NODE_MIN_W = 150;
const NODE_PAD_X = 16;
const MARKER_H = 18;
const MARKER_MIN_W = 86;
const MARKER_PAD_X = 10;
/** Between a marker and the node under it. */
const MARKER_GAP = 4;
const LABEL_H = 14;
const LABEL_PAD_X = 8;
const MARGIN = 16;
const NODESEP = 24;
const RANKSEP = 40;
const EDGESEP = 14;

/**
 * Text widths estimated from the styles in `workflows.css`: edge and marker labels are `--font-mono`
 * at 10px (6.1px a character); a node's name is the UI font at 12px, semibold, and its meta line at
 * 11px. Generous, so a real font narrower than the estimate leaves room rather than overlapping.
 */
export const estimate: Measure = (text, font) => text.length * (font === "label" ? 6.1 : font === "name" ? 7.4 : 6.4);

/** A state's second line: who acts next, or `settled`; `undefined` for none. */
export function nodeMeta(s: GraphState): string | undefined {
  if (!s.next_role) return undefined;
  return s.settled ? "settled" : s.next_role;
}

/** A marker's text: `observed ×N`, N the distinct states its rows come from. */
export function markerText(m: ObservedMarker): string {
  return `observed ×${markerSources(m).length}`;
}

/** A drawn edge's key, as the table keys rows. */
export const edgeKey = (row: GraphEdge): string => `${row.from}|${row.event}`;

/**
 * Lay out what `machineDrawing` says to draw. `label` is each edge's text (its event, by default);
 * the pane passes one with the task's count on it, so the box fits what is printed.
 *
 * Ranks: states are ordered by breadth-first distance from the initial state (the first, if none
 * is initial) over every transition, collapsed rows included, with every settled state after every
 * live one, and a state nothing reaches after the rest. (Distance alone puts `landed` one step from
 * the initial state, through its collapsed rows, which would make `reopen` a forward edge and draw
 * the settled states mid-graph.) An edge whose target is not later than its source is a back edge
 * (`reopen`, `reject_design`, `rework`, …): dagre is given it reversed, so it never breaks a cycle
 * by its own heuristic, and its points are turned back round. Each collapsed marker row is an
 * invisible edge of weight 0, so `landed` ranks after the states that reach it without pulling
 * them about.
 */
export function machineGeometry(d: MachineDrawing, measure: Measure = estimate, label: (row: GraphEdge) => string = (r) => r.event): MachineGeometry {
  if (d.states.length === 0) return { width: 2 * MARGIN, height: 2 * MARGIN, nodes: [], edges: [], markers: [] };
  const depth = ranks(d);
  const isBack = (from: string, to: string): boolean => (depth.get(to) ?? 0) <= (depth.get(from) ?? 0);
  const known = new Set(d.states.map((s) => s.name));

  const g = new Graph<GraphLabel, NodeLabel, EdgeLabel>({ multigraph: true });
  g.setGraph({ rankdir: "LR", ranker: "network-simplex", nodesep: NODESEP, ranksep: RANKSEP, edgesep: EDGESEP, marginx: MARGIN, marginy: MARGIN });

  const markerOf = new Map(d.markers.map((m) => [m.to, m]));
  const markerW = (m: ObservedMarker): number => Math.max(MARKER_MIN_W, measure(markerText(m), "label") + 2 * MARKER_PAD_X);
  const reserve = (id: string): number => (markerOf.has(id) ? MARKER_H + MARKER_GAP : 0);
  for (const s of d.states) {
    const meta = nodeMeta(s);
    const text = Math.max(measure(s.name, "name"), meta === undefined ? 0 : measure(meta, "meta"));
    const m = markerOf.get(s.name);
    const w = Math.max(NODE_MIN_W, Math.ceil(text) + 2 * NODE_PAD_X, m === undefined ? 0 : markerW(m));
    g.setNode(s.name, { width: w, height: NODE_H + reserve(s.name) });
  }

  const drawn = d.edges.filter((r): r is GraphEdge & { readonly to: string } => r.to !== null && known.has(r.from) && known.has(r.to));
  for (const r of drawn) {
    const w = Math.ceil(measure(label(r), "label")) + 2 * LABEL_PAD_X;
    const [v, u] = isBack(r.from, r.to) ? [r.to, r.from] : [r.from, r.to];
    g.setEdge(v, u, { width: w, height: LABEL_H, labelpos: "c" }, edgeKey(r));
  }
  for (const m of d.markers) {
    for (const r of m.groups.flatMap((x) => x.rows)) {
      if (r.to === null || !known.has(r.from) || !known.has(r.to)) continue;
      const [v, u] = isBack(r.from, r.to) ? [r.to, r.from] : [r.from, r.to];
      g.setEdge(v, u, { width: 0, height: 0, weight: 0 }, `marker:${edgeKey(r)}`);
    }
  }

  layout(g);

  const boxes = new Map<string, Box>();
  const nodes = d.states.map((s) => {
    const n = g.node(s.name);
    const top = (n.y ?? 0) - n.height / 2 + reserve(s.name);
    const box = { x: (n.x ?? 0) - n.width / 2, y: top, w: n.width, h: NODE_H };
    boxes.set(s.name, box);
    return { id: s.name, ...box };
  });

  const edges = drawn.map((r) => {
    const back = isBack(r.from, r.to);
    const e = back ? g.edge(r.to, r.from, edgeKey(r)) : g.edge(r.from, r.to, edgeKey(r));
    const points = (e.points ?? []).map((p) => ({ x: p.x, y: p.y }));
    if (back) points.reverse();
    // dagre ends an edge on the box it laid out, which for a state with a marker includes the band
    // reserved above it: end it on the state's drawn box instead.
    const from = boxes.get(r.from);
    const to = boxes.get(r.to);
    if (points.length >= 2 && from !== undefined && to !== undefined) {
      points[0] = clip(from, points[1] as Point);
      points[points.length - 1] = clip(to, points[points.length - 2] as Point);
    }
    const w = e.width ?? 0;
    const h = e.height ?? 0;
    return { key: edgeKey(r), row: r, points, label: { x: (e.x ?? 0) - w / 2, y: (e.y ?? 0) - h / 2, w, h }, back };
  });

  const markers = d.markers.flatMap((m) => {
    const n = boxes.get(m.to);
    if (n === undefined) return [];
    const w = markerW(m);
    return [{ to: m.to, x: n.x + (n.w - w) / 2, y: n.y - MARKER_GAP - MARKER_H, w, h: MARKER_H }];
  });

  // dagre's own extent covers nodes and labels but not the bends of a long edge, which can route
  // outside it: frame everything drawn, with the margin all round.
  const all = [...nodes, ...markers, ...edges.map((e) => e.label), ...edges.flatMap((e) => e.points.map((p) => ({ ...p, w: 0, h: 0 })))];
  const minX = Math.min(...all.map((b) => b.x));
  const minY = Math.min(...all.map((b) => b.y));
  const dx = MARGIN - minX;
  const dy = MARGIN - minY;
  const shift = <T extends Point>(b: T): T => ({ ...b, x: b.x + dx, y: b.y + dy });
  return {
    width: Math.max(...all.map((b) => b.x + b.w)) - minX + 2 * MARGIN,
    height: Math.max(...all.map((b) => b.y + b.h)) - minY + 2 * MARGIN,
    nodes: nodes.map(shift),
    edges: edges.map((e) => ({ ...e, points: e.points.map(shift), label: shift(e.label) })),
    markers: markers.map(shift),
  };
}

/**
 * Each state's place in the order dagre is handed: breadth-first distance from the initial state
 * (or the first) along every drawn edge and collapsed row; a state nothing reaches starts a layer
 * after the rest; then every settled state one past the last live one.
 */
function ranks(d: MachineDrawing): Map<string, number> {
  const ids = d.states.map((s) => s.name);
  const known = new Set(ids);
  const rows = [...d.edges, ...d.markers.flatMap((m) => m.groups.flatMap((g) => g.rows))].filter(
    (r): r is GraphEdge & { readonly to: string } => r.to !== null && known.has(r.from) && known.has(r.to),
  );
  const depth = new Map<string, number>();
  const queue: string[] = [];
  const walk = (): void => {
    while (queue.length > 0) {
      const at = queue.shift() as string;
      const next = (depth.get(at) as number) + 1;
      for (const r of rows) {
        if (r.from === at && !depth.has(r.to)) {
          depth.set(r.to, next);
          queue.push(r.to);
        }
      }
    }
  };
  const root = d.states.find((s) => s.initial)?.name ?? (ids[0] as string);
  depth.set(root, 0);
  queue.push(root);
  walk();
  for (const id of ids) {
    if (!depth.has(id)) {
      depth.set(id, Math.max(...depth.values()) + 1);
      queue.push(id);
      walk();
    }
  }
  const live = d.states.filter((s) => !s.settled).map((s) => depth.get(s.name) as number);
  const last = live.length === 0 ? -1 : Math.max(...live);
  for (const s of d.states) if (s.settled) depth.set(s.name, last + 1);
  return depth;
}

/** Where the line from `p` to `box`'s centre crosses `box`'s border. */
function clip(box: Box, p: Point): Point {
  const cx = box.x + box.w / 2;
  const cy = box.y + box.h / 2;
  const dx = p.x - cx;
  const dy = p.y - cy;
  if (dx === 0 && dy === 0) return { x: cx, y: cy };
  const hw = box.w / 2;
  const hh = box.h / 2;
  const s = Math.abs(dy) * hw > Math.abs(dx) * hh ? hh / Math.abs(dy) : hw / Math.abs(dx);
  return { x: cx + dx * s, y: cy + dy * s };
}
