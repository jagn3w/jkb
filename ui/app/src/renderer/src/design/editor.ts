//! The Document pane's CodeMirror extensions (D53.4–5): Markdown shown as live preview, the span
//! states drawn over the text, and *Discuss* offered on a selection.
//
// The text is the design's one `Y.Text`, bound by `y-codemirror.next` (see `DocumentEditor.tsx`);
// everything here only decorates it. Decorations never change the text: hiding a heading's `#` is a
// replace decoration, not an edit, so what Claude reads with `jkb design cat` is what is typed.

import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { syntaxTree } from "@codemirror/language";
import { RangeSetBuilder, StateEffect, StateField, type EditorState, type Extension, type Range } from "@codemirror/state";
import {
  Decoration,
  EditorView,
  GutterMarker,
  ViewPlugin,
  WidgetType,
  gutter,
  showTooltip,
  type DecorationSet,
  type Tooltip,
  type ViewUpdate,
} from "@codemirror/view";

import { SPAN_STATES, type SpanState, type StateMap } from "@jkb/core";

// ---- live preview -----------------------------------------------------------------------------

/** Markup hidden on lines the cursor is not on, so the line reads as rendered text. */
const MARKS_HIDDEN_AWAY = new Set(["HeaderMark", "EmphasisMark", "StrikethroughMark", "QuoteMark"]);

const INLINE_CLASS: Record<string, string> = {
  Emphasis: "cm-md-em",
  StrongEmphasis: "cm-md-strong",
  Strikethrough: "cm-md-strike",
  InlineCode: "cm-md-code",
  Link: "cm-md-link",
  URL: "cm-md-url",
  LinkMark: "cm-md-mark",
  ListMark: "cm-md-listmark",
};

const hide = Decoration.replace({});

/** The line numbers any selection range touches: there, markup stays visible to be edited. */
function activeLines(state: EditorState): Set<number> {
  const lines = new Set<number>();
  for (const r of state.selection.ranges) {
    const first = state.doc.lineAt(r.from).number;
    const last = state.doc.lineAt(r.to).number;
    for (let n = first; n <= last; n++) lines.add(n);
  }
  return lines;
}

function previewDecorations(view: EditorView, focused: boolean): DecorationSet {
  const { state } = view;
  const active = focused ? activeLines(state) : new Set<number>();
  const out: Range<Decoration>[] = [];
  const lineClass = (from: number, to: number, cls: string): void => {
    for (let pos = from; pos <= to; ) {
      const line = state.doc.lineAt(pos);
      out.push(Decoration.line({ class: cls }).range(line.from));
      pos = line.to + 1;
    }
  };
  for (const { from, to } of view.visibleRanges) {
    syntaxTree(state).iterate({
      from,
      to,
      enter: (node) => {
        const name = node.name;
        const heading = /^(?:ATX|Setext)Heading([1-6])$/.exec(name);
        if (heading !== null) {
          lineClass(node.from, node.to, `cm-md-h${heading[1]}`);
          return;
        }
        switch (name) {
          case "FencedCode":
          case "CodeBlock":
            lineClass(node.from, node.to, "cm-md-codeblock");
            return;
          case "Blockquote":
            lineClass(node.from, node.to, "cm-md-quote");
            return;
          case "HorizontalRule":
            lineClass(node.from, node.to, "cm-md-hr");
            return;
          case "CodeMark":
            // A fence's backticks stay, muted; an inline code span's are hidden away from the cursor.
            if (node.node.parent?.name !== "InlineCode") {
              out.push(Decoration.mark({ class: "cm-md-mark" }).range(node.from, node.to));
              return;
            }
            break;
          default:
            break;
        }
        const cls = INLINE_CLASS[name];
        if (cls !== undefined && node.to > node.from) out.push(Decoration.mark({ class: cls }).range(node.from, node.to));
        if ((MARKS_HIDDEN_AWAY.has(name) || name === "CodeMark") && node.to > node.from) {
          const line = state.doc.lineAt(node.from);
          if (active.has(line.number)) return;
          // A heading's or quote's marker takes the space after it with it.
          const trailing = (name === "HeaderMark" || name === "QuoteMark") && state.doc.sliceString(node.to, node.to + 1) === " " ? 1 : 0;
          const end = Math.min(node.to + trailing, line.to);
          if (end > node.from) out.push(hide.range(node.from, end));
        }
      },
    });
  }
  return Decoration.set(out, true);
}

/** Headings, emphasis, code, quotes and lists drawn in place (Notion-like), markup shown only where edited. */
export const livePreview: Extension = [
  markdown({ base: markdownLanguage }),
  EditorView.lineWrapping,
  ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;
      constructor(view: EditorView) {
        this.decorations = previewDecorations(view, view.hasFocus);
      }
      update(u: ViewUpdate): void {
        if (u.docChanged || u.viewportChanged || u.selectionSet || u.focusChanged || syntaxTree(u.startState) !== syntaxTree(u.state)) {
          this.decorations = previewDecorations(u.view, u.view.hasFocus);
        }
      }
    },
    { decorations: (v) => v.decorations },
  ),
];

// ---- span states ------------------------------------------------------------------------------

/** Replace the drawn span states with `design.cat`'s, already checked to describe this text. */
export const setStateMap = StateEffect.define<StateMap>();

/** How far along each state is: a line is marked with its least advanced. */
const RANK: Record<SpanState, number> = { PROPOSED: 0, APPROVED: 1, STAGED: 2, IMPLEMENTED: 3 };

const stateClass = (state: SpanState): string => `cm-state cm-state-${state.toLowerCase()}`;

const MARKS: Record<SpanState, { inSpan: Decoration; free: Decoration }> = Object.fromEntries(
  SPAN_STATES.map((s) => [
    s,
    {
      inSpan: Decoration.mark({ class: `${stateClass(s)} cm-state-span`, attributes: { "data-state": s }, state: s }),
      free: Decoration.mark({ class: stateClass(s), attributes: { "data-state": s }, state: s }),
    },
  ]),
) as Record<SpanState, { inSpan: Decoration; free: Decoration }>;

class RemovedWords extends WidgetType {
  constructor(readonly text: string) {
    super();
  }
  override eq(other: RemovedWords): boolean {
    return other.text === this.text;
  }
  toDOM(): HTMLElement {
    const el = document.createElement("span");
    el.className = "cm-state-removed";
    el.textContent = this.text;
    el.title = "Removed since the span was approved: it reads PROPOSED until re-approved";
    return el;
  }
  override ignoreEvent(): boolean {
    return true;
  }
}

interface Drawn {
  readonly marks: DecorationSet;
  readonly removed: DecorationSet;
}

function draw(map: StateMap, length: number): Drawn {
  const marks = new RangeSetBuilder<Decoration>();
  for (const run of map.runs) {
    const to = Math.min(run.to, length);
    if (to <= run.from) continue;
    const kind = MARKS[run.state];
    marks.add(run.from, to, run.span === undefined ? kind.free : kind.inSpan);
  }
  const removed = new RangeSetBuilder<Decoration>();
  for (const r of map.removed) {
    if (r.at > length) continue;
    removed.add(r.at, r.at, Decoration.widget({ widget: new RemovedWords(r.text), side: 1 }));
  }
  return { marks: marks.finish(), removed: removed.finish() };
}

const drawnStates = StateField.define<Drawn>({
  create: () => ({ marks: Decoration.none, removed: Decoration.none }),
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setStateMap)) return draw(e.value, tr.newDoc.length);
    }
    // Between answers, the drawing moves with the text; new words fall outside every mark (the
    // engine's anchors put an edge insertion outside the span too), and the next answer redraws.
    return tr.docChanged ? { marks: value.marks.map(tr.changes), removed: value.removed.map(tr.changes) } : value;
  },
  provide: (f) => [
    EditorView.decorations.from(f, (d) => d.marks),
    EditorView.decorations.from(f, (d) => d.removed),
  ],
});

class StateMarker extends GutterMarker {
  constructor(readonly state: SpanState) {
    super();
  }
  override eq(other: StateMarker): boolean {
    return other.state === this.state;
  }
  override toDOM(): Node {
    const el = document.createElement("span");
    el.className = `cm-state-bar cm-state-bar-${this.state.toLowerCase()}`;
    el.title = this.state;
    return el;
  }
}

const BARS: Record<SpanState, StateMarker> = Object.fromEntries(SPAN_STATES.map((s) => [s, new StateMarker(s)])) as Record<
  SpanState,
  StateMarker
>;

/** The least advanced state of the text on the line from `from` to `to`, if the line has text. */
export function lineState(marks: DecorationSet, from: number, to: number): SpanState | undefined {
  let least: SpanState | undefined;
  marks.between(from, to, (f, t, deco) => {
    // A run that only touches the line — ending at its start or starting at its end — is the
    // neighbouring line's.
    if ((t <= from && f < from) || (f >= to && to > from)) return;
    const s = (deco.spec as { state?: SpanState }).state;
    if (s !== undefined && (least === undefined || RANK[s] < RANK[least])) least = s;
  });
  return least;
}

/** The span states: tinted text inside spans, and a bar per line in the margin in the line's state. */
export const spanStates: Extension = [
  drawnStates,
  gutter({
    class: "cm-state-gutter",
    lineMarker(view, line) {
      const s = lineState(view.state.field(drawnStates).marks, line.from, line.to);
      return s === undefined ? null : BARS[s];
    },
    lineMarkerChange: (u) => u.docChanged || u.transactions.some((t) => t.effects.some((e) => e.is(setStateMap))),
  }),
];

// ---- Discuss ----------------------------------------------------------------------------------

/**
 * *Discuss* on a selection (D53.5): a small button over the selected text, which hands the range
 * (UTF-16 offsets, as the editor and Yjs count) to `onDiscuss`.
 */
export function discussOnSelection(onDiscuss: (from: number, to: number) => void): Extension {
  const tooltip = (state: EditorState): Tooltip | null => {
    const sel = state.selection.main;
    if (sel.empty) return null;
    return {
      pos: sel.from,
      above: true,
      create: () => {
        const dom = document.createElement("div");
        dom.className = "cm-discuss";
        const button = document.createElement("button");
        button.type = "button";
        button.textContent = "Discuss";
        button.title = "Discuss the selected text with Claude";
        // Keep the selection: a press on the button must not move the cursor first.
        button.addEventListener("mousedown", (e) => e.preventDefault());
        button.addEventListener("click", () => onDiscuss(sel.from, sel.to));
        dom.appendChild(button);
        return { dom };
      },
    };
  };
  return StateField.define<Tooltip | null>({
    create: tooltip,
    update: (value, tr) => (tr.docChanged || tr.selection !== undefined ? tooltip(tr.state) : value),
    provide: (f) => showTooltip.from(f),
  });
}
