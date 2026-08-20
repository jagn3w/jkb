//! Starting a task session in the Claude Code extension rather than in a terminal.
//
// The extension runs Claude in the window's **first workspace folder** — its panel derives a
// cwd from `workspaceFolders[0]` and takes no directory argument — while a task session is a
// git worktree under the repo (design D36). So a panel opened from the repo's own window
// would run Claude on the main checkout, editing the files the session exists to keep apart.
// The session gets its own VS Code window instead.
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
const CLAUDE_OPEN_COMMAND = "claude-vscode.editor.open";

/** Where a launch went, so the caller can fall back when Claude Code is not there. */
export type Launch = "here" | "window" | "unavailable";

/** A prompt waiting for the window that opens on a session's worktree. */
interface PendingPrompt {
  readonly uid: string;
  readonly prompt: string;
}

/**
 * Start Claude Code on `worktree` with `prompt` in its input.
 *
 * `"unavailable"` means the caller should fall back to a terminal: either the extension is
 * not installed, or it refused the command. It is never returned once a window has been
 * opened, so a fallback can only ever add a terminal to the window the user is already in.
 *
 * What VS Code does when a window is already open on the worktree is its call, and this does
 * not depend on knowing: the prompt is queued either way. A window that opens takes it on
 * activation; one already open leaves it queued until its next start. Both end at the same
 * prompt for the same task, which is where the button leads anyway.
 */
export async function launchClaude(
  context: vscode.ExtensionContext,
  worktree: string,
  uid: string,
  prompt: string,
): Promise<Launch> {
  if (!vscode.extensions.getExtension(CLAUDE_EXTENSION_ID)) return "unavailable";

  const here = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  if (here !== undefined && sameDirectory(here, worktree)) {
    return (await openClaudeHere(prompt)) ? "here" : "unavailable";
  }

  try {
    queuePrompt(context, worktree, { uid, prompt });
  } catch {
    // The queue IS the hand-off. Opening the window anyway would give an empty Claude panel
    // in a worktree with no sign of which task it is for; a terminal carries the prompt.
    return "unavailable";
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
  } catch {
    // No window will ever open on it, so nothing would ever take the prompt back out.
    takePrompt(context, worktree);
    return "unavailable";
  }
  return "window";
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
  if (!(await openClaudeHere(pending.prompt))) {
    vscode.window.showErrorMessage(
      `jkb: could not start Claude Code for ${pending.uid} — run \`claude\` in a terminal here, ` +
        "or click Work this task with Claude again.",
    );
  }
}

/** Open a Claude Code chat in this window with `prompt` in its input; false if it would not. */
async function openClaudeHere(prompt: string): Promise<boolean> {
  const claude = vscode.extensions.getExtension(CLAUDE_EXTENSION_ID);
  if (!claude) return false;
  try {
    // Its commands are registered by `activate`, and this runs at startup, where the order
    // two extensions activate in is not ours to assume.
    if (!claude.isActive) await claude.activate();
    // (session, prompt): no session, so a fresh conversation. The prompt lands in the input
    // box rather than being sent — the extension offers no way to submit it, and seeing what
    // is about to be asked before pressing enter is the better half of that trade.
    await vscode.commands.executeCommand(CLAUDE_OPEN_COMMAND, undefined, prompt);
    return true;
  } catch {
    return false;
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
