# jkb

Welcome to John's Knowledge Base (jkb). This is a flexible knowledge base that I'm
writing to serve several different use cases. jkb is based on an opinionated virtual
file system built on top of sqlite. Everything inside of jkb is a node in a tree, with
typed edges between nodes, but jkb is opinionated about what namespaces exist, how nodes
relate to each other, what kind of tags each node has, and how nodes are discoverable
(e.g. indices, vector search).

## What can jkb do?

- Right now, jkb's most advanced usage is in driving agents in a DAG task manager with
  git-backed work sessions and review-gated landing. This is currently the focus of
  active development. The agent lifecycle is already a formal state machine, and a
  container sandbox ships in `.container/`; next up is RBAC on jkb commands, so that
  agents can coordinate and implement tasks as independently as possible. jkb has a
  VS Code extension to facilitate coordinating agents.
- jkb also has a searchable document store that embeds documents and provides an API to
  search your documents, completely offline. This document store can hold the documents
  in jkb, or it can do bidirectional file sync, so these documents can live separately
  on disk.
- jkb provides the scaffolding for long-running agent investigations, storing facts,
  hypotheses and connections.
- jkb provides an MCP server and CLI to let agents query and update the KB.
- In the future, I'll provide several different release targets, with jkb as a basis
  for different applications running on top of a local virtual filesystem (e.g. a
  release target for a local app to hold interview transcripts).

**A note on privacy.** jkb does not log any data remotely on its own — embeddings are
computed locally by ollama, so document content never leaves the machine. The caveat
worth reading twice: **any agent you run on top of jkb has access to the whole store.**
And `jkb ingest <url>` fetches the page you point it at, so the sites you ingest may be
tracked by their owners.

> **Status:** jkb is used to run its own development — jkb's tasks, code-review
> findings, and design notes live in jkb. The v1 substrate (the virtual filesystem, the
> query engine and derived indexes, the task DAG, file sync, the CLI and the MCP server)
> is complete and covered by the test suite; the newer surface — task sessions and
> staging branches, typed namespaces, investigations, the VS Code explorer — is younger
> and still moving. There are no released binaries and no cross-version stability
> promise yet; you build from source.
>
> jkb is **single-machine-local**: one SQLite file, not safe to open concurrently from two
> machines over a cloud-sync folder (`jkb doctor` warns if it detects one). See
> [Gotchas](#gotchas) before you run two builds of it side by side.

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
`jkb ns ls` browses them, and `jkb ns type` shows or sets what a namespace may hold.

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
jkb task add "fix the flaky test !p1 @2026-07-15 +tasks/app #size=small"
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

## Working a task: sessions, review, landing

A task can carry its own git worktree, so several can be worked at once — by you, by
parallel agents, or both — without sharing a checkout, and without any of them being
started twice.

```sh
jkb task work <uid>          # open (or re-open) the task's session: a git worktree at
                             # <repo>/.jkb/work/<session> on branch task/<session>,
                             # with the task claimed so nothing else picks it up
jkb task sessions            # what is in flight in this repo
jkb task land <uid>          # rebase the session onto its target, fast-forward, run the
                             # gate, and on green mark the task done and clean up
jkb task abandon <uid>       # drop the session; the branch is kept unless you say otherwise
```

A session lands on a **staging branch** — the branch a batch of tasks integrates onto
before trunk. It is never stored: it is derived from the land targets recorded on tasks,
plus git.

```sh
jkb staging ls               # staging branches, the tasks on each, and why any is blocked
jkb task gate ./scripts/check.sh   # the command that verifies a landing (remembered per repo)
```

Landing is **review-gated**. `jkb task land` refuses a task with no recorded review, or one
whose review still has an open must-fix finding; `--no-review` overrides and records the
waiver on the task, so a bypass is visible rather than invisible.

```sh
jkb task review record --findings <ns>   # record that a review ran against this branch
```

Two more verbs close the loop when work lands outside a session:

```sh
jkb task start <uid> --branch <b> --onto <target>   # claim a task and record where it is
                                                    # being done, without opening a worktree
jkb task close-merged        # close tasks whose branch has landed in trunk (merge-commit,
                             # squash and rebase merges alike) and whose subtasks are done
```

Subtasks are first-class: `jkb task add "…" --under <uid>` creates one, and a parent
leaves the ready frontier until every subtask is terminal — you work the leaves.

## The command surface

`jkb --help` lists everything and `jkb <cmd> --help` documents each verb; `jkb guide`
prints the agent-facing cheat sheet (mirrored in [AGENTS.md](AGENTS.md)). By capability:

**Find things.** The KB is a virtual filesystem, so it answers to the commands you'd
expect:

```sh
jkb tree tasks/jkb            # a recursive map of a subtree, with per-folder counts
jkb ls repos/jkb -l -R        # children of a namespace — or of an item, expanding a
                              # document into its chunks or a task into its subtasks
jkb grep "TODO" repos/jkb     # literal-substring content search (exit 1 if none; -i/-l/-c)
jkb find --kind task --tag design=approved tasks/**   # structured (typed) search
jkb recent references         # the most-recently-updated items in a subtree
jkb cat <uid>                 # an item's raw body to stdout;  jkb stat <uid> = metadata
jkb related <uid> --depth 2   # walk the typed edge graph out from one item
```

`grep` is literal (exit-coded like real grep); `find`/`query` are structured; `search` is
ranked vector/FTS retrieval — pick by whether you know the string, the fields, or neither.

**Capture and edit.** `jkb ingest` (file, PDF, HTML, or URL), `jkb item show|edit|rm`,
`jkb view save|ls|run` for named queries.

**Tasks.** `jkb task add|next|show|set|edit`, plus `tag`, `depend`/`undepend`,
`place`/`unplace`, `bind`, `subtasks`, and `claim`/`release`/`reclaim` for the agent-claim
model. Sessions and landing are the section above.

**File sync.** `jkb mount create|ls`, `jkb sync [ns] [--watch]`, `jkb service
print|install|uninstall`. Sync stores the bytes of every version it settles, and blobs are
never deleted, so `jkb history <path>` and `jkb blob ls --contains "<a line you remember>"`
/ `jkb blob cat <hash>` are a complete recovery path for a synced file.

**Investigations.** `jkb inv new|ls|frontier|core|tombstones|do|add|link|resolve|digest|…`
drives open-ended, multi-agent knowledge work over a typed namespace under `memory/`, read
back as a ranked frontier, a confirmed core, and tombstones (dead ends are never deleted —
the graveyard is the memory).

**Maintenance.** `jkb undo [txn]` reverts the last transaction, CLI or agent. `jkb doctor
[--backup <path>] [--fix]` runs health and integrity checks. `jkb index [--sweep]` embeds
what the embedder was down for, or sweeps derived-index rows whose item is gone. `jkb ns`
and `jkb tag` manage the namespace tree and tag facets.

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

`jkb commands install` additionally writes jkb's bundled Claude Code slash commands
(`/jkb-design-pass`, `/jkb-next-task`, `/jkb-review`, `/jkb-review-log`,
`/jkb-task-swarm`) and the workflow scripts they launch into your Claude Code config, so
they travel with the binary rather than only existing inside this repo; `list` is a dry
run and `uninstall` removes them.

## Visual explorer (VS Code)

Two views over the KB live in [`ui/`](ui/README.md): an **Explorer** (a lazy namespace
tree with type-specific detail panes, inline editing, colour-coded rows, and a right-click
"work this task with Claude") and **In Flight** (staging branches, the tasks landing on
each, and why any of them cannot land yet). Both are **clients of the `jkb` CLI**
(`jkb … --json`), never a bespoke backend, so anything they do the terminal can too.
`./scripts/setup.sh` builds and installs the extension; see [`ui/README.md`](ui/README.md)
to run it from source.

## Running Claude unattended (auto mode, safely)

`./scripts/auto-mode.sh` sets up a machine posture that lets you run Claude Code — terminal
**and** IDE — with no permission prompts, inside a boundary that still holds when the model is
wrong (design D48).

```sh
./scripts/auto-mode.sh print      # the posture, as JSON (scripts/auto-mode-posture.json)
./scripts/auto-mode.sh install    # merge it into ~/.claude/settings.json (idempotent, backs up)
./scripts/auto-mode.sh check      # is the posture still intact? exit 1 if it drifted
./scripts/auto-mode.sh run        # check, then exec `claude --permission-mode auto`
./scripts/auto-mode.sh probe      # live end-to-end smoke — costs a real session
```

Two layers, because they fail differently. `--permission-mode auto` is a **classifier**: it
decides what is worth asking about, it is a model judgment, and it buys ergonomics. Claude
Code's **sandbox** (macOS seatbelt, Linux bubblewrap + seccomp) bounds what can happen when that
judgment is wrong, and it buys the guarantee. `autoAllowBashIfSandboxed` joins them: a sandboxed
command is never shown to the classifier — the OS boundary *is* the check.

Not `--dangerously-skip-permissions`: the sandbox confines Bash and everything it spawns, but
**not** Claude Code's in-process tools (Read/Edit/Write/WebFetch), so bypassing permissions
leaves a hole exactly the shape of the file-editing tools. Auto mode keeps the classifier over
precisely what the kernel does not cover, and the posture's `permissions.deny` rules close the
named paths in *both* layers at once.

**File access is an allowlist.** Writes were already default-deny (the workspace only), so
`filesystem.allowWrite` is the list: `~/repos`, `~/.jkb`, the Rust/pnpm caches, `/tmp`. Reads are
default-deny too — `denyRead: ["~", "/Volumes"]` blankets your data and `allowRead` re-opens the
work roots and the toolchain. System paths (`/usr`, `/bin`, `/Library`) are deliberately not
denied: a command that cannot read its own dynamic linker cannot run. Extend `allowRead` freely —
`check` asserts a subset, so entries you add are fine.

**What still runs outside the sandbox**, because a guarantee you cannot state is not one. The
sandbox covers Bash and everything it spawns — compilers, git, package managers, `jkb` itself.
It does **not** cover Claude Code's in-process tools:

| Runs unsandboxed | Bounded by |
|---|---|
| `Read`, `Glob`, `Grep` | `permissions.deny Read(...)` only — an unnamed path is readable |
| `Write`, `Edit`, `NotebookEdit` | permission rules + the permission scope (`additionalDirectories`, kept empty) |
| `WebFetch`, `WebSearch` | permission rules only — in-process tools are **not** gated by `strictAllowlist` |
| MCP servers | nothing: long-lived processes started at session start, never per-command wrapped |
| Hooks | nothing established — a cloned repo's hooks are code you did not write |

So the posture adds three keys aimed squarely at that column: `permissions.ask: ["WebFetch"]`
(reading anything and fetching anywhere is read-everything-send-anywhere outside the kernel
boundary — the one composition that defeats the posture, so it is the single surviving prompt;
drop that line if you would rather not be asked), `disableBypassPermissionsMode: "disable"` (the
in-process layer is the only bound those tools have, so being able to switch it off is being able
to remove them all), and `defaultMode: "auto"` so IDE sessions are prompt-free too. For a repo
that is not yours, add `--strict-mcp-config`.

`~/.claude/settings.json` is deliberately **not** writable — it *is* the posture, and an agent
that can edit it can switch off its own sandbox. `~/.claude/projects/**` stays writable, so
auto-memory still works.

**Linux and WSL work, with three differences.** The sandbox shells out to **bubblewrap + seccomp**
instead of macOS's built-in `sandbox-exec`, so `bubblewrap` and `socat` must be installed —
`auto-mode.sh check` warns if they are missing and `run` refuses to launch without them, rather
than letting you discover it when Claude Code declines to start. Unprivileged user namespaces must
be available (measured working on Ubuntu 26.04 even with
`kernel.apparmor_restrict_unprivileged_userns=1`). And `JKB_AUTO_MODE_SSH_AGENT` is **macOS-only**:
`sandbox.network.allowUnixSockets` is documented as ignored on Linux, so `run` says so instead of
emitting an overlay that would do nothing. On WSL the `/mnt` deny is the valuable one — that is
where the Windows filesystem lives.

**Both layers, in a container.** `.container/` runs Claude Code's sandbox *nested inside* a
dev container, which adds the one thing the host posture cannot express: file access that is
default-deny **by the kernel**, since an unmounted host path does not exist in the container at
all. It needs a container runtime (macOS ships none) and a non-root user plus a targeted seccomp
profile — measured requirements, not folklore: stock Docker cannot run the nested sandbox, and
neither `--privileged` nor `seccomp=unconfined` is needed. See
[`.container/README.md`](.container/README.md).

Stated consequence: with `~/.ssh` unreadable, `git push` over SSH fails inside the sandbox —
the right default for an unattended agent. Set `JKB_AUTO_MODE_SSH_AGENT=1` before
`auto-mode.sh run` to allow the ssh-agent *socket* instead: the agent can authenticate, but can
never read the key.

## Workspace

jkb is a Cargo workspace of small crates under `crates/`, plus a **pnpm** workspace under
[`ui/`](ui/README.md) (`@jkb/core` portable logic + the `jkb-explorer` VS Code adapter):

| Crate | Role |
|-------|------|
| `jkb-types`  | Shared IDs, enums, core traits, errors |
| `jkb-core`   | SQLite virtual filesystem: schema, migrations, writer-actor, repos, changelog, query engine, task DAG, investigations |
| `jkb-embed`  | Pluggable text embeddings (ollama default, `fastembed` feature) |
| `jkb-index`  | Derived indexes: sqlite-vec (vectors) + FTS5 (keyword) |
| `jkb-search` | Multi-route search (vector / FTS / hybrid) + context-expansion |
| `jkb-ingest` | Staged, idempotent ingestion; text/Markdown/PDF/HTML adapters + headless-browser URL rendering |
| `jkb-sync`   | Bidirectional `file://` mounts with pluggable serializers + file watcher |
| `jkb-mcp`    | MCP server for agents |
| `jkb-cli`    | The `jkb` binary, plus git sessions, staging, and landing |

## Gotchas

- **The database is global, and it migrates in place.** `~/.jkb/jkb.db` is shared by every
  checkout, worktree, and branch on the machine. Once a newer binary opens it and applies
  its migrations, an older binary fails at startup rather than reading a schema it doesn't
  know. If you switch between branches whose migrations differ, use `--db` / `$JKB_DB` to
  give each one its own database.
- **Design docs and review history are not in the clone.** `openspec/` (the design
  decisions `CLAUDE.md` points at) and `.codereviews/` are gitignored — they are local
  working state, not published artefacts. References to them in `CLAUDE.md` will dangle on
  a fresh clone; the code and its comments are the authority.

## Development

```sh
./scripts/fix.sh       # fmt (write) + clippy (-D warnings) + tests  ← run before committing
./scripts/check.sh     # fmt --check + clippy + tests + cargo-deny + the ui build (CI gate)
./scripts/test.sh      # tests (pass-through args, e.g. -p jkb-sync)
./scripts/clippy.sh    # clippy only
./scripts/setup.sh     # one-shot install (binary + KB scaffold + extension + watcher)
./scripts/install-extension.sh   # rebuild + reinstall just the VS Code extension
./scripts/auto-mode.sh check     # verify the unattended-Claude posture is still intact
```

The `ui/` packages build with **pnpm only** (never npm): `cd ui && pnpm install && pnpm run build`.

GitHub Actions (`.github/workflows/ci.yml`) runs the same gates on every push and pull
request, and is a **strict superset** of `check.sh` — a local green does not guarantee a
green CI. It runs clippy and the tests twice (default features, then `--features
fastembed`) where `check.sh` runs each once, and its `ui` job always runs, where
`check.sh` skips the UI when pnpm isn't on `PATH`.

Conventions: no `unsafe` (workspace `unsafe_code = "deny"`, with one scoped exception
for the sqlite-vec FFI registration in `jkb-index`), clippy `pedantic`, errors via
`thiserror` (libraries) / `anyhow` (the CLI edge), all SQL parameterized, all writes
through the single writer-actor and the changelog. See `CLAUDE.md` for the full
architecture and ways-of-working.
