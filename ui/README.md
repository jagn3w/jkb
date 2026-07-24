# jkb UI

A visual tree explorer for the [jkb](../README.md) knowledge base. First host is a VS Code
extension; the reusable logic is a portable package so a web app can follow.

**Everything is backed by the `jkb` CLI** (`jkb … --json`) — the UI never touches the
database directly. Anything the UI does, the terminal can do (design D31).

## Packages (pnpm workspace)

| Package | What it is |
|---------|------------|
| **`core/`** (`@jkb/core`) | Portable TypeScript — **no `vscode`, no Node APIs**. The `JkbClient` transport interface, domain models (`NodeRef` / `TreeChild` / `NodeDetails`), the node-kind **registry**, and detail HTML rendering. Reused verbatim by any host. |
| **`vscode/`** (`jkb-explorer`) | The VS Code extension: `CliJkbClient` (spawns `jkb --json` — the only Node-specific transport), a `TreeDataProvider`, a Webview details host, and command wiring. |

A future web app is a third package that reuses `@jkb/core` with an HTTP-backed `JkbClient`
— no rewrite of the models, registry, or rendering.

## Develop

Uses **pnpm only** (never npm).

```sh
pnpm install          # from this ui/ directory
pnpm run build        # build @jkb/core, then bundle the extension
pnpm run typecheck    # typecheck both packages
```

## Run the extension

1. `pnpm run build` (produces `vscode/dist/extension.js`).
2. Ensure `jkb` is on your `PATH` (or set `jkb.cliPath` in settings).
3. Open the `ui/vscode` folder in VS Code and press **F5** to launch an Extension
   Development Host.
4. Open the **jkb** view in the activity bar. Expand namespaces (lazy — nothing expands by
   default), click a node to open its details, and edit tasks/namespaces inline.

Settings: `jkb.cliPath` (default `jkb`) and `jkb.dbPath` (blank = `$JKB_DB` / `~/.jkb/jkb.db`).

## What works

- **Lazy tree** of the VFS (namespaces + items homed under them). Sorted: namespaces
  first, then tasks by priority (most important first), then other items. Completed /
  cancelled items are hidden by default (toggle in the view title, like ignored files).
- **Row colours + badges**: tasks coloured by importance (p1 red → p3 yellow, with a `p1`
  badge), items by kind. The colour policy is portable (`@jkb/core`); the VS Code adapter
  maps it to ThemeColors.
- **Type-specific details** with a **bounded preview** (a large PDF/document shows metadata
  + a snippet, never the whole thing).
- **Inline edits** routed through the CLI: task status/priority/due/title, namespace
  rename/move, add task tag, and **item/text body editing** (`jkb item edit`).
- **Live refresh**: the tree updates when the database changes (CLI, swarm, sync).
- **Search** (view-title): run a query DSL string and jump to a result's details.
- **Work a task with Claude**: right-click a task → opens a terminal running `claude`
  seeded with a prompt to do it.

Deferred (the abstractions leave room): drag re-placement/re-binding and the web-app package.
