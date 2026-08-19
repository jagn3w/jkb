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

import { launchClaude, deliverQueuedPrompt } from "./claude.js";
import { CliJkbClient, type SessionInfo } from "./cliClient.js";
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
      workTaskWithClaude(context, client, child),
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

  // This window may be the one "Work this task with Claude" just opened on a session
  // worktree. `vscode.openFolder` carries no payload, so the prompt was left in a queue for
  // it — which is also why the extension activates at startup rather than on its first view.
  void deliverQueuedPrompt(context);
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
 * Open the task's isolated session and start Claude Code in it.
 *
 * Each task gets its own git worktree and branch, so several of these can run at once
 * without sharing a checkout, and the task is claimed so nothing else — another click, a
 * swarm run — starts it a second time (design D36). Clicking twice returns the same session.
 *
 * The session opens as its own VS Code window with a Claude Code chat in it, rather than as a
 * `claude` terminal in this one: the Claude Code extension runs in the window's first
 * workspace folder and takes no directory argument, so the session's worktree has to *be*
 * that folder (see `claude.ts`). A window is also the surface the work wants — the worktree's
 * own file tree, source control and terminals, beside the chat driving them. The terminal
 * remains the fallback for a machine without the extension, where it is still the whole
 * feature rather than a degraded one.
 */
async function workTaskWithClaude(
  context: vscode.ExtensionContext,
  client: CliJkbClient,
  child?: TreeChild,
): Promise<void> {
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

  const prompt = taskPrompt(uid, child?.label ?? uid, session);
  const launch = await launchClaude(context, session.worktree, uid, prompt);
  if (launch === "unavailable") {
    startClaudeInTerminal(session, prompt);
    vscode.window.showInformationMessage(
      "jkb: started `claude` in a terminal — install the Claude Code extension to work the " +
        "session in its own window instead.",
    );
  }
  vscode.window.setStatusBarMessage(
    `jkb: ${session.resumed ? "resumed" : "opened"} session ${session.session} on ${session.branch}` +
      (launch === "window" ? " — opening its window" : ""),
    4000,
  );
}

/** What Claude is asked to do in a session, in the one place that knows the session's shape. */
function taskPrompt(uid: string, title: string, session: SessionInfo): string {
  return (
    `Work on this jkb task. uid: ${uid}. Title: "${title}". ` +
    `You are in an isolated git worktree on branch ${session.branch}, which will land on ` +
    `${session.onto} — other tasks are being worked in parallel in their own worktrees, so ` +
    `stay inside this directory and change nothing outside it. ` +
    `Read the full task with \`jkb task show ${uid}\`, implement it end-to-end following the ` +
    `repo's conventions (see CLAUDE.md), verify it, and COMMIT here. ` +
    `Do not mark the task done and do not merge or rebase onto ${session.onto} — landing is ` +
    `\`jkb task land ${uid}\`, which the human runs, and which marks the task done itself.`
  );
}

/** The fallback when Claude Code is not there: the CLI, in a terminal in the worktree. */
function startClaudeInTerminal(session: SessionInfo, prompt: string): void {
  const terminal = vscode.window.createTerminal({
    name: `claude: ${session.session.slice(0, 24)}`,
    cwd: session.worktree,
  });
  terminal.show();
  terminal.sendText(`claude ${shellQuote(prompt)}`);
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
  // There must be something to abandon: a recorded branch is enough, since the CLI cleans up
  // a session whose checkout is already gone. The menu is scoped the same way (see
  // `contextValue` in inflight.ts); this covers the palette, which is scoped by nothing.
  if (!t.branch) {
    vscode.window.showWarningMessage(`jkb: ${t.title} has no session to abandon.`);
    return;
  }
  const warn = t.dirty ? " It has uncommitted changes, which will be lost." : "";
  // A finished task keeps its status: abandoning disposes of the checkout, and putting
  // already-merged or deliberately-cancelled work back on the ready frontier is the one
  // thing it must not do. Say which is about to happen, because they are different acts.
  const outcome =
    t.state === "landed" || t.state === "dropped"
      ? `The task stays ${t.status}.`
      : "The task returns to open.";
  const what = t.worktree ? "session" : "session record";
  const ok = await vscode.window.showWarningMessage(
    `Abandon the ${what} for "${t.title}"?${warn} ${outcome}`,
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

/**
 * Open one of the task's review findings.
 *
 * The findings themselves are the point: this used to open the generic namespace panel for
 * one guessed `review=` namespace, which shows a child count, a kind breakdown and a rename
 * box — no titles, nothing clickable, and a rename that would break the `review=` facet the
 * land gate resolves. It also picked whichever namespace sorted last while the blocking
 * count came from the union of all of them, so the panel could open the clean one.
 */
async function openFindings(client: CliJkbClient, node?: FlightNode): Promise<void> {
  if (node?.kind !== "task") return;
  const nss = node.task.review_nss;
  if (nss.length === 0) {
    vscode.window.showInformationMessage(
      "jkb: no review recorded for this task yet — run /review-log in its session.",
    );
    return;
  }
  // Every recorded review, not one of them: which namespace a finding is in is not something
  // the reader should have to guess at, and open must-fix findings sort to the top anyway.
  let findings: FindingPick[];
  try {
    findings = (await Promise.all(nss.map((ns) => findingsUnder(client, ns)))).flat();
  } catch (e) {
    // Said as what it is — the read failed — rather than as a conclusion about the review.
    vscode.window.showErrorMessage(`jkb: could not read ${nss.join(", ")}: ${(e as Error).message}`);
    return;
  }
  if (findings.length === 0) {
    vscode.window.showWarningMessage(
      `jkb: ${nss.join(", ")} holds no findings — they never reached the KB. Re-run /review-log.`,
    );
    return;
  }
  const picked = await vscode.window.showQuickPick(
    findings.map((f) => ({
      label: f.label,
      description: f.detail,
      uid: f.uid,
      itemKind: f.itemKind,
    })),
    { title: `Review findings for ${node.task.title}`, matchOnDescription: true },
  );
  if (!picked) return;
  DetailsPanel.show(client, { kind: "item", uid: picked.uid, itemKind: picked.itemKind }, () => {});
}

/** One finding, flattened out of the review namespace's severity sub-folders. */
interface FindingPick {
  readonly uid: string;
  readonly itemKind: string;
  readonly label: string;
  readonly detail: string;
  readonly open: boolean;
  readonly priority: number;
}

/**
 * Every finding item under a review namespace, must-fix and still-open first.
 *
 * `/review-log` writes one `## <Severity>` header per severity and the `tasks` serializer
 * turns each into a sub-namespace, so the items live one level down — but a review with a
 * single section puts them at the top level, and both are listed. Terminal findings are
 * included (`includeTerminal`): a fixed finding is the useful half of the record when you
 * are looking at why a landing was refused.
 */
async function findingsUnder(client: CliJkbClient, ns: string): Promise<FindingPick[]> {
  const opts = { includeTerminal: true };
  // Recursive, because the gate's own count is: `review::findings_in` scopes the whole
  // subtree, so a review whose `tasks.md` grew a third heading level would put findings a
  // level deeper than a fixed two-level walk reaches — and this panel would then report that
  // the findings never arrived while `jkb task land` refused on the ones it could not see.
  // Depth is bounded (four, well past any review layout) so a cycle cannot hang the UI.
  // A failed read is NOT an empty review. Swallowing the error reported a broken CLI or an
  // unreadable database as "the findings never reached the KB — re-run /review-log", which is
  // advice for a different problem entirely; the top-level read is therefore allowed to throw
  // and the caller says what actually happened. Failures deeper in the walk are tolerated —
  // one unreadable sub-namespace should not hide the findings that did load.
  const walk = async (ref: NodeRef, depth: number, strict = false): Promise<readonly TreeChild[]> => {
    const children = strict
      ? await client.listChildren(ref, opts)
      : await client.listChildren(ref, opts).catch(() => []);
    if (depth === 0) return children;
    const deeper = await Promise.all(
      children
        .filter((c) => c.ref.kind === "namespace")
        .map((c) => walk(c.ref, depth - 1)),
    );
    return [...children, ...deeper.flat()];
  };
  const out: FindingPick[] = [];
  for (const child of await walk({ kind: "namespace", path: ns }, 4, true)) {
    if (child.ref.kind !== "item") continue;
    const priority = child.priority ?? Number.MAX_SAFE_INTEGER;
    out.push({
      uid: child.ref.uid,
      itemKind: child.ref.itemKind,
      label: child.label,
      detail: [child.status, child.priority != null ? `p${child.priority}` : null, ns]
        .filter(Boolean)
        .join(" · "),
      open: child.status !== "done" && child.status !== "cancelled",
      priority,
    });
  }
  // Open before closed, must-fix before the rest — the reader is here because something is
  // holding a landing, and that is exactly the open `!p1` findings.
  return out.sort(
    (a, b) => Number(b.open) - Number(a.open) || a.priority - b.priority,
  );
}
