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
import test, { after, afterEach } from "node:test";

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
const { discussSpec, exclusive, repoDir } = await load(path.join(src, "renderer", "src", "design", "discuss.ts"));
const { SessionRegistry } = await load(path.join(src, "renderer", "src", "design", "registry.ts"));
const { INITIAL_LISTING, listingFailed, listingLoaded, listingLoading } = await load(path.join(src, "renderer", "src", "design", "listing.ts"));
const { LAUNCH_SCRIPT } = await load(path.join(src, "renderer", "src", "design", "launch.ts"));
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
        { seq: 10, kind: "prompt", payload: { design: "design:factory-1", prompt: "prompt:p" } },
        { seq: 11, kind: "prompt", payload: { design: "design:factory-1" } },
      ],
    }),
  );
  await until(() => daemon.calls.some((c) => c.request.op === "mq.ack"), "the ack");
  assert.deepEqual(
    heard.map((h) => [h.owner, h.event.kind, h.event.seq ?? h.event.prompt, h.event.update]),
    [
      [1, "update", 3, "AA=="],
      [2, "update", 3, "AA=="],
      [1, "update", 4, null],
      [2, "update", 4, null],
      [1, "prompt", "prompt:p", undefined],
      [2, "prompt", "prompt:p", undefined],
    ],
  );
  assert.equal(daemon.calls.find((c) => c.request.op === "mq.ack").request.seq, 11, "acked through the last message");
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

// Every session a test opens is disposed after it, passed or failed: one left retrying against a
// stand-in that stays down would keep its timers, and the run, alive forever.
const opened = [];
function Session(...args) {
  const s = new DesignSession(...args);
  opened.push(s);
  return s;
}
afterEach(() => {
  for (const s of opened.splice(0)) s.dispose();
});
const bodyOf = (doc) => doc.getText("body").toString();

test("a session loads the design, sends local edits as updates, and merges updates from elsewhere", async () => {
  const jkb = standInJkb("hello world");
  const s = Session(jkb.bridge, "d", TOPIC, quick);
  await s.open();
  assert.equal(s.text.toString(), "hello world");
  assert.equal(s.status.kind, "live");

  s.text.insert(5, ",");
  s.text.insert(12, "!");
  await s.idle();
  assert.equal(bodyOf(jkb.table), "hello, world!");
  const applies = jkb.asked.filter((r) => r.op === "design.apply");
  assert.ok(applies.length >= 1 && applies.length <= 2, "edits leave in order, merged while one is in flight");

  // The CLI edits: the announcement is a hint, and what the table holds is fetched.
  const before = jkb.asked.length;
  jkb.writeElsewhere((t) => t.insert(0, "Oh, "));
  await until(() => s.text.toString() === "Oh, hello, world!", "the announced update");
  assert.ok(jkb.asked.slice(before).some((r) => r.op === "design.state"), "an announcement is read from the table");

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
  const s = Session(jkb.bridge, "d", TOPIC, quick);
  await s.open();
  jkb.writeElsewhere((t) => t.insert(3, "d"), { announce: false });
  jkb.writeElsewhere((t) => t.insert(4, "e"));
  await until(() => s.text.toString() === "abcde", "the pending merge to complete");
  s.dispose();
});

test("spans are kept only from an answer whose text is the document's, and *Discuss* waits for one", async () => {
  const span = { uid: "span:x", reviewer: "operator", anchored: true, start: 0, end: 5, text: "hello", state: "APPROVED", demoted: false, pieces: [], approved_by: "operator", approved_at: null, steps: [] };
  const jkb = standInJkb("hello world", { spans: [span] });
  const s = Session(jkb.bridge, "d", TOPIC, quick);
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
  const t = Session(jkb2.bridge, "d", TOPIC, quick);
  await t.open();
  t.text.insert(7, " three");
  const settled = await t.settledVersion();
  assert.ok(settled.ok);
  assert.equal(settled.doc.text, "one two three");
  assert.equal(settled.doc.version.split(".")[0], String(jkb2.seq));
  t.dispose();
});

test("an announcement's bytes are never merged: a forged update cannot put text in the editor", async () => {
  const jkb = standInJkb("abc");
  const s = Session(jkb.bridge, "d", TOPIC, quick);
  await s.open();
  // Anyone who may send to the queue could announce any bytes, from any client and clock.
  const forger = new Y.Doc({ gc: false });
  forger.getText("body").insert(0, "INJECTED ");
  jkb.emit({ kind: "update", design: "d", seq: 99, update: b64(Y.encodeStateAsUpdate(forger)) });
  jkb.emit({ kind: "update", design: "d", seq: 100, update: b64(new Uint8Array([255, 1, 2])) });
  await until(() => jkb.asked.filter((r) => r.op === "design.state").length >= 2, "the re-read");
  await s.idle();
  await tick();
  assert.equal(s.text.toString(), "abc", "the table's text, not the announcement's");
  assert.notEqual(s.status.kind, "failed", "undecodable bytes in an announcement do not stop the session");
  s.dispose();
});

test("why there is no settled version is said as it is: stopped, unread, or moved", async () => {
  const jkb = standInJkb("abc");
  const s = Session(jkb.bridge, "d", TOPIC, quick);
  await s.open();
  const realOp = jkb.bridge.op;
  jkb.bridge.op = async (request) => (request.op === "design.cat" ? err("unavailable", "cannot reach jkb serve") : realOp(request));
  assert.deepEqual(await s.settledVersion(), { ok: false, why: { kind: "unread", message: "cannot reach jkb serve" } });
  jkb.bridge.op = async (request) => {
    // Another editor writes between the save and the read-back.
    if (request.op === "design.cat") jkb.writeElsewhere((t) => t.insert(0, "z"), { announce: false });
    return realOp(request);
  };
  assert.deepEqual(await s.settledVersion(), { ok: false, why: { kind: "moved" } });
  jkb.bridge.op = async (request) => (request.op === "design.apply" ? err("invalid", "the update does not apply") : realOp(request));
  s.text.insert(0, "x");
  await until(() => s.status.kind === "failed", "the refusal");
  const failed = await s.settledVersion();
  assert.equal(failed.ok, false);
  assert.equal(failed.why.kind, "failed");
  assert.match(failed.why.message, /does not apply/);
  s.dispose();
  assert.deepEqual(await s.settledVersion(), { ok: false, why: { kind: "closed" } });
});

test("a session is not editable until its first load has merged, nor after a refusal", async () => {
  const jkb = standInJkb("the real text");
  const realOp = jkb.bridge.op;
  let down = true;
  jkb.bridge.op = async (request) => (down && request.op === "design.state" ? err("unavailable", "down") : realOp(request));
  const s = Session(jkb.bridge, "d", TOPIC, quick);
  await s.open();
  assert.equal(s.status.kind, "retrying");
  assert.equal(s.editable, false, "an empty document before the load is not the design");
  down = false;
  await until(() => s.editable, "the load");
  assert.equal(s.text.toString(), "the real text");
  jkb.bridge.op = async (request) => (request.op === "design.apply" ? err("invalid", "no") : realOp(request));
  s.text.insert(0, "x");
  await until(() => s.status.kind === "failed", "the refusal");
  assert.equal(s.editable, false);
  s.dispose();
});

test("a failed pull does not cancel a failed edit's retry: both are retried, and the edit is sent", async () => {
  const jkb = standInJkb("abc");
  const s = Session(jkb.bridge, "d", TOPIC, { spansDelayMs: 0, minBackoffMs: 5, maxBackoffMs: 5 });
  await s.open();
  const realOp = jkb.bridge.op;
  let down = true;
  jkb.bridge.op = async (request) => (down ? err("unavailable", "down") : realOp(request));
  s.text.insert(3, "d");
  await until(() => s.status.kind === "retrying", "the edit's retry");
  // A gap while the daemon is still down: the pull fails too, and schedules its own retry.
  jkb.emit({ kind: "gap", message: "lost" });
  await tick();
  down = false;
  await until(() => bodyOf(jkb.table) === "abcd", "the edit, resent");
  await s.idle();
  await until(() => s.status.kind === "live", "Saved");
  s.dispose();
});

// ---- the window's registry: one session per design -----------------------------------------------

/** A registry over a stand-in jkb, counting feed joins and leaves; its sessions are disposed after. */
function registryOver(jkb, options = quick) {
  const feed = { subscribed: 0, unsubscribed: 0 };
  const bridge = {
    ...jkb.bridge,
    op: (request) => jkb.bridge.op(request),
    subscribe: async (topic) => {
      feed.subscribed += 1;
      return jkb.bridge.subscribe(topic);
    },
    unsubscribe: (topic) => {
      feed.unsubscribed += 1;
      jkb.bridge.unsubscribe(topic);
    },
  };
  const registry = new SessionRegistry(bridge, options);
  const notices = () => registry.notices.map((n) => n.message);
  const attach = () => {
    const s = registry.attach({ uid: "d", topic: TOPIC, title: "Code Factory" });
    opened.push(s);
    return s;
  };
  return { registry, feed, notices, attach };
}

/** Make the stand-in's daemon unreachable until `back()`. */
function downUntilBack(jkb) {
  const realOp = jkb.bridge.op;
  let down = true;
  jkb.bridge.op = async (request) => (down ? err("unavailable", "down") : realOp(request));
  return {
    back() {
      down = false;
    },
  };
}

test("a design reopened while its old pane's edits are unsent reuses the session, and stays live", async () => {
  const jkb = standInJkb("abc");
  const { registry, feed, notices, attach } = registryOver(jkb);
  const first = attach();
  await until(() => first.status.kind === "live", "the load");
  const daemon = downUntilBack(jkb);
  first.text.insert(3, " typed");
  await until(() => first.status.kind === "retrying", "the retry");
  registry.detach(first);
  assert.deepEqual(notices(), ["Edits to Code Factory are still being saved in the background."]);

  const again = attach();
  assert.equal(again, first, "one session per design: the pane reattaches to the one still sending");
  assert.deepEqual(notices(), [], "shown again, its own status says how saving goes");
  daemon.back();
  await until(() => bodyOf(jkb.table) === "abc typed", "the old pane's edits, sent");
  await until(() => again.status.kind === "live", "Saved");
  assert.equal(feed.subscribed, 1);
  assert.equal(feed.unsubscribed, 0, "nothing ended the feed under the reopened pane");
  // Still hearing other editors.
  jkb.writeElsewhere((t) => t.insert(0, ">"));
  await until(() => again.text.toString() === ">abc typed", "a later edit from elsewhere");
  registry.detach(again);
  assert.equal(registry.session("d"), undefined, "nothing unsent and no pane: disposed");
  assert.equal(feed.unsubscribed, 1);
});

test("a detached session sends what it holds, then is disposed and leaves the feed", async () => {
  const jkb = standInJkb("abc");
  // No span re-read to wake the registry: the drained outbox alone must.
  const { registry, feed, notices, attach } = registryOver(jkb, { ...quick, spansDelayMs: 60_000 });
  const s = attach();
  await until(() => s.status.kind === "live", "the load");
  const daemon = downUntilBack(jkb);
  s.text.insert(3, " typed while away");
  await until(() => s.status.kind === "retrying", "the retry");
  registry.detach(s);
  assert.equal(registry.session("d"), s, "kept while it holds edits");
  assert.equal(notices().length, 1);
  daemon.back();
  await until(() => bodyOf(jkb.table) === "abc typed while away", "the held edit, sent after the pane left");
  await until(() => registry.session("d") === undefined, "the dispose once sent");
  assert.ok(s.doc.isDestroyed);
  assert.deepEqual(notices(), [], "\"still being saved\" is withdrawn once saved");
  assert.equal(feed.unsubscribed, 1);
});

test("a detached session's refused edit is reported naming the design, and its retries are bounded", async () => {
  const refused = standInJkb("abc");
  const r1 = registryOver(refused);
  const s = r1.attach();
  await until(() => s.status.kind === "live", "the load");
  const realOp = refused.bridge.op;
  let answer = err("unavailable", "down");
  refused.bridge.op = async (request) => (request.op === "design.apply" ? answer : realOp(request));
  s.text.insert(3, "d");
  await until(() => s.status.kind === "retrying", "the retry");
  r1.registry.detach(s);
  answer = err("invalid", "the update does not apply");
  await until(() => r1.registry.session("d") === undefined, "the dispose");
  assert.deepEqual(r1.notices(), ["Edits to Code Factory were not saved: invalid: the update does not apply"]);

  // A daemon that never answers: a detached session gives up after its retries, and says so.
  const gone = standInJkb("abc");
  const r2 = registryOver(gone, { ...quick, detachedRetries: 3 });
  const t = r2.attach();
  await until(() => t.status.kind === "live", "the load");
  downUntilBack(gone);
  t.text.insert(3, "d");
  await until(() => t.status.kind === "retrying", "the retry");
  r2.registry.detach(t);
  await until(() => r2.registry.session("d") === undefined, "the give-up");
  assert.match(r2.notices().at(-1), /^Edits to Code Factory were not saved: not saved after 3 retries/);
});

test("a report of lost edits stays until it is dismissed, whatever is said after it", async () => {
  const jkb = standInJkb("abc");
  const { registry, notices, attach } = registryOver(jkb);
  const s = attach();
  await until(() => s.status.kind === "live", "the load");
  const realOp = jkb.bridge.op;
  jkb.bridge.op = async (request) => (request.op === "design.apply" ? err("unavailable", "down") : realOp(request));
  s.text.insert(3, "d");
  await until(() => s.status.kind === "retrying", "the retry");
  registry.detach(s);
  jkb.bridge.op = async (request) => (request.op === "design.apply" ? err("invalid", "no") : realOp(request));
  await until(() => registry.session("d") === undefined, "the refusal");
  // Later notices — another design's, a Discuss not started — and a design reopened do not erase it.
  registry.notify("other", "closed", "A Discuss of Other was not started.");
  jkb.bridge.op = realOp;
  const t = attach();
  await until(() => t.status.kind === "live", "the fresh load");
  assert.deepEqual(notices(), ["Edits to Code Factory were not saved: invalid: no", "A Discuss of Other was not started."]);
  registry.notify("other", "closed", "A Discuss of Other was not started.");
  assert.equal(notices().length, 2, "the same notice is not repeated");
  // The reopened design is left with edits unsent, then they land: only "still being saved" goes.
  const daemon = downUntilBack(jkb);
  t.text.insert(0, "x");
  await until(() => t.status.kind === "retrying", "the retry");
  registry.detach(t);
  assert.equal(notices().length, 3);
  daemon.back();
  await until(() => registry.session("d") === undefined, "the save");
  assert.deepEqual(notices(), ["Edits to Code Factory were not saved: invalid: no", "A Discuss of Other was not started."]);
  const lost = registry.notices[0];
  registry.dismiss(lost.id);
  assert.deepEqual(notices(), ["A Discuss of Other was not started."]);
});

test("a Discuss waiting on a design the pane has left resolves as closed", async () => {
  const jkb = standInJkb("abc");
  // Retries far from their bound: the wait ends because the pane left, not because the session gave up.
  const { registry, attach } = registryOver(jkb, { ...quick, detachedRetries: 1_000_000 });
  const s = attach();
  await until(() => s.status.kind === "live", "the load");
  downUntilBack(jkb);
  s.text.insert(3, "d");
  const waiting = s.settledVersion();
  await until(() => s.status.kind === "retrying", "the retry");
  registry.detach(s);
  const late = new Promise((r) => setTimeout(() => r("still waiting"), 2000));
  assert.deepEqual(await Promise.race([waiting, late]), { ok: false, why: { kind: "closed" } });
  assert.notEqual(s.status.kind, "failed");
});

test("a stopped session is not reused: reopening the design opens a fresh one", async () => {
  const jkb = standInJkb("abc");
  const { registry, attach } = registryOver(jkb);
  const s = attach();
  await until(() => s.status.kind === "live", "the load");
  const realOp = jkb.bridge.op;
  jkb.bridge.op = async (request) => (request.op === "design.apply" ? err("invalid", "no") : realOp(request));
  s.text.insert(0, "x");
  await until(() => s.status.kind === "failed", "the refusal");
  jkb.bridge.op = realOp;
  const t = attach();
  assert.notEqual(t, s);
  assert.ok(s.doc.isDestroyed);
  await until(() => t.status.kind === "live", "the fresh load");
  registry.detach(t);
});

test("a design with no topic yet is joined once it has one, and re-read then", async () => {
  const jkb = standInJkb("");
  let topicExists = false;
  const joins = [];
  jkb.bridge.subscribe = async () => {
    joins.push(topicExists);
    return topicExists ? ok(null) : err("no_such_topic", "no topic");
  };
  const s = Session(jkb.bridge, "d", TOPIC, quick);
  await s.open();
  assert.match(s.feedProblem, /no topic/);
  // The CLI's first edit creates the topic; this editor never heard it announced.
  jkb.writeElsewhere((t) => t.insert(0, "first words"), { announce: false });
  topicExists = true;
  await until(() => s.feedProblem === undefined, "the join");
  await until(() => s.text.toString() === "first words", "the re-read after the join");
  assert.ok(joins.length >= 2);
  s.dispose();
});

test("a refresh keeps the designs it has while it loads, and when it fails", () => {
  const a = { uid: "design:a", title: "A", namespace: "designs/jkb", topic: "design/design.a" };
  let l = listingLoaded([a]);
  l = listingLoading(l);
  assert.deepEqual(l.designs, [a], "loading keeps the list, so the open design stays open");
  assert.equal(l.loading, true);
  l = listingFailed(l, "cannot reach jkb serve");
  assert.deepEqual(l.designs, [a]);
  assert.equal(l.error, "cannot reach jkb serve");
  assert.equal(listingLoaded([]).error, undefined);
  assert.equal(listingFailed(INITIAL_LISTING, "x").designs, undefined, "before any read there is no list to keep");
});

test("a Discuss is one at a time: a click while one is pending is ignored", async () => {
  let release;
  let runs = 0;
  const run = exclusive(async () => {
    runs += 1;
    await new Promise((r) => {
      release = r;
    });
  });
  const first = run();
  await run();
  assert.equal(runs, 1);
  release();
  await first;
  const again = run();
  assert.equal(runs, 2, "once the first is done, the next runs");
  release();
  await again;
});

test("a refused edit stops the session; an unreachable daemon is retried and the edit kept", async () => {
  const jkb = standInJkb("abc");
  const s = Session(jkb.bridge, "d", TOPIC, quick);
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
  const s = Session(jkb.bridge, "d", TOPIC, quick);
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

test("typing inside a drawn mark does not widen it: new words read PROPOSED until the next answer", async () => {
  const { mapStates } = await load(path.join(src, "renderer", "src", "design", "editor.ts"));
  const { Decoration } = require("@codemirror/view");
  const { ChangeSet } = require("@codemirror/state");
  const approved = Decoration.mark({ class: "cm-state cm-state-approved cm-state-span", state: "APPROVED" });
  const marks = Decoration.set([approved.range(0, 5)]);
  const runs = (set) => {
    const out = [];
    for (const it = set.iter(); it.value !== null; it.next()) out.push([it.from, it.to, it.value.spec.state]);
    return out;
  };
  // "hello" → "heXYZllo": the insert lands strictly inside the approved mark.
  assert.deepEqual(runs(mapStates(marks, ChangeSet.of({ from: 2, insert: "XYZ" }, 10))), [
    [0, 2, "APPROVED"],
    [2, 5, "PROPOSED"],
    [5, 8, "APPROVED"],
  ]);
  // A replacement inside it: the new words are PROPOSED, the rest keep their state.
  assert.deepEqual(runs(mapStates(marks, ChangeSet.of({ from: 1, to: 3, insert: "AB" }, 10))), [
    [0, 1, "APPROVED"],
    [1, 3, "PROPOSED"],
    [3, 5, "APPROVED"],
  ]);
  // A deletion only shrinks it.
  assert.deepEqual(runs(mapStates(marks, ChangeSet.of({ from: 1, to: 3 }, 10))), [[0, 3, "APPROVED"]]);
});

// ---- Discuss -------------------------------------------------------------------------------------

const ROOTS = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: "/Users/me/repos", hostReposReal: "/Users/me/repos", hostHome: "/Users/me" };

test("Discuss runs Claude in the design's repo with the prompt as one argument, never as shell", () => {
  const prompt = { kind: "discuss", uid: "design:x", title: "Code Factory", version: "3.AQ", start: 0, end: 4, quote: "text", occurrence: null, spans: [], prompt: "it's $(rm -rf /) `x` \"q\"" };
  const uuid = "0f8fad5b-d9cb-469f-a165-70867728950e";
  const spec = discussSpec(prompt, "jkb", ROOTS, uuid);
  assert.deepEqual(spec, {
    target: "container",
    cwd: "/home/vscode/repos/jkb",
    // Recorded as one of the design's prompts (no subject: the design itself), then Claude.
    argv: ["/bin/bash", "-lc", LAUNCH_SCRIPT, "claude", "design:x", uuid, "discuss", "", "Discuss · Code Factory", prompt.prompt],
    title: "Discuss · Code Factory",
    sessionUuid: uuid,
  });
  assert.ok(!LAUNCH_SCRIPT.includes(prompt.prompt));
  assert.equal(repoDir(ROOTS, ".."), "/home/vscode/repos", "a repo name that is not one directory stays at the root");
  assert.equal(repoDir(ROOTS, "a/b"), "/home/vscode/repos");
  assert.equal(discussSpec({ ...prompt, title: "t".repeat(400) }, "jkb", ROOTS, uuid).title.length, 200);
});
