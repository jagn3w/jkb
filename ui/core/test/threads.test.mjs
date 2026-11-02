//! Comment threads as @jkb/core reads them, and where the margin puts their cards. Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import { callerOps, decodeThread, decodeThreads, decodeWhoAmI, parseThreadAnnouncement, stackCards, threadOps } from "../dist/index.js";

const thread = (over = {}) => ({
  uid: "thread:a",
  design: "design:x",
  anchored: true,
  start: 3,
  end: 9,
  quote: "words",
  started_on: "words",
  status: "open",
  comments: [{ uid: "comment:a", by: "operator", at: "2026-10-11T00:00:00Z", text: "why?" }],
  ...over,
});

test("threads decode from their answers, and a malformed one is refused", () => {
  const list = decodeThreads({ ok: true, value: { result: "design_thread_list", threads: [thread()], version: "3.AA" } });
  assert.equal(list.ok && list.value.threads[0].comments[0].by, "operator");
  assert.equal(list.ok && list.value.version, "3.AA");
  assert.equal(decodeThreads({ ok: true, value: { result: "design_thread_list", threads: [] } }).ok, false, "no version");
  const one = decodeThread({ ok: true, value: { result: "design_thread", thread: thread({ status: "resolved" }) } });
  assert.equal(one.ok && one.value.status, "resolved");
  const bad = decodeThread({ ok: true, value: { result: "design_thread", thread: { uid: "x" } } });
  assert.equal(bad.ok, false);
});

test("the ops name the selection by its offsets", () => {
  assert.deepEqual(threadOps.start("design:x", "1.AA", 3, 9, "why?"), {
    op: "design.thread_start",
    uid: "design:x",
    base: "1.AA",
    start: 3,
    end: 9,
    text: "why?",
  });
  assert.equal(threadOps.approveSelection("d", "b", 0, 1).op, "design.approve_selection");
});

test("an announcement names its thread", () => {
  assert.deepEqual(parseThreadAnnouncement({ design: "d", thread: "t", what: "replied" }), {
    design: "d",
    thread: "t",
    what: "replied",
  });
  assert.equal(parseThreadAnnouncement({ design: "d" }), undefined);
});

test("cards keep their place unless the card above would overlap", () => {
  assert.deepEqual(
    stackCards([
      { want: 0, height: 50 },
      { want: 20, height: 30 },
      { want: 200, height: 10 },
    ]),
    [0, 58, 200],
  );
});

test("who Code Factory is connected as decodes from role.whoami, and a malformed answer is refused", () => {
  assert.deepEqual(callerOps.whoami(), { op: "role.whoami" });
  const whoami = (w, result = "who_am_i") => ({ ok: true, value: { result, whoami: w } });
  const grant = { label: "grant:3", roles: ["designer"], task: null };
  assert.deepEqual(decodeWhoAmI(whoami(grant)), { ok: true, value: grant });
  const scoped = decodeWhoAmI(whoami({ label: "ticket:9", roles: ["implementer"], task: "task:x" }));
  assert.equal(scoped.ok && scoped.value.task, "task:x");
  assert.equal(decodeWhoAmI(whoami(grant, "grants")).ok, false, "a wrong result tag");
  assert.equal(decodeWhoAmI(whoami({ ...grant, roles: "designer" })).ok, false, "roles not an array");
  assert.equal(decodeWhoAmI(whoami({ ...grant, roles: ["designer", 3] })).ok, false, "a role not a string");
  assert.equal(decodeWhoAmI(whoami({ ...grant, label: 3 })).ok, false, "a label not a string");
  assert.equal(decodeWhoAmI(whoami({ ...grant, task: 3 })).ok, false, "a task neither a uid nor null");
  const refused = { ok: false, error: { code: "unauthorized", message: "no such token" } };
  assert.equal(decodeWhoAmI(refused), refused, "a refusal passes through unchanged");
});
