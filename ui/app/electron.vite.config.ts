import react from "@vitejs/plugin-react";
import { defineConfig } from "electron-vite";
import type { Plugin } from "vite";

/**
 * The built page's Content-Security-Policy. Everything the renderer loads is bundled beside it,
 * and it talks to nothing over the network — every request goes through the bridge to main —
 * so `connect-src` is `'none'`. Fonts may be inlined as data: URLs by the bundler.
 *
 * Added at build time only: the dev server injects an inline React-refresh preamble and talks to
 * its HMR socket, which this policy would (rightly) refuse.
 */
const CSP = [
  "default-src 'none'",
  "script-src 'self'",
  "style-src 'self'",
  "font-src 'self' data:",
  "img-src 'self' data:",
  "connect-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
].join("; ");

function contentSecurityPolicy(): Plugin {
  return {
    name: "jkb-csp",
    apply: "build",
    transformIndexHtml: () => [
      { tag: "meta", attrs: { "http-equiv": "Content-Security-Policy", content: CSP }, injectTo: "head-prepend" },
    ],
  };
}

// `@jkb/core` is a workspace package: bundled into main (never `require`d from node_modules at
// run time) and into the sandboxed preload, which may not require anything but `electron`.
const bundleCore = { exclude: ["@jkb/core"] };

export default defineConfig({
  main: {
    build: { externalizeDeps: bundleCore },
  },
  preload: {
    build: { externalizeDeps: bundleCore },
  },
  renderer: {
    plugins: [react(), contentSecurityPolicy()],
  },
});
