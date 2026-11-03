import { useCallback, useEffect, useState } from "react";

import {
  callerOps,
  decodeNewPrompt,
  decodeThread,
  decodeThreads,
  decodeWhoAmI,
  newPromptTitle,
  promptOps,
  threadOps,
  type Caller,
  type Design,
  type Outcome,
  type Thread,
} from "@jkb/core";

import { useTerminals } from "../terminal/TerminalProvider";
import { approveBlocked, approveSelection, inFlight, settle, versionOf, type Doing } from "./approve";
import { launchSpec } from "./launch";
import type { DesignSession } from "./session";
import type { Draft } from "./ThreadMargin";

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

const STARTING: Doing = { during: "the comment was being started", verb: "start the comment" };
const REPLYING: Doing = { during: "the reply was being sent", verb: "reply" };
const RESOLVING: Doing = { during: "the thread was being resolved", verb: "resolve the thread" };
const ASKING: Doing = { during: "Claude was being asked", verb: "ask Claude" };

/** A selection, and the text it is offsets into: a comment or approval acts only on that text. */
interface Selected extends Draft {
  readonly shown: string;
}

/** What the thread margin and the selection's actions need: the threads, and each action. */
export interface Threads {
  readonly threads: readonly Thread[];
  /**
   * Whether the threads' offsets are into the text on screen: they were read at the version the
   * editor shows. Until a re-read catches up with an edit, cards keep their place and select nothing.
   */
  readonly fresh: boolean;
  readonly draft: Draft | undefined;
  readonly busy: boolean;
  readonly comment: (from: number, to: number, shown: string) => void;
  readonly cancelDraft: () => void;
  /** Resolves true once the thread is started: the composer clears only then. */
  readonly start: (text: string) => Promise<boolean>;
  /** Resolves true once the reply is in. */
  readonly reply: (thread: Thread, text: string) => Promise<boolean>;
  readonly resolve: (thread: Thread) => void;
  /** Does nothing while an action on this design is being sent: a stale icon cannot start a second. */
  readonly approve: (from: number, to: number, shown: string) => void;
  /** Why Approve cannot be pressed now (`approveBlocked`), or `undefined` when it can. */
  readonly approveBlocked: string | undefined;
  readonly askClaude: (thread: Thread) => void;
}

/**
 * A design's comment threads, kept live through the design's topic (a `thread` message, or a `gap`
 * that may have dropped one), and the actions on a selection. A selection's offsets are into the
 * text on screen, so a comment or an approval is sent with the version whose text is exactly that,
 * once every edit is saved — the rule *Discuss* follows — and refused when the text moved.
 */
export function useThreads(
  design: Design,
  session: DesignSession | undefined,
  repo: string | undefined,
  onNotice: (message: string) => void,
): Threads {
  const terminals = useTerminals();
  const [listed, setListed] = useState<{ readonly threads: readonly Thread[]; readonly at: string | undefined }>({
    threads: [],
    at: undefined,
  });
  // The version on screen: a thread's offsets are good only against the text they were read for.
  const shownVersion = session?.onScreen()?.version;
  const [draft, setDraft] = useState<Selected | undefined>(undefined);
  const [busy, setBusyState] = useState(false);
  // The actions in flight, counted: each holds and releases only its own busy, so one ending cannot
  // clear another's. Built once; `flight.busy` is read by the callbacks, which a click can reach
  // before React has re-rendered `busy`.
  const [flight] = useState(() => inFlight(setBusyState));
  // Who Code Factory is connected as, asked once: `undefined` until answered, and after a failure —
  // which says nothing, since the server's refusal on a click still says why.
  const [caller, setCaller] = useState<Caller | undefined>(undefined);
  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const answer = decodeWhoAmI(await op(callerOps.whoami()));
        if (live && answer.ok) setCaller(answer.value);
      } catch {
        // Left unknown: Approve stays offered and the server decides.
      }
    })();
    return () => {
      live = false;
    };
  }, []);

  const load = useCallback(async () => {
    // The answer names the version its offsets are into: fresh only while that is the screen's.
    // Comparing with the version on screen when asked was wrong whenever the server had moved on
    // and the session had not yet pulled (review round 2).
    const answer = decodeThreads(await op(threadOps.list(design.uid)));
    if (answer.ok) setListed({ threads: answer.value.threads, at: answer.value.version });
  }, [design.uid]);

  // Re-read whenever the text on screen reaches a new version — a local edit saved and read back, or
  // another editor's merged — not only when a thread message arrives: offsets move with every edit.
  useEffect(() => {
    void load();
  }, [load, shownVersion]);
  useEffect(
    () =>
      window.jkb.design.onEvent((event) => {
        if (event.topic === design.topic && (event.kind === "thread" || event.kind === "gap")) void load();
      }),
    [design.topic, load],
  );

  const run = useCallback(
    // `mark` is the action's own busy: `start` passes the one it already holds.
    async <T>(request: () => Promise<Outcome<T>>, doing: Doing, mark = flight.track()): Promise<T | undefined> => {
      mark(true);
      try {
        return await settle(request, doing, onNotice);
      } finally {
        mark(false);
      }
    },
    [onNotice, flight],
  );

  const start = useCallback(
    async (text: string): Promise<boolean> => {
      const d = draft;
      if (d === undefined) return false;
      // Busy from the call, as Approve is: the wait for the session to settle is in flight too.
      const mark = flight.track();
      mark(true);
      try {
        const base = await versionOf({ session, shown: d.shown, doing: STARTING, onNotice });
        if (base === undefined) return false;
        const made = await run(() => op(threadOps.start(design.uid, base, d.from, d.to, text)).then(decodeThread), STARTING, mark);
        if (made === undefined) return false;
      } finally {
        mark(false);
      }
      setDraft(undefined);
      void load();
      return true;
    },
    [draft, session, onNotice, run, design.uid, load, flight],
  );

  const reply = useCallback(
    async (thread: Thread, text: string): Promise<boolean> => {
      const done = await run(() => op(threadOps.reply(thread.uid, text)).then(decodeThread), REPLYING);
      void load();
      return done !== undefined;
    },
    [run, load],
  );

  const resolve = useCallback(
    (thread: Thread) => {
      void run(() => op(threadOps.resolve(thread.uid)).then(decodeThread), RESOLVING).then(() => load());
    },
    [run, load],
  );

  const approve = useCallback(
    (from: number, to: number, shown: string) => {
      if (flight.busy) return;
      void approveSelection({ design: design.uid, session, from, to, shown, op, onNotice, onBusy: flight.track() });
    },
    [session, onNotice, design.uid, flight],
  );

  const askClaude = useCallback(
    (thread: Thread) => {
      const roots = terminals.roots;
      if (roots === undefined || repo === undefined) {
        onNotice("The terminal is not ready yet.");
        return;
      }
      const ask =
        `Reply in comment thread ${thread.uid} on design ${design.uid}, about the words ` +
        `${JSON.stringify(thread.quote || thread.started_on)}. Read the thread with ` +
        `\`jkb design thread ls ${design.uid}\`, then answer it with ` +
        `\`jkb design thread reply ${thread.uid} --text -\` (the reply on stdin). Do not resolve it: ` +
        `that is the operator's.`;
      void (async () => {
        const answer = await run(() => op(promptOps.newPrompt(design.uid, ask)).then(decodeNewPrompt), ASKING);
        if (answer === undefined) return;
        const spec = launchSpec(
          {
            design: design.uid,
            launch: "new",
            label: "Thread",
            title: newPromptTitle(`Thread: ${thread.comments[0]?.text ?? thread.quote}`, design.title),
            prompt: answer.prompt,
          },
          repo,
          roots,
          crypto.randomUUID(),
        );
        terminals.open(spec, "drawer");
      })();
    },
    [terminals, repo, design.uid, design.title, run, onNotice],
  );

  return {
    threads: listed.threads,
    fresh: shownVersion !== undefined && listed.at === shownVersion,
    draft,
    busy,
    comment: (from, to, shown) => setDraft({ from, to, shown }),
    cancelDraft: () => setDraft(undefined),
    start,
    reply,
    resolve,
    approve,
    approveBlocked: approveBlocked(caller, busy),
    askClaude,
  };
}
