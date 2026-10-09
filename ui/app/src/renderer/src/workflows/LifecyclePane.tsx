import { useMemo, useState } from "react";

import { machineLayout, STATED, type MachineTable, type WorkflowGraph } from "@jkb/core";

const NODE_W = 150;
const NODE_H = 46;
const GAP_X = 96;
const GAP_Y = 26;
const PAD = 16;

const x = (layer: number): number => PAD + layer * (NODE_W + GAP_X);
const y = (row: number): number => PAD + row * (NODE_H + GAP_Y);

type Which = "workflow" | "lifecycle";

/** Who fires a transition, as the table says: the strategy's roles, or an observation's guard. */
/** Who fires a row, as jkb words it (`workflow.graph`'s `fired_by`): the CLI prints the same. */
function firedBy(t: MachineTable["transitions"][number]): string {
  return t.fired_by;
}

/**
 * The Lifecycle pane (D53.7): the task's state machines drawn from the compiled `jkb-fsm` tables
 * (`workflow.graph`, what `jkb workflow show --graph --json` prints) — never redrawn by hand. The
 * strategy's workflow (its phases, who acts next in each, and who may fire each event), or the task
 * lifecycle (its statuses). Observations are dashed. Self-loops and the operator's override, which
 * every live state has, are listed under the picture rather than drawn into it, with every other
 * row: the list is the whole table.
 *
 * `role` highlights the states where that role acts next — the selected agent's place in the
 * strategy.
 */
export function LifecyclePane({
  graph,
  role,
}: {
  readonly graph: WorkflowGraph;
  readonly role: string | undefined;
}): React.JSX.Element {
  const [which, setWhich] = useState<Which>("workflow");
  const machine = which === "workflow" ? graph.workflow : graph.lifecycle;
  const layout = useMemo(() => machineLayout(machine), [machine]);
  const states = useMemo(() => new Map(machine.states.map((s) => [s.name, s])), [machine]);
  const at = new Map(layout.nodes.map((n) => [n.id, n]));
  const width = PAD * 2 + Math.max(1, layout.layers) * NODE_W + Math.max(0, layout.layers - 1) * GAP_X;
  const height = PAD * 2 + Math.max(1, layout.rows) * NODE_H + Math.max(0, layout.rows - 1) * GAP_Y + 40;

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
              {w === "workflow" ? `Workflow · ${graph.graph}` : "Task status"}
            </button>
          ))}
        </div>
        <span className="spacer" />
        <span className="muted lifecycle-note" title={graph.describe}>
          {graph.strategy}
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
            {layout.links.map((l, i) => {
              const from = at.get(l.from);
              const to = at.get(l.to);
              const t = machine.transitions.find((r) => r.from === l.from && r.event === l.label);
              if (from === undefined || to === undefined || t === undefined) return null;
              let d: string;
              let lx: number;
              let ly: number;
              if (l.back) {
                const sx = x(from.layer) + NODE_W / 2;
                const sy = y(from.row) + NODE_H;
                const tx = x(to.layer) + NODE_W / 2 + 10;
                const ty = y(to.row) + NODE_H;
                const dip = Math.max(sy, ty) + 22 + (i % 3) * 6;
                d = `M${sx},${sy} C${sx},${dip} ${tx},${dip} ${tx},${ty}`;
                lx = (sx + tx) / 2;
                ly = dip - 4;
              } else {
                const x1 = x(from.layer) + NODE_W;
                const y1 = y(from.row) + NODE_H / 2;
                const x2 = x(to.layer);
                const y2 = y(to.row) + NODE_H / 2;
                d = `M${x1},${y1} C${x1 + GAP_X / 2},${y1} ${x2 - GAP_X / 2},${y2} ${x2},${y2}`;
                lx = (x1 + x2) / 2;
                ly = (y1 + y2) / 2 - 4;
              }
              return (
                <g key={`${l.from}-${l.label}`} className="machine-edge" data-reconciled={t.reconciled ? "true" : undefined}>
                  <path d={d} className="graph-edge" markerEnd="url(#machine-arrow)" />
                  <text x={lx} y={ly} textAnchor="middle" className="graph-edge-label">
                    {l.label}
                  </text>
                  <title>{`${l.from} --${l.label}--> ${l.to}${firedBy(t) ? ` (${firedBy(t)})` : ""}${t.guarded ? ", guarded" : ""}`}</title>
                </g>
              );
            })}
            {layout.nodes.map((n) => {
              const s = states.get(n.id);
              const acts = role !== undefined && s?.next_role === role;
              const current = which === "workflow" ? graph.phase === n.id : graph.status === n.id;
              return (
                <g
                  key={n.id}
                  className="machine-state"
                  data-settled={s?.settled ? "true" : undefined}
                  data-initial={s?.initial ? "true" : undefined}
                  data-acts={acts ? "true" : undefined}
                  data-current={current ? "true" : undefined}
                  transform={`translate(${x(n.layer)},${y(n.row)})`}
                >
                  <rect width={NODE_W} height={NODE_H} rx={NODE_H / 2} />
                  <text x={NODE_W / 2} y={s?.next_role ? 19 : 28} textAnchor="middle" className="graph-node-name">
                    {n.id}
                  </text>
                  {s?.next_role && (
                    <text x={NODE_W / 2} y={35} textAnchor="middle" className="graph-node-meta">
                      {s.settled ? "settled" : s.next_role}
                    </text>
                  )}
                  <title>{s?.next_step ?? (s?.settled ? "settled" : n.id)}</title>
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
            </tr>
          </thead>
          <tbody>
            {machine.transitions.map((t) => (
              <tr key={`${t.from}-${t.event}`} data-reconciled={t.reconciled ? "true" : undefined}>
                <td>{t.from}</td>
                <td>
                  {t.event}
                  {t.guarded ? <span className="muted"> · guarded</span> : null}
                </td>
                <td>{t.to ?? STATED}</td>
                <td>{firedBy(t)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}
