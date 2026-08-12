# Handoff — `staging-workflow`, after the two structural fixes

## State

- Branch `staging-workflow`, HEAD `db33aa4`, 34 commits, working tree clean, `./scripts/check.sh`
  green (fmt, clippy, 431 tests, deny, ui).
- 22 review passes run. **No pass has ever returned zero must-fix.** Pass 23 has not been run.
- Findings live in the KB: `repos/jkb/codereviews/<folder>`, mirrored under `tasks/jkb/codereviews/…`.
  Reviewer accuracy so far: 91% (240 accepted / 24 dismissed of 264 settled).

## What just landed (§A and §B of the previous handoff)

Both were changes of *shape*, not fixes at the two named sites. The two open must-fix are closed
as a consequence, and each is pinned by a test verified to fail for the right reason.

### A. The cut point has one owner — `crates/jkb-cli/src/base.rs`

`FACET` is **private** to that module: nothing else can name the facet, format `<branch>:<sha>`,
or take one apart. `ensure_recorded` is the only "should I write?" and `resolve` the only "what is
recorded?", adjacent so the deliberate asymmetry between them is one screen.

- `repo::set_location_facets` calls `ensure_recorded` **before** rewriting `branch=`, because
  attribution of a legacy bare value depends on what other branch the task names. Neither caller
  is told about that ordering — it is inside the shared writer.
- `landed_for_action` resolves the cut point itself from the task's tags. It was a parameter every
  reader had to derive per branch, and `base_for_branch`/`qualified_base_for`/`set_qualified_facet`
  are gone.
- **`jkb task base <uid> <branch> <sha>`** is the only writer a user or workflow can reach.
  `jkb task tag add|set base=…` refuses and names it (`rm` still works). `/task-swarm` calls the
  verb; the old help text and error strings that recommended `tag set base=` are gone.
- A git ref (`refs/jkb/base/<branch>`) was argued and **rejected by the user**: jkb runs inside
  professional repositories and must not decorate them with refs nobody asked for. Recorded in the
  module docs and in CLAUDE.md so it is not re-proposed.

### B. One guard at the export seam — `crates/jkb-sync/src/engine.rs`

- `finish_export` takes the `SyncDoc` and renders it itself, so the document the guard judged is
  necessarily the bytes written.
- `export_blocker`'s first condition is `wholesale_loss`: the render about to be written vs the
  document just parsed off disk. It refuses when the KB side contributes **zero** items to a file
  that declares some. It reads no bindings, because emptied bindings are exactly the damage.
- A mount-mode matrix — {import, export, bidirectional} × {first sight, settled, disk-changed,
  kb-changed, both-changed, post-undo} — asserts the D45 invariant in every cell. Only
  Export/PostUndo fails without the guard, which is the pass-22 must-fix.
- The stale comment at `undo.rs:262` that asserted this could not happen is corrected.

## Working rules (non-negotiable, learned the hard way on this branch)

- **Loop until a pass returns zero must-fix.** Never propose stopping early. Fix what this change
  introduced; file pre-existing bugs to `tasks/jkb/.backlog` and name them in the report.
- **A doubt you can name is a test to write, not a line in the reviewer's focus argument.**
- **Verify a test fails for the right reason**: revert the fix, read the panic, confirm it names
  the harm — not a bookkeeping assertion. Two tests written this session were **vacuous** until
  that check was run (`--dry-run` writes no status; an undo after a `mount_dir` unwinds the mount).
  Neither would have been caught any other way.
- **Never pipe the gate through `tail`** — you get `tail`'s exit code, and a red gate reads as
  green. `./scripts/check.sh > <file> 2>&1; echo "EXIT: $?"`, then grep the file.
- **Assert edits against a re-read of the file.** Four edits on this branch silently never applied
  while their commit messages described the intent.
- **Run step 0 of `/review-log` before launching a review.** ~6 agents / ~800K tokens at the
  default `low` tier; `medium` is ~15 agents / ~3M.
- Use `./scripts/*.sh`, never raw cargo. Never `sqlite3` against a jkb db. pnpm, never npm. No
  `Co-Authored-By` trailers.

## Open, not fixed here

From pass 22, still in the review folder:

- `V011__items_sequence_high_water.sql` — no repair for ids `V010` already reused;
  `Dispatcher::rebuild_all` exists but has no CLI caller. Wire `jkb index --rebuild`.
- `engine.rs` `sync_paths` reconciles directories (no `is_file()`), journalling a permanent
  `needs_attention` row a `mkdir` can create. Filed to the backlog; pre-existing.
- 10 nits, several stale comments. `.claude/commands/review.md` frontmatter still advertises only
  the old tiers.

Known gap in §B, deliberate and documented: `wholesale_loss` catches *total* loss, not partial. A
KB left holding some of a file's items (an undo of one of several sync transactions) can still
export away the rest. That is bounded and shaped like an ordinary edit; a general "fewer items
than disk" rule would refuse every legitimate export on a projection mount.

## Commands

```sh
./scripts/check.sh > /tmp/gate.txt 2>&1; echo "EXIT: $?"   # never pipe to tail
./scripts/test.sh -p jkb-sync      # one crate
/review-log                        # step 0, then the reviewer, then file findings as tasks
jkb task add "…" +tasks/jkb/.backlog
```
