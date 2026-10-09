import { useCallback, useEffect, useMemo, useState } from "react";

import {
  decodePlanList,
  decodeStrategies,
  partitionPlans,
  pickOf,
  planOps,
  planTasks,
  strategyName,
  type ExecPlan,
  type PlanList,
  type PlanTask,
  type Strategies,
} from "@jkb/core";

import { useTerminals } from "../terminal/TerminalProvider";
import { pinThenPrompt, planTarget, playPlanSpec, playTaskSpec, taskTarget } from "./play";
import { TaskPane } from "./TaskPane";

type Listing =
  | { readonly kind: "loading" }
  | { readonly kind: "loaded"; readonly list: PlanList }
  | { readonly kind: "failed"; readonly message: string };

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

function StepSpans({ spans }: { readonly spans: ExecPlan["steps"][number]["spans"] }): React.JSX.Element | null {
  if (spans.length === 0) return null;
  return (
    <ul className="step-spans" aria-label="Staged spans">
      {spans.map((s) => (
        <li key={s.uid} data-state={s.state} title={`${s.uid} · ${s.state}`}>
          {s.text.length > 80 ? `${s.text.slice(0, 79)}…` : s.text}
        </li>
      ))}
    </ul>
  );
}

function TaskRow({
  task,
  selected,
  onSelect,
}: {
  readonly task: PlanTask;
  readonly selected: boolean;
  readonly onSelect: (uid: string) => void;
}): React.JSX.Element {
  return (
    <li>
      <button
        type="button"
        className="plan-task"
        aria-pressed={selected}
        style={{ paddingLeft: `calc(var(--space-2) + ${task.depth} * var(--space-3))` }}
        onClick={() => onSelect(task.uid)}
        title={task.uid}
      >
        <span className="task-status" data-status={task.status ?? "unknown"}>
          {task.status ?? "?"}
        </span>
        <span className="task-title">{task.title}</span>
        {task.claimed_by !== null && <span className="task-claim">claimed</span>}
      </button>
    </li>
  );
}

function PlanCard({
  plan,
  selectedTask,
  onSelect,
  onPlay,
  busy,
}: {
  readonly plan: ExecPlan;
  readonly selectedTask: string | undefined;
  readonly onSelect: (uid: string) => void;
  readonly onPlay?: (plan: ExecPlan) => void;
  readonly busy: boolean;
}): React.JSX.Element {
  return (
    <article className="plan-card" data-archived={plan.archived}>
      <header>
        <h3 title={plan.uid}>{plan.title}</h3>
        {onPlay !== undefined && (
          <button type="button" className="bar-button play-button" disabled={busy} onClick={() => onPlay(plan)}>
            ▶ Play
          </button>
        )}
      </header>
      {plan.steps.length === 0 ? (
        <p className="muted plan-hint">
          No steps — <code>jkb design plan step {plan.uid} &lt;text&gt;</code>
        </p>
      ) : (
        <ol className="plan-steps">
          {plan.steps.map((step) => (
            <li key={step.uid}>
              <div className="step-text" title={step.uid}>
                {step.text}
              </div>
              <StepSpans spans={step.spans} />
              {step.tasks.length === 0 ? (
                <p className="muted plan-hint">No tasks yet.</p>
              ) : (
                <ul className="plan-tasks">
                  {step.tasks.map((t) => (
                    <TaskRow key={t.uid} task={t} selected={t.uid === selectedTask} onSelect={onSelect} />
                  ))}
                </ul>
              )}
            </li>
          ))}
        </ol>
      )}
    </article>
  );
}

/**
 * The Design tab's right-hand column (D53.6): the Execution Plan pane — the design's live plans,
 * their steps, staged spans and tasks, with *Play* — the history drawer of archived plans, and the
 * Tasks pane for the task selected, then any panes the tab puts below (the Prompts pane). Every
 * read and write is an op; the strategy picker chooses what a *Play* pins its tasks to before Claude
 * starts — only when the operator picked one: left on "each task's own", nothing is pinned and
 * unpinned tasks keep following the default. A pick is held by its bare name, the one identity
 * `workflow.set` resolves; the strategies are re-read with the plans, and again by *Play* itself.
 */
export function PlanColumn({
  design,
  repo,
  onNotice,
  children,
}: {
  readonly design: string;
  readonly repo: string;
  readonly onNotice: (message: string) => void;
  /** Panes below the Tasks pane in the same column (the Prompts pane). */
  readonly children?: React.ReactNode;
}): React.JSX.Element {
  const terminals = useTerminals();
  const [listing, setListing] = useState<Listing>({ kind: "loading" });
  const [strategies, setStrategies] = useState<Strategies | undefined>(undefined);
  /** The operator's explicit pick, by bare name; `undefined` is "each task's own". */
  const [strategy, setStrategy] = useState<string | undefined>(undefined);
  const [selected, setSelected] = useState<string | undefined>(undefined);
  const [history, setHistory] = useState(false);
  const [busy, setBusy] = useState(false);

  const loadStrategies = useCallback(async () => {
    const answer = decodeStrategies(await op(planOps.strategies()));
    if (!answer.ok) return;
    setStrategies(answer.value);
    // A pick no longer listed (its definition gone) falls back to "each task's own", never to a
    // value the picker cannot show.
    setStrategy((s) => (pickOf(answer.value, s) === undefined ? undefined : s));
  }, []);

  const load = useCallback(async () => {
    setListing((l) => (l.kind === "loaded" ? l : { kind: "loading" }));
    const [answer] = await Promise.all([op(planOps.plans(design)).then(decodePlanList), loadStrategies()]);
    setListing(answer.ok ? { kind: "loaded", list: answer.value } : { kind: "failed", message: answer.error.message });
  }, [design, loadStrategies]);

  useEffect(() => {
    setListing({ kind: "loading" });
    setSelected(undefined);
    void load();
  }, [load]);

  const list = listing.kind === "loaded" ? listing.list : undefined;
  const { live, archived } = useMemo(() => partitionPlans(list?.plans ?? []), [list]);
  const allTasks = useMemo(
    () => [...(list?.plans ?? []).flatMap(planTasks), ...(list?.tasks ?? [])],
    [list],
  );
  const task = allTasks.find((t) => t.uid === selected);
  const pick = pickOf(strategies, strategy);

  const play = useCallback(
    async (what: "plan" | "task", uid: string) => {
      const roots = terminals.roots;
      if (roots === undefined) {
        onNotice("The terminal is not ready yet.");
        return;
      }
      setBusy(true);
      try {
        const request = what === "plan" ? planOps.play(uid, strategy) : planOps.playTask(uid);
        const target = what === "plan" ? planTarget(design, uid) : taskTarget(design, uid);
        const answer = await pinThenPrompt(op, target, strategy, request);
        if (!answer.ok) {
          onNotice(answer.error.message);
          return;
        }
        const spec = (what === "plan" ? playPlanSpec : playTaskSpec)(answer.value, design, repo, roots, crypto.randomUUID());
        terminals.open(spec, "drawer");
      } finally {
        setBusy(false);
        void load();
      }
    },
    [terminals, strategy, design, repo, onNotice, load],
  );

  return (
    <aside className="plan-column" aria-label="Execution plans and tasks">
      <section className="plan-pane">
        <header className="pane-bar">
          <h2>Execution plans</h2>
          <span className="spacer" />
          <label className="picker" title="The workflow strategy a Play pins its tasks to">
            <span>Strategy</span>
            <select
              value={strategy ?? ""}
              disabled={strategies === undefined}
              onChange={(e) => setStrategy(e.target.value === "" ? undefined : e.target.value)}
              aria-label="Strategy"
            >
              <option value="" title="Pins nothing: each task keeps its own strategy, the default when unpinned">
                Each task's own{strategies === undefined ? "" : ` (default ${strategies.default})`}
              </option>
              {(strategies?.strategies ?? []).map((s) => (
                <option key={s.name} value={strategyName(s.name)} title={s.describe}>
                  {s.name}
                  {s.name === strategies?.default ? " (default)" : ""}
                </option>
              ))}
            </select>
          </label>
          <button type="button" className="bar-button" onClick={() => void load()} aria-label="Refresh plans">
            ↻
          </button>
        </header>
        <div className="pane-body">
          {listing.kind === "failed" ? (
            <p className="muted plan-hint">Cannot list plans: {listing.message}</p>
          ) : listing.kind === "loading" ? (
            <p className="muted plan-hint">Loading plans…</p>
          ) : live.length === 0 ? (
            <p className="muted plan-hint">
              No live plans. Create one with{" "}
              <code>
                jkb design plan create {design} &lt;title&gt; --step &lt;text&gt;
              </code>
              .
            </p>
          ) : (
            live.map((p) => (
              <PlanCard
                key={p.uid}
                plan={p}
                selectedTask={selected}
                onSelect={setSelected}
                busy={busy}
                onPlay={(plan) => void play("plan", plan.uid)}
              />
            ))
          )}
          {list !== undefined && list.tasks.length > 0 && (
            <div className="plan-card">
              <header>
                <h3>One-off tasks</h3>
              </header>
              <ul className="plan-tasks">
                {list.tasks.map((t) => (
                  <TaskRow key={t.uid} task={t} selected={t.uid === selected} onSelect={setSelected} />
                ))}
              </ul>
            </div>
          )}
        </div>
        <footer className="history">
          <button
            type="button"
            className="bar-button"
            aria-expanded={history}
            disabled={archived.length === 0}
            onClick={() => setHistory((h) => !h)}
          >
            History · {archived.length} archived
          </button>
          {history && archived.length > 0 && (
            <div className="history-drawer" aria-label="Archived plans">
              {archived.map((p) => (
                <PlanCard key={p.uid} plan={p} selectedTask={selected} onSelect={setSelected} busy={busy} />
              ))}
            </div>
          )}
        </footer>
      </section>
      <TaskPane
        key={task?.uid ?? "none"}
        task={task}
        pick={pick}
        busy={busy}
        onPlay={(t) => void play("task", t.uid)}
        onChanged={() => void load()}
        onNotice={onNotice}
      />
      {children}
    </aside>
  );
}
