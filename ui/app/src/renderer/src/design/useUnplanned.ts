import { useCallback, useEffect, useState } from "react";

import { decodeUnplanned, unplannedOps, type UnplannedList } from "@jkb/core";

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

/** The unplanned tasks by repo, and how to read them again. */
export interface Unplanned {
  /** The last answer; `undefined` until one arrives, and kept across a failed re-read. */
  readonly list: UnplannedList | undefined;
  readonly error: string | undefined;
  readonly load: () => Promise<void>;
}

/**
 * `task.unplanned`, for the Design sidebar's counts and Unplanned folds. Read when the tab opens, on
 * the sidebar's Refresh and after a Move to step — the cadence of `design.list`, since no topic
 * announces task changes. A daemon older than the op refuses it: that is no unplanned tasks shown,
 * never a failed sidebar.
 */
export function useUnplanned(): Unplanned {
  const [list, setList] = useState<UnplannedList | undefined>(undefined);
  const [error, setError] = useState<string | undefined>(undefined);
  const load = useCallback(async () => {
    try {
      const answer = decodeUnplanned(await op(unplannedOps.list()));
      if (answer.ok) {
        setList(answer.value);
        setError(undefined);
      } else if (answer.error.code === "bad_request") {
        setList({ count: 0, repos: [] });
      } else {
        setError(answer.error.message);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);
  return { list, error, load };
}
