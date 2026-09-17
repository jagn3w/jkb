# Subsystem reference — what each crate is and how it got that way

Completed subsystems, kept for reference. Nothing here is a live decision; it is the map
you read before changing one of these crates. The live rules are in `CLAUDE.md`.

Part of the jkb documentation set; see [CLAUDE.md](../CLAUDE.md) for the
conventions every session is expected to know.

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
`logic.rs` holds the tool work as **plain synchronous fns** over `Tools { backend }` — a
`jkb_api::Backend`, so every tool is an operation: a `LocalBackend` with the embedder on the host,
`jkb serve` in the dev container (design-s6-4.md K; `search` defaults to FTS where the backend
does not embed, and a file or URL is read where the server runs). The fns
(search/get_context/query/list_views/run_view/task_next; ingest/task_create/task_update) return
an `Answer` (JSON plus whether the read was cut, which the server tells the agent) — directly
unit-testable with no transport or runtime. `server.rs` is the thin async adapter:
`JkbServer { tools }`, `#[tool_router]`/`#[tool]` methods that `run()` each
logic fn on `tokio::task::spawn_blocking` (the writer-actor + ollama block, so they
must leave the async runtime) and wrap the JSON in a `CallToolResult`; `#[tool_handler]
impl ServerHandler` with `get_info` advertising tools. rmcp gotchas learned via
`./scripts/inspect-dep.sh rmcp-2.0.0 …`: `#[tool_handler]` calls the generated
`Self::tool_router()` (do **not** store a `tool_router` field — it'd be dead), args
derive `serde::Deserialize + schemars::JsonSchema` and arrive as `Parameters<T>`,
content is `rmcp::model::ContentBlock` (`::json`/`::text`), `ServerInfo`/`ServerCapabilities`
are `#[non_exhaustive]` (mutate a `default()`). Errors → `ErrorData` (user-input →
`invalid_params`). `lib.rs::run_stdio(tools)` builds a tokio runtime + `serve(stdio())`;
`jkb mcp` (in the CLI) calls it in both modes. All writes are ops → audited + undoable.

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
  claimed-but-`open` window). `release` clears the claim (leaves `status`). The reclaim
  (`transition::reclaim_judged`, driven by `task.reclaim` with the owners the client proved gone) NULLs
  **only** claims whose owner is proven gone, writing **only**
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
  `doctor --fix` and `task reclaim --keep <owner>` run the owner-existence reclaim — the owners
  probed by the command, freed by the `task.reclaim` op (`docs/message-queue.md`).
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
