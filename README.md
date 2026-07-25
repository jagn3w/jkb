# jkb

A local-first, agent-native knowledge base. Everything is an item in a virtual
filesystem of logical **namespaces**, with **typed tags** and a **typed graph**;
vector and keyword search are pluggable **indexes** derived from that filesystem.
The same substrate powers a DAG **task manager** and bidirectional **file sync**,
and an **MCP server** lets agents (e.g. Claude) query and update the KB.

> **Status:** v1 complete. All planned capabilities are implemented, tested, and
> usable via the `jkb` CLI and the MCP server. jkb is **single-machine-local** — a
> single SQLite file, not safe to open concurrently from two machines over a
> cloud-sync folder (`jkb doctor` warns if it detects one).

## Install & prerequisites

The one-shot installer sets everything up (and is safe to re-run after a `git pull`):

```sh
./scripts/setup.sh         # installs the jkb binary, scaffolds the KB roots,
                           # installs the VS Code extension + file-sync watcher
```

It's idempotent and each step is best-effort — a missing optional tool (VS Code,
pnpm) just warns and continues. Flags: `--no-extension`, `--no-service`,
`--no-scaffold`, `--db <path>`. To build the binary alone, use the toolchain pinned
in `rust-toolchain.toml` (rustup installs it automatically):

```sh
cargo build --release      # produces target/release/jkb
```

Two capabilities need external programs; everything else is self-contained:

- **Embeddings** (vector/hybrid search, and embedding ingested docs) use a local
  [ollama](https://ollama.com) with `nomic-embed-text`. Without it, ingestion still
  *captures* content (keyword-searchable immediately) and reports it un-embedded —
  `jkb doctor` shows the backlog, and re-running `jkb ingest` on the same source
  resumes the embed stage once ollama is up (ingestion is idempotent).
- **URL ingestion** (`jkb ingest <url>`) renders the page in a headless **Chrome /
  Chromium** (so client-side JavaScript runs). A missing browser is an actionable
  error; local files, tasks, sync, and query need no browser.

The database lives at `$JKB_DB` or `~/.jkb/jkb.db` by default (override per-command
with `--db <path>`); it is created on first use.

## Namespaces

jkb is one global KB across every repo and project, with a fixed top-level layout:
`repos/<repo>/…` for a repo's own content, and semantic roots for cross-cutting
knowledge — `media/`, `references/`, `memory/`, plus `tasks/` and `_sys/`. `jkb ns mk
<path>…` creates namespaces (the scaffold step does this for the reserved roots);
`jkb ns ls` browses them.

## Quick start

```sh
# Ingest a local file, a PDF, or a JS-rendered web page
jkb ingest notes.md --ns references
jkb ingest paper.pdf --ns references/papers
jkb ingest https://example.com/article --ns references

# Search (vector / fts / hybrid) with neighbour context, and structured query
jkb search "distinctive phrase" --route hybrid --context 2
jkb query "kind:task is:ready ns:tasks/**"

# Tasks: quick-add DSL (the first +<ns> is the task's home, the rest are mirrors),
# then the ready frontier (ordered by priority then due)
jkb task add "fix the flaky test" !p1 @2026-07-15 +repos/app #size=small
jkb task add "triage this later" --backlog   # home in the ambient repo's backlog
jkb task next                                # ready tasks; scoped to the repo when inside one

# Saved views (named queries)
jkb view save my-day "kind:task is:ready due:today"
jkb view run my-day

# Bidirectional file sync: bind a namespace to a directory, then reconcile
jkb mount create repos/app ~/repos/app --include "**/*.md" --mode bidirectional
jkb mount ls                       # list all mounts
jkb sync repos/app                 # one-shot (omit the namespace for all mounts)
jkb sync --watch                   # watch every mount until Ctrl-C
jkb service install                # run the watcher at login (launchd / systemd)

# Safety net + health
jkb undo                           # revert the last change (agent or CLI)
jkb doctor --backup ~/jkb-backup.db

# Any command with --json emits machine-readable output; --global ignores
# the cwd-based ambient namespace scope.
```

## Using it from Claude (MCP)

`jkb mcp` runs a stdio [MCP](https://modelcontextprotocol.io) server exposing read
tools (`search`, `get_context`, `query`, `list_views`, `run_view`, `task_next`) and
audited write tools (`ingest_path`, `ingest_url`, `task_create`, `task_update`) over
the same database, writer-actor, and changelog as the CLI — so agent writes are
undoable with `jkb undo`. Point an MCP client at it, e.g.:

```json
{
  "mcpServers": {
    "jkb": { "command": "jkb", "args": ["mcp"] }
  }
}
```

## Visual explorer (VS Code)

A tree explorer over the KB lives in [`ui/`](ui/README.md) — a lazy namespace tree with
type-specific detail panes, inline editing, colour-coded rows, and a right-click "work
this task with Claude". It is a **client of the `jkb` CLI** (`jkb … --json`), never a
bespoke backend, so anything it does the terminal can too. `./scripts/setup.sh` builds and
installs it; see [`ui/README.md`](ui/README.md) to run it from source.

## Workspace

jkb is a Cargo workspace of small crates under `crates/`, plus a **pnpm** workspace under
[`ui/`](ui/README.md) (`@jkb/core` portable logic + the `jkb-explorer` VS Code adapter):

| Crate | Role |
|-------|------|
| `jkb-types`  | Shared IDs, enums, core traits, errors |
| `jkb-core`   | SQLite virtual filesystem: schema, migrations, writer-actor, repos, changelog, query engine, task DAG |
| `jkb-embed`  | Pluggable text embeddings (ollama default, `fastembed` feature) |
| `jkb-index`  | Derived indexes: sqlite-vec (vectors) + FTS5 (keyword) |
| `jkb-search` | Multi-route search (vector / FTS / hybrid) + context-expansion |
| `jkb-ingest` | Staged, idempotent ingestion; text/Markdown/PDF/HTML adapters + headless-browser URL rendering |
| `jkb-sync`   | Bidirectional `file://` mounts with pluggable serializers + file watcher |
| `jkb-mcp`    | MCP server for agents |
| `jkb-cli`    | The `jkb` binary |

## Development

```sh
./scripts/fix.sh       # fmt (write) + clippy (-D warnings) + tests  ← run before committing
./scripts/check.sh     # fmt --check + clippy + tests + cargo-deny (CI gate)
./scripts/test.sh      # tests (pass-through args, e.g. -p jkb-sync)
./scripts/clippy.sh    # clippy only
./scripts/setup.sh     # one-shot install (binary + KB scaffold + extension + watcher)
./scripts/install-extension.sh   # rebuild + reinstall just the VS Code extension
```

The `ui/` packages build with **pnpm only** (never npm): `cd ui && pnpm install && pnpm run build`.

GitHub Actions runs the same gate (`.github/workflows/ci.yml`, mirroring `check.sh` with
`--all-features`) on every push and pull request.

Conventions: no `unsafe` (workspace `unsafe_code = "deny"`, with one scoped exception
for the sqlite-vec FFI registration in `jkb-index`), clippy `pedantic`, errors via
`thiserror` (libraries) / `anyhow` (the CLI edge), all SQL parameterized, all writes
through the single writer-actor and the changelog. See `CLAUDE.md` for the full
architecture and ways-of-working.
