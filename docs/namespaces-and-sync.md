# Namespaces, file sync, and the storage invariants

The global namespace layout (D32), typed namespaces and their contracts (D33),
investigation namespaces (Dmem), and the file-sync data-loss cluster — the openspec
collapse, a file's document living on its journal row (D45), one file per namespace
(D39), item-id stability (D40/D42), and the changelog/undo split (D47).

Part of the jkb documentation set; see [CLAUDE.md](../CLAUDE.md) for the
conventions every session is expected to know.

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

## Sync never follows a symbolic link on a bound file's path

**Decided (stage-6.2 review, 2026-09-15):** every read and write of a synced file goes through
`jkb_core::nofollow`, which walks the path from `/` a component at a time with `O_NOFOLLOW`, refuses a
link at the file or any directory above it, reads only a regular file (non-blocking, so a FIFO cannot
hang the watcher), and writes a temporary file beside the target and `renameat`s it into place — which
replaces whatever is at the name rather than writing through it, and keeps an existing file's mode.

Why: the host's `jkb sync --watch` writes a bound file whenever the knowledge base changes, and since
the task-mutate set a dev container can make those changes through `jkb serve`. The container can also
write inside the host directories it binds. With plain `std::fs::write`, replacing a bound `tasks.md`
under `~/repos` with a link to `~/.zshrc` and then editing the task turned the next sync into the
container writing to the host user's shell startup file. `jkb_api::tasks::FileRoots` could not stop it:
it judges a path's spelling, and a check followed by a write races a link swapped in between. Walking
by file descriptor is the check and the use in one.

Around it, the same threat is closed at the other ways in (a third review): a read is refused past
64 MiB (`nofollow::MAX_READ_BYTES` — a sparse file planted at a bound path costs the container nothing
and allocated its whole apparent size on the host); the watcher is built with
`with_follow_symlinks(false)`, since a directory link planted in a mount made the recursive watch
subscribe to the host directories behind it; a watch event for an unbound path reached through a link
below the mount directory is dropped, and sync never takes up `nofollow::write`'s own temporary files,
which an interrupted write leaves behind and a full sync would otherwise import as a second copy of
every task; and a new file is created under the user's umask, with an existing file keeping its mode.

A **bound** file reached through a link is not skipped (a fourth review): it is reconciled, `nofollow`
refuses it, and its journal row stays `needs_attention` naming the link. Skipping it at discovery, as
the third review's fix did, let the full sync's out-of-scope sweep settle the row to `ok`, so the file
stopped syncing with `jkb doctor` reporting nothing. A size or type refusal carries no link remedy, and
names the 64 MiB limit.

What it costs: a synced path may not contain a link at all. `jkb mount create` stores a mount's
directory canonical, so real mounts qualify; a test that mounts a raw temp directory must canonicalize
it first (on macOS `/var` is a link), which `jkb-sync`'s tests now do. Pinned by
`jkb-core`'s `nofollow` tests (a link at the file, above it, dangling; a FIFO; mode kept; the umask, in
a child process) and, because a bound path is no longer filtered before it is read,
`sync_never_writes_through_a_symlink_planted_at_a_bound_file` and
`a_bound_file_reached_through_a_link_stays_flagged_naming_the_link`, which fail when the engine goes
back to following links. Not verified on macOS here — the tests ran on Linux.

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
