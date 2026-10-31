import { useMemo } from "react";

import {
  eventsFiredBy,
  machineDrawing,
  rowKey,
  STATED,
  stateRows,
  type AgentTemplate,
  type GraphEdge,
  type MachineDrawing,
  type MachineName,
  type MachineSelection,
  type MachineTable,
  type WorkflowGraph,
} from "@jkb/core";

import { edgeLabel } from "./LifecyclePane";
import { markerText } from "./machineGeometry";

/** How often a task took each row (`takenByRow`), on the Executions tab; absent elsewhere. */
type Taken = ReadonlyMap<string, number> | undefined;

/** `×N` for a row a task took, or nothing. */
function takenText(taken: Taken, t: GraphEdge): string {
  const n = taken?.get(rowKey(t.from, t.event));
  return n === undefined ? "" : ` · taken ×${n}`;
}

/** A row's destination as words: an override names none, the operator does. */
function destination(t: GraphEdge): string {
  return t.to ?? STATED;
}

/** Whether a row is an observation (the facts decide) or an act someone performs. */
const kindOf = (t: GraphEdge): string => (t.reconciled ? "observed" : "act");

/** One row of a state's lists: selectable when it is drawn as an edge, its words in full when not. */
function Row({
  t,
  drawn,
  taken,
  machine,
  onSelect,
}: {
  readonly t: GraphEdge;
  readonly drawn: boolean;
  readonly taken: Taken;
  readonly machine: MachineName;
  readonly onSelect: (sel: MachineSelection) => void;
}): React.JSX.Element {
  const words = `${t.from} --${t.event}--> ${destination(t)}`;
  const tail = `${t.fired_by}${t.guarded ? " · guarded" : ""}${takenText(taken, t)}`;
  return (
    <li className="machine-row" data-reconciled={t.reconciled ? "true" : undefined} data-taken={taken?.has(rowKey(t.from, t.event)) ? "true" : undefined}>
      {drawn ? (
        <button type="button" className="link-button" onClick={() => onSelect({ kind: "edge", machine, from: t.from, event: t.event })}>
          {words}
        </button>
      ) : (
        <code>{words}</code>
      )}
      <span className="muted"> · {tail}</span>
      {t.to === null && <div className="muted machine-row-note">the operator names the destination</div>}
      {!drawn && t.guard !== null && <div className="muted machine-row-note">guard: {t.guard}</div>}
    </li>
  );
}

function Rows({
  title,
  rows,
  empty,
  drawing,
  taken,
  machine,
  onSelect,
}: {
  readonly title: string;
  readonly rows: readonly GraphEdge[];
  readonly empty: string;
  readonly drawing: MachineDrawing;
  readonly taken: Taken;
  readonly machine: MachineName;
  readonly onSelect: (sel: MachineSelection) => void;
}): React.JSX.Element {
  return (
    <section className="machine-rows" aria-label={title}>
      <h3>{title}</h3>
      {rows.length === 0 ? (
        <p className="muted plan-hint">{empty}</p>
      ) : (
        <ul>
          {rows.map((t) => (
            <Row key={rowKey(t.from, t.event)} t={t} drawn={drawing.edges.includes(t)} taken={taken} machine={machine} onSelect={onSelect} />
          ))}
        </ul>
      )}
    </section>
  );
}

/** A labelled value in the panel's facts list. */
function Fact({ name, children }: { readonly name: string; readonly children: React.ReactNode }): React.JSX.Element {
  return (
    <>
      <dt>{name}</dt>
      <dd>{children}</dd>
    </>
  );
}

/** What a phase's actor is, under the strategy: their agent templates, op classes and events. */
function Actor({
  role,
  table,
  graph,
  agents,
  onOpenAgent,
}: {
  readonly role: string;
  readonly table: MachineTable;
  readonly graph: WorkflowGraph;
  readonly agents: readonly AgentTemplate[] | undefined;
  readonly onOpenAgent: ((name: string) => void) | undefined;
}): React.JSX.Element {
  const templates = (agents ?? []).filter((a) => a.role === role && !a.fragment);
  const ops = graph.role_ops[role];
  const fires = eventsFiredBy(table, role);
  return (
    <>
      {agents !== undefined && (
        <Fact name="agent template">
          {templates.length === 0
            ? `no agent template acts as ${role}`
            : templates.map((a) => (
                <button key={a.name} type="button" className="link-button" onClick={() => onOpenAgent?.(a.name)}>
                  {a.name}
                </button>
              ))}
        </Fact>
      )}
      <Fact name="may run">{ops === undefined ? "not answered by this jkb" : ops.length === 0 ? "no op classes" : ops.join(", ")}</Fact>
      <Fact name="may fire">{fires.length === 0 ? "no events under this strategy" : fires.join(", ")}</Fact>
    </>
  );
}

/**
 * The Lifecycle pane's side panel: the rows of the state, edge or marker selected in the picture.
 * With the transition table gone this is where rows are read — every row is in some state's panel
 * (`stateRows`), and `jkb workflow show --graph` remains the full listing.
 *
 * A state shows who acts there under the strategy, that role's agent templates (opening one in the
 * agent panel, where `agents` is given), its op classes and the events it may fire, then what leaves
 * it, what arrives, and its rows the picture does not draw. An edge shows who fires it, its guard
 * in jkb's words and the command that fires it. A marker lists one row per source state. Read-only.
 */
export function MachinePanel({
  graph,
  selection,
  taken,
  agents,
  onSelect,
  onOpenAgent,
}: {
  readonly graph: WorkflowGraph;
  readonly selection: MachineSelection;
  readonly taken?: ReadonlyMap<string, number>;
  /** The agent set's templates, to link a phase's actor to; omitted where there is no agent panel. */
  readonly agents?: readonly AgentTemplate[];
  readonly onSelect: (sel: MachineSelection | undefined) => void;
  readonly onOpenAgent?: (name: string) => void;
}): React.JSX.Element {
  const machine = selection.machine;
  const table = machine === "workflow" ? graph.workflow : graph.lifecycle;
  const drawing = useMemo(() => machineDrawing(table), [table]);
  const select = (sel: MachineSelection): void => onSelect(sel);

  let title: string;
  let body: React.JSX.Element;
  if (selection.kind === "state") {
    const s = table.states.find((x) => x.name === selection.name);
    const rows = stateRows(drawing, selection.name);
    const current = machine === "workflow" ? graph.phase === selection.name : graph.status === selection.name;
    const flags = [s?.initial ? "initial" : null, s?.settled ? "settled" : null, s?.awaits_input ? "awaits input" : null].filter((x) => x !== null);
    title = `State ${selection.name}`;
    body = (
      <>
        <dl className="machine-facts">
          {flags.length > 0 && <Fact name="kind">{flags.join(", ")}</Fact>}
          {s?.next_role != null && <Fact name="who acts">{s.next_role}</Fact>}
          {s?.next_step != null && (
            <Fact name="next step">
              <code>{s.next_step}</code>
            </Fact>
          )}
          {s?.next_role != null && machine === "workflow" && (
            <Actor role={s.next_role} table={table} graph={graph} agents={agents} onOpenAgent={onOpenAgent} />
          )}
          {graph.task !== null && current && (
            <Fact name={machine === "workflow" ? "phase of" : "status of"}>
              <code>{graph.task}</code>
            </Fact>
          )}
        </dl>
        <Rows
          title="Leaves"
          rows={rows.leaves}
          empty={`no transitions${s?.settled ? " (settled)" : ""}`}
          drawing={drawing}
          taken={taken}
          machine={machine}
          onSelect={select}
        />
        <Rows title="Arrives" rows={rows.arrives} empty="nothing arrives here" drawing={drawing} taken={taken} machine={machine} onSelect={select} />
        <Rows title="Not drawn" rows={rows.listed} empty="none" drawing={drawing} taken={taken} machine={machine} onSelect={select} />
      </>
    );
  } else if (selection.kind === "edge") {
    const t = drawing.edges.find((x) => x.from === selection.from && x.event === selection.event);
    title = t === undefined ? `${selection.from} --${selection.event}-->` : edgeLabel(t);
    body =
      t === undefined ? (
        <p className="muted plan-hint">This edge is not in the machine.</p>
      ) : (
        <dl className="machine-facts">
          <Fact name="from">
            <button type="button" className="link-button" onClick={() => select({ kind: "state", machine, name: t.from })}>
              {t.from}
            </button>
          </Fact>
          <Fact name="event">
            <code>{t.event}</code> ({kindOf(t)})
          </Fact>
          <Fact name="to">
            {t.to === null ? (
              `${STATED}: the operator names the destination`
            ) : (
              <button type="button" className="link-button" onClick={() => select({ kind: "state", machine, name: t.to as string })}>
                {t.to}
              </button>
            )}
          </Fact>
          <Fact name="fired by">{t.fired_by}</Fact>
          {t.roles.length > 0 && <Fact name="roles">{t.roles.join(", ")}</Fact>}
          <Fact name="guard">{t.guard ?? (t.guarded ? "guarded" : "none")}</Fact>
          <Fact name="effects">{t.planned ? "planned with the move" : "none beyond the move"}</Fact>
          {t.command !== null && (
            <Fact name="command">
              <code>{t.command}</code>
            </Fact>
          )}
          {taken !== undefined && <Fact name="taken">{`×${taken.get(rowKey(t.from, t.event)) ?? 0}`}</Fact>}
        </dl>
      );
  } else {
    const target = selection.to;
    const m = drawing.markers.find((x) => x.to === target);
    title = m === undefined ? `Observed into ${target}` : `${markerText(m)} into ${target}`;
    body =
      m === undefined ? (
        <p className="muted plan-hint">This marker is not in the machine.</p>
      ) : (
        <>
          <dl className="machine-facts">
            <Fact name="to">
              <button type="button" className="link-button" onClick={() => select({ kind: "state", machine, name: target })}>
                {target}
              </button>
            </Fact>
          </dl>
          {m.groups.map((g) => {
            const guard = g.rows.find((r) => r.guard !== null)?.guard ?? null;
            const command = g.rows.find((r) => r.command !== null)?.command ?? null;
            return (
              <section key={g.event} className="machine-rows" aria-label={g.event}>
                <h3>
                  <code>{g.event}</code>
                </h3>
                {guard !== null && <p className="muted machine-row-note">guard: {guard}</p>}
                {command !== null && (
                  <p className="muted machine-row-note">
                    command: <code>{command}</code>
                  </p>
                )}
                <ul>
                  {g.rows.map((r) => (
                    <li key={rowKey(r.from, r.event)} className="machine-row" data-taken={taken?.has(rowKey(r.from, r.event)) ? "true" : undefined}>
                      <button type="button" className="link-button" onClick={() => select({ kind: "state", machine, name: r.from })}>
                        {r.from}
                      </button>
                      <span className="muted">
                        {" "}
                        · {r.guarded ? "guarded" : "unguarded"} · {r.fired_by}
                        {takenText(taken, r)}
                      </span>
                    </li>
                  ))}
                </ul>
              </section>
            );
          })}
        </>
      );
  }

  return (
    <section
      className="machine-panel"
      aria-label={title}
      onKeyDown={(e) => {
        if (e.key === "Escape") onSelect(undefined);
      }}
    >
      <header className="pane-bar">
        <h2 title={title}>{title}</h2>
        <span className="spacer" />
        <button type="button" className="bar-button" onClick={() => onSelect(undefined)} aria-label="Close">
          ×
        </button>
      </header>
      <div className="pane-body">{body}</div>
    </section>
  );
}
