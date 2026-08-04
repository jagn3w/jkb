# Driving jkb from an agent

jkb is a local knowledge base modelled as a **virtual filesystem**: logical namespaces are
directories, items are files, and typed edges + tags cross-cut them. Everything an agent
needs is on the `jkb` CLI — reads, writes, search — and every mutation is audited and
undoable. Run `jkb guide` for this cheat-sheet inline, or `jkb <cmd> --help` for any command.

## Conventions (rely on these)

- **`--json`** — every read command emits machine-readable JSON. Parse that, not the human
  text. Pair it with the command, e.g. `jkb --json find --kind task`.
- **`--global`** — ignore the cwd-based *ambient* namespace scope and act over the whole KB.
  Without it, listing/search commands default to the namespace mounted at your current
  directory (or everything, if the cwd isn't under a mount).
- **Exit codes** — `jkb grep` exits **1 when nothing matched** (0 = found), so it composes in
  shell `if`/`&&`. Looking up a missing uid errors (nonzero). Otherwise success is 0.
- **Namespaces** — the fixed top-level layout is `repos/<repo>/…` (a repo's own content),
  plus `tasks/`, `media/`, `references/`, `memory/`, `_sys/`. Paths use `/`; `ns:foo/**`
  in the DSL matches a subtree.

## Orient (read-only)

| Command | What it gives you |
|---|---|
| `jkb tree [path]` | A recursive map of a subtree (folders + per-folder item counts) in one call. Start here. |
| `jkb ls [path] [-l -R -t -a]` | A namespace's direct children; `-l` long, `-R` recursive, `-t` by-time, `-a` include done/cancelled. |
| `jkb recent [path]` | The most-recently-updated items — what changed lately. |
| `jkb find [path] --kind K --tag f=v --status S` | Structured (typed) item search; compiles to the query DSL. |
| `jkb grep <pattern> [path] [-i -l -c]` | Literal-substring content search; `uid:line:text`; exit 1 on no match. |
| `jkb query "<DSL>"` | Full query DSL: `kind: tag: status: resolution: ns: is:ready is:frontier is:tombstone priority<= due<= …` (negate with `-tag:f=v` / `-kind:k`). `--count` for a total. |
| `jkb search "<terms>" --route hybrid` | Ranked vector/FTS retrieval (needs a running embedder). |

**grep vs find vs search:** use `find`/`query` when you know the *kind/tag/status* (structured,
exact); `grep` for a *literal string* in content; `search` for *fuzzy/semantic* ranking.

## Read one item

- `jkb cat <uid>` — the raw body to stdout (pipe it; no metadata, no truncation).
- `jkb stat <uid>` — compact metadata (kind, namespace, tags, timestamps), no body.
- `jkb item show <uid>` — metadata + a bounded body preview.

## Tasks

- `jkb task add "text !p1 @2026-07-15 +ns #facet=value"` — quick-add (priority / due / place / tag).
- `jkb task next [DSL]` — the ready frontier (unblocked tasks, by priority then due).
- `jkb task show <uid>` — the full task body.
- `jkb task set <uid> --status open|in_progress|needs_review|done` (`blocked` is derived, never set).

## Working tasks in parallel (a session is a git worktree)

Several tasks can be worked at once without sharing a checkout. Each session has its own
worktree and branch, and its task is claimed, so nothing else — another terminal, a swarm
run — starts the same one.

- `jkb task work <uid>` — open (or return to) the task's session. Work and **commit** inside
  the worktree it prints, and nowhere else. Running it twice returns the same session.
- `jkb task land <uid>` — rebase the session onto its target branch, run the repo's gate, and
  on green mark the task done and clean the session up. Landing is serial, so a red gate
  means *your* branch broke the integrated result.
- `jkb task abandon <uid>` — drop the session and reopen the task (the branch is kept).
- `jkb task sessions` — what is in flight, and which sessions nobody is sitting in.
- `jkb task gate ["<cmd>"]` — the command that verifies a landing here (remembered per repo).

If you are working *inside* a session, landing is the human's call: commit your work and say
so. Do not mark the task done, and do not merge or rebase onto the target yourself.

## Recover a previous version

File sync stores the bytes of **every version it settles**, and blobs are never deleted — so
a bad write that already landed on disk is recoverable.

- `jkb history <path>` — that file's settled versions, newest first.
- `jkb blob ls --contains "<a line you remember>"` — find the version that still has it.
- `jkb blob cat <hash>` — its raw bytes to stdout; pipe to a file or `diff`.

## Follow the graph

`jkb related <uid> [--edge <type>] [--depth N] [--direction out|in|both]` walks the typed
edges out from an item. Use it when an item's own body doesn't tell you enough: what it
depends on, what killed it, what it answers, what was derived from it.

## Investigations (long-running work with durable state)

An **investigation** is a typed namespace under `memory/…` holding a graph of units —
hypotheses, experiments, observations, dead ends. It exists so work survives past one
context: you can pick up an investigation you have never seen and know where it stands.

**Orienting, in this order.** Do not skip step 2.

| Step | Command | Why |
|---|---|---|
| 1 | `jkb inv digest <ns>` | The state digest: all three buckets + whether the goal is met. |
| 2 | `jkb inv tombstones <ns>` | Dead ends **and what killed each**. This is what stops you re-treading. |
| 3 | `jkb inv frontier <ns>` | Live, unblocked units, ranked. Pick your work here. |

Then, before you start on a unit: `jkb inv retread <uid>` (has anything near this already
been ruled out?) and `jkb related <uid>` (how does it connect to the goal?).

**Recording what you learn.** Every write is audited and undoable.

- `jkb inv verbs <ns>` — the strategy's verbs. This is the normal way to add units; the verb
  creates the right kind of unit and links the right edge for you.
- `jkb inv do <ns> <verb> "text" [--on <uid>] [--weight N] [--tag f=v]`
- `jkb inv evidence <uid>` — the signed `supports` − `contradicts` balance for a unit.
- `jkb inv link <src> <edge> <dst>` — an edge no verb covers.
- `jkb inv resolve <uid> <resolution>` — `unresolved` / `success` / `dead_end` / `superseded`
  / `abandoned`.

**The one rule that matters: never delete a dead end.** Resolve it `dead_end` and link what
killed it (`refutes`, `rules_out`). A dead end with no edge saying *why* teaches the next
agent nothing; a deleted one costs them the day it took you to rule it out.

Starting one: `jkb inv ls` lists yours and the available strategy types;
`jkb inv new <type> <name> --goal "…"` creates one (homed under `memory/<repo>/<name>`
inside a repo). `jkb inv digest` again at the end of a session so the next agent lands on
current state.

## Write (audited + undoable)

- `jkb item edit <uid> [--append] <text>` — replace or append an item's content.
- `jkb item rm <uid>` — delete an item and its cascade (placements, edges, tags, binding).
  `jkb undo` restores all of it. Refuses investigation memory (a `dead_end`/`superseded`
  tombstone, or a unit an edge records as killed) and synced-file items sync would recreate;
  `--force` overrides.
- `jkb task tag add <uid> facet=value` — apply a tag; `jkb task depend <uid> <dep-uid>` — add a dep.
- `jkb undo` — revert the last change (yours or another agent's).

Prefer structured reads (`find`/`query`) over scraping text, always add `--json`, and scope
with a path (or lean on the ambient cwd namespace) so results stay small and relevant.
