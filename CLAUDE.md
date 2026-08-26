# Working in jkb

jkb is a Rust Cargo workspace (crates under `crates/`) building a local-first,
agent-native knowledge base. The full plan lives in `openspec/` (local only, not
committed), one folder per change under `openspec/changes/<name>/`: each holds the
`design.md` for its decisions (the D-series, which runs to D47, plus the per-change
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
  stored as namespace `metadata.prose` (**never items** — see the sync note below),
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
- Remaining work is the explicitly-deferred items below, not a numbered section.
- **Workspace `unsafe_code` is now `deny` (was `forbid`)** so `vector.rs` can carry
  one commented `#[allow(unsafe_code)]` for the `sqlite-vec` FFI registration; every
  other crate is still unsafe-free.
- **`Db::write_txn`/`read` gained generic `write_txn_with`/`read_with`** variants
  (closure error type `E: From<jkb_core::Error>`) so `jkb-ingest` can `?` across
  `jkb_core` + `jkb_index` errors inside one transaction. `rusqlite` is now a single
  `[workspace.dependencies]` pin (all crates use `{ workspace = true }`).
- **Next (deferred, not a section):** the per-file serializer override
  (`bindings.serializer`) is now **wired** (Section 15 reads it in `engine::resolve_serializer`);
  still deferred are the `spec` serializer (OpenSpec `spec.md` ⇄ requirement items),
  remote *bindings* (`https://`/`git://`), and an optional MCP `sync_status` tool. Live
  smokes needing external resources — `jkb search` vector/hybrid (ollama) and `jkb ingest
  <url>` (Chrome) — remain `#[ignore]` tests.
- **Section 17 (fleet hardening, D27) is COMPLETE and green** — the agent-claim model
  (`jkb-core/src/claim.rs`, migration `V005`), the full CLI mutate surface + `doctor`/`task`
  reclaim, the no-raw-sqlite hook, the four-state lifecycle (`needs_review` no longer
  unblocks), and the SCHEDULER-groups + REVIEWER + deterministic-merge-queue swarm pipeline.
  See `openspec/changes/jkb-fleet-hardening/` and the Section 17 reference block below.
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
  `fmt --check`, `clippy -D warnings`, tests, and `cargo deny`. Keep it green.
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
./scripts/check.sh      # fmt --check + clippy -D warnings + test + cargo-deny + the ui build
```

`check.sh` skips `cargo-deny` gracefully when it is not installed
(`cargo install cargo-deny`). Update `tasks.md` checkboxes (`[x]` done, `[~]` partial +
inline note, `[ ]` todo) as each item lands.

## Sections 5–6 — jkb-embed & jkb-index (DONE, for reference)

**`jkb-embed`** (`jkb_types::Embedder`): `ollama.rs` default over `reqwest::blocking`
(`nomic-embed-text`, dim 768, `health_check` via `/api/tags`, char-boundary truncation,
actionable `EmbedderUnavailable`); `fastembed.rs` feature-gated ONNX; `lib.rs`
`EmbedderConfig`+`build()`+`truncate_to_chars`. The `Embedder` trait gained
`resolved_version()` (ollama digest / stable fastembed id). The pure catalog guards
(`ensure_compatible` = dim **and** model, `check_version_drift`) live in **`jkb-types`**
(re-exported by `jkb-embed`) so `jkb-index` uses them without `reqwest`.

**`jkb-index`** (`crates/jkb-index`): `trait Indexer` + `IndexItem` + `Dispatcher`
(`on_upsert`/`on_delete`/`rebuild_all`). The trait lives here (not `jkb-types`)
because it takes a `rusqlite::Connection`; that keeps `SQLite` out of the DB-free
vocabulary crate. `vector.rs` is the ONLY `sqlite-vec`/`unsafe` module — `register()`
(a `jkb_core::ExtensionRegistrar`, plugged in via `Db::open_with`), `VectorIndexer`
over `vec_items_<dim>` (`item_id INTEGER PRIMARY KEY`, f32-blob binding, KNN returns
ids), `ensure_ready` creates the table + reconciles the `embeddings_meta` catalog
(`ensure_compatible`), `rebuild` re-embeds from content. `fts.rs` `FtsIndexer`
(bm25 search / integrity-check / `'rebuild'`; per-item writes are trigger-driven no-ops).

**`jkb-ingest`** (`crates/jkb-ingest`): `Pipeline` drives capture→embed. `capture`
(one `write_txn_with`): idempotency check on the `ingestions` row, then store blob
(`blob.rs`), create the document item + chunk items (`chunk.rs`, char windows with
overlap, uid `b3:<hash>:<idx>`) with `derived_from` edges + placements. `embed_and_complete`
(separate txn, embeddings computed off-thread): `VectorIndexer` writes vectors, marks
the ingestion complete. Down embedder → captured (FTS-searchable) but not embedded;
`index_pending`/`unembedded_count` mop up later (D21). `adapter.rs` = `SourceAdapter`
trait + text/Markdown (`parse()` dispatch by extension). `Error` bridges `jkb_core` +
`jkb_index` (the only cross-`From` seam). Open with `Db::open_with(path, &[jkb_index::register])`.

**`jkb-core` query engine** (`query/mod.rs` + `query/parse.rs`, `view.rs`,
`mount::ambient_namespace`): `Query::evaluate(conn)` builds ONE parameterized SQL
query (`Value` params via `params_from_iter`) over items+placements+tags+edges+fts,
returning the candidate item-id set. The `~"…"` vector term is parsed onto the AST
but ranked by `jkb-search`, not core. `query::parse` is a quote-aware DSL. Saved
views = `kind='view'` items under `_sys/views` (`view::save/list/get/run`). Tag
comparison is lexical `TEXT` (ordinal facets are a known limitation).

**`jkb-search`** (`crates/jkb-search`, Section 9): `Searcher::new(embedder)` +
`search(db, &Query, Route, limit)` / `get_context(db, item, n)`. `Route::{Vector,Fts,
Hybrid}`. The query text is embedded on the *caller's* thread (the sole model call —
never inside `db.read`, which is serialized on the writer thread). `scope_query`
turns a `Query`'s structural part (scope/tags/kind/…, ranking terms stripped) into
the in-scope candidate id set via `Query::evaluate`; `None` = unrestricted (rank
globally). `vector_ranked` (D9 recall preservation): unrestricted → plain `knn`;
scope ≤ `EXACT_SCORING_CAP` (256) → exact `VectorIndexer::distances_for` (the new
`vec_distance_cosine` method — sqlite-vec SQL stays in `vector.rs`, partition seam
noted there); larger restricted scope → over-fetch `k×8`, filter, grow ×2 to
`OVERFETCH_CAP` (2048). `fts_ranked` over-fetches-then-filters (FTS candidates are
small). Hybrid = RRF (K=60). `SearchHit { item, route, score (higher=better),
distance, namespace_path, source_document }`. `jkb search --json` **resolves**
those ids: every hit carries `uid`/`kind`/`status`/`snippet` and `source_document` is an
object (`{id,uid,kind}`), not a bare row id — a result identified only by an integer is not
interpretable by the agent that asked for it, and search is the one read an agent cannot
fall back to `query` for. `Error` bridges `jkb_core` + `jkb_index`
(mirrors `jkb-ingest`); use `Db::read_with::<_, Error, _>` to `?` across both.

## Section 10 — jkb-core task DAG (DONE, for reference)

`crates/jkb-core/src/task.rs` is the typed repo API over the item substrate (design
D5/D19). Tasks are items `kind='task'`; lifecycle lives in the real `status` column
(`open`/`in_progress`/`done`/`cancelled`), `blocked` is **derived** (a `depends_on`
edge to a non-**terminal** task — `done` *and* `cancelled` both unblock, since a
cancelled dep will never complete), never stored. `create(&NewTask)` inserts the item (status
`open`) then places a `Primary` home + `Reference` mirrors, sets the binding, applies
tags, and links `depends_on` edges (cycle-guarded by `edge::link`). `set_status_str`
is the boundary that rejects `blocked` + unknown (via `jkb_types::TaskStatus::from_manual_str`);
`set_status(TaskStatus)` can't even represent `blocked`. `ready(conn, Scope, &[TagPred])`
does **not** duplicate SQL — it builds a `Query { kind:task, ready:true, scope, tags }`,
calls `Query::evaluate` (the one `is:ready` anti-join), then orders the ids by priority
(asc, nulls last) then `date(due)`. `is_blocked` mirrors that anti-join for one task.
Quick-add: `parse_quick_add` (quote-aware, mirrors `query/parse.rs`) →
`NewTask::from_quick_add`. **Homing (D26, the `jkb-task-homing` change):** the first
`+<ns>` placement is the Primary `home`, the rest are Reference `mirrors`; `tasks/inbox`
(`DEFAULT_HOME`) is only the fallback when none is given. Binding defaults to `managed:`
(`MANAGED_BINDING`). The CLI derives homes from the ambient repo (full mount ns): a plain
`task add` inside a mounted repo → `tasks/<repo>/inbox` + a `tasks/inbox` mirror;
`task add --backlog` → `tasks/<repo>/.backlog`. `task next`/unscoped task queries default
to `tasks/<repo>/**` inside a repo, else the global `tasks/**`. `task unplace <uid> <ns>`
removes a mirror. (The synced-file binding path and repo-root mounting remain follow-ups.)

## Section 11 — jkb-sync (DONE, for reference)

`crates/jkb-sync` reconciles `file://` mounts with items (design D3/D24/D25).
`serializer.rs`: `trait SyncSerializer` (`parse(bytes)->String` / `render(&str)->
Vec<u8>` + `name`) with `resolve(name)` rejecting unknown names (lists `AVAILABLE`);
v1 ships `DocumentSerializer` (whole file ⇄ one item's content — the `{items,edges}`
payload generalization waits for the v2 `tasks`/`spec` serializers). `engine.rs`:
`sync(db, mount_ns) -> SyncReport` loads the mount (`mount::get`), validates the
serializer, `discover`s files (walk backing dir with `globset` include/exclude,
**unioned** with `binding::synced_uris_under` so KB-created / disk-deleted files
reconcile), then reconciles each file in its own `write_txn`. Direction: hash the
disk bytes and the KB render, compare both to the binding's `last_synced_hash` — only
disk changed → import (`item::set_content`), only KB → export (`write_file`), both →
`conflict_policy` (`disk_wins`/`kb_wins`/`manual`; manual reports via
`SyncReport::conflicts()` and touches nothing). For `document`, file bytes == KB
render, so one hash tracks both sides (D25 v1 simplification). Export writes the file
*inside* the txn so a failure rolls back with no `last_synced_hash` drift.
`sync_paths(db, mount_ns, &[PathBuf])` reconciles just the given files (deduped +
scoped by a shared `Filter`). `watch.rs`: `notify` watcher → initial full reconcile →
debounced (drain-until-quiet) re-sync of **only the event paths** via `sync_paths`
(full `sync` only on watcher error / `need_rescan`); the OS watches a dir subtree, so
glob relevance filtering is ours. Stop is a shared `Arc<AtomicBool>`; `watch_all`
(11.6) watches every mount concurrently (thread per mount via `mount::all_paths`) so
`jkb service`'s launchd/systemd unit can run `jkb sync --watch` (all mounts) at login.
New `jkb-core` seams it needed: `item::set_content`/`get_content`,
`binding::item_for_uri`/`mark_synced`/`synced_uris_under`, `mount::all_paths`.

## Section 15 — jkb-sync v2: multi-item serializers + robustness (DONE, for reference)

The v2 file-sync change (design `openspec/changes/jkb-v2-file-sync/`, D24/D25). The
`SyncSerializer` trait generalized to `parse(&[u8]) -> SyncDoc` / `render(&SyncDoc) ->
Vec<u8>` + `quarantine_on_parse_error()`; `serializer.rs` became
`serializers/{mod,document,tasks}.rs`. `SyncDoc { sections, items, edges }` with
`SyncItem { local_id, kind, content, section, position, status, priority, due, tags,
mirrors, parent }`. **`document`** is one item with empty `local_id` (bare `file://`
uri, byte-compatible with v1; non-UTF-8 stays a hard error). **`tasks`** maps one
`tasks.md` ⇄ many items: `##` headers → namespaces (header line + order stored in
`namespaces.metadata`), prose/legend/blank → `text` items, `- [ ]/[x]/[~]/[-]` →
task status, `!p @ #f=v +ns` modifiers, `needs:^id` → `depends_on`, indentation →
`parent_of`, trailing `^id` = stable identity (minted `slug-<b3:6>` + counter when
absent; uri-safe; dep-cycle detected at parse → quarantine). `render` is idempotent —
the engine stores rendered bytes as the base so a settled file re-syncs `UpToDate`.

**Identity/binding:** each item binds to `file://<path>#<local_id>` (document: bare
`file://<path>`); `binding::synced_uris_for_file` groups a file's item bindings.

**Engine (`engine.rs`)** is journal-driven three-way: the `_sys/sync` journal
(`sync_state`, V004) holds per-file `last_synced_hash` + `base_blob_hash` (a content-
addressed blob) + `status`. `disk_changed`/`kb_changed` compare each side against the
**base** (never disk vs KB); `assemble_kb_doc` inverts `apply_doc` (walk `ns::subtree`
for sections, primary placement for items) so `render(assemble)` reproduces the base.
Both-changed → per-item `three_way`: disjoint local_ids auto-`Merged`, same-item →
`conflict_policy` (`manual` flags the journal `conflict` and touches nothing). `apply_doc`
is two-pass (items with `content_hash=None`, then edges via `edge::link`/`edge::unlink`);
removed items are `cancelled` + rebound to `managed:` (detached, not deleted). A `tasks`
parse failure `quarantine`s (stash bytes → `quarantine_blob_hash`, journal
`needs_attention`) and auto-recovers on the next good edit. New `Outcome::{Merged,
Quarantined}` + `SyncReport::{merged,quarantined}`.

**New `jkb-core` seams:** `blob::{hash_bytes,store,load}` (core owns the `blobs` table;
`jkb-ingest::blob` re-exports it), `sync_state::{SyncState,SyncStateWrite,get,upsert,
needs_attention}`, `binding::synced_uris_for_file`, `tag::{remove,applications}`,
`edge::{edges_from,unlink}`, `ns::{set_metadata,get_metadata}`. `jkb-sync::Error` gained
`Sqlite(#[from])` for the engine's inline reconciliation queries. CLI: `jkb doctor`
surfaces `sync_state::needs_attention`; `jkb sync` reports merged/quarantined counts. The
per-file `bindings.serializer` override is now read (`engine::resolve_serializer`).

## Section 12 — jkb-cli (DONE, for reference)

`crates/jkb-cli` builds the `jkb` binary (`[[bin]] name = "jkb"`). `main.rs` is a
`clap` derive `Cli` + `Command` enum; `run()` opens the DB once
(`Db::open_with(&[jkb_index::register])`) and dispatches to `cmd_*` fns. Global args
`--db` (default `$JKB_DB` / `~/.jkb/jkb.db`), `--json`, `--global`. Output goes through
`output.rs` (`DisplayItem` + `fetch_items` + `print_items`, human or JSON). Ambient
scoping: `apply_ambient` rewrites an unscoped `Query` to the cwd mount's subtree via
`mount::ambient_namespace` unless `--global`. Commands: `ingest` (local path, or an
`http(s)://` URL rendered via headless browser), `query`, `search` (`--route`/`--limit`/`--context`), `ns ls|mv`
(added `ns::roots` for top-level listing), `tag ls|rename`, `mount create|ls` (`create`
canonicalizes dir → `file://`; `ls` lists mounts), `sync [ns] [--watch]` (ns optional → all mounts; ctrl-c → shared stop flag),
`service print|install|uninstall` (launchd/systemd unit for the watcher), `task add` (quick-add → slug+nanos
uid) / `task next` (trailing DSL → scope+tags), `view save|ls|run`, `undo [txn]`,
`doctor [--backup]`, `mcp` (a stub in this section; wired to `jkb_mcp::run_stdio` by
Section 13 below). Embedder is the ollama default,
built lazily only where needed so read/task/query/sync/undo work fully offline; ingest
captures (FTS-searchable) even when the embedder is down. Errors use `anyhow` at this
edge. Tests: `tests/cli.rs` via `assert_cmd`, all offline.

**Linux-style + agent-facing verbs** (ergonomic wrappers over the same reads; `output.rs`
human+`--json`): `ls [path] [-l -R -t -a]` (children; `list_children`), `tree [path]`
(recursive map + per-folder `ns::subtree_leaf_count`), `grep <pat> [path] [-i -l -c]`
(literal substring via `SQLite` `instr`, new `item::grep`; **exits 1 on no match** — the
only nonzero-on-empty command), `find [path] --kind/--tag/--status` (typed search → query
DSL), `recent [path]` (updated-desc listing), `cat <uid>` (raw body) / `stat <uid>`
(metadata, no body), and `guide` (the agent cheat-sheet, mirrored in root `AGENTS.md`).
`grep` = literal, `find`/`query` = structured, `search` = ranked — pick by what you know.

## Section 13 — jkb-mcp (DONE, for reference)

`crates/jkb-mcp` is the `rmcp` 2.0 stdio MCP server (design D17). Split in two:
`logic.rs` holds the tool work as **plain synchronous fns** over `&Db` + `&Arc<dyn
Embedder>` (search/get_context/query/list_views/run_view/task_next; ingest_path/
ingest_url/task_create/task_update) returning `serde_json::Value` — directly unit-
testable with no transport or runtime. `server.rs` is the thin async adapter:
`JkbServer { db, embedder }`, `#[tool_router]`/`#[tool]` methods that `run()` each
logic fn on `tokio::task::spawn_blocking` (the writer-actor + ollama block, so they
must leave the async runtime) and wrap the JSON in a `CallToolResult`; `#[tool_handler]
impl ServerHandler` with `get_info` advertising tools. rmcp gotchas learned via
`./scripts/inspect-dep.sh rmcp-2.0.0 …`: `#[tool_handler]` calls the generated
`Self::tool_router()` (do **not** store a `tool_router` field — it'd be dead), args
derive `serde::Deserialize + schemars::JsonSchema` and arrive as `Parameters<T>`,
content is `rmcp::model::ContentBlock` (`::json`/`::text`), `ServerInfo`/`ServerCapabilities`
are `#[non_exhaustive]` (mutate a `default()`). Errors → `ErrorData` (user-input →
`invalid_params`). `lib.rs::run_stdio(db, embedder)` builds a tokio runtime + `serve(stdio())`;
`jkb mcp` (in the CLI) calls it. All writes go through `db.write_txn` → audited + undoable.

## v1 foundation complete — deferred follow-ups

Sections 1–14 (plus 7.3 and 11.6) are done and green. `jkb service install` sets up
the launchd/systemd watcher; the OS supervisor owns lifecycle (no in-process daemon by
design). What's intentionally left (each noted at its origin in `tasks.md`):

- **D24/D25 — multi-item serializers + sync robustness.** DONE in Section 15: the
  `tasks` serializer, the per-file `bindings.serializer` override, the `_sys/sync`
  journal, three-way merge, and quarantine all shipped. Still deferred: the **`spec`**
  serializer (OpenSpec `spec.md` ⇄ requirement/scenario items) behind the same seam.
- **Live smokes (external deps).** `jkb search` vector/hybrid needs a running ollama;
  `jkb ingest <url>` needs a local Chrome/Chromium. Both are `#[ignore]` tests our
  offline suite can't cover.
- **PDF OCR (v2).** Scanned/image-only PDFs extract near-zero text (the pipeline
  warns); OCR is out of scope for v1.

## Section 17 — fleet hardening (D27, DONE, for reference)

The robustness pass on the agent swarm that drives jkb task execution (design
`openspec/changes/jkb-fleet-hardening/`). Four axes, all landed:

- **Agent-claim model (D27.1, `jkb-core/src/claim.rs`, migration `V005`).** A claim is
  a **property of the task** — two nullable `items` columns (`claimant_id`, `claimed_at`),
  **not** a side table, never encoded in `status`. `claim(item, owner)` is a **CAS** that
  succeeds only if free or same-owner and **atomically sets `status='in_progress'`** (no
  claimed-but-`open` window). `release` clears the claim (leaves `status`). `reclaim_dead(
  live_owners)` NULLs **only** claims whose owner ∉ the verified-alive set, writing **only**
  claim columns — so it never clashes with a status transition and a live run never reclaims
  its own work. **Liveness is by owner-existence, never age**: no TTL, no heartbeat — a
  paused-but-alive agent keeps its claim. All three are **changelogged** (op
  `claim`/`release`/`reclaim`), not undoable (undo inverts only inserts). `ready` gained the
  plain predicate `AND claimant_id IS NULL`.
- **CLI mutate surface + reclaim (D27.2/D27.3, `jkb-cli`).** `task
  show`/`set`/`edit`/`tag`/`depend`/`undepend`/`place`/`bind`/`claim`/`release`/`reclaim` cover
  every read/write over existing audited core seams; owner ids are `host:pid`
  (`owner.rs`, `ps -p` liveness probe — `kill -0` exits non-zero on `EPERM` for a foreign-owned
  but live process, so it would reclaim a running agent's claim). `doctor` reports orphaned
  claims (owner gone);
  `doctor --fix` and `task reclaim --keep <owner>` run the owner-existence reclaim.
- **Four-state lifecycle (D27.7).** `open → in_progress → needs_review → done` reusing the
  existing `TaskStatus` (no new variant). **`needs_review` no longer unblocks dependents** —
  `unblocks_dependents()` is now just the terminal set `{done, cancelled}` (a task under
  review may bounce back). `needs_review` means "a reviewer is reviewing" (transient).
- **Status is KB-local, never in git.** Task status, task ids, and the fact a swarm ran
  are personal bookkeeping — commits are ordinary professional messages (no `swarm:` prefix,
  no uid, no trailer), history is linear (the merge queue rebase/fast-forwards, no merge
  commits), and the integration branch is nameable as an ordinary feature branch.

The **swarm pipeline** (agent-tooling, not a crate: `.claude/workflows/task-swarm.js`,
`scripts/merge-queue.sh`, `.claude/commands/task-swarm.md`): **SCHEDULER** clusters
overlapping ready tasks into work-groups (≤~4) → one **IMPLEMENTER** per group (all its
tasks on one clean branch, stays with the group) → a **fresh REVIEWER** per pass (checks
the whole group, seeded with the prior handoff) → a **deterministic merge queue** (no
RESOLVER agent; `merge-queue.sh` rebase/fast-forwards, runs the gate, marks the group
`done` on green, ejects on conflict/red). Pipelined (no per-round barrier), the merge
queue the one serial stage; the coordinator loop claims each group before dispatch,
releases on settle, and runs `task reclaim --keep <owner>` each pass as the crash net.

## Task branch lifecycle (D34) — subtasks, branch tags, merge-driven close

Two chores that were manual — re-running `setup.sh` after a pull, and closing tasks whose PR
landed — are now automatic (design `openspec/changes/jkb-task-branch-lifecycle/`).

- **Subtasks are `parent_of` edges.** The edge type existed and the `tasks` serializer already
  wrote it from indentation; nothing read it. Now **a task with a non-terminal child is off
  the frontier** — you work the leaves, the parent is a container. One anti-join
  (`SUBTASK_CLAUSE`), added to `is:ready` and `is:frontier` **identically**, because those two
  must stay equivalent for tasks. `jkb task add --under <uid>` creates one (inheriting the
  parent's home); `jkb task show` lists them and says why the parent is held. Deliberately no
  status rollup: auto-close is a separate, git-triggered decision, and two mechanisms racing
  to close one task is how it closes for the wrong reason.
- **`jkb task start <uid>`** claims the task *and* records `branch=`/`repo=`, its land target and
  its measured cut point, from the
  ambient git repo — one moment, one command, so the tag is never missing on exactly the
  tasks that needed it. It refuses the trunk branch (which would auto-close instantly).
- **Merge detection is strategy-agnostic** (`jkb-cli/src/gitrepo.rs`). `--is-ancestor` and
  `git cherry` both report *not merged* for **squash**, GitHub's most popular strategy, since
  it rewrites the branch into one new commit. The check that works for all three asks a
  different question — `git merge-tree --write-tree trunk branch` equalling trunk's own tree
  means the branch **adds nothing**, however it landed. Falls back to `--is-ancestor` on git
  <2.38 and *says so*. A recorded cut point exists because refs alone cannot separate a rebase-merged
  branch (GitHub fast-forwards, leaving it byte-identical to trunk) from one just created.
- **`jkb task close-merged`** closes a task only when its branch merged **and** every subtask
  is terminal; anything else is reported. A merged branch is evidence, not proof — a missed
  close costs one command, a wrong close buries unfinished work.
- **Containment is a placement, not a derived view (D35).** `placements.parent_item_id`
  (migration `V009`) says where a node lives: *in namespace N, contained by item P*. `NULL`
  means directly in the namespace. Listing is then one query over one table —
  `items_directly_in` for a namespace, `items_under` for a container — with **no filter, no
  edge join and no de-duplication rule**. It previously simulated this at read time, placing
  the child in its container's namespace and hiding it again on the way out.
  `namespace_id` is deliberately kept alongside: `ns:tasks/**` scoping resolves through it,
  so a contained item must stay findable by scope. `ON DELETE SET NULL`, never CASCADE —
  deleting a container returns its children to the namespace rather than deleting their
  placement rows, which would make them invisible rather than un-parented.
- **The edges survive, carrying what a placement cannot** — `edge::link`'s cycle guard,
  `jkb related` traversal, `derived_from` as provenance for search's `source_document`, and
  the `tasks` serializer's indentation + three-way merge `Sig`. `task::add_subtask` writes
  edge and placement in one call so they cannot drift.
- **Containment is a relationship between items (D35).** `containment(child_item_id PRIMARY
  KEY, parent_item_id, position)` — its own table, keyed on the **child**, because "X is
  contained by Y" is a property of X and not of one of X's several placements (a home plus
  the `tasks/<repo>` mirror). The PK makes *at most one container* structural. `placements`
  is untouched and still carries `namespace_id`, so `ns:tasks/**` scoping still finds a
  contained item: listing and scoping ask different questions and both stay right. A
  contained item is listed under its container **and nowhere else**, even when homed in
  another namespace — never unreachable, since expanding the container always reaches it.
  Rejected alternatives are recorded in the design: a namespace per parent (derives a path
  from a mutable title — the identity failure the sync prose bug already taught, and it grows
  the organizational tree with content) and `placements.parent_item_id` (stores one fact once
  per placement).
- **Containment is a behaviour, not a node kind.** A *pure namespace* is a node that only
  contains; a parent task both **is** a task and **contains** its subtasks; a document
  **contains** its chunks. So `jkb ls <path-or-uid>` is the one container read — it resolves
  a namespace first (the historical meaning) and falls back to an item uid — and `jkb tree`
  descends into **any** child with `has_children`. `jkb task subtasks` is a thin alias for
  discoverability, not a second implementation. The UI passes a node's address and does not
  branch on kind. The two containment edges stay distinct where it matters: a task
  *decomposes into* subtasks (`parent_of`, authored), a document is *fragmented into* chunks
  (`derived_from`, generated and rebuildable).
- **A contained node is listed once.** `--under` homes a subtask beside its parent, and
  ingest places chunks beside their document, so either would otherwise appear both as a
  namespace sibling and nested under its container. `ls` hides it **only where its container
  is in the same listing** — one homed elsewhere keeps its own row, because hiding it there
  would make it unreachable rather than merely un-duplicated.
- **Chunks are nested, not flag-hidden.** They were previously dropped from listings unless
  `--all`; now they are reached by expanding their document (`jkb ls <document-uid>`, in
  document order via the `chunk` placement's `position`). `--all` no longer re-flattens them
  — that would reintroduce the duplicate — it governs terminal tasks and whether chunks count
  toward per-folder totals, which is a separate question from where they are listed.
- **The explorer shows the hold.** A task carries `subtask_count`/`open_subtask_count` and
  the row reads `2 of 4 subtasks open`, with a hover saying the parent is held. Without it a
  container renders identically to the pickable tasks beside it, which is worse than having
  no subtasks at all.
- **`scripts/hooks/post-merge`** (installed by `setup.sh`) runs `setup.sh` when the pull
  touched `crates/`/`ui/`/`scripts/`/`Cargo.*`, then `jkb task close-merged`. It never fails
  the merge. **Install wrinkle:** `core.hooksPath` set globally *replaces* `.git/hooks`, so
  `setup.sh` also writes a global chainer — without it the repo hook is silently dead.

## Parallel task sessions (D36) — driving tasks by hand, safely

The manual counterpart of `/task-swarm`: the same isolation and the same merge queue, driven
by a human (design `openspec/changes/jkb-parallel-sessions/`). Before this, clicking "Work
this task with Claude" twice gave two agents one checkout, and neither claimed its task.

- **A session is a git worktree** — `<repo>/.jkb/work/<session>` on branch `task/<session>`,
  one per task. `.jkb/` is added to `.git/info/exclude` on first use (locally, not by editing
  someone else's `.gitignore`), because otherwise the first session makes the tree dirty and
  `land` refuses a dirty target.
- **`jkb task work <uid>`** opens or *returns* the session — idempotent, so the button cannot
  fork the work. **`land`** rebases detached (never checking the branch out, which git refuses
  while the session holds it), fast-forwards the target, runs the gate on the integrated
  result, and rolls the target back on red. **`abandon`** drops it; **`sessions`** lists what
  is in flight; **`gate`** shows/sets the verify command.
- **The land target** is the branch you started from — unless that is trunk, in which case a
  branch is cut from trunk named after the first task and sessions hang off *that* (landing on
  trunk would make every task read as merged, D34.3). Later sessions join the batch the live
  ones share. Recorded as `branch_records.land_target`, beside the branch's measured cut point.
- **The gate is remembered per repo** in `namespaces.metadata.gate` on `repos/<repo>`:
  `--gate` wins, then the stored command, then autodetect (`scripts/check.sh`, `scripts/test.sh`,
  `make test`) — and a flag or a detection is *stored*, so the guess is made once. The chosen
  command is always printed; a gate that silently did not run is worse than none, because the
  landing reads as verified.
- **A session's claim is owned by the worktree; the pid is provenance, not liveness** (D36.6).
  Owner ids gained the form `session:<pid>:<worktree>`, and `is_alive` judges a session owner
  **only** by whether its worktree exists. `jkb task work` exits in a second, so a plain
  `host:pid` owner would be dead on arrival and `doctor --fix` would free the task mid-session.
  The pid is not consulted even as a fallback: it belongs to a process that has already exited,
  so it can only ever be wrong — falsely dead (its original bug) or, once recycled, falsely
  alive for a session `land` already removed. Only `land`/`abandon`, which remove the worktree,
  free a claim. There is deliberately **no attended/unattended axis**: nothing observable can
  tell a session you are sitting in from one you walked away from, and a flag built on that pid
  labelled *every* session unattended and advised abandoning it. `sessions`/`doctor` report what
  is observable — uncommitted work and commits ahead.
- **Branch existence counts the remote-tracking copy, and creating is not adopting.**
  `gitrepo::branch_ref(dir, branch, prefer)` is the one answer to "does this branch exist, and
  under what name" — `is_merged` and `close-merged` both ask it, because a branch living only on
  `origin/` is the ordinary state after a merged PR deletes the local copy, and a bare
  `refs/heads/` probe called that gone and advised deleting the tag tracking live work. The
  create-side is deliberately **two** functions, chosen per caller: `ensure_branch` prefers an
  existing remote copy (the caller is *referring to* a branch — an explicit `--onto <batch>`, a
  session branch whose commits may be pushed), `create_branch` takes `start` literally (the caller
  is *making* one, and a stale namesake on the remote must not be adopted in its place). Folding
  both into the primitive made it ignore its own `start` argument.
- **Location facets are set, not added.** `branch=`/`repo=` go through `set_facet`, which
  clears the facet's other values first. `tag::apply` is additive, which is right for open-ended
  facets and wrong here: a second `branch=` is a contradiction, not extra information, and readers
  that collapse the multi-map pick one and mint a second session for a task that already has one.
  `task_tags` therefore returns **all** values per facet, and the session lookup matches a task's
  recorded branches against the worktrees that actually exist.
- **`.jkb/base` is a reusable cache, released when its batch is spent.** It is switched to
  whatever branch a land needs (`git worktree add` refuses an existing path, so a second one
  would wedge landing until the directory was deleted by hand), and it is removed once its batch
  has merged — otherwise it both attracts new sessions onto a dead branch and stops
  `git branch -d` from deleting it.
- **The land lock is taken before the checks, not just before the graft** — which is what lets
  "is the target checkout dirty?" be asked **once**, by `staging::target_dirty_reason`, the same
  function the In Flight row renders. It used to be asked twice, in two wordings, on either side of
  the lock; the second copy did not close the window it justified itself with (it has the same gap
  to the graft) and, because both wordings shared a phrase, the test asserting on that phrase
  stayed green with *either* one disabled. A redundant guard that reads as protection is worse than
  none. `land_dir_for` keeps a dirty check of its own: that one guards the `git switch` it is about
  to perform across branches, with its own remedy, and is not a second copy of the land rule.
- **Which recorded branch a task's work is on is one rule** (`repo::work_branch`), shared by the In
  Flight row and `jkb task land`. Sharing the existence *predicate* was not enough: the row
  preferred a branch that resolves while the command took whichever `tag::applications` returned
  first — the lexicographically smallest — so a task carrying a stale `a-gone` beside a live
  `z-live` got two opposite explanations from the one shared blocker, and the command's advice for
  the branch it picked (`jkb task work`) cuts a *second* branch and detaches the task from its
  batch. A live session still wins outright: it is the branch with a checkout on disk.
  **It is asked through `repo::work_for`**, which returns the session *and* the branch together, so
  a caller cannot take one and pick the other for itself — which is what `jkb task abandon` did as
  a third implementation, taking the first `branch=` value (`tag::applications` orders by value) and
  deleting a stale sibling under `--delete-branch` while the row the user clicked named the live
  one. The batched listing still calls `work_branch` directly with the sessions and refs it has
  already read once: same rule, not a second one.
- **A land target is a *branch*, not a revision that resolves** (`gitrepo::branch_name` →
  `Is`/`Unknown`/`NotABranch`). `branch_ref` maps a branch name to a ref you may hand to git;
  this maps an arbitrary string to the **key** `branch_refs` uses, and the two come apart on
  exactly the values that hurt — `origin/<batch>` and a tag both `rev-parse` fine, and both were
  accepted and stored, the first under a key `jkb staging ls` cannot look up. The canonicalization
  and the refusal live at `repo::record_land_target`, the single writer, so the next flag that
  accepts a branch cannot get it wrong; the CLI verbs ask the same question first only for the sake
  of a sentence the user can act on. Trunk is compared against the canonical name, not against two
  spellings guessed by hand.
- `scripts/merge-queue.sh` is still the swarm's queue and still a git/gate runner, with **one**
  knowledge-base call: after a genuine fast-forward it runs `jkb task landed <branch> --onto
  <target>` to record the landing event (D46). That makes it a jkb client, so its caller must
  export `JKB` and `JKB_DB` — `.claude/workflows/task-swarm.js`'s `QUEUE_ENV` does, and the script
  header states the contract. `jkb task land` is the same algorithm in Rust for the human path
  (D36.1). The CLI is the home because the UI calls it directly and it must work in any repo.

## The lifecycle is a checkable state machine, and a landing is an event (D48)

**Reviewed in three ranges (`low`, ~18 agents): 36 findings, 8 must-fix, all fixed.** What the
reviewer caught that the machine's own checks did not, recorded because the pattern repeats:

- **Absorption discarded a plan.** The idempotence rule treated any event whose destination you
  were already in as a no-op — but arriving there another way leaves the plan unapplied.
  `abandon` on an operator-reopened task skipped its guard *and* its claim release, reported
  success, and the surviving claim held the task off every frontier. A row with a guard or a plan
  is never absorbed now; the domain declares the self-loop.
- **Two of my own tests could not fail.** One asserted `stdout contains uid` where the failure
  path prints the uid too; one asserted a defect that is structurally unreachable for its machine.
  Both are why `jkb task landed` never credited a swarm group for a whole branch.
- **A guard that only reports is not a guard.** The open-subtasks rule lived in the machine's
  `land` plan, which is applied *last* — so it narrated a landing that had already grafted and
  disposed of the session. It belongs in the preflight, beside every other precondition.
- **`Unknown` spelled as `No`, in the probe that protects every claim.** `pid_exists` folded "`ps`
  would not spawn" into "that process is gone" — the exact defect `Fact` exists to prevent.
- **A choke point with a third door.** `jkb task claim`, the verb the swarm runs on every task,
  still wrote the claim directly; swarm work had no `start` entry at all, and the two claim verbs
  answered `needs_review` oppositely.


The `staging-workflow` branch took 44 review passes and ~80 must-fix findings. Sorting the
task-lifecycle ones by *cause* rather than by site gives six groups, and each maps to a property
the code had no way to have. Design: `openspec/changes/jkb-state-machine/`.

- **The lifecycle was written down nowhere**, so about a dozen sites each derived the part their
  own question needed — `claim::claim`'s terminal pre-check, `staging::State::from_status`,
  `land_blocker`, `land_preflight`, `close-merged`, `task abandon`, `review record`,
  `merge-queue.sh`, the VS Code row. *Two of them answering one question differently* is the most
  common finding shape in the corpus, and the standing fix — make the two share a function — never
  reaches the thirteenth site.
- **`crates/jkb-fsm` is a dependency-free library where a lifecycle is a `&'static` table**, so it
  can be *walked*, and walking it is what makes these checkable at all: every state reaches a
  terminal one (`Wedged`), every state is reachable, no two rows compete (`Nondeterministic`),
  every reconciliation carries evidence (`UnguardedReconciliation`), every refusal's advice is an
  event the machine really accepts (`UnreachableRemedy`), every verb can be run twice
  (`Unrepeatable`), and under every observation something can still move the object (`DeadEnd`).
  `Machine::dot()` renders it — the artifact whose absence is the first item on the list.
- **`Unrepeatable` was found by the fix for the absorption bug below, and is the pair to it.**
  Correcting absorption — a row with a guard or a plan is never absorbed implicitly, because the
  object may have arrived by another route with that plan still owed — is right, and it silently
  turned five destinations into refusals: `land` on an already-landed task among them. That is
  S1.6's *the verb is re-runnable* lapsing, and a lapsed guarantee is worse than one never
  claimed, because the retry advice everywhere else assumes it. The two rules together say: **a
  verb you run is always answerable at its own destination, an observation only where somebody
  wrote down what re-seeing it means.** Satisfying it does not mean "make it a no-op" — a domain
  that wants the second run to fail declares a self-loop whose guard denies, and gets a sentence
  and a remedy instead of the silent absence of a row. Two rows here keep their guards on purpose
  (`abandon` from `open`, `observed_landed` from `done`): those verbs may still have work to do.
- **`Fact` is three-valued and has no method that collapses `Unknown` to a `bool`.** Nine
  must-fixes are one unobtainable answer spelled `false`: `ahead_count` returning `0` (which means
  *nothing to land*) for a branch it could not resolve; `has_own_commits` answering *no* when
  `rev-list` failed; a land gate that could not tell *no findings* from *the namespace resolved to
  nothing*. `is_yes` and `is_no` **both** mean *proven*, so a guard states its polarity in code:
  landing needs `work_dirty.is_no()` (an unreadable checkout refuses) and `has_commits.is_yes()`.
- **A transition yields its effects as one value.** `settle_landing` wrote the status, cleared the
  claim, then asked git to remove a worktree git refused — leaving a task `done`, unclaimed, with
  a live session. `Outcome::Moved` carries a `Vec<TaskEffect>` produced *with* the move, and
  `transition::perform` is the one seam that applies it. The ordering rule that makes a git
  failure survivable is stated once: **apply the plan last**, after every fallible external step,
  so a failure leaves the task where it was and the verb is re-runnable (which S1.6 guarantees is
  a no-op once it has worked).
- **A refusal names an *event*, not a sentence.** Passes 31 and 32 are the same finding one
  message apart — a printed remedy whose obvious argument froze the task permanently — and the
  fix each time was to reword. `Denial::remedy` holds a `TaskEvent`, and `Machine::audit`
  validates every remedy the machine can produce over a whole context matrix. **It caught a bad
  remedy in this change as it was written**, and a state passed beside the context that could
  disagree with it (fixed by `Stateful`: the state is read *out of* the observation).
- **`task::set_status` is not a hole beside the machine — it is the `override` event**, and a
  synced file's checkbox is `set_from_file`, a *guarded reconciliation* (the file may only speak
  for a task it backs). Both use `Dest::Stated`, a destination the caller names. One rule keeps
  the checks honest: **a `Stated` edge is excluded from the liveness walks**, because a state
  whose only exit is somebody naming a different state is still wedged.

### `branch_records` is gone; the history replaced it (V015, V016)

- **Two facts outlive git's memory**: where a branch was cut, and that it landed. `branch_records`
  stored them as *properties of a branch* — a mutable projection of the past, keyed by a name git
  lets you delete, recreate and reuse — so the row had to be kept in agreement with a moving
  world. The supersede clause, `landed_head`, the reflog instance anchor, `--forget`: every one
  was added after a defect, and all of them existed for that reconciliation.
- **`task_transitions` is append-only**, so it makes no claim about the present and there is
  nothing to reconcile. A name that changes hands appends a row rather than corrupting one;
  superseding stops being an operation. Deliberately **not** changelogged (the `blobs` precedent):
  it *is* an audit record, and a transition later reverted by `jkb undo` stays, which is the
  honest reading of a history.
- **Branch names became labels on events.** `land_target` is the last `onto` recorded — reset by
  an `abandon`, because where work lands is a property of the session doing it. Two tasks told
  different targets are two entries with timestamps, not one row keeping whichever wrote last.
- **`jkb task why <uid>`** prints it: every transition, who applied it, and the evidence each guard
  fired on. Fourteen must-fixes are "held for ever with no way to see why"; that is now one command.

### Evidence of a landing is spent once the task is put back to work

The log removed the reconciliation problem from **writing** — an append-only history makes no
claim about the present, so nothing has to be kept in agreement. It moved it to **reading**. Every
caller asks a present-tense question (*has this landed?*, *where does it land?*), and turning a
history into a present-tense answer needs a rule for when an older row stops counting — which was
written separately in each reader, and they disagreed. `land_target` stopped at `abandon`;
`landed` stopped at nothing. Five findings across two review rounds are that one gap.

- **The rule is asked of the status ORDER, not of a list of events.** `transition::resumed` is
  the one statement: the newest row that moved the task **backwards** through
  `open -> in_progress -> needs_review -> done` (`TaskStatus::stage`, the D27.7 lifecycle written
  down as an order). The obvious repair — give `landed` the same stop-list its sibling has — is a
  fourth private rule for a fifth reader to get wrong, and one a newly-added event has to be
  *remembered* and added to. Every row already records where it moved the task.
- **It took two goes, and the first was a narrower rule that looked identical.** *Moved out of a
  terminal status* is the same answer for the case it was written against — a landed task
  reopened — and misses the one that matters most: **`abandon` is `in_progress -> open`**, neither
  side terminal. A landing recorded while a task was held by an open subtask survived the abandon
  that destroyed its session, and the task auto-closed over live work. Asking the order covers
  both, plus `request_changes` and a resume out of `needs_review`, with no special case.
- **Nothing that stands still is a resumption**, which is what stops the row recording a held
  landing (`in_progress -> in_progress`) superseding **itself** and freezing its own task for
  ever. Found by running it.
- **`jkb undo` had to start recording what it did.** It restores `items.status` straight from the
  changelog, and `task_transitions` is deliberately not changelogged — so undoing a close left the
  landing looking live and the next `git pull` closed the task again, a loop undo could not break.
  It now appends an `undo` transition, from the statuses observed either side of the inversion
  rather than from what the entry claimed, and **only for a task that still exists**: inverting an
  insert deletes the item, and the history's foreign key onto `items` failed the whole undo.
- **A superseded landing is context, never a verdict** — and getting that wrong in *both*
  directions took two rounds. Spelling "spent" and "never landed" the same way sent `close-merged`
  off to ask GitHub about a pull request a locally-grafted branch never had, and reported that as
  the reason. Then treating "spent" as *the* answer, and returning on it, left a task whose work
  was redone and merged as a pull request permanently unclosable — printing *it will close when
  the new work lands* after the new work had landed. A stale local graft says nothing about
  whether the work reached its destination another way, so it falls through to the other evidence
  and only colours the reason when that proves nothing either.
- **`Landing` carries all of it from one read** — the landing, the resumption, the pull request
  number — because those three were fetched separately and the third re-derived a row the first
  had already found and thrown away, three history scans per task per `git pull`.
- **A review asks the present tense first, and the historical question only where it has no
  answer** — and getting that order wrong is the sharpest hole in the area, because it ends in
  landing unreviewed commits. `live()` credits; a task still aiming at this branch is *reported*,
  never credited, whatever it grafted before; and only a task aiming nowhere falls through to
  `recorded()`. That last case is what `abandon` leaves — it retires the land target — and a graft
  does not un-happen, so a session abandoned after its work reached the branch is covered.
  Asking `recorded()` first credited a task that landed, was reopened for a must-fix, and had its
  fix committed in a session the branch had never seen — recording that a review read work it did
  not read, and moving the task to `needs_review` under a live session. (It does **not** follow
  that refusing the credit stops an unreviewed landing in general: `gate_with` checks only that a
  `reviewed=` facet *exists*, never that it is current, and D38 declines to enforce staleness on
  purpose. It stops one only where the task had never been reviewed at all.) Asking
  `live()` alone dropped the abandoned case into `Credit::Unrelated`, which the loop discards. The
  discard is right and stays: that loop walks every task in the repo, so reporting `Unrelated`
  would list most of the backlog on every run.
- **The order is pinned where it is declared**, over all twenty-five status pairs plus the
  `None`/garbage cases. It had been rewritten twice, checked only through `close-merged`'s
  behaviour, and the one arguable rank — whether `cancelled` shares `done`'s — is exactly the edit
  a later reader would make.
- **It reaches the pull-request path too** (`pr::spent`), which is where it matters most: a merge
  reads as `MERGED` for ever, so reopening a landed task and running `git pull` closed it again —
  unattended, from the `post-merge` hook, over every task at once. That half **predates** the
  recorded-landing path and predates this branch.
- **Where it cannot be told, the task is held.** A merge with a known resumption it cannot be
  placed against is `Undecidable`, not "live" — closing there picks the burying direction on the
  strength of a missing field. `Live` is the default because *no resumption* is the normal case,
  not because a missed close is cheap: a missed close costs one command, a wrong one buries work
  in flight (D34.4).
- **The pure half is separated from the `gh` call** so it is testable at all — a rule exercisable
  only by shelling out to an authenticated network client is a rule nothing checks.

### Auto-close is a lookup on an id that is never reused

- **The inference was hard for one reason**: a squash or rebase merge rewrites the commits, so
  containment cannot be tested, and the weaker question `is_merged` asked — *does this branch add
  anything to trunk?* — cannot tell a branch squashed away from one that never started. Making
  that answerable needed the whole cut-point/anchor apparatus, and it produced roughly a quarter
  of the corpus's must-fixes.
- **A pull request number is minted by GitHub and never reused**, so there is nothing to
  disambiguate. `jkb task pr <uid> [number]` records or discovers it (refusing to guess when a
  reused branch name matches two); after that the branch name is never consulted. `close-merged`
  asks `gh`, and **everything degrades to `Fact::Unknown`, never to a `no`** — no `gh`, no
  network, no GitHub remote, an unrecognized state — so the task is *held with the reason printed*.
  It also produces an answer the inference could not: *closed without merging*. The field names,
  the flags and the **uppercase** state values are verified against `gh` itself rather than from
  memory (`gh pr view --json`'s own field list, `gh pr list --help`, and `gh`'s `display.go`); the
  one live call is an `#[ignore]` test beside the ollama and Chrome smokes.
- **Deleted:** `jkb-cli/src/base.rs` (932 lines), `jkb_core::branch`, `gitrepo::is_merged` /
  `MergeState` / `merge_base` / `has_own_commits` / `is_ancestor` / the reflog-anchor plumbing,
  `repo::landed_for_action` / `credited` / `clear_land_targets` / `measure_root_for`,
  `jkb task base`, and ~35 tests that pinned the mechanics rather than the rules. `V016` drops
  `branch_records` and migrates nothing, for `V013`'s own reason: importing values whose
  reliability was the problem defeats the store they are imported into.
- **What jkb performs, jkb records.** `jkb task land` writes a `land` transition after its gate is
  green; `scripts/merge-queue.sh` calls `jkb task landed <branch> --onto <target>`, which now
  closes every task on that branch. `jkb task review record` credits a task whose work jkb
  *grafted* onto the reviewed branch — a recorded event, where it used to be a containment probe
  that could not tell an empty session from a landed one.

### The second machine: the sync journal, and what it moved

`jkb-sync/src/lifecycle.rs` declares the per-file journal on the same library — a **reconciler**,
not a lifecycle: nothing finishes, every event is `Reconciled`, and the question is never *what
may I do next* but *which condition applies to what I just saw*. It moved the library three
times, which is the evidence that "it generalizes" is a claim worth making:

- **`is_terminal` → `is_settled`.** A synced file is never finished; it settles and is edited
  again. Under the old name the machine either had no terminal state — making `Wedged` vacuous —
  or had to lie about one. What the checks want is *rest*: the object owes the system nothing.
- **`State::awaits_input`** (default `false`). A conflicted file is waiting on a person, so no
  observation moves it; without this, `DeadEnd` fired on every such observation. A lifecycle
  keeps the default because an operator escape (`cancel`) is always available.
- **The initial state may be at rest.** A file an export-only mount holds no items for is nobody's
  business.
- What did **not** move is what carries the value — and `reconcile` refusing ambiguity turns out
  to be the *central* property here rather than a corner case, because evaluating every
  candidate's guard against one observation is exactly D45.5's *"a route is not a cause; the
  condition must dominate every arm"*.
- **The modelling found that `needs_attention` is two states** — a quarantine wants the file
  fixed, a blocked write wants the store fixed — which `Outcome::Refused`'s own doc already
  warned about in prose. And that a flag whose cause has gone is not always cleared (an
  import-only mount with a store-side-only change writes no row): modelled faithfully, filed, not
  fixed.
- `sync_state.status` now has **one writer** (`lifecycle::status_for`), replacing four
  hand-written spellings.

### The third machine: investigation units, where the rules are strategy-supplied

`jkb-core/src/nstype/lifecycle.rs` declares `items.resolution` on the same library — **two tables
over one state set**, which is the axis neither earlier machine had. It moved the library twice
more and found two rules that existed only in the shape of a function:

- **`debugging` concludes differently, twice.** A settled result can go **stale** and return to
  the frontier (an observation about a mutable system carries a `commit-range=`); and a tombstone
  is **not** revived by fresh evidence, where the base table's is. Both were already true — the
  first is one `if` in `debugging::resolution_rollup`, the second is that rollup's early return
  versus `default_rollup`'s fall-through — and neither was discoverable from anywhere else.
- **The strategy supplies the facts; the machine supplies the rules.** `resolution_rollup` (which
  returned a *conclusion*) became `unit_facts`. A rollup that concludes has to encode the priority
  of contradictory evidence in the order of its `if`s, where nothing can see it and nothing would
  notice a reorder. As guard clauses the priority is arguable and `audit` proves it exclusive. A
  strategy that merely *observes* differently — a `debugging` symptom is confirmed by a verified
  fix, not a `confirms` edge — now needs no table of its own.
- **Reachability counts a `Dest::Stated` edge; liveness still does not.** *Can the object be here*
  is answered yes by an operator override; *can the lifecycle get it out of here* is not.
  Collapsing them reported `abandoned` — which only a person ever sets — as unreachable dead code.
- **`Resolution::Unresolved` declares `awaits_input`.** Nothing the system can do moves it;
  evidence arrives from outside as an edge somebody links. Unlike a task's `open`, which always
  has `cancel`.
- **`UnusedEvent` is a per-machine statement**, and this is the first place two machines share one
  event enum. The domain filters it only where *another* machine in the family declares the event,
  and asserts the union separately — a narrow filter, so an event no table uses is still a defect.

### Claim keying: an owner id is a type, and `Unknown` is not `dead`

- **`jkb_types::AgentId`** replaces `split(':').nth(1)`: `Process { host, pid, run }` /
  `Session { pid, worktree }` / **`Agent { id }`** (new — an externally-minted identity from
  `JKB_AGENT_ID`, for a caller whose process and checkout are not the thing that persists) /
  `Unrecognized`. Each declares what would prove it via `Liveness`, a **closed enum**, so a new
  shape cannot be added without the compiler demanding a probe for it.
- **`owner::is_alive` returns `Fact`.** An `agent:` id and an id we cannot read are
  `Fact::Unknown` — *unestablished*, never *dead*. `transition::reclaim_dead` frees only claims
  **proven** gone and returns the rest in an `unverifiable` bucket that `jkb doctor` and
  `jkb task reclaim` report but never clear. That is a behaviour change: the old predicate treated
  an unreadable owner as reclaimable, which silently frees a live agent's task. Of the two ways to
  be wrong, the one that costs a command wins (D34.4).
- **The old objection to session ids answered a different question** — whether jkb could go and
  *ask* an agent something. A claim needs only a value stable for the life of the work; it does
  not need to be reachable. There is still no TTL and no heartbeat.
- **Reclaiming is a lifecycle transition** (`observed_owner_gone`, an effect-only self-loop), so it
  appears in the task's history and obeys the same evidence rule as everything else. Its effect is
  `ReclaimFrom(agent)`, distinct from `ReleaseClaim`, because the audit trail distinguishes the
  holder letting go from somebody else deciding it had.

## A branch is a record, not a tag value (D46) — SUPERSEDED by D48

**The `branch_records` table is gone** (`V016`), and with it the cut point, the instance anchor
and the landing columns. What survives is the *diagnosis* — an item-keyed, multi-valued, untyped,
open-write store cannot hold a per-branch fact — and the rule it produced: prefer an invariant the
schema enforces over one every caller must uphold. D48 applies it one level further, to the
question the table existed to answer. `branch=`/`repo=` stay facets, for the reasons below.


- **Re-founded by the B-series.** "Branch X was cut
  from commit Y", "X lands on Y", and "jkb merged X into Y" are facts about a *branch*. They lived
  as tag applications on whichever tasks happened to name the branch, and tag applications are
  **item-keyed, multi-valued, untyped and writable from any route**. Each of those four properties
  produced its own family of defects across fifteen review passes — 47 findings, 100 in the wider
  cluster, 20 must-fix:
  - item-keyed → the per-branch fact had to be encoded into the value (`base=<branch>:<sha>`), and
    that encoding leaked to ~12 sites with their own attribution rules;
  - multi-valued → the documented repair (`jkb task tag set base=`) **deleted other branches'
    records**, and records otherwise accumulated;
  - untyped → `HEAD` stored verbatim; a 40-hex string that is no commit accepted;
  - open-write → five write routes had to be taught the rule one at a time, the fifth found *after*
    a store-side reservation was added for the other four, and the reservation's own asymmetry was
    itself a must-fix.
  Six ascending choke points did not close it. The fix is the one D40 and D45 already made twice:
  **prefer an invariant the schema enforces over one every caller must uphold.** `branch_records`
  (migration `V013`, `jkb_core::branch`) is keyed `(repo, branch)`, so the encoding, the
  attribution rules and the question *"which branch does this value belong to?"* stop existing.
  Design: `openspec/changes/jkb-branch-records/`.
  - **D38.1's "no table" clause is repealed, openly — its *argument* is kept.** Branch **existence**
    is still derived from refs (`gitrepo::branch_ref(s)`), and no row is ever evidence a branch
    exists. What is stored is only the facts git does not own. The argument against a stored entity
    was always about copying a git-owned fact and then needing to reconcile it.
  - **`branch=` deliberately does **not** move.** "Which branch is this task on" is genuinely
    item-keyed, legitimately multi-valued, and round-trips through a synced `tasks.md` line. The
    findings there (`work_branch`, `close-merged`'s picker, `task abandon`) are **choice-rule**
    defects; a table permits two rows just as a facet permits two values and fixes none of them.
    `repo=` stays too, and is also the row's key column — that duplicates a *value*, not a fact.
  - **`onto=` does move**, to `land_target`. It was branch-keyed by accident of having one writer:
    two tasks on one branch could record different targets, and `None` could not be told from
    "never recorded". Now NULL on an existing row means *lands on trunk / on no batch* and a
    missing row means *unknown*. `reviewed=`/`review=` stay facets — nothing in the corpus is about
    their cardinality.
  - **Measurement is unchanged, and `jkb-cli/src/base.rs` still owns all of it.** Core owns storage
    and the CHECK; core does not shell out to git. Every rule below survives verbatim.
    - **The tip is a measurement result under exactly one condition, and never a fallback.** A
      branch with no commits of its own forked at its own tip, provably (`untouched_tip`, the one
      place that is turned into a value). Everywhere else a failed measurement records **nothing**
      and says why (`base::Missing` → `base_missing_because`, `close-merged`'s `undecidable`
      bucket): nothing is *reported and repairable*, a tip is silent and permanent.
    - **What is measured is a merge-base, not a tip** — the same commit whenever it is taken, which
      is why there is no longer a right moment to call the writer. `/task-swarm` can only name a
      group's branch after an implementer has committed on it.
    - **The parent is what the caller states in the call**, never a stored land target, which
      records an earlier moment.
    - **"Has this branch done anything?" is asked of git** (`has_own_commits`), so a stale, wrong,
      unresolvable or *grandparent* parent cannot change the one thing readers ask of the record.
      It answers `Option<bool>`, and the third state is load-bearing: `rev-list` exits non-zero on
      a broken ref anywhere under `refs/heads`/`refs/remotes`, and "git could not answer" spelled
      as *no* is the single worst value available — "untouched" is exactly the state in which the
      tip becomes storable. Same rule as `ahead_count`. It is **not** safe by construction and
      `base::rejected` is not its backstop, since `rejected` re-asks the same predicate and so
      agrees with a wrong answer; what covers a mis-exclusion is a test that fails loudly.
    - **The backstop:** the fork point is the later of `merge-base(branch, onto)` and
      `merge-base(branch, trunk)`. Every way of getting the parent wrong degrades towards **holding
      the task, never towards closing it**.
  - **The staleness rule is the write's *shape*, not a step in it.** A branch name outlives the
    branch that held it, so a recorded value on an untouched branch that is not its tip belongs to
    whatever had the name before. That is no longer `forget` ∘ insert: it is the `WHERE` clause of
    `branch::record_cut_point`'s single `INSERT … ON CONFLICT DO UPDATE`, which clears the
    predecessor's `landed_*` in the same statement. A port cannot drop it by omission or
    mis-sequence it — there is no sequence. What `base.rs` contributes is the *evidence*:
    `Cut::UntouchedTip` versus `Cut::Fork`, constructed only from `untouched_tip`'s answer.
  - **The instance anchor is the one sound read-time check, because it is not a signature.** Three
    states present one identical observable signature — no commits of its own, record ≠ tip, adds
    nothing to trunk: rebase-ff-merged externally, merge-commit-merged externally, and a recycled
    name. D34.2 requires closing the first and D34.4 forbids closing the last, so **no signature
    predicate evaluated at read time can be right**. A branch's *creation reflog entry* separates
    them: written once per instance, destroyed by the deletion that ends it, forged by no verb
    (`branch -f`/`checkout -B` append `Reset`-class entries), and its loss is structurally
    detectable because expiry removes oldest-first and only a creation entry has `old = zeros`.
    Stored as `(anchor_sha, anchor_ts)` — the pair, because recreating a branch from the same start
    point yields the same sha. **Not the message text**, which varies (`from main` / `from HEAD` /
    `from main~0`), and not `git log -g --format=%ct`, which prints the *commit's* time.
    - A **mismatch** is positive proof of recycling: it supersedes on the write side and refuses to
      act on the read side (`base::stale_instance`, `close-merged` and `review record`).
    - A **match plus a `commit`-class-only journal** licenses *retaining* a record on an untouched
      branch — the merged-away case, whose fork point discard-and-hold used to throw away. That
      relaxes a previously pinned direction, knowingly; unknown entry classes fail **closed**.
    - **Absent or truncated declines**, degrading to the untouched-tip predicate. Every failure
      mode lands on the old behaviour, never on a new close. Coverage is *established*, not
      assumed: `gc.refs/heads/<branch>.reflogExpire = never` is written beside the record (exact
      ref, so no naming scheme is needed) and removed when the branch is forgotten; `jkb doctor`
      reports entries for branches nothing records.
    - Residual, stated rather than guaranteed over: recycling where the anchor is unverifiable
      (reflogs off, hand-expired, or read in a different checkout), plus the remote-only path.
  - **Landing is an event where jkb performs it** — `jkb task land` after its gate is green, and
    `jkb task landed <branch> --onto <target>` for the merge queue, which is bash. It does not
    replace the inference, it *shrinks its domain*: from one branch per task to one per batch, and
    the survivor is the branch whose cut point is provable. `landed_head` — the branch's own tip at
    that moment — is what stops the event re-creating the same name-staleness one column over; the
    event is credited only while the branch still points there **or is gone**. The queue's verb is
    a new write route for a trusted fact, so it refuses unless the work really is in the target,
    judged by the same predicate readers use (**not** by ancestry: the queue rebases a detached
    HEAD, so every entry after the first has rewritten commits and its tip is no ancestor of the
    target).
    - **A landing onto the branch you are asking about *is* the answer** — `landed_for_action`
      stops there rather than walking on to ask "and is `S` contained in `S`?", which needs `S`'s
      own cut point. `jkb task review record` passes the *reviewed branch*, so without this it
      declined to credit work jkb had itself just grafted onto that branch. It is not the "landed
      onto a batch with no record" state, which is still **held**: there the target is a different
      branch, and whether it in turn reached trunk is a question the record genuinely cannot
      answer.
    - **The queue's verb reports, and deliberately does not measure.** The obvious fix for a
      landing onto a target with no cut point is to record one there — and it is wrong: a cut point
      is provable only while a branch is untouched, and a landing is exactly the moment the target
      stops being one. The queue's first entry fast-forwards the target onto commits its source
      branch still holds, so `has_own_commits` truthfully says "nothing of its own" and the **tip**
      gets stored for the whole batch, which is permanent. The record has to be made when the batch
      is *cut* (`--onto <batch>`); `jkb task landed` says so, on stderr and as `creditable: false`,
      and `merge-queue.sh` no longer swallows that.
  - **No verb anywhere accepts a commit id.** `jkb task base <uid> <branch> <sha>` produced three
    findings across three passes, all the same shape — the sha nearest a user's hand is the branch
    tip, and a cut point equal to the tip freezes the task at `NothingToMerge` with no repair path.
    Each was fixed by rewording a message; there are only so many messages. It is now
    **`jkb task base --forget <branch>`**, which drops the cut point (not the row: the branch still
    exists, and taking its land target with it would drop the task out of `jkb staging ls` as a
    side effect of repairing a commit id). `branch::forget` — the row delete — is
    `abandon --delete-branch`'s verb, where the branch really is gone.
  - **The transition deleted and back-filled nothing.** Back-filling imports exactly the values
    five passes proved unreliable; leaving them inert was unsafe once the reserved-facet apparatus
    went, since a surviving `base=` on a file-backed task would start exporting `#base=…` into
    synced files. The rows and the reservation had to go together.
  - **`V013` locks older binaries out of the global `~/.jkb/jkb.db`.** Accepted: `V012` already did
    on this branch, so anything that can open the database today is built from `staging-workflow`.
  - **A git ref (`refs/jkb/base/<branch>`) is still rejected.** jkb runs inside other people's
    professional repositories and must not decorate them with refs the user never asked for.
    Writing `.git/config` locally is judged differently — like `.git/info/exclude` (D36) it is
    local, unpushed, and cannot leak via push.

## Staging branches and review-gated landing (D38)

The branch a batch of tasks lands on before trunk. It is the **same thing** `/task-swarm`
calls its integration branch — cut from trunk, sub-branches rebase and fast-forward into it
linearly, the gate runs on the integrated result — reached by hand instead of by a
coordinator. Design in `openspec/changes/jkb-staging-workflow/`.

- **A staging branch is derived, never stored.** It is any git branch named by some task's
  branch's `land_target` that still exists. There is no `kind='staging'` item: which branches
  exist comes from git and which tasks are on them comes from the records, sessions live in git
  worktrees, merge state comes from `gitrepo::is_merged` (squash-safe, D34.2). A staging
  *item* would copy facts git owns and then need reconciling — the failure D36.2 avoided by
  refusing a session state file.
- **`jkb staging ls [--all]` is the ONE read** behind both the explorer's branch picker and
  its In Flight view, so the two cannot disagree about what is live. Each task carries a
  derived `state`: `implementing` / `review` / `landed` / `dropped` — `dropped` being a
  **cancelled** task that was on the branch, kept apart from `landed` because reporting the two
  as one would say a dropped task shipped. A branch adding nothing to trunk is
  either landed *or* freshly cut and still empty, and refs cannot tell those apart — **live
  work is the tie-break**, or the branch cut by the very first `task work` is hidden from the
  picker that exists to offer it.
- **"Does this branch exist" is answered with a ref, not a boolean** (`gitrepo::branch_refs`, one
  `for-each-ref` over `refs/heads` + `refs/remotes/origin`, local winning). Counting the
  remote-tracking copy admitted a pruned batch to the listing, and then every count was still taken
  with its bare short name, which resolves to nothing: `rev-list` exited non-zero, the failure read
  as **zero commits**, and the row refused a landing the command performed. Membership answers "may
  I show this" but not "may I ask git about it", and the second question is the one every consumer
  actually had. So `ahead_count` now **refuses** an operand it cannot resolve rather than returning
  zero — zero is a load-bearing answer here ("nothing to land"), and a count that could not be
  taken must not be spelled the same way. `land_preflight` asks `branch_ref` for the same reason:
  it asked `has_branch` while the row asked remote-inclusively, so the one shared blocker printed
  two opposite explanations of the same task.
- **Review state is two facets on the task**: `reviewed=<sha>` and `review=<ns>`. It is the
  one fact here with nowhere authoritative to live — git does not know, and the reviewer is a
  Claude workflow the CLI cannot run, so the CLI can only *require a record*. It deliberately
  does **not** live on the review folder's namespace metadata, which the sync engine owns
  (`layout`, `header_line`, `prose`); a second writer there is the class of bug that collapsed
  `openspec/`. Recording is keyed by **branch** — that is what a review knows — and a branch
  no task claims is a note, not an error.
- **The gate: reviewed, and no open must-fix.** `jkb task land` refuses a task with no
  `reviewed=`, or whose review has a `!p1` finding that is neither `done` nor `cancelled`
  (counted with `priority<=1`, terminal statuses filtered in Rust — `is:ready` is wrong
  because a *blocked* must-fix must still block). Checked **before the graft**, so a refusal
  has moved nothing. Concerns and nits never block: a previous run put 34 of 45 findings on
  `concern`, and blocking on those would make the override the normal path within a week.
  `--no-review` overrides and records `review-waived=<sha>` — an override nobody can see is
  indistinguishable from a rule that does not exist.
- **Status and the gate are not fused.** A task in `needs_review` with nothing outstanding
  lands; one moved back to `in_progress` with an open must-fix does not. Fusing them would
  make `jkb task set --status` the bypass. `needs_review` is the display state (D27.7);
  the findings decide landing. Recording a review is the **only** author of that transition.
- **`jkb task tag set`** is the sibling of `add`/`rm` that makes a value a facet's only one.
  `add` stays additive, honest to its name — an open-ended facet legitimately holds several
  values. `set` is for `branch=`/`repo=`, where a second value is a contradiction and a reader
  collapsing the multi-map picks one at random (D36.6). Load-bearing because `/task-swarm` re-tags
  a group on every pass. **It refuses `onto=`** — where a branch lands is a fact about the branch
  and lives in its record, so a facet of that name would reach no reader; use
  `jkb task work --onto` / `task start --onto`.
- **The swarm records where it is working.** `/task-swarm` sets `repo=` at claim, and runs
  `jkb task start --branch <group-branch> --onto <integration>` once the implementer has one —
  which records `branch=`/`repo=`, the land target and the *measured* cut point in one write, so
  the swarm supplies no value it could get wrong (see the measurement rules under D46). The land
  target cannot be recorded at claim, because at that point the group has no branch and the target
  is a fact about a branch. `staging ls` then shows swarm work and
  hand-driven work in one view rather than the half it was told about. `/review-log` calls
  `jkb task review record` after mounting its findings, and says whether the branch can land.
- **No review gate in `scripts/merge-queue.sh`** — deliberately, and that is the only sense in
  which D38 left it alone (it gained a `jkb task landed` call under D46).
  The swarm already runs a fresh REVIEWER before a group reaches the queue (D27.6) — that *is* its
  gate, and stricter.
  Requiring `reviewed=` there would make the REVIEWER write facets to satisfy a check its own
  approval already answered. **Review staleness** is recorded (`reviewed=<sha>`) but not
  enforced: making every post-review fixup force a re-review is the fastest way to make people
  reach for `--no-review` by reflex.

## Unattended Claude: the sandbox is the guarantee, the classifier is the ergonomics (D48)

Running the IDE and the CLI with no permission prompts, with a boundary that holds when the
model is wrong. `scripts/auto-mode.sh` + `scripts/auto-mode-posture.json`; design in
`openspec/changes/jkb-safe-auto-mode/`.

- **Claude Code already ships the boundary, so we do not build one.** 2.1.237 embeds
  `@anthropic-ai/sandbox-runtime`: every Bash command is re-executed under `sandbox-exec` with a
  generated seatbelt profile (macOS) or `bubblewrap` + seccomp (Linux), with a settings schema
  covering `filesystem.{allowWrite,denyWrite,denyRead,allowRead,disabled}`,
  `network.{allowedDomains,strictAllowlist,allowUnixSockets,…}` and `credentials.{files,envVars}`.
  That is strictly more targeted than a container — per-command OS confinement **plus** an egress
  allowlist **plus** credential denial, on the host, with the host toolchain.
- **Two layers, because they fail differently.** `--permission-mode auto` is a **classifier**
  (`claude auto-mode defaults` prints its rules as English prose): it decides what is worth
  asking about, it can be wrong, and it buys ergonomics. The sandbox bounds what happens when it
  is wrong, and it buys the guarantee. `autoAllowBashIfSandboxed` joins them — a sandboxed
  command is never shown to the classifier, so the OS boundary **is** the check.
- **`bypassPermissions` is the wrong mode, precisely.** The sandbox confines Bash and its
  children; it does **not** confine the in-process tools — the schema says so about
  `strictAllowlist` ("in-process tools such as WebFetch are not gated by this setting"). Skipping
  permissions leaves Read/Edit/Write/WebFetch unbounded, a hole the shape of the file-editing
  tools. `auto` keeps the classifier over exactly what the kernel does not cover, and the
  posture's `permissions.deny` rules close the named paths in **both** layers at once (the schema
  merges `Read(...)` deny rules into `filesystem.denyRead`, `Edit(...)` into `denyWrite`) — one
  list, two enforcers, rather than two lists that drift.
- **The posture is user-level, and Claude Code enforces that.** Several keys are honored only
  from user/managed/`--settings`, and the binary carries an **operator-posture guard** that
  refuses to run when a repo's `.claude/settings.json` negates `sandbox.enabled`,
  `sandbox.failIfUnavailable`, `sandbox.allowUnsandboxedCommands` or `disableAllHooks`
  ("operator posture belongs in the user-level settings.json"), and likewise refuses a project
  `env` block (`BASH_ENV`/`LD_PRELOAD`/`NODE_OPTIONS`/`GIT_*` are unsandboxed-exec inlets). So a
  cloned repo cannot switch it off — and installing the posture into a repo would silently drop
  half of it. It also has to hold in **every** repo under `~/repos`, which is why this is not jkb
  configuration.
- **Four settings, each closing one silent degradation.** `failIfUnavailable: true` is the hard
  gate: its default is `false`, under which a *warning* prints and commands run **unsandboxed** —
  the exact failure of believing you are protected. `allowUnsandboxedCommands: false` deletes the
  `dangerouslyDisableSandbox` parameter, or one argument steps outside and in auto mode nobody is
  asked. `network.strictAllowlist: true` **denies** rather than prompting, because a prompt in
  auto mode is a question nobody answers. `enabled: true` is the rest.
- **File access is an allowlist, both ways.** Writes were already default-deny (workspace only),
  so `filesystem.allowWrite` *is* the allowlist. Reads are default-deny too:
  `denyRead: ["~", "/Volumes"]` blankets the user's data and `allowRead` — which takes precedence
  over `denyRead` — re-opens the work roots and the toolchain. System paths (`/usr`, `/bin`,
  `/Library`) are deliberately **not** denied: a command that cannot read its own dynamic linker
  cannot run, so "allowlist everything" is not a posture but an inoperative machine; what is
  denied by default is *your data*, which is what a leak is about. The in-process `Read` tool
  **cannot** be made default-deny — Claude Code's rule model is deny-beats-allow with no
  re-allow, so a blanket `Read(~/**)` could never be punched through for `~/repos` — so there it
  is an enumerated deny list plus an empty `additionalDirectories`, and the stronger option
  (deny the `Read` tool outright, read through sandboxed `cat`/`rg`) is offered, not imposed.
- **What actually runs unsandboxed**, since that is the residual worth naming: `Read`/`Glob`/
  `Grep` (bounded by deny rules only), `Write`/`Edit`/`NotebookEdit` (deny rules + the permission
  scope), `WebFetch`/`WebSearch` (the schema states in-process tools are **not** gated by
  `strictAllowlist`), **MCP servers** (long-lived processes started at session start, never
  per-command wrapped — `jkb mcp` is one), **hooks** (not evidenced as sandboxed anywhere in the
  binary), and the `claude` process itself. Bash and everything it spawns — which is where the
  real capability lives — is the sandboxed part. Three keys answer what can be answered:
  `permissions.ask: ["WebFetch"]` (Read-anything ∘ WebFetch-anywhere is read-everything-send-
  anywhere outside the kernel boundary — the one composition that defeats the posture, so it is
  the single surviving prompt), `disableBypassPermissionsMode: "disable"` (the in-process layer
  is the *only* bound on those tools, so being able to switch it off is being able to remove
  them all), and `defaultMode: "auto"` (without it every IDE session starts prompting, and the
  ergonomic half of the ask is silently unmet).
- **The one place the obvious sketch inverts.** Granting `~/.claude` write access is the grant
  that must not be made: `~/.claude/settings.json` **is** the posture, so an agent that can write
  it can disable its own sandbox next session, and the guard above does not defend the file it
  lives in. The deny is kept narrow — `~/.claude/projects/**` stays writable (the auto-memory),
  and a repo's own `.claude/settings.json` stays writable because it cannot weaken the posture.
- **`check` is two generic rules, not a list of assertions.** Claude Code enforces the boundary;
  re-checking its enforcement here would be a second model of the world. What is ours is that
  **the posture is a file and files drift** — Claude Code appends to `permissions.allow` on every
  "always allow", `/statusline` edits the same file, `claude auto-mode reset` rewrites a section.
  So the posture file has two halves and `check` asks two questions. **`require`** (what `install`
  merges): is it a **deep subset** of the effective settings? Arrays are subset-by-membership,
  never equality, so domains you add yourself are fine. **`forbid`**: is each named key empty or
  absent? That second rule exists because **a subset check cannot express emptiness** — a posture
  entry of `excludedCommands: []` would assert nothing, and `excludedCommands` is the sandbox's
  own bypass list ("all bash commands must run in the sandbox unless they are explicitly listed
  in excludedCommands"); `permissions.additionalDirectories` is the other, since it widens the
  only bound the unsandboxed tools have. Adding a key to either half extends the check *and* the
  **tests**, which generate their cases from the posture file (flip every boolean, drop every
  list entry, populate every forbid key).
- **The sandbox engages — established on the host, with a control** (D48.14). This was the last
  open question of D48/D49, and it is settled for the host: with the posture installed, a `$HOME`
  write is refused with **`EPERM`** while a control write inside `~/repos` succeeds. Neither TCC nor
  ordinary permissions explains that — `$HOME` is `drwxr-x---` owned by the user — and the read side
  tracks the posture exactly across three plain dotfiles of identical TCC status: `~/.gitconfig` and
  `~/.zshrc` readable (both `allowRead`), `~/.zsh_history` denied. The container case is separate
  and still open: it needs an authenticated session *inside* the container, because the sandbox
  wraps commands Claude Code runs and a plain `docker run` shell has no Claude Code in it.
  - **`CLAUDE_CODE_SANDBOXED` is not the test, and this file used to say it was.** It was **unset**
    throughout the measurement above. `auto-mode.sh sandboxed` asks the kernel instead — a control
    write inside an allowWrite root, a canary write to `$HOME` — and reports CONFINED / NOT CONFINED
    / **INCONCLUSIVE**. Three rounds reported CONFINED for refusals that were nothing to do with
    the sandbox — a directory squatting the canary path, an absent `$HOME`, a read-only `$HOME`,
    then a writable `allowWrite` subdirectory *beneath* an unwritable one — each fix adding another
    observation to establish the premise *a write to `$HOME` would otherwise have landed*. **That
    premise is not establishable from inside**: the sandbox intercepts `access(2)` too, so
    `[ -w $HOME ]` reports policy rather than permissions and every side channel is filtered by the
    thing being detected. The **errno answers it directly and subsumes all of them** — `EACCES` is
    the permission bits, `ENOENT` is no parent, `EISDIR` is something in the way, and only `EPERM`
    (seatbelt) or `EROFS` (a bubblewrap read-only bind) is policy. Compared numerically, so no
    locale or wording is involved. The verdict stays a **pure function** so the unconfined arm is
    testable from a confined machine, and the classifier is pinned against real kernel answers.
  - Costs nothing and needs no session, unlike `probe` — which remains the fuller check (egress and
    credential reads as well) and which correctly reported **INCONCLUSIVE** here rather than a pass,
    because a subprocess `claude` has no credentials in an agent session (`loggedIn: false` even
    with the sandbox explicitly overridden off, which is what attributes it to auth and not to the
    posture).
- **The posture makes Docker unreachable, and that is the right answer.** After installing it,
  `~/.docker/bin/docker` fails with `Operation not permitted` — the directory is under
  `denyRead: ["~"]` and in no `allowRead` entry. An unattended agent that can reach Docker can
  mount `/` into a container and is root on the host, so this is the boundary doing its job; the
  cost is that `.devcontainer/verify.sh` and `mutate-verify.sh` become **human-run** steps, which
  is now stated where they are documented rather than discovered when they stop working.
- **Installing it for real found two things no amount of checking could.** `install` ran clean
  (preflight green, 45 pre-existing allow rules and the theme preserved, `/tmp`, `$TMPDIR` and
  `mktemp` all still working — the three failures of the first attempt, absent), and then:
  - **Three `Write(...)` deny rules were inert, and Claude Code says so on every session start**:
    *"Write(path) is not matched by file permission checks — only Edit(path) rules are. Use
    Edit(path) instead (Edit rules cover all file-editing tools)."* The `Edit(...)` rules for the
    same three paths were already there, so nothing was unprotected — but an inert rule in a
    security posture reads as protection, and a warning printed at every start is how people learn
    to ignore warnings. **The `claude doctor` schema check could not catch it**: the rules are
    schema-valid, and what is wrong is their *semantics*. Only running it surfaced them.
  - **A subset merge cannot express removal**, which is the same shape as the reason `forbid`
    exists (a subset check cannot express emptiness). Deleting those three rules from `require`
    did nothing: the merge is add-only for arrays — deliberately, so your own `permissions.allow`
    survives a re-install — so an entry once installed stays for ever while `check` tolerates it
    as an extra. The posture gained a third half, **`retire`**: array members it has withdrawn,
    removed by `install` and reported as drift by `check`. Without it the only repair is editing
    `settings.json` by hand, which is the thing this script exists to stop people doing.
  - **And the agent locked itself out, exactly as designed.** The first `install` succeeded because
    the deny rule was not yet in force; the repairing `install` could not write, and said so:
    *"the posture denies writes to itself, so installing or repairing it is deliberately a human
    action."* That property had only ever been asserted in a comment. It is now demonstrated — and
    it means a posture repair is the operator's to run, which is the correct end state and worth
    knowing before you need it.
  - `jq` gotcha, pinned by a test: `false // x` is `x`, so the obvious spelling of "the value, or
    null if absent" turns every correct `false` into a failure — and the strongest setting here
    (`allowUnsandboxedCommands`) is exactly that shape. Use `has`, never `//`.
- **The posture is validated against Claude Code's own schema.** `claude doctor` reports settings
  violations for the directory it runs in, so the tests hand it the committed `require` block in
  a temp project and fail on `Invalid settings` — a typo'd key or an out-of-range enum installs
  cleanly, is ignored at runtime, and is indistinguishable from a posture in force. That check
  was **inert when first written**: the stub `claude` the `run` tests put on `PATH` shadowed the
  real binary, so it was validating against a stub that prints nothing, and only the mutation run
  found it. Resolve the real binary before the stub exists.
- **`probe` takes its verdict from the filesystem, not the transcript** — what a model narrates
  about its own confinement is not evidence. It needs a real billed session, so it is this
  change's `#[ignore]` test and is never in `check.sh`. **Two files, because one cannot tell the
  two failures apart**: "the canary is absent" is evidence the *sandbox* denied the write only if
  the session ran the command at all, and with the sandbox off — the state the probe exists to
  detect — Bash is no longer auto-allowed, so the **classifier** gets the out-of-bounds write and
  will very likely refuse it, and an absent canary would read as a clean pass. A control file
  written inside the workspace separates "denied at the boundary" from "never ran", and the
  second is reported **inconclusive**, never as a pass. The canary is deliberately not
  dot-prefixed: `~/.jkb-…` shares a prefix with the allowed `~/.jkb`, and that near-miss is how a
  probe comes to lie.
- **The container: measured, and the first answer here was wrong.** This section originally
  argued a container "buys nothing, because the sketch mounts `~/repos` and `~/.claude` and that
  *is* the blast radius". That is right about **Bash** and wrong about everything else — the
  generalisation was the error. For the in-process tools a container is not a second copy of the
  seatbelt: it puts the `claude` process itself in a mount namespace, so `Read`/`Glob`/`Grep`/
  `Edit` become default-deny **by the kernel**, which is exactly what the deny-beats-allow rule
  model cannot express. Genuine depth, and it closes the hole above.
  - **Whether the layers compose was measured** (Lima VM, Ubuntu 26.04 / kernel 7.0, Docker 29.7),
    with a no-container baseline first so a failure is attributable to the container profile and
    not the kernel. **Stock Docker cannot host it** — not root, not non-root, not with
    `--cap-add SYS_ADMIN`, not with AppArmor off; `bwrap` fails at namespace creation every time.
    The blocker is **seccomp**, and the fix is narrower than the folklore: neither `--privileged`
    nor `seccomp=unconfined` is required. Docker's *default* profile plus an unconditional allow
    for `clone, clone3, unshare, setns, mount, umount2, pivot_root, mount_setattr, open_tree,
    move_mount, fsopen, fsconfig, fsmount, fspick` suffices — and those are then usable only
    *inside* the user namespace `bwrap` creates, where the process holds no privilege over the
    host. It must also run **non-root**: with seccomp off, root in a container still cannot create
    a mount/net/pid namespace directly. Dev Containers already default to non-root.
  - **Two questions, and only one is about Docker.** Docker hosts limited mounts trivially — that
    is where the default-deny read property comes from, and it needs no seccomp work. The table is
    about the *different* question of running Claude Code's own sandbox **nested inside** such a
    container. "Stock Docker cannot host it" conflated them: false of the mounts, true only of the
    nesting. **Container-only is available today**; the seccomp profile is the price of keeping
    both layers, not of admission.
  - **Not established:** that Claude Code *itself* engages or refuses in a container. Two probes
    failed instructively. `claude -p` with an **invalid** key hangs with zero output — and hangs
    identically with no sandbox config, so the control proved it was the fake key; with **no** key
    it exits in a second (`Not logged in`), so Claude Code runs fine in a stock container. That
    suggested a credential-free discriminator, since `failIfUnavailable` is documented to error at
    startup and so should precede auth — **it does not**: in a stock container, where bwrap
    provably cannot create a namespace, it still printed `Not logged in`. So the sandbox is
    checked lazily, or auth precedes it; either way the probe cannot discriminate and the
    prediction behind it was wrong. Settling it needs a real session plus one `printenv
    CLAUDE_CODE_SANDBOXED` — i.e. credentials inside the container, which is the credential
    owner's call.
  - **What it does not buy:** `~/repos` mounted is still writable and push-able — the win is
    bounded to what you did not mount. And container egress is unrestricted by default, so if the
    inner sandbox ever fails to start you lose `strictAllowlist`; a container without its own
    iptables/ipset allowlist is a **downgrade** on egress. On macOS both container paths are a
    Linux VM, so the native loop (pinned rustup, `sqlite-vec` FFI, headless Chrome, launchd,
    worktrees under `~/repos`) has to be re-plumbed. That cost is unchanged; what changed is that
    the security argument now favours the container where it did not before.
- **Cross-platform, with the differences named rather than smoothed over.** The posture file is
  `~`-relative and carries both platforms' paths; macOS-only keys (`allowAppleEvents`,
  `enableWeakerNetworkIsolation`, `allowUnixSockets`) are inert on Linux and harmless. What
  actually differs: the mechanism is **bubblewrap + seccomp**, so `bubblewrap` and `socat` must be
  installed — `check` **warns** and `run` **refuses**, deliberately split, because "has the posture
  drifted" and "can this machine honour it" are different questions with different fixes and one
  exit code must not mean both. `denyRead` covers `/media`, `/mnt` and `/run/media` as well as
  `/Volumes` — `/mnt` being the most valuable entry on WSL, where the Windows filesystem lives —
  and `~/.cache` is in `allowRead`/`allowWrite` because without it a Linux build cannot read its
  own caches, and a posture too tight to work is one that gets switched off.
  - **`JKB_AUTO_MODE_SSH_AGENT` is macOS-only, and now says so.** `allowUnixSockets` is documented
    "Ignored on Linux (seccomp cannot filter by path)", so the overlay was a flag that reported
    success and did nothing — a guard that cannot fire. Linux's only lever is
    `allowAllUnixSockets`, all-or-nothing, which is not something to switch on behind a flag whose
    name promises a single socket. The test is branched per platform and each branch was run on
    its own platform, not inferred.
- **`preflight` exists because every live breakage was knowable without installing.** The first
  real install denied its own settings file, `$TMPDIR` and `/tmp`, and all three are facts about
  the machine's *resolved* paths rather than about the settings file — so no amount of checking
  the posture could find them. `auto-mode.sh preflight` resolves what the machine actually needs
  (`$TMPDIR`, the real path of `/tmp`, `$PWD`, the settings file, the toolchain roots) and reports
  any that no `allowRead`/`allowWrite` entry covers; `install` runs it and **refuses** on a gap
  (`--force` overrides). Verified by reverting the posture to the version that broke the machine:
  it names all three, each with its fix.
  - **It compares against the entries AS WRITTEN, not only resolved.** Resolving both sides makes
    `/tmp` and `/private/tmp` agree, which would have hidden the exact symlink mismatch that
    denied `/tmp` — the sandbox matched the real path while the posture named the link. A path
    covered *only* after resolution is reported as a latent gap, not as covered.
  - **A passing preflight names what it cannot check.** `install` refuses on its verdict, which
    makes it read as authoritative, so "no gaps — this posture is workable" claimed far more than
    a filesystem-path check supports. It now says "no FILESYSTEM gaps" and prints the four blind
    spots every run: setuid-root exec (refused under any posture, not configurable, surfacing as
    an opaque exec denial — it cost a peer session a red gate), unix sockets, the unvalidated
    domain allowlist, and whether the sandbox engages at all. Same rule as everywhere else here —
    an unstated gap in a tool something gates on is indistinguishable from coverage.
  - **Deliberately not in `check.sh`**: whether the real posture covers the real paths depends on
    where the checkout lives (`~/repos` on a dev box, `/home/runner/work` in CI), so a passing
    assertion would be a test of the machine. The tests exercise the *logic* — a posture covering
    nothing is refused, one covering everything is not, a symlink listed only by its link name is
    flagged.
  - **It asked the deny side against `$HOME` while the posture declares five deny roots.** The
    posture also blankets `/Volumes`, `/media`, `/mnt` and `/run/media`, so a cargo home on an
    external volume — or, on WSL, anything under `/mnt/c`, which is where the Windows filesystem
    lives — was reported "outside denyRead", `install` was not refused, and every sandboxed build
    then failed to read its own registry. Exactly the two-readers-of-one-fact shape this file
    argues against everywhere else, in the tool whose whole job is to predict that breakage.
    `denyRead` is now read from the posture like its allow-side siblings.
  - **`cd ""` succeeds in bash, which quietly made an empty posture list mean `$PWD`.** jq prints
    nothing for an empty array, a here-string of nothing is still **one empty line**, and the
    resulting empty entry resolved to the current directory and entered the list as a prefix.
    **It never produced a false pass** — reproduced against a posture covering nothing, `$PWD`
    still reports a GAP, because the arrays feeding the *covered* branch are built without `cd`
    and `covered()` skips an empty prefix. What it produced was the wrong **remedy**: the checkout
    matched the resolved-only list, so the gap advised "covered only if the sandbox follows
    symlinks, list it literally" instead of "is in no allowWrite entry" — and on the deny side it
    would have been a false GAP, over-strict rather than under. (An earlier version of this bullet
    claimed the checkout read as *covered*. That was wrong, and wrong in the dangerous direction;
    a reviewer caught it and the paragraph now records what running it shows.) Fixing it exposed
    the other half: arrays that had always held at least one element could now be genuinely empty,
    and `"${arr[@]}"` under `set -u` on bash 3.2 aborts the script. Both were latent behind the
    same masking bug.
  - **`set -e` made the three-state check unreachable.** `settings_state` returns 0/1/2 and a bare
    call returning non-zero aborts the script before `case $?` runs, so the distinction existed
    and never fired. `|| st=$?` is what turns a return code into a value. Caught by the tests,
    and pinned by reverting it.
- **`~/Documents` is a useless sandbox canary on macOS.** TCC denies it to the terminal whether or
  not any sandbox is running, so a probe that reads it always looks confined — which is how a
  restored, sandbox-free machine was briefly misreported here as still sandboxed. Test
  confinement against a path the posture itself governs, never one the OS already protects.
- **The liveness probe stopped shelling out (D48.12), and the dilemma dissolved.** `ps` is
  setuid-root on macOS, a sandboxed process cannot exec setuid, so under this posture
  `owner::pid_exists` could never run and every `host:pid` owner read as `Fact::Unknown`. The only
  sandbox-level lever was `sandbox.excludedCommands`, which runs a command **wholly outside** the
  sandbox — and `forbid` requires that list empty precisely because `require` cannot bound it
  (subset semantics would let `["ps"]` become `["ps","bash"]`). Neither was needed: `ps` was
  chosen over `kill -0` because it reports processes it does not own (D27.2), and that reasoning
  is about the **shell builtin**, which collapses `EPERM` and `ESRCH` into one non-zero exit. The
  *syscall* separates them, and **`EPERM` is positive evidence of existence** — the kernel refuses
  because the process is there and is not ours. `rustix::process::test_kill_process` is a safe
  wrapper (no `unsafe`, and rustix was already in the tree), so the probe needs no subprocess, no
  `PATH`, and no setuid binary.
  - **Better with no sandbox in the picture at all**, which is the test that keeps it from being
    chosen for the wrong reason: no fork/exec per probe, no `PATH` dependency, identical on macOS
    and Linux. The mapping is a pure function, so the `Unknown` arm — the one that protects every
    claim — is an ordinary assertion instead of needing a deliberately-broken spawn (the previous
    version reached it by naming a nonexistent program, after an earlier one emptied `PATH` and
    reddened the shared gate one run in six).
  - **A pid outside `pid_t` is `No`, not `Unknown`**: no process can carry that id, so its absence
    is established rather than unobserved. That preserves the prior behaviour exactly.
  - **Untested:** whether a sandbox profile permits `kill(pid, 0)` against a *foreign-owned*
    process. It barely matters in practice — jkb's claimants are the same user's processes, so the
    live answers are `Ok`/`ESRCH`, neither of which needs privilege — and a denial would return
    `EPERM`, i.e. "alive", which is the safe direction (never reclaims live work).
- **A review round found six must-fix and eight concerns, and four of them were one shape**
  (D48.13): an assertion that matched text present on **both** the pass and fail paths, or read a
  file nothing had written. `mutate-verify.sh` grepped a label `verify.sh` prints identically
  either way, so 2 of 5 mutations reported CAUGHT with the guard deleted — under a summary line
  reading "every guard fired". `verify.sh`'s mount check filtered `mountinfo` by target prefix, so
  it *was* the list of absences this file claims it is not; `/var/run/docker.sock` passed. The
  seccomp assertion was satisfied by the generator's own trailing allow group, true by
  construction. And a Linux-only test grepped an argv file `run` never creates, passing having
  observed nothing. The fix is the same in all four: **assert on a discriminating signal** — a
  non-zero exit plus the FAIL-only rendering, the full mount set minus the runtime's own, the
  negative "no restricted entry still names these", the precondition that the file exists — and
  where a harness judges other guards, give it a **negative control**: an unmutated run must be
  reported MISSED, or the matcher is matching something present when nothing is wrong.
  - **The next round's must-fix was inside that round's fix, and it is the same shape one level
    up.** Removing the credential mount and renaming the cargo volume deleted **three** lines of
    `verify.sh`'s hand-written mount list where two were intended, dropping `.cargo/registry` — so
    a correctly-built container failed its own verifier, after the full toolchain build, because
    `setup.sh` ends by running it. Two lists that must agree **is** the defect: the list is now
    **derived** from `devcontainer.json` (both the string and object mount spellings), so there is
    one. A `CARGO_TARGET_DIR` guard added an hour earlier pinned a single string in that very file
    and could not see the list beside it — a guard aimed at the instance, not the class.
  - **The third round found the same class again, so the fix stopped being a fix and became a
    harness.** `verify.sh` had `mutate-verify.sh` watching it fail; `check-config.sh` had nothing,
    and rounds two and three each found an assertion in it that could not fail — a regex that
    could not cross a shell quote and so never caught the exact code it existed to prevent, and a
    rewrite that dropped the `type=volume` half of its own check while keeping the failure message
    about volumes. Hand-mutating after each round works until nobody does it.
    `.devcontainer/mutate-config.sh` breaks each config property in turn (18 of them) and requires
    a FAIL naming it, with the same negative control — and it needs no Docker, so unlike
    `mutate-verify.sh` it runs in `check.sh` and CI. **It found a live one on its first run**: the
    seccomp assertion grepped for the `seccomp=…` value anywhere in the file, so deleting the
    `--security-opt` flag and orphaning its value passed — Docker would apply its default profile,
    bubblewrap would fail, and the config still read as declaring one. It is asserted as a
    flag/value pair in `runArgs` now.
  - **A skip decided per-assertion is not a skip.** `run` refuses on a Linux host without
    bubblewrap, correctly — but the argv assertions ran unconditionally, so the shared gate went
    red for a fact about the machine; and the drift assertion three lines below asked only for a
    non-zero exit and no argv file, which is *exactly* what that dependency refusal produces. It
    would have reported a pass having never exercised the refusal it names. The group is skipped as
    a group, announced, and the drift assertion now matches the refusal **text**.
- **Two more were the posture not covering what the machine needs**, the D48.10 shape again in a
  new place: `CARGO_TARGET_DIR` was a *sibling* of the allowlisted `~/.cargo`, and `covered()`
  matches at component boundaries, so `denyRead: ["~"]` blanketed every sandboxed build in the
  container while both guards reported it healthy. Moved inside `~/.cargo/target`, and `preflight`
  now reads `CARGO_TARGET_DIR` so the class is checkable rather than latent.
- **`install`'s preflight gate turned CI red**, which is the coupling CLAUDE.md already says to
  avoid: preflight asks about the *machine*, and the hermetic suite drove `install` through it, so
  on a runner (`/home/runner/work/...`, under no allowWrite root) install wrote nothing and two
  assertions passed **vacuously** on the empty result. The suite now passes `--force`, and the gate
  gets one deliberate test of its own instead of being exercised incidentally by all of them.
- **The credential mount could not work on macOS.** `~/.claude/.credentials.json` is Linux/WSL
  only — macOS keeps credentials in the Keychain — and a bind mount with a missing source is a
  hard error, so the container never started on the host this repo is developed on. Removed
  entirely: authenticate *inside* the container (`claude auth login`), into the state volume,
  which also fixes the read-only mount that a login could not have written.
  - **Removing a mount left the plumbing shaped around it.** `setup.sh`'s symlink loop linked only
    the *directories* under `~/.claude`, because the credential file was the one thing bind-mounted
    and the loop had to go around it. With the mount gone, nothing linked it — so an in-container
    login sat in the writable layer and died with the next rebuild, while `devcontainer.json`
    promised the opposite. `.credentials.json` and `~/.claude.json` are now linked into the volume
    too, dangling until first write. The residual is stated rather than designed away: a writer
    that replaces a file by temp-and-rename would drop the link, which costs one re-login at the
    next create and can never reach the host.
- **Stated residuals, not guaranteed over.** In-process tools are bounded by permission rules and
  not by the kernel, so a path nobody named is Read-able. MCP servers and hooks are unsandboxed
  processes with no posture key to reach them (use `--strict-mcp-config` in a repo that is not
  yours). Any allowlisted host is an exfiltration channel — egress control bounds *where*, not
  *what*. And a posture too tight to work gets switched off, which is why `check` tolerates
  entries you add and `probe` reports inconclusive rather than pass. `~/.ssh` being unreadable means SSH pushes
  fail inside the sandbox (correct for an unattended agent; `JKB_AUTO_MODE_SSH_AGENT=1` allows the
  agent **socket** instead of the key, so it can authenticate but never read it). `localhost`
  egress and that socket overlay are unverified without a live session, and both fail in the safe
  direction.

## Both layers: the dev container with the sandbox nested inside (D49)

`.devcontainer/` is the "both" configuration of D48 — a container **and** Claude Code's own
sandbox running inside it. `scripts/auto-mode.sh` alone is host-only; this adds the one property
the host cannot express. Design in `openspec/changes/jkb-safe-auto-mode/` (D48.7, D49).

- **The container's job is file access; the sandbox's job is everything else.** An unmounted host
  path does not exist in the container, so the in-process tools (`Read`/`Glob`/`Grep`/`Edit`) are
  bounded by the **mount namespace** rather than by permission rules — default-deny by the kernel,
  which the deny-beats-allow rule model cannot express at all. The nested sandbox still supplies
  per-command Bash confinement and the precise hostname allowlist.
- **The egress firewall exists because `strictAllowlist` lives inside the layer that might not
  start.** A container's default egress is unrestricted, so a container whose nested sandbox
  failed silently would be a **downgrade** on exfiltration versus the host. `init-firewall.sh` is
  coarse (IP-level) and independent; the sandbox is precise (hostname at a proxy) and in-process.
  Coarse-but-independent under precise-but-fragile is the point — they fail for different reasons.
  - **One allowlist, not two.** The firewall reads `.require.sandbox.network.allowedDomains` out
    of `scripts/auto-mode-posture.json` — the same file the sandbox posture comes from. Two egress
    lists that can disagree is how the tighter one ends up decorative.
  - **A wildcard cannot be pinned to an IP**, so the firewall skips `*.rust-lang.org` and says so;
    the posture therefore also names `static.rust-lang.org` concretely. Without that the toolchain
    download is blocked by the coarse layer while looking allowlisted in the file.
  - **It is installed into the image, root-owned, with sudoers granting exactly that one path.** A
    script the agent can edit, that the agent can also `sudo`, is a root shell with extra steps.
  - **…and all of that was decorative, because the base image grants blanket passwordless root.**
    `mcr.microsoft.com/devcontainers/base` ships `/etc/sudoers.d/vscode` containing
    `vscode ALL=(root) NOPASSWD:ALL`, so the agent could flush the firewall, delete the allowlist
    snapshot, rewrite the root-owned script, or simply `sudo -i`. Every root-ownership protection
    in this design rests on the agent not being able to become root, and none of them checked.
    Removed in the Dockerfile, with `visudo -c` failing the **build** on a malformed result — a
    broken sudoers file locks out the one grant the firewall needs. **Found only by running
    `sudo -n -l` inside the container**: two review passes and every static check missed it,
    because nothing in the repo *says* the base image grants it. The general lesson is the one
    this section keeps relearning one level up — a guard is worth what its weakest bypass is
    worth, and the bypass here was inherited rather than written.
  - **The check for it asks sudo, not the file.** `verify.sh` requires that every command `vscode`
    may run as root names the firewall, so a blanket grant re-added by *any* route — a rebuilt
    base image, a feature, a helpful `RUN` line — fails it. `mutate-verify.sh` restores the grant
    in a one-layer image built `FROM` the real one and requires that failure, since a sudoers
    entry cannot be broken with a `docker run` flag.
  - **…and that argument was the hole the script itself closed for the script.** The caller passed
    the allowlist's path, and the path passed was the repo's own copy — bind-mounted, under
    `allowWrite`, writable by the agent this layer exists to bound. Appending a domain and waiting
    for the next container start had root add it to the ipset. The sudoers entry made it worse
    rather than better: **a command naming no argument accepts every argument**, so any readable
    JSON on the box was a valid allowlist. Now the workspace copy is read exactly **once**, at
    container create, before any agent session exists, and snapshotted root-owned; every later run
    reads the snapshot, the script **refuses** arguments, and sudoers pins it to none (`… ""`). A
    rebuild is what re-reads the repo, which is the right ceremony for widening egress. Divergence
    between the two is **reported and never acted on** — silence would let a legitimate edit look
    applied when the coarse layer never saw it.
  - **Order:** the Dev Containers lifecycle is postCreate → postStart, so `setup.sh` raises the
    firewall as its *first* act. Leaving it to `postStartCommand` alone would run the whole of
    create — including a toolchain download — with open egress.
- **Measured, not assumed** (Ubuntu 26.04 / kernel 7.0 / Docker 29.7, baseline outside a container
  first): **stock Docker cannot run the nested sandbox** — `bwrap` fails at namespace creation as
  root and as non-root, with `--cap-add SYS_ADMIN`, and with AppArmor off. The blocker is
  **seccomp**, and neither `--privileged` nor `seccomp=unconfined` is required: the default profile
  plus 14 namespace/mount syscalls suffices. **Non-root is load-bearing, not hygiene** — with
  seccomp fully disabled, *root* in a container still cannot create a mount/net/pid namespace
  directly.
- **The mount list is the security boundary, and it is asserted exhaustively.** `verify.sh` reads
  `/proc/self/mountinfo` and fails if the mounted set is anything other than what
  `devcontainer.json` declares — rather than listing paths that ought to be absent, which is the
  "enumerate the secrets" shape the host posture is *forced* into and the container is not. Only
  `~/.claude/.credentials.json` is mounted, never `~/.claude`: that directory holds the posture,
  and a process the posture bounds must not read or write the file deciding whether it is bounded.
- **Every guard has been watched failing.** `mutate-verify.sh` breaks each property in turn — an
  undeclared mount, the host `~/.claude` mounted, stock seccomp, no `NET_ADMIN`, running as root —
  and requires `verify.sh` to fail naming it. It needs a Docker host, so it is an `#[ignore]`-class
  test; `check-config.sh` is the host-side half that runs in `check.sh`, and its real job is the
  **generated** seccomp profile: a patch that no-ops against a changed upstream yields a profile
  that parses, applies, and leaves the sandbox unable to start.
- **Three defects found only by building and running it**, none of which static review would have
  caught. (1) `jkb` was not installed in the container at all — the explorer extension spawns it
  and every workflow verb needs it; `setup.sh` now builds and installs it, and `jkb 0.1.0` was
  confirmed running inside. (2) **A named volume whose path does not exist in the image is created
  root-owned**, so cargo died with `EACCES` minutes into the first build; every volume mount point
  is now created in the Dockerfile, which is what makes Docker seed ownership from it. (3) Cargo's
  `target/` moved off the bind mount into a volume — required by (2)'s uid mismatch, and
  independently right because it is the heaviest write path and a macOS bind mount is a VM
  filesystem crossing.
  - **An out-of-memory build does not say so.** `rustc` is SIGKILLed and cargo reports a bare
    `(signal: 9, SIGKILL: kill)`. Diagnosed from `dmesg` rather than guessed at, twice — the first
    diagnosis was right about OOM and wrong about the cause, since the test VM was holding a 2.9 GB
    copy of the repo on tmpfs. Worth knowing because the symptom names neither memory nor the fix.
- **Where VS Code runs does not matter; where the `claude` process runs does.** Under Dev
  Containers the UI is on the host and the server, extension host and terminals are in the
  container. The Claude Code extension declares no `extensionKind` and has a Node `main`, so VS
  Code runs it as a **workspace** extension — in the container — and `devcontainer.json` lists it
  so the linux build is installed inside (a host copy is platform-specific and separate).
- **Open the repo root, not a session worktree.** `jkb task work` puts worktrees at
  `<repo>/.jkb/work/<session>` and a linked worktree's `.git` is a *file* pointing into
  `<repo>/.git/worktrees/…`. Mounting the root puts both ends inside; mounting only the worktree
  breaks git, because the gitdir it names is not there.
- **Still not established:** that the nested sandbox actually *engages* for a tool call. `bwrap`
  working is the mechanism, not the product, and the credential-free probe does not discriminate
  (see D48.7). Settle it in a live session with `./scripts/auto-mode.sh sandboxed`, **not** with
  `printenv CLAUDE_CODE_SANDBOXED` — see the measurement under D48 below.

## The dev container mounts ~/repos, and a session worktree is archived (D49)

The container follow-up bucket. Design in `.devcontainer/README.md`; the container harnesses are
`check-config.sh` + `mutate-config.sh` (static, in the gate) and `verify.sh` + `mutate-verify.sh`
(need Docker).

- **`workspaceMount` binds all of `~/repos`**, not just this repo. The argument is *consistency*,
  not convenience: `scripts/auto-mode-posture.json` already grants `~/repos` in both `allowRead`
  and `allowWrite`, so a container holding only jkb was **tighter** than the boundary the same
  agent runs under on the host — a difference nothing had decided, which made a cross-repo task
  impossible rather than deliberately refused. `workspaceFolder` FOLLOWS the folder you opened
  (`${localWorkspaceFolderBasename}`), and `initializeCommand` refuses one the mount cannot
  place — see the derived-folder note below.
- **A nested bind must be NAMED, never inferred.** `verify.sh` compares exact mount points with no
  prefix logic (prefix filtering is what once let `$HOME` at `/host` through), so once the declared
  target is the parent, `mutate-verify.sh`'s own `-v $REPO:/home/vscode/repos/jkb` is undeclared —
  and it cannot mount the parent instead, because in a `jkb task work` session `$REPO`'s parent is
  `.jkb/work`. The tempting rule — *nested is fine* — is wrong: a mount point and a mount SOURCE
  are independent, `-v ~/.ssh:/home/vscode/repos/jkb/secrets` is inside the declared region and is
  still exfiltration, and the source cannot be checked from inside (Docker Desktop for macOS
  reports the path inside the VM, which is why `dc_mount_sources` is host-side only). So
  `verify.sh --declare <target>` **adds** to the derived set and is **refused** unless the value is
  a strict descendant of something already declared **as a bind** (not a volume, which reaches no
  host filesystem, so nothing nested under one is reviewable this way) — `/host`, the docker socket and
  `~/.claude/settings.json` are all refused by verify.sh itself, which is exactly the mutation set
  the harness exists to catch. The count prints in the passing line (D38's `--no-review` lesson).
  A blanket rule would have weakened the *shipped* container; this weakens only the harness, and
  only where a path is written down in a diff.
- **Auto-memory is shared through `~/.jkb`, with no new mount.** Claude Code keys memory by the
  project's absolute path, so one repo has two keys (`-Users-…-repos-jkb` /
  `-home-vscode-repos-jkb`) and widening the workspace mount does not change that. Binding the
  host's memory dir is the one mount forbidden everywhere in this design (`~/.claude` holds
  `settings.json`, which IS the posture), it collides with `dc_link_state`'s symlink, and the slug
  is inexpressible in `devcontainer.json`. So `scripts/link-claude-memory.sh` symlinks each side's
  `memory` dir at `~/.jkb/claude-memory/<repo>/` — inside the bind that already exists. It migrates
  file by file and **never overwrites**: a name on both sides is left alone and reported. Opt-in on
  the host (`setup.sh --link-memory`), because `post-merge` re-runs `setup.sh` and a `git pull`
  must not rearrange somebody's `~/.claude`. Stated plainly: agent-writable prose flowing from a
  bounded context to a less bounded one is a channel, argued for rather than added by reflex.
- **A session worktree is ARCHIVED, never deleted** (`jkb-cli/src/archive.rs`). `git worktree
  remove` unlinks recursively and stops at the first refusal; from inside a sandboxed session that
  refusal is `<worktree>/.claude/settings.json` — Claude Code protects a project's policy files
  from the agent whose policy they are — and 152 files were already gone, with the error naming the
  *directory* and not the 62,421 lines. Disposal is now one atomic `fs::rename` into
  `<repo>/.jkb/archive/<session>-<stamp>`: partial destruction stops being representable rather
  than being guarded against. `jkb task reap` deletes an archive once it is 30 days old, probing
  with `remove_dir` first (`EPERM` vs `ENOTEMPTY`) so it never begins a walk it cannot finish.
- **The refusal is scoped to the session's OWN working directories, and that is what makes
  deferral work.** Measured across five live worktrees: only the session's own tree answers
  `EPERM`; every other one answers `ENOTEMPTY`. And the deny is **not** ours — `auto-mode-posture
  .json` names only `~/.claude/*` — so there is no knob, and there should not be: a session that
  could write its own `.claude/hooks/` could run anything. So `land` never blocks: it grafts,
  records the worktree it could not move, applies its plan (D48's ordering intact), and any other
  process finishes it. `jkb service install` now writes **two** units — `com.jkb.sync` and
  `com.jkb.reap` — kept apart so a wedged file watcher does not also stop every deferred landing.
  `jkb doctor` reports what is outstanding; `--fix` sweeps it.
- **The reviewer found the second disposal route, and it is the shape this repo keeps meeting.**
  `jkb task abandon` still called `git worktree remove` — the verb an operator reaches for to clear
  the directory a deferred landing leaves behind was the one that gutted it. Both verbs now go
  through `archive::dispose`, which is the callee that remembers the rule instead of two call sites
  that must. Its `delete_branch` is the caller's, because a landing's branch is a duplicate of
  commits already in the target while an abandoned branch holds the only copy.
- **A record names a path and a branch, and both are reusable names**, so the sweep establishes
  identity before acting: git still registers that path as a worktree, it is still on the commit
  the landing recorded (`Entry.head`), and it is clean. Remove a deferred worktree by hand and
  `jkb task work` recreates a session at the same path on the same branch; a sweep keyed on those
  two would archive the live tree and force-delete its branch. A commit id is not reused.
- **Unknown is not settled, and one sweep runs at a time.** A repo root the sweep cannot reach used
  to clear the record — ordinary once host and container share `~/.jkb` at different paths, and it
  deleted the only record of a live worktree. Two concurrent sweeps did lose each other's updates
  (the second finds the worktree gone and drops the record the first just wrote), so a `SweepLock`
  covers the reads as well as the writes, with `LandLock`'s rule that a lock is stale only when its
  holder is **proven** gone.
- **`workspaceFolder` follows the folder you opened, and `initializeCommand` refuses one the mount
  cannot place.** A literal path under the target opens whichever repo sits there — for a session
  worktree, the main checkout — silently, with every guard passing, because the wrong repo is a
  perfectly good repo. `check-config.sh` asserts both halves and `mutate-config.sh` watches each
  fail.
- **The memory linker decides the whole migration before moving anything**, and refuses a store
  holding anything but plain files (a symlink planted by either side redirects the other's reads
  and writes, including back into `~/.claude`). `verify.sh` **asks** it for the state rather than
  inferring breakage from a missing link — the linker leaves the link absent on purpose in states
  it recognises, so the inference failed `postCreate` for a state the design calls normal.
- **The record carries the decision that produced it (`archive::Plan`), and can be cancelled.**
  Three findings in review 2 were one cause: `dispose` took `delete_branch` as an argument and
  threw it away, so the reaper applied *land's* defaults to an `abandon` record and force-deleted
  the branch the verb had just printed "kept" for; `--force`'s acceptance of a dirty tree was
  likewise unrecorded, so the sweep's own dirty check held that record for ever; and nothing could
  revoke a record, so `jkb task work` resuming a deferred session got the directory back with a
  reaper still holding a claim on it — which then either archived the checkout the operator was
  sitting in or, once they committed, refused for ever as "a different session reusing the name".
  `Plan` is part of the `Entry`, `archive::revoke` is the cancel, and `task work` calls it.
- **A guard whose expectation no longer matches its subject is a guard nobody has seen fire.**
  Rewording verify.sh's `--declare` refusal left `mutate-verify.sh` grepping for text it never
  prints — and that harness needs Docker, so the gate could not notice. `check-config.sh` now
  checks statically that every expectation is a string verify.sh can print. Same round: an
  assertion that the workspace is mounted had been rewritten to say the directory containing the
  running script contains a `Cargo.toml`, true by construction (its third wording, below, is the
  one the harness accepted); and a self-test assertion could
  not fail because `note` wrote to the stdout `run` was capturing, so every clean link run exited
  1 with its own report eaten.
- **`init-firewall.sh` discovers its workspace instead of naming it.** The hard-coded
  `~/repos/jkb` was a statement about whichever checkout sat there once the mount widened — it
  would snapshot another checkout's egress allowlist as the root-owned list every later start
  runs on. It cannot be told which (the sudoers grant forbids arguments, and an environment
  variable is agent-settable), so one repo carrying `scripts/auto-mode-posture.json` is the
  answer and two is a refusal — on the **first raise only**, because a later raise that exits
  non-zero leaves the rules unapplied, and unapplied rules mean unrestricted egress.
- **A remedy the machine does not accept is worse than no remedy.** `check-workspace.sh` advised
  setting `JKB_REPOS_DIR`, read by nothing else in the tree: following the advice switched the
  preflight off without moving the mount, producing the exact silent wrong-checkout open the file
  exists to prevent. One statement of where repos live, and it is `devcontainer.json`'s.
- **The sweep may only delete inside the tree it owns.** The worst defect on this branch, and it
  survived two reviews: the retention arm passed whatever absolute path a record's `archive` field
  named to `remove_dir_all`. The record store lives in `~/.jkb`, which is bind-mounted into the
  container and granted in the posture's `allowWrite`, while the host's reaper is a launchd agent
  outside every sandbox — so an agent-writable JSON file steered an unsandboxed recursive delete
  at any directory, past a probe that answers "permitted" for any ordinary one. Corruption reaches
  the same place with no adversary. Both paths are constrained to `<repo>/.jkb/{work,archive}`,
  checked **once above both arms** (D45.5's rule: a condition that dominates every arm belongs
  above the dispatch) — the pending arm had a guard and the archived arm had nothing.
- **A refusal that installs no rule is not a refusal.** Two guards in `init-firewall.sh` exited
  before any iptables rule was applied, and rules do not survive a container restart — so a
  truncated snapshot, the state one of those guards' own comments calls real, left unrestricted
  egress on every later start, permanently (the snapshot is root-owned and 0444). `fail_closed` is
  defined above every refusal now and `check-config.sh` fails the gate on an `exit 1` in that file
  outside it. That guard's own first version anchored its pattern to the start of a line and
  walked past `|| exit 1` — the exact shape it existed for; the mutation caught it.
- **A pid is meaningless without the host that issued it.** `Liveness::Process` carried only the
  pid, so a claim (or a sweep lock) written inside the container was probed against this machine's
  process table: a live owner reported dead and freed, or a dead one reported alive. It carries
  the host, and a foreign one is `Unknown`, which frees nothing. `hostname()` also stopped falling
  back to the literal `"localhost"` — which both sides of the boundary answered, so the rule's two
  sides gave the same name and the rule was not one.
- **The container is where deferral is normal, and it had no finisher.** A session cannot archive
  its own checkout, so every `land` in there records one — and the host's reaper correctly holds
  those records, because it cannot see `/home/vscode/...`. There is no init system in the
  container to run a service, so `postStartCommand` sweeps once per start, best-effort behind the
  firewall raise.
- **The container harness settled two findings no amount of reading would have.** `verify.sh`'s
  workspace assertion went through three wordings — a hard-coded path (describes whichever
  checkout sits there), the script's own directory (true wherever it can run), and the declared
  target in `mountinfo` (which the harness's own bind layout never produces, so every mutation
  reported CAUGHT and then the control failed and the run judged nothing). It asks whether this
  checkout is inside a mount point that is both mounted and declared, `--declare` folded in. And
  the harness's negative control could not fail: the health check establishes the control's exit
  code is 0 and `judge` reports CAUGHT only on non-zero, so `MATCHER IS BROKEN` was unreachable —
  in the file whose whole job is finding guards that cannot fire. It asks the discriminating half
  instead: the label must appear in a healthy container and must not be on a `FAIL` line.
- **The record store is untrusted input, so it gets a parser.** The containment guard that closed
  round 3's arbitrary-delete finding did not hold: `Path::starts_with` compares components without
  interpreting them, so `<repo>/.jkb/archive/../../../Documents` "starts with" the archive root
  while naming something else. A check the sweep remembers to call, over paths nobody normalized,
  is two mistakes. `Entry` is now the wire form and is trusted for nothing; `archive::Record` is
  what the sweep sees; `Record::parse` is the only way between them, so no arm can be written that
  skips it. `..` and `.` are **refused** rather than resolved — nothing here writes one, so a
  record containing one is corrupt or hostile and neither deserves a best-effort reading.
- **Reachability belongs above the dispatch, like containment.** The pending arm held an
  unreachable `repo_root`; the archived arm read "not visible from here" as "somebody removed it
  by hand" and dropped the record — so each side of the container bind destroyed the other's
  archived records, and the multi-gigabyte checkout each named became unreferenced and permanent.
  An absent directory is evidence of removal only when the repo it lives under is reachable.
- **One disposal, one record.** The marker's name was a pure function of the worktree path, and a
  session name is reused: abandon, reopen, `task work` mints the same name at the same path, and
  the next disposal wrote over the first record. `Entry.head` exists because a path and a branch
  are reusable names; the record's own identity was still the path.
- **A lock that nothing can break is a wedge.** Making a foreign host `Unknown` was right, and it
  made the sweep lock permanent for a container killed mid-sweep and then rebuilt — its hostname
  gone with it, so every sweep on both sides no-ops for ever. The default stays (breaking a live
  sweeper's lock is what the lock prevents); what was missing is an escape a person can take, so
  the refusal names the lock file and its holder and `jkb task reap --break-lock` exists.
- **Sharing memory through `~/.jkb` opened a channel nobody had priced.** `~/.claude/projects` is
  under the posture's blanket `denyRead` and in no allow list, so sandboxed Bash cannot touch
  auto-memory; `~/.jkb` is in `allowRead` **and** `allowWrite`, because the database lives there.
  Linking therefore moves memory from a place sandboxed Bash cannot reach into one where a single
  auto-approved command rewrites it, for every repo. The posture has no write-deny to carve it
  back out with, so it is **accepted and stated** rather than mitigated — and the round-1
  comparison that chose `~/.jkb` over a dedicated bind was made without this, so it is worth
  re-deciding rather than inheriting.
- **`verify.sh` refuses to run outside the container, and never passes on a table it could not
  read.** Run on the macOS host it printed fourteen confident FAILs about a machine that was never
  the subject — and two `ok` lines, because `/proc/self/mountinfo` does not exist there, so the
  mount-boundary check compared an EMPTY set and passed. `EXPECTED` had been guarded against
  emptiness since the day it was derived; `actual` never was, in the one assertion the file exists
  for. The read is now kept separate from the result, and an unreadable table is a FAIL. Found by
  running the script in the wrong place, which is the sort of thing no amount of reading finds.
- **`gitrepo::deletions_only`** tells a part-way removal from work in progress. The second land
  attempt refused with *"it has uncommitted changes — commit them in the session first"*, which
  over 152 deletions means committing the wreckage. Asked as four whitespace-free git questions,
  not by parsing `status --porcelain` — whose leading status column is exactly what the trimming
  capture helper eats.

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

## Design gate (D28) — human design, swarm implementation

The swarm implementers run headless (Workflow sub-agents) and **cannot ask the user** about
undecided design. So design is separated from implementation by a tag gate (D28):

- A task is swarm-eligible only when tagged **`design=approved`**. `/task-swarm` (scout +
  every SCHEDULER pass) ANDs `tag:design=approved` into its `task next`/`query` selection in
  scope mode, so un-triaged tasks are invisible to the swarm. Bypasses: `--no-design-gate`
  and explicit-uid mode.
- **`/design-pass <path>`** is the interactive counterpart: it walks open, un-triaged tasks,
  settles each design *with the user* (via `AskUserQuestion`), records it, and only then runs
  `jkb task tag add <uid> design=approved`.
- Decisions are recorded in an openspec change's `design.md` under `openspec/changes/<name>/`
  (one folder per group of related tasks), keyed by `Governs: <uid>` so the implementer greps
  it by uid — **not** in a running `design-notes.md` log. A small, standalone design can instead
  live only as the inline `Design:` note on the task. Either way the decision is also stamped
  into the task body (`jkb task edit --append` for managed tasks; the source-file line for
  file-backed ones); trivial tasks skip the write-up and are fast-tracked straight to the tag.
  The IMPLEMENTER reads the approved design first and follows it rather than re-deciding.
- **Gate DSL gotcha:** use `tag:design=approved` (the query DSL). The `#facet=value` form is
  quick-add-only, and `task next` silently drops non-`tag:`/`ns:` terms — so `#design=approved`
  in a `task next` scope is ignored (parsed as dropped free text).

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
skips.

Deferred: item/document body editing, drag re-placement, live refresh, in-tree search, the
web-app package.

## Investigation namespaces (Dmem, the `jkb-memory` change) — `memory/`

`memory/` is no longer an empty reserved root: it holds **investigations** — open-ended,
multi-agent knowledge work whose state outlives any one context (design
`openspec/changes/jkb-memory/`, Dmem.0–9). The bet is the same one jkb was founded on:
coordination lives in the *store* (items + typed edges), never in agent chat.

- **The universal shape (Dmem.0).** An investigation is a typed namespace holding a typed,
  scored graph read back as three buckets — **frontier** (live + unblocked, ranked),
  **confirmed core** (settled results), **tombstones** (dead ends + the edge to what killed
  each) — plus a `reflection` **digest**. It terminates on a **goal predicate**, never a timer.
- **Thin base + pluggable strategy (Dmem.1).** `jkb-core/src/nstype/` is the seam:
  `trait NamespaceType` declares node kinds, edge subset, verbs (as *data* — `VerbSpec`),
  `frontier`, `ranking`, `resolution_rollup`, and `goal_predicate`; `resolve(name)` mirrors
  `jkb_sync::serializers::resolve` (one match arm + one `AVAILABLE` entry per strategy). A
  namespace's type lives in `namespaces.metadata.type` (`ns::set_type`/`effective_type`, which
  **inherits** down the subtree). Untyped namespaces behave exactly as before.
- **Two registered strategies.** `nstype/debugging.rs` (symptom → hypothesis → experiment →
  observation → root-cause → fix → **verify**; mutable system, so observations carry
  `commit-range=` and go `staleness=stale`, excluded from frontier/ranking but never deleted)
  and `nstype/conjecture.rs` (prove **or** disprove one conjecture under one structure —
  approach-family registry + `family_pressure`, blocked-with-reason via a `gap` the route
  `depends_on`, gated `reopen_gate`, `is_anti_progress` over `equivalent_in_strength_to`,
  adversarial `audit`, and `open_gaps_under` as the machine-checkable "no partial results" bar;
  prove vs disprove differ **only** by the acceptance preset on the goal).
- **Schema (`V007`, additive).** `items.resolution` (indexed, CHECK-constrained: `unresolved|
  success|dead_end|superseded|abandoned`; NULL reads as `unresolved`, so nothing is back-filled)
  and `edges.weight REAL` (signed evidence; NULL reads as 1.0). Memory units are ordinary items
  of non-task kinds with NULL `status` — **no new tables**.
- **Query primitives.** `Query` gained `resolution`, `kinds` (union), `exclude_kinds`
  (`-kind:k`), `exclude_tags` (`-tag:f=v`), `frontier`, `tombstone`, `claimed`; the DSL gained
  `resolution:<r>`, `is:frontier`, `is:tombstone`, `is:claimed`/`is:unclaimed`. `is:frontier` is a
  strict generalization of `is:ready` — for a task (NULL resolution) it selects exactly the same
  rows, and nothing may write a task's `resolution` (`investigation::resolve_unit` and `roll_up`
  both refuse) or the two would diverge. Every strategy's frontier starts from
  `nstype::base_frontier`, which excludes `NON_WORK_KINDS` (`reflection` — a digest is memory
  *about* the investigation, not work in it).
- **Engine + surface.** `jkb-core/src/investigation.rs` owns create/add/`apply_verb`/the three
  buckets/`anti_retread`/`roll_up`/`digest`; `jkb related <uid>` is the new edge-walk read
  (`edge::walk`), and `jkb inv ls|new|verbs|kinds|frontier|core|tombstones|retread|evidence|
  digest|rollup|do|add|link|promise|resolve|reopen|stale` is the surface. `jkb inv new` also
  saves three per-bucket views so the buckets are reachable from the generic `jkb view` surface.
- **Never hard-delete a dead end.** Resolve it and link the edge that killed it. The graveyard
  is the memory — it is the single highest-value thing in the store.
- **Deliberately deferred** (documented in Dmem.7, do not build without a design pass): the
  evolutionary-search / tournament / blackboard / literature-synthesis strategies, the
  software-swarm retrofit, bi-temporal validity, the scheduled reflection pass, and an MCP
  memory surface. Driving investigations at large fan-out is
  `task:scale-up-the-task-swarm-to-drive-18c6cc7853efc280`, not part of this change.

## Sync: one directory, one synced file (the `openspec` collapse)

A `tasks`-serializer mount over `openspec/` overwrote **62 of 63 files**: in each change
folder `design.md`, `proposal.md` and `.openspec.yaml` were left byte-identical to one
another, and every markdown header in the tree was stripped. Two independent defects lined up.

- **`namespace_for` drops the filename.** A file's namespace is derived from its *containing
  directory*, so every file in a directory shares one namespace — and with it the `layout`
  that `assemble_kb_doc` reads and that `render` treats as the sole authority on document
  order (see the prose note below). Items were correctly per-file, via
  `binding::synced_uris_for_file`; the document *structure* was not. So each file rendered
  whichever sibling last wrote the shared layout.
- **`mount create` is a full-row replace that doubles as the update command.** Its SQL sets
  every column from the arguments, so a re-run that omitted `--include` wrote NULL over the
  stored glob. The mount had been restricted to `**/tasks.md`; one re-run to change the
  conflict policy silently removed that restriction, and the next sync discovered the whole
  tree.

Three guards, each closing a different link:

- **`Outcome::Collided`.** `colliding_paths` refuses — reads nothing, writes nothing — any
  file sharing a namespace with another synced file. It checks both the current batch and the
  bindings already in the KB, so a single watch event still sees the sibling it would collide
  with. Gated on `SyncSerializer::requires_exclusive_namespace()`: `tasks` opts in, `document`
  does not, because one item per whole file consults no layout and many of them share a
  directory safely. Two files in a directory are not a merge to resolve — nothing in the store
  says which file the shared layout belongs to — so refusing is the only correct answer.
- **`mount create` preserves what you did not name.** It reads the existing mount and only
  applies the flags actually passed (`FieldEdit::{Keep,Set,Clear}`); `--no-include` /
  `--no-exclude` clear explicitly. It prints the resulting configuration every time, and
  `mount ls` now shows mode/policy/globs — a mount whose glob had been dropped previously
  looked identical to one that still had it.
- **`jkb sync --conflict <policy>`** overrides the policy for one run. The only way to unstick
  a conflicted file used to be re-creating the mount with a different `--policy`, which is
  precisely the write that dropped the glob. The mount no longer has to be edited to get a
  sync moving.

**Superseded.** "A `tasks` mount can hold at most one synced file per directory" was true when
this was written and is no longer: D39 below makes the filename part of the namespace, which is
the migration and design pass this paragraph deferred. `Outcome::Collided` and the whole
ownership guard are gone. The two defects above — `namespace_for` dropping the filename, and
`mount create` being a full-row replace — are both closed, the first at the root.

**Recovery, for next time.** `blobs` is content-addressed and never garbage-collected, and
file sync stores the bytes of every version it settles — so the store is a complete history of
every synced file. `jkb blob ls --contains "<a line you remember>"` finds the version, `jkb
blob cat <hash>` writes it out. That is how all 62 files were recovered here; the originals
were the import cohort, distinguishable from the damaged exports by still having headers.

## A file's document lives on its journal row, not in the namespace tree (D45)

The root fix for a class of data loss that produced a must-fix in eight of nine review passes.
Design in `openspec/changes/jkb-staging-pr/`.

- **One sentence covers every incident**: *an unverified KB render reached `write_file`.* The
  openspec collapse, prose orphaning, layout ownership (seven guards), `retire_undeclared_sections`
  retiring a neighbour's sections, `jkb ns mv` destroying a document — six **causes**, one
  mechanism. D39 removed a cause; D41 tried to check the output. Neither touched *why* the render
  can be wrong.
- **The cause is storage.** A file's structure — its `##` headers, their order, its prose — sat in
  `namespaces.metadata`: a shared, globally addressable, **user-mutable** hierarchy. A file's
  structure is private to that file and must round-trip exactly. `jkb ns mv` and the VS Code
  Rename button reach it; `namespace_for` then recomputes the path from the *file*, the layout is
  unreachable, and the export arm writes a structureless render over your file.
- **It moves to `sync_state.document`** (migration `V012`), keyed `uri TEXT PRIMARY KEY` — at most
  one row per file, so two files sharing one structure is **unrepresentable**. `reconcile` already
  loads that row first, so reading structure from it is free, and `decide_direction`'s byte fast
  path and `Outcome::Normalized` both survive (deriving it from the base blob instead would have
  cost a load + parse per file per sync and given up both).
- **The property this buys:** `apply_doc` is the only writer of a file's structure, and the
  `(false, true)` export arm does not call it — so **an export can change item lines but not
  structure.** Two paths escape that (`missing_file`, and `kb_wins`, which incorporates disk
  changes by design), which is what the guard below is for.
- **Being a migration is load-bearing.** Refinery verifies every applied migration before running
  any, so a binary older than this one fails at `Db::open` rather than silently reading namespace
  metadata nothing refreshes any more and exporting from it. That ruled out a dual-write.
- **One guard survives**, and it is a *different* harm: `assemble_kb_doc` skips a bound item with
  no primary placement, so `jkb undo` after a re-home (`placement::set_primary`'s delete has no
  inverse) silently deletes its line. `finish_export` now refuses — `Outcome::Refused`, journalled
  `needs_attention`, nothing written — and recovery is any edit to the file, which imports
  normally.
- **`wholesale_loss` — one condition, judged on documents, decided above the direction dispatch
  (D45.5).** The two routes D45 left open were each found and fixed *at the route* — pass 21 at
  `finish_export`'s `(false, true)` arm, pass 22 at `three_way_resolve`'s `!ctx.imports()` arm.
  Both fixes were correct and neither was the last, because a route is not a cause. The condition
  is: **the KB contributes zero items to a file that declares some.** It compares two documents,
  never the store, because the store is what these incidents damage — `jkb undo` of a sync deletes
  a file's items **and their bindings** together, so `dropped_items`, which walks bindings,
  truthfully reports nothing dropped; there is nothing left to walk. One condition covers undo,
  `jkb item rm`, a half-applied migration, an emptied binding table, and the next thing with that
  shape. Deliberately *not* a general "fewer items than disk" rule: on an export-only mount the
  file is a projection and hand-added lines are legitimately removed.
  - **An empty rendered document is not proof of an empty store.** `assemble_kb_doc` also omits an
    item that is still *bound* and has merely lost its primary placement — what `jkb undo` after a
    re-home leaves, which is D45's own motivating verb — and a `document` mount is one item per
    file, so a single dropped placement empties the render. So the condition asks the store too:
    anything still bound means the `dropped_items` refusal and its one-command re-home remedy, on
    every mount mode; nothing bound means the items really are gone and re-reading the file is the
    recovery. Without that split the guard turned a refusal into a silent import that overwrote
    content, status and priority from disk.
  - **Detecting it is not refusing it.** Pass 23: sited inside `export_blocker` the only available
    answer was "refuse", and refusing is wrong on two of the three mount modes — it protected the
    file and left the KB **permanently** empty, since a refusal never advances the base, so the
    next sync re-entered the same arm forever and the message's own remedy ("edit the file") is
    what routes it there. It now runs in `reconcile` **above the direction dispatch**, where it
    dominates every arm, and the mount mode decides: a mount that can import **re-imports the
    file** — the disk being the good copy is the condition's own premise — and only an export-only
    mount, which cannot read the file back, refuses. Each arm below would otherwise have needed
    its own gate, which is the shape this whole area keeps failing at.
  - `finish_export` still takes the **`SyncDoc`** and renders it itself, so what was judged is
    necessarily what gets written.
- **The mount-mode axis is a test matrix, and it asserts BOTH sides.** Three consecutive passes
  produced the same shape of must-fix — "this arm behaves differently on an export-only mount and
  nothing tested that axis". `no_mount_mode_and_stage_loses_a_task_line` runs {import, export,
  bidirectional} × {first sight, settled, disk-changed, kb-changed, both-changed, post-undo,
  kb-emptied}. Its first version asserted only that the *file* keeps its lines and passed the very
  bug it was written to catch: a refusal protects the file perfectly while leaving the KB empty.
  So it also asserts that a mount which **can** import is never left holding nothing for a file
  that declares work. `kb-emptied` is distinct from `post-undo` on purpose — undo also clears
  `base_blob_hash`/`document`, and with no base the disk's items read as additions that a merge
  keeps, so undo alone never produces an item-less merged document. It asserts the *harm*, not the
  outcomes: those legitimately differ per mode, and pinning them would make it a change-detector.
- **Section namespaces are now derived**, kept for browsing and `ns:` scoping, authoritative for
  nothing. `retire_undeclared_sections` is **re-keyed on `sync_section`**: it was gated on
  `header_line`, which no longer decides anything, so leaving it would have made it a silent
  permanent no-op — invisible, because nothing renders from namespaces for a render test to catch.
- **Deleted, not guarded:** `adopt_legacy_namespace` and its vacuous ownership gate (the source of
  both pass-9 sync must-fixes), `set_layout`, `read_layout`, `legacy_layout`, `collect_legacy_prose`.
- **Still open, filed not fixed:** the import direction is unvalidated (`parse_text` is lenient, so
  a truncated file imports cleanly and cancels every task below the cut) — now the largest
  remaining data-loss path; and `ns mv`/`ns rm` are unguarded on synced namespaces, though D45
  took the teeth out of that one by moving a file's structure off the namespace tree.
  - **Two of the four have since landed.** A *first-sight export over an unimported file* is
    refused by `export_blocker`'s second condition — content on disk with no recorded structure —
    on any mount that can import. And the *losing bytes are blobbed*: `archive_current_bytes` moved
    up into `reconcile_file`, above the direction dispatch, so it covers all four sites that
    overwrite a synced file rather than the one that used to carry the rule, and a failed archive
    stops that file instead of degrading to best-effort.

## A synced file owns its own namespace (D39) — the collapse, fixed at the root

The `Collided` refusal above was a guard around a modelling error, and the error is now fixed:
**`namespace_for` includes the filename**, so one namespace holds exactly one file. Design in
`openspec/changes/jkb-sync-file-namespaces/`.

- **The root cause was one dropped path segment.** A file's namespace came from its containing
  *directory*, so every file there shared the `layout` that `render` treats as the sole
  authority on document order — one layout describing two documents, last writer wins, and the
  next export of the other file wrote its sibling's headers and prose over itself.
- **Seven guards over eight review passes** tried to keep answering *whose layout is this?* —
  `layout_uri`, `LayoutOwner`, `unclaimed_legacy`, `foreign_layout`, `refuse_foreign`,
  `colliding_paths`, `shares_namespace_with_other_bound_file`. Every one was a **proxy for
  authorship** (did it sync cleanly, is a sibling still bound, is there a journal row) and every
  one was satisfied by the recovery step the refusal itself recommended: deleting the sibling.
  On a legacy database the two files are indistinguishable claimants, so no proxy can work. All
  of it is **deleted** — a guard that cannot fire is a second model of the world, not defence in
  depth.
- **The filename keeps its extension.** `tasks` reads better, but `tasks.md` beside `tasks.txt`
  would collide again — the same defect, rarer, therefore worse.
- **Adoption is gone** (it was `adopt_legacy_namespace` + `Outcome::Adopted`). Its ownership gate
  was vacuous — it inspected only items placed *directly* in the directory namespace, and a
  sectioned file has none there — and both of pass 9's sync must-fixes were in it. D45 deletes it:
  with structure on the journal row there is nothing to adopt, and a legacy row is populated from
  the file's own base blob.
- **A directory may now hold many synced files.** That is the user-visible gain, and why this
  was worth a re-home rather than an eighth guard.

## An item id is never reused (D40), and a vector row goes with its item (D42)

`items.id` is `INTEGER PRIMARY KEY AUTOINCREMENT` (migration `V010`, **repaired by `V011`**).
Designs in `openspec/changes/jkb-item-id-stability/` and `openspec/changes/jkb-vector-liveness/`.

- **The hazard was rowid reuse.** `vec_items_<dim>` is a `vec0` virtual table and cannot carry a
  foreign key, so a deleted item left its vector behind — keyed on an id SQLite then handed to
  the next item created, which **inherited the dead embedding**, read as already-indexed to
  `index_pending`, and made ingest fail on a UNIQUE collision forever after.
- **It was fixed four times, once per call site** (`undo`, `item rm`, ingest's re-capture arm,
  ingest's fresh-capture arm) across review passes 5–8. Each fix was correct and incomplete,
  because the enforcement was procedural: every present and future deleter had to remember.
  Prefer an invariant the **schema** enforces over one every caller must uphold.
- **All four in-transaction sweeps are removed.** Removing them is the point — it deletes the
  question *which call sites sweep?*, which is what produced four passes of findings. Cleanup is
  housekeeping now: `jkb_index::count_stale` / `sweep_stale`, surfaced as **`jkb index --sweep`**
  and `jkb doctor [--fix]`.
- **But `V010` did not work, and D40's "a stale row is now inert" was false for two more passes.**
  Its `INSERT OR IGNORE` into `sqlite_sequence` **cannot ignore** — that table has no primary key
  and no unique index, so there is no conflict to ignore and it always inserted a second
  `('items', …)` row. And it seeded from `MAX(id) FROM items`, the maximum *surviving* id, which
  resets the high-water mark **below** every id freed at the top of the range. So `AUTOINCREMENT`
  protected ids freed after the migration and did nothing for the orphans D40 had just stopped
  sweeping. Reproduced against real SQLite with the migration's exact body.
- **`V011` recomputes the sequence from the changelog**, which records every item insert and is
  never pruned, so it remembers ids the table no longer holds — and it `DELETE`s the row before
  inserting, which also clears `V010`'s duplicate. `V010` is **not edited, not even its misleading
  comment**: refinery hashes a migration's entire SQL text, comments included, so a comment-only
  edit reports a divergent migration on every database that already applied it.
- **The invariant that actually holds is a `DELETE` trigger** (`vector.rs::ensure_gc_trigger`,
  created beside each `vec_items_<dim>` table). A trigger lives in the database file, so it fires
  for every connection, every process and every future call site — the objection that killed an
  `ItemDeleteHook` seam does not apply to it. Cost, stated: a connection without the `sqlite-vec`
  extension cannot resolve the virtual table, so an item delete on one fails loudly. Every binary
  here opens with `Db::open_with(&[jkb_index::register])`.
- **Reads filter too, as defence in depth with a named budget.** `VectorIndexer::knn_live` drops
  rows whose item is gone and **also returns whether the index was exhausted**, so `jkb-search`'s
  growth loop can tell that from "live rows ran out inside knn's budget" — its `hits.len() < fetch`
  test is wrong once filtering happens inside. The filter is applied in Rust, not as a join
  (`sqlite-vec` needs a `k` and rejects `ORDER BY` on anything but distance), and the internal
  fetch never exceeds **4096** — `sqlite-vec` hard-errors above that, and `vector_ranked` already
  over-fetches to 2048.
- **A liveness join alone could never have fixed it**: under reuse the `item_id` names a live
  item — the wrong one. That is also why `jkb doctor` reported `ok` for an affected database.
  D42.1 is the load-bearing fix; the trigger and the read filter are what make it hold.
- Pinned by `item::tests::a_deleted_items_id_is_never_reused` and, for the migration itself, a
  test inside `src/migrate.rs` (the runner is private) that builds a `V010`-era database with
  `Target::Version(10)` and asserts the next ids exceed every id ever used. **Note:** `V010`
  rebuilds `items`, so an older branch's binary cannot open a database this one has migrated —
  the usual shared-`jkb.db` divergence.

## The changelog is an audit log; `undo` reads it as an undo log (D47)

Three consecutive review passes each found a defect *inside the previous pass's fix* — round 4
`branch_records` missing from the undoable set (**which tables**), round 5 four upserts logging
the op `insert` (**which op**), round 6 derived-correctly `update` entries nothing could invert
(**invertibility**). Three axes of one question, because the mechanism underneath was untouched:
*cannot invert this? revert something else.* The diagnosis is that the two logs have different
contracts — an audit entry says what happened, an undo entry has to carry enough to put it back —
and nothing ever held an entry to the second.

- **The entity is a type, not a string.** `changelog::Entity` is a closed enum whose variants and
  `Entity::ALL` are generated together by one macro, and `Entity::insert_inverse` is an exhaustive
  match — so a new table cannot reach a writer without saying how an insert into it comes back.
  The allowlist is **derived** from that match rather than hand-maintained beside it.
- **The op is derived, never chosen.** `changelog::upsert(…, before, after)` records `insert` only
  when `before` is `None`; `changelog::append` **refuses** the op `insert` outright. Choosing it is
  how four upserts (`view::save`, `placement::place`, `binding::set`, `tag::apply`) logged `insert`
  for `ON CONFLICT` arms that updated pre-existing rows, after which `undo` deleted them.
- **A before-state that could not restore anything is refused at the write** (`undo::check_restorable`,
  called from `changelog::write`): non-empty, naming only real columns, and for a `delete` naming
  **every** column — checked against the live schema, so adding a column fails every deleter of
  that table at its next write.
- **Refuse rather than retarget.** `undo_last` selects the newest transaction containing any
  *work*, not the newest it can invert, and `undo` wraps the whole apply loop so **any** error
  becomes one named refusal that writes nothing. It stops predicting which entries are unrunnable;
  a kind nobody taught it about is a refusal, not a silently reverted stranger.
- **A restore that restored nothing is an error.** `restored()` is the one place a row count is
  judged, and zero is honest only where a named guard says the row was deliberately skipped. Arms
  answering `Ok(0)` for work they had not done were worse than raising: `undo` wrote its marker on
  the strength of it, so clearing the obstruction and retrying met "already undone".
- **User-visible: `V014` draws a date line.** A write-time guard cannot reach backwards, and
  inferring whether a legacy payload happens to be invertible is the same mistake one level along.
  So `undo_watermark` is seeded to `MAX(txn_id)` at upgrade: **`jkb undo` cannot reach anything
  from before the upgrade**. `undo_last` never selects below it, and an explicit `jkb undo <txn>`
  below it is told the transaction predates undo history rather than dying part-way through.
  A fresh database has an empty changelog, so the mark is 0 and nothing is excluded.

## Sync: prose is not an item (the `memory/sync-export-wins` fix)

The `tasks` serializer used to turn every non-item line into a `text` item whose identity was
a content hash plus an occurrence counter. That identity cannot survive an edit — two blank
lines are indistinguishable, and inserting a line above shifts every ordinal below — so old
prose items orphaned. An orphan stayed *placed* in its section namespace, `assemble_kb_doc`
emitted a `##` header for **every** namespace carrying `header_line` metadata regardless of
whether the file still declared it, and from then on the KB render permanently disagreed with
the disk: `kb_changed` was stuck true, so every disk-only edit was resolved as a both-changed
conflict and the stale header was written back over it.

Two changes close it:

- **Prose is never an item.** It carries no knowledge, nothing links to it, nothing queries
  it — giving it an identity (a content hash plus an occurrence counter) produced ids that
  broke on the next edit.
- **One authoritative `SyncDoc::layout`** replaces three drifting integer sequences. Document
  order used to be reconstructed by merging a section's `namespaces.metadata.position`, an
  item's `placements.position`, and a prose block's own ordinal — written at different times,
  and mixed across up to three *different parses* by a three-way merge. The numbers stopped
  describing one document and a `##` header rendered into the middle of an item (observed
  twice on a real file). Now `layout: Vec<SyncBlock>` (`Section(path)` | `Item(local_id)` |
  `Prose(text)`, prose inline) is stored whole on the file's namespace and is the **only**
  thing `render` consults for ordering; a merge takes it wholesale from the disk side, and
  anything it does not mention is appended rather than dropped. `SyncItem::position` survives
  only as the KB-side `placements.position` hint, never as document order.
- **An item's `section` is derived from the layout**, not from the namespace it is placed in,
  so the two cannot disagree. (Consequence: re-homing a *file-backed* item does not move it
  between sections in its file — edit the file for that. Before, the disagreement made the KB
  permanently differ from the base and turned every later disk edit into a conflict.)
- **`retire_undeclared_sections`** clears `header_line`/`position`/`sync_section`/`prose` from
  any namespace under the file the document no longer declares. The namespace and its contents
  survive (it may hold cancelled tasks, which are deliberate history); it just stops being a
  *section*. This covers the other way a section outlived its file — a cancelled **task**.

Related, same root: **`create_item` re-attaches**. A line deleted from a file is detached, not
deleted (D25), and keeps its file-derived uid — so re-adding it hit `UNIQUE constraint failed:
items.uid` and failed the whole sync. It now updates and re-binds the existing item, so
deleting a line and putting it back restores the same item with its edges and history.

Legacy `text` items migrate lazily: the new parser emits none, so each file's next import
cancels + detaches them exactly as designed, and `render` still emits a non-task item verbatim
so an un-migrated KB round-trips rather than losing lines.

## Namespace organization (D32) — a global, multi-repo layout

jkb is one global DB across every repo/project. Namespaces follow a fixed, automatically-
applied organization (design `openspec/changes/jkb-namespace-organization/`):

- **`repos/<repo>/…`** — a repo's file-synced content (its `openspec/`, `codereviews/`,
  source-derived docs). `<repo>` is the repo's namespace key; mirrors `tasks/<repo>` (D26).
  `ns:repos/jkb/**` is *everything about jkb*, and repos never collide. Mount a repo dir at
  `repos/<repo>/<subdir>`; ingest inside it inherits that root via ambient scoping.
- **Semantic top-level roots** for cross-cutting/global knowledge, tied to no repo:
  `media/` (ingested media/transcripts), `references/` (external docs/web), `memory/` (LLM
  long-term memory — modelled in a later pass), plus `tasks/` (D26) and `_sys/`. Reserved
  top-level roots: `repos tasks media references memory _sys`.

The two axes are unchanged: these are *logical* namespaces; a subtree may be a `file://`
mount or `managed:`. `jkb mount ls` lists mounts; `jkb ns rm` removes an empty namespace.
The 2026-07-24 migration moved the old top-level `openspec`/`codereviews` mounts under
`repos/jkb/…` and dropped a stray `<ns>`.

## Typed namespaces (D33) — the retrofit, COMPLETE and green

`jkb-memory` shipped the *mechanism* for typed namespaces (Dmem.1); this change retrofits
the hand-coded namespaces onto it and makes the type a **guarantee** rather than a hint
(design `openspec/changes/jkb-typed-namespaces/`).

- **A type now has a role.** `nstype::TypeRole::{Investigation, Contract}`. An
  *investigation* type is a coordination strategy (verbs, frontier, ranking, acceptance
  predicate — `debugging`, `conjecture-attack`). A *contract* type states only what may live
  in the namespace: `verbs`/`edge_types` default to empty and `goal_predicate` defaults to an
  error that **says** "this is a contract type" instead of faking a `DoneState`.
  `base_kinds()` is overridable — strategies get `INVESTIGATION_KINDS` (the four base roles
  **plus `task`**, because tasks legitimately live in an investigation namespace: that is
  what makes `is:frontier` a strict generalization of `is:ready`), contracts override to
  `&[]` and are exact.
- **The contract is enforced at the writer boundary.** `nstype::check_placement` runs inside
  `placement::place` — the single choke point through which an item enters a namespace — so
  it binds the task repo, the sync engine, the ingest pipeline and any future writer, not
  just the investigation engine. Untyped namespaces are unaffected. `ns::effective_type` is
  now **one** query over the ancestor chain (it is on that hot path), with `json_extract`
  guarded by `json_valid`. Edges are still **not** validated (Dmem.8 pitfall 1).
- **Three contracts, applied automatically.** `tasks` (accepts `task`), `views` (accepts
  `view`), `journal` (accepts **nothing** — the `_sys/*` markers surface a system table and
  hold no items; one contract covers `_sys/sync`, `_sys/transactions`, `_sys/ingestions`).
  `nstype::RESERVED_TYPES` maps each reserved root to its contract; migration **`V008`**
  back-fills existing DBs and `ns::ensure` stamps a reserved path as it creates it (seeded,
  not changelogged — matching how `V001` seeds `_sys`).
- **A type is NOT a location marker.** This was built and removed — record it so it is not
  re-added. Resolving the tasks root from the type system (`ns::typed_root`,
  `NamespaceType::locator`) fused a **contract** (what may live here — naturally
  many-to-many; `journal` types all three `_sys` markers) with a **location** (where a
  subsystem points — singular), and needed four propping mechanisms: a `locator()` trait
  marker, a uniqueness guard in `set_type`, `clear_type` as an escape hatch, and a
  re-seed guard. `tasks/`, `repos/`, `media/`, `references/`, `memory/`, `_sys/` are
  **reserved roots of the fixed D32 layout** — declared special cases, not instances of a
  general mechanism. `task::DEFAULT_ROOT` is a literal and `task.rs` is unchanged.
  `nstype::RESERVED_TYPES` is deliberately **one-way**: a reserved root is told its
  contract; nothing searches for a contract to find a root.
- **Surface.** `jkb ns type [path] [type] [--list] [--clear]` shows a namespace's own type
  *and* the inherited one with its source (a path that does not exist errors rather than
  reading as "untyped"), sets, clears, or lists types grouped by role. `ns::clear_type`
  survived the removal above on its own merit: the plain inverse of setting a type.
- **Deliberately deferred:** per-namespace-type command dispatch beyond `jkb inv do`
  (`jkb task` already is the task surface, keyed by item kind), edge validation, and
  status-lifecycle validation in the descriptor (guarded by `TaskStatus::from_manual_str`
  and the `V006` CHECK — a third copy is a third disagreement).
