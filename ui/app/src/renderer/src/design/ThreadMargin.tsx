import type { EditorView } from "@codemirror/view";
import { useLayoutEffect, useRef, useState } from "react";

import { orderThreads, stackCards, type Thread } from "@jkb/core";

/** A comment being started: the selection it is on, until it is sent or dropped. */
export interface Draft {
  readonly from: number;
  readonly to: number;
}

/** How tall a card is assumed to be before it has been measured. */
const UNMEASURED = 96;

function when(at: string): string {
  return at.replace("T", " ").replace(/:\d\d(\.\d+)?Z$/, "Z");
}

function Composer({
  placeholder,
  busy,
  onSend,
  onCancel,
}: {
  readonly placeholder: string;
  readonly busy: boolean;
  readonly onSend: (text: string) => void;
  readonly onCancel?: () => void;
}): React.JSX.Element {
  const [text, setText] = useState("");
  const send = (): void => {
    if (text.trim() === "") return;
    onSend(text);
    setText("");
  };
  return (
    <form
      className="thread-composer"
      onSubmit={(e) => {
        e.preventDefault();
        send();
      }}
    >
      <textarea
        value={text}
        rows={2}
        placeholder={placeholder}
        disabled={busy}
        aria-label={placeholder}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
            e.preventDefault();
            send();
          } else if (e.key === "Escape" && onCancel !== undefined) {
            onCancel();
          }
        }}
      />
      <div className="thread-buttons">
        {onCancel !== undefined && (
          <button type="button" className="bar-button" onClick={onCancel}>
            Cancel
          </button>
        )}
        <button type="submit" className="bar-button" disabled={busy || text.trim() === ""}>
          {onCancel !== undefined ? "Comment" : "Reply"}
        </button>
      </div>
    </form>
  );
}

/**
 * The thread margin (operator feedback, 2026-10-11): each comment thread as a card in a column on
 * the right, level with the line its words start on, as Google Docs places comments, pushed down
 * only where it would cover the card above (`stackCards`). The operator starts and resolves threads;
 * Claude replies — here through *Ask Claude*, which starts a session told to answer in the thread.
 * Choosing a card selects its words; a resolved card folds away unless asked for, and a thread whose
 * words were deleted stays listed below the others as *detached*.
 */
export function ThreadMargin({
  threads,
  view,
  layout,
  draft,
  busy,
  onStart,
  onCancelDraft,
  onReply,
  onResolve,
  onAskClaude,
}: {
  readonly threads: readonly Thread[];
  /** The editor the threads' offsets are into. */
  readonly view: () => EditorView | undefined;
  /** Changes whenever the editor's geometry may have: the cards are placed again. */
  readonly layout: number;
  readonly draft: Draft | undefined;
  readonly busy: boolean;
  readonly onStart: (text: string) => void;
  readonly onCancelDraft: () => void;
  readonly onReply: (thread: Thread, text: string) => void;
  readonly onResolve: (thread: Thread) => void;
  readonly onAskClaude: (thread: Thread) => void;
}): React.JSX.Element {
  const [showResolved, setShowResolved] = useState(false);
  const [open, setOpen] = useState<string | undefined>(undefined);
  const column = useRef<HTMLDivElement>(null);
  const cards = useRef(new Map<string, HTMLElement>());
  const [heights, setHeights] = useState<ReadonlyMap<string, number>>(new Map());

  const shown = orderThreads(threads).filter((t) => showResolved || t.status !== "resolved");
  const resolved = threads.filter((t) => t.status === "resolved").length;

  // Where a card wants to be: level with the top of the line its words start on, in the column's
  // own coordinates. `lineBlockAt` answers for any position, rendered or not.
  const v = view();
  const columnTop = column.current?.getBoundingClientRect().top;
  const wantAt = (pos: number): number => {
    if (v === undefined || columnTop === undefined) return 0;
    const at = Math.max(0, Math.min(pos, v.state.doc.length));
    return v.lineBlockAt(at).top + v.documentTop - columnTop;
  };
  const items: { key: string; want: number }[] = [];
  if (draft !== undefined) items.push({ key: "draft", want: wantAt(draft.from) });
  for (const t of shown) items.push({ key: t.uid, want: t.anchored ? wantAt(t.start) : Number.MAX_SAFE_INTEGER });
  items.sort((a, b) => a.want - b.want);
  const last = items.filter((i) => i.want !== Number.MAX_SAFE_INTEGER).at(-1);
  const floor = last === undefined ? 0 : last.want;
  const tops = stackCards(
    items.map((i) => ({ want: i.want === Number.MAX_SAFE_INTEGER ? floor : i.want, height: heights.get(i.key) ?? UNMEASURED })),
  );
  const topOf = new Map(items.map((i, k) => [i.key, tops[k] ?? 0]));

  // Measure after each render; place again only when a height actually changed.
  useLayoutEffect(() => {
    let changed = false;
    const next = new Map(heights);
    for (const [key, el] of cards.current) {
      const h = el.offsetHeight;
      if (next.get(key) !== h) {
        next.set(key, h);
        changed = true;
      }
    }
    if (changed) setHeights(next);
  });

  const select = (t: Thread): void => {
    setOpen(t.uid);
    const ed = view();
    if (ed === undefined || !t.anchored || t.end > ed.state.doc.length) return;
    ed.dispatch({ selection: { anchor: t.start, head: t.end } });
  };
  const ref = (key: string) => (el: HTMLElement | null) => {
    if (el === null) cards.current.delete(key);
    else cards.current.set(key, el);
  };
  const height = Math.max(0, ...items.map((i) => (topOf.get(i.key) ?? 0) + (heights.get(i.key) ?? UNMEASURED)));
  void layout;

  return (
    <aside className="thread-margin" aria-label="Comment threads">
      <div className="thread-margin-bar">
        <label className="toggle">
          <input type="checkbox" checked={showResolved} onChange={(e) => setShowResolved(e.target.checked)} />
          <span>Resolved{resolved > 0 ? ` (${resolved})` : ""}</span>
        </label>
      </div>
      <div className="thread-column" ref={column} style={{ height }}>
        {draft !== undefined && (
          <section className="thread-card" data-draft="true" ref={ref("draft")} style={{ top: topOf.get("draft") }}>
            <Composer placeholder="Add a comment" busy={busy} onSend={onStart} onCancel={onCancelDraft} />
          </section>
        )}
        {shown.map((t) => (
          <section
            key={t.uid}
            ref={ref(t.uid)}
            className="thread-card"
            data-open={open === t.uid || undefined}
            data-status={t.status}
            data-detached={!t.anchored || undefined}
            style={{ top: topOf.get(t.uid) }}
            onClick={() => select(t)}
          >
            {!t.anchored && <p className="thread-detached muted">Detached — its words were deleted: “{t.started_on}”</p>}
            <ol className="thread-comments">
              {t.comments.map((c) => (
                <li key={c.uid}>
                  <div className="thread-meta">
                    <strong>{c.by === "operator" ? "You" : "Claude"}</strong>
                    <time dateTime={c.at} className="muted">
                      {when(c.at)}
                    </time>
                  </div>
                  <p>{c.text}</p>
                </li>
              ))}
            </ol>
            {t.status !== "resolved" && open === t.uid && (
              <>
                <Composer placeholder="Reply" busy={busy} onSend={(text) => onReply(t, text)} />
                <div className="thread-buttons">
                  <button type="button" className="bar-button" disabled={busy} onClick={() => onAskClaude(t)}>
                    Ask Claude
                  </button>
                  <button type="button" className="bar-button" disabled={busy} onClick={() => onResolve(t)}>
                    Resolve
                  </button>
                </div>
              </>
            )}
            {t.status === "resolved" && <p className="muted thread-resolved">Resolved</p>}
          </section>
        ))}
      </div>
    </aside>
  );
}
