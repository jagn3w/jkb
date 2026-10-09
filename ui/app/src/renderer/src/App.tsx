import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";

import { DaemonStatus } from "./DaemonStatus";
import { NavigationContext, type DesignRequest, type Navigation } from "./navigation";
import { NeedsInputProvider, useNeedsInput } from "./sessions/NeedsInputProvider";
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
  return (
    <TerminalProvider>
      <NeedsInputProvider>
        <Shell />
      </NeedsInputProvider>
    </TerminalProvider>
  );
}

/** The id of a tab's dot, which describes the tab (its name stays the tab's label). */
const dotId = (tab: TabId): string => `tab-${tab}-needs`;

/**
 * The red dot on the Sessions tab: sessions whose notification awaits the operator (D53.9). Hidden
 * from the tab's name, so the tab is still called "Sessions", and read as its description instead.
 */
function TabDot({ tab }: { readonly tab: TabId }): React.JSX.Element | null {
  const { needing } = useNeedsInput();
  if (tab !== "sessions" || needing.size === 0) return null;
  const said = needing.size === 1 ? "1 session needs input" : `${needing.size} sessions need input`;
  return <span id={dotId(tab)} className="needs-dot" aria-hidden="true" aria-label={said} title={said} />;
}

function Shell(): React.JSX.Element {
  const [active, setActive] = useState<TabId>(loadLastTab);
  const [designRequest, setDesignRequest] = useState<DesignRequest | undefined>(undefined);
  const { needing } = useNeedsInput();
  const tabRefs = useRef(new Map<TabId, HTMLButtonElement>());

  const select = useCallback((tab: TabId, focus: boolean) => {
    setActive(tab);
    saveLastTab(tab);
    if (focus) tabRefs.current.get(tab)?.focus();
  }, []);

  const navigation = useMemo<Navigation>(
    () => ({
      goTo: (tab) => select(tab, false),
      openDesign: (uid) => {
        setDesignRequest((r) => ({ uid, seq: (r?.seq ?? 0) + 1 }));
        select("design", false);
      },
      designRequest,
    }),
    [select, designRequest],
  );

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
    <NavigationContext.Provider value={navigation}>
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
                aria-describedby={t.id === "sessions" && needing.size > 0 ? dotId(t.id) : undefined}
                tabIndex={active === t.id ? 0 : -1}
                onClick={() => select(t.id, false)}
              >
                {t.label}
                <TabDot tab={t.id} />
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
    </NavigationContext.Provider>
  );
}
