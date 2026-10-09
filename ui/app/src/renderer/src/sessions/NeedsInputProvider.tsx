import { createContext, useContext, useEffect, useState } from "react";

import type { NotifyRecord } from "@jkb/core";

import { NotifyWatch } from "./watch";

/** The needs-input state every part of the window reads: the tab's dot and the Sessions list's (D53.9). */
export interface NeedsInput {
  readonly records: readonly NotifyRecord[];
  readonly needing: ReadonlySet<string>;
  readonly problem: string | undefined;
  readonly loaded: boolean;
  /** Read the records again now. */
  refresh(): void;
}

const EMPTY: NeedsInput = { records: [], needing: new Set(), problem: undefined, loaded: false, refresh: () => undefined };

const NeedsInputContext = createContext<NeedsInput>(EMPTY);

export function useNeedsInput(): NeedsInput {
  return useContext(NeedsInputContext);
}

/** One watch on `claude/notify` for the window, from the shell down, so the dot is right on every tab. */
export function NeedsInputProvider({ children }: { readonly children: React.ReactNode }): React.JSX.Element {
  const [value, setValue] = useState<NeedsInput>(EMPTY);
  useEffect(() => {
    const watch = new NotifyWatch({
      op: (request) => window.jkb.op(request),
      subscribe: () => window.jkb.notify.subscribe(),
      unsubscribe: () => window.jkb.notify.unsubscribe(),
      onEvent: (listener) => window.jkb.notify.onEvent(listener),
    });
    const refresh = (): void => void watch.read();
    const publish = (): void =>
      setValue({ records: watch.records, needing: watch.needing, problem: watch.problem, loaded: watch.loaded, refresh });
    const off = watch.onChange(publish);
    publish();
    void watch.open();
    return () => {
      off();
      watch.dispose();
    };
  }, []);
  return <NeedsInputContext.Provider value={value}>{children}</NeedsInputContext.Provider>;
}
