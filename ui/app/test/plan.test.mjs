//! The Design tab's Execution Plan and Tasks panes (D53.6) without a window: what *Play* pins before
//! it starts, and the terminal it opens — run for real against stand-in `jkb` and `claude` programs.
//
// Bundled with esbuild, as in design.test.mjs.

import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-plan-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const require = createRequire(import.meta.url);

async function load(entry) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({ entryPoints: [entry], bundle: true, format: "cjs", platform: "node", outfile, logLevel: "silent" });
  return require(outfile);
}

const src = path.join(here, "..", "src");
const { pinThenPrompt, playPlanSpec, playTaskSpec, PLAY_TASK_SCRIPT } = await load(
  path.join(src, "renderer", "src", "design", "play.ts"),
);
const { LAUNCH_SCRIPT } = await load(path.join(src, "renderer", "src", "design", "launch.ts"));
const { parseSpec } = await load(path.join(src, "shared", "terminal.ts"));

const ROOTS = { container: "jkb-dev", containerRepos: "/home/vscode/repos", hostRepos: "/Users/me/repos", hostHome: "/Users/me" };
const UUID = "0f8fad5b-d9cb-469f-a165-70867728950e";
const ok = (value) => ({ ok: true, value });

const task = (over = {}) => ({
  uid: "task:a",
  title: "Build it",
  status: "open",
  priority: null,
  depth: 0,
  claimed_by: null,
  strategy: "default:design-reviewed",
  ...over,
});

const workPrompt = (over = {}) => ({
  kind: "play",
  uid: "plan:x",
  title: "First cut",
  design: "design:d",
  strategy: "coordinated",
  prompt: "it's $(rm -rf /) `x`",
  ...over,
});

/** A stand-in `op`: records each request, answers pins with `pinAnswer` and prompts with `prompt`. */
function stub({ pinAnswer = () => ok({ result: "workflow" }), prompt = workPrompt() } = {}) {
  const calls = [];
  const op = async (request) => {
    calls.push(request);
    if (request.op === "workflow.set") return pinAnswer(request);
    return ok({ result: "design_work_prompt", prompt });
  };
  return { op, calls };
}

test("Play pins the open tasks not already on the chosen strategy, then asks for the prompt", async () => {
  const { op, calls } = stub();
  const tasks = [
    task({ uid: "task:open" }),
    task({ uid: "task:on-it", strategy: "mine@2" }),
    task({ uid: "task:done", status: "done" }),
  ];
  const ask = { op: "design.prompt", ask: { kind: "play", plan: "plan:x", strategy: "mine" } };
  const answer = await pinThenPrompt(op, tasks, "mine@2", ask);
  assert.equal(answer.ok, true);
  assert.deepEqual(calls, [{ op: "workflow.set", uid: "task:open", strategy: "mine" }, ask]);
});

test("a refused pin stops the Play before any prompt, naming the task", async () => {
  const { op, calls } = stub({
    pinAnswer: (r) => (r.uid === "task:b" ? { ok: false, error: { code: "forbidden", message: "operator only" } } : ok({ result: "workflow" })),
  });
  const answer = await pinThenPrompt(op, [task(), task({ uid: "task:b" }), task({ uid: "task:c" })], "coordinated", { op: "design.prompt" });
  assert.equal(answer.ok, false);
  assert.equal(answer.error.code, "forbidden");
  assert.match(answer.error.message, /task:b to coordinated: operator only/);
  assert.deepEqual(
    calls.map((c) => c.uid ?? c.op),
    ["task:a", "task:b"],
    "stopped at the refusal: no third pin, no prompt",
  );
});

test("with no strategy chosen nothing is pinned, and a malformed prompt answer is refused", async () => {
  const { op, calls } = stub({ prompt: workPrompt({ prompt: "" }) });
  const answer = await pinThenPrompt(op, [task()], undefined, { op: "design.prompt" });
  assert.equal(answer.ok, false);
  assert.equal(answer.error.code, "internal");
  assert.deepEqual(calls, [{ op: "design.prompt" }]);
});

test("a Play left on the default pins no defaulting task; an explicit pick of the default pins them", async () => {
  const tasks = [task({ uid: "task:a" }), task({ uid: "task:b", status: "in_progress" })];
  const ask = { op: "design.prompt", ask: { kind: "play", plan: "plan:x" } };
  const left = stub();
  assert.equal((await pinThenPrompt(left.op, tasks, undefined, ask)).ok, true);
  assert.deepEqual(left.calls, [ask], "no workflow.set row for a task reporting default:design-reviewed");

  const picked = stub();
  const pickedAsk = { op: "design.prompt", ask: { kind: "play", plan: "plan:x", strategy: "design-reviewed" } };
  assert.equal((await pinThenPrompt(picked.op, tasks, "design-reviewed", pickedAsk)).ok, true);
  assert.deepEqual(picked.calls, [
    { op: "workflow.set", uid: "task:a", strategy: "design-reviewed" },
    { op: "workflow.set", uid: "task:b", strategy: "design-reviewed" },
    pickedAsk,
  ]);
});

test("a plan's Play runs Claude in the design's repo with the prompt as one argument", () => {
  const prompt = workPrompt();
  const spec = playPlanSpec(prompt, "design:d", "jkb", ROOTS, UUID);
  assert.deepEqual(spec, {
    target: "container",
    cwd: "/home/vscode/repos/jkb",
    // Recorded as the design's prompt, on the plan, before Claude starts.
    argv: ["/bin/bash", "-lc", LAUNCH_SCRIPT, "claude", "design:d", UUID, "play", "plan:x", "Play · First cut", prompt.prompt],
    title: "Play · First cut",
    sessionUuid: UUID,
  });
  assert.equal(parseSpec(spec).ok, true, "main accepts it");
  assert.equal(playPlanSpec({ ...prompt, title: "t".repeat(400) }, "design:d", "jkb", ROOTS, UUID).title.length, 200);
});

test("a task's Play opens its worktree with `jkb task work`, then Claude there", () => {
  // A task under no design still records under the design whose pane played it.
  const prompt = workPrompt({ kind: "task", uid: "task:a", title: "Build it", design: null });
  const spec = playTaskSpec(prompt, "design:d", "jkb", ROOTS, UUID);
  assert.deepEqual(spec.argv, ["/bin/bash", "-lc", PLAY_TASK_SCRIPT, "claude", "design:d", UUID, "task", "task:a", "Play · Build it", prompt.prompt]);
  assert.equal(spec.cwd, "/home/vscode/repos/jkb");
  assert.equal(spec.sessionUuid, UUID);
  assert.equal(parseSpec(spec).ok, true, "main accepts it");
  assert.ok(!PLAY_TASK_SCRIPT.includes(prompt.prompt));
});

/**
 * A directory of stand-in programs: `jkb` answering `task work` with `jkbBody` and logging a
 * `design prompt record` call (its cwd and arguments) to `record.log`; `claude` reporting what it got.
 */
function stand_ins(jkbBody) {
  const bin = fs.mkdtempSync(path.join(work, "bin-"));
  const wt = fs.mkdtempSync(path.join(work, "wt dir-"));
  const log = path.join(bin, "record.log");
  const jkb = [
    "#!/bin/bash",
    `if [ "$1 $2 $3" = "design prompt record" ]; then printf '%s|' "$PWD" "$@" >> '${log}'; exit 0; fi`,
    jkbBody.replaceAll("$WT", wt),
  ].join("\n");
  fs.writeFileSync(path.join(bin, "jkb"), `${jkb}\n`, { mode: 0o755 });
  fs.writeFileSync(path.join(bin, "claude"), `#!/bin/bash\nprintf '%s|' "$PWD" "$@"\n`, { mode: 0o755 });
  return { bin, wt, recorded: () => (fs.existsSync(log) ? fs.readFileSync(log, "utf8") : "") };
}

const hasJq = spawnSync("/bin/sh", ["-c", "command -v jq"]).status === 0;

test("the task script runs Claude in the worktree `jkb task work` answered, its arguments untouched", { skip: !hasJq && "jq is not installed here (the image has it)" }, () => {
  const { bin, wt, recorded } = stand_ins(
    // The note `task work` prints before its answer when it cancels a pending removal.
    `[ "$1 $2 $3 $4" = "--json task work task:a" ] || { echo "unexpected: $*" >&2; exit 9; }\necho "cancelled the pending removal of $WT"\nprintf '{"uid":"task:a","worktree":"%s"}\\n' "$WT"`,
  );
  const nasty = "it's $(echo pwned) `x` \"q\" $HOME";
  const out = execFileSync("/bin/bash", ["-c", PLAY_TASK_SCRIPT, "claude", "design:d", UUID, "task", "task:a", "Play · it", nasty], {
    encoding: "utf8",
    env: { ...process.env, PATH: `${bin}:${process.env.PATH}` },
  });
  assert.equal(out, `${wt}|--session-id|${UUID}|${nasty}|`);
  // Recorded from the worktree, before Claude started there.
  assert.equal(
    recorded(),
    `${wt}|design|prompt|record|design:d|--session|${UUID}|--launch|task|--subject|task:a|--title|Play · it|`,
  );
});

test("a refused `jkb task work` stops the script before Claude starts", { skip: !hasJq && "jq is not installed here (the image has it)" }, () => {
  const { bin, recorded } = stand_ins('echo "task:a is claimed by someone else" >&2; exit 1');
  const r = spawnSync("/bin/bash", ["-c", PLAY_TASK_SCRIPT, "claude", "design:d", UUID, "task", "task:a", "t", "p"], {
    encoding: "utf8",
    env: { ...process.env, PATH: `${bin}:${process.env.PATH}` },
  });
  assert.notEqual(r.status, 0);
  assert.equal(r.stdout, "", "claude never ran");
  assert.match(r.stderr, /claimed by someone else/);
  assert.equal(recorded(), "", "nothing recorded for a session that never started");
  // An answer that names no worktree stops it too, rather than starting Claude wherever it stands.
  const { bin: odd } = stand_ins('echo \'{"uid":"task:a"}\'');
  const r2 = spawnSync("/bin/bash", ["-c", PLAY_TASK_SCRIPT, "claude", "design:d", UUID, "task", "task:a", "t", "p"], {
    encoding: "utf8",
    env: { ...process.env, PATH: `${odd}:${process.env.PATH}` },
  });
  assert.notEqual(r2.status, 0);
  assert.equal(r2.stdout, "", "claude never ran");
});
