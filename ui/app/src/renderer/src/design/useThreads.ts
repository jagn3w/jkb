import { useCallback, useEffect, useRef, useState } from "react";

import {
  decodeDesignSpan,
  decodeNewPrompt,
  decodeThread,
  decodeThreads,
  newPromptTitle,
  promptOps,
  threadOps,
  type Design,
  type Outcome,
  type Thread,
} from "@jkb/core";

import { useTerminals } from "../terminal/TerminalProvider";
import { launchSpec } from "./launch";
import type { DesignSession } from "./session";
import type { Draft } from "./ThreadMargin";

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

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
  readonly approve: (from: number, to: number, shown: string) => void;
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
  const [busy, setBusy] = useState(false);

  const shownRef = useRef(shownVersion);
  shownRef.current = shownVersion;
  const load = useCallback(async () => {
    // Read for the version on screen when asked; an answer that comes back after the screen moved
    // on is kept but not fresh, and the move itself asks again (below).
    const at = shownRef.current;
    const answer = decodeThreads(await op(threadOps.list(design.uid)));
    if (answer.ok) setListed({ threads: answer.value, at });
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

  /**
   * The version whose text is `shown`, or `undefined` with a notice saying why not — each way a
   * session can fail to settle said as *Discuss* says it, never all as "the text changed".
   */
  const versionOf = useCallback(
    async (shown: string, doing: string): Promise<string | undefined> => {
      if (session === undefined) return undefined;
      const settled = await session.settledVersion();
      if (!settled.ok) {
        const why = settled.why;
        switch (why.kind) {
          case "closed":
            onNotice(`The design was left before its edits were saved, while ${doing} — select the text again.`);
            return undefined;
          case "failed":
            onNotice(`${why.message} — reopen the design.`);
            return undefined;
          case "unread":
            onNotice(`Cannot read the design: ${why.message}`);
            return undefined;
          case "moved":
            break;
        }
      }
      if (!settled.ok || settled.doc.text !== shown) {
        onNotice(`The text changed while ${doing} — select it again.`);
        return undefined;
      }
      return settled.doc.version;
    },
    [session, onNotice],
  );

  const run = useCallback(
    async <T>(request: Promise<Outcome<T>>): Promise<T | undefined> => {
      setBusy(true);
      try {
        const answer = await request;
        if (!answer.ok) {
          onNotice(answer.error.message);
          return undefined;
        }
        return answer.value;
      } finally {
        setBusy(false);
      }
    },
    [onNotice],
  );

  const start = useCallback(
    async (text: string): Promise<boolean> => {
      const d = draft;
      if (d === undefined) return false;
      const base = await versionOf(d.shown, "the comment was being started");
      if (base === undefined) return false;
      const made = await run(op(threadOps.start(design.uid, base, d.from, d.to, text)).then(decodeThread));
      if (made === undefined) return false;
      setDraft(undefined);
      void load();
      return true;
    },
    [draft, versionOf, run, design.uid, load],
  );

  const reply = useCallback(
    async (thread: Thread, text: string): Promise<boolean> => {
      const done = await run(op(threadOps.reply(thread.uid, text)).then(decodeThread));
      void load();
      return done !== undefined;
    },
    [run, load],
  );

  const resolve = useCallback(
    (thread: Thread) => {
      void run(op(threadOps.resolve(thread.uid)).then(decodeThread)).then(() => load());
    },
    [run, load],
  );

  const approve = useCallback(
    (from: number, to: number, shown: string) => {
      void (async () => {
        const base = await versionOf(shown, "the approval was being sent");
        if (base === undefined) return;
        await run(op(threadOps.approveSelection(design.uid, base, from, to)).then(decodeDesignSpan));
      })();
    },
    [versionOf, run, design.uid],
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
        const answer = await run(op(promptOps.newPrompt(design.uid, ask)).then(decodeNewPrompt));
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
    askClaude,
  };
}
