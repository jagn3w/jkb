//! Comment threads as @jkb/core reads them, and where the margin puts their cards. Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import { decodeThread, decodeThreads, parseThreadAnnouncement, stackCards, threadOps } from "../dist/index.js";

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
  const list = decodeThreads({ ok: true, value: { result: "design_thread_list", threads: [thread()] } });
  assert.equal(list.ok && list.value[0].comments[0].by, "operator");
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
