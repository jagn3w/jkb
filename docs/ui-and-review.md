# The UI explorer and our own code reviewer

The `ui/` pnpm workspace, which is a client of the `jkb` CLI and never a bespoke backend
(D31), and `.claude/workflows/code-review.js`, which returns structured findings so
`/review-log` can file them as tasks (D37).

Part of the jkb documentation set; see [CLAUDE.md](../CLAUDE.md) for the
conventions every session is expected to know.

## UI explorer (D31) — the `ui/` pnpm workspace

A visual tree explorer over the VFS lives in `ui/` (a **pnpm** workspace — pnpm only, never
npm). Design in `openspec/changes/jkb-ui-explorer/`. The load-bearing rule: **the UI is a
client of the `jkb` CLI** (`jkb … --json`), never a bespoke backend — anything the UI does,
the terminal can too. It drives two general CLI reads: `jkb ls [path]` (lazy tree children:
sub-namespaces + items homed there, `has_children`, hides `done`/`cancelled` unless `--all`)
and `jkb item show <uid>` (kind-aware details + a bounded preview, never the whole document).

**Chunks are hidden.** Ingest stores one `chunk` item per document fragment; listing them
buried every ingested document under its own pieces. `jkb ls`/`tree` omit `kind='chunk'` from
both the listing *and* the counts unless `-a`/`--all`, and surface the number against the
document instead (`Doc (9 chunks)`, `chunk_count` in JSON, via the new
`item::derived_kind_counts` over the `chunk --derived_from--> document` edge). `--all` now
means "show hidden entries" — terminal tasks *and* chunks — and `-a` is finally wired.

A folder's count is a **per-kind breakdown**, not a total: `ns::subtree_leaf_counts` groups
the one recursive CTE by `items.kind` and returns `BTreeMap<String, i64>`, surfaced as
`leaf_kinds` on `jkb ls --json` and rendered `8 task · 2 document` (kinds are *not*
pluralized — `items.kind` is an open vocabulary and `hypothesis` has no regular plural). A
bare total previously rendered as "N task(s) in subtree", so a folder of documents read as a
folder of tasks. `jkb ls --json` also carries `type`/`type_about` — the namespace's **own**
type (`ns::get_type_by_id`, never `effective_type`), so a typed root is labelled `[tasks]`
and the subtree that merely inherits it is not. The portable formatter is
`ui/core/src/summary.ts` (`formatLeafKinds`/`totalLeaves`), shared with any future host.

- `ui/core` (`@jkb/core`) — **portable TypeScript, no `vscode`/Node**: the `JkbClient`
  transport interface, models, the node-kind registry, detail HTML rendering. A future web
  app reuses it with an HTTP-backed client.
- `ui/vscode` (`jkb-explorer`) — the VS Code adapter: `CliJkbClient` (spawns the CLI), a
  `TreeDataProvider`, a Webview details host; bundled with esbuild. `cd ui && pnpm install &&
  pnpm run build`, then F5 in `ui/vscode`.

**The UI is gated.** `pnpm run build` from `ui/` is the single correct entry point and is what
`./scripts/check.sh` and the CI `ui` job run. Two traps it closes: esbuild **strips types
without checking them**, so the adapter's `build` runs `tsc --noEmit` first; and every
package's `typecheck` is `--noEmit`, so a bare `pnpm -r run typecheck` cannot resolve
`@jkb/core` on a clean tree (nothing emits its `.d.ts`) — `-r run build` is topological, so
core emits before the adapter checks. `check.sh` skips the UI when pnpm is missing (it lives
under `PNPM_HOME`, which `~/.zshrc` only exports for interactive shells); the CI job never
skips. **`pnpm run test` runs beside the build**, in both — `node --test` over
`ui/vscode/test/*.test.mjs`, no framework and no new dependency. A test bundles its module
with esbuild (already there for the extension bundle) and aliases `vscode` to a stub, so it
needs neither a running VS Code nor `dist/`; the stub is kept **external** to the bundle, or
the recorders the test reads are a second copy the code never touches. That fits glue over an
API we do not own: what it pins is our half — which command is asked for, with which
arguments, and the state kept between two windows.

**A session is worked in its own VS Code window, in the Claude Code extension.** "Work this
task with Claude" used to run `claude <prompt>` in a terminal — a terminal is where the whole
UX then lived. The extension is the better surface, and **`jkb.taskLauncher` lets the operator
say so or not** (`auto` | `extension` | `terminal`): `auto` is the only value that substitutes
one surface for the other, and the two explicit values are honoured or *reported*, never
quietly swapped. One function (`unreachable`) turns a failure into either a fallback or a
refusal, and **every path goes through it — including the receiving window**, which reads the
setting before it touches the extension rather than only on the way out of a failure. The
extension forces the window: its panel
derives a cwd from **`workspaceFolders[0]`** and takes no directory argument, so a chat opened
from the repo's window would work the **main checkout** — the one thing a session exists to
keep apart (D36). So the worktree has to *be* the window's folder. `vscode.openFolder` carries
no payload and the new window is a different extension host, so the prompt is handed over
through a queue in global storage (`ui/vscode/src/claude.ts`) — **one file per waiting prompt**
under `<globalStorage>/pending/`, named `sha1(realpath(worktree)).json`: click writes one, the
opened window takes it by `unlink` — which is why the extension now
activates `onStartupFinished` rather than when its view is first shown. An entry expires when
its worktree is gone, which is exactly the set that can never be delivered; no clock decides
it. Without the Claude Code extension installed the terminal remains the fallback, where it is
still the whole feature rather than a degraded one. The prompt lands in the chat input and is
not sent — the extension offers no way to submit it, and seeing what is about to be asked is
the better half of that trade.

Three things this got wrong on the first pass, all found by `/review-log` and all worth
keeping written down. **The command is `claude-vscode.primaryEditor.open`, never
`editor.open`** — the latter is `(session, prompt, column) => { if (column !==
ViewColumn.Active) setPreferredLocation("panel"); … }`, and that setter writes
`claudeCode.preferredLocation` with `ConfigurationTarget.Global`. Calling it without a column
rewrites the user's global settings on **every** hand-off, moving Claude Code out of their
sidebar for every window and project; jkb does not edit other people's configuration, which is
the same rule that kept D46 from writing a git ref. **Clicking twice asks nothing; each surface converges on what
exists.** The guard wanted here is "is an agent live on this checkout", which is *not
obtainable* — no API reports another window's folder, and D27/D36.6 deliberately refused the
heartbeat that would track a live process. `resumed` is the nearest signal and it is the wrong
one: a session opened yesterday and closed is equally resumed, so a confirmation keyed on it
fires on every ordinary return to a task and catches nothing, which is how a guard becomes a
reflex click (the D38 lesson about `--no-review`). A first attempt did exactly that, and put
the question on the *extension* path — where VS Code opens one window per folder and the
handed-over prompt lands **unsent**, so a duplicate is merely possible — while leaving the
*terminal* path, where `sendText` makes a second agent certain, to fork silently. So the
question is gone and idempotence replaces it — **on the terminal surface**, where
`startSessionTerminal` finds the session's own `claude: <session>` terminal (matched on name,
cwd and `exitStatus`, since In Flight's plain shell shares the cwd and a dead tab looks
identical to a live one) and shows it without sending, the caller saying so rather than
reporting a launch that did not happen. On the **window** surface it was finally
*measured* rather than argued: `code --new-window <folder>` twice leaves VS Code's window count
unchanged, so `forceNewWindow` does **not** defeat folder reuse and a second click focuses the
session's window. (CLI entry point, not this API — strong evidence, not a guarantee, and
written down that way.) That killed the hazard three passes had been designing against, and
exposed the real defect: nothing *activates* in a focused window, so a queued prompt sat unread
until that window was next opened cold, then fired with a stale branch and land target. The
receiving side therefore **watches** the queue (`watchQueuedPrompts`) rather than reading it
once at startup. The **`here`** surface is genuinely not idempotent — `primaryEditor.open(
undefined, …)` is a fresh conversation by design and Claude Code exposes no way to find an
existing panel — which is pinned by a test rather than asserted in prose. What holds on the two
extension surfaces: a handed-over prompt lands **unsent**, so nothing runs without a keystroke.
The terminal surface is the opposite — `sendText` executes — which is why the duplicate guard
lives there. Residual: a `claude` in *another* window is
invisible, `vscode.window.terminals` being per-window.

**The lesson worth keeping is about evidence, not windows.** This one fact was reasoned about
across three review rounds and three fix rounds; a two-command probe settled it in under a
minute. A claim was even *removed* from a comment for being unverifiable and then reinstated
two commits later as the foundation of a redesign. When a design rests on what another program
does, measure it before building on it, and record the measurement with its method and its
limits so the next reader knows what kind of thing it is. **The queue is one file per entry, not one file holding a map.** It
was a map, and every window watching the directory then meant every window did a
read-modify-write of one shared file on each delivery — whose lost update is an
already-delivered prompt resurrected and delivered twice. Per-entry files make that
unrepresentable rather than rule-avoided: there is no shared document to lose an update to, a
take is one `unlink` touching no other entry, and "must a take write the file back?" stops
being a question anyone can get wrong. Same move as `containment`'s primary key (D35). Expiry
is `sweepUndeliverable` on worktree existence, running after a write and never touching the
entry it was just handed. No migration was written and none is owed: the branch had not
landed, so no released build ever wrote the map.

Deferred: item/document body editing, drag re-placement, live refresh, in-tree search, the
web-app package.

## Code review (D37) — our own reviewer, because the host's is not composable

`/review-log` used to wrap the host's `/code-review`, which reports to the user rather than
returning findings — so the wrapper's middle step was a hole. We write the reviewer now
(design `openspec/changes/jkb-code-review/`), which makes these prompts a load-bearing input
to the project. `.claude/workflows/code-review.js` holds all of it and returns structured
findings; `/review` prints them, `/review-log` files them as tasks. Portable: it runs in any
git repo, and project context is used when found and skipped when absent.

- **Two axes, because they miss different things.** Eight **lenses** run horizontally (one
  question, whole diff); a dynamic number of **feature reviewers** run vertically (one
  capability, end to end — is it complete across its surfaces, coherent between its parts, and
  does it actually work when run?). A two-agent scout (survey ∥ context, both bounded) clusters
  the diff into functional units the way `/task-swarm`'s SCHEDULER clusters tasks. Two of this
  repo's escaped bugs were feature-level: `8a50925` shipped a frontier rule with no view, and
  `16d4e4d` ran to completion having embedded 0 of 56,402 items.
- **A ninth reviewer, `structure`, owns "is there a better way to factor this?"** — deliberately
  not a lens, because it asks whether the code is well built rather than whether it is wrong. It
  must name **what the shape costs today** or the finding is dropped, and it is verified by its
  own skeptic asking whether the change is worth the churn — the defect skeptics would refute
  every structural suggestion by construction, since a suggestion has no reproduction. The eight
  lenses are told structure is not theirs, so they stay on defects. Duplication straddles: copies
  that can **drift apart** are a defect (`contract`); copies that are merely repetitive belong to
  `structure`.
- **Quality is priced, not capped.** A structural finding can reach `concern` or `must-fix` — a
  missing seam where an invariant needed a choke point outranks a bounds check — but it earns the
  rank with evidence, on the same ladder defects use. `concern` requires citing where the cost is
  **already being paid** (the second place that had to change and did not; two live names for one
  concept); `must-fix` requires showing the mechanism that makes a property unenforceable or
  forces a coming change to go wrong. "This would be better" is a nit however well argued, and
  the ranking pass demotes it — a bar that is checkable without re-reading the code, unlike a
  ceiling, which was the blunt first version of this rule.
- **The lenses are derived from kinds of assumption, not from our bug history** — a defect is a
  violated assumption, and each kind has a testing discipline that exists because nothing else
  finds it: `input` (boundary/fuzz), `state` (state machine — *what happens the second time?*),
  `inference` (*X is treated as evidence of Y — when do they come apart?*), `contract`
  (integration — *who else touches this fact?*), `concurrency`, `failure` (fault injection),
  `scale` (load), `intent` (oracle — does it do what its name, docs, types and **tests** claim,
  including *would this test fail if the change were reverted?*). Fitting the set to our own 57
  past findings would have produced something that transfers to no other repo. **Security is
  not a ninth lens**: injection is `input`, authorization is `contract`, "this token proves that
  claim" is `inference`, and each of those three is told to cover its half; `/security-review`
  is the dedicated pass.
- **Verification is optional, and unverified is the default (D37.9).** Measured, adversarial
  verification refuted **6% of findings** while costing most of the run — so findings are filed
  **unverified**: whoever picks one up is the verification, and discovering a false one while
  already in that code costs minutes. `high` adds the three-angle vote for before merging
  something risky. There is no single-skeptic tier, because one skeptic is neither cheap nor a
  vote, and verification's value lives in the disagreement between angles.
- **Three tiers, and the axis is BREADTH OF FAN-OUT (D37.10).** Every lens question is asked at
  every tier — a question skipped is a class of bug nobody looked for. What changes is whether
  each question gets its own agent and its own reading of the diff. **`low` is the default**: up
  to three reviewers, split by feature area, each asking all ten questions against **one** reading
  of its code (~6 agents). `medium` is the old default — nine lens reviewers plus one holistic
  reviewer per functional unit (~15 agents). `high` is `medium` plus skeptics. The old default
  cost ~3M tokens and an hour per run, and its reviewers overlapped heavily: nine agents each
  loaded the same file, then their near-duplicate findings had to be merged back together by a
  consolidation pass that existed only because of the fan-out. Nine independent readings do catch
  what one reader misses, which is why `medium` remains — it is a choice to spend, not the price
  of admission. Two rules keep `low` honest: every changed file must land in exactly one area
  (a file in no area is a file no reviewer opens, which reads exactly like a clean review of it),
  and the per-reviewer finding cap **scales with what each reviewer owns**, or a cap meant to stop
  padding silently becomes the budget.
- **Skeptics are batched by file.** Loading the code around a finding is the expensive part;
  judging a second finding a few lines away is nearly free once it is in hand. So a skeptic gets
  every finding in one file, ordered by line, and returns a verdict on each — cost scales with
  how many *files* carry findings, not how many findings there are, and because each **defect**
  batch faces all three angles the vote there is a true 2-of-3. (A *quality* batch faces the one
  angle that can kill a restructuring suggestion — the defect angles would refute every one of
  them by construction, since a suggestion has no reproduction to walk.) Skeptics **default to
  refuted when uncertain** and
  the burden of proof is on the finding: `refuted=false` requires writing the verified chain,
  since "I could not find a guard" is not "I confirmed there is none on any path".
- **Severity is assigned once, at the end.** Finders each see only their own findings, so their
  severities are not comparable. One ranking pass merges near-duplicates and puts everything on
  one scale: `must-fix`/`concern`/`nit` → `!p1`/`!p2`/`!p3`, and orders the whole set strictly,
  since the reader works down it and stops when time runs out. The test for `must-fix` is **would
  you hold the merge for this**, asked of each finding on its own. **There is no target
  proportion**: an earlier version of the prompt priced `concern` as meaningless when most of a
  run shared it and capped `must-fix` at "about a fifth", which is a rule about the shape of the
  set rather than about any finding in it — and it pushes both ways, inflating one finding so it
  gets read and deflating another because its tier is crowded. What prioritizes is the **strict
  order**, which works just as well on a set that is all one severity.
- **Accuracy is measured, never fed back.** Findings are tasks, so `done` vs `cancelled` gives an
  acceptance rate, reported per run. It is deliberately not used to suppress a class: a class
  that keeps being dismissed may be a real problem the team keeps deciding not to fix, and
  silently ceasing to report it would turn that decision into an invisible one.
- **Verification was the cost, and it was a product of three terms.** The first full run cost 153
  agents and 6.5M tokens on a 2,851-line diff, ~85% of it verification: `findings (69) × skeptics
  (3, on everything) × context per skeptic (a whole 5,271-line main.rs)`. Each term multiplied the
  others. All three are bounded now — a per-reviewer finding cap (which also improves output:
  forced to pick five, a reviewer reports its five best rather than padding), batching by file,
  and bounded reading (`grep -n` the enclosing function, never a large file end to end). Findings
  past the verify cap are reported `unverified`, never dropped, or a budget limit would look like
  a clean review. Roughly, on a 1,000-line diff: **`low` ≈ 6 agents**, `medium` ≈ 15, and `high`
  adds three agents per file carrying findings. Above ~2,000 changed lines, several smaller ranges are both cheaper
  and a better review — a reviewer reasoning about 3,000 lines at once reasons worse about each.
