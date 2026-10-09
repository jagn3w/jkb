//! Where the renderer's page comes from (`src/main/devRenderer.ts`, D53.1). The page main loads is
//! the page it trusts with the bridge, so an inherited ELECTRON_RENDERER_URL must not make that a
//! remote page. Plain logic, bundled with esbuild and run without Electron.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-renderer-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));

const bundle = path.join(work, "devRenderer.mjs");
await esbuild.build({
  entryPoints: [path.join(here, "..", "src", "main", "devRenderer.ts")],
  bundle: true,
  format: "esm",
  platform: "node",
  outfile: bundle,
  logLevel: "silent",
});
const { rendererSource } = await import(bundle);

const dev = (url) => ({ ELECTRON_RENDERER_URL: url, JKB_APP_DEV_RENDERER: "1" });

test("with no dev server named, the built page loads", () => {
  assert.deepEqual(rendererSource(false, {}), { kind: "file" });
  assert.deepEqual(rendererSource(false, { ELECTRON_RENDERER_URL: "  " }), { kind: "file" });
});

test("a packaged app never loads a dev server, whatever the environment says", () => {
  assert.deepEqual(rendererSource(true, dev("http://example.net")), { kind: "file" });
  assert.deepEqual(rendererSource(true, { ELECTRON_RENDERER_URL: "http://localhost:5173" }), { kind: "file" });
});

test("a loopback http dev server loads with the opt-in", () => {
  assert.deepEqual(rendererSource(false, dev("http://localhost:5173")), { kind: "dev", url: "http://localhost:5173/" });
  assert.deepEqual(rendererSource(false, dev("http://127.0.0.1:5173/")), { kind: "dev", url: "http://127.0.0.1:5173/" });
  assert.deepEqual(rendererSource(false, dev("http://[::1]:5173")), { kind: "dev", url: "http://[::1]:5173/" });
});

test("an inherited ELECTRON_RENDERER_URL without the opt-in refuses to start", () => {
  const out = rendererSource(false, { ELECTRON_RENDERER_URL: "http://localhost:5173" });
  assert.equal(out.kind, "refused");
  assert.match(out.reason, /JKB_APP_DEV_RENDERER=1 is not/);
  assert.equal(rendererSource(false, { ...dev("http://localhost:5173"), JKB_APP_DEV_RENDERER: "yes" }).kind, "refused");
});

test("a dev server off loopback, or not plain http, refuses to start even with the opt-in", () => {
  for (const url of [
    "http://example.net",
    "http://example.net:5173",
    "https://localhost:5173",
    "file:///etc/passwd",
    "http://10.0.0.5:5173",
    "http://localhost.example.net:5173",
    "http://127.0.0.1.nip.io:5173",
    "http://user:pw@localhost:5173",
    "not a url",
  ]) {
    const out = rendererSource(false, dev(url));
    assert.equal(out.kind, "refused", `${url} was accepted`);
  }
});
