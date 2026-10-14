//! The sidebar's tree and the unreviewed passages *See next* walks. Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import { designTree, passageAt, stepPassage, subtreeReview, treeOrder, unreviewedPassages } from "../dist/index.js";

const design = (uid, title, repo = "jkb", parent) => ({
  uid,
  title,
  namespace: repo === null ? null : `designs/${repo}`,
  seq: 1,
  topic: `design/${uid}`,
  ...(parent === undefined ? {} : { parent }),
});

const span = (start, end, state = "APPROVED", over = {}) => ({
  uid: `span:${start}`,
  reviewer: "operator",
  anchored: true,
  start,
  end,
  text: "",
  state,
  demoted: false,
  pieces: [],
  approved_by: "operator",
  approved_at: null,
  steps: [],
  ...over,
});

test("the tree groups designs by repo, sorted, and nests a child under its parent", () => {
  const tree = designTree([
    design("d:b", "Beta"),
    design("d:a", "Alpha"),
    design("d:c", "Child", "jkb", "d:b"),
    design("d:x", "Other", "zed"),
    design("d:n", "No repo", null),
  ]);
  assert.deepEqual(
    tree.map((r) => r.repo),
    ["jkb", "zed"],
  );
  const jkb = tree[0].designs;
  assert.deepEqual(
    jkb.map((n) => n.design.uid),
    ["d:a", "d:b"],
  );
  assert.deepEqual(
    jkb[1].children.map((n) => n.design.uid),
    ["d:c"],
  );
  assert.deepEqual(
    treeOrder(tree).map((d) => d.uid),
    ["d:a", "d:b", "d:c", "d:x"],
  );
});

test("a design whose parent is missing or in another repo is shown at the top, never lost", () => {
  const tree = designTree([design("d:a", "A", "jkb", "d:gone"), design("d:b", "B", "zed", "d:a")]);
  assert.deepEqual(
    treeOrder(tree).map((d) => d.uid),
    ["d:a", "d:b"],
  );
});

test("a parent cycle is broken: every design stays reachable, once", () => {
  const tree = designTree([design("d:a", "A", "jkb", "d:b"), design("d:b", "B", "jkb", "d:a"), design("d:s", "Self", "jkb", "d:s")]);
  const order = treeOrder(tree).map((d) => d.uid);
  assert.deepEqual([...order].sort(), ["d:a", "d:b", "d:s"]);
});

test("unreviewed passages are the PROPOSED text, trimmed, with blank gaps dropped", () => {
  //            0         1         2         3
  //            0123456789012345678901234567890123456
  const text = "# Title\n\nApproved one.\n\nDraft words.\n\n";
  const a = text.indexOf("Approved");
  const spans = [span(0, 7), span(a, a + "Approved one.".length)];
  const passages = unreviewedPassages(text, spans);
  const d = text.indexOf("Draft");
  assert.deepEqual(passages, [{ from: d, to: d + "Draft words.".length }]);
});

test("a demoted span reads unreviewed, and joins the uncovered text beside it", () => {
  const text = "Loose.\n\nChanged.\n\nKept.";
  const c = text.indexOf("Changed");
  const k = text.indexOf("Kept");
  const spans = [span(c, c + 8, "APPROVED", { demoted: true }), span(k, k + 5)];
  assert.deepEqual(unreviewedPassages(text, spans), [{ from: 0, to: c + 8 }]);
});

test("a fully approved document has no passages", () => {
  const text = "All of it.";
  assert.deepEqual(unreviewedPassages(text, [span(0, text.length, "IMPLEMENTED")]), []);
});

test("stepping moves to the next or previous passage, and stops at the ends", () => {
  const ps = [
    { from: 10, to: 20 },
    { from: 30, to: 40 },
    { from: 50, to: 60 },
  ];
  assert.equal(stepPassage(ps, 0, 1), 0);
  assert.equal(stepPassage(ps, 10, 1), 1);
  assert.equal(stepPassage(ps, 15, 1), 1);
  assert.equal(stepPassage(ps, 50, 1), undefined);
  assert.equal(stepPassage(ps, 50, -1), 1);
  assert.equal(stepPassage(ps, 10, -1), undefined);
  assert.equal(stepPassage(ps, 99, -1), 2);
  assert.equal(passageAt(ps, 35), 1);
  assert.equal(passageAt(ps, 25), undefined);
});

test("a design with unknown counts, or any PROPOSED text, has something to review", async () => {
  const { hasUnreviewed, designOps, decodeDesigns } = await import("../dist/index.js");
  const d = (review) => ({ uid: "d", title: "D", namespace: "designs/jkb", seq: 1, topic: "design/d", ...(review ? { review } : {}) });
  assert.equal(hasUnreviewed(d()), true);
  assert.equal(hasUnreviewed(d({ proposed: 3, approved: 0, staged: 0, implemented: 9 })), true);
  assert.equal(hasUnreviewed(d({ proposed: 0, approved: 2, staged: 0, implemented: 9 })), false);
  assert.deepEqual(designOps.list(undefined, true), { op: "design.list", review: true });
  const bad = decodeDesigns({ ok: true, value: { result: "designs", designs: [d({ proposed: "x" })] } });
  assert.equal(bad.ok, false, "malformed counts are refused");
});

test("a row's counts are its design's own plus every descendant's; unknown counts add nothing", () => {
  const counts = (proposed, approved, staged, implemented) => ({ proposed, approved, staged, implemented });
  const withReview = (d, review) => (review === undefined ? d : { ...d, review });
  const [jkb] = designTree([
    withReview(design("d:p", "Parent"), counts(1, 2, 0, 0)),
    withReview(design("d:c", "Child", "jkb", "d:p"), counts(3, 0, 1, 0)),
    withReview(design("d:g", "Grandchild", "jkb", "d:c"), counts(0, 0, 0, 5)),
    design("d:u", "Unknown", "jkb", "d:p"),
    design("d:lone", "Lone unknown"),
    withReview(design("d:under", "Under unknown", "jkb", "d:lone"), counts(2, 0, 0, 0)),
    design("d:none", "Nothing known"),
  ]).map((r) => r.designs);
  const at = (uid) => jkb.find((n) => n.design.uid === uid);
  assert.deepEqual(subtreeReview(at("d:p")), counts(4, 2, 1, 5));
  // The child alone: its own and its grandchild's, not its parent's.
  assert.deepEqual(subtreeReview(at("d:p").children.find((n) => n.design.uid === "d:c")), counts(3, 0, 1, 5));
  // A design with unknown counts still shows what is known under it.
  assert.deepEqual(subtreeReview(at("d:lone")), counts(2, 0, 0, 0));
  assert.equal(subtreeReview(at("d:none")), undefined);
});
