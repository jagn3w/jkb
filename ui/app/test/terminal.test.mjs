//! The integrated terminal (D53.10) without a window: the spec contract, the command each spec
//! runs, main's PTY host against real PTYs (and a stand-in `docker`), and the renderer's state and
//! event routing.
//
// The modules are bundled with esbuild, as in shell.test.mjs. `node-pty` is the real one: it is a
// Node-API module, so the build that loads in Electron loads here too.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-terminal-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const require = createRequire(import.meta.url);

async function load(entry) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({ entryPoints: [entry], bundle: true, format: "cjs", platform: "node", outfile, logLevel: "silent" });
  return require(outfile);
}

const src = path.join(here, "..", "src");
const shared = await load(path.join(src, "shared", "terminal.ts"));
const main = await load(path.join(src, "main", "terminals.ts"));
const state = await load(path.join(src, "renderer", "src", "terminal", "state.ts"));
const { TerminalEventRouter, HELD_LIMIT_CHARS } = await load(path.join(src, "renderer", "src", "terminal", "router.ts"));
const pty = require("node-pty");

const ROOTS = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: "/Users/me/repos", hostHome: "/Users/me" };
const spec = (over = {}) => ({ target: "container", cwd: "/home/vscode/repos", argv: [], title: "shell", ...over });

// ---- the spec ------------------------------------------------------------------------------

test("a well-formed spec parses, and comes back as a fresh object", () => {
  const input = spec({ argv: ["claude", "--session-id", "x"], sessionUuid: "0f8fad5b-d9cb-469f-a165-70867728950e" });
  const r = shared.parseSpec(input);
  assert.equal(r.ok, true);
  assert.deepEqual(r.value, input);
  assert.notEqual(r.value.argv, input.argv, "argv is copied, so a later change to the caller's array runs nothing");
});

test("a malformed spec is refused with the reason", () => {
  const refused = (value, why) => {
    const r = shared.parseSpec(value);
    assert.equal(r.ok, false, `${JSON.stringify(value)} should be refused`);
    assert.match(r.error, why);
  };
  refused(null, /object/);
  refused([], /object/);
  refused(spec({ target: "vm" }), /target/);
  refused(spec({ cwd: "repos" }), /absolute/);
  refused(spec({ cwd: "/a\0b" }), /absolute/);
  refused(spec({ argv: "bash" }), /array/);
  refused(spec({ argv: ["bash", 1] }), /string/);
  refused(spec({ argv: ["bash", "a\0b"] }), /NUL/);
  refused(spec({ argv: [""] }), /program/);
  refused(spec({ argv: Array(300).fill("x") }), /at most/);
  refused(spec({ argv: ["x".repeat(70 * 1024)] }), /too long/);
  refused(spec({ title: " " }), /title/);
  refused(spec({ title: "t".repeat(201) }), /title/);
  refused(spec({ sessionUuid: "not-a-uuid" }), /UUID/);
  refused({ ...spec(), env: { LD_PRELOAD: "/x" } }, /unknown spec field\(s\): env/);
});

test("sizes are bounded integers", () => {
  assert.equal(shared.isValidSize(80, 24), true);
  assert.equal(shared.isValidSize(80.5, 24), false);
  assert.equal(shared.isValidSize("80", 24), false);
  assert.equal(shared.isValidSize(1, 24), false);
  assert.equal(shared.isValidSize(80, 0), false);
  assert.equal(shared.isValidSize(5000, 24), false);
});

test("the toggle moves a terminal through the repos mount, or to the target's default", () => {
  const inRepo = spec({ cwd: "/home/vscode/repos/jkb/crates", argv: ["make"], title: "build" });
  const host = shared.retarget(inRepo, "host", ROOTS);
  assert.deepEqual(host, { ...inRepo, target: "host", cwd: "/Users/me/repos/jkb/crates" });
  assert.deepEqual(shared.retarget(host, "container", ROOTS), inRepo, "and back");
  assert.equal(shared.retarget(spec({ cwd: "/home/vscode/repos" }), "host", ROOTS).cwd, "/Users/me/repos", "the root itself");
  assert.equal(shared.retarget(spec({ cwd: "/home/vscode/repos-old" }), "host", ROOTS).cwd, "/Users/me", "a sibling is not inside");
  assert.equal(shared.retarget(spec({ cwd: "/tmp" }), "host", ROOTS).cwd, "/Users/me", "outside the mount");
  assert.equal(shared.retarget(spec({ target: "host", cwd: "/etc" }), "container", ROOTS).cwd, "/home/vscode/repos");
  assert.equal(shared.retarget(inRepo, "container", ROOTS), inRepo, "the same target is no change");
  assert.equal(shared.DEFAULT_TARGET, "container");
});

// ---- the command each spec runs ------------------------------------------------------------

function environment(over = {}) {
  return {
    roots: ROOTS,
    docker: () => "/usr/local/bin/docker",
    hostShell: "/bin/zsh",
    env: {},
    isDirectory: () => true,
    ...over,
  };
}

test("a container terminal is docker exec -it -w <cwd> <container> <argv>", () => {
  const r = main.commandFor(spec({ cwd: "/home/vscode/repos/jkb", argv: ["claude", "--resume", "u"] }), environment());
  assert.deepEqual(r, {
    ok: true,
    value: {
      file: "/usr/local/bin/docker",
      args: ["exec", "-i", "-t", "-e", "TERM=xterm-256color", "-e", "COLORTERM=truecolor", "-w", "/home/vscode/repos/jkb", "jkb-dev", "claude", "--resume", "u"],
      cwd: "/Users/me",
    },
  });
  const shell = main.commandFor(spec(), environment());
  assert.deepEqual(shell.value.args.slice(-3), ["jkb-dev", "/bin/bash", "-l"], "no argv runs the image's shell, by absolute path");
});

test("a container terminal without docker, or with a flag for a container name, is refused", () => {
  const none = main.commandFor(spec(), environment({ docker: () => undefined }));
  assert.equal(none.ok, false);
  assert.match(none.error, /docker not found \(looked in \/usr\/local\/bin\/docker/);
  const flag = main.commandFor(spec(), environment({ roots: { ...ROOTS, container: "--privileged" } }));
  assert.equal(flag.ok, false);
  assert.match(flag.error, /not a container name/);
});

test("a host terminal runs its argv in its cwd, or the login shell, and needs the directory", () => {
  assert.deepEqual(main.commandFor(spec({ target: "host", cwd: "/w", argv: ["make", "-j"] }), environment()), {
    ok: true,
    value: { file: "make", args: ["-j"], cwd: "/w" },
  });
  assert.deepEqual(main.commandFor(spec({ target: "host", cwd: "/w" }), environment()).value, { file: "/bin/zsh", args: ["-l"], cwd: "/w" });
  const missing = main.commandFor(spec({ target: "host", cwd: "/gone" }), environment({ isDirectory: () => false }));
  assert.equal(missing.ok, false);
  assert.match(missing.error, /no such directory on the host: \/gone/);
});

test("a terminal's environment drops Electron's variables and sets TERM", () => {
  const env = main.terminalEnv({ PATH: "/bin", ELECTRON_RUN_AS_NODE: "1", ELECTRON_RENDERER_URL: "x", TERM: "dumb", GONE: undefined });
  assert.deepEqual(env, { PATH: "/bin", TERM: "xterm-256color", COLORTERM: "truecolor" });
});

test("docker is looked for by absolute path only", () => {
  for (const p of main.DOCKER_CANDIDATES) assert.ok(p.startsWith("/"), p);
  const machine = main.machineEnvironment(ROOTS, { SHELL: "relative-shell" }, undefined);
  assert.ok(machine.hostShell.startsWith("/"), "a relative $SHELL is not taken");
});

// ---- main's PTY host, with real PTYs --------------------------------------------------------

function host(over = {}) {
  const events = [];
  const waiters = [];
  const emit = (owner, event) => {
    events.push({ owner, ...event });
    for (const w of [...waiters]) w();
  };
  const h = new main.TerminalHost(pty.spawn, environment({ isDirectory: (p) => fs.existsSync(p), env: process.env, ...over }), emit);
  const until = (pred, what) =>
    new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`timed out waiting for ${what}; saw ${JSON.stringify(events)}`)), 10_000);
      const check = () => {
        if (pred(events)) {
          clearTimeout(timer);
          waiters.splice(waiters.indexOf(check), 1);
          resolve(events);
        }
      };
      waiters.push(check);
      check();
    });
  return { h, events, until };
}

const output = (events, id) => events.filter((e) => e.id === id && e.kind === "data").map((e) => e.data).join("");

test("a host terminal runs, its output reaches its owner, and its exit code is reported", async () => {
  const { h, events, until } = host();
  const r = h.open(7, spec({ target: "host", cwd: work, argv: ["/bin/sh", "-c", "printf 'hi from %s' \"$(pwd)\"; exit 3"] }), 80, 24);
  assert.equal(r.ok, true, r.error);
  const id = r.value.id;
  await until((ev) => ev.some((e) => e.id === id && e.kind === "exit"), "the exit");
  assert.match(output(events, id), new RegExp(`hi from ${fs.realpathSync(work)}`));
  const exit = events.find((e) => e.kind === "exit");
  assert.equal(exit.exitCode, 3);
  assert.ok(events.every((e) => e.owner === 7), "only the owner hears it");
  assert.ok(events.indexOf(exit) === events.length - 1, "the exit comes after all the output");
  assert.deepEqual(h.list(7), [], "an exited terminal is gone");
});

test("only the owner can type into, resize or close a terminal", async () => {
  const { h, events, until } = host();
  const r = h.open(1, spec({ target: "host", cwd: work, argv: ["/bin/cat"] }), 80, 24);
  const id = r.value.id;
  assert.equal(h.write(2, id, "intruder\n"), false);
  assert.equal(h.resize(2, id, 100, 30), false);
  assert.equal(h.close(2, id), false);
  assert.equal(h.write(1, id, 42), false, "data must be a string");
  assert.equal(h.write(1, id, "x".repeat(shared.MAX_WRITE_CHARS + 1)), false, "and bounded");
  assert.equal(h.resize(1, id, 0, 0), false, "and a size must be one");
  assert.equal(h.write(1, String(id), "x"), false, "an id is a number");
  assert.equal(h.write(1, id, "echoed\n"), true);
  assert.equal(h.resize(1, id, 100, 30), true);
  await until((ev) => output(ev, id).includes("echoed"), "the echo");
  assert.ok(!output(events, id).includes("intruder"));
  assert.equal(h.close(1, id), true);
  await until((ev) => ev.some((e) => e.id === id && e.kind === "exit"), "the exit after close");
});

test("closing a window ends its terminals and no one else's", async () => {
  const { h, until } = host();
  const a = h.open(1, spec({ target: "host", cwd: work, argv: ["/bin/cat"] }), 80, 24).value.id;
  const b = h.open(2, spec({ target: "host", cwd: work, argv: ["/bin/cat"] }), 80, 24).value.id;
  h.closeAll(1);
  assert.deepEqual(h.list(1), []);
  assert.deepEqual(h.list(2).map((t) => t.id), [b]);
  assert.equal(h.write(1, a, "x"), false, "a closed window's terminal takes nothing");
  h.closeAll();
  assert.deepEqual(h.list(2), []);
  await until((ev) => ev.some((e) => e.id === b && e.kind === "exit"), "b's exit");
});

test("a bad spec, size or missing directory starts nothing", () => {
  let spawned = 0;
  const h = new main.TerminalHost(() => spawned++, environment({ isDirectory: () => false }), () => {});
  assert.match(h.open(1, { target: "host" }, 80, 24).error, /cwd/);
  assert.match(h.open(1, spec(), 0, 24).error, /size/);
  assert.match(h.open(1, spec({ target: "host" }), 80, 24).error, /no such directory/);
  assert.equal(spawned, 0);
});

test("a spawn that throws is reported, not raised", () => {
  const h = new main.TerminalHost(
    () => {
      throw new Error("forkpty failed");
    },
    environment(),
    () => {},
  );
  const r = h.open(1, spec(), 80, 24);
  assert.equal(r.ok, false);
  assert.match(r.error, /could not start \/usr\/local\/bin\/docker: forkpty failed/);
});

test("a container terminal runs docker exec with the spec's cwd, container and argv", async () => {
  // A stand-in docker that prints the arguments it was given, one per line.
  const docker = path.join(work, "docker");
  fs.writeFileSync(docker, "#!/bin/sh\nfor a in \"$@\"; do printf '<%s>\\n' \"$a\"; done\n", { mode: 0o755 });
  const { h, events, until } = host({ docker: () => docker, roots: { ...ROOTS, hostHome: work } });
  const id = h.open(3, spec({ cwd: "/home/vscode/repos/jkb", argv: ["echo", "two words"] }), 80, 24).value.id;
  await until((ev) => ev.some((e) => e.id === id && e.kind === "exit"), "the exit");
  const args = output(events, id).match(/<[^>]*>/g);
  assert.deepEqual(args, [
    "<exec>", "<-i>", "<-t>", "<-e>", "<TERM=xterm-256color>", "<-e>", "<COLORTERM=truecolor>",
    "<-w>", "</home/vscode/repos/jkb>", "<jkb-dev>", "<echo>", "<two words>",
  ]);
});

test("output is gathered into few messages", async () => {
  const { h, events, until } = host();
  const id = h.open(1, spec({ target: "host", cwd: work, argv: ["/bin/sh", "-c", "i=0; while [ $i -lt 2000 ]; do echo line$i; i=$((i+1)); done"] }), 80, 24).value.id;
  await until((ev) => ev.some((e) => e.id === id && e.kind === "exit"), "the exit");
  const text = output(events, id);
  assert.match(text, /line1999/);
  const messages = events.filter((e) => e.kind === "data").length;
  // Measured: 2-3 messages gathered, 469 when every PTY read is sent as it comes.
  assert.ok(messages < 50, `2000 lines arrived in ${messages} messages`);
});

// ---- the renderer's state -------------------------------------------------------------------

const S = state;

test("opening a terminal puts it in front and opens the drawer", () => {
  let s = S.reduce(S.INITIAL_STATE, { type: "open", key: 1, spec: spec(), placement: "drawer" });
  s = S.reduce(s, { type: "open", key: 2, spec: spec({ title: "two" }), placement: "drawer" });
  assert.equal(s.active, 2);
  assert.equal(s.drawerOpen, true);
  assert.deepEqual(s.entries.map((e) => e.status.kind), ["starting", "starting"]);
  s = S.reduce(s, { type: "status", key: 1, status: { kind: "exited", exitCode: 1 } });
  assert.equal(S.statusLabel(s.entries[0].status), "exit 1");
  assert.equal(S.statusLabel({ kind: "running" }), "");
});

test("closing the front terminal brings its neighbour forward, and the last one folds the drawer", () => {
  let s = S.INITIAL_STATE;
  for (const key of [1, 2, 3]) s = S.reduce(s, { type: "open", key, spec: spec(), placement: "drawer" });
  s = S.reduce(s, { type: "select", key: 2 });
  s = S.reduce(s, { type: "close", key: 2 });
  assert.equal(s.active, 3, "the one after");
  s = S.reduce(s, { type: "close", key: 3 });
  assert.equal(s.active, 1, "else the one before");
  s = S.reduce(s, { type: "close", key: 1 });
  assert.equal(s.active, undefined);
  assert.equal(s.drawerOpen, false);
});

test("the popover holds one terminal; a second moves the first to the drawer; it can be moved there", () => {
  let s = S.reduce(S.INITIAL_STATE, { type: "open", key: 1, spec: spec(), placement: "popover" });
  assert.equal(s.popover, 1);
  assert.equal(s.drawerOpen, false, "a popover does not open the drawer");
  assert.deepEqual(S.drawerEntries(s), []);
  s = S.reduce(s, { type: "select", key: 1 });
  assert.equal(s.active, undefined, "a popover terminal is not a drawer tab");
  s = S.reduce(s, { type: "open", key: 2, spec: spec(), placement: "popover" });
  assert.equal(s.popover, 2);
  assert.deepEqual(S.drawerEntries(s).map((e) => e.key), [1], "the first is not lost");
  s = S.reduce(s, { type: "toDrawer", key: 2 });
  assert.equal(s.popover, undefined);
  assert.equal(s.active, 2);
  assert.equal(s.drawerOpen, true);
  assert.deepEqual(S.drawerEntries(s).map((e) => e.key), [1, 2]);
});

test("a restart keeps the tab and takes the new spec; a session uuid finds its terminal", () => {
  const uuid = "0f8fad5b-d9cb-469f-a165-70867728950e";
  let s = S.reduce(S.INITIAL_STATE, { type: "open", key: 1, spec: spec({ sessionUuid: uuid }), placement: "drawer" });
  s = S.reduce(s, { type: "status", key: 1, status: { kind: "running" } });
  s = S.reduce(s, { type: "restart", key: 1, spec: spec({ target: "host", cwd: "/Users/me", sessionUuid: uuid }) });
  assert.equal(s.entries[0].spec.target, "host");
  assert.equal(s.entries[0].status.kind, "starting");
  assert.equal(S.findSession(s, uuid)?.key, 1);
  assert.equal(S.findSession(s, undefined), undefined);
  assert.equal(S.findSession(s, "0f8fad5b-d9cb-469f-a165-000000000000"), undefined);
});

test("the drawer's height is clamped to the window", () => {
  assert.equal(S.clampHeight(50, 1000), S.DRAWER_HEIGHT.min);
  assert.equal(S.clampHeight(900, 1000), 800);
  assert.equal(S.clampHeight(Number.NaN, 1000), S.DRAWER_HEIGHT.default);
  assert.equal(S.clampHeight(300, Number.NaN), 300, "no window size is no upper bound");
});

// ---- event routing --------------------------------------------------------------------------

function router() {
  let emit;
  const unsub = { called: false };
  const r = new TerminalEventRouter((listener) => {
    emit = listener;
    return () => (unsub.called = true);
  });
  return { r, emit: (e) => emit(e), unsub };
}

test("output that arrives before its terminal is claimed is held, then delivered in order", () => {
  const { r, emit } = router();
  emit({ id: 5, kind: "data", data: "a" });
  emit({ id: 5, kind: "data", data: "b" });
  const got = [];
  r.claim(5, (e) => got.push(e.kind === "data" ? e.data : "exit"));
  emit({ id: 5, kind: "data", data: "c" });
  emit({ id: 5, kind: "exit", exitCode: 0 });
  emit({ id: 5, kind: "data", data: "after exit" });
  assert.deepEqual(got, ["a", "b", "c", "exit"]);
});

test("a closed terminal's late output and exit are dropped, not held", () => {
  const { r, emit } = router();
  const got = [];
  r.claim(1, (e) => got.push(e));
  r.retire(1);
  emit({ id: 1, kind: "data", data: "late" });
  emit({ id: 1, kind: "exit", exitCode: 0 });
  r.claim(1, (e) => got.push(e));
  assert.deepEqual(got, [], "nothing was held for it");
  // Retired before it was ever claimed (closed while starting): the same.
  emit({ id: 2, kind: "data", data: "early" });
  r.retire(2);
  emit({ id: 2, kind: "exit", exitCode: 0 });
  r.claim(2, (e) => got.push(e));
  assert.deepEqual(got, []);
});

test("what is held for unclaimed terminals is bounded", () => {
  const { r, emit, unsub } = router();
  const chunk = "x".repeat(HELD_LIMIT_CHARS / 2);
  emit({ id: 1, kind: "data", data: chunk });
  emit({ id: 2, kind: "data", data: chunk });
  emit({ id: 3, kind: "data", data: chunk });
  const got = [];
  for (const id of [1, 2, 3]) r.claim(id, (e) => got.push(e.id));
  assert.deepEqual(got, [2, 3], "the oldest went first");
  r.dispose();
  assert.equal(unsub.called, true);
});
