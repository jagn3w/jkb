//! Staging-branch rows: the portable half of the In Flight view (design D38.8).
//
// The shapes mirror `jkb staging ls --json` exactly, and the formatters turn a row into the
// text a host shows. Kept here rather than in the VS Code adapter so a future web host
// renders the same labels — the same reason `summary.ts` lives here.

/** Where a task sits in the pipeline. Derived by the CLI, never stored. */
export type StagedState = "implementing" | "review" | "landed" | "dropped";

/** One task on a staging branch. */
export interface StagedTask {
  readonly uid: string;
  readonly title: string;
  readonly status: string;
  readonly state: StagedState;
  /** The session branch (`task/<session>`), when one is recorded. */
  readonly branch: string | null;
  readonly worktree: string | null;
  readonly dirty: boolean;
  /** Commits the session branch has that the staging branch does not. */
  readonly commits: number;
  /** The branch HEAD a review ran against. */
  readonly reviewed: string | null;
  /**
   * **Every** namespace holding findings recorded for this task. All of them, because the
   * land gate unions them: offering only one means opening a clean namespace while the
   * blocking count came from another.
   */
  readonly review_nss: readonly string[];
  /** A recorded `--no-review` override. */
  readonly review_waived: string | null;
  readonly open_must_fix: number;
  /**
   * Whether the **review** half of the land gate passes: a review is recorded and its
   * findings are in the KB with nothing must-fix outstanding. Separate from
   * {@link StagedTask.land_blocked}, which also covers session and git preconditions.
   */
  readonly review_ok: boolean;
  /**
   * Why `jkb task land` would refuse this task, computed by the CLI, or `null` if it would
   * go ahead. Rendered, never re-derived — see {@link landBlocker}.
   */
  readonly land_blocked: string | null;
}

/** One staging branch and the tasks landing on it. */
export interface StagingBranch {
  readonly branch: string;
  readonly merged: boolean;
  /** Commits it has that trunk does not. */
  readonly ahead: number;
  readonly checkout: string | null;
  readonly tasks: readonly StagedTask[];
}

/** The dim text beside a staging branch: `3 tasks · 7 commits · merged`. */
export function formatBranchSummary(b: StagingBranch): string {
  const parts = [plural(b.tasks.length, "task"), plural(b.ahead, "commit")];
  if (b.merged) parts.push("merged");
  return parts.join(" · ");
}

/**
 * The dim text beside a task: its state, then whatever is *holding* it.
 *
 * Saying what holds a row is the point. A task blocked on must-fix findings that rendered
 * identically to a landable one would be worse than showing no row at all — the same lesson
 * as the subtask count in D35.
 */
export function formatTaskSummary(t: StagedTask): string {
  const parts: string[] = [t.state];
  if (t.commits > 0) parts.push(plural(t.commits, "commit"));
  if (t.dirty) parts.push("uncommitted");
  if (t.open_must_fix > 0) parts.push(`${t.open_must_fix} must-fix open`);
  // Keyed on the REVIEW verdict, not on `land_blocked`. A `reviewed=` sha whose findings never
  // reached the KB must not read as reviewed — but `land_blocked` also fires for uncommitted
  // changes, no commits, a dirty target and a finished task, and using it here dropped the
  // label from properly reviewed rows for reasons that have nothing to do with the review.
  else if (t.reviewed && t.review_ok) parts.push("reviewed");
  if (t.review_waived) parts.push("review waived");
  return parts.join(" · ");
}

/**
 * Why this task cannot land right now, or `null` when it can.
 *
 * This **renders** the CLI's verdict; it does not compute one. The previous version restated
 * the rule here, and a projection of a row cannot express two of `jkb task land`'s
 * preconditions — whether a session worktree still exists, and whether the recorded review
 * namespace holds any findings at all — so a row read "Landable" for tasks the CLI refused
 * outright (every abandoned session, and every swarm task in the view). The rule lives in
 * `staging::land_blocker` beside `land_preflight`, which is the code that enforces it.
 */
export function landBlocker(t: StagedTask): string | null {
  return t.land_blocked ?? null;
}

function plural(n: number, noun: string): string {
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
}
