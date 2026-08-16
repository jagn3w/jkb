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
- **486 tests** green across the workspace (+2 `#[ignore]`: live-ollama, live-URL — both need an
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
  `/review-log` cost ~16 agents, ~3M tokens and an hour per run. Run step 0 of `/review-log` — did
  every edit land, does each comment match its code, can each guard fire, who else implements this
  rule, does any test cover this mode, does every call site pass the new argument, did you
  actually run it — then `./scripts/check.sh`, and only then launch the workflow. **A doubt you can name is a test to
  write, not a line in the reviewer's focus argument** — the focus is for perspectives you lack,
  and a finding that merely confirms a doubt you already held is a review spent on work you owed
  it. **Anything short of high confidence is a blocker, not a disclosure**: what you are unsure of is exactly what to
  test, and the reviewer's budget must not be spent rediscovering a gap you could already name. `staging-workflow` needed 19 passes, and a large share of
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
- **A branch is a record, not a tag value (D46, re-founded by the B-series).** "Branch X was cut
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
- `scripts/merge-queue.sh` is unchanged and still the swarm's queue; `jkb task land` is the same
  algorithm in Rust for the human path (D36.1). The CLI is the home because the UI calls it
  directly and it must work in any repo.

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
  derived `state`: `implementing` / `review` / `landed`. A branch adding nothing to trunk is
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
- **Deliberately unchanged:** `scripts/merge-queue.sh`. The swarm already runs a fresh
  REVIEWER before a group reaches the queue (D27.6) — that *is* its gate, and stricter.
  Requiring `reviewed=` there would make the REVIEWER write facets to satisfy a check its own
  approval already answered. **Review staleness** is recorded (`reviewed=<sha>`) but not
  enforced: making every post-review fixup force a re-review is the fastest way to make people
  reach for `--no-review` by reflex.

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
  how many *files* carry findings, not how many findings there are, and because each batch faces
  all three angles the vote is a true 2-of-3. Skeptics **default to refuted when uncertain** and
  the burden of proof is on the finding: `refuted=false` requires writing the verified chain,
  since "I could not find a guard" is not "I confirmed there is none on any path".
- **Severity is assigned once, at the end.** Finders each see only their own findings, so their
  severities are not comparable. One ranking pass merges near-duplicates and puts everything on
  one scale: `must-fix`/`concern`/`nit` → `!p1`/`!p2`/`!p3`, and orders the whole set strictly,
  since the reader works down it and stops when time runs out. The test for `must-fix` is **would
  you hold the merge for this** — a previous run put 34 of 45 findings on `concern`, a severity
  every finding shares and which therefore tells the reader nothing.
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
  a clean review. Roughly, on a 1,000-line diff: **`low` ≈ 15 agents**, `high` adds three agents
  per file carrying findings. Above ~2,000 changed lines, several smaller ranges are both cheaper
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
  remaining data-loss path; `ns mv`/`ns rm` are unguarded on synced namespaces; a first-sight
  export can overwrite an unimported file; `finish_export` does not blob the losing bytes.

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

## An item id is never reused (D40)

`items.id` is `INTEGER PRIMARY KEY AUTOINCREMENT` (migration `V010`). Design in
`openspec/changes/jkb-item-id-stability/`.

- **The hazard was rowid reuse.** `vec_items_<dim>` is a `vec0` virtual table and cannot carry a
  foreign key, so a deleted item left its vector behind — keyed on an id SQLite then handed to
  the next item created, which **inherited the dead embedding**, read as already-indexed to
  `index_pending`, and made ingest fail on a UNIQUE collision forever after.
- **It was fixed four times, once per call site** (`undo`, `item rm`, ingest's re-capture arm,
  ingest's fresh-capture arm) across review passes 5–8. Each fix was correct and incomplete,
  because the enforcement was procedural: every present and future deleter had to remember.
  Prefer an invariant the **schema** enforces over one every caller must uphold.
- **All four in-transaction sweeps are removed.** A stale row is now inert, so cleanup is
  housekeeping: `jkb_index::count_stale` / `sweep_stale`, surfaced as **`jkb index --sweep`**
  and `jkb doctor [--fix]`. Removing them is the point — it deletes the question *which call
  sites sweep?*, which is what produced four passes of findings.
- Pinned by `item::tests::a_deleted_items_id_is_never_reused` (verified to fail without the
  migration). **Note:** `V010` rebuilds `items`, so an older branch's binary cannot open a
  database this one has migrated — the usual shared-`jkb.db` divergence.

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
