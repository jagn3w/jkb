# Working in jkb

jkb is a Rust Cargo workspace (crates under `crates/`) building a local-first,
agent-native knowledge base. The full plan lives in `openspec/` (local only, not
committed): `design.md` holds the decisions (D1–D25), `tasks.md` is the numbered
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
- **316 tests** green (131 core = 103 unit + 12 query + 16 investigation; 15 embed + 17 types
  + 8 index + 23 ingest + 7 search + 40 sync + 71 cli/e2e + 6 mcp; +2 `#[ignore]`: live-ollama,
  live-URL); `clippy -D warnings` clean
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
- **Changelog on every mutation:**
  `changelog::append(conn, meta, op, entity_type, entity_id, before, after)`
  where `entity_type` = the table name and `entity_id` = the row's rowid, so
  `undo` can `DELETE FROM {table} WHERE rowid = ?`. Use op `"insert"` for creates
  (the only op `undo` currently inverts).
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

Always `source ~/.cargo/env` first (rustup installs the pinned 1.96.1 toolchain).

```sh
cargo build
cargo test --all
./scripts/check.sh      # fmt --check + clippy -D warnings + test + cargo-deny
```

`cargo-deny` isn't installed yet (`cargo install cargo-deny`); the script skips it
gracefully. Update `tasks.md` checkboxes (`[x]` done, `[~]` partial + inline note,
`[ ]` todo) as each item lands.

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
distance, namespace_path, source_document }`. `Error` bridges `jkb_core` + `jkb_index`
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
`doctor [--backup]`, `mcp` (stub → "Section 13" error). Embedder is the ollama default,
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
  (`owner.rs`, `kill -0` liveness probe). `doctor` reports orphaned claims (owner gone);
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
