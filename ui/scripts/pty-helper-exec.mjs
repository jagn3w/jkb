// node-pty runs a helper executable (`spawn-helper`) to start every process on macOS, and the
// prebuilds node-pty 1.1.0 publishes ship it WITHOUT its execute bit (measured: `-rw-r--r--` in the
// pnpm store). Every terminal Code Factory opened on a Mac then failed with `posix_spawnp failed`,
// while Linux, which needs no helper, was fine — so CI never saw it. Run as the workspace's
// postinstall (a dev checkout, CI) AND first in the app's `package` script: build-app.sh reuses its
// clone, and pnpm skips lifecycle scripts for an install it finds up to date, so the postinstall
// alone could leave a packaged app with the helper it had. Idempotent; pinned by
// ui/app/test/pty-helper.test.mjs.
import { chmodSync, existsSync, readdirSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/** Every `spawn-helper` node-pty can load: one per prebuilt platform, and a local build's. */
export function spawnHelpers(ptyDir) {
  const found = [];
  const prebuilds = join(ptyDir, "prebuilds");
  if (existsSync(prebuilds)) {
    for (const platform of readdirSync(prebuilds)) {
      const helper = join(prebuilds, platform, "spawn-helper");
      if (existsSync(helper)) found.push(helper);
    }
  }
  for (const kind of ["Release", "Debug"]) {
    const helper = join(ptyDir, "build", kind, "spawn-helper");
    if (existsSync(helper)) found.push(helper);
  }
  return found;
}

/** node-pty's directory, resolved the way the app resolves it (it is the app's dependency). */
export function ptyDir() {
  const app = join(dirname(fileURLToPath(import.meta.url)), "..", "app", "package.json");
  return dirname(createRequire(app).resolve("node-pty/package.json"));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  for (const helper of spawnHelpers(ptyDir())) {
    const mode = statSync(helper).mode;
    if ((mode & 0o111) !== 0o111) {
      chmodSync(helper, mode | 0o755);
      console.log(`made ${helper} executable`);
    }
  }
}
