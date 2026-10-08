//! The integrated terminal's contract (D53.10): what a terminal is made from, and what crosses the
//! bridge about it.
//
// Pure data and pure functions, compiled into main, the preload and the renderer alike. A
// terminal is made from a `TerminalSpec` that its CALLER builds (from CLI output, usually): the
// terminal knows where to run a program and how to show it, and nothing about Claude or jkb.

/** Where a terminal's program runs. The container is the default; the host is a deliberate act. */
export type TerminalTarget = "container" | "host";

export const TERMINAL_TARGETS: readonly TerminalTarget[] = ["container", "host"];

export const DEFAULT_TARGET: TerminalTarget = "container";

/** What a terminal is made from. Built by the caller; the terminal adds nothing to it. */
export interface TerminalSpec {
  readonly target: TerminalTarget;
  /** The working directory, absolute, in the TARGET's filesystem (a container path for the container). */
  readonly cwd: string;
  /** The program and its arguments. Empty runs the target's login shell. */
  readonly argv: readonly string[];
  /** The tab's title. */
  readonly title: string;
  /**
   * An identity the caller chose for what runs here (a Claude Code session it pre-minted, say).
   * Opaque to the terminal: carried, never interpreted, except that a second open with the same
   * one shows the terminal already running it rather than starting another.
   */
  readonly sessionUuid?: string;
}

/** Bounds on what the renderer may ask main to run or send. Main enforces them; nothing trusts the caller. */
export const SPEC_LIMITS = {
  maxArgs: 256,
  maxArgBytes: 64 * 1024,
  maxTitle: 200,
  maxCwd: 4096,
} as const;

/** The largest single write main accepts. A paste is chunked below this by the renderer. */
export const MAX_WRITE_CHARS = 64 * 1024;

/** The terminal sizes main accepts. */
export const SIZE_LIMITS = { minCols: 2, maxCols: 1000, minRows: 1, maxRows: 500 } as const;

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** A result across the bridge: the value, or why there is none. */
export type TerminalResult<T> = { readonly ok: true; readonly value: T } | { readonly ok: false; readonly error: string };

function hasNul(s: string): boolean {
  return s.includes("\0");
}

/**
 * `value` as a `TerminalSpec`, or why it is not one. The one check, run by main on everything the
 * renderer sends, so nothing it spawns came from an unvalidated object.
 */
export function parseSpec(value: unknown): TerminalResult<TerminalSpec> {
  const bad = (error: string): TerminalResult<TerminalSpec> => ({ ok: false, error });
  if (typeof value !== "object" || value === null || Array.isArray(value)) return bad("a terminal spec is an object");
  const v = value as Record<string, unknown>;
  const known = new Set(["target", "cwd", "argv", "title", "sessionUuid"]);
  const extra = Object.keys(v).filter((k) => !known.has(k));
  if (extra.length > 0) return bad(`unknown spec field(s): ${extra.join(", ")}`);

  const { target, cwd, argv, title, sessionUuid } = v;
  if (typeof target !== "string" || !(TERMINAL_TARGETS as readonly string[]).includes(target)) {
    return bad(`target must be one of ${TERMINAL_TARGETS.join(", ")}`);
  }
  if (typeof cwd !== "string" || !cwd.startsWith("/") || hasNul(cwd) || cwd.length > SPEC_LIMITS.maxCwd) {
    return bad("cwd must be an absolute path");
  }
  if (!Array.isArray(argv) || argv.length > SPEC_LIMITS.maxArgs) {
    return bad(`argv must be an array of at most ${SPEC_LIMITS.maxArgs} strings`);
  }
  let bytes = 0;
  for (const a of argv) {
    if (typeof a !== "string" || hasNul(a)) return bad("every argv entry must be a string without NUL");
    bytes += a.length;
  }
  if (bytes > SPEC_LIMITS.maxArgBytes) return bad("argv is too long");
  if (argv.length > 0 && argv[0] === "") return bad("argv[0] must name a program");
  if (typeof title !== "string" || title.trim() === "" || title.length > SPEC_LIMITS.maxTitle) {
    return bad(`title must be a non-empty string of at most ${SPEC_LIMITS.maxTitle} characters`);
  }
  if (sessionUuid !== undefined && (typeof sessionUuid !== "string" || !UUID.test(sessionUuid))) {
    return bad("sessionUuid must be a UUID");
  }
  return {
    ok: true,
    value: {
      target: target as TerminalTarget,
      cwd,
      argv: [...(argv as string[])],
      title,
      ...(sessionUuid === undefined ? {} : { sessionUuid: sessionUuid as string }),
    },
  };
}

/** Whether `cols` × `rows` is a size main accepts. */
export function isValidSize(cols: unknown, rows: unknown): boolean {
  return (
    Number.isInteger(cols) &&
    Number.isInteger(rows) &&
    (cols as number) >= SIZE_LIMITS.minCols &&
    (cols as number) <= SIZE_LIMITS.maxCols &&
    (rows as number) >= SIZE_LIMITS.minRows &&
    (rows as number) <= SIZE_LIMITS.maxRows
  );
}

/** Where the two targets' working trees are: the container's repos mount and the host's. */
export interface TerminalRoots {
  /** The container's name, as `docker exec` addresses it. */
  readonly container: string;
  /** The repos directory inside the container (`CTR_REPOS` in `.container/run.sh`). */
  readonly containerRepos: string;
  /** The same directory on the host (`HOST_REPOS`). */
  readonly hostRepos: string;
  /** Where a host terminal starts when nothing better is known. */
  readonly hostHome: string;
}

/** Where a new terminal on `target` starts when its caller names nowhere in particular. */
export function defaultCwd(target: TerminalTarget, roots: TerminalRoots): string {
  return target === "container" ? roots.containerRepos : roots.hostHome;
}

function rebase(path: string, from: string, to: string): string | undefined {
  const root = from.replace(/\/+$/, "");
  if (path === root) return to;
  if (path.startsWith(`${root}/`)) return to.replace(/\/+$/, "") + path.slice(root.length);
  return undefined;
}

/**
 * `spec` moved to `target` (the per-terminal toggle). The program and title are kept; the
 * working directory is translated through the repos mount, the one directory both sides see,
 * and falls back to the target's default when it lies outside it — a container path means
 * nothing on the host, and naming it there would only fail.
 */
export function retarget(spec: TerminalSpec, target: TerminalTarget, roots: TerminalRoots): TerminalSpec {
  if (spec.target === target) return spec;
  const [from, to] =
    target === "host" ? [roots.containerRepos, roots.hostRepos] : [roots.hostRepos, roots.containerRepos];
  const cwd = rebase(spec.cwd, from, to) ?? defaultCwd(target, roots);
  return { ...spec, target, cwd };
}

/**
 * `path` in the host's filesystem: a container path under the repos mount is carried to the host's
 * side of it; a path already under the host's repos directory is kept; anything else is `undefined`,
 * since neither side can say where it is on the other.
 */
export function hostPathOf(path: string, roots: TerminalRoots): string | undefined {
  return rebase(path, roots.containerRepos, roots.hostRepos) ?? rebase(path, roots.hostRepos, roots.hostRepos);
}

/** How a target is named on a terminal's tab, so it is never ambiguous where a command runs. */
export function targetLabel(target: TerminalTarget): string {
  return target === "container" ? "container" : "host";
}

/** A terminal main is running for a window. */
export interface TerminalInfo {
  /** Main's id for it; unique for the life of the app. */
  readonly id: number;
  readonly spec: TerminalSpec;
}

/** What main tells the renderer about a terminal it runs. */
export type TerminalEvent =
  | { readonly id: number; readonly kind: "data"; readonly data: string }
  | { readonly id: number; readonly kind: "exit"; readonly exitCode: number; readonly signal?: number };
