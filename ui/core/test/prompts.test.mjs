//! A design's prompts as `@jkb/core` reads them (D53.6): the answers' shapes, the requests, and the
//! title a *New prompt* is recorded under. Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import { decodeDesignPrompts, decodeNewPrompt, newPromptTitle, parsePromptAnnouncement, promptOps } from "../dist/index.js";

const ok = (value) => ({ ok: true, value });

const record = (over = {}) => ({
  uid: "prompt:0f8fad5b-d9cb-469f-a165-70867728950e",
  design: "design:d",
  session: "0f8fad5b-d9cb-469f-a165-70867728950e",
  cwd: "/home/vscode/repos/jkb",
  launch: "discuss",
  subject: null,
  title: "Discuss · Factory",
  created_at: "2026-10-08T10:00:00.000Z",
  ...over,
});

test("design.prompts decodes the records the wire carries, a missing subject as null", () => {
  const { subject: _, ...noSubject } = record({ launch: "new" });
  const answer = decodeDesignPrompts(
    ok({ result: "design_prompts", uid: "design:d", prompts: [record({ launch: "task", subject: "task:a" }), noSubject] }),
  );
  assert.equal(answer.ok, true);
  assert.deepEqual(answer.value, [record({ launch: "task", subject: "task:a" }), record({ launch: "new" })]);
});

test("a malformed listing is refused, not half-read", () => {
  for (const bad of [
    [record({ launch: "resume" })],
    [record({ cwd: "repos/jkb" })],
    [record({ session: 7 })],
    [record({ created_at: undefined })],
    "nope",
  ]) {
    const answer = decodeDesignPrompts(ok({ result: "design_prompts", uid: "design:d", prompts: bad }));
    assert.equal(answer.ok, false, JSON.stringify(bad));
    assert.equal(answer.error.code, "internal");
  }
  assert.equal(decodeDesignPrompts(ok({ result: "design_plans", prompts: [] })).ok, false);
  const refused = { ok: false, error: { code: "not_found", message: "no design" } };
  assert.deepEqual(decodeDesignPrompts(refused), refused);
});

test("a New prompt answer is decoded; an empty prompt is refused", () => {
  const prompt = { kind: "new", uid: "design:d", title: "Factory", prompt: "Read it." };
  assert.deepEqual(decodeNewPrompt(ok({ result: "design_new_prompt", prompt })), ok(prompt));
  assert.equal(decodeNewPrompt(ok({ result: "design_new_prompt", prompt: { ...prompt, prompt: "" } })).ok, false);
  assert.equal(decodeNewPrompt(ok({ result: "design_work_prompt", prompt })).ok, false);
});

test("the requests are the ops the Rust side serves", () => {
  assert.deepEqual(promptOps.list("design:d"), { op: "design.prompts", uid: "design:d" });
  assert.deepEqual(promptOps.newPrompt("design:d", "Do it"), {
    op: "design.prompt",
    ask: { kind: "new", uid: "design:d", text: "Do it" },
  });
});

test("a prompt message on a design topic is read; anything else is not one", () => {
  assert.deepEqual(parsePromptAnnouncement({ design: "design:d", prompt: "prompt:x" }), { design: "design:d", prompt: "prompt:x" });
  assert.equal(parsePromptAnnouncement({ design: "design:d", seq: 3, update: null }), undefined);
  assert.equal(parsePromptAnnouncement(null), undefined);
});

test("a New prompt is titled by the first line the operator wrote", () => {
  assert.equal(newPromptTitle("\n  Tighten the intro  \nand more", "Factory"), "Tighten the intro");
  assert.equal(newPromptTitle("   \n", "Factory"), "Factory");
});
