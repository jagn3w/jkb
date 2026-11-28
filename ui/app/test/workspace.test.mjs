//! The `ui/` workspace links Linux's platform packages beside the installing machine's, so the dev
//! container can run these tests against the host's `ui/node_modules` (see `supportedArchitectures`
//! in pnpm-workspace.yaml). A macOS-only install left the container no esbuild binary, and every test
//! here that bundles with esbuild failed there.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import test from "node:test";

const workspace = fs.readFileSync(path.join(import.meta.dirname, "..", "..", "pnpm-workspace.yaml"), "utf8");

// The block's list under `key`, read without a YAML dependency: the lines `    - <value>` that follow
// `  <key>:` inside `supportedArchitectures:`, up to the next key or the block's end.
function supported(key) {
  const block = workspace.match(/^supportedArchitectures:\n((?:[ \t]+.*\n|[ \t]*#.*\n)*)/m);
  assert.ok(block, "pnpm-workspace.yaml has a top-level supportedArchitectures block");
  const list = block[1].match(new RegExp(`^  ${key}:\\n((?:    - .*\\n)*)`, "m"));
  assert.ok(list, `supportedArchitectures.${key} is set`);
  return list[1].split("\n").filter(Boolean).map((line) => line.replace(/^ {4}- /, "").trim());
}

test("an install links Linux's platform packages beside this machine's", () => {
  assert.deepEqual(supported("os"), ["current", "linux"]);
});

test("the CPU is the installing machine's own, which the container shares", () => {
  assert.deepEqual(supported("cpu"), ["current"]);
});
