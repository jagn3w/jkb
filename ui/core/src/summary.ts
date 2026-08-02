//! Portable rendering of a namespace's subtree contents.
//
// Shared by every host so a folder reads the same in VS Code and in a future web app.

/** A per-kind leaf count, as carried by `TreeChild.leafKinds`. */
export type LeafKinds = Readonly<Record<string, number>>;

/** Total visible leaves across all kinds. */
export function totalLeaves(kinds: LeafKinds | null | undefined): number {
  if (!kinds) return 0;
  return Object.values(kinds).reduce((sum, n) => sum + n, 0);
}

/** Options for {@link formatLeafKinds}. */
export interface FormatLeafKindsOptions {
  /**
   * Show at most this many kinds, summarising the rest as `+N more`. Keeps a tree row
   * readable when a folder holds a long tail of kinds; pass `Infinity` for a tooltip.
   */
  readonly maxKinds?: number;
}

/**
 * Render a per-kind breakdown as `8 task · 4 document`, most numerous first (ties broken by
 * kind name, so the order is stable across calls).
 *
 * Kinds are **not** pluralized. They are `items.kind` values verbatim — an open vocabulary
 * that grows with every namespace type — and English pluralization goes wrong immediately
 * on it (`hypothesis` → `hypothesiss`). The leading count makes the reading unambiguous
 * without it.
 *
 * Returns `""` when there is nothing to show, so callers can pick their own empty label.
 */
export function formatLeafKinds(
  kinds: LeafKinds | null | undefined,
  options: FormatLeafKindsOptions = {},
): string {
  const entries = Object.entries(kinds ?? {}).filter(([, n]) => n > 0);
  if (entries.length === 0) return "";
  entries.sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));

  const max = options.maxKinds ?? Infinity;
  const shown = entries.slice(0, max);
  const parts = shown.map(([kind, n]) => `${n} ${kind}`);
  const hidden = entries.length - shown.length;
  if (hidden > 0) parts.push(`+${hidden} more`);
  return parts.join(" · ");
}
