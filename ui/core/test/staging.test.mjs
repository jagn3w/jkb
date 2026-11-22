//! The In Flight view's staging rows as `@jkb/core` formats them (D38.8). Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import { formatTaskSummary } from "../dist/index.js";

/** A staged task as `jkb staging ls --json` writes it, landable and with nothing to say. */
const staged = (extra = {}) => ({
  uid: "task:t",
  title: "T",
  status: "in_progress",
  state: "review",
  branch: "task/t",
  worktree: null,
  dirty: "no",
  commits: 2,
  reviewed: "abc123",
  review_nss: ["repos/proj/codereviews/r1"],
  review_waived: null,
  open_must_fix: 0,
  review_ok: true,
  land_blocked: null,
  ...extra,
});

test("formatTaskSummary: a land note is said last, and a row without one says nothing of it", () => {
  const note = "1 other unplanned task(s) in proj";
  assert.equal(
    formatTaskSummary(staged({ land_note: note })),
    `review · 2 commits · reviewed · ${note}`,
  );
  // Beside a hold, the note is still only a note: the hold is said first.
  assert.equal(
    formatTaskSummary(staged({ land_note: note, open_must_fix: 1, review_waived: "operator" })),
    `review · 2 commits · 1 must-fix open · review waived · ${note}`,
  );
  // Absent (a CLI that predates it) and null both say nothing.
  assert.equal(formatTaskSummary(staged()), "review · 2 commits · reviewed");
  assert.equal(formatTaskSummary(staged({ land_note: null })), "review · 2 commits · reviewed");
});
