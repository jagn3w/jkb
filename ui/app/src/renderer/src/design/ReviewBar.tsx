import { EditorView } from "@codemirror/view";
import { useEffect, useRef, useState } from "react";

import { passageAt, stepPassage, type Passage } from "@jkb/core";

/** Whether a key press belongs to something the user is typing in, which `n` and `p` leave alone. */
function typing(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  return target.closest("input, textarea, select, [contenteditable='true']") !== null;
}

/**
 * *See next* (operator feedback, 2026-10-11): *Previous* and *Next* over the open design's
 * unreviewed passages, with an "n of m" counter, and the `n` and `p` keys whenever nothing that
 * takes text is focused. A passage is selected and scrolled to the middle of the editor. Past the
 * last one, *Next* moves on to the next design in the sidebar's order, when there is one.
 */
export function ReviewBar({
  passages,
  view,
  shown,
  nextDesign,
  onNextDesign,
}: {
  readonly passages: readonly Passage[] | undefined;
  /** The editor the passages are offsets into. */
  readonly view: () => EditorView | undefined;
  /** Whether the bar is on screen: its keys are live only then. */
  readonly shown: boolean;
  /** The design *Next* opens past the last passage, by title. */
  readonly nextDesign: string | undefined;
  readonly onNextDesign: () => void;
}): React.JSX.Element {
  const [current, setCurrent] = useState<number | undefined>(undefined);
  const bar = useRef<HTMLDivElement>(null);

  // A new answer can move or drop passages: the counter follows the selection, never a stale index.
  useEffect(() => {
    const v = view();
    setCurrent(passages === undefined || v === undefined ? undefined : passageAt(passages, v.state.selection.main.head));
  }, [passages, view]);

  const go = (dir: 1 | -1): void => {
    const v = view();
    if (passages === undefined || v === undefined) return;
    // Where the reader is now, read from the editor each time, so a click elsewhere in the text
    // moves the walk with it: inside a passage, from that passage; otherwise from the cursor.
    const sel = v.state.selection.main;
    const inside = passageAt(passages, sel.head);
    const here = inside === undefined ? undefined : passages[inside];
    const at = here !== undefined ? here.from : dir === 1 ? sel.head - 1 : sel.head + 1;
    const i = stepPassage(passages, at, dir);
    if (i === undefined) {
      if (dir === 1 && nextDesign !== undefined) onNextDesign();
      return;
    }
    const p = passages[i];
    if (p === undefined) return;
    setCurrent(i);
    v.dispatch({ selection: { anchor: p.from, head: p.to }, effects: EditorView.scrollIntoView(p.from, { y: "center" }) });
  };
  const latest = useRef(go);
  latest.current = go;

  useEffect(() => {
    if (!shown) return undefined;
    const onKey = (e: KeyboardEvent): void => {
      if (e.metaKey || e.ctrlKey || e.altKey || typing(e.target)) return;
      // Only while this design view is actually on screen (the app's own tab included).
      if (bar.current === null || bar.current.offsetParent === null) return;
      if (e.key === "n") latest.current(1);
      else if (e.key === "p") latest.current(-1);
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [shown]);

  const count = passages?.length;
  const atEnd = passages !== undefined && (current === undefined ? count === 0 : current >= passages.length - 1);
  return (
    <div className="review-bar" ref={bar} role="toolbar" aria-label="Unreviewed passages">
      {count === undefined ? (
        <span className="muted">Reading spans…</span>
      ) : count === 0 ? (
        <span className="review-count" data-clean="true">
          Nothing unreviewed
        </span>
      ) : (
        <span className="review-count">
          {current === undefined ? `${count} unreviewed` : `${current + 1} of ${count} unreviewed`}
        </span>
      )}
      <button type="button" className="bar-button" disabled={!count} onClick={() => go(-1)} title="Previous unreviewed passage (p)">
        ‹ Previous
      </button>
      <button
        type="button"
        className="bar-button"
        disabled={count === undefined || (atEnd && nextDesign === undefined)}
        onClick={() => go(1)}
        title={atEnd && nextDesign !== undefined ? `Open ${nextDesign} (n)` : "Next unreviewed passage (n)"}
      >
        {atEnd && nextDesign !== undefined ? `Next design: ${nextDesign} ›` : "Next ›"}
      </button>
    </div>
  );
}
