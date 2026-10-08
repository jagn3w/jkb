//! The `jkb serve` wire protocol as `@jkb/core` reads it. Runs against the emitted `dist/`, so
//! `pnpm run build` precedes it (as in `scripts/check.sh` and CI).

import assert from "node:assert/strict";
import test from "node:test";

import {
  DEFAULT_DAEMON_ADDR,
  daemonUrl,
  decodeHelloReply,
  decodeOpReply,
  helloUrl,
  opUrl,
  portOf,
  tokenPath,
} from "../dist/index.js";

test("the daemon URL mirrors the CLI's: default, bare host:port, explicit scheme", () => {
  assert.equal(daemonUrl(undefined), `http://${DEFAULT_DAEMON_ADDR}`);
  assert.equal(daemonUrl("   "), `http://${DEFAULT_DAEMON_ADDR}`, "blank is unset");
  assert.equal(daemonUrl(" host.docker.internal:7117 "), "http://host.docker.internal:7117");
  assert.equal(daemonUrl("https://example:9"), "https://example:9");
});

test("the port is explicit or the scheme's default, and IPv6 colons are not one", () => {
  assert.equal(portOf("http://127.0.0.1:7117"), 7117);
  assert.equal(portOf("http://127.0.0.1:7117/v1/op"), 7117);
  assert.equal(portOf("http://localhost"), 80);
  assert.equal(portOf("https://localhost"), 443);
  assert.equal(portOf("http://[::1]:8000"), 8000);
  assert.equal(portOf("http://[::1]"), 80, "the address's own colons are not a port");
  assert.equal(portOf("127.0.0.1:7200"), 7200, "no scheme reads as http");
  assert.equal(portOf("http://h:99999"), 80, "an out-of-range port is not a port");
});

test("the token lives per port under ~/.jkb/daemon, as jkb serve writes it", () => {
  assert.equal(tokenPath("/home/u", 7117), "/home/u/.jkb/daemon/7117/token");
  assert.equal(tokenPath("/home/u/", 80), "/home/u/.jkb/daemon/80/token");
});

test("op and hello URLs, with the long-poll wait only when asked", () => {
  assert.equal(opUrl("http://h:1/"), "http://h:1/v1/op");
  assert.equal(opUrl("http://h:1", 2500.7), "http://h:1/v1/op?wait_ms=2500");
  assert.equal(opUrl("http://h:1", -5), "http://h:1/v1/op?wait_ms=0");
  assert.equal(helloUrl("http://h:1"), "http://h:1/v1/hello");
});

test("a 2xx response is a success only when it carries a result tag", () => {
  const ok = decodeOpReply(200, JSON.stringify({ result: "sessions", sessions: [] }));
  assert.deepEqual(ok, { ok: true, value: { result: "sessions", sessions: [] } });

  const notJson = decodeOpReply(200, "<html>");
  assert.equal(notJson.ok, false);
  assert.equal(notJson.error.code, "internal");

  const untagged = decodeOpReply(200, JSON.stringify({ sessions: [] }));
  assert.equal(untagged.ok, false, "a 2xx without a result is not reported as a success");
});

test("an error reply keeps its code, message and seq; an unknown code reads as unknown", () => {
  const busy = decodeOpReply(503, JSON.stringify({ code: "busy", message: "retry" }));
  assert.deepEqual(busy, { ok: false, error: { code: "busy", message: "retry" } });

  const corrupt = decodeOpReply(500, JSON.stringify({ code: "corrupt_payload", message: "m", seq: 7 }));
  assert.deepEqual(corrupt.error, { code: "corrupt_payload", message: "m", seq: 7 });

  const newer = decodeOpReply(400, JSON.stringify({ code: "from_the_future", message: "m" }));
  assert.equal(newer.error.code, "unknown");

  const bare = decodeOpReply(502, "Bad Gateway");
  assert.equal(bare.error.code, "internal");
  assert.match(bare.error.message, /HTTP 502/);
});

test("hello is checked for shape and protocol", () => {
  const body = { protocol: 1, schema_version: 23, supported_schema: 23, ops: ["mq.poll"] };
  assert.deepEqual(decodeHelloReply(200, JSON.stringify(body)), { ok: true, value: body });

  const other = decodeHelloReply(200, JSON.stringify({ ...body, protocol: 2 }));
  assert.equal(other.ok, false);
  assert.equal(other.error.code, "unsupported");

  const malformed = decodeHelloReply(200, JSON.stringify({ ...body, ops: [1] }));
  assert.equal(malformed.error.code, "internal");

  const refused = decodeHelloReply(401, JSON.stringify({ code: "unauthorized", message: "no" }));
  assert.equal(refused.error.code, "unauthorized");
});
