//! The Workflows tab's *Contribute to jkb* (D53.7) without a window: the terminal it opens, and its
//! script run for real — against a real git repository with a local `origin`, and stand-in `jkb` and
//! `gh` programs.
//
// Bundled with esbuild, as in prompts.test.mjs.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
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

const ROOTS = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: "/Users/me/repos", hostHome: "/Users/me" };

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
  return { origin, checkout };
}

/** Stand-ins: `jkb` writing an export into the packaged file of the tree it runs in; `gh` logging. */
function stand_ins({ refuse = false } = {}) {
  const bin = fs.mkdtempSync(path.join(work, "bin-"));
  const log = path.join(bin, "gh.log");
  const jkb = refuse
    ? 'echo "jkb serve is not reachable" >&2; exit 1'
    : `printf '{\\n  "agents": ["%s"]\\n}\\n' "$4" > '${PACKAGED_FILE}'; echo "$4 packaged as v1"`;
  fs.writeFileSync(path.join(bin, "jkb"), `#!/bin/bash\n${jkb}\n`, { mode: 0o755 });
  fs.writeFileSync(path.join(bin, "gh"), `#!/bin/bash\nprintf '%s|' "$PWD" "$@" >> '${log}'\n`, { mode: 0o755 });
  const run = (args, cwd) =>
    spawnSync("/bin/bash", ["-c", CONTRIBUTE_SCRIPT, "contribute", ...args], {
      cwd,
      encoding: "utf8",
      env: { ...process.env, ...GIT_ENV, PATH: `${bin}:${process.env.PATH}` },
    });
  return { run, gh: () => (fs.existsSync(log) ? fs.readFileSync(log, "utf8") : "") };
}

test("Contribute commits only the exported file, off origin/main, pushes it and opens the pull request", () => {
  const { origin, checkout } = repos();
  const { run, gh } = stand_ins();
  const r = run(["swarm-status"], checkout);
  assert.equal(r.status, 0, r.stderr);
  const branch = /contributed swarm-status on (\S+)/.exec(r.stdout)?.[1];
  assert.match(branch ?? "", /^agent-template\/swarm-status-\d{14}$/);
  // Pushed: one commit on top of main, changing the packaged file and nothing else.
  assert.equal(git(origin, "rev-parse", `${branch}~1`), git(origin, "rev-parse", "main"));
  assert.equal(git(origin, "diff", "--name-only", `main..${branch}`), PACKAGED_FILE);
  assert.match(git(origin, "show", `${branch}:${PACKAGED_FILE}`), /"swarm-status"/);
  // The pull request is asked for that branch, against main.
  const asked = gh();
  assert.match(asked, /\|pr\|create\|--base\|main\|--head\|agent-template\/swarm-status-\d{14}\|--title\|workflow agents: contribute swarm-status\|/);
  // The operator's checkout is untouched, and the worktree is gone.
  assert.equal(git(checkout, "branch", "--show-current"), "mine");
  assert.equal(fs.readFileSync(path.join(checkout, "scratch.txt"), "utf8"), "mine\n");
  assert.equal(git(checkout, "worktree", "list").split("\n").length, 1);
});

test("a refused export stops before anything is committed or pushed", () => {
  const { origin, checkout } = repos();
  const { run, gh } = stand_ins({ refuse: true });
  const r = run(["swarm-status"], checkout);
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /not reachable/);
  assert.equal(gh(), "", "no pull request");
  assert.equal(git(origin, "branch", "--list", "agent-template/*"), "", "nothing pushed");
});
