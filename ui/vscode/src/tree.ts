//! The tree view: maps @jkb/core TreeChildren onto VS Code TreeItems, lazily.

import * as vscode from "vscode";

import {
  formatLeafKinds,
  kindInfo,
  totalLeaves,
  type JkbClient,
  type NodeRef,
  type TreeChild,
} from "@jkb/core";

import { nodeIcon, nodeUri } from "./decorations.js";

export class JkbTreeProvider implements vscode.TreeDataProvider<TreeChild> {
  private readonly emitter = new vscode.EventEmitter<TreeChild | undefined>();
  readonly onDidChangeTreeData = this.emitter.event;
  private includeTerminal = false;

  constructor(private readonly client: JkbClient) {}

  refresh(): void {
    this.emitter.fire(undefined);
  }

  /** Toggle whether completed/cancelled tasks show; returns the new state. */
  toggleTerminal(): boolean {
    this.includeTerminal = !this.includeTerminal;
    this.refresh();
    return this.includeTerminal;
  }

  getTreeItem(child: TreeChild): vscode.TreeItem {
    const kind = child.ref.kind === "namespace" ? "namespace" : child.ref.itemKind;
    const item = new vscode.TreeItem(
      child.label,
      child.hasChildren
        ? vscode.TreeItemCollapsibleState.Collapsed
        : vscode.TreeItemCollapsibleState.None,
    );
    item.iconPath = nodeIcon(kind, kindInfo(kind).icon, child.status, child.priority);
    // A synthetic resource URI drives the FileDecorationProvider (label colour + badge);
    // the explicit label above still wins over the URI basename.
    item.resourceUri = nodeUri(child);
    item.contextValue = kind;
    if (child.ref.kind === "namespace") {
      item.description = namespaceDescription(child);
      item.tooltip = namespaceTooltip(child);
    } else if (child.status) {
      item.description = child.status;
    }
    item.command = {
      command: "jkb.openDetails",
      title: "Open Details",
      arguments: [child.ref],
    };
    return item;
  }

  async getChildren(child?: TreeChild): Promise<TreeChild[]> {
    const ref: NodeRef | null = child ? child.ref : null;
    if (ref && ref.kind === "item") return []; // items are leaves in the first pass
    const children = await this.client.listChildren(ref, {
      includeTerminal: this.includeTerminal,
    });
    return [...children];
  }
}

/**
 * The dim text beside a folder: its own type (when it has one) and what its subtree
 * actually holds, e.g. `[tasks] 8 task · 2 document`.
 *
 * The breakdown replaces a bare total that read as a task count for every kind of leaf —
 * a folder of documents is not a folder of 12 tasks. Kinds are capped so a long tail cannot
 * push the row off-screen; the tooltip carries the full list.
 */
function namespaceDescription(child: TreeChild): string {
  const parts: string[] = [];
  if (child.nsType) parts.push(`[${child.nsType}]`);
  const breakdown = formatLeafKinds(child.leafKinds, { maxKinds: DESCRIPTION_KIND_LIMIT });
  // Fall back to the total when the host has a count but no breakdown (an older `jkb`).
  const total = totalLeaves(child.leafKinds) || (child.leafCount ?? 0);
  parts.push(breakdown || (total > 0 ? String(total) : "empty"));
  return parts.join(" ");
}

/** The hover text: the full breakdown plus what the namespace's type means. */
function namespaceTooltip(child: TreeChild): vscode.MarkdownString {
  const lines: string[] = [];
  if (child.nsType) {
    const about = child.nsTypeAbout ? ` — ${child.nsTypeAbout}` : "";
    lines.push(`**type \`${child.nsType}\`**${about}`, "");
  }
  const entries = Object.entries(child.leafKinds ?? {}).filter(([, n]) => n > 0);
  if (entries.length === 0) {
    lines.push("No items in this subtree.");
  } else {
    entries.sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
    const total = totalLeaves(child.leafKinds);
    lines.push(`${total} item${total === 1 ? "" : "s"} in subtree:`, "");
    for (const [kind, n] of entries) lines.push(`- ${n} \`${kind}\``);
  }
  return new vscode.MarkdownString(lines.join("\n"));
}

/**
 * How many kinds a tree row shows before collapsing the rest into `+N more`. Three keeps
 * the common cases (tasks; tasks + docs; a mixed ingest folder) fully legible.
 */
const DESCRIPTION_KIND_LIMIT = 3;
