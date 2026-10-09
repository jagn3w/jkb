import { useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";

import { DRAWER_HEIGHT, clampHeight, drawerEntries } from "./state";
import { RestartButton, StatusNote, TargetBadge } from "./TerminalChrome";
import { useTerminals } from "./TerminalProvider";
import { TerminalView } from "./TerminalView";

/** The drawer's height, a per-window convenience (D53.1). */
const HEIGHT_KEY = "jkb.app.terminalHeight";

function loadHeight(): number {
  try {
    return Number(window.localStorage.getItem(HEIGHT_KEY) ?? DRAWER_HEIGHT.default);
  } catch {
    return DRAWER_HEIGHT.default;
  }
}

function saveHeight(height: number): void {
  try {
    window.localStorage.setItem(HEIGHT_KEY, String(height));
  } catch {
    // Storage unavailable: the drawer opens at its default height next time.
  }
}

/** The collapsible bottom drawer: a bar of terminal tabs, and the front terminal below it (D53.10). */
export function TerminalDrawer(): React.JSX.Element {
  const terminals = useTerminals();
  const { state } = terminals;
  const entries = drawerEntries(state);
  const active = entries.find((e) => e.key === state.active);
  const [height, setHeight] = useState(() => clampHeight(loadHeight(), window.innerHeight));
  const drag = useRef<{ startY: number; startHeight: number } | undefined>(undefined);

  useEffect(() => {
    const onResize = (): void => setHeight((h) => clampHeight(h, window.innerHeight));
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  const onPointerDown = useCallback(
    (e: ReactPointerEvent<HTMLDivElement>) => {
      drag.current = { startY: e.clientY, startHeight: height };
      e.currentTarget.setPointerCapture(e.pointerId);
    },
    [height],
  );
  const onPointerMove = useCallback((e: ReactPointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (d !== undefined) setHeight(clampHeight(d.startHeight + (d.startY - e.clientY), window.innerHeight));
  }, []);
  const onPointerUp = useCallback(() => {
    if (drag.current === undefined) return;
    drag.current = undefined;
    setHeight((h) => {
      saveHeight(h);
      return h;
    });
  }, []);

  return (
    <section className="terminal-drawer" aria-label="Terminal" data-open={state.drawerOpen}>
      {state.drawerOpen && (
        <div
          className="drawer-resize"
          role="separator"
          aria-orientation="horizontal"
          aria-label="Resize the terminal"
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
        />
      )}
      <div className="drawer-bar">
        <button
          type="button"
          className="drawer-toggle"
          aria-expanded={state.drawerOpen}
          aria-controls="terminal-drawer-body"
          title="Show or hide the terminal (Ctrl+`)"
          onClick={terminals.toggleDrawer}
        >
          <span className="chevron" aria-hidden="true">
            {state.drawerOpen ? "▾" : "▸"}
          </span>
          Terminal
          {entries.length > 0 && <span className="count">{entries.length}</span>}
        </button>
        <div className="terminal-tabs" role="tablist" aria-label="Terminals">
          {entries.map((e) => (
            <div
              key={e.key}
              className="terminal-tab"
              role="tab"
              id={`terminal-tab-${e.key}`}
              aria-selected={state.drawerOpen && e.key === state.active}
              aria-controls={`terminal-panel-${e.key}`}
              tabIndex={e.key === state.active ? 0 : -1}
              onClick={() => terminals.select(e.key)}
              onKeyDown={(k) => {
                // Only the tab's own keys: Enter on its close button must still press that button.
                if (k.target === k.currentTarget && (k.key === "Enter" || k.key === " ")) {
                  k.preventDefault();
                  terminals.select(e.key);
                }
              }}
            >
              <span className="terminal-title">{e.spec.title}</span>
              <TargetBadge target={e.spec.target} />
              <StatusNote entry={e} />
              <button
                type="button"
                className="terminal-close"
                aria-label={`Close ${e.spec.title}`}
                title="Close (signals its program to end; in the container, from inside it)"
                onClick={(ev) => {
                  ev.stopPropagation();
                  terminals.close(e.key);
                }}
              >
                ×
              </button>
            </div>
          ))}
        </div>
        <button
          type="button"
          className="terminal-action"
          aria-label="New terminal"
          title={`New terminal in the container${terminals.roots === undefined ? "" : ` ${terminals.roots.container}`}`}
          disabled={terminals.roots === undefined}
          onClick={() => terminals.openShell()}
        >
          +
        </button>
        <button
          type="button"
          className="terminal-action"
          aria-label="New host terminal"
          title="New login shell on this machine, outside the container: whatever is typed into it runs with your account's full access"
          disabled={terminals.roots === undefined}
          onClick={() => terminals.openShell("host")}
        >
          + host
        </button>
        {state.drawerOpen && active !== undefined && (
          <>
            <RestartButton entry={active} />
          </>
        )}
      </div>
      <div id="terminal-drawer-body" className="drawer-body" hidden={!state.drawerOpen} style={{ height }}>
        {entries.length === 0 && (
          <p className="drawer-empty muted">No terminals open. + starts a shell in the container.</p>
        )}
        {entries.map((e) => (
          <div
            key={e.key}
            id={`terminal-panel-${e.key}`}
            className="terminal-panel"
            role="tabpanel"
            aria-labelledby={`terminal-tab-${e.key}`}
            hidden={e.key !== state.active}
          >
            <TerminalView session={terminals.session(e.key)} visible={state.drawerOpen && e.key === state.active} />
          </div>
        ))}
      </div>
    </section>
  );
}
