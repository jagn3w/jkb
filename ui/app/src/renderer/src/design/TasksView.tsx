import { useEffect, useState } from "react";

import { isTerminal, nestFindings, visibleTasks, type PlanTask, type RepoNode } from "@jkb/core";

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
  repo,
  designs,
  onMoved,
}: {
  readonly plans: Plans;
  readonly selected: string | undefined;
  readonly onSelect: (uid: string) => void;
  readonly onNotice: (message: string) => void;
  /** The design's repo: Move to step's Inbox is this repo's. */
  readonly repo: string | undefined;
  /** The repo's designs as the sidebar shows them, for Move to step's design choice. */
  readonly designs: RepoNode | undefined;
  /** After a Move to step: what else lists tasks (the sidebar's unplanned counts) is read again. */
  readonly onMoved: () => void;
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
      .map((step) => ({ key: step.uid, plan: plan.title, step: step.text, lines: nestFindings(visibleTasks(step.tasks, shown)) }))
      .filter((g) => g.lines.length > 0),
  );
  const loose = nestFindings(visibleTasks(oneOff, shown));
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
                    {g.lines.map(({ task: t, muted, findingOf, from }) => (
                      <TaskRow
                        key={t.uid}
                        task={t}
                        muted={muted}
                        finding={findingOf}
                        from={from}
                        selected={t.uid === selected}
                        onSelect={onSelect}
                      />
                    ))}
                  </ul>
                </div>
              ))}
              {loose.length > 0 && (
                <div className="task-group">
                  <h3>One-off tasks</h3>
                  <ul className="plan-tasks">
                    {loose.map(({ task: t, muted, findingOf, from }) => (
                      <TaskRow
                        key={t.uid}
                        task={t}
                        muted={muted}
                        finding={findingOf}
                        from={from}
                        selected={t.uid === selected}
                        onSelect={onSelect}
                      />
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
          repo={repo}
          designs={designs}
          listed={(uid) => tasks.some((t) => t.uid === uid)}
          onSelect={onSelect}
          onMoved={onMoved}
        />
      </div>
    </section>
  );
}
