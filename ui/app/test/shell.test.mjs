//! The tab shell without a window: its navigation rules, and what it renders.
//
// The renderer modules are bundled with esbuild (JSX included) and rendered to a string with
// react-dom/server, so the four tabs and their panes are checked with neither Electron nor a DOM.
// Clicking through them in the real app is `smoke.test.mjs`'s job.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import test, { after } from "node:test";

import * as esbuild from "esbuild";

const here = import.meta.dirname;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "jkb-app-shell-"));
after(() => fs.rmSync(work, { recursive: true, force: true }));

// CommonJS, loaded with `require`: React's server build is CommonJS and requires Node builtins,
// which an ESM bundle cannot do.
const require = createRequire(import.meta.url);

async function load(entry, contents) {
  const outfile = path.join(work, `${path.basename(entry)}.cjs`);
  await esbuild.build({
    ...(contents === undefined
      ? { entryPoints: [entry] }
      : { stdin: { contents, resolveDir: path.dirname(entry), loader: "tsx" } }),
    bundle: true,
    format: "cjs",
    platform: "node",
    jsx: "automatic",
    outfile,
    logLevel: "silent",
  });
  return require(outfile);
}

const src = path.join(here, "..", "src", "renderer", "src");
const { TABS, DEFAULT_TAB, isTabId, tabForKey } = await load(path.join(src, "tabs.ts"));

test("four tabs, in the order the design names them", () => {
  assert.deepEqual(
    TABS.map((t) => t.label),
    ["Design", "Workflows", "Container", "Sessions"],
  );
  assert.equal(DEFAULT_TAB, "design");
  assert.equal(isTabId("sessions"), true);
  assert.equal(isTabId("settings"), false);
  assert.equal(isTabId(null), false);
});

test("arrows wrap, Home/End jump, and other keys are not tab keys", () => {
  assert.equal(tabForKey("design", "ArrowRight"), "workflows");
  assert.equal(tabForKey("sessions", "ArrowRight"), "design", "wraps forward");
  assert.equal(tabForKey("design", "ArrowLeft"), "sessions", "wraps back");
  assert.equal(tabForKey("container", "Home"), "design");
  assert.equal(tabForKey("workflows", "End"), "sessions");
  assert.equal(tabForKey("design", "a"), undefined);
  assert.equal(tabForKey("design", "2"), undefined, "a digit alone is typing, not navigation");
});

test("Cmd/Ctrl+1…4 picks a tab by position, and nothing past the last", () => {
  assert.equal(tabForKey("design", "1", { mod: true }), "design");
  assert.equal(tabForKey("design", "4", { mod: true }), "sessions");
  assert.equal(tabForKey("design", "5", { mod: true }), undefined);
  assert.equal(tabForKey("design", "0", { mod: true }), undefined);
  assert.equal(tabForKey("design", "ArrowRight", { mod: true }), undefined);
});

test("the shell renders every tab and pane, with only the active pane shown", async () => {
  const { render } = await load(
    path.join(src, "render-entry.tsx"),
    `import { renderToString } from "react-dom/server";
     import { App } from "./App";
     export const render = () => renderToString(<App />);`,
  );
  // The bridge is only called from effects, which a server render does not run; stub it anyway
  // so a regression that called it during render fails here, not with "jkb is undefined".
  globalThis.window = {
    jkb: {
      hello: () => assert.fail("hello during render"),
      op: () => assert.fail("op during render"),
      info: () => assert.fail("info during render"),
      terminal: {
        open: () => assert.fail("terminal.open during render"),
        onEvent: () => assert.fail("terminal.onEvent during render"),
      },
    },
    innerHeight: 800,
    localStorage: { getItem: () => "container", setItem: () => {} },
  };
  try {
    const html = render();
    for (const id of ["design", "workflows", "container", "sessions"]) {
      assert.match(html, new RegExp(`role="tab"[^>]*id="tab-${id}"`), `tab ${id}`);
      assert.match(html, new RegExp(`id="pane-${id}"`), `pane ${id}`);
    }
    assert.match(html, /id="tab-container"[^>]*aria-selected="true"/, "the remembered tab is selected");
    assert.match(html, /id="pane-container"(?![^>]*hidden)[^>]*>/, "its pane is shown");
    assert.match(html, /id="pane-design"[^>]*hidden/, "the others are hidden");
    assert.match(html, /data-state="checking"/, "the daemon's status starts unknown");
    // The terminal drawer (D53.10): present and folded, with nothing to start a terminal in until
    // main has said where terminals run.
    assert.match(html, /class="drawer-toggle"[^>]*aria-expanded="false"/, "the drawer starts folded");
    assert.match(html, /id="terminal-drawer-body"[^>]*hidden/, "its body is hidden");
    assert.match(html, /aria-label="New terminal"[^>]*disabled/, "no new terminal before the roots are known");
    assert.doesNotMatch(html, /terminal-popover/, "no popover");
    // The Design tab (D53.4): its pickers, empty until the designs are listed — which the bridge
    // is asked for from an effect, never during render.
    assert.match(html, /<select(?=[^>]*aria-label="Repo")(?=[^>]*disabled)/, "the repo picker waits for the listing");
    assert.match(html, /<select(?=[^>]*aria-label="Design")(?=[^>]*disabled)/, "so does the design picker");
    assert.match(html, /Loading designs…/);
    // The Workflows tab (D53.7): its pickers wait for the agents and strategies, asked for from
    // effects, never during render.
    assert.match(html, /<select(?=[^>]*aria-label="Workflow")(?=[^>]*disabled)/, "the workflow picker waits for the agents");
    assert.match(html, /<select(?=[^>]*aria-label="Strategy")(?=[^>]*disabled)/, "the strategy picker waits for the strategies");
    assert.match(html, /Loading agents…/);
  } finally {
    delete globalThis.window;
  }
});
