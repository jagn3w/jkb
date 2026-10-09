//! Designs as `@jkb/core` reads them (D53.4–5): the answers' shapes, base64 for Yjs bytes, a
//! live-update message, and which state each piece of text is drawn in. Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import {
  decodeDesignDoc,
  decodeDesignPrompt,
  decodeDesigns,
  decodeDesignUpdate,
  decodeDesignWritten,
  designOps,
  designRepos,
  fromBase64,
  isDesignTopic,
  parseAnnouncement,
  repoOf,
  stateRuns,
  toBase64,
} from "../dist/index.js";

const ok = (value) => ({ ok: true, value });

const span = (over = {}) => ({
  uid: "span:a",
  reviewer: "operator",
  anchored: true,
  start: 0,
  end: 0,
  text: "",
  state: "APPROVED",
  demoted: false,
  pieces: [],
  approved_by: null,
  approved_at: null,
  steps: [],
  ...over,
});

test("base64 round-trips every length and refuses what is not padded standard base64", () => {
  for (let n = 0; n < 70; n++) {
    const bytes = Uint8Array.from({ length: n }, (_, i) => (i * 37 + n) & 255);
    const text = toBase64(bytes);
    assert.equal(text, Buffer.from(bytes).toString("base64"), `length ${n}`);
    assert.deepEqual(fromBase64(text), bytes, `length ${n}`);
  }
  for (const bad of ["abc", "ab=c", "a=bc", "@@@@", "AA==AA==", "A==="]) {
    assert.equal(fromBase64(bad), undefined, bad);
  }
});

test("answers decode by their result tag, and a malformed one is an internal failure", () => {
  const design = { uid: "design:x", title: "T", namespace: "designs/jkb", seq: 2, topic: "design/design.x" };
  assert.deepEqual(decodeDesigns(ok({ result: "designs", designs: [design] })), ok([design]));
  const wrong = decodeDesigns(ok({ result: "design_text", design: {} }));
  assert.equal(wrong.ok, false);
  assert.equal(wrong.error.code, "internal");
  const refused = { ok: false, error: { code: "forbidden", message: "no" } };
  assert.equal(decodeDesigns(refused), refused, "a refusal passes through untouched");

  const doc = { ...design, text: "abc", marked: "abc", version: "1.AQ", spans: [span({ end: 2 })] };
  delete doc.namespace;
  assert.deepEqual(decodeDesignDoc(ok({ result: "design_text", design: doc })), ok(doc));
  assert.equal(decodeDesignDoc(ok({ result: "design_text", design: { ...doc, spans: [{ uid: 1 }] } })).ok, false);

  const update = { result: "design_update", update: "AA==", version: "1.AQ", seq: 1 };
  assert.deepEqual(decodeDesignUpdate(ok(update)), ok(update));
  const written = { uid: "design:x", seq: null, version: "1.AQ", demoted: [] };
  assert.deepEqual(decodeDesignWritten(ok({ result: "design_written", written })), ok(written));
  const prompt = { kind: "discuss", uid: "design:x", title: "T", version: "1.AQ", start: 0, end: 1, quote: "a", occurrence: null, spans: [], prompt: "p" };
  assert.deepEqual(decodeDesignPrompt(ok({ result: "design_prompt", prompt })), ok(prompt));
  assert.equal(decodeDesignPrompt(ok({ result: "design_prompt", prompt: { ...prompt, prompt: "" } })).ok, false);
});

test("requests are the ops' own shapes", () => {
  assert.deepEqual(designOps.list(), { op: "design.list" });
  assert.deepEqual(designOps.list("jkb"), { op: "design.list", repo: "jkb" });
  assert.deepEqual(designOps.state("d"), { op: "design.state", uid: "d" });
  assert.deepEqual(designOps.state("d", "AA=="), { op: "design.state", uid: "d", since: "AA==" });
  assert.deepEqual(designOps.discuss("d", "1.AQ", 3, 8), {
    op: "design.prompt",
    ask: { kind: "discuss", uid: "d", base: "1.AQ", start: 3, end: 8 },
  });
});

test("a design's repo is the segment after designs/", () => {
  assert.equal(repoOf("designs/jkb"), "jkb");
  assert.equal(repoOf("designs/jkb/sub"), "jkb");
  assert.equal(repoOf("designs"), undefined);
  assert.equal(repoOf("tasks/jkb"), undefined);
  assert.equal(repoOf(null), undefined);
  const d = (namespace) => ({ uid: "u", title: "t", namespace, seq: 0, topic: "design/u" });
  assert.deepEqual(designRepos([d("designs/b"), d("designs/a"), d("designs/b"), d(null)]), ["a", "b"]);
});

test("only a design topic is one the app subscribes to, and an announcement is parsed strictly", () => {
  assert.equal(isDesignTopic("design/design.code-factory-18dc"), true);
  for (const t of ["claude/notify", "design/", "design/a/b", "design/a b", 7, "design/" + "x".repeat(300)]) {
    assert.equal(isDesignTopic(t), false, String(t));
  }
  assert.deepEqual(parseAnnouncement({ design: "d", seq: 3, update: "AA==" }), { design: "d", seq: 3, update: "AA==" });
  assert.deepEqual(parseAnnouncement({ design: "d", seq: 3, update: null }), { design: "d", seq: 3, update: null });
  assert.equal(parseAnnouncement({ design: "d", seq: "3" }), undefined);
  assert.equal(parseAnnouncement({ design: "d", seq: 3, update: 5 }), undefined);
  assert.equal(parseAnnouncement(null), undefined);
});

test("text no span covers reads PROPOSED, and runs cover the text once, in order", () => {
  assert.deepEqual(stateRuns(10, []), { runs: [{ from: 0, to: 10, state: "PROPOSED" }], removed: [] });
  assert.deepEqual(stateRuns(0, []), { runs: [], removed: [] });
  const { runs } = stateRuns(20, [
    span({ uid: "s2", start: 12, end: 15, state: "STAGED" }),
    span({ uid: "s1", start: 2, end: 6, state: "APPROVED" }),
    span({ uid: "gone", anchored: false, start: 0, end: 20, state: "IMPLEMENTED" }),
  ]);
  assert.deepEqual(runs, [
    { from: 0, to: 2, state: "PROPOSED" },
    { from: 2, to: 6, state: "APPROVED", span: "s1" },
    { from: 6, to: 12, state: "PROPOSED" },
    { from: 12, to: 15, state: "STAGED", span: "s2" },
    { from: 15, to: 20, state: "PROPOSED" },
  ]);
});

test("a demoted span is drawn PROPOSED throughout, with removed words as zero-width marks", () => {
  // The engine's shape: every piece of a demoted span is PROPOSED; `added`/`removed` say which
  // words changed since the approval.
  const pieces = [
    { start: 4, end: 8, state: "PROPOSED", removed: false, added: false, text: "keep" },
    { start: 8, end: 10, state: "PROPOSED", removed: false, added: true, text: "ne" },
    { start: 10, end: 10, state: "PROPOSED", removed: true, added: false, text: "old " },
    { start: 10, end: 14, state: "PROPOSED", removed: false, added: false, text: "rest" },
  ];
  const drawn = {
    runs: [
      { from: 0, to: 4, state: "PROPOSED" },
      { from: 4, to: 8, state: "PROPOSED", span: "s" },
      { from: 8, to: 10, state: "PROPOSED", span: "s" },
      { from: 10, to: 14, state: "PROPOSED", span: "s" },
    ],
    removed: [{ at: 10, text: "old ", span: "s" }],
  };
  const demoted = (p) => span({ uid: "s", start: 4, end: 14, state: "PROPOSED", demoted: true, pieces: p });
  assert.deepEqual(stateRuns(14, [demoted(pieces)]), drawn);
  // …and so it is drawn even from an answer whose pieces still carry their old approval.
  const stale = pieces.map((p) => (p.added || p.removed ? p : { ...p, state: "APPROVED" }));
  assert.deepEqual(stateRuns(14, [demoted(stale)]), drawn);
});

test("ranges past the text are clipped and overlaps are drawn once", () => {
  const { runs } = stateRuns(10, [
    span({ uid: "a", start: 2, end: 7, state: "APPROVED" }),
    span({ uid: "b", start: 5, end: 40, state: "STAGED" }),
  ]);
  assert.deepEqual(runs, [
    { from: 0, to: 2, state: "PROPOSED" },
    { from: 2, to: 7, state: "APPROVED", span: "a" },
    { from: 7, to: 10, state: "STAGED", span: "b" },
  ]);
});
