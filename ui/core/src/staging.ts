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
  /** The namespace holding that review's findings. */
  readonly review_ns: string | null;
  /** A recorded `--no-review` override. */
  readonly review_waived: string | null;
  readonly open_must_fix: number;
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
  else if (t.reviewed) parts.push("reviewed");
  if (t.review_waived) parts.push("review waived");
  return parts.join(" · ");
}

/**
 * Why this task cannot land right now, or `null` when it can.
 *
 * Terminal states get a reason too rather than reading as landable — `null` means "go ahead",
 * and a landed or cancelled task is the one thing that must never be offered a landing.
 */
export function landBlocker(t: StagedTask): string | null {
  if (t.state === "landed") return "It has already landed on this branch.";
  if (t.state === "dropped") return "It was cancelled, so it will not be landing.";
  if (t.dirty) return "It has uncommitted changes — commit them in the session first.";
  if (t.commits === 0) return "It has no commits that the staging branch does not.";
  if (t.open_must_fix > 0) {
    return `Its review left ${plural(t.open_must_fix, "must-fix finding")} open. Fix or cancel each one, then land.`;
  }
  // Mirrors the CLI predicate exactly: `reviewed=` present, nothing must-fix outstanding. A
  // past `review-waived=` is deliberately NOT a pass — the CLI's gate does not consult it, so
  // accepting it here made the row read "Landable" for a task `jkb task land` then refused.
  // A waiver covers the landing it was granted for, not the next one.
  if (!t.reviewed) {
    return "No review has been recorded. Run /review-log in the session, or land with --no-review.";
  }
  return null;
}

function plural(n: number, noun: string): string {
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
}
