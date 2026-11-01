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
const queue = await load(path.join(src, "renderer", "src", "executions", "queue.ts"));
const feed = await load(path.join(src, "main", "execFeed.ts"));
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
    /** `exec.begin` commits, then its answer is lost; `exec.status` is unreachable for this long after. */
    loseBegin: undefined,
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
          if (d.loseBegin !== undefined) {
            d.downFor.add("exec.status");
            setTimeout(() => d.downFor.delete("exec.status"), d.loseBegin);
            return fail("unavailable", "jkb serve restarted mid-call");
          }
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
function rig({ d = daemon(), h = home(), timing = {}, pid = process.pid, env: appEnv = process.env, realpath, identity, deps = {} } = {}) {
  const roots = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: path.join(h.h, "repos"), hostReposReal: path.join(h.h, "repos"), hostHome: h.h };
  const env = environment(roots, { env: appEnv });
  const events = [];
  const host = new terminals.TerminalHost(pty.spawn, env, (owner, event) => events.push({ owner, ...event }));
  const issued = [];
  const log = [];
  const runner = new exec.ExecRunner({
    op: (r) => d.op(r),
    pin: exec.machinePin(h.h, update.machineRunner(h.h, process.env)),
    roots,
    isDirectory: env.isDirectory,
    realpath: (p) => {
      try {
        return (realpath ?? fs.realpathSync.native)(p);
      } catch {
        return undefined;
      }
    },
    identity: identity ?? ((p) => {
      try {
        const st = fs.statSync(p, { bigint: true });
        return { dev: st.dev.toString(), ino: st.ino.toString() };
      } catch {
        return undefined;
      }
    }),
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
    ...deps,
  });
  // The queue's click: Run with what the preview showed, unless a test names something else.
  const runAsShown = runner.run.bind(runner);
  runner.run = async (owner, id, shown) => {
    if (shown === undefined) {
      const p = await runner.preview(id);
      const v = p.ok ? p.value : {};
      shown = { hostDir: v.hostDir ?? "", ...(v.program === undefined ? {} : { program: v.program }) };
    }
    return runAsShown(owner, id, shown);
  };
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
  // The app was started from a shell that exported a repository selection: the run must not inherit it.
  const { d, h, host, runner, issued } = rig({ env: { ...process.env, GIT_DIR: "/other/.git", GIT_WORK_TREE: "/other" } });
  const row = d.add({ dir: h.repo, argv: ["/bin/sh", "-c", 'printf "hi from %s in %s [%s]" "$JKB_EXEC_REQUEST" "$(pwd)" "${GIT_DIR-unset}${GIT_WORK_TREE-}"; exit 3'] });
  const started = await runner.run(1, row.id);
  assert.equal(started.ok, true, started.error);
  assert.deepEqual(started.value.argv, row.argv, "exactly the stored argv");
  const ino = fs.statSync(h.repo, { bigint: true }).ino.toString();
  const nonce = started.value.spec.argv[6];
  assert.match(nonce, /^[0-9a-f-]{36}$/, "a fresh nonce per run");
  assert.deepEqual(started.value.spec.argv, ["/bin/sh", "-c", exec.HOST_LAUNCHER, "jkb-exec", h.repo, ino, nonce, ...row.argv], "through the launcher");
  assert.equal(started.value.spec.cwd, h.repo);
  assert.equal(started.value.spec.target, "host");
  assert.equal(started.value.timeoutMs, 900000, "the request's timeout");
  assert.deepEqual(issued, [started.value.spec]);
  assert.ok(h.pinCalls().length > 0 && h.pinCalls().every((c) => c === `--json exec pin --dir ${h.repo}`), "the host jkb recomputed the pin");
  assert.equal(d.rows.get(row.id).runner, `code-factory:${process.pid}@${BOOT}`);

  const opened = host.open(1, started.value.spec, 80, 24);
  assert.equal(opened.ok, true, opened.error);
  await until(() => row.state === "finished", "the finish");
  assert.equal(row.outcome, "exited");
  assert.equal(row.exit_status, 3);
  assert.equal(d.outputs(row.id), `hi from ${row.id} in ${h.repo} [unset]`, "GIT_DIR and GIT_WORK_TREE are dropped");
  assert.equal(Buffer.from(row.tail, "base64").toString(), `hi from ${row.id} in ${h.repo} [unset]`);
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
  assert.deepEqual(a.value.argv, [path.join(h.repo, "go.sh"), "x"], "shown joined to the directory");
  assert.deepEqual(a.value.spec.argv.slice(-2), ["././go.sh", "x"], "exec'd from the verified directory");
  const bare = d.add({ dir: h.repo, argv: ["sh", "-c", "true"] });
  const b = await runner.run(1, bare.id);
  assert.deepEqual(b.value.argv, ["/bin/sh", "-c", "true"]);
  const missing = d.add({ dir: h.repo, argv: ["nosuchprogram"] });
  const c = await runner.run(1, missing.id);
  assert.equal(c.ok, false);
  assert.match(c.error, /not found where run.sh looks/);
  assert.equal(missing.state, "pending", "a program that cannot be found refuses nothing: the operator declines");
});

test("a second run of one request is refused while the first is live, and after it ends", async () => {
  const { d, h, host, runner, issued } = rig();
  const row = d.add({ dir: h.repo, argv: ["/bin/sh", "-c", "sleep 0.3"] });
  const { hostDir, program } = (await runner.preview(row.id)).value;
  const shown = { hostDir, program };
  const [first, second] = await Promise.all([runner.run(1, row.id, shown), runner.run(2, row.id, shown)]);
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

test("a container directory that resolves outside the repos mount is refused dir_gone, by `..` or by a link", async () => {
  const { d, h, runner, issued } = rig();
  const outside = path.join(h.h, "outside");
  fs.mkdirSync(outside);
  fs.symlinkSync(outside, path.join(h.repo, "scratch"));
  for (const dir of ["/home/vscode/repos/r/scratch", "/home/vscode/repos/../outside", "/home/vscode/repos/r/../../outside"]) {
    for (const target of ["host", "container"]) {
      const row = d.add({ dir, dir_side: "container", target, pin: null, argv: ["/bin/true"] });
      const preview = await runner.preview(row.id);
      assert.match(preview.value.problem, /outside the repos mount/, `${dir} ${target}`);
      const r = await runner.run(1, row.id);
      assert.equal(r.ok, false, `${dir} ${target}`);
      assert.equal(row.refuse_reason, "dir_gone", `${dir} ${target}`);
    }
  }
  assert.deepEqual(issued, []);
  assert.deepEqual(h.pinCalls(), [], "nothing outside the mount is even pinned");
});

test("a directory reached through a link inside the mount is shown, pinned and spawned resolved", async () => {
  const { d, h, runner } = rig();
  fs.mkdirSync(path.join(h.repo, "real"));
  fs.symlinkSync(path.join(h.repo, "real"), path.join(h.repo, "alias"));
  fs.writeFileSync(path.join(h.repo, "real", "go.sh"), "#!/bin/sh\n", { mode: 0o755 });
  const row = d.add({ dir: "/home/vscode/repos/r/alias", dir_side: "container", argv: ["./go.sh"] });
  const preview = await runner.preview(row.id);
  assert.equal(preview.value.hostDir, path.join(h.repo, "real"));
  assert.equal(preview.value.program, path.join(h.repo, "real", "go.sh"), "the resolved absolute program is shown");
  assert.equal(preview.value.programShown, path.join(h.repo, "real", "go.sh"));
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, true, r.error);
  assert.equal(r.value.spec.cwd, path.join(h.repo, "real"));
  assert.deepEqual(r.value.argv, [path.join(h.repo, "real", "go.sh")]);
  assert.ok(h.pinCalls().every((c) => c === `--json exec pin --dir ${path.join(h.repo, "real")}`), h.pinCalls().join("; "));
});

test("a directory swapped for a link after the pin and the begin, before the terminal opens, runs nothing", async () => {
  const { d, h, host, runner } = rig();
  const sub = path.join(h.repo, "sub");
  fs.mkdirSync(sub);
  const outside = path.join(h.h, "outside");
  fs.mkdirSync(outside);
  const ran = path.join(h.h, "ran");
  fs.writeFileSync(path.join(outside, "go.sh"), `#!/bin/sh\ntouch ${ran}\n`, { mode: 0o755 });
  fs.writeFileSync(path.join(sub, "go.sh"), `#!/bin/sh\ntouch ${ran}\n`, { mode: 0o755 });
  const row = d.add({ dir: "/home/vscode/repos/r/sub", dir_side: "container", argv: ["./go.sh"], pin: PIN });
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, true, r.error);
  // The container swaps the directory for a link to outside the mount.
  fs.renameSync(sub, `${sub}.old`);
  fs.symlinkSync(outside, sub);
  const opened = host.open(1, r.value.spec, 80, 24);
  assert.equal(opened.ok, false);
  assert.match(opened.error, /no longer the directory that was pinned/);
  await until(() => row.state === "finished", "the finish");
  assert.equal(row.outcome, "interrupted");
  assert.equal(fs.existsSync(ran), false, "nothing ran");
});

test("a swap the spawn-time check misses is caught by the launcher: nothing runs, and the run ends interrupted", async () => {
  const realIdentity = (p) => {
    const st = fs.statSync(p, { bigint: true });
    return { dev: st.dev.toString(), ino: st.ino.toString() };
  };
  // An identity read that keeps answering what it first saw: the race in which the swap lands after
  // main's last look and before the launcher's.
  const seen = new Map();
  const identity = (p) => {
    if (!seen.has(p)) seen.set(p, realIdentity(p));
    return seen.get(p);
  };
  const { d, h, host, runner } = rig({ identity });
  const sub = path.join(h.repo, "sub");
  fs.mkdirSync(sub);
  const ran = path.join(h.h, "ran");
  fs.writeFileSync(path.join(sub, "go.sh"), `#!/bin/sh\ntouch ${ran}\n`, { mode: 0o755 });
  const row = d.add({ dir: "/home/vscode/repos/r/sub", dir_side: "container", argv: ["./go.sh"] });
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, true, r.error);
  fs.renameSync(sub, `${sub}.old`);
  fs.mkdirSync(sub);
  fs.writeFileSync(path.join(sub, "go.sh"), `#!/bin/sh\ntouch ${ran}\n`, { mode: 0o755 });
  assert.equal(host.open(1, r.value.spec, 80, 24).ok, true, "main's own check was fooled");
  await until(() => row.state === "finished", "the finish");
  assert.equal(row.outcome, "interrupted");
  assert.match(d.outputs(row.id), /jkb: refusing to run \[[0-9a-f-]{36}\]: .* is not the directory that was pinned/);
  assert.equal(fs.existsSync(ran), false, "nothing ran");
});

test("the launcher itself refuses a directory that is not the pinned one, and execs nothing", async () => {
  const { execFileSync } = await import("node:child_process");
  const h = home();
  const ran = path.join(h.h, "ran");
  const ino = fs.statSync(h.repo, { bigint: true }).ino.toString();
  const nonce = "n0nce";
  const run = (dir, want, env = process.env) => {
    try {
      return { code: 0, out: execFileSync("/bin/sh", ["-c", exec.HOST_LAUNCHER, "jkb-exec", dir, want, nonce, "/bin/sh", "-c", `pwd -P; touch ${ran}`], { encoding: "utf8", stdio: "pipe", env }) };
    } catch (e) {
      return { code: e.status, out: `${e.stdout}${e.stderr}` };
    }
  };
  const ok = run(h.repo, ino);
  assert.equal(ok.code, 0, ok.out);
  assert.equal(ok.out.trim(), h.repo, "it runs in the verified directory");
  fs.rmSync(ran);
  const link = path.join(h.h, "link");
  fs.symlinkSync(h.h, link);
  for (const [dir, want] of [[link, ino], [h.h, ino], [path.join(h.h, "gone"), ino]]) {
    const r = run(dir, want);
    assert.equal(r.code, exec.LAUNCHER_REFUSED, `${dir}: ${r.out}`);
    assert.ok(r.out.startsWith(exec.launcherRefusal(nonce)), r.out);
    assert.equal(fs.existsSync(ran), false, `${dir}: nothing ran`);
  }
  // An `ls` first on the PATH that answers the pinned inode for any directory changes nothing: the
  // launcher runs /bin/ls.
  const fake = path.join(h.h, "fakebin");
  fs.mkdirSync(fake);
  fs.writeFileSync(path.join(fake, "ls"), `#!/bin/sh\necho "${ino} ."\n`, { mode: 0o755 });
  const fooled = run(h.h, ino, { ...process.env, PATH: `${fake}:${process.env.PATH}` });
  assert.equal(fooled.code, exec.LAUNCHER_REFUSED, fooled.out);
  assert.equal(fs.existsSync(ran), false, "a planted ls did not let it run");
});

test("a relative program that starts with - is the program, not an option of exec", async () => {
  // macOS's /bin/sh (bash 3.2) read `exec -a/x make` as `exec -a /x make` and ran `make` from PATH.
  const fake = path.join(work, `fakemake-${homes}`);
  fs.mkdirSync(fake);
  const wrong = path.join(fake, "wrong-ran");
  fs.writeFileSync(path.join(fake, "make"), `#!/bin/sh\ntouch ${wrong}\n`, { mode: 0o755 });
  const { d, h, host, runner } = rig({ env: { ...process.env, PATH: `${fake}:${process.env.PATH}` } });
  fs.mkdirSync(path.join(h.repo, "-a"));
  fs.writeFileSync(path.join(h.repo, "-a", "x"), '#!/bin/sh\nprintf "ran x with [%s]" "$1"\n', { mode: 0o755 });
  const row = d.add({ dir: h.repo, argv: ["-a/x", "make"] });
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, true, r.error);
  assert.deepEqual(r.value.argv, [path.join(h.repo, "-a", "x"), "make"]);
  assert.equal(host.open(1, r.value.spec, 80, 24).ok, true);
  await until(() => row.state === "finished", "the finish");
  assert.equal(d.outputs(row.id), "ran x with [make]");
  assert.equal(fs.existsSync(wrong), false, "make was an argument, never a program");
  for (const p of ["-a/x", "./x", "x/y"]) assert.ok(!exec.launchProgram(p).startsWith("-"), p);
  assert.equal(exec.launchProgram("/abs/x"), "/abs/x");
});

test("only the launcher's own refusal, with this run's nonce, counts as nothing run", async () => {
  const { d, h, host, runner } = rig();
  for (const line of ["jkb: refusing to run: fake", "jkb: refusing to run [guess]: fake"]) {
    const row = d.add({ dir: h.repo, argv: ["/bin/sh", "-c", `printf '%s\\n' '${line}'; exit 125`] });
    const r = await runner.run(1, row.id);
    assert.equal(r.ok, true, r.error);
    assert.equal(host.open(1, r.value.spec, 80, 24).ok, true);
    await until(() => row.state === "finished", "the finish");
    assert.equal(row.outcome, "exited", line);
    assert.equal(row.exit_status, 125, line);
  }
});

test("a run whose directory resolves differently than its preview showed is refused", async () => {
  const { d, h, runner, issued } = rig();
  fs.mkdirSync(path.join(h.repo, "one"));
  fs.mkdirSync(path.join(h.repo, "two"));
  fs.symlinkSync(path.join(h.repo, "one"), path.join(h.repo, "alias"));
  const row = d.add({ dir: "/home/vscode/repos/r/alias", dir_side: "container", argv: ["/bin/true"] });
  const preview = await runner.preview(row.id);
  assert.equal(preview.value.hostDir, path.join(h.repo, "one"));
  fs.rmSync(path.join(h.repo, "alias"));
  fs.symlinkSync(path.join(h.repo, "two"), path.join(h.repo, "alias"));
  const r = await runner.run(1, row.id, { hostDir: preview.value.hostDir, program: preview.value.program });
  assert.equal(r.ok, false);
  assert.match(r.error, /not what the preview showed/);
  assert.equal(row.refuse_reason, "tree_changed");
  assert.deepEqual(issued, []);
  const unnamed = d.add({ dir: h.repo, argv: ["/bin/true"] });
  const bare = await runner.run(1, unnamed.id, null);
  assert.match(bare.error, /names what its preview showed/);
  assert.equal(unnamed.state, "pending");
});

test("the pin's directory is compared by identity, not spelling: another spelling of the same directory is no move", async () => {
  // A case-insensitive file system's realpath and the pin can spell one directory two ways; stand
  // that in with a link to it, the same directory by identity.
  const h = home();
  const spelled = path.join(h.h, "repos", "R-other-spelling");
  fs.symlinkSync(h.repo, spelled);
  const answer = { dir: h.repo, pin: PIN };
  fs.writeFileSync(path.join(h.h, ".cargo", "bin", "jkb"), `#!/bin/sh\nprintf '%s\\n' '${JSON.stringify(answer)}'\n`, { mode: 0o755 });
  const { d, runner } = rig({ h, realpath: (p) => (p === h.repo ? spelled : fs.realpathSync.native(p)), deps: { diff: async (dir) => shownDiff(PIN, dir) } });
  const row = d.add({ dir: h.repo, argv: ["/bin/true"] });
  const preview = await runner.preview(row.id);
  assert.equal(preview.value.hostDir, spelled);
  assert.equal(preview.value.pin.state, "match", preview.value.problem);
  assert.equal(preview.value.problem, undefined);
});

test("a pin of another directory than the one resolved refuses the request tree_changed", async () => {
  const h = home({ answer: { dir: "/somewhere/else", pin: PIN } });
  const { d, runner, issued } = rig({ h });
  const row = d.add({ dir: h.repo, argv: ["/bin/true"] });
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, false);
  assert.match(r.error, /moved while it was pinned/);
  assert.equal(row.refuse_reason, "tree_changed");
  assert.deepEqual(issued, []);
});

test("a begin whose answer was lost is followed to its outcome: taken, the run goes on", async () => {
  const { d, h, runner, issued } = rig();
  const row = d.add({ dir: h.repo, argv: ["/bin/true"] });
  d.loseBegin = 150;
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, true, r.error);
  assert.equal(issued.length, 1);
});

test("a begin whose outcome stays unknown holds the gate, and is ended interrupted once the record answers", async () => {
  const { d, h, runner, issued } = rig({ timing: { unknownWaitMs: 50 } });
  const row = d.add({ dir: h.repo, argv: ["/bin/true"] });
  d.loseBegin = 400;
  const r = await runner.run(1, row.id);
  assert.equal(r.ok, false);
  assert.match(r.error, /could not tell whether/);
  assert.deepEqual(issued, []);
  assert.match((await runner.run(1, row.id)).error, /already been run/, "the gate stays held");
  await until(() => row.state === "finished", "the interrupted finish", 5_000);
  assert.equal(row.outcome, "interrupted");
  assert.equal(row.runner, runner.runner);
});

test("a once-run with an environment main may not set is refused at issue, for a host run too", () => {
  const h = home();
  const roots = { container: "jkb-dev", containerRepos: "/c", hostRepos: h.h, hostReposReal: h.h, hostHome: h.h };
  const host = new terminals.TerminalHost(pty.spawn, environment(roots), () => {});
  const spec = { target: "host", cwd: h.repo, argv: ["/bin/true"], title: "exec-1: true", sessionUuid: "2f8fad5b-d9cb-469f-a165-70867728950e" };
  for (const env of [{ "A=B": "x" }, { LD_X: "a\0b" }, { "-e": "x" }]) {
    const failed = [];
    const revoke = host.issueOnce(1, spec, { env, onOpen: () => failed.push("opened"), onData: () => {}, onExit: () => {}, onFail: (e) => failed.push(e) });
    assert.equal(failed.length, 1, JSON.stringify(env));
    assert.match(failed[0], /not an environment variable main sets/);
    assert.equal(revoke(), false);
    assert.equal(host.open(1, spec, 80, 24).ok, false, "and nothing can open it");
  }
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
  assert.deepEqual(exec.parsePinOutput(0, `${JSON.stringify({ dir: "/r", pin: PIN, gitignores: [] })}\n`, ""), { kind: "pinned", pin: PIN, dir: "/r" });
  assert.deepEqual(exec.parsePinOutput(0, JSON.stringify({ dir: "/r", pin: null }), ""), { kind: "pinned", pin: null, dir: "/r" });
  assert.equal(exec.parsePinOutput(1, JSON.stringify({ dir: "/r", error: { message: "EACCES" } }), "").kind, "error", "a fault refuses nothing");
  assert.deepEqual(exec.parsePinOutput(1, JSON.stringify({ refused: { reason: "audit_refused", message: "m" } }), "Error: m"), { kind: "refused", reason: "audit_refused", message: "m" });
  assert.equal(exec.parsePinOutput(1, JSON.stringify({ pin: PIN }), "boom").kind, "error", "a failed exit is never a pin");
  assert.equal(exec.parsePinOutput(0, "", "").kind, "error");
  assert.equal(exec.refuseReasonFor("unpinnable"), "tree_changed");
});

// ---- the request queue: the preview's pin → diff → pin, the base, Decline ---------------------

const BASE_SHA = "e".repeat(40);
const verified = async () => ({ state: "verified", sha: BASE_SHA, ref: "refs/heads/main", source: "ls-remote", url: "git@h:r.git" });
const shownDiff = (pin, top) => ({ state: "shown", base: BASE_SHA, top, pin, files: [{ status: "M", path: "a.txt" }], filesTruncated: false, patch: "+x\n", truncated: false });

test("the preview pins, diffs and pins again; all three the request's, Run may be clicked", async () => {
  const h = home();
  const diffs = [];
  const { d, runner } = rig({
    h,
    deps: {
      base: verified,
      diff: async (dir, base) => {
        diffs.push([dir, base]);
        return shownDiff(PIN, dir);
      },
    },
  });
  const row = d.add({ dir: h.repo, argv: ["/bin/true"] });
  const p = await runner.preview(row.id);
  assert.equal(p.ok, true);
  assert.deepEqual(p.value.verdict, { state: "match" });
  assert.equal(p.value.problem, undefined);
  assert.equal(p.value.base.state, "verified");
  assert.deepEqual(diffs, [[h.repo, BASE_SHA]], "diffed against the verified base");
  assert.equal(p.value.diff.against, BASE_SHA);
  assert.equal(queue.runBlocked(p.value), undefined);
  assert.equal(h.pinCalls().length, 2, "a pin before the diff and one after");
});

test("a tree that moves during the preview — between the diff and the last pin, or under the diff — disables Run", async () => {
  // Moved after the diff: the last pin differs.
  const h = home();
  let diffed = false;
  const moving = rig({
    h,
    deps: {
      base: verified,
      pin: async () => ({ kind: "pinned", pin: diffed ? OTHER_PIN : PIN, dir: h.repo }),
      diff: async (dir) => {
        diffed = true;
        return shownDiff(PIN, dir);
      },
    },
  });
  const row = moving.d.add({ dir: h.repo, argv: ["/bin/true"] });
  const p = await moving.runner.preview(row.id);
  assert.equal(p.value.verdict.state, "changed");
  assert.equal(p.value.problem, "Tree changed since the request: Run will refuse");
  assert.ok(queue.runBlocked(p.value), "Run is disabled");
  assert.equal(row.state, "pending", "a preview changes nothing");
  // Moved under the diff: the diff's own pin differs, or the diff saw a file change.
  for (const diff of [async (dir) => shownDiff(OTHER_PIN, dir), async () => ({ state: "refused", reason: "tree_changed", message: "a.txt changed" })]) {
    const r = rig({ h: home(), deps: { base: verified, diff } });
    const row2 = r.d.add({ dir: r.h.repo, argv: ["/bin/true"] });
    const p2 = await r.runner.preview(row2.id);
    assert.equal(p2.value.verdict.state, "changed", JSON.stringify(p2.value.diff));
    assert.ok(queue.runBlocked(p2.value));
  }
});

test("with no verified base the diff is against the request's HEAD label, and the preview says base unverified", async () => {
  const h = home();
  const diffs = [];
  const { d, runner } = rig({
    h,
    deps: {
      base: async () => ({ state: "unverified", message: "the network is down" }),
      diff: async (dir, base) => {
        diffs.push(base);
        return shownDiff(PIN, dir);
      },
    },
  });
  const row = d.add({ dir: h.repo, argv: ["/bin/true"] });
  const p = await runner.preview(row.id);
  assert.equal(p.value.base.state, "unverified");
  assert.deepEqual(diffs, [PIN.head]);
  assert.equal(queue.runBlocked(p.value), undefined, "an unverified base warns; it does not block");
});

test("an over-cap diff leaves Run enabled; a request in no repository shows no diff and needs none", async () => {
  const h = home();
  const big = rig({ h, deps: { base: verified, diff: async (dir) => ({ ...shownDiff(PIN, dir), truncated: true }) } });
  const row = big.d.add({ dir: h.repo, argv: ["/bin/true"] });
  const p = await big.runner.preview(row.id);
  assert.equal(p.value.diff.truncated, true);
  assert.equal(queue.runBlocked(p.value), undefined);
  const h2 = home({ answer: { pin: null } });
  let asked = false;
  const none = rig({ h: h2, deps: { base: async () => ((asked = true), verified()), diff: async () => ((asked = true), shownDiff(PIN, "")) } });
  const row2 = none.d.add({ dir: h2.repo, argv: ["/bin/true"], pin: undefined });
  const p2 = await none.runner.preview(row2.id);
  assert.deepEqual(p2.value.verdict, { state: "no_repo" });
  assert.equal(asked, false, "no base and no diff for a directory in no repository");
  assert.equal(queue.runBlocked(p2.value), undefined);
});

test("the program file is shown when argv[0] is a path in the repository, a link as a link", async () => {
  const h = home();
  fs.writeFileSync(path.join(h.repo, "go.sh"), "#!/bin/sh\necho ‮\n", { mode: 0o755 });
  fs.symlinkSync("/etc/passwd", path.join(h.repo, "via-link"));
  const { d, runner } = rig({ h, deps: { base: verified, diff: async (dir) => shownDiff(PIN, dir), readProgram: exec.readProgramFile } });
  const a = d.add({ dir: h.repo, argv: ["./go.sh"] });
  const pa = await runner.preview(a.id);
  assert.equal(pa.value.programFile.kind, "file");
  assert.match(pa.value.programFile.text, /echo/);
  const b = d.add({ dir: h.repo, argv: ["./via-link"] });
  const pb = await runner.preview(b.id);
  assert.deepEqual(pb.value.programFile, { path: path.join(h.repo, "via-link"), kind: "link", target: "/etc/passwd" });
  const c = d.add({ dir: h.repo, argv: ["/bin/true"] });
  assert.equal((await runner.preview(c.id)).value.programFile, undefined, "a program outside the repository is not shown as one");
});

test("execPreview refuses anything but a run request id, before any call", async () => {
  const { d, runner, h } = rig();
  for (const id of ["x", "exec-0", "exec-1; rm", 7, null, { id: "exec-1" }]) {
    const p = await runner.preview(id);
    assert.equal(p.ok, false, String(id));
    assert.match(p.error, /not a run request id/);
  }
  assert.deepEqual(d.calls, []);
  assert.deepEqual(h.pinCalls(), []);
});

test("Decline requires a note; one click per request, Run and Decline alike", () => {
  for (const note of ["", "   ", "\n\t"]) assert.equal(queue.declineOp("exec-1", note).ok, false, JSON.stringify(note));
  assert.deepEqual(queue.declineOp("exec-1", " ask for make check "), { ok: true, request: { op: "exec.decline", id: "exec-1", note: "ask for make check" } });
  assert.equal(queue.declineOp("exec-x", "n").ok, false);
  const gate = new queue.ClickGate();
  assert.equal(gate.take("exec-1"), true);
  assert.equal(gate.take("exec-1"), false, "a second click, or Decline after Run");
  assert.equal(gate.take("exec-2"), true);
  gate.release("exec-1");
  assert.equal(gate.take("exec-1"), true);
});

test("the history lists what left the queue, newest first", () => {
  const rows = ["pending", "finished", "declined", "running", "refused", "expired"].map((state, i) => ({ id: `exec-${i + 1}`, state }));
  assert.deepEqual(queue.history(rows).map((r) => r.id), ["exec-6", "exec-5", "exec-3", "exec-2"]);
});

test("the exec feed hears only exec.changed with an id and a state", () => {
  const t = "exec/requests";
  assert.equal(feed.EXEC_FEED.accepts(t), true);
  assert.equal(feed.EXEC_FEED.accepts("exec/3"), false);
  assert.deepEqual(feed.EXEC_FEED.events(t, "exec.changed", { id: "exec-3", state: "declined" }), [{ topic: t, kind: "changed", id: "exec-3", state: "declined" }]);
  assert.deepEqual(feed.EXEC_FEED.events(t, "exec.changed", { id: "../x", state: "s" }), []);
  assert.deepEqual(feed.EXEC_FEED.events(t, "notify.post", { id: "exec-3", state: "s" }), []);
  assert.equal(feed.EXEC_FEED.leaves, true);
});

test("machineDiff asks the host jkb for the diff with the cap Rust sets, offering the clean clone's objects", async () => {
  const cli = fs.readFileSync(path.join(here, "..", "..", "..", "crates", "jkb-cli", "src", "exec_cli.rs"), "utf8");
  const m = /pub const DIFF_MAX_BYTES: usize = ([\d *]+);/.exec(cli);
  assert.ok(m);
  assert.equal(m[1].split("*").reduce((n, f) => n * Number(f.trim()), 1), exec.DIFF_MAX_BYTES);
  const h = home({ answer: { dir: "/r", top: "/r", base: BASE_SHA, pin: PIN, files: [], files_truncated: false, patch: "", truncated: false } });
  const clone = path.join(h.h, "clone");
  fs.mkdirSync(clone);
  const r = await exec.machineDiff(h.h, update.machineRunner(h.h, process.env), clone)(h.repo, BASE_SHA);
  assert.equal(r.state, "shown");
  assert.deepEqual(h.pinCalls(), [`--json exec diff --dir ${h.repo} --base ${BASE_SHA} --max ${exec.DIFF_MAX_BYTES} --objects-from ${clone}`]);
  const b = await exec.machineBase(h.h, update.machineRunner(h.h, process.env), path.join(h.h, "no-clone"))(h.repo);
  assert.equal(b.state, "unverified", "a diff object is no base");
  assert.equal(h.pinCalls()[1], `--json exec base --dir ${h.repo}`, "no --clone when there is none");
});

test("a request in a repository whose diff could not be produced cannot be run; an unverified base with a diff can", async () => {
  for (const diff of [
    async () => ({ state: "error", message: "the base eee is not in /r: fetch it on the host" }),
    async () => ({ state: "refused", reason: "unpinnable", message: "a nested repository" }),
    undefined,
  ]) {
    const r = rig({ h: home(), deps: { base: verified, ...(diff === undefined ? {} : { diff }) } });
    const row = r.d.add({ dir: r.h.repo, argv: ["/bin/true"] });
    const p = await r.runner.preview(row.id);
    assert.equal(p.ok, true);
    assert.match(p.value.problem, /No diff could be shown/);
    assert.ok(queue.runBlocked(p.value), "Run is disabled by main's verdict");
  }
  const r = rig({ h: home(), deps: { base: async () => ({ state: "unverified", message: "offline" }), diff: async (dir) => shownDiff(PIN, dir) } });
  const row = r.d.add({ dir: r.h.repo, argv: ["/bin/true"] });
  const p = await r.runner.preview(row.id);
  assert.equal(p.value.problem, undefined);
  assert.equal(queue.runBlocked(p.value), undefined);
});
