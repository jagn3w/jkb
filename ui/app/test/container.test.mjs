//! The Container tab's half in main (D53.8) without a window: finding the installed kit, refusing one
//! whose own `run.sh --kit-path` names somewhere else, reading `run.sh --status`, and the terminal
//! spec each button opens. Against a stand-in kit `run.sh` in a scratch home, run for real.
//
// The module is bundled with esbuild, as in terminal.test.mjs.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-container-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));
const require = createRequire(import.meta.url);

async function load(entry) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({ entryPoints: [entry], bundle: true, format: "cjs", platform: "node", outfile, logLevel: "silent" });
  return require(outfile);
}

const src = path.join(here, "..", "src");
const { ContainerKit, machineKit, plain } = await load(path.join(src, "main", "container.ts"));
const { parseSpec } = await load(path.join(src, "shared", "terminal.ts"));

const STATUS = {
  schema: 1,
  docker: "reachable",
  name: "jkb-dev",
  image: "jkb-dev",
  kit: null,
  checkout: null,
  kit_changed: [],
  want_args_hash: "h",
  image_on_disk: { id: "sha256:a", created: null, built_at: "2026-10-08T10:00:00Z", source_commit: "abc", source_branch: "main" },
  container: null,
  drift: { args: null, image: null },
};

let n = 0;
/**
 * A scratch home with a stand-in kit whose run.sh answers `--kit-path` with `kitPath` (default: the
 * kit itself) and `--status` with `status` on stdout, `stderr` and `exit`. Every call is logged.
 */
function home({ kitPath, status = JSON.stringify(STATUS), stderr = "", exit = 0, kit = true } = {}) {
  const h = path.join(work, `home-${n++}`);
  const kitDir = path.join(h, ".local/share/jkb-container-kit/kit");
  fs.mkdirSync(path.join(kitDir, ".container"), { recursive: true });
  const log = path.join(h, "calls");
  if (kit) {
    const q = (s) => `'${s.replaceAll("'", "'\\''")}'`;
    fs.writeFileSync(
      path.join(kitDir, ".container", "run.sh"),
      `#!/bin/sh
printf '%s\\n' "$*" >> ${q(log)}
env > ${q(path.join(h, "env"))}
case "$1" in
  --kit-path) printf '%s\\n' ${q(kitPath ?? kitDir)} ;;
  --status) printf '%s' ${q(status)}; printf '%s' ${q(stderr)} >&2; exit ${exit} ;;
esac
`,
      { mode: 0o755 },
    );
  }
  return { h, kitDir, runSh: path.join(kitDir, ".container", "run.sh"), calls: () => (fs.existsSync(log) ? fs.readFileSync(log, "utf8").trim().split("\n") : []) };
}

test("with no kit installed, the tab is told how to install one, and nothing runs", async () => {
  const { h } = home({ kit: false });
  const r = await new ContainerKit(machineKit(h, process.env)).status();
  assert.equal(r.ok, false);
  assert.match(r.error, /No container kit at .*--install-kit/);
});

test("a kit whose run.sh names another kit is refused before its --status runs", async () => {
  const k = home({ kitPath: "/somewhere/else" });
  const kit = new ContainerKit(machineKit(k.h, process.env));
  const r = await kit.status();
  assert.equal(r.ok, false);
  assert.match(r.error, /says the kit is at \/somewhere\/else.*refusing/);
  assert.deepEqual(k.calls(), ["--kit-path"], "only --kit-path ran");
  const s = await kit.spec("build");
  assert.equal(s.ok, false, "nor does a button get a terminal for it");
});

test("run.sh --status is read into the tab's shape", async () => {
  const k = home();
  const r = await new ContainerKit(machineKit(k.h, process.env)).status();
  assert.equal(r.ok, true, r.ok ? "" : r.error);
  assert.equal(r.value.imageOnDisk.builtAt, "2026-10-08T10:00:00Z");
  assert.deepEqual(k.calls(), ["--kit-path", "--status"]);
});

test("a failed --status is reported by its last lines, without run.sh's colours", async () => {
  const k = home({ status: "", stderr: "\u001b[31merror:\u001b[0m no /x/container.json\n", exit: 1 });
  const r = await new ContainerKit(machineKit(k.h, process.env)).status();
  assert.equal(r.ok, false);
  assert.match(r.error, /--status failed \(exit 1\):\nerror: no \/x\/container\.json$/);
  assert.equal(plain("\u001b[1m==> a\u001b[0m "), "==> a");
});

test("output that is not a status is refused, not shown as one", async () => {
  const k = home({ status: "docker run --name jkb-dev" });
  const r = await new ContainerKit(machineKit(k.h, process.env)).status();
  assert.equal(r.ok, false);
  assert.match(r.error, /not JSON/);
});

test("a timeout is said as one", async () => {
  const kit = new ContainerKit({
    home: "/h",
    isFile: () => true,
    realpath: (p) => p,
    run: async (_file, args) => (args[0] === "--kit-path" ? { code: 0, stdout: "/h/.local/share/jkb-container-kit/kit\n", stderr: "" } : { code: null, stdout: "", stderr: "" }),
  });
  const r = await kit.status();
  assert.equal(r.ok, false);
  assert.match(r.error, /timed out after 60s/);
});

test("run.sh is started without Electron's variables", async () => {
  const k = home();
  await new ContainerKit(machineKit(k.h, { ...process.env, ELECTRON_RUN_AS_NODE: "1", JKB_CONTAINER_NAME: "other" })).status();
  const env = fs.readFileSync(path.join(k.h, "env"), "utf8");
  assert.doesNotMatch(env, /^ELECTRON_RUN_AS_NODE=/m);
  assert.match(env, /^JKB_CONTAINER_NAME=other$/m, "the container-name override run.sh reads still reaches it");
});

test("each button opens the kit's run.sh with its one flag, on the host, from the home", async () => {
  const k = home();
  const kit = new ContainerKit(machineKit(k.h, process.env));
  const flags = { build: "--build", verify: "--verify", "install-extensions": "--install-extensions", stop: "--stop", remove: "--rm" };
  for (const [action, flag] of Object.entries(flags)) {
    const r = await kit.spec(action);
    assert.equal(r.ok, true, r.ok ? "" : r.error);
    assert.deepEqual([r.value.target, r.value.cwd, r.value.argv], ["host", k.h, [k.runSh, flag]]);
    assert.equal(parseSpec(r.value).ok, true, "and it is a spec main's terminal host accepts");
  }
});

test("an action the tab does not offer is refused, flags and programs included", async () => {
  const k = home();
  const kit = new ContainerKit(machineKit(k.h, process.env));
  for (const bad of ["--build", "shell", "../run.sh", undefined, { action: "build" }]) {
    const r = await kit.spec(bad);
    assert.equal(r.ok, false, `${JSON.stringify(bad)} should be refused`);
    assert.match(r.error, /not a container action/);
  }
  assert.deepEqual(k.calls(), [], "nothing ran for any of them");
});
