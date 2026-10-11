import { useCallback, useEffect, useMemo, useState } from "react";

import {
  decodePlanList,
  decodeStrategies,
  isTerminal,
  partitionPlans,
  pickOf,
  planOps,
  planTasks,
  type ExecPlan,
  type Pick,
  type PlanList,
  type PlanTask,
  type Strategies,
} from "@jkb/core";

import { useTerminals } from "../terminal/TerminalProvider";
import { pinThenPrompt, planTarget, playPlanSpec, playTaskSpec, taskTarget } from "./play";

export type PlanListing =
  | { readonly kind: "loading" }
  | { readonly kind: "loaded"; readonly list: PlanList }
  | { readonly kind: "failed"; readonly message: string };

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

/** What the Plans and Tasks tabs share: one read of the design's plans, and *Play*. */
export interface Plans {
  readonly listing: PlanListing;
  readonly live: readonly ExecPlan[];
  readonly archived: readonly ExecPlan[];
  /** Tasks directly under the design, in no plan. */
  readonly oneOff: readonly PlanTask[];
  /** Every task the design's plans hold, and its one-off tasks. */
  readonly tasks: readonly PlanTask[];
  /** How many of them are not done or cancelled. */
  readonly open: number;
  readonly strategies: Strategies | undefined;
  /** The operator's explicit pick, by bare name; `undefined` is "each task's own". */
  readonly strategy: string | undefined;
  readonly setStrategy: (name: string | undefined) => void;
  readonly pick: Pick | undefined;
  readonly busy: boolean;
  readonly load: () => Promise<void>;
  readonly play: (what: "plan" | "task", uid: string) => Promise<void>;
}

/**
 * The design's execution plans and tasks (D53.6), read once for both tabs that show them. Every
 * read and write is an op; the strategy picker chooses what a *Play* pins its tasks to before Claude
 * starts — only when the operator picked one: left on "each task's own", nothing is pinned and
 * unpinned tasks keep following the default. A pick is held by its bare name, the one identity
 * `workflow.set` resolves; the strategies are re-read with the plans, and again by *Play* itself.
 */
export function usePlans(design: string, repo: string | undefined, onNotice: (message: string) => void): Plans {
  const terminals = useTerminals();
  const [listing, setListing] = useState<PlanListing>({ kind: "loading" });
  const [strategies, setStrategies] = useState<Strategies | undefined>(undefined);
  const [strategy, setStrategy] = useState<string | undefined>(undefined);
  const [busy, setBusy] = useState(false);

  const loadStrategies = useCallback(async () => {
    const answer = decodeStrategies(await op(planOps.strategies()));
    if (!answer.ok) return;
    setStrategies(answer.value);
    // A pick no longer listed (its definition gone) falls back to "each task's own", never to a
    // value the picker cannot show.
    setStrategy((s) => (pickOf(answer.value, s) === undefined ? undefined : s));
  }, []);

  const load = useCallback(async () => {
    setListing((l) => (l.kind === "loaded" ? l : { kind: "loading" }));
    const [answer] = await Promise.all([op(planOps.plans(design)).then(decodePlanList), loadStrategies()]);
    setListing(answer.ok ? { kind: "loaded", list: answer.value } : { kind: "failed", message: answer.error.message });
  }, [design, loadStrategies]);

  useEffect(() => {
    setListing({ kind: "loading" });
    void load();
  }, [load]);

  const list = listing.kind === "loaded" ? listing.list : undefined;
  const { live, archived } = useMemo(() => partitionPlans(list?.plans ?? []), [list]);
  const oneOff = useMemo(() => list?.tasks ?? [], [list]);
  const tasks = useMemo(() => [...(list?.plans ?? []).flatMap(planTasks), ...oneOff], [list, oneOff]);
  const open = useMemo(() => tasks.filter((t) => !isTerminal(t.status)).length, [tasks]);

  const play = useCallback(
    async (what: "plan" | "task", uid: string) => {
      const roots = terminals.roots;
      if (roots === undefined || repo === undefined) {
        onNotice("The terminal is not ready yet.");
        return;
      }
      setBusy(true);
      try {
        const request = what === "plan" ? planOps.play(uid, strategy) : planOps.playTask(uid);
        const target = what === "plan" ? planTarget(design, uid) : taskTarget(design, uid);
        const answer = await pinThenPrompt(op, target, strategy, request);
        if (!answer.ok) {
          onNotice(answer.error.message);
          return;
        }
        const spec = (what === "plan" ? playPlanSpec : playTaskSpec)(answer.value, design, repo, roots, crypto.randomUUID());
        terminals.open(spec, "drawer");
      } finally {
        setBusy(false);
        void load();
      }
    },
    [terminals, strategy, design, repo, onNotice, load],
  );

  return {
    listing,
    live,
    archived,
    oneOff,
    tasks,
    open,
    strategies,
    strategy,
    setStrategy,
    pick: pickOf(strategies, strategy),
    busy,
    load,
    play,
  };
}
