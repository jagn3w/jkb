//! Portable node colouring: a node's kind/status/priority → a semantic colour key + badge.
//
// Returns *semantic* keys (not host colours), so the VS Code adapter maps them to
// ThemeColors and a web app maps them to CSS — the ranking/colour policy stays shared.

export interface NodeDecoration {
  /** Semantic colour key (host maps to an actual colour). */
  readonly colorKey: string;
  /** A short badge (≤ 2 chars), e.g. a task's priority. */
  readonly badge?: string;
}

/**
 * The colour + badge for a node, given its kind (`namespace` or an item kind), status, and
 * priority. Tasks are coloured by importance: p1 danger, p2 warning, p3+ notice; terminal
 * tasks are muted. Other kinds get a per-kind key.
 */
export function nodeDecoration(
  kind: string,
  status: string | null | undefined,
  priority: number | null | undefined,
): NodeDecoration {
  if (kind === "namespace") return { colorKey: "kind-namespace" };
  if (kind === "task") {
    if (status === "done" || status === "cancelled") return { colorKey: "muted" };
    if (priority === 1) return { colorKey: "priority-1", badge: "p1" };
    if (priority === 2) return { colorKey: "priority-2", badge: "p2" };
    if (priority != null && priority >= 3) {
      return { colorKey: "priority-3", badge: `p${Math.min(priority, 9)}` };
    }
    return { colorKey: "task" };
  }
  return { colorKey: `kind-${kind}` };
}
