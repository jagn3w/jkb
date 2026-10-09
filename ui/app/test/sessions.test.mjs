//! The Sessions tab (D53.9) without a window: main's `claude/notify` feed and git-file reader, the
//! renderer's needs-input watch against a stand-in daemon, the registry reads, and re-attach after a
//! container rebuild.
//
// Bundled with esbuild, as in shell.test.mjs.

import assert from "node:assert/strict";
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
const { RESUME_SCRIPT } = await load(path.join(src, "renderer", "src", "design", "launch.ts"));

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
  const root = fs.mkdtempSync(path.join(work, "repos-"));
  const main = path.join(root, "jkb");
  fs.mkdirSync(path.join(main, ".git", "worktrees", "build"), { recursive: true });
  fs.writeFileSync(path.join(main, ".git", "HEAD"), "ref: refs/heads/main\n");
  const wt = path.join(main, ".jkb", "work", "build");
  fs.mkdirSync(path.join(wt, "src"), { recursive: true });
  fs.writeFileSync(path.join(wt, ".git"), "gitdir: ../../../.git/worktrees/build\n");
  fs.writeFileSync(path.join(main, ".git", "worktrees", "build", "HEAD"), "ref: refs/heads/task/build-1\n");
  return { root, main, wt };
}

test("a session's directory resolves to its checkout, repo key and branch, from git's files alone", () => {
  const { root, main, wt } = repos();
  const roots = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: root, hostHome: "/home/me" };
  assert.deepEqual(gitPlace(main, roots), ok({ root: main, repo: "jkb", branch: "main" }));
  assert.deepEqual(gitPlace(path.join(wt, "src"), roots), ok({ root: wt, repo: "build", branch: "task/build-1" }), "a linked worktree, from a subdirectory");
  assert.deepEqual(
    gitPlace("/home/vscode/repos/jkb/.jkb/work/build", roots),
    ok({ root: wt, repo: "build", branch: "task/build-1" }),
    "a container path is read on the host's side of the repos mount",
  );
  // An absolute gitdir in the container's spelling (git writes absolute ones too) is carried across.
  fs.writeFileSync(path.join(wt, ".git"), "gitdir: /home/vscode/repos/jkb/.git/worktrees/build\n");
  assert.equal(gitPlace(wt, roots).value.branch, "task/build-1");
  fs.writeFileSync(path.join(main, ".git", "worktrees", "build", "HEAD"), "0123456789abcdef0123456789abcdef01234567\n");
  assert.equal(gitPlace(wt, roots).value.branch, null, "detached");
});

test("only the repos directory is looked at, nothing is followed out of it, and no file's contents cross", () => {
  const { root, main, wt } = repos();
  const roots = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: root, hostHome: "/home/me" };
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

const ROOTS = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: "/Users/me/repos", hostHome: "/Users/me" };
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
  const recorded = recordAttached(entries, (s) => (s === S1 ? "/home/vscode/repos/jkb/.jkb/work/build" : undefined));
  assert.deepEqual(
    recorded,
    [
      { key: 1, session: S1, cwd: "/home/vscode/repos/jkb/.jkb/work/build", target: "container", title: "Play · 0f8f" },
      { key: 5, session: "bbbbbbbb-0000-0000-0000-000000000000", cwd: "/home/vscode/repos/jkb", target: "container", title: "Play · bbbb" },
    ],
    "a host session survives the container; a shell is no session; an ended one is not live; the registry's directory wins",
  );
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
