//! Starting a task session in the Claude Code extension, which by default means its own window.
//
// The extension runs Claude in the window's **first workspace folder** — its panel derives a
// cwd from `workspaceFolders[0]` and takes no directory argument — while a task session is a
// git worktree under the repo (design D36). So a panel opened from the repo's own window
// would run Claude on the main checkout, editing the files the session exists to keep apart.
// The session gets its own VS Code window instead.
//
// Which surface is used at all is the operator's, via `jkb.taskLauncher` (see `Launcher`): a
// terminal in the worktree is a supported answer, not a degraded one, and everything below
// about windows and hand-off applies only when Claude Code is the chosen surface.
//
// `vscode.openFolder` carries no payload and the new window is a different extension host, so
// the prompt is handed over through a small queue in global storage: the opening window
// writes it, the opened one takes it on activation. That is why the extension activates
// `onStartupFinished` — a window nobody has clicked the jkb icon in still has a prompt to
// deliver.

import * as fs from "node:fs";
import * as path from "node:path";

import * as vscode from "vscode";

/** The Claude Code extension, and the command that opens a chat with an initial prompt. */
const CLAUDE_EXTENSION_ID = "anthropic.claude-code";
// `primaryEditor.open`, deliberately, and NOT `editor.open`. The latter is registered as
// `(session, prompt, column) => { if (column !== ViewColumn.Active) setPreferredLocation("panel"); … }`
// and `setPreferredLocation` writes `claudeCode.preferredLocation` with
// `ConfigurationTarget.Global` — so calling it without a column silently rewrites the user's
// GLOBAL settings on every hand-off, moving Claude Code out of their sidebar for every window
// and every project. jkb does not edit other people's configuration. `primaryEditor.open` is
// `createPanel(session, prompt, ViewColumn.Active)` with no such write, and is what Claude
// Code's own `vscode://…/open?prompt=` URI handler calls.
const CLAUDE_OPEN_COMMAND = "claude-vscode.primaryEditor.open";

/**
 * How the operator wants a session started (`jkb.taskLauncher`).
 *
 * The Claude Code extension is the better surface, not an obligation: someone who works in a
 * terminal, or does not want a window per task, says so once instead of being handed a
 * workflow. `auto` is the default and is the only value that silently substitutes one for the
 * other — the two explicit values are honoured or reported, never quietly swapped.
 */
export type Launcher = "auto" | "extension" | "terminal";

/** Where a launch went, and why it did not reach Claude Code when it did not. */
export type Launch =
  | { readonly where: "here" }
  | { readonly where: "window" }
  /** The user declined to reopen a session that may already have a window. */
  | { readonly where: "declined" }
  /**
   * The caller should start `claude` in a terminal. `fallback` is absent when that is what the
   * operator asked for — there is nothing to apologise for — and present when Claude Code was
   * wanted and could not be had. `missing` then distinguishes "not installed", the one case
   * where advising an install helps, from a failure that has a `cause`.
   */
  | {
      readonly where: "terminal";
      readonly fallback?: { readonly missing: boolean; readonly cause: string };
    }
  /** `launcher: "extension"` and it could not be started. Report it; substitute nothing. */
  | { readonly where: "blocked"; readonly cause: string };

/** What to start, and what is already known about the session. */
export interface LaunchRequest {
  readonly worktree: string;
  readonly uid: string;
  readonly prompt: string;
  /**
   * `jkb task work` returned an existing session, so a window may already be open on it.
   * It is evidence, not proof — a session opened yesterday and closed is also `resumed` —
   * which is why this asks rather than deciding.
   */
  readonly resumed: boolean;
  readonly launcher: Launcher;
}

/** A prompt waiting for the window that opens on a session's worktree. */
interface PendingPrompt {
  readonly uid: string;
  readonly prompt: string;
}

/**
 * Start Claude Code on the session's worktree with `prompt` in its input.
 *
 * `where: "unavailable"` means the caller should fall back to a terminal. It is never returned
 * once a window has been opened, so a fallback can only ever add a terminal to the window the
 * user is already in.
 *
 * A **resumed** session is asked about rather than reopened. Forcing a window would either put
 * a second Claude chat on a checkout the first is mid-edit on — the hazard D36 exists to
 * prevent, one level up from the session — or, if VS Code focuses the existing window instead,
 * deliver nothing and say nothing. Nothing observable tells the two apart from here: no API
 * reports another window's folder, and `resumed` is about the *session*, not the window. So
 * the person who can see their own windows is the one asked.
 */
export async function launchClaude(
  context: vscode.ExtensionContext,
  req: LaunchRequest,
): Promise<Launch> {
  const { worktree, uid, prompt, launcher } = req;
  // Asked for a terminal: no window, no queue, no question. The rest of this function is about
  // reaching Claude Code, and none of it is something the operator asked to happen.
  if (launcher === "terminal") return { where: "terminal" };

  if (!vscode.extensions.getExtension(CLAUDE_EXTENSION_ID)) {
    return unreachable(launcher, true, "the Claude Code extension is not installed");
  }

  const here = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  if (here !== undefined && sameDirectory(here, worktree)) {
    // This window *is* the session, so there is nothing to ask and nothing to hand over.
    const failure = await openClaudeHere(prompt);
    return failure === undefined ? { where: "here" } : unreachable(launcher, false, failure);
  }

  if (req.resumed && !(await confirmReopen(worktree))) return { where: "declined" };

  try {
    queuePrompt(context, worktree, { uid, prompt });
  } catch (e) {
    // The queue IS the hand-off. Opening the window anyway would give an empty Claude panel
    // in a worktree with no sign of which task it is for; a terminal carries the prompt.
    return unreachable(launcher, false, (e as Error).message);
  }
  try {
    // `forceNewWindow` is load-bearing, not a preference: without it VS Code opens the folder
    // in THIS window, which shuts the current extension host down — killing the click
    // mid-command, and taking the repo's own explorer with it.
    await vscode.commands.executeCommand(
      "vscode.openFolder",
      vscode.Uri.file(worktree),
      { forceNewWindow: true },
    );
  } catch (e) {
    // No window will ever open on it, so nothing would ever take the prompt back out.
    takePrompt(context, worktree);
    return unreachable(launcher, false, (e as Error).message);
  }
  return { where: "window" };
}

/**
 * Claude Code was wanted and could not be started: fall back, or report, per the operator.
 *
 * One place decides, so the `extension` setting cannot be honoured on three of the four
 * failure paths and quietly ignored on the fourth.
 */
function unreachable(launcher: Launcher, missing: boolean, cause: string): Launch {
  return launcher === "extension"
    ? { where: "blocked", cause }
    : { where: "terminal", fallback: { missing, cause } };
}

/** Ask before reopening a session that may already have a window; false means leave it alone. */
async function confirmReopen(worktree: string): Promise<boolean> {
  const choice = await vscode.window.showInformationMessage(
    `jkb: ${path.basename(worktree)} is already open as a session. If its window is up, switch ` +
      "to it — reopening starts a second Claude on the same checkout.",
    "Open its window",
  );
  return choice === "Open its window";
}

/**
 * Start Claude Code on the prompt queued for this window's folder, if there is one.
 *
 * The prompt is taken before the launch is attempted, so a window that fails to start Claude
 * does not re-attempt on every later open. Recovery is one click of "Work this task with
 * Claude" in the repo window: `jkb task work` returns the same session, so re-running it
 * costs nothing.
 */
export async function deliverQueuedPrompt(context: vscode.ExtensionContext): Promise<void> {
  const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  if (folder === undefined) return;
  const pending = takePrompt(context, folder);
  if (!pending) return;
  const failure = await openClaudeHere(pending.prompt);
  if (failure !== undefined) {
    vscode.window.showErrorMessage(
      `jkb: could not start Claude Code for ${pending.uid} (${failure}) — run \`claude\` in a ` +
        "terminal here, or click Work this task with Claude again.",
    );
  }
}

/**
 * Open a Claude Code chat in this window with `prompt` in its input.
 *
 * Returns `undefined` on success, or why it failed — the cause is what the user needs and what
 * a swallowed `catch` throws away, leaving every failure to be reported as the one cause that
 * happens to have advice attached.
 */
async function openClaudeHere(prompt: string): Promise<string | undefined> {
  const claude = vscode.extensions.getExtension(CLAUDE_EXTENSION_ID);
  if (!claude) return "the Claude Code extension is not installed";
  try {
    // Its commands are registered by `activate`, and this runs at startup, where the order
    // two extensions activate in is not ours to assume.
    if (!claude.isActive) await claude.activate();
    // (session, prompt): no session, so a fresh conversation. The prompt lands in the input
    // box rather than being sent — the extension offers no way to submit it, and seeing what
    // is about to be asked before pressing enter is the better half of that trade.
    await vscode.commands.executeCommand(CLAUDE_OPEN_COMMAND, undefined, prompt);
    return undefined;
  } catch (e) {
    return (e as Error).message;
  }
}

// ---------------------------------------------------------------------------
// The hand-off queue: one JSON file in global storage, keyed by worktree.
// ---------------------------------------------------------------------------

function queuePrompt(
  context: vscode.ExtensionContext,
  worktree: string,
  pending: PendingPrompt,
): void {
  const queue = readQueue(context);
  queue[directoryKey(worktree)] = pending;
  writeQueue(context, queue);
}

/** Remove and return the prompt queued for `dir`, if any. */
function takePrompt(
  context: vscode.ExtensionContext,
  dir: string,
): PendingPrompt | undefined {
  const queue = readQueue(context);
  const key = directoryKey(dir);
  const pending = queue[key];
  // Nothing here: return without writing. Every VS Code window now activates at startup and
  // calls this, so writing anyway would make each of them a read-modify-write of one shared
  // file — and the lost update that race allows is an already-delivered prompt written back.
  // Pruning rides on genuine writes instead, which is enough: an entry whose worktree is gone
  // is undeliverable whether or not it is still in the file.
  if (!pending) return undefined;
  delete queue[key];
  try {
    writeQueue(context, queue);
  } catch {
    // The prompt is already in hand, and delivering it matters more than the bookkeeping.
    // The cost of not clearing it is that the next window on this worktree starts Claude
    // again — the same prompt for the same task, which is where the button leads anyway.
  }
  return pending;
}

function queueFile(context: vscode.ExtensionContext): string {
  return path.join(context.globalStorageUri.fsPath, "pending-prompts.json");
}

/** The queue as it is on disk — every window shares one file, so it is re-read each time. */
function readQueue(context: vscode.ExtensionContext): Record<string, PendingPrompt> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(fs.readFileSync(queueFile(context), "utf8"));
  } catch {
    // Missing (nothing queued yet) or unreadable: either way there is no prompt to deliver,
    // and the next write replaces the file wholesale.
    return {};
  }
  if (typeof parsed !== "object" || parsed === null) return {};
  const queue: Record<string, PendingPrompt> = {};
  for (const [key, value] of Object.entries(parsed as Record<string, unknown>)) {
    const entry = value as Partial<PendingPrompt>;
    if (typeof entry?.uid === "string" && typeof entry?.prompt === "string") {
      queue[key] = { uid: entry.uid, prompt: entry.prompt };
    }
  }
  return queue;
}

function writeQueue(
  context: vscode.ExtensionContext,
  queue: Record<string, PendingPrompt>,
): void {
  // A worktree that is gone — `jkb task abandon`, or a landed session — can never open a
  // window, so its prompt can never be taken. That is the whole expiry rule: the entries
  // that expire are exactly the undeliverable ones, and no clock decides it.
  const live: Record<string, PendingPrompt> = {};
  for (const [dir, pending] of Object.entries(queue)) {
    if (fs.existsSync(dir)) live[dir] = pending;
  }
  const file = queueFile(context);
  try {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    // Written aside and renamed: another window may be reading this file, and a half-written
    // one would read as an empty queue and silently drop a prompt. Two windows writing in the
    // same instant can still lose the earlier entry — untreated, because a write happens only
    // on a click or on a window opening, and losing one costs a second click.
    const temp = `${file}.${process.pid}.tmp`;
    fs.writeFileSync(temp, JSON.stringify(live, null, 2));
    fs.renameSync(temp, file);
  } catch (e) {
    // Queuing is the whole mechanism, so a failure here must not pass for a queued prompt.
    throw new Error(`could not write ${file}: ${(e as Error).message}`);
  }
}

/** A directory's identity: resolved through symlinks, so two spellings of one path match. */
function directoryKey(dir: string): string {
  try {
    return fs.realpathSync(path.resolve(dir));
  } catch {
    return path.resolve(dir);
  }
}

function sameDirectory(a: string, b: string): boolean {
  return directoryKey(a) === directoryKey(b);
}
