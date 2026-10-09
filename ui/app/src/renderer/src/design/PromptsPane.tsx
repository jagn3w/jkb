import { useCallback, useEffect, useState } from "react";

import {
  decodeDesignPrompts,
  decodeNewPrompt,
  newPromptTitle,
  promptOps,
  type Design,
  type DesignPromptRecord,
} from "@jkb/core";

import { useTerminals } from "../terminal/TerminalProvider";
import { launchSpec, resumeSpec } from "./launch";

type Listing =
  | { readonly kind: "loading" }
  | { readonly kind: "loaded"; readonly prompts: readonly DesignPromptRecord[] }
  | { readonly kind: "failed"; readonly message: string };

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

function when(at: string): string {
  return at.replace("T", " ").replace(/:\d\d(\.\d+)?Z$/, "Z");
}

/**
 * The Design tab's Prompts pane (D53.6): every Claude Code session that worked the design, newest
 * first, each resumable where it runs (`claude --resume <uuid>` in its recorded cwd), and *New
 * prompt* — a session on the design started with the operator's own words. A session is recorded
 * by its launch before Claude starts (`launch.ts`), and announced on the design's topic, so the
 * list re-reads when one is: the editor's subscription, never a poll.
 */
export function PromptsPane({
  design,
  repo,
  onNotice,
}: {
  readonly design: Design;
  readonly repo: string;
  readonly onNotice: (message: string) => void;
}): React.JSX.Element {
  const terminals = useTerminals();
  const [listing, setListing] = useState<Listing>({ kind: "loading" });
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    const answer = decodeDesignPrompts(await op(promptOps.list(design.uid)));
    setListing(answer.ok ? { kind: "loaded", prompts: answer.value } : { kind: "failed", message: answer.error.message });
  }, [design.uid]);

  useEffect(() => {
    setListing({ kind: "loading" });
    void load();
  }, [load]);

  // A recorded prompt is announced on the design's topic, which the open document already hears;
  // a gap may have dropped one.
  useEffect(
    () =>
      window.jkb.design.onEvent((event) => {
        if (event.topic === design.topic && (event.kind === "prompt" || event.kind === "gap")) void load();
      }),
    [design.topic, load],
  );

  const start = useCallback(async () => {
    const roots = terminals.roots;
    if (roots === undefined) {
      onNotice("The terminal is not ready yet.");
      return;
    }
    setBusy(true);
    try {
      const answer = decodeNewPrompt(await op(promptOps.newPrompt(design.uid, text)));
      if (!answer.ok) {
        onNotice(answer.error.message);
        return;
      }
      const spec = launchSpec(
        {
          design: design.uid,
          launch: "new",
          label: "New",
          title: newPromptTitle(text, design.title),
          prompt: answer.value.prompt,
        },
        repo,
        roots,
        crypto.randomUUID(),
      );
      terminals.open(spec, "drawer");
      setText("");
    } finally {
      setBusy(false);
    }
  }, [terminals, design.uid, design.title, text, repo, onNotice]);

  const resume = (prompt: DesignPromptRecord): void => {
    const roots = terminals.roots;
    if (roots === undefined) {
      onNotice("The terminal is not ready yet.");
      return;
    }
    const spec = resumeSpec(prompt, roots);
    if (spec === undefined) {
      onNotice(`${prompt.session} is not a session uuid, so it is not resumed.`);
      return;
    }
    terminals.open(spec, "drawer");
  };

  return (
    <section className="prompts-pane" aria-label="Prompts">
      <header className="pane-bar">
        <h2>Prompts</h2>
        <span className="spacer" />
        <button type="button" className="bar-button" onClick={() => void load()} aria-label="Refresh prompts">
          ↻
        </button>
      </header>
      <div className="pane-body">
        <form
          className="prompt-new"
          onSubmit={(e) => {
            e.preventDefault();
            void start();
          }}
        >
          <textarea
            value={text}
            rows={3}
            placeholder="New prompt — what should Claude do with this design? (empty: it reads the design and asks)"
            disabled={busy}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                void start();
              }
            }}
            aria-label="New prompt"
          />
          <button type="submit" className="bar-button play-button" disabled={busy}>
            ▶ New prompt
          </button>
        </form>
        {listing.kind === "failed" ? (
          <p className="muted plan-hint">Cannot list prompts: {listing.message}</p>
        ) : listing.kind === "loading" ? (
          <p className="muted plan-hint">Loading prompts…</p>
        ) : listing.prompts.length === 0 ? (
          <p className="muted plan-hint">No sessions have worked this design yet.</p>
        ) : (
          <ul className="prompt-list">
            {listing.prompts.map((p) => (
              <li key={p.uid} className="prompt-row">
                <div className="prompt-head">
                  <span className="prompt-launch" data-launch={p.launch}>
                    {p.launch}
                  </span>
                  <span className="prompt-title" title={p.title}>
                    {p.title}
                  </span>
                  <button
                    type="button"
                    className="bar-button"
                    onClick={() => resume(p)}
                    title={`claude --resume ${p.session} in ${p.cwd}`}
                  >
                    Resume
                  </button>
                </div>
                <div className="prompt-meta muted">
                  <time dateTime={p.created_at}>{when(p.created_at)}</time>
                  {p.subject !== null && <span title={p.subject}> · {p.subject}</span>}
                  <span title={p.cwd}> · {p.cwd}</span>
                </div>
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}
