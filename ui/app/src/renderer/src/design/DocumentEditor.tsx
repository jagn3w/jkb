import { defaultKeymap, indentWithTab } from "@codemirror/commands";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { useEffect, useRef } from "react";
import { yCollab, yUndoManagerKeymap } from "y-codemirror.next";
import * as Y from "yjs";

import { stateRuns, type DesignDoc } from "@jkb/core";

import { discussOnSelection, livePreview, setStateMap, spanStates } from "./editor";
import type { DesignSession } from "./session";

/**
 * The design's text in CodeMirror, bound to the session's `Y.Text` by `y-codemirror.next` (D53.4):
 * a keystroke is a Yjs update the session sends, an update from elsewhere is a change the editor
 * shows. Span states are redrawn from each `design.cat` answer the session keeps.
 */
export function DocumentEditor({
  session,
  onDiscuss,
}: {
  readonly session: DesignSession;
  readonly onDiscuss: (from: number, to: number, text: string) => void;
}): React.JSX.Element {
  const host = useRef<HTMLDivElement>(null);
  const discuss = useRef(onDiscuss);
  discuss.current = onDiscuss;

  useEffect(() => {
    const parent = host.current;
    if (parent === null) return undefined;
    const undo = new Y.UndoManager(session.text);
    const editable = new Compartment();
    const writable = (): boolean => session.status.kind !== "failed";
    const view = new EditorView({
      parent,
      state: EditorState.create({
        doc: session.text.toString(),
        extensions: [
          keymap.of([...yUndoManagerKeymap, indentWithTab, ...defaultKeymap]),
          livePreview,
          spanStates,
          discussOnSelection((from, to) => discuss.current(from, to, view.state.doc.toString())),
          yCollab(session.text, null, { undoManager: undo }),
          editable.of([EditorView.editable.of(writable()), EditorState.readOnly.of(!writable())]),
          EditorView.contentAttributes.of({ "aria-label": "Design document", spellcheck: "true" }),
        ],
      }),
    });

    let drawn: DesignDoc | undefined;
    let wasWritable = writable();
    const sync = (): void => {
      const current = session.current;
      // The session keeps an answer only when its text was the document's, and the editor shows
      // the document, so the answer's offsets are the editor's.
      if (current !== undefined && current !== drawn && current.text === view.state.doc.toString()) {
        drawn = current;
        view.dispatch({ effects: setStateMap.of(stateRuns(view.state.doc.length, current.spans)) });
      }
      if (writable() !== wasWritable) {
        wasWritable = writable();
        view.dispatch({
          effects: editable.reconfigure([EditorView.editable.of(wasWritable), EditorState.readOnly.of(!wasWritable)]),
        });
      }
    };
    sync();
    const off = session.onChange(sync);
    return () => {
      off();
      view.destroy();
      undo.destroy();
    };
  }, [session]);

  return <div className="design-editor" ref={host} />;
}
