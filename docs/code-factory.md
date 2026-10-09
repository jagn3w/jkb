# Code Factory — the jkb desktop app (D53)

The first standalone app on jkb: one place to **design, plan and implement** with Claude Code,
replacing the VS Code + Markdown-design workflow. Four tabs — **Design**, **Workflows**,
**Container**, **Sessions** — and one shared component, a collapsible integrated terminal.
Governs `task:jkb-code-factory-18dc73efcc248068` and its subtasks.

Part of the jkb documentation set; see [CLAUDE.md](../CLAUDE.md) for the conventions every
session is expected to know. This is the first app with jkb as its substrate and will not be the
last, so the rules in **D53.1** are written to be inherited, not just followed here.

Status: **decided; subtasks 1 (the scaffold), 2 (the terminal), 3 (the design data model), 4 (the Document pane), 5 (execution plans and the Tasks pane), 6 (the Prompts pane), 7a (the Workflows tab; 7b, rewiring the workflow scripts to read their templates from jkb, is not), 8 (the Container tab), 9 (the Sessions tab) and 10 (the installed copy and Update from main) are built, the rest are not.** Each subsection names the subtask that builds it. Mark a
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
- **The daemon token is read with no link anywhere below the home** — `~/.jkb`, `daemon`, the
  `<port>` directory and the token itself are each refused if a symbolic link, the leaf opened
  `O_NOFOLLOW|O_NONBLOCK` — and must be a small regular file holding one word: `~/.jkb` is
  writable from the dev container, and a planted link at any of those would have the app send
  whatever it points at as a header. The first version checked only the leaf (`O_NOFOLLOW` guards
  the last component alone), so a linked `<port>` directory sent a file the container chose; and
  without `O_NONBLOCK` a FIFO at the path parked a libuv thread in `open()` before the
  regular-file check could run. Node has no `openat`, so the chain is walked by path before and
  after the open and the opened file must still be the one the path names (device, inode); a link
  swapped in and back out between those calls is the residual race, closed on the writer's side
  (`jkb_daemon::token::write` holds the directory by handle). A refusal is its own code,
  `token_refused` (client-side, never on the wire), shown as *jkb token refused* rather than
  *unreachable* — restarting `jkb serve` would rewrite the token and erase the evidence — and no
  failure message carries the token's absolute path across the bridge. The CLI's reader follows
  links; the app is the first host client that holds the root token in a long-lived GUI process,
  so it is the stricter one.
- **The renderer's page is the built file, or a loopback dev server on purpose.** The page main
  loads is the page it trusts with the bridge, so `ELECTRON_RENDERER_URL` (set by
  `electron-vite dev`) is honoured only with `JKB_APP_DEV_RENDERER=1` (set by `pnpm run dev` and
  nowhere else) and only as an `http://` URL on `127.0.0.1`, `[::1]` or `localhost`; set any other
  way, main refuses to start. The first version honoured any value in an unpackaged app, and every
  build then was unpackaged: an inherited `ELECTRON_RENDERER_URL=http://example.net` became a
  remote page with `window.jkb` and the root token behind it. A packaged app ignores the variable.
- **The wire constants are pinned to Rust by a test** (`ui/core/test/wire-parity.test.mjs` reads
  `DEFAULT_ADDR`, `PROTOCOL_VERSION`, `MAX_BODY_BYTES` and the `ErrorCode` variants out of the
  crates' sources). A non-2xx reply that is not a jkb `ApiError` is `unavailable` — something else
  answered — as `jkb_daemon::client` reads it, not `internal`.
- **pnpm 11 holds back releases younger than its minimum release age**, and `pnpm add` answered by
  adding an exemption to `pnpm-workspace.yaml`. The exemption was removed and slightly older
  versions pinned instead (electron 44.5.1, playwright-core 1.63.0): the gate is a supply-chain
  defence, not an obstacle.
- **CI runs the smoke under `xvfb-run`** with Ubuntu 24.04's AppArmor userns restriction lifted,
  rather than with `--no-sandbox` — the renderer sandbox is what the smoke is there to exercise.
  Not yet observed green on a runner when this was written; the agent sandbox has no Electron
  binary and no display, so there the smoke skips, saying so. **In CI a skip is a failure:** the
  job sets `JKB_REQUIRE_ELECTRON_SMOKE=1`, which turns any reason to skip into a failing test
  (`smoke-required.test.mjs` runs the smoke with its binary taken away to pin that). Before it, a
  runner that lost the binary or the display passed green having run nothing.
- **The smoke asserts the window's `webPreferences` from main** (`sandbox`, `contextIsolation`,
  `nodeIntegration`, `webviewTag`, via `webContents.getLastWebPreferences()`), not only what the
  page can see: with `sandbox: false` and context isolation intact, the page still has no
  `require` or `process`, so the renderer-side check alone passed with the sandbox off.
  `getLastWebPreferences` is undocumented; if Electron drops it the test fails rather than passes.
  The unreachable-daemon smoke writes a token for its closed port, so the status it checks comes
  from a refused connection, not from a missing token.

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

**As built (subtask 10).** Four pieces, one rule: what is installed is built by the clone's own
code, at `origin/main`, and a checkout never gets a say.

- **`scripts/lib.sh`** holds the shell half, so `scripts/tests/app-install.test.sh` drives it against
  real repositories: `app_clone_refresh` (clone from the checkout's `origin` on first use only — after
  that the clone's own `origin` is fetched, so a checkout cannot redirect it later; fetch
  `+refs/heads/main:refs/remotes/origin/main`; detach there with `--force`; `git clean -ffd`, which
  keeps ignored build state such as `node_modules`), `app_clone_check` (HEAD is `origin/main`'s tip and
  nothing is changed or untracked), `app_swap`, and `install_app`, setup.sh's step. setup.sh runs it
  after the kit (`--no-app` skips it); it is `unchanged`, and builds nothing, when the stamp already
  names `main`'s tip and the app is where it was put — setup.sh runs after every pull touching `ui/`.
- **`scripts/build-app.sh`**, run only as the clone's copy, refuses unless its own repository *is*
  `<app-home>/src` and `app_clone_check` passes, so a checkout's or worktree's copy builds nothing.
  Then `pnpm install --frozen-lockfile`, `pnpm --filter "@jkb/app..." run build` (each package
  type-checks before it emits), `pnpm --filter @jkb/app run package` (`electron-builder --dir`,
  `ui/app/electron-builder.yml`), the swap, and last the stamp `<app-home>/installed` (`commit=<sha>`),
  which is what the update counts from. On Linux it writes a desktop entry.
- **Where things go:** `~/.local/share/jkb-app/{src,installed,update.log,previous}`; the app at
  `~/Applications/Code Factory.app` (macOS) or `~/.local/share/jkb-app/app/code-factory` (Linux).
- **The swap moves, never deletes, the running app.** The copy is made beside the destination first
  (a failed copy changes nothing), the installed app is moved to `previous/` — the update is usually
  run *by* that app, which keeps reading its files until it relaunches, and it is the one-step
  rollback — and the copy renamed in. A failed rename moves the old one back.
- **In the app**, `src/main/update.ts` (`AppUpdater`) and `@jkb/core`'s `update.ts` (paths, the stamp,
  the commit log, the confirmation's words, `checkoutRefusal`). *jkb ▸ Update from main…* (on macOS
  the second menu, after the app menu Apple names after the app) fetches, shows
  `installed..origin/main` with its commits — saying so when the installed commit is not an ancestor,
  i.e. `main` was rewritten — and on a yes checks that `origin/main` is still the commit it showed,
  moves the clone there, cleans it, runs the clone's `build-app.sh`, requires the stamp to name that
  commit, then `app.relaunch()` and `app.quit()` (so `will-quit` ends the terminals and feeds). A
  failure says the installed app is unchanged and leaves the builder's output in `update.log`. Git and
  the builder run with Electron's variables and git's repository selection stripped from the
  environment, so a launching shell's `GIT_DIR` cannot point the fetch elsewhere.
- **The refusal is in main**, before any window: not `app.isPackaged` and not
  `JKB_APP_FROM_CHECKOUT=1` prints why and exits 1. One check covers `dev`, `start` and any other way
  of running `out/` from a checkout; the Electron smoke sets the variable, and has a case that the
  build refuses without it.
- **Packaging decisions** (`electron-builder.yml`): `asarUnpack` for `node-pty` (a native module and,
  on macOS, an executable `spawn-helper`, neither of which runs from inside an asar); `npmRebuild:
  false`, because node-pty is Node-API and the build pnpm made loads in Electron as it is (D53.10);
  `@jkb/core` moved to `devDependencies`, since it is bundled into `out/` and node-pty is then the
  only runtime dependency the packager collects. macOS signs **ad-hoc** (`identity: "-"`, hardened
  runtime off): renaming the bundle invalidates Electron's signature, and Apple silicon kills an
  executable whose signature is invalid; with hardened runtime on, library validation would reject
  the Electron framework signed by another team (electron-builder 26.17's own schema says both).
  `electron-winstaller`, which arrives with electron-builder, has its install script denied by name
  (`pnpm-workspace.yaml`): it serves Windows installers only, and pnpm 11 fails an install on a
  script that is neither allowed nor denied.
- *Measured here:* `electron-builder --dir` with this config, against a stand-in Electron
  distribution (`electronDist`; the real one is a GitHub download the sandbox cannot reach), found
  node-pty through pnpm and left it — `build/Release/pty.node` included — under
  `resources/app.asar.unpacked`. *Unmeasured, stated:* a real package and launch on macOS and Linux
  (the Electron download), the relaunch after a swap, and on Linux whether Chromium's sandbox starts
  under Ubuntu 24.04's AppArmor user-namespace restriction outside CI, where it is lifted (D53.2).

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
  *Extended by subtask 6:* the script now records the session as one of the design's prompts before
  it becomes Claude, the one launch script every launch shares (D53.6, `launch.ts`).
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

**As built (subtask 5, plans and the Tasks pane).** The engine is `crates/jkb-core/src/design/plan.rs`;
the ops `design.plan_create`/`plan_step`/`plan`/`plans` and the *Play* prompts are
`crates/jkb-api/src/designs/plans.rs`; `jkb design plan ls|create|step|show` and `jkb design prompt
play|task` are the CLI. In the app, `@jkb/core`'s `plan.ts` (the shapes, the requests, the archive
split, which tasks a Play pins), and `src/renderer/src/design/` `PlanColumn.tsx` (the Execution Plan
pane and history drawer), `TaskPane.tsx` and `play.ts` (the terminal specs). What was decided past the
text above:

- **Nothing about a plan is stored that the graph says.** A plan is contained by its design, its steps
  by the plan (containment position is their order), a step's tasks by the step (`jkb task add …
  --under <step>`, the ordinary subtask path, so the step is the task's parent the way a parent task
  is). **Archived is derived on every read**: a plan with at least one task, every task under it at
  any depth `done` or `cancelled`. A plan with no tasks is a draft, not finished work — the vacuous
  truth IMPLEMENTED also refuses — and a reopened task brings its plan back with no write.
- **`design.plans` takes `all`**, as `jkb ls` does for terminal tasks; the listing says how many
  archived plans it left out. The app always asks for all and splits them itself, so the drawer is
  one read with the pane.
- **The strategy is pinned before the work starts, by the app, as the operator.** A strategy (D52) is
  pinned per task (`workflow.set`, operator-only), so *Play* with a strategy pins each open task of the
  plan that does not already run it, one at a time, stopping at the first refusal so nothing starts
  under a half-applied choice; a task already on it is left alone, since a pin writes a row of its
  history. **Only an explicit pick pins.** The picker starts on "each task's own" (naming the default)
  and a *Play* left there pins nothing and sends no strategy: an unpinned task reports
  `default:<name>` while the default is listed as bare `<name>`, so treating the shown default as a
  choice once pinned every unpinned task on every Play, freezing it off any later change of the
  default (caught in review). An explicit pick that equals the default does pin those tasks — that
  freeze is what choosing asks for — and the decision is one pure helper, `playPins`, that the
  terminal and the Tasks pane's "Play pins it to" hint both use. With no pick, the prompt says no
  strategy was chosen rather than claiming the operator chose the default. The prompt names the
  strategy chosen and each task's own (`default:<name>` when unpinned),
  and tells Claude that a task it adds runs the default until the operator pins it — an agent cannot
  pin, by D52's rule. A definition is listed as `name@version` and pinned by `name` (its newest).
- **A task's *Play* is `jkb task work` in the container, then Claude in the worktree it answers.** The
  script reads the worktree from `--json` with `jq` (in the image), skipping lines that are not JSON
  (`task work` prints a note first when it cancels a pending removal); a refusal or an answer with no
  worktree stops it before Claude starts. The prompt (`design.prompt` `{kind: "task"}`) carries the
  task, its design, plan and step, the spans that step stages, and the strategy it runs — read after
  the pin, so it is the one its gates will use.
- **The Tasks pane is `task.show` and `task.edit`**: status, claim holder, strategy and transitions
  shown; the task's text edited in place (replace) or a note appended. Claims are shown, not changed
  here — a claim is a session's, taken by `task work`.
- **Plans are re-read on demand** (a design switch, the refresh button, after a Play or an edit), not
  live: no topic carries task or plan changes yet, and D53.1 rules out a poll loop.

**As built (subtask 6, the Prompts pane).** The engine is `crates/jkb-core/src/design/prompts.rs`; the
ops `design.prompt_record` (a designer's write) and `design.prompts` (a read), and the *New prompt*
prompt (`design.prompt` `{kind: "new", uid, text}`), are `crates/jkb-api/src/designs/prompts.rs`;
`jkb design prompt record|ls|new` is the CLI. In the app, `@jkb/core`'s `prompts.ts` (the shapes, the
requests), and `src/renderer/src/design/` `launch.ts` (every launch and resume as a terminal spec) and
`PromptsPane.tsx`, below the Tasks pane. What was decided past the text above:

- **A prompt's uid is its session's: `prompt:<uuid>`.** "One per session" is then the uniqueness of a
  uid, not a rule every caller must remember. Recording the same session again is the same prompt:
  nothing is written when it is recorded from where it already was, and its cwd moves when it is not
  (the terminal's toggle restarts the program on the other side, which runs the launch again from
  there). Recording it under another design is refused. The item's content is its title; the design,
  session, cwd, launch (`discuss`/`play`/`task`/`new`) and subject (the plan or task a *Play* named)
  are its metadata. The subject is checked against the launch: a plan's *Play* names one of this
  design's plans, a task's *Play* one of its tasks (under a step, or directly under the design), and a
  *Discuss* or *New prompt* names none — the pane lists the subject under the design, so one from
  another design, or of another kind, is refused (caught in review). A session id is a **lowercase**
  uuid, and another spelling is **refused, not rewritten**: the launch hands Claude the caller's
  spelling, so a record that folded case could name a session other than the one Claude was given.
  *Unmeasured:* whether Claude folds a session id's case — the refusal makes it moot, and the app mints
  lowercase ids (`crypto.randomUUID`). Looking a session up (`design.prompt_of`) folds case, since
  every record is lowercase.
- **The launch records, not the app, and it records where Claude starts.** Every launch the Design tab
  makes — *Discuss*, a plan's *Play*, a task's *Play*, *New prompt* — runs one script:
  `jkb design prompt record <design> --session <uuid> …` then `exec claude --session-id <uuid>`. The
  record takes the directory it runs in (no `--cwd`), so the cwd a resume needs is the one the session
  really has. That is what makes a task's *Play* recordable at all: its worktree is known only after
  `jkb task work` answers inside the container, and the script records from there. A record refused
  (the daemon unreachable, an unknown design) stops the script before Claude starts, so no session
  exists without its prompt. The container's credential is a coordinator grant, which holds `design`;
  an implementer's does not.
- **Resume is `claude --resume <uuid>` in the recorded cwd, in the container**, opened with the session
  id as the terminal's `sessionUuid`, so resuming a session whose terminal is still open shows that
  terminal. A session the toggle moved to the host recorded a host path; the resume carries it back
  through the repos mount, and the toggle takes it to the host again. *Unmeasured, stated:* the
  container's and the host's Claude session stores are separate, so a session started on the host
  resumes only on the host.
- **A prompt whose directory is gone is unresumable, said so in its terminal.** A task's *Play* records
  the task's worktree, which is removed when the task lands, so every task-launched prompt eventually
  names a directory that no longer exists — and `docker exec -w` into it fails before anything of ours
  runs (caught in review). So a resume's terminal starts at the repos mount's root, which always
  exists on both sides, and the script moves into the recorded directory *relative to it* (the toggle
  rebases the root and the relative path follows); outside the mount it starts at `/` and moves to the
  absolute path. A directory that is gone ends the script before Claude starts with `jkb: <dir> no
  longer exists (a task removes its worktree when it lands), so this session cannot be resumed.` It is
  **not** resumed in the design's repo instead: Claude finds a session by the directory it ran in
  (*unmeasured, stated*, the same premise D53.9's re-attach rests on), so a resume elsewhere would
  not find it. The pane still offers Resume on every row: whether a container
  directory exists is known only in the container, and asking before every render would be a poll.
- **The pane is live through the design's topic.** A record that wrote something is announced on
  `design/<uid>` as a `prompt` message (the editor's feed already holds that topic, and ignores the
  kind); main forwards it, and the pane re-reads its list — on that, a `gap`, a design switch or its
  refresh button. `design.prompt_record`'s answer carries `wrote`, so a repeat that wrote nothing wakes
  no poller (`announces_a_send`).
- ***New prompt* is an op first** (D53.1): the text is the operator's, fenced, after how Claude reads
  and edits the design; empty, Claude reads the design and asks. At most 16 KiB: it is an argument of
  the program it starts. Its record is titled by the request's first line.

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

**Scope split (operator decision, 2026-10-08).** An agent cannot write `.claude/` in its own worktree
(the harness keeps it read-only), so subtask 7 is two: **7a** builds everything below and edits nothing
under `.claude/`; **7b**, host-only, rewires `.claude/workflows/*.js` to read their prompts with `jkb
workflow agent show`. Until 7b lands the scripts still carry their own copies, and an operator copy
saved in the tab is not yet what they run.

**As built (subtask 7a).** The engine is `crates/jkb-core/src/workflow/agents.rs` with the packaged
templates in `agents.json` beside it and the operator's copies in `workflow_agents` (V025); the ops
`workflow.agents`/`agent` (reads), `workflow.agent_copy`/`agent_set` (the operator's) and
`workflow.graph` (a read) are `crates/jkb-api/src/workflows.rs`; `jkb workflow agent
list|show|copy|set|export` (`crates/jkb-cli/src/agent_cli.rs`) and `jkb workflow show --graph` are the
CLI; `jkb_fsm::Machine::table()` is the table as data. In the app, `@jkb/core`'s `workflows.ts` (the
shapes, the requests, what a save sends, both layouts) and `src/renderer/src/workflows/`
(`AgentGraphView`, `AgentPanel`, `LifecyclePane`, `contribute.ts`). What was decided past the text above:

- **Packaged templates are compiled in, not rows — "seeded" as the strategy presets are.** The file
  `crates/jkb-core/src/workflow/agents.json` is `include_str!`'d and served beside the stored copies as
  read-only rows, exactly as `PRESETS` sit beside `workflow_strategies`. Writing them into the table at
  open was rejected: the reader connection is `query_only`, a newer jkb's packaged text would need a
  migration (or a write on every open) to reach an existing database, and a row is something an
  `UPDATE` can edit — a compiled-in template cannot be edited in place by anything. The file is the one
  a contribution changes, so "the file and the tab cannot disagree" holds by construction.
- **A copy under a packaged name overrides it.** `workflow.agent <name>` answers the newest operator
  copy, else the packaged template — so once 7b lands, a script asking for `swarm-implementer` runs the
  operator's text. A copy under *another* packaged name is refused (it would silently replace that
  agent with an unrelated prompt); under a new name it is the operator's own template. Copies are
  versioned and append-only like strategy definitions, and not changelogged for the same reason.
  Reverting is `copy <name> --packaged`, a new version holding the packaged text — nothing is deleted.
  A copy records `packaged:<name>@<v>` it was taken from, so the listing says when the packaged
  template has moved on since (`behind_packaged`).
- **The templates were copied out of the scripts, not rewritten.** Each prompt function's template
  literal became a template whose `${…}` interpolations are `{{placeholders}}`; a conditional block
  (`${reviewHint ? … : ''}`) is one placeholder the script fills with the block it computes. The code
  review's shared contract is a **fragment** (`review-preamble`, included as `{{preamble}}`, not drawn
  as an agent); each lens and each skeptic angle is a template of its own, its question copied in.
  The file stores each prompt as an array of lines, so a contribution's pull request diffs line by line;
  a test holds the file to the canonical form `export` writes, so the diff is only the change.
- **Placeholders are checked when saved and when filled.** `{{name}}` of `a-z0-9_`; an unclosed `{{` or
  a malformed name is refused at save. Filling (`show --var k=v`, the op's `vars`) refuses a placeholder
  with no value and a value with no placeholder, so a script and a template that have drifted apart
  fail loudly rather than run a prompt with a hole in it.
- **Permissions are two halves, both shown.** The role's jkb op classes (`OP_GRANTS`, enforced by
  `jkb serve`) come with every template; the template's own `permissions` — where it runs (`none` /
  `worktree`), its model (none: the session's), the most it may change (`nothing`/`kb`/`git`/`code`) —
  are what the script enforces, read from the `agent()` options the scripts pass today. Stored with
  `deny_unknown_fields`: a permission a newer jkb added is refused by an older one, never dropped.
- **The agent graph is drawn from the hand-offs, per workflow.** Each template names the agents it
  hands its result to (`hands_off_to`); the tab lays a workflow's agents out by distance from those
  nothing hands to, and an arrow running back (review → implementer) bends below. The strategy picker
  drives the Lifecycle pane, which marks the states where the selected agent's role acts next — the
  agents' place in that strategy.
- **The Lifecycle pane is the table.** `workflow.graph` answers the strategy's workflow machine — each
  state with who acts next (`next_actor`), each transition with the roles the strategy's permission
  table lets fire it — and the task lifecycle machine, both from `Machine::table()`. Self-loops and
  overrides are listed under the picture rather than drawn (every live state has an override), and the
  list under it is every row.
- **Contribute runs in a fresh worktree off `origin/main`**, under the checkout's `.jkb/work/`: export
  the saved copy, commit only `agents.json`, push, `gh pr create`, remove the worktree. Never the
  operator's checkout, so neither their branch nor their uncommitted work rides along. Pinned against a
  real git repository with a local `origin` and stand-in `jkb`/`gh` (`ui/app/test/workflows.test.mjs`).
  *Unmeasured, stated:* the push and the pull request need the container's git credential and `gh`
  login, which the sandbox has neither of.

## D53.8 — Container tab: buttons over the kit, never over the checkout

Build / verify / stop / remove / install-extensions are the kit's `run.sh` flags, invoked from
the kit path (`run.sh --kit-path`), never the checkout — the D53.3 rule again. Output streams into
the integrated terminal. The tab shows what the container is: build time, source commit and
branch, image, args-hash drift. Build time/commit/branch do not exist yet: the build stamps them
as image labels (`jkb.built-at`, `jkb.source-commit`, `jkb.source-branch`), recorded in
`.container/README.md` where the container's own decisions live.

**As built (subtask 8).** The run.sh side — `--verify`, `--install-extensions` and `--status`, the labels
and why `jkb.built-at` does not change the image id — is recorded in `.container/README.md` ("The
buttons over the kit, and the image's own record of where it came from"), and pinned by
`scripts/tests/container-status.test.sh` and `run.sh --self-test`. In the app: `@jkb/core`'s
`container.ts` (the buttons as data, each one flag; `--status` parsed; what the tab says about each
standing; which button can act), main's `src/main/container.ts` (`ContainerKit`: find the kit, check
it, read `--status`, build each button's terminal spec), `window.jkb.container` on the bridge, and
`tabs/ContainerTab.tsx`. What was decided past the text above:

- **The renderer names an action; main picks the program and its flag.** `container.spec(action)`
  refuses anything that is not one of the five actions (a flag, a path, an object), and answers a
  **host** terminal spec — `[<kit>/.container/run.sh, <flag>]`, from the account's home — that the tab
  opens in the integrated terminal's drawer, so the output streams there and the drawer's restart and
  close work on it as on any terminal. When that terminal ends, the tab reads `--status` again.
- **The kit is found where `lib.sh` puts it, and asked to agree.** `DC_KIT_DIR` is under the
  *account's* home (the passwd entry, which is what `run.sh` builds its own HOME from), not `$HOME`,
  which a launching terminal can set; and the kit's own `run.sh --kit-path` must name that same
  directory before its `--status` or any button runs. No kit is an answer that says how to install one.
- **The tab's words are `run.sh`'s.** Drift is read from `--status` (`args_drift`/`image_drift`), never
  recomputed, and each finding carries the remedy `run.sh` itself prints (*Remove it, then Build*). A
  button that cannot act on the container as it stands is disabled with the reason in its title
  (Verify, Install extensions and Stop need it running; Remove needs one to exist); with no status at
  all every button is offered and `run.sh` decides. Stop and Remove ask first.
- **Re-read on demand**, like the Workflows tab: the refresh button and the end of a button's run.
  Nothing announces a change to a container, and D53.1 rules out a poll loop.
- *Unmeasured, stated:* the Electron smoke for the tab needs the binary the sandbox cannot download,
  and on a machine with a kit it reads that kit's (read-only) `--status`, since the kit is found under
  the account's home and not the smoke's.

## D53.9 — Sessions tab and the needs-input dot

- The list is `session.list` joined with notify state (`notify_sessions`: `awaiting_user` /
  `awaiting_tool`). The **red dot** — on the tab and on the session — is `awaiting_user`, fed by
  the app's own consumer group on `claude/notify`, the same stream the macOS notifier reads.
- *Jump to context* resolves a session to its worktree → task (`task.by_branch`) or design prompt.
- **Re-attach after a rebuild:** before the Container tab tears the container down it records the
  live app-owned sessions (uuid, cwd, target); after the rebuild it relaunches each as
  `claude --resume <uuid>` in its terminal. Sessions the app did not start (another editor) can be
  viewed and resumed, not re-attached — the app does not own their process.

**As built (subtask 9).** `@jkb/core`'s `sessions.ts` (the registry and the records decoded and
joined, the dot's rule, the `claude/notify` messages, and the pure halves of *Jump to context*); main's
`topicFeeds.ts` (the one long-poll loop, now shared: `designFeeds.ts` and `notifyFeed.ts` are its two
kinds), `notifyFeed.ts` and `gitPlace.ts`; `window.jkb.notify` and `window.jkb.sessions` on the bridge;
the renderer's `sessions/` (`watch.ts`, the provider every tab reads the dot from, the registry reads,
`reattach.ts`) and `tabs/SessionsTab.tsx`. What was decided past the text above:

- **The state is derived once, in Rust, and carried.** `notify.open_sessions` gained `state`
  (`notify::SessionRecord::state`, the machine's own `state_of`), so the app never re-decides
  "tool empty means awaiting the user". The dot is `awaiting_user` only: a permission prompt
  (`awaiting_tool`) is shown as *awaiting permission: Bash* on its row, without the dot, as the decision
  says. A daemon that predates the field gives no dot rather than a guessed one.
- **The feed says when, the records say what.** A message on `claude/notify` only means a session's
  record moved; the window re-reads `notify.open_sessions`, joined after the feed is, with reads that
  overlap coalesced into one more — so a post and its withdrawal arriving together, twice, or out of
  order cost a re-read and never leave a stale dot. One feed for the window, from the shell down, so
  the dot is right on every tab. Without the topic (`no_such_topic`: setup never ran) the records are
  still read on demand, and the tab says why the dot will not move by itself.
- **Jump to context is two lookups.** The design prompt is a new read op, `design.prompt_of {session}`
  (`jkb design prompt of <session>`), since a prompt's uid is its session's: *Open design* moves to the
  Design tab with that design open (the shell's `navigation.ts`). The worktree is read by **main from
  git's files** — the `.git` file's `gitdir:` line, then `HEAD` — never by running git: the app runs
  unsandboxed on the host, and a repo under the repos mount is writable from the container. Only paths
  under the repos mount are looked at (either side's spelling), the walk stops at the first `.git` and
  never leaves the mount, a `gitdir` pointing outside it is refused, links are not followed, and only
  the place (root, repo key = the root's basename as `gitrepo::key` derives it, branch) crosses back.
  The repo key and branch then go to `task.by_branch`.
- **Re-attach follows the terminal, not a guess.** Build, Stop and Remove record before their run starts
  every terminal the app opened with a `sessionUuid`, on the container, still running; where each
  session *runs* comes from the registry (`session.list`'s cwd), not the terminal's spec — a task's
  *Play* moves into its worktree after the terminal opens, and `claude --resume` finds a session by its
  directory. When the run ends and `--status` says the container is running, each recorded terminal that
  ended is relaunched in place with `claude --resume` (a closed one gets a new terminal; one still
  running was not torn down and is left alone). After a Stop or a Remove the record waits for the next
  run that leaves the container running, with *Re-attach now* and *Forget* on the Container tab. The
  record is the window's memory only: what the app owns ends with the app.
- **Read on demand, otherwise.** The registry is re-read on Refresh, the *Ended too* toggle and every
  notification change; nothing announces a session starting or ending, and D53.1 rules out a poll loop.
  The preview is the session's facts and context (and a task's text on *Show task*), not its
  transcript, which lives in Claude Code's own store and is not jkb's to serve.
- *Residual, stated:* while the app is closed its group still exists and holds what it has not read
  unreapable, until the queue removes it as idle (7 days) — the same exposure a stopped notifier's group
  has ([notifications.md](notifications.md)). The queue has no op to drop a group, so the app cannot
  leave on quit.
- *Unmeasured here, stated:* the Electron smoke for the tab (no binary in the sandbox), and a rebuild
  re-attaching against a real container (no Docker in the sandbox): `reattach.ts`'s plan is what is pinned.

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
  of node-pty (no Mac). Packaging the native module (`asarUnpack`) is subtask 10's (D53.3, as built).

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
