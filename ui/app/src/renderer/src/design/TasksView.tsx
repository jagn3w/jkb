import { useEffect, useState } from "react";

import { isTerminal, visibleTasks, type PlanTask } from "@jkb/core";

import { StrategyPicker, TaskRow } from "./PlansView";
import { TaskPane } from "./TaskPane";
import type { Plans } from "./usePlans";

/**
 * The Tasks tab: every task under the design's plans, grouped by plan and step, and its one-off
 * tasks, beside the Tasks pane for the one chosen (`task.show` and `task.edit`, and *Play*).
 * Finished work is folded away unless asked for, since a long-lived design's done tasks would
 * otherwise bury the open ones.
 */
export function TasksView({
  plans,
  selected,
  onSelect,
  onNotice,
}: {
  readonly plans: Plans;
  readonly selected: string | undefined;
  readonly onSelect: (uid: string) => void;
  readonly onNotice: (message: string) => void;
}): React.JSX.Element {
  const [showDone, setShowDone] = useState(false);
  const { listing, live, archived, oneOff, tasks, busy } = plans;
  const task = tasks.find((t) => t.uid === selected);
  // A finished task chosen elsewhere (the Plans tab's history) is listed, not shown with no row.
  const chosenFinished = task !== undefined && isTerminal(task.status);
  useEffect(() => {
    if (chosenFinished) setShowDone(true);
  }, [chosenFinished, selected]);
  const shown = (t: PlanTask): boolean => showDone || !isTerminal(t.status);
  const groups = (showDone ? [...live, ...archived] : live).flatMap((plan) =>
    plan.steps
      .map((step) => ({ key: step.uid, plan: plan.title, step: step.text, lines: visibleTasks(step.tasks, shown) }))
      .filter((g) => g.lines.length > 0),
  );
  const loose = visibleTasks(oneOff, shown);
  const hidden = tasks.length - tasks.filter(shown).length;

  return (
    <section className="view tasks-view" aria-label="Tasks">
      <div className="tasks-list">
        <header className="pane-bar">
          <h2>Tasks</h2>
          <span className="spacer" />
          <label className="toggle">
            <input type="checkbox" checked={showDone} onChange={(e) => setShowDone(e.target.checked)} />
            <span>Finished{hidden > 0 && !showDone ? ` (${hidden})` : ""}</span>
          </label>
          <button type="button" className="bar-button" onClick={() => void plans.load()} aria-label="Refresh tasks">
            ↻
          </button>
        </header>
        <div className="view-body">
          {listing.kind === "failed" ? (
            <p className="muted plan-hint">Cannot list tasks: {listing.message}</p>
          ) : listing.kind === "loading" ? (
            <p className="muted plan-hint">Loading tasks…</p>
          ) : groups.length === 0 && loose.length === 0 ? (
            <p className="muted plan-hint">{tasks.length === 0 ? "No tasks yet." : "No open tasks."}</p>
          ) : (
            <>
              {groups.map((g) => (
                <div key={g.key} className="task-group">
                  <h3>
                    <span className="muted">{g.plan} ›</span> {g.step}
                  </h3>
                  <ul className="plan-tasks">
                    {g.lines.map(({ task: t, muted }) => (
                      <TaskRow key={t.uid} task={t} muted={muted} selected={t.uid === selected} onSelect={onSelect} />
                    ))}
                  </ul>
                </div>
              ))}
              {loose.length > 0 && (
                <div className="task-group">
                  <h3>One-off tasks</h3>
                  <ul className="plan-tasks">
                    {loose.map(({ task: t, muted }) => (
                      <TaskRow key={t.uid} task={t} muted={muted} selected={t.uid === selected} onSelect={onSelect} />
                    ))}
                  </ul>
                </div>
              )}
            </>
          )}
        </div>
      </div>
      <div className="tasks-detail">
        <header className="pane-bar">
          <span className="spacer" />
          <StrategyPicker plans={plans} />
        </header>
        <TaskPane
          key={task?.uid ?? "none"}
          task={task}
          pick={plans.pick}
          busy={busy}
          onPlay={(t) => void plans.play("task", t.uid)}
          onChanged={() => void plans.load()}
          onNotice={onNotice}
        />
      </div>
    </section>
  );
}
