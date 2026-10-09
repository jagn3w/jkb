//! The wire constants `@jkb/core` copies from Rust, read back out of the Rust sources they name, so
//! a change on either side fails here rather than as a misread reply at run time. Runs against the
//! emitted `dist/`, so `pnpm run build` precedes it.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import test from "node:test";

import { DEFAULT_DAEMON_ADDR, MAX_BODY_BYTES, PROTOCOL_VERSION, WIRE_ERROR_CODES } from "../dist/index.js";

const crates = path.join(import.meta.dirname, "..", "..", "..", "crates");
const daemonLib = fs.readFileSync(path.join(crates, "jkb-daemon", "src", "lib.rs"), "utf8");
const apiLib = fs.readFileSync(path.join(crates, "jkb-api", "src", "lib.rs"), "utf8");

/** The right-hand side of `pub const <name>: <type> = <value>;`. */
function rustConst(source, name) {
  const m = new RegExp(`pub const ${name}: [^=]+= ([^;]+);`).exec(source);
  assert.ok(m, `pub const ${name} not found in the Rust source`);
  return m[1].trim();
}

/** An integer expression of literals and `*`, as `1024 * 1024`. */
function product(expr) {
  assert.match(expr, /^[\d_ *]+$/, `${expr} is not a product of literals`);
  return expr.split("*").reduce((n, f) => n * Number(f.trim().replaceAll("_", "")), 1);
}

test("the daemon's address, protocol and body limit are jkb_daemon's", () => {
  assert.equal(JSON.parse(rustConst(daemonLib, "DEFAULT_ADDR")), DEFAULT_DAEMON_ADDR);
  assert.equal(product(rustConst(daemonLib, "PROTOCOL_VERSION")), PROTOCOL_VERSION);
  assert.equal(product(rustConst(daemonLib, "MAX_BODY_BYTES")), MAX_BODY_BYTES);
});

test("the wire error codes are jkb_api::ErrorCode's variants, snake_case, in order", () => {
  assert.match(apiLib, /#\[serde\(rename_all = "snake_case"\)\]\s*#\[non_exhaustive\]\s*pub enum ErrorCode/);
  const m = /pub enum ErrorCode \{([\s\S]*?)\n\}/.exec(apiLib);
  assert.ok(m, "pub enum ErrorCode not found in jkb-api");
  const variants = m[1]
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => {
      // A per-variant rename would make the snake_case derivation below wrong: say so instead.
      if (line.startsWith("#")) assert.equal(line, "#[serde(other)]", `unexpected attribute in ErrorCode: ${line}`);
      return !line.startsWith("//") && !line.startsWith("#") && line !== "";
    })
    .map((line) => {
      const v = /^([A-Z][A-Za-z0-9]*),?$/.exec(line);
      assert.ok(v, `unexpected line in ErrorCode: ${line}`);
      return v[1].replace(/(?<!^)([A-Z])/g, "_$1").toLowerCase();
    });
  assert.deepEqual([...WIRE_ERROR_CODES], variants);
});
