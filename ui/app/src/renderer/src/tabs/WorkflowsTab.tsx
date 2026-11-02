import { useCallback, useEffect, useMemo, useState } from "react";

import {
  agentSetSentence,
  agentSetsOf,
  decodeAgents,
  decodeRoles,
  decodeStrategies,
  decodeWorkflowGraph,
  machineDrawing,
  planOps,
  selectionIn,
  strategyName,
  strategyOption,
  workflowOps,
  type AgentList,
  type MachineName,
  type MachineSelection,
  type RolesView,
  type Strategies,
  type WorkflowGraph,
} from "@jkb/core";

import { AgentGraphView } from "../workflows/AgentGraphView";
import { AgentPanel } from "../workflows/AgentPanel";
import { LifecyclePane } from "../workflows/LifecyclePane";
import { MachinePanel } from "../workflows/MachinePanel";
import { PickerSentences } from "../workflows/PickerSentences";
import { RolePanel, RolePending, RolesTable, type KnownAgents } from "../workflows/RolesView";
import { AGENT, machinePart, roleSide, sideForView, type AgentsView, type Loaded, type Side } from "../workflows/side";
import { WorkflowLegend } from "../workflows/WorkflowLegend";

/** The last agent set and agent, per-window conveniences (D53.1): the only state the tab keeps itself. */
// The key predates the name "agent set" and keeps its old spelling so a remembered pick survives.
const LAST_WORKFLOW_KEY = "jkb.app.workflows.workflow";
const LAST_AGENT_KEY = "jkb.app.workflows.agent";

function remembered(key: string): string | undefined {
  try {
    return window.localStorage.getItem(key) ?? undefined;
  } catch {
    return undefined;
  }
}

function remember(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // Storage unavailable: the tab opens on the first workflow next time.
  }
}


const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

/**
 * The Workflows tab (D53.7): the agents a workflow script runs, as data in jkb. A workflow's agents
 * are drawn as a graph — a node per template, an arrow per hand-off — and the selected one opens in a
 * side panel: its template and permissions, saved as an operator copy that the script then reads by
 * name, reverted to the packaged text, or contributed back to jkb as a pull request. Below the graph,
 * the Lifecycle pane draws the chosen strategy's machines from their compiled tables, marking where
 * the selected agent's role acts. Clicking a state, edge or marker there shows its rows in the side
 * panel instead (`MachinePanel`); whichever was picked last fills it, and Escape gives it back to
 * the agent.
 *
 * The Agents pane's other face is the Roles view (`RolesTable`): every role under the strategy, with
 * what it may do in jkb and the agents that run as it. Picking a role opens it in the side panel and
 * marks where it acts in the Lifecycle pane; an agent's panel links to its role there.
 *
 * Re-read on demand (the refresh button, a save, a change of strategy): nothing announces a template
 * edit, and D53.1 rules out a poll loop.
 */
export function WorkflowsTab(): React.JSX.Element {
  const [agents, setAgents] = useState<Loaded<AgentList>>({ kind: "loading" });
  const [strategies, setStrategies] = useState<Strategies | undefined>(undefined);
  const [strategy, setStrategy] = useState<string | undefined>(undefined);
  const [graph, setGraph] = useState<Loaded<WorkflowGraph>>({ kind: "loading" });
  const [agentSet, setAgentSet] = useState<string | undefined>(() => remembered(LAST_WORKFLOW_KEY));
  const [selected, setSelected] = useState<string | undefined>(() => remembered(LAST_AGENT_KEY));
  const [which, setWhich] = useState<MachineName>("workflow");
  const [side, setSide] = useState<Side>(AGENT);
  const [view, setView] = useState<AgentsView>("agents");
  const [roles, setRoles] = useState<Loaded<RolesView>>({ kind: "loading" });
  // Bumped by Refresh: the agent-type map can change with no strategy change.
  const [rolesAsked, setRolesAsked] = useState(0);
  const [notice, setNotice] = useState<{ readonly text: string; readonly failed: boolean } | undefined>(undefined);

  const loadAgents = useCallback(async () => {
    const answer = decodeAgents(await op(workflowOps.agents()));
    setAgents(answer.ok ? { kind: "loaded", value: answer.value } : { kind: "failed", message: answer.error.message });
  }, []);

  const loadStrategies = useCallback(async () => {
    const answer = decodeStrategies(await op(planOps.strategies()));
    if (answer.ok) setStrategies(answer.value);
    else setNotice({ text: `Cannot list strategies: ${answer.error.message}`, failed: true });
  }, []);

  useEffect(() => {
    void loadAgents();
    void loadStrategies();
  }, [loadAgents, loadStrategies]);

  // The machines of the chosen strategy; the default's until one is chosen.
  useEffect(() => {
    let live = true;
    setGraph({ kind: "loading" });
    void (async () => {
      const answer = decodeWorkflowGraph(await op(workflowOps.graph(strategy === undefined ? undefined : strategyName(strategy))));
      if (live) setGraph(answer.ok ? { kind: "loaded", value: answer.value } : { kind: "failed", message: answer.error.message });
    })();
    return () => {
      live = false;
    };
  }, [strategy]);

  // Every role under the chosen strategy, read while the Roles view or a role's panel is shown, and
  // again whenever the strategy changes, as the graph is, or Refresh is pressed.
  const showingRoles = view === "roles";
  const selectedRole = side.kind === "role" ? side.name : undefined;
  const wantRoles = showingRoles || selectedRole !== undefined;
  useEffect(() => {
    if (!wantRoles) return;
    let live = true;
    setRoles({ kind: "loading" });
    void (async () => {
      const answer = decodeRoles(await op(workflowOps.roles(strategy === undefined ? undefined : strategyName(strategy))));
      if (live) setRoles(answer.ok ? { kind: "loaded", value: answer.value } : { kind: "failed", message: answer.error.message });
    })();
    return () => {
      live = false;
    };
  }, [strategy, wantRoles, rolesAsked]);

  const list = useMemo(() => (agents.kind === "loaded" ? agents.value.agents : []), [agents]);
  const roleNames = agents.kind === "loaded" ? agents.value.roles : [];
  const agentSets = useMemo(() => agentSetsOf(list), [list]);
  const activeAgentSet = agentSet !== undefined && agentSets.includes(agentSet) ? agentSet : agentSets[0];
  // Each picker's selection, in one sentence: absent from an older daemon, and then not shown.
  const agentSetLine = agentSetSentence(agents.kind === "loaded" ? agents.value.agent_sets : [], activeAgentSet);
  const strategyValue = strategy ?? strategies?.default ?? "";
  const pickedStrategy = strategies?.strategies.find((s) => s.name === strategyValue);
  const agent = list.find((a) => a.name === selected && a.workflow === activeAgentSet);
  const fragments = list.filter((a) => a.workflow === activeAgentSet && a.fragment);

  const pickAgentSet = (w: string): void => {
    setAgentSet(w);
    remember(LAST_WORKFLOW_KEY, w);
  };
  const pick = (name: string): void => {
    setSelected(name);
    setSide(AGENT);
    remember(LAST_AGENT_KEY, name);
  };
  // An agent named from a role, in whichever agent set it belongs to.
  const openAgent = (name: string, inSet: string): void => {
    pickAgentSet(inSet);
    pick(name);
    setView("agents");
  };
  const openRole = useCallback((name: string) => {
    setView("roles");
    setSide({ kind: "role", name });
  }, []);
  const role = roleSide(side, roles);
  // Whether the templates are known: until they are, no role can be said to have no agent.
  const knownAgents: KnownAgents = agents.kind === "loaded" ? { kind: "loaded", agents: list } : agents;
  const pickView = (v: AgentsView): void => {
    setView(v);
    setSide((s) => sideForView(s, v));
  };
  // A selection names its machine, so switching machines clears one (and only a machine's).
  const pickMachine = (w: MachineName): void => {
    setWhich(w);
    setSide((s) => (machinePart(s) === undefined ? s : AGENT));
  };
  const onSelectPart = useCallback((sel: MachineSelection | undefined) => setSide(sel ?? AGENT), []);

  // What the machine selection names, if it still exists: a strategy switch can take it away, and
  // the panel then falls back to the agent.
  const shown = useMemo((): MachineSelection | undefined => {
    const part = machinePart(side);
    if (part === undefined || graph.kind !== "loaded") return undefined;
    const table = part.machine === "workflow" ? graph.value.workflow : graph.value.lifecycle;
    return selectionIn(machineDrawing(table), part) ? part : undefined;
  }, [side, graph]);
  // ...and is forgotten, so switching back does not bring it back unasked.
  useEffect(() => {
    if (machinePart(side) !== undefined && graph.kind === "loaded" && shown === undefined) setSide(AGENT);
  }, [side, graph, shown]);

  const onNotice = useCallback((text: string, failed = false) => setNotice({ text, failed }), []);
  const onSaved = useCallback(
    (name: string) => {
      setSelected(name);
      void loadAgents();
    },
    [loadAgents],
  );

  return (
    <div className="workflows-tab">
      <header className="design-bar">
        <h1>Workflows</h1>
        <label className="picker">
          <span>Agent set</span>
          <select value={activeAgentSet ?? ""} disabled={agentSets.length === 0} onChange={(e) => pickAgentSet(e.target.value)} aria-label="Agent set">
            {agentSets.map((w) => (
              <option key={w} value={w}>
                {w}
              </option>
            ))}
          </select>
        </label>
        <label className="picker">
          <span>Strategy</span>
          <select
            value={strategyValue}
            disabled={strategies === undefined}
            onChange={(e) => setStrategy(e.target.value)}
            aria-label="Strategy"
          >
            {(strategies?.strategies ?? []).map((s) => (
              <option key={s.name} value={s.name} title={s.describe}>
                {strategyOption(s, strategies?.default ?? "")}
              </option>
            ))}
          </select>
        </label>
        <button
          type="button"
          className="bar-button"
          onClick={() => {
            setNotice(undefined);
            void loadAgents();
            void loadStrategies();
            setRolesAsked((n) => n + 1);
          }}
          disabled={agents.kind === "loading"}
        >
          Refresh
        </button>
      </header>
      <PickerSentences agentSet={activeAgentSet} agentSetLine={agentSetLine} strategy={pickedStrategy} />
      <WorkflowLegend graph={graph.kind === "loaded" ? graph.value : undefined} />
      {notice !== undefined && (
        <p className="design-notice" data-kind={notice.failed ? "failed" : undefined} role="status">
          {notice.text}
          <span className="spacer" />
          <button type="button" className="bar-button" onClick={() => setNotice(undefined)} aria-label="Dismiss">
            ×
          </button>
        </p>
      )}
      <div className="workflows-split">
        <div className="workflows-main">
          <section className="agents-pane" aria-label={showingRoles ? "Roles" : "Agents"}>
            <header className="pane-bar">
              <div className="segmented" role="radiogroup" aria-label="Agents or roles">
                {(["agents", "roles"] as const).map((v) => (
                  <button
                    key={v}
                    type="button"
                    role="radio"
                    aria-checked={view === v}
                    className="bar-button"
                    data-active={view === v ? "true" : undefined}
                    onClick={() => pickView(v)}
                  >
                    {v === "agents" ? "Agents" : "Roles"}
                  </button>
                ))}
              </div>
              <span className="spacer" />
              {!showingRoles && fragments.length > 0 && (
                <span className="muted lifecycle-note">
                  fragments:{" "}
                  {fragments.map((f) => (
                    <button key={f.name} type="button" className="link-button" onClick={() => pick(f.name)}>
                      {f.name}
                    </button>
                  ))}
                </span>
              )}
            </header>
            <div className="pane-body">
              {showingRoles ? (
                roles.kind === "failed" ? (
                  <p className="muted plan-hint">Cannot list the roles: {roles.message}</p>
                ) : roles.kind === "loading" ? (
                  <p className="muted plan-hint">Loading roles…</p>
                ) : (
                  <RolesTable
                    view={roles.value}
                    agents={knownAgents}
                    selected={selectedRole}
                    onSelect={(name) => setSide({ kind: "role", name })}
                    onOpenAgent={openAgent}
                  />
                )
              ) : agents.kind === "failed" ? (
                <p className="muted plan-hint">Cannot list agent templates: {agents.message}</p>
              ) : agents.kind === "loading" ? (
                <p className="muted plan-hint">Loading agents…</p>
              ) : activeAgentSet === undefined ? (
                <p className="muted plan-hint">No agent templates.</p>
              ) : (
                <AgentGraphView agents={list} workflow={activeAgentSet} selected={agent?.name} onSelect={pick} />
              )}
            </div>
          </section>
          {graph.kind === "loaded" ? (
            <LifecyclePane graph={graph.value} role={selectedRole ?? (showingRoles ? undefined : agent?.role)} which={which} onWhich={pickMachine} selected={shown} onSelect={onSelectPart} />
          ) : (
            <p className="muted plan-hint lifecycle-pending">
              {graph.kind === "failed" ? `Cannot draw the lifecycle: ${graph.message}` : "Loading the lifecycle…"}
            </p>
          )}
        </div>
        <aside className="workflows-side">
          {shown !== undefined && graph.kind === "loaded" && (
            <MachinePanel
              graph={graph.value}
              selection={shown}
              agents={list.filter((a) => a.workflow === activeAgentSet)}
              onSelect={onSelectPart}
              onOpenAgent={pick}
            />
          )}
          {shown === undefined && role.kind === "shown" && (
            <RolePanel view={role.view} role={role.role} agents={knownAgents} onClose={() => setSide(AGENT)} onOpenAgent={openAgent} />
          )}
          {shown === undefined && role.kind === "pending" && <RolePending name={role.name} message={role.message} onClose={() => setSide(AGENT)} />}
          {/* Kept mounted while a machine part or a role fills the aside, so an unsaved template edit survives. */}
          {agent !== undefined ? (
            <div className="agent-panel-host" hidden={shown !== undefined || role.kind !== "none"}>
              <AgentPanel agent={agent} roles={roleNames} onSaved={onSaved} onNotice={onNotice} onOpenRole={openRole} />
            </div>
          ) : (
            shown === undefined &&
            role.kind === "none" && (
              <p className="muted plan-hint workflows-pick">
                {showingRoles ? "Pick a role to see what it may do." : "Pick an agent in the graph to see its template and permissions."}
              </p>
            )
          )}
        </aside>
      </div>
    </div>
  );
}
