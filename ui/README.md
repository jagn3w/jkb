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

A future web app is a third package that reuses `@jkb/core` with an HTTP-backed `JkbClient`
— no rewrite of the models, registry, staging labels, or rendering.

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

## Run the extension

1. `pnpm run build` (produces `vscode/dist/extension.js`).
2. Ensure `jkb` is on your `PATH` (or set `jkb.cliPath` in settings).
3. Open the `ui/vscode` folder in VS Code and press **F5** to launch an Extension
   Development Host.
4. Open the **jkb** container in the activity bar. It holds two views: **Explorer**
   (expand namespaces — lazy, nothing expands by default — click a node for its details,
   and edit tasks/namespaces inline) and **In Flight** (staging branches and the tasks
   landing on them).

Settings: `jkb.cliPath` (default `jkb`) and `jkb.dbPath` (blank = `$JKB_DB` / `~/.jkb/jkb.db`).

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
  Claude would work the main checkout. Without the Claude Code extension installed it falls
  back to `claude` in a terminal in the worktree.
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
