//! *jkb ▸ Update from main…* in main, without a window (D53.3): fetching main to show it, the commits
//! an update would take, building and staging exactly the commit shown with the clone's own builder,
//! and starting the install step as the app quits. Against real git repositories in a scratch home;
//! the builder is a stand-in that stages what it was run on, as scripts/build-app.sh does (that
//! script is tested in scripts/tests/app-install.test.sh). The install step is the real
//! scripts/install-app.sh.
//
// The module is bundled with esbuild, as in container.test.mjs.

import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const repoRoot = path.join(here, "..", "..", "..");
const work = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-update-")));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const require = createRequire(import.meta.url);

async function load(entry) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({ entryPoints: [entry], bundle: true, format: "cjs", platform: "node", outfile, logLevel: "silent" });
  return require(outfile);
}

const { AppUpdater, machineRunner, machineStarter, isInstalledCopy, builtCommit, startupRefusal, describeEnd } = await load(
  path.join(here, "..", "src", "main", "update.ts"),
);
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

/** Whether a process `pid` exists. */
function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return e.code === "EPERM";
  }
}

/** Wait (polling) until `cond()` holds, or fail after `ms`. */
async function until(cond, what, ms = 30_000) {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) assert.fail(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

/**
 * The stand-in builder, as scripts/build-app.sh: with --update-to SHA it fetches main, refuses
 * unless the tip is SHA, and moves and cleans the clone. Then by marker file in the clone: BUSY exits
 * 75, FAIL_BUILD fails, NO_STAGE exits 0 staging nothing, SLOW hangs (with a background child, as pnpm
 * has), KILLED kills itself; otherwise it stages the clone's HEAD — an app whose executable, run,
 * appends "ran" to $STUB_RAN.
 */
const BUILDER = `#!/bin/bash
set -eu
src="$(cd "$(dirname "$0")/.." && pwd)"
app_home="$(dirname "$src")"
echo "built $*" >>"$app_home/builds"
if [ "\${1:-}" = --update-to ]; then
  git -C "$src" fetch -q --no-tags origin "+refs/heads/main:refs/remotes/origin/main"
  tip="$(git -C "$src" rev-parse refs/remotes/origin/main)"
  [ "$tip" = "$2" ] || { echo "origin/main moved since it was shown" >&2; exit 1; }
  git -C "$src" checkout -q --detach --force "$tip"
  git -C "$src" clean -ffdq
fi
if [ -e "$src/BUSY" ]; then exit 75; fi
if [ -e "$src/FAIL_BUILD" ]; then echo "the build broke here" >&2; exit 3; fi
if [ -e "$src/NO_STAGE" ]; then echo "finished, staged nothing"; exit 0; fi
if [ -e "$src/SLOW" ] || [ -e "$src/KILLED" ]; then
  sleep 60 >/dev/null 2>&1 &
  echo $! >"$app_home/sleeper"
  if [ -e "$src/KILLED" ]; then kill -9 $$; fi
  wait
fi
head="$(git -C "$src" rev-parse HEAD)"
echo "building $head"
rm -rf "$app_home/staged"
if [ "$(uname -s)" = Darwin ]; then exe="$app_home/staged/app/Contents/MacOS/Code Factory"; else exe="$app_home/staged/app/code-factory"; fi
mkdir -p "$(dirname "$exe")"
printf '#!/bin/sh\\necho ran >>"\${STUB_RAN:-/dev/null}"\\n' >"$exe"
chmod +x "$exe"
echo "$head" >"$app_home/staged/app/id"
echo "$head" >"$app_home/staged/commit"
`;

let n = 0;
/**
 * A home holding the app's clone of a bare origin whose main carries the stand-in builder and the
 * real install-app.sh and lib.sh; and an installed copy's executable (a plain file) where the install
 * step puts it. The updater runs as that copy unless `exe` says otherwise. `starterEnv` starts the
 * install step for real (machineStarter); otherwise starts are recorded in `started`.
 */
function fixture({ clone = true, exe, commit, buildTimeoutMs, starterEnv } = {}) {
  const d = path.join(work, `f${n++}`);
  const seed = path.join(d, "seed");
  const origin = path.join(d, "origin.git");
  const home = path.join(d, "home");
  const appHome = path.join(home, core.APP_HOME_IN_HOME);
  const src = path.join(home, core.APP_SRC_IN_HOME);
  fs.mkdirSync(path.join(seed, "scripts"), { recursive: true });
  git(d, "init", "-q", "-b", "main", seed);
  fs.writeFileSync(path.join(seed, "scripts", "build-app.sh"), BUILDER, { mode: 0o755 });
  for (const f of ["install-app.sh", "lib.sh"]) fs.copyFileSync(path.join(repoRoot, "scripts", f), path.join(seed, "scripts", f));
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
  const installedDir = core.installedAppDir(process.platform, home);
  // The stamp names where jkb installed the app, so the install step may replace it.
  const stamp = (c) => fs.writeFileSync(path.join(appHome, "installed"), `commit=${c}\ndest=${installedDir}\n`);
  const installedExe = core.installedExecutable(process.platform, home);
  fs.mkdirSync(path.dirname(installedExe), { recursive: true });
  fs.writeFileSync(installedExe, "");
  const started = [];
  const updater = new AppUpdater(
    home,
    machineRunner(home, gitEnv, 200),
    starterEnv !== undefined ? machineStarter(home, starterEnv) : (file, args, log) => started.push({ file, args, log }),
    { exe: exe ?? installedExe, platform: process.platform, commit },
    { buildTimeoutMs },
  );
  return { seed, home, appHome, src, land, stamp, updater, installedDir, installedExe, started, head: () => git(seed, "rev-parse", "HEAD") };
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

test("a plan fetches main to a ref no build reads and lists what landed since, newest first; it moves nothing", async () => {
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
  assert.equal(git(f.src, "rev-parse", "HEAD"), installed);
  assert.equal(git(f.src, "rev-parse", "refs/remotes/origin/main"), installed, "origin/main, which a build checks, is not moved by a plan");
  assert.equal(git(f.src, "rev-parse", core.UPDATE_SHOWN_REF), tip);
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

test("apply runs the clone's builder --update-to the commit shown, which stages it; the installed app is untouched", async () => {
  const f = fixture();
  const installed = f.head();
  f.stamp(installed);
  const tip = f.land("second");
  const plan = await f.updater.plan();
  assert.ok(plan.ok, plan.error);
  fs.writeFileSync(path.join(f.src, "stray"), "planted\n");
  const r = await f.updater.apply(plan.value.target);
  assert.ok(r.ok, r.error);
  assert.equal(r.value, tip);
  assert.equal(fs.readFileSync(path.join(f.appHome, "builds"), "utf8"), `built --update-to ${tip}\n`);
  assert.equal(git(f.src, "rev-parse", "HEAD"), tip);
  assert.ok(!fs.existsSync(path.join(f.src, "stray")));
  assert.equal(f.updater.stagedCommit(), tip);
  assert.equal(f.updater.installed(), installed, "nothing is installed by a build");
  assert.equal(fs.readFileSync(f.installedExe, "utf8"), "");
  assert.match(fs.readFileSync(f.updater.logFile, "utf8"), new RegExp(`building ${tip}`));
});

test("apply refuses what is not a commit id, and a commit main has moved past is refused by the builder", async () => {
  const f = fixture();
  for (const bad of ["main", "HEAD", "a".repeat(39), undefined, { id: f.head() }]) {
    const b = await f.updater.apply(bad);
    assert.equal(b.ok, false);
    assert.match(b.error, /not a commit id/);
  }
  assert.ok(!fs.existsSync(path.join(f.appHome, "builds")));
  const old = f.head();
  f.land("second");
  const r = await f.updater.apply(old);
  assert.equal(r.ok, false);
  assert.match(r.error, /origin\/main moved/);
  assert.equal(f.updater.stagedCommit(), undefined);
});

test("a failed build is reported with its output; nothing is installed or staged", async () => {
  const f = fixture();
  const installed = f.head();
  f.stamp(installed);
  f.land("breaks the build", "FAIL_BUILD");
  const plan = await f.updater.plan();
  const r = await f.updater.apply(plan.value.target);
  assert.equal(r.ok, false);
  assert.match(r.error, /The build failed \(exit 3\)\. Nothing was installed/);
  assert.match(r.error, /the build broke here/);
  assert.equal(f.updater.installed(), installed);
  assert.equal(f.updater.stagedCommit(), undefined);
  assert.equal(f.updater.busy, false);
});

test("a build that exits 0 without staging the commit shown is a failure", async () => {
  const f = fixture();
  f.land("stages nothing", "NO_STAGE");
  const plan = await f.updater.plan();
  const r = await f.updater.apply(plan.value.target);
  assert.equal(r.ok, false);
  assert.match(r.error, /did not stage/);
});

test("a builder that finds the lock held (75) says another build or install is running", async () => {
  const f = fixture();
  f.land("busy", "BUSY");
  const plan = await f.updater.plan();
  const r = await f.updater.apply(plan.value.target);
  assert.equal(r.ok, false);
  assert.match(r.error, /Another build or install of Code Factory is running/);
});

test("a timed-out build is stopped with its whole process group, and said to have installed nothing", async () => {
  const f = fixture({ buildTimeoutMs: 500 });
  f.land("hangs", "SLOW");
  const plan = await f.updater.plan();
  const r = await f.updater.apply(plan.value.target);
  assert.equal(r.ok, false);
  assert.match(r.error, /The build timed out after .* and was stopped\. Nothing was installed/);
  const sleeper = Number(fs.readFileSync(path.join(f.appHome, "sleeper"), "utf8"));
  await until(() => !alive(sleeper), `the build's child ${sleeper} to be stopped`, 3_000);
});

test("a builder killed by a signal is reported as killed, not as a timeout", async () => {
  const f = fixture();
  f.land("killed", "KILLED");
  const plan = await f.updater.plan();
  const r = await f.updater.apply(plan.value.target);
  assert.equal(r.ok, false);
  assert.match(r.error, /The build was killed \(SIGKILL\)/);
  assert.doesNotMatch(r.error, /timed out/);
  process.kill(Number(fs.readFileSync(path.join(f.appHome, "sleeper"), "utf8")), "SIGKILL");
});

test("describeEnd words each way a run can end", () => {
  assert.equal(describeEnd({ code: 3, stdout: "", stderr: "" }, 1000), "failed (exit 3)");
  assert.equal(describeEnd({ code: null, stdout: "", stderr: "", timedOut: true }, 2000), "timed out after 2s and was stopped");
  assert.equal(describeEnd({ code: null, stdout: "", stderr: "", signal: "SIGKILL" }, 1000), "was killed (SIGKILL)");
  assert.equal(describeEnd({ code: null, stdout: "", stderr: "" }, 1000), "could not be run");
});

test("the runner drops a launching shell's repository selection and Electron's variables, and sets GIT_TERMINAL_PROMPT=0", async () => {
  const f = fixture();
  f.stamp(f.head());
  const tip = f.land("second");
  const updater = new AppUpdater(
    f.home,
    machineRunner(f.home, { ...gitEnv, GIT_DIR: path.join(work, "elsewhere"), ELECTRON_RUN_AS_NODE: "1" }),
    () => {},
    { exe: f.installedExe, platform: process.platform, commit: undefined },
  );
  const r = await updater.plan();
  assert.ok(r.ok, r.error);
  assert.equal(r.value.target, tip);
  const env = await machineRunner(f.home, { ...gitEnv, GIT_TERMINAL_PROMPT: "1", ELECTRON_RUN_AS_NODE: "1" })("/usr/bin/env", [], 10_000);
  assert.match(env.stdout, /^GIT_TERMINAL_PROMPT=0$/m);
  assert.doesNotMatch(env.stdout, /ELECTRON_RUN_AS_NODE/);
  assert.match(env.stdout, new RegExp(`^HOME=${f.home}$`, "m"));
});

test("a timeout stops the whole process group, not only the program started", async () => {
  const home = fs.mkdtempSync(path.join(work, "group-"));
  const pidFile = path.join(home, "child");
  const r = await machineRunner(home, gitEnv, 200)("/bin/bash", ["-c", `sleep 60 >/dev/null 2>&1 & echo $! >"${pidFile}"; wait`], 300);
  assert.equal(r.code, null);
  assert.equal(r.timedOut, true);
  const child = Number(fs.readFileSync(pidFile, "utf8"));
  await until(() => !alive(child), `the background child ${child} to be stopped`, 3_000);
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
  assert.throws(() => git(f.src, "rev-parse", "--verify", "--quiet", core.UPDATE_SHOWN_REF), "nothing was fetched");
  assert.ok(!fs.existsSync(path.join(f.appHome, "builds")));
  const link = path.join(work, `link-${n}`);
  fs.symlinkSync(f.installedExe, link);
  assert.equal(isInstalledCopy(link, process.platform, f.home), true);
  assert.equal(isInstalledCopy(elsewhere, process.platform, f.home), false);
});

test("at startup only the installed copy runs, unless JKB_APP_FROM_CHECKOUT=1", () => {
  const f = fixture();
  const elsewhere = path.join(work, `dist-${n}`, "code-factory");
  fs.mkdirSync(path.dirname(elsewhere), { recursive: true });
  fs.writeFileSync(elsewhere, "");
  const link = path.join(work, `startup-link-${n}`);
  fs.symlinkSync(f.installedExe, link);
  const facts = (over) => ({ isPackaged: true, exe: f.installedExe, platform: process.platform, home: f.home, env: {}, ...over });
  assert.equal(startupRefusal(facts({})), undefined, "the installed copy runs");
  assert.equal(startupRefusal(facts({ exe: link })), undefined, "a link to it is it");
  const packagedElsewhere = startupRefusal(facts({ exe: elsewhere }));
  assert.match(packagedElsewhere ?? "", /not running from its installed copy/, "a package left in a checkout's dist/ is refused");
  assert.ok((packagedElsewhere ?? "").includes(f.installedExe), "the refusal names the installed copy");
  assert.match(startupRefusal(facts({ isPackaged: false })) ?? "", /JKB_APP_FROM_CHECKOUT=1/, "a checkout's out/ is refused");
  assert.equal(startupRefusal(facts({ exe: elsewhere, env: { JKB_APP_FROM_CHECKOUT: "1" } })), undefined);
});

test("what is running beats the stamp", async () => {
  const f0 = fixture();
  const old = f0.head();
  const f = fixture({ commit: old });
  const tip = f.land("second");
  f.stamp(tip);
  const r = await f.updater.plan();
  assert.ok(r.ok, r.error);
  assert.equal(r.value.target, tip);
  assert.notEqual(r.value.installed, tip);
  const out = fs.mkdtempSync(path.join(work, "out-"));
  assert.equal(builtCommit(out), undefined);
  fs.writeFileSync(path.join(out, "commit"), `${tip}\n`);
  assert.equal(builtCommit(out), tip);
  fs.writeFileSync(path.join(out, "commit"), "main\n");
  assert.equal(builtCommit(out), undefined);
});

test("the install step is started only with something staged, for this pid, relaunching when asked", async () => {
  const f = fixture();
  assert.equal(f.updater.startInstaller(1234, true).ok, false);
  assert.deepEqual(f.started, []);
  f.land("second");
  const plan = await f.updater.plan();
  assert.ok((await f.updater.apply(plan.value.target)).ok);
  const r = f.updater.startInstaller(1234, true);
  assert.ok(r.ok, r.error);
  assert.deepEqual(f.started, [
    { file: "/bin/bash", args: [path.join(f.src, core.APP_INSTALLER_IN_SRC), "--wait-pid", "1234", "--relaunch"], log: f.updater.installLogFile },
  ]);
  f.updater.startInstaller(1234, false);
  assert.deepEqual(f.started[1].args.slice(1), ["--wait-pid", "1234"]);
});

test("as the app quits: the real install step waits for it to exit, then swaps, stamps and relaunches", async () => {
  const ran = path.join(work, `ran-${n}`);
  const f = fixture({ starterEnv: { ...gitEnv, STUB_RAN: ran } });
  const before = f.head();
  f.stamp(before);
  const tip = f.land("second");
  const plan = await f.updater.plan();
  assert.ok((await f.updater.apply(plan.value.target)).ok);
  // The app: a process the install step must outwait.
  const app = spawn("sleep", ["30"], { stdio: "ignore" });
  const r = f.updater.startInstaller(app.pid, true);
  assert.ok(r.ok, r.error);
  await new Promise((res) => setTimeout(res, 600));
  assert.equal(f.updater.installed(), before, "nothing is swapped while the app runs");
  assert.ok(!fs.existsSync(path.join(f.installedDir, "id")));
  app.kill("SIGKILL");
  // The step stamps, then removes the staged copy: wait for both.
  await until(() => f.updater.installed() === tip && f.updater.stagedCommit() === undefined, "the install step to finish");
  assert.equal(fs.readFileSync(path.join(f.installedDir, "id"), "utf8").trim(), tip);
  assert.equal(f.updater.stagedCommit(), undefined, "the staged copy is consumed");
  await until(() => fs.existsSync(ran), "the relaunch");
  assert.match(fs.readFileSync(f.updater.installLogFile, "utf8"), /installed Code Factory/);
});

test("the installed copy's place, the app home, staging and the exit statuses agree with scripts/lib.sh", () => {
  const lib = path.join(repoRoot, "scripts", "lib.sh");
  const sh = (script, ...args) => execFileSync("/bin/bash", ["-c", `. "$0"; ${script}`, lib, ...args], { encoding: "utf8" }).trim();
  for (const [uname, platform] of [["Darwin", "darwin"], ["Linux", "linux"]]) {
    const home = "/home/u x";
    const dest = sh('app_default_dest "$1" "$2" "$3"', uname, home, `${home}/${core.APP_HOME_IN_HOME}`);
    assert.equal(dest, core.installedAppDir(platform, home));
    assert.equal(sh('app_executable "$1" "$2"', uname, dest), core.installedExecutable(platform, home));
  }
  const appHome = execFileSync("/bin/bash", ["-c", '. "$0"; app_default_home', lib], { encoding: "utf8", env: { ...process.env, HOME: "/h" } }).trim();
  assert.equal(appHome, `/h/${core.APP_HOME_IN_HOME}`);
  assert.equal(`${core.APP_HOME_IN_HOME}/${sh('printf %s "$APP_STAGED_IN_APP_HOME"')}`, core.APP_STAGED_IN_HOME);
  assert.deepEqual({ busy: Number(sh('printf %s "$APP_EXIT_BUSY"')), running: Number(sh('printf %s "$APP_EXIT_RUNNING"')) }, { ...core.BUILD_EXIT });
});
