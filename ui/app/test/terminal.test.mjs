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
const theme = await load(path.join(src, "renderer", "src", "terminal", "theme.ts"));
const pty = require("node-pty");

const ROOTS = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: "/Users/me/repos", hostReposReal: "/Users/me/repos", hostHome: "/Users/me" };
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

test("the toggle maps a host cwd reached through either spelling of a symlinked repos root", () => {
  // ~/repos is a link to /Volumes/dev/repos; git answers the resolved spelling.
  const roots = { ...ROOTS, hostRepos: "/Users/me/repos", hostReposReal: "/Volumes/dev/repos" };
  const viaReal = spec({ target: "host", cwd: "/Volumes/dev/repos/jkb/crates", argv: ["make"], title: "build" });
  assert.equal(shared.retarget(viaReal, "container", roots).cwd, "/home/vscode/repos/jkb/crates", "the resolved spelling");
  assert.equal(shared.retarget({ ...viaReal, cwd: "/Users/me/repos/jkb" }, "container", roots).cwd, "/home/vscode/repos/jkb", "the literal one");
  assert.equal(shared.retarget({ ...viaReal, cwd: "/Volumes/dev/repos" }, "container", roots).cwd, "/home/vscode/repos", "the root itself");
  assert.equal(shared.retarget({ ...viaReal, cwd: "/Volumes/dev/repos-old" }, "container", roots).cwd, "/home/vscode/repos", "a sibling falls back");
  assert.equal(shared.containerPathOf("/Volumes/dev/repos/x", roots), "/home/vscode/repos/x");
  assert.equal(shared.containerPathOf("/Volumes/dev/other", roots), undefined);
  assert.equal(shared.hostPathOf("/Volumes/dev/repos/jkb", roots), "/Users/me/repos/jkb", "re-spelled under hostRepos");
  assert.equal(shared.retarget(spec({ cwd: "/home/vscode/repos/jkb" }), "host", roots).cwd, "/Users/me/repos/jkb", "back to the literal");
});

test("main resolves the repos root's link once, and keeps the spelling", () => {
  const home = path.join(work, "home-link");
  const real = path.join(work, "real-repos");
  fs.mkdirSync(home, { recursive: true });
  fs.mkdirSync(real, { recursive: true });
  fs.symlinkSync(real, path.join(home, "repos"));
  const roots = main.machineRoots(home, { JKB_CONTAINER_NAME: " jkb-alt " });
  assert.equal(roots.hostRepos, path.join(home, "repos"));
  assert.equal(roots.hostReposReal, fs.realpathSync(real));
  assert.equal(roots.container, "jkb-alt");
  assert.equal(shared.retarget(spec({ target: "host", cwd: path.join(fs.realpathSync(real), "jkb") }), "container", roots).cwd, "/home/vscode/repos/jkb");
  const bare = main.machineRoots(path.join(work, "no-such-home"), {});
  assert.equal(bare.hostReposReal, bare.hostRepos, "a root that does not exist yet is only its spelling");
  assert.equal(bare.container, "jkb-dev");
});

test("a paste is chunked without splitting a surrogate pair", () => {
  const emoji = "\u{1F600}"; // two UTF-16 units
  const text = "a".repeat(9) + emoji + "b".repeat(5); // the pair straddles index 9/10
  const chunks = shared.chunkWrite(text, 10);
  assert.equal(chunks.join(""), text, "nothing lost or added");
  for (const c of chunks) {
    assert.ok(c.length <= 10);
    assert.ok(c.isWellFormed(), `${JSON.stringify(c)} holds a lone surrogate`);
  }
  assert.deepEqual(shared.chunkWrite("", 10), []);
  assert.deepEqual(shared.chunkWrite("abc", 10), ["abc"]);
  assert.deepEqual(shared.chunkWrite(emoji, 1).join(""), emoji, "a limit below a pair still makes progress");
  const big = "x".repeat(shared.MAX_WRITE_CHARS - 1) + emoji + "y";
  assert.ok(shared.chunkWrite(big).every((c) => c.isWellFormed() && c.length <= shared.MAX_WRITE_CHARS));
});

test("argv is quoted so a POSIX shell reads the same words", () => {
  assert.equal(shared.formatArgv(["claude", "--session-id", "abc-1"]), "claude --session-id abc-1");
  assert.equal(shared.formatArgv(["it's", "a b", "$HOME", ""]), `'it'\\''s' 'a b' '$HOME' ''`);
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
    run: main.runToEnd(process.env),
    containerRunDir: path.join(work, "run"),
    ...over,
  };
}

const TAG = "t-0123";

test("a container terminal is docker exec -it -w <cwd> <container> <wrapper> <argv>, with its end", () => {
  const r = main.commandFor(spec({ cwd: "/home/vscode/repos/jkb", argv: ["claude", "--resume", "u"] }), environment({ containerRunDir: "/tmp/jkb-terminals" }), TAG);
  assert.deepEqual(r, {
    ok: true,
    value: {
      file: "/usr/local/bin/docker",
      args: [
        "exec", "-i", "-t", "-e", "TERM=xterm-256color", "-e", "COLORTERM=truecolor", "-w", "/home/vscode/repos/jkb", "jkb-dev",
        "/bin/sh", "-c", main.WRAPPER_SCRIPT, "jkb-terminal", "/tmp/jkb-terminals/t-0123.pid", "claude", "--resume", "u",
      ],
      cwd: "/Users/me",
      end: {
        file: "/usr/local/bin/docker",
        args: ["exec", "jkb-dev", "/bin/sh", "-c", main.END_SCRIPT, "jkb-terminal-end", "/tmp/jkb-terminals/t-0123.pid"],
      },
    },
  });
  const shell = main.commandFor(spec(), environment(), TAG);
  assert.deepEqual(shell.value.args.slice(-2), ["/bin/bash", "-l"], "no argv runs the image's shell, by absolute path");
  assert.equal(main.commandFor(spec(), environment(), "../x").ok, false, "a tag is never a path");
});

test("a container terminal without docker, or with a flag for a container name, is refused", () => {
  const none = main.commandFor(spec(), environment({ docker: () => undefined }), TAG);
  assert.equal(none.ok, false);
  assert.match(none.error, /docker not found \(looked in \/usr\/local\/bin\/docker/);
  const flag = main.commandFor(spec(), environment({ roots: { ...ROOTS, container: "--privileged" } }), TAG);
  assert.equal(flag.ok, false);
  assert.match(flag.error, /not a container name/);
});

test("a host terminal runs its argv in its cwd (a bare name through the login shell), or the login shell, and needs the directory", () => {
  assert.deepEqual(main.commandFor(spec({ target: "host", cwd: "/w", argv: ["/usr/bin/make", "-j"] }), environment(), TAG), {
    ok: true,
    value: { file: "/usr/bin/make", args: ["-j"], cwd: "/w" },
  });
  assert.deepEqual(main.commandFor(spec({ target: "host", cwd: "/w", argv: ["claude", "--session-id", "it's"] }), environment(), TAG), {
    ok: true,
    value: { file: "/bin/zsh", args: ["-l", "-c", `exec claude --session-id 'it'\\''s'`], cwd: "/w" },
  });
  assert.equal(main.hostArgv(["claude"], "/opt/homebrew/bin/fish").file, "/bin/sh", "a shell without POSIX quoting is not trusted with the words");
  assert.deepEqual(main.commandFor(spec({ target: "host", cwd: "/w" }), environment(), TAG).value, { file: "/bin/zsh", args: ["-l"], cwd: "/w" });
  const missing = main.commandFor(spec({ target: "host", cwd: "/gone" }), environment({ isDirectory: () => false }), TAG);
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
  const h = new main.TerminalHost(
    pty.spawn,
    environment({ isDirectory: (p) => fs.existsSync(p), env: process.env, ...over }),
    emit,
    async () => true,
  );
  // Host programs need the person's yes first; these tests give it.
  const open = h.open.bind(h);
  h.open = (owner, s, cols, rows) => {
    if (s?.target === "host" && s.argv?.length > 0) h.approveHost(owner, s);
    return open(owner, s, cols, rows);
  };
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
  assert.equal((await h.close(2, id)).ok, false);
  assert.equal(h.ack(2, id, 10), false);
  assert.equal(h.write(1, id, 42), false, "data must be a string");
  assert.equal(h.write(1, id, "x".repeat(shared.MAX_WRITE_CHARS + 1)), false, "and bounded");
  assert.equal(h.resize(1, id, 0, 0), false, "and a size must be one");
  assert.equal(h.write(1, String(id), "x"), false, "an id is a number");
  assert.equal(h.write(1, id, "echoed\n"), true);
  assert.equal(h.resize(1, id, 100, 30), true);
  await until((ev) => output(ev, id).includes("echoed"), "the echo");
  assert.ok(!output(events, id).includes("intruder"));
  const ended = await h.close(1, id);
  assert.deepEqual(ended, { ok: true, value: { target: "host", confirmed: true, detail: "the program ended" } });
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

/**
 * A stand-in `docker` that runs the command on this machine, the way the real one runs it in the
 * container: `exec` with `-t` runs it in a NEW SESSION (`setsid`), so ending the client is not
 * ending it — the premise of the in-container end, which is unmeasured against real Docker
 * (D53.10). Every invocation is logged, one argument per line.
 */
function fakeDocker(name, { endFails = false } = {}) {
  const docker = path.join(work, name);
  const log = path.join(work, `${name}.log`);
  fs.writeFileSync(
    docker,
    `#!/bin/sh
for a in "$@"; do printf '<%s>\\n' "$a"; done >>'${log}'
echo "--" >>'${log}'
[ "$1" = exec ] || exit 2; shift
tty=no
while :; do
  case "$1" in
    -t) tty=yes; shift ;;
    -i) shift ;;
    -e|-w) shift 2 ;;
    *) break ;;
  esac
done
shift # the container
${endFails ? '[ "$tty" = no ] && { echo "Error response from daemon: container is not running" >&2; exit 1; }' : ""}
if [ "$tty" = yes ]; then exec setsid -w "$@"; else exec "$@"; fi
`,
    { mode: 0o755 },
  );
  return { docker, log: () => (fs.existsSync(log) ? fs.readFileSync(log, "utf8") : "") };
}

/** Whether `pid` runs: exists and is not a zombie waiting to be reaped (a reparented one may wait). */
const alive = (pid) => {
  try {
    process.kill(pid, 0);
  } catch {
    return false;
  }
  try {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
    return !stat.slice(stat.lastIndexOf(")") + 2).startsWith("Z");
  } catch {
    return true;
  }
};

/** Wait for `pred` by polling: for what no terminal event announces (a process ending elsewhere). */
async function poll(pred, what, ms = 10_000) {
  const deadline = Date.now() + ms;
  while (!pred()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
}

const hasSetsid = fs.existsSync("/usr/bin/setsid") || fs.existsSync("/bin/setsid");

test("a container terminal runs docker exec with the spec's cwd, container, the wrapper and argv", { skip: !hasSetsid && "no setsid" }, async () => {
  const { docker, log } = fakeDocker("docker-args");
  const runDir = path.join(work, "run-args");
  const { h, events, until } = host({ docker: () => docker, roots: { ...ROOTS, hostHome: work }, containerRunDir: runDir });
  const id = h.open(3, spec({ cwd: "/home/vscode/repos/jkb", argv: ["/bin/echo", "two words"] }), 80, 24).value.id;
  await until((ev) => ev.some((e) => e.id === id && e.kind === "exit"), "the exit");
  assert.match(output(events, id), /two words/, "the program ran under the wrapper");
  const args = log().split("--")[0].match(/<[^>]*>/g);
  const lines = log().split("--\n")[0].trimEnd().split("\n");
  const record = path.join(runDir, fs.readdirSync(runDir)[0]);
  assert.deepEqual(lines, [
    "<exec>", "<-i>", "<-t>", "<-e>", "<TERM=xterm-256color>", "<-e>", "<COLORTERM=truecolor>",
    "<-w>", "</home/vscode/repos/jkb>", "<jkb-dev>", "</bin/sh>", "<-c>", `<${main.WRAPPER_SCRIPT}>`, "<jkb-terminal>", `<${record}>`,
    "</bin/echo>", "<two words>",
  ]);
});

test("closing a container terminal ends its program inside the container, and says so only when it did", { skip: !hasSetsid && "no setsid" }, async () => {
  const { docker, log } = fakeDocker("docker-end");
  const runDir = path.join(work, "run-end");
  const { h, events, until } = host({ docker: () => docker, roots: { ...ROOTS, hostHome: work }, containerRunDir: runDir });
  // An interactive shell (which ignores TERM) with a job of its own, as `bash -l` with a build running.
  const id = h.open(4, spec({ argv: ["/bin/bash", "--norc", "--noprofile", "-i"] }), 80, 24).value.id;
  h.write(4, id, "sleep 300 & echo CHILD=$!\n");
  await until((ev) => /CHILD=\d+/.test(output(ev, id)), "the job's pid");
  const child = Number(output(events, id).match(/CHILD=(\d+)/)[1]);
  const record = path.join(runDir, fs.readdirSync(runDir)[0]);
  const leader = Number(fs.readFileSync(record, "utf8"));
  assert.ok(alive(leader) && alive(child));
  const ended = await h.close(4, id);
  assert.deepEqual(ended, { ok: true, value: { target: "container", confirmed: true, detail: "the program in the container ended" } });
  assert.ok(!alive(leader), "the shell is gone");
  assert.match(log(), /<jkb-terminal-end>/, "ended by a second docker exec");
  await poll(() => !alive(child), "the job to go with its shell");
  assert.ok(!fs.existsSync(record), "its record is removed");
});

test("a container program whose end is not acknowledged is reported as possibly still running", { skip: !hasSetsid && "no setsid" }, async () => {
  const { docker } = fakeDocker("docker-noend", { endFails: true });
  const runDir = path.join(work, "run-noend");
  const { h, events, until } = host({ docker: () => docker, roots: { ...ROOTS, hostHome: work }, containerRunDir: runDir });
  const id = h.open(5, spec({ argv: ["/bin/sh", "-c", "echo UP; exec sleep 300"] }), 80, 24).value.id;
  await until((ev) => output(ev, id).includes("UP"), "the program");
  const leader = Number(fs.readFileSync(path.join(runDir, fs.readdirSync(runDir)[0]), "utf8"));
  const ended = await h.close(5, id);
  assert.equal(ended.ok, true);
  assert.equal(ended.value.confirmed, false);
  assert.match(ended.value.detail, /could not confirm the program in the container ended \(exit 1: Error response from daemon: container is not running\); it may still be running/);
  // Ending the client alone did not end it: the case the explicit end exists for.
  assert.ok(alive(leader), "the program outlived its docker client");
  process.kill(-leader, "SIGKILL");
});

test("closing a window sends every container program its end", { skip: !hasSetsid && "no setsid" }, async () => {
  const { docker, log } = fakeDocker("docker-closeall");
  const runDir = path.join(work, "run-closeall");
  const { h, events, until } = host({ docker: () => docker, roots: { ...ROOTS, hostHome: work }, containerRunDir: runDir });
  const id = h.open(6, spec({ argv: ["/bin/sh", "-c", "echo UP; exec sleep 300"] }), 80, 24).value.id;
  await until((ev) => output(ev, id).includes("UP"), "the program");
  const leader = Number(fs.readFileSync(path.join(runDir, fs.readdirSync(runDir)[0]), "utf8"));
  h.closeAll(6);
  await poll(() => !alive(leader), "the program to end");
  assert.match(log(), /<jkb-terminal-end>/);
});

test("a host program runs only once main's own confirmation said yes, for exactly that argv and cwd", async () => {
  const asked = [];
  let answer = false;
  const spawned = [];
  const h = new main.TerminalHost(
    (file, args) => {
      spawned.push([file, ...args]);
      return { pid: 1, onData: () => ({ dispose() {} }), onExit: () => ({ dispose() {} }), write() {}, resize() {}, kill() {}, pause() {}, resume() {} };
    },
    environment(),
    () => {},
    async (owner, prompt) => {
      asked.push({ owner, ...prompt });
      return answer;
    },
  );
  const prog = spec({ target: "host", cwd: "/w", argv: ["claude", "--resume", "it's"], title: "claude" });
  assert.match(h.open(1, prog, 80, 24).error, /needs your confirmation/, "no renderer call runs it unasked");
  assert.deepEqual(await h.confirmHost(1, prog), { ok: true, value: false });
  assert.deepEqual(asked, [{ owner: 1, argv: ["claude", "--resume", "it's"], cwd: "/w", command: `claude --resume 'it'\\''s'`, title: "claude" }], "the exact argv and cwd");
  assert.equal(h.open(1, prog, 80, 24).ok, false, "a no is a no");
  answer = true;
  assert.deepEqual(await h.confirmHost(1, prog), { ok: true, value: true });
  assert.equal(h.open(1, prog, 80, 24).ok, true);
  assert.equal(h.open(1, prog, 80, 24).ok, true, "a restart does not ask again");
  assert.equal(h.open(2, prog, 80, 24).ok, false, "only for the window that said yes");
  assert.equal(h.open(1, { ...prog, cwd: "/elsewhere" }, 80, 24).ok, false, "only for that cwd");
  assert.equal(h.open(1, { ...prog, argv: ["claude", "--dangerously-skip-permissions"] }, 80, 24).ok, false, "only for that argv");
  assert.equal(h.open(1, spec({ target: "host", cwd: "/w" }), 80, 24).ok, true, "the login shell runs nothing by itself");
  assert.equal(h.open(1, spec(), 80, 24).ok, true, "a container program is not asked about");
  assert.equal(asked.length, 2);
  assert.deepEqual(await h.confirmHost(1, spec()), { ok: true, value: true }, "nothing to ask for a container spec");
  assert.equal(asked.length, 2);
  h.closeAll(1);
  assert.equal(h.open(1, prog, 80, 24).ok, false, "a reload forgets the yes");
  assert.equal((await h.confirmHost(1, { ...prog, env: {} })).ok, false, "the spec is validated first");
  const k = spec({ target: "host", cwd: "/kit", argv: ["/kit/run.sh", "--status"] });
  h.approveHost(1, k);
  assert.equal(h.open(1, k, 80, 24).ok, true, "a spec main built itself runs without asking");
});

test("a bare program name on the host is found through the login shell, its words intact", async () => {
  const bin = path.join(work, "bin");
  fs.mkdirSync(bin, { recursive: true });
  fs.writeFileSync(path.join(bin, "jkb-words"), '#!/bin/sh\nfor a in "$@"; do printf "<%s>" "$a"; done; echo\n', { mode: 0o755 });
  // The login shell's PATH is where the program is; the app's own PATH (launchd's) does not have it.
  const profileHome = path.join(work, "profile-home");
  fs.mkdirSync(profileHome, { recursive: true });
  fs.writeFileSync(path.join(profileHome, ".profile"), `PATH="${bin}:$PATH"; export PATH\n`);
  const { h, events, until } = host({ hostShell: "/bin/sh", env: { HOME: profileHome, PATH: "/usr/bin:/bin" } });
  const words = ["it's", "a b", "$HOME", "x\\", "*", ""];
  const id = h.open(1, spec({ target: "host", cwd: work, argv: ["jkb-words", ...words] }), 200, 24).value.id;
  await until((ev) => ev.some((e) => e.id === id && e.kind === "exit"), "the exit");
  assert.match(output(events, id), /<it's><a b><\$HOME><x\\><\*><>/);
  assert.equal(events.find((e) => e.kind === "exit").exitCode, 0);
});

// ---- flow control ----------------------------------------------------------------------------

function fakePty() {
  const p = { pid: 1, paused: 0, resumed: 0, data: undefined };
  return Object.assign(p, {
    onData: (l) => ((p.data = l), { dispose() {} }),
    onExit: () => ({ dispose() {} }),
    write() {},
    resize() {},
    kill() {},
    pause: () => p.paused++,
    resume: () => p.resumed++,
  });
}

test("main stops reading a PTY past the high watermark and resumes below the low one", () => {
  const p = fakePty();
  const sent = [];
  const h = new main.TerminalHost(() => p, environment(), (_o, e) => sent.push(e));
  const id = h.open(1, spec(), 80, 24).value.id;
  p.data("x".repeat(main.FLUSH_BYTES)); // sent at once
  assert.equal(p.paused, 1, "more than FLOW.high unacknowledged pauses it");
  assert.equal(h.isPaused(id), true);
  p.data("x".repeat(main.FLUSH_BYTES));
  assert.equal(p.paused, 1, "paused once");
  assert.equal(h.ack(1, id, 0), false, "an ack is a positive count");
  assert.equal(h.ack(1, id, 1.5), false);
  assert.equal(h.ack(1, id, "5"), false);
  assert.equal(h.ack(1, id, 2 * main.FLUSH_BYTES - shared.FLOW.low), true);
  assert.equal(p.resumed, 0, "at the low watermark it stays paused");
  assert.equal(h.ack(1, id, 1), true);
  assert.equal(p.resumed, 1, "below it, it is read again");
  assert.equal(h.isPaused(id), false);
  assert.ok(shared.FLOW.ackBatch < shared.FLOW.low, "what is drawn but not yet acknowledged cannot hold it paused");
});

test("a fast producer is held near the high watermark until its output is acknowledged", async () => {
  const { h, events, until } = host();
  const id = h.open(1, spec({ target: "host", cwd: work, argv: ["/usr/bin/yes", "0123456789"] }), 80, 24).value.id;
  const sentChars = () => events.filter((e) => e.id === id && e.kind === "data").reduce((n, e) => n + e.data.length, 0);
  await poll(() => h.isPaused(id), "the pause");
  await new Promise((r) => setTimeout(r, 300));
  const held = sentChars();
  // Bounded: the watermark, one flush, and what the PTY had already read.
  assert.ok(held < shared.FLOW.high + main.FLUSH_BYTES + 128 * 1024, `${held} chars sent while unacknowledged`);
  await new Promise((r) => setTimeout(r, 300));
  assert.equal(sentChars(), held, "nothing more while paused");
  h.ack(1, id, held);
  await until(() => sentChars() > held, "more output once it is drawn");
  await h.close(1, id);
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

test("opening a session that is still running shows it; one whose program ended runs the new spec in its tab", () => {
  const uuid = "0f8fad5b-d9cb-469f-a165-70867728950e";
  const launch = spec({ argv: ["claude", "--session-id", uuid], title: "Play", sessionUuid: uuid });
  const resume = spec({ argv: ["/bin/bash", "-lc", "resume", "claude", uuid], title: "Resume", sessionUuid: uuid });
  let s = S.reduce(S.INITIAL_STATE, { type: "open", key: 1, spec: launch, placement: "drawer" });
  assert.equal(S.planOpen(s, resume).kind, "show", "starting");
  s = S.reduce(s, { type: "status", key: 1, status: { kind: "running" } });
  assert.equal(S.planOpen(s, resume).kind, "show", "running");
  s = S.reduce(s, { type: "status", key: 1, status: { kind: "exited", exitCode: 0 } });
  const plan = S.planOpen(s, resume);
  assert.equal(plan.kind, "relaunch", "an exited launch does not swallow the resume");
  assert.equal(plan.entry.key, 1);
  // What the provider does with a relaunch: the tab takes the resume spec, and that is what restarts.
  s = S.reduce(s, { type: "restart", key: plan.entry.key, spec: resume });
  assert.deepEqual(s.entries[0].spec.argv, resume.argv, "the resume argv runs");
  assert.equal(s.entries.length, 1);
  s = S.reduce(s, { type: "status", key: 1, status: { kind: "failed", error: "x" } });
  assert.equal(S.planOpen(s, launch).kind, "relaunch", "a failed start too");
  assert.equal(S.planOpen(s, spec()).kind, "new", "no uuid is always new");
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

test("held output dropped unseen is reported, so main can count it as drawn", () => {
  let emit;
  const dropped = [];
  const r = new TerminalEventRouter(
    (listener) => ((emit = listener), () => {}),
    (id, chars) => dropped.push([id, chars]),
  );
  const chunk = "x".repeat(HELD_LIMIT_CHARS / 2);
  emit({ id: 1, kind: "data", data: chunk });
  emit({ id: 2, kind: "data", data: chunk });
  emit({ id: 3, kind: "data", data: "y" });
  assert.deepEqual(dropped, [[1, chunk.length]]);
  r.dispose();
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

// ---- the theme ---------------------------------------------------------------------------------

const tokensCss = fs.readFileSync(path.join(src, "renderer", "src", "styles", "tokens.css"), "utf8");

/** The custom properties set in `css`'s light :root block and in its dark one. */
function schemes() {
  const darkAt = tokensCss.indexOf("@media (prefers-color-scheme: dark)");
  const parse = (text) => Object.fromEntries([...text.matchAll(/(--[a-z0-9-]+):\s*([^;]+);/g)].map((m) => [m[1], m[2].trim()]));
  return { light: parse(tokensCss.slice(0, darkAt)), dark: { ...parse(tokensCss.slice(0, darkAt)), ...parse(tokensCss.slice(darkAt)) } };
}

function contrast(a, b) {
  const lum = (hex) => {
    const [r, g, bl] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255).map((c) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
    return 0.2126 * r + 0.7152 * g + 0.0722 * bl;
  };
  const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p);
  return (x + 0.05) / (y + 0.05);
}

test("the terminal has all 16 ANSI colours as light and dark tokens, each readable on its ground", () => {
  const { light, dark } = schemes();
  const darkOnly = tokensCss.slice(tokensCss.indexOf("@media (prefers-color-scheme: dark)"));
  for (const [key, token] of Object.entries(theme.ANSI_TOKENS)) {
    assert.match(light[token] ?? "", /^#[0-9a-f]{6}$/i, `${token} has a light value`);
    assert.ok(darkOnly.includes(`${token}:`), `${token} has a dark value`);
    assert.ok(contrast(light[token], light["--terminal-bg"]) >= 3, `${key} ${light[token]} on the light ground`);
    if (key !== "black") assert.ok(contrast(dark[token], dark["--terminal-bg"]) >= 3, `${key} ${dark[token]} on the dark ground`);
  }
  assert.equal(Object.keys(theme.ANSI_TOKENS).length, 16);
});

test("the theme reads every colour from its token", () => {
  const { light } = schemes();
  const t = theme.themeFrom((name) => light[name] ?? "");
  assert.equal(t.background, "#ffffff");
  assert.equal(t.brightWhite, light["--terminal-ansi-bright-white"]);
  assert.equal(t.yellow, light["--terminal-ansi-yellow"]);
  const bare = theme.themeFrom(() => "");
  assert.equal(bare.brightWhite, undefined, "an unset token leaves xterm's own colour");
  assert.equal(bare.background, "#ffffff");
});
