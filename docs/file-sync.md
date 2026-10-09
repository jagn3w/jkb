<!-- generated from jkb design design:file-sync-18dcdeef6c224108fa2300, edit there (version 68.RIOou72fgOIEAYaImc2B2ncBhsWF0bmAxAEBh7m0rZXXrwMBif2Pn-b6iAUBjLvqmPXIggoBjqOpm6TkoAEBj9bemOnWjgcBkMbDkJT7qA8BjpjYqprXkwsBk6yl8K64wQQBlaCDwuzPYgGVm4DUrKHXCwGXvYrd2vORAgGViKXG3caPAQGX2vzBpaLKBgGYwq3b6_a8BQGl1ob2qvGXBgGmv_Gy3J7TDAGmj8-I_aWBBgGms7eD4q7hBAGpnqzIr47yDwGq4dmf4erZAQGrtqaxiYjqDwGsv9P2p7UrAa3stqT1svoKAa7enPKY680IAamPpqnV6KABAbDf1pnD8ZcMAbHqyILSmJUHAbP65YyTvo0DAbWo-qqettYOAbbR9sOc6a4NAbbGiqaMwZsJAbaFjuCY0foOAbmxj7u5jd0GAbmGqZv9wZkPAbzJgeCz_KEDAcP1vODh_NgLAcbRgIqdmqAGAciDo5GK7MYIAcrnnIur-cUPAcuv_d2S1twNAdDQ_Jmi170OAdLChZPEzqMBAdPs5634w7ADAdKFk_3OpOcFAdXJp7Hmy5kBAdrDl9CQ0JIJAd60tPrl4b4MAeDriqTy2_MFAeK8u764quUEAeOau42pzZgMAeL9uJWA4_4EAeSkq43xuYYPAeaG7tyAoP0CAem147Wxi8gDAeyH54GUlIgBAe7Mx5HNu4sMAe6nuK-09uIDAfHci53TraQPAfL9lo7AhyQB86Ld9cCpswwB8oaGyr6D2wsB9Zid07Wxggb-tgP27ZCasMGDBQHz5uiP39fICgHyjsLoyKXaAQE, blake3 34a3f7c03b9463effb3290310a5cf33cb89259983930663aeeb0ba935cd72a59) -->
Migrated from: openspec/changes/jkb-v2-file-sync/design.md, openspec/changes/jkb-v2-file-sync/proposal.md, openspec/changes/jkb-v2-file-sync/specs/file-sync/spec.md, openspec/changes/jkb-v2-file-sync/tasks.md, openspec/changes/jkb-sync-file-namespaces/design.md, openspec/changes/jkb-sync-file-namespaces/tasks.md, openspec/changes/jkb-derived-state-safety/design.md, openspec/changes/jkb-document-not-in-namespaces/design.md, openspec/changes/jkb-staging-pr/design.md, docs/namespaces-and-sync.md (lines 127-410, 523-565)

# File sync

File sync round-trips files on disk to items in the knowledge base. A mount binds a directory
to a namespace; each file under it is read by a **serializer** into a document of items, and
the engine in `jkb-sync` (`engine.rs`) reconciles the file against the KB in both directions.
This design records what a serializer owns, how the engine decides direction with a
three-way merge, why each synced file has its own namespace, and the data-loss cluster: the
run of incidents, from the `openspec` collapse onward, in which a sync wrote the wrong bytes
over a user's file, and the model that ended it.

Two facts from the namespaces design run under all of it. A namespace is *logical*: it says
where an item is filed, not where its bytes live. The **binding** is the second axis: an item
is `managed:` (KB only) or bound to a `file://` URI and round-tripped by a serializer. The
storage substrate (items, edges, tags, blobs, the changelog and `jkb undo`) is the foundation
design; the task DAG the `tasks` serializer writes into is the task-lifecycle design.

One rule outranks every other decision here: **sync never silently loses data or writes
garbage back.** Most of what follows is either a mechanism serving that rule or the record of
a place it was broken.

## Serializers

A synced file is a serialization of items, not intrinsically one item. The `SyncSerializer`
seam began as "a whole file ⇄ one content string" and was generalized so that one file can
carry many items with hierarchy, dependencies and status: the north-star case was replacing
OpenSpec for local planning, where a single hand-edited `tasks.md` is the serialization of
many tasks.

### The serializer owns the bytes; the engine owns persistence

A serializer owns only the file ⇄ `SyncDoc` boundary: it is pure and touches no database. The
engine owns identity mapping, the reconcile state machine, the three-way merge, quarantine and
every write, all inside the per-file `write_txn`. So a new file format is a pure function, and
every persistence decision lives in one place. The serializers live in
`jkb-sync/src/serializers/{mod,document,tasks}.rs`; `mod.rs` holds the payload types, the
trait, `resolve()` and `AVAILABLE` (`["document","tasks"]`).

### The `SyncDoc` payload

`SyncSerializer` is `parse(&[u8]) -> Result<SyncDoc>` and `render(&SyncDoc) -> Result<Vec<u8>>`,
plus `quarantine_on_parse_error() -> bool` (default `false`).

```
SyncDoc    { sections, items, edges, layout }
SyncSection{ path, header_line }                    // a header, which becomes a namespace
SyncItem   { local_id, kind, content, section, position,
             status, priority, due, tags, mirrors, parent }
SyncEdge   { src, dst, edge_type }                  // local_id → local_id
SyncBlock  = Section(path) | Item(local_id) | Prose(text)
```

`layout: Vec<SyncBlock>` is the document's block order with prose inline, and it is the only
thing `render` consults for ordering (see "One authoritative layout" below).
`SyncItem::position` survives only as the KB-side `placements.position` hint, never as
document order.

### The `document` serializer: a whole file is one item

`document` yields one `SyncItem { local_id: "", kind: "document" }` with no sections, edges or
layout, and its binding URI stays the bare `file://<path>`. It is byte-compatible with the
first sync implementation, and its tests survived every change in this design unchanged.
`quarantine_on_parse_error()` is `false` for it: its only parse failure, non-UTF-8, stays a
hard error that rolls back. It is the default serializer for a mount.

### The `tasks` serializer: one `tasks.md` is many tasks

One `tasks.md` ⇄ many `kind='task'` items. A checkbox is a status (`[ ]` open, `[x]` done,
`[~]` in progress, `[-]` cancelled). A trailing run of quick-add modifiers (`!p<n>` priority,
`@<date>` due, `#<facet>=<value>` tags, `+<ns>` mirror placements) sets the matching fields,
using the quote-aware tokenizer style of `task.rs`. `needs:^<id>` becomes a `depends_on` edge,
and two-space indentation becomes `parent_of`. `##`/`###` headers become sections, and
everything else (prose, the legend comment, blank lines) is preserved verbatim.

### Stable in-file identity is a visible caret `^id`

Each task line carries a trailing `^<id>`. It is the item's `local_id`, and the item's binding
URI is `file://<path>#<local_id>`, so `binding::item_for_uri` (exact match, unique) is the
reconciliation key, independent of the item `uid`. Editing a line's title on disk updates the
same item, keeping its status, edges and history. A caret-less line is given an id **minted
deterministically**, `slug(title)-<blake3(title)[:6]>` disambiguated by a counter, with no RNG
or clock, so `parse` stays pure. Minting means an import may rewrite the file to stamp the id
(a caret write-back), and that is allowed even on an import-only mount, so identity is
durable.

Rationale: the user chose a visible caret up front. Positional matching is what a hand-edit
breaks first; an invisible identity (an HTML comment, a sidecar file) is what a hand-edit
deletes without noticing.

### A `^id` is read in the alphabet `slug` mints it in

The `tasks` serializer reads `^id` as an identity when every character is a dash or satisfies
`jkb_core::dsl::is_slug_char`: a letter or digit already in lowercase in any script (including
one with no lowercase form, such as `ℝ`), or U+0307, the one combining mark lowercasing
produces. `dsl::slug` emits nothing outside that set. One predicate, owned beside the minter.

Why: `mint_id` slugs a title with the Unicode-aware `slug`, so `Café résumé cleanup` mints
`café-résumé-cleanup-e0b106` and `修复` mints `修复-108866`. The parser's original ASCII-only
rule read each stamped id back as title words, minted another and stamped it too: measured
over three parse/render passes, such a task took a new id every pass and its line grew a
`^id` per sync. Nothing noticed until the task-write line check refused every write to such a
task. A first fix widened the reader to "non-ASCII, not uppercase, not whitespace", which was
too wide (a title's trailing `^🎉` became an identity, so a file an older build had settled
changed ids or was quarantined for a duplicate `^🎉`) and too narrow (`prove-ℝ-…`, which
`slug` emits, was still refused, reproduced churning). A second accepted every combining mark,
which made a trailing `^1️⃣` an id. Reading exactly the minter's alphabet fixes all three: an
id the minter makes always comes back, and anything it never makes (emoji, punctuation,
format characters, uppercase) stays title text. Every stored id is kept, and a churned line
heals to its first id by the existing left-most-anchor rule. Pinned by
`every_alphanumeric_lowercases_into_the_slug_alphabet` (both directions, every `char`),
`slug_keeps_unicode_alphanumerics`, `a_title_in_any_script_keeps_one_id_across_syncs` and,
through the ops, `a_task_titled_in_any_script_is_filed_and_written_like_any_other`.

### Modifier parsing is lenient; only structural corruption fails

Learned from ingesting a real openspec `tasks.md`: modifiers are recognized **only as a
well-formed trailing run** on a task line. Everything before the first non-modifier, scanning
from the end, is verbatim title. A bare `+`, a `#tag` without `=`, a non-integer `!p`, a
non-slug `^id`, or any mid-line sigil is ordinary text and never an error, because real task
descriptions are prose that contains `+`, `#`, `@` and `^` ("run fmt + clippy and fix #123").
This also matches `render`, which always emits modifiers trailing. The only hard parse
failures, which quarantine the file, are genuine structural corruption: two lines claiming
the same `^id`, an in-file `depends_on` cycle, or non-UTF-8. Prose never fails.

The cost of that leniency is recorded under "Still open" below: a truncated file parses
cleanly.

### Section headers become namespaces, as a derived view

A header becomes a namespace under the file's own namespace (`<file-ns>/<section-slug>`;
nested headers extend the path), and the tasks under it are placed there. A section is real,
queryable structure: browsing a file's sections in the tree, `task next` scoped to one, and
`ns:repos/…/backend/**` all depend on it.

Those namespaces are **derived and rebuildable**, jkb's existing rule for indexes. Nothing
reads them to decide what a document looks like; the document's structure lives on the file's
journal row (see "A file's document lives on its journal row"). If a user renames one with
`jkb ns mv`, an item's placement moves and the file still renders correctly. A header rename
yields a new slug and re-derives placement on the next import.

### Prose is never an item

Prose, the legend and blank lines are `Prose(text)` blocks inline in the layout. They carry no
knowledge, nothing links to them and nothing queries them, so they get no identity. Every
attempt to give them one (a content hash plus an occurrence counter) produced ids that broke on
the next edit: two blank lines are indistinguishable, and inserting a line shifts every ordinal
below it. The orphaning that caused is in History.

### One authoritative layout orders the document

`render` consults only `SyncDoc::layout` for order. A merge takes the layout wholesale from the
disk side, and anything the layout does not mention is appended rather than dropped. This
replaced three integer sequences (a section's `position`, an item's `placements.position`, a
prose block's ordinal) that were written at different times and mixed across up to three
different parses by a three-way merge; the numbers stopped describing one document, and a
`##` header rendered into the middle of an item, observed twice on a real file.

### An item's section is derived from the layout, not its placement

`apply_layout_sections` sets each item's `section` from the layout rather than from the
namespace it is placed in, so the two cannot disagree. Consequence: re-homing a file-backed
item does not move it between sections in its file; edit the file for that. Before, the
disagreement made the KB permanently differ from the base and turned every later disk edit
into a conflict.

### `render` must be idempotent

For `tasks`, `render(parse(x))` is not byte-identical to `x`: it stamps `^id`s and
canonicalizes whitespace. So "disk hash equals render hash when settled" is false, and
hash-only change detection would read "KB changed" forever: an export storm and an infinite
watch loop. The engine therefore stores the **rendered** bytes as the three-way base after
every successful reconcile, and `render` must satisfy
`render(parse(render(doc))) == render(doc)`, guarded by a serializer round-trip test.

### Serializers are chosen per mount and overridable per file

A mount names its serializer (default `document`); a file's `bindings.serializer` overrides it,
read in `engine::resolve_serializer`, so other files under the mount keep the mount's.
`resolve()` rejects an unknown name with an error naming the available serializers.

## The journal and the three-way merge

Hash-only detection (compare the disk hash and the KB render hash against
`bindings.last_synced_hash`) cannot tell which side changed once one file holds many items,
and cannot keep both of two disjoint edits. Direction is decided three-way against a
persisted base, and the per-file record of that base is the sync journal.

### The `_sys/sync` journal

`V004__sync_journal.sql` adds `sync_state(uri PK, serializer, status
CHECK(ok|conflict|needs_attention) DEFAULT 'ok', last_synced_hash, base_blob_hash,
parse_error, quarantine_blob_hash, updated_at)`, the `sys_sync` view, and the `_sys/sync`
namespace (mirroring `_sys/transactions`). `V012` adds `document` (below) and recreates the
view to carry it. The repo is `jkb_core::sync_state` (`get`, `upsert`, `set_quarantined`,
`needs_attention`), parameterized and changelogged; blobs go through `jkb_core::blob`
(`hash_bytes`, `store`, `load`), the one blob store, which `jkb-ingest` delegates to. The
journal is authoritative for direction. `bindings.last_synced_hash` stays for back-compat (the
`document` path reads it) but no longer decides anything, and is `NULL` for `tasks`.

The `sys_sync` view is the only read surface for the journal, so any MCP client reads it
through the existing `query` tool. That is why the optional MCP `sync_status` tool was not
built.

### Three-way: base, disk and KB

- **base**: the rendered bytes at the last successful sync, a content-addressed blob named by
  `sync_state.base_blob_hash`.
- **disk**: the file's current bytes, possibly absent.
- **kb**: `serializer.render(kb_doc)`, where `kb_doc` is assembled from the file's structure
  and the items currently bound to it.

A side changed if and only if it differs from the base (`disk_changed`, `kb_changed`). Disk is
never compared directly to the KB. If the rendered KB bytes equal the base bytes, nothing is
loaded or parsed, so a settled file costs nothing.

### The merge is per item; the conflict unit is one task

For `document`, a base only lets "both sides changed to the same bytes" resolve as converged.
For `tasks`, base, disk and KB are compared per `local_id`: disjoint edits (a different task
changed on each side) merge automatically (`Outcome::Merged`); edits to the same task are the
conflict unit, resolved by the mount's `conflict_policy`: `manual` reports and touches
nothing, `disk_wins` imports, `kb_wins` exports. Direction is decided per file, the merge is
per item, and the write-back re-renders the whole file, never a partial one. A merge builds
its sections as the union of disk and KB, and takes its layout from disk alone.

### Every outcome is named, and a resolution is never silent

`Outcome` is `Created`, `Imported`, `Exported`, `Merged`, `Conflict`, `Quarantined`,
`Normalized` (neither side changed in substance but the stored bytes were not today's
canonical render, so the file and base were re-settled; reported apart from `UpToDate`
because it writes the file), `UpToDate`, `Skipped` (the mount's `sync_mode` forbids the
direction), `ResolvedFromDisk`, `ResolvedFromKb`, `Refused` and `Failed`. A policy resolution
throws one side's edits away, so it must never read as an ordinary import or export, and
`SyncReport::resolved()` names it. `Failed` is per file: one bad file, or one unparseable
base, is reported rather than propagated with `?`, so it cannot end the run or, under the
watcher, kill a mount's thread. `Refused` and `Failed` are reported like `Conflict`, not as a
non-zero exit, so the watcher does not die on one file. The watcher prints every
resolution, failure, refusal and conflict: a `kb_wins` resolution settles the journal `ok`,
so without that line it was invisible on every surface in the system, `jkb doctor` included.

### Quarantine, don't destroy

When a serializer with `quarantine_on_parse_error() == true` (`tasks`) fails to parse a
changed file, the engine stashes the failing bytes as a blob (`quarantine_blob_hash`), records
`parse_error`, sets the journal `needs_attention`, and touches neither the items nor the base,
nor writes a render over the file. It returns `Outcome::Quarantined`, not `Err`, so the
journal update commits. Recovery is automatic: the next well-formed edit imports normally and
the status returns to `ok`. `jkb doctor` lists every `needs_attention` file with an
actionable message.

### Import is two passes: items, then edges

Items are created or updated before any edge is linked, so every `local_id` resolves to an
`ItemId` by the time an edge names it. Pass 1 upserts items (with `content_hash = None`, so
identical-title tasks do not dedup-collapse), sets status, priority and due only where they
differ (keeping the changelog quiet), reconciles tags (`tag::remove` stale, `tag::apply`
missing) and places each item under its section. Pass 2 links the desired `parent_of` and
`depends_on` edges (idempotent `edge::link`, skipping `src == dst`) and `edge::unlink`s any
current edge of those types no longer desired; an in-file cycle quarantines. Then the items
absent from the disk document are detached, the base blob is persisted and the journal
upserted.

### A task removed from the file is cancelled and detached, never deleted

A line deleted from `tasks.md` marks its item `status=cancelled` and detaches it (its binding
becomes `managed:`). The item, its edges and its history stay, and it drops out of the ready
frontier. Putting the line back restores the same item: `create_item` re-attaches an item
whose file-derived uid already exists rather than failing the whole sync on `UNIQUE
constraint failed: items.uid`, as it once did.

### `jkb sync --conflict <policy>` overrides the policy for one run

The only way to unstick a conflicted file used to be re-creating the mount with a different
`--policy`, and that was the very write that once dropped a mount's include glob (see
History). The mount no longer has to be edited to get a sync moving.

### `mount create` preserves what you did not name

Re-running `jkb mount create` on an existing mount reads it and applies only the flags
actually passed (`FieldEdit::{Keep,Set,Clear}`); `--no-include`/`--no-exclude` clear
explicitly. It prints the resulting configuration every time, and `jkb mount ls` shows mode,
policy and globs, because a mount whose glob had been dropped looked identical to one that
still had it.

### Every settled version of every file is recoverable

`blobs` is content-addressed and never garbage-collected, and sync stores the bytes of every
version it settles and every version it overwrites, so the store is a complete history of
every synced file. `jkb blob ls --contains "<a line you remember>"` finds a version and
`jkb blob cat <hash>` writes it out. That is how all 62 files of the `openspec` collapse were
recovered: the originals were the import cohort, distinguishable from the damaged exports by
still having headers.

## One file per namespace

### A file's namespace includes its filename

`namespace_for` maps a file to `<mount_ns>/<dir components…>/<filename>`: `changes/foo/tasks.md`
to `…/changes/foo/tasks.md` and `changes/foo/design.md` to `…/changes/foo/design.md`. Two
files can no longer share a namespace, a section tree or a structure, by construction rather
than by a guard that has to be right. The segment keeps the extension: `tasks` reads better,
but `tasks.md` beside `tasks.txt` would collide again, the same defect in a rarer and
therefore worse form. A `.` in a segment is already legal; segments are opaque strings joined
by `/`.

Rationale: the directory-derived rule this replaced was the root of the `openspec` collapse
(History). It also bounds `retire_undeclared_sections` to one file's subtree; without it,
importing one file retires the sections of every nested synced file, and for a file at the
mount root, of the entire mount. Reverting it was proposed once and rejected on exactly that
evidence.

### A directory may hold many synced files

A `tasks` mount over `openspec/` can sync `changes/foo/tasks.md` and `changes/foo/design.md`,
each with its own sections, structure and items. This is the user-visible gain, and why the
change was worth re-homing every synced item rather than adding an eighth guard. `document`
mounts were unaffected in behaviour: one item per file consults no structure, so sharing a
directory namespace had always been safe for them.

### The ownership scaffolding was deleted, not kept as defence in depth

`Outcome::Collided`, `colliding_paths`, `requires_exclusive_namespace`,
`shares_namespace_with_other_bound_file`, `LAYOUT_URI_KEY`, `LayoutOwner`, `layout_owner`,
`claim_layout`, `unclaimed_legacy`, `foreign_layout` and `refuse_foreign` all existed to answer
one question: whose structure is this namespace's? With one file per namespace the question
has no instances. Kept "just in case", they would be ~400 lines whose correctness depends on a
condition that can no longer occur: nothing would exercise them, nothing would keep them
honest, and the next reader would have to re-derive that they are dead before touching
anything nearby. A guard that cannot fire is not defence in depth; it is a second model of the
world that quietly disagrees with the first.

Rejected: a `layout_owner` column on `namespaces`. It records an answer that legacy rows do not
have, so the whole problem reappears as "what do we do about NULL". Rejected: keeping the
directory namespace with a per-file `{filename: layout}` map on it. It works, but the
namespace tree stops describing the file tree, and `jkb ls` on that namespace shows two
documents' sections interleaved with no way to tell them apart. The namespace tree mirroring
the file tree is what makes a mount browsable at all.

## The data-loss cluster

Every sync data-loss incident in this project's history is one sentence:

> **An unverified KB render reached `write_file`.**

The `openspec` collapse, prose orphaning, layout ownership (seven guards over eight review
passes), `retire_undeclared_sections` retiring a neighbour's sections, and `jkb ns mv`
destroying a document were five causes of that one mechanism; an absent namespace, found in
the ninth pass, was a sixth. Nine review passes on the staging branch produced 24 must-fix
findings, all downstream of where the structure was stored. The guards and partial fixes
along the way are in History; this section is the model that held.

### The diagnosis: structure was stored in shared space

A file's document structure (its `##` headers, their order, its prose) sat in
`namespaces.metadata`, on namespaces derived from the file's path. The namespace tree is a
shared, globally addressable, user-mutable hierarchy; a file's structure is private to that
file, file-owned, and must round-trip exactly. The two have incompatible ownership, and the
mismatch is what produced bad renders: `jkb ns mv` or one click of the VS Code Rename button
moved the namespace, `namespace_for` recomputed the path from the *file*, the structure was
unreachable, and the export arm wrote a structureless render over the file. `jkb ns rm` was
the same story.

The structure was never hard to protect because the protection was subtle. It was hard to
protect because it was in the wrong place.

### Structure is file-owned

A synced file's sections, prose and block order can only be created, changed or removed by the
**file**. `apply_doc` writes them from the parsed disk document; `retire_undeclared_sections`
removes a section once the file stops declaring it. No CLI verb, MCP tool or UI action authors
them; `jkb task add --sync` adds an *item*. Items are KB-owned, and that exemption is the
feature.

The proof this needs is about where structure is read from, not about which code writes
certain keys. An earlier draft verified that only `apply_doc` and
`retire_undeclared_sections` wrote `header_line`, `sync_section` and `layout`: true, and the
wrong proposition, because `assemble_kb_doc` then derived structure from namespace existence
and paths, so anything that renamed, re-parented or deleted a namespace changed the structure
without touching those keys.

### A file's document lives on its journal row

`V012__sync_document_structure.sql` adds `sync_state.document`, one JSON object holding the
whole structure:

```
{"layout": [ {"section": "backend"} | {"item": "<local_id>"} | {"prose": "..."} ],
 "sections": [ {"path": "backend", "header_line": "## Backend"} ]}
```

`sync_state` is keyed `uri TEXT PRIMARY KEY`, so it holds at most one row per file and two
files sharing one structure is unrepresentable, whatever `namespace_for` does. `apply_doc` is
its only writer. `assemble_kb_doc` is *structure from the journal row, items from this file's
bindings*, and bindings are already file-specific (`binding::synced_uris_for_file`, which
escapes LIKE metacharacters and anchors on `<uri>#`).

Rejected: deriving structure by re-parsing the base blob on every reconcile. `reconcile`
already loads the journal row as its first act (`sync_state::get`), so a column on it is free;
deriving from the blob would cost a blob load and a full parse per file per sync, where a
settled file pays zero. It would also have given up `decide_direction`'s byte fast path and
`Outcome::Normalized`, which is what stops serializer skew costing forever. And it needed no
schema change, which is the next block's problem.

### Being a migration is load-bearing, so there is no dual write

Verified against refinery 0.9.2: `Runner::new` defaults `abort_missing: true`, and
`verify_migrations` checks every applied migration before applying anything, from `Db::open`,
failing with `MissingVersion` on one it does not know. So a binary older than `V012` cannot
open the database at all. Without a migration, a stale installed watcher would have kept
reading namespace metadata that nothing refreshed any more and exported from it: the collapse
mechanism resurrected by the absence of a migration.

An earlier draft kept writing namespace metadata "for one release, so a straggler binary
reads something correct". The reader that dual write protects does not exist, and what it
would do is maintain a second store of document structure, which is the root cause above. The
loud failure misdiagnoses itself, though: the user sees "migration V012 is missing from the
filesystem", not "your binary is older than this database". Worth a line in any release note.

### A new journal column lands in three hand-enumerated lists

A column is cheap to read and not free to add. `document` had to be added to
`sync_state::upsert`'s `before` snapshot (hand-written), to `undo::revert_sync_state`'s
`UPDATE` (hand-written; a review had already found that inverse restoring four of the row's
six fields), and to the `sys_sync` view. The view is the only read surface for the journal,
and omitting the column would make the thing that decides what a file looks like invisible;
SQLite cannot alter a view, so `V012` drops and recreates it. A test asserts that `jkb undo`
after a sync rewinds structure and hashes together.

### A legacy row is populated once, from the file's own bytes

A row with `document IS NULL` is populated once, immediately after `sync_state::get`, before
the quarantine early return (or a quarantined file would never be populated). The source is
the base blob, else the file on disk; both are per file by construction. Not the namespace
metadata: an earlier draft listed it first and called all three sources unambiguous. The
directory namespace a legacy store used is shared by every file in the directory, so reading
it is the `openspec` collapse restated as a migration; and what still had structure on a
directory namespace was precisely the set the old ownership gate had refused to adopt.

When bindings exist but the journal row does not, the structure is empty and the file is
treated as never synced, unconstrained. The primary key gives at most one row, not exactly
one: `tasks_mount_file` lets `jkb task add` bind a task to a file never reconciled, and
`revert_sync_state` deletes the row when the undone transaction created it. Both are
legitimate.

### `retire_undeclared_sections` is kept and re-keyed on `sync_section`

It clears the section markers (`sync_section`, and any leftover `header_line`, `position`,
`prose`) from a namespace under the file that the document no longer declares. The namespace
and its contents survive, since it may hold cancelled tasks, which are deliberate history; it
just stops being a section. Deleting the function while keeping section namespaces, as one
proposal did, accumulates a ghost namespace per header a file ever had.

It was gated on `if map.remove("header_line").is_none() { continue; }`. With structure on the
journal row no namespace carries `header_line`, so every iteration would `continue` and the
function would retire nothing, forever and invisibly, because nothing renders from namespaces
any more for a render test to catch. It is re-keyed on `sync_section`, which `apply_doc`
already writes, and tested by asserting the marker is gone, not the render. Its blast radius
is bounded by one file per namespace, and getting it wrong is now cosmetic rather than
destructive.

### An export can change item lines, never structure

`apply_doc` is the only writer of a file's structure, and it is reached only from
`finish_import` and the `Merged` arm. The `(false, true)` export arm does not call it, so an
export cannot change a file's structure. Two other paths reach `finish_export` and are not
covered by that argument:

| path | why it escapes |
| --- | --- |
| `missing_file` | `disk_changed` is never computed; reached when the file is gone, and on first sight of a file an export-only mount has never read |
| `three_way_resolve`'s `kb_wins` arm | reached only when both sides changed; it writes the journal's structure over a disk that changed structurally, which is what `kb_wins` means |

`kb_wins` is a shipped policy, so this is not obscure. The guards below are what cover both.

An earlier draft added "and `jkb undo`, since `namespaces` is in `UNDOABLE_TABLES`". That was
false: `ns::ensure` takes no `WriteMeta` and never changelogs, and `INVERSES` had no entry for
an update or delete on `namespaces`, so the listing was inert for this path. The other legs
were sufficient on their own.

### No bound item is silently dropped

`assemble_kb_doc` skips any bound item with no row or no primary placement, and an export
then writes that render over the file. That is a different harm from structure loss. It is
reachable through a documented verb: `placement::set_primary` logged the old primary's removal
as `op="delete"` on `placements`, which had no inverse, while the replacement was an
invertible `insert`, so `jkb undo` after a re-home deleted the new primary without restoring
the old, and the item's line vanished on the next export.

So every item the file declares whose binding still exists must appear in the assembled
document, or the export is refused: one set difference (`dropped_items`), not a constraint on
KB-owned state, since an item deleted in the KB loses its binding and is legitimately absent.
Expectation is judged from the **disk** document, not the base: an earlier version judged it
from the base, which made the refusal unclearable from the file. The check lives inside
`finish_export`, not at the `(false, true)` call site, or `missing_file` and `kb_wins` would
pass it; `missing_file` therefore loads the base document itself.

`Outcome::Refused` is journalled `needs_attention` with a reason naming the missing items,
never a bare counter. The engine computes the reason once and the printers render it rather
than restating the rule: a printer that restated it drifted, and told a refused file to
exclude a sibling that did not exist. The remedy is the one-command re-home
(`jkb task place <uid> <ns> --home`), or deleting those lines from the file if the items really
are meant to go. A refusal needs a way out or it is a wedge, and this codebase had shipped one
(a `needs_attention` flag for a deleted file that nothing could clear). Here any disk edit
imports normally, because `disk_changed` is computed from bytes against the base and never
consults the KB; `jkb doctor` names the file and says so. Asserted by a test, not claimed.

### Wholesale loss is one condition, judged on documents, above the direction dispatch

The condition (`wholesale_loss`): **the KB contributes zero items to a file that declares
some.** It compares two documents, never the store, because the store is what these
incidents damage: `jkb undo` of a sync deletes a file's items and their bindings together,
so `dropped_items`, which walks bindings, truthfully reports nothing dropped. One condition
covers undo, `jkb item rm`, a half-applied migration, an emptied binding table, and the next
thing with that shape. It is deliberately not a general "fewer items than disk" rule: on an
export-only mount the file is a projection, and hand-added lines are legitimately removed.

An empty rendered document is not proof of an empty store. `assemble_kb_doc` also omits an
item still bound but without a primary placement, and a `document` mount is one item per
file, so one dropped placement empties the render. So the condition asks the store too:
anything still bound means the `dropped_items` refusal and its re-home remedy, on every mount
mode; nothing bound means the items are really gone and re-reading the file is the recovery.
Without that split the guard turned a refusal into a silent import that overwrote content,
status and priority from disk.

Detecting it is not refusing it. It runs in `reconcile` above the direction dispatch, where it
dominates every arm, and the mount mode decides: a mount that can import **re-imports the
file** (the disk being the good copy is the condition's own premise), and only an export-only
mount, which cannot read the file back, refuses. There, deleting lines from the file destroys
the very thing being protected and does not clear the refusal, since the count comes from
disk; the remedy is to re-read the file. `finish_export` takes the `SyncDoc` and renders it
itself, so what was judged is necessarily what gets written. How this condition was first
fixed route by route, and then refused where it should have healed, is in History.

### First-sight export over an unimported file is refused

On any mount that can import, `export_blocker` refuses to export over a file that has content
on disk and no recorded structure. Before it, an export-only or first-sight path could
overwrite a hand-written file it had never read.

### The bytes about to be overwritten are archived first

`archive_current_bytes` runs in `reconcile_file`, above the direction dispatch, so it covers
all four sites that overwrite a synced file rather than the one (`kb_wins`) that used to carry
the rule. A failed archive stops that file instead of degrading to best effort.

### A data-loss test asserts the bytes on disk, and the sibling's

The assertion of record is the bytes on disk: every data-loss test reads the file back, and
reads its sibling back too. `report.count(Outcome::X)` may be asserted in addition, never
instead; a test that asserted a counter stayed green while the files it was about would have
been destroyed. A test must be shown to fail without its fix.

The required cases: two files in one directory, delete one, the survivor is byte-identical;
`jkb ns mv` on a file's namespace, the next sync writes nothing destructive; `jkb undo` after a
re-home, the line survives the next export; a legacy `document IS NULL` row is populated once
and the second sync is `UpToDate`; a legitimate KB-only edit (`task set --status done`)
exports normally; a merge deleting a section on disk merges and is not refused (the
union/layout divergence is not a false positive); `kb_wins` over a structurally changed disk,
asserting the bytes; and a `document` mount round-trips unchanged. For `document`, structure
is empty on both sides, and the assembly must not merge base items, or it would re-inject the
whole previous file content.

### The mount-mode axis is a test matrix that asserts both sides

Three consecutive review passes produced the same must-fix shape: this arm behaves differently
on an export-only mount and nothing tested that axis.
`no_mount_mode_and_stage_loses_a_task_line` runs {import, export, bidirectional} × {first
sight, settled, disk-changed, kb-changed, both-changed, post-undo, kb-emptied}. Its first
version asserted only that the file keeps its lines and passed the very bug it was written to
catch: a refusal protects the file perfectly while leaving the KB empty. So it also asserts
that a mount which can import is never left holding nothing for a file that declares work.
`kb-emptied` is distinct from `post-undo` on purpose: undo also clears `base_blob_hash` and
`document`, and with no base the disk's items read as additions that a merge keeps, so undo
alone never produces an item-less merged document. It asserts the harm, not the outcomes,
which legitimately differ per mode; pinning them would make it a change detector.

### Still open, filed not fixed

- **The import direction is unvalidated.** `tasks::parse_text` is lenient, so a truncated or
  partially written file imports cleanly, cancels every task below the cut, and writes the
  truncated render back. It is now the largest remaining data-loss path.
- **`jkb ns mv`/`ns rm` are unguarded on synced namespaces.** With structure off the namespace
  tree they can no longer destroy a document, but they still move an item's placement out from
  under a section.
- **`write_file` runs inside the open transaction.** A rollback leaves the file rewritten and
  the journal not.
- **Overlapping mounts over one directory are not forbidden.** Unexamined.

## The watcher and the filesystem boundary

`jkb sync --watch` (`watch.rs`, `notify`-based, debounced) runs unattended on the host and
writes files there, while a dev container can change the database through `jkb serve` and
write inside the host directories it binds. These decisions keep that safe and keep the
watcher's view of the database current. The container boundary itself is the
sandbox-and-container design.

### Sync never follows a symbolic link on a bound file's path

Decided in a stage-6.2 review, 2026-09-15: every read and write of a synced file goes through
`jkb_core::nofollow`, which walks the path from `/` a component at a time with `O_NOFOLLOW`,
refuses a link at the file or any directory above it, reads only a regular file (non-blocking,
so a FIFO cannot hang the watcher), and writes a temporary file beside the target and
`renameat`s it into place, which replaces whatever is at the name rather than writing through
it and keeps an existing file's mode.

Why: the host watcher writes a bound file whenever the knowledge base changes, and a container
can make those changes. With plain `std::fs::write`, replacing a bound `tasks.md` under
`~/repos` with a link to `~/.zshrc` and then editing the task turned the next sync into the
container writing to the host user's shell startup file. `jkb_api::tasks::FileRoots` could
not stop it: it judges a path's spelling, and a check followed by a write races a link swapped
in between. Walking by file descriptor is the check and the use in one.

What it costs: a synced path may not contain a link at all. `jkb mount create` stores a mount's
directory canonical, so real mounts qualify; a test that mounts a raw temp directory must
canonicalize it first (on macOS `/var` is a link). Pinned by `jkb-core`'s `nofollow` tests (a
link at the file, above it, dangling; a FIFO; mode kept; the umask, in a child process) and by
`sync_never_writes_through_a_symlink_planted_at_a_bound_file`, which fails when the engine goes
back to following links. Not verified on macOS; the tests ran on Linux.

### The other ways in are closed the same way

From a third review: a read is refused past 64 MiB (`nofollow::MAX_READ_BYTES`), because a
sparse file planted at a bound path costs the container nothing and allocated its whole
apparent size on the host. The watcher is built with `with_follow_symlinks(false)`, because a
directory link planted in a mount made the recursive watch subscribe to the host directories
behind it. A watch event for an unbound path reached through a link below the mount is
dropped. Sync never takes up `nofollow::write`'s own temporary files, which an interrupted
write leaves behind and a full sync would otherwise import as a second copy of every task. A
new file is created under the user's umask; an existing file keeps its mode.

### A bound file behind a link stays flagged

A bound file reached through a link is not skipped at discovery: it is reconciled, `nofollow`
refuses it, and its journal row stays `needs_attention` naming the link. Skipping it, as the
third review's fix did, let the full sync's out-of-scope sweep settle the row to `ok`, so the
file stopped syncing with `jkb doctor` reporting nothing (found in a fourth review). A size or
type refusal carries no link remedy and names the 64 MiB limit. An unbound file flagged on its
first sync (a quarantine, so no bindings) whose directory is then replaced by a link is settled
by the out-of-scope sweep (a fifth review): `discover` never walks into the link, so kept in
scope its flag could never be reconciled or cleared. Pinned by
`a_bound_file_reached_through_a_link_stays_flagged_naming_the_link` and
`a_flagged_unbound_file_behind_a_link_is_settled`.

### The watcher exports a change made in the database

Decided 2026-09-15, after the user saw it on the host: after each iteration of its loop, at
most once per debounce, each mount's watcher asks the changelog what was written past its last
look (`sync_state::writes_since`). When anyone but sync itself (`sync_state::SYNC_ACTOR`) has
written, a pass is owed, and it runs at most once per three debounces (`DatabaseWrites`, which
keeps a write that lands between passes owed); spacing is measured from when the last pass
ended. The pass reconciles the bound files whose KB render no longer hashes to their
last-synced hash (`engine::sync_kb_changes`). It is read-only to decide, one short read per
file so writers interleave, so a write elsewhere costs a render per bound file, not an archive
and a transaction each. An import-only mount is skipped; a `conflict` and a quarantined parse
failure are left to the disk.

Why: the watcher heard only the filesystem, so a task edited through `jkb`, on the host or by
a container through `jkb serve`, stayed out of its file until that file changed on disk or the
watcher restarted. The user edited a task in a change's `tasks.md` through `jkb`; the database
had it and the file never did. Two things hid it. On Linux, notify's inotify backend reports
opens, so each reconcile's own read of a file raised an event that reconciled it again at
every debounce: a loop that exported database edits by accident, in every Linux test, while
macOS's FSEvents carry no opens, so the Mac showed the real behaviour. And counting those
reads as activity let any reader faster than the debounce (an editor, a grep, a test polling
for the result) keep the loop from ever going idle. So read-only events (open, read,
close-after-read) are dropped in the watcher's callback, and one burst of events is coalesced
for at most ten debounces.

A first review added what the first version missed: asked only on an idle tick, file churn
under a mount (a build, git, a task worktree) held the export off for as long as it lasted,
and so did an unbounded drain of that churn; unspaced, a fleet writing anywhere made every
mount render every bound file on each tick.

Sync's own writes do not count, or every pass would trigger the next. What that leaves: a
mount's `ensure_all_mirrors` (a sync write) that changes another mount's render is not seen
until that mount's next pass.

### A flagged file is re-judged only when its remedy could have landed

Files are judged by `FileState::from_journal`: a never-synced file is reconciled. A `Blocked`
file (a refusal, whose remedy is a database write, or a failure) and a file whose check fails
are reconciled when what they are judged on differs from the watcher's record of the last time
(`FlaggedJudgements`): the render's hash or the error, each bound item with whether it has a
primary placement, and the mount's direction. So a standing flag costs no reconcile and no log
line per pass (second review), yet clears when any remedy lands: restoring the placement,
unbinding the item, switching the mount (third review; keyed on the render alone, the last two
never cleared). Only a file the pass left flagged keeps an entry. And `sync_state::upsert`
writes nothing when the row would not change, so no route, the backed-off full-sync retry
included, leaves a changelog row per re-flag of a standing flag.

Pinned by `writes_since_counts_what_others_wrote_and_passes_sync_s_own`,
`a_database_edit_is_exported_to_its_file_and_to_no_other`,
`the_watcher_exports_a_database_edit_without_a_file_event` (reading the file faster than the
debounce, and writing another file under the mount faster still, while it waits),
`a_read_only_access_is_not_a_change`,
`a_database_pass_follows_the_mount_s_direction_and_writes_only_as_sync`,
`a_refused_file_is_re_judged_after_its_database_remedy` (and not before),
`a_file_whose_check_fails_is_flagged_once_and_does_not_stop_the_pass`,
`every_remedy_is_re_judged_and_a_settled_file_holds_no_judgement`,
`an_unchanged_row_is_not_rewritten` and
`database_passes_are_spaced_and_a_write_between_them_is_kept`. Untested: a mount-direction
switch as the remedy (the direction is in the key). Not verified on macOS.

## Deferred

### Not built yet

The `spec` serializer (OpenSpec `spec.md` ⇄ requirement and scenario items); remote bindings
(`https://`, `git://`); a closure table; and a multi-version base history (a single
last-synced base blob per file is enough for the three-way merge). A future KB-authoring
serializer such as `spec` would need the KB to create sections. That does not reopen the
namespace tree: it means the KB writes the per-file `sync_state.document`, which is still
private to one file and still not shared space.

## History

Superseded and reversed decisions, with what replaced them and why. Most of them are steps of
one search: the guards and partial fixes that preceded storing a file's structure on its
journal row, kept because each wrong turn names a trap the current model still has to avoid.

### Hash-only conflict detection

The first sync shipped one serializer, `document`, and decided direction by comparing the disk
hash and the KB render hash against `bindings.last_synced_hash`. Replaced by the journal and
the three-way merge, because a multi-item file can be edited on both sides at once, and
because `tasks` rendering is not byte-identical to its input, so hash comparison would have
read "KB changed" forever.

### Prose as `text` items

The first `tasks` serializer kept prose, the legend and blank lines as ordered `kind='text'`
items with a content-hash identity (`text-<b3:8>` plus an occurrence counter, disambiguated
against the task ids because blank lines recur and collided on `items.uid UNIQUE`). The
identity could not survive an edit, so old prose items orphaned. An orphan stayed placed in
its section namespace, `assemble_kb_doc` emitted a `##` header for every namespace carrying
`header_line` whether or not the file still declared it, and the KB render permanently
disagreed with the disk: `kb_changed` stuck true, every disk-only edit resolved as a
both-changed conflict, and the stale header was written back over it (the
`memory/sync-export-wins` incident). Replaced by prose as inline layout blocks. Legacy `text`
items migrated lazily: the new parser emits none, so each file's next import cancelled and
detached them, and `render` still emitted a non-task item verbatim so an unmigrated KB
round-tripped without losing lines.

### Document order from three position sequences

Order was reconstructed by merging section, item and prose positions written at different
times. Replaced by the single `layout`, after a header rendered into the middle of an item
twice on a real file.

### The directory-derived namespace and the `openspec` collapse

`namespace_for` once dropped the filename, so every file in a directory shared one namespace,
and with it the `layout` that `render` treated as the sole authority on order. Items were
correctly per file; structure was not, so each file rendered whichever sibling last wrote the
shared layout. A second defect lined up with it: `mount create` was a full-row replace that
doubled as the update command, so re-running it to change the conflict policy without
repeating `--include` wrote NULL over the `**/tasks.md` glob, and the next sync discovered the
whole tree. Together they overwrote 62 of 63 files under `openspec/`: in each change folder
`design.md`, `proposal.md` and `.openspec.yaml` were left byte-identical, with every markdown
header stripped. Fixed by `mount create` preserving unnamed fields, `jkb sync --conflict`, and
at the root by the filename in the namespace and then structure on the journal row.

### `Outcome::Collided` and the seven ownership guards

The first guard, `Outcome::Collided` via `colliding_paths`, refused any `tasks` file sharing a
namespace with another synced file (gated on `requires_exclusive_namespace()`), checking both
the batch and the bindings in the KB. It was correct; its recovery path was not. Deleting the
sibling, the remedy it recommended, released the survivor while the namespace still held the
sibling's layout, and the next sync exported that over the survivor. Eight review passes added
seven guards, each a proxy for authorship: apply the globs when collecting siblings (the
collision could never be cleared), detect the released survivor (covered only the case it was
shown), stamp the layout with its author (`layout_uri`; every pre-existing database had no
stamp), treat an unstamped layout as foreign (refused forever) or as import-only (silently
reverted KB edits), claim it when the journal says `ok` (a file at `conflict` fell through),
claim it when no sibling is bound (which is what deleting the sibling causes). Every proxy is
true in some case where the real answer is the other file: on a legacy database the two files
are indistinguishable claimants, because authorship was never written down. A ninth, better
proxy (claim only when the KB render already equals the base) was rejected as still an
inference, and one that gives up on any file with unexported edits. Replaced by one file per
namespace, and all of it deleted.

### Adopting legacy directory namespaces

Introducing the filename segment stranded every existing mount: measured on the real database,
17 mounts, all `tasks`, all with directory-derived namespaces. A global schema migration was
rejected (it would have had to answer "which file owns this namespace" in SQL, for every
mount at once, at `Db::open`, with no way to report or skip a case). Instead
`adopt_legacy_namespace` moved one file's structure, sections and placements down a level
inside its own reconcile, gated on every `file://`-bound item in the legacy namespace being
bound to this file. The gate was vacuous: it inspected only items placed directly in the
directory namespace, and a sectioned file has none there. Both of the ninth pass's sync
must-fixes were in this code, including one where an absent namespace assembled an empty
structure that was then exported over the file. Deleted with `legacy_items_are_all_ours`,
`binding_is_ours`, `DOCUMENT_METADATA_KEYS` and `Outcome::Adopted` once structure moved to the
journal row: there is nothing to adopt.

### Reverting per-file namespaces and checking the render for faithfulness

After the ninth pass one design proposed reverting `namespace_for` to directory-derived,
restoring `Collided`, and checking at assembly that the KB document was "faithful" to the base
(same structure with item blocks removed, same header lines, no bound item dropped), refusing
otherwise. The reasoning it recorded still holds: the check's verdict meant "safe to write",
not "equal", since `render` drops unmatched blocks and appends unlisted items; it gated only
the KB-derived write sites, because gating the import write-back would stop a refused file
healing and gating the merge would refuse merges that incorporate disk structure by design;
and comparing header-bearing namespace sets would have fired on states the engine itself
manufactures (the merge's union of sections, and `slug` changes for non-ASCII headers).
Superseded because it protected the output of a broken model instead of fixing the model, and
because reverting the filename segment would have re-widened `retire_undeclared_sections` to
neighbouring files. Its "no bound item silently dropped" rule survived as `dropped_items`.

It also rejected an automatic repair of the KB from the base blob, and that lesson stands:
repair would have been the third consecutive fix that added a writer here, and was
irreversible. `ns::set_metadata` changelogged with `before: None`, `ns::ensure` never
changelogged, and the old primary placement's removal had no inverse, so `jkb undo` after a
repair left an item with no primary placement, which assembly dropped: the harm being
prevented, reached through a documented verb by the mechanism meant to prevent it. Automatic
re-import from disk was rejected too, since `apply_doc` reverts every KB-side edit since the
last sync.

### Deriving structure from the base blob

A later design moved structure out of the namespace tree by deriving it from each file's base
blob on every reconcile, with no schema change, and deleting the faithfulness check. Its
diagnosis (structure is file-owned and was held in shared space) is the one that stands.
Replaced by the journal column for cost (a blob load and parse per file per sync), for keeping
the byte fast path and `Normalized`, and because the absence of a migration would have let a
stale watcher keep exporting from namespace metadata.

### Wholesale loss, fixed route by route and then refused

The routes structure-on-the-journal left open were each found and fixed at the route: pass 21
at `finish_export`'s `(false, true)` arm, pass 22 at `three_way_resolve`'s import-forbidden
arm. Both fixes were correct and neither was the last, because a route is not a cause. Pass 23
found the next shape: sited inside `export_blocker` the only available answer was "refuse",
which protected the file and left the KB permanently empty on two of three mount modes, since
a refusal never advances the base and the message's own remedy ("edit the file") routed back
into the same arm. Replaced by the one condition above the direction dispatch, where the mount
mode decides between re-import and refusal.

### Bound files behind a link skipped at discovery

The third filesystem-boundary review skipped a bound file reached through a link when
discovering files. The out-of-scope sweep then settled its row to `ok` and the file silently
stopped syncing. Replaced by reconciling it and keeping it flagged.

### The watcher that heard only the filesystem

The watcher reconciled only on file events, so database edits reached a file by accident on
Linux (inotify reporting the watcher's own opens) and never on macOS. Replaced by the
changelog-driven database pass.

### Task-lifecycle fixes recorded with the staging branch

The design of record for this cluster also carried three non-sync fixes from the same review
pass, since reworked by the task-lifecycle design. `close_merged_row` picked the
lexicographically smallest `branch=` tag (`tags.iter().find` over `tag::applications`, ordered
`facet, value`), so a task could close while another branch was in flight; the fix considered
every recorded branch, kept the reading that a missing branch blocks closing (auto-closing on
ambiguity is the one thing that verb must not do) with a message naming `jkb task tag rm`, and
rejected a `facet_sole` that errors, which would have aborted `close-merged` for every task
inside a hook that swallows errors. `settle_landing`'s claim compare-and-swap was inert on the
main path (a terminal `set_status` clears the claim) but live on the already-terminal early
return, and `land_preflight` never checked who held the claim, recorded as open.
`cmd_task_abandon`'s compare-and-swap was extended so a claim taken during the git
subprocesses is neither reopened over nor has its `onto=` cleared.
