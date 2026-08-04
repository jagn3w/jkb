//! The transport seam between the UI and jkb.
//
// A host provides one implementation: VS Code's `CliJkbClient` shells out to `jkb --json`;
// a future web app supplies an HTTP-backed client. Everything above this interface (tree,
// details rendering, registry) is portable.

import type { ListOptions, MutationIntent, NodeDetails, NodeRef, TreeChild } from "./model.js";

export interface JkbClient {
  /**
   * The direct children of `ref`, or the top-level namespaces when `ref` is `null`.
   * Backed by `jkb ls`.
   *
   * `ref` may be any *container*: a pure namespace, or an item that contains others (a
   * task with subtasks). Containment is a behaviour a node takes on, not a separate kind —
   * so there is one call here rather than one per kind. `TreeChild.hasChildren` says
   * whether a node has the behaviour at all.
   */
  listChildren(ref: NodeRef | null, opts?: ListOptions): Promise<readonly TreeChild[]>;

  /** Full details for a node. Backed by `jkb item show` (items) or `jkb ls` (namespaces). */
  getDetails(ref: NodeRef): Promise<NodeDetails>;

  /** Apply one edit intent, mapped to an audited `jkb` command. */
  mutate(intent: MutationIntent): Promise<void>;

  /** Items matching a query DSL string (e.g. `kind:task is:ready`). Backed by `jkb query`. */
  search(query: string): Promise<readonly TreeChild[]>;
}
