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
  the commit range being taken, build, then install it as the app quits and relaunch (as built:
  never swapped while it runs, below). `main` is code that passed review
  and landed; the lockfile is frozen (`--frozen-lockfile`). The update never builds from a branch
  or worktree.
- Running from a checkout (`pnpm --filter @jkb/app dev`) refuses unless
  `JKB_APP_FROM_CHECKOUT=1` — the same opt-in `run.sh` uses — so it is a deliberate developer
  act, never the default.

**As built (subtask 10, restructured after review round 3).** One rule above all: **a running app
is never swapped.** An Electron app finds its helpers (renderer, GPU, node-pty's) *by path* when it
spawns them, so a bundle replaced under a running copy makes the old browser process run the new
bundle's helpers. So building and installing are two steps, and only one of them touches the
installed app:

- **`scripts/build-app.sh` builds and STAGES; it installs nothing.** Run only as the clean clone's
  copy (it refuses unless its own repository *is* `<app-home>/src` and `app_clone_check` passes —
  HEAD is `origin/main`'s tip, nothing changed or untracked). It first drops git's six
  repository-selection variables and every `ELECTRON_*` for every caller (`app_scrub_env`: the
  post-merge hook hands setup.sh a `GIT_DIR`, which pnpm's lifecycle scripts would act on). Then
  `pnpm install --frozen-lockfile`, `pnpm --filter "@jkb/app..." run build` (each package
  type-checks before it emits), the commit written to `ui/app/out/commit` (packed with `out/**`, so
  the app knows what *it* is), `pnpm --filter @jkb/app run package` (`electron-builder --dir`), and
  the result copied whole to `<app-home>/staged/{app,commit}` (copied to `staged.new` and renamed
  over the last staged copy only once complete, so a failed copy keeps that one). `--update-to SHA` is the app's way in: under the lock it
  fetches `main`, refuses unless the tip is SHA (the commit the user was shown), moves and cleans
  the clone, and builds with *that* commit's copy of the script.
- **`scripts/install-app.sh` is the one step that swaps,** and only while no copy is running
  (`app_running`, from `ps -ww -A -o args=` — `-ww` because BSD ps otherwise cuts the path to the
  terminal width; it matches any process whose arguments name the bundle prefix `<dest>/`, so
  Electron's helpers count too — on macOS they run from `Contents/Frameworks/Code Factory
  Helper*.app/`, not `Contents/MacOS/`). It checks before it starts, and `app_swap` checks again
  right before it moves the old bundle aside (the copy takes seconds; a copy opened meanwhile from
  the Dock must not lose its bundle). If one runs it swaps nothing, keeps the staged copy, and exits
  76. Otherwise `app_swap` (below) puts `staged/app` at the one place the app runs from, the stamp
  `<app-home>/installed` (`commit=`, `dest=`) names it, the staged copy is removed, and on Linux a
  desktop entry is written. `--wait-pid PID` first waits (up to two minutes) for PID and every
  process of the bundle to exit; `--relaunch` then starts the app — the new one, or the old one
  when the install failed — but never after 75 (another install is at work) or 76, and never while
  a copy runs. Every outcome is written to `<app-home>/install.result` (`status=`, `commit=`, the
  staged build read before the wait and the lock, so even a 75 or 76 names it), and
  the release, the record and the relaunch all happen in the EXIT trap, so a `set -e` failure or a
  signal ends the same way as success.
- **setup.sh (`install_app`)**: under the lock, `app_clone_refresh` (clone from the checkout's
  `origin` on first use only — after that the clone's own `origin` is fetched, so a checkout cannot
  redirect it; fetch `+refs/heads/main:refs/remotes/origin/main`; detach there with `--force`;
  `git clean -ffd`, which keeps ignored build state such as `node_modules`), then the clone's
  `build-app.sh` and `install-app.sh`, all in a subshell of its own so the lock's traps (below)
  release it however the step ends — Ctrl-C during the build included. It is `unchanged`, building
  nothing, when the stamp names
  `main`'s tip and the app is where it was put (setup.sh runs after every pull touching `ui/`);
  **`running`, building nothing, when the app is running** — the summary says to quit Code Factory
  and re-run setup.sh (or use its own update) — and `busy` when the lock is held.
- **The app (*jkb ▸ Update from main…*, `src/main/update.ts`)** runs only as the installed copy
  (its executable, links resolved, is `installedExecutable`; anywhere else it would install there
  and leave itself "up to date" for good). It fetches `main` into `refs/jkb-app/shown` — a ref no
  build or install reads, with `--refmap=` so git does not also move `origin/main`
  "opportunistically" (measured: without it the plan's fetch moved `origin/main`), so it needs no
  lock — and shows `installed..main`, where *installed* is the running copy's `out/commit` and only
  failing that the stamp. On a yes it runs `build-app.sh --update-to <sha>` while the app keeps
  working — unless that sha is already staged (an install that did not go through), which is
  offered as it is — requires the staged commit to be that sha, and asks *Quit and Install* or
  *Install When I Quit*. On `will-quit` it starts `install-app.sh --wait-pid <its pid> [--relaunch]`
  detached (output in `<app-home>/install.log`). The build's output goes straight to
  `update.log`, never through a pipe into the app (review round 4 measured a detached build with
  piped output dying at its next write once the app had exited), so a quit mid-build leaves the
  build running to finish staging. **At its next start the app offers any staged build that is not
  what runs** (`pendingInstall`) — one staged by a build the user quit during as much as one whose
  install failed. `install.result` only explains why, and only when it names that build (a copy
  was running, another build or install held the lock, or a failure, with what to delete if it
  keeps failing). *Not Now* sets nothing, and the same build with the same result is not offered
  again until one of them changes (review round 5: the offer read install.result alone, so it
  missed a build staged after a quit, and pinned a result naming no build on whatever was staged).
- **The staging lock** is `<app-home>/lock`, a plain `mkdir` with `token` and `pid`, held by
  `build-app.sh`, `install-app.sh` and `install_app` (which hands its token to the two scripts in
  `JKB_APP_LOCK_TOKEN`, so they proceed under it). It serializes the clone, the staging directory
  and the swap. There is **no stale-lock breaking**: a lock left by a run that died is reported,
  with its path and holder pid, for the operator to remove. So releasing it is `app_lock`'s own job
  (review round 4: holders that had to remember an explicit unlock leaked it on Ctrl-C and on `set -e`
  exits): taking it installs the holder's EXIT/INT/TERM/HUP traps (and ignores SIGPIPE, so a write to
  a closed pipe fails through `set -e` rather than killing the holder past its trap). The traps go
  in before the `mkdir`, and EXIT releases only once the lock is marked as the shell's (with or
  without its token written yet). Only SIGKILL leaves it, or a signal in the instant between the
  `mkdir` and the statement after it — which the app says, naming the lock, when its build timeout
  had to SIGKILL the build.
  Busy is exit 75 in both scripts, and the message names the lock and its holder; any other lock
  error is a plain failure.
- **The swap replaces only what jkb installed.** `app_swap` replaces something already at the
  destination only when the stamp's `dest=` names it (a hand-built or foreign app refuses, so jkb
  never moves aside, and later deletes, what it cannot show it wrote). Before anything moves it
  rewrites the stamp to name the destination (keeping the old commit), so if the final stamp then
  cannot be written the app is still jkb's and the next install, seeing a stale commit, replaces
  it; and on every failure it puts the stamp back as it found it (review round 5: a failed *first*
  swap left it vouching for an empty destination, so a later install would have moved aside, and
  deleted, whatever the user put there). A stamp that cannot be written at all refuses the swap.
  The copy is made beside the destination first, under the fixed name `<dest>.new` (so an
  interrupted run's half copy is cleared by the next one); the old `previous/` goes aside to
  `previous.trash` by one rename, the running-copy check runs, and only then is the installed app
  renamed to `previous/` (the one-step rollback) and the copy renamed in; the trash is deleted
  after. Deleting the old `previous/` first took seconds — a window between the check and the
  rename (review round 5). A failed rename moves everything back.
- **One place to install, and it is the place the app accepts.** `--dest` (install-app.sh) and an
  `--app-home` other than `~/.local/share/jkb-app` (both scripts) are for the tests only
  (`JKB_APP_BUILD_TEST=1`): a copy installed anywhere else refuses to start, and a stamp naming it
  would make every later install refuse the real one. The app's `installedAppDir` /
  `installedExecutable` and lib.sh's `app_default_dest` / `app_executable` / `app_default_home` are
  held equal by a test, as are the exit statuses (`BUILD_EXIT`, `APP_EXIT_*`) and the environment
  scrub list (`GIT_SELECTION`, `APP_GIT_SELECTION`).
- **One home: the account's.** Both sides take it from the user database, not `$HOME`: the app's
  `accountHome()` (`os.userInfo().homedir`) and lib.sh's `app_account_home` (`~user`, expanded by
  getpwnam), which `app_default_home`, the destination and setup.sh all use. Review round 4: an
  install made under a `HOME` that is not the passwd home landed where the app would never accept
  it. A test sets `HOME` elsewhere and requires the passwd home.
- **Where things go:** `~/.local/share/jkb-app/{src,staged,installed,update.log,install.log,previous,lock}`;
  the app at `~/Applications/Code Factory.app` (macOS) or `~/.local/share/jkb-app/app/code-factory`
  (Linux).
- **The app's processes.** Git and the builder run with Electron's variables and git's repository
  selection stripped, `HOME` the account's, and — git and the build only — `GIT_TERMINAL_PROMPT=0`
  (a credential prompt on the terminal the app was started from would hang rather than fail). Not
  the install step: on Linux it relaunches the app, which would inherit it and pass it to every
  terminal, turning git's credential prompts off for the whole session (review round 5). Each build is its own process
  group, and a timeout stops the whole group (SIGTERM, then SIGKILL); `describeEnd` words a timeout,
  a signal and an exit apart. A build that fails installs nothing, because a build never installs.
- **The refusal is in main**, before any window (`startupRefusal`, a plain function under test):
  unless it is the installed copy — packaged, *and* its executable is `installedExecutable` — or
  `JKB_APP_FROM_CHECKOUT=1` is set, it prints why and exits 1. One check covers `dev`, `start`, any
  other way of running `out/` from a checkout, a package electron-builder left in a checkout's
  `dist/` (`app.isPackaged` alone let that run), and a copy moved elsewhere such as
  `/Applications`; the message names the installed copy. The Electron smoke sets the variable, and
  has a case that the build refuses without it.
- ~~**The lock-supervised in-place update** (review rounds 1–2).~~ *Superseded, review round 3.*
  The first design swapped the app in place while it ran and relaunched at once; making that safe
  grew a shared lock with holder and builder pids, stale-lock breaking with a rename-back for the
  break race, a `--replacing-running` exemption, exit codes for "swapped but unstamped", and a
  cancel-on-quit of the in-flight build. **Each review round found defects in the locking the round
  before had added** (round 2: a quit left a live build under a lock that read as stale, and two
  breakers could unlock the winner; round 3: the exemption let an update swap under a second
  instance, and the shell and TypeScript lock twins disagreed on EPERM). The operator stopped
  patching and removed the cause: nothing swaps a running app, so nothing needs to supervise one.
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
  (the Electron download); `install-app.sh` started from a real quitting Electron app and its
  relaunch (`open` on macOS, the executable on Linux — the tests drive it with a stand-in process
  and a stub executable, Linux only); `out/commit` read back from inside the packaged asar
  (`files: out/**` packs it; main reads it at `join(__dirname, "..")`); `app_running` against a
  real Electron process tree on macOS (the macOS paths are exercised only through
  `app_executable`/`installedExecutable` agreement here); and on Linux whether Chromium's sandbox starts
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
- **Every update is a `write_txn` with a changelog entry** (`Entity::DesignUpdates` — the name as built), so it is
  audited like any mutation. **`jkb undo` of an update is a new forward update** that reverts it
  (delete what it inserted, restore what it deleted — the Yjs `UndoManager` construction, done in
  `yrs`), never a row deletion: deleting history from a CRDT corrupts every peer that already
  merged it.
- **Live co-editing:** the app applies a local edit as `design.apply` (the update bytes,
  base64); the daemon appends it and publishes on topic `design/<uid>`; every subscriber fetches
  what it lacks from the table (`design.state` since its state vector) and merges that — the
  announcement is a hint, never the bytes merged (subtask 4, below). Updates are idempotent and
  commutative, so at-least-once delivery is enough.
- **Claude edits through the CLI against the version it read, and the CRDT merges.**
  `jkb design cat <uid>` prints the text with span markers **and a version token** (~~the Yjs state
  vector it was read at~~ — *superseded as built:* `<seq>.<state vector>`, because a state vector
  alone cannot rebuild the text that was read; see the as-built bullet below). `jkb design edit <uid> --base <token> --find <quote> [--occurrence n]
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
- **APPROVED is the only recorded state.** `jkb design approve <span> --base <token>` (by the reviewer the span
  names: `operator` or `claude`, enforced by D52 RBAC) records it.
- **STAGED and IMPLEMENTED are derived, never set** — the D47 rule that an op is derived, never
  chosen, applied to state. STAGED = an approved span with a `stages` edge to at least one
  execution-plan step. IMPLEMENTED = every task reachable from those steps is `done` (*as built:*
  every staged step has at least one task, and all are `done` — see below). A column
  that stored them would disagree with the edges the first time a task reopened.
- **Editing approved text demotes it.** ~~An insertion inside an approved span splits it, and the
  edited range is PROPOSED again~~ — *superseded as built:* the span's pieces show which words are
  new or removed, and the **whole span** reads PROPOSED until re-approved (a span half-approved
  would let STAGED rest on words no reviewer read); see "APPROVED is recorded on the span's item"
  below. The CLI reports the demotion. Approval attests to words, so changed words are unapproved
  words.
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
- **`design.apply` stores the change it made, not the bytes it was sent** (`Crdt::merge`: the
  transaction's own insertions and deletions, re-encoded). A peer's sync-step-2 answer
  (`encodeStateAsUpdate(doc, sv)`) carries the document's whole delete set; stored verbatim, `jkb undo`
  of it seeded the `UndoManager` with every deletion in the design and put text deleted long before
  back (review round 1; `undoing_a_full_state_update_reverts_only_what_it_changed` fails with the raw
  bytes stored). What is announced is the same delta.
- **Undoing a design's creation is refused while later work would cascade away with it** — the rule,
  its scope (designs only, by an explicit list of design-owned tables with `txn_id`) and the generic
  guard tried and dropped are recorded once, under D47 in docs/namespaces-and-sync.md.
- **A write folds old rows into the snapshot as it goes**: past 4096 rows, all but the newest 1024 are
  compacted (`compact_if_due`, from the one logged writer of rows). Every write rebuilds the document
  from its rows on the single writer thread, so one editor applying per keystroke made every write
  slower than the last (round 1). The cost is the operator's compaction's, on old rows only: a version
  older than the fold is re-read, an update older than it is not undone. Span diffing on a write is
  skipped entirely when no span has an approval.
- **APPROVED is recorded on the span's item, not in the CRDT**: `metadata.approval` holds what the
  approval attests to, the version it was made at and who made it; the reviewer the span names is on
  the item too. Any editor can write the document, so a reviewer stored there could be rewritten by any
  peer.
- **An approval attests to the span's words, by their Yjs ids** (`Crdt::attest`): it stores a snapshot
  whose only visible body characters are the span's characters in the version read. *Superseded first
  build:* a snapshot of the whole document, cut to the span's current range when diffed — so a
  `design.apply` that moved an approved span's anchors over unapproved text made that text read
  APPROVED, and `design.stage` took it (review round 1). Diffing against the attested words, a
  character is approved only if it is one of them and still present: words written inside the span
  since read PROPOSED, attested words deleted are a zero-width removed piece, and attested words left
  outside the anchors (the anchors were moved) mark the span `displaced`. Any of the three demotes the
  span, which reads PROPOSED as a whole until re-approved — **every piece of it too**, its untouched
  words included, so the editor (which draws from the pieces, `stateRuns`) agrees with `render`,
  `design.stage` and the export; removed words are still marked `removed` (round 4: per-word states
  had a demoted span's untouched words drawn APPROVED). `jkb design edit` names the spans it demoted.
  An approval recording no readable attestation reads demoted, never approved. Pinned by the proptest
  `a_span_reads_approved_only_while_its_text_is_the_approved_words` (random peer inserts, deletes and
  anchor rewrites; fails against the whole-document snapshot).
- **Approval is of the version the reviewer read** (`design.approve {span, base}`, `jkb design approve
  <span> --base <token>`): the attested words are taken from `base`, and the approval is refused if
  they are not the span's words now — the D53.4 read-version rule, applied to approval. *Superseded
  first build:* approve snapshotted the latest document, so an edit landing between the reviewer's read
  and the approval was approved unseen (round 1). A span covering no characters (its words all
  deleted) is refused: an approval attests to words.
- **One predicate decides whether an approval holds** (`approval_holds`: approved, not demoted,
  anchored over at least one character), behind both the span's derived state and `design.stage`'s
  gate, which refuses a span whose derived state is PROPOSED. A hand-copied gate in `stage` once lacked
  the anchored clause and staged a span that read PROPOSED (round 1).
- **A span is staged only into a step of its own design's plans** (`plan::design_of_step`): staged into
  another design's plan, its STAGED/IMPLEMENTED derived from tasks no view of its design connects to.
  `design.stage` refuses one, and the derivation itself (`approved_state`) counts only `stages` edges
  into the span's own design's steps — the same function — because the generic edge writer (`jkb inv
  link <span> stages <step>`) writes one without asking (round 2).
- **Anchors**: the start sticks to the span's first character and the end to its last, so text typed at
  either edge stays outside and only an insertion strictly inside splits the span. Spans may not
  overlap — each piece of text is in exactly one state. The rule is one function,
  `refuse_new_overlaps` (a pair overlapping now that did not before refuses the write), asked by every
  writer of a row that can move an anchor: `finish` for edits, spans and `design.apply` — which writes
  the `spans` map as freely as `design.span` does; the rule was first checked in `add_span` alone
  (round 1) — and `revert_update` for `jkb undo`, which skipped it: undoing an update that shrank a
  span, after a neighbour was added in the room it made, laid the span back over the neighbour, and the
  pair then sat in every later write's "already overlapping" set for good (round 2).
- **Who approves**: a span naming `operator` is the operator's alone; one naming `claude` is approved by
  a Claude principal (recorded by its label) or the operator, who holds every permission. RBAC adds
  `design` (coordinator, designer) and `design_approve` (those and the reviewer); compaction is the
  operator's. A design write is no one task's (`Target::Shared`), so a principal held to one task —
  a task-scoped grant, or an attested subagent — does not write designs: the main session and unscoped
  grants do. **An approval is `Target::Free`**: the engine holds it to the reviewer the span names, and
  it is how a `reviewer`-typed subagent (attested by a harness ticket) signs off a span naming `claude`.
  Shared, that path could never run (round 1; `an_attested_reviewer_subagent_approves_a_span_naming_claude`).
- **IMPLEMENTED needs every staged step to have at least one task** under it (containment, any depth),
  all `done`: a step with no tasks has implemented nothing, so vacuous truth is refused — per step, not
  across them, or one step's finished task stood in for an empty second step (round 1). A plan's
  **archived** reads the same per-step count (`plan::step_tasks`): every step has tasks, all terminal.
  Counted over the whole plan at once, a plan with an empty step was archived — hidden — while a span
  staged into that step stayed STAGED (round 2). The two differ only on `cancelled`, deliberately: a
  cancelled task leaves nothing to do (archived) and implemented nothing (not IMPLEMENTED).
- **Live updates** go to topic `design/<uid>` with the uid's `:` spelled `.` (not a topic character),
  payload `{design, seq, update}` with the update inline up to 32 KiB. Best effort: a full or oversized
  queue costs a subscriber a `design.state` re-read, never the write. An approval or a staging writes
  no update, so each is announced as `kind = "span"`, `{design, span, state}`, and the editor re-reads
  the span states on it (round 1). `jkb undo` of an approval or a staging is announced the same way
  (`design::spans_touched` before the inversion, `announce_spans` after), or an open editor kept
  drawing the reverted state (round 2). A task finishing (IMPLEMENTED) is not announced: it is task
  status, which the design engine does not write, and the editor re-reads states after every update
  and gap.
- **A design cannot be removed** (`item::remove`, even with `--force`): its updates are not in the
  delete's snapshot, so `jkb undo` would bring back a design with no text. Nor can its creation be
  undone while later work would cascade away with it (above).
- **`jkb design` and `jkb inv` share one ambient-repo rule** (`Ops::ambient_repo`): the first segment
  after `repos/` of the ambient mount, and none for a mount elsewhere. Each carried a copy that took a
  non-repo mount's first segment for a repo (`designs/references`), and `design ls` turned a failed
  ambient lookup into listing every repo's designs (round 1).
- ~~**Unmeasured here, stated:** byte compatibility with the app's JavaScript `yjs`.~~ *Measured in
  subtask 4* (below): `ui/app/test/yjs-wire.test.mjs` drives a real `jkb` with `yjs` 13.6.33 as the
  editor — jkb's state loaded, an editor update merged after a two-unit character, a CLI edit merged
  back from `design.state --since`, and a span's anchors (`yrs` `StickyIndex` v1 bytes) resolved by
  `Y.decodeRelativePosition` to the offsets jkb reports. It held at the first run; no adapter needed.
  *Corrected in review:* CI's `ui` job has no `jkb`, so the test skipped there on every run and passed
  green. It now runs in the `check` job against the `jkb` that job builds, and `scripts/check.sh`
  runs it too; both set `JKB_REQUIRE_WIRE`, under which a missing binary fails instead of skipping.
  `ui/app/test/wire-required.test.mjs` pins that flag: with a missing `JKB_BIN` it exits non-zero
  with the flag set and skips without it. `check.sh` locates its built `jkb` once, from `cargo
  metadata`, and hands it on as `JKB_CHECK_BIN`, a name only the gate sets. In CI the shell tests now
  run after the build with that variable and `JKB_REQUIRE_BUILT_JKB`. Run before the build,
  `notify-hook.test.sh`'s `jkb notify events` cross-check found no binary and skipped green.

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
- **The occurrence is given only beside its own version's token.** It counts matches in the version
  the selection was made in, and the prompt also tells Claude to re-read with `jkb design cat`. A
  generic `--base <token> --find <quote> --occurrence k` line invited pairing that count with a newer
  token, where an earlier match inserted since makes the same count name other words, and the edit
  succeeds silently. The generic line now carries no occurrence and says to quote enough context to
  be unique in a version Claude read itself. The concrete line `--base V --find=… --occurrence k`
  says the count holds only with `V`. *Found in review.*
- **The quote is also given as a JSON string**, and a passage with whitespace or a line break at an
  edge says so. A fence cannot show those, but the quote and its occurrence count them: copying the
  visible `foo` of a selected `foo\n` matched a third time and edited the wrong line. Such a passage
  names bash's `$'…'` quoting, the shell spelling that passes a real line break, and says that an
  apostrophe inside it is written `\'`. JSON leaves `'` bare, so a transcribed quote would end early. `--find`,
  `--insert-after` and `design span --find` take values beginning with `-` (`allow_hyphen_values`),
  because a Markdown list item is a common quote. Both prompts that teach an edit (*Discuss* and
  *New prompt*) build it from one string, `designs::edit_usage`, so the two cannot drift apart. That
  string carries both the spelling and the rule to quote enough context to be unique in the version
  read.
- **The offsets are sent with the version whose text is the screen's.** The pane waits for every
  local edit to be saved, re-reads with `design.cat`, and sends that version only if its text is the
  text the selection was made in; otherwise it asks for the selection again. Every other reason
  is reported as itself: a stopped session (where *Discuss* is not offered at all), a failed read, or
  a closed pane. It is never reported as "the text changed". One *Discuss* runs at a time, and a
  second click while one waits is ignored. Each click mints a session uuid, so the terminal's dedupe
  could not catch a double click. Sending the latest version
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
  `gap`.
- **An announcement is a hint; its bytes are never merged.** On each `update` the session fetches
  `design.state` since its own state vector, so everything it merges comes from the table. `mq.send`
  is open at Hook permission with a sender-chosen kind and producer. Merging the inline bytes let any
  token that may send to the queue put text into every open editor that the table does not hold,
  stop the editor with bytes that do not decode, or reuse a real client's clock so that client's
  genuine update was dropped. Reads are coalesced (a pull asked for during a pull runs once more).
  The inline update is still published for other consumers. *Found in review.*
- **A design with no topic is joined later.** A design created before every design got its topic
  at creation answers the join with `no_such_topic`. The session retries the join on a backoff, and
  also right after its own first saved edit, which creates the topic. Once joined, it re-reads. Two app instances would share the group and split its messages:
  not a case the app guards today (one instance per machine).
- **Span states are drawn only from an answer that describes the screen.** `design.cat`'s text is
  compared with the document's; an answer read while an edit was in flight is dropped for the next.
  Between answers the drawing moves with the text. Text typed between answers is cut out of any mark it
  landed in and reads PROPOSED until the next answer. CodeMirror widens a mark over an insertion
  strictly inside it, and new words showed the APPROVED tint until a matching answer arrived; during
  steady typing, or while the daemon was away, that never happened. Text in a span is tinted with its state; text no
  span covers reads PROPOSED but is marked only by an amber bar in the margin (a line's bar is its
  least advanced state), so a fresh draft is not a page of amber; words removed from an approved span
  show struck through where they were.
- **A refused `design.apply` stops the session** (the editor turns read-only and says why) rather than
  retrying; an unreachable or busy daemon is retried with backoff and the edits kept, in order, merged
  into one update per call. **The send and the read each have their own retry.** With a shared timer,
  a failed `design.state` cancelled the pending resend of a failed edit. The edit was never sent,
  the status stayed "Saving…", and every *Discuss* waited forever (found in review, reproduced by
  `a failed pull does not cancel a failed edit's retry`).
- **The editor is read-only until the first load has merged.** Text typed into the empty document
  before the load was sent as a real edit, and landed at one end of the design's text.
- **One session per design per window, reused (`design/registry.ts`).** Edits outlive the pane
  that held them. A pane *attaches* to the registry's session for a design and *detaches* when it
  stops showing it, by picker, *Jump to context* or the tab closing. The session it leaves keeps
  sending what it holds and is disposed only once it has nothing unsent and no pane. Reopening the
  design reattaches to that same session, unless the session has stopped. A session owns the
  design's feed: it subscribes when it opens and unsubscribes when it is disposed. *Why one, not one
  per pane:* the first fix closed the old pane's session lazily. Main keeps one feed owner per window
  and topic, with no count, so when a design was reopened while the old session was still sending,
  that session's late unsubscribe ended the feed under the new one, which went silently deaf. Any
  wait on the old session also stayed blocked, and with it the one-at-a-time *Discuss* guard for
  every design. Making the overlap impossible was simpler than counting it (found in review, round
  2; pinned by `a design reopened while its old pane's edits are unsent reuses the session, and
  stays live`). A detached session also ends every wait on it: a *Discuss* pending there resolves
  as `closed`, and that is said as a notice naming the design. Its resend retries are bounded (10 by
  default). A refusal or a give-up while detached is reported as a notice naming the design, since
  no pane showed its status. Leaving a design with unsent edits says they are still being saved; that
  notice is withdrawn once they land or the design is shown again. **These notices are the
  registry's own list, each kept until dismissed**, not the tab's single notice slot. Every
  *Discuss* clears that slot, so an unread report of lost edits was erased by the next click (found
  in review, round 3). A Refresh keeps the designs it already
  has while it loads and when it fails: emptying the list for a moment detached the open design.
- **`@codemirror/language` is held at 6.12.4** (a workspace `overrides` entry): 6.13.0, published the
  day before, imports `@codemirror/streamparser` without declaring it, and the renderer did not bundle.

## D53.6 — Execution plans, tasks and prompts are items and edges

The cardinality the operator gave: **prompts (n:1) design (1:n) spans (1:n) execution plans
(1:n) tasks.**

- **Execution plan** — `kind = 'exec_plan'`, contained by the design; ordered **steps**
  (`kind = 'plan_step'`, natural language, coarse: *scaffold → database → frontend → deploy*).
  A span is staged into a step by a `stages` edge (`jkb design stage <span> <step>`). Several
  plans may be live at once when work is truly parallel. A plan every one of whose steps has tasks,
  all terminal, is **archived** (D53.5's as-built note: the per-step count IMPLEMENTED reads): hidden from listings unless asked (`--all`, the same rule `jkb ls` uses for
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
  is). **Archived is derived on every read**: a plan with at least one task, every task its steps
  list (at any depth below a step) `done` or `cancelled`. A plan with no tasks is a draft, not
  finished work — the vacuous truth IMPLEMENTED also refuses — and a reopened task brings its plan
  back with no write. Archived and the listing are **one walk**, from the steps: archived once
  counted every task contained anywhere under the plan while the listing reached only tasks under a
  step, so a task put directly under the plan held it open forever and showed nowhere (caught in
  review). What prevents such a task is the guard, not the walk: `task::add_subtask` refuses a plan
  or a span as a task's parent, so no op can put one there. One written before the guard (only by a
  pre-merge build of this change) is neither listed nor counted.
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
  terminal and the Tasks pane's "Play pins it to" hint both use. **A pick has one identity**: the
  picker holds the bare name, which is what `workflow.set` and the prompt resolve, and tasks are
  compared with what that name resolves to now (`pickOf`: the listing's `name@version`, which is what
  a task pinned to it reports). **Play pins from a fresh read**: it re-reads `workflow.strategies`
  and `design.plans` right before pinning, since the pane is not live — a task Claude added since, or
  a definition redefined since, was otherwise skipped or needlessly repinned (caught in review); the
  refresh button re-reads the strategies with the plans. **The prompt says what the tasks run, never
  what was meant to happen**: `design.prompt` pins nothing (nor does `jkb design prompt play
  --strategy`), so with a pick it says the pick covers the open tasks only when each is on it, and
  otherwise names the ones that are not. With no pick it says no strategy was chosen, and its
  answer's `strategy` is `null` rather than the default posing as a choice. The prompt names each
  task's own strategy (`default:<name>` when unpinned), and tells Claude that a task it adds runs the
  default until the operator pins it — an agent cannot pin, by D52's rule.
- **A task's *Play* is `jkb task work` in the container, then Claude in the worktree it answers.** The
  script reads the worktree from `--json` with `jq` (in the image), skipping lines that are not JSON
  (`task work` prints a note first when it cancels a pending removal); a refusal or an answer with no
  worktree stops it before Claude starts. The prompt (`design.prompt` `{kind: "task"}`) carries the
  task, its design, plan and step, the spans that step stages, and the strategy it runs — read after
  the pin, so it is the one its gates will use.
- **The Tasks pane is `task.show` and `task.edit`**: status, claim holder, strategy and transitions
  shown; the task's text edited in place (replace) or a note appended. Claims are shown, not changed
  here — a claim is a session's, taken by `task work`. **A replace names its base**: `task.edit`
  takes `expected`, the text the draft started from, and `item::edit_content` refuses the replace as
  `stale` (a new error code) when the text is no longer that — a Save once silently erased a note
  the Play session appended while the draft was open (caught in review). The pane keeps the draft,
  shows the task's text as it now is, and offers to discard the draft or keep it over that text; an
  append takes no base, since it loses nothing. The CLI has the same guard: `jkb task edit` and `jkb
  item edit` take `--expected <text>` or `--expected-file <path>`, and exit non-zero on `stale`.
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
  (a launch run again from elsewhere; the terminal's host toggle, which once did this, was cut —
  D53.10). Recording it under another design is refused. The item's content is its title; the design,
  session, cwd, launch (`discuss`/`play`/`task`/`new`) and subject (the plan or task a *Play* named)
  are its metadata. The subject is checked against the launch when the session is first recorded: a
  plan's *Play* must name one of this design's plans, a task's *Play* one of its tasks (under a step,
  or directly under the design), and a *Discuss* or *New prompt* names none — the pane lists the
  subject under the design, so a missing one, one from another design, or one of another kind is
  refused (caught in review). Recording the session again checks no subject: it writes only the cwd,
  and the plan or task leaving the design since must not stop a re-record. A session id is a **lowercase**
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
- **Resume is `claude --resume=<uuid>` in the recorded cwd, in the container — never on the host**
  (D53.9: the app runs no program on the host, and a lowercase uuid is the only id it resumes), opened with the session
  id as the terminal's `sessionUuid`, so resuming a session whose terminal is still open shows that
  terminal. A recorded host path (from before the host toggle was cut, D53.10) is carried back
  through the repos mount (`containerPathOf`, either spelling of the host's root), so the resume
  still runs in the container.
- **A prompt whose directory is gone is unresumable, said so in its terminal.** A task's *Play* records
  the task's worktree, which is removed when the task lands, so every task-launched prompt eventually
  names a directory that no longer exists — and `docker exec -w` into it fails before anything of ours
  runs (caught in review). So a resume's terminal starts at the repos mount's root, which always
  exists on both sides, and the script moves into the recorded directory *relative to it*; outside the mount it starts at `/` and moves to the
  absolute path. Since a resume's own cwd is no longer the session's directory, re-attach (D53.9),
  when the registry does not know where a session runs, reads the directory back from the resume's
  argv (`resumedDir`, `sessionResumeSpec`'s inverse) rather than the spec's cwd. A directory that is
  gone ends the script before Claude starts with `jkb: <dir> no longer exists (a task removes its
  worktree when it lands), so this session cannot be resumed.` It is
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
  A copy records what it was taken from — `packaged:<name>@<v>`, or `<name>@<v>` for another copy —
  and its **packaged base** (`packaged_base`) is found by following that chain back to a packaged
  template, so re-copying a copy onto its own name does not reset it. A copy whose text **equals**
  this jkb's packaged template is built on that version whatever its chain says, and an edit or a
  re-copy of it records `packaged:<name>@<v>` — so once the operator's own contribution is merged and
  installed, their copy is current rather than behind, and their next edit exports cleanly. The
  listing says when the packaged template has moved on since (`behind_packaged`), and when a copy
  says exactly what the packaged one says (`matches_packaged`: nothing to contribute, and
  *Contribute* is disabled).
- **A first save of a packaged template is one op.** `workflow.agent_copy` takes an optional `edit`,
  applied to the copied text and validated with it in one write. Copying and then editing as two ops
  left a copy holding the packaged text whenever the edit was refused — an override of every later
  packaged version that Revert, which only appends, cannot remove. `jkb workflow agent copy` takes
  `set`'s edit flags and sends them as that `edit`, so the CLI has the same one-op path.
- **The templates were copied out of the scripts, not rewritten.** Each prompt function's template
  literal became a template whose `${…}` interpolations are `{{placeholders}}`; a conditional block
  (`${reviewHint ? … : ''}`) is one placeholder the script fills with the block it computes. The code
  review's shared contract is a **fragment** (`review-preamble`, included as `{{preamble}}`, not drawn
  as an agent); each lens and each skeptic angle is a template of its own, its question copied in.
  The file stores each prompt as an array of lines, so a contribution's pull request diffs line by line;
  a test holds the file to the canonical form `export` writes, so the diff is only the change.
- **Placeholders are checked when saved and when filled.** `{{name}}` of `a-z0-9_`, starting with a
  letter or `_` (so `{{1st_task}}` is refused, and the refusal says why); an unclosed `{{` or a
  malformed name is refused at save. Filling (`show --var k=v`, the op's `vars`) refuses a placeholder
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
  agents' place in that strategy. The role picker offers the roles `workflow.agents` answers beside the
  templates, never a list the app keeps.
- **The Lifecycle pane is the table.** `workflow.graph` answers the strategy's workflow machine — each
  state with who acts next (`next_actor`), each transition with the roles the strategy's permission
  table lets fire it — and the task lifecycle machine, both from `Machine::table()`. Each transition
  carries `fired_by`, the one answer both `jkb workflow show --graph` and the pane print: `observed`,
  the roles, `applied` (a lifecycle move jkb makes inside the op that moves it, whose own grant
  decides), or `no one`. Two lifecycle acts do have roles: `land` is the strategy's landers (its
  `lands` toggle, what `task.land` is checked against), and `override` is whoever holds
  `task.set --status` (`OP_GRANTS`'s `task_status`) — both read from those tables, not restated. Self-loops and
  overrides are listed under the picture rather than drawn (every live state has an override), and the
  list under it is every row.
- **Contribute runs in a fresh worktree off `origin/main`**, under the checkout's `.jkb/work/`: export
  the saved copy, commit only `agents.json`, push, `gh pr create`. Never the operator's checkout, so
  neither their branch nor their uncommitted work rides along. Pinned against a real git repository
  with a local `origin` and stand-in `jkb`/`gh` (`ui/app/test/workflows.test.mjs`). Three rules the
  script keeps, each pinned there:
  - *Nothing writes `.git/config`*, which the container binds read-only: the branch is `--no-track`
    and the push has no `-u`. Measured on git 2.51.1 with the config's lock held (how a read-only
    config looks to git without a bind mount, and what the fixture does): `worktree add -b … origin/main`
    exits 255 with `could not lock config file`, leaving a stray branch; with `--no-track` it exits 0.
  - *No inherited repository*: it unsets the variables `gitrepo.rs` scrubs from jkb's git spawns, plus
    `GH_REPO`/`GH_HOST` (`pr.rs`), from one list in `ui/app/src/shared/gitEnv.ts` that a test holds to
    the Rust ones.
  - *Nothing left behind, and nothing of anyone else's removed*: the worktree directory comes from
    `mktemp -d` and the branch is named after it, `git branch` refuses one that already exists, and
    an `EXIT` trap removes the branch only once this run created it and the worktree only once this
    run's `worktree add` succeeded. Two runs forced onto one name (a stand-in `mktemp`) leave the
    live one untouched; once pushed, the branch lives on `origin`.
- **Export refuses a copy built on another version than the file holds.** `jkb workflow agent export`
  checks the template's packaged base against the entry's version in the target file and refuses a
  mismatch, naming both: otherwise a copy of v1, exported over an `origin/main` at v2, is written as v3
  and silently reverts every v2 change under a pull request describing one edit. With no copy, the
  installed packaged version is the base, so an installed jkb older than `origin/main` is caught the
  same way. The refusal says which side is behind: when this jkb's packaged version is the file's, the
  cure is to copy afresh and redo the edit; when it is not, a fresh copy would record the same stale
  version, so it says to update jkb (or build it from the file's commit) first. `--override-base` is
  the deliberate revert.
  *Unmeasured, stated:* the push and the pull request need the container's git credential and `gh`
  login, which the sandbox has neither of.

## D53.8 — Container tab: buttons over the kit, never over the checkout

Build / verify / stop / remove / install-extensions are the kit's `run.sh` flags, invoked from
the kit path (`run.sh --kit-path`), never the checkout — the D53.3 rule again. Output streams into
the integrated terminal. The tab shows what the container is: build time, source commit and
branch, image, args-hash drift. Build time, commit and branch did not exist when this was decided;
the build now stamps them as image labels (`jkb.built-at`, `jkb.source-commit`, `jkb.source-branch`) —
see *As built (subtask 8)* below — recorded in `.container/README.md` where the container's own
decisions live.

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
  opens in the integrated terminal's drawer, so the output streams there. Review s8 round 1 called this
  a convenience rather than a boundary, because `terminal.open` then took any host argv; D53.10's
  `issueHost` has since made main's spec the only host argv a window may open, so the action check is
  the gate for host programs, within the limits D53.10 states for it. main's `run.sh --status` and the terminal start from one
  environment rule (`hostEnv`), so both hand `run.sh` the same `JKB_CONTAINER_NAME`; a container
  terminal enters `containerName`'s answer, `run.sh`'s own `${JKB_CONTAINER_NAME:-jkb-dev}`, untrimmed
  (it trimmed, and a name with a trailing space split the terminals from the buttons). A container
  terminal finds `docker` by run.sh's rule too (D53.10, *docker*; round 3: a Docker Desktop named only
  in `path-keep` worked for the buttons and not for any terminal).
- **A button's terminal runs once.** It is opened with no Restart. As first built it was "a terminal
  like any other", and the drawer's Restart re-ran a finished Remove with no confirmation, with the
  tab's buttons enabled, and with no `--status` read after. The rule is the terminal reducer's: its
  `restart` on a run-once terminal returns the state unchanged, and the provider's one `rerun` starts a
  program only when the reducer changed the state — Restart, relaunch and `open`'s relaunch of an ended
  session all go through it (round 1 had put a check in each caller, which the next one would forget).
- **One action at a time, from the click.** `containerRun.ts`'s `ActionGate` is taken synchronously,
  before the first await, and released when the run's terminal ends or by any path that opens none. The
  tab's `running` state is set only after main's spec and the sessions read, so a quick second Build had
  started a second `run.sh` against the same container, whose end then went unnoticed (round 2).
- **The kit is found where `lib.sh` puts it, and asked to agree.** `DC_KIT_DIR` is under the
  *account's* home (the passwd entry, which is what `run.sh` builds its own HOME from), not `$HOME`,
  which a launching terminal can set; and the kit's own `run.sh --kit-path` must name that same
  directory before its `--status` or any button runs. No kit is an answer that says how to install one.
- **The tab's words are `run.sh`'s.** Drift is read from `--status` (`args_drift`/`image_drift`), never
  recomputed, and each finding carries the remedy `run.sh` itself prints (*Remove it, then Build*). The
  stale-kit remedy is `--status`'s `kit_refresh`, shown verbatim: the **kit's** `run.sh --install-kit`.
  The tab had composed `<checkout>/.container/run.sh --install-kit` itself — a command the operator
  pastes into a host terminal, naming a script the agent can rewrite. For an exited or created
  container with no drift the start path refuses, the tab says *Build rebuilds the image, then starts it
  again; if the build changed the image, run.sh refuses the old container: Remove, then Build* —
  both outcomes, because Build rebuilds first and `--status` cannot know whether that changes the image
  (a reinstalled kit's source labels do). Round 1 said a bare *Build starts it again*, which the path the
  tab's own kit remedy leads to then contradicted. A stale one gets its state and the stale line's
  remedy, and one paused, restarting or dead (a state the start path has no
  arm for: its `docker run` would collide with the name) is stale itself, *Remove it, then Build*. A
  button that cannot act on the container as it stands is disabled with the reason in its title
  (Verify and Install extensions need it running; Stop needs it running, paused or restarting, which
  `docker stop` all ends; Remove needs one to exist); with no status at all every button is offered and
  `run.sh` decides. Stop and Remove ask first.
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
  under the repos mount are looked at (either side's spelling), and only once `realpath` puts them under
  the mount's real path too — a link anywhere along the path, not only its last component, cannot lead
  the reader out; the walk stops at the first `.git` and never leaves the mount; a `gitdir` or a
  worktree's `commondir` resolving outside it is refused; the file read is never a link (`O_NOFOLLOW`)
  and is opened `O_NONBLOCK`, so a FIFO the container plants as `HEAD` is refused rather than hanging
  main in `open`. The resolution and the reads are separate steps — a directory swapped for a link
  between them is followed — so what that race can reach is bounded by what is read: a few hundred
  bytes, of which only a parsed ref crosses back. The root crosses in the host's own spelling of the
  repos directory, never its real path, so *Shell here* can carry it across a symlinked one. Only the place crosses back: the checkout's root, its branch, and the repo key — the
  **main checkout's** basename (for a linked worktree, the directory holding its `commondir`), because
  that is what `jkb task work` tags `repo=` with (`repo_ctx`: `gitrepo::key(gitrepo::main_root(cwd))`).
  Keying by the worktree's own directory name, as first built, found no task for any worktree session
  (review round 1). The repo key and branch then go to `task.by_branch`. A design the jump finds outside
  every `designs/<repo>` is refused with a notice rather than answered by the active repo's first design,
  and a design listing that failed is reported as failed, not as the design's absence.
- **Re-attach follows the terminal, not a guess.** Build, Stop and Remove record before their run starts
  every terminal the app opened with a `sessionUuid`, on the container, still running; where each
  session *runs* comes from the registry (`session.list`'s cwd), not the terminal's spec — a task's
  *Play* moves into its worktree after the terminal opens, and `claude --resume` finds a session by its
  directory. A task's *Play* the registry cannot place (its read failed) is not recorded at the
  terminal's directory, which for a Play is the repo root (see *Re-attach reports exactly what it
  could not record*, below).
  When the run ends and `--status` says the container is running, each recorded terminal that
  ended is relaunched in place with `claude --resume` (a closed one gets a new terminal; one still
  running was not torn down and is left alone). After a Stop or a Remove the record waits for the next
  run that leaves the container running, with *Re-attach now* and *Forget* on the Container tab. The
  record is the window's memory only: what the app owns ends with the app.
- **Read on demand, otherwise.** The registry is re-read on Refresh, the *Ended too* toggle and every
  notification change; nothing announces a session starting or ending, and D53.1 rules out a poll loop.
  The preview is the session's facts and context (and a task's text on *Show task*), not its
  transcript, which lives in Claude Code's own store and is not jkb's to serve.
- **Owning a session is running it.** A terminal owns a session only while it is starting or running
  it — the rule re-attach records by — so a terminal whose program ended neither shows *Show terminal*
  nor claims a re-attach; *Resume* starts it again in that terminal (`terminals.open` relaunches an
  ended terminal of the session in place, `planOpen`, D53.10). "May be live" is `mayBeLive`, the one
  statement re-attach and `planOpen` share. No terminal of the app can move to the host any more (the
  toggle is cut, D53.10), so a session terminal stays in the container.
- **Resume runs only in the container; the app runs no program on the host, and offers none.** The
  operator's rule: Claude sessions — Play, Discuss, Resume — always run in the container. Round 1
  resumed a host editor's session on the host, choosing the side and the directory from the holder's
  `instance` and `cwd`; round 2 found that those fields, and the session id, are written by the
  session's own hooks, so a container process could have the unsandboxed app run `claude` with a
  flag-shaped "id" (`--dangerously-skip-permissions`) in any host directory. Round 2 replaced it with a
  quoted command for the operator to copy; round 3 found the quoting was POSIX-only (a fish user could
  be handed a command that breaks out of its quotes) and that a forged row could turn the app's own
  container session into that command. Both removed, not patched:
  - **The app's own terminal decides first.** A terminal of the app that ran the session — running,
    or ended — is the app's record that it ran in the container; Resume relaunches it there, in that
    terminal, in that terminal's directory (a resume's own directory; for a task's *Play*, which moves
    into its worktree, a container-side registry row or its prompt, else refused). No registry row can
    turn it into anything else.
  - **The registry decides only for a session the app has no terminal for.** A holder whose instance
    names a boot (`#…`, which the dev container always records) resumes in the container, where a
    forged row reaches nothing the container could not already. Any other is a host session, and the tab
    says only *This session ran on the host; resume it from a terminal there* — no command, no
    directory, nothing to copy. A holder with no instance or no directory is refused with the reason;
    only a session nothing has a process for falls back to its design prompt's recorded directory, as
    the Design tab's Resume does.
- **A resume is built only from a lowercase uuid, as `--resume=<id>`.** `sessionResumeSpec` — the one
  builder every resume goes through (the Sessions tab, the Prompts pane, re-attach) — returns nothing
  for any other id, and `RESUME_SCRIPT` passes the id glued to its flag, so even a value that slipped
  past could not be read as a flag of its own.
- **Re-attach reports exactly what it could not record.** `recordAttached` returns the sessions it
  dropped with the reason — a task's *Play* the registry has no record of (or could not be read for),
  or an id that is not a uuid — and the Container tab names exactly those; a failed registry read that
  dropped nothing says nothing.
- **The app leaves `claude/notify` when it stops reading.** A new op, `mq.group_delete` (CLI:
  `jkb mq group rm <topic> <group>`), removes the app's group when the feed's last window closes
  (macOS keeps a windowless app running) and on quit, which waits up to 1.5 s for it — and for any
  removal already in flight. As first built the group stayed after every ordinary quit, holding each
  later post and withdrawal unreapable; at the topic's cap `send` is refused, so `notify.event` failed
  `queue_full` and the notifier showed nothing. A join waits for a removal still in flight and then
  asks again whether anyone still wants the feed; after quit nothing joins; and a join already in
  flight when quit begins is waited for within the same bound, and the group it created is removed —
  so a removal never lands after a join, and no join recreates the group after quit (round 3 found the
  in-flight join). A removal that does not land is written to main's stderr.
- *Residual, stated:* the group stays when the app **crashes**, when the daemon does not answer within
  the wait, and when the daemon predates `mq.group_delete` (it refuses the op `bad_request`; main logs
  it) — until the queue removes it as idle (7 days), like a stopped notifier's, or the operator runs
  `jkb mq group rm claude/notify code-factory`. The estimate, unmeasured: each prompt is a post and a
  withdrawal, and a turn ending with something on screen one more, so a few agents at ~1,500 messages
  a day reach the 10,000-message cap in about 6.7 days — inside the idle bound, so a crash during heavy
  use can still fill the topic before the group goes.
- *Unmeasured here, stated:* the Electron smoke for the tab (no binary in the sandbox), and a rebuild
  re-attaching against a real container (no Docker in the sandbox): `reattach.ts`'s plan is what is pinned.

## D53.10 — The integrated terminal

One React component used by every tab: a collapsible bottom drawer (and a popover variant for
*Discuss*), multiple tabs, xterm.js in the renderer, `node-pty` in main.

- **Default target is the container** (`docker exec -it -w <cwd> <container> …`); a **host**
  terminal is labelled as such in the tab so it is never ambiguous where a command runs.
- A terminal is created from a **spec** (`{target, cwd, argv, title, sessionUuid?}`) built by the
  caller from CLI output; the component has no knowledge of Claude or jkb.

**Superseded in part (review s2-r3, the operator's call): the per-terminal Container | Host toggle
is cut.** Three review rounds kept finding defects in the one mechanism the toggle needed — running
a renderer-built argv on the host (a confirmation dialog the renderer must not be able to answer
or mislead, POSIX quoting across sh/bash/zsh, PATH lookup through a login shell that skips
`~/.zshrc`) and tracking ends across a stop on one side and a start on the other. A terminal's
target is now FIXED when it opens. The host runs only an interactive login shell (the person types
the command) and the specs main itself builds (the Container tab's `run.sh` actions); Claude
sessions, *Play*, *Discuss* and *Resume* always run in the container. Running chosen programs on
the host, and moving a terminal between the two, is tracked as
`task:code-factory-terminal-run-progra-18dcc253c3beb828` (cut from D53.10).

**As built (subtask 2).** The contract is `src/shared/terminal.ts` (the spec, its validator
`parseSpec`, the path mapping, the events); main's half is `src/main/terminals.ts` (`TerminalHost`:
the PTYs, the command each spec runs, ending them); the renderer's is `src/renderer/src/terminal/`
(a reducer for what is open where, `run.ts` for the order one terminal starts and ends its
programs, one xterm instance per terminal that outlives its view, the drawer, the popover, and
`useTerminals()` — `open(spec, "drawer" | "popover")` is what a tab calls). The drawer folds with
Ctrl+` and is resized by its top edge; its height is a per-window convenience.

- **A container terminal is `docker exec -i -t -e TERM=… -w <cwd> <container> /bin/sh -c
  <WRAPPER_SCRIPT> jkb-terminal <record> <argv>`**, with `/bin/bash -l` (absolute, as the kit names
  every program it execs) when the spec has no argv. The wrapper writes its PID to
  `/tmp/jkb-terminals/<uuid>.pid` inside the container and `exec`s the program, so the PID is the
  program's; it refuses to start a program it could not record. The container is
  `$JKB_CONTAINER_NAME`, default `jkb-dev`: the variable `run.sh` reads, but read from the APP's
  environment — a Dock- or Finder-launched app does not see shell-rc exports, so a non-default name
  must be set where the app is launched (`launchctl setenv JKB_CONTAINER_NAME …` on macOS, then
  relaunch; or start the app from that shell). The *New terminal* button's tooltip names the
  container it enters, so a mismatch is visible. A name that is not one (`--privileged`) is refused
  rather than handed to docker as a flag. `docker` itself is looked for by **run.sh's rule**, never
  on the app's `PATH` (a GUI app's is not the shell's): each absolute line of the kit home's
  `path-keep` under the account's home, then run.sh's own fixed PATH in its order
  (`dockerSearchPath`; a test reads run.sh's `jkb_path` and `jkb_keepf` and holds them equal). It
  had a fixed list of its own, which missed a Docker Desktop named only in `path-keep` and put
  `/usr/local/bin` before `/usr/bin` (review s8 round 3).
- **A host terminal is the login shell, or a spec main built.** `TerminalHost.open` refuses a host
  spec with an argv unless main issued exactly that `(cwd, argv)` to that window (`issueHost`, as
  the Container tab's `run.sh <flag>` is handed out; D53.8), until the window closes or reloads. A
  host program is named by absolute path; nothing is looked up on the app's `PATH`.
  **What this gate is, and is not (review s2-r4).** It stops a program being routed to the host BY
  ACCIDENT — a caller building a spec with the wrong target, a session spec carried to the wrong
  side. It is not a boundary against the renderer: the host login shell (the operator's chosen
  "host session") takes `write`, so whatever drives the renderer can type any command into one it
  opens and have it run on the host with the person's account. The renderer is therefore trusted
  for host execution, as it is for every daemon op it sends. The actual defence is keeping the
  renderer the app's own code: `contextIsolation`, `sandbox`, no `nodeIntegration`, no navigation
  or new windows, every bridge call checked to come from the top frame of the app's own page
  (`isTrusted`), and a built page whose CSP allows scripts from `'self'` only (`electron.vite.config.ts`)
  with network sources closed (D53.1).
- **Ending a program is explicit, and confirmed (review s2-r1).** Closing a container terminal
  kills the `docker exec` client AND runs a second `docker exec <container> /bin/sh -c <END_SCRIPT>
  jkb-terminal-end <record>`, which sends the recorded process group SIGHUP (what a closing terminal
  sends, and what an interactive `bash`, which ignores TERM, passes on to its jobs), TERM after 2 s
  and KILL after 4 s, and exits 0 only once the recorded process is gone. A host program gets the
  same ladder from main (`HOST_END_MS`: hangup, TERM at 2 s, KILL at 4 s, unconfirmed at 5 s;
  review s2-r3). `close` answers a `TerminalEnd { target, confirmed, detail }`, and the UI says a
  program ended only when `confirmed`. Main reads a terminal through from the moment it starts
  ending it (round 3's must-fix): a program that writes as it handles its hangup — `make`'s
  "*** Hangup", a cleanup message — would otherwise block on a paused PTY nobody will acknowledge
  and never exit. Measured: a hangup handler that writes 300 KB after the PTY paused exits 0 on its
  own (with the read-through removed it was killed by TERM).
- **An unconfirmed end keeps the tab.** Closing a tab whose program runs marks it `closing`
  ("ending") until the end answers: confirmed, the tab goes; unconfirmed, it stays, reads "may still
  run" (`failed` with `mayBeRunning`) and says why on its screen; closing it again forgets it.
  `run.ts` (`TerminalRun`) remembers the last end, and nothing starts in that terminal while it is
  unconfirmed — not a re-attach, not a *Resume* reaching it through `planOpen`, which shows such a
  tab rather than relaunching it — until *Restart*, the person's explicit override. An open still in
  flight is awaited before an end, so a PTY whose open lands after its tab was closed (or its start
  was superseded) is ended like any other rather than dropped, and the start that opened it carries
  on only while it still owns that PTY — a close that took it meanwhile is not undone by marking
  the tab running again (review s2-r4). Whether a program may be running is one predicate,
  `mayBeLive` (starting, running, closing, or failed with `mayBeRunning`), used by `planOpen` and by
  re-attach (D53.9) alike. That one remembered end is all the
  tracking a fixed-target terminal needs, since its only stop-then-start is its own; round 2's
  `EndRecord` and closed-tab orphans existed for the toggle and went with it. Tested with a fake
  bridge (`test/terminal.test.mjs`, "one terminal's starts and ends"). A window's close or reload,
  and quit, send the ends without waiting: the renderer that would show them is gone. Such a
  terminal is still read until it ends, but its output and exit are no longer sent to that window —
  a reloaded page under the same id would hold them as a terminal still opening. A program
  that exits on its own leaves its record file in the container's `/tmp` (one line, gone when the
  container restarts).
- **Main trusts nothing the renderer sends.** `parseSpec` refuses unknown fields (there is no `env`
  to smuggle in), relative or NUL-bearing paths, an `argv[0]` that is an option (`-a`: bash's and
  zsh's `exec -a x rm …` runs `rm`), and oversized argv; sizes and writes are bounded. A terminal
  belongs to the window that opened it: only that window can write to it, resize it, acknowledge
  it or close it, and it is ended when that window closes or reloads. The PTYs never cross the
  bridge, only their output does.
- **Paths cross through the repos mount** (`HOST_REPOS` ⇄ `CTR_REPOS` in `run.sh`), the one
  directory both sides see. Like `run.sh`'s `container_path`, both spellings of the host root map:
  `TerminalRoots.hostReposReal` is `~/repos` with its links resolved, once, by main
  (`machineRoots`; `~` is the ACCOUNT's home, `accountHome`, as `run.sh` mounts it, not a `$HOME`
  the app was started with), so a cwd from `git rev-parse --show-toplevel` under a symlinked `~/repos` still
  lands in the repo rather than in `CTR_REPOS`. `containerPathOf` is the one statement of the host
  → container rule (the Sessions tab's resume and its "shell here" use it); `hostPathOf` the other
  way (`gitPlace`). A host badge on a tab is drawn inverted so it cannot be read as the quiet
  default.
- **A second open with the same `sessionUuid` shows the terminal while its program is starting,
  running, ending or may still be running** (`planOpen`), so a double-clicked *Play* or *Discuss*
  does not start a second Claude on one session. Once that program has exited or failed, the open
  runs the NEW spec in the same tab: a *Resume* on a prompt whose launch terminal is still in the
  drawer, dead, runs `claude --resume` rather than reselecting the corpse (review s6).
- **Output is flow-controlled.** Main counts the characters it sent a terminal that the renderer has
  not acknowledged; past `FLOW.high` (100 000) it pauses the PTY (`pty.pause()`), so a `yes` blocks
  on a full kernel buffer instead of growing the IPC queue and xterm's write buffer until the
  renderer dies, and below `FLOW.low` (5 000) it resumes. The renderer acknowledges from xterm's
  write callback — characters actually parsed — in batches of `FLOW.ackBatch` (4 096, below `low`,
  so a drawn-but-unacknowledged remainder cannot hold a terminal paused), and the event router
  acknowledges output it held for an unclaimed terminal and had to drop. Measured in
  `test/terminal.test.mjs`: a real `yes` stops within the watermark plus one flush while
  unacknowledged, sends nothing more for 300 ms, and resumes on the ack.
  **A paused PTY is read through once its program exits (review s2-r2).** node-pty 1.1.0 waits at
  most 200 ms after its child exits for the socket to drain (`DESTROY_SOCKET_TIMEOUT_MS`,
  `lib/unixTerminal.js`), then destroys it with whatever is unread, and only then reports the exit;
  a paused socket never drains, so the end of a burst was lost whenever the ack came late. While
  paused, main checks every `PAUSED_EXIT_CHECK_MS` (50) whether the program's process still exists,
  and once it does not, resumes and never pauses that terminal again; the exit's own final flush
  never pauses either, so no check is left polling. Measured: a program that writes past the
  watermark, waits for the pause, writes a 2 000-character tail and exits with no acknowledgement at
  all now delivers every character (it lost the whole tail with the check removed); an ack arriving
  after the exit is refused as for nothing. **The cost, computed, not measured in Electron:** a
  hidden or minimized window's renderer is throttled (Chromium's `backgroundThrottling`, left on),
  so its write callbacks — and so the acks — slow down, and a terminal in a background window is
  held to roughly the watermark per acknowledgement round. A long build in a hidden window runs
  slower than it would unthrottled; it does not lose output, and nothing the person has not seen
  piles up in the renderer. Turning `backgroundThrottling` off would lift the cap at the price of a
  busy hidden renderer; not chosen.
- **Pastes are chunked below `MAX_WRITE_CHARS` without splitting a surrogate pair** (`chunkWrite`),
  which would otherwise reach the PTY as two U+FFFD.
- **The 16 ANSI colours are design tokens** (`--terminal-ansi-*`, a light and a dark value each, read
  by `terminal/theme.ts`). xterm's defaults are for a dark ground; on the light one its yellow all
  but vanished. On the light ground every colour but white and bright white reads at 3:1 or better
  as text; those two stay light because programs use them as BACKGROUNDS behind black text
  (ESC[30;107m — round 2 darkened bright white to #404040, which left such text at 1.7:1; review
  s2-r3), and black on them reads at 3:1 or better. Drawn as text, xterm's `minimumContrastRatio`
  (3, `MIN_CONTRAST_RATIO`) darkens them where they meet the ground. Every dark value but `black`
  (conventionally a ground) reads at 3:1 on the dark ground. White and bright white also differ
  from `--terminal-selection` in both schemes, so a selection over them shows (light white was
  #d3d3d1, the selection's own colour; review s2-r4). Pinned in the test.

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
  `docker exec` client is killed (no Docker in the agent sandbox; moby#9098 reports it survives).
  The explicit end does not depend on the answer, but its own premises are unmeasured against real
  Docker too: that `docker exec -t` starts the program as a session and process-group leader (so the
  recorded PID names its group), that a second `docker exec` can signal it, and that `/proc` and
  `sleep 0.1` behave in the image as on Debian. What IS measured is the mechanism, against a stand-in
  `docker` that runs the program in a new session (`setsid`), the way the real client is a separate
  process from what it runs: with the end, an interactive `bash` and its background job are gone and
  the record removed; without it (an end that fails), the program outlives its client and the UI
  says "may still be running". Until it is measured live, the UI claims an ending only on the end
  command's success. Also unmeasured: the macOS `spawn-helper` path of node-pty (no Mac). Packaging
  the native module (`asarUnpack`) is subtask 10's (D53.3, as built).
- **A pre-existing flake, measured, not fixed:** "output is gathered into few messages" loses the
  tail of a fast program's output in a few runs (2 of 15 on the unchanged tree, linux-arm64, node-pty
  1.1.0): node-pty reports the exit and drops what was still in the master's buffer. Delaying the
  exit by 50 ms did not help (3 of 20), so the data is lost rather than late. The test now has its
  program pause 200 ms before exiting, since what it measures is the gathering (0 of 20 failed
  after); the loss itself is node-pty's and is not fixed here.

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
