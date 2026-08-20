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

const { launchClaude, deliverQueuedPrompt } = await import(bundle);
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

const opened = (prompt) => [["claude-vscode.editor.open", undefined, prompt]];

test("a session opens as a window, and that window takes its prompt exactly once", async () => {
  const t = fresh();
  const work = t.worktree("task-a");

  assert.equal(await launchClaude(t.context, work, "task:a", "PROMPT"), "window");
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

  await launchClaude(t.context, a, "task:a", "PROMPT A");
  await launchClaude(t.context, b, "task:b", "PROMPT B");
  assert.deepEqual(Object.keys(t.queue()).sort(), [a, b].sort());

  reset(b);
  await deliverQueuedPrompt(t.context);
  assert.deepEqual(state.calls, opened("PROMPT B"));
  assert.deepEqual(Object.keys(t.queue()), [a]);
});

test("a prompt whose worktree is gone expires; no clock decides it", async () => {
  const t = fresh();
  const abandoned = t.worktree("task-a");
  await launchClaude(t.context, abandoned, "task:a", "PROMPT A");

  // `jkb task abandon`, or a landed session: no window can ever open on it again.
  fs.rmSync(abandoned, { recursive: true, force: true });

  reset(t.repo);
  const live = t.worktree("task-b");
  await launchClaude(t.context, live, "task:b", "PROMPT B");
  assert.deepEqual(Object.keys(t.queue()), [live]);
});

test("clicking from the session's own window starts Claude in it, queueing nothing", async () => {
  const t = fresh();
  const work = t.worktree("task-a");

  reset(work);
  assert.equal(await launchClaude(t.context, work, "task:a", "PROMPT"), "here");
  assert.deepEqual(state.calls, opened("PROMPT"));
  assert.deepEqual(t.queue(), {});
});

test("without the Claude Code extension the caller is told to fall back", async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  state.claudeInstalled = false;

  // No window is opened and nothing is queued: the caller's terminal is the whole feature,
  // and a queued prompt would fire at some unrelated later opening of that folder.
  assert.equal(await launchClaude(t.context, work, "task:a", "PROMPT"), "unavailable");
  assert.deepEqual(state.calls, []);
  assert.deepEqual(t.queue(), {});
});

test("a window that fails to open leaves no undeliverable prompt behind", async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  state.openFolderFails = true;

  assert.equal(await launchClaude(t.context, work, "task:a", "PROMPT"), "unavailable");
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
  assert.equal(await launchClaude(t.context, work, "task:a", "PROMPT"), "window");
  assert.deepEqual(t.queue()[work], { uid: "task:a", prompt: "PROMPT" });
});

test("a Claude Code that refuses is reported, not swallowed", async () => {
  const t = fresh();
  const work = t.worktree("task-a");
  await launchClaude(t.context, work, "task:a", "PROMPT");

  reset(work);
  state.claudeRefuses = true;
  await deliverQueuedPrompt(t.context);
  assert.equal(state.errors.length, 1);
  assert.match(state.errors[0], /task:a/);
  // Taken all the same: a prompt that re-fires on every later opening of the folder is worse
  // than one click of the button, which returns the same session.
  assert.deepEqual(t.queue(), {});
});
