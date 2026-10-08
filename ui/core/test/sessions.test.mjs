//! The Sessions tab's data as `@jkb/core` reads it (D53.9): the registry and the notification records
//! decoded and joined, the needs-input rule, the `claude/notify` messages, and the facts *Jump to
//! context* reads. Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import {
  NOTIFY_TOPIC,
  decodeBranchTasks,
  decodeNotified,
  decodeSessionPage,
  decodeSessionPrompt,
  isSessionUuid,
  joinSessions,
  needingInput,
  notifyLabel,
  parseGitdirFile,
  parseHead,
  parseNotifyMessage,
  promptOps,
  repoKeyOf,
  sessionOps,
  tasksOn,
} from "../dist/index.js";

const ok = (value) => ({ ok: true, value });
const S1 = "0f8fad5b-d9cb-469f-a165-70867728950e";
const S2 = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
const S3 = "16fd2706-8baf-433b-82eb-8c7fada847da";

/** A `session.list` row as `jkb_api::ClaudeSession` serializes it. */
const wireHolder = (over = {}) => ({ session: S1, pid: "41", instance: "box#b/4026531836", cwd: "/home/vscode/repos/jkb", started_at: 100, start_source: "startup", seen_at: 200, ...over });

test("session.list and notify.open_sessions decode as the daemon writes them, and nothing else does", () => {
  assert.deepEqual(sessionOps.list(false), { op: "session.list", all: false });
  assert.deepEqual(sessionOps.list(true, "c"), { op: "session.list", all: true, after: "c" });
  assert.deepEqual(sessionOps.notified(), { op: "notify.open_sessions" });
  assert.deepEqual(sessionOps.byBranch("jkb"), { op: "task.by_branch", repo: "jkb" });
  assert.deepEqual(promptOps.of(S1), { op: "design.prompt_of", session: S1 });

  const page = decodeSessionPage(
    ok({ result: "claude_sessions", sessions: [wireHolder(), wireHolder({ pid: "", started_at: undefined, start_source: undefined, ended_at: 300, end_reason: "gone" })], next: "n" }),
  );
  assert.ok(page.ok);
  assert.equal(page.value.next, "n");
  assert.deepEqual(page.value.holders[1], {
    session: S1,
    pid: "",
    instance: "box#b/4026531836",
    cwd: "/home/vscode/repos/jkb",
    startedAt: null,
    startSource: null,
    seenAt: 200,
    endedAt: 300,
    endReason: "gone",
  });
  assert.equal(decodeSessionPage(ok({ result: "claude_sessions", sessions: [] })).value.next, null, "no next: the last page");
  for (const bad of [{ result: "sessions", sessions: [] }, { result: "claude_sessions", sessions: [wireHolder({ seen_at: "x" })] }, { result: "claude_sessions", sessions: [{}] }]) {
    assert.equal(decodeSessionPage(ok(bad)).ok, false, JSON.stringify(bad));
  }

  const notified = decodeNotified(
    ok({
      result: "sessions",
      sessions: [
        { session: S1, tool: "", owner: "41", instance: "i", updated_at: 5, state: "awaiting_user" },
        { session: S2, tool: "Bash", owner: "", instance: "", updated_at: 6, state: "awaiting_tool" },
        { session: S3, tool: "", owner: "", instance: "", updated_at: 7 },
      ],
    }),
  );
  assert.ok(notified.ok);
  assert.deepEqual(
    notified.value.map((n) => [n.session, n.state]),
    [
      [S1, "awaiting_user"],
      [S2, "awaiting_tool"],
      [S3, null],
    ],
    "a daemon that predates the state is read, never re-derived here",
  );
  assert.equal(decodeNotified(ok({ result: "claude_sessions", sessions: [] })).ok, false);
});

test("the dot is awaiting_user, as the record's derived state says — not a permission prompt, not an unknown state", () => {
  const records = [
    { session: S1, tool: "", state: "awaiting_user", updatedAt: 1 },
    { session: S2, tool: "Bash", state: "awaiting_tool", updatedAt: 1 },
    { session: S3, tool: "", state: null, updatedAt: 1 },
  ];
  assert.deepEqual([...needingInput(records)], [S1]);
  assert.equal(notifyLabel(records[0]), "needs input");
  assert.equal(notifyLabel(records[1]), "awaiting permission: Bash");
  assert.equal(notifyLabel(records[2]), "notified");
  assert.equal(notifyLabel(null), undefined);
});

test("rows join a session's processes with its notification, needing input first, then live, then ended", () => {
  const h = (session, over) => ({ session, pid: "1", instance: "i", cwd: `/r/${session.slice(0, 2)}`, startedAt: 10, startSource: "startup", seenAt: 100, endedAt: null, endReason: null, ...over });
  const holders = [
    h(S2, { seenAt: 500 }),
    h(S1, { seenAt: 300, cwd: "/r/old", endedAt: 310, endReason: "other" }),
    h(S1, { seenAt: 400, cwd: "/r/new", startedAt: 5 }),
    h(S3, { seenAt: 900, endedAt: 950, endReason: "prompt_input_exit" }),
  ];
  const lost = "aaaaaaaa-0000-0000-0000-000000000000";
  const notified = [
    { session: S1, tool: "", state: "awaiting_user", updatedAt: 450 },
    { session: lost, tool: "", state: "awaiting_user", updatedAt: 20 },
  ];
  const rows = joinSessions(holders, notified);
  assert.deepEqual(
    rows.map((r) => [r.session, r.needsInput, r.live]),
    [
      [S1, true, true],
      [lost, true, true],
      [S2, false, true],
      [S3, false, false],
    ],
  );
  const s1 = rows[0];
  assert.equal(s1.cwd, "/r/new", "the live process's directory, not the ended one's");
  assert.equal(s1.holders.length, 2);
  assert.equal(s1.startedAt, 5);
  assert.equal(s1.endedAt, null, "live: not ended, though one of its processes was");
  assert.equal(rows[1].cwd, "", "a session known only by its notification is still listed");
  assert.deepEqual([rows[3].endedAt, rows[3].endReason], [950, "prompt_input_exit"]);
  assert.deepEqual(joinSessions([], []), []);
});

test("a claude/notify message says which session moved; anything else on the topic is nothing", () => {
  assert.equal(NOTIFY_TOPIC, "claude/notify");
  assert.deepEqual(parseNotifyMessage("notify.post", { id: "x", session: S1, title: "Claude Code" }), { session: S1 });
  assert.deepEqual(parseNotifyMessage("notify.withdraw", { id: "x", session: S1 }), { session: S1 });
  assert.equal(parseNotifyMessage("update", { session: S1 }), undefined);
  assert.equal(parseNotifyMessage("notify.post", { id: "x" }), undefined);
  assert.equal(parseNotifyMessage("notify.post", null), undefined);
});

test("jump to context: a session's prompt, a worktree's git files, and the tasks on its branch", () => {
  const prompt = { uid: `prompt:${S1}`, design: "design:x", session: S1, cwd: "/r/jkb", launch: "play", subject: null, title: "Play · x", created_at: "2026-10-08T00:00:00Z" };
  assert.deepEqual(decodeSessionPrompt(ok({ result: "design_prompt_of", session: S1, prompt })), ok(prompt));
  assert.deepEqual(decodeSessionPrompt(ok({ result: "design_prompt_of", session: "abc" })), ok(null), "no launch recorded it");
  assert.equal(decodeSessionPrompt(ok({ result: "design_prompt_of", prompt: { uid: "x" } })).ok, false);
  assert.equal(decodeSessionPrompt(ok({ result: "design_prompts", prompts: [] })).ok, false);

  assert.equal(parseGitdirFile("gitdir: ../../../.git/worktrees/build\n"), "../../../.git/worktrees/build");
  assert.equal(parseGitdirFile("gitdir:/abs/.git/worktrees/w"), "/abs/.git/worktrees/w");
  assert.equal(parseGitdirFile("[core]\n"), undefined);
  assert.equal(parseHead("ref: refs/heads/task/x-1\n"), "task/x-1");
  assert.equal(parseHead("0123456789abcdef0123456789abcdef01234567\n"), null, "detached");
  assert.equal(parseHead("ref: refs/remotes/origin/main"), undefined);
  assert.equal(parseHead(""), undefined);
  assert.equal(repoKeyOf("/home/vscode/repos/jkb/.jkb/work/build/"), "build", "a worktree is keyed by its own root, as gitrepo::key does");
  assert.equal(repoKeyOf("/"), undefined);

  const by = decodeBranchTasks(
    ok({
      result: "branch_tasks",
      tasks: { "task/a": [{ uid: "task:a", status: "in_progress", onto: "main" }, { uid: "task:b", status: "done" }] },
    }),
  );
  assert.ok(by.ok);
  assert.deepEqual(tasksOn(by.value, "task/a"), [
    { uid: "task:a", status: "in_progress", onto: "main" },
    { uid: "task:b", status: "done", onto: null },
  ]);
  assert.deepEqual(tasksOn(by.value, "main"), []);
  assert.deepEqual(tasksOn(by.value, null), [], "a detached HEAD records no branch");
  assert.equal(decodeBranchTasks(ok({ result: "branch_tasks", tasks: { a: [{ uid: 1 }] } })).ok, false);

  assert.ok(isSessionUuid(S1));
  assert.ok(isSessionUuid(S1.toUpperCase()));
  assert.equal(isSessionUuid("abc"), false);
});
