//! Extension entry point: wire the tree view, details panel, decorations, live refresh,
//! search, and the "work task with Claude" command to a CLI-backed JkbClient. All logic
//! lives in @jkb/core + the jkb CLI; this is glue.

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import * as vscode from "vscode";

import type { NodeRef, TreeChild } from "@jkb/core";

import { CliJkbClient } from "./cliClient.js";
import { JkbDecorationProvider } from "./decorations.js";
import { DetailsPanel } from "./detailsPanel.js";
import { JkbTreeProvider } from "./tree.js";

export function activate(context: vscode.ExtensionContext): void {
  const client = makeClient();
  const tree = new JkbTreeProvider(client);
  const decorations = new JkbDecorationProvider();

  const refreshAll = () => {
    tree.refresh();
    decorations.refresh();
  };

  context.subscriptions.push(
    vscode.window.createTreeView("jkb.explorer", { treeDataProvider: tree }),
    vscode.window.registerFileDecorationProvider(decorations),
    watchDatabase(refreshAll),
    vscode.commands.registerCommand("jkb.refresh", refreshAll),
    vscode.commands.registerCommand("jkb.toggleTerminal", () => {
      const shown = tree.toggleTerminal();
      vscode.window.setStatusBarMessage(
        `jkb: completed tasks ${shown ? "shown" : "hidden"}`,
        2000,
      );
    }),
    vscode.commands.registerCommand("jkb.openDetails", (ref: NodeRef) => {
      DetailsPanel.show(client, ref, refreshAll);
    }),
    vscode.commands.registerCommand("jkb.search", () => searchNodes(client, tree)),
    vscode.commands.registerCommand("jkb.workTask", (child?: TreeChild) =>
      workTaskWithClaude(child),
    ),
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

/** The database path jkb will use (config → $JKB_DB → ~/.jkb/jkb.db). */
function databasePath(): string {
  const cfg = vscode.workspace.getConfiguration("jkb").get<string>("dbPath");
  if (cfg) return cfg;
  if (process.env["JKB_DB"]) return process.env["JKB_DB"];
  return path.join(os.homedir(), ".jkb", "jkb.db");
}

/** Watch the jkb database (and its WAL) and refresh the tree on change, debounced. */
function watchDatabase(onChange: () => void): vscode.Disposable {
  const db = databasePath();
  const dir = path.dirname(db);
  const base = path.basename(db);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let watcher: fs.FSWatcher | undefined;
  try {
    watcher = fs.watch(dir, (_event, filename) => {
      if (!filename || !filename.startsWith(base)) return;
      if (timer) clearTimeout(timer);
      timer = setTimeout(onChange, 400);
    });
  } catch {
    // The directory may not exist yet; live refresh is best-effort.
  }
  return new vscode.Disposable(() => {
    if (timer) clearTimeout(timer);
    watcher?.close();
  });
}

/** Prompt for a query DSL string, run it, and open the picked result's details. */
async function searchNodes(client: CliJkbClient, tree: JkbTreeProvider): Promise<void> {
  const query = await vscode.window.showInputBox({
    prompt: "jkb query (DSL) — e.g. `kind:task is:ready ns:tasks/**`",
    placeHolder: "kind:task is:ready",
  });
  if (!query) return;
  let results: readonly TreeChild[];
  try {
    results = await client.search(query);
  } catch (e) {
    vscode.window.showErrorMessage(`jkb: ${(e as Error).message}`);
    return;
  }
  if (results.length === 0) {
    vscode.window.showInformationMessage("jkb: no matches");
    return;
  }
  const pick = await vscode.window.showQuickPick(
    results.map((r) => ({
      label: r.label,
      description: r.status ?? "",
      detail: r.ref.kind === "item" ? r.ref.uid : "",
      ref: r.ref,
    })),
    { placeHolder: `${results.length} matches — pick one to open` },
  );
  if (pick) DetailsPanel.show(client, pick.ref, () => tree.refresh());
}

/** Open a terminal running `claude` seeded with a prompt to do the selected task. */
function workTaskWithClaude(child?: TreeChild): void {
  if (!child || child.ref.kind !== "item" || child.ref.itemKind !== "task") {
    vscode.window.showWarningMessage("jkb: right-click a task to work it with Claude.");
    return;
  }
  const uid = child.ref.uid;
  const prompt =
    `Work on this jkb task. uid: ${uid}. Title: "${child.label}". ` +
    `Read the full task with \`jkb task show ${uid}\`, implement it end-to-end following ` +
    `the repo's conventions (see CLAUDE.md), and when it is done mark it with ` +
    `\`jkb task set ${uid} --status done\`.`;
  const terminal = vscode.window.createTerminal({ name: `claude: ${child.label.slice(0, 24)}` });
  terminal.show();
  terminal.sendText(`claude ${shellQuote(prompt)}`);
}

/** Single-quote a string for safe use in a POSIX shell. */
function shellQuote(s: string): string {
  return `'${s.replace(/'/g, "'\\''")}'`;
}
