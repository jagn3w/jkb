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
const { pinThenPrompt, planTarget, playPlanSpec, playTaskSpec, taskTarget, PLAY_TASK_SCRIPT } = await load(
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

const STRATEGIES = {
  result: "strategies",
  default: "design-reviewed",
  strategies: [
    { name: "design-reviewed", preset: true, describe: "" },
    { name: "coordinated", preset: true, describe: "" },
    { name: "mine@2", preset: false, describe: "" },
  ],
};

/** A `design.plans` answer: one plan `plan:x` with `tasks` under its one step, and `oneOffs`. */
const listing = (tasks, oneOffs = []) => ({
  result: "design_plans",
  list: {
    uid: "design:d",
    plans: [{ uid: "plan:x", title: "First cut", design: "design:d", archived: false, steps: [{ uid: "step:s", text: "s", spans: [], tasks }] }],
    hidden: 0,
    tasks: oneOffs,
  },
});

/**
 * A stand-in `op`: records each request, answers pins with `pinAnswer`, prompts with `prompt`, the
 * strategies with `strategies` and `design.plans` with `plans()` — read at the call, so a test can
 * change what jkb holds between the pane's read and the Play's.
 */
function stub({ pinAnswer = () => ok({ result: "workflow" }), prompt = workPrompt(), plans = () => listing([task()]), strategies = STRATEGIES } = {}) {
  const calls = [];
  const op = async (request) => {
    calls.push(request);
    if (request.op === "workflow.set") return pinAnswer(request);
    if (request.op === "workflow.strategies") return ok(strategies);
    if (request.op === "design.plans") return ok(plans());
    return ok({ result: "design_work_prompt", prompt });
  };
  return { op, calls, pins: () => calls.filter((c) => c.op === "workflow.set") };
}

const PLAN = planTarget("design:d", "plan:x");

test("Play pins the open tasks not already on the chosen strategy, then asks for the prompt", async () => {
  const tasks = [
    task({ uid: "task:open" }),
    task({ uid: "task:on-it", strategy: "mine@2" }),
    task({ uid: "task:done", status: "done" }),
  ];
  const { op, calls } = stub({ plans: () => listing(tasks) });
  const ask = { op: "design.prompt", ask: { kind: "play", plan: "plan:x", strategy: "mine" } };
  const answer = await pinThenPrompt(op, PLAN, "mine", ask);
  assert.equal(answer.ok, true);
  assert.deepEqual(calls, [
    { op: "workflow.strategies" },
    { op: "design.plans", uid: "design:d", all: true },
    { op: "workflow.set", uid: "task:open", strategy: "mine" },
    ask,
  ]);
});

test("Play pins from what jkb holds when it is pressed, not from the pane's last read", async () => {
  // The pane read one task; Claude has since added another under the step, and `mine` was
  // redefined (v2 -> v3), so the task the pane saw on `mine@2` is no longer on what `mine` names.
  const fresh = [task({ uid: "task:seen", strategy: "mine@2" }), task({ uid: "task:added-since" })];
  const { op, pins } = stub({
    plans: () => listing(fresh),
    strategies: { ...STRATEGIES, strategies: [...STRATEGIES.strategies.slice(0, 2), { name: "mine@3", preset: false, describe: "" }] },
  });
  assert.equal((await pinThenPrompt(op, PLAN, "mine", { op: "design.prompt" })).ok, true);
  assert.deepEqual(
    pins().map((p) => p.uid),
    ["task:seen", "task:added-since"],
  );
  // A task's own Play reads its task afresh too, one-offs included; one already on the pick is left.
  const one = stub({ plans: () => listing([], [task({ uid: "task:one-off", strategy: "coordinated" })]) });
  assert.equal((await pinThenPrompt(one.op, taskTarget("design:d", "task:one-off"), "coordinated", { op: "design.prompt" })).ok, true);
  assert.deepEqual(one.pins(), []);
  // A pick no longer listed, or a target gone from the design, starts nothing.
  const gone = stub();
  const r = await pinThenPrompt(gone.op, PLAN, "deleted", { op: "design.prompt" });
  assert.equal(r.ok, false);
  assert.match(r.error.message, /no longer listed/);
  const r2 = await pinThenPrompt(gone.op, taskTarget("design:d", "task:nope"), "coordinated", { op: "design.prompt" });
  assert.equal(r2.ok, false);
  assert.deepEqual(gone.pins(), []);
  assert.equal(gone.calls.filter((c) => c.op === "design.prompt").length, 0);
});

test("a refused pin stops the Play before any prompt, naming the task", async () => {
  const { op, calls } = stub({
    pinAnswer: (r) => (r.uid === "task:b" ? { ok: false, error: { code: "forbidden", message: "operator only" } } : ok({ result: "workflow" })),
    plans: () => listing([task(), task({ uid: "task:b" }), task({ uid: "task:c" })]),
  });
  const answer = await pinThenPrompt(op, PLAN, "coordinated", { op: "design.prompt" });
  assert.equal(answer.ok, false);
  assert.equal(answer.error.code, "forbidden");
  assert.match(answer.error.message, /task:b to coordinated: operator only/);
  assert.deepEqual(
    calls.filter((c) => c.op !== "workflow.strategies" && c.op !== "design.plans").map((c) => c.uid ?? c.op),
    ["task:a", "task:b"],
    "stopped at the refusal: no third pin, no prompt",
  );
});

test("with no strategy chosen nothing is pinned, and a malformed prompt answer is refused", async () => {
  const { op, calls } = stub({ prompt: workPrompt({ prompt: "" }) });
  const answer = await pinThenPrompt(op, PLAN, undefined, { op: "design.prompt" });
  assert.equal(answer.ok, false);
  assert.equal(answer.error.code, "internal");
  assert.deepEqual(calls, [{ op: "design.prompt" }], "nothing read, nothing pinned");
});

test("a Play left on the default pins no defaulting task; an explicit pick of the default pins them", async () => {
  const tasks = [task({ uid: "task:a" }), task({ uid: "task:b", status: "in_progress" })];
  const ask = { op: "design.prompt", ask: { kind: "play", plan: "plan:x" } };
  const left = stub({ plans: () => listing(tasks) });
  assert.equal((await pinThenPrompt(left.op, PLAN, undefined, ask)).ok, true);
  assert.deepEqual(left.calls, [ask], "no workflow.set row for a task reporting default:design-reviewed");

  const picked = stub({ plans: () => listing(tasks) });
  const pickedAsk = { op: "design.prompt", ask: { kind: "play", plan: "plan:x", strategy: "design-reviewed" } };
  assert.equal((await pinThenPrompt(picked.op, PLAN, "design-reviewed", pickedAsk)).ok, true);
  assert.deepEqual(picked.calls.slice(2), [
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
