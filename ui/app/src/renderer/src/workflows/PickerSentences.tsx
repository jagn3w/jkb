import { strategyLabel, type Strategy } from "@jkb/core";

/**
 * One muted sentence per picker in the Workflows tab's bar: what the selected agent set is for, and
 * why to choose the selected strategy (its graph's sentence as the tooltip). Either is left out when
 * the daemon did not send it — an older `jkb serve` has no agent-set sentences — and with neither,
 * nothing is drawn.
 */
export function PickerSentences({
  agentSet,
  agentSetLine,
  strategy,
}: {
  readonly agentSet: string | undefined;
  readonly agentSetLine: string | undefined;
  readonly strategy: Strategy | undefined;
}): React.JSX.Element | null {
  if ((agentSet === undefined || agentSetLine === undefined) && strategy === undefined) return null;
  return (
    <div className="workflows-describe">
      {agentSet !== undefined && agentSetLine !== undefined && (
        <p className="muted" data-describes="agent-set">
          <span className="workflows-describe-label">{agentSet}</span> {agentSetLine}
        </p>
      )}
      {strategy !== undefined && (
        <p className="muted" data-describes="strategy" title={strategy.graph ?? undefined}>
          <span className="workflows-describe-label">{strategyLabel(strategy.name)}</span> {strategy.describe}
        </p>
      )}
    </div>
  );
}
