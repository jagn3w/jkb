# Code Factory — the jkb desktop app (D53)

The first standalone app on jkb: one place to **design, plan and implement** with Claude Code,
replacing the VS Code + Markdown-design workflow. Four tabs — **Design**, **Workflows**,
**Container**, **Sessions** — and one shared component, a collapsible integrated terminal.
Governs `task:jkb-code-factory-18dc73efcc248068` and its subtasks.

Part of the jkb documentation set; see [CLAUDE.md](../CLAUDE.md) for the conventions every
session is expected to know. This is the first app with jkb as its substrate and will not be the
last, so the rules in **D53.1** are written to be inherited, not just followed here.

Status: **decided, not yet built.** Each subsection names the subtask that builds it. Mark a
decision superseded in place (with the measurement that reversed it) rather than editing it away.

## D53.1 — An app over jkb is a client of the op set, never a backend

The rule D31 set for the VS Code explorer, generalized to every app:

- **Every read and write is a `jkb serve` op** (`POST /v1/op`), the same op set `jkb` speaks in
  remote mode. Anything the app does, the CLI does too, and Claude does it *through* the CLI —
  that is what "agent-first" means here. A feature that needs a new capability adds the op and its
  CLI subcommand **first**, then the button.
- **The app never opens the database**, never shells out to `sqlite3`, and holds no state of its
  own that jkb should know. Per-window conveniences (pane sizes, last tab) are the only local
  state.
- **Electron process split is a security boundary.** The renderer has no Node
  (`contextIsolation: true`, `nodeIntegration: false`, `sandbox: true`); it reaches the outside
  world only through a typed `preload` bridge. The **main** process holds the daemon's root
  token (`~/.jkb/daemon/<port>/token`), the HTTP client, and the PTYs. The token never crosses
  into the renderer.
- **Portable logic lives in `@jkb/core`** (no Node, no Electron, no DOM-host specifics): the op
  types, models, the span-state derivation, the prompt builders' inputs. `ui/app` is an adapter
  like `ui/vscode`. A second app is a third adapter.
- **Live updates are a subscription, never a poll loop.** The app consumes `mq` topics with its
  own consumer group over the existing long-poll (`mq.poll` with `wait_ms`), in the main process,
  and forwards events to the renderer. No SSE/WebSocket is added until a measurement says
  long-poll is insufficient.

## D53.2 — Shell: Electron + React, inside the `ui/` pnpm workspace

Electron, as the operator proposed. Tauri was weighed (fits the Rust workspace, small bundle)
and rejected: the terminal is the app's most-used component, and xterm.js + `node-pty` under
Chromium is the stack VS Code itself ships; the Tauri equivalent (portable-pty + a hand bridge
+ WKWebView on macOS) is the less-proven path for exactly the part that must not flake.

- Package: `ui/app` (`@jkb/app`), built by `electron-vite` (Vite for renderer/preload/main),
  React 19 + TypeScript, strict. pnpm only, as the rest of `ui/`.
- **Gated like the rest of `ui/`**: `pnpm run build` type-checks before it emits, and is what
  `./scripts/check.sh` and CI run. Pure logic is tested with `node --test` (the existing rule:
  no framework where none is needed); the app gets one Playwright `_electron` smoke per tab.
- **Sandbox note:** the npm registry is reachable from the agent sandbox; Electron's binary
  download (GitHub releases) may not be. Type-check and bundle need no binary
  (`ELECTRON_SKIP_BINARY_DOWNLOAD=1`); the Electron smoke tests are skipped with a stated reason
  when the binary is absent, never reported green.

### Visual language

Notion-inspired black and white; dense, quiet, developer-grade. Light and dark are both first
class, from one set of CSS custom properties (`--bg`, `--ink`, `--ink-muted`, `--hairline`,
`--surface`, …). Inter for UI, JetBrains Mono for code and terminals. 1px hairlines instead of
shadows; motion only where it explains a change of state (≤150ms). **Colour carries meaning
only** — the accent palette is the span-state set plus one alert:

| Meaning | Accent |
|---|---|
| PROPOSED | amber |
| APPROVED | blue |
| STAGED | violet |
| IMPLEMENTED | green |
| needs input / failure | red (the notification dot) |

## D53.3 — Trust: the app runs from an installed copy, updated from `main`

The app runs **unsandboxed on the host** and opens host terminals, so it falls under the rule
that put the container's unsandboxed steps in the kit: **nothing that runs outside the sandbox
runs from an agent-writable checkout.**

- `scripts/setup.sh` gains an app step: build from a **host-side clean clone** of `origin/main`
  (`~/.local/share/jkb-app/src`, never a worktree an agent can write) and install the packaged
  app (`electron-builder`) to `~/Applications` (macOS) / `~/.local/share/jkb-app` (Linux).
- **In-app update** (menu: *jkb ▸ Update from main…*): fetch `origin/main` into that clone, show
  the commit range being taken, build, then swap and relaunch. `main` is code that passed review
  and landed; the lockfile is frozen (`--frozen-lockfile`). The update never builds from a branch
  or worktree.
- Running from a checkout (`pnpm --filter @jkb/app dev`) refuses unless
  `JKB_APP_FROM_CHECKOUT=1` — the same opt-in `run.sh` uses — so it is a deliberate developer
  act, never the default.

## D53.4 — Designs are CRDT documents stored in jkb

A design is **not a Markdown file**: it is a jkb item (`kind = 'design'`) with a schema, homed
under `designs/<repo>/`, and every structured thing it mentions (spans, plans, tasks, prompts) is
an item or an edge rather than text.

**The document body is a CRDT** (the operator's choice over block-items): a Yjs document whose
text is one `Y.Text` (`body`). The Rust side uses **`yrs`** (the Rust port by the Yjs authors,
wire-compatible), so the CLI and the app edit the same document with no translation layer.

- **Storage:** `design_updates(design_id, seq, update BLOB, actor, txn_id, created_at)` —
  append-only Yjs v1 updates — plus a periodic `design_snapshots` compaction. The document is
  the merge of its updates; the table is the source of truth, any rendered text is derived.
- **Every update is a `write_txn` with a changelog entry** (`Entity::DesignUpdate`), so it is
  audited like any mutation. **`jkb undo` of an update is a new forward update** that reverts it
  (delete what it inserted, restore what it deleted — the Yjs `UndoManager` construction, done in
  `yrs`), never a row deletion: deleting history from a CRDT corrupts every peer that already
  merged it.
- **Live co-editing:** the app applies a local edit as `design.apply` (the update bytes,
  base64); the daemon appends it and publishes on topic `design/<uid>`; every subscriber merges.
  Updates are idempotent and commutative, so at-least-once delivery is enough.
- **Claude edits through the CLI against the version it read, and the CRDT merges.**
  `jkb design cat <uid>` prints the text with span markers **and a version token** (the Yjs state
  vector it was read at). `jkb design edit <uid> --base <token> --find <quote> [--occurrence n]
  --replace <text>` / `--insert-after <quote>` / `--span <id> --replace` resolves the quote **in
  the base snapshot**, converts it to Yjs item ids, and builds the update against that snapshot.
  Appending it merges with everything written since, the same way two app peers merge. Both
  edits survive, even inside one paragraph. Refused only where no merge exists: the targeted
  text was deleted after the base, or the base predates the last compaction (re-read and retry).
  An ambiguous or missing quote *in the base* is refused, never guessed.
- **Quotes are addressing, not concurrency control.** A model copies a quote exactly and counts
  offsets badly, so quotes are how Claude names text. They are matched in the version Claude
  saw, never the latest one. *Superseded first draft:* "resolve the anchor against the current
  document inside one transaction". That is read-latest-then-write, a lock in all but name, and
  it threw away the CRDT's one guarantee. An edit to text the operator had just touched was
  refused, or it landed on words Claude had never read. Caught by the operator in design review,
  before any code.
- **Editor:** CodeMirror 6 + `y-codemirror.next` over the same `Y.Text`, styled as live preview
  (headings, lists, code rendered in place, Notion-like). Text-first because Claude's CLI edits
  are text, and CodeMirror's decoration model is how span states are drawn.

## D53.5 — Span states: one recorded fact, two derived ones

Each piece of design text is in exactly one state:
**PROPOSED → APPROVED → STAGED → IMPLEMENTED.**

- **A span** is a range anchored by two Yjs `RelativePosition`s (they move with the text), stored
  in the document's `spans` `Y.Map` and mirrored to a `design_span` item so edges can point at
  it. New text is covered by no span and **reads as PROPOSED**.
- **APPROVED is the only recorded state.** `jkb design approve <span>` (by the reviewer the span
  names: `operator` or `claude`, enforced by D52 RBAC) records it.
- **STAGED and IMPLEMENTED are derived, never set** — the D47 rule that an op is derived, never
  chosen, applied to state. STAGED = an approved span with a `stages` edge to at least one
  execution-plan step. IMPLEMENTED = every task reachable from those steps is `done`. A column
  that stored them would disagree with the edges the first time a task reopened.
- **Editing approved text demotes it.** An insertion inside an approved span splits it, and the
  edited range is PROPOSED again; the CLI reports the demotion. Approval attests to words, so
  changed words are unapproved words.
- **Selection → Claude:** selecting text and choosing *Discuss* opens the terminal popover with a
  Claude session whose prompt carries the design uid and the selected span's anchors
  (`jkb design prompt discuss <uid> --range …` builds it — the prompt is CLI output, not app
  string-building).

## D53.6 — Execution plans, tasks and prompts are items and edges

The cardinality the operator gave: **prompts (n:1) design (1:n) spans (1:n) execution plans
(1:n) tasks.**

- **Execution plan** — `kind = 'exec_plan'`, contained by the design; ordered **steps**
  (`kind = 'plan_step'`, natural language, coarse: *scaffold → database → frontend → deploy*).
  A span is staged into a step by a `stages` edge (`jkb design stage <span> <step>`). Several
  plans may be live at once when work is truly parallel. A plan whose tasks are all terminal is
  **archived**: hidden from listings unless asked (`--all`, the same rule `jkb ls` uses for
  terminal tasks), shown in a version-history drawer.
- **Play** on a plan opens a terminal running Claude with the prompt
  `jkb design prompt play <plan>` emits: the plan, its steps, their tasks, the spans they stage,
  and the workflow strategy (D52) the tasks will run under.
- **Tasks** are ordinary jkb tasks, linked by containment under their step (or directly under the
  design for one-offs). The Tasks pane is a viewer/editor over the existing `task.*` ops — claim
  status, notes, transitions — and *Play* on a task is `jkb task work` + a Claude session under a
  chosen strategy. Existing Markdown-backed tasks migrate in a dedicated subtask once the schema
  has settled.
- **Prompts** — `kind = 'design_prompt'`, one per Claude Code session that worked the design. The
  app **pre-mints the session uuid** and launches `claude --session-id <uuid>`, so the link is
  recorded before the session exists rather than reconstructed from hooks afterwards. Resuming is
  `claude --resume <uuid>` in the prompt's recorded cwd.

## D53.7 — Workflows tab: agent templates move into jkb

Today the agent prompts live as JS template functions in `.claude/workflows/*.js`. A tab that
edits them needs them as data.

- **`workflow_agents`** (name, role, prompt template with `{{placeholders}}`, permissions,
  version) beside the existing `workflow_strategies`; packaged templates are seeded and read-only,
  operator copies are editable. The workflow scripts read templates via `jkb workflow agent show`
  so the file and the tab cannot disagree.
- The tab draws a strategy's agents as a graph (nodes = agents, edges = hand-offs), with each
  agent's template and permissions in a side panel, and a **Lifecycle** pane rendering the task
  state machine from the compiled `jkb-fsm` tables (`jkb workflow show --graph --json`) — drawn
  from the table, never redrawn by hand.
- *Contribute to jkb* exports a template to the repo's packaged-templates file on a new branch
  and opens a PR — run in the container, as every git write is.

## D53.8 — Container tab: buttons over the kit, never over the checkout

Build / verify / stop / remove / install-extensions are the kit's `run.sh` flags, invoked from
the kit path (`run.sh --kit-path`), never the checkout — the D53.3 rule again. Output streams into
the integrated terminal. The tab shows what the container is: build time, source commit and
branch, image, args-hash drift. Build time/commit/branch do not exist yet: the build stamps them
as image labels (`jkb.built-at`, `jkb.source-commit`, `jkb.source-branch`), recorded in
`.container/README.md` where the container's own decisions live.

## D53.9 — Sessions tab and the needs-input dot

- The list is `session.list` joined with notify state (`notify_sessions`: `awaiting_user` /
  `awaiting_tool`). The **red dot** — on the tab and on the session — is `awaiting_user`, fed by
  the app's own consumer group on `claude/notify`, the same stream the macOS notifier reads.
- *Jump to context* resolves a session to its worktree → task (`task.by_branch`) or design prompt.
- **Re-attach after a rebuild:** before the Container tab tears the container down it records the
  live app-owned sessions (uuid, cwd, target); after the rebuild it relaunches each as
  `claude --resume <uuid>` in its terminal. Sessions the app did not start (another editor) can be
  viewed and resumed, not re-attached — the app does not own their process.

## D53.10 — The integrated terminal

One React component used by every tab: a collapsible bottom drawer (and a popover variant for
*Discuss*), multiple tabs, xterm.js in the renderer, `node-pty` in main.

- **Default target is the container** (`docker exec -it -w <cwd> <container> …`); a per-terminal
  toggle makes it a **host** session, labelled as such in the tab so it is never ambiguous where a
  command runs.
- A terminal is created from a **spec** (`{target, cwd, argv, title, sessionUuid?}`) built by the
  caller from CLI output; the component has no knowledge of Claude or jkb.

## Subtasks, in landing order

Landed one at a time onto the staging branch, each reviewed before the next starts:

1. App scaffold — `ui/app`, process split, typed bridge, daemon HTTP client, tab shell, design
   tokens, build gate (D53.1–2).
2. Integrated terminal (D53.10).
3. Design data model in jkb — `yrs`, migrations, `design.*` ops + `jkb design` CLI (D53.4–5).
4. Design ▸ Document pane — editor, live sync, span states, Discuss (D53.4–5).
5. Execution plans + Tasks pane (D53.6).
6. Prompts pane (D53.6).
7. Workflows tab — agent templates as data, graph, lifecycle (D53.7).
8. Container tab + image labels (D53.8).
9. Sessions tab, needs-input dot, re-attach (D53.9).
10. Installed copy + update-from-main (D53.3).
11. Migrate existing tasks into the design schema (D53.6).
