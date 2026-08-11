# Handoff — `staging-workflow`, after review pass 22

## State

- Branch `staging-workflow`, HEAD `2702cc3`, 33 commits, working tree clean, `./scripts/check.sh` green.
- 22 review passes run. **No pass has ever returned zero must-fix.**
- Findings live in the KB: `repos/jkb/codereviews/<folder>`, mirrored under `tasks/jkb/codereviews/…`.
  Reviewer accuracy so far: 91% (240 accepted / 24 dismissed of 264 settled).
- The branch **cannot land**: 2 open must-fix from pass 22.

## The thing that actually needs fixing

For **four consecutive passes**, every must-fix was introduced by the previous pass's fix, and all
of them landed in the same two places. Each round the response was a procedural rule — "trace the
consumers", "check every call site" — and each round the next pass found the consumer that was
missed. That rule now exists three times (step 0 check 4, CLAUDE.md, memory). Writing it a fourth
time is the error, not the cure.

Both areas need a change of *shape*. Do these before, or instead of, patching the two must-fix
at their named sites — the must-fix are symptoms of exactly these two causes.

### A. `base=` should not be a tag facet

One fact — "branch X was cut from commit Y" — is currently spread over ~12 sites: `set_location_facets`,
`set_qualified_facet`, `base_for_branch`, `qualified_base_for`, `landed_for_action`, `landed_with_base`,
`cmd_task_start`, `cmd_task_work`, `review::others_are_covered`, `review::work_is_in`,
`merged_state_of_all`, and `jkb task tag set`. Every pass has fixed a subset. Pass 22 found that the
remedy string printed by my own error message (`jkb task tag set base=<branch>:<sha>`) *destroys*
other branches' bases, and `/task-swarm` now runs that exact broken command.

**Preferred fix: move it into git.** Record the cut point as a ref — `refs/jkb/base/<branch>` — at the
moment the branch is created. Properties this buys for free, none of which need remembering:
per-branch by construction; deleted with the branch; invisible to `jkb task tag`; no qualified/unqualified
ambiguity; no legacy-value fallback; and it lives where branches live, so `gitrepo` owns the whole
question. Migration: on read, fall back to the `base=` facet if no ref exists, and write a ref when
one is missing. Then delete the facet path.

**Minimum acceptable fix if the ref approach is rejected:** one module owns the facet, the raw string
is never written anywhere else, `jkb task tag set base=` refuses with a pointer to the real verb
(`jkb task base <uid> <branch> <sha>`), and `/task-swarm` calls that verb.

Either way, **write down first**: what does each of the two readers (`close-merged`, `review record`)
do for each of {no base, base for this branch, base for another branch only, legacy bare base}? That
is a 2×4 table with 8 cells. Three separate must-fix have been one cell of it.

### B. One guard at the export seam, not one per cause

CLAUDE.md D45 already states the invariant: *an unverified KB render reached `write_file`* is the
single mechanism behind every sync data-loss incident. The undo path keeps finding new routes to it —
pass 21 stripped files via `finish_export`'s `(false, true)` arm, pass 22 via `three_way_resolve`'s
`!ctx.imports()` arm. Both times the fix was at the route.

**Fix at the seam instead:** refuse any export where the KB side contributes **zero items** while the
disk document declares some, on a mount that cannot import them back. That is one condition covering
undo, a failed migration, an emptied binding table, and whatever comes next. `export_blocker` already
exists and is the right home — its current weakness is that it judges from live bindings, which is
precisely the state these bugs have emptied. Judge from the **rendered document about to be written**
versus the **disk document just parsed**, which is the comparison that cannot be fooled by upstream
damage.

Add the mount-mode axis to the sync tests: the recurring shape is "this arm behaves differently on an
export-only mount and nothing tested that axis". A matrix over {import, export, bidirectional} ×
{first sight, settled, disk-changed, kb-changed, both-changed, post-undo} would have caught pass 21's
and pass 22's must-fix before either shipped.

## The two open must-fix (symptoms of the above)

1. `crates/jkb-sync/src/engine.rs:1370` — after `jkb undo` of a sync on an **export-only** mount, the
   next sync exports a render with no task lines and strips every `- [ ]` from the file. Reaches
   `three_way_resolve`'s `!ctx.imports()` arm → `finish_export`; `export_blocker` passes because undo
   emptied the bindings it judges from. The comment at `undo.rs:262` asserts this cannot happen.
2. `crates/jkb-cli/src/main.rs:4543` — `task work` overwrites `base=` with `onto`'s *current* tip
   whenever no live session exists. `resumed` is worktree-based, so re-working an abandoned branch has
   `resumed == false` while `worktree_add` re-attaches the existing branch. Gate on
   `qualified_base_for(...).is_none()` (as `cmd_task_start` does) or on whether the branch already
   exists, not on `resumed`.

## Open concerns from pass 22 (in the review folder, not yet filed to backlog)

- `main.rs:4384` — `jkb task tag set base=` deletes other branches' bases; it is the remedy the new
  error message names **and** what `/task-swarm` now runs. Fixed by A.
- `main.rs:4281` — `task start` replaces a legacy bare `base=` with today's trunk tip instead of
  re-qualifying the existing value. Fixed by A.
- `V011__items_sequence_high_water.sql` — no repair for ids V010 already reused;
  `Dispatcher::rebuild_all` exists but has no CLI caller. Wire `jkb index --rebuild`.
- `engine.rs:327` — `sync_paths` reconciles directories (no `is_file()`), journalling a permanent
  `needs_attention` row a `mkdir` can create. Already filed to the backlog; pre-existing.
- 10 nits, several stale comments. `.claude/commands/review.md` frontmatter still advertises only the
  old tiers.

## Working rules (non-negotiable, learned the hard way on this branch)

- **Loop until a pass returns zero must-fix.** Never propose stopping early. Fix what this change
  introduced; file pre-existing bugs to `tasks/jkb/.backlog` and name them in the report.
- **A doubt you can name is a test to write, not a line in the reviewer's focus argument.** Pass 22's
  must-fix was verbatim the question put into its focus. Findings should be *surprising*.
- **Verify a test fails for the right reason**: revert the fix, read the panic, confirm it names the
  harm — not a bookkeeping assertion.
- **Assert edits against a re-read of the file.** Four edits on this branch silently never applied
  while their commit messages described the intent.
- **Run step 0 of `/review-log` before launching a review.** A run is ~6 agents / ~800K tokens at the
  default `low` tier (3 area reviewers); `medium` is ~15 agents / ~3M.
- Use `./scripts/*.sh`, never raw cargo. Never `sqlite3` against a jkb db. pnpm, never npm. No
  `Co-Authored-By` trailers.
- **Read the gate output, don't tail it.** `check.sh` prints fmt diffs that look like success at a
  glance; I committed on a red gate twice this way.

## Commands

```sh
./scripts/check.sh                 # fmt + clippy -D warnings + tests + deny + ui build
./scripts/test.sh -p jkb-sync      # one crate
/review-log                        # step 0, then the reviewer, then file findings as tasks
jkb task add "…" +tasks/jkb/.backlog
```
