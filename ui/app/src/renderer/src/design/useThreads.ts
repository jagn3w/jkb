import { useCallback, useEffect, useState } from "react";

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
  readonly draft: Draft | undefined;
  readonly busy: boolean;
  readonly comment: (from: number, to: number, shown: string) => void;
  readonly cancelDraft: () => void;
  readonly start: (text: string) => void;
  readonly reply: (thread: Thread, text: string) => void;
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
  const [threads, setThreads] = useState<readonly Thread[]>([]);
  const [draft, setDraft] = useState<Selected | undefined>(undefined);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    const answer = decodeThreads(await op(threadOps.list(design.uid)));
    if (answer.ok) setThreads(answer.value);
  }, [design.uid]);

  useEffect(() => {
    void load();
  }, [load]);
  useEffect(
    () =>
      window.jkb.design.onEvent((event) => {
        if (event.topic === design.topic && (event.kind === "thread" || event.kind === "gap")) void load();
      }),
    [design.topic, load],
  );

  /** The version whose text is `shown`, or `undefined` (and a notice) when it moved. */
  const versionOf = useCallback(
    async (shown: string, doing: string): Promise<string | undefined> => {
      if (session === undefined) return undefined;
      const settled = await session.settledVersion();
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
    (text: string) => {
      const d = draft;
      if (d === undefined) return;
      void (async () => {
        const base = await versionOf(d.shown, "the comment was being started");
        if (base === undefined) return;
        const made = await run(op(threadOps.start(design.uid, base, d.from, d.to, text)).then(decodeThread));
        if (made !== undefined) {
          setDraft(undefined);
          void load();
        }
      })();
    },
    [draft, versionOf, run, design.uid, load],
  );

  const reply = useCallback(
    (thread: Thread, text: string) => {
      void run(op(threadOps.reply(thread.uid, text)).then(decodeThread)).then(() => load());
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
    threads,
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
