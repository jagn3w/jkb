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
| `jkb query "<DSL>"` | Full query DSL: `kind: tag: status: ns: is:ready priority<= due<= …`. `--count` for a total. |
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

## Write (audited + undoable)

- `jkb item edit <uid> [--append] <text>` — replace or append an item's content.
- `jkb task tag add <uid> facet=value` — apply a tag; `jkb task depend <uid> <dep-uid>` — add a dep.
- `jkb undo` — revert the last change (yours or another agent's).

Prefer structured reads (`find`/`query`) over scraping text, always add `--json`, and scope
with a path (or lean on the ambient cwd namespace) so results stay small and relevant.
