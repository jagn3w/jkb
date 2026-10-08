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

const { AppUpdater, machineRunner } = await load(path.join(here, "..", "src", "main", "update.ts"));

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
 * The stand-in builder: logs its arguments, fails when FAIL_BUILD is in the clone, else stamps the
 * clone's HEAD into <app-home>/installed.
 */
const BUILDER = `#!/bin/bash
set -eu
app_home="$2"
src="$(cd "$(dirname "$0")/.." && pwd)"
echo "built $*" >>"$app_home/builds"
if [ -e "$src/FAIL_BUILD" ]; then echo "the build broke here" >&2; exit 3; fi
echo "building $(git -C "$src" rev-parse HEAD)"
printf 'commit=%s\\n' "$(git -C "$src" rev-parse HEAD)" >"$app_home/installed"
`;

let n = 0;
/** A home holding the app's clone of a bare origin whose main carries the stand-in builder. */
function fixture({ clone = true } = {}) {
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
  const stamp = (commit) => fs.writeFileSync(path.join(appHome, "installed"), `commit=${commit}\n`);
  const updater = new AppUpdater(home, machineRunner(home, gitEnv));
  return { seed, home, appHome, src, land, stamp, updater, head: () => git(seed, "rev-parse", "HEAD") };
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
  const updater = new AppUpdater(f.home, machineRunner(f.home, { ...gitEnv, GIT_DIR: path.join(work, "elsewhere"), ELECTRON_RUN_AS_NODE: "1" }));
  const r = await updater.plan();
  assert.ok(r.ok, r.error);
  assert.equal(r.value.target, tip);
});
