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

export interface Navigation {
  goTo(tab: TabId): void;
  /** Show the Design tab with this design open. */
  openDesign(uid: string): void;
  /** The last design asked for, which the Design tab opens once it can. */
  readonly designRequest: DesignRequest | undefined;
}

export const NavigationContext = createContext<Navigation>({
  goTo: () => undefined,
  openDesign: () => undefined,
  designRequest: undefined,
});

export function useNavigation(): Navigation {
  return useContext(NavigationContext);
}
