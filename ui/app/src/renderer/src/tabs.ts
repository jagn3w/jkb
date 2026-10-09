//! The four tabs, as data: which exist, in what order, and how the keyboard moves between them.
//
// Pure (no React, no DOM), so the shell's navigation rules are tested without a window.

export type TabId = "design" | "workflows" | "container" | "sessions";

export interface TabSpec {
  readonly id: TabId;
  readonly label: string;
  /** One line on what the tab is for. */
  readonly summary: string;
  /** The decision in docs/code-factory.md that governs it. */
  readonly decision: string;
}

export const TABS: readonly TabSpec[] = [
  {
    id: "design",
    label: "Design",
    summary: "Designs as live documents: spans, execution plans, tasks and the prompts that worked them.",
    decision: "D53.4–D53.6",
  },
  {
    id: "workflows",
    label: "Workflows",
    summary: "Agent templates and strategies as data, drawn as a graph beside the task lifecycle.",
    decision: "D53.7",
  },
  {
    id: "container",
    label: "Container",
    summary: "Build, verify and manage the dev container through its installed kit.",
    decision: "D53.8",
  },
  {
    id: "sessions",
    label: "Sessions",
    summary: "Claude Code sessions, which ones need you, and the context each was working in.",
    decision: "D53.9",
  },
];

export const DEFAULT_TAB: TabId = "design";

export function isTabId(value: unknown): value is TabId {
  return TABS.some((t) => t.id === value);
}

/**
 * The tab a key press moves to from `current`, or `undefined` when the key is not a tab key.
 * Arrows wrap (the WAI-ARIA tabs pattern), Home/End jump to the ends, and Cmd/Ctrl+1…4 picks a
 * tab by position from anywhere in the window.
 */
export function tabForKey(
  current: TabId,
  key: string,
  modifiers: { readonly mod: boolean } = { mod: false },
): TabId | undefined {
  const index = TABS.findIndex((t) => t.id === current);
  const at = (i: number): TabId | undefined => TABS[((i % TABS.length) + TABS.length) % TABS.length]?.id;
  if (modifiers.mod) {
    const n = Number(key);
    return Number.isInteger(n) && n >= 1 && n <= TABS.length ? TABS[n - 1]?.id : undefined;
  }
  switch (key) {
    case "ArrowRight":
      return at(index + 1);
    case "ArrowLeft":
      return at(index - 1);
    case "Home":
      return TABS[0]?.id;
    case "End":
      return TABS[TABS.length - 1]?.id;
    default:
      return undefined;
  }
}
