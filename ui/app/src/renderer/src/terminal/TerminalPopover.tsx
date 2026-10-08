import { RestartButton, StatusNote, TargetBadge, TargetToggle } from "./TerminalChrome";
import { useTerminals } from "./TerminalProvider";
import { TerminalView } from "./TerminalView";

/**
 * The popover: one terminal floating over the tab that opened it (D53.10 — *Discuss* opens one
 * beside the text it is about). It can be moved to the drawer, which keeps its program and its
 * scrollback, or closed, which ends it.
 */
export function TerminalPopover(): React.JSX.Element | null {
  const terminals = useTerminals();
  const entry = terminals.state.entries.find((e) => e.key === terminals.state.popover);
  if (entry === undefined) return null;
  return (
    <div className="terminal-popover" role="dialog" aria-label={entry.spec.title}>
      <header className="popover-bar">
        <span className="terminal-title">{entry.spec.title}</span>
        <TargetBadge target={entry.spec.target} />
        <StatusNote entry={entry} />
        <span className="spacer" />
        <RestartButton entry={entry} />
        <TargetToggle entry={entry} />
        <button type="button" className="terminal-action" onClick={() => terminals.toDrawer(entry.key)}>
          Move to drawer
        </button>
        <button
          type="button"
          className="terminal-close"
          aria-label={`Close ${entry.spec.title}`}
          title="Close (ends its program)"
          onClick={() => terminals.close(entry.key)}
        >
          ×
        </button>
      </header>
      <div className="popover-body">
        <TerminalView session={terminals.session(entry.key)} visible />
      </div>
    </div>
  );
}
