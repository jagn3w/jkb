//! The main process's `jkb serve` client, against a real HTTP server on loopback.
//
// `src/main/daemon.ts` is plain Node (no Electron), so it is bundled here with esbuild and driven
// against a `node:http` server standing in for `jkb serve`: real sockets, real headers, a real
// token file on disk. Nothing is mocked but the daemon's answers.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as http from "node:http";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-daemon-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));

const bundle = path.join(work, "daemon.mjs");
await esbuild.build({
  entryPoints: [path.join(here, "..", "src", "main", "daemon.ts")],
  bundle: true,
  format: "esm",
  platform: "node",
  outfile: bundle,
  logLevel: "silent",
});
const { DaemonClient, readToken } = await import(bundle);

/**
 * A stand-in `jkb serve`: answers with `handler`'s `[status, body]`, accepting only `token()`,
 * and records every request it saw.
 */
async function daemon(handler, token = () => "tok") {
  const seen = [];
  const server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", async () => {
      seen.push({ method: req.method, url: req.url, auth: req.headers.authorization, type: req.headers["content-type"], body });
      const [status, reply] =
        req.headers.authorization === `Bearer ${token()}`
          ? await handler(req, body)
          : [401, { code: "unauthorized", message: "bad token" }];
      res.writeHead(status, { "content-type": "application/json" });
      res.end(typeof reply === "string" ? reply : JSON.stringify(reply));
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  after(() => server.close());
  return { url: `http://127.0.0.1:${server.address().port}`, seen };
}

let n = 0;
function tokenFile(contents) {
  const file = path.join(work, `token-${n++}`);
  if (contents !== undefined) fs.writeFileSync(file, contents, { mode: 0o600 });
  return file;
}

test("an op is POSTed with the bearer token and decoded", async () => {
  const d = await daemon(() => [200, { result: "sessions", sessions: [] }]);
  const client = new DaemonClient({ url: d.url, tokenFile: tokenFile("tok\n") });
  const out = await client.op({ op: "session.list", all: false });
  assert.deepEqual(out, { ok: true, value: { result: "sessions", sessions: [] } });
  assert.deepEqual(d.seen[0], {
    method: "POST",
    url: "/v1/op",
    auth: "Bearer tok",
    type: "application/json",
    body: '{"op":"session.list","all":false}',
  });
});

test("hello is a GET and is checked for protocol", async () => {
  const hello = { protocol: 1, schema_version: 23, supported_schema: 23, ops: ["mq.poll"] };
  const d = await daemon(() => [200, hello]);
  const client = new DaemonClient({ url: d.url, tokenFile: tokenFile("tok") });
  assert.deepEqual(await client.hello(), { ok: true, value: hello });
  assert.equal(d.seen[0].method, "GET");
  assert.equal(d.seen[0].url, "/v1/hello");
});

test("a long-poll asks for its wait, and its timeout covers the wait", async () => {
  const d = await daemon(async () => {
    await new Promise((r) => setTimeout(r, 150));
    return [200, { result: "messages", messages: [] }];
  });
  const client = new DaemonClient({ url: d.url, tokenFile: tokenFile("tok"), timeoutMs: 50 });
  const out = await client.op({ op: "mq.poll", topic: "t", group: "g", max: 1 }, { waitMs: 500 });
  assert.equal(out.ok, true, "the 150ms hold is inside 50ms + the 500ms wait");
  assert.equal(d.seen[0].url, "/v1/op?wait_ms=500");
});

test("a call past its timeout is unavailable, saying so", async () => {
  const d = await daemon(async () => {
    await new Promise((r) => setTimeout(r, 300));
    return [200, { result: "x" }];
  });
  const client = new DaemonClient({ url: d.url, tokenFile: tokenFile("tok"), timeoutMs: 50 });
  const out = await client.op({ op: "mq.inspect" });
  assert.equal(out.ok, false);
  assert.equal(out.error.code, "unavailable");
  assert.match(out.error.message, /did not answer/);
});

test("a rotated token is re-read once after the daemon says unauthorized", async () => {
  let current = "old";
  const d = await daemon(() => [200, { result: "topics", topics: [] }], () => current);
  const file = tokenFile("old");
  const client = new DaemonClient({ url: d.url, tokenFile: file });
  assert.equal((await client.op({ op: "mq.inspect" })).ok, true);

  // The daemon restarts: a new token, written where it always is.
  current = "new";
  fs.writeFileSync(file, "new");
  const out = await client.op({ op: "mq.inspect" });
  assert.equal(out.ok, true);
  assert.deepEqual(
    d.seen.map((s) => s.auth),
    ["Bearer old", "Bearer old", "Bearer new"],
    "the cached token first, then exactly one retry with the fresh one",
  );
});

test("a token the daemon still refuses is the answer, after one retry", async () => {
  const d = await daemon(() => [200, { result: "x" }], () => "something-else");
  const client = new DaemonClient({ url: d.url, tokenFile: tokenFile("tok") });
  const out = await client.op({ op: "mq.inspect" });
  assert.equal(out.error.code, "unauthorized");
  assert.equal(d.seen.length, 2);
});

test("another error is not retried", async () => {
  const d = await daemon(() => [400, { code: "bad_request", message: "unknown field" }]);
  const client = new DaemonClient({ url: d.url, tokenFile: tokenFile("tok") });
  const out = await client.op({ op: "mq.inspect", nope: 1 });
  assert.deepEqual(out, { ok: false, error: { code: "bad_request", message: "unknown field" } });
  assert.equal(d.seen.length, 1);
});

test("an oversized body is refused before it is sent", async () => {
  const d = await daemon(() => [200, { result: "x" }]);
  const client = new DaemonClient({ url: d.url, tokenFile: tokenFile("tok") });
  const out = await client.op({ op: "ingest.text", text: "x".repeat(1024 * 1024) });
  assert.equal(out.error.code, "too_large");
  assert.equal(d.seen.length, 0, "nothing reached the daemon");
});

test("no token file, or no daemon, is unavailable with a reason", async () => {
  const missing = new DaemonClient({ url: "http://127.0.0.1:1", tokenFile: tokenFile(undefined) });
  const noToken = await missing.hello();
  assert.equal(noToken.error.code, "unavailable");
  assert.match(noToken.error.message, /is jkb serve running/);

  // A port nothing listens on: bind one, note it, close it.
  const probe = http.createServer();
  await new Promise((r) => probe.listen(0, "127.0.0.1", r));
  const port = probe.address().port;
  await new Promise((r) => probe.close(r));
  const down = new DaemonClient({ url: `http://127.0.0.1:${port}`, tokenFile: tokenFile("tok") });
  const unreachable = await down.op({ op: "mq.inspect" });
  assert.equal(unreachable.error.code, "unavailable");
  assert.match(unreachable.error.message, /cannot reach jkb serve/);
});

test("the token file must be a small regular file holding one word, never a link", async () => {
  assert.deepEqual(await readToken(tokenFile("  abc \n")), { ok: true, value: "abc" });
  assert.match((await readToken(tokenFile(" \n"))).error.message, /holds no token/);
  assert.match((await readToken(tokenFile("two words"))).error.message, /one printable word/);
  assert.match((await readToken(tokenFile("x".repeat(5000)))).error.message, /is not a token/);

  const secret = tokenFile("not-for-the-daemon");
  const link = path.join(work, "link-token");
  fs.symlinkSync(secret, link);
  const linked = await readToken(link);
  assert.equal(linked.ok, false);
  assert.match(linked.error.message, /symbolic link/);

  const dir = path.join(work, "a-dir");
  fs.mkdirSync(dir);
  assert.match((await readToken(dir)).error.message, /not a regular file|EISDIR/);
});

/** A home with `~/.jkb/daemon/7117/token` holding `tok`, and a decoy token the container chose. */
function home() {
  const h = fs.mkdtempSync(path.join(work, "home-"));
  const port = path.join(h, ".jkb", "daemon", "7117");
  fs.mkdirSync(port, { recursive: true });
  fs.writeFileSync(path.join(port, "token"), "tok", { mode: 0o600 });
  const decoy = path.join(h, "elsewhere", "7117");
  fs.mkdirSync(decoy, { recursive: true });
  fs.writeFileSync(path.join(decoy, "token"), "chosen-by-the-container", { mode: 0o600 });
  return { h, token: path.join(port, "token"), decoy };
}

// `~/.jkb` is the container's to write: a link at ANY component below the home — the `daemon`
// directory, the `<port>` directory, the token — would have the app send the file it points at.
test("a symbolic link anywhere below the home is refused, not only at the token", async () => {
  const plain = home();
  assert.deepEqual(await readToken(plain.token, plain.h), { ok: true, value: "tok" });

  const port = home();
  fs.rmSync(path.dirname(port.token), { recursive: true });
  fs.symlinkSync(port.decoy, path.dirname(port.token));
  const viaPort = await readToken(port.token, port.h);
  assert.equal(viaPort.ok, false, "the decoy behind a linked <port> directory was read");
  assert.equal(viaPort.error.code, "token_refused");
  assert.match(viaPort.error.message, /\.jkb\/daemon\/7117 is a symbolic link/);

  const daemonDir = home();
  const real = path.join(daemonDir.h, ".jkb", "daemon");
  fs.renameSync(real, path.join(daemonDir.h, "moved"));
  fs.symlinkSync(path.join(daemonDir.h, "moved"), real);
  const viaDaemon = await readToken(daemonDir.token, daemonDir.h);
  assert.equal(viaDaemon.ok, false, "a linked daemon directory was followed");
  assert.match(viaDaemon.error.message, /\.jkb\/daemon is a symbolic link/);

  // The client passes its trusted root through.
  const client = new DaemonClient({ url: "http://127.0.0.1:1", tokenFile: port.token, trustedRoot: port.h });
  assert.equal((await client.hello()).error.code, "token_refused");
});

// Without O_NONBLOCK, open() on a FIFO waits for a writer, parking a libuv thread for good.
test("a FIFO at the token path is refused at once, not waited on", { skip: process.platform === "win32" }, async () => {
  const fifo = path.join(work, "fifo-token");
  const made = spawnSync("mkfifo", [fifo]);
  assert.equal(made.status, 0, `mkfifo: ${made.stderr}`);
  let timer;
  const read = readToken(fifo);
  const timedOut = new Promise((resolve) => {
    timer = setTimeout(() => resolve("blocked"), 2000);
  });
  const outcome = await Promise.race([read, timedOut]);
  clearTimeout(timer);
  if (outcome === "blocked") {
    // Release the parked open so the test process can exit, then fail.
    fs.closeSync(fs.openSync(fifo, fs.constants.O_WRONLY | fs.constants.O_NONBLOCK));
    await read;
    assert.fail("readToken blocked opening a FIFO");
  }
  assert.equal(outcome.ok, false);
  assert.equal(outcome.error.code, "token_refused");
  assert.match(outcome.error.message, /not a regular file/);
});

// The message crosses the bridge, and the renderer is not told where the token lives (AppInfo).
test("a token failure names no absolute path", async () => {
  const { h, token } = home();
  const missing = await readToken(path.join(h, ".jkb", "daemon", "9", "token"), h);
  assert.equal(missing.error.code, "unavailable");
  assert.match(missing.error.message, /ENOENT/);
  fs.writeFileSync(token, "two words");
  const bad = await readToken(token, h);
  for (const outcome of [missing, bad]) {
    assert.ok(!outcome.error.message.includes(h), `the home path leaked: ${outcome.error.message}`);
    assert.ok(!outcome.error.message.includes(work), `the work path leaked: ${outcome.error.message}`);
  }
});
