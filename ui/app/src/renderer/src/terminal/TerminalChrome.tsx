//! The small pieces the drawer and the popover share: where a terminal runs (fixed when it opens,
//! D53.10), how it stands, and its Restart.

import { targetLabel, type TerminalTarget } from "../../../shared/terminal";
import { statusLabel, type TerminalEntry } from "./state";
import { useTerminals } from "./TerminalProvider";

/** Where the terminal's commands run, on its tab, so it is never ambiguous (D53.10). */
export function TargetBadge({ target }: { readonly target: TerminalTarget }): React.JSX.Element {
  return (
    <span className="target-badge" data-target={target} title={target === "host" ? "Runs on this machine, outside the container" : "Runs in the dev container"}>
      {targetLabel(target)}
    </span>
  );
}

export function StatusNote({ entry }: { readonly entry: TerminalEntry }): React.JSX.Element | null {
  const label = statusLabel(entry.status);
  if (label === "") return null;
  const failed = entry.status.kind === "failed" || (entry.status.kind === "exited" && entry.status.exitCode !== 0);
  return (
    <span className="terminal-status" data-failed={failed || undefined} title={entry.status.kind === "failed" ? entry.status.error : undefined}>
      {label}
    </span>
  );
}

/** Restart, shown once the program has ended. */
export function RestartButton({ entry }: { readonly entry: TerminalEntry }): React.JSX.Element | null {
  const { restart } = useTerminals();
  if (entry.status.kind !== "exited" && entry.status.kind !== "failed") return null;
  return (
    <button type="button" className="terminal-action" onClick={() => restart(entry.key)}>
      Restart
    </button>
  );
}
