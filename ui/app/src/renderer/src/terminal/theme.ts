//! The terminal's colours, from the design tokens (D53.2, D53.10).
//
// Pure (the token reader is passed in), so the mapping is tested without a window. Every colour is
// a token with a light and a dark value in `styles/tokens.css`, including the 16 ANSI colours:
// xterm's own palette is drawn for a dark ground, and on the light one its white, bright white and
// yellow are all but invisible.

import type { ITheme } from "@xterm/xterm";

/** xterm's ANSI palette keys and the token each one reads. */
export const ANSI_TOKENS = {
  black: "--terminal-ansi-black",
  red: "--terminal-ansi-red",
  green: "--terminal-ansi-green",
  yellow: "--terminal-ansi-yellow",
  blue: "--terminal-ansi-blue",
  magenta: "--terminal-ansi-magenta",
  cyan: "--terminal-ansi-cyan",
  white: "--terminal-ansi-white",
  brightBlack: "--terminal-ansi-bright-black",
  brightRed: "--terminal-ansi-bright-red",
  brightGreen: "--terminal-ansi-bright-green",
  brightYellow: "--terminal-ansi-bright-yellow",
  brightBlue: "--terminal-ansi-bright-blue",
  brightMagenta: "--terminal-ansi-bright-magenta",
  brightCyan: "--terminal-ansi-bright-cyan",
  brightWhite: "--terminal-ansi-bright-white",
} as const satisfies Partial<Record<keyof ITheme, string>>;

/**
 * The theme, given how to read a token (`""` when unset). An unset ANSI token is left out, so xterm
 * falls back to its own colour for it rather than drawing nothing.
 */
export function themeFrom(read: (token: string) => string): ITheme {
  const token = (name: string, fallback: string): string => read(name).trim() || fallback;
  const theme: Record<string, string> = {
    background: token("--terminal-bg", "#ffffff"),
    foreground: token("--ink", "#191919"),
    cursor: token("--ink", "#191919"),
    cursorAccent: token("--terminal-bg", "#ffffff"),
    selectionBackground: token("--terminal-selection", "#d3d3d1"),
  };
  for (const [key, name] of Object.entries(ANSI_TOKENS)) {
    const value = read(name).trim();
    if (value !== "") theme[key] = value;
  }
  return theme as ITheme;
}
