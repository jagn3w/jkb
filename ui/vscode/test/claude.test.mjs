//! The Claude Code hand-off: what is queued, who takes it, and when it expires.
//
// `claude.ts` imports `vscode`, which only resolves inside a running VS Code — so the module
// is bundled here with `vscode` aliased to a stub (kept external, so the test and the bundle
// share one module instance and the recorders actually record). Nothing else is faked: the
// queue is written to a real directory, and the expiry rule is exercised by deleting a real
// worktree.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import test from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const stub = path.join(here, "vscode-stub.mjs");
const bundle = path.join(
  fs.mkdtempSync(path.join(os.tmpdir(), "jkb-claude-build-")),
  "claude.mjs",
);

await esbuild.build({
  entryPoints: [path.join(here, "..", "src", "claude.ts")],
  bundle: true,
  format: "esm",
  platform: "node",
  outfile: bundle,
  plugins: [
    {
      name: "vscode-stub",
      setup(build) {
        // `external` keeps the stub a separate module rather than inlining a second copy of
        // its state — without it every recorder in the bundle is one the test cannot see.
        build.onResolve({ filter: /^vscode$/ }, () => ({ path: stub, external: true }));
      },
    },
  ],
});

const {
  launchClaude,
  deliverQueuedPrompt,
  watchQueuedPrompts,
  sessionTerminal,
  sessionTerminalName,
  startSessionTerminal,
} = await import(bundle);
const { state, reset } = await import(stub);

/** A fresh repo, global storage and worktree factory, so no test can depend on another. */
function fresh() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-claude-"));
  const context = { globalStorageUri: { fsPath: path.join(home, "globalStorage") } };
  const queueFile = path.join(context.globalStorageUri.fsPath, "pending-prompts.json");
  const repo = path.join(home, "repo");
  fs.mkdirSync(repo, { recursive: true });
  reset(repo);
  return {
    context,
    queueFile,
    repo,
    /** The queue as it is on disk. */
    queue: () => (fs.existsSync(queueFile) ? JSON.parse(fs.readFileSync(queueFile, "utf8")) : {}),
    /** A session worktree, by its real path — the key the module stores it under. */
    worktree: (name) => {
      const dir = path.join(home, "work", name);
      fs.mkdirSync(dir, { recursive: true });
      return fs.realpathSync(dir);
    },
  };
}

const opened = (prompt) => [["claude-vscode.primaryEditor.open", undefined, prompt]];

/** A launch request under the default policy. */
const first = (worktree, uid, prompt) => ({ worktree, uid, prompt, launcher: "auto" });

test("a session opens as a window, and that window takes its prompt exactly once", async () => {
  const t = fresh();
  const work = t.worktree("task-a");

  assert.deepEqual(await launchClaude(t.context, first(work, "task:a", "PROMPT")), { where: "window" });
  assert.equal(state.calls[0][0], "vscode.openFolder");
  assert.equal(state.calls[0][1].fsPath, work);
  assert.deepEqual(state.calls[0][2], { forceNewWindow: true });
  assert.deepEqual(t.queue()[work], { uid: "task:a", prompt: "PROMPT" });

  // The window opens on the worktree, and the extension activates there.
  reset(work);
  await deliverQueuedPrompt(t.context);
  assert.deepEqual(state.calls, opened("PROMPT"));
  assert.deepEqual(t.queue(), {});

  // Reopening that window later must not start the task again.
  reset(work);
  await deliverQueuedPrompt(t.context);
  assert.deepEqual(state.calls, []);
  assert.deepEqual(state.errors, []);
});

test("two sessions in flight keep their own prompts", async () => {
  const t = fresh();
  const a = t.worktree("task-a");
  const b = t.worktree("task-b");

  await launchClaude(t.context, first(a, "task:a", "PROMPT A"));
  await launchClaude(t.context, first(b, "task:b", "PROMPT B"));
  assert.deepEqual(Object.keys(t.queue()).sort(), [a, b].sort());

  reset(b);
  await deliverQueuedPrompt(t.context);
  assert.deepEqual(state.calls, opened("PROMPT B"));
  assert.deepEqual(Object.keys(t.queue()), [a]);
});

test("a prompt whose worktree is gone expires; no clock decides it", async () => {
  const t = fresh();
  const abandoned = t.worktree("task-a");
  await launchClaude(t.context, first(abandoned, "task:a", "PROMPT A"));

  // `jkb task abandon`, or a landed session: no window can ever open on it again.
  fs.rmSync(abandoned, { recursive: true, force: true });

  reset(t.repo);
  const live = t.worktree("task-b");
  await launchClaude(t.context, first(live, "task:b", "PROMPT B"));
  assert.deepEqual(Object.keys(t.queue()), [live]);
});

test("clicking from the session's own window starts Claude in it, queueing nothing", async () => {
  const t = fresh();
  const work = t.worktree("task-a");

  reset(work);
  assert.deepEqual(await launchClaude(t.context, first(work, "task:a", "PROMPT")), { where: "here" });
  assert.deepEqual(state.calls, opened("PROMPT"));
  assert.deepEqual(t.queue(), {});
});

test("without the Claude Code extension the caller is told to fall back", async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  state.claudeInstalled = false;

  // No window is opened and nothing is queued: the caller's terminal is the whole feature,
  // and a queued prompt would fire at some unrelated later opening of that folder.
  assert.deepEqual(await launchClaude(t.context, first(work, "task:a", "PROMPT")), {
    where: "terminal",
    fallback: { missing: true, cause: "the Claude Code extension is not installed" },
  });
  assert.deepEqual(state.calls, []);
  assert.deepEqual(t.queue(), {});
});

test("a window that fails to open leaves no undeliverable prompt behind", async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  state.openFolderFails = true;

  const launch = await launchClaude(t.context, first(work, "task:a", "PROMPT"));
  assert.equal(launch.where, "terminal");
  // Not `missing`: the extension is installed, and advising an install would be wrong.
  assert.equal(launch.fallback.missing, false);
  assert.match(launch.fallback.cause, /no window/);
  assert.deepEqual(t.queue(), {});
});

test("a corrupt queue file reads as empty rather than throwing", async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  fs.mkdirSync(path.dirname(t.queueFile), { recursive: true });
  fs.writeFileSync(t.queueFile, "{not json");

  reset(work);
  await deliverQueuedPrompt(t.context);
  assert.deepEqual(state.calls, []);

  // And the next write replaces it wholesale, so one bad file is not permanent.
  reset(t.repo);
  assert.deepEqual(await launchClaude(t.context, first(work, "task:a", "PROMPT")), { where: "window" });
  assert.deepEqual(t.queue()[work], { uid: "task:a", prompt: "PROMPT" });
});

test("a Claude Code that refuses is reported, not swallowed", async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));

  reset(work);
  state.claudeRefuses = true;
  await deliverQueuedPrompt(t.context);
  assert.equal(state.errors.length, 1);
  assert.match(state.errors[0], /task:a/);
  // Taken all the same: a prompt that re-fires on every later opening of the folder is worse
  // than one click of the button, which returns the same session.
  assert.deepEqual(t.queue(), {});
});

// ---------------------------------------------------------------------------
// `jkb.taskLauncher` — the operator's choice, honoured rather than inferred.
// ---------------------------------------------------------------------------

test('launcher "terminal" opens no window and asks nothing, even for a fresh session', async () => {
  const t = fresh();
  const work = t.worktree("task-a");

  const launch = await launchClaude(t.context, {
    ...first(work, "task:a", "PROMPT"),
    launcher: "terminal",
  });
  // No `fallback`: this is what was asked for, so the caller has nothing to apologise for.
  assert.deepEqual(launch, { where: "terminal" });
  assert.deepEqual(state.calls, []);
  assert.deepEqual(t.queue(), {});
});

test('launcher "terminal" is honoured even from the session\'s own window', async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  reset(work);

  const launch = await launchClaude(t.context, {
    ...first(work, "task:a", "PROMPT"),
    launcher: "terminal",
  });
  assert.deepEqual(launch, { where: "terminal" });
  assert.deepEqual(state.calls, []);
});

test('launcher "extension" reports a failure instead of substituting a terminal', async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  state.claudeInstalled = false;

  const launch = await launchClaude(t.context, {
    ...first(work, "task:a", "PROMPT"),
    launcher: "extension",
  });
  assert.equal(launch.where, "blocked");
  assert.match(launch.cause, /not installed/);
  assert.deepEqual(t.queue(), {});
});

test('launcher "extension" blocks on every failure path, not just a missing extension', async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  state.openFolderFails = true;

  const launch = await launchClaude(t.context, {
    ...first(work, "task:a", "PROMPT"),
    launcher: "extension",
  });
  assert.equal(launch.where, "blocked");
  assert.match(launch.cause, /no window/);
  assert.deepEqual(t.queue(), {});
});

// ---------------------------------------------------------------------------
// The queue file has as few writers as possible.
// ---------------------------------------------------------------------------

test("a window with no prompt waiting does not write the queue at all", async () => {
  const t = fresh();
  // Every VS Code window now activates at startup and calls this. If each one wrote the file
  // back, the read-modify-write race would have every window in it, and the lost update it
  // allows is an already-delivered prompt resurrected.
  reset(path.join(t.repo, "unrelated"));
  await deliverQueuedPrompt(t.context);
  assert.equal(fs.existsSync(t.queueFile), false);

  // And with a queue that exists, an unrelated window leaves it byte-identical.
  const work = t.worktree("task-a");
  reset(t.repo);
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));
  const before = fs.readFileSync(t.queueFile);
  reset(path.join(t.repo, "unrelated"));
  await deliverQueuedPrompt(t.context);
  assert.deepEqual(fs.readFileSync(t.queueFile), before);
});

// ---------------------------------------------------------------------------
// Clicking twice does one thing — the guard, in place of a question nobody can answer.
// ---------------------------------------------------------------------------

/** A stand-in for `vscode.window.terminals`, which is all `sessionTerminal` reads. */
const term = (name, cwd) => ({ name, creationOptions: cwd === undefined ? {} : { cwd } });

test("a session's own claude terminal is found again, so a second click shows it", () => {
  const name = sessionTerminalName("task-a-1730");
  const found = sessionTerminal(
    [term("bash", "/w/task-a"), term(name, "/w/task-a")],
    "task-a-1730",
    "/w/task-a",
  );
  assert.equal(found?.name, name);
});

test("a Uri cwd matches as well as a string, since VS Code allows either", () => {
  const name = sessionTerminalName("task-a-1730");
  const found = sessionTerminal([term(name, { fsPath: "/w/task-a" })], "task-a-1730", "/w/task-a");
  assert.equal(found?.name, name);
});

test("In Flight's plain shell in the same worktree is not mistaken for the agent", () => {
  // `openSessionTerminal` opens a bare shell with the same cwd and a different name.
  // Converging onto it would show a shell while a claude runs unseen next door.
  assert.equal(
    sessionTerminal([term("jkb: task:use-claude-18cd", "/w/task-a")], "task-a-1730", "/w/task-a"),
    undefined,
  );
});

test("another session's terminal is not mistaken for this one", () => {
  const other = sessionTerminalName("task-b-9910");
  assert.equal(sessionTerminal([term(other, "/w/task-b")], "task-a-1730", "/w/task-a"), undefined);
});

test("the same name in a different worktree does not match", () => {
  const name = sessionTerminalName("task-a-1730");
  assert.equal(sessionTerminal([term(name, "/w/elsewhere")], "task-a-1730", "/w/task-a"), undefined);
});

test("a terminal with no cwd never matches — it says nothing about which checkout it is in", () => {
  const name = sessionTerminalName("task-a-1730");
  assert.equal(sessionTerminal([term(name, undefined)], "task-a-1730", "/w/task-a"), undefined);
});

test("the name is what createTerminal is given, so the two cannot drift apart", () => {
  // The finder matches on the name the starter sets; a literal in either place would rot.
  assert.equal(sessionTerminalName("task-a-1730"), "claude: task-a-1730");
  assert.equal(sessionTerminalName("x".repeat(40)), `claude: ${"x".repeat(24)}`);
});

// ---------------------------------------------------------------------------
// The find-or-create decision itself — the guard, not just the matcher.
// ---------------------------------------------------------------------------

/**
 * A `vscode.window` stand-in that records what was created, shown and sent, into one log the
 * caller owns — so an existing terminal's `show` and a created one's land in the same order.
 */
function host(log, existing = []) {
  return {
    terminals: existing,
    createTerminal(options) {
      log.push(["created", options.name, options.cwd]);
      return {
        show: () => log.push(["shown", options.name]),
        sendText: (text) => log.push(["sent", text]),
      };
    },
  };
}
const liveTerm = (name, cwd, log) => ({
  name,
  creationOptions: { cwd },
  show: () => log.push(["shown", name]),
});

test("a first click creates the session's terminal and sends the command", () => {
  const log = [];
  const h = host(log);
  assert.equal(startSessionTerminal(h, "task-a-1730", "/w/task-a", "claude 'GO'"), "started");
  assert.deepEqual(log, [
    ["created", "claude: task-a-1730", "/w/task-a"],
    ["shown", "claude: task-a-1730"],
    ["sent", "claude 'GO'"],
  ]);
});

test("a second click shows the existing terminal and sends NOTHING", () => {
  const log = [];
  const h = host(log, [liveTerm("claude: task-a-1730", "/w/task-a", log)]);
  // "shown" is the caller's cue to stop reporting a launch: no agent was started, and the
  // prompt this click built — carrying its own branch and land target — was not delivered.
  assert.equal(startSessionTerminal(h, "task-a-1730", "/w/task-a", "claude 'GO'"), "shown");
  assert.deepEqual(log, [["shown", "claude: task-a-1730"]]);
});

test("a terminal whose shell has exited is not reused — showing it would start nothing", () => {
  const log = [];
  const dead = { ...liveTerm("claude: task-a-1730", "/w/task-a", log), exitStatus: { code: 0 } };
  const h = host(log, [dead]);
  assert.equal(startSessionTerminal(h, "task-a-1730", "/w/task-a", "claude 'GO'"), "started");
  assert.deepEqual(log[0], ["created", "claude: task-a-1730", "/w/task-a"]);
});

test("another task's terminal is never reused, so each session gets its own", () => {
  const log = [];
  const h = host(log, [liveTerm("claude: task-b-9910", "/w/task-b", log)]);
  assert.equal(startSessionTerminal(h, "task-a-1730", "/w/task-a", "claude 'GO'"), "started");
});

test("the session's own window opens a NEW chat each click — pinned, not endorsed", async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  reset(work);

  // Claude Code exposes no way to find an existing panel, and `primaryEditor.open(undefined,…)`
  // means a fresh conversation by design — so this surface is the one place a second click is
  // not idempotent. Pinned so the docs and the code cannot drift apart again: if this ever
  // starts reusing a panel, the claim in CLAUDE.md and ui/README.md has to change with it.
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));
  assert.deepEqual(state.calls, [...opened("PROMPT"), ...opened("PROMPT")]);
  assert.deepEqual(t.queue(), {});
});

// ---------------------------------------------------------------------------
// The queue is watched, not only read at startup — VS Code reuses windows.
// ---------------------------------------------------------------------------

/** Wait for `check()` to hold, up to `ms`; fs.watch latency is real but small. */
async function until(check, ms = 4000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (check()) return true;
    await new Promise((r) => setTimeout(r, 25));
  }
  return false;
}

test("watching delivers a prompt already waiting when the window starts", async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));

  reset(work);
  const sub = watchQueuedPrompts(t.context);
  try {
    assert.ok(await until(() => state.calls.length > 0), "nothing was delivered");
    assert.deepEqual(state.calls, opened("PROMPT"));
  } finally {
    sub.dispose();
  }
});

test("a prompt queued while the window is already up is delivered without a restart", async () => {
  const t = fresh();
  const work = t.worktree("task-a");

  // This window is the session's, already running and with nothing waiting for it.
  reset(work);
  const sub = watchQueuedPrompts(t.context);
  try {
    await new Promise((r) => setTimeout(r, 200));
    assert.deepEqual(state.calls, [], "nothing was waiting, so nothing should have started");

    // A second click in the repo window queues a prompt and focuses this window — VS Code
    // reuses the window for an open folder, so nothing here activates. Only the watch sees it.
    const queued = { [work]: { uid: "task:a", prompt: "SECOND" } };
    fs.mkdirSync(path.dirname(t.queueFile), { recursive: true });
    const temp = `${t.queueFile}.tmp`;
    fs.writeFileSync(temp, JSON.stringify(queued));
    fs.renameSync(temp, t.queueFile);

    assert.ok(await until(() => state.calls.length > 0), "the queued prompt was never delivered");
    assert.deepEqual(state.calls, opened("SECOND"));
    assert.deepEqual(t.queue(), {});
  } finally {
    sub.dispose();
  }
});

test("disposing stops delivery, so a closed window does not keep taking prompts", async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  reset(work);
  watchQueuedPrompts(t.context).dispose();

  fs.mkdirSync(path.dirname(t.queueFile), { recursive: true });
  fs.writeFileSync(t.queueFile, JSON.stringify({ [work]: { uid: "task:a", prompt: "AFTER" } }));
  await new Promise((r) => setTimeout(r, 600));
  assert.deepEqual(state.calls, []);
  assert.deepEqual(t.queue()[work], { uid: "task:a", prompt: "AFTER" });
});
