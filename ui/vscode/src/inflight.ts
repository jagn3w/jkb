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
  | { readonly kind: "task"; readonly task: StagedTask; readonly onto: string };

export class InFlightProvider implements vscode.TreeDataProvider<FlightNode> {
  private readonly emitter = new vscode.EventEmitter<FlightNode | undefined>();
  readonly onDidChangeTreeData = this.emitter.event;
  private includeMerged = false;
  /** The last error, shown as a row rather than a popup — this view polls. */
  private lastError: string | null = null;

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
    item.contextValue = "stagingTask";
    const blocker = landBlocker(t);
    item.tooltip = new vscode.MarkdownString(
      [
        `\`${t.uid}\``,
        "",
        `- state: **${t.state}** (status \`${t.status}\`)`,
        t.branch ? `- branch: \`${t.branch}\` → \`${node.onto}\`` : "- no session branch",
        t.review_ns ? `- review: \`${t.review_ns}\`` : "- not reviewed",
        "",
        blocker ? `**Not landable.** ${blocker}` : "**Landable** — `jkb task land`.",
      ].join("\n"),
    );
    return item;
  }

  async getChildren(node?: FlightNode): Promise<FlightNode[]> {
    if (node?.kind === "task") return [];
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
      this.lastError = null;
      return rows.map((branch) => ({ kind: "branch" as const, branch }));
    } catch (e) {
      // Outside a git repo this read legitimately fails, and this view refreshes on every
      // database write — a popup per refresh would be unusable. The message is available on
      // demand instead.
      this.lastError = (e as Error).message;
      return [];
    }
  }

  /** The last failure, for a caller that wants to surface it deliberately. */
  error(): string | null {
    return this.lastError;
  }
}

/** A glyph that distinguishes the three states at a glance. */
function stateIcon(t: StagedTask): string {
  if (t.state === "landed") return "check";
  if (t.state === "review") return "eye";
  return t.dirty ? "circle-outline" : "circle-filled";
}
