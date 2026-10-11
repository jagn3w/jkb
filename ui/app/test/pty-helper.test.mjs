// node-pty starts every process on macOS through `spawn-helper`, and 1.1.0's prebuilds ship it
// without its execute bit, so every Code Factory terminal on a Mac failed with `posix_spawnp failed`.
// The workspace postinstall (ui/scripts/pty-helper-exec.mjs) restores the bit; this holds every
// helper node-pty can load to it, on any platform, so CI on Linux catches what only a Mac would hit.
import { test } from "node:test";
import assert from "node:assert/strict";
import { statSync } from "node:fs";
import { ptyDir, spawnHelpers } from "../../scripts/pty-helper-exec.mjs";

test("every spawn-helper node-pty can load is executable", () => {
  const helpers = spawnHelpers(ptyDir());
  assert.ok(
    helpers.some((h) => h.includes("darwin")),
    `no macOS spawn-helper found under ${ptyDir()}, so this would pass vacuously`,
  );
  for (const helper of helpers) {
    const mode = statSync(helper).mode & 0o777;
    assert.equal(
      mode & 0o111,
      0o111,
      `${helper} is ${mode.toString(8)}: node-pty cannot run it. Run \`pnpm install\` in ui/ (its ` +
        `postinstall sets the bit), or \`node ui/scripts/pty-helper-exec.mjs\`.`,
    );
  }
});
