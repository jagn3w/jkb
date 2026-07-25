//! Portable domain model for the jkb tree explorer.
//
// These types are the contract between the jkb CLI (`jkb ls` / `jkb item show`, via a
// `JkbClient`) and any host (VS Code today, a web app later). No host or transport
// specifics leak in here.

/** A stable reference to a node in the jkb VFS tree. */
export type NodeRef =
  | { readonly kind: "namespace"; readonly path: string }
  | { readonly kind: "item"; readonly uid: string; readonly itemKind: string };

/** A direct child of a namespace, as returned by `jkb ls`. */
export interface TreeChild {
  readonly ref: NodeRef;
  readonly label: string;
  /** Whether the node can be expanded (drives the lazy expand arrow). */
  readonly hasChildren: boolean;
  /**
   * For a namespace: how many visible item leaves live anywhere in its subtree (respecting
   * the terminal-status toggle). Lets the host indicate which folders lead to real content.
   * Absent for item leaves.
   */
  readonly leafCount?: number | null;
  /** Task status, when the child is a task; otherwise absent. */
  readonly status?: string | null;
  /** Task priority (lower = more important), when set; drives ranking + colour. */
  readonly priority?: number | null;
}

/** A tag application on an item. */
export interface Tag {
  readonly facet: string;
  readonly value: string;
}

/** Details for a namespace node (derived from its children). */
export interface NamespaceDetails {
  readonly kind: "namespace";
  readonly path: string;
  readonly childCount: number;
  /** Count of direct children by kind (`namespace`, `task`, `document`, …). */
  readonly breakdown: Readonly<Record<string, number>>;
}

/** Details for an item node (from `jkb item show`). */
export interface ItemDetails {
  readonly kind: "item";
  readonly uid: string;
  readonly itemKind: string;
  readonly status?: string | null;
  readonly priority?: number | null;
  readonly due?: string | null;
  readonly mime?: string | null;
  readonly binding?: string | null;
  readonly namespace?: string | null;
  readonly contentChars: number;
  readonly contentHash?: string | null;
  readonly createdAt: string;
  readonly updatedAt: string;
  readonly tags: readonly Tag[];
  /** A bounded preview — never the whole document. */
  readonly preview: string;
  readonly previewTruncated: boolean;
}

/** The polymorphic details payload for the details pane. */
export type NodeDetails = NamespaceDetails | ItemDetails;

/** The canonical set of task statuses (mirrors `jkb-types::TaskStatus`). */
export const TASK_STATUSES = [
  "open",
  "in_progress",
  "needs_review",
  "done",
  "cancelled",
] as const;
export type TaskStatus = (typeof TASK_STATUSES)[number];

/**
 * An edit the UI can request. Each variant maps to exactly one audited `jkb` command in
 * a {@link JkbClient} — the UI never mutates the DB directly.
 */
export type MutationIntent =
  | { readonly type: "setTaskStatus"; readonly uid: string; readonly status: TaskStatus }
  | { readonly type: "setTaskPriority"; readonly uid: string; readonly priority: number }
  | { readonly type: "setTaskDue"; readonly uid: string; readonly due: string }
  | { readonly type: "setTaskTitle"; readonly uid: string; readonly title: string }
  | { readonly type: "addTaskTag"; readonly uid: string; readonly facet: string; readonly value: string }
  | { readonly type: "renameNamespace"; readonly from: string; readonly to: string }
  | { readonly type: "setItemContent"; readonly uid: string; readonly content: string };

/** All mutation intent discriminants — used by the registry to declare allowed edits. */
export type MutationType = MutationIntent["type"];

/** Options for listing a node's children. */
export interface ListOptions {
  /** Include terminal (`done`/`cancelled`) tasks. Default false. */
  readonly includeTerminal?: boolean;
}
