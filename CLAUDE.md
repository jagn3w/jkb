# Working in jkb

jkb is a Rust Cargo workspace (crates under `crates/`) building a local-first,
agent-native knowledge base. The full plan lives in `openspec/` (local only, not
committed), one folder per change under `openspec/changes/<name>/`: each holds the
`design.md` for its decisions (the D-series, which runs to D51, plus the per-change
series such as `Dmem` and the branch-records `B`) and a `tasks.md`, the numbered
implementation checklist and the **source of truth for what's done**.

## Current status

- **Done:** Section 1 (workspace + guardrails), Section 2 (`jkb-types`),
  Section 3 (`jkb-core` schema/migrations), Section 4 (`jkb-core` writer-actor +
  all repos + undo/rename/backup + a `prepare_cached`/`RETURNING` perf pass),
  Section 5 (`jkb-embed`: ollama default over `reqwest::blocking`, feature-gated
  `fastembed`, `EmbedderConfig`/`build()` selection; the pure catalog guards
  `ensure_compatible`/`check_version_drift` now live in `jkb-types`, re-exported by
  `jkb-embed`; `V003` adds `embeddings_meta.model_version`),
  Section 6 (`jkb-index`: `trait Indexer` + `Dispatcher`, `VectorIndexer` over
  `sqlite-vec` with the one `unsafe` isolated in `vector.rs` behind `register()`,
  `FtsIndexer` wrapper; vec table + `embeddings_meta` catalog written by
  `ensure_ready`; per-indexer `rebuild`),
  Section 7 (`jkb-ingest`: staged idempotent `Pipeline` — capture (parse→chunk→
  items+edges+blob) in one txn, then a separate resumable embed stage; blake3 blob
  store; text/Markdown/PDF (`pdf-extract`)/HTML (`scraper`) adapters behind
  `SourceAdapter`; URL ingestion via a headless browser (`headless_chrome`, `fetch.rs`)
  → `Pipeline::ingest_url`; non-blocking capture + `index_pending`/`unembedded_count`),
  Section 8 (`jkb-core` query engine: typed `Query` AST + `evaluate()` to one
  parameterized SQL query; quote-aware DSL parser; saved views under `_sys/views`;
  `mount::ambient_namespace` for cwd-scoping),
  Section 9 (`jkb-search`: `Searcher` over `Route::{Vector,Fts,Hybrid}`; query text
  embedded on the caller's thread — the only model call, never inside `db.read`
  which serializes on the writer thread; RRF hybrid fusion; `scope_query` derives
  the structural candidate set from a `Query` via `evaluate`, honoured on every
  route; recall-preserving vector pre-filter — plain KNN unrestricted, exact
  `VectorIndexer::distances_for` (new `vec_distance_cosine` method in `vector.rs`)
  for scopes ≤256, adaptive over-fetch ×2 to a 2048 cap for larger; `SearchHit`
  provenance route/score/distance/namespace-path/source-doc; `get_context(item,n)`
  ±n neighbour chunks by `position`, no re-embed),
  Section 10 (`jkb-core` task DAG — new `task.rs`: `create` (multi-placed, bindable,
  `depends_on` edges), `set_status`/`set_status_str` (the string boundary rejects the
  derived `blocked` + unknown statuses), `set_priority`/`set_due`, `is_blocked`, and
  the `ready(Scope, &[TagPred]) -> Vec<TaskRow>` frontier that reuses §8's `is:ready`
  anti-join via `Query::evaluate` then orders by priority→due; quote-aware quick-add
  parser `parse_quick_add` (`!p<n> @<date> +<ns> #<facet>=<value> ^<uid>`) →
  `NewTask::from_quick_add`, defaults `tasks/inbox`+`managed:`; `TaskStatus` gained
  `as_str`/`is_terminal`/`from_manual_str` in `jkb-types`; `item::id_for_uid` helper),
  Section 11 (`jkb-sync`: `trait SyncSerializer` + `resolve(name)` registry shipping
  `DocumentSerializer` (file ⇄ one item), unknown names rejected; `sync(db, mount_ns)
  -> SyncReport` one-shot reconcile — per-file `write_txn`, direction chosen by
  comparing disk-hash & KB-render-hash vs `last_synced_hash`, honouring
  `sync_mode`/`conflict_policy` (`disk_wins`/`kb_wins`/`manual`); `notify`-based
  `watch` with debounce; new `jkb-core` helpers `item::set_content`/`get_content`,
  `binding::item_for_uri`/`mark_synced`/`synced_uris_under`),
  Section 12 (`jkb-cli`: the `jkb` binary — `clap` derive subcommands `ingest`/`query`/
  `search`/`ns`/`tag`/`mount`/`sync`/`task`/`view`/`undo`/`index`/`doctor`/`mcp`, global
  `--db`/`--json`/`--global`, human+JSON output via `output.rs`, ambient cwd scoping,
  ollama embedder built lazily; `jkb ingest <url>` renders via headless browser),
  Section 13 (`jkb-mcp`: `rmcp` 2.0 stdio MCP server `JkbServer` sharing the CLI's
  `Db`/writer-actor; read tools search/get_context/query/list_views/run_view/task_next
  + audited write tools ingest_path/ingest_url/task_create/task_update; `jkb mcp` →
  `jkb_mcp::run_stdio`; tool bodies are sync fns in `logic.rs`, `server.rs` is the
  `spawn_blocking` async adapter),
  Section 14 (end-to-end verification — `crates/jkb-cli/tests/e2e.rs`: a library-level
  full-flow test (mount+bidi-sync round-trip → ingest → query open-small/due:today →
  search all 3 routes + context → task DAG ready-frontier flip → view → undo) with a
  dim-16 fake embedder, an MCP smoke over `jkb_mcp::logic`, and an idempotency+audit
  test; live URL render is an `#[ignore]` test needing Chrome).
- **v1 foundation (Sections 1–14) is COMPLETE and green.**
- **Section 15 (v2 file-sync, D24/D25) is COMPLETE and green.** `jkb-sync`'s
  `SyncSerializer` generalized from a content string to `parse(bytes) -> SyncDoc` /
  `render(&SyncDoc) -> bytes` (`SyncDoc { sections, items, edges }`), split into
  `serializers/{mod,document,tasks}.rs`. Ships the **`tasks` serializer** (one `tasks.md`
  ⇄ many `kind='task'` items): `##` headers → namespaces, prose/legend → `SyncProse` blocks
  stored as namespace `metadata.prose` (**never items** — see [docs/namespaces-and-sync.md](docs/namespaces-and-sync.md)),
  checkbox status (`[ ]/[x]/[~]/[-]`), quick-add modifiers (`!p @ #f=v +ns`), `needs:^id`
  → `depends_on`, indentation → `parent_of`, and a **visible trailing `^id`** stable
  identity (minted deterministically when absent; write-back stamps it). The engine
  (`engine.rs`) is journal-driven **three-way** (base blob vs disk vs KB render, never
  disk vs KB): disjoint per-item edits auto-`Merged`, same-item edits → `conflict_policy`;
  a `tasks` parse failure **quarantines** (stash bytes, journal `needs_attention`, keep
  last-good items) instead of erroring; a task removed from the file is `cancelled` +
  detached (`managed:`), never deleted. New `Outcome::{Merged,Quarantined}`. The
  `document` path and its tests are unchanged (one mechanical test rename). See
  `openspec/changes/jkb-v2-file-sync/`.
- Remaining work is the explicitly-deferred items in
  [docs/subsystems.md](docs/subsystems.md), not a numbered section.
- **Workspace `unsafe_code` is now `deny` (was `forbid`)** so `vector.rs` can carry
  one commented `#[allow(unsafe_code)]` for the `sqlite-vec` FFI registration; every
  other crate is still unsafe-free.
- **`Db::write_txn`/`read` gained generic `write_txn_with`/`read_with`** variants
  (closure error type `E: From<jkb_core::Error>`) so `jkb-ingest` can `?` across
  `jkb_core` + `jkb_index` errors inside one transaction. `rusqlite` is now a single
  `[workspace.dependencies]` pin (all crates use `{ workspace = true }`).
- **Next (deferred, not a section; full detail in [docs/subsystems.md](docs/subsystems.md)):** the per-file serializer override
  (`bindings.serializer`) is now **wired** (Section 15 reads it in `engine::resolve_serializer`);
  still deferred are the `spec` serializer (OpenSpec `spec.md` ⇄ requirement items),
  remote *bindings* (`https://`/`git://`), and an optional MCP `sync_status` tool. Live
  smokes needing external resources — `jkb search` vector/hybrid (ollama) and `jkb ingest
  <url>` (Chrome) — remain `#[ignore]` tests.
- **Section 17 (fleet hardening, D27) is COMPLETE and green** — the agent-claim model
  (`jkb-core/src/claim.rs`, migration `V005`), the full CLI mutate surface + `doctor`/`task`
  reclaim, the no-raw-sqlite hook, the four-state lifecycle (`needs_review` no longer
  unblocks), and the SCHEDULER-groups + REVIEWER + deterministic-merge-queue swarm pipeline.
  See `openspec/changes/jkb-fleet-hardening/` and [docs/subsystems.md](docs/subsystems.md).
- **590 tests** green across the workspace (+2 `#[ignore]`: live-ollama, live-URL — both need an
  external service). `./scripts/check.sh` prints the per-binary breakdown; a count copied here
  goes stale within a pass, so treat this as an order of magnitude. `clippy -D warnings` clean
  (also `--features fastembed`). Dev scripts (all accept pass-through args + allowlisted;
  they self-source `~/.cargo/env`, so run them directly — no `source ~/.cargo/env &&` prefix):
  `./scripts/fix.sh` (fmt+check), `build.sh`, `test.sh`, `clippy.sh`, `test-count.sh`,
  `inspect-dep.sh` (read a dep's extracted registry source).
- **Fresh-machine setup:** `./scripts/setup.sh` is the one-shot, idempotent installer —
  `cargo install`s the `jkb` binary, scaffolds the standard KB roots via `jkb ns mk repos
  tasks media references memory`, builds+installs the VS Code extension (`install-extension.sh`),
  and installs+activates the file-sync watcher service (launchd/systemd). Flags:
  `--no-extension`/`--no-service`/`--no-scaffold`/`--db`. `jkb ns mk <path>…` creates namespaces
  idempotently (the only way to make an empty namespace; others arise from placements/mounts).
- Per-task status (with `[~]` partials and inline notes) is in
  `openspec/changes/jkb-v1-foundation/tasks.md` (v1) and
  `openspec/changes/jkb-v2-file-sync/tasks.md` (Section 15). Keep them updated as you go.

## Architecture in one breath

The source of truth is a `SQLite`-backed **virtual filesystem**: logical
namespaces + items + typed edges + tags, with two-axis addressing (logical
namespace vs `managed:`/`file://` binding). Vector (`sqlite-vec`) and keyword
(FTS5) search are **derived, rebuildable indexes** behind `trait Indexer`. The
same item+edge substrate powers the task DAG, file sync (pluggable serializers),
and the MCP server. See `openspec/changes/jkb-v1-foundation/design.md`.

## Ways of working (non-negotiable)

- **No unsafe**, with exactly one exception. `unsafe_code = "deny"` is set
  workspace-wide; the *only* `#[allow(unsafe_code)]` is the `sqlite-vec` FFI
  registration in `jkb-index`'s `vector.rs` (`register()`). Do not add others.
- **Lints are gates.** clippy `pedantic` is on; `./scripts/check.sh` runs
  `fmt --check`, `clippy -D warnings`, the `scripts/tests/*.test.sh` shell tests, tests,
  and `cargo deny`. Keep it green.
- **Errors:** `thiserror` in libraries, `anyhow` at the binary edge. No
  `unwrap`/`expect` outside tests.
- **IDs are newtypes** so `ItemId`/`NamespaceId` can't be crossed.
- **SQL is always parameterized.** Never string-interpolate values.
- **No raw `sqlite3` against a jkb db** (mirrors the no-raw-cargo rule). The `jkb`
  CLI covers every read/write an agent needs — reads (`task show`/`next`, `query`,
  `search`), edits (`task set`/`edit`/`tag`/`depend`/`undepend`/`place`/`bind`/`claim`/
  `release`/`reclaim`), `undo`, `doctor --fix` — each routed through the audited
  writer-actor + changelog + undo. A PreToolUse hook (`.claude/hooks/block-raw-sqlite.sh`,
  fail-open) denies `sqlite3` targeting `jkb.db`/`$JKB_DB`/`~/.jkb/…`. Scripts under
  `./scripts/` may read the DB directly (the sanctioned path).
- **Writes go through the single writer-actor**; core is synchronous, async only
  at the edges (ollama HTTP, file-watching, MCP).
- **Indexes are derived** — anything in an index must be rebuildable from the VFS.
- **Tests:** unit + integration + `proptest` for load-bearing invariants.
- **Self-review before the reviewer, and reach high confidence first.** `/review` and
  `/review-log` cost ~6 agents per run at the default `low` tier, and ~15 at `medium`. Run step 0
  of `/review-log` — did every edit land, does each comment match its code, can each guard fire, who else implements this
  rule, does any test cover this mode, does every call site pass the new argument, did you
  actually run it — then `./scripts/check.sh`, and only then launch the workflow. **A doubt you can name is a test to
  write, not a line in the reviewer's focus argument** — the focus is for perspectives you lack,
  and a finding that merely confirms a doubt you already held is a review spent on work you owed
  it. **Anything short of high confidence is a blocker, not a disclosure**: what you are unsure of is exactly what to
  test, and the reviewer's budget must not be spent rediscovering a gap you could already name. `staging-workflow` needed 41 passes, and a large share of
  the findings were self-catchable. **A rule every call site must remember is the defect** — the
  four vector sweeps, the seven layout guards, the retry debt, the write-seam snapshot were all
  one shape. Put it in the callee, a type, or the schema instead.

## Implementation conventions (follow for consistency)

- **Repos are plain functions over `&Connection`** (e.g.
  `jkb_core::item::upsert(conn, meta, &item)`), composed inside
  `db.write_txn("actor", |conn, meta| …)` (one atomic transaction with a fresh
  `txn_id`) or `db.read(|conn| …)`. `Db` (in `store.rs`) is the only public
  handle; it clones cheaply and routes all access through one writer thread.
- **Mutations require `&WriteMeta`**, which only exists inside `write_txn` — this
  funnels every write through a transaction.
- **SQL uses `conn.prepare_cached(sql)`** (not `execute`/`prepare`) so statements
  compile once and are reused across the long-lived writer connection. To get a
  new-or-existing row id in one statement, use
  `INSERT … ON CONFLICT(…) DO UPDATE SET <no-op> RETURNING id|rowid`.
- **Changelog on every mutation**, and the op is **derived, never chosen** (D47).
  A row-writing mutation calls
  `changelog::upsert(conn, meta, Entity::Foo, entity_id, before, after)` — it records
  `insert` when `before` is `None` and `update` otherwise. `changelog::append` takes
  an op for everything else (`delete`, `claim`, `release`, …) and **refuses `insert`
  outright**. `Entity` is a closed enum, so `entity_type` cannot be a typo'd table.
  `entity_id` is the row's rowid wherever the inverse is keyed by one.
  `undo::INVERSES` covers ~20 `(op, table)` pairs and **refuses** anything it does
  not, so a gap is a named refusal rather than an unrelated transaction being
  reverted instead.
- **A before-state must be able to restore something.** `changelog::write` calls
  `undo::check_restorable` on every entry: the before-state must be a non-empty
  object naming only real columns of the table, and for a **`delete`** it must name
  **every** column — an unnamed column would come back as its default. Checked
  against the live schema, so adding a column makes every deleter of that table fail
  at its next write until the column is logged.
- **Enums** in `jkb_types` carry `as_str()` returning the snake_case DB string
  (matches their serde form). IDs: `.new(i64)` / `.get() -> i64`.
- **Migrations:** add `V00N__<name>.sql` under `crates/jkb-core/src/migrations/`
  (refinery embeds them at compile time). Virtual tables (vec0/FTS5) are
  **additive** — never `ALTER` a populated one. `rusqlite` is pinned **once** in
  `[workspace.dependencies]` (0.39, `bundled`) and every SQLite-touching crate uses
  `rusqlite = { workspace = true }`; 0.39 matches refinery **0.9.2** so they share one
  `libsqlite3-sys`. Don't add a per-crate version — desyncing reintroduces the
  `links = "sqlite3"` conflict.
- **Lints gotchas:** `clippy::doc_markdown` fires on bare code identifiers and
  `SQLite` in doc comments — backtick them. Every public fn returning `Result`
  needs a `# Errors` doc. Only macro-generated modules get
  `#[allow(clippy::pedantic)]` (see `migrate.rs`).
- **`sqlite-vec` (done in Section 6).** All of it — the SQL and the one `unsafe`
  FFI registration — lives in `jkb-index/src/vector.rs` (per D9). Workspace lint is
  `unsafe_code = "deny"` with a single scoped `#[allow(unsafe_code)]` there.
  **Extension setup is a core-owned seam** (D15: core owns connection/extension
  setup): `jkb_core::ExtensionRegistrar = fn()`, and you open with
  `Db::open_with(path, &[jkb_index::register])` (or `open_in_memory_with`) — core
  sequences the registration before opening the connection. `jkb-index` provides the
  `sqlite-vec` `register()`; it does **not** depend on `jkb-core`. This mirrors the
  `Embedder` seam (trait in `jkb-types`, impls in `jkb-embed`). The `vec_items_<dim>`
  table is created dynamically by `VectorIndexer::ensure_ready` (not a migration).
  `rusqlite` is pinned once in `[workspace.dependencies]` (all crates use
  `{ workspace = true }`) so there is one `libsqlite3-sys`.

## Build / verify

Raw `cargo build|test|clippy|fmt|check` is denied by a PreToolUse hook
(`.claude/hooks/block-raw-cargo.sh`) — go through the wrappers, which self-source
`~/.cargo/env` (rustup installs the pinned 1.96.1 toolchain) and pass args through.

```sh
./scripts/build.sh
./scripts/test.sh       # e.g. ./scripts/test.sh -p jkb-core
./scripts/check.sh      # fmt --check + clippy -D warnings + shell tests + test + cargo-deny + ui
```

`check.sh` skips `cargo-deny` gracefully when it is not installed
(`cargo install cargo-deny`). Update `tasks.md` checkboxes (`[x]` done, `[~]` partial +
inline note, `[ ]` todo) as each item lands.

## Where the rest lives

This file is loaded into **every** session, so it holds only what every session needs: the
status above, the architecture, the non-negotiables, the conventions, and how to build. The
rest of the project's decision record lives beside it and is read **on demand** — open the one
whose subject you are about to touch. Each is written the same way: what was decided, what it
cost to learn, and which alternatives were rejected and why.

| Read this | Before you touch | Why it exists |
|---|---|---|
| [docs/git-hooks-installer.md](docs/git-hooks-installer.md) | `scripts/lib.sh`, `scripts/setup.sh`, `scripts/hooks/post-merge`, and the repository-selection scrub in `gitrepo.rs`/`pr.rs`/`session.rs` | The longest defect cluster in the repo. Seventeen review rounds on one installer, and nearly every lesson generalizes. |
| [docs/task-lifecycle.md](docs/task-lifecycle.md) | `jkb task *`, `jkb staging *`, `crates/jkb-fsm`, `crates/jkb-cli/src/{gitrepo,session,repo,archive,pr}.rs`, `scripts/merge-queue.sh` | Subtasks and containment (D34/D35), per-task worktrees (D36), the checkable state machine and transition log (D48), review-gated landing (D38), the design gate (D28). |
| [docs/sandbox-and-container.md](docs/sandbox-and-container.md) | `scripts/auto-mode*`, `.container/` | The unattended-agent boundary (D48) and the container nested inside it (D49), plus the egress firewall and its verdict (D50/D51). |
| [docs/namespaces-and-sync.md](docs/namespaces-and-sync.md) | `jkb-sync`, `jkb-core`'s namespace/item/undo code | The namespace layout (D32), typed namespaces (D33), investigations (Dmem), and the file-sync data-loss cluster (D45/D39/D40/D42/D47). |
| [docs/subsystems.md](docs/subsystems.md) | a crate you need to orient in | What each finished subsystem is and how it is put together. Reference, not live decisions — so it is the *last* row to check, never the first: if another row names your file, that row governs. |
| [docs/ui-and-review.md](docs/ui-and-review.md) | `ui/`, `.claude/workflows/code-review.js` | The explorer is a CLI client, never a bespoke backend (D31); our reviewer returns structured findings (D37). |

Three rules about this set, because a split decision record fails in predictable ways:

- **A file can appear in two rows, and then both govern.** `gitrepo.rs` is the clearest case:
  its merge-detection and branch-ref rules are in `task-lifecycle.md`, its repository-selection
  scrub is in `git-hooks-installer.md`. The rows name the *subject*, not an owner — read the row
  whose subject you are changing.
- **A decision is recorded in exactly one of these files.** If a change spans two subjects,
  write it where the *code* lives and cross-reference from the other, rather than describing it
  twice — two copies of a rule is the defect this whole record keeps rediscovering.
- **Correct in place, and keep the diagnosis.** When a decision is reversed, mark it superseded
  and say what measurement reversed it (see D46's heading, or the `dispatch=transient` reversal
  in the hooks doc). The wrong turning is usually worth more than the destination.
- **A claim about behaviour carries its measurement.** "Measured on git 2.51.1", "reproduced
  against real SQLite", "verified by disabling the guard". A claim with no method behind it has
  twice been wrong in this record, in the direction that cost the most.
