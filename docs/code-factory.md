# Code Factory — the jkb desktop app (D53)

The first standalone app on jkb: one place to **design, plan and implement** with Claude Code,
replacing the VS Code + Markdown-design workflow. Four tabs — **Design**, **Workflows**,
**Container**, **Sessions** — and one shared component, a collapsible integrated terminal.
Governs `task:jkb-code-factory-18dc73efcc248068` and its subtasks.

Part of the jkb documentation set; see [CLAUDE.md](../CLAUDE.md) for the conventions every
session is expected to know. This is the first app with jkb as its substrate and will not be the
last, so the rules in **D53.1** are written to be inherited, not just followed here.

Status: **decided; subtasks 1 (the scaffold), 2 (the terminal), 3 (the design data model) and 4 (the Document pane) are built, the rest are not.** Each subsection names the subtask that builds it. Mark a
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

**As built (subtask 1).** `ui/app` is laid out as `src/main` (the window, the bridge's handlers,
`daemon.ts` — the `jkb serve` client), `src/preload` (builds `window.jkb`), `src/shared/bridge.ts`
(the typed contract both sides compile against) and `src/renderer` (React). The wire format —
daemon URL and token path rules, op/response/error shapes, reply decoding — is `@jkb/core`'s
`daemon.ts`, mirroring `crates/jkb-cli/src/remote.rs` and `crates/jkb-api`. What it cost to learn:

- **The preload must be CommonJS.** A sandboxed preload cannot be an ES module, and electron-vite
  emits ESM when the package says `"type": "module"`, so `@jkb/app` does not; `@jkb/core` is
  bundled into main and the preload (`externalizeDeps.exclude`), never required at run time.
- **The CSP is added at build time only.** `script-src 'self'; connect-src 'none'` is right for the
  built page — every request goes through the bridge — but refuses the dev server's inline
  React-refresh preamble, so `electron.vite.config.ts` injects it into the built `index.html`.
- **`require("electron")` downloads the binary when it is missing** (measured on electron 44.5:
  its `index.js` spawns `install.js` on a miss), so the smoke finds the binary through `path.txt`
  itself and skips without touching the network.
- **The daemon token is read `O_NOFOLLOW`** and must be a small regular file holding one word:
  `~/.jkb` is writable from the dev container, and a planted link would have the app send
  whatever it points at as a header. The CLI's reader follows links; the app is the first host
  client that holds the root token in a long-lived GUI process, so it is the stricter one.
- **pnpm 11 holds back releases younger than its minimum release age**, and `pnpm add` answered by
  adding an exemption to `pnpm-workspace.yaml`. The exemption was removed and slightly older
  versions pinned instead (electron 44.5.1, playwright-core 1.63.0): the gate is a supply-chain
  defence, not an obstacle.
- **CI runs the smoke under `xvfb-run`** with Ubuntu 24.04's AppArmor userns restriction lifted,
  rather than with `--no-sandbox` — the renderer sandbox is what the smoke is there to exercise.
  Not yet observed green on a runner when this was written; the agent sandbox has no Electron
  binary and no display, so there the smoke skips, saying so.

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

**As built (subtask 3, D53.4–5).** The engine is `crates/jkb-core/src/design/` (`mod.rs`: storage, the
edit, spans and their states; `crdt.rs`: the pure `yrs` half), migration `V024__designs.sql`, the ops
`design.list`/`create`/`cat`/`state`/`spans`/`apply`/`edit`/`span`/`approve`/`stage`/`compact` in
`crates/jkb-api/src/designs.rs`, and `jkb design …` in `crates/jkb-cli/src/design_cli.rs`. What it cost
to learn, and what was decided past the text above:

- **The version token is `<seq>.<state vector>`, not the state vector alone.** A state vector says
  which inserts a peer has seen but nothing about deletions, so the text Claude read cannot be rebuilt
  from one. The document at a version is the compaction plus every update with `seq` at or below the
  token's; the state vector rides along so a token cannot be replayed against another design (it must
  match what that seq rebuilds to) and so an editor can ask `design.state --since` for what it lacks.
- **Offsets are UTF-16.** `yrs` defaults to byte offsets while a string item's clock counts UTF-16
  units, so a sticky index after a multi-byte character resolved to the wrong place. Every document is
  built with `OffsetKind::Utf16`, as Yjs counts; pinned by an edit and a span after `🦀` and `ï`
  (measured: with `OffsetKind::Bytes` that test fails).
- **`design_updates.id` is AUTOINCREMENT.** A compaction deletes rows and the changelog names an update
  by its id; with a plain rowid the next update was handed the compacted row's id, and `jkb undo` of the
  compacted update reverted the newer one (measured: the compaction test did exactly that, and
  `an_update_id_is_never_reused_after_a_compaction` fails again with the keyword removed). The
  `items.id` lesson (V010), again.
- **Undo is `InsertInverse::ForwardUpdate`** (`undo::Inverse::DesignRevert`): the `UndoManager` seeded
  with the one stack item the update is, run over the current document with garbage collection off so a
  deleted run can be put back beside text written after it. The appended row is not itself
  changelogged — the `undo` marker is its record, and undoing an undo is not something `undo` does. A
  compaction is logged as bookkeeping (`BOOKKEEPING`), so a bare `jkb undo` reaches past it; an update
  it folded away is refused by name, never reverted on a guess.
- **APPROVED is recorded on the span's item, not in the CRDT**: `metadata.approval` holds a Yjs snapshot
  (state vector + deletions) of the document at the approval, and the reviewer the span names is on the
  item too. Any editor can write the document, so a reviewer stored there could be rewritten by any peer.
  Which words are still the approved ones is derived by diffing the text against that snapshot: words
  written inside the span since read PROPOSED, the rest keep the span's state, and words deleted from it
  are reported as a zero-width removed piece. A span with any of either is `demoted` and reads PROPOSED
  as a whole until re-approved; `jkb design edit` names the spans it demoted.
- **Anchors**: the start sticks to the span's first character and the end to its last, so text typed at
  either edge stays outside and only an insertion strictly inside splits the span. Spans may not
  overlap — each piece of text is in exactly one state.
- **Who approves**: a span naming `operator` is the operator's alone; one naming `claude` is approved by
  a Claude principal (recorded by its label) or the operator, who holds every permission. RBAC adds
  `design` (coordinator, designer) and `design_approve` (those and the reviewer); compaction is the
  operator's; a design write is no one task's (`Target::Shared`).
- **IMPLEMENTED needs at least one task** under the span's steps (containment, any depth), all `done`:
  a step with no tasks has implemented nothing, so vacuous truth is refused.
- **Live updates** go to topic `design/<uid>` with the uid's `:` spelled `.` (not a topic character),
  payload `{design, seq, update}` with the update inline up to 32 KiB. Best effort: a full or oversized
  queue costs a subscriber a `design.state` re-read, never the write.
- **A design cannot be removed** (`item::remove`, even with `--force`): its updates are not in the
  delete's snapshot, so `jkb undo` would bring back a design with no text.
- ~~**Unmeasured here, stated:** byte compatibility with the app's JavaScript `yjs`.~~ *Measured in
  subtask 4* (below): `ui/app/test/yjs-wire.test.mjs` drives a real `jkb` with `yjs` 13.6.33 as the
  editor — jkb's state loaded, an editor update merged after a two-unit character, a CLI edit merged
  back from `design.state --since`, and a span's anchors (`yrs` `StickyIndex` v1 bytes) resolved by
  `Y.decodeRelativePosition` to the offsets jkb reports. It held at the first run; no adapter needed.

**As built (subtask 4, the Document pane).** `@jkb/core`'s `design.ts` (the answers' shapes and
decoders, base64, the live-update message, and `stateRuns` — the span-state derivation the editor
draws), the app's `src/main/designFeeds.ts` (the live feeds), `src/renderer/src/design/` (`session.ts`
the Yjs peer, `editor.ts` the CodeMirror extensions, `DocumentEditor.tsx`, `discuss.ts`) and
`tabs/DesignTab.tsx` (the repo and design pickers). What was decided past the text above:

- ***Discuss* is an op first** (D53.1): `design.prompt` with `{kind: "discuss", uid, base, start,
  end}` and `jkb design prompt discuss <uid> --range <start>..<end> [--base <token>]`, the same answer
  both ways. The selection crosses as UTF-16 offsets into the version the editor showed, and the engine
  (`design/discuss.rs`) turns it into what Claude edits by: the quote, which match of it the selection
  is (`--occurrence`), and the spans it touches. A selection no quote could name — one starting inside
  an earlier match of itself, or splitting a surrogate pair — is refused, never widened. The app only
  carries the prompt text; it builds none of it.
- **The offsets are sent with the version whose text is the screen's.** The pane waits for every
  local edit to be saved, re-reads with `design.cat`, and sends that version only if its text is the
  text the selection was made in; otherwise it asks for the selection again. Sending the latest version
  with offsets taken from an older screen is the read-latest mistake D53.4 rejected, in the other
  direction.
- **A *Discuss* is `bash -lc 'exec claude --session-id "$1" "$2"'`** in the container, in the
  design's repo under the repos mount, the session uuid minted by the app and passed as the terminal's
  `sessionUuid` too. The login shell finds `claude` on the login `PATH` (a `docker exec` has the
  image's environment); the prompt is a positional parameter, never spliced into the script.
- **Every design has its topic from creation.** `mq.group_create` refuses a topic that does not exist,
  and a design created with no text had none until its first update, so the app could not subscribe
  to it. `design::create` now creates it; publishing still does too, for a design made before.
- **One consumer group, `code-factory`, one long-poll per open design, in main.** A per-launch group
  would leave a group per launch on every topic, each holding messages until the queue's idle removal
  (7 days) — at the topic's cap that refuses the announcement of every later edit. Windows showing the
  same design share the feed. The window subscribes *then* loads (`design.state` since its own state
  vector), so nothing falls between the two. A group the queue removed is rejoined and the window told
  `gap`; an update too large to announce inline, or one whose dependencies never arrived (Yjs keeps it
  pending), is completed the same way. Two app instances would share the group and split its messages:
  not a case the app guards today (one instance per machine).
- **Span states are drawn only from an answer that describes the screen.** `design.cat`'s text is
  compared with the document's; an answer read while an edit was in flight is dropped for the next.
  Between answers the drawing moves with the text. Text in a span is tinted with its state; text no
  span covers reads PROPOSED but is marked only by an amber bar in the margin (a line's bar is its
  least advanced state), so a fresh draft is not a page of amber; words removed from an approved span
  show struck through where they were.
- **A refused `design.apply` stops the session** (the editor turns read-only and says why) rather than
  retrying; an unreachable or busy daemon is retried with backoff and the edits kept, in order, merged
  into one update per call.
- **`@codemirror/language` is held at 6.12.4** (a workspace `overrides` entry): 6.13.0, published the
  day before, imports `@codemirror/streamparser` without declaring it, and the renderer did not bundle.

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

**As built (subtask 2).** The contract is `src/shared/terminal.ts` (the spec, its validator
`parseSpec`, `retarget`, the events); main's half is `src/main/terminals.ts` (`TerminalHost`: the
PTYs, the command each spec runs); the renderer's is `src/renderer/src/terminal/` (a reducer for
what is open where, one xterm instance per terminal that outlives its view, the drawer, the
popover, and `useTerminals()` — `open(spec, "drawer" | "popover")` is what a tab calls). The drawer
folds with Ctrl+` and is resized by its top edge; its height is a per-window convenience.

- **A container terminal is `docker exec -i -t -e TERM=… -w <cwd> <container> <argv>`**, with
  `/bin/bash -l` (absolute, as the kit names every program it execs) when the spec has no argv. The
  container is `$JKB_CONTAINER_NAME`, default `jkb-dev`: the variable `run.sh` reads. A name that is
  not one (`--privileged`) is refused rather than handed to docker as a flag. `docker` itself is
  looked for at fixed absolute paths, never on `PATH`: a GUI app's `PATH` is not the shell's.
- **Main trusts nothing the renderer sends.** `parseSpec` refuses unknown fields (there is no `env`
  to smuggle in), relative or NUL-bearing paths, and oversized argv; sizes and writes are bounded. A
  terminal belongs to the window that opened it: only that window can write to it, resize it or
  close it, and it is killed when that window closes or reloads. The PTYs never cross the bridge,
  only their output does.
- **The toggle restarts the program on the other side.** A process cannot move between the
  container and the host, so switching ends it (after asking, while it runs) and starts the spec
  again there. The cwd crosses through the repos mount (`HOST_REPOS` ⇄ `CTR_REPOS` in `run.sh`), the
  one directory both sides see; outside it, the target's default. The badge on the tab says where
  it runs, and a host badge is drawn inverted so it cannot be read as the quiet default.
- **A second open with the same `sessionUuid` shows the terminal already running it**, so a
  double-clicked *Play* or *Discuss* does not start a second Claude on one session.

What it cost to learn:

- **The CSP allows inline styles now.** xterm.js 6.0.0 writes its theme and cell size into `<style>`
  elements it creates at run time, and true-colour cells through `setAttribute("style", …)` (both
  read in its `lib/xterm.js`); it takes no nonce. `style-src` therefore has `'unsafe-inline'`.
  Scripts are still `'self'` only, and with connect, image and font sources closed to the network an
  injected style has nowhere to send anything.
- **xterm measures its cell once, when it opens.** It does not watch `document.fonts`, so a terminal
  opened before JetBrains Mono loads keeps the fallback font's metrics. The session opens xterm only
  after `document.fonts.load` resolves.
- **Output is gathered for 4 ms before it crosses.** Measured in `test/terminal.test.mjs`: 2000 lines
  from a real PTY arrive in 2–3 messages, and in 469 when each PTY read is sent as it comes.
- **`node-pty` builds from source on Linux**, with node-gyp, which fetches Node's headers from
  nodejs.org. The agent sandbox cannot reach it; `npm_config_nodedir=/usr` builds against the local
  headers instead (measured: node-pty 1.1.0 on linux-arm64, Node 22). It is a Node-API module, so
  that one build is what the tests load in Node. Loading it in Electron is exercised by the
  Electron smoke, which needs the binary the sandbox cannot download; it runs in CI.
- **Unmeasured here, stated:** what Docker does to the process inside the container when the
  `docker exec` client is killed (no Docker in the agent sandbox), and the macOS `spawn-helper` path
  of node-pty (no Mac). Packaging the native module (`asarUnpack`) belongs to subtask 10.

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
