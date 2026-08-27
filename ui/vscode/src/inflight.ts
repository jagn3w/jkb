//! The In Flight view: staging branches and the tasks landing on them (design D38.8).
//
// A separate view rather than a branch of the explorer tree, because it is a different axis:
// the explorer organizes by *where things live*, this organizes by *what is being worked*,
// and the same task legitimately appears in both.
//
// Backed entirely by `jkb staging ls --json` — the same read the branch picker uses, so the
// two cannot disagree about what is live.

import * as vscode from "vscode";

import {
  formatBranchSummary,
  formatTaskSummary,
  landBlocker,
  type StagedTask,
  type StagingBranch,
} from "@jkb/core";

import type { CliJkbClient } from "./cliClient.js";

/** A row: either a staging branch or one task on it. */
export type FlightNode =
  | { readonly kind: "branch"; readonly branch: StagingBranch }
  | { readonly kind: "task"; readonly task: StagedTask; readonly onto: string }
  // A failed read is a ROW, not an empty tree. An empty tree means "nothing is in flight",
  // which is a completely different fact — and a stale `jkb.cliPath` or a DB carrying a newer
  // migration than the installed binary (a failure this project has hit) renders identically
  // to a quiet repo unless the failure is shown.
  | { readonly kind: "error"; readonly message: string };

export class InFlightProvider implements vscode.TreeDataProvider<FlightNode> {
  private readonly emitter = new vscode.EventEmitter<FlightNode | undefined>();
  readonly onDidChangeTreeData = this.emitter.event;
  private includeMerged = false;

  constructor(
    private readonly client: CliJkbClient,
    private readonly repoRoot: () => string | undefined,
  ) {}

  refresh(): void {
    this.emitter.fire(undefined);
  }

  /** Toggle whether already-merged staging branches show; returns the new state. */
  toggleMerged(): boolean {
    this.includeMerged = !this.includeMerged;
    this.refresh();
    return this.includeMerged;
  }

  getTreeItem(node: FlightNode): vscode.TreeItem {
    if (node.kind === "error") {
      const item = new vscode.TreeItem("Could not read staging branches");
      item.description = node.message;
      item.iconPath = new vscode.ThemeIcon("error");
      item.tooltip = new vscode.MarkdownString(
        `\`jkb staging ls\` failed:\n\n\`\`\`\n${node.message}\n\`\`\`\n\n` +
          "This is **not** an empty repo — the read did not succeed. Check `jkb.cliPath` " +
          "points at a current binary and that it can open the database.",
      );
      return item;
    }
    if (node.kind === "branch") {
      const b = node.branch;
      const item = new vscode.TreeItem(
        b.branch,
        b.tasks.length
          ? vscode.TreeItemCollapsibleState.Expanded
          : vscode.TreeItemCollapsibleState.None,
      );
      item.description = formatBranchSummary(b);
      item.iconPath = new vscode.ThemeIcon(b.merged ? "git-merge" : "git-branch");
      item.contextValue = "stagingBranch";
      item.tooltip = new vscode.MarkdownString(
        [
          `**${b.branch}** — the branch this batch lands on before trunk.`,
          "",
          `- ${b.ahead} commit(s) ahead of trunk`,
          `- ${b.tasks.length} task(s)`,
          b.checkout ? `- checked out at \`${b.checkout}\`` : "- not checked out",
          b.merged ? "- already merged into trunk" : "",
        ]
          .filter(Boolean)
          .join("\n"),
      );
      return item;
    }

    const t = node.task;
    const item = new vscode.TreeItem(t.title, vscode.TreeItemCollapsibleState.None);
    item.description = formatTaskSummary(t);
    item.iconPath = new vscode.ThemeIcon(stateIcon(t));
    // Abandon is offered for any row that records a **branch**, because that is exactly what
    // `jkb task abandon` can act on: it falls back to the recorded `branch=` so a session
    // whose checkout was deleted by hand can still be cleaned up in the KB — clearing the
    // claim, clearing its branches' land targets, optionally deleting the branch. Keying this on the worktree
    // removed the only route to that path; keying it on *state* was wrong in both directions,
    // offering the action on a landed row whose worktree `land` had already removed and
    // hiding it on a cancelled row whose worktree nothing else will ever clean up.
    item.contextValue = t.branch ? "stagingTaskSession" : "stagingTask";
    const blocker = landBlocker(t);
    item.tooltip = new vscode.MarkdownString(
      [
        `\`${t.uid}\``,
        "",
        `- state: **${t.state}** (status \`${t.status}\`)`,
        t.branch ? `- branch: \`${t.branch}\` → \`${node.onto}\`` : "- no session branch",
        t.review_nss.length
          ? `- review: ${t.review_nss.map((ns) => `\`${ns}\``).join(", ")}`
          : "- not reviewed",
        "",
        blocker ? `**Not landable.** ${blocker}` : "**Landable** — `jkb task land`.",
      ].join("\n"),
    );
    return item;
  }

  async getChildren(node?: FlightNode): Promise<FlightNode[]> {
    if (node?.kind === "task" || node?.kind === "error") return [];
    if (node?.kind === "branch") {
      return node.branch.tasks.map((task) => ({
        kind: "task" as const,
        task,
        onto: node.branch.branch,
      }));
    }
    const cwd = this.repoRoot();
    if (!cwd) return [];
    try {
      const rows = await this.client.staging(cwd, this.includeMerged);
      return rows.map((branch) => ({ kind: "branch" as const, branch }));
    } catch (e) {
      // Reported as a row, not a popup: this view refreshes on every database write, so a
      // dialog per refresh would be unusable — but silence would make a broken CLI look like
      // an idle repo.
      return [{ kind: "error", message: (e as Error).message }];
    }
  }
}

/** A glyph that distinguishes the states at a glance. */
function stateIcon(t: StagedTask): string {
  if (t.state === "landed") return "check";
  if (t.state === "dropped") return "circle-slash";
  if (t.state === "review") return "eye";
  // `=== "yes"`, not truthiness: `dirty` is a three-valued string, so every value is truthy
  // and the plain test would mark every session dirty. `"unknown"` is a checkout git could not
  // read, which is not the settled state `circle-filled` claims.
  return t.dirty === "yes" || t.dirty === "unknown"
    ? "circle-outline"
    : "circle-filled";
}
