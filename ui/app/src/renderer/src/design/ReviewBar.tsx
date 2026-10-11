import { EditorView } from "@codemirror/view";
import { useEffect, useRef } from "react";

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
 * last one, *Next* moves on to the next design in the sidebar's order that still has unreviewed
 * text, by counts read again at the press.
 */
export function ReviewBar({
  passages,
  view,
  head,
  shown,
  nextDesign,
  onNextDesign,
}: {
  readonly passages: readonly Passage[] | undefined;
  /** The editor the passages are offsets into. */
  readonly view: () => EditorView | undefined;
  /** Where the cursor is now: the counter and the end are read from it, never remembered. */
  readonly head: number;
  /** Whether the bar is on screen: its keys are live only then. */
  readonly shown: boolean;
  /** The design *Next* opens past the last passage, by title. */
  readonly nextDesign: string | undefined;
  readonly onNextDesign: () => void;
}): React.JSX.Element {
  const bar = useRef<HTMLDivElement>(null);
  // Read from the cursor every render, so a click elsewhere moves the counter and the end with it:
  // a remembered index once promised "Next design" while the click stepped within the text.
  const current = passages === undefined ? undefined : passageAt(passages, head);

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
    // The passages are offsets into the text on screen (`DesignSession.onScreen`); the bound is the
    // last line of defence, never the rule.
    if (p === undefined || p.to > v.state.doc.length) return;
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
  const from = current === undefined ? head - 1 : (passages?.[current]?.from ?? head);
  const atEnd = passages !== undefined && stepPassage(passages, from, 1) === undefined;
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
