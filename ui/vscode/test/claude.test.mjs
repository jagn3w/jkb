//! The Claude Code hand-off: what is queued, who takes it, and when it expires.
//
// `claude.ts` imports `vscode`, which only resolves inside a running VS Code — so the module
// is bundled here with `vscode` aliased to a stub (kept external, so the test and the bundle
// share one module instance and the recorders actually record). Nothing else is faked: the
// queue is written to a real directory, and the expiry rule is exercised by deleting a real
// worktree.

import assert from "node:assert/strict";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const stub = path.join(here, "vscode-stub.mjs");
const buildDir = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-claude-build-"));
const bundle = path.join(buildDir, "claude.mjs");
// The per-test homes are cleaned by `fresh`; this one is module-scope and was not, so every
// run — every `pnpm run test`, every check.sh, every CI job — still left one tree behind.
after(() => fs.rmSync(buildDir, { recursive: true, force: true }));

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
function fresh(ctx) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-claude-"));
  // Removed with the test that made it. Without this the suite leaves one tree per test —
  // 31 of them a run, and 1721 had accumulated in TMPDIR over this branch's development.
  ctx?.after(() => fs.rmSync(home, { recursive: true, force: true }));
  const context = { globalStorageUri: { fsPath: path.join(home, "globalStorage") } };
  const queueDir = path.join(context.globalStorageUri.fsPath, "pending");
  /** The file a given worktree's entry lands in — the code names it by hash of the key. */
  const fileFor = (key) =>
    path.join(queueDir, `${crypto.createHash("sha1").update(key).digest("hex")}.json`);
  const repo = path.join(home, "repo");
  fs.mkdirSync(repo, { recursive: true });
  reset(repo);
  return {
    context,
    queueDir,
    fileFor,
    repo,
    /** The queue as it is on disk, keyed by worktree — one file per waiting prompt. */
    queue: () => {
      const out = {};
      let names = [];
      try {
        names = fs.readdirSync(queueDir);
      } catch {
        return out;
      }
      for (const name of names.filter((n) => n.endsWith(".json"))) {
        const e = JSON.parse(fs.readFileSync(path.join(queueDir, name), "utf8"));
        out[e.key] = e;
      }
      return out;
    },
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
const first = (worktree, uid, prompt) => ({
  worktree,
  uid,
  prompt,
  session: path.basename(worktree),
  launcher: "auto",
});

/** What a queued entry looks like on disk, so an omitted field cannot pass for a match. */
const entry = (worktree, uid, prompt) => ({
  uid,
  prompt,
  session: path.basename(worktree),
  key: worktree,
});

test("a session opens as a window, and that window takes its prompt exactly once", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");

  assert.deepEqual(await launchClaude(t.context, first(work, "task:a", "PROMPT")), { where: "window" });
  assert.equal(state.calls[0][0], "vscode.openFolder");
  assert.equal(state.calls[0][1].fsPath, work);
  assert.deepEqual(state.calls[0][2], { forceNewWindow: true });
  assert.deepEqual(t.queue()[work], entry(work, "task:a", "PROMPT"));

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

test("two sessions in flight keep their own prompts", async (ctx) => {
  const t = fresh(ctx);
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

test("a prompt whose worktree is gone expires; no clock decides it", async (ctx) => {
  const t = fresh(ctx);
  const abandoned = t.worktree("task-a");
  await launchClaude(t.context, first(abandoned, "task:a", "PROMPT A"));

  // `jkb task abandon`, or a landed session: no window can ever open on it again.
  fs.rmSync(abandoned, { recursive: true, force: true });

  reset(t.repo);
  const live = t.worktree("task-b");
  await launchClaude(t.context, first(live, "task:b", "PROMPT B"));
  assert.deepEqual(Object.keys(t.queue()), [live]);
});

test("clicking from the session's own window starts Claude in it, queueing nothing", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");

  reset(work);
  assert.deepEqual(await launchClaude(t.context, first(work, "task:a", "PROMPT")), { where: "here" });
  assert.deepEqual(state.calls, opened("PROMPT"));
  assert.deepEqual(t.queue(), {});
});

test("without the Claude Code extension the caller is told to fall back", async (ctx) => {
  const t = fresh(ctx);
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

test("a window that fails to open leaves no undeliverable prompt behind", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  state.openFolderFails = true;

  const launch = await launchClaude(t.context, first(work, "task:a", "PROMPT"));
  assert.equal(launch.where, "terminal");
  // Not `missing`: the extension is installed, and advising an install would be wrong.
  assert.equal(launch.fallback.missing, false);
  assert.match(launch.fallback.cause, /no window/);
  assert.deepEqual(t.queue(), {});
});

test("a corrupt queue file reads as empty rather than throwing", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  fs.mkdirSync(t.queueDir, { recursive: true });
  fs.writeFileSync(t.fileFor(work), "{not json");

  reset(work);
  await deliverQueuedPrompt(t.context);
  assert.deepEqual(state.calls, []);

  // And the next write replaces it wholesale, so one bad file is not permanent.
  reset(t.repo);
  assert.deepEqual(await launchClaude(t.context, first(work, "task:a", "PROMPT")), { where: "window" });
  assert.deepEqual(t.queue()[work], entry(work, "task:a", "PROMPT"));
});

test("a Claude Code that refuses falls back here, rather than reporting and stopping", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));

  reset(work);
  state.claudeRefuses = true;
  await deliverQueuedPrompt(t.context);
  // Under `auto` the policy for "extension not usable" is a terminal, and this window's folder
  // already IS the worktree — so reporting and stopping withheld the one thing it could do.
  assert.ok(
    state.calls.some(([kind]) => kind === "createTerminal"),
    "no terminal was started in the window that could trivially start one",
  );
  assert.ok(state.notices.some((m) => /terminal/.test(m)), "the fallback was not reported");
  // Taken all the same: a prompt that re-fires on every later opening of the folder is worse
  // than one click of the button, which returns the same session.
  assert.deepEqual(t.queue(), {});
});

// ---------------------------------------------------------------------------
// `jkb.taskLauncher` — the operator's choice, honoured rather than inferred.
// ---------------------------------------------------------------------------

test('launcher "terminal" opens no window and asks nothing, even for a fresh session', async (ctx) => {
  const t = fresh(ctx);
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

test('launcher "terminal" is honoured even from the session\'s own window', async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  reset(work);

  const launch = await launchClaude(t.context, {
    ...first(work, "task:a", "PROMPT"),
    launcher: "terminal",
  });
  assert.deepEqual(launch, { where: "terminal" });
  assert.deepEqual(state.calls, []);
});

test('launcher "extension" reports a failure instead of substituting a terminal', async (ctx) => {
  const t = fresh(ctx);
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

test('launcher "extension" blocks on every failure path, not just a missing extension', async (ctx) => {
  const t = fresh(ctx);
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

test("a window with no prompt waiting does not write the queue at all", async (ctx) => {
  const t = fresh(ctx);
  // Every VS Code window now activates at startup and calls this. If each one wrote the file
  // back, the read-modify-write race would have every window in it, and the lost update it
  // allows is an already-delivered prompt resurrected.
  reset(path.join(t.repo, "unrelated"));
  await deliverQueuedPrompt(t.context);
  assert.equal(fs.existsSync(t.queueDir) && fs.readdirSync(t.queueDir).length > 0, false);

  // And with a queue that exists, an unrelated window leaves it byte-identical.
  const work = t.worktree("task-a");
  reset(t.repo);
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));
  const before = fs.readFileSync(t.fileFor(work));
  reset(path.join(t.repo, "unrelated"));
  await deliverQueuedPrompt(t.context);
  // An unrelated window reads only its own (absent) file and writes nothing at all.
  assert.deepEqual(fs.readFileSync(t.fileFor(work)), before);
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

test("the session's own window opens a NEW chat each click — pinned, not endorsed", async (ctx) => {
  const t = fresh(ctx);
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

test("watching delivers a prompt already waiting when the window starts", async (ctx) => {
  const t = fresh(ctx);
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

test("a prompt queued while the window is already up is delivered without a restart", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");

  // This window is the session's, already running and with nothing waiting for it.
  reset(work);
  const sub = watchQueuedPrompts(t.context);
  try {
    await new Promise((r) => setTimeout(r, 200));
    assert.deepEqual(state.calls, [], "nothing was waiting, so nothing should have started");

    // A second click in the repo window queues a prompt and focuses this window — VS Code
    // reuses the window for an open folder, so nothing here activates. Only the watch sees it.
    fs.mkdirSync(t.queueDir, { recursive: true });
    const temp = `${t.fileFor(work)}.tmp`;
    fs.writeFileSync(temp, JSON.stringify(entry(work, "task:a", "SECOND")));
    fs.renameSync(temp, t.fileFor(work));

    assert.ok(await until(() => state.calls.length > 0), "the queued prompt was never delivered");
    assert.deepEqual(state.calls, opened("SECOND"));
    assert.deepEqual(t.queue(), {});
  } finally {
    sub.dispose();
  }
});

test("disposing stops delivery, so a closed window does not keep taking prompts", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  reset(work);
  watchQueuedPrompts(t.context).dispose();

  fs.mkdirSync(t.queueDir, { recursive: true });
  fs.writeFileSync(t.fileFor(work), JSON.stringify(entry(work, "task:a", "AFTER")));
  await new Promise((r) => setTimeout(r, 600));
  assert.deepEqual(state.calls, []);
  assert.deepEqual(t.queue()[work], entry(work, "task:a", "AFTER"));
});

// ---------------------------------------------------------------------------
// Round 5: one test per fix that reverted green in review 4.
// ---------------------------------------------------------------------------

test("a remote window opens the worktree on the remote, not on the local disk", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  // Remote-SSH: the adapter runs in the workspace extension host, so every path it holds is a
  // remote one. Uri.file would build an authority-less file:// and open it locally — a window
  // on a path that does not exist there, while the prompt waits in the remote queue.
  state.scheme = "vscode-remote";
  state.authority = "ssh-remote+box";

  await launchClaude(t.context, first(work, "task:a", "PROMPT"));
  const [, target] = state.calls[0];
  assert.equal(target.scheme, "vscode-remote");
  assert.equal(target.authority, "ssh-remote+box");
  assert.equal(target.path, work);
});

test("queuing never sweeps away the entry it was just handed", async (ctx) => {
  const t = fresh(ctx);
  // A worktree removed outside jkb — an interrupted land, a stray rm -rf. The expiry sweep
  // must not drop the entry this very write stored and then let the launch report success.
  const gone = path.join(t.repo, "vanished");
  const launch = await launchClaude(t.context, first(gone, "task:a", "PROMPT"));
  assert.deepEqual(launch, { where: "window" });
  assert.deepEqual(t.queue()[path.resolve(gone)], entry(path.resolve(gone), "task:a", "PROMPT"));
});

test("an entry whose worktree is gone is swept when the next one is queued", async (ctx) => {
  const t = fresh(ctx);
  const doomed = t.worktree("task-a");
  await launchClaude(t.context, first(doomed, "task:a", "A"));
  fs.rmSync(doomed, { recursive: true, force: true });

  const live = t.worktree("task-b");
  await launchClaude(t.context, first(live, "task:b", "B"));
  assert.deepEqual(Object.keys(t.queue()), [live]);
});

test("the session name reaches the terminal the receiving window falls back to", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  await launchClaude(t.context, { ...first(work, "task:a", "PROMPT"), session: "sess-xyz" });
  assert.equal(t.queue()[work].session, "sess-xyz");

  reset(work);
  state.claudeRefuses = true;
  const log = [];
  await deliverQueuedPrompt(t.context, host(log));
  // Named from the queued session, not re-derived from the worktree's last path segment.
  assert.deepEqual(log[0], ["created", "claude: sess-xyz", work]);
});

test("an entry missing a required field is ignored rather than half-delivered", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  fs.mkdirSync(t.queueDir, { recursive: true });
  fs.writeFileSync(t.fileFor(work), JSON.stringify({ uid: "task:a", prompt: "P" }));

  reset(work);
  await deliverQueuedPrompt(t.context);
  assert.deepEqual(state.calls, []);
});

test('launcher "extension" with no extension advises installing it, not switching away', async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  state.claudeInstalled = false;
  const launch = await launchClaude(t.context, {
    ...first(work, "task:a", "PROMPT"),
    launcher: "extension",
  });
  assert.equal(launch.where, "blocked");
  // Without `missing` the only remedy offered is abandoning the setting the operator chose.
  assert.equal(launch.missing, true);
});

test("a rejection that is not an Error still yields a readable cause", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  reset(work);
  state.claudeRefuses = "rejected with a bare string, as executeCommand may";
  const launch = await launchClaude(t.context, first(work, "task:a", "PROMPT"));
  assert.equal(launch.where, "terminal");
  // The text itself must survive. "not the literal undefined, and non-empty" is satisfied by
  // any fixed placeholder, so it could not tell a carried cause from a discarded one.
  assert.match(launch.fallback.cause, /bare string/);
});

test("the delivering window honours launcher terminal without touching the extension", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));

  // Queued under auto, delivered after the operator switched to terminal.
  reset(work);
  state.launcher = "terminal";
  const log = [];
  await deliverQueuedPrompt(t.context, host(log));
  assert.deepEqual(state.calls, [], "the chat surface was opened despite the setting");
  assert.deepEqual(log[0], ["created", `claude: ${path.basename(work)}`, work]);
});

test("the shown arm still reports why Claude Code failed", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));

  reset(work);
  state.claudeRefuses = true;
  const log = [];
  const existing = { name: `claude: ${path.basename(work)}`, creationOptions: { cwd: work },
    show: () => log.push(["shown"]) };
  await deliverQueuedPrompt(t.context, host(log, [existing]));
  // The arm has to be asserted too: the created arm's message matches the same regex, so if
  // sessionTerminal stopped matching, this test would stay green while pinning the opposite.
  assert.deepEqual(log, [["shown"]]);
  assert.ok(state.notices.some((m) => /refused|could not/i.test(m)), state.notices.join(" | "));
});

test("the Claude extension is activated before its command is used", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  reset(work);
  // `onStartupFinished` activates jkb-explorer and Claude Code concurrently, and the order is
  // not ours to assume: until Claude Code's activate() has run, its command is unregistered.
  state.claudeActive = false;

  assert.deepEqual(await launchClaude(t.context, first(work, "task:a", "PROMPT")), {
    where: "here",
  });
  assert.deepEqual(state.calls, [
    ["activate", "anthropic.claude-code"],
    ["claude-vscode.primaryEditor.open", undefined, "PROMPT"],
  ]);
});

test("a window with no workspace folder delivers nothing and does not throw", async (ctx) => {
  const t = fresh(ctx);
  // The empty window `onStartupFinished` now activates in — the only case that reaches
  // deliverQueuedPrompt's `folder === undefined` guard.
  reset(undefined);
  await deliverQueuedPrompt(t.context);
  assert.deepEqual(state.calls, []);
});

// ---------------------------------------------------------------------------
// Round 6: the receiving window's `blocked` arm, and the default terminal host.
// ---------------------------------------------------------------------------

test('the receiving window refuses under "extension" and advises installing it', async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));

  // Queued while Claude Code was present; by delivery it is gone and the operator has ruled
  // out a terminal. The whole `blocked` arm of the receiving window was deletable with the
  // suite green, and so was the `missing: true` that decides which remedy is offered.
  reset(work);
  state.launcher = "extension";
  state.claudeInstalled = false;
  const log = [];
  await deliverQueuedPrompt(t.context, host(log));

  assert.deepEqual(log, [], "a terminal was started for an operator who ruled it out");
  assert.equal(state.errors.length, 1);
  assert.match(state.errors[0], /Install the Claude Code extension/);
  assert.doesNotMatch(state.errors[0], /set jkb.taskLauncher/i);
  // The caller's context clause is the only thing naming WHICH task failed; the shared
  // template can drop it without any other assertion noticing.
  assert.match(state.errors[0], /task:a/);
});

test("the receiving window falls back through the real vscode.window by default", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");

  // No host argument: this is the production seam, and the only path where sendText actually
  // runs. The folder is moved by hand rather than via reset(), which clears the terminal list
  // — the very state this is about. Queue from the repo window, deliver in the session's.
  reset(t.repo);
  state.claudeRefuses = true;
  const queueThenDeliver = async (prompt) => {
    state.folder = t.repo;
    await launchClaude(t.context, first(work, "task:a", prompt));
    state.folder = work;
    await deliverQueuedPrompt(t.context);
  };
  const created = () => state.calls.filter(([kind]) => kind === "createTerminal").length;

  await queueThenDeliver("ONE");
  assert.equal(created(), 1, "the first delivery did not start a terminal");

  await queueThenDeliver("TWO");
  assert.equal(created(), 1, "a second agent was started in the same worktree");
});

// ---------------------------------------------------------------------------
// Round 7: the reportTerminal arms, measured to be unheld by any test.
// ---------------------------------------------------------------------------

test("the auto fallback tells the operator a chat was available", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));

  // The commonest non-happy path: auto, no extension. Deleting the install advice left the
  // suite green, so the operator could silently lose the only hint that a chat surface exists.
  reset(work);
  state.claudeInstalled = false;
  const log = [];
  await deliverQueuedPrompt(t.context, host(log));
  assert.equal(log.filter(([k]) => k === "created").length, 1);
  assert.ok(
    state.notices.some((m) => /Install the Claude Code extension/.test(m)),
    state.notices.join(" | "),
  );
});

test("a terminal-launcher delivery onto an existing terminal still says nothing was sent", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));

  // launcher: terminal has no fallback to report, so the only thing reportTerminal has to say
  // on this path is the shown arm — and dropping the wrapper entirely left 41 tests green.
  reset(work);
  state.launcher = "terminal";
  const log = [];
  const existing = {
    name: `claude: ${path.basename(work)}`,
    creationOptions: { cwd: work },
    show: () => log.push(["shown"]),
  };
  await deliverQueuedPrompt(t.context, host(log, [existing]));
  assert.deepEqual(log, [["shown"]]);
  assert.equal(state.notices.length, 1, "the shown arm reported nothing");
});

test("the shown arm says how to recover the prompt it did not deliver", async (ctx) => {
  const t = fresh(ctx);
  const work = t.worktree("task-a");
  await launchClaude(t.context, first(work, "task:a", "PROMPT"));

  reset(work);
  const log = [];
  const existing = {
    name: `claude: ${path.basename(work)}`,
    creationOptions: { cwd: work },
    show: () => log.push(["shown"]),
  };
  state.launcher = "terminal";
  await deliverQueuedPrompt(t.context, host(log, [existing]));
  // The entry was consumed and nothing was sent, so the prompt is gone. Clicking again lands
  // on this same arm, which is why the advice has to be to clear the terminal first.
  assert.deepEqual(t.queue(), {});
  const notice = state.notices.join(" | ");
  assert.match(notice, /not delivered/);
  assert.match(notice, /Close that terminal/);
});
