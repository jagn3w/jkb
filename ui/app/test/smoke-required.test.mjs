//! The Electron smokes' skip, where the smokes are required (D53.2). Each smoke skips when it has
//! no Electron binary or no display; CI sets JKB_REQUIRE_ELECTRON_SMOKE=1 so that a skip there is a
//! failure, never a green run that exercised nothing. This runs each smoke file with its binary
//! taken away, with and without the flag. Needs no Electron.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

/** Every file that launches the app in Electron (they share electron-harness.mjs). */
const SMOKES = ["smoke.test.mjs", "lifecycle-smoke.test.mjs"];

const empty = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-no-electron-"));
after(() => fs.rmSync(empty, { recursive: true, force: true }));

/** Run a smoke with no Electron binary to be found. */
function runSmoke(file, extra) {
  const env = { ...process.env, ELECTRON_OVERRIDE_DIST_PATH: empty, ...extra };
  // The parent runner's context would make the child report to it rather than exit with a status.
  delete env.NODE_TEST_CONTEXT;
  if (extra.JKB_REQUIRE_ELECTRON_SMOKE === undefined) delete env.JKB_REQUIRE_ELECTRON_SMOKE;
  return spawnSync(process.execPath, ["--test", path.join(import.meta.dirname, file)], { env, encoding: "utf8", timeout: 60_000 });
}

test("every file that launches Electron is one this test covers", () => {
  const launching = fs
    .readdirSync(import.meta.dirname)
    .filter((f) => f.endsWith(".test.mjs") && f !== path.basename(import.meta.filename) && /_electron\b/.test(fs.readFileSync(path.join(import.meta.dirname, f), "utf8")));
  assert.deepEqual(launching.sort(), [...SMOKES].sort());
});

for (const file of SMOKES) {
  test(`${file}: with JKB_REQUIRE_ELECTRON_SMOKE=1, a smoke that cannot run fails, saying why`, () => {
    const r = runSmoke(file, { JKB_REQUIRE_ELECTRON_SMOKE: "1" });
    assert.notEqual(r.status, 0, `the smoke passed with nothing run:\n${r.stdout}`);
    assert.match(r.stdout, /JKB_REQUIRE_ELECTRON_SMOKE=1, but the Electron smoke cannot run here: no Electron binary/);
  });

  test(`${file}: without it, the same smoke skips and passes, saying why`, () => {
    const r = runSmoke(file, {});
    assert.equal(r.status, 0, `exit ${r.status}:\n${r.stdout}\n${r.stderr}`);
    assert.match(r.stdout, /Electron smoke skipped: no Electron binary/);
  });
}
