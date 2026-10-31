import { useMemo, useState } from "react";

import { machineDrawing, STATED, strategyLabel, type GraphEdge, type MachineTable, type VisitedEdge, type WorkflowGraph } from "@jkb/core";

import { estimate, machineGeometry, markerText, nodeMeta, type Font, type Measure, type Point } from "./machineGeometry";

/**
 * A smooth path through `points` (Catmull-Rom, as cubic Béziers): it passes through every point
 * the layout gave, the label's centre among them, so the label sits on its own edge.
 */
export function pathThrough(points: readonly Point[]): string {
  const [first] = points;
  if (first === undefined) return "";
  let d = `M${first.x},${first.y}`;
  for (let i = 0; i + 1 < points.length; i++) {
    const p0 = points[i - 1] ?? (points[i] as Point);
    const p1 = points[i] as Point;
    const p2 = points[i + 1] as Point;
    const p3 = points[i + 2] ?? p2;
    const c1 = { x: p1.x + (p2.x - p0.x) / 6, y: p1.y + (p2.y - p0.y) / 6 };
    const c2 = { x: p2.x - (p3.x - p1.x) / 6, y: p2.y - (p3.y - p1.y) / 6 };
    d += ` C${c1.x},${c1.y} ${c2.x},${c2.y} ${p2.x},${p2.y}`;
  }
  return d;
}

/** The CSS each measured style uses (`workflows.css`), with the font stack read from the page. */
const FONTS: Record<Font, (ui: string, mono: string) => string> = {
  name: (ui) => `600 12px ${ui}`,
  meta: (ui) => `11px ${ui}`,
  label: (_, mono) => `10px ${mono}`,
};

/**
 * Text measured in the page's own fonts, never narrower than the estimate: a font still loading
 * measures as its fallback, and a box too small for the text is the overlap this layout prevents.
 */
function pageMeasure(): Measure {
  const ctx = typeof document === "undefined" ? null : document.createElement("canvas").getContext("2d");
  if (ctx === null) return estimate;
  const css = getComputedStyle(document.documentElement);
  const ui = css.getPropertyValue("--font-ui").trim() || "sans-serif";
  const mono = css.getPropertyValue("--font-mono").trim() || "monospace";
  const seen = new Map<string, number>();
  return (text, font) => {
    const k = `${font}\u0000${text}`;
    let w = seen.get(k);
    if (w === undefined) {
      ctx.font = FONTS[font](ui, mono);
      w = Math.max(estimate(text, font), ctx.measureText(text).width);
      seen.set(k, w);
    }
    return w;
  };
}

type Which = "workflow" | "lifecycle";

/** Who fires a transition, as the table says: the strategy's roles, or an observation's guard. */
/** Who fires a row, as jkb words it (`workflow.graph`'s `fired_by`): the CLI prints the same. */
function firedBy(t: MachineTable["transitions"][number]): string {
  return t.fired_by;
}

/** The edges a task's history took, per machine: `timeline.task`'s `visited` and `visited_status`. */
export interface VisitedOverlay {
  readonly workflow: readonly VisitedEdge[];
  readonly lifecycle: readonly VisitedEdge[];
}

/** A row's key, as the table keys rows: the state it applies in and the event it answers. */
const rowKey = (from: string, event: string): string => `${from}\u0000${event}`;

/**
 * The Lifecycle pane (D53.7): the task's state machines drawn from the compiled `jkb-fsm` tables
 * (`workflow.graph`, what `jkb workflow show --graph --json` prints) — never redrawn by hand. The
 * strategy's workflow (its phases, who acts next in each, and who may fire each event), or the task
 * lifecycle (its statuses), laid out in layers left to right with routed edges (`machineGeometry`),
 * so no label sits on a node or another label. Observations are dashed. One observation reaching a
 * state from several states (`observed_landed` from every live phase) is one dashed `observed ×N`
 * pill above that state, not an arrow per phase (`machineDrawing`). Self-loops and the operator's
 * override, which every live state has, are listed under the picture rather than drawn into it,
 * with every other row: the list is the whole table.
 *
 * `role` highlights the states where that role acts next — the selected agent's place in the
 * strategy. `visited`, from the Executions tab, draws one task's path over the same table: each
 * row it took — matched by (from, event), as the table keys its rows, so two events between the same
 * states are told apart — is inked and counted, and each state it stood in is marked. A move whose
 * row this table does not hold (a row of another graph the task ran before its strategy changed) is
 * listed under it.
 */
export function LifecyclePane({
  graph,
  role,
  visited,
}: {
  readonly graph: WorkflowGraph;
  readonly role: string | undefined;
  readonly visited?: VisitedOverlay;
}): React.JSX.Element {
  const [which, setWhich] = useState<Which>("workflow");
  const machine = which === "workflow" ? graph.workflow : graph.lifecycle;
  const moves = useMemo(
    () => (visited === undefined ? [] : which === "workflow" ? visited.workflow : visited.lifecycle),
    [visited, which],
  );
  // Per row, how often it was taken: an override is one row with several destinations, so summed.
  const taken = useMemo(() => {
    const counts = new Map<string, number>();
    for (const v of moves) counts.set(rowKey(v.from, v.event), (counts.get(rowKey(v.from, v.event)) ?? 0) + v.count);
    return counts;
  }, [moves]);
  const stood = useMemo(() => new Set(moves.flatMap((v) => [v.from, v.to])), [moves]);
  const offTable = useMemo(
    () => moves.filter((v) => !machine.transitions.some((t) => t.from === v.from && t.event === v.event)),
    [moves, machine],
  );
  const drawing = useMemo(() => machineDrawing(machine), [machine]);
  const measure = useMemo(pageMeasure, []);
  // An edge's label is its event and, on a task's path, how often it was taken: the layout sizes
  // the box for what is printed.
  const labelOf = useMemo(() => {
    return (t: GraphEdge): string => {
      const went = taken.get(rowKey(t.from, t.event));
      return went === undefined ? t.event : `${t.event} ×${went}`;
    };
  }, [taken]);
  const geometry = useMemo(() => machineGeometry(drawing, measure, labelOf), [drawing, measure, labelOf]);
  const states = useMemo(() => new Map(machine.states.map((s) => [s.name, s])), [machine]);
  const markerOf = new Map(drawing.markers.map((m) => [m.to, m]));
  const { width, height } = geometry;

  return (
    <section className="lifecycle-pane" aria-label="Lifecycle">
      <header className="pane-bar">
        <h2>Lifecycle</h2>
        <div className="segmented" role="radiogroup" aria-label="Machine">
          {(["workflow", "lifecycle"] as const).map((w) => (
            <button
              key={w}
              type="button"
              role="radio"
              aria-checked={which === w}
              className="bar-button"
              data-active={which === w ? "true" : undefined}
              onClick={() => setWhich(w)}
            >
              {w === "workflow" ? `Phases · ${graph.graph} graph` : "Task status"}
            </button>
          ))}
        </div>
        <span className="spacer" />
        <span className="muted lifecycle-note" title={graph.describe}>
          {strategyLabel(graph.strategy)}
        </span>
      </header>
      <div className="pane-body">
        <div className="machine-graph">
          <svg width={width} height={height} viewBox={`0 0 ${width} ${height}`} role="img" aria-label={`The ${which} state machine`}>
            <defs>
              <marker id="machine-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
                <path d="M0,0 L8,4 L0,8 z" className="graph-arrowhead" />
              </marker>
            </defs>
            {geometry.edges.map((e) => {
              const t = e.row;
              const went = taken.get(rowKey(t.from, t.event));
              const l = e.label;
              return (
                <g
                  key={e.key}
                  className="machine-edge"
                  data-reconciled={t.reconciled ? "true" : undefined}
                  data-visited={went !== undefined ? "true" : undefined}
                  data-back={e.back ? "true" : undefined}
                >
                  <path d={pathThrough(e.points)} className="graph-edge" markerEnd="url(#machine-arrow)" />
                  <text x={l.x + l.w / 2} y={l.y + l.h / 2} textAnchor="middle" dominantBaseline="central" className="graph-edge-label">
                    {labelOf(t)}
                  </text>
                  <title>{`${t.from} --${t.event}--> ${t.to ?? STATED}${firedBy(t) ? ` (${firedBy(t)})` : ""}${t.guarded ? ", guarded" : ""}`}</title>
                </g>
              );
            })}
            {geometry.nodes.map((n) => {
              const s = states.get(n.id);
              const meta = s === undefined ? undefined : nodeMeta(s);
              const acts = role !== undefined && s?.next_role === role;
              const current = which === "workflow" ? graph.phase === n.id : graph.status === n.id;
              return (
                <g
                  key={n.id}
                  className="machine-state"
                  data-state={n.id}
                  data-settled={s?.settled ? "true" : undefined}
                  data-initial={s?.initial ? "true" : undefined}
                  data-acts={acts ? "true" : undefined}
                  data-current={current ? "true" : undefined}
                  data-visited={stood.has(n.id) ? "true" : undefined}
                >
                  <rect x={n.x} y={n.y} width={n.w} height={n.h} rx={n.h / 2} />
                  <text x={n.x + n.w / 2} y={n.y + (meta !== undefined ? 19 : 28)} textAnchor="middle" className="graph-node-name">
                    {n.id}
                  </text>
                  {meta !== undefined && (
                    <text x={n.x + n.w / 2} y={n.y + 35} textAnchor="middle" className="graph-node-meta">
                      {meta}
                    </text>
                  )}
                  <title>{s?.next_step ?? (s?.settled ? "settled" : n.id)}</title>
                </g>
              );
            })}
            {geometry.markers.map((b) => {
              const m = markerOf.get(b.to);
              if (m === undefined) return null;
              // The task's path can run through a row folded into this marker; ink the marker then,
              // or the step would show in the table below but vanish from the picture.
              const went = m.groups.flatMap((g) => g.rows).reduce((sum, r) => sum + (taken.get(rowKey(r.from, r.event)) ?? 0), 0);
              return (
                <g key={`marker-${m.to}`} className="machine-marker" data-visited={went > 0 ? "true" : undefined}>
                  <rect x={b.x} y={b.y} width={b.w} height={b.h} rx={b.h / 2} />
                  <text x={b.x + b.w / 2} y={b.y + b.h / 2} textAnchor="middle" dominantBaseline="central" className="machine-marker-label">
                    {markerText(m)}
                  </text>
                  <title>
                    {m.groups
                      .map((g) => `${g.event} ← ${[...new Set(g.rows.map((r) => r.from))].join(", ")}`)
                      .concat(went > 0 ? [`taken ×${went}`] : [])
                      .join("\n")}
                  </title>
                </g>
              );
            })}
          </svg>
        </div>
        <table className="machine-table">
          <thead>
            <tr>
              <th>from</th>
              <th>event</th>
              <th>to</th>
              <th>{which === "workflow" ? "fired by" : "kind"}</th>
              {visited !== undefined && <th>taken</th>}
            </tr>
          </thead>
          <tbody>
            {machine.transitions.map((t) => (
              <tr
                key={`${t.from}-${t.event}`}
                data-reconciled={t.reconciled ? "true" : undefined}
                data-visited={taken.has(rowKey(t.from, t.event)) ? "true" : undefined}
              >
                <td>{t.from}</td>
                <td>
                  {t.event}
                  {t.guarded ? <span className="muted"> · guarded</span> : null}
                </td>
                <td>{t.to ?? STATED}</td>
                <td>{firedBy(t)}</td>
                {visited !== undefined && <td>{taken.get(rowKey(t.from, t.event)) ?? ""}</td>}
              </tr>
            ))}
          </tbody>
        </table>
        {offTable.length > 0 && (
          <ul className="visited-off-table" aria-label="Moves this table has no row for">
            {offTable.map((v) => (
              <li key={`${rowKey(v.from, v.event)}\u0000${v.to}`}>
                {v.from} --{v.event}--&gt; {v.to} ×{v.count} <span className="muted">(a row of another graph)</span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}
