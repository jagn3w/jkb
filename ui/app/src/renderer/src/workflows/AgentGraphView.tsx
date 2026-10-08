import { useMemo } from "react";

import { agentGraph, type AgentTemplate } from "@jkb/core";

/** Node and grid geometry, in SVG user units. */
const NODE_W = 184;
const NODE_H = 44;
const GAP_X = 64;
const GAP_Y = 20;
const PAD = 16;

const x = (layer: number): number => PAD + layer * (NODE_W + GAP_X);
const y = (row: number): number => PAD + row * (NODE_H + GAP_Y);

/**
 * A workflow's agents as a graph (D53.7): one node per template the workflow runs, an arrow per
 * hand-off, laid out by `@jkb/core`'s `agentGraph` from the templates themselves — so an edited
 * hand-off redraws the picture, and nothing here is drawn by hand. A hand-off that runs back (a
 * review sending work back to the implementer) bends below the row it leaves.
 */
export function AgentGraphView({
  agents,
  workflow,
  selected,
  onSelect,
}: {
  readonly agents: readonly AgentTemplate[];
  readonly workflow: string;
  readonly selected: string | undefined;
  readonly onSelect: (name: string) => void;
}): React.JSX.Element {
  const graph = useMemo(() => agentGraph(agents, workflow), [agents, workflow]);
  const byName = useMemo(() => new Map(agents.map((a) => [a.name, a])), [agents]);
  const at = new Map(graph.nodes.map((n) => [n.id, n]));
  const width = PAD * 2 + Math.max(1, graph.layers) * NODE_W + Math.max(0, graph.layers - 1) * GAP_X;
  const height = PAD * 2 + Math.max(1, graph.rows) * NODE_H + Math.max(0, graph.rows - 1) * GAP_Y + 24;

  if (graph.nodes.length === 0) {
    return <p className="muted plan-hint">No agents in this workflow.</p>;
  }
  return (
    <div className="agent-graph">
      <svg width={width} height={height} viewBox={`0 0 ${width} ${height}`} role="img" aria-label={`The ${workflow} agents and their hand-offs`}>
        <defs>
          <marker id="agent-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
            <path d="M0,0 L8,4 L0,8 z" className="graph-arrowhead" />
          </marker>
        </defs>
        {graph.links.map((l) => {
          const from = at.get(l.from);
          const to = at.get(l.to);
          if (from === undefined || to === undefined) return null;
          const x1 = x(from.layer) + NODE_W;
          const y1 = y(from.row) + NODE_H / 2;
          const x2 = x(to.layer);
          const y2 = y(to.row) + NODE_H / 2;
          const d = l.back
            ? (() => {
                // Leave from the bottom, come in at the bottom: clear of the forward arrows.
                const sx = x(from.layer) + NODE_W / 2;
                const sy = y(from.row) + NODE_H;
                const tx = x(to.layer) + NODE_W / 2 + 12;
                const ty = y(to.row) + NODE_H;
                const dip = Math.max(sy, ty) + 18;
                return `M${sx},${sy} C${sx},${dip} ${tx},${dip} ${tx},${ty}`;
              })()
            : `M${x1},${y1} C${x1 + GAP_X / 2},${y1} ${x2 - GAP_X / 2},${y2} ${x2},${y2}`;
          return (
            <path
              key={`${l.from}->${l.to}`}
              d={d}
              className="graph-edge"
              data-back={l.back ? "true" : undefined}
              markerEnd="url(#agent-arrow)"
            >
              <title>{`${l.from} hands off to ${l.to}`}</title>
            </path>
          );
        })}
        {graph.nodes.map((n) => {
          const a = byName.get(n.id);
          const isSelected = n.id === selected;
          return (
            <g
              key={n.id}
              className="graph-node"
              data-selected={isSelected ? "true" : undefined}
              data-source={a?.source}
              transform={`translate(${x(n.layer)},${y(n.row)})`}
              onClick={() => onSelect(n.id)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  onSelect(n.id);
                }
              }}
              tabIndex={0}
              role="button"
              aria-pressed={isSelected}
              aria-label={`${n.id}, ${a?.role ?? ""}${a?.source === "operator" ? ", operator copy" : ""}`}
            >
              <rect width={NODE_W} height={NODE_H} rx={4} />
              <text x={10} y={18} className="graph-node-name">
                {n.id}
              </text>
              <text x={10} y={34} className="graph-node-meta">
                {a?.role}
                {a?.source === "operator" ? " · copy" : ""}
                {a?.permissions.model ? ` · ${a.permissions.model}` : ""}
              </text>
              <title>{a?.describe}</title>
            </g>
          );
        })}
      </svg>
      {graph.dangling.length > 0 && (
        <p className="muted plan-hint">
          Hand-offs to agents not in this workflow:{" "}
          {graph.dangling.map((d) => `${d.from} → ${d.to}`).join(", ")}
        </p>
      )}
    </div>
  );
}
