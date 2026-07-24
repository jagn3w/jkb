//! Extension entry point: wire the tree view, details panel, and commands to a
//! CLI-backed JkbClient. All logic lives in @jkb/core + the jkb CLI; this is glue.

import * as vscode from "vscode";

import type { NodeRef } from "@jkb/core";

import { CliJkbClient } from "./cliClient.js";
import { DetailsPanel } from "./detailsPanel.js";
import { JkbTreeProvider } from "./tree.js";

export function activate(context: vscode.ExtensionContext): void {
  const client = makeClient();
  const tree = new JkbTreeProvider(client);

  context.subscriptions.push(
    vscode.window.createTreeView("jkb.explorer", { treeDataProvider: tree }),
    vscode.commands.registerCommand("jkb.refresh", () => tree.refresh()),
    vscode.commands.registerCommand("jkb.toggleTerminal", () => {
      const shown = tree.toggleTerminal();
      vscode.window.setStatusBarMessage(
        `jkb: completed tasks ${shown ? "shown" : "hidden"}`,
        2000,
      );
    }),
    vscode.commands.registerCommand("jkb.openDetails", (ref: NodeRef) => {
      DetailsPanel.show(client, ref, () => tree.refresh());
    }),
  );
}

export function deactivate(): void {
  // no-op: subscriptions are disposed by VS Code.
}

function makeClient(): CliJkbClient {
  const cfg = vscode.workspace.getConfiguration("jkb");
  const dbPath = cfg.get<string>("dbPath");
  return new CliJkbClient({
    cliPath: cfg.get<string>("cliPath") || "jkb",
    dbPath: dbPath ? dbPath : undefined,
  });
}
