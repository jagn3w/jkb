//! The Sessions tab (D53.9) without a window: main's `claude/notify` feed and git-file reader, the
//! renderer's needs-input watch against a stand-in daemon, the registry reads, and re-attach after a
//! container rebuild.
//
// Bundled with esbuild, as in shell.test.mjs.

import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-sessions-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const require = createRequire(import.meta.url);

async function load(entry) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({ entryPoints: [entry], bundle: true, format: "cjs", platform: "node", outfile, logLevel: "silent" });
  return require(outfile);
}

const src = path.join(here, "..", "src");
const { NotifyFeed } = await load(path.join(src, "main", "notifyFeed.ts"));
const { APP_GROUP } = await load(path.join(src, "main", "topicFeeds.ts"));
const { DESIGN_GROUP } = await load(path.join(src, "main", "designFeeds.ts"));
const { gitPlace } = await load(path.join(src, "main", "gitPlace.ts"));
const { NotifyWatch } = await load(path.join(src, "renderer", "src", "sessions", "watch.ts"));
const { loadHolders, liveCwds, MAX_PAGES } = await load(path.join(src, "renderer", "src", "sessions", "data.ts"));
const { recordAttached, reattachPlan, mergeRecords, tearsDown } = await load(path.join(src, "renderer", "src", "sessions", "reattach.ts"));
const { RESUME_SCRIPT, PLAY_TASK_SCRIPT, sessionResumeSpec, resumedDir } = await load(path.join(src, "renderer", "src", "design", "launch.ts"));
const { sessionAction, owningTerminals, HOST_SESSION_NOTE } = await load(path.join(src, "renderer", "src", "sessions", "resume.ts"));
const { jumpTo } = await load(path.join(src, "renderer", "src", "design", "jump.ts"));

const ok = (value) => ({ ok: true, value });
const err = (code, message = code) => ({ ok: false, error: { code, message } });
const tick = () => new Promise((r) => setImmediate(r));
async function until(cond, what) {
  for (let i = 0; i < 400; i++) {
    if (cond()) return;
    await new Promise((r) => setTimeout(r, 2));
  }
  assert.fail(`timed out waiting for ${what}`);
}

const S1 = "0f8fad5b-d9cb-469f-a165-70867728950e";
const S2 = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

function scriptedDaemon(handlers) {
  const calls = [];
  const op = async (request, options) => {
    calls.push({ request, options });
    const h = handlers[request.op];
    if (h === undefined) return err("bad_request", `unexpected ${request.op}`);
    return h(request, options);
  };
  return { op, calls };
}

function heldPolls() {
  const pending = [];
  return {
    poll: () => new Promise((resolve) => pending.push(resolve)),
    answer(value) {
      const r = pending.shift();
      assert.ok(r, "a poll is held");
      r(value);
    },
    get waiting() {
      return pending.length;
    },
  };
}

// ---- main: the claude/notify feed ---------------------------------------------------------------

test("the needs-input feed is the app's own group on claude/notify, and says which session moved", async () => {
  assert.equal(APP_GROUP, "code-factory");
  assert.equal(DESIGN_GROUP, APP_GROUP, "one group name for every topic the app reads");
  const polls = heldPolls();
  const daemon = scriptedDaemon({
    "mq.group_create": () => ok({ result: "created", created: true }),
    "mq.poll": polls.poll,
    "mq.ack": (r) => ok({ result: "position", position: r.seq }),
  });
  const heard = [];
  const feed = new NotifyFeed(daemon.op, (owner, event) => heard.push({ owner, event }), { sleep: tick });
  assert.deepEqual(await feed.join(1), ok(null));
  assert.deepEqual(await feed.join(2), ok(null), "every window shares the one feed");
  assert.deepEqual(daemon.calls[0].request, { op: "mq.group_create", topic: "claude/notify", group: "code-factory" });
  await until(() => polls.waiting === 1, "the long-poll");
  polls.answer(
    ok({
      result: "messages",
      messages: [
        { seq: 3, kind: "notify.post", payload: { id: "n", session: S1, title: "Claude Code", body: "x" } },
        { seq: 4, kind: "something.else", payload: { session: S2 } },
        { seq: 5, kind: "notify.withdraw", payload: { id: "n", session: S2 } },
      ],
    }),
  );
  await until(() => daemon.calls.some((c) => c.request.op === "mq.ack"), "the ack");
  assert.deepEqual(
    heard.map((h) => [h.owner, h.event.kind, h.event.session]),
    [
      [1, "changed", S1],
      [2, "changed", S1],
      [1, "changed", S2],
      [2, "changed", S2],
    ],
  );
  assert.equal(daemon.calls.find((c) => c.request.op === "mq.ack").request.seq, 5);
  feed.leave(1);
  feed.closeAll(2);
  await until(() => polls.waiting === 1, "the next poll");
  polls.answer(ok({ result: "messages", messages: [] }));
  await until(() => feed.topics.length === 0, "the feed to end");
  await until(() => daemon.calls.some((c) => c.request.op === "mq.group_delete"), "the group to be removed");
  assert.deepEqual(
    daemon.calls.filter((c) => c.request.op === "mq.group_delete").map((c) => c.request),
    [{ op: "mq.group_delete", topic: "claude/notify", group: "code-factory" }],
    "its last window gone, the app takes its group off the topic rather than hold it until the idle removal",
  );
});

test("a feed ending waits for no one, and a window joining while its removal is in flight joins after it lands", async () => {
  const polls = heldPolls();
  const removals = [];
  const daemon = scriptedDaemon({
    "mq.group_create": () => ok({ result: "created", created: true }),
    "mq.poll": polls.poll,
    "mq.group_delete": () => new Promise((resolve) => removals.push(resolve)),
  });
  const feed = new NotifyFeed(daemon.op, () => {}, { sleep: tick });
  await feed.join(1);
  await until(() => polls.waiting === 1, "the long-poll");
  feed.leave(1);
  polls.answer(ok({ result: "messages", messages: [] }));
  await until(() => removals.length === 1, "the removal");
  const rejoined = feed.join(2);
  await tick();
  assert.equal(daemon.calls.filter((c) => c.request.op === "mq.group_create").length, 1, "the join waits for the removal");
  removals.shift()(ok({ result: "group_deleted", deleted: true }));
  assert.deepEqual(await rejoined, ok(null));
  assert.equal(daemon.calls.filter((c) => c.request.op === "mq.group_create").length, 2, "joined after it landed");
  feed.closeAll();
  await until(() => polls.waiting === 1, "the rejoined poll");
  polls.answer(ok({ result: "messages", messages: [] }));
  await until(() => feed.topics.length === 0, "the feed to end");
});

test("quitting takes the app's group off claude/notify, waits a bounded time, and nothing joins after it", async () => {
  const polls = heldPolls();
  const removals = [];
  const logged = [];
  const daemon = scriptedDaemon({
    "mq.group_create": () => ok({ result: "created", created: true }),
    "mq.poll": polls.poll,
    "mq.group_delete": () => new Promise((resolve) => removals.push(resolve)),
  });
  const feed = new NotifyFeed(daemon.op, () => {}, { sleep: tick, log: (m) => logged.push(m) });
  await feed.join(1);
  await until(() => polls.waiting === 1, "the long-poll");
  // A window's feed ended and is removing the group; another window is waiting on that to rejoin.
  feed.leave(1);
  polls.answer(ok({ result: "messages", messages: [] }));
  await until(() => removals.length === 1, "the first removal");
  const waiting = feed.join(2);
  // Quit now. The daemon never answers: quitting is held only for the wait.
  const started = Date.now();
  await feed.leaveAll(30);
  assert.ok(Date.now() - started < 1000, "bounded");
  assert.equal((await feed.join(3)).error.message, "the app is quitting", "no join after quit");
  // The removal lands after the quit: the waiting join finds no owner and never recreates the group.
  removals.shift()(ok({ result: "group_deleted", deleted: true }));
  await waiting;
  await until(() => feed.topics.length === 0, "the feed to end");
  assert.equal(daemon.calls.filter((c) => c.request.op === "mq.group_create").length, 1, "the group is not recreated after quit");

  // A daemon that predates the op refuses it: said, not swallowed.
  const old = scriptedDaemon({ "mq.group_create": () => ok({ result: "created", created: true }), "mq.poll": heldPolls().poll });
  const f2 = new NotifyFeed(old.op, () => {}, { sleep: tick, log: (m) => logged.push(m) });
  await f2.join(1);
  await f2.leaveAll(1000);
  assert.match(logged.at(-1), /could not take the app's group off claude\/notify.*unexpected mq.group_delete/);

  // Quitting while a window's removal is still in flight waits for that one too (within the bound).
  const p3 = heldPolls();
  const held = [];
  const d3 = scriptedDaemon({
    "mq.group_create": () => ok({ result: "created", created: true }),
    "mq.poll": p3.poll,
    "mq.group_delete": () => new Promise((resolve) => held.push(resolve)),
  });
  const f3 = new NotifyFeed(d3.op, () => {}, { sleep: tick });
  await f3.join(1);
  await until(() => p3.waiting === 1, "the long-poll");
  f3.leave(1);
  p3.answer(ok({ result: "messages", messages: [] }));
  await until(() => held.length === 1, "the in-flight removal");
  let quit = false;
  const quitting = f3.leaveAll(5000).then(() => (quit = true));
  for (let i = 0; i < 5; i++) await tick();
  assert.equal(quit, false, "the quit waits for the removal in flight");
  held.shift()(ok({ result: "group_deleted", deleted: true }));
  await quitting;
});

test("a join still in flight at quit is waited for, and the group it created is removed", async () => {
  const creates = [];
  const deletes = [];
  const d = scriptedDaemon({
    "mq.group_create": () => new Promise((resolve) => creates.push(resolve)),
    "mq.poll": heldPolls().poll,
    "mq.group_delete": (r) => {
      deletes.push(r);
      return ok({ result: "group_deleted", deleted: true });
    },
  });
  const f = new NotifyFeed(d.op, () => {}, { sleep: tick });
  const joining = f.join(1);
  await until(() => creates.length === 1, "the join in flight");
  const quitting = f.leaveAll(5000);
  // The create lands after the quit began: the group it made is taken off again, once.
  creates.shift()(ok({ result: "created", created: true }));
  await quitting;
  await joining;
  await until(() => f.topics.length === 0, "the feed to end");
  for (let i = 0; i < 5; i++) await tick();
  assert.deepEqual(deletes.map((r) => r.request ?? r), [{ op: "mq.group_delete", topic: "claude/notify", group: "code-factory" }]);
});

test("the needs-input feed reads only claude/notify, and a daemon without it is told, not retried", async () => {
  const daemon = scriptedDaemon({ "mq.group_create": () => err("no_such_topic", "no topic claude/notify") });
  const feed = new NotifyFeed(daemon.op, () => assert.fail("nothing to deliver"), { sleep: tick });
  for (const topic of ["design/design.x", "claude/notify/x", 5]) {
    const r = await feed.subscribe(1, topic);
    assert.equal(r.error.code, "bad_request", String(topic));
  }
  assert.equal(daemon.calls.length, 0);
  const r = await feed.join(1);
  assert.equal(r.error.code, "no_such_topic");
  assert.equal(daemon.calls.length, 1, "not retried");
  assert.deepEqual(feed.topics, []);
});

// ---- main: where a session's directory is --------------------------------------------------------

/** A repos directory with a main checkout and a linked worktree, as git lays them out. */
function repos() {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(work, "repos-")));
  const main = path.join(root, "jkb");
  fs.mkdirSync(path.join(main, ".git", "worktrees", "build"), { recursive: true });
  fs.writeFileSync(path.join(main, ".git", "HEAD"), "ref: refs/heads/main\n");
  const wt = path.join(main, ".jkb", "work", "build");
  fs.mkdirSync(path.join(wt, "src"), { recursive: true });
  fs.writeFileSync(path.join(wt, ".git"), "gitdir: ../../../.git/worktrees/build\n");
  fs.writeFileSync(path.join(main, ".git", "worktrees", "build", "HEAD"), "ref: refs/heads/task/build-1\n");
  fs.writeFileSync(path.join(main, ".git", "worktrees", "build", "commondir"), "../..\n");
  return { root, main, wt };
}

test("a session's directory resolves to its checkout, repo key and branch, from git's files alone", () => {
  const { root, main, wt } = repos();
  const roots = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: root, hostReposReal: root, hostHome: "/home/me" };
  assert.deepEqual(gitPlace(main, roots), ok({ root: main, repo: "jkb", branch: "main" }));
  assert.deepEqual(gitPlace(path.join(wt, "src"), roots), ok({ root: wt, repo: "jkb", branch: "task/build-1" }), "a linked worktree, from a subdirectory, keyed by its main checkout as `repo_ctx` tags tasks");
  assert.deepEqual(
    gitPlace("/home/vscode/repos/jkb/.jkb/work/build", roots),
    ok({ root: wt, repo: "jkb", branch: "task/build-1" }),
    "a container path is read on the host's side of the repos mount",
  );
  // An absolute gitdir in the container's spelling (git writes absolute ones too) is carried across.
  fs.writeFileSync(path.join(wt, ".git"), "gitdir: /home/vscode/repos/jkb/.git/worktrees/build\n");
  assert.equal(gitPlace(wt, roots).value.branch, "task/build-1");
  fs.writeFileSync(path.join(main, ".git", "worktrees", "build", "HEAD"), "0123456789abcdef0123456789abcdef01234567\n");
  assert.equal(gitPlace(wt, roots).value.branch, null, "detached");
  // The common dir in the container's spelling, as git writes it when the worktree was added there.
  fs.writeFileSync(path.join(main, ".git", "worktrees", "build", "commondir"), "/home/vscode/repos/jkb/.git\n");
  assert.equal(gitPlace(wt, roots).value.repo, "jkb");
  // A git dir with no `commondir` (a submodule's) is keyed by its own checkout.
  fs.rmSync(path.join(main, ".git", "worktrees", "build", "commondir"));
  assert.equal(gitPlace(wt, roots).value.repo, "build");
});

test("only the repos directory is looked at, nothing is followed out of it, and no file's contents cross", () => {
  const { root, main, wt } = repos();
  const roots = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: root, hostReposReal: root, hostHome: "/home/me" };
  for (const cwd of ["/etc", `${root}/../`, `${root}/jkb/../../`, "relative/path", 5, undefined]) {
    const r = gitPlace(cwd, roots);
    assert.equal(r.ok, false, String(cwd));
  }
  assert.match(gitPlace(path.join(root), roots).error, /not inside a git checkout/, "the walk stops at the repos directory");
  // A .git file pointing outside the repos directory is refused, not read.
  const outside = fs.mkdtempSync(path.join(work, "outside-"));
  fs.writeFileSync(path.join(outside, "HEAD"), "ref: refs/heads/secret\n");
  fs.writeFileSync(path.join(wt, ".git"), `gitdir: ${outside}\n`);
  assert.match(gitPlace(wt, roots).error, /outside the repos directory/);
  fs.writeFileSync(path.join(wt, ".git"), "gitdir: ../../../../../../../../../../tmp\n");
  assert.equal(gitPlace(wt, roots).ok, false);
  // A .git that is a link is not followed.
  fs.rmSync(path.join(wt, ".git"));
  fs.symlinkSync(path.join(main, ".git"), path.join(wt, ".git"));
  assert.match(gitPlace(wt, roots).error, /links are not followed/, "nor climbed past into the enclosing checkout");
  // A HEAD that is a link is not read.
  fs.rmSync(path.join(main, ".git", "HEAD"));
  fs.symlinkSync(path.join(outside, "HEAD"), path.join(main, ".git", "HEAD"));
  assert.match(gitPlace(main, roots).error, /HEAD names no branch/);
  // A .git file that is not a gitdir line says so and nothing more.
  fs.mkdirSync(path.join(root, "other"));
  fs.writeFileSync(path.join(root, "other", ".git"), "[core]\n\tbare = false\n");
  assert.equal(gitPlace(path.join(root, "other"), roots).error, `${path.join(root, "other", ".git")} is not a worktree's gitdir file`);
});

test("a link anywhere along the path, not only at its end, cannot lead the reader out of the repos directory as the path stands when resolved", () => {
  const { root, main, wt } = repos();
  const roots = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: root, hostHome: "/home/me" };
  const outside = fs.realpathSync(fs.mkdtempSync(path.join(work, "outside-")));
  fs.mkdirSync(path.join(outside, "proj", ".git"), { recursive: true });
  fs.writeFileSync(path.join(outside, "proj", ".git", "HEAD"), "ref: refs/heads/secret\n");
  fs.mkdirSync(path.join(outside, "wt"), { recursive: true });
  fs.writeFileSync(path.join(outside, "wt", "HEAD"), "ref: refs/heads/secret\n");
  fs.symlinkSync(outside, path.join(root, "x"));
  // The session's directory runs through the link: refused, nothing under it is read.
  for (const cwd of [path.join(root, "x", "proj"), "/home/vscode/repos/x/proj"]) {
    const r = gitPlace(cwd, roots);
    assert.equal(r.ok, false, cwd);
    assert.match(r.error, /outside the repos directory through a link/, cwd);
  }
  // A gitdir line through a linked directory: refused.
  fs.writeFileSync(path.join(wt, ".git"), "gitdir: ../../../../x/wt\n");
  assert.match(gitPlace(wt, roots).error, /points outside the repos directory/);
  // A commondir through one: refused, not keyed by whatever is out there.
  fs.writeFileSync(path.join(wt, ".git"), "gitdir: ../../../.git/worktrees/build\n");
  fs.writeFileSync(path.join(main, ".git", "worktrees", "build", "commondir"), "../../../../x/proj/.git\n");
  assert.match(gitPlace(wt, roots).error, /commondir points outside the repos directory/);
  // Missing is said apart from outside: a removed git dir, or a commondir naming nothing.
  fs.writeFileSync(path.join(main, ".git", "worktrees", "build", "commondir"), "../gone\n");
  assert.match(gitPlace(wt, roots).error, /commondir names a directory that does not exist$/);
  fs.writeFileSync(path.join(wt, ".git"), "gitdir: ../../../.git/worktrees/removed\n");
  assert.equal(gitPlace(wt, roots).error, `${path.join(wt, ".git")} names a directory that does not exist`);
  fs.writeFileSync(path.join(wt, ".git"), "gitdir: ../../../.git/worktrees/build\n");
  fs.writeFileSync(path.join(main, ".git", "worktrees", "build", "commondir"), "../..\n");
  // A link that stays inside is resolved, and the place is reported where it really is.
  fs.symlinkSync(main, path.join(root, "alias"));
  assert.deepEqual(gitPlace(path.join(root, "alias"), roots), ok({ root: main, repo: "jkb", branch: "main" }));
});

test("with a symlinked repos directory the root crosses in the host's spelling, so Shell here can carry it across", () => {
  const { root, main, wt } = repos();
  const link = path.join(fs.realpathSync(fs.mkdtempSync(path.join(work, "link-"))), "repos");
  fs.symlinkSync(root, link);
  const roots = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: link, hostHome: "/home/me" };
  const rel = (p) => path.join(link, path.relative(root, p));
  assert.deepEqual(gitPlace(rel(main), roots), ok({ root: rel(main), repo: "jkb", branch: "main" }));
  assert.deepEqual(gitPlace("/home/vscode/repos/jkb/.jkb/work/build/src", roots), ok({ root: rel(wt), repo: "jkb", branch: "task/build-1" }));
  assert.deepEqual(gitPlace(link, roots).ok, false, "the repos directory itself is no checkout");
});

test("a FIFO planted as HEAD (or .git, or commondir) is refused, never waited on", () => {
  const { root, main, wt } = repos();
  const roots = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: root, hostHome: "/home/me" };
  // Run in a child, so a blocking open shows as a timeout rather than hanging the test runner itself.
  const probe = path.join(work, "probe-fifo.cjs");
  fs.writeFileSync(
    probe,
    `const { gitPlace } = require(${JSON.stringify(path.join(work, "gitPlace.ts.cjs"))});\n` +
      `const [cwd, roots] = JSON.parse(process.argv[2]);\n` +
      `process.stdout.write(JSON.stringify(gitPlace(cwd, roots)));\n`,
  );
  const run = (cwd) => {
    const r = spawnSync(process.execPath, [probe, JSON.stringify([cwd, roots])], { timeout: 5000, encoding: "utf8" });
    assert.equal(r.error?.code, undefined, `gitPlace(${cwd}) did not return: ${r.error?.message}`);
    return JSON.parse(r.stdout);
  };
  fs.rmSync(path.join(main, ".git", "HEAD"));
  execFileSync("mkfifo", [path.join(main, ".git", "HEAD")]);
  assert.match(run(main).error, /HEAD names no branch/);
  fs.rmSync(path.join(main, ".git", "worktrees", "build", "commondir"));
  execFileSync("mkfifo", [path.join(main, ".git", "worktrees", "build", "commondir")]);
  assert.match(run(wt).error, /commondir names no directory/, "a commondir that cannot be read is not taken as none");
  fs.rmSync(path.join(wt, ".git"));
  execFileSync("mkfifo", [path.join(wt, ".git")]);
  assert.match(run(wt).error, /is not a file or a directory/);
});

// ---- the renderer's needs-input watch -------------------------------------------------------------

/** A bridge over a stand-in daemon whose records the test sets, and whose feed it drives. */
function watchBridge({ joined = ok(null), records = () => [] } = {}) {
  const listeners = new Set();
  let reads = 0;
  const bridge = {
    gate: undefined,
    op: async (request) => {
      assert.equal(request.op, "notify.open_sessions");
      reads++;
      if (bridge.gate !== undefined) await bridge.gate;
      const value = records();
      return value instanceof Error ? err("unavailable", value.message) : ok({ result: "sessions", sessions: value });
    },
    subscribe: async () => joined,
    unsubscribe: () => {
      bridge.unsubscribed = true;
    },
    onEvent: (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    emit: (event) => {
      for (const l of listeners) l({ topic: "claude/notify", ...event });
    },
    get reads() {
      return reads;
    },
    get listeners() {
      return listeners.size;
    },
  };
  return bridge;
}

const rec = (session, state, tool = "") => ({ session, tool, owner: "", instance: "", updated_at: 1, state });

test("the watch joins the feed before it reads, re-reads on every change, and the dot is awaiting_user", async () => {
  let now = [rec(S1, "awaiting_user"), rec(S2, "awaiting_tool", "Bash")];
  const bridge = watchBridge({ records: () => now });
  const watch = new NotifyWatch(bridge);
  let changes = 0;
  watch.onChange(() => changes++);
  await watch.open();
  assert.equal(bridge.reads, 1);
  assert.ok(watch.loaded);
  assert.deepEqual([...watch.needing], [S1], "a permission prompt is not the dot");
  assert.equal(watch.problem, undefined);

  now = [];
  bridge.emit({ kind: "changed", session: S1 });
  await until(() => watch.needing.size === 0, "the withdrawal");
  assert.equal(bridge.reads, 2);

  // Changes while a read is in flight cost one more read after it, never one per message, and the
  // answer kept is the read made after the last change.
  let release;
  bridge.gate = new Promise((r) => (release = r));
  bridge.emit({ kind: "changed", session: S1 });
  bridge.emit({ kind: "changed", session: S2 });
  bridge.emit({ kind: "changed", session: S1 });
  now = [rec(S2, "awaiting_user")];
  await tick();
  bridge.gate = undefined;
  release();
  await until(() => bridge.reads === 4, "the one read after the in-flight one");
  await until(() => watch.needing.has(S2), "the newest answer");
  await tick();
  assert.equal(bridge.reads, 4);

  bridge.emit({ kind: "gap", message: "lost" });
  await until(() => bridge.reads === 5, "a gap re-reads");
  assert.ok(changes > 0);
  watch.dispose();
  assert.ok(bridge.unsubscribed);
  assert.equal(bridge.listeners, 0);
});

test("without the feed the records are still read, and why the dot may be stale is said", async () => {
  const bridge = watchBridge({ joined: err("no_such_topic"), records: () => [rec(S1, "awaiting_user")] });
  const watch = new NotifyWatch(bridge);
  await watch.open();
  assert.deepEqual([...watch.needing], [S1]);
  assert.match(watch.problem, /claude\/notify does not exist.*setup\.sh/);

  // The daemon going away: the feed says so, a failed read says so, and the feed coming back re-reads.
  let down = false;
  const b2 = watchBridge({ records: () => (down ? new Error("cannot reach jkb serve") : [rec(S1, "awaiting_user")]) });
  const w2 = new NotifyWatch(b2);
  await w2.open();
  down = true;
  b2.emit({ kind: "error", message: "reading claude/notify: down" });
  assert.match(w2.problem, /down/);
  b2.emit({ kind: "changed", session: S1 });
  await until(() => /cannot read notifications/.test(w2.problem ?? ""), "the failed read");
  assert.deepEqual([...w2.needing], [S1], "the last answer is kept, not cleared");
  down = false;
  const before = b2.reads;
  b2.emit({ kind: "live" });
  await until(() => w2.problem === undefined && b2.reads === before + 1, "the read on recovery");
});

// ---- the registry reads ----------------------------------------------------------------------------

test("the registry is read a page at a time, to its end or a bound", async () => {
  const holder = (session, over = {}) => ({ session, pid: "1", instance: "i", cwd: `/r/${session.slice(0, 4)}`, seen_at: 10, ...over });
  const pages = { undefined: { sessions: [holder(S1)], next: "p2" }, p2: { sessions: [holder(S2)] } };
  const asked = [];
  const op = async (r) => {
    asked.push(r);
    return ok({ result: "claude_sessions", ...pages[String(r.after)] });
  };
  const all = await loadHolders(op, true);
  assert.deepEqual(all.value.holders.map((h) => h.session), [S1, S2]);
  assert.equal(all.value.truncated, false);
  assert.deepEqual(asked, [{ op: "session.list", all: true }, { op: "session.list", all: true, after: "p2" }]);

  let n = 0;
  const endless = async () => ok({ result: "claude_sessions", sessions: [], next: `p${n++}` });
  const bounded = await loadHolders(endless, false);
  assert.equal(bounded.value.truncated, true);
  assert.equal(n, MAX_PAGES);
  const stuck = await loadHolders(async () => ok({ result: "claude_sessions", sessions: [], next: "same" }), false);
  assert.equal(stuck.ok, false, "a page that repeats is an error, not a loop");
  assert.equal((await loadHolders(async () => err("unavailable"), false)).error.code, "unavailable");

  const holders = [
    { session: S1, cwd: "/old", seenAt: 1, endedAt: null },
    { session: S1, cwd: "/new", seenAt: 5, endedAt: null },
    { session: S1, cwd: "/ended", seenAt: 9, endedAt: 9 },
    { session: S2, cwd: "", seenAt: 1, endedAt: null },
  ];
  assert.deepEqual([...liveCwds(holders)], [[S1, "/new"]]);
});

// ---- re-attach after a rebuild -------------------------------------------------------------------

const ROOTS = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: "/Users/me/repos", hostReposReal: "/Users/me/repos", hostHome: "/Users/me" };
const entry = (key, spec, status = { kind: "running" }) => ({ key, spec, placement: "drawer", status });
const claudeSpec = (session, over = {}) => ({ target: "container", cwd: "/home/vscode/repos/jkb", argv: ["/bin/bash", "-lc", "x"], title: `Play · ${session.slice(0, 4)}`, sessionUuid: session, ...over });

test("a teardown records the app's live container sessions, where they really run", () => {
  assert.deepEqual(["build", "stop", "remove", "verify", "install-extensions"].map(tearsDown), [true, true, true, false, false]);
  const entries = [
    entry(1, claudeSpec(S1)),
    entry(2, claudeSpec(S2, { target: "host", cwd: "/Users/me/repos/jkb" })),
    entry(3, { target: "container", cwd: "/home/vscode/repos", argv: [], title: "shell" }),
    entry(4, claudeSpec("aaaaaaaa-0000-0000-0000-000000000000"), { kind: "exited", exitCode: 0 }),
    entry(5, claudeSpec("bbbbbbbb-0000-0000-0000-000000000000"), { kind: "starting" }),
  ];
  const { attached: recorded, dropped } = recordAttached(entries, (s) => (s === S1 ? "/home/vscode/repos/jkb/.jkb/work/build" : undefined));
  assert.deepEqual(dropped, []);
  assert.deepEqual(
    recorded,
    [
      { key: 1, session: S1, cwd: "/home/vscode/repos/jkb/.jkb/work/build", target: "container", title: "Play · 0f8f" },
      { key: 5, session: "bbbbbbbb-0000-0000-0000-000000000000", cwd: "/home/vscode/repos/jkb", target: "container", title: "Play · bbbb" },
    ],
    "a host session survives the container; a shell is no session; an ended one is not live; the registry's directory wins",
  );
});

test("a resumed session the registry does not know is recorded where the resume moved, not where its terminal started", () => {
  const resumed = sessionResumeSpec({ session: S1, cwd: "/home/vscode/repos/jkb/.jkb/work/build", title: "Resume · x" }, ROOTS);
  const atRoot = sessionResumeSpec({ session: S2, cwd: "/home/vscode/repos", title: "Resume · y" }, ROOTS);
  const outside = sessionResumeSpec({ session: "aaaaaaaa-0000-0000-0000-000000000000", cwd: "/elsewhere", title: "Resume · z" }, ROOTS);
  assert.equal(resumed.cwd, "/home/vscode/repos", "the terminal starts at the mount's root");
  const { attached: recorded } = recordAttached([entry(1, resumed), entry(2, atRoot), entry(3, outside)], () => undefined);
  assert.deepEqual(
    recorded.map((a) => a.cwd),
    ["/home/vscode/repos/jkb/.jkb/work/build", "/home/vscode/repos", "/elsewhere"],
  );
  // ... so the rebuild resumes it there again.
  const plan = reattachPlan(recorded, [], true, ROOTS);
  assert.deepEqual(plan[0].spec, resumed);
});

test("after the rebuild each recorded session is resumed in its terminal — and only once the container runs", () => {
  const recorded = [
    { key: 1, session: S1, cwd: "/home/vscode/repos/jkb/.jkb/work/build", target: "container", title: "Play · build" },
    { key: 2, session: S2, cwd: "/home/vscode/repos/jkb", target: "container", title: "Discuss · x" },
    { key: 3, session: "cccccccc-0000-0000-0000-000000000000", cwd: "/home/vscode/repos/jkb", target: "container", title: "New · y" },
  ];
  const entries = [
    entry(1, claudeSpec(S1), { kind: "exited", exitCode: 137 }),
    entry(2, claudeSpec(S2)),
    // Terminal 3 was closed meanwhile; terminal 9 runs something else.
    entry(9, { target: "container", cwd: "/home/vscode/repos", argv: [], title: "shell" }),
  ];
  assert.equal(reattachPlan(recorded, entries, false, ROOTS), undefined, "a Stop or Remove keeps the record for the next Build");
  const plan = reattachPlan(recorded, entries, true, ROOTS);
  assert.deepEqual(
    plan.map((p) => [p.kind, p.key ?? null]),
    [
      ["relaunch", 1],
      ["survived", 2],
      ["open", null],
    ],
    "an ended terminal is relaunched in place; a running one was not torn down and is left alone",
  );
  assert.deepEqual(plan[0].spec, {
    target: "container",
    cwd: "/home/vscode/repos",
    argv: ["/bin/bash", "-lc", RESUME_SCRIPT, "claude", S1, "./jkb/.jkb/work/build"],
    title: "Play · build",
    sessionUuid: S1,
  });
  assert.equal(plan[2].spec.sessionUuid, "cccccccc-0000-0000-0000-000000000000");

  const merged = mergeRecords(recorded.slice(0, 2), [{ ...recorded[0], key: 7 }]);
  assert.deepEqual(merged.map((a) => [a.session, a.key]), [[S2, 2], [S1, 7]], "a second teardown keeps the newest record of a session");
});

test("re-attach leaves alone a terminal whose program may still run, as planOpen does", () => {
  const recorded = [{ key: 1, session: S1, cwd: "/home/vscode/repos/jkb", target: "container", title: "Play · build" }];
  for (const status of [{ kind: "closing" }, { kind: "failed", error: "x", mayBeRunning: true }]) {
    const plan = reattachPlan(recorded, [entry(1, claudeSpec(S1), status)], true, ROOTS);
    assert.deepEqual(plan.map((p) => p.kind), ["survived"], `${JSON.stringify(status)} is not relaunched beside`);
  }
  const ended = reattachPlan(recorded, [entry(1, claudeSpec(S1), { kind: "failed", error: "x" })], true, ROOTS);
  assert.deepEqual(ended.map((p) => p.kind), ["relaunch"]);
});

test("a teardown does not record a task's Play at its repo root when the registry cannot place it, and says exactly which and why", () => {
  const play = (session) => claudeSpec(session, { argv: ["/bin/bash", "-lc", PLAY_TASK_SCRIPT, "claude", "design:x", session, "task"] });
  const entries = [
    entry(1, play(S1)),
    entry(2, play(S2)),
    entry(3, claudeSpec("cccccccc-0000-0000-0000-000000000000")),
    entry(4, claudeSpec("DDDDDDDD-0000-0000-0000-000000000000")),
  ];
  const placed = (s) => (s === S2 ? "/home/vscode/repos/jkb/.jkb/work/t" : undefined);
  const { attached, dropped } = recordAttached(entries, placed);
  assert.deepEqual(
    attached.map((a) => [a.session, a.cwd]),
    [
      [S2, "/home/vscode/repos/jkb/.jkb/work/t"],
      ["cccccccc-0000-0000-0000-000000000000", "/home/vscode/repos/jkb"],
    ],
    "the Play the registry did not place is left out, not resumed in the repo root; another launch keeps its own directory",
  );
  assert.deepEqual(
    dropped.map((d) => [d.key, d.why]),
    [
      [1, "the session registry has no record of where this task's Play runs"],
      [4, "its id is not a lowercase session uuid"],
    ],
    "after a read that worked, a dropped Play is still reported",
  );
  const failed = recordAttached(entries, () => undefined, "daemon down");
  assert.deepEqual(
    failed.dropped.map((d) => d.key),
    [1, 2, 4],
  );
  assert.match(failed.dropped[0].why, /could not be read \(daemon down\)/);
  assert.deepEqual(recordAttached([entry(3, claudeSpec("cccccccc-0000-0000-0000-000000000000"))], () => undefined, "down").dropped, [], "a failed read that drops nothing says nothing");
});

test("a resume is built only from a lowercase uuid, and passes it as --resume=<id>", () => {
  for (const bad of ["--dangerously-skip-permissions", "0F8FAD5B-D9CB-469F-A165-70867728950E", "", "x"]) {
    assert.equal(sessionResumeSpec({ session: bad, cwd: "/home/vscode/repos/jkb", title: "t" }, ROOTS), undefined, bad);
  }
  assert.match(RESUME_SCRIPT, /exec claude --resume="\$1"$/);
});

// ---- the Sessions tab's button ---------------------------------------------------------------------

const CONTAINER_INSTANCE = "jkb-dev#pid:[4026532556]/pid:[4026532556]";
const holder = (over = {}) => ({
  session: S1,
  pid: "42",
  instance: CONTAINER_INSTANCE,
  cwd: "/home/vscode/repos/jkb/.jkb/work/build",
  startedAt: 1,
  startSource: "startup",
  seenAt: 5,
  endedAt: null,
  endReason: null,
  ...over,
});
const row = (holders, over = {}) => ({
  session: S1,
  live: holders.some((h) => h.endedAt === null),
  holders,
  cwd: holders.find((h) => h.endedAt === null)?.cwd ?? holders[0]?.cwd ?? "",
  startedAt: 1,
  seenAt: 5,
  endedAt: null,
  endReason: null,
  notify: null,
  needsInput: false,
  ...over,
});

test("Resume runs only in the container; of a host session the tab says only that, with no command", () => {
  const container = sessionAction(row([holder()]), null, [], ROOTS, "Resume · x");
  assert.equal(container.kind, "resume");
  assert.deepEqual(
    [container.spec.target, resumedDir(container.spec), container.spec.argv[4]],
    ["container", "/home/vscode/repos/jkb/.jkb/work/build", S1],
  );

  // A host editor's session: no command, no directory — nothing to run, nothing to copy.
  for (const cwd of ["/Users/me/my proj", "/a\\'b; rm -rf ~"]) {
    assert.deepEqual(sessionAction(row([holder({ instance: "Johns-Mac", cwd })]), null, [], ROOTS, "t"), { kind: "host" });
  }
  assert.equal(HOST_SESSION_NOTE, "This session ran on the host; resume it from a terminal there.");
  assert.equal(sessionAction(row([holder({ instance: "box/pid:[1]", cwd: "/x" })]), null, [], ROOTS, "t").kind, "host", "a Linux host records a namespace but no boot");

  // An id that is not a lowercase uuid is never put on any command line.
  for (const bad of ["--dangerously-skip-permissions", S1.toUpperCase()]) {
    for (const instance of [CONTAINER_INSTANCE, "Johns-Mac"]) {
      const a = sessionAction(row([holder({ session: bad, instance })], { session: bad }), null, [], ROOTS, "t");
      assert.equal(a.kind, "refused", `${bad} ${instance}`);
    }
  }

  // Nothing says where: refused, never the container at the repos root.
  assert.match(sessionAction(row([holder({ instance: "" })]), null, [], ROOTS, "t").why, /host or in the container/);
  assert.match(sessionAction(row([holder({ cwd: "" })]), null, [], ROOTS, "t").why, /no directory/);
  assert.match(sessionAction(row([]), null, [], ROOTS, "t").why, /no process/);
  // Known only by its design prompt: where the prompt was recorded, as the Design tab resumes it.
  const prompted = sessionAction(row([]), { cwd: "/home/vscode/repos/jkb/.jkb/work/p" }, [], ROOTS, "t");
  assert.deepEqual([prompted.spec.target, resumedDir(prompted.spec)], ["container", "/home/vscode/repos/jkb/.jkb/work/p"]);

  // The lead holder decides: the live one over a more recent ended one.
  const mixed = row([holder({ instance: "Johns-Mac", cwd: "/Users/me/proj", endedAt: 9, seenAt: 9 }), holder()]);
  assert.equal(sessionAction(mixed, null, [], ROOTS, "t").spec.target, "container");
});

test("the app's own terminal for a session decides over any registry row: it ran in the container", () => {
  const forged = row([holder({ instance: "Johns-Mac", cwd: "/Users/me/anywhere" })]);
  // A Discuss/New launch ran in its terminal's directory.
  const launched = entry(4, claudeSpec(S1), { kind: "exited", exitCode: 0 });
  const a = sessionAction(forged, null, [launched], ROOTS, "t");
  assert.deepEqual([a.kind, a.key, a.spec.target, resumedDir(a.spec)], ["resume", 4, "container", "/home/vscode/repos/jkb"]);
  // A resume terminal: where that resume moved.
  const resumed = entry(5, sessionResumeSpec({ session: S1, cwd: "/home/vscode/repos/jkb/.jkb/work/w", title: "t" }, ROOTS), { kind: "exited", exitCode: 0 });
  assert.equal(resumedDir(sessionAction(forged, null, [resumed], ROOTS, "t").spec), "/home/vscode/repos/jkb/.jkb/work/w");
  // A task's Play: its worktree from a container-side row or its prompt — never a host row.
  const play = entry(6, claudeSpec(S1, { argv: ["/bin/bash", "-lc", PLAY_TASK_SCRIPT, "claude", "design:x", S1, "task"] }), { kind: "exited", exitCode: 0 });
  assert.match(sessionAction(forged, null, [play], ROOTS, "t").why, /Nothing records where this task's Play ran/);
  assert.equal(resumedDir(sessionAction(forged, { cwd: "/home/vscode/repos/jkb/.jkb/work/p" }, [play], ROOTS, "t").spec), "/home/vscode/repos/jkb/.jkb/work/p");
  assert.equal(resumedDir(sessionAction(row([holder()]), null, [play], ROOTS, "t").spec), "/home/vscode/repos/jkb/.jkb/work/build");
});

test("only a starting or running terminal owns a session; an ended one is where Resume relaunches it", () => {
  const running = entry(4, claudeSpec(S1));
  const exited = entry(4, claudeSpec(S1), { kind: "exited", exitCode: 137 });
  const failed = entry(4, claudeSpec(S1), { kind: "failed", error: "gone" });
  assert.deepEqual(sessionAction(row([holder()]), null, [running], ROOTS, "t"), { kind: "show", key: 4 });
  for (const dead of [exited, failed]) {
    assert.equal(owningTerminals([dead]).size, 0, dead.status.kind);
    const a = sessionAction(row([holder()]), null, [dead], ROOTS, "t");
    assert.deepEqual([a.kind, a.key, resumedDir(a.spec)], ["resume", 4, "/home/vscode/repos/jkb"], `${dead.status.kind}: where its own terminal ran it`);
  }
  assert.equal(owningTerminals([entry(5, claudeSpec(S1), { kind: "starting" })]).get(S1).key, 5);
});

// ---- the Design tab, on a jump --------------------------------------------------------------------

test("a jump opens the design in its own repo, or says why it cannot — never another design", () => {
  const d = (uid, namespace) => ({ uid, title: uid, namespace, seq: 1, topic: `design/${uid}` });
  const listing = { designs: [d("design:a", "designs/jkb"), d("design:x", "elsewhere/x")], loading: false, error: undefined };
  assert.deepEqual(jumpTo({ ...listing, loading: true }, "design:a", false), { kind: "wait" });
  assert.deepEqual(jumpTo(listing, "design:a", false), { kind: "open", repo: "jkb", uid: "design:a" });
  assert.match(jumpTo(listing, "design:x", false).notice, /not under designs\/<repo>/, "found, but in no repo the tab lists");
  assert.deepEqual(jumpTo(listing, "design:new", false), { kind: "reload" });
  assert.match(jumpTo(listing, "design:new", true).notice, /not a design this daemon lists/);
  const failedListing = { designs: undefined, loading: false, error: "daemon down" };
  assert.deepEqual(jumpTo(failedListing, "design:a", false), { kind: "reload" });
  assert.match(jumpTo(failedListing, "design:a", true).notice, /could not be listed \(daemon down\)/, "a failed listing is not the design's absence");
});
