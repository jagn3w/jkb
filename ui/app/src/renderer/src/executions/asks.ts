//! The Timeline pane's rules about which answer may land, kept apart from React so they are tested
//! without a window: asks numbered per scope, a page applied only to the view it was asked from, and
//! a re-read each time the tab is shown that a link showing the tab pre-empts.

import type { TimelineScope } from "../navigation";

/** The latest ask in each scope. */
export type Asks = Record<TimelineScope, number>;

export function newAsks(): Asks {
  return { sessions: 0, session: 0, task: 0, plan: 0 };
}

/** Number a new ask in `scope`; only its answer may land until the next ask in the same scope. */
export function ask(asks: Asks, scope: TimelineScope): number {
  asks[scope] += 1;
  return asks[scope];
}

/** Whether the answer to ask `n` in `scope` is still the one to show. */
export function latest(asks: Asks, scope: TimelineScope, n: number): boolean {
  return asks[scope] === n;
}

/**
 * Whether a page asked for `id` after `cursor` still belongs on screen: the view shows the same
 * session or task, at the same end. A page that arrives after the operator moved on is dropped.
 */
export function pageApplies(shown: { readonly id: string; readonly next: string | null } | undefined, id: string, cursor: string): boolean {
  return shown !== undefined && shown.id === id && shown.next === cursor;
}

/** What showing or hiding the tab means: whether to re-read, and what to remember. */
export interface ShowState {
  readonly wasShown: boolean;
  /** A link is showing the tab, and names what to read itself. */
  readonly linked: boolean;
}

/** The tab became `shown` (or did not): re-read when it has just been shown and no link pre-empts. */
export function onShown(state: ShowState, shown: boolean): { readonly reRead: boolean; readonly next: ShowState } {
  if (!shown || state.wasShown) return { reRead: false, next: { wasShown: shown, linked: state.linked } };
  return { reRead: !state.linked, next: { wasShown: true, linked: false } };
}

/** A link was followed: it pre-empts the re-read only if it is what shows the tab. */
export function onLinked(state: ShowState): ShowState {
  return { wasShown: state.wasShown, linked: !state.wasShown };
}
