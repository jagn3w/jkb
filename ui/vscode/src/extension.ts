//! Extension entry point: wire the tree view, details panel, decorations, live refresh,
//! search, and the "work task with Claude" command to a CLI-backed JkbClient. All logic
//! lives in @jkb/core + the jkb CLI; this is glue.

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import * as vscode from "vscode";

import {
  landBlocker,
  type NodeRef,
  type StagedTask,
  type StagingBranch,
  type TreeChild,
} from "@jkb/core";

import { CliJkbClient } from "./cliClient.js";
import { JkbDecorationProvider } from "./decorations.js";
import { InFlightProvider, type FlightNode } from "./inflight.js";
import { DetailsPanel } from "./detailsPanel.js";
import { JkbTreeProvider } from "./tree.js";

export function activate(context: vscode.ExtensionContext): void {
  const client = makeClient();
  const tree = new JkbTreeProvider(client);
  const decorations = new JkbDecorationProvider();
  const inflight = new InFlightProvider(client, () => vscode.workspace.workspaceFolders?.[0]?.uri.fsPath);

  // Refresh the tree only. Decorations follow automatically: each node's resource URI
  // encodes its status/priority, so a changed node is a *new* URI (freshly decorated) and
  // an unchanged node keeps its cached decoration — no colour flashing on refresh.
  // In Flight rides the same signal: it is derived from the same tasks.
  const refreshAll = () => {
    tree.refresh();
    inflight.refresh();
  };

  context.subscriptions.push(
    vscode.window.createTreeView("jkb.explorer", { treeDataProvider: tree }),
    vscode.window.createTreeView("jkb.inflight", { treeDataProvider: inflight }),
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
    vscode.commands.registerCommand("jkb.newTask", (child?: TreeChild) =>
      createTask(client, child, refreshAll),
    ),
    vscode.commands.registerCommand("jkb.newSubtask", (child?: TreeChild) =>
      createSubtask(client, child, refreshAll),
    ),
    vscode.commands.registerCommand("jkb.inflight.refresh", () => inflight.refresh()),
    vscode.commands.registerCommand("jkb.inflight.toggleMerged", () => {
      const shown = inflight.toggleMerged();
      vscode.window.setStatusBarMessage(
        `jkb: merged staging branches ${shown ? "shown" : "hidden"}`,
        2000,
      );
    }),
    vscode.commands.registerCommand("jkb.inflight.openSession", (node?: FlightNode) =>
      openSessionTerminal(node),
    ),
    vscode.commands.registerCommand("jkb.inflight.land", (node?: FlightNode) =>
      landFromFlight(client, node),
    ),
    vscode.commands.registerCommand("jkb.inflight.abandon", (node?: FlightNode) =>
      abandonFromFlight(client, node),
    ),
    vscode.commands.registerCommand("jkb.inflight.openFindings", (node?: FlightNode) =>
      openFindings(client, node),
    ),
  );
}

/**
 * Create a task homed in the namespace that was right-clicked.
 *
 * The input takes a **raw quick-add line**, not just a title, so `!p1 @2026-08-12 #area=ui`
 * work exactly as they do in the terminal (design D38.7). This is the D31 rule applied to a
 * write: the UI is a client of the CLI, and one that accepted only a title would be a second,
 * poorer task-creation grammar.
 */
async function createTask(
  client: CliJkbClient,
  child: TreeChild | undefined,
  refresh: () => void,
): Promise<void> {
  if (!child || child.ref.kind !== "namespace") {
    vscode.window.showWarningMessage("jkb: right-click a folder to add a task to it.");
    return;
  }
  const path = child.ref.path;
  await addTask(refresh, `New task in ${path}`, (text) => client.addTask(text, { home: path }));
}

/** Create a subtask of the task that was right-clicked; it inherits the parent's home. */
async function createSubtask(
  client: CliJkbClient,
  child: TreeChild | undefined,
  refresh: () => void,
): Promise<void> {
  const uid = taskUid(child, "add a subtask to");
  if (!uid) return;
  await addTask(refresh, `New subtask of "${child?.label ?? uid}"`, (text) =>
    client.addTask(text, { under: uid }),
  );
}

/** Prompt for a quick-add line, create the task, and reveal it. */
async function addTask(
  refresh: () => void,
  prompt: string,
  create: (text: string) => Promise<{ uid: string }>,
): Promise<void> {
  const text = await vscode.window.showInputBox({
    prompt,
    placeHolder: "Title, plus optional !p1  @2026-08-12  #facet=value",
  });
  if (!text || !text.trim()) return;
  try {
    const created = await create(text.trim());
    refresh();
    vscode.window.setStatusBarMessage(`jkb: created ${created.uid}`, 4000);
  } catch (e) {
    vscode.window.showErrorMessage(`jkb: ${(e as Error).message}`);
  }
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

  const onto = await pickStagingBranch(client, cwd);
  if (onto === undefined) return; // cancelled — open no session

  let session;
  try {
    session = await client.openSession(uid, cwd, onto || undefined);
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
 * Ask which staging branch this session should land on (design D38.3).
 *
 * A staging branch is the branch a batch of tasks lands on before trunk — the same thing
 * `/task-swarm` calls its integration branch. `resolve_onto` has always been able to pick one
 * from four fallbacks, and has always done it invisibly; this makes the choice *visible and
 * overridable*, not mandatory.
 *
 * Returns the branch name, `""` for "let jkb decide" (pass no `--onto`, keeping the fallback
 * chain exactly), or `undefined` when the user cancelled.
 */
async function pickStagingBranch(
  client: CliJkbClient,
  cwd: string,
): Promise<string | undefined> {
  let branches: readonly StagingBranch[] = [];
  try {
    branches = await client.staging(cwd);
  } catch {
    // Outside a git repo, or a jkb too old to know `staging ls`. Falling back to the
    // fallback chain is strictly better than refusing to open a session.
    return "";
  }

  const auto = {
    label: "$(sparkle) Let jkb decide",
    description: branches.length
      ? "join the batch already in flight"
      : "cut a new batch from trunk",
    value: "",
  };
  const create = {
    label: "$(add) New staging branch…",
    description: "cut a fresh branch from trunk",
    value: " new",
  };
  const existing = branches.map((b) => ({
    label: `$(git-branch) ${b.branch}`,
    description: `${b.tasks.length} task${b.tasks.length === 1 ? "" : "s"} · ${b.ahead} commit${
      b.ahead === 1 ? "" : "s"
    } ahead of trunk`,
    value: b.branch,
  }));

  // "Let jkb decide" leads, because the fallback chain is usually right: a picker with no
  // default turns a one-click action into a decision you must make every time, which is how
  // people stop using it.
  const pick = await vscode.window.showQuickPick([auto, ...existing, create], {
    placeHolder: "Where should this task's work land?",
  });
  if (!pick) return undefined;
  if (pick.value !== " new") return pick.value;

  const name = await vscode.window.showInputBox({
    prompt: "Name for the new staging branch (cut from trunk)",
    placeHolder: "e.g. ui-and-staging",
    validateInput: (v) =>
      /^[\w./-]+$/.test(v.trim()) ? undefined : "letters, digits, and . _ - / only",
  });
  const trimmed = name?.trim();
  return trimmed ? trimmed : undefined;
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
  // The Explorer has no StagedTask in hand, so there is nothing to pre-check against; the CLI
  // gate is still the authority either way.
  land(client, uid, undefined);
}

/**
 * Land a task, in a terminal — the gate is a build, and a red one needs its output.
 *
 * One implementation for both the Explorer and In Flight. They were separate six-line copies
 * where only In Flight pre-checked the blocker, so the same task landed differently depending
 * on which tree you clicked, and any later change to UI landing (`--onto`, prompting for
 * `--no-review`) had to be made twice with nothing forcing the second.
 */
function land(client: CliJkbClient, uid: string, task: StagedTask | undefined): void {
  if (task) {
    const blocker = landBlocker(task);
    if (blocker) {
      // Say why, rather than spending a build on a refusal the row already knew about.
      vscode.window.showWarningMessage(`jkb: ${task.title} cannot land yet. ${blocker}`);
      return;
    }
  }
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

// ---------------------------------------------------------------------------
// In Flight row actions — each one reuses a command that already exists.
// ---------------------------------------------------------------------------

/** Open a terminal in the task's session worktree. */
function openSessionTerminal(node?: FlightNode): void {
  if (node?.kind !== "task" || !node.task.worktree) {
    vscode.window.showWarningMessage("jkb: that task has no session checkout.");
    return;
  }
  const terminal = vscode.window.createTerminal({
    name: `jkb: ${node.task.uid.slice(-24)}`,
    cwd: node.task.worktree,
  });
  terminal.show();
}

/** Land the task, in a terminal — the gate is a build, and a red one needs its output. */
function landFromFlight(client: CliJkbClient, node?: FlightNode): void {
  if (node?.kind !== "task") return;
  land(client, node.task.uid, node.task);
}

/** Abandon the session, after confirming — it discards a checkout. */
async function abandonFromFlight(client: CliJkbClient, node?: FlightNode): Promise<void> {
  if (node?.kind !== "task") return;
  const t = node.task;
  const warn = t.dirty ? " It has uncommitted changes, which will be lost." : "";
  const ok = await vscode.window.showWarningMessage(
    `Abandon the session for "${t.title}"?${warn} The task returns to open.`,
    { modal: true },
    "Abandon",
  );
  if (ok !== "Abandon") return;
  const cwd = repoFolder();
  if (!cwd) return;
  const terminal = vscode.window.createTerminal({ name: `jkb abandon`, cwd });
  terminal.show();
  const args = ["task", "abandon", t.uid];
  if (t.dirty) args.push("--force");
  terminal.sendText(client.terminalCommand(args));
}

/** Open the task's review findings in the explorer's details panel. */
async function openFindings(client: CliJkbClient, node?: FlightNode): Promise<void> {
  if (node?.kind !== "task") return;
  const ns = node.task.review_ns;
  if (!ns) {
    vscode.window.showInformationMessage(
      "jkb: no review recorded for this task yet — run /review-log in its session.",
    );
    return;
  }
  DetailsPanel.show(client, { kind: "namespace", path: ns }, () => {});
}
