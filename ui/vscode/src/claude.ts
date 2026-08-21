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
  /**
   * `launcher: "extension"` and it could not be started. Report it; substitute nothing.
   *
   * `missing` rides along for the same reason it does on `terminal`: with no extension
   * installed the only remedy that honours what the operator asked for is to install it, and
   * that is precisely the branch that must not tell them to abandon their own setting.
   */
  | { readonly where: "blocked"; readonly cause: string; readonly missing: boolean };

/** What to start, and what is already known about the session. */
export interface LaunchRequest {
  readonly worktree: string;
  readonly uid: string;
  readonly prompt: string;
  /** The session's name, used to name (and later find) its terminal. */
  readonly session: string;
  readonly launcher: Launcher;
}

/**
 * A prompt waiting for the window that opens on a session's worktree.
 *
 * `session` is carried rather than re-derived from the worktree path: the delivering window
 * needs it to name the session's terminal when it has to fall back, and inferring it from the
 * last path segment would be a second, quieter definition of how a session is named.
 */
interface PendingPrompt {
  readonly uid: string;
  readonly prompt: string;
  readonly session: string;
}

/**
 * Start Claude Code on the session's worktree with `prompt` in its input.
 *
 * Returns `terminal` when the caller should run `claude` itself — either because that is what
 * the operator asked for (no `fallback`) or because Claude Code could not be reached (with
 * one) — and `blocked` when the operator ruled a terminal out. Neither is returned once a
 * window has been opened, so a fallback can only ever add a terminal to the window the user
 * is already in.
 *
 * **Clicking twice asks nothing.** The guard that would need is "is an agent live on this
 * checkout", and that is not obtainable: no API reports another window's folder, and D27/D36.6
 * deliberately refused the heartbeat that would track a live process. `resumed` is the nearest
 * available signal and it is the wrong one — a session opened yesterday and closed is equally
 * resumed — so a confirmation keyed on it fires on every ordinary return to a task and catches
 * nothing, which is how a guard becomes a reflex click.
 *
 * What replaces it is convergence, per surface. Stated exactly, because two earlier versions
 * of this comment claimed more than the code delivered:
 *
 * - `window`: **VS Code reuses the window for a folder already open in one**, so a second
 *   click focuses the session's window rather than making a second checkout. Measured on
 *   1.131 — `code --new-window <folder>` twice leaves the window count unchanged — which is
 *   the CLI entry point rather than this API, so: strong evidence, not a guarantee. What it
 *   costs is that nothing *activates* in a focused window, which is why the receiving side
 *   watches the queue (`watchQueuedPrompts`) instead of only reading it once at startup.
 * - `terminal`: `startSessionTerminal` reuses the session's own terminal and sends nothing.
 * - `here`: `primaryEditor.open(undefined, …)` means a fresh conversation by design, so a
 *   second click in the session's own window adds a second chat tab. Claude Code exposes no
 *   way to find an existing panel, so there is nothing to converge on. Not idempotent, pinned
 *   by a test so this stays true rather than becoming another claim nobody rechecked.
 *
 * What is load-bearing on all three: the handed-over prompt lands *unsent*, so no agent runs
 * without a keystroke from someone looking at the window it is in.
 */
export async function launchClaude(
  context: vscode.ExtensionContext,
  req: LaunchRequest,
): Promise<Launch> {
  const { worktree, uid, prompt, session, launcher } = req;
  // Asked for a terminal: no window and no queue. Everything below is about reaching Claude
  // Code, and none of it is something the operator asked to happen. Returning early is safe
  // only because the duplicate guard is no longer down there — it is `sessionTerminal`, which
  // the caller applies to the surface it is actually about to start.
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

  try {
    queuePrompt(context, worktree, { uid, prompt, session });
  } catch (e) {
    // The queue IS the hand-off. Opening the window anyway would give an empty Claude panel
    // in a worktree with no sign of which task it is for; a terminal carries the prompt.
    return unreachable(launcher, false, causeOf(e));
  }
  try {
    // `forceNewWindow` is load-bearing, not a preference: without it VS Code opens the folder
    // in THIS window, which shuts the current extension host down — killing the click
    // mid-command, and taking the repo's own explorer with it.
    await vscode.commands.executeCommand("vscode.openFolder", folderUri(worktree), {
      forceNewWindow: true,
    });
  } catch (e) {
    // No window will ever open on it, so nothing would ever take the prompt back out.
    takePrompt(context, worktree);
    return unreachable(launcher, false, causeOf(e));
  }
  return { where: "window" };
}

/**
 * The worktree as a URI **in this window's world**, not necessarily the local disk.
 *
 * The adapter declares no `extensionKind`, so over Remote-SSH/WSL/devcontainer it runs in the
 * workspace extension host and every path it handles is a *remote* path — `jkb` is spawned
 * there, terminals open there, the queue is written there. `Uri.file` builds an authority-less
 * `file://`, which asks the client to open the folder on the local machine: a window on a path
 * that does not exist locally, while the prompt waits in the remote queue under a key no local
 * window will ever match. Deriving from the window's own folder URI carries scheme and
 * authority across; `Uri.file` remains the fallback only when there is no folder to derive from.
 */
function folderUri(worktree: string): vscode.Uri {
  const base = vscode.workspace.workspaceFolders?.[0]?.uri;
  return base ? base.with({ path: worktree }) : vscode.Uri.file(worktree);
}

/** The terminal a session's `claude` runs in, named so a later click can find it again. */
export function sessionTerminalName(session: string): string {
  return `claude: ${session.slice(0, 24)}`;
}

/**
 * Only what matching needs, so this is testable without a running VS Code.
 *
 * `creationOptions` is left as `object` on purpose. VS Code types it as a union whose other arm
 * (an extension-owned pty) has no `cwd` at all, so `{ cwd?: … }` is a *weak* type that a real
 * `Terminal` does not satisfy — the generic then degrades to this interface and `show()` is
 * gone at the call site. The shape is narrowed where it is read instead.
 */
export interface TerminalLike {
  readonly name: string;
  readonly creationOptions: object;
  /** Set once the shell has exited; VS Code keeps the tab until it is dismissed. */
  readonly exitStatus?: unknown;
}

/** Which arm `startSessionTerminal` took. `"shown"` means **nothing was sent**. */
export type TerminalStart = "started" | "shown";

/** The slice of `vscode.window` this needs, so the decision is testable without VS Code. */
export interface TerminalHost {
  readonly terminals: readonly (TerminalLike & { show(): void })[];
  createTerminal(options: { name: string; cwd: string }): {
    show(): void;
    sendText(text: string): void;
  };
}

/**
 * Run `command` in the session's terminal, or show the one it already has.
 *
 * The decision lives here, beside the name and the matcher that make it, rather than at the
 * call site. It is the entire duplicate guard on this surface, and as three lines in a command
 * handler no test could reach it: deleting them left the build and the whole suite green,
 * which is how a guard gets removed by a later edit without anything noticing.
 *
 * The caller must distinguish the arms. `"shown"` sends nothing, so reporting it as a launch
 * would name an agent that is not running — and the prompt it declined to send carries this
 * click's branch and land target, which may differ from the one the terminal was given.
 */
export function startSessionTerminal(
  window: TerminalHost,
  session: string,
  worktree: string,
  command: string,
): TerminalStart {
  const existing = sessionTerminal(window.terminals, session, worktree);
  if (existing) {
    existing.show();
    return "shown";
  }
  const terminal = window.createTerminal({ name: sessionTerminalName(session), cwd: worktree });
  terminal.show();
  terminal.sendText(command);
  return "started";
}

/**
 * The session's own `claude` terminal, if this window already has one.
 *
 * This is what makes a second click harmless on the terminal surface, where a duplicate is
 * otherwise *certain* — `sendText` runs the command, unlike the extension path where the
 * handed-over prompt waits unsent. A tab whose *shell* has exited is skipped (`exitStatus`) —
 * showing it would start nothing at all. A live shell whose `claude` has exited cannot be told
 * from one still working, since nothing exposes that, so it matches and the user sees a shell
 * in the worktree; what makes that recoverable is the caller saying nothing was sent.
 *
 * Matched on name **and** working directory. In Flight's "Open Session Terminal" opens a plain
 * shell with the same `cwd` and a different name, and converging onto a bare shell when a
 * `claude` is running next door would be the wrong answer to the same question.
 *
 * Only this window's terminals are visible, so a `claude` started from another window is not
 * found. That residue is stated rather than closed: it is the same thing no API can observe.
 */
export function sessionTerminal<T extends TerminalLike>(
  terminals: readonly T[],
  session: string,
  worktree: string,
): T | undefined {
  const name = sessionTerminalName(session);
  return terminals.find((t) => {
    if (t.name !== name) return false;
    // A dead tab matches by name and cwd exactly as a live one does, and showing it starts
    // nothing at all — so liveness is part of the match, not a caveat in the docstring.
    if (t.exitStatus !== undefined) return false;
    const dir = directoryOf(t.creationOptions);
    return dir !== undefined && sameDirectory(dir, worktree);
  });
}

/**
 * What went wrong, as text fit to show someone.
 *
 * `executeCommand` propagates a rejection value as-is, so a command that rejects with a string
 * or a plain object makes `causeOf(e)` `undefined` — and the cause this whole shape
 * exists to carry renders as the literal "undefined". An `Error` with an empty message gives
 * "()". One helper, so all three catches agree.
 */
function causeOf(e: unknown): string {
  if (e instanceof Error && e.message) return e.message;
  const text = String(e);
  return text && text !== "undefined" ? text : "no reason given";
}

/** A terminal's working directory, from the `string | Uri` VS Code accepts — or nothing. */
function directoryOf(options: object): string | undefined {
  const cwd: unknown = "cwd" in options ? (options as { cwd?: unknown }).cwd : undefined;
  if (typeof cwd === "string") return cwd;
  if (cwd !== null && typeof cwd === "object" && "fsPath" in cwd) {
    const { fsPath } = cwd as { fsPath: unknown };
    if (typeof fsPath === "string") return fsPath;
  }
  return undefined;
}

/**
 * Claude Code was wanted and could not be started: fall back, or report, per the operator.
 *
 * One place decides, so the `extension` setting cannot be honoured on three of the four
 * failure paths and quietly ignored on the fourth.
 */
function unreachable(launcher: Launcher, missing: boolean, cause: string): Launch {
  return launcher === "extension"
    ? { where: "blocked", cause, missing }
    : { where: "terminal", fallback: { missing, cause } };
}

/**
 * Deliver queued prompts to this window: once on activation, and again whenever the queue
 * changes while the window is already up.
 *
 * The second half exists because **VS Code reuses a window for a folder already open in one**
 * — `forceNewWindow` does not override that. Measured on VS Code 1.131: `code --new-window
 * <folder>` twice leaves the window count unchanged (evidence from the CLI entry point, not
 * the extension API, so: strong, not proof). The consequence is that a second click from the
 * repo window *focuses* the session's window instead of starting an extension host, so nothing
 * activates — and a prompt queued for that worktree would sit unread until the window was next
 * opened cold, then fire with a stale branch and land target.
 *
 * Watching the file is what makes the second click land where the user is now looking. Every
 * window watches, and each takes only its own folder's entry, so the writer does not deliver
 * to itself and a window with nothing waiting does not write (see `takePrompt`) — no storm.
 */
export function watchQueuedPrompts(context: vscode.ExtensionContext): vscode.Disposable {
  void deliverQueuedPrompt(context);

  const file = queueFile(context);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let watcher: fs.FSWatcher | undefined;
  try {
    // The directory may not exist until the first prompt is queued, and `fs.watch` on a
    // missing path throws — so make it, then watch the directory rather than the file (an
    // atomic rename replaces the inode, which a file watch would stop following).
    fs.mkdirSync(path.dirname(file), { recursive: true });
    watcher = fs.watch(path.dirname(file), (_event, name) => {
      if (name !== null && name !== path.basename(file)) return;
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => void deliverQueuedPrompt(context), 300);
    });
  } catch {
    // Best-effort, exactly like the database watcher: without it a prompt still arrives on the
    // window's next cold start, which is the behaviour this improves on rather than replaces.
  }
  return new vscode.Disposable(() => {
    if (timer) clearTimeout(timer);
    watcher?.close();
  });
}

/**
 * Start Claude Code on the prompt queued for this window's folder, if there is one.
 *
 * The prompt is taken before the launch is attempted, so a window that fails to start Claude
 * does not re-attempt on every later open. Recovery is one click of "Work this task with
 * Claude" in the repo window: `jkb task work` returns the same session, so re-running it
 * costs nothing.
 */
export async function deliverQueuedPrompt(
  context: vscode.ExtensionContext,
  host: TerminalHost = vscode.window,
): Promise<void> {
  const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  if (folder === undefined) return;
  const pending = takePrompt(context, folder);
  if (!pending) return;
  const failure = await openClaudeHere(pending.prompt);
  if (failure === undefined) return;

  // This is the same "Claude Code could not be reached" question `launchClaude` asks, one
  // window over, and it goes through the same decision. It used to report and stop, which
  // under `auto` withheld the terminal the policy promises — in the one window whose folder
  // already *is* the worktree, so a terminal is trivially available — and under `extension`
  // advised running `claude` by hand, the surface the operator ruled out.
  const outcome = unreachable(taskLauncher(), false, failure);
  if (outcome.where === "blocked") {
    vscode.window.showErrorMessage(
      `jkb: could not start Claude Code for ${pending.uid} (${failure}). ` +
        (outcome.missing
          ? "Install the Claude Code extension"
          : 'Set jkb.taskLauncher to "auto" to fall back to a terminal') +
        ", then click Work this task with Claude again.",
    );
    return;
  }
  const arm = startSessionTerminal(
    host,
    pending.session,
    folder,
    claudeCommand(pending.prompt),
  );
  vscode.window.showInformationMessage(
    arm === "started"
      ? `jkb: could not start Claude Code (${failure}) — ran \`claude\` in a terminal instead.`
      : "jkb: this session already has a `claude` terminal — showed it, and sent nothing.",
  );
}

/**
 * How the operator wants a session started; anything unrecognised reads as the default.
 *
 * Lives here, beside the policy it feeds, so the two windows that ask the question — the one
 * clicking and the one receiving — cannot answer it from two different readings of the setting.
 */
export function taskLauncher(): Launcher {
  const choice = vscode.workspace.getConfiguration("jkb").get<string>("taskLauncher");
  return choice === "extension" || choice === "terminal" ? choice : "auto";
}

/**
 * The shell command that starts an agent on `prompt`.
 *
 * Exported so the two windows that can start one — the clicking window and the receiving
 * window falling back — build the identical line. Two copies of a quoting rule is one copy
 * that eventually differs, and the difference would be in shell quoting of a prompt carrying
 * task titles and branch names.
 */
export function claudeCommand(prompt: string): string {
  return `claude ${shellQuote(prompt)}`;
}

/** Single-quote a string for safe use in a POSIX shell. */
function shellQuote(s: string): string {
  return `'${s.replace(/'/g, "'\\''")}'`;
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
    return causeOf(e);
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
  const key = directoryKey(worktree);
  queue[key] = pending;
  // `key` is exempt from the expiry sweep below. Without that, a worktree removed outside jkb
  // — an interrupted land, a stray rm -rf — makes the sweep drop the very entry it was handed
  // and return normally, so the launch reports "opening its window" having stored nothing.
  // A store that discarded what it was given must not report success.
  writeQueue(context, queue, key);
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
      // `session` post-dates the first release of this file, so an entry written by an older
      // build has none. Default it from the worktree's last segment — the shape `jkb task
      // work` builds — rather than dropping a prompt someone is waiting on.
      const session = typeof entry.session === "string" ? entry.session : path.basename(key);
      queue[key] = { uid: entry.uid, prompt: entry.prompt, session };
    }
  }
  return queue;
}

function writeQueue(
  context: vscode.ExtensionContext,
  queue: Record<string, PendingPrompt>,
  keep?: string,
): void {
  // A worktree that is gone — `jkb task abandon`, or a landed session — can never open a
  // window, so its prompt can never be taken. That is the whole expiry rule: the entries
  // that expire are exactly the undeliverable ones, and no clock decides it.
  const live: Record<string, PendingPrompt> = {};
  for (const [dir, pending] of Object.entries(queue)) {
    if (dir === keep || fs.existsSync(dir)) live[dir] = pending;
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
    throw new Error(`could not write ${file}: ${causeOf(e)}`);
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
