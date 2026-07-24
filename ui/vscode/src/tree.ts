//! The tree view: maps @jkb/core TreeChildren onto VS Code TreeItems, lazily.

import * as vscode from "vscode";

import { kindInfo, type JkbClient, type NodeRef, type TreeChild } from "@jkb/core";

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
    item.iconPath = new vscode.ThemeIcon(kindInfo(kind).icon);
    item.contextValue = kind;
    if (child.status) item.description = child.status;
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
