<!-- generated from jkb design design:namespaces-18dcd5bda1dce798858c69, edit there (version 66.QYCbtJ2e474NAYGhk6zryPEBAf-4_azcsMoFAYnEiZzUubsOgPECip3usezp-w8BiZSAmbL_7g0BjOmAk7GMwAMBjtWo9_nAuw8Bj4GL_ufeiwEBkLGb8_3CnAwBkvjUhJDTqQEBlJHuxo6sjwQBmPv2l67ksAMBqefihKOnuA8Brd-MganIvwkBrtevofrfywcBrr-WoazRlQUBsJKU_8qh2QUBscO73MKTxgwBsqi29p_U9gIBsrC52vi-jQsBu_rpp-CkwgcBvI7Li7GJtQ0BwO6rtf_TyAEBwrir0PbCswMBwuuKl7_iVwHE8cbi9u8bAcTKj82RoOwCAcbC74-ix_wOAcODlOKRp3YByJHZ_p7PnwcBysv7reaPngkBytbB0qD_hwsBzLfbwJPKyQMBy5OFiqqK0gUBzrzgqPeVnwIB0ZGdpavwug4B05X6s6Or8QIB1sLnqtDruQIB2fy-lumW0AQB2uuaj52S3QoB2rWdo-rbng4B3J-tvPXp8wsB2vqrs-fdyQQB4IjM7K75pA4B4JGG0NyujAsB45n9ourfgAEB45vW0ZOT2wcB5Nv7yK64jAcB6tXa-bvpjQsB7dupluG9JgHu8NvqndOwBQHu1K7B3rp8AfLLweHIxcAOAfKNwc6v6OgIAfbvwcWV4a8GAffut-zDw4MNAfjQ35S4ydQPAfni4Oum0MIGAfqnp6v_xI4PAffUusnOktsFAffUufPww7IFAfn1x_PYjfkFAfqsqL_o5tAHAf_ioaCM6_kJAQ, blake3 487f6d6ef03c9b241248d2b768aeeffd83bc2d422750dc8876d0eb3ee5583553) -->
# Namespaces

jkb is one global database (`~/.jkb/jkb.db`) meant to span every repo and project, to serve
as long-term memory for LLM work, and to be a surface people and agents share. This design
records how that one store is laid out and what each part of the layout guarantees: the
global layout, task homing, typed namespaces, and the investigations that live in `memory/`.

Two facts from the storage model run under all of it. Namespaces are *logical*: a namespace
path is where an item is filed, not where its bytes live. The second axis, the **binding**,
says whether an item is `managed:` (it exists only in the KB) or bound to a `file://` URI and
round-tripped to disk by a serializer. Every decision below keeps the two axes independent.
How files map onto namespaces, and what keeps file sync from losing data, is the file-sync
design; the storage substrate itself is the foundation design.

## The global layout

The layout was set in an audit and design session on 2026-07-24. The audit found the store
repo-specific at the global top level (see History): a second repo would have collided with
the first. Only `tasks/`, already partitioned per repo by task homing, was organized for a
global tool, and nothing put new data in a sensible place automatically. The decisions below
generalize the one pattern that already scaled.

### Repo content lives under `repos/<repo>/…`

A repo's file-synced content (its `openspec/`, its code-review logs, source-derived
documents) homes under a per-repo root, `repos/<repo>/<subtree>`: `repos/jkb/openspec`,
`repos/jkb/codereviews`, and a second repo's `repos/<other>/openspec` never collides with
them. `<repo>` is the repo's namespace key. With the repo's task partition, `ns:repos/jkb/**`
is everything about jkb. A subtree under `repos/` may be a `file://` mount or `managed:`; the
root says nothing about the binding axis.

Rationale: the per-repo task tree was the one part of the old layout the audit showed already
scaled past one repo. Generalizing it to all repo content costs one path segment and removes
the collision class outright.

### Global knowledge lives in semantic top-level roots

Knowledge tied to no repo lives in roots named for what it is: `media/` (ingested media and
transcripts), `references/` (external documents, papers, web captures), `memory/`
(investigations, below), `tasks/` (the task DAG, partitioned per repo) and `_sys/` (saved
views and the marker namespaces that surface system tables).

Rationale: this keeps the "one KB for everything" goal without coupling global knowledge to
any repo. A paper read while working in one repo is still a paper, and should be found from
every other.

### The reserved roots are a fixed list of declared special cases

The reserved top-level roots are exactly `repos`, `tasks`, `media`, `references`, `memory`
and `_sys`. Each is a declared special case of this layout, named as a literal where code
needs it (`task::DEFAULT_ROOT` is the string `"tasks"`). None is found by searching for a
property such as a namespace type; the typed-namespaces section records why that was tried
and removed.

### Placement is automatic

Users and agents should not have to pick a namespace; the sensible default is applied.
Mounting a repo directory puts it under `repos/<repo>/…`: today a convention the operator
follows, since `jkb mount` takes the namespace as an argument, with deriving `<repo>` from the
git root a tracked follow-up. Ingesting inside a mounted repo inherits that root through
ambient scoping (the cwd's covering mount supplies the namespace, so no `--ns` is needed;
confirmed against a mounted repo when the layout shipped). External content ingested outside
any repo lands in a semantic root by type. Tasks and investigations home per their own rules
below.

Rejected: a bespoke placement router. Convention plus the ambient scoping the CLI already
does keeps the CLI the single source of truth for where things go.

### Moving a namespace never moves a file

`jkb ns mv` re-homes a namespace without touching disk. `ns::move_subtree` relocates the
whole subtree; a `mounts` row is keyed by `namespace_id`, which is stable across a path
change, and item bindings are `file://` disk paths, not namespace paths. So mounts stay
attached and files are untouched; only logical paths change, and ambient resolution from the
mounted directories yields the new path.

That is what made the layout migration safe. On 2026-07-24, after a backup, `openspec` moved
to `repos/jkb/openspec` and `codereviews` to `repos/jkb/codereviews`, and the stray `<ns>`
namespace was removed with its placements. Checked afterwards: `jkb mount ls` showed both
mounts attached, bindings and files were unchanged, and `jkb sync` was a no-op.

### `jkb mount ls`, `jkb ns mk` and `jkb ns rm`

The CLI could not list mounts, a gap for audits and the UI. `jkb mount ls` (human or
`--json`) lists each mount as namespace, serializer, backing directory and sync mode, over a
`mount::all` core read. `jkb ns mk <path>…` creates namespaces idempotently and is the only
way to make an empty one (others arise from placements and mounts); fresh-machine setup uses
it to scaffold `repos tasks media references memory`. `jkb ns rm` removes an empty namespace.

## Task homing

Task homing decides where a task lives logically. Its shape was settled with the operator up
front (a global `tasks/` root keyed by repo, binding chosen per task, a per-repo inbox
mirrored into the global inbox) and its four open questions were closed in a design pass on
2026-07-06.

### A task's first placement is its home

A task is placed in its home namespace as the **Primary** placement and in any others as
**Reference** mirrors. In quick-add, the first `+<ns>` (or an explicit home argument) is the
home and every later `+<ns>` a mirror. `tasks/inbox` (`task::DEFAULT_HOME`) is used only when
no target is given and there is no ambient repo: a genuine fallback, not a forced home.

`task::create` already honoured home versus mirrors; only `NewTask::from_quick_add`'s default
wiring changed. Unit tests pin the cases: `+a` alone homes at `a` with no inbox placement,
`+a +b` homes at `a` and mirrors `b`, no `+` homes at `tasks/inbox`, each asserting Primary
and Reference roles. This is the load-bearing change; the rest of this section is convention
and CLI sugar over it. The new meaning of `+<ns>` was documented as a behaviour change, not
treated as a break: plain `jkb task add "x"` outside a repo behaves as before.

### One global `tasks/` root, partitioned by repo

Every task bucket lives under one `tasks/` subtree, so `ns:tasks/**` is the single query for
every task everywhere, while each repo keeps its own partition:

```
tasks/
  inbox/                 # global cross-repo un-triaged capture (the fallback home)
  .backlog/              # global backlog, only by explicit confirmation
  <repo>/
    inbox/               # per-repo un-triaged inbox; mirrored into tasks/inbox
    .backlog/            # per-repo triaged, unscheduled work; not mirrored globally
```

Rejected: a per-repo tree under the repo's own mount namespace (a bare top-level `.backlog`,
say). It scatters task state across the VFS and mixes it with repo content, so `ns:tasks/**`
no longer finds everything.

### The repo key is the full mount namespace

`<repo>` in `tasks/<repo>/…` is the **full** namespace path of the mount covering the cwd,
derived at runtime from `mount::ambient_namespace(cwd)`, never hard-coded. A mount at
`repos/jkb` homes backlog tasks at `tasks/repos/jkb/.backlog`.

Rejected: the mount's leaf segment (`tasks/jkb/.backlog`): shorter, but two mounts with the
same leaf collide. A repo that wants short task paths can be mounted at a short namespace.
Wherever this record writes `tasks/<repo>/…`, read `<repo>` as the full mount namespace.

### The per-repo inbox is mirrored into the global inbox

Capturing inside a repo with no explicit target homes the task at `tasks/<repo>/inbox`
(Primary) and adds a `tasks/inbox` Reference mirror. The per-repo inbox is the task's real
home, and the global inbox stays a complete cross-repo capture view: nothing captured anywhere
is invisible from the top.

The backlog is deliberately **not** mirrored globally. A backlog is inherently per-repo, and
a global backlog view is one query away (`ns:tasks/*/.backlog`).

### `--backlog` homes a task in its repo's backlog, and asks outside one

`jkb task add --backlog "…"` homes the task at `tasks/<repo>/.backlog` (Primary, no global
mirror): pure sugar so the path is never hand-typed, with `+<ns>` remaining the general
escape hatch. Outside any mounted repo there is no repo to key by, so `--backlog` asks on
stdin (`y/N`) before homing the task at a global `tasks/.backlog`, and aborts if declined.
When stdin is not a TTY (piped, headless, an agent's shell) that counts as declined and the
command errors with an actionable message.

Rationale: per-repo backlogs stay the norm while a deliberate global capture is still
possible. Rejected: a silent global fallback, which would quietly collect an agent's backlog
tasks where no repo-scoped query finds them.

### Binding is chosen per task, independently of the home

Homing decides *where a task lives logically*; binding decides *whether its bytes round-trip
to a file*. These are the store's two addressing axes and they stay independent: a task is
`managed:` or bound to a `file://` URI and synced by a serializer, and that choice belongs to
the task, not its home. This repo runs both: some backlog tasks are `managed:` planning notes
that never leave the KB, others sync into an on-disk `.backlog/` mount through the `tasks`
serializer so they appear in the repo.

### Binding is inferred from the home's mount, with flags to override

The default is `managed:`. If the home namespace is covered by a `file://` mount using the
`tasks` serializer, the default becomes a synced binding into that mount's file. `--sync` and
`--managed` on `jkb task add` always override, and `--sync` with no covering `tasks` mount is
an error.

The inference is `jkb_sync::tasks_mount_file(db, home_ns)`: it walks the home namespace up
its ancestry to the first mount. A `tasks` mount yields the bare URI of the mount's root
tasks file, `file://<backing_dir>/tasks.md`, and the task binds to `<bare>#<local_id>` (the
task's uid slug). Any other mount stops the walk rather than crossing its boundary, and the
task stays `managed:`. The synced task is created bound but unwritten; `jkb sync` exports it,
and the CLI prints a hint saying so.

### A synced task binds to the mount's root `tasks.md`

Because the file is the mount's root `tasks.md`, a synced task round-trips cleanly when it is
**homed at the mount namespace itself**, and is written at the file's root. A home deeper than
the mount renders as a `##` section whose header namespace has no `header_line` metadata, so
that section's round trip is not guaranteed. The working rule is to mount at the exact task
namespace. Mounting at the repo root, so `--sync` and `--backlog` resolve from anywhere in the
repo, is a separate follow-up.

### A bound file is exported even before it has a sync journal

The sync engine's `reconcile` used to export a missing file only when a sync journal for it
already existed, so a binding freshly created in the KB (`task add --sync`) was skipped and
never written. Synced-binding inference exposed it. The rule now: export whenever the mount
exports and the KB has items bound to the file, journal or not. That also covers a
previously-synced file deleted on disk.

### Task reads default to the ambient repo

`jkb task next` and unscoped task queries default to `tasks/<repo>/**` when the cwd is inside
a mounted repo, and to the whole `tasks/**` tree otherwise; `--global` forces the whole tree.
This follows the CLI's ambient-scoping convention for every other read.

## Typed namespaces

A namespace may carry a **type**. The investigations work introduced the mechanism; the
typed-namespaces retrofit turned it from a hint honoured only by the investigation engine into
a guarantee every writer is held to, and applied it to the namespaces that had been
special-cased around literal path strings (`tasks/`, `_sys/views`, `_sys/sync`,
`_sys/transactions`, `_sys/ingestions`).

### A type lives in namespace metadata, is inherited, and resolves to a static descriptor

A namespace's type is the `type` key of `namespaces.metadata` (`ns::TYPE_KEY`), written by
`ns::set_type`, which merges and never clobbers other keys. `ns::effective_type` resolves the
nearest typed ancestor, so a sub-namespace of an investigation resolves the same strategy and
everything under `tasks/` is held to the `tasks` contract. Untyped namespaces resolve to no
descriptor and behave exactly as before.

`nstype::resolve(name)` mirrors the sync serializers' `resolve`: each type is a `static`
descriptor, `resolve` is one `match`, `AVAILABLE` lists every name, and an unknown name is
refused with that list. Adding a type is one static, one match arm and one list entry.
`STRATEGIES`, the subset `jkb inv new` accepts (`debugging`, `conjecture-attack`), is split
from `AVAILABLE` so a contract type is never offered as a strategy. Behaviour is data where it
can be: verbs are `VerbSpec` values, so a strategy gains a verb with no engine or CLI change.

### A type has one of two roles

`NamespaceType` began as an investigation-strategy descriptor: `goal_predicate` was required,
and `frontier`, `ranking`, `resolution_rollup` and `verbs` are investigation concepts. A
`tasks` type has none of them, and a stub `goal_predicate` would be a lie the CLI then had to
special-case. So the trait keeps one name and gains `TypeRole::{Investigation, Contract}`: an
investigation type coordinates (verbs, frontier, ranking, acceptance predicate); a contract
type constrains what may live in the namespace, and nothing more.

`role()` defaults to `Investigation`, so the strategies were untouched. `edge_types()`,
`verbs()` and `acceptance_presets()` default to empty. `goal_predicate()` defaults to an error
saying "`<name>` is a contract type, it has no acceptance predicate" rather than a fabricated
verdict; both strategies override it, so the default is reachable only where it is true.

Rejected: a separate `InvestigationStrategy` sub-trait behind `fn strategy(&self) ->
Option<&dyn …>`. Cleaner on paper, but it churns every call site in the investigation engine
and the CLI for no behavioural difference (both designs must still refuse `jkb inv verbs` on
a contract namespace), and breaks "one static, one match arm, one list entry".

### A contract type accepts exactly its kinds

`base_kinds()` is a trait method. Strategies get `INVESTIGATION_KINDS`: the four base roles
(`goal`, `node`, `artifact`, `reflection`) **plus `task`**, because tasks legitimately live in
an investigation namespace, which is what lets `is:frontier` generalize `is:ready` there. A
contract overrides `base_kinds()` to nothing, so `accepts_kind` is exact: the `tasks` type
accepts `task` and only `task`.

### The contract is enforced where an item enters a namespace

`nstype::check_placement(conn, item, namespace)` runs inside `placement::place`, the single
choke point through which an item enters a namespace (`set_primary` calls it; nothing else
writes `placements`). It resolves the effective type (untyped means no check), reads the
item's `kind`, and refuses a kind the type does not accept, naming the namespace, the type,
the offending kind and the accepted set.

Because the check is in the callee, it binds every writer: `task::create`, the sync engine,
the ingest pipeline, any future serializer. A test pins it: a raw `item::upsert` plus
`placement::place` into `tasks/**` is refused, an untyped namespace still accepts anything,
and the failing `place` rolls its transaction back; CLI tests check it on the ingest path.
Accepted behaviour change: placing a wrong-kind item into `tasks/**`, `_sys/views` or a `_sys`
marker now fails where it used to succeed.

### Resolving a namespace's type is one query

The placement check runs on every placement, and ingest places one item per chunk, so
`ns::effective_type` is on a hot path. It is one query over the ancestor paths (computed in
Rust, matched with `IN (…)`, deepest match wins), strictly fewer queries than before at every
existing call site too. Its `json_extract` is guarded by `json_valid`, so non-JSON metadata
cannot make it raise.

### Three contract types

| type | applied to | accepts |
|---|---|---|
| `tasks` | the `tasks` root (and so its subtree) | `task` only |
| `views` | `_sys/views` | `view` only |
| `journal` | `_sys/sync`, `_sys/transactions`, `_sys/ingestions` | nothing |

`journal` covers all three system marker namespaces rather than minting a `sync` type and two
identical siblings: they exist to surface a system table in the VFS and hold no items, which
is one contract, stated once. "Accepts nothing" is a real guarantee: nothing can file an item
into a namespace whose contents a migration owns.

### Reserved roots carry their contract automatically

A type nobody applies is a type nobody has, so `nstype::RESERVED_TYPES` declares each reserved
path's contract, applied from two directions. Migration `V008` back-fills existing databases
with `json_set` on `namespaces.metadata`, guarded by `json_extract(metadata,'$.type') IS NULL`
so a hand-set type is never clobbered. `ns::ensure` stamps a reserved path as it creates it,
so a fresh database and a later `jkb ns mk tasks` both get the type; that stamp has no
changelog entry, matching how `V001` seeds the `_sys` namespaces (schema, not a user edit).
`V008` raised the schema version, so a binary built before it refuses the shared database
until rebuilt.

### A type is not a location marker

A type says what may live in a namespace, never where a subsystem's root is. `RESERVED_TYPES`
is deliberately **one-way**: a reserved root is told its contract, and nothing searches for a
contract to find a root. `tasks/` is the tasks root because the layout reserves it, and
`task::DEFAULT_ROOT` is a literal; the rule is documented on the const so the inverse is not
re-added. A contract ("only tasks live here") is naturally many-to-many (`journal` types three
namespaces); a location ("where the task surface points") is singular. Resolving a root from a
type was built and removed; History records what it cost.

### `jkb ns type`, and `jkb inv` refusing a contract namespace

`jkb ns type [path] [type] [--list] [--clear]` shows a namespace's own type *and* the one it
inherits with the ancestor it comes from, so "why is this enforced here?" needs no tree walk.
A path that does not exist is an error, not "untyped". Given a type it sets one; `--clear`
removes one (`ns::clear_type`, for a namespace typed by mistake); `--list` groups types by
role. `jkb inv new`, `verbs` and `kinds` refuse a contract namespace with an error naming the
type instead of an empty verb list, and `resolve_strategy` reports an unknown name against
`STRATEGIES`, not `AVAILABLE`. CLI tests cover show, set, list, clear, the contract on ingest,
several namespaces carrying one contract while relocating nothing, and missing versus untyped.

### What the type system deliberately does not do

- **Validate edges.** A type's `edge_types()` stays advisory, because an investigation must
  always be able to record an association it has no vocabulary for yet (the over-structuring
  pitfall, below).
- **Dispatch commands per type beyond `jkb inv do`.** `jkb task` already is the task surface,
  keyed by item kind; routing it through a descriptor adds a layer with no new capability.
- **Validate status lifecycles.** `TaskStatus::from_manual_str` at the string boundary and
  the `V006` CHECK already enforce them; a third copy is a third place to disagree.

## Investigations in `memory/`

`memory/` holds **investigations**: open-ended, multi-agent knowledge work whose state
outlives any one context, such as debugging a heisenbug, hunting a counterexample, or
searching for a proof. The bet is the one jkb was founded on: coordination lives in the
*store* (items and typed edges), never in agent chat, so a fresh agent picks up where the last
stopped without re-treading its dead ends. The design was approved in a human design pass on
2026-07-29 and built in full, through a dogfood run. The task swarm's claim, DAG and
merge-queue pipeline remains the strategy for shipping code; investigations are the general
case.

### An investigation is a typed graph read back as three buckets and a digest

An investigation is a typed namespace holding a typed, scored graph of knowledge and work
units. A fresh agent resumes it by reading:

- **Frontier** — open units with no unresolved blocker, ranked by promise. The work queue.
- **Confirmed core** — settled results: the current best model.
- **Tombstones** — dead ends, ruled-out regions and refutations, **each linked by an edge to
  what killed it**. The anti-retread set: without it, fresh agents re-tread.
- **Digest** — one `reflection` unit rendering the three buckets at a glance.

The units record a domain-agnostic loop: orient, hypothesize, design a test, execute, observe,
update belief, narrow, confirm or refute, spawn or close. An investigation ends on a **goal
predicate** (its acceptance test), never a timer; "keep going" means leaving the frontier
non-empty and ranked.

Rationale: a survey of systems that made real progress on hard problems (linked from History)
converged on this substrate: memory is an external typed, scored graph; ready work is derived
from a blocking graph; dead ends are first-class and retained; belief moves with provenance;
synthesis nodes are retrievable. One of them, Beads, independently reinvented jkb's own bet: a
DB-backed item and edge store with a derived `ready` frontier.

### A thin shared core plus a pluggable strategy per namespace type

The surveyed systems agree on the substrate and differ on two things: the coordination
protocol (population database, tournament, blackboard, claim plus DAG) and the "done" test
(AND/OR proof rollup, score dominance, all gates green, a signed-evidence threshold). So the
base engine stays thin (items, typed edges, tags, blobs, changelog and undo, claims, the
`Query::evaluate` frontier, vector and keyword search), and what differs lives in the type's
descriptor: node kinds, edge subset, verbs, `frontier`, `ranking`, `resolution_rollup`,
`accepts_kind`, `goal_predicate`. `nstype::for_namespace` resolves a namespace to its
descriptor at every seam that needs one.

Rationale: forcing an AND/OR proof tree and a MAP-Elites population into one schema loses the
tree rollup and the diversity niching respectively. Rejected: an incremental type tag plus
match arms; at least five strategies were already wanted, so the registry paid off at once.

In an untyped namespace `investigation::add` accepts any kind: recording something is never
blocked on typing it first. Cross-strategy tests pin that the seam dispatches per namespace,
a verb or kind from the wrong strategy is refused with the right list, base retrieval works
under both strategies, and untyped namespaces are unaffected (no item gains a resolution
unless asked).

### Memory units are ordinary items, with no new tables

Memory units are ordinary `items`. Every strategy shares four **base kinds**, which it may
refine or extend but never remove: `goal` (the root intent and its acceptance predicate),
`node` (the generic unit, specialized as `hypothesis`, `lemma`, `observation`, …), `artifact`
(a produced result: a repro, a proof, a fix) and `reflection` (synthesized memory such as a
post-mortem or the digest, first-class and retrievable).

The `V006` CHECK on `items.status` permits only task statuses or NULL, so memory units are
non-task kinds with NULL `status`; their lifecycle lives in `resolution` and tags. A later
migration may broaden the CHECK if a strategy needs a stored status enum.

### `resolution` records how a unit ended

`status` answers "how far along?"; `resolution` answers "how did it end?". They are
orthogonal, and `resolution` powers anti-retread. Values: `unresolved`, `success`, `dead_end`,
`superseded`, `abandoned`.

It is a column, not a tag, because the frontier and anti-retread queries filter on it hot.
Migration `V007` added `items.resolution TEXT`, nullable, indexed and CHECK-constrained, as an
additive `ALTER TABLE` with no back-fill: NULL reads as `unresolved`, so every existing row
behaves as before. `item::set_resolution`, the string-boundary `set_resolution_str` and
`get_resolution` write and read it; it is shown in listings, `--json`, `stat` and `item show`.

### Softer axes are tag facets

Axes not filtered on hot stay tags: `confidence=` (`unverified`, `screened`,
`machine-checked`, `peer-reviewed`, a ladder taken from a surveyed counterexample hunt),
`promise=` (frontier rank), `source=` (the producing run), `method=` (how a claim was
verified), and strategy facets such as `commit-range=`, `staleness=` and `reliability=`. The
confidence ladder and `method=` exist so a screen cannot masquerade as a validation.

### The edge vocabulary is global; edges carry a weight; belief is a query

The edge types grew from `depends_on`, `derived_from` and `parent_of` to a global
investigation vocabulary, of which each descriptor declares the subset it uses:

| Edge | Meaning |
|---|---|
| `supports` / `contradicts` | evidence for or against a unit; signed, weighted |
| `refutes` / `rules_out` | kills a unit / eliminates a whole region (the pruning edge) |
| `supersedes`, `confirms`, `answers` | replaces; promotes to confirmed; answers a goal |
| `narrows` / `constrains` | confirming one unit constrains a sibling (bisection) |
| `spawns`, `discovered_from` | a discovery spawns a unit; emergent provenance |
| `member_of` | clustering into a family, regime or niche |
| `tests`, `verifies`, `fixes` | experiment to hypothesis, check to result, fix to symptom |
| `reduces_to`, `equivalent_in_strength_to`, `explains_failure`, `informs` | proof-search relations |

`EdgeType::ALL`, `from_str_opt` and `evidence_sign` cover them, with round-trip and
uniqueness tested. `relates_to` parses as an alias of the existing `references` escape hatch,
so the untyped edge stays one edge. `edge::link` cycle-guards the acyclic types as before.

`V007` also added `edges.weight REAL` (NULL reads as 1.0). `edge::link_weighted` writes it and
refuses non-finite weights; a plain `edge::link` **preserves** an existing weight
(`COALESCE`), so re-linking never silently erases evidence. Belief in a unit is
`edge::evidence_for(node)`, Σ(`supports` − `contradicts`) × weight, itemized by
`edge::evidence_edges`. Nothing stores the sum, so it cannot drift from its edges, and
conflicting claims stay live side by side.

### `is:frontier` generalizes `is:ready`, so nothing writes a task's resolution

`Query` gained `resolution`, `kinds`, `exclude_kinds`, `exclude_tags`, `frontier`, `tombstone`
and `claimed`; the DSL gained `resolution:<r>`, `-kind:k`, `-tag:f=v`, `is:frontier`,
`is:tombstone` and `is:claimed`/`is:unclaimed`. `is:frontier` selects units that are
unresolved, non-terminal, unclaimed and free of any still-unresolved `depends_on` blocker.

For a task, whose resolution is NULL, it selects exactly the rows `is:ready` does, and a test
verifies it. That holds only while no task acquires a resolution, so
`investigation::resolve_unit` and `roll_up` both refuse to write one to a task; if either
could, the two queries would silently diverge.

### A digest is not work

Every strategy's frontier starts from `nstype::base_frontier`, which excludes `NON_WORK_KINDS`
(today `reflection`). A digest is memory *about* the investigation, not work in it, and must
never surface as the next thing to do.

### Six reads a cold agent uses

All scope by namespace and compose with vector and keyword search:

1. **Frontier** — `is:frontier` ordered by the descriptor's `ranking`; generalizes `task next`.
2. **Ancestry** — `parent_of`, `derived_from` and `answers` up to the goal: why a unit matters.
3. **Anti-retread** — the graveyard around a unit, read *before* work starts: `is:tombstone`
   (resolution `dead_end` or `superseded`, or an incident `refutes`/`rules_out`),
   `investigation::tombstones` (each with what killed it), `investigation::anti_retread`.
4. **Ranked pool** — the frontier ordered by promise or score.
5. **Signed evidence** — the belief aggregate.
6. **Digest** — the latest `reflection` in scope; the default cold-start read.

`jkb inv frontier|core|tombstones|retread|evidence|digest <ns|uid>` exposes them, human or
`--json`. Ranking is meant to blend relevance (vector), recency (decay on `updated_at`) and
importance (promise or score), since pure similarity surfaces stale and trivial units.

### `jkb related` walks edges

`jkb related <uid> [--edge T] [--depth N] [--direction out|in|both] [--json]` makes "traverse
prior attempts" one command. It runs over `edge::walk`, a breadth-first search deduplicated at
the shortest depth, so a cycle among the edge types that are not acyclicity-guarded still
terminates.

### Never hard-delete a dead end

A dead, superseded or refuted unit is never deleted. It is resolved and linked by the edge
that killed it (`refutes`, `rules_out`, `supersedes`; `explain-failure` records why). The
graveyard is the memory. In the dogfood run a refuted hypothesis left the frontier and stayed
in the tombstones with its refutation attached, legible later with no extra notes, because the
reason was the edge.

### The `debugging` strategy

`nstype/debugging.rs` covers a hard bug in a large codebase: the hardest case for the
substrate, and one jkb could drive at once.

- **Kinds:** `symptom`, `repro`, `hypothesis`, `experiment`, `observation`, `suspect-area`,
  `regression-window`, `root-cause`, `fix`, `invariant`.
- **Edges:** `tests`, `supports`/`contradicts`, `refutes`, `narrows`, `rules_out`,
  `confirms`, `answers`, `fixes`, `verifies`, `discovered_from`.
- **Verbs** (`jkb inv do`, listed by `jkb inv verbs <ns>`): symptom, repro, hypothesize,
  experiment, observe, support, contradict, refute, suspect, rule-out, narrow, root-cause,
  confirm, fix, verify, invariant, note.
- **Done:** a confirmed `root-cause` **and** a `fix` that `verifies` against the minimal
  repro. Localization runs on two axes, where (component) and when (regression window).
- **Protocol:** claim and narrow with one or a few agents, reusing the claim model.

The extra terminal leg is deliberate: a symptom resolves on fix plus verify, not on a
confirmed diagnosis, so the frontier does not go quiet when somebody writes down an answer.
The dogfood run bore it out: after root-cause and confirm the verdict still read "root cause
confirmed, but no fix has been verified against the repro yet", easy to get wrong because a
confirmed diagnosis reads like a finish line. A scripted test drives a whole investigation and
asserts frontier, anti-retread, tombstone reasons and rollup at every stage.

### Observations go stale when the code moves

A debugged system is mutable, so an observation can stop being true. Observations carry
`commit-range=`, and `debugging::mark_stale_observations` (`jkb inv stale <ns> --window`) tags
out-of-window ones `staleness=stale`. The frontier excludes them, ranking sinks them, and a
stale observation cannot roll a unit up to success; nothing is deleted and every edge is kept.
An observation with no recorded range is left alone: absence of provenance is not staleness.
Repro reliability is the `reliability=` facet.

### The `conjecture-attack` strategy: prove or disprove under one structure

`nstype/conjecture.rs` resolves a hard conjecture by proof **or** disproof, grounded in
published frontier-lab prompts (a cycle-double-cover proof, the Jacobian conjecture, and a
flow-conjecture counterexample hunt).

- **Kinds:** conjecture, approach-family, approach (a route), reduction, lemma, construction,
  invariant, candidate-proof, candidate-construction, parameter-regime, obstruction, gap,
  partial-result, audit, tool, mechanism.
- **Edges:** `reduces_to`, `equivalent_in_strength_to`, `member_of`, `rules_out`, `refutes`,
  `explains_failure`, `informs`, `verifies`.
- **Done:** a candidate that survives an audit, clears the goal's acceptance predicate, and
  has **no open `gap`** in its `depends_on` closure: the machine-checkable form of the
  prompts' "do not return a reduction, an isolated missing lemma, or a best-effort summary".
  The handoff to the next run is the best partial result plus the open gaps
  (`open_gaps_under`).
- **Protocol:** an orchestrator, diverse explorers that cross-pollinate late, adversarial
  auditors, and a light coordinator that demotes a premature `confirmed`.

The finding that shaped it: the labs run proof search and counterexample hunting as **one**
structure, from one portfolio of approaches, differing only in acceptance. So it is one
strategy with presets. `ACCEPTANCE_PROVE`, `ACCEPTANCE_DISPROVE` and `ACCEPTANCE_BOTH` are the
enumerated "insufficient" lists transcribed from the prompts: prove rejects, among others,
low-dimension-only, bounded-degree, formal-power-series, local-analytic, reduction to another
open conjecture, and computation up to a fixed bound; disprove requires an explicit object, an
exact invariant computation and a complete impossibility proof. `jkb inv new --accept` seeds
the preset into the goal's body and tags it, so `accepted_kinds` decides which candidate kind
can ever satisfy the goal.

Honest limit: jkb has no runtime for dozens of concurrent agents, so this is the north-star
schema driven at small fan-out. Its value is externalizing the registry and graveyard the
prompts juggle in context, so they survive past one context or run.

### Conjecture-attack's coordination primitives

What the prompts specify by hand, made durable:

- **Approach-family registry.** Routes group by mathematical idea through `member_of`;
  `family_pressure` counts live members, so too many routes on one idea is visible.
- **Blocked with a reason, reopened behind a gate.** The `gap` verb blocks a route through a
  reverse `depends_on` on a first-class `gap`, so the route leaves the frontier with its reason
  attached. `jkb inv reopen` passes `reopen_gate`, which admits only a materially new
  mechanism, invariant, construction or obstruction.
- **Incompatible routes coexist.** Nothing auto-prunes on conflict; pruning is an
  orchestrator's act.
- **Anti-progress is detected.** `is_anti_progress` flags a route reducing to a lemma
  `equivalent_in_strength_to` the goal, which is worth nothing.
- **The graveyard is load-bearing.** Refuted candidates and their obstructions are kept, and a
  new candidate is checked against the `rules_out` regimes first.
- **Reusable assets.** A `tool` or harness is built once; the best `partial-result` tells a
  stalled investigation from an advancing one.

A scripted test drives all of it through the public verbs, including both candidate
directions, the gated reopen and the acceptance presets.

### An audit must be done by an audit

Adversarial audit is a first-class `audit` unit carrying the enumerated `AUDIT_CHECKLIST`
(a formal inverse passed off as a polynomial one, circular use of an equivalent statement,
hidden assumptions, …). `survived_audit` requires the verifying unit to *be* an audit, so a
candidate's self-assessment does not count.

### `jkb inv` is the write path, and every write is audited

`jkb inv new <type> <path>` creates the typed namespace, seeds the `goal` with its acceptance
predicate, and saves three views (`<path>-frontier`, `-core`, `-tombstones`) so the buckets
are reachable from the generic `jkb view` surface. A bare name homes at `memory/<repo>/<name>`
from the ambient repo, mirroring task homing, or at `memory/<name>` outside a repo or with
`--global`. So `ns:memory/**` recalls across every repo's investigations and
`ns:memory/<repo>/**` narrows to one.

`jkb inv do <verb>` is the preferred write and `jkb inv add <kind> … [--edge type:uid] [--tag
f=v] [--weight N]` the general one; the strategy validates the kind either way. `link` covers
edges no verb has; `promise`, `resolve` and `rollup` the rest. Every write goes through the
writer-actor, so it is audited and undoable. Agent guidance (AGENTS.md, the `jkb guide`
investigations stanza) says how to orient: digest, then tombstones, then frontier, and
`retread` or `related` before working a unit.

### A bounded read must name what it bounded

Any read meant to stand in for looking at the graph (a digest, an `inv` verb, an MCP tool, a
UI panel) must say what it left out. `jkb inv digest <ns> [--dry-run]` writes one stable
`reflection` unit per investigation, rewritten in place. `digest()` reads each bucket in full,
records the elided count, then truncates to `DIGEST_BUCKET_CAP` (12), and `render()` prints
"… N more not shown here — run `jkb inv tombstones`". A bound is fine; a silent bound is not.

What it cost to learn: the dogfood run investigated a real jkb bug found by using the tool.
Fifteen ruled-out regimes went into an investigation; the digest rendered twelve and said
nothing about the other three. A digest is read *instead of* the graph, so an unmarked cut
reads as "this is everything"; on the tombstones bucket an agent would see twelve dead ends,
believe that was all, and spend a day on the thirteenth. The store held all fifteen and every
edge throughout: the failure was entirely in the one read designed to replace looking. The
investigation reached a verified fix, and its post-mortem is stored as a `reflection`.

### `jkb inv ls` shows unit counts

`jkb inv ls` shows each investigation's unit count, so an emptied investigation is visibly
empty. Found in the same dogfood run: after `jkb undo` removed an investigation's units, the
namespace and its type survived (undo inverts item inserts, not `ns::ensure`), and the listing
implied a populated investigation. Undo is unchanged and consistent with tasks; the defect was
a listing implying state that was not there.

### Pitfalls every strategy must honour

Each from a failure observed in the surveyed systems:

1. **Over-structuring.** Keep a free-text `node` and the `references` (`relates_to`) escape
   hatch; only base fields are mandatory. This is also why edges are never type-validated.
2. **Re-treading dead ends.** Read anti-retread *before* work; never hard-delete a failure;
   keep the edge to what killed it.
3. **Provenance rot.** Supersede, don't delete; conflicting claims stay live via signed edges.
4. **Stale or flooded retrieval.** Blend relevance, recency and importance; filter by scope
   before ranking; cap, and report, what reflections inject.
5. **Coordinating through chat.** Memory is items and edges; messages are ephemeral. The
   writer-actor and claims already enforce this.
6. **One schema forced on every problem.** Hence the shared core plus descriptor.
7. **No synthesis layer.** `reflection` units are first-class; a scheduled pass is deferred.

### Strategies documented but not built

Each is a future namespace type, recorded so the seam is checked against it. None is blocked;
none is to be built without a design pass.

- **`evolutionary-search`** (FunSearch, AlphaEvolve): scored candidates, evaluators,
  MAP-Elites niches; `mutated_from`, `dominates`; a population database with stateless workers
  and no coordinator. A genuinely different protocol, which earns its own descriptor where
  prove versus disprove did not.
- **`tournament`** (Google's AI co-scientist): hypotheses with Elo ratings; generate, reflect,
  rank, evolve, plus a meta-review writing failure-mode reflections. For open-ended empirical
  work, where belief is continuous.
- **`blackboard`** (the Hearsay-II revival): agents self-select against a shared workspace
  while a controller picks who acts. A protocol variant more than a schema; for work that
  cannot be decomposed in advance.
- **`literature-synthesis`** (Zep/Graphiti, A-Mem): sources, claims, entities, syntheses, with
  bi-temporal validity. jkb's ingest, search and context reads are most of it already; the gap
  is concept edges and bi-temporal validity.
- **`software-swarm`**: the task DAG, claims, review and merge queue as a registered strategy.
  A late consistency check that the base subsumes what jkb runs, not a rewrite of a working
  system.

### Cross-cutting work deferred

Each needs its own design pass:

- **Bi-temporal validity** (`valid_from`/`invalidated_at`), for "what did we believe at time
  T"; the hardening of supersede-don't-delete.
- **A scheduled reflection pass** writing digest units so retrieval does not decay as the
  graph grows.
- **An MCP memory surface.**
- **Composite relevance × recency × importance** as a first-class search route.
- **`jkb inv goal <ns>`.** Resuming mid-investigation in the dogfood run took a `jkb query
  "kind:symptom ns:…"` to recover the goal's uid. `investigation::goals()` exists; it was left
  off the CLI because the digest shows the goal and the surface is already wide. (Also noted
  then, not fixed: `jkb inv frontier --limit 0` prints "(empty)", which reads like "no work".)
- **Large fan-out**, which belongs to scaling up the task swarm.

## History

Superseded and reversed decisions, with what replaced them and why. The survey behind the
investigations work (Beads, FunSearch and AlphaEvolve, Google's AI co-scientist, Zep/Graphiti,
the blackboard revival, the frontier-lab proof and counterexample prompts, and classical
cryptanalysis and debugging practice) is in
[openspec/changes/jkb-memory/research.md](../openspec/changes/jkb-memory/research.md). It is
evidence rather than decisions, and was not migrated.

### Repo content at the global top level

Before the layout, `openspec` was a top-level namespace mounting this repo's `openspec/`
directory, `codereviews` likewise, and a stray `<ns>` namespace sat beside them. Replaced by
`repos/<repo>/…` because a second repo's `openspec/` would have collided with the first. The
2026-07-24 migration moved both mounts under `repos/jkb/` without touching a file, and removed
`<ns>`.

### Every task's home was `tasks/inbox`

Quick-add originally hard-coded every task's Primary home to `tasks/inbox` and routed every
`+<ns>` into a Reference mirror, so a task's real home could not be set. Organization was
faked with mirrors, and `tasks/inbox` meant both un-triaged capture and the primary of things
filed elsewhere; the existing backlog tasks sat at `tasks/inbox` plus a `.backlog` mirror, the
convention leaning on the mirror while the primary said something false. Replaced by
first-placement homing. A one-shot migration re-homed those tasks to the repo backlog as
Primary, dropped the redundant mirror, folded the bare top-level `.backlog` namespace in with
`ns::move_subtree`, and removed it; CLAUDE.md and the task-filing memory were updated.

### The repo key as the mount's leaf segment

Task homing's first draft and its migration examples used the mount's leaf as `<repo>`
(`tasks/jkb/.backlog`). The 2026-07-06 design pass replaced it with the full mount namespace
(`tasks/repos/jkb/.backlog`) because leaves of different mounts can collide. The one-shot
re-homing migration and some tests still name `tasks/jkb/.backlog`.

### `memory/` as an empty reserved root

The layout reserved `memory/` for LLM long-term memory and deferred its shape to its own
design pass. Replaced by the investigations design: typed namespaces, three buckets and a
digest, and two strategies.

### Types enforced only inside the investigation engine

When typed namespaces first landed, `accepts_kind` was consulted only in
`investigation::create` and `investigation::add`; a plain `item::upsert` plus
`placement::place` could file anything into a typed namespace. A type was a hint, and the
namespaces that most needed one had none and were special-cased around literal paths.
Replaced by the check in `placement::place` and the three contract types, applied
automatically.

### Resolving the tasks root from the type system

Typed namespaces were asked to let "configuration establish the global tasks directory". It
was built: `task::root` found the tasks directory by asking which namespace carried the
`tasks` contract (`ns::typed_root`). It worked, and it was wrong: a type meant a many-to-many
contract and a singular location at once. Two namespaces could carry `tasks` with one silently
losing, and holding the conflation together took four mechanisms: a `NamespaceType::locator()`
marker, a uniqueness guard in `set_type`, `clear_type` as an escape hatch (the root was
auto-seeded, so relocation was otherwise impossible), and a re-seed guard so re-creating a
vacated `tasks/` could not steal the root back. All four were removed; the reserved roots are
layout, and `task::DEFAULT_ROOT` is a literal. `ns::clear_type` alone survived, on its merit.

### `effective_type` as a query per ancestor level

`ns::effective_type` first issued one query per ancestor level. Replaced by one query over the
ancestor list when the placement check put it on the per-chunk ingest path.

### Investigation kinds without `task`

The base kinds were first only `goal`, `node`, `artifact` and `reflection`. Once placement was
enforced, an investigation namespace would have refused tasks, breaking the shipped
`is:ready` ≡ `is:frontier` equivalence for tasks inside one. Replaced by
`INVESTIGATION_KINDS`, the four plus `task`.

### Proof search and counterexample hunting as two strategies

The first design named three strategies: `debugging`, `proof-search` and
`counterexample-hunt`. The frontier-lab prompts showed both directions driven from one
portfolio under one registry, differing only in acceptance, so the two merged into
`conjecture-attack` with acceptance presets, and their planned milestone folded into it.

### A runtime-registered strategy trait

The first sketch had a `register(Box<dyn NamespaceType>)` runtime registry and a `cli_verbs`
method naming `jkb inv <type> …` subcommands. Replaced by `static` descriptors resolved by one
`match`, the sync serializers' shape, with verbs as `VerbSpec` data behind one `jkb inv do`.

### `relates_to` as a new edge type

The vocabulary listed `relates_to` as a new untyped escape hatch. Replaced by parsing it as an
alias of the existing `references` edge: one escape hatch, not two synonyms.
