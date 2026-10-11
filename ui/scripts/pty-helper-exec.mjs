// node-pty runs a helper executable (`spawn-helper`) to start every process on macOS, and the
// prebuilds node-pty 1.1.0 publishes ship it WITHOUT its execute bit (measured: `-rw-r--r--` in the
// pnpm store). Every terminal Code Factory opened on a Mac then failed with `posix_spawnp failed`,
// while Linux, which needs no helper, was fine — so CI never saw it. Run as the workspace's
// postinstall (a dev checkout, CI, build-app.sh's clone) AND first in the app's `package` script,
// so a packaged app has an executable helper whatever its install did: pnpm 11.17 does run the root
// postinstall on an up-to-date install (measured), but an install with `--ignore-scripts` leaves the
// bit unset and a later up-to-date install did not restore it here (also measured). Idempotent;
// pinned by ui/app/test/pty-helper.test.mjs.
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
  // A filtered install (`pnpm install --filter ./vscode`) has no node-pty, and nothing to fix:
  // failing here would fail that whole install.
  let dir;
  try {
    dir = ptyDir();
  } catch {
    console.log("node-pty is not installed here; no spawn-helper to fix");
    process.exit(0);
  }
  for (const helper of spawnHelpers(dir)) {
    const mode = statSync(helper).mode;
    if ((mode & 0o111) !== 0o111) {
      chmodSync(helper, mode | 0o755);
      console.log(`made ${helper} executable`);
    }
  }
}
