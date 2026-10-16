import { createContext, useContext } from "react";

import type { TabId } from "./tabs";

/**
 * Moving between tabs from inside one (D53.9's *Jump to context*): show a tab, or open a design in
 * the Design tab. A request carries a fresh `seq`, so asking for the same design twice still moves.
 */
export interface DesignRequest {
  readonly uid: string;
  readonly seq: number;
}

/** What the Executions tab's timeline shows: the sessions overview, or one session, task or plan. */
export type TimelineScope = "sessions" | "session" | "task" | "plan";

/** A request to show the timeline of one thing, from another tab; `seq` as for a design. */
export interface TimelineRequest {
  readonly scope: TimelineScope;
  /** The session id, task uid or plan uid; empty for the overview. */
  readonly id: string;
  readonly seq: number;
}

export interface Navigation {
  /** The tab on screen: a pane stays mounted when hidden, so one that re-reads on show asks this. */
  readonly active: TabId | undefined;
  goTo(tab: TabId): void;
  /** Show the Design tab with this design open. */
  openDesign(uid: string): void;
  /** The last design asked for, which the Design tab opens once it can. */
  readonly designRequest: DesignRequest | undefined;
  /** Show the Executions tab's timeline of this session, task or plan. */
  openTimeline(scope: TimelineScope, id: string): void;
  /** The last timeline asked for, which the Executions tab shows. */
  readonly timelineRequest: TimelineRequest | undefined;
}

export const NavigationContext = createContext<Navigation>({
  active: undefined,
  goTo: () => undefined,
  openDesign: () => undefined,
  designRequest: undefined,
  openTimeline: () => undefined,
  timelineRequest: undefined,
});

export function useNavigation(): Navigation {
  return useContext(NavigationContext);
}
