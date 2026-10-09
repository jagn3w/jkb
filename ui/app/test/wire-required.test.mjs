//! The yjs-wire measurement's skip, where the measurement is required (D53.4). yjs-wire.test.mjs
//! skips when it has no jkb to drive; scripts/check.sh and CI's `check` job set JKB_REQUIRE_WIRE=1 so
//! a skip there is a failure, never a green run that measured nothing. This runs the wire file with
//! JKB_BIN pointing nowhere, with and without the flag. Needs no jkb.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

const wire = path.join(import.meta.dirname, "yjs-wire.test.mjs");
const empty = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-no-jkb-"));
after(() => fs.rmSync(empty, { recursive: true, force: true }));
const missing = path.join(empty, "jkb");

/** Run the wire test with no jkb at JKB_BIN. */
function runWire(extra) {
  const env = { ...process.env, JKB_BIN: missing, ...extra };
  // The parent runner's context would make the child report to it rather than exit with a status.
  delete env.NODE_TEST_CONTEXT;
  if (extra.JKB_REQUIRE_WIRE === undefined) delete env.JKB_REQUIRE_WIRE;
  return spawnSync(process.execPath, ["--test", wire], { env, encoding: "utf8", timeout: 60_000 });
}

test("with JKB_REQUIRE_WIRE=1, a wire test with no jkb fails, saying why", () => {
  const r = runWire({ JKB_REQUIRE_WIRE: "1" });
  assert.notEqual(r.status, 0, `the wire test passed with nothing measured:\n${r.stdout}`);
  assert.ok(r.stdout.includes(`JKB_REQUIRE_WIRE is set, but no jkb at ${missing}`), r.stdout);
});

test("without it, the same wire test skips and passes, saying why", () => {
  const r = runWire({});
  assert.equal(r.status, 0, `exit ${r.status}:\n${r.stdout}\n${r.stderr}`);
  assert.ok(r.stdout.includes(`yjs-wire skipped: no jkb at ${missing}`), r.stdout);
});
