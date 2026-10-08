//! The editor's `yjs` against jkb's `yrs`, byte for byte (D53.4): the measurement subtask 3 left
//! open. A real `jkb` writes and reads a design in a scratch database; the test's Yjs document plays
//! the app's editor. Covered: loading jkb's state, an editor update jkb merges (after a character
//! that is two UTF-16 units), a CLI edit the editor merges from `design.state --since` its own state
//! vector, and a span's anchors read in Yjs at the offsets jkb reports.
//
// Needs a `jkb` built from this tree: set JKB_BIN to it (`./scripts/build.sh` puts it in cargo's
// target directory). Without it the test SKIPS and says so; it is never reported green.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

const require = createRequire(import.meta.url);
const Y = require("yjs");

const bin = process.env.JKB_BIN?.trim();
const skip =
  bin === undefined || bin === ""
    ? "JKB_BIN is not set to a jkb built from this tree"
    : !fs.existsSync(bin)
      ? `no jkb at ${bin}`
      : undefined;
if (skip !== undefined) console.log(`# yjs-wire skipped: ${skip}`);

const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-wire-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const db = path.join(work, "jkb.db");

/**
 * The scratch database's own environment, as the CLI's tests isolate theirs: nothing that would send
 * the command to a daemon (`--db` is refused with one named), and no container to reach.
 */
const env = { ...process.env, JKB_CONTAINER_NAME: "jkb-test-no-such-container" };
for (const key of ["JKB_REMOTE", "JKB_REMOTE_TOKEN_FILE", "JKB_AGENT_TOKEN", "JKB_ATTEST", "JKB_DB"]) delete env[key];

function jkb(...args) {
  const out = execFileSync(bin, ["--db", db, "--json", ...args], { encoding: "utf8", env, cwd: work });
  return JSON.parse(out);
}

const b64 = (bytes) => Buffer.from(bytes).toString("base64");
const unb64 = (text) => new Uint8Array(Buffer.from(text, "base64"));

test("an editor's yjs and jkb's yrs exchange updates, UTF-16 offsets and span anchors", { skip }, () => {
  const design = jkb("design", "create", "Wire", "--repo", "jkb", "--body", "héllo 🦀 world");
  const uid = design.uid;

  // The editor loads jkb's state.
  const editor = new Y.Doc({ gc: false });
  const body = editor.getText("body");
  Y.applyUpdate(editor, unb64(jkb("design", "state", uid).update));
  assert.equal(body.toString(), "héllo 🦀 world");

  // An edit after the crab, at a UTF-16 offset, merged by jkb.
  const sv = Y.encodeStateVector(editor);
  body.insert("héllo 🦀".length, ",");
  const written = jkb("design", "apply", uid, b64(Y.encodeStateAsUpdate(editor, sv)));
  assert.ok(written.seq > 0);
  assert.equal(jkb("design", "cat", uid).text, "héllo 🦀, world");

  // Claude edits through the CLI; the editor asks for exactly what it lacks.
  const read = jkb("design", "cat", uid);
  jkb("design", "edit", uid, "--base", read.version, "--insert-after", "world", "--text", "!");
  const lacking = jkb("design", "state", uid, "--since", b64(Y.encodeStateVector(editor)));
  Y.applyUpdate(editor, unb64(lacking.update));
  assert.equal(body.toString(), "héllo 🦀, world!");

  // A span made by jkb resolves, in Yjs, to the range jkb reports.
  const now = jkb("design", "cat", uid);
  const made = jkb("design", "span", uid, "--base", now.version, "--find", "🦀, world");
  Y.applyUpdate(editor, unb64(jkb("design", "state", uid, "--since", b64(Y.encodeStateVector(editor))).update));
  const entry = editor.getMap("spans").get(made.span);
  assert.ok(entry !== undefined, "the span's anchors are in the document's spans map");
  const at = (bytes) => Y.createAbsolutePositionFromRelativePosition(Y.decodeRelativePosition(bytes), editor).index;
  const reported = jkb("design", "spans", uid).find((s) => s.uid === made.span);
  assert.deepEqual([at(entry.start), at(entry.end)], [reported.start, reported.end]);
  assert.equal(body.toString().slice(reported.start, reported.end), "🦀, world");

  // The editor types inside the span; jkb reads the words inside it too.
  const sv2 = Y.encodeStateVector(editor);
  body.insert(body.toString().indexOf("world"), "big ");
  jkb("design", "apply", uid, b64(Y.encodeStateAsUpdate(editor, sv2)));
  const grown = jkb("design", "spans", uid).find((s) => s.uid === made.span);
  assert.equal(grown.text, "🦀, big world", "an insertion inside the span is inside it in jkb too");
  assert.equal(jkb("design", "cat", uid).text, body.toString());
});
