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

  // Refresh the tree only. Decorations follow automatically: each node's resource URI
  // encodes its status/priority, so a changed node is a *new* URI (freshly decorated) and
  // an unchanged node keeps its cached decoration — no colour flashing on refresh.
  const refreshAll = () => tree.refresh();

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
      workTaskWithClaude(client, child),
    ),
    vscode.commands.registerCommand("jkb.landTask", (child?: TreeChild) =>
      landTask(client, child),
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

/** Watch the jkb database and refresh the tree only on real writes, debounced. */
function watchDatabase(onChange: () => void): vscode.Disposable {
  const db = databasePath();
  const dir = path.dirname(db);
  const base = path.basename(db);

  // A cheap signature of the DB's *write* state: the main file's size+mtime plus the WAL's
  // size. Reads in WAL mode only touch the `-shm` side-file (which fs.watch also reports),
  // so gating on this signature ignores read-induced churn — including the extension's own
  // `jkb` reads — and refreshes only on genuine writes. Without it, each read would trigger
  // a refresh that spawns more reads: a flicker loop.
  const signature = (): string => {
    let sig = "";
    for (const suffix of ["", "-wal"]) {
      try {
        const st = fs.statSync(`${db}${suffix}`);
        sig += `${suffix}=${st.size}:${Math.round(st.mtimeMs)};`;
      } catch {
        // side-file may not exist yet
      }
    }
    return sig;
  };

  let last = signature();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const check = (): void => {
    const now = signature();
    if (now !== last) {
      last = now;
      onChange();
    }
  };

  let watcher: fs.FSWatcher | undefined;
  try {
    watcher = fs.watch(dir, (_event, filename) => {
      if (!filename || !filename.startsWith(base)) return;
      if (timer) clearTimeout(timer);
      timer = setTimeout(check, 500);
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

/** The task uid behind a tree selection, or `undefined` with a warning if it is not one. */
function taskUid(child: TreeChild | undefined, verb: string): string | undefined {
  if (!child || child.ref.kind !== "item" || child.ref.itemKind !== "task") {
    vscode.window.showWarningMessage(`jkb: right-click a task to ${verb} it.`);
    return undefined;
  }
  return child.ref.uid;
}

/** The workspace folder to run git-aware `jkb` commands in. */
function repoFolder(): string | undefined {
  const folder = vscode.workspace.workspaceFolders?.[0];
  if (!folder) {
    vscode.window.showErrorMessage(
      "jkb: open the repository as a workspace folder — a task session is a git worktree in it.",
    );
    return undefined;
  }
  return folder.uri.fsPath;
}

/**
 * Open the task's isolated session and start Claude inside it.
 *
 * Each task gets its own git worktree and branch, so several of these can run at once
 * without sharing a checkout, and the task is claimed so nothing else — another click, a
 * swarm run — starts it a second time (design D36). Clicking twice returns the same session.
 */
async function workTaskWithClaude(client: CliJkbClient, child?: TreeChild): Promise<void> {
  const uid = taskUid(child, "work");
  if (!uid) return;
  const cwd = repoFolder();
  if (!cwd) return;

  let session;
  try {
    session = await client.openSession(uid, cwd);
  } catch (e) {
    vscode.window.showErrorMessage(`jkb: ${(e as Error).message}`);
    return;
  }

  const prompt =
    `Work on this jkb task. uid: ${uid}. Title: "${child?.label ?? uid}". ` +
    `You are in an isolated git worktree on branch ${session.branch}, which will land on ` +
    `${session.onto} — other tasks are being worked in parallel in their own worktrees, so ` +
    `stay inside this directory and change nothing outside it. ` +
    `Read the full task with \`jkb task show ${uid}\`, implement it end-to-end following the ` +
    `repo's conventions (see CLAUDE.md), verify it, and COMMIT here. ` +
    `Do not mark the task done and do not merge or rebase onto ${session.onto} — landing is ` +
    `\`jkb task land ${uid}\`, which the human runs, and which marks the task done itself.`;

  const terminal = vscode.window.createTerminal({
    name: `claude: ${session.session.slice(0, 24)}`,
    cwd: session.worktree,
  });
  terminal.show();
  terminal.sendText(`claude ${shellQuote(prompt)}`);
  vscode.window.setStatusBarMessage(
    `jkb: ${session.resumed ? "resumed" : "opened"} session ${session.session} on ${session.branch}`,
    4000,
  );
}

/**
 * Land a task's session: rebase onto the target, run the gate, mark it done.
 *
 * Run in a terminal rather than captured, because the gate is a build — the user needs to
 * watch it, and a red one needs its output.
 */
function landTask(client: CliJkbClient, child?: TreeChild): void {
  const uid = taskUid(child, "land");
  if (!uid) return;
  const cwd = repoFolder();
  if (!cwd) return;
  const terminal = vscode.window.createTerminal({ name: `jkb land: ${uid.slice(-24)}`, cwd });
  terminal.show();
  // Built by the client so it carries the configured cliPath/dbPath, like every other call.
  terminal.sendText(client.terminalCommand(["task", "land", uid]));
}

/** Single-quote a string for safe use in a POSIX shell. */
function shellQuote(s: string): string {
  return `'${s.replace(/'/g, "'\\''")}'`;
}
