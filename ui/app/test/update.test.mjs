//! *jkb ▸ Update from main…* in main, without a window (D53.3): fetching main into the clean clone,
//! the commits an update would take, and building exactly the commit shown with the clone's own
//! builder. Against real git repositories in a scratch home; the builder is a stand-in that stamps
//! what it was run on, as scripts/build-app.sh does (that script is tested in
//! scripts/tests/app-install.test.sh).
//
// The module is bundled with esbuild, as in container.test.mjs.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-update-")));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const require = createRequire(import.meta.url);

async function load(entry) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({ entryPoints: [entry], bundle: true, format: "cjs", platform: "node", outfile, logLevel: "silent" });
  return require(outfile);
}

const { AppUpdater, machineRunner, isInstalledCopy, builtCommit } = await load(path.join(here, "..", "src", "main", "update.ts"));
const core = await import(path.join(here, "..", "..", "core", "dist", "index.js"));

/** git with an empty configuration, so the machine's own never reaches these repositories. */
const gitEnv = {
  ...process.env,
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_AUTHOR_NAME: "t",
  GIT_AUTHOR_EMAIL: "t@example.com",
  GIT_COMMITTER_NAME: "t",
  GIT_COMMITTER_EMAIL: "t@example.com",
};
for (const k of ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"]) delete gitEnv[k];
const git = (cwd, ...args) => execFileSync("git", ["-C", cwd, ...args], { env: gitEnv, encoding: "utf8" }).trim();

/**
 * The stand-in builder: logs its arguments and the lock token it was handed beside the lock's own,
 * fails when FAIL_BUILD is in the clone, exits 0 without stamping when NO_STAMP is, hangs (with a
 * background child, as pnpm has) when SLOW is, else stamps the clone's HEAD into <app-home>/installed.
 */
const BUILDER = `#!/bin/bash
set -eu
app_home="$2"
src="$(cd "$(dirname "$0")/.." && pwd)"
echo "built $*" >>"$app_home/builds"
echo "handed=\${JKB_APP_LOCK_TOKEN:-} lock=$(cat "$app_home/lock/token" 2>/dev/null)" >>"$app_home/lockseen"
if [ -e "$src/FAIL_BUILD" ]; then echo "the build broke here" >&2; exit 3; fi
if [ -e "$src/NO_STAMP" ]; then echo "finished, stamped nothing"; exit 0; fi
if [ -e "$src/SLOW" ]; then
  sleep 60 >/dev/null 2>&1 &
  echo $! >"$app_home/sleeper"
  wait
fi
echo "building $(git -C "$src" rev-parse HEAD)"
printf 'commit=%s\\n' "$(git -C "$src" rev-parse HEAD)" >"$app_home/installed"
`;

let n = 0;
/**
 * A home holding the app's clone of a bare origin whose main carries the stand-in builder, and an
 * installed copy's executable (a plain file) where build-app.sh puts it. The updater runs as that
 * copy unless `exe` says otherwise.
 */
function fixture({ clone = true, exe, commit, run, buildTimeoutMs } = {}) {
  const d = path.join(work, `f${n++}`);
  const seed = path.join(d, "seed");
  const origin = path.join(d, "origin.git");
  const home = path.join(d, "home");
  const appHome = path.join(home, ".local/share/jkb-app");
  const src = path.join(appHome, "src");
  fs.mkdirSync(path.join(seed, "scripts"), { recursive: true });
  git(d, "init", "-q", "-b", "main", seed);
  fs.writeFileSync(path.join(seed, "scripts", "build-app.sh"), BUILDER, { mode: 0o755 });
  git(seed, "add", "-A");
  git(seed, "commit", "-q", "-m", "one");
  git(d, "clone", "-q", "--bare", seed, origin);
  git(seed, "remote", "add", "origin", origin);
  fs.mkdirSync(appHome, { recursive: true });
  if (clone) git(d, "clone", "-q", origin, src);
  const land = (subject, file = "landed.txt") => {
    fs.appendFileSync(path.join(seed, file), `${subject}\n`);
    git(seed, "add", "-A");
    git(seed, "commit", "-q", "-m", subject);
    git(seed, "push", "-q", "origin", "main");
    return git(seed, "rev-parse", "HEAD");
  };
  const stamp = (c) => fs.writeFileSync(path.join(appHome, "installed"), `commit=${c}\n`);
  const installedExe = core.installedExecutable(process.platform, home);
  fs.mkdirSync(path.dirname(installedExe), { recursive: true });
  fs.writeFileSync(installedExe, "");
  const runner = machineRunner(home, gitEnv, 200);
  const updater = new AppUpdater(
    home,
    run === undefined ? runner : run(runner),
    { exe: exe ?? installedExe, platform: process.platform, commit },
    { buildTimeoutMs },
  );
  return { seed, home, appHome, src, land, stamp, updater, installedExe, head: () => git(seed, "rev-parse", "HEAD") };
}

test("with no clone, nothing is fetched and setup.sh is named", async () => {
  const f = fixture({ clone: false });
  const r = await f.updater.plan();
  assert.equal(r.ok, false);
  assert.match(r.error, /No clean clone at .*jkb-app\/src.*setup\.sh/);
});

test("installed at main's tip: nothing to take", async () => {
  const f = fixture();
  f.stamp(f.head());
  const r = await f.updater.plan();
  assert.ok(r.ok, r.error);
  assert.deepEqual(r.value, { installed: f.head(), target: f.head(), commits: [], diverged: false });
});

test("a plan fetches main and lists what landed since the installed commit, newest first", async () => {
  const f = fixture();
  const installed = f.head();
  f.stamp(installed);
  f.land("second change");
  const tip = f.land("third change");
  const r = await f.updater.plan();
  assert.ok(r.ok, r.error);
  assert.equal(r.value.installed, installed);
  assert.equal(r.value.target, tip);
  assert.deepEqual(
    r.value.commits.map((c) => c.subject),
    ["third change", "second change"],
  );
  assert.equal(r.value.diverged, false);
  // A plan moves nothing: the clone's HEAD is where it was, and nothing was built.
  assert.equal(git(f.src, "rev-parse", "HEAD"), installed);
  assert.ok(!fs.existsSync(path.join(f.appHome, "builds")));
});

test("a stamp naming a commit main does not have is a divergence, not an error", async () => {
  const f = fixture();
  f.stamp("f".repeat(40));
  const r = await f.updater.plan();
  assert.ok(r.ok, r.error);
  assert.equal(r.value.diverged, true);
  assert.deepEqual(r.value.commits, []);
});

test("apply moves the clone to the commit shown, cleans it, runs the clone's builder, and checks the stamp", async () => {
  const f = fixture();
  f.stamp(f.head());
  const tip = f.land("second");
  const plan = await f.updater.plan();
  assert.ok(plan.ok, plan.error);
  fs.writeFileSync(path.join(f.src, "stray"), "planted\n");
  const r = await f.updater.apply(plan.value.target);
  assert.ok(r.ok, r.error);
  assert.equal(r.value, tip);
  assert.equal(git(f.src, "rev-parse", "HEAD"), tip);
  assert.ok(!fs.existsSync(path.join(f.src, "stray")), "an untracked file in the clone is removed before the build");
  assert.equal(f.updater.installed(), tip);
  assert.equal(fs.readFileSync(path.join(f.appHome, "builds"), "utf8"), `built --app-home ${f.appHome}\n`);
  assert.match(fs.readFileSync(f.updater.logFile, "utf8"), new RegExp(`building ${tip}`));
});

test("apply refuses a commit that is not main's fetched tip, and builds nothing", async () => {
  const f = fixture();
  const old = f.head();
  f.land("second");
  assert.ok((await f.updater.plan()).ok);
  const r = await f.updater.apply(old);
  assert.equal(r.ok, false);
  assert.match(r.error, /origin\/main moved/);
  for (const bad of ["main", "HEAD", "a".repeat(39), undefined, { id: old }]) {
    const b = await f.updater.apply(bad);
    assert.equal(b.ok, false);
    assert.match(b.error, /not a commit id/);
  }
  assert.ok(!fs.existsSync(path.join(f.appHome, "builds")));
});

test("a failed build is reported with its output, and the stamp is not moved", async () => {
  const f = fixture();
  const installed = f.head();
  f.stamp(installed);
  f.land("breaks the build", "FAIL_BUILD");
  const plan = await f.updater.plan();
  assert.ok(plan.ok, plan.error);
  const r = await f.updater.apply(plan.value.target);
  assert.equal(r.ok, false);
  assert.match(r.error, /The build failed \(exit 3\); the installed app is unchanged/);
  assert.match(r.error, /the build broke here/);
  assert.equal(f.updater.installed(), installed);
  assert.match(fs.readFileSync(f.updater.logFile, "utf8"), /the build broke here/);
  assert.equal(f.updater.busy, false);
});

test("the runner drops a launching shell's repository selection", async () => {
  const f = fixture();
  f.stamp(f.head());
  const tip = f.land("second");
  const updater = new AppUpdater(f.home, machineRunner(f.home, { ...gitEnv, GIT_DIR: path.join(work, "elsewhere"), ELECTRON_RUN_AS_NODE: "1" }), {
    exe: f.installedExe,
    platform: process.platform,
    commit: undefined,
  });
  const r = await updater.plan();
  assert.ok(r.ok, r.error);
  assert.equal(r.value.target, tip);
});

test("the runner sets GIT_TERMINAL_PROMPT=0, whatever the launching shell had", async () => {
  const home = fs.mkdtempSync(path.join(work, "env-"));
  const r = await machineRunner(home, { ...gitEnv, GIT_TERMINAL_PROMPT: "1" })("/usr/bin/env", [], 10_000);
  assert.equal(r.code, 0);
  assert.match(r.stdout, /^GIT_TERMINAL_PROMPT=0$/m);
});

/** Whether a process `pid` exists. */
function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return e.code === "EPERM";
  }
}

test("a timeout stops the whole process group, not only the program started", async () => {
  const home = fs.mkdtempSync(path.join(work, "group-"));
  const pidFile = path.join(home, "child");
  const r = await machineRunner(home, gitEnv, 200)(
    "/bin/bash",
    ["-c", `sleep 60 >/dev/null 2>&1 & echo $! >"${pidFile}"; wait`],
    300,
  );
  assert.equal(r.code, null);
  assert.equal(r.survivors, false);
  const child = Number(fs.readFileSync(pidFile, "utf8"));
  assert.ok(!alive(child), `the background child ${child} outlived the timeout`);
});

test("an update runs only as the installed copy: any other copy is refused before it fetches or builds", async () => {
  const elsewhere = path.join(work, "checkout-dist", "code-factory");
  fs.mkdirSync(path.dirname(elsewhere), { recursive: true });
  fs.writeFileSync(elsewhere, "");
  const f = fixture({ exe: elsewhere });
  f.stamp(f.head());
  const tip = f.land("second");
  const p = await f.updater.plan();
  assert.equal(p.ok, false);
  assert.match(p.error, /is not the installed one/);
  const a = await f.updater.apply(tip);
  assert.equal(a.ok, false);
  assert.match(a.error, /is not the installed one/);
  assert.notEqual(git(f.src, "rev-parse", "origin/main"), tip, "nothing was fetched");
  assert.ok(!fs.existsSync(path.join(f.appHome, "builds")));
  // A link to the installed executable is the installed copy.
  const link = path.join(work, `link-${n}`);
  fs.symlinkSync(f.installedExe, link);
  assert.equal(isInstalledCopy(link, process.platform, f.home), true);
  assert.equal(isInstalledCopy(elsewhere, process.platform, f.home), false);
});

test("what is running beats the stamp: a copy swapped in under it does not make it up to date", async () => {
  const f0 = fixture();
  const old = f0.head();
  const f = fixture({ commit: old });
  const tip = f.land("second");
  f.stamp(tip);
  const r = await f.updater.plan();
  assert.ok(r.ok, r.error);
  assert.equal(r.value.target, tip);
  assert.notEqual(r.value.installed, tip);
  // builtCommit reads out/commit, and nothing else.
  const out = fs.mkdtempSync(path.join(work, "out-"));
  assert.equal(builtCommit(out), undefined);
  fs.writeFileSync(path.join(out, "commit"), `${tip}\n`);
  assert.equal(builtCommit(out), tip);
  fs.writeFileSync(path.join(out, "commit"), "main\n");
  assert.equal(builtCommit(out), undefined);
});

test("a build that exits 0 without stamping the commit shown is a failure, not an install", async () => {
  const f = fixture();
  f.stamp(f.head());
  f.land("stamps nothing", "NO_STAMP");
  const plan = await f.updater.plan();
  assert.ok(plan.ok, plan.error);
  const r = await f.updater.apply(plan.value.target);
  assert.equal(r.ok, false);
  assert.match(r.error, /did not record .* as installed/);
});

test("a timed-out build is stopped with everything it started, not reported unchanged, and the lock is released", async () => {
  const f = fixture({ buildTimeoutMs: 500 });
  const installed = f.head();
  f.stamp(installed);
  f.land("hangs", "SLOW");
  const plan = await f.updater.plan();
  assert.ok(plan.ok, plan.error);
  const r = await f.updater.apply(plan.value.target);
  assert.equal(r.ok, false);
  assert.match(r.error, /timed out after .* and was stopped, every process it started with it/);
  assert.doesNotMatch(r.error, /unchanged/);
  const sleeper = Number(fs.readFileSync(path.join(f.appHome, "sleeper"), "utf8"));
  assert.ok(!alive(sleeper), `the build's child ${sleeper} outlived the timeout`);
  assert.equal(f.updater.installed(), installed);
  assert.ok(!fs.existsSync(f.updater.lockDir));
});

test("a timed-out build whose processes may survive keeps the lock, and says the app may still change", async () => {
  const f = fixture({
    run: (real) => (file, args, ms, env) =>
      file === "/bin/bash" ? Promise.resolve({ code: null, stdout: "", stderr: "", survivors: true }) : real(file, args, ms, env),
  });
  f.stamp(f.head());
  f.land("second");
  const plan = await f.updater.plan();
  assert.ok(plan.ok, plan.error);
  const r = await f.updater.apply(plan.value.target);
  assert.equal(r.ok, false);
  assert.match(r.error, /could not be stopped: the installed app may still change/);
  assert.ok(fs.existsSync(f.updater.lockDir), "the lock stays held");
  assert.equal(fs.readFileSync(path.join(f.updater.lockDir, "pid"), "utf8").trim(), String(process.pid));
});

test("apply holds the install lock, hands the builder its token, and refuses a lock another holds", async () => {
  const f = fixture();
  f.stamp(f.head());
  f.land("second");
  const plan = await f.updater.plan();
  assert.ok(plan.ok, plan.error);
  // Held by a live holder (this test process): busy, and nothing moves or builds.
  const before = git(f.src, "rev-parse", "HEAD");
  fs.mkdirSync(f.updater.lockDir);
  fs.writeFileSync(path.join(f.updater.lockDir, "pid"), `${process.pid}\n`);
  fs.writeFileSync(path.join(f.updater.lockDir, "token"), "theirs\n");
  const busy = await f.updater.apply(plan.value.target);
  assert.equal(busy.ok, false);
  assert.match(busy.error, /Another install or update is running/);
  assert.equal(git(f.src, "rev-parse", "HEAD"), before);
  assert.ok(!fs.existsSync(path.join(f.appHome, "builds")));
  assert.equal(fs.readFileSync(path.join(f.updater.lockDir, "token"), "utf8").trim(), "theirs");
  // Left by a holder that is gone: broken, and the update proceeds under its own.
  const dead = execFileSync("/bin/sh", ["-c", "echo $$"], { encoding: "utf8" }).trim();
  fs.writeFileSync(path.join(f.updater.lockDir, "pid"), `${dead}\n`);
  const r = await f.updater.apply(plan.value.target);
  assert.ok(r.ok, r.error);
  const seen = fs.readFileSync(path.join(f.appHome, "lockseen"), "utf8").trim();
  const m = /^handed=(\S+) lock=(\S+)$/.exec(seen);
  assert.ok(m !== null && m[1] === m[2] && m[1] !== "theirs", `the builder saw ${seen}`);
  assert.ok(!fs.existsSync(f.updater.lockDir), "released after the build");
});

test("the installed copy's place agrees with scripts/lib.sh", () => {
  const lib = path.join(here, "..", "..", "..", "scripts", "lib.sh");
  const sh = (fn, ...args) =>
    execFileSync("/bin/bash", ["-c", `. "$0"; ${fn} "$@"`, lib, ...args], { encoding: "utf8" }).trim();
  for (const [uname, platform] of [["Darwin", "darwin"], ["Linux", "linux"]]) {
    const home = "/home/u x";
    const dest = sh("app_default_dest", uname, home, `${home}/${core.APP_HOME_IN_HOME}`);
    assert.equal(dest, core.installedAppDir(platform, home));
    assert.equal(sh("app_executable", uname, dest), core.installedExecutable(platform, home));
  }
  const lockName = execFileSync("/bin/bash", ["-c", '. "$0"; printf %s "$APP_LOCK_IN_APP_HOME"', lib], { encoding: "utf8" });
  assert.equal(core.APP_LOCK_IN_HOME, `${core.APP_HOME_IN_HOME}/${lockName}`);
});
