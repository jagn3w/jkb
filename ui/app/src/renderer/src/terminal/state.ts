//! The integrated terminal's window state, as a reducer: which terminals are open, where each is
//! shown (the drawer, or the popover), which one is in front, and how each one stands.
//
// Pure (no React, no DOM, no bridge), so the drawer's rules are tested without a window. The
// PTYs are main's and the xterm instances are `session.ts`'s; this only says what to draw.

import type { TerminalSpec } from "../../../shared/terminal";

/** Where a terminal is shown: the bottom drawer, or the floating popover (D53.10, *Discuss*). */
export type Placement = "drawer" | "popover";

/** How a terminal stands. */
export type TerminalStatus =
  | { readonly kind: "starting" }
  | { readonly kind: "running" }
  /** Its tab was closed and its program is being ended; the tab goes once that is confirmed. */
  | { readonly kind: "closing" }
  | { readonly kind: "exited"; readonly exitCode: number; readonly signal?: number }
  /**
   * `mayBeRunning`: its program's end was not confirmed (a close, or a start refused because of
   * one), so it may still be running.
   */
  | { readonly kind: "failed"; readonly error: string; readonly mayBeRunning?: boolean };

export interface TerminalEntry {
  /** The renderer's id for this terminal; it outlives restarts (each of which is a new PTY). */
  readonly key: number;
  readonly spec: TerminalSpec;
  readonly placement: Placement;
  readonly status: TerminalStatus;
}

export interface TerminalsState {
  readonly entries: readonly TerminalEntry[];
  /** The drawer's front terminal. */
  readonly active: number | undefined;
  /** The terminal in the popover, if it is open. */
  readonly popover: number | undefined;
  readonly drawerOpen: boolean;
}

export const INITIAL_STATE: TerminalsState = { entries: [], active: undefined, popover: undefined, drawerOpen: false };

export type TerminalAction =
  | { readonly type: "open"; readonly key: number; readonly spec: TerminalSpec; readonly placement: Placement }
  | { readonly type: "status"; readonly key: number; readonly status: TerminalStatus }
  /** The terminal starts again from `spec` (a restart, or the target toggle). */
  | { readonly type: "restart"; readonly key: number; readonly spec: TerminalSpec }
  | { readonly type: "close"; readonly key: number }
  | { readonly type: "select"; readonly key: number }
  | { readonly type: "toDrawer"; readonly key: number }
  | { readonly type: "drawer"; readonly open: boolean }
  | { readonly type: "toggleDrawer" };

function drawerKeys(entries: readonly TerminalEntry[]): number[] {
  return entries.filter((e) => e.placement === "drawer").map((e) => e.key);
}

export function reduce(state: TerminalsState, action: TerminalAction): TerminalsState {
  switch (action.type) {
    case "open": {
      const entry: TerminalEntry = { key: action.key, spec: action.spec, placement: action.placement, status: { kind: "starting" } };
      const entries = [...state.entries, entry];
      if (action.placement === "popover") {
        // One popover at a time: the one it replaces moves to the drawer rather than vanishing
        // with its process still running.
        const moved = entries.map((e) => (e.key === state.popover ? { ...e, placement: "drawer" as const } : e));
        return {
          ...state,
          entries: moved,
          popover: action.key,
          active: state.active ?? state.popover,
        };
      }
      return { ...state, entries, active: action.key, drawerOpen: true };
    }
    case "status":
      return { ...state, entries: state.entries.map((e) => (e.key === action.key ? { ...e, status: action.status } : e)) };
    case "restart":
      return {
        ...state,
        entries: state.entries.map((e) => (e.key === action.key ? { ...e, spec: action.spec, status: { kind: "starting" } } : e)),
      };
    case "close": {
      const index = drawerKeys(state.entries).indexOf(action.key);
      const entries = state.entries.filter((e) => e.key !== action.key);
      const remaining = drawerKeys(entries);
      // The neighbour takes the front, as closing an editor tab does: the one after, else before.
      const active =
        state.active === action.key ? (remaining[Math.min(index, remaining.length - 1)] ?? undefined) : state.active;
      return {
        ...state,
        entries,
        active,
        popover: state.popover === action.key ? undefined : state.popover,
        drawerOpen: state.drawerOpen && remaining.length > 0,
      };
    }

    case "select":
      return drawerKeys(state.entries).includes(action.key) ? { ...state, active: action.key, drawerOpen: true } : state;
    case "toDrawer":
      if (state.popover !== action.key) return state;
      return {
        ...state,
        entries: state.entries.map((e) => (e.key === action.key ? { ...e, placement: "drawer" as const } : e)),
        popover: undefined,
        active: action.key,
        drawerOpen: true,
      };
    case "drawer":
      return { ...state, drawerOpen: action.open };
    case "toggleDrawer":
      return { ...state, drawerOpen: !state.drawerOpen };
  }
}

/** The open terminal already running `sessionUuid`, so a second open shows it instead of starting another. */
export function findSession(state: TerminalsState, sessionUuid: string | undefined): TerminalEntry | undefined {
  if (sessionUuid === undefined) return undefined;
  return state.entries.find((e) => e.spec.sessionUuid === sessionUuid);
}

/**
 * What opening `spec` does (D53.10). A terminal with the same `sessionUuid` that is still starting
 * or running is shown, so a double-clicked *Play* does not start a second Claude on one session. One
 * whose program has ended (exited, or failed to start) is started again from `spec` — the new ask
 * (a *Resume*, say) rather than whatever it first ran — in the same tab. Anything else is new.
 */
export type OpenPlan =
  | { readonly kind: "show"; readonly entry: TerminalEntry }
  | { readonly kind: "relaunch"; readonly entry: TerminalEntry }
  | { readonly kind: "new" };

export function planOpen(state: TerminalsState, spec: TerminalSpec): OpenPlan {
  const entry = findSession(state, spec.sessionUuid);
  if (entry === undefined) return { kind: "new" };
  // A program that may still be running (its end unconfirmed) is shown, never started beside:
  // only an explicit Restart overrides that.
  const live =
    entry.status.kind === "starting" ||
    entry.status.kind === "running" ||
    entry.status.kind === "closing" ||
    (entry.status.kind === "failed" && entry.status.mayBeRunning === true);
  return { kind: live ? "show" : "relaunch", entry };
}

/** The drawer's terminals, in tab order. */
export function drawerEntries(state: TerminalsState): TerminalEntry[] {
  return state.entries.filter((e) => e.placement === "drawer");
}

/** A tab's status suffix: nothing while it runs, else why it stopped. */
export function statusLabel(status: TerminalStatus): string {
  switch (status.kind) {
    case "starting":
      return "starting";
    case "running":
      return "";
    case "closing":
      return "ending";
    case "exited":
      return status.signal ? `signal ${status.signal}` : `exit ${status.exitCode}`;
    case "failed":
      return status.mayBeRunning === true ? "may still run" : "failed";
  }
}

/** The drawer's height bounds, in pixels, and its default. */
export const DRAWER_HEIGHT = { min: 120, default: 280, maxFraction: 0.8 } as const;

/** `height` clamped to what fits a window `windowHeight` tall. */
export function clampHeight(height: number, windowHeight: number): number {
  const max = Number.isFinite(windowHeight)
    ? Math.max(DRAWER_HEIGHT.min, Math.floor(windowHeight * DRAWER_HEIGHT.maxFraction))
    : Number.POSITIVE_INFINITY;
  if (!Number.isFinite(height)) return Math.min(DRAWER_HEIGHT.default, max);
  return Math.min(Math.max(Math.round(height), DRAWER_HEIGHT.min), max);
}
