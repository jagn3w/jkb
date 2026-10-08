import { useCallback, useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";

import { DaemonStatus } from "./DaemonStatus";
import { DEFAULT_TAB, TABS, isTabId, tabForKey, type TabId } from "./tabs";
import { TerminalDrawer } from "./terminal/TerminalDrawer";
import { TerminalPopover } from "./terminal/TerminalPopover";
import { TerminalProvider } from "./terminal/TerminalProvider";
import { ContainerTab } from "./tabs/ContainerTab";
import { DesignTab } from "./tabs/DesignTab";
import { SessionsTab } from "./tabs/SessionsTab";
import { WorkflowsTab } from "./tabs/WorkflowsTab";

/** The last tab, a per-window convenience: the only kind of state the app keeps itself (D53.1). */
const LAST_TAB_KEY = "jkb.app.lastTab";

function loadLastTab(): TabId {
  try {
    const stored = window.localStorage.getItem(LAST_TAB_KEY);
    return isTabId(stored) ? stored : DEFAULT_TAB;
  } catch {
    return DEFAULT_TAB;
  }
}

function saveLastTab(tab: TabId): void {
  try {
    window.localStorage.setItem(LAST_TAB_KEY, tab);
  } catch {
    // Storage unavailable: the app opens on the default tab next time, which is fine.
  }
}

const PANES: Record<TabId, () => React.JSX.Element> = {
  design: DesignTab,
  workflows: WorkflowsTab,
  container: ContainerTab,
  sessions: SessionsTab,
};

export function App(): React.JSX.Element {
  const [active, setActive] = useState<TabId>(loadLastTab);
  const tabRefs = useRef(new Map<TabId, HTMLButtonElement>());

  const select = useCallback((tab: TabId, focus: boolean) => {
    setActive(tab);
    saveLastTab(tab);
    if (focus) tabRefs.current.get(tab)?.focus();
  }, []);

  // Cmd/Ctrl+1…4 from anywhere in the window.
  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey) return;
      const tab = tabForKey(active, e.key, { mod: true });
      if (tab !== undefined) {
        e.preventDefault();
        select(tab, false);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [active, select]);

  const onTabKey = (e: ReactKeyboardEvent<HTMLDivElement>): void => {
    const tab = tabForKey(active, e.key);
    if (tab !== undefined) {
      e.preventDefault();
      select(tab, true);
    }
  };

  return (
    <TerminalProvider>
      <div className="shell">
        <header className="topbar">
          <span className="wordmark">Code Factory</span>
          <div className="tablist" role="tablist" aria-label="Sections" onKeyDown={onTabKey}>
            {TABS.map((t) => (
              <button
                key={t.id}
                ref={(el) => {
                  if (el) tabRefs.current.set(t.id, el);
                  else tabRefs.current.delete(t.id);
                }}
                type="button"
                role="tab"
                id={`tab-${t.id}`}
                className="tab"
                aria-selected={active === t.id}
                aria-controls={`pane-${t.id}`}
                tabIndex={active === t.id ? 0 : -1}
                onClick={() => select(t.id, false)}
              >
                {t.label}
              </button>
            ))}
          </div>
          <DaemonStatus />
        </header>
        <main className="panes">
          {TABS.map((t) => {
            const Pane = PANES[t.id];
            // Every pane stays mounted and only the active one is shown, so a pane keeps its state
            // (an open document, a scrolled list) across tab switches.
            return (
              <section
                key={t.id}
                id={`pane-${t.id}`}
                className="pane"
                role="tabpanel"
                aria-labelledby={`tab-${t.id}`}
                data-tab={t.id}
                hidden={active !== t.id}
              >
                <Pane />
              </section>
            );
          })}
        </main>
        <TerminalDrawer />
        <TerminalPopover />
      </div>
    </TerminalProvider>
  );
}
