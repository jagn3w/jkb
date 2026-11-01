//! Running a run request (the Executions design) in main, without a window: `ExecRunner` against a
//! stand-in daemon that keeps `jkb_core::exec`'s compare-and-set rules, a stand-in host `jkb` whose
//! `exec pin --json` answers what each test needs, and main's real `TerminalHost` over real PTYs.
//
// The modules are bundled with esbuild, as in terminal.test.mjs.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-exec-")));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const require = createRequire(import.meta.url);

async function load(entry) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({ entryPoints: [entry], bundle: true, format: "cjs", platform: "node", outfile, logLevel: "silent", external: ["node-pty"] });
  return require(outfile);
}

const src = path.join(here, "..", "src");
const exec = await load(path.join(src, "main", "execRun.ts"));
const terminals = await load(path.join(src, "main", "terminals.ts"));
const update = await load(path.join(src, "main", "update.ts"));
const pty = require("node-pty");

const PIN = { head: "a".repeat(40), changes: "b".repeat(64) };
const OTHER_PIN = { head: "a".repeat(40), changes: "c".repeat(64) };
const BOOT = "test-boot";

let homes = 0;
/** A scratch account home with a stand-in host jkb whose `exec pin --json` prints `answer` and exits `code`. */
function home({ answer = { pin: PIN }, code = 0 } = {}) {
  const h = path.join(work, `home-${homes++}`);
  fs.mkdirSync(path.join(h, ".cargo", "bin"), { recursive: true });
  fs.mkdirSync(path.join(h, "repos", "r"), { recursive: true });
  const calls = path.join(h, "pin-calls");
  const q = (s) => `'${s.replaceAll("'", "'\\''")}'`;
  fs.writeFileSync(
    path.join(h, ".cargo", "bin", "jkb"),
    `#!/bin/sh\nprintf '%s\\n' "$*" >> ${q(calls)}\nprintf '%s\\n' ${q(JSON.stringify(answer))}\nexit ${code}\n`,
    { mode: 0o755 },
  );
  return { h, repo: path.join(h, "repos", "r"), pinCalls: () => (fs.existsSync(calls) ? fs.readFileSync(calls, "utf8").trim().split("\n") : []) };
}

/**
 * A stand-in `jkb serve` holding run requests by `jkb_core::exec`'s rules: every move a
 * compare-and-set on state, `begin` refusing a different pin, output and finish only from the runner
 * that began it. `down` makes every op unreachable; `downFor` only the named ones.
 */
function daemon() {
  const rows = new Map();
  const d = {
    rows,
    calls: [],
    down: false,
    downFor: new Set(),
    outputs: (id) => (rows.get(id)?.chunks ?? []).map((c) => Buffer.from(c, "base64")).join(""),
    add(over = {}) {
      const n = rows.size + 1;
      const row = {
        id: `exec-${n}`, topic: `exec/${n}`, state: "pending", target: "host", dir: "/", dir_side: "host",
        argv: ["/bin/true"], reason: "", session: "s", role: "implementer", timeout_ms: 900000,
        created_at: 1, expires_at: 2, pin: PIN, chunks: [], ...over,
      };
      rows.set(row.id, row);
      return row;
    },
    async op(req) {
      d.calls.push(req);
      const fail = (code, message) => ({ ok: false, error: { code, message } });
      if (d.down || d.downFor.has(req.op)) return fail("unavailable", "jkb serve is down");
      const row = rows.get(req.id);
      const view = (r) => {
        const { chunks, ...v } = r;
        return v;
      };
      switch (req.op) {
        case "exec.status":
          return row ? { ok: true, value: { result: "exec_request", request: view(row) } } : fail("not_found", "no such request");
        case "exec.list": {
          const requests = [...rows.values()].filter((r) => r.state === req.state).map(view);
          return { ok: true, value: { result: "exec_requests", requests } };
        }
        case "exec.begin": {
          if (row.state !== "pending") return fail("stale", `cannot begin ${row.id}: it is ${row.state}, not pending`);
          const same = (row.pin ?? null) === null ? req.recomputed_pin === undefined : JSON.stringify(req.recomputed_pin) === JSON.stringify(row.pin);
          if (!same) {
            Object.assign(row, { state: "refused", refuse_reason: "tree_changed" });
            return { ok: true, value: { result: "exec_begun", begun: "refused", request: view(row) } };
          }
          Object.assign(row, { state: "running", runner: req.runner });
          return { ok: true, value: { result: "exec_begun", begun: "running", request: view(row) } };
        }
        case "exec.refuse":
          if (row.state !== "pending") return fail("stale", "not pending");
          Object.assign(row, { state: "refused", refuse_reason: req.reason });
          return { ok: true, value: { result: "exec_request", request: view(row) } };
        case "exec.output":
          if (row.state !== "running" || row.runner !== req.runner) return fail("stale", "not running under this runner");
          row.chunks.push(req.chunk_b64);
          return { ok: true, value: { result: "exec_output_sent", seq: row.chunks.length, n: row.chunks.length - 1 } };
        case "exec.finish":
          if (row.state !== "running" || row.runner !== req.runner) return fail("stale", "not running under this runner");
          Object.assign(row, { state: "finished", outcome: req.outcome, exit_status: req.exit_status, tail: req.tail_b64, total: req.total_bytes, log_path: req.log_path });
          return { ok: true, value: { result: "exec_request", request: view(row) } };
        default:
          return fail("bad_request", `unknown op ${req.op}`);
      }
    },
  };
  return d;
}

function environment(roots, over = {}) {
  return {
    roots,
    docker: () => undefined,
    dockerSearched: () => [],
    hostShell: "/bin/sh",
    env: process.env,
    isDirectory: (p) => fs.existsSync(p) && fs.statSync(p).isDirectory(),
    run: terminals.runToEnd(process.env),
    containerRunDir: path.join(work, "run"),
    processAlive: (pid) => {
      try {
        process.kill(pid, 0);
        return true;
      } catch {
        return false;
      }
    },
    signalGroup: (pgid, signal) => {
      try {
        process.kill(-pgid, signal);
      } catch {
        // Gone.
      }
    },
    ...over,
  };
}

/** A runner over a stand-in daemon, the stand-in jkb in `h`, and a real terminal host. */
function rig({ d = daemon(), h = home(), timing = {}, pid = process.pid } = {}) {
  const roots = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: path.join(h.h, "repos"), hostReposReal: path.join(h.h, "repos"), hostHome: h.h };
  const env = environment(roots);
  const events = [];
  const host = new terminals.TerminalHost(pty.spawn, env, (owner, event) => events.push({ owner, ...event }));
  const issued = [];
  const log = [];
  const runner = new exec.ExecRunner({
    op: (r) => d.op(r),
    pin: exec.machinePin(h.h, update.machineRunner(h.h, process.env)),
    roots,
    isDirectory: env.isDirectory,
    findHostProgram: (name) => (name === "sh" ? "/bin/sh" : undefined),
    docker: () => undefined,
    issueOnce: (owner, spec, run) => {
      issued.push(spec);
      return host.issueOnce(owner, spec, run);
    },
    processAlive: env.processAlive,
    pid,
    boot: BOOT,
    home: h.h,
    log: (m) => log.push(m),
    timing: { retryMs: 10, maxRetryMs: 50, ...timing },
  });
  return { d, h, host, runner, issued, events, log };
}

async function until(pred, what, ms = 15_000) {
  const start = Date.now();
  while (!pred()) {
    if (Date.now() - start > ms) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
}

test("a run issues exactly the stored argv, in its directory, with JKB_EXEC_REQUEST, and streams to the topic and the log", async () => {
  const { d, h, host, runner, issued } = rig();
  const row = d.add({ dir: h.repo, argv: ["/bin/sh", "-c", 'printf "hi from %s in %s" "$JKB_EXEC_REQUEST" "$(pwd)"; exit 3'] });
  const started = await runner.run(1, row.id);
  assert.equal(started.ok, true, started.error);
  assert.deepEqual(started.value.spec.argv, row.argv, "exactly the stored argv");
  assert.equal(started.value.spec.cwd, h.repo);
  assert.equal(started.value.spec.target, "host");
  assert.equal(started.value.timeoutMs, 900000, "the request's timeout");
  assert.deepEqual(issued, [started.value.spec]);
  assert.deepEqual(h.pinCalls(), [`--json exec pin --dir ${h.repo}`], "the host jkb recomputed the pin");
  assert.equal(d.rows.get(row.id).runner, `code-factory:${process.pid}@${BOOT}`);

  const opened = host.open(1, started.value.spec, 80, 24);
  assert.equal(opened.ok, true, opened.error);
  await until(() => row.state === "finished", "the finish");
  assert.equal(row.outcome, "exited");
  assert.equal(row.exit_status, 3);
  assert.equal(d.outputs(row.id), `hi from ${row.id} in ${h.repo}`);
  assert.equal(Buffer.from(row.tail, "base64").toString(), `hi from ${row.id} in ${h.repo}`);
  assert.equal(row.total, Buffer.byteLength(d.outputs(row.id)));
  assert.equal(row.log_path, path.join(h.h, ".jkb", "logs", "exec", `${row.id}.log`));
  assert.equal(fs.readFileSync(row.log_path, "utf8"), d.outputs(row.id));
  assert.equal(fs.statSync(row.log_path).mode & 0o777, 0o600);
  const finishAt = d.calls.findIndex((c) => c.op === "exec.finish");
  assert.ok(d.calls.slice(finishAt).every((c) => c.op !== "exec.output"), "every chunk before the finish");
});

test("a relative program runs from the request's directory; a bare name is found where run.sh looks", async () => {
  const { d, h, runner } = rig();
  fs.writeFileSync(path.join(h.repo, "go.sh"), "#!/bin/sh\necho go\n", { mode: 0o755 });
  const rel = d.add({ dir: h.repo, argv: ["./go.sh", "x"] });
  const a = await runner.run(1, rel.id);
  assert.equal(a.ok, true, a.error);
  assert.deepEqual(a.value.spec.argv, [path.join(h.repo, "./go.sh"), "x"]);
  const bare = d.add({ dir: h.repo, argv: ["sh", "-c", "true"] });
  const b = await runner.run(1, bare.id);
  assert.deepEqual(b.value.spec.argv, ["/bin/sh", "-c", "true"]);
  const missing = d.add({ dir: h.repo, argv: ["nosuchprogram"] });
  const c = await runner.run(1, missing.id);
  assert.equal(c.ok, false);
  assert.match(c.error, /not found where run.sh looks/);
  assert.equal(missing.state, "pending", "a program that cannot be found refuses nothing: the operator declines");
});

test("a second run of one request is refused while the first is live, and after it ends", async () => {
  const { d, h, host, runner, issued } = rig();
  const row = d.add({ dir: h.repo, argv: ["/bin/sh", "-c", "sleep 0.3"] });
  const [first, second] = await Promise.all([runner.run(1, row.id), runner.run(2, row.id)]);
  assert.equal(first.ok, true, first.error);
  assert.equal(second.ok, false);
  assert.match(second.error, /already been run/);
  assert.equal(d.calls.filter((c) => c.op === "exec.begin").length, 1, "one begin, from the click");
  assert.equal(host.open(1, first.value.spec, 80, 24).ok, true);
  await until(() => row.state === "finished", "the finish");
  const again = await runner.run(1, row.id);
  assert.equal(again.ok, false);
  assert.equal(issued.length, 1);
  // Another app instance (a fresh runner) is held off by the record itself.
  const other = rig({ d, h, pid: process.pid + 1 });
  const theirs = await other.runner.run(1, row.id);
  assert.equal(theirs.ok, false);
  assert.match(theirs.error, /finished, not pending/);
});

test("a pin that differs never opens a terminal, and the request ends refused", async () => {
  const h = home({ answer: { pin: OTHER_PIN } });
  const { d, runner, issued } = rig({ h });
  const row = d.add({ dir: h.repo, argv: ["/bin/true"] });
  const preview = await runner.preview(row.id);
  assert.equal(preview.ok, true);
  assert.equal(preview.value.pin.state, "differs");
  assert.deepEqual(preview.value.pin.now, OTHER_PIN);
  assert.match(preview.value.problem, /tree changed/);
  assert.equal(row.state, "pending", "a preview changes nothing");
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, false);
  assert.match(r.error, /refused \(tree_changed\)/);
  assert.equal(row.state, "refused");
  assert.equal(row.refuse_reason, "tree_changed");
  assert.deepEqual(issued, [], "no terminal was issued");
});

test("an audit the host jkb refuses refuses the request audit_refused; an unpinnable tree, tree_changed", async () => {
  for (const [reason, refused] of [["audit_refused", "audit_refused"], ["unpinnable", "tree_changed"], ["dir_gone", "dir_gone"]]) {
    const h = home({ answer: { dir: "/x", refused: { reason, message: `the ${reason} detail` } }, code: 1 });
    const { d, runner, issued } = rig({ h });
    const row = d.add({ dir: h.repo, argv: ["/bin/true"] });
    const r = await runner.run(1, row.id);
    assert.equal(r.ok, false);
    assert.match(r.error, new RegExp(`the ${reason} detail`));
    assert.equal(row.state, "refused", reason);
    assert.equal(row.refuse_reason, refused, reason);
    assert.deepEqual(issued, []);
    assert.equal(d.calls.filter((c) => c.op === "exec.begin").length, 0);
  }
});

test("a pin the host jkb could not answer refuses nothing, and says why", async () => {
  const h = home({ answer: "not json at all", code: 2 });
  const { d, runner } = rig({ h });
  const row = d.add({ dir: h.repo });
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, false);
  assert.match(r.error, /jkb exec pin failed \(exit 2\)/);
  assert.equal(row.state, "pending");
  fs.rmSync(path.join(h.h, ".cargo", "bin", "jkb"));
  const gone = await runner.run(1, row.id);
  assert.match(gone.error, /no jkb at .*\.cargo\/bin\/jkb/);
  assert.equal(row.state, "pending");
});

test("a directory the host cannot reach is refused dir_gone before any pin", async () => {
  const { d, h, runner } = rig();
  const row = d.add({ dir: path.join(h.repo, "gone") });
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, false);
  assert.equal(row.refuse_reason, "dir_gone");
  assert.deepEqual(h.pinCalls(), []);
  // A container path under the repos mount is carried to the host's side of it.
  const mapped = d.add({ dir: "/home/vscode/repos/r", dir_side: "container", argv: ["/bin/true"] });
  const m = await runner.run(1, mapped.id);
  assert.equal(m.ok, true, m.error);
  assert.equal(m.value.spec.cwd, h.repo);
});

test("the renderer's generic op goes through the gate that refuses the runner's ops", () => {
  const index = fs.readFileSync(path.join(src, "main", "index.ts"), "utf8");
  const handler = /ipcMain\.handle\(BRIDGE_CHANNELS\.op,[\s\S]*?\n {2}\}\);/.exec(index);
  assert.ok(handler, "the op handler");
  assert.match(handler[0], /return rendererOp\(request, \(r\) => daemon\.op\(r\)\);/);
  assert.equal(handler[0].match(/daemon\.op/g).length, 1, "the daemon is reached only through the gate");
});

test("an issued run opens once: a second open of the same spec is refused, and so is a revoked one", () => {
  const h = home();
  const roots = { container: "jkb-dev", containerRepos: "/c", hostRepos: h.h, hostReposReal: h.h, hostHome: h.h };
  const host = new terminals.TerminalHost(pty.spawn, environment(roots), () => {});
  const spec = { target: "host", cwd: h.repo, argv: ["/bin/sh", "-c", "sleep 0.2"], title: "exec-1: sh", sessionUuid: "0f8fad5b-d9cb-469f-a165-70867728950e" };
  const seen = [];
  const run = { env: { JKB_EXEC_REQUEST: "exec-1" }, onOpen: () => seen.push("open"), onData: () => {}, onExit: () => seen.push("exit"), onFail: (e) => seen.push(`fail ${e}`) };
  host.issueOnce(1, spec, run);
  assert.equal(host.open(2, spec, 80, 24).ok, false, "another window cannot take it");
  assert.equal(host.open(1, spec, 80, 24).ok, true);
  const again = host.open(1, spec, 80, 24);
  assert.equal(again.ok, false);
  assert.match(again.error, /the host runs only/);
  assert.deepEqual(seen, ["open"]);
  const revoke = host.issueOnce(1, { ...spec, sessionUuid: "1f8fad5b-d9cb-469f-a165-70867728950e" }, run);
  assert.equal(revoke(), true);
  assert.equal(revoke(), false, "revoked once");
  assert.equal(host.open(1, { ...spec, sessionUuid: "1f8fad5b-d9cb-469f-a165-70867728950e" }, 80, 24).ok, false);
  host.closeAll();
});

test("a container run gets JKB_EXEC_REQUEST as docker exec -e", () => {
  const roots = { container: "jkb-dev", containerRepos: "/c", hostRepos: "/h", hostReposReal: "/h", hostHome: "/h" };
  const r = terminals.commandFor({ target: "container", cwd: "/c/r", argv: ["make"], title: "t" }, environment(roots, { docker: () => "/usr/bin/docker" }), "t-1", { JKB_EXEC_REQUEST: "exec-4" });
  assert.equal(r.ok, true);
  const at = r.value.args.indexOf("JKB_EXEC_REQUEST=exec-4");
  assert.ok(at > 0 && r.value.args[at - 1] === "-e" && at < r.value.args.indexOf("jkb-dev"), r.value.args.join(" "));
  assert.equal(terminals.commandFor({ target: "container", cwd: "/c", argv: [], title: "t" }, environment(roots, { docker: () => "/d" }), "t-1", { "-x": "y" }).ok, false);
});

test("the timeout ends the whole process group, with SIGKILL for what ignores SIGTERM, and reports timed_out", async () => {
  const { d, h, host, runner } = rig({ timing: { killGraceMs: 300 } });
  const child = path.join(h.h, "child.pid");
  const row = d.add({
    dir: h.repo,
    timeout_ms: 300,
    // The child ignores the hangup the PTY's close sends too: only the group's SIGKILL ends it.
    argv: ["/bin/sh", "-c", `trap '' TERM HUP; sh -c 'trap "" TERM HUP; sleep 30' & echo $! > ${child}; sleep 30`],
  });
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, true, r.error);
  assert.equal(host.open(1, r.value.spec, 80, 24).ok, true);
  await until(() => row.state === "finished", "the finish");
  assert.equal(row.outcome, "timed_out");
  const pid = Number(fs.readFileSync(child, "utf8"));
  await until(() => {
    try {
      process.kill(pid, 0);
      return false;
    } catch {
      return true;
    }
  }, "the background child to be gone");
});

test("a terminal never opened is ended interrupted at the deadline, and cannot be opened after", async () => {
  const { d, h, host, runner } = rig({ timing: { openDeadlineMs: 100 } });
  const row = d.add({ dir: h.repo, argv: ["/bin/true"] });
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, true);
  await until(() => row.state === "finished", "the interrupted finish");
  assert.equal(row.outcome, "interrupted");
  assert.equal(host.open(1, r.value.spec, 80, 24).ok, false);
});

test("a window that closes before it opens its run ends the run interrupted at once", async () => {
  const { d, h, host, runner } = rig();
  const row = d.add({ dir: h.repo, argv: ["/bin/true"] });
  assert.equal((await runner.run(7, row.id)).ok, true);
  host.closeAll(7);
  await until(() => row.state === "finished", "the finish", 2_000);
  assert.equal(row.outcome, "interrupted");
});

test("while the daemon is down output waits and is delivered in order; past the buffer, the stream says what it missed", async () => {
  const d = daemon();
  const h = home();
  const row = d.add({ state: "running", runner: "r" });
  const log = [];
  const stream = new exec.ExecStream(row.id, "r", { op: (q) => d.op(q), home: h.h, log: (m) => log.push(m) }, { retryMs: 5, maxRetryMs: 20, bufferBytes: 1000 }, true);
  d.down = true;
  for (const part of ["one ", "two ", "three "]) stream.push(part);
  await new Promise((r) => setTimeout(r, 60));
  assert.equal(d.outputs(row.id), "", "nothing got through while it was down");
  stream.push("x".repeat(2000));
  stream.push("tail");
  d.down = false;
  await stream.finish("exited", 0);
  const got = d.outputs(row.id);
  // What did not fit is said where it was missed; what fit again afterwards follows it, in order.
  assert.equal(
    got,
    `one two three \r\n[jkb: 2000 bytes of output did not reach this stream while jkb serve could not take them; the full output is in ${row.log_path}]\r\ntail`,
  );
  assert.equal(row.state, "finished");
  assert.equal(row.total, 14 + 2004);
  assert.equal(fs.readFileSync(row.log_path, "utf8"), `one two three ${"x".repeat(2000)}tail`, "the log has all of it");
  const finishAt = d.calls.findIndex((c) => c.op === "exec.finish");
  assert.ok(d.calls.slice(finishAt + 1).every((c) => c.op !== "exec.output"));
});

test("output is chunked at the daemon's limit, and the tail is the last 256 KiB", async () => {
  const d = daemon();
  const h = home();
  const row = d.add({ state: "running", runner: "r" });
  const stream = new exec.ExecStream(row.id, "r", { op: (q) => d.op(q), home: h.h, log: () => {} }, { retryMs: 5, maxRetryMs: 20, bufferBytes: 4 * 1024 * 1024 }, false);
  const big = "é".repeat(200 * 1024);
  stream.push(big);
  await stream.finish("exited", 0);
  assert.ok(row.chunks.every((c) => Buffer.from(c, "base64").length <= 47 * 1024));
  assert.equal(Buffer.concat(row.chunks.map((c) => Buffer.from(c, "base64"))).toString(), big);
  assert.equal(Buffer.from(row.tail, "base64").length, 256 * 1024);
  assert.equal(row.total, Buffer.byteLength(big));
  assert.equal(row.log_path, undefined);
});

test("a log directory that is a link is refused: the run goes on without a log", async () => {
  const d = daemon();
  const h = home();
  fs.mkdirSync(path.join(h.h, ".jkb", "logs"), { recursive: true });
  const elsewhere = path.join(h.h, "elsewhere");
  fs.mkdirSync(elsewhere);
  fs.symlinkSync(elsewhere, path.join(h.h, ".jkb", "logs", "exec"));
  const row = d.add({ state: "running", runner: "r" });
  const log = [];
  const stream = new exec.ExecStream(row.id, "r", { op: (q) => d.op(q), home: h.h, log: (m) => log.push(m) }, { retryMs: 5, maxRetryMs: 20, bufferBytes: 1000 }, true);
  stream.push("hi");
  await stream.finish("exited", 0);
  assert.deepEqual(fs.readdirSync(elsewhere), [], "nothing was written through the link");
  assert.equal(row.log_path, undefined);
  assert.equal(d.outputs(row.id), "hi");
  assert.match(log.join("\n"), /no log/);
});

test("at startup, a run another Code Factory left running is finished interrupted, with its log's tail", async () => {
  const h = home();
  const d = daemon();
  const dead = 2 ** 22 + 12345;
  const earlierBoot = d.add({ state: "running", runner: `code-factory:${process.pid}@an-earlier-boot` });
  const deadPid = d.add({ state: "running", runner: `code-factory:${dead}@${BOOT}` });
  const alive = d.add({ state: "running", runner: `code-factory:${process.ppid}@${BOOT}` });
  const cli = d.add({ state: "running", runner: "someone-else" });
  fs.mkdirSync(path.join(h.h, ".jkb", "logs", "exec"), { recursive: true });
  fs.writeFileSync(path.join(h.h, ".jkb", "logs", "exec", `${deadPid.id}.log`), "partial output");
  const { runner } = rig({ d, h, pid: process.pid + 7 });
  assert.equal(await runner.reconcile(1), 2);
  assert.equal(earlierBoot.state, "finished");
  assert.equal(earlierBoot.outcome, "interrupted");
  assert.equal(deadPid.state, "finished");
  assert.equal(Buffer.from(deadPid.tail, "base64").toString(), "partial output");
  assert.equal(deadPid.total, 14);
  assert.equal(alive.state, "running", "a live instance's run is its own");
  assert.equal(cli.state, "running", "another kind of runner's is not ours to end");
});

test("jkb exec pin's output is read as pinned, refused, or an error", () => {
  assert.deepEqual(exec.parsePinOutput(0, `${JSON.stringify({ dir: "/r", pin: PIN, gitignores: [] })}\n`, ""), { kind: "pinned", pin: PIN });
  assert.deepEqual(exec.parsePinOutput(0, JSON.stringify({ dir: "/r", pin: null }), ""), { kind: "pinned", pin: null });
  assert.deepEqual(exec.parsePinOutput(1, JSON.stringify({ refused: { reason: "audit_refused", message: "m" } }), "Error: m"), { kind: "refused", reason: "audit_refused", message: "m" });
  assert.equal(exec.parsePinOutput(1, JSON.stringify({ pin: PIN }), "boom").kind, "error", "a failed exit is never a pin");
  assert.equal(exec.parsePinOutput(0, "", "").kind, "error");
  assert.equal(exec.refuseReasonFor("unpinnable"), "tree_changed");
});
