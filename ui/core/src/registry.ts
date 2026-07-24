//! The node-kind registry: per-kind presentation + allowed edits.
//
// One entry per node kind. Adding a new kind (or new editable field) is a registry change,
// not new plumbing in the tree/details code. Icons use VS Code codicon ids; a web host can
// map them to its own icon set.

import type { MutationType } from "./model.js";

export interface KindInfo {
  /** Codicon id for the tree/detail icon. */
  readonly icon: string;
  /** Human label for the kind. */
  readonly label: string;
  /** Edit intents allowed for this kind (empty = read-only). */
  readonly edits: readonly MutationType[];
}

const TASK_EDITS: readonly MutationType[] = [
  "setTaskStatus",
  "setTaskPriority",
  "setTaskDue",
  "setTaskTitle",
  "addTaskTag",
];

export const KIND_REGISTRY: Readonly<Record<string, KindInfo>> = {
  namespace: { icon: "folder", label: "Namespace", edits: ["renameNamespace"] },
  task: { icon: "checklist", label: "Task", edits: TASK_EDITS },
  document: { icon: "file", label: "Document", edits: [] },
  chunk: { icon: "symbol-string", label: "Chunk", edits: [] },
  text: { icon: "note", label: "Text", edits: [] },
  view: { icon: "eye", label: "View", edits: [] },
};

export const DEFAULT_KIND: KindInfo = {
  icon: "circle-outline",
  label: "Item",
  edits: [],
};

/** Registry entry for a kind, falling back to a generic item entry. */
export function kindInfo(kind: string): KindInfo {
  return KIND_REGISTRY[kind] ?? DEFAULT_KIND;
}

/** Whether a kind allows a given edit intent. */
export function allowsEdit(kind: string, edit: MutationType): boolean {
  return kindInfo(kind).edits.includes(edit);
}
