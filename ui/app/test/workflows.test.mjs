//! The Workflows tab's *Contribute to jkb* (D53.7) without a window: the terminal it opens, and its
//! script run for real — against a real git repository with a local `origin`, and stand-in `jkb` and
//! `gh` programs.
//
// Bundled with esbuild, as in prompts.test.mjs.

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-workflows-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const require = createRequire(import.meta.url);

async function load(entry) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({ entryPoints: [entry], bundle: true, format: "cjs", platform: "node", outfile, logLevel: "silent" });
  return require(outfile);
}

const src = path.join(here, "..", "src");
const { contributeSpec, CONTRIBUTE_SCRIPT, PACKAGED_FILE } = await load(path.join(src, "renderer", "src", "workflows", "contribute.ts"));
const { parseSpec } = await load(path.join(src, "shared", "terminal.ts"));
const { GIT_SELECTION, GH_SELECTION } = await load(path.join(src, "shared", "gitEnv.ts"));

const ROOTS = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: "/Users/me/repos", hostReposReal: "/Users/me/repos", hostHome: "/Users/me" };

test("Contribute opens a container terminal in the jkb checkout, the name a parameter", () => {
  const spec = contributeSpec("swarm-implementer", ROOTS);
  assert.deepEqual(spec, {
    target: "container",
    cwd: "/home/vscode/repos/jkb",
    argv: ["/bin/bash", "-lc", CONTRIBUTE_SCRIPT, "contribute", "swarm-implementer"],
    title: "Contribute · swarm-implementer",
  });
  assert.equal(parseSpec(spec).ok, true, "main accepts it");
  // A name jkb would refuse never reaches a shell.
  assert.equal(contributeSpec("x; rm -rf /", ROOTS), undefined);
  assert.equal(contributeSpec("", ROOTS), undefined);
});

const GIT_ENV = {
  GIT_AUTHOR_NAME: "t",
  GIT_AUTHOR_EMAIL: "t@example.com",
  GIT_COMMITTER_NAME: "t",
  GIT_COMMITTER_EMAIL: "t@example.com",
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_NOSYSTEM: "1",
};

function git(cwd, ...args) {
  const r = spawnSync("git", args, { cwd, encoding: "utf8", env: { ...process.env, ...GIT_ENV } });
  assert.equal(r.status, 0, `git ${args.join(" ")}: ${r.stderr}`);
  return r.stdout.trim();
}

/** A bare `origin` with `main` holding the packaged file, and a checkout of it on another branch. */
function repos() {
  const root = fs.mkdtempSync(path.join(work, "repo-"));
  const origin = path.join(root, "origin.git");
  const seed = path.join(root, "seed");
  git(root, "init", "-q", "--bare", "-b", "main", origin);
  git(root, "init", "-q", "-b", "main", seed);
  fs.mkdirSync(path.join(seed, path.dirname(PACKAGED_FILE)), { recursive: true });
  fs.writeFileSync(path.join(seed, PACKAGED_FILE), '{\n  "agents": []\n}\n');
  git(seed, "add", ".");
  git(seed, "commit", "-q", "-m", "seed");
  git(seed, "remote", "add", "origin", origin);
  git(seed, "push", "-q", "origin", "main");
  const checkout = path.join(root, "jkb");
  git(root, "clone", "-q", origin, checkout);
  // The operator is on some branch of their own, with uncommitted work: none of it may be taken.
  git(checkout, "switch", "-q", "-c", "mine");
  fs.writeFileSync(path.join(checkout, "scratch.txt"), "mine\n");
  // The container binds `.git/config` read-only. A held lock is what that looks like to git without
  // a bind mount (measured on git 2.51.1: `worktree add -b` without `--no-track` then fails with
  // `could not lock config file`, as it does in the container), so nothing here may write it.
  fs.writeFileSync(path.join(checkout, ".git", "config.lock"), "");
  return { origin, checkout };
}

/** What the contribution leaves in the checkout: its worktrees and its `agent-template/*` branches. */
function leftovers(checkout) {
  const work = path.join(checkout, ".jkb", "work");
  return {
    worktrees: git(checkout, "worktree", "list").split("\n").length - 1,
    dirs: fs.existsSync(work) ? fs.readdirSync(work) : [],
    branches: git(checkout, "branch", "--list", "agent-template/*"),
  };
}

/** Stand-ins: `jkb` writing an export into the packaged file of the tree it runs in; `gh` logging. */
/**
 * Stand-ins: `jkb` writing an export into the packaged file of the tree it runs in; `gh` logging.
 * `hold`: the first `jkb` marks `reached` and waits while `hold` exists, so a test can start a
 * second run while the first is mid-export. `fixedName`: `mktemp` always answers the same directory, forcing
 * two runs onto one name. `fixedDate`: `date` always answers the same second.
 */
function stand_ins({ refuse = false, hold = false, fixedName = false, fixedDate = false } = {}) {
  const bin = fs.mkdtempSync(path.join(work, "bin-"));
  const log = path.join(bin, "gh.log");
  const holdFile = path.join(bin, "hold");
  const reached = path.join(bin, "reached");
  if (hold) fs.writeFileSync(holdFile, "");
  // Only the first run to get here waits: `mkdir` is atomic, so a second run never holds.
  const wait = hold ? `if mkdir '${reached}' 2>/dev/null; then while [ -e '${holdFile}' ]; do sleep 0.05; done; fi; ` : "";
  const jkb = refuse
    ? 'echo "jkb serve is not reachable" >&2; exit 1'
    : `${wait}printf '{\\n  "agents": ["%s"]\\n}\\n' "$4" > '${PACKAGED_FILE}'; echo "$4 packaged as v1"`;
  fs.writeFileSync(path.join(bin, "jkb"), `#!/bin/bash\n${jkb}\n`, { mode: 0o755 });
  if (fixedDate) fs.writeFileSync(path.join(bin, "date"), "#!/bin/bash\necho 20261009120000\n", { mode: 0o755 });
  if (fixedName) {
    fs.writeFileSync(path.join(bin, "mktemp"), `#!/bin/bash\nd="\${@: -1}"; d="\${d%-*}-fixed"; mkdir -p "$d"; echo "$d"\n`, { mode: 0o755 });
  }
  fs.writeFileSync(
    path.join(bin, "gh"),
    `#!/bin/bash\nprintf '%s|' "$PWD" "GH_REPO=\${GH_REPO-}" "GH_HOST=\${GH_HOST-}" "$@" >> '${log}'\n`,
    { mode: 0o755 },
  );
  const run = (args, cwd, env = {}) =>
    spawnSync("/bin/bash", ["-c", CONTRIBUTE_SCRIPT, "contribute", ...args], {
      cwd,
      encoding: "utf8",
      env: { ...process.env, ...GIT_ENV, PATH: `${bin}:${process.env.PATH}`, ...env },
    });
  const start = (args, cwd) => {
    const child = spawn("/bin/bash", ["-c", CONTRIBUTE_SCRIPT, "contribute", ...args], {
      cwd,
      env: { ...process.env, ...GIT_ENV, PATH: `${bin}:${process.env.PATH}` },
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d));
    child.stderr.on("data", (d) => (stderr += d));
    return new Promise((resolve) => child.on("close", (status) => resolve({ status, stdout, stderr })));
  };
  const reachedExport = async () => {
    for (let i = 0; i < 400 && !fs.existsSync(reached); i++) await new Promise((r) => setTimeout(r, 25));
    assert.ok(fs.existsSync(reached), "the first run reached its export");
  };
  const release = () => fs.rmSync(holdFile, { force: true });
  return { run, start, reachedExport, release, gh: () => (fs.existsSync(log) ? fs.readFileSync(log, "utf8") : "") };
}

test("Contribute commits only the exported file, off origin/main, pushes it and opens the pull request", () => {
  const { origin, checkout } = repos();
  const { run, gh } = stand_ins();
  const r = run(["swarm-status"], checkout);
  assert.equal(r.status, 0, r.stderr);
  const branch = /contributed swarm-status on (\S+)/.exec(r.stdout)?.[1];
  assert.match(branch ?? "", /^agent-template\/swarm-status-\d{14}-\w{6}$/);
  // Pushed: one commit on top of main, changing the packaged file and nothing else.
  assert.equal(git(origin, "rev-parse", `${branch}~1`), git(origin, "rev-parse", "main"));
  assert.equal(git(origin, "diff", "--name-only", `main..${branch}`), PACKAGED_FILE);
  assert.match(git(origin, "show", `${branch}:${PACKAGED_FILE}`), /"swarm-status"/);
  // The pull request is asked for that branch, against main.
  const asked = gh();
  assert.match(asked, /\|pr\|create\|--base\|main\|--head\|agent-template\/swarm-status-\d{14}-\w{6}\|--title\|workflow agents: contribute swarm-status\|/);
  // The operator's checkout is untouched, and the worktree and its local branch are gone.
  assert.equal(git(checkout, "branch", "--show-current"), "mine");
  assert.equal(fs.readFileSync(path.join(checkout, "scratch.txt"), "utf8"), "mine\n");
  assert.deepEqual(leftovers(checkout), { worktrees: 0, dirs: [], branches: "" });
  // Nothing recorded an upstream: the config the container cannot write was never asked to change.
  assert.doesNotMatch(r.stderr, /could not lock config/);
});

test("a repository the launching shell selected is never the one contributed to", () => {
  const { origin, checkout } = repos();
  // A victim repository, named by every variable that selects one, and a gh pointed elsewhere.
  const victim = repos();
  const victimHead = git(victim.checkout, "rev-parse", "HEAD");
  const victimGit = path.join(victim.checkout, ".git");
  const env = {
    GIT_DIR: victimGit,
    GIT_WORK_TREE: victim.checkout,
    GIT_COMMON_DIR: victimGit,
    GIT_INDEX_FILE: path.join(victimGit, "index"),
    GIT_OBJECT_DIRECTORY: path.join(victimGit, "objects"),
    GIT_ALTERNATE_OBJECT_DIRECTORIES: path.join(victimGit, "objects"),
    GH_REPO: "someone/else",
    GH_HOST: "example.invalid",
  };
  assert.deepEqual(Object.keys(env).sort(), [...GIT_SELECTION, ...GH_SELECTION].sort(), "every selector is exercised");
  const { run, gh } = stand_ins();
  const r = run(["swarm-status"], checkout, env);
  assert.equal(r.status, 0, r.stderr);
  const branch = /contributed swarm-status on (\S+)/.exec(r.stdout)?.[1];
  assert.equal(git(origin, "rev-parse", `${branch}~1`), git(origin, "rev-parse", "main"));
  assert.match(gh(), /\|GH_REPO=\|GH_HOST=\|pr\|create\|/, "gh resolves the repository from the worktree");
  assert.equal(git(victim.checkout, "rev-parse", "HEAD"), victimHead);
  assert.equal(git(victim.origin, "branch", "--list", "agent-template/*"), "");
});

test("the variables unset are the ones jkb's own git and gh spawns scrub", () => {
  const crates = path.join(here, "..", "..", "..", "crates", "jkb-cli", "src");
  const names = (file, constant) => {
    const text = fs.readFileSync(path.join(crates, file), "utf8");
    const at = text.indexOf(`const ${constant}: &[&str] = &[`);
    assert.notEqual(at, -1, `${constant} in ${file}`);
    const body = text.slice(at, text.indexOf("];", at));
    const code = body
      .split("\n")
      .map((l) => l.replace(/\/\/.*$/, ""))
      .join("\n");
    return [...code.matchAll(/"([A-Z_]+)"/g)].map((m) => m[1]);
  };
  assert.deepEqual([...GIT_SELECTION].sort(), names("gitrepo.rs", "REPO_SELECTION_VARS").sort());
  assert.deepEqual([...GH_SELECTION].sort(), names("pr.rs", "GH_SELECTION_VARS").sort());
  for (const v of [...GIT_SELECTION, ...GH_SELECTION]) assert.match(CONTRIBUTE_SCRIPT, new RegExp(`\\b${v}\\b`));
});

test("a refused export stops before anything is committed or pushed", () => {
  const { origin, checkout } = repos();
  const { run, gh } = stand_ins({ refuse: true });
  const r = run(["swarm-status"], checkout);
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /not reachable/);
  assert.equal(gh(), "", "no pull request");
  assert.equal(git(origin, "branch", "--list", "agent-template/*"), "", "nothing pushed");
  // Nothing is left behind for the next click to pile onto.
  assert.deepEqual(leftovers(checkout), { worktrees: 0, dirs: [], branches: "" });
  const again = run(["swarm-status"], checkout);
  assert.notEqual(again.status, 0);
  assert.deepEqual(leftovers(checkout), { worktrees: 0, dirs: [], branches: "" });
});

test("two contributions started together each get their own name, and both land", async () => {
  const { origin, checkout } = repos();
  const { start, run, reachedExport, release } = stand_ins({ hold: true, fixedDate: true });
  const first = start(["swarm-status"], checkout);
  await reachedExport();
  // The same second, the same template: a second click while the first is mid-export.
  const second = run(["swarm-status"], checkout);
  release();
  const one = await first;
  assert.equal(second.status, 0, second.stderr);
  assert.equal(one.status, 0, one.stderr);
  const branches = git(origin, "branch", "--list", "agent-template/*").split("\n").map((b) => b.trim());
  assert.equal(new Set(branches).size, 2, branches.join(","));
  assert.deepEqual(leftovers(checkout), { worktrees: 0, dirs: [], branches: "" });
});

test("a run that collides with a live one removes nothing of it", async () => {
  const { origin, checkout } = repos();
  const { start, run, reachedExport, release } = stand_ins({ hold: true, fixedName: true, fixedDate: true });
  const first = start(["swarm-status"], checkout);
  await reachedExport();
  const live = git(checkout, "worktree", "list", "--porcelain")
    .split("\n")
    .filter((l) => l.startsWith("worktree "))
    .map((l) => l.slice("worktree ".length))
    .find((w) => w.includes(`${path.sep}.jkb${path.sep}work${path.sep}`));
  assert.ok(live, "the first run's worktree");
  // Forced onto the first run's name, the second is refused before it makes anything...
  const second = run(["swarm-status"], checkout);
  assert.notEqual(second.status, 0);
  assert.match(second.stderr, /already exists/);
  // ...and leaves the first run's worktree and branch where they were.
  assert.ok(fs.existsSync(path.join(live, PACKAGED_FILE)), "the live worktree is intact");
  assert.notEqual(git(checkout, "branch", "--list", "agent-template/*"), "");
  release();
  const one = await first;
  assert.equal(one.status, 0, one.stderr);
  assert.notEqual(git(origin, "branch", "--list", "agent-template/*"), "");
  assert.deepEqual(leftovers(checkout), { worktrees: 0, dirs: [], branches: "" });
});
