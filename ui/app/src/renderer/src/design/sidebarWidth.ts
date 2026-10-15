//! The Design sidebar's width (operator feedback, 2026-10-14): dragged by a handle on its right edge,
//! kept per window like the fold set (D53.1). Pure, so the tests read the same answers as the tab.

/** The width's bounds: never narrower than a readable row, never wider than half the window. */
export const SIDEBAR_WIDTH = { min: 180, default: 260, maxFraction: 0.5 } as const;

/** `width` clamped to what fits a window `windowWidth` wide; anything unreadable is the default. */
export function clampSidebarWidth(width: number, windowWidth: number): number {
  const max = Number.isFinite(windowWidth)
    ? Math.max(SIDEBAR_WIDTH.min, Math.floor(windowWidth * SIDEBAR_WIDTH.maxFraction))
    : Number.POSITIVE_INFINITY;
  if (!Number.isFinite(width)) return Math.min(SIDEBAR_WIDTH.default, max);
  return Math.min(Math.max(Math.round(width), SIDEBAR_WIDTH.min), max);
}

/** Where a stored width is kept, and how it reads back: a missing or unparsable one is the default. */
export const SIDEBAR_WIDTH_KEY = "jkb.app.design.sidebarWidth";

export function parseSidebarWidth(stored: string | null): number {
  if (stored === null || stored.trim() === "") return SIDEBAR_WIDTH.default;
  const n = Number(stored);
  return Number.isFinite(n) ? n : SIDEBAR_WIDTH.default;
}
