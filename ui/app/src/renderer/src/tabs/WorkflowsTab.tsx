import { useCallback, useEffect, useMemo, useState } from "react";

import {
  decodeAgents,
  decodeStrategies,
  decodeWorkflowGraph,
  planOps,
  strategyName,
  workflowOps,
  workflowsOf,
  type AgentTemplate,
  type Strategies,
  type WorkflowGraph,
} from "@jkb/core";

import { AgentGraphView } from "../workflows/AgentGraphView";
import { AgentPanel } from "../workflows/AgentPanel";
import { LifecyclePane } from "../workflows/LifecyclePane";

/** The last workflow and agent, per-window conveniences (D53.1): the only state the tab keeps itself. */
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

type Loaded<T> =
  | { readonly kind: "loading" }
  | { readonly kind: "loaded"; readonly value: T }
  | { readonly kind: "failed"; readonly message: string };

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

/**
 * The Workflows tab (D53.7): the agents a workflow script runs, as data in jkb. A workflow's agents
 * are drawn as a graph — a node per template, an arrow per hand-off — and the selected one opens in a
 * side panel: its template and permissions, saved as an operator copy that the script then reads by
 * name, reverted to the packaged text, or contributed back to jkb as a pull request. Below the graph,
 * the Lifecycle pane draws the chosen strategy's machines from their compiled tables, marking where
 * the selected agent's role acts.
 *
 * Re-read on demand (the refresh button, a save, a change of strategy): nothing announces a template
 * edit, and D53.1 rules out a poll loop.
 */
export function WorkflowsTab(): React.JSX.Element {
  const [agents, setAgents] = useState<Loaded<readonly AgentTemplate[]>>({ kind: "loading" });
  const [strategies, setStrategies] = useState<Strategies | undefined>(undefined);
  const [strategy, setStrategy] = useState<string | undefined>(undefined);
  const [graph, setGraph] = useState<Loaded<WorkflowGraph>>({ kind: "loading" });
  const [workflow, setWorkflow] = useState<string | undefined>(() => remembered(LAST_WORKFLOW_KEY));
  const [selected, setSelected] = useState<string | undefined>(() => remembered(LAST_AGENT_KEY));
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

  const list = agents.kind === "loaded" ? agents.value : [];
  const workflows = useMemo(() => workflowsOf(list), [list]);
  const activeWorkflow = workflow !== undefined && workflows.includes(workflow) ? workflow : workflows[0];
  const agent = list.find((a) => a.name === selected && a.workflow === activeWorkflow);
  const fragments = list.filter((a) => a.workflow === activeWorkflow && a.fragment);

  const pickWorkflow = (w: string): void => {
    setWorkflow(w);
    remember(LAST_WORKFLOW_KEY, w);
  };
  const pick = (name: string): void => {
    setSelected(name);
    remember(LAST_AGENT_KEY, name);
  };

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
          <span>Workflow</span>
          <select value={activeWorkflow ?? ""} disabled={workflows.length === 0} onChange={(e) => pickWorkflow(e.target.value)} aria-label="Workflow">
            {workflows.map((w) => (
              <option key={w} value={w}>
                {w}
              </option>
            ))}
          </select>
        </label>
        <label className="picker">
          <span>Strategy</span>
          <select
            value={strategy ?? strategies?.default ?? ""}
            disabled={strategies === undefined}
            onChange={(e) => setStrategy(e.target.value)}
            aria-label="Strategy"
          >
            {(strategies?.strategies ?? []).map((s) => (
              <option key={s.name} value={s.name} title={s.describe}>
                {s.name}
                {strategyName(s.name) === strategyName(strategies?.default ?? "") ? " (default)" : ""}
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
          }}
          disabled={agents.kind === "loading"}
        >
          Refresh
        </button>
      </header>
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
          <section className="agents-pane" aria-label="Agents">
            <header className="pane-bar">
              <h2>Agents</h2>
              <span className="spacer" />
              {fragments.length > 0 && (
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
              {agents.kind === "failed" ? (
                <p className="muted plan-hint">Cannot list agent templates: {agents.message}</p>
              ) : agents.kind === "loading" ? (
                <p className="muted plan-hint">Loading agents…</p>
              ) : activeWorkflow === undefined ? (
                <p className="muted plan-hint">No agent templates.</p>
              ) : (
                <AgentGraphView agents={list} workflow={activeWorkflow} selected={agent?.name} onSelect={pick} />
              )}
            </div>
          </section>
          {graph.kind === "loaded" ? (
            <LifecyclePane graph={graph.value} role={agent?.role} />
          ) : (
            <p className="muted plan-hint lifecycle-pending">
              {graph.kind === "failed" ? `Cannot draw the lifecycle: ${graph.message}` : "Loading the lifecycle…"}
            </p>
          )}
        </div>
        <aside className="workflows-side">
          {agent !== undefined ? (
            <AgentPanel agent={agent} onSaved={onSaved} onNotice={onNotice} />
          ) : (
            <p className="muted plan-hint workflows-pick">Pick an agent in the graph to see its template and permissions.</p>
          )}
        </aside>
      </div>
    </div>
  );
}
