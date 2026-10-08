//! The Design tab's Document pane (D53.4–5) without a window: main's live-update feeds against a
//! stand-in daemon, the renderer's design session against a stand-in jkb (a Yjs document playing the
//! table), the *Discuss* terminal spec, and the margin's line state.
//
// Bundled with esbuild, as in shell.test.mjs. `yjs` is left external, so the test's own documents and
// the session's are one Yjs.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-design-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const require = createRequire(import.meta.url);

async function load(entry) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({
    entryPoints: [entry],
    bundle: true,
    format: "cjs",
    platform: "node",
    outfile,
    logLevel: "silent",
    // The bundle is written outside the package, so `yjs` is kept external by its resolved path.
    plugins: [
      {
        name: "external-yjs",
        setup(build) {
          build.onResolve({ filter: /^yjs$/ }, () => ({ path: require.resolve("yjs"), external: true }));
        },
      },
    ],
  });
  return require(outfile);
}

const src = path.join(here, "..", "src");
const { DesignFeeds, DESIGN_GROUP } = await load(path.join(src, "main", "designFeeds.ts"));
const { DesignSession } = await load(path.join(src, "renderer", "src", "design", "session.ts"));
const { discussSpec, repoDir, DISCUSS_SCRIPT } = await load(path.join(src, "renderer", "src", "design", "discuss.ts"));
const Y = require("yjs");

const TOPIC = "design/design.factory-1";
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

// ---- main's feeds --------------------------------------------------------------------------------

/** A daemon that answers each op from a script of handlers, recording what it was asked. */
function scriptedDaemon(handlers) {
  const calls = [];
  const queue = [];
  const op = async (request, options) => {
    calls.push({ request, options });
    const h = handlers[request.op];
    if (h === undefined) return err("bad_request", `unexpected ${request.op}`);
    return h(request, options);
  };
  return { op, calls, queue };
}

/** A poll answer that waits until released, as a held long-poll does. */
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

test("a feed joins the app's group, then delivers each update to every window holding it, and acks", async () => {
  const polls = heldPolls();
  const daemon = scriptedDaemon({
    "mq.group_create": () => ok({ result: "created", created: true }),
    "mq.poll": polls.poll,
    "mq.ack": (r) => ok({ result: "position", position: r.seq }),
  });
  const heard = [];
  const feeds = new DesignFeeds(daemon.op, (owner, event) => heard.push({ owner, event }), { sleep: tick });
  const joined = await feeds.subscribe(1, TOPIC);
  assert.deepEqual(joined, ok(null));
  assert.deepEqual(daemon.calls[0].request, { op: "mq.group_create", topic: TOPIC, group: DESIGN_GROUP });
  assert.deepEqual(await feeds.subscribe(2, TOPIC), ok(null), "a second window shares the joined feed");
  await until(() => polls.waiting === 1, "the long-poll");
  const poll = daemon.calls.find((c) => c.request.op === "mq.poll");
  assert.equal(poll.request.group, DESIGN_GROUP);
  assert.ok(poll.options.waitMs > 0, "a poll is a long-poll");

  polls.answer(
    ok({
      result: "messages",
      messages: [
        { seq: 7, kind: "update", payload: { design: "design:factory-1", seq: 3, update: "AA==" } },
        { seq: 8, kind: "other", payload: {} },
        { seq: 9, kind: "update", payload: { design: "design:factory-1", seq: 4, update: null } },
      ],
    }),
  );
  await until(() => daemon.calls.some((c) => c.request.op === "mq.ack"), "the ack");
  assert.deepEqual(
    heard.map((h) => [h.owner, h.event.kind, h.event.seq, h.event.update]),
    [
      [1, "update", 3, "AA=="],
      [2, "update", 3, "AA=="],
      [1, "update", 4, null],
      [2, "update", 4, null],
    ],
  );
  assert.equal(daemon.calls.find((c) => c.request.op === "mq.ack").request.seq, 9, "acked through the last message");
  assert.equal(daemon.calls.filter((c) => c.request.op === "mq.group_create").length, 1, "one feed per topic");

  // The feed ends when its last window goes: after the held poll answers, nothing more is asked.
  feeds.unsubscribe(1, TOPIC);
  feeds.closeAll(2);
  await until(() => polls.waiting === 1, "the next poll");
  polls.answer(ok({ result: "messages", messages: [] }));
  await until(() => feeds.topics.length === 0, "the feed to end");
  const asked = daemon.calls.length;
  await tick();
  assert.equal(daemon.calls.length, asked);
});

test("a feed waits out an unreachable daemon, says so, and says when it is back", async () => {
  let down = 2;
  const polls = heldPolls();
  const daemon = scriptedDaemon({
    "mq.group_create": () => (down-- > 0 ? err("unavailable", "cannot reach jkb serve") : ok({ result: "created", created: true })),
    "mq.poll": polls.poll,
  });
  const heard = [];
  const feeds = new DesignFeeds(daemon.op, (_owner, event) => heard.push(event), { sleep: tick });
  const joined = await feeds.subscribe(1, TOPIC);
  assert.deepEqual(joined, ok(null), "the subscriber waited for the join rather than failing");
  assert.deepEqual(
    heard.map((e) => e.kind),
    ["error", "error", "live"],
  );
  assert.match(heard[0].message, /cannot reach/);
  feeds.closeAll();
  await until(() => polls.waiting === 1, "the poll");
  polls.answer(ok({ result: "messages", messages: [] }));
  await until(() => feeds.topics.length === 0, "the feed to end");
});

test("a group the queue removed is rejoined, and the window is told it may have missed updates", async () => {
  let polled = 0;
  const polls = heldPolls();
  const daemon = scriptedDaemon({
    "mq.group_create": () => ok({ result: "created", created: true }),
    "mq.poll": () => (polled++ === 0 ? err("no_such_group") : polls.poll()),
  });
  const heard = [];
  const feeds = new DesignFeeds(daemon.op, (_owner, event) => heard.push(event), { sleep: tick });
  await feeds.subscribe(1, TOPIC);
  await until(() => polls.waiting === 1, "the poll after the rejoin");
  assert.equal(daemon.calls.filter((c) => c.request.op === "mq.group_create").length, 2);
  assert.deepEqual(heard.map((e) => e.kind), ["gap"]);
  feeds.closeAll();
  polls.answer(ok({ result: "messages", messages: [] }));
  await until(() => feeds.topics.length === 0, "the feed to end");
});

test("only a design topic is subscribed to, and a design with no topic is refused, not retried", async () => {
  const daemon = scriptedDaemon({ "mq.group_create": () => err("no_such_topic", "no topic") });
  const feeds = new DesignFeeds(daemon.op, () => assert.fail("nothing to deliver"), { sleep: tick });
  for (const topic of ["claude/notify", "design/a/b", 5, undefined]) {
    const r = await feeds.subscribe(1, topic);
    assert.equal(r.ok, false);
    assert.equal(r.error.code, "bad_request");
  }
  assert.equal(daemon.calls.length, 0, "a refused topic asks the daemon nothing");
  const r = await feeds.subscribe(1, TOPIC);
  assert.equal(r.ok, false);
  assert.equal(r.error.code, "no_such_topic");
  assert.deepEqual(feeds.topics, []);
  assert.equal(daemon.calls.length, 1);
});

// ---- the renderer's session ----------------------------------------------------------------------

const b64 = (bytes) => Buffer.from(bytes).toString("base64");
const unb64 = (text) => new Uint8Array(Buffer.from(text, "base64"));

/** jkb, played by a Yjs document: the table is the merge of every update it accepted. */
function standInJkb(initial, { spans = [] } = {}) {
  const table = new Y.Doc({ gc: false });
  table.getText("body").insert(0, initial);
  let seq = 1;
  const listeners = new Set();
  const asked = [];
  const version = () => `${seq}.${b64(Y.encodeStateVector(table))}`;
  const jkb = {
    table,
    asked,
    get seq() {
      return seq;
    },
    refuseApply: undefined,
    spans,
    /** Another editor writes: stored, then announced. */
    writeElsewhere(change, { announce = true, inline = true } = {}) {
      let update;
      const capture = (u) => {
        update = u;
      };
      table.on("update", capture);
      table.transact(() => change(table.getText("body")));
      table.off("update", capture);
      seq += 1;
      if (announce) {
        for (const l of listeners) l({ topic: TOPIC, kind: "update", design: "d", seq, update: inline ? b64(update) : null });
      }
    },
    emit(event) {
      for (const l of listeners) l({ topic: TOPIC, ...event });
    },
    bridge: {
      async op(request) {
        asked.push(request);
        switch (request.op) {
          case "design.state": {
            const since = request.since === undefined ? undefined : unb64(request.since);
            return ok({ result: "design_update", update: b64(Y.encodeStateAsUpdate(table, since)), version: version(), seq });
          }
          case "design.apply": {
            if (jkb.refuseApply !== undefined) return jkb.refuseApply;
            Y.applyUpdate(table, unb64(request.update));
            seq += 1;
            return ok({ result: "design_written", written: { uid: "d", seq, version: version(), demoted: [] } });
          }
          case "design.cat": {
            const text = table.getText("body").toString();
            return ok({
              result: "design_text",
              design: { uid: "d", title: "T", text, marked: text, version: version(), seq, topic: TOPIC, spans: jkb.spans },
            });
          }
          default:
            return err("bad_request", request.op);
        }
      },
      subscribe: async () => ok(null),
      unsubscribe: () => undefined,
      onEvent(listener) {
        listeners.add(listener);
        return () => listeners.delete(listener);
      },
    },
  };
  return jkb;
}

const quick = { spansDelayMs: 0, minBackoffMs: 1, maxBackoffMs: 2 };
const bodyOf = (doc) => doc.getText("body").toString();

test("a session loads the design, sends local edits as updates, and merges updates from elsewhere", async () => {
  const jkb = standInJkb("hello world");
  const s = new DesignSession(jkb.bridge, "d", TOPIC, quick);
  await s.open();
  assert.equal(s.text.toString(), "hello world");
  assert.equal(s.status.kind, "live");

  s.text.insert(5, ",");
  s.text.insert(12, "!");
  await s.idle();
  assert.equal(bodyOf(jkb.table), "hello, world!");
  const applies = jkb.asked.filter((r) => r.op === "design.apply");
  assert.ok(applies.length >= 1 && applies.length <= 2, "edits leave in order, merged while one is in flight");

  // The CLI edits: announced inline, merged without asking.
  const before = jkb.asked.length;
  jkb.writeElsewhere((t) => t.insert(0, "Oh, "));
  assert.equal(s.text.toString(), "Oh, hello, world!");
  assert.ok(!jkb.asked.slice(before).some((r) => r.op === "design.state"), "an inline update needs no read");

  // Too large to announce inline: the session fetches what it lacks.
  jkb.writeElsewhere((t) => t.insert(t.length, " Bye."), { inline: false });
  await until(() => s.text.toString().endsWith("Bye."), "the fetched update");

  // Missed entirely, then a gap is announced: the session catches up from its state vector.
  jkb.writeElsewhere((t) => t.delete(0, 4), { announce: false });
  assert.equal(s.text.toString(), "Oh, hello, world! Bye.");
  jkb.emit({ kind: "gap", message: "lost" });
  await until(() => s.text.toString() === "hello, world! Bye.", "the catch-up");
  s.dispose();
});

test("an update built on one the session missed is completed from the table", async () => {
  const jkb = standInJkb("abc");
  const s = new DesignSession(jkb.bridge, "d", TOPIC, quick);
  await s.open();
  jkb.writeElsewhere((t) => t.insert(3, "d"), { announce: false });
  jkb.writeElsewhere((t) => t.insert(4, "e"));
  await until(() => s.text.toString() === "abcde", "the pending merge to complete");
  s.dispose();
});

test("spans are kept only from an answer whose text is the document's, and *Discuss* waits for one", async () => {
  const span = { uid: "span:x", reviewer: "operator", anchored: true, start: 0, end: 5, text: "hello", state: "APPROVED", demoted: false, pieces: [], approved_by: "operator", approved_at: null, steps: [] };
  const jkb = standInJkb("hello world", { spans: [span] });
  const s = new DesignSession(jkb.bridge, "d", TOPIC, quick);
  await s.open();
  await until(() => s.current !== undefined, "the first spans");
  assert.equal(s.current.spans[0].uid, "span:x");

  // An answer read while the document differs is dropped.
  const kept = s.current;
  jkb.refuseApply = new Promise(() => undefined); // the apply hangs: the table lags the document
  s.text.insert(0, "x");
  assert.equal(await s.refreshSpans(), false);
  assert.equal(s.current, kept);
  s.dispose();

  // Settled: every edit sent, and the answer's text is what is shown.
  const jkb2 = standInJkb("one two");
  const t = new DesignSession(jkb2.bridge, "d", TOPIC, quick);
  await t.open();
  t.text.insert(7, " three");
  const settled = await t.settledVersion();
  assert.equal(settled.text, "one two three");
  assert.equal(settled.version.split(".")[0], String(jkb2.seq));
  t.dispose();
});

test("a refused edit stops the session; an unreachable daemon is retried and the edit kept", async () => {
  const jkb = standInJkb("abc");
  const s = new DesignSession(jkb.bridge, "d", TOPIC, quick);
  await s.open();
  let attempts = 0;
  const realOp = jkb.bridge.op;
  jkb.bridge.op = async (request) => {
    if (request.op === "design.apply" && attempts++ < 2) return err("unavailable", "down");
    return realOp(request);
  };
  s.text.insert(3, "d");
  await until(() => bodyOf(jkb.table) === "abcd", "the retried edit");
  assert.equal(s.status.kind, "live");

  jkb.bridge.op = async (request) =>
    request.op === "design.apply" ? err("invalid", "the update does not apply") : realOp(request);
  s.text.insert(4, "e");
  await until(() => s.status.kind === "failed", "the refusal");
  assert.match(s.status.message, /does not apply/);
  await s.idle();
  s.dispose();
});

test("a feed problem is shown beside the status and cleared when the feed is back", async () => {
  const jkb = standInJkb("abc");
  const s = new DesignSession(jkb.bridge, "d", TOPIC, quick);
  await s.open();
  jkb.emit({ kind: "error", message: "cannot reach jkb serve" });
  assert.equal(s.feedProblem, "cannot reach jkb serve");
  assert.equal(s.status.kind, "live", "saving is unaffected");
  jkb.emit({ kind: "live" });
  assert.equal(s.feedProblem, undefined);
  s.dispose();
});

// ---- the margin ----------------------------------------------------------------------------------

test("a line's margin shows its least advanced state, and a run that only touches it is its neighbour's", async () => {
  const { lineState } = await load(path.join(src, "renderer", "src", "design", "editor.ts"));
  const { Decoration } = require("@codemirror/view");
  const mark = (state) => Decoration.mark({ state });
  // "aaaa\nbbbb\ncccc": lines at 0–4, 5–9, 10–14.
  const marks = Decoration.set([
    mark("APPROVED").range(0, 5),
    mark("PROPOSED").range(5, 7),
    mark("STAGED").range(7, 10),
    mark("IMPLEMENTED").range(10, 14),
  ]);
  assert.equal(lineState(marks, 0, 4), "APPROVED", "the run ending at the next line's start is not that line's");
  assert.equal(lineState(marks, 5, 9), "PROPOSED", "a line mixing states shows the least advanced");
  assert.equal(lineState(marks, 10, 14), "IMPLEMENTED");
  assert.equal(lineState(Decoration.none, 0, 4), undefined);
});

// ---- Discuss -------------------------------------------------------------------------------------

const ROOTS = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: "/Users/me/repos", hostHome: "/Users/me" };

test("Discuss runs Claude in the design's repo with the prompt as one argument, never as shell", () => {
  const prompt = { kind: "discuss", uid: "design:x", title: "Code Factory", version: "3.AQ", start: 0, end: 4, quote: "text", occurrence: null, spans: [], prompt: "it's $(rm -rf /) `x` \"q\"" };
  const uuid = "0f8fad5b-d9cb-469f-a165-70867728950e";
  const spec = discussSpec(prompt, "jkb", ROOTS, uuid);
  assert.deepEqual(spec, {
    target: "container",
    cwd: "/home/vscode/repos/jkb",
    argv: ["/bin/bash", "-lc", DISCUSS_SCRIPT, "claude", uuid, prompt.prompt],
    title: "Discuss · Code Factory",
    sessionUuid: uuid,
  });
  assert.ok(!DISCUSS_SCRIPT.includes(prompt.prompt));
  assert.equal(repoDir(ROOTS, ".."), "/home/vscode/repos", "a repo name that is not one directory stays at the root");
  assert.equal(repoDir(ROOTS, "a/b"), "/home/vscode/repos");
  assert.equal(discussSpec({ ...prompt, title: "t".repeat(400) }, "jkb", ROOTS, uuid).title.length, 200);
});

test("the script passes its arguments through bash untouched", async () => {
  const { execFileSync } = await import("node:child_process");
  const nasty = "it's $(echo pwned) `x` \"q\" $HOME";
  const out = execFileSync("/bin/bash", ["-c", DISCUSS_SCRIPT.replace("exec claude", "printf '%s|%s|%s'"), "claude", "id", nasty], {
    encoding: "utf8",
  });
  assert.equal(out, `--session-id|id|${nasty}`);
});
