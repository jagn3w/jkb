import { NO_REPO_LABEL, unplannedOf, type PlanTask, type RepoNode, type UnplannedList } from "@jkb/core";

import { TaskRow } from "./PlansView";
import { TaskPane } from "./TaskPane";

/**
 * A repo's unplanned tasks (`task.unplanned`), opened from the sidebar's Unplanned fold in place of
 * a design: the list, and the Tasks pane for the one chosen, whose *Move to step* files it under a
 * plan step. Tasks in no repo (`repo` `null`) are offered only design steps: there is no Inbox to
 * send them to.
 */
export function UnplannedView({
  list,
  repo,
  selected,
  designs,
  onSelect,
  onMoved,
  onNotice,
}: {
  readonly list: UnplannedList | undefined;
  readonly repo: string | null;
  readonly selected: string | undefined;
  /** The designs Move to step offers: the repo's, or every repo's for the Elsewhere row. */
  readonly designs: readonly RepoNode[];
  readonly onSelect: (uid: string) => void;
  /** After a move: the unplanned list is read again. */
  readonly onMoved: () => void;
  readonly onNotice: (message: string) => void;
}): React.JSX.Element {
  const entry = unplannedOf(list, repo);
  const tasks: readonly PlanTask[] = entry?.tasks.map((t) => t.task) ?? [];
  const task = tasks.find((t) => t.uid === selected);
  const name = repo ?? NO_REPO_LABEL;
  return (
    <section className="view tasks-view unplanned-view" aria-label={`Unplanned tasks of ${name}`}>
      <div className="tasks-list">
        <header className="pane-bar">
          <h2>Unplanned · {name}</h2>
          <span className="spacer" />
          <button type="button" className="bar-button" onClick={onMoved} aria-label="Refresh unplanned tasks">
            ↻
          </button>
        </header>
        <div className="view-body">
          {list === undefined ? (
            <p className="muted plan-hint">Loading unplanned tasks…</p>
          ) : entry === undefined ? (
            <p className="muted plan-hint">No unplanned tasks: every open task here is under a plan step.</p>
          ) : (
            <>
              <p className="muted plan-hint">
                Open tasks no plan step reaches. Choose one, then <strong>Move to step</strong> to plan it.
                {entry.count > entry.tasks.length ? ` The first ${entry.tasks.length} of ${entry.count}.` : ""}
              </p>
              <ul className="plan-tasks">
                {entry.tasks.map(({ task: t }) => (
                  <TaskRow key={t.uid} task={t} selected={t.uid === selected} onSelect={onSelect} />
                ))}
              </ul>
            </>
          )}
        </div>
      </div>
      <div className="tasks-detail">
        <TaskPane
          key={task?.uid ?? "none"}
          task={task}
          pick={undefined}
          busy={false}
          onPlay={() => onNotice("Plan the task first: Play runs a task from its design's Tasks tab.")}
          onChanged={() => undefined}
          onNotice={onNotice}
          repo={repo ?? undefined}
          designs={designs}
          listed={(uid) => tasks.some((t) => t.uid === uid)}
          onSelect={onSelect}
          onMoved={onMoved}
        />
      </div>
    </section>
  );
}
