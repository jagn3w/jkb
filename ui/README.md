# jkb UI

Two visual views over the [jkb](../README.md) knowledge base — an **Explorer** (where
things live) and **In Flight** (what is being worked). First host is a VS Code extension;
the reusable logic is a portable package so a web app can follow.

**Everything is backed by the `jkb` CLI** (`jkb … --json`) — the UI never touches the
database directly. Anything the UI does, the terminal can do (design D31).

## Packages (pnpm workspace)

| Package | What it is |
|---------|------------|
| **`core/`** (`@jkb/core`) | Portable TypeScript — **no `vscode`, no Node APIs**. The `JkbClient` transport interface (`client.ts`), domain models (`model.ts`: `NodeRef` / `TreeChild` / `NodeDetails` / `MutationIntent`), the node-kind **registry** (`registry.ts`), row colour policy (`decoration.ts`), detail HTML rendering (`details.ts`), per-folder count formatting (`summary.ts`), and the staging/In-Flight row shapes and labels (`staging.ts`). Reused verbatim by any host. |
| **`vscode/`** (`jkb-explorer`) | The VS Code extension: `cliClient.ts` (spawns `jkb --json` — the only Node-specific transport), `tree.ts` (the Explorer `TreeDataProvider`), `inflight.ts` (the In Flight `TreeDataProvider`), `detailsPanel.ts` (the Webview details host), `decorations.ts` (row colours/badges), `claude.ts` (starting a session in the Claude Code extension), and `extension.ts` (command wiring). |

| **`app/`** (`@jkb/app`) | **Code Factory**, the jkb desktop app (Electron + React, built by electron-vite; design record [docs/code-factory.md](../docs/code-factory.md), D53). `src/main` holds the window, the bridge's handlers and `daemon.ts` — the `jkb serve` HTTP client and the only holder of the daemon's token; `src/preload` builds `window.jkb` from `src/shared/bridge.ts`, the typed contract; `src/renderer` is the React shell (the four tabs, design tokens in `styles/tokens.css`) and the integrated terminal (`src/renderer/src/terminal/`: xterm.js, drawer and popover; its PTYs are `node-pty` in `src/main/terminals.ts`, D53.10). The renderer has no Node and reaches jkb only through the bridge. |

`@jkb/core`'s `daemon.ts` is the `jkb serve` wire protocol as data (where the daemon and its
token are, the op/response/error shapes, reply decoding), shared by every adapter that speaks
HTTP to the daemon. A future web app is another package that reuses `@jkb/core` the same way —
no rewrite of the models, registry, staging labels, or rendering.

## Develop

Uses **pnpm only** (never npm).

```sh
pnpm install          # from this ui/ directory
pnpm run build        # build @jkb/core, then bundle the extension
pnpm run typecheck    # typecheck both packages
pnpm run test         # node --test; no framework, no new dependency
```

`pnpm run build` is the real gate (and what `scripts/check.sh` and CI run): esbuild strips
types *without* checking them, so each package type-checks before it emits, and `pnpm -r`
runs topologically so `@jkb/core` emits its `.d.ts` before the adapter checks against it.

`pnpm run test` runs beside it, in both. It is `node --test` over `vscode/test/*.test.mjs` —
no framework and no new dependency. A test bundles the module it covers with esbuild (already
here for the extension bundle), aliasing `vscode` to a stub, so it needs neither a running
VS Code nor `dist/`. That suits glue over an API we do not own: what it pins is our half —
which command is asked for, with which arguments, and the state kept between two windows.

`@jkb/core`'s tests (`core/test`) run against its emitted `dist/`, so `build` precedes `test`.

## The desktop app (`app/`)

```sh
pnpm --filter @jkb/app run build   # type-check main/preload and renderer, then bundle to out/
pnpm --filter @jkb/app run test    # client against a real HTTP server, tab shell, Electron smoke
JKB_APP_FROM_CHECKOUT=1 pnpm --filter @jkb/app run start   # run the built app (needs the Electron binary)
```

The app you use is the **installed copy** (D53.3): `scripts/setup.sh` builds it from a clean clone of
`origin/main` under `~/.local/share/jkb-app/src` with that clone's `scripts/build-app.sh`, and the app
updates itself from its menu (*jkb ▸ Update from main…*). Run from this checkout (`dev`, `start`), it
refuses to start unless `JKB_APP_FROM_CHECKOUT=1` says that is deliberate: it runs unsandboxed on the
host, and an agent can write this checkout. `dev` loads the page from electron-vite's dev server, and
sets `JKB_APP_DEV_RENDERER=1` to say so: an `ELECTRON_RENDERER_URL` without it, or naming anything
but an `http://` server on loopback, refuses to start (D53.1). `pnpm --filter @jkb/app run package` is the packaging
step (`electron-builder --dir`, `app/electron-builder.yml`); `build-app.sh` is what runs it.

It talks to `jkb serve` at `$JKB_REMOTE` (default `127.0.0.1:7117`) with the token at
`$JKB_REMOTE_TOKEN_FILE` (default `~/.jkb/daemon/<port>/token`) — the same variables and paths the
CLI's remote mode uses. Start the daemon first (`jkb serve`).

`pnpm install` downloads the Electron binary from GitHub releases. Where that is unreachable (the
agent sandbox) install with `ELECTRON_SKIP_BINARY_DOWNLOAD=1`: type-check and bundle need no
binary, and the Electron smoke (`app/test/smoke.test.mjs`, one test per tab and one for the
terminal) skips and says why. On Linux it also needs a display (`xvfb-run`), as in CI.

The Design tab's Document pane (D53.4–5) edits a design's Yjs text live. `app/test/yjs-wire.test.mjs`
measures the editor's `yjs` against jkb's `yrs` with a real `jkb`: set `JKB_BIN` to one built from
this tree (`scripts/check.sh` does); without it the test skips and says so.

The terminal's `node-pty` compiles on Linux with node-gyp, which downloads Node's headers from
nodejs.org; where that is unreachable add `npm_config_nodedir=/usr` to the install. A container
terminal enters `$JKB_CONTAINER_NAME` (default `jkb-dev`, as `.container/run.sh`), read from the
app's own environment: an app launched from the Dock or Finder does not see what your shell rc
exports, so for a non-default name run `launchctl setenv JKB_CONTAINER_NAME <name>` and relaunch
the app (or start it from that shell). The *New terminal* button's tooltip names the container it
enters. A terminal's target is fixed when it opens: Claude sessions and every program run in the
container, and a host terminal is only an interactive login shell or one of the Container tab's
`run.sh` actions (running chosen programs on the host was cut from D53.10 and is a backlog task).
Closing a container terminal also ends its program inside the container with a second
`docker exec` (D53.10); when that cannot be confirmed the tab stays, saying the program may still
be running, and nothing new starts in it until you press Restart. Ctrl+` folds the drawer.

## Run the extension

1. `pnpm run build` (produces `vscode/dist/extension.js`).
2. Ensure `jkb` is on your `PATH` (or set `jkb.cliPath` in settings).
3. Open the `ui/vscode` folder in VS Code and press **F5** to launch an Extension
   Development Host.
4. Open the **jkb** container in the activity bar. It holds two views: **Explorer**
   (expand namespaces — lazy, nothing expands by default — click a node for its details,
   and edit tasks/namespaces inline) and **In Flight** (staging branches and the tasks
   landing on them).

Settings: `jkb.cliPath` (default `jkb`), `jkb.dbPath` (blank = `$JKB_DB` / `~/.jkb/jkb.db`),
and `jkb.taskLauncher` — `auto` (default), `extension`, or `terminal`; see below.

## What works

### Explorer

- **Lazy tree** of the VFS (namespaces + items homed under them, and containers expanded
  into what they contain — a document into its chunks, a task into its subtasks).
  Sub-namespaces first, then tasks by importance, then everything else by label — the tree
  does no sorting of its own, it prints the order `jkb ls` returns. (Expanding a *container*
  is different: its children come back in containment order, so a document's chunks read in
  document order.) Completed / cancelled items are hidden by default (toggle in the view
  title, like ignored files).
- **Row colours + badges**: tasks coloured by importance (p1 danger → p3+ notice, with a
  `p1`/`p2`/`p3` badge), items by kind. The colour policy is portable (`@jkb/core`); the VS Code adapter
  maps it to ThemeColors.
- **Per-folder counts** as a per-kind breakdown (`8 task · 2 document`), and a parent task
  showing how many of its subtasks are still open — the reason it is off the ready frontier.
- **Type-specific details** with a **bounded preview** (a large PDF/document shows metadata
  + a snippet, never the whole thing).
- **Inline edits** routed through the CLI: task status/priority/due/title, namespace
  rename/move, add task tag, and **item/text body editing** (`jkb item edit`).
- **Create tasks**: right-click a folder for *New Task Here…*, or a task for *New
  Subtask…*. The input takes a raw quick-add line, so `!p1 @2026-08-12 #area=ui` work
  exactly as they do in the terminal.
- **Search** (view-title): run a query DSL string and jump to a result's details.
- **Work a task with Claude**: right-click a task → pick which staging branch its work
  should land on → opens its isolated session (`jkb task work`: a git worktree and branch,
  with the task claimed) as **its own VS Code window**, with a Claude Code chat seeded with
  the task's prompt. Clicking twice returns the same session rather than forking the work.
  It is a window because the Claude Code extension runs in the window's first workspace
  folder and takes no directory argument — so the worktree has to *be* that folder, or
  Claude would work the main checkout.
  - **`jkb.taskLauncher` decides how**, because the extension is the better surface and not
    an obligation. `auto` (default) uses it when installed and runs `claude` in a terminal
    otherwise; `extension` always uses it and *says why* if it cannot, rather than handing
    you the thing you ruled out; `terminal` always runs `claude` in the worktree and never
    opens a window.
  - **Clicking twice asks nothing**, because whether an agent is live on a checkout is not
    observable — no API reports another window's folder. On the terminal surface the second
    click shows the session's existing `claude` terminal and sends nothing, and says so. A
    second click that opens a window *focuses* the one already on that worktree (VS Code
    reuses a window per folder, measured), and since nothing activates in a focused window the
    extension **watches** the prompt queue rather than only reading it at startup — so the
    prompt lands in the window you were just sent to. That means the *window* surface converges
    on one window but still opens a fresh chat per click, as the session's own window does;
    only the terminal surface is fully idempotent. On the extension surfaces a handed-over
    prompt waits **unsent** until someone presses enter; the terminal surface runs it.
- **Land this task**: runs `jkb task land` in a terminal — the gate is a build, so its
  output is watchable.

### In Flight

- **Staging branches and the tasks on each**, read from `jkb staging ls --json` — the same
  read the branch picker uses, so the two cannot disagree about what is live. Merged
  branches are hidden by default (toggle in the view title).
- **Per-task state** — implementing / review / landed / dropped — plus its session's uncommitted
  work, commits ahead, the reviewed SHA, and open must-fix findings.
- **Why a task cannot land yet**, shown on the row rather than discovered by spending a
  build on a refusal.
- **Row actions**: open a terminal in the session's worktree, land it, abandon the session,
  or open the review's findings namespace.
- **A failed read is a row, not an empty tree** — "nothing in flight" and "the CLI call
  failed" are different facts and must not render identically.

### Both

- **Live refresh**: the views update when the database changes (CLI, swarm, sync), gated on
  genuine writes so the extension's own reads don't cause a refresh loop.

Deferred (the abstractions leave room): drag re-placement/re-binding and the web-app package.
