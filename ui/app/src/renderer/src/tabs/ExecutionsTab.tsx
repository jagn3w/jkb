import { TimelinePane } from "../executions/TimelinePane";

/**
 * The Executions tab (the Executions design): the operator's view of sessions at work. Its first
 * section is the lifecycle timeline — which sessions hold which tasks and which have stalled, each
 * task's path through its strategy, each plan's progress — read from facts jkb already records.
 *
 * The body is a column of sections, each its own region under the one bar, so the run-request
 * queue (the design's second job) is a second section beside the timeline rather than a new tab.
 */
export function ExecutionsTab(): React.JSX.Element {
  return (
    <div className="executions-tab">
      <header className="design-bar">
        <h1>Executions</h1>
      </header>
      <div className="executions-body">
        <TimelinePane />
      </div>
    </div>
  );
}
