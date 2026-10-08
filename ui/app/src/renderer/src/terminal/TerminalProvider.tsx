import { createContext, useCallback, useContext, useEffect, useMemo, useReducer, useRef, useState } from "react";

import {
  DEFAULT_TARGET,
  defaultCwd,
  retarget as retargetSpec,
  targetLabel,
  type TerminalRoots,
  type TerminalSpec,
  type TerminalTarget,
} from "../../../shared/terminal";
import { TerminalEventRouter } from "./router";
import { TerminalSession, themeFromTokens } from "./session";
import { INITIAL_STATE, findSession, reduce, type Placement, type TerminalsState } from "./state";

/**
 * The integrated terminal, as every tab sees it (D53.10). A caller builds a `TerminalSpec` (from
 * CLI output, usually) and opens it in the drawer or the popover; the terminal knows nothing of
 * Claude or jkb.
 */
export interface TerminalsApi {
  readonly state: TerminalsState;
  /** Where terminals run, once main has said; `undefined` until then. */
  readonly roots: TerminalRoots | undefined;
  /**
   * Open a terminal from `spec`. A spec naming a `sessionUuid` that an open terminal already
   * runs shows that terminal instead of starting a second. Returns the terminal's key.
   */
  open(spec: TerminalSpec, placement?: Placement): number;
  /** Open the target's login shell in its default directory (the drawer's "+"). */
  openShell(target?: TerminalTarget): void;
  close(key: number): void;
  select(key: number): void;
  toDrawer(key: number): void;
  setDrawer(open: boolean): void;
  toggleDrawer(): void;
  /** Run the terminal's spec again (after it exited, say). */
  restart(key: number): void;
  /** Move the terminal to the other target: its program starts again there. */
  retarget(key: number, target: TerminalTarget): void;
  session(key: number): TerminalSession | undefined;
}

const TerminalsContext = createContext<TerminalsApi | undefined>(undefined);

export function useTerminals(): TerminalsApi {
  const api = useContext(TerminalsContext);
  if (api === undefined) throw new Error("useTerminals outside <TerminalProvider>");
  return api;
}

/** Whether `e` is the drawer's shortcut, Ctrl+` (VS Code's, and free on macOS where Cmd+` is not). */
export function isDrawerShortcut(e: { key: string; ctrlKey: boolean; metaKey: boolean; altKey: boolean; shiftKey: boolean }): boolean {
  return e.key === "`" && e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey;
}

export function TerminalProvider({ children }: { readonly children: React.ReactNode }): React.JSX.Element {
  const [state, dispatch] = useReducer(reduce, INITIAL_STATE);
  const [roots, setRoots] = useState<TerminalRoots | undefined>(undefined);
  const stateRef = useRef(state);
  stateRef.current = state;
  const sessions = useRef(new Map<number, TerminalSession>());
  const router = useRef<TerminalEventRouter | undefined>(undefined);
  const nextKey = useRef(1);

  // The bridge is reached only from effects and handlers, never during render.
  useEffect(() => {
    const r = new TerminalEventRouter((listener) => window.jkb.terminal.onEvent(listener));
    router.current = r;
    let live = true;
    window.jkb.info().then(
      (info) => {
        if (live) setRoots(info.terminal);
      },
      () => undefined,
    );
    const all = sessions.current;
    return () => {
      live = false;
      for (const s of all.values()) s.dispose();
      all.clear();
      r.dispose();
      router.current = undefined;
    };
  }, []);

  // The terminal follows the app between light and dark.
  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = (): void => {
      const theme = themeFromTokens();
      for (const s of sessions.current.values()) s.setTheme(theme);
    };
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }, []);

  const open = useCallback((spec: TerminalSpec, placement: Placement = "drawer"): number => {
    const existing = findSession(stateRef.current, spec.sessionUuid);
    if (existing !== undefined) {
      if (existing.placement === "drawer") dispatch({ type: "select", key: existing.key });
      sessions.current.get(existing.key)?.focus();
      return existing.key;
    }
    const r = router.current;
    if (r === undefined) throw new Error("the terminal is not ready");
    const key = nextKey.current++;
    const session = new TerminalSession(r, (status) => dispatch({ type: "status", key, status }));
    sessions.current.set(key, session);
    dispatch({ type: "open", key, spec, placement });
    void session.start(spec);
    return key;
  }, []);

  const openShell = useCallback(
    (target: TerminalTarget = DEFAULT_TARGET): void => {
      if (roots === undefined) return;
      // Titled for what it runs, not where: the toggle can move it, and the badge says where.
      open({ target, cwd: defaultCwd(target, roots), argv: [], title: "shell" });
    },
    [open, roots],
  );

  const close = useCallback((key: number): void => {
    sessions.current.get(key)?.dispose();
    sessions.current.delete(key);
    dispatch({ type: "close", key });
  }, []);

  const restart = useCallback((key: number): void => {
    const entry = stateRef.current.entries.find((e) => e.key === key);
    const session = sessions.current.get(key);
    if (entry === undefined || session === undefined) return;
    dispatch({ type: "restart", key, spec: entry.spec });
    void session.start(entry.spec);
  }, []);

  const retarget = useCallback(
    (key: number, target: TerminalTarget): void => {
      const entry = stateRef.current.entries.find((e) => e.key === key);
      const session = sessions.current.get(key);
      if (entry === undefined || session === undefined || roots === undefined || entry.spec.target === target) return;
      const live = entry.status.kind === "running" || entry.status.kind === "starting";
      if (live && !window.confirm(`Run "${entry.spec.title}" on the ${targetLabel(target)} instead? Its program is ended and started again there.`)) {
        return;
      }
      const spec = retargetSpec(entry.spec, target, roots);
      dispatch({ type: "restart", key, spec });
      session.term.write(`\r\n\x1b[2m— now on the ${targetLabel(target)}, in ${spec.cwd} —\x1b[22m\r\n`);
      void session.start(spec);
    },
    [roots],
  );

  const toggleDrawer = useCallback(() => dispatch({ type: "toggleDrawer" }), []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if (isDrawerShortcut(e)) {
        e.preventDefault();
        toggleDrawer();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [toggleDrawer]);

  const api = useMemo<TerminalsApi>(
    () => ({
      state,
      roots,
      open,
      openShell,
      close,
      select: (key) => dispatch({ type: "select", key }),
      toDrawer: (key) => dispatch({ type: "toDrawer", key }),
      setDrawer: (o) => dispatch({ type: "drawer", open: o }),
      toggleDrawer,
      restart,
      retarget,
      session: (key) => sessions.current.get(key),
    }),
    [state, roots, open, openShell, close, toggleDrawer, restart, retarget],
  );

  return <TerminalsContext.Provider value={api}>{children}</TerminalsContext.Provider>;
}
