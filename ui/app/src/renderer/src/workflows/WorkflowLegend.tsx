import { useState } from "react";

import { WORKFLOW_TERMS, strategyChanges, type WorkflowGraph } from "@jkb/core";

/** Whether the legend is folded, per window (D53.1's per-window conveniences): open until folded. */
const LEGEND_KEY = "jkb.app.workflows.legend";

function rememberedOpen(): boolean {
  try {
    return window.localStorage.getItem(LEGEND_KEY) !== "collapsed";
  } catch {
    return true;
  }
}

function rememberOpen(open: boolean): void {
  try {
    window.localStorage.setItem(LEGEND_KEY, open ? "expanded" : "collapsed");
  } catch {
    // Storage unavailable: the legend opens expanded next time.
  }
}

/**
 * The Workflows tab's legend, under the pickers' sentences: the model's words in one line each
 * (`WORKFLOW_TERMS`), then what the drawn strategy changes from the default, in jkb's own lines.
 * That half is left out while the machines load, and for a daemon that predates the field.
 * Collapsible; whether it is folded is remembered per window.
 */
export function WorkflowLegend({ graph }: { readonly graph: WorkflowGraph | undefined }): React.JSX.Element {
  const [open, setOpen] = useState(rememberedOpen);
  const changes = graph === undefined ? { kind: "unknown" as const } : strategyChanges(graph);
  const toggle = (): void => {
    setOpen(!open);
    rememberOpen(!open);
  };
  return (
    <section className="workflows-legend" aria-label="Legend">
      <button type="button" className="link-button workflows-legend-toggle" aria-expanded={open} aria-controls="workflows-legend-body" onClick={toggle}>
        <span aria-hidden="true">{open ? "▾ " : "▸ "}</span>
        Legend
      </button>
      {open && (
        <div id="workflows-legend-body" className="workflows-legend-body">
          <dl className="workflows-legend-terms">
            {WORKFLOW_TERMS.map(({ term, line }) => (
              <div key={term}>
                <dt>{term}</dt>
                <dd className="muted">{line}</dd>
              </div>
            ))}
          </dl>
          {changes.kind === "default" && (
            <p className="workflows-legend-changes" data-changes="default">
              <span className="workflows-describe-label">{changes.label}</span> is the default strategy.
            </p>
          )}
          {changes.kind === "same" && (
            <p className="workflows-legend-changes" data-changes="same">
              <span className="workflows-describe-label">{changes.label}</span> grants and checks the same as the default,{" "}
              <span className="workflows-describe-label">{changes.defaultLabel}</span>.
            </p>
          )}
          {changes.kind === "changes" && (
            <div className="workflows-legend-changes" data-changes="changes">
              <p>
                What <span className="workflows-describe-label">{changes.label}</span> changes from the default (
                <span className="workflows-describe-label">{changes.defaultLabel}</span>):
              </p>
              <ul>
                {changes.lines.map((line) => (
                  <li key={line}>{line}</li>
                ))}
              </ul>
            </div>
          )}
        </div>
      )}
    </section>
  );
}
