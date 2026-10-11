import { useState } from "react";

import { strategyName, type ExecPlan, type PlanTask } from "@jkb/core";

import type { Plans } from "./usePlans";

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

/** A task in a list: its status, title and claim; choosing it opens it in the Tasks tab. */
export function TaskRow({
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

/** The strategy a *Play* pins its tasks to, shared by the Plans and Tasks tabs. */
export function StrategyPicker({ plans }: { readonly plans: Plans }): React.JSX.Element {
  const { strategies, strategy, setStrategy } = plans;
  return (
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
  );
}

/**
 * The Plans tab (D53.6): the design's live execution plans, their steps, the spans staged into each
 * and the tasks under it, with *Play*, and the archived plans in a history drawer. Choosing a task
 * opens it in the Tasks tab.
 */
export function PlansView({
  design,
  plans,
  onOpenTask,
}: {
  readonly design: string;
  readonly plans: Plans;
  readonly onOpenTask: (uid: string) => void;
}): React.JSX.Element {
  const [history, setHistory] = useState(false);
  const { listing, live, archived, busy } = plans;
  return (
    <section className="view plans-view" aria-label="Execution plans">
      <header className="pane-bar">
        <h2>Execution plans</h2>
        <span className="spacer" />
        <StrategyPicker plans={plans} />
        <button type="button" className="bar-button" onClick={() => void plans.load()} aria-label="Refresh plans">
          ↻
        </button>
      </header>
      <div className="view-body">
        {listing.kind === "failed" ? (
          <p className="muted plan-hint">Cannot list plans: {listing.message}</p>
        ) : listing.kind === "loading" ? (
          <p className="muted plan-hint">Loading plans…</p>
        ) : live.length === 0 ? (
          <p className="muted plan-hint">
            No live plans. Create one with{" "}
            <code>jkb design plan create {design} &lt;title&gt;</code>.
          </p>
        ) : (
          live.map((p) => (
            <PlanCard
              key={p.uid}
              plan={p}
              selectedTask={undefined}
              onSelect={onOpenTask}
              busy={busy}
              onPlay={(plan) => void plans.play("plan", plan.uid)}
            />
          ))
        )}
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
                <PlanCard key={p.uid} plan={p} selectedTask={undefined} onSelect={onOpenTask} busy={busy} />
              ))}
            </div>
          )}
        </footer>
      </div>
    </section>
  );
}
