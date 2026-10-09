import { createContext, useCallback, useContext, useEffect, useMemo, useReducer, useRef, useState } from "react";

import {
  DEFAULT_TARGET,
  defaultCwd,
  type TerminalRoots,
  type TerminalSpec,
  type TerminalTarget,
} from "../../../shared/terminal";
import { TerminalEventRouter } from "./router";
import { TerminalSession, themeFromTokens } from "./session";
import { INITIAL_STATE, planOpen, reduce, type Placement, type TerminalsState } from "./state";

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
   * Open a terminal from `spec`. A spec naming a `sessionUuid` that an open terminal is still
   * running shows that terminal instead of starting a second; one whose program has ended runs
   * `spec` in that terminal (`planOpen`). Returns the terminal's key. `once`: it runs its program
   * this one time, with no Restart (`canRerun`).
   */
  open(spec: TerminalSpec, placement?: Placement, options?: { readonly once?: boolean }): number;
  /** Open the target's login shell in its default directory (the drawer's "+"). */
  openShell(target?: TerminalTarget): void;
  /**
   * Close the terminal. A running program is ended first and the tab goes once that is confirmed;
   * if it is not, the tab stays, saying the program may still be running, and closing it again
   * forgets it.
   */
  close(key: number): void;
  select(key: number): void;
  toDrawer(key: number): void;
  setDrawer(open: boolean): void;
  toggleDrawer(): void;
  /** Run the terminal's spec again (after it exited, say). */
  restart(key: number): void;
  /**
   * Run `spec` in the terminal `key` from now on (a session re-attached after a container rebuild,
   * D53.9). `false` when that terminal is gone.
   */
  relaunch(key: number, spec: TerminalSpec): boolean;
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
    const r = new TerminalEventRouter(
      (listener) => window.jkb.terminal.onEvent(listener),
      (id, chars) => window.jkb.terminal.ack(id, chars),
    );
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

  /**
   * The ONE way a terminal's program starts again: the reducer's `restart` decides, and the program
   * starts only when it changed the state (a run-once terminal's restart changes nothing). Restart,
   * relaunch and `open`'s relaunch of an ended session all come through here. `override`: the
   * person's explicit act, which starts even beside a program that may still be running.
   */
  const rerun = useCallback((key: number, spec: TerminalSpec, override = false): boolean => {
    const session = sessions.current.get(key);
    const action = { type: "restart", key, spec } as const;
    if (session === undefined || reduce(stateRef.current, action) === stateRef.current) return false;
    dispatch(action);
    void session.start(spec, override ? { override: true } : undefined);
    return true;
  }, []);

  const open = useCallback((spec: TerminalSpec, placement: Placement = "drawer", options?: { readonly once?: boolean }): number => {
    const plan = planOpen(stateRef.current, spec);
    if (plan.kind === "show" || plan.kind === "relaunch") {
      const { key } = plan.entry;
      const session = sessions.current.get(key);
      if (plan.kind === "relaunch") rerun(key, spec);
      if (plan.entry.placement === "drawer") dispatch({ type: "select", key });
      session?.focus();
      return key;
    }
    const r = router.current;
    if (r === undefined) throw new Error("the terminal is not ready");
    const key = nextKey.current++;
    const session = new TerminalSession(r, (status) => dispatch({ type: "status", key, status }));
    sessions.current.set(key, session);
    dispatch({ type: "open", key, spec, placement, once: options?.once === true });
    void session.start(spec);
    return key;
  }, [rerun]);

  const openShell = useCallback(
    (target: TerminalTarget = DEFAULT_TARGET): void => {
      if (roots === undefined) return;
      // Titled for what it runs, not where: the badge says where.
      open({ target, cwd: defaultCwd(target, roots), argv: [], title: "shell" });
    },
    [open, roots],
  );

  const close = useCallback((key: number): void => {
    const entry = stateRef.current.entries.find((e) => e.key === key);
    const session = sessions.current.get(key);
    const forget = (): void => {
      void session?.dispose();
      sessions.current.delete(key);
      dispatch({ type: "close", key });
    };
    if (entry === undefined || session === undefined) return forget();
    if (entry.status.kind === "closing") return; // Its end is on its way.
    if (entry.status.kind !== "running" && entry.status.kind !== "starting") return forget();
    // A running program is ended first; the tab is the record of an end that was not confirmed.
    dispatch({ type: "status", key, status: { kind: "closing" } });
    void session.end().then((end) => {
      if (end === undefined || end.confirmed) return forget();
      dispatch({ type: "status", key, status: { kind: "failed", error: end.detail, mayBeRunning: true } });
    });
  }, []);

  const restart = useCallback(
    (key: number): void => {
      const entry = stateRef.current.entries.find((e) => e.key === key);
      if (entry !== undefined) rerun(key, entry.spec, true);
    },
    [rerun],
  );

  const relaunch = useCallback((key: number, spec: TerminalSpec): boolean => rerun(key, spec), [rerun]);

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
      relaunch,
      session: (key) => sessions.current.get(key),
    }),
    [state, roots, open, openShell, close, toggleDrawer, restart, relaunch],
  );

  return <TerminalsContext.Provider value={api}>{children}</TerminalsContext.Provider>;
}
