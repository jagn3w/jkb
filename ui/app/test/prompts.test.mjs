//! The Design tab's Prompts pane (D53.6) without a window: every launch records its session before
//! Claude starts, and a recorded session resumes where it runs — the scripts run for real against
//! stand-in `jkb` and `claude` programs.
//
// Bundled with esbuild, as in design.test.mjs.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-prompts-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const require = createRequire(import.meta.url);

async function load(entry) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({ entryPoints: [entry], bundle: true, format: "cjs", platform: "node", outfile, logLevel: "silent" });
  return require(outfile);
}

const src = path.join(here, "..", "src");
const { launchSpec, resumeSpec, LAUNCH_SCRIPT, RESUME_SCRIPT } = await load(path.join(src, "renderer", "src", "design", "launch.ts"));
const { parseSpec, retarget } = await load(path.join(src, "shared", "terminal.ts"));

const ROOTS = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: "/Users/me/repos", hostHome: "/Users/me" };
const UUID = "0f8fad5b-d9cb-469f-a165-70867728950e";

const record = (over = {}) => ({
  uid: `prompt:${UUID}`,
  design: "design:d",
  session: UUID,
  cwd: "/home/vscode/repos/jkb/.jkb/work/build",
  launch: "task",
  subject: "task:a",
  title: "Play · Build it",
  created_at: "2026-10-08T10:00:00.000Z",
  ...over,
});

test("a New prompt records the session on the design itself, then starts Claude in the repo", () => {
  const spec = launchSpec(
    { design: "design:d", launch: "new", label: "New", title: "Tighten the intro", prompt: "Read it." },
    "jkb",
    ROOTS,
    UUID,
  );
  assert.deepEqual(spec, {
    target: "container",
    cwd: "/home/vscode/repos/jkb",
    argv: ["/bin/bash", "-lc", LAUNCH_SCRIPT, "claude", "design:d", UUID, "new", "", "New · Tighten the intro", "Read it."],
    title: "New · Tighten the intro",
    sessionUuid: UUID,
  });
  assert.equal(parseSpec(spec).ok, true, "main accepts it");
});

test("a recorded session resumes in its recorded cwd, under its own session id", () => {
  const spec = resumeSpec(record(), ROOTS);
  assert.deepEqual(spec, {
    target: "container",
    cwd: "/home/vscode/repos/jkb/.jkb/work/build",
    argv: ["/bin/bash", "-lc", RESUME_SCRIPT, "claude", UUID],
    title: "Resume · Play · Build it",
    sessionUuid: UUID,
  });
  assert.equal(parseSpec(spec).ok, true, "main accepts it");
  // A session the toggle moved to the host recorded a host path: carried back through the mount.
  assert.equal(resumeSpec(record({ cwd: "/Users/me/repos/jkb" }), ROOTS).cwd, "/home/vscode/repos/jkb");
  // ... and the toggle takes the resume to the host again, where it was recorded.
  assert.equal(retarget(resumeSpec(record({ cwd: "/Users/me/repos/jkb" }), ROOTS), "host", ROOTS).cwd, "/Users/me/repos/jkb");
  assert.equal(resumeSpec(record({ cwd: "/elsewhere" }), ROOTS).cwd, "/elsewhere", "outside both mounts: as recorded");
});

/** Stand-ins: `jkb` logging a record call (or refusing one), `claude` reporting what it got. */
function stand_ins({ refuse = false } = {}) {
  const bin = fs.mkdtempSync(path.join(work, "bin-"));
  const log = path.join(bin, "record.log");
  const jkb = refuse
    ? 'echo "jkb serve is not reachable" >&2; exit 1'
    : `printf '%s|' "$PWD" "$@" >> '${log}'`;
  fs.writeFileSync(path.join(bin, "jkb"), `#!/bin/bash\n${jkb}\n`, { mode: 0o755 });
  fs.writeFileSync(path.join(bin, "claude"), `#!/bin/bash\nprintf '%s|' "$PWD" "$@"\n`, { mode: 0o755 });
  const run = (script, args, cwd) =>
    spawnSync("/bin/bash", ["-c", script, "claude", ...args], {
      cwd,
      encoding: "utf8",
      env: { ...process.env, PATH: `${bin}:${process.env.PATH}` },
    });
  return { run, recorded: () => (fs.existsSync(log) ? fs.readFileSync(log, "utf8") : "") };
}

test("a launch records its session from where Claude starts, then Claude gets its arguments untouched", () => {
  const { run, recorded } = stand_ins();
  const cwd = fs.mkdtempSync(path.join(work, "repo dir-"));
  const nasty = "it's $(echo pwned) `x` \"q\" $HOME";
  const r = run(LAUNCH_SCRIPT, ["design:d", UUID, "discuss", "", "Discuss · $(x)", nasty], cwd);
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.stdout, `${cwd}|--session-id|${UUID}|${nasty}|`);
  assert.equal(
    recorded(),
    `${cwd}|design|prompt|record|design:d|--session|${UUID}|--launch|discuss|--subject||--title|Discuss · $(x)|`,
  );
});

test("a refused record stops the launch before Claude starts", () => {
  const { run } = stand_ins({ refuse: true });
  const r = run(LAUNCH_SCRIPT, ["design:d", UUID, "new", "", "New · t", "p"], work);
  assert.notEqual(r.status, 0);
  assert.equal(r.stdout, "", "claude never ran");
  assert.match(r.stderr, /not reachable/);
});

test("a resume runs `claude --resume` and nothing else", () => {
  const { run, recorded } = stand_ins();
  const r = run(RESUME_SCRIPT, [UUID], work);
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.stdout, `${work}|--resume|${UUID}|`);
  assert.equal(recorded(), "", "a resume records nothing: the session already is one");
});
