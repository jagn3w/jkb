<!-- generated from jkb design design:foundation-18dcdeed0ab4d7709fd734, edit there (version 96.X4LfyqDozfwPAYLqkPH_p0UBhO2vnI20zgcBgvvZ1Mnb6Q0Bhu3u9vOuwQkBiefImrSgjgUBidepvMKL7AEBi-7vkr7UzwIBi4v13NKE4A0BjYWn0daWdwGQh6fnuO31DwGR4vys_rxrAZL-t5Wzl7ICAZSF453X3fYEAZXKovzQiI0NAZaB3JHjhKcNAZXZobTRvY8BAZi63c-qh70EAZmJ45HLrq0FAZXg2b340Z4KAZSTl_SJ7oQFAZ2Mx8bk4tkPAZ6VgO30xcYDAZ_PkrepqIABAZ-1w8KT-x0BosCV7pXhvg4BpfzbiJXP4AYBqY3Z4duPywYBqprvqf_k7gEBq7u0h9q0nwUBrcmgvui4_wgBreSM8OOM2gkBree0z5aL5Q4BsdCT8faJigQBtZjXif6wiwrfigS3l83i_4DFDwG46drXpriLDAG5hZyS-ojYDwG5wtCE2tavCgG72ZXjnJ-SCwG3kt3Iv43oAwG9x6qT5dr8CAG-9pjn9d3dBQG7npCdiY76CQHAwa2RrfHpBwHCkYvEusHvBAHEtOjXn4XQDgHGlI6whPnXCgHH3YKwwavqCAHIma7i1oOPBQHJ2JD6kb3dBQHKnpvM_br_DwHHlqzaquDIAQHMifX37fX2BgHNh9aKpqLsDQHJkYme__L_BAHPj-2Nzu2ZAwHP0onzzJWFCwHQ3-Lz5JSaCAHS5JfUxey_CgHQgOOmwqabCAHP8rD1vuNbAdWdgLio6tkIAdbDh5TWtcsPAdG3oL6mwcgIAdmW_t3a-ZELAdrm6Y-sraINAdvzrNjN48QKAdvs6rbhh7oPAduVj4GcpKMOAd6KqrX5jeMJAd62u72N-u4IAeDB0Pji9bcEAeHIyrDgov4LAeGGtb_I-v4OAeOTuJeas8gEAeT-nOrLy70IAeX8mPaHz6QEAebo062N5XoB5LHZ1qPBvQIB6OOByP2wgAMB6Jnf1O2t0QgB6KTkp5qUtgsB6O7o0pLooAUB4K_Th6zmqgcB6JOdntPMtwcB7pjK-piqhgcB8JSB9sq66g0B8ZCj_YuxuAQB8q_6gYm9lQQB8Prj56_kzgoB9vaf2vzcpA8B-dnGtKGi-A8B_PG7hYj2wgYB_6Px-ICjmg0B, blake3 9dd7c15a518fb9adc5bf1dbcc3b77edeab0c8d7c50d4d05276cdc75d0905da88) -->
# Foundation

jkb is a local-first, agent-native knowledge base. It replaced two rigid tools: the arch-helper
KB (chromadb plus ollama, two hard-coded collections, ingestion records as loose files on disk)
and a monorepo's linked READMEs plus OpenSpec planning, which forced re-reading large Markdown
files to find the next thing to do. jkb kept what worked in both (staged, re-runnable ingestion;
stable content-hash ids; agent-facing tools; linked, structured documents) and moved all of it
onto one substrate with real database internals.

This design records that substrate and the rules that keep it honest: the `SQLite` virtual
filesystem, the crate layout, the write path, ingestion and the derived indexes, what the store
guarantees about ids, vectors and the changelog, the ways of working, and the CI gate. It ends
with a reference section describing each finished subsystem, which is read last, never first.

The other designs build on this one. How namespaces are laid out and typed is the namespaces
design; how files round-trip to the store is the file-sync design; the task state machine,
branches and landing are the task-lifecycle design; agents, roles and review are the
agents-and-roles design; the dev container is the sandbox-and-container design; `jkb serve` and
the queue are the daemon-and-messaging design; the desktop app and designs themselves are the
code-factory design; the git hooks installer is the git-hooks-installer design.

## The substrate

The first design pass set the goals: one substrate for many data types, organized by logical
namespaces, with multi-placement, namespaced tags with properties, and a typed graph; in-engine
idempotency and ACID transactions; single-file backup; scoped multi-route search; a task DAG on
the same item and edge substrate; and a clear, idiomatic Rust codebase, since this was the
author's first Rust project. Each decision states the choice, the rationale and what was
rejected, so a later change can see why the shape is what it is.

### The store is a virtual filesystem; indexes are derived and pluggable

The durable source of truth is a virtual filesystem: logical namespaces are paths and items are
nodes. Vector search and full-text search are not part of the source of truth. They are indexes
derived from the filesystem, behind `trait Indexer`, and the engine dispatches each item to the
indexers that accept its kind or mime.

Every index is therefore fully rebuildable from the filesystem, and must hold nothing that
exists only in the index. That makes migrations safe (drop and rebuild an index table), gives
corruption recovery, and lets an index engine be swapped or added later (an embedded graph
engine, a filtered-ANN engine) as just another indexer without touching the source of truth.

Rejected: the vector database as the primary store, arch-helper's model. It couples
organization to one index, has no keyword or graph story, and makes the embedding model a
load-bearing part of identity.

### `SQLite` is the source-of-truth engine

`SQLite` (via `rusqlite`, `bundled`) holds the virtual filesystem and the relational and tag
store. For: ACID, a single file (trivial backup), zero operations, clean Rust embedding, the FTS5
and `sqlite-vec` extensions, recursive CTEs for moderate graph walks, ubiquitous tooling and
longevity. Against: it is not a native graph database (deep or variable-length traversals are
awkward, there is no Cypher), and vector search is a brute-force bolt-on.

Rejected for the source-of-truth layer: Kùzu (embedded graph plus vector with Cypher, but
younger, with thinner Rust bindings; kept as a candidate for an added graph indexer), SurrealDB
(heavier and younger, its own query language), DuckDB (an OLAP shape, wrong for graph and OLTP),
redb, RocksDB and sled (graph, indexes and FTS all hand-rolled), and Postgres with pgvector and
AGE (a server, which ends the single-file, zero-ops backup story). The principle: pick the
bulletproof engine for the layer that is hard to change, and keep the fast-moving parts, the
indexes, swappable.

### Two orthogonal addressing axes: logical namespace and binding

A namespace path is always a logical jkb address, never a filesystem path by itself. The first
axis is logical organization: virtual folders such as `tasks/`, `repos/` and `_sys/`, with
`namespaces.kind` one of `logical`, `mount` or `system`, and an item multi-placed across many
namespaces. The second axis is the backing of an item's bytes, its binding: `managed:` (jkb
owns the bytes, for example an ingested PDF or a task not checked in) or `file:///abs/path` (a
synced README or checked-in task, with sync mode `import`, `export` or `bidirectional`). Remote
bindings (`https://`, `git://`) are deferred. A mount ties a subtree of the first axis to a
backing root and a policy, for example `docs/monorepo → file:///Users/jagnew/repos/monorepo,
bidirectional, include:**/*.md`.

Three distinct fields, and the namespace path is never overloaded: the logical address
(`jkb:/tasks/monorepo/backend/fix-x`), the backing address (`managed:` or `file:///…`) and the
item identity (`jkb:item/<uid>`, stable across moves and placements).

A `managed:` task associates with a repo by being placed under the repo's namespace, not by an
edge or a tag, so the association is a subtree query. Placement answers "where does it belong
and show up"; binding answers "where do its bytes live, and does it touch my disk". That is why
a `managed:` task can appear in a repo's namespace without ever being written into the repo.
Rejected: an edge to a location node (association becomes a traversal) and location as a tag (a
flat string with no hierarchy). How the layout uses the two axes is the namespaces design.

### Item identity is a rowid key plus a stable uid, and content dedups globally

`items.id` is an `INTEGER PRIMARY KEY`, a rowid alias. FTS5 external content needs that
(`content_rowid`), and `vec0` is rowid-oriented. A stable string identity lives in
`uid TEXT UNIQUE` (`book:sicp`, `b3:<hash>:<idx>`, `task:reread-sicp-ch1`). `content_hash`
(blake3) is globally `UNIQUE`: identical content anywhere is one item with several placements,
so cross-source dedup falls out of the schema. An item keeps its `id` and `uid` when it is
re-placed.

Rejected: UUID or text primary keys (they break FTS5 external content and complicate `vec0`),
and content-hash uniqueness per namespace (identical chunks from two sources would be
duplicated instead of multi-placed). The id is also never reused; that guarantee and what it
cost are under "What the store guarantees".

### Lifecycle state is a column; open-ended facets are tags

`status` is a real, nullable column on `items`, read on every DAG computation, so it must be
indexable and cheap, not a JSON field or a join. Open-ended, sparse facets (`read_year`, `topic`,
`size`) are `tag_applications`, each with optional per-application `props`. Ready tasks are an
anti-join: a non-terminal task with no `depends_on` edge to a non-terminal task. `depends_on`
must stay acyclic, and since `SQLite` will not enforce that, it is checked at edge insert with a
reachability query.

Rejected: status as a tag only (the ready query becomes join-heavy and "is this done?" a
lookup), and status in JSON metadata (not indexable).

### If you query it, it's a column or a tag; JSON is for the rest

`metadata` and `props` JSON hold rarely queried display and provenance attributes. Anything
filtered or sorted on (status, priority, due date, `read_year`, `size`) is a real column or an
indexed tag application. A JSON field that becomes hot graduates to a `STORED` generated column
plus an index, which is non-breaking.

### Task status is enforced by the database, not only by Rust

`items.status` accepts only NULL or one of the five `TaskStatus` strings, by a `CHECK`
constraint (migration `V006`): `CHECK (status IS NULL OR status IN ('open','in_progress',
'needs_review','done','cancelled'))`. Before it, the column was free-form `TEXT` and the type was
enforced only by `TaskStatus::from_manual_str`, so the durable source of truth could still hold
garbage. The Rust boundary stays the first line of defence; the `CHECK` is the storage
backstop. NULL stays permitted, so kinds without a status are unaffected, and the derived
`blocked` state is not storable.

Rejected: a `BEFORE INSERT/UPDATE` trigger (less discoverable, two objects to keep in sync) and
enforcement in application code only (the column stays free-form). Pinned by a migration test: a
NULL-status non-task item inserts, a valid status inserts, and an invalid status is rejected by
the database itself.

### The namespace tree is adjacency plus a recursive CTE

`namespaces` stores `parent_id` and a display `path`, and subtree reads use `WITH RECURSIVE`,
which is fast at the expected scale of thousands to hundreds of thousands of namespaces. A
closure table (every ancestor-descendant pair with depth) is deferred: it is rebuildable from
adjacency, so adding it later is non-breaking. Rejected: a materialized path as the truth (a
rename rewrites every descendant's string, and escaping `/` in names is fiddly), and a closure
table built before profiling shows subtree reads are hot.

Paths are normalized and validated before storage: empty segments, `.` and `..` traversal and
control characters are rejected, and Unicode is NFC-normalized. A path such as
`books/../_sys/x` fails validation.

### Write-heavy internals stay in their own tables

The user-facing model is "everything is a namespaced item", but write-heavy internals are not
items. `changelog` is append-only, high-volume and never embedded or linked, so it is its own
table, surfaced as `_sys/transactions`; storing it in `items` would bloat every `items` index and
fight `content_hash UNIQUE`. `blobs` is already content-addressed and is referenced by
`ingestions`. `ingestions` is its own queryable state. This keeps the thesis without paying the
generality tax on internals.

### The tables

The v1 schema, with full DDL in `jkb-core`'s migrations:

- `namespaces(id, path UNIQUE, parent_id, kind CHECK in (logical,mount,system), metadata, created_at)`
- `mounts(namespace_id PK, backing_uri, sync_mode, serializer, include_glob, exclude_glob, conflict_policy)`; `serializer` defaults to `document`
- `items(id INTEGER PK, uid UNIQUE, kind, content, content_hash UNIQUE, mime, status, priority, due, metadata, created_at, updated_at)` with `INDEX(kind, status)` and `INDEX(due)`
- `bindings(item_id PK, uri, sync_mode, serializer, last_synced_hash, last_synced_at)`; default `managed:`, a NULL `serializer` inherits the mount's
- `placements(item_id, namespace_id, role, position, metadata)`, PK `(item_id, namespace_id, role)`, `INDEX(namespace_id, role, position)`
- `edges(id, src_item_id, dst_item_id, type, props, created_at)` with `INDEX(src, type)` and `INDEX(dst, type)`; v1 types `depends_on`, `derived_from`, `references`, `parent_of`
- `tag_defs(id, facet UNIQUE, value_kind)` and `tag_applications(item_id, facet, value, props)` with `INDEX(facet, value, item_id)` and `INDEX(item_id)`
- `blobs(hash PK, bytes, mime, size, created_at)`
- `ingestions(id, source_hash, pipeline_version, strategy, embedder_model, stage, status, blob_hash, started_at, completed_at)`, `UNIQUE(source_hash, pipeline_version, strategy, embedder_model)`
- `embeddings_meta(model, dim, table_name, populated_at, model_version)`, a catalog
- `vec_items_<dim>` (`vec0`), created at run time, and `fts_items` (FTS5 external content on `items.content`)
- `changelog(txn_id, ts, op, entity_type, entity_id, before, after, actor)`

Worked example: ingesting SICP and filing a task. The book is item 100 (`book:sicp`, a
document) placed `primary` under `books/sicp`; chunks 101 and 102 (`b3:cc:0`, `b3:dd:1`) are
placed with role `chunk` at positions 0 and 1 and carry `derived_from` edges to 100. Task 200
(`task:reread-sicp-ch1`, status `open`) is placed `primary` under `tasks/monorepo/backend` and
`reference` under `repos/monorepo/backend`, with a `references` edge to the book. Both are
`managed:`, so neither touches disk. The book carries `read_year=2025` and
`topic=programming {"confidence":0.9}`. The raw PDF is one blob; one `ingestions` row records
`page_based`, `nomic-embed-text`, stage `embed`, complete; `vec_items_768` holds rows for 101
and 102; `fts_items` holds 101, 102 and 200. "Books I read in 2025" is then items under
`books/**` tagged `read_year=2025`, ranked by vector or FTS within that scope.

Later migrations, each owned by the design that needed it, run from `V003` (the catalog's
`model_version`) to `V026` (design exports). Two belong here: `V010` and `V011`, under "What the
store guarantees".

### Migrations are refinery's, forward-only; virtual tables are additive

Schema evolution uses `refinery` embedded migrations (ordered, checksummed, transactional) under
`crates/jkb-core/src/migrations/`, named `V00N__<name>.sql`. `PRAGMA user_version` is set as a
human-readable marker for `jkb doctor`. Migrations are forward-only. Virtual tables (`vec0`,
FTS5) are never altered in place: a new model or dimension is a new `vec_items_<dim>` table plus
backfill, and a changed FTS configuration is a drop and rebuild, which is safe because indexes
are derived. The `vec_items_<dim>` table is not a migration at all: `VectorIndexer::ensure_ready`
creates it, because it is per dimension and needs the extension registered first, which a
compile-time migration cannot guarantee.

`rusqlite` is pinned once in `[workspace.dependencies]` (0.39, `bundled`) and every crate uses
`rusqlite = { workspace = true }`. 0.39 matches refinery 0.9.2, so they share one
`libsqlite3-sys`; a per-crate version reintroduces the `links = "sqlite3"` conflict.

### The migration harness owns `PRAGMA foreign_keys`

`SQLite` cannot `ALTER TABLE … ADD CONSTRAINT`, so adding a constraint is a table rebuild:
create the new table, copy with an explicit column list, `DROP` the old one, rename, and
recreate every dependent object the drop took with it (indexes and the three FTS-sync triggers;
`fts_items` itself survives and stays aligned because rowids are copied). With foreign keys on,
`DROP TABLE items` is an implicit cascading delete of its five `ON DELETE CASCADE` children.
`PRAGMA foreign_keys` is a no-op inside a transaction, and refinery wraps each migration in one,
so a `PRAGMA foreign_keys=OFF` inside the migration silently does nothing.

So `migrate::run` runs migrations with `foreign_keys` off, runs `foreign_key_check` afterwards,
and restores `foreign_keys=ON`, which is how `SQLite` recommends running schema migrations. It
is a small general change that makes every later table rebuild safe (`V010` reused it), and the
check catches a rebuild that broke referential integrity.

### The store is single-machine local

A single `SQLite` file is ideal for backup but unsafe to open concurrently from two machines
over a file-sync service such as Dropbox or iCloud; that risks corruption. jkb is
single-machine local. Continuity across machines comes only from checked-in `file://` tasks and
documents travelling through git; real multi-machine sync (replication, litestream) is a
deliberate later design. `jkb doctor` warns when the database path is inside a known cloud-sync
folder (`cloud_sync_warning`).

## The write path

`jkb-core` is synchronous. Async lives only at the edges: ollama HTTP, file watching, MCP and
the daemon.

### WAL plus one writer-actor

`SQLite` is single-writer: WAL lets readers proceed during a write, but concurrent writers hit
`SQLITE_BUSY`. All writes go through one owned `Connection` on a dedicated writer thread fed by
an mpsc queue, so the application never sees `BUSY`. It is also the idiomatic Rust shape: one
owner for the writer. Pragmas: `journal_mode=WAL`, `foreign_keys=ON`, `synchronous=NORMAL`, a
`busy_timeout`. `Db` (in `store.rs`) is the only public handle; it clones cheaply, and today
reads are routed through the same thread as writes, which is fine at single-user scale.

Mutations take a `&WriteMeta`, which exists only inside `db.write_txn("actor", |conn, meta| …)`,
one atomic transaction with a fresh `txn_id`. That funnels every write through a transaction by
type. Repos are plain functions over `&Connection` composed inside it. The generic
`write_txn_with` and `read_with` let the closure's error type be any `E: From<jkb_core::Error>`,
so `jkb-ingest` and `jkb-search` can `?` across `jkb_core` and `jkb_index` errors in one
transaction.

### SQL is prepared once and parameterized always

Repos use `conn.prepare_cached(sql)`, so a statement compiles once and is reused across the
long-lived writer connection, and get a new-or-existing row id in one statement with
`INSERT … ON CONFLICT(…) DO UPDATE SET <no-op> RETURNING id`, dropping the extra `SELECT` per
mutation. Values are always bound as parameters, never interpolated.

### A backup checkpoints the WAL first

WAL keeps a `.db-wal` sidecar, so a naive copy of the single file can miss committed state.
`Db::backup` (and `jkb doctor --backup <path>`) runs `wal_checkpoint(TRUNCATE)` and then copies,
so the file reflects every committed write before the single-file backup story is relied on.

## Ingestion, embeddings and the indexes

### Ingestion is staged and idempotent in the engine

Ingestion runs in independently re-runnable stages, as arch-helper did: parse, chunk, embed,
with a fetch stage first for URLs. Idempotency is enforced by the database, not by files:
content-addressed `blobs` (blake3) dedup raw sources, so the same file from two paths is one
blob; an `ingestions` row keyed `UNIQUE (source_hash, pipeline_version, strategy,
embedder_model)` makes a completed re-run a no-op and an interrupted one resumable per stage; and
items upsert by `content_hash`. A different strategy is a distinct ingestion. Chunk uids are
stable (`b3:<source_hash>:<idx>`), so re-embedding after an embedder change updates items in
place.

`Pipeline` (in `jkb-ingest`) captures in one transaction (parse, chunk, the document and chunk
items, placements, `derived_from` edges and the blob) and embeds in a separate one. A failure
during capture rolls back with no partial items. Rejected: arch-helper's filesystem offsets and
JSON records; in the database, idempotency and provenance are transactional and queryable.

### Capture never blocks on the embedder

Capture must never fail or stall because the embedder is down. An item is written and
FTS-indexed at once by triggers, so it is keyword-searchable immediately. Embedding is its own
resumable stage that can run inline, in a background pass, or at the next `jkb index` or
`jkb doctor`; until then the item is simply not vector-searchable. `Pipeline::index_pending`
embeds what is missing and `Pipeline::unembedded_count` reports it. Embeddings are computed off
the writer thread, so no database lock is held during network I/O.

### URL ingestion is one-way and rendered by a headless browser

Two things were separated. URL ingestion fetches a page once, extracts its text, and stores it as
a `managed:` document with chunks; it is in. A remote binding, an item whose source of truth
stays remote and re-syncs, is deferred. `Pipeline::ingest_url` drives a headless Chrome
(`headless_chrome`, `fetch.rs`) so client-side JavaScript runs, then feeds the rendered DOM
through `HtmlAdapter` and the normal capture and embed path. A fetch failure is actionable
(`Error::Fetch` names a missing Chrome) and writes no item, blob or completed ingestion. It needs
a Chrome or Chromium binary, which is why its live test is `#[ignore]`.

### Source adapters, and a warning for near-empty sources

Adapters sit behind `SourceAdapter`: text, Markdown (`pulldown-cmark`, plain text plus the first
H1 as title), PDF (`pdf-extract`) and HTML (`scraper`, `<title>` plus visible text with scripts,
styles and head dropped), dispatched by extension with text as the fallback. Untrusted PDF and
HTML are parsed by pure-Rust crates, never by shelling out. When a source yields fewer than
`MIN_USABLE_CHARS` of usable text (a scanned PDF with no text layer), ingestion completes with a
warning rather than silently succeeding; OCR is deferred.

### Embeddings sit behind a trait; ollama is the default

`trait Embedder` (in `jkb-types`) has `embed`, `model`, `dim`, `health_check` and
`resolved_version`, and ingestion and search depend on the trait. The default implementation
calls ollama at `localhost:11434` (`nomic-embed-text`, dimension 768) over
`reqwest::blocking`: no native build, and likely already running. The in-process `fastembed`
(ONNX via `ort`) implementation is behind the `fastembed` feature: powerful, but a heavier native
build plus a first-run model download, the most likely first-run failure. Selection is
configuration (`EmbedderConfig` and `build()`). Input longer than the model's limit is truncated
deterministically at a character boundary. An unavailable embedder is a clear
`EmbedderUnavailable` naming the model and how to start it, never a silent fallback to another
model.

### The catalog refuses a model change, not only a dimension change

`embeddings_meta` records which vector tables exist, with model, dimension and
`model_version`. `VectorIndexer::ensure_ready` records them on first use and checks them after
with `ensure_compatible`, which refuses on a dimension or a model mismatch. Dimension alone is
not enough: 768 is nearly universal, and a same-dimension model swap silently poisons the vector
space. `check_version_drift` is a `doctor` diagnostic that compares the embedder's resolved
version (ollama's content digest, fastembed's stable id) with the catalog, to catch a `:latest`
tag that was re-pointed. These guards are pure and live in `jkb-types`, re-exported by
`jkb-embed`, so `jkb-index` uses them without pulling in `reqwest`.

### One `vec0` table per dimension, and all `sqlite-vec` code in one module

Each embedding dimension gets a `vec_items_<dim>` `vec0` table keyed by `item_id INTEGER
PRIMARY KEY` (the `vec0` rowid), so KNN returns `item_id` and distance with no join, and upsert
and delete by id are direct. Vectors are bound as little-endian `f32` blobs. One model and
dimension are supported at a time; the per-dimension tables only permit more later.

`sqlite-vec` is the highest-churn dependency, so all of its SQL lives in `jkb-index`'s
`vector.rs`, together with the only `unsafe` in the workspace: the FFI registration in
`register()`, `Once`-guarded, under one commented `#[allow(unsafe_code)]`. A breaking change in
the extension is then a local edit. The extension is pinned and vendored, never loaded from a
user path.

### Extension setup is a seam that core owns

Core owns connection and extension setup. `jkb_core::ExtensionRegistrar` is a plain `fn()`, and
a database is opened with `Db::open_with(path, &[jkb_index::register])` (or
`open_in_memory_with`), so core sequences registration before opening the connection. That
removes the "call `register()` before open" footgun. `jkb-index` provides `register()` and does
not depend on `jkb-core`, mirroring the `Embedder` seam: trait in `jkb-types`, implementations
in `jkb-embed`.

### Scoped vector search preserves recall

`sqlite-vec` resolves KNN across the whole table before any join, so it does not AND with
namespace or tag predicates, and a selective scope loses recall. This was named as the largest
practical risk. `jkb-search`'s `vector_ranked` handles it in three tiers: no scope is a plain
KNN; a scope of at most `EXACT_SCORING_CAP` (256) items is scored exactly with
`VectorIndexer::distances_for` (`vec_distance_cosine`), skipping the approximate search; a larger
scope over-fetches `k×8`, filters, and grows ×2 up to `OVERFETCH_CAP` (2048). The scope is never
dropped: every route filters to the structural candidate set. A partition seam is noted on
`distances_for`: `vec0` partition columns on a coarse key (top-level namespace, year) would
restore recall for scopes such as `books/**`, and if scoping ever dominates, a filtered-ANN
indexer can replace it. The flat index scans linearly, which caps out around 10^5 to 10^6 rows
per partition.

### FTS5 external content, kept in sync by a trigger triad

`fts_items` is FTS5 external content over `items(content)` with `content_rowid=id`, which is why
item ids are rowids. `AFTER INSERT/UPDATE/DELETE` triggers keep the shadow tables in sync, with
the update and delete triggers using `old.content` to un-index. `FtsIndexer`'s per-item index and
remove are therefore no-ops; it provides bm25 search, `integrity_check` and rebuild through FTS5's
`'rebuild'` command. Tests assert `INSERT INTO fts_items(fts_items) VALUES('integrity-check')`
passes after inserts, updates and deletes.

### Search routes are chosen by the caller

Search has three routes, `vector`, `fts` and `hybrid`, and the caller chooses; there is no
hidden LLM call. The query text is embedded on the caller's thread, the only model call, never
inside `db.read`, which runs on the writer thread. Hybrid is reciprocal-rank fusion (K=60), with
no tuning. Each hit carries its identity, route, score (higher is better), distance, namespace
path (preferring a `primary` placement) and, for a chunk, its source document via
`derived_from`. Context expansion, `get_context(item, n)`, returns up to `n` chunks either side
of a hit from the same source, ordered by `position`, with no re-embedding; a first chunk returns
only what follows, and a non-chunk returns itself. A notes and annotations flow is deferred; the
substrate already permits `note` items with `references` edges.

## The first surfaces over the substrate

These are the v1 decisions about the surfaces. Each has since grown its own design, named in the
block; what stays here is the shape the substrate gave them.

### A typed query AST compiles to one parameterized query

`Query` is a typed AST (kind, status, priority, due, tag predicates, namespace scope as a path,
subtree or union, `ready`, `blocks`, FTS match, vector term, limit) usable without any text
syntax. `Query::evaluate` compiles it to one parameterized SQL query over items, placements,
tags, edges and `fts_items`, and returns the candidate id set; structured predicates narrow the
set before a ranking route runs. The `~"…"` vector term is carried on the AST and ranked by
`jkb-search`, not core. `query::parse` is a quote-aware DSL (`kind:`, `status:`, `priority<op>n`,
`due<op>date`, `due:today`, `tag:<facet><op>v`, `ns:…/**` with comma unions, `is:ready`,
`blocks:`, `~"…"`, bare or quoted FTS terms) whose errors name the offending token. Known limit:
tag values compare as `TEXT`, so an ordinal facet such as `size<=small` does not order
semantically.

Saved views are `kind='view'` items (uid `view:<name>`) under `_sys/views`, validated by parsing
before they are saved. Ambient scope: inside a directory covered by a mount, an unscoped query,
`task next` or search defaults to that mount's namespace (`mount::ambient_namespace`), unless
`--global` is given.

### A literal quote is escaped with a backslash in the strict line DSLs

`dsl::has_unterminated_quote` used to report any odd count of `"` as unterminated, so the strict
DSLs (`parse_quick_add`, `query::parse`) refused a title or term with one literal quote:
`jkb task add 'ship the 6" pipe'`. Now `\"` is a literal quote that neither toggles quote state
nor counts toward balance, and `\\` is a literal backslash. `dsl::tokenize` honours the escape,
`unquote` unescapes, and an odd count of unescaped quotes is still an unterminated-quote error.
Input without a backslash behaves exactly as before.

Rejected: treating every stray quote as literal, which would silently mis-parse a real typo such
as `"a b`. The `tasks` serializer keeps its own lenient, escape-free tokenization, so its settled
round-trip behaviour is untouched.

### Tasks are items, and `blocked` is derived

A task is an item of kind `task` with `priority` and `due` as real columns (sorted and filtered
on, so columns, not tags). Manual statuses are set; `blocked` is derived from `depends_on` edges
and is never stored, so there is one source of truth. A terminal dependency, `done` or
`cancelled`, unblocks: a cancelled dependency will never complete. `jkb task next` orders the
ready frontier by priority and then due date. Quick-add puts capture on one line:
`jkb task add "text" !p1 @2026-07-15 +<ns> #size=small ^<uid>`, with the binding defaulting to
`managed:`. A task may be `managed:` or bound to a checked-in file; changing the binding does not
change its identity or placements. Homing is the namespaces design; the state machine, claims
and landing are the task-lifecycle design.

### A synced file is a serialization of items

A synced file is not intrinsically one item. The mapping is a pluggable `SyncSerializer` with a
stable identity mapping, chosen per mount (`mounts.serializer`) and overridable per file
(`bindings.serializer`, NULL inherits). v1 shipped `document` (whole file to one item); the
`tasks` serializer, the sync journal and three-way merge followed, and the `spec` serializer is
still deferred. A serializer only concerns the file boundary, so swapping or adding one never
risks the stored knowledge. Everything else about sync is the file-sync design.

### The MCP server is a first-class interface

The author's daily interface is Claude Code, so the highest-leverage query surface is an MCP
server (`jkb-mcp`, on `rmcp`), not the CLI. It delivers "a local agent that answers with
pointers to documents" without embedding an LLM in core: the LLM is Claude, on the other side of
MCP. It exposes read tools (`search`, `get_context`, `query`, `list_views`, `run_view`,
`task_next`) and write tools (`ingest_path`, `ingest_url`, `task_create`, `task_update`). Every
write goes through the same audited write path as the CLI, so `jkb undo` reverts an agent's
change like any other. Inputs are validated (namespace normalization, existence checks) and fail
with an actionable error rather than leaving malformed state. A local-LLM answer route stays
deferred.

### Reversibility and reorganization are part of the daily driver

`jkb undo` reverts the last transaction (or a named one) from the changelog in a new,
itself-audited transaction, whether the change came from the CLI or MCP. `jkb ns mv` and
`jkb tag rename` are transactional and audited, and need no index rewrite because indexes are
derived. `jkb ns ls [scope]` and `jkb tag ls` show the vocabulary so near-duplicate namespaces
and facets are not created by accident. What `undo` may and may not reverse is under "What the
store guarantees".

## What the store guarantees

Three guarantees cost more review passes than anything else in the substrate, and all three
taught the same rule: prefer an invariant the schema or a type enforces over one every caller
must uphold. A guard that depends on remembering fails at the site nobody thought of, and it
fails silently.

### An item id is never reused

`items.id` is `INTEGER PRIMARY KEY AUTOINCREMENT` (migration `V010`, repaired by `V011`).
`SQLite` otherwise hands the largest freed rowid to the next insert, and derived indexes key on
that id. `vec_items_<dim>` is a virtual table, so it cannot carry a foreign key or
`ON DELETE CASCADE`: a deleted item left its vector behind, and the next item created
inherited the dead embedding. Vector search returned the new item for the deleted item's text;
`index_pending` read it as already indexed, so `jkb index` never corrected it; `jkb doctor` saw
nothing wrong, because every row belonged to a live item; and ingest failed on
`UNIQUE constraint failed`, permanently, for every later ingest into that database.

`AUTOINCREMENT` costs a `sqlite_sequence` row and slightly more work per insert, the correct
trade against a defect that corrupted a database in practice. `SQLite` cannot add it to an
existing table, so `V010` rebuilds `items` under the harness's foreign-key toggle, copying
rowids explicitly so the FTS index stays aligned and is not rebuilt.

A liveness join on reads could never have fixed this alone: under reuse the `item_id` names a
live item, the wrong one. Pinned by `item::tests::a_deleted_items_id_is_never_reused` and by
`jkb-index`'s integration test, which asserts a successor gets a fresh id (it previously
asserted the id was reused, "the whole hazard"). A consequence: `V010` rebuilds `items`, so an
older branch's binary cannot open a database this one has migrated, the usual shared-`jkb.db`
divergence.

### The high-water mark is seeded from the changelog

The sequence must exceed every id the database has ever used, not every id it still has.
`V011` recomputes it from `MAX(CAST(entity_id AS INTEGER))` over the changelog's item entries,
which remember ids the table no longer holds, since the changelog records every item insert,
`undo` appends rather than deletes, and nothing prunes it. It also takes `MAX(items.id)`, which
is redundant (`SQLite` already uses `max(seq, MAX(rowid))`, so a low seed never collides) and is
kept for readability. It replaces the row with `DELETE` then `INSERT`, because
`sqlite_sequence` has no key to conflict on; `SQLite` reads only the first matching row, and the
`DELETE` also removes `V010`'s inert duplicate. Both facts were verified against `SQLite` 3.51.

Rejected: seeding from the vector tables, as a third source in the migration or lazily in
`ensure_ready`. Review killed it on four counts: `ensure_ready` runs after the ids it protects
are allocated (the vector write is a separate transaction from capture); it is skipped on the
embedder-down path, exactly where items without vectors are created; `UPDATE sqlite_sequence`
is a silent no-op when the row is absent; and `MAX(item_id)` on a `vec0` table full-scans,
decoding every embedding, on every ingest. It also buys nothing, because no vector table can hold
an id the changelog lacks. The residual case, a restored partial backup, is repair: `jkb doctor
--fix` and `jkb index --sweep` already enumerate the vector tables, and they raise the sequence
past any `MAX(item_id)` they find. Also rejected: a large constant. `INT64_MAX` makes the next
insert fail with `SQLITE_FULL` (verified), and any bounded constant discards what the changelog
knows.

### An applied migration is never edited, not even a comment

`V010`'s comment claims something it does not do, and it stays. Refinery hashes a migration's
whole SQL text, comments included (`refinery-core-0.9.2/src/runner.rs:92-96`: name, version and
sql are all hashed), so a comment-only edit makes every database that applied it report a
divergent migration. An earlier draft asserted the opposite. Corrections go in the next
migration, which is also where a reader of the sequence will look.

### A deleted item's vector goes with it, by trigger

`VectorIndexer::ensure_ready` creates, beside each vector table, a `DELETE` trigger on `items`
(`vector.rs::ensure_gc_trigger`):

```sql
CREATE TRIGGER IF NOT EXISTS vec_items_<dim>_gc
AFTER DELETE ON items BEGIN
    DELETE FROM vec_items_<dim> WHERE item_id = old.id;
END;
```

A trigger lives in the database file, so it fires for every connection, every process and every
future call site, including ones that never go through `jkb-index`. That is why the objection
that killed a core-owned `ItemDeleteHook` (it works only if the database was opened with the hook
registered, so `Db::open` instead of `Db::open_with` silently reinstates the defect) does not
apply. The cost, stated: a connection that has not registered `sqlite-vec` cannot resolve the
virtual table, so an item delete on it fails. Every binary opens with
`Db::open_with(&[jkb_index::register])`, the trigger exists only in databases that created a
vector table through `jkb-index`, and `jkb-core`'s own tests never create one. If a future tool
opens the store without the extension, item deletion fails loudly rather than corrupting
silently, which is the correct direction of error. A test deletes an item on a registered
connection with a vector table present and asserts the row is gone.

Rejected: "every reader must filter" as the guarantee. It is the same procedural bet one layer
out, and readers outgrow deleters: `vector.rs` already had three readers (`knn`, `vectors_for`,
`distances_for`) plus `rebuild` and `doctor`.

### Vector reads filter liveness, inside `sqlite-vec`'s budget

The trigger closes future deletes, but existing databases carry orphans until swept, so
`VectorIndexer::knn_live` drops rows whose item is gone, as defence in depth. The filter is
applied in Rust after the query, not as a join: `sqlite-vec` requires a `k` and rejects
`ORDER BY` on anything but distance, and with `items` as the outer loop a join would run the KNN
once per item. The internal fetch never exceeds 4096, because `sqlite-vec` hard-errors above
that (`SQLITE_VEC_VEC0_K_MAX`, `sqlite-vec.c:7111`) and `vector_ranked` already over-fetches to
2048; multiplying the two over-fetches would have reached `k = 16384` and failed with a raw
extension error on the main read. `knn_live` also reports whether the index was exhausted, so
the search growth loop can tell that apart from live rows running out inside the budget; its old
test, `hits.len() < fetch`, stops growing early once filtering happens inside.

The ingest pending query got the same filter for the mirror defect (it read the presence of an
`item_id` as "indexed"); that matters only without reuse, which is why the sequence fix is the
load-bearing one. `distances_for` is not filtered, since its ids come from `Query::evaluate` over
`items` and are live by construction. A table that is mostly orphans can still return fewer than
`k` hits until swept, and `jkb doctor` reports the stale count.

### Cleaning stale rows is housekeeping, called explicitly

Nothing sweeps implicitly. The four in-transaction sweeps (in `undo`, `item rm` and both ingest
arms) were removed, and removing them was the point: it deleted the question "which call sites
sweep?", which had produced four passes of findings and would have been asked about the fifth.
One detect and clean pair lives in `jkb-index`, beside the schema it owns: `count_stale(conn)`
finds and changes nothing, `sweep_stale(conn)` removes and reports. They share their predicates,
including the non-obvious `vec0` shadow-table filter, so a report can never describe a different
set from what a sweep would remove; that drift existed while `jkb doctor` carried its own copy.
They surface as `jkb index --sweep` (needs no embedder, so it works offline) and
`jkb doctor` / `jkb doctor --fix`. `StaleRows` is a struct rather than a count because the set of
derived indexes is open, and a caller reading a field gets a compile error when one is added.
With the trigger in place, these are repair for rows orphaned before it existed.

### Never pair two lists by index

`cmd_search`'s human output zipped hits against `fetch_items` positionally, and `fetch_items`
drops rows it cannot find, so one missing item printed nothing and exited 0, and a partial gap
mislabelled every later hit. It now resolves each hit by id, as the JSON branch already did. The
rule is independent of vectors: never pair two lists by position when one can be shorter.

### Tests assert the harm, not counts

Every earlier vector test asserted counts or non-emptiness, so the suite was blind to returning
the wrong item. The tests that matter: a wrong-item test (embed A, delete A, create B, query A's
text, assert B is not returned); a migration test that builds a `V010`-era database, reproduces
its two-row `sqlite_sequence`, applies `V011` and asserts new ids exceed every id ever used; the
trigger test; and a budget test (a scoped search over orphans still returns `k` live hits, and
`knn` is never asked for more than 4096). The migration test lives in `#[cfg(test)] mod tests`
inside `src/migrate.rs`, because `mod migrate` and `mod db` are private and an integration test
cannot reach the runner. Refinery's `set_target(Target::Version(10))` is what makes "populate an
old database, then migrate it" expressible, and the harness's foreign-key toggle has to be
replicated or the rebuild trips.

### The changelog is an audit log, and `undo` reads it as an undo log

Every mutation appends to the changelog in its own transaction. The two readers have different
contracts: an audit entry says what happened, and an undo entry must carry enough to put it
back. For a long time nothing held an entry to the second contract, and three consecutive review
passes each found a defect inside the previous pass's fix: a table missing from the undoable set
(which tables), four upserts logging `insert` (which op), and correctly derived `update` entries
nothing could invert (invertibility). They were three axes of one question, because the
mechanism underneath was "cannot invert this? revert something else". The rules below close
each axis in a type, the write, or the schema.

### The entity is a type, not a string

`changelog::Entity` is a closed enum, so `entity_type` cannot be a mistyped table name. Its
variants and `Entity::ALL` are generated together by one macro, and `Entity::insert_inverse` is
an exhaustive match, so a new table cannot reach a writer without saying how an insert into it
comes back. The allowlist is derived from that match, not maintained beside it. `entity_id` is
the row's rowid wherever the inverse is keyed by one. `undo::INVERSES` covers about twenty
`(op, table)` pairs and refuses everything else, so a gap is a named refusal.

### The op is derived, never chosen

A row-writing mutation calls `changelog::upsert(conn, meta, Entity::Foo, id, before, after)`,
which records `insert` when `before` is `None` and `update` otherwise. `changelog::append` takes
an op for everything else (`delete`, `claim`, `release`, …) and refuses `insert` outright.
Choosing the op is how `view::save`, `placement::place`, `binding::set` and `tag::apply` logged
`insert` for `ON CONFLICT` arms that had updated existing rows, after which `undo` deleted those
rows.

### A before-state must be able to restore something

`changelog::write` calls `undo::check_restorable` on every entry. A before-state must be a
non-empty object naming only real columns of the table, and for a `delete` it must name every
column, because an unnamed column would come back as its default. It is checked against the live
schema, so adding a column makes every deleter of that table fail at its next write until the
column is logged.

### Refuse rather than retarget

`undo_last` selects the newest transaction containing any work, not the newest one it can
invert. `undo` wraps its whole apply loop, so any error becomes one named refusal that writes
nothing. It no longer predicts which entries are unrunnable: a kind nobody taught it about is a
refusal, never a silently reverted stranger.

### A restore that restored nothing is an error

`restored()` is the one place a row count is judged, and zero is accepted only where a named
guard says the row was deliberately skipped. Arms that answered `Ok(0)` for work they had not
done were worse than raising: `undo` wrote its marker on the strength of it, so clearing the
obstruction and retrying met "already undone".

### Undoing a design's creation waits for later work on it

Deleting a design item cascades every row keyed by it, and a design's updates are append-only
(the code-factory design): text written after the create, its spans, and every later edit's undo
would go with it. So `undo`'s pre-flight asks `design::undo_would_lose` for every
`(insert, items)` entry of a design item, and refuses while a table in `design::DESIGN_OWNED`
holds a row whose `txn_id` is a later transaction that is neither undone nor itself an `undo`.
The list is explicit, one line per table, today `design_updates`, `design_doc_targets` and
`design_sources`. The refusal names the newest such transaction, which `jkb undo` takes back
next; once the later work is undone, the creation undoes. A compacted design
(`design_snapshots`, which has no `txn_id`, folds later rows into itself and is never undone)
keeps its creation for good, and that refusal is checked first, because naming a transaction to
undo instead sent the user to undo their own edits for nothing.

A table belongs on the list only if it records its writer in `txn_id`; there is no changelog
fallback. The stated gap is `containment`: a plan put under a design by a later transaction is
orphaned by undoing the design's creation (a span is not, since `design.span` writes a
`design_updates` row too). Undo of anything other than a design is unchanged.

### `V014` draws a date line for undo

A write-time guard cannot reach backwards, and inferring whether a legacy payload happens to be
invertible is the same mistake one level along. So `undo_watermark` is seeded to `MAX(txn_id)`
at upgrade, and `jkb undo` cannot reach anything from before it. `undo_last` never selects below
the mark, and an explicit `jkb undo <txn>` below it is told the transaction predates undo history
rather than dying part-way through. A fresh database has an empty changelog, so its mark is 0.
Later migrations that change what an entry must carry raise the mark the same way.

## Ways of working

These were set for the first Rust project and have held. They are non-negotiable, and the gate
enforces most of them.

### No unsafe, with exactly one exception

`[workspace.lints.rust] unsafe_code = "deny"` applies to every crate. The only
`#[allow(unsafe_code)]` is the `sqlite-vec` FFI registration in `jkb-index`'s `vector.rs`, which
has no safe wrapper. `deny` fails the build on any other unsafe, and every other crate is
unsafe-free. Do not add a second exception.

### Lints are gates

clippy `pedantic` is on workspace-wide and warnings are errors. Two common trips:
`clippy::doc_markdown` fires on bare identifiers and on `SQLite` in doc comments, so backtick
them; and every public function returning `Result` needs a `# Errors` section. Only
macro-generated modules (see `migrate.rs`) get `#[allow(clippy::pedantic)]`.

### Errors, ids and enums

`thiserror` in libraries, `anyhow` at the binary edge. No `unwrap` or `expect` outside tests.
Ids are newtypes (`ItemId`, `NamespaceId`, `EdgeId`, with `.new(i64)` and `.get()`), so they
cannot be crossed. Enums in `jkb-types` carry `as_str()` returning the snake_case database
string, which matches their serde form.

### Security at the boundaries

Parameterized SQL only. Strict namespace-path normalization against traversal and Unicode
tricks. The `sqlite-vec` extension is pinned and vendored, with no `load_extension` of a user
path. Untrusted PDF and HTML are parsed by pure-Rust crates with size limits, never by shelling
out. `cargo-deny` audits licenses and advisories.

### Tests pin the load-bearing invariants

Unit and integration tests, plus `proptest` for invariants: re-ingest is a no-op, the same hash
is one item, `depends_on` stays acyclic, the FTS integrity check passes. Tests run offline: a
deterministic dimension-16 fake embedder covers the vector and hybrid routes, and the two live
smokes that need an external service (ollama for vector search, Chrome for URL ingestion) are
`#[ignore]`. A doubt you can name is a test to write, not a caveat.

### Cargo goes through the wrappers

Raw `cargo build|test|clippy|fmt|check` is denied by a PreToolUse hook; the scripts under
`./scripts/` self-source `~/.cargo/env`, which installs the pinned 1.96.1 toolchain from
`rust-toolchain.toml`, and pass their arguments through (`build.sh`, `test.sh`, `clippy.sh`,
`fix.sh`). `./scripts/check.sh` takes no arguments and always runs the whole gate. Likewise no
raw `sqlite3` against a jkb database: the CLI covers every read and write an agent needs, each
through the audited writer.

## The CI gate

### Hosted CI mirrors the local gate

`./scripts/check.sh` once ran only on a developer's machine, so regressions could reach `main`
unnoticed and the swarm's merge queue had no external green signal. `.github/workflows/ci.yml`
runs on every `push` and `pull_request`, one `ubuntu-latest` job, with the toolchain pinned by
`rust-toolchain.toml` (1.96.1) so CI matches local, and `~/.cargo/registry`, `~/.cargo/git` and
`target/` cached. The checks run as separate steps so a failure is legible in the Actions UI:
`cargo fmt --all --check`, clippy with `-D warnings` on default features and on
`--features fastembed`, `cargo test --all` on both feature sets, and `cargo deny`. The live
smokes stay `#[ignore]`, since CI has neither ollama nor Chrome. `deny.toml`'s advisory ignores
(RUSTSEC-2026-0192, the unmaintained `ttf-parser`, and RUSTSEC-2026-0215, `smallstr` via `yrs`)
keep CI green until those dependencies are swapped.

The job has since gained steps owned by other designs (the shell tests, the yjs wire test,
`jkb design export --check`, the container checks). Where a step can share code with
`check.sh`, it does: two hand-written copies of the shell-syntax list drifted by a `*.md` skip
within one commit of each other, green locally and red in CI on the same tree. A skip in
`check.sh` that says "CI runs this gate" is honest only if CI really does.

### Making CI a required check is the owner's setting

Making the check a required status before merge is a GitHub branch-protection setting, not a
file, so it cannot be expressed in the repo. The change that added CI said so in its pull
request and left the toggle to the repository owner.

### `check.sh` stops at the first failing gate

Every step is mandatory and fails loudly, except `cargo-deny`, which is skipped when not
installed. Because the script stops at the first failure, a step that cannot run in an
environment hides every step after it. `--all-features` pulls `ort-sys`, which downloads a
prebuilt binary, so in a network-restricted sandbox clippy cannot finish and neither can anything
below it. Lint what is reachable with `./scripts/clippy.sh -p <crate>` per crate and say which
crate was not covered, rather than reporting a gate that stopped early as green.

## Scope

What the first version set out to do, and what it left for later on purpose.

### v1 was scoped to be a daily driver

The capability set was tested against daily use as a task manager, a knowledge base and a
book-recall tool, and that review pulled into v1 what would have made the tool unpleasant to
adopt: the MCP server, one-way URL ingestion, task priority, due date and quick-add, context
expansion, non-blocking capture, and undo, rename and listing. It also surfaced the one hard
operational caveat, that the store is single-machine local.

v1 shipped nine capabilities: the namespace filesystem, pluggable indexing, idempotent
ingestion, pluggable embeddings, the query engine, multi-route search, the task DAG,
bidirectional `file://` sync with the `document` serializer, and the MCP server, plus the `jkb`
CLI, all with tests (Sections 1 to 14 of the plan).

### What v1 deferred

Deferred on purpose, with the schema and trait seams shaped so each is reachable without
rework: a local-LLM answer route; a notes and annotations flow; hypothesis and research charts;
a closure table; several embedding models at once, and API embedders; automated multi-machine
database sync; epub and OCR; remote bindings (`https://`, `git://`); near-duplicate detection;
image, audio, graph and filtered-ANN indexers; and the `spec` serializer. Multi-item sync
serializers, the sync journal and three-way merge were on this list and shipped (the file-sync
design); research charts grew into the investigations in the namespaces design; a frontend
became the desktop app in the code-factory design. An MCP `sync_status` tool is an optional
follow-up.

## Reference: finished subsystems (read last, never first)

What each finished crate is and how it is put together. Nothing in this section is a live
decision: it is the map to read before changing one of these crates, after the decisions above.
If another design names the file you are changing, that design governs; check this section last.
The section numbers are those of the original v1 plan.

### `jkb-embed` and `jkb-index` (Sections 5 and 6)

`jkb-embed`: `ollama.rs` (the default), `fastembed.rs` (feature-gated, its model behind a
`Mutex` because fastembed's `embed` takes `&mut self`), and `lib.rs` with `EmbedderConfig`,
`build()` and `truncate_to_chars`. `jkb-index`: `trait Indexer` (`name`, `accepts`, `index`,
`remove`, `rebuild`), `IndexItem` and `Dispatcher` (`on_upsert`, `on_delete`, `rebuild_all`);
`vector.rs`, the only `sqlite-vec` and `unsafe` module (`register()`, `VectorIndexer`,
`ensure_ready` with its delete trigger, `knn_live`, `distances_for`, and `rebuild`, which
re-embeds from content through an injected `Embedder`); `fts.rs` (`FtsIndexer`); and
`count_stale` / `sweep_stale`.

### `jkb-ingest` (Section 7)

`Pipeline` drives capture then embed. `capture` runs in one `write_txn_with`: the idempotency
check on the `ingestions` row, the blob (`blob.rs`, which re-exports core's
`blob::{hash_bytes,store,load}`), the document item and chunk items (`chunk.rs`: character
windows with overlap and min/max bounds, a too-small tail folded into the final window, uid
`b3:<hash>:<idx>`), `derived_from` edges and placements. `embed_and_complete` is a separate
transaction with embeddings computed off-thread; it writes vectors and marks the ingestion
complete. `index_pending` and `unembedded_count` finish what a down embedder left. `adapter.rs`
is the `SourceAdapter` trait plus the adapters and `parse()` dispatch; `fetch.rs` is the
headless-browser URL render. `Error` bridges `jkb_core` and `jkb_index`.

### The `jkb-core` query engine (Section 8)

`query/mod.rs` (the AST and `Query::evaluate`, with `Value` parameters through
`params_from_iter`), `query/parse.rs` (the DSL, over the shared quote-aware `dsl.rs`
tokenizer), `view.rs` and `mount::ambient_namespace`.

### `jkb-search` (Section 9)

`Searcher::new(embedder)` with `search(db, &Query, Route, limit)` and `get_context(db, item, n)`.
`scope_query` turns a `Query`'s structural part (ranking terms stripped) into the candidate id
set via `Query::evaluate`, with `None` meaning unrestricted. `vector_ranked` implements the three
recall tiers; `fts_ranked` over-fetches then filters, since FTS candidates are few. `SearchHit {
item, route, score, distance, namespace_path, source_document }`. `jkb search --json` resolves
those ids: every hit carries `uid`, `kind`, `status` and `snippet`, and `source_document` is an
object (`{id,uid,kind}`), not a bare row id, because a result identified only by an integer is
not interpretable by the agent that asked, and search is the one read an agent cannot fall back
to `query` for. Use `Db::read_with::<_, Error, _>` to `?` across core and index errors.

### The `jkb-core` task DAG (Section 10)

`crates/jkb-core/src/task.rs` is the typed API over the item substrate. `create(&NewTask)`
inserts the item, places a `Primary` home and `Reference` mirrors, sets the binding, applies
tags and links `depends_on` edges (cycle-guarded by `edge::link`). `set_status_str` is the
string boundary that rejects `blocked` and unknown values (`TaskStatus::from_manual_str`), and
`set_status(TaskStatus)` cannot represent `blocked` at all. `ready(conn, Scope, &[TagPred])`
does not duplicate SQL: it builds a `Query { kind: task, ready: true, scope, tags }`, calls
`Query::evaluate` (the one `is:ready` anti-join), and orders the ids by priority (ascending,
nulls last) and then `date(due)`. `is_blocked` mirrors that anti-join for one task.
`parse_quick_add` feeds `NewTask::from_quick_add`, which defaults the binding to `managed:`
(`MANAGED_BINDING`) and homes per the namespaces design, falling back to `tasks/inbox`
(`DEFAULT_HOME`).

### `jkb-sync`, first version (Section 11)

`engine.rs`'s `sync(db, mount_ns) -> SyncReport` discovers files (a `globset` walk of the
backing directory, unioned with `binding::synced_uris_under` so files created in the store or
deleted on disk reconcile) and reconciles each in its own `write_txn`; export writes the file
inside the transaction, so a failure rolls back with no hash drift. `watch.rs` runs a `notify`
watcher: a full reconcile, then a debounced re-sync of only the event paths (`sync_paths`), with
a full `sync` only on a watcher error or `need_rescan`, since the OS watches a directory subtree
and glob filtering is ours. `watch_all` watches every mount on a thread each, stopped by a shared
`Arc<AtomicBool>`, so `jkb service install`'s launchd or systemd unit can run
`jkb sync --watch` at login; the OS supervisor owns the lifecycle.

### `jkb-sync` v2: multi-item serializers and the journal (Section 15)

`SyncSerializer` became `parse(&[u8]) -> SyncDoc` / `render(&SyncDoc) -> Vec<u8>` plus
`quarantine_on_parse_error()`, in `serializers/{mod,document,tasks}.rs`. `document` stays
byte-compatible with v1 (one item, a bare `file://` uri); `tasks` maps one `tasks.md` to many
items, each bound to `file://<path>#<local_id>`. The engine is journal-driven and three-way over
the `_sys/sync` journal (`sync_state`, `V004`). Its rules are the file-sync design.

### `jkb-cli` (Section 12)

`crates/jkb-cli` builds the `jkb` binary. `main.rs` is a `clap` derive `Cli` and `Command`;
`run()` opens the database once with `Db::open_with(&[jkb_index::register])` and dispatches to
`cmd_*` functions. Global flags: `--db` (default `$JKB_DB` or `~/.jkb/jkb.db`), `--json`,
`--global`. Output goes through `output.rs` (`DisplayItem`, `fetch_items`, `print_items`), human
or JSON. `apply_ambient` scopes an unscoped query to the cwd's mount. The embedder is built
lazily, only where needed, so reads, tasks, queries, sync and undo work fully offline, and
ingest still captures when it is down. Tests are `tests/cli.rs` via `assert_cmd`, offline.

Linux-style, agent-facing verbs wrap the same reads: `ls`, `tree` (with per-folder
`ns::subtree_leaf_count`), `grep` (a literal substring via `SQLite` `instr`; it exits 1 on no
match, the only command nonzero on empty), `find` (typed filters mapped to the query DSL),
`recent`, `cat` (raw body), `stat` (metadata without body) and `guide` (the agent cheat-sheet,
mirrored in the root `AGENTS.md`). `grep` is literal, `find` and `query` are structured, and
`search` is ranked: pick by what you know.

### `jkb-mcp` (Section 13)

Two halves. `logic.rs` holds the tool work as plain synchronous functions over
`Tools { backend }`, a `jkb_api::Backend`, so every tool is an operation: a `LocalBackend` with
the embedder on the host, or `jkb serve` in the dev container (where `search` defaults to FTS if
the backend does not embed, and a file or URL is read where the server runs). Each returns an
`Answer`, JSON plus whether the read was cut, and is unit-testable with no transport or runtime.
`server.rs` is the thin async adapter: `JkbServer { tools }` with `#[tool_router]` and `#[tool]`
methods that run each function on `tokio::task::spawn_blocking` (the writer and ollama block),
and `#[tool_handler] impl ServerHandler` advertising the tools. `lib.rs::run_stdio(tools)` builds
a tokio runtime and serves stdio; `jkb mcp` calls it.

The `rmcp` 2.0 lessons, learned with `./scripts/inspect-dep.sh rmcp-2.0.0 …`: `#[tool_handler]`
calls the generated `Self::tool_router()`, so do not store a `tool_router` field (it would be
dead); arguments derive `serde::Deserialize` and `schemars::JsonSchema` and arrive as
`Parameters<T>`; content is `rmcp::model::ContentBlock`; `ServerInfo` and `ServerCapabilities`
are `#[non_exhaustive]`, so mutate a `default()`. Errors map to `ErrorData`, user input to
`invalid_params`.

### Fleet hardening (Section 17)

The robustness pass on the agent swarm: claims as two `items` columns (`claimant_id`,
`claimed_at`, `V005`) with a compare-and-swap `claim` and liveness by owner existence, never age;
the full CLI mutate surface (`task show`, `set`, `edit`, `tag`, `depend`, `undepend`, `place`,
`bind`, `claim`, `release`, `reclaim`); `needs_review` no longer unblocking dependents; task
status kept out of git; and the scheduler, implementer, reviewer and merge-queue pipeline. Its
current rules are the task-lifecycle and agents-and-roles designs.

## History

Superseded and reversed decisions, with what replaced them and why.

### `unsafe_code = "forbid"`

The first guardrail was `forbid(unsafe_code)` in every crate, stronger than a per-file
attribute. It held through Section 5. Registering `sqlite-vec` needs one FFI call with no safe
wrapper, and `forbid` cannot be locally allowed, so Section 6 relaxed the workspace lint to
`deny` with one scoped, commented `#[allow(unsafe_code)]` in `vector.rs`.

### Every trait in `jkb-types`

The crate layout put `Embedder`, `Indexer` and `SourceAdapter` in `jkb-types`, so consumers
could depend on a trait without its heavy implementation. `Indexer` moved to `jkb-index`: it
takes a `rusqlite::Connection`, and putting that in the vocabulary crate would have dragged
`SQLite` into `jkb-embed`. The original rationale was hiding heavy dependencies such as ONNX,
which does not apply to a connection type.

### `item_id` as a `vec0` auxiliary column

The design gave each vector table an `item_id` auxiliary column. The implementation made
`item_id` the `vec0` rowid (`INTEGER PRIMARY KEY`) instead: KNN still returns ids with no join,
and upsert and delete by id become direct.

### URL fetch with `reqwest` and readability

The first plan put fetching remote sources in a later version, and the URL decision then pulled
one-way URL ingestion into v1 as a static fetch plus readability extraction. A static fetch sees
only server-sent HTML and misses client-rendered pages, so ingestion renders with a real
headless Chrome and extracts from the rendered DOM; `reqwest` is not used there.

### A read-connection pool

The writer-actor design gave readers a small pool of connections. It was deferred at
implementation: all access is serialized through the writer thread, which is fine at single-user
scale. That is why the query embedding must happen on the caller's thread, outside `db.read`.

### `undo` inverted inserts only

The first `undo` replayed a transaction by deleting the rows it inserted, by rowid; inverting
updates and deletes was future work, and the fleet-hardening notes still say claims are "not
undoable (undo inverts only inserts)". Replaced by `undo::INVERSES` and the rules under "What
the store guarantees", after three review passes in a row found the defect inside the previous
fix. The lesson: when findings keep landing on the last fix, the mechanism is the defect.

### Sweeping orphan vectors at each call site

The first response to rowid reuse made each deleter sweep its vectors in the same transaction:
`jkb undo` in review pass 5, `jkb item rm` in pass 6, ingest's re-capture arm in pass 7, its
fresh-capture arm in pass 8. Each fix was correct and incomplete, and pass 8 found that
`index_pending` itself could mint an item on an orphan, so the list was never going to close.
Replaced by `AUTOINCREMENT`, the delete trigger and explicit housekeeping.

### `V010`'s seed and "a stale row is inert"

The item-id fix claimed that with `AUTOINCREMENT` a leftover vector row was stale, not
dangerous, and removed the sweeps on that basis. Both halves were false for two more passes.
`V010`'s `INSERT OR IGNORE` into `sqlite_sequence` cannot ignore, since that table has no key,
so it always added a second row; and it seeded from `MAX(id) FROM items`, the surviving maximum,
resetting the high-water mark below every id freed at the top. Reproduced against real `SQLite`
with the migration's exact body: with items 1 to 5 and 4 and 5 undone, the next two inserts got
ids 4 and 5 and inherited the dead embeddings, while `jkb doctor` reported `ok`. Replaced by
`V011`'s changelog seed, the trigger and the liveness filter.

### A core-owned `ItemDeleteHook`

The strongest alternative to `AUTOINCREMENT` was a delete hook registered like
`ExtensionRegistrar` and called from `item::remove` and `undo`: two call sites, no migration, and
clean data. Rejected because it worked only if the database was opened with the hook, so
`Db::open` instead of `Db::open_with` silently brought the whole defect back, and it did nothing
for databases already holding orphans. The delete trigger later delivered its benefit without
that flaw.

### A generic undo guard for every item's cascade

Before the design-owned list, undo tried a generic guard: refuse an item's creation while any
later changelog entry, or any row of any `ON DELETE CASCADE` table found in the live schema,
belonged to the item. It confused rowids with item ids when attributing a row to its writer, so
unrelated transactions were blamed and rows that `undo` re-inserted had no writer, which made a
creation permanently un-undoable; and it counted rows of unlogged audit tables
(`task_transitions`), so a task's creation and the transaction the refusal named refused each
other for ever. Replaced by `design::DESIGN_OWNED`, scoped to designs, in the Code Factory work.

### `V006` toggling foreign keys itself

The status-check migration was specified to wrap its rebuild in `PRAGMA foreign_keys=OFF` with
an in-transaction `foreign_key_check`. That pragma is a no-op inside refinery's per-migration
transaction, so `DROP TABLE items` would have cascaded through its children. Replaced, at
implementation, by the harness owning the toggle. The plan's task text still describes the
original approach.

### Four manual task statuses

v1 had four manual statuses, `open`, `in_progress`, `done` and `cancelled`. `needs_review` was
added as a fifth by fleet hardening, which is why the status `CHECK` lists five, and it does not
unblock dependents.

### Single-hash sync for documents

v1's sync tracked one `last_synced_hash` per file; for `document`, the file bytes equal the
store's render, so one hash covered both sides. Replaced by the journal-driven three-way engine
when multi-item serializers arrived, because a hash detects drift but cannot merge it. The
file-sync design owns the details.
