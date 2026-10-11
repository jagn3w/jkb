import { defaultKeymap, indentWithTab } from "@codemirror/commands";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { useEffect, useRef } from "react";
import { yCollab, yUndoManagerKeymap } from "y-codemirror.next";
import * as Y from "yjs";

import { stateRuns, type DesignDoc } from "@jkb/core";

import { livePreview, selectionActions, setStateMap, spanStates } from "./editor";
import type { DesignSession } from "./session";

/**
 * The design's text in CodeMirror, bound to the session's `Y.Text` by `y-codemirror.next` (D53.4):
 * a keystroke is a Yjs update the session sends, an update from elsewhere is a change the editor
 * shows. Span states are redrawn from each `design.cat` answer the session keeps.
 */
export function DocumentEditor({
  session,
  onDiscuss,
  onComment,
  onApprove,
  onView,
  onCursor,
  onGeometry,
}: {
  readonly session: DesignSession;
  readonly onDiscuss: (from: number, to: number, text: string) => void;
  /** Start a thread on the selection; `text` is the document the offsets are into. */
  readonly onComment: (from: number, to: number, text: string) => void;
  /** Approve exactly the selection (the operator's). */
  readonly onApprove: (from: number, to: number, text: string) => void;
  /** The editor once it exists, and `undefined` once it is gone: what *See next* moves. */
  readonly onView?: (view: EditorView | undefined) => void;
  /** Where the cursor is, each time it moves: the place *See next* walks from. */
  readonly onCursor?: (head: number) => void;
  /** Called when the editor's lines may have moved: what the thread margin is placed against. */
  readonly onGeometry?: () => void;
}): React.JSX.Element {
  const host = useRef<HTMLDivElement>(null);
  const discuss = useRef(onDiscuss);
  discuss.current = onDiscuss;
  const comment = useRef(onComment);
  comment.current = onComment;
  const approve = useRef(onApprove);
  approve.current = onApprove;
  const viewed = useRef(onView);
  viewed.current = onView;
  const cursor = useRef(onCursor);
  cursor.current = onCursor;
  const geometry = useRef(onGeometry);
  geometry.current = onGeometry;

  useEffect(() => {
    const parent = host.current;
    if (parent === null) return undefined;
    const undo = new Y.UndoManager(session.text);
    const editable = new Compartment();
    // Read-only until the first load has merged, and after a refusal (`DesignSession.editable`).
    const writable = (): boolean => session.editable;
    const view = new EditorView({
      parent,
      state: EditorState.create({
        doc: session.text.toString(),
        extensions: [
          keymap.of([...yUndoManagerKeymap, indentWithTab, ...defaultKeymap]),
          livePreview,
          spanStates,
          selectionActions(
            () => ({
              comment: (from, to) => comment.current(from, to, view.state.doc.toString()),
              approve: (from, to) => approve.current(from, to, view.state.doc.toString()),
              discuss: (from, to) => discuss.current(from, to, view.state.doc.toString()),
            }),
            writable,
          ),
          yCollab(session.text, null, { undoManager: undo }),
          editable.of([EditorView.editable.of(writable()), EditorState.readOnly.of(!writable())]),
          EditorView.contentAttributes.of({ "aria-label": "Design document", spellcheck: "true" }),
          EditorView.updateListener.of((u) => {
            if (u.selectionSet || u.docChanged) cursor.current?.(u.state.selection.main.head);
            if (u.geometryChanged || u.heightChanged || u.docChanged) geometry.current?.();
          }),
        ],
      }),
    });

    let drawn: DesignDoc | undefined;
    let wasWritable = writable();
    const sync = (): void => {
      // Only an answer whose text is still the document's (`onScreen`): the editor shows the
      // document, so that answer's offsets are the editor's.
      const current = session.onScreen();
      if (current !== undefined && current !== drawn) {
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
    viewed.current?.(view);
    return () => {
      off();
      viewed.current?.(undefined);
      view.destroy();
      undo.destroy();
    };
  }, [session]);

  return <div className="design-editor" ref={host} />;
}
