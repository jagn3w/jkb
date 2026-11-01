//! Run requests in `@jkb/core`: the output frame codec, the renderer-refused ops, the exit-code map
//! and how an argument is shown — and the constants copied from Rust, read back out of the Rust
//! sources. Runs against the emitted `dist/`, so `pnpm run build` precedes it.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import test from "node:test";

import {
  EXEC_DEFAULT_TIMEOUT_MS,
  EXEC_EXIT,
  EXEC_MAX_CHUNK_BYTES,
  EXEC_MAX_TAIL_BYTES,
  EXEC_MAX_TIMEOUT_MS,
  EXEC_RUNNER_OPS,
  OutputText,
  base64ToBytes,
  bytesToBase64,
  decodeExecView,
  decodeOutputFrame,
  encodeOutputChunks,
  EXEC_INDEX_TOPIC,
  NOT_IN_A_REPOSITORY,
  TREE_CHANGED,
  escapeArg,
  escapeSegments,
  execExitCode,
  outcomeText,
  parseBaseOutput,
  parseDiffOutput,
  previewVerdict,
  requestView,
  showArg,
  execId,
  parseRunnerName,
  rendererOpRefusal,
  runnerName,
} from "../dist/index.js";

const crates = path.join(import.meta.dirname, "..", "..", "..", "crates");
const coreExec = fs.readFileSync(path.join(crates, "jkb-core", "src", "exec.rs"), "utf8");
const cliExec = fs.readFileSync(path.join(crates, "jkb-cli", "src", "exec_cli.rs"), "utf8");
const apiLib = fs.readFileSync(path.join(crates, "jkb-api", "src", "lib.rs"), "utf8");

/** The value of `pub const <name>: <type> = <product of literals>;` in `source`. */
function rustProduct(source, name) {
  const m = new RegExp(`pub const ${name}: [^=]+= ([^;]+);`).exec(source);
  assert.ok(m, `pub const ${name} not found`);
  assert.match(m[1], /^[\d_ *]+$/, `${m[1]} is not a product of literals`);
  return m[1].split("*").reduce((n, f) => n * Number(f.trim().replaceAll("_", "")), 1);
}

/** A deterministic byte source, so a failure reproduces. */
function bytes(n, seed) {
  const out = new Uint8Array(n);
  let x = seed >>> 0 || 1;
  for (let i = 0; i < n; i++) {
    x ^= x << 13;
    x ^= x >>> 17;
    x ^= x << 5;
    out[i] = x & 255;
  }
  return out;
}

test("the constants are jkb_core::exec's and jkb-cli's", () => {
  assert.equal(rustProduct(coreExec, "MAX_CHUNK_BYTES"), EXEC_MAX_CHUNK_BYTES);
  assert.equal(rustProduct(coreExec, "MAX_TAIL_BYTES"), EXEC_MAX_TAIL_BYTES);
  assert.equal(rustProduct(coreExec, "DEFAULT_TIMEOUT_MS"), EXEC_DEFAULT_TIMEOUT_MS);
  assert.equal(rustProduct(coreExec, "MAX_TIMEOUT_MS"), EXEC_MAX_TIMEOUT_MS);
  assert.equal(rustProduct(cliExec, "EXIT_DECLINED"), EXEC_EXIT.declined);
  assert.equal(rustProduct(cliExec, "EXIT_REFUSED"), EXEC_EXIT.refused);
  assert.equal(rustProduct(cliExec, "EXIT_EXPIRED"), EXEC_EXIT.expired);
  assert.equal(rustProduct(cliExec, "EXIT_TIMED_OUT"), EXEC_EXIT.timedOut);
});

test("the renderer-refused ops are real ops, and only the runner's moves are refused", () => {
  for (const op of EXEC_RUNNER_OPS) {
    assert.ok(apiLib.includes(`#[serde(rename = "${op}")]`), `${op} is not a jkb_api op`);
    assert.match(rendererOpRefusal(op), /runner/);
  }
  for (const op of ["exec.status", "exec.list", "exec.decline", "exec.follow", "exec.request", "task.show"]) {
    assert.equal(rendererOpRefusal(op), undefined, op);
  }
});

test("base64 is the standard alphabet with padding, as Rust's STANDARD engine", () => {
  for (let n = 0; n < 70; n++) {
    const b = bytes(n, n + 1);
    const ours = bytesToBase64(b);
    assert.equal(ours, Buffer.from(b).toString("base64"), `length ${n}`);
    assert.deepEqual(base64ToBytes(ours), b, `length ${n}`);
  }
  for (const bad of ["a", "ab=c", "a b=", "@@@@", "abc=="]) assert.equal(base64ToBytes(bad), undefined, bad);
});

test("the frame codec round-trips arbitrary bytes at any chunk size", () => {
  for (const [n, max] of [[0, 4], [1, 4], [100_000, EXEC_MAX_CHUNK_BYTES], [1000, 7], [EXEC_MAX_CHUNK_BYTES * 2, EXEC_MAX_CHUNK_BYTES]]) {
    const all = bytes(n, n + max);
    const chunks = encodeOutputChunks(all, max);
    const back = chunks.map((b64, i) => decodeOutputFrame({ n: i, b64 }));
    assert.ok(back.every((f, i) => f !== undefined && f.n === i && f.bytes.length <= max));
    assert.deepEqual(Buffer.concat(back.map((f) => f.bytes)), Buffer.from(all), `${n} bytes in ${max}-byte chunks`);
  }
  assert.equal(decodeOutputFrame({ n: 1.5, b64: "" }), undefined);
  assert.equal(decodeOutputFrame({ n: 0, b64: "%%%%" }), undefined);
  assert.equal(decodeOutputFrame(null), undefined);
});

test("text split inside a UTF-8 sequence decodes whole, at every cut", () => {
  const text = "build ✓ — 完成 🦀 done\r\n";
  const encoded = new TextEncoder().encode(text);
  for (let max = 1; max <= encoded.length; max++) {
    const out = new OutputText();
    let got = "";
    for (const b64 of encodeOutputChunks(encoded, max)) got += out.push(decodeOutputFrame({ n: 0, b64 }).bytes);
    got += out.end();
    assert.equal(got, text, `cut every ${max} bytes`);
  }
  const dangling = new OutputText();
  assert.equal(dangling.push(new Uint8Array([0xe2, 0x9c])), "", "half a sequence is held");
  assert.equal(dangling.end(), "\ufffd", "and a dangling half is said at the end");
});

test("an argument shows every control and invisible character escaped, and keeps printable text", () => {
  assert.equal(escapeArg("plain ./scripts/check.sh"), "plain ./scripts/check.sh");
  assert.equal(escapeArg("完成 🦀 é"), "完成 🦀 é");
  assert.equal(escapeArg("a\\b"), "a\\\\b");
  assert.equal(escapeArg("a\nb\tc\rd"), "a\\nb\\tc\\rd");
  assert.equal(escapeArg("\x1b[31mred"), "\\u{001b}[31mred");
  assert.equal(escapeArg("\x7f\x85"), "\\u{007f}\\u{0085}");
  assert.equal(escapeArg("rm\u202e-rf"), "rm\\u{202e}-rf", "a bidi override");
  assert.equal(escapeArg("a\u200bb"), "a\\u{200b}b", "a zero-width space");
  assert.equal(escapeArg("a\u00a0b"), "a\\u{00a0}b", "a space that is not U+0020");
  assert.equal(escapeArg("\ufeffx"), "\\u{feff}x");
  assert.equal(escapeArg("t\u{e0041}"), "t\\u{e0041}", "a tag character");
  assert.equal(escapeArg("\ud800"), "\\u{d800}", "a lone surrogate");
});

test("the exit status is jkb exec request's", () => {
  assert.equal(execExitCode({ state: "finished", outcome: "exited", exit_status: 3 }), 3);
  assert.equal(execExitCode({ state: "finished", outcome: "signalled", exit_status: 15 }), 143);
  assert.equal(execExitCode({ state: "finished", outcome: "timed_out" }), 124);
  assert.equal(execExitCode({ state: "finished", outcome: "interrupted" }), 124);
  assert.equal(execExitCode({ state: "declined" }), 77);
  assert.equal(execExitCode({ state: "refused" }), 78);
  assert.equal(execExitCode({ state: "expired" }), 79);
  assert.equal(execExitCode({ state: "pending" }), undefined);
  assert.equal(execExitCode({ state: "running" }), undefined);
});

test("ids and runner names", () => {
  assert.equal(execId("exec-12"), "exec-12");
  for (const bad of ["12", "exec-0", "exec--1", "exec-1 ", 12, null]) assert.equal(execId(bad), undefined, String(bad));
  const name = runnerName(4242, "3f1c-boot");
  assert.equal(name, "code-factory:4242@3f1c-boot");
  assert.deepEqual(parseRunnerName(name), { pid: 4242, boot: "3f1c-boot" });
  assert.equal(parseRunnerName("cli"), undefined);
  assert.equal(parseRunnerName("code-factory:0@b"), undefined);
});

test("a request decodes, and a malformed one says why", () => {
  const view = {
    id: "exec-3", topic: "exec/3", state: "pending", target: "host", dir: "/r", dir_side: "host",
    argv: ["./scripts/check.sh"], reason: "", session: "s", role: "implementer", timeout_ms: 900000,
    created_at: 1, expires_at: 2, pin: { head: "a", changes: "b" }, extra_field_from_a_newer_daemon: 1,
  };
  assert.equal(decodeExecView(view).ok, true);
  assert.equal(decodeExecView({ ...view, argv: [] }).ok, false);
  assert.equal(decodeExecView({ ...view, id: "3" }).ok, false);
  assert.equal(decodeExecView({ ...view, pin: { head: 1 } }).ok, false);
});

test("the generic op gate never sends a runner's op, and sends everything else", async () => {
  const { rendererOp } = await import("../dist/index.js");
  const sent = [];
  const send = async (r) => {
    sent.push(r.op);
    return { ok: true, value: { result: "ok" } };
  };
  const refused = await rendererOp({ op: "exec.begin", id: "exec-1", runner: "x" }, send);
  assert.equal(refused.ok, false);
  assert.equal(refused.error.code, "forbidden");
  assert.equal((await rendererOp({ op: "exec.status", id: "exec-1" }, send)).ok, true);
  assert.deepEqual(sent, ["exec.status"]);
});

test("an argument escapes one code point of every hidden category, by Unicode property", () => {
  for (const [ch, shown, category] of [
    ["\u{001b}", "\\u{001b}", "Cc"],
    ["\u{0085}", "\\u{0085}", "Cc (C1)"],
    ["\u{0600}", "\\u{0600}", "Cf"],
    ["\u{1d173}", "\\u{1d173}", "Cf, default-ignorable"],
    ["\u{200b}", "\\u{200b}", "Cf"],
    ["\u{e000}", "\\u{e000}", "Co"],
    ["\u{0378}", "\\u{0378}", "Cn"],
    ["\u{2028}", "\\u{2028}", "Zl"],
    ["\u{2029}", "\\u{2029}", "Zp"],
    ["\u{034f}", "\\u{034f}", "default-ignorable (Mn)"],
    ["\u{180b}", "\\u{180b}", "default-ignorable (Mn)"],
    ["\u{ffa0}", "\\u{ffa0}", "default-ignorable (Lo)"],
    ["\u{00a0}", "\\u{00a0}", "Zs"],
    ["\u{3000}", "\\u{3000}", "Zs"],
    ["\u{1bca0}", "\\u{1bca0}", "Cf"],
  ]) {
    assert.equal(escapeArg(`a${ch}b`), `a${shown}b`, category);
  }
  assert.equal(escapeArg("./scripts/check.sh a b"), "./scripts/check.sh a b", "U+0020 is a space like any other");
});

test("the index topic is jkb_core::exec::INDEX_TOPIC", () => {
  const m = /pub const INDEX_TOPIC: &str = "([^"]+)";/.exec(coreExec);
  assert.ok(m);
  assert.equal(m[1], EXEC_INDEX_TOPIC);
});

test("an argument in the queue: every hidden and every non-ASCII character spelt out, a trailing space shown", () => {
  const table = [
    ["rm‮-rf", "rm\\u{202e}-rf", "an RLO"],
    ["a‍b", "a\\u{200d}b", "a ZWJ"],
    ["a\tb", "a\\tb", "a tab"],
    ["\x1b[2J", "\\u{001b}[2J", "a clear-screen escape"],
    ["a\u0000b", "a\\u{0000}b", "a NUL"],
    ["make ", "make␠", "a trailing space"],
    ["x  ", "x␠␠", "two trailing spaces"],
    ["аdd", "\\u{0430}dd", "a Cyrillic а passing for a Latin a"],
    ["␠", "\\u{2420}", "a literal ␠ is not a trailing space"],
    ["", "∅", "an empty argument"],
    ["plain/path.sh", "plain/path.sh", "plain ASCII"],
  ];
  for (const [arg, text, what] of table) assert.equal(showArg(arg).text, text, what);
  // The glyph is shown alongside where the text spelt a character out.
  assert.deepEqual(showArg("\u0430dd"), { text: "\\u{0430}dd", glyphs: "\u0430dd" });
  assert.equal(showArg("make ").glyphs, undefined, "a trailing space alone needs no glyph");
  assert.equal(showArg("plain").glyphs, undefined);
});

test("text that is not an argument is escaped into marked segments, newlines kept", () => {
  assert.deepEqual(escapeSegments("a\u202eb\r\nc\td"), [
    { text: "a", escaped: false },
    { text: "\\u{202e}", escaped: true },
    { text: "b\nc\td", escaped: false },
  ]);
  assert.deepEqual(escapeSegments("x\ry"), [
    { text: "x", escaped: false },
    { text: "\\r", escaped: true },
    { text: "y", escaped: false },
  ]);
  // A literal escape spelt in the text is plain text, never marked.
  assert.deepEqual(escapeSegments("\\u{202e}"), [{ text: "\\u{202e}", escaped: false }]);
});

test("the request view model escapes every field a session chose", () => {
  const v = {
    id: "exec-3", topic: "exec/3", state: "pending", target: "host", dir: "/w/r\u202e", dir_side: "container",
    argv: ["sh", "-c", "echo \u0430 "], reason: "need \u001b[2Jdocker", session: "s\u200b1", role: "implementer",
    agent_type: "implementer", task: "task:x", timeout_ms: 90_000, created_at: 0, expires_at: 7_200_000,
    pin: { head: "a".repeat(40), changes: "b".repeat(64) },
  };
  const r = requestView(v, 3_600_000);
  assert.equal(r.session, "s\\u{200b}1");
  assert.equal(r.dir, "/w/r\\u{202e}");
  assert.deepEqual(r.argv.map((a) => a.text), ["sh", "-c", "echo \\u{0430}␠"]);
  assert.ok(r.reason.some((s) => s.escaped && s.text === "\\u{001b}"));
  assert.equal(r.timeout, "90s");
  assert.equal(r.expiresIn, "60m");
  assert.equal(requestView({ ...v, state: "declined" }, 0).expiresIn, undefined);
  assert.equal(outcomeText({ state: "finished", outcome: "exited", exit_status: 2 }), "exited 2");
  assert.equal(outcomeText({ state: "refused", refuse_reason: "tree_changed" }), "refused (tree_changed)");
  assert.equal(outcomeText({ state: "declined" }), "declined");
});

test("the preview's verdict: all pins the request's, one that differs, no repository", () => {
  const p = { head: "a".repeat(40), changes: "b".repeat(64) };
  const q = { head: "a".repeat(40), changes: "c".repeat(64) };
  assert.deepEqual(previewVerdict(p, [p, p, p]), { state: "match" });
  assert.deepEqual(previewVerdict(p, [p, q, p]), { state: "changed", message: TREE_CHANGED });
  assert.deepEqual(previewVerdict(p, [p, p, q]), { state: "changed", message: TREE_CHANGED }, "moved after the diff");
  assert.deepEqual(previewVerdict(p, [null]), { state: "changed", message: TREE_CHANGED }, "the repository went away");
  assert.deepEqual(previewVerdict(undefined, [null, undefined, null]), { state: "no_repo" });
  assert.deepEqual(previewVerdict(p, [p, undefined, p]), { state: "match" }, "a pin not taken is said elsewhere");
  assert.equal(NOT_IN_A_REPOSITORY, "Not in a repository: no tree pin, no diff.");
});

test("jkb exec base and jkb exec diff are read as answers or faults", () => {
  const sha = "d".repeat(40);
  assert.deepEqual(parseBaseOutput(0, JSON.stringify({ dir: "/r", base: { sha, ref: "refs/heads/main", source: "ls-remote", url: "u" } }), ""), {
    state: "verified", sha, ref: "refs/heads/main", source: "ls-remote", url: "u",
  });
  assert.deepEqual(parseBaseOutput(0, JSON.stringify({ dir: "/r", unverified: { message: "offline" } }), ""), { state: "unverified", message: "offline" });
  assert.deepEqual(parseBaseOutput(0, JSON.stringify({ dir: "/r", repo: null }), ""), { state: "no_repo" });
  assert.equal(parseBaseOutput(0, JSON.stringify({ base: { sha: "HEAD", ref: "x" } }), "").state, "unverified", "a base that is not an object id");
  assert.match(parseBaseOutput(1, "", "boom").message, /failed \(exit 1\): boom/);
  const pin = { head: "a".repeat(40), changes: "b".repeat(64) };
  const shown = parseDiffOutput(0, JSON.stringify({ dir: "/r", top: "/r", base: sha, pin, files: [{ status: "M", path: "a" }], files_truncated: false, patch: "+x", truncated: true }), "");
  assert.equal(shown.state, "shown");
  assert.equal(shown.truncated, true);
  assert.equal(shown.programPinned, undefined, "no program asked about");
  const asked = parseDiffOutput(0, JSON.stringify({ dir: "/r", top: "/r", base: sha, pin, files: [], patch: "", truncated: false, program_pinned: false }), "");
  assert.equal(asked.programPinned, false);
  assert.deepEqual(shown.files, [{ status: "M", path: "a" }]);
  assert.deepEqual(parseDiffOutput(1, JSON.stringify({ dir: "/r", refused: { reason: "tree_changed", message: "moved" } }), ""), {
    state: "refused", reason: "tree_changed", message: "moved",
  });
  assert.deepEqual(parseDiffOutput(0, JSON.stringify({ dir: "/r", repo: null }), ""), { state: "no_repo" });
  assert.equal(parseDiffOutput(1, "", "no base").state, "error");
});
