<!-- generated from jkb design design:agents-and-roles-18dcdf01b7c25370eba53a, edit there (version 131.gwGKxuu-nM74CQGLms6H9aGbAQGK1JWxm8B6AYqSsu6Sl64FAYuk55aS2r8BAZW4zIydqdoJAZfQwr7ppVMBnJKttYj_OQGdwP-1quLfAwGdwI68ouLDAwGftsCloubBBwGgwLas1JHhAwGh6Lrm866CCgGgrsi-v8yyBwGk_urW7pfnAwGqhLytiocMAa-0qZqRw_oMAbaWxOOy_PsKAbaO4bOUtyIBtqzipKThtgkBuuSovNqj3gUBwLCLqMiNhQwBwfa32su3jAgBwubmgvLeqgIBwsa6246T9A4Bw9Lu6ZTxxwoBwr6am6aLuwEBxKCk_-yYwg0BxcaJ9PLPrwoByIjH8aHB3AoBybyak4mEdwHHnrqX__2HCQHNtN-W1LzQAQHOzIy1jL2BBQHPxNPf4fe-CAHOutK83ITNCgHUhPDx18n9AgHWiqyb0fnYCAHX0IaJwe7CBAHYjOfaoJxYAdbM0O6Ao4oBAdrW8aD3rMQGAdvyoK-P8bwDAd2qiZmUrYcMAeGcmJbXrvILAeWu06XgprEKAeXwv5652rUIAefO2sjjldQHAefQ7ObC-PADAemMkN-ol5EKAer6zvn9vOwMAeXomMT6q6gMAezewL7qy_0HAeXoxJH48VIB7sSt1t3UmgEB88CwgJbqMgHzpsDtvJLWDwH53qLj4-fHBwH5mujYn9zXBwGA78b5lb2BBAGB_cfJ74zXBAGB3_XPxLmIBwGDp6jb0dyABwGD0cGIl5iqDwGC38ekieSZCgGFwfH5zeCDDAGC29eU6ZLTCwGIm42Qmo3iBQGKo5uujcmDBgGPqbL2z_vrDgGR3-Lu9fmNBwGTy8jDjrDIDgGYocLnpefUDQGatZOtnZakDLvsBZvX8-OF69YHAZz_gZ6C9fULAZutj_KlzPEPAaLtseyy0oACAaPDrfub2-QIAaSVmbSOmOkBAaPX1sTuwO4BAafX86W1o5oBAanz_vP_6cYCAarHxMSKxu8KAa6_1M6X_6kFAa_V_fmh2-IOAbSDtdvp8s8LAbjn9L_o4JEMAbnbhOGZ-K8EAbvPv-_DnZAPAb3Xz9vtrL4IAcC9q62dp20ByenWzqWqww0ByqmdydTJzQgBy8_MidOWnAkBy-uk8s-wvAUBz4OWyIvI9A0Bz52PhrKM5wUB0enr2bPxkwwBz_v8veSLlgkB0tGNztyhuAcB1OeqqofxrQgB1aXvmMDJ-w8B1tPf6sDO7wQB14fhhPijyAMB1vOBpOOXnggB2oHy_OK24AwB3N2ZxueX1wcB3pmchp7e1gQB3_meg7eQ6AsB3_Hft6zzmAQB4v_fgYGZmQwB47PXx9qX_QMB5JeqkemK5QQB5LuJ_YyKUAHmj5r1hIqZBQHjsZrDw7zWBQHnwYSkqdjUAwHphYG9vsj8BwHrxbTZl5wOAey9gMffwrAPAfCPp9Tl5dYCAfGz1LuDov4CAfPxh56U41UB9bPctv_PBQH198zSturhBQH34aC_1c2aBQH1ha7W37zyCQH5gcD8kfzhCwH7nfWK4r7jAgH9n9-HrbbzDgE, blake3 9bf9978ae6d325aa1bedbd79773293d0a7c09027fbb71e673630a17937111136) -->
Migrated from: openspec/changes/jkb-fleet-hardening/proposal.md, openspec/changes/jkb-fleet-hardening/design.md, openspec/changes/jkb-fleet-hardening/tasks.md, openspec/changes/jkb-fleet-hardening/specs/agent-swarm/spec.md, openspec/changes/jkb-fleet-hardening/specs/agent-cli-contract/spec.md, openspec/changes/jkb-fleet-hardening/specs/task-dag/spec.md, openspec/changes/jkb-code-review/design.md, docs/task-lifecycle.md (lines 744-1276), docs/ui-and-review.md (lines 137-233), docs/git-hooks-installer.md (lines 1085-1093), docs/sandbox-and-container.md (lines 1077-1085)

# Agents and roles

jkb is driven by agents as much as by people: a task swarm that schedules, implements, reviews
and lands work, a code reviewer that files its findings as tasks, and interactive sessions that
coordinate both. This design records who may do what, and how the agents work: the claim model
and the swarm's roles (SCHEDULER, IMPLEMENTER, REVIEWER, and a merge queue that is not an
agent), the CLI contract that keeps agents off raw SQL, the design gate, the composable code
reviewer, roles and grant tokens, workflow strategies, harness attestation, and the hooks-off git
that keeps the host from running code the container wrote.

The task statuses, the transition log, per-task worktrees and the review-gated landing itself
are the task-lifecycle design; this one adds who may move a task through them. The container and
its egress boundary are the sandbox-and-container design; the daemon that answers every request
is the daemon-and-messaging design. The installer's own git hook is the git-hooks-installer
design.

Four rules thread through everything below. A claim is not a status. Each swarm stage maps to
one task status. Task status, task ids and the fact that a swarm ran are KB-local and never
appear in git. And enforcement never precedes capability: a guard is switched on only once the
sanctioned path covers every need it would block.

## The swarm: claims, roles and the merge queue

The swarm is `.claude/workflows/task-swarm.js`, launched by `/task-swarm`. Running its first
version, which fanned out one IMPLEMENTER per ready task into isolated worktrees and merged each
branch through a serialized LLM RESOLVER, surfaced a cluster of weaknesses on three axes at once:
roles (which agent does what), statuses (how a task's lifecycle and in-flight ownership are
tracked) and enforcement (how agents are kept on the sanctioned tools). Nothing reviewed a branch
before it merged; a task in flight was indistinguishable from a ready one, so two schedulers
could grab it and a dead agent's task stayed `in_progress` forever; every integration spent
agent tokens even when the merge was trivial; and agents reached around the CLI into raw
`sqlite3`. They were fixed together so the three axes could not drift apart, for example into an
ad-hoc "claimed" status colliding with the derived `blocked` state, or a `sqlite3` guard blocking
reads the CLI could not yet do.

### Three agent roles and one deterministic integrator

Three agent roles exist. The **SCHEDULER** plans overlap-free work-groups, the **IMPLEMENTER**
builds a group, and the **REVIEWER** checks one. Integration is a deterministic mechanism, the
merge queue, not an agent. All of them are distinct from the deterministic **coordinator loop**:
plain workflow code that dispatches the agents, owns the claims and drives the merge queue. The
pipeline is: SCHEDULER groups, then IMPLEMENTER (`in_progress`), then REVIEWER (`needs_review`),
then the merge queue (`done`). The SCHEDULER prevents the conflicts the queue would otherwise
catch late, and no LLM sits anywhere in the integration or completion path.

```
open ─▶ in_progress ─▶ needs_review ─▶ done
       (IMPLEMENTER)   (REVIEWER       (merge queue landed it on the
                        reviewing)      feature branch; agent stops)
```

### A claim is its own columns, never a status

A task being worked must be distinguishable from one that is free, or two schedulers (or a
re-dispatch racing the original) take the same task, and a crashed agent leaves a task with
nothing to reclaim it. A claim is a **liveness-checkable owner id** plus a claimed-at time,
stored as two nullable columns on `items` (migration `V005__task_claims.sql`, an additive
`ALTER TABLE items ADD COLUMN`, safe because `items` is a regular table):

```
items.claimant_id  TEXT   -- NULL = unclaimed; else a LIVENESS-CHECKABLE owner id
items.claimed_at   TEXT   -- when the claim was taken; NULL when unclaimed
```

Both NULL means unclaimed, and the migration back-fills nothing, so every existing task behaves
exactly as before. The claim lives on the row rather than in a side table because it is a
single-holder fact about the row: acquisition becomes a compare-and-swap and the frontier filter
a plain column predicate. There is deliberately no expiry column.

The claim is emphatically not a new `status` value. `status` already carries one derived,
never-stored state (`blocked`: a `depends_on` edge to a task that does not unblock dependents);
a stored `claimed` string would put two sources of truth in one column and collide with the
typed-status direction. Claim and lifecycle answer different questions, "is anyone holding this
right now?" and "how far along is the work?", so a task can be `in_progress` and unclaimed after
a reclaim.

### Acquiring a claim is a compare-and-swap that starts the task

`claim` is a single conditional `UPDATE` that succeeds only if the slot is free or already held
by the same owner, and in the same statement advances `status` to `in_progress`:

```sql
UPDATE items
   SET claimant_id = ?owner, claimed_at = ?now, status = 'in_progress'
 WHERE id = ?item
   AND (claimant_id IS NULL       -- free
        OR claimant_id = ?owner)   -- same owner → idempotent
-- changes()==1 → acquired (and now in_progress); changes()==0 → a live claim by someone else
```

Mutual exclusion and the start transition happen in one atomic statement, so there is no
claimed-but-`open` window and no read-then-write race. jkb already serializes every write
through the writer-actor, so there is no true row race; the CAS is still the right idiom, simpler
than read-check-write and robust if the actor model changes. `claim` acts on a ready task, which
is non-terminal, so forcing `in_progress` is always the right start state.

The seams live in `jkb-core/src/claim.rs`: `claim(item, owner) -> Result<bool>` (the CAS;
`Ok(false)` means held by another owner); `release(item, owner)`, a CAS that NULLs the claim
columns `WHERE claimant_id = ?owner` and leaves status to the lifecycle (on give-up the
coordinator sets it back to `open`); and `reclaim_dead(&[live_owner])`. There is no time-based
variant, no expiry sweep and no `heartbeat` seam.

### Liveness is owner-existence: no TTL, no heartbeat

A claim never lapses by age, because an agent can be legitimately paused for a long time, for
example blocked on a permission prompt the operator has not seen yet. An age-based expiry would
reclaim live work out from under it. So the IMPLEMENTER is responsible for nothing beyond
existing; it never pings, reports or refreshes anything, which is all "no heartbeat" means.
Liveness is entirely the coordinator's deterministic job.

`claimant_id` is `host:pid` of the coordinator process plus the run id. Subagents share their
coordinator's process, so the owner is that process: if it lives, its claims live; if it died,
they are reclaimable. `reclaim_dead` probes whether each claim's owner still exists (`kill -0
pid` on this host, or the run is active) and NULLs the columns of every claim whose owner is not
in the caller's verified-alive set. A paused but alive owner passes the probe and keeps its
claim. jkb is local-first, so a single-host probe is enough. There is no agent-facing heartbeat
command.

### The coordinator claims, releases on settle, and scans every minute

The coordinator CAS-claims every task in a group, to its own owner id, immediately before
dispatching the group's IMPLEMENTER, and releases them all when that agent's promise settles:
success, eject, failure or abort, through a `finally` around the `agent()` call, so a crashed or
aborted agent still frees its claims. It `await`s each agent, so a claim exists exactly while its
agent is in flight. That is the normal path.

The crash net is the same owner-existence reclaim, run both by `jkb doctor` and by the
coordinator on a periodic timer of about 60 seconds, a lightweight janitor that clears claims
left by crashed prior runs within about a minute regardless of scheduling cadence. The timer is a
scan interval, not a claim lifetime: the reclaim decision is always owner-existence, never "this
claim is N minutes old".

### Reclaim cannot clash with a status transition

Three properties keep the coordinator's scan from fighting the merge queue setting a task `done`,
or any other transition. First, `reclaim_dead` never touches the live coordinator's own claims:
the coordinator passes its own owner id in the live set, so a run only reclaims claims of other,
dead runs, and the merge queue that sets `done` belongs to the same live coordinator. Second,
reclaim and status transitions write disjoint columns: `reclaim_dead` and `release` write only
`claimant_id` and `claimed_at`, transitions write only `status`, and only `claim` writes both, as
one atomic acquisition. Third, every one of them is a `write_txn` on the single writer thread, so
none can interleave mid-statement; a scan firing mid-integration queues behind or ahead of the
merge queue's write rather than racing it.

### Claim writes are changelogged

`claim`, `release` and `reclaim` each append a changelog row (ops `claim`/`release`/`reclaim`,
entity `items`, the item rowid, before and after claim values, and the status a `claim` moved in
the same entry). The first draft avoided logging them for fear of bloat, but that assumed a
heartbeat. With liveness by owner-existence, claims are low-frequency (about one `claim` and one
`release` per group attempt, plus the rare `reclaim`), and "who claimed what, when it was
released, and when and why a reclaim fired" is exactly the record wanted when a swarm task
sticks or a double claim is suspected. `undo::INVERSES` now inverts all three by restoring the
logged columns.

### The ready frontier excludes claimed tasks

`task::ready` gains one plain column predicate, `AND claimant_id IS NULL`, with no anti-join, and
keeps its priority-then-due ordering. Any non-null claimant excludes the task, so a second
scheduler never hands out work already in flight; a cleared claim (released, or reclaimed because
its owner is gone) returns the task to the frontier.

### `jkb doctor` reports and repairs orphaned claims

`jkb doctor` has a claim-health section, mirroring how it surfaces
`sync_state::needs_attention`. For each claimed task it parses `claimant_id` and probes whether
the owner still exists; a claim whose owner is gone is **orphaned**. There is no time-based
staleness, so a paused but alive owner is not orphaned. A bare run reports orphaned claims (task
and owner); `--fix` clears them so the tasks return to `task next`. `doctor` is therefore the code
that checks an agent exists, not the LLM. It is advisory and idempotent, and the crash-recovery
net on top of the coordinator's release-on-settle and periodic scan. `jkb task reclaim` exposes
the same reclaim directly.

### The SCHEDULER groups overlapping ready tasks

One IMPLEMENTER per ready task meant two ready tasks touching the same files raced in separate
worktrees and collided at integration, where the queue ejected the second for a conflict that was
predictable up front. So a **SCHEDULER** agent runs at the head of each round (named so as not to
collide with the deterministic coordinator loop). It reads the ready frontier and clusters tasks
likely to touch the same code into **work-groups**, one per IMPLEMENTER, so intra-group overlap
is resolved by a single author instead of surfacing as a merge conflict.

Its signals are shared target files and paths, shared symbols, the same crate or module, and
similar descriptions. Deterministic file and path overlap is precomputed from task bodies and
handed to the agent as a starting point, but the final clustering is the agent's judgement, which
is why it is an agent and not a rule. It groups only clear overlaps, under a hard cap of about
three or four tasks per group: grouping trades parallelism for fewer conflicts, over-grouping
serializes independent work, and the merge queue absorbs the residual. A task with no clear
overlap is a singleton group, so the common case is unchanged. `schedulerPrompt` returns
`{ groups: [{ tasks: [...] }], remaining }`.

Groups hold only ready tasks (unblocked, unclaimed), and only from the current round's frontier.
There is no cross-round grouping: a group never spans a task still blocked this round, and tasks
that still overlap simply re-cluster next round. That keeps the SCHEDULER bounded and stateless.
Pipelined cross-round grouping is a tracked limitation (`task-swarm-pipelined-grouping-ca`).

### The work-group is the unit of work

Downstream, a group replaces a task everywhere. The IMPLEMENTER implements all its tasks on one
branch, as one coherent clean commit set. The REVIEWER reviews the branch against every task in
it. The coordinator claims every task in the group before dispatch and releases them all on
settle. When the branch lands, all the group's tasks close together. Feedback goes back for the
whole group, and the retry budget (`RETRY_CAP`, default 3) is per group.

### One implementer per group; a fresh reviewer per pass, with a handoff

An IMPLEMENTER is bound to exactly one work-group for that group's whole life. It builds the
group and receives every review `request_changes` and every merge-queue eject for that group,
keeping the context of the work it just did; that continuity is why feedback goes to the same
implementer rather than a fresh one. What an implementer never gets is a different group or new
tasks: cross-group reuse is what poisons context, with one group's stale assumptions bleeding
into another's code.

The REVIEWER is deliberately asymmetric. Each review pass gets a **fresh** reviewer, so it is not
anchored to its own earlier take, seeded with the previous reviewer's `handoff` note (what it
required and what the next reviewer should re-verify), so the thread of what was flagged and why
survives. A reviewer instance still only ever touches one group. When the group lands or is given
up, its agents are torn down; only the coordinator loop persists across groups, and it never
hands an existing agent a second group.

### The REVIEWER checks the whole group before it reaches the queue

Before this, an implementer's branch went straight from Implement to integration, and the only
quality gate was the operator's after the merge. Now a REVIEWER runs per group between Implement
and the merge queue, in parallel across groups. It is told the full group and checks the branch
diff against the spec and acceptance criteria of every task in it (the requirement text at each
`^id` for a file-backed task, `jkb task show` for a managed one); that the branch implements all
the group's tasks and nothing beyond their union (no drift, no unrelated edits); and that tests
and scripts are green. `reviewerPrompt(group, branch, priorHandoff)` returns `{ verdict: 'approve'
| 'request_changes', notes, handoff }`: `notes` go to the implementer, `handoff` to the next
reviewer. Only an approved branch enters the merge queue. Entering review sets every task in the
group `needs_review`.

### One bounded feedback loop for review and ejects

Two paths send a group back to its implementer: a review `request_changes`, and a merge-queue
eject (a rebase conflict or a red gate). On an eject the implementer is told to pull the updated
feature branch, reproduce the failure and fix it, after which a fresh reviewer re-reviews. Both
paths share the one per-group budget (`RETRY_CAP`, with the review notes fed back as a
`reviewHint`), and the next Implement pass sets the group's tasks `in_progress` again. A group
that exhausts the budget is given up and reported, never landed unreviewed.

### The merge queue is deterministic: no resolver agent

The RESOLVER agent is removed entirely. Integration is a deterministic merge train
(Bors-style), `scripts/merge-queue.sh`, run mechanically by the workflow: a thin step that only
reports the script's exit status and timing. "No agent" means no LLM judgement anywhere in the
merge path. Agents only author (IMPLEMENTER) and review (REVIEWER); a conflict is never reconciled
by an agent mid-merge but ejected and handed back to the implementer to rebase, which is whose job
it was all along. Integration spends zero agent tokens.

### The queue tests against the live base tip, one commit at a time

The queue is a FIFO of approved branches, drained one at a time in the integration worktree. It
rebases the head entry onto the current tip of the base feature branch and fast-forwards the base
to it: a linear graft, never `git merge --no-ff`, so the base gains regular serial commits with no
merge commit and no branch-name artifact. It then runs the gate (`./scripts/build.sh` and
`./scripts/test.sh`) on the result. Green keeps the fast-forward and records the landing; a
rebase conflict or a red gate resets the base to its pre-graft tip and ejects the entry.

Because the queue tests against the live tip, which already holds everything landed earlier in
the run, a branch green in isolation can still fail once an earlier entry lands. That semantic or
textual conflict between two in-flight branches is exactly what a serial queue exists to catch,
and the fix is the implementer rebasing onto the new base, not a merger reconciling blind. It is
the standard merge-train trade: each entry runs the full gate serially, so integration is slower
per commit but the base is never broken, and at most one integration is ever in flight.

### Implement, review and merge are pipelined

The first loop had a barrier each round, `await parallel(ready.map(...))`, so one slow
implementer stalled integration of all the fast ones and their work could not unblock dependents
until the slowest peer finished. Each group now flows Implement, Review, merge queue
independently, through `pipeline(groups, implementStage, reviewStage, mergeQueueStage)`:
wall-clock is the slowest single group's chain, not the slowest stage across all groups. The
merge-queue stage takes a shared serial lock, so pipelining widens the parallel stages without
weakening serial integration. As a landed group unblocks dependents, a fresh SCHEDULER pass
groups the new frontier and appends the newly ready groups; the pipeline runs until nothing is
ready or in flight. DAG ordering is unchanged, because a dependent group only becomes ready, and
so only enters the pipeline, once its prerequisites have landed.

### `needs_review` does not unblock dependents

The swarm's lifecycle reuses existing statuses, with no new variant and no migration: `open` (not
started), `in_progress` (an IMPLEMENTER is working it, and while claimed), `needs_review` (a
REVIEWER is reviewing the branch), `done` (landed on the feature branch) and `cancelled`.
`needs_review` is transient and re-enterable and no longer unblocks dependents: a branch under
review is not on the shared feature branch and may still bounce, so a dependent must not start
against it. `TaskStatus::unblocks_dependents()` is just the terminal set `{ Done, Cancelled }`. A
cancelled dependency unblocks too, since it will never complete. Dependents unblock at `done`,
which is precisely when later implementers, who branch off the feature branch, can see the work.
The full status set and its transitions are the task-lifecycle design.

### Task status and swarm details never enter git

Task status, the ephemeral local task ids, and the fact that a swarm ran are personal, KB-local
bookkeeping. They never appear in git, which is a hard requirement for using jkb in a shared
professional codebase. An implementer writes a normal, descriptive commit message: no `swarm:`
or `swarm-task:` prefix, no task uid, no trailer, no reference to the swarm, squashed to one
clean commit if it likes. The merge queue rebases, so history is linear regular commits with no
`Merge swarm-task/…` commits. The task-to-branch mapping lives in the coordinator's memory
(`built.branch`, `b.uid`), so nothing is written to git to recover it. The integration branch the
operator eventually opens a PR from is an ordinary feature branch (`/task-swarm --branch <name>`,
never `swarm/<base>`), and the per-task `swarm-task/*` branches are local-only, deleted after
landing, and must never be pushed. So there is no commit trailer, no auto-`done` on PR merge and
no `post-merge` hook for task state: git carries only the clean code.

### Integration is instrumented and visible

The workflow `log()`s each queue entry's gate time and pass or eject outcome, and the run
summary reports pass/eject counts and the tokens and latency saved, so the queue's health and
cost are measurable. `scripts/swarm-status.sh` shows the pipeline without reading the database
directly: its RUN view reports SCHEDULER groups, REVIEWER outcomes and merge-queue pass/eject
counts beside the task-status counts, keeping `needs_review` distinct from `done` so in-review
work is visible, and its FILE view keeps the per-task markers.

### The swarm closes a task only by landing it

The merge queue closes a landed group itself, by recording the landing (`jkb task landed`, an
`observed_landed` transition). The step after a landing only checks the group's tasks
(`unclosedTasks`, pinned by `dev-scripts.test.sh` case12): it reports any that did not close as
a stall, with the remedy, and never closes anything. Under the default `design-reviewed`
strategy, where only the operator lands, that is every group: the queue's `jkb task landed` is
refused, prints a note, and the operator lands.

### Swarm landings follow the task's strategy

`scripts/merge-queue.sh` records a landing with `jkb task landed`, and under the default
`design-reviewed` strategy only the operator lands. A batch the swarm lands on its own runs under
a strategy whose `lands` toggle includes the coordinator (`jkb workflow set <uid> autonomous`, or
an operator-defined one); otherwise the operator lands it.

### Not yet guarded: a non-operator closing an unstarted task

Nothing in jkb yet stops a non-operator closing an unstarted task; the swarm simply no longer
asks one to. A guard in `rbac::authorize` refusing a non-operator `task.set --status done` on an
`open` task was built and dropped after review round 3: it cannot tell a swarm agent closing an
untouched task from `/next-task` closing the task it just worked, because both are a
coordinator's `open -> done` and `/next-task` never claims its task. Making `/next-task` claim
first drew a must-fix in each of the next two rounds (a release matching a per-process owner, no
reopen on give-back, a `[~]` checkbox the close step no longer matched). The guard is a design
task, `task:guard-in-jkb-against-a-non-opera-18dc8d24f389a8d8`: tie a `done` to whoever claimed or
started the task.

### Typed status is out of scope

Storing `status` as a typed column instead of plaintext (`items-status-should-be-a-typed-v`)
stays its own task. The claim was built to neither depend on nor block it: the claim lives in its
own columns and the lifecycle reuses existing variants, so the two do not entangle.

## The CLI contract: agents never touch SQL

Agents and people interact with a jkb database through the `jkb` CLI or the repo's `scripts/`,
never raw `sqlite3`. That keeps every write on the audited writer-actor with changelog and undo,
keeps reads on the committed read seam rather than uncommitted WAL state, and keeps callers off
the schema. The rule sits in CLAUDE.md beside the no-raw-cargo rule, in the same spirit.

### Every task read and mutation has a CLI command

When the CLI could only `task add` and `task next`, agents ran raw SQL for everything else, and
`next` cut content to 80 characters, so a task's own requirement text was unreadable through the
CLI; that blocked reading the fleet tasks to specify this very work. The CLI now covers every
read and write an agent needs, each a thin edge over an existing audited, cycle-checked core seam,
with no new core logic:

- **Read:** `jkb task show <uid>`, the full untruncated item (metadata and content), accepting the
  bare slug or the full `task:` uid. `task next` and `query` keep their snippet listings.
- **Metadata:** `task set <uid> --status|--priority|--due` over `task::set_status_str` (which
  rejects the derived `blocked` and unknown strings), `set_priority` and `set_due`; `task tag
  add|rm <uid> <facet>=<value>` over `tag::apply`/`tag::remove`.
- **Edges:** `task depend <uid> <dep-uid>` and `task undepend` over `task::add_dependency`
  (cycle-guarded by `edge::link`) and `edge::unlink`. This was the capability missing when the
  four fleet tasks could not be wired together with `depends_on`.
- **Placement and binding:** `task place <uid> +<ns>` / `--home` and `task bind <uid>
  --managed|--sync <uri>`, keeping homing and binding the two orthogonal axes the namespaces
  design defines.
- **Claims:** `task claim|release <uid> [--owner <id>]` and `task reclaim`, so the coordinator
  claims through the CLI. The owner defaults to a liveness-checkable identity so `doctor` can
  probe it. There is no `heartbeat` subcommand.

Every read takes `--json`, so agents parse structured output and never scrape human lines. Every
edit round-trips and is undoable.

### `jkb task show` landed first

`task show` landed before anything else in the fleet work, as the bootstrap: until the fleet
tasks' own bodies were readable through the CLI, nothing else in the change could be read without
the SQL it was meant to retire.

### A hook denies raw `sqlite3` against a jkb database

`.claude/hooks/block-raw-sqlite.sh`, a PreToolUse Bash hook wired by its own entry in
`.claude/settings.json`, denies a `sqlite3` invocation that targets a jkb database (`jkb.db`,
`$JKB_DB`, anything under `~/.jkb/`), with an actionable message pointing at the CLI and scripts.
It mirrors `block-raw-cargo.sh`: it matches `sqlite3` only at a command position (line start or
after a shell separator, allowing `FOO=bar` prefixes), so `sqlite3 /tmp/other.db` and the word
inside a quoted commit message pass; and it fails open, so an error in the hook allows the
command rather than wedging Bash. Scripts under `./scripts/` may read the database directly; they
are the sanctioned path.

### Enforcement follows capability

The ordering is the decision, not a step: the CLI surface first, the hook last. A guard that
denies raw SQL before the CLI can serve a need hard-blocks agents, so the hook was the final
task of the fleet work, gated on the read and mutate surface being complete.

## The design gate

Swarm implementers run headless, as Workflow sub-agents, and cannot ask the user about a design
that has not been decided. So design is separated from implementation by a tag gate: people
decide designs, the swarm implements decided ones.

### A task is swarm-eligible only when tagged `design=approved`

`/task-swarm` ANDs `tag:design=approved` into its `task next`/`query` selection, in its scout and
in every SCHEDULER pass, whenever it runs in scope mode, so un-triaged tasks are invisible to the
swarm. Two bypasses exist: `--no-design-gate` (`cfg.designGate: false`) and explicit-uid mode,
where the operator names the tasks.

### `/design-pass` settles a design with the user

`/design-pass <path>` is the interactive counterpart. It walks open, un-triaged tasks under a
path, settles each task's design with the user through `AskUserQuestion`, records the decision,
and only then runs `jkb task tag add <uid> design=approved`. Trivial tasks skip the write-up and
are fast-tracked straight to the tag.

### Where a decided design is recorded

A decision is recorded in the design document for its group of related tasks, keyed by a
`Governs: <uid>` line so an implementer finds it by grepping for the uid, not in a running
`design-notes.md` log. A small, standalone design can instead live only as the inline `Design:`
note on the task. Either way the decision is also stamped into the task body (`jkb task edit
--append` for a managed task; the source-file line for a file-backed one). The IMPLEMENTER reads
the approved design first and follows it rather than re-deciding.

### The gate is spelled in the query DSL

Use `tag:design=approved`. The `#facet=value` form is quick-add only, and `task next` silently
drops terms that are not `tag:` or `ns:`, so `#design=approved` in a `task next` scope is parsed
as free text and dropped, and the gate silently admits everything.

## The code reviewer

`/review-log` used to wrap the host's `/code-review`, which reports to the user rather than
returning findings, so the wrapper's middle step ("invoke the code-review skill and collect the
findings") was a hole. We write the reviewer instead, which makes the quality of its prompts a
load-bearing input to the project. It is portable: it runs in any git repo, using project
context where it finds it and skipping it where it does not.

### One workflow, two thin callers

`.claude/workflows/code-review.js` holds everything and returns structured findings. `/review`
prints them. `/review-log` writes them as a `tasks.md` in a per-run folder
(`.codereviews/<datetime>-<branch>-<N>/`) and mounts it into the KB with the `tasks` serializer,
the part of the old command that always worked. Neither caller contains review logic, so the two
can never disagree about what counts as a finding.

### Two axes, because they miss different things

Horizontally, a **lens** is a question form ("what values break this?", "who else touches this
fact?") asked of the whole diff, slicing every changed file by one kind of mistake. Vertically, a
**feature reviewer** takes one coherent capability out of the change and asks whether it is
right as a capability: complete across its surfaces, coherent between its parts, and actually
working when run. Neither subsumes the other. Two of this repo's escaped bugs were feature-level:
`8a50925` shipped a frontier rule with no view, so `task next` correctly withheld a parent task
while the tree invited you to pick it, and no single lens owns "this feature is half-delivered";
`16d4e4d` ran to completion having embedded 0 of 56,402 items, visible only to someone asking
whether it works end to end on real data. The number of feature reviewers is dynamic: a scout
clusters the diff into functional units, the way the SCHEDULER clusters tasks into work-groups.

### The scout is two bounded agents in parallel

The scout is a survey (what changed, which capabilities) and a context gather (the project's
rules), run in parallel. One agent doing both read the whole diff and searched the repo for three
kinds of document; on a 2k-line change it ran 23 minutes and died mid-response. Splitting halves
the wall clock, and either half can fail without the other. The context gather is capped at about
40 lines per section, and the prompt says why: its output is pasted into a dozen reviewer
prompts, so every line costs twelve times.

### The lenses are derived from kinds of assumption

A defect is a violated assumption. Assumptions come in kinds, and each kind has a testing
discipline that exists because nothing else finds that kind. That derivation is repo-independent,
which matters because the reviewer runs in other repos; fitting the lenses to jkb's own 57 past
findings would have produced a set that transfers to nothing.

| Lens | Question | Discipline |
|---|---|---|
| `input` | What values break this? | boundary / fuzz |
| `state` | What happens the second and third time? | state machine |
| `inference` | X is treated as evidence of Y — when do they come apart? | predicate vs specification |
| `contract` | Who else touches this fact, and do they agree? | integration |
| `concurrency` | Who else is running right now? | interleaving |
| `failure` | What happens on the error path, and how much does it take down? | fault injection |
| `scale` | What does this cost on real data? | load |
| `intent` | Does it do what its name, docs, types and tests claim? | oracle problem |

`intent` includes "would this test fail if the change were reverted?". Our own escapes are then
instances: the sync-direction bug (a byte hash meaning "content changed or the renderer changed",
indistinguishable) is `inference`; the prose-identity bug is `state`; `jkb index` embedding
nothing is `scale` and `contract`.

### Security is not a lens

Security decomposes: injection and malformed input are `input`, authorization and trust
boundaries are `contract`, and "this token proves that claim" is `inference`. Each of those three
lenses is told to cover its security half explicitly. The dedicated security pass is
`/security-review`, which already exists.

### `structure` is a separate reviewer, not a lens

"Is there a better way to organize and factor this?" is a real review question but not an
assumption kind: it asks whether the code is well built, not whether it is wrong. So it is a
reviewer of its own with three properties the lenses lack. It must name **what the shape costs
today** (a reader misled, one change that will need two edits, a concept with two names);
"cleaner" is not a cost, and a finding without one is dropped, which is what separates it from
bikeshedding. Its findings are `kind="quality"`. And it is verified by its own skeptic, because a
structural suggestion has no reproduction and the defect skeptics would refute every one by
construction; that skeptic asks whether the change is worth the churn, and refutes taste,
proposals larger than the problem, abstractions with one use, and anything that would make the
code the odd one out against the project's own idiom. The lenses are told structure is not
theirs, so they stay on defects. Duplication straddles the two: copies that can drift apart and
disagree are a defect (`contract`); copies that are merely repetitive are `structure`'s.

### Three tiers, by breadth of fan-out

Every lens question is asked at every tier, because a question skipped is a class of bug nobody
looked for. What changes is whether each question gets its own agent and its own reading of the
diff. **`low`, the default**, runs up to three reviewers split by feature area, each asking all
ten questions (the eight lenses, structure, and the feature view) against one reading of its code:
about 6 agents. **`medium`** runs nine lens reviewers plus one holistic reviewer per functional
unit, about 15 agents. **`high`** is `medium` plus the skeptics. `medium` was once the default and
cost about 3M tokens and an hour a run, and its reviewers overlapped heavily: nine agents each
loaded the same file, then a consolidation pass merged back their near-duplicates, a pass that
existed only because of the fan-out. Nine independent readings do catch what one reader misses,
which is why `medium` remains: a choice to spend, not the price of admission.

Two rules keep `low` honest. Every changed file lands in exactly one area, because a file in no
area is a file no reviewer opens, which reads exactly like a clean review of it. And the
per-reviewer finding cap scales with what each reviewer owns, or a cap meant to stop padding
silently becomes the budget.

### Verification is optional, and unverified is the default

Measured over two runs, adversarial verification refuted 6% of findings and cost most of the run.
That does not pay: a false positive here costs one implementer a few minutes discovering it is
not real while already in that code, and three skeptics per finding up front is insurance for more
than the claim. So below `high`, findings are filed **unverified** and marked as such, and
whoever picks one up is the verification. `high` adds the three-angle vote, for before merging
something risky. There is no single-skeptic tier: one skeptic is neither cheap nor a vote, and
verification's value lives in the disagreement between angles.

### Skeptics vote 2-of-3, batched by file

At `high`, every defect finding faces three skeptics with different angles: *is it already
handled?* (read the surrounding and called code for the guard the finder missed); *does it
reproduce?* (name concrete inputs and state that produce the wrong result); *is it real and in
scope?* (a deliberate, recorded decision, or something the diff is not responsible for). A finding
survives on a majority of the verdicts cast about it. Skeptics default to **refuted when
uncertain**: a false positive costs the reader the same attention as a real finding and, repeated,
costs the reviewer its credibility, after which the real findings are not read either.

Skeptics are batched by file. Loading the code around a finding is the expensive part; judging a
second finding a few lines away, once that context is in hand, is nearly free. So a skeptic gets
every finding in one file, ordered by line, and returns a verdict on each. Cost scales with how
many files carry findings, not how many findings there are, and because each defect batch faces
all three angles the vote is a true 2-of-3. Quality findings batch separately and face the one
angle that can kill a restructuring suggestion: is it worth the churn.

### The burden of proof is on the finding

The first full run refuted 3 of 48 findings. The skeptic prompt said "default to refuted when
uncertain", but its output contract required a guard to quote in order to refute, and a skeptic
that merely could not confirm a claim had nothing to cite, so it let the finding stand. The
instruction and the contract asked for opposite things, and the contract won. Now `refuted=false`
requires writing the verified chain: which lines were read and why each step of the failure path
follows. "I could not find a guard" and "I confirmed there is no guard on any path that reaches
this" are different claims, and only the second earns a stand; uncertainty is an explicit
refutation reason. Measured: the kill rate moved from 6% to about 20% on the next run.

### A skeptic's verdict carries its reasoning

A verdict is not a bit. On the first real run a skeptic let a finding stand and corrected it:
"survives, but must be narrowed to AGENTS.md alone; the `jkb guide` half is refuted". That
correction is the most reliable evidence a run produces, because it came from someone who read the
code intending to kill the finding, and the first version computed a boolean and threw it away.
Surviving skeptics' reasons now ride on the finding, and the ranking pass applies them (narrow the
summary, drop a disproved detail, move the severity), believing the skeptic over the finder where
they disagree.

### Consolidate before verifying, not after

Reviewers working from different angles report one defect several ways; in the first run the same
stale-documentation issue arrived four times at four locations. Deterministic dedup catches only
an identical `file:line` or summary, so each copy bought its own skeptics, and the merge happened
afterwards in the ranking pass, where it saved nothing. One cheap agent, judging from summaries
alone with no code access, now groups them before verification, so the saving lands on the stage
that dominates cost. It keeps the gravest reading of each group and records every lens that saw
it, since agreement across independent angles is itself evidence for the ranking pass. It is
best-effort by construction: out-of-range, duplicated and overlapping indices are dropped rather
than trusted, so a failure costs duplicate verification and never loses a finding. When unsure it
leaves findings separate, because a wrong merge loses a finding and a wrong split costs one more
verification.

### Severity is assigned once, at the end, in a strict order

Finders each see only their own findings, so `input`'s "high" and `scale`'s "high" mean different
things. One ranking pass reads every finding together, merges near-duplicates, and assigns
severity on one scale, whose names say what to do: **must-fix** (`!p1`), wrong now, wrong on the
next run, or losing data; **concern** (`!p2`), wrong under conditions that will occur; **nit**
(`!p3`), quality, such as duplication that can drift, a claim the code does not keep, a test that
would pass without its fix. Formatting, whitespace and taste-only renames are out entirely. The
test for must-fix is "would you hold the merge for this?", asked of each finding on its own. There
is no target proportion. The pass also orders the whole set strictly, since a reader works down it
and stops when time runs out; the strict order is what prioritizes, and it works as well on a set
that is all one severity.

### A structural finding is priced, not capped

A quality finding earns its severity on the same ladder defects use, with "wrongness" replaced by
"cost the code is already paying". **Nit** is the default: a real, named cost, but small or local.
**Concern** only if the cost is already being paid somewhere you can point at (the second place
that had to change and did not, the caller that got a repeated dance wrong, two live names for one
concept), and the finding cites that file and line. **Must-fix** only if the shape makes a
correctness property unenforceable or forces a coming change to go wrong (an invariant with no
choke point because of where the code lives, a new case to add in three places with nothing
forcing the third), with the mechanism shown, not asserted. A missing seam where an invariant
needed a choke point can outrank a bounds check. Crowding-out is handled by the evidence bar,
which is checkable without re-reading the code: the ranking pass demotes any quality finding above
nit whose argument reduces to "this would be better", and the quality skeptic checks the citation
against the real code and refutes the finding if the evidence is not there. "This would be better"
is a nit however strongly held; "here is where this already went wrong" is a concern.

### Report a failure as a failure

The first live run died in the scout and the workflow reported `note: "empty diff"`: the guard
`if (!scout || scout.files.length === 0)` folded "the scout crashed" into "there was nothing to
review". That is a specific wrong cause for a generic failure, and it sends the reader to check
their range instead of re-running. It is the same defect class as a `graft` finding in an earlier
review (every `checkout --detach` failure reported as a rebase conflict), which shows how easy the
mistake is. The two are now distinguished, and a failed context gather degrades the review rather
than ending it, with the run saying it reviewed without the project's conventions.

### Context is discovered, and absence is not an error

The reviewer must work in a repo with no CLAUDE.md, no design docs and no review history. The
scout looks for, and passes on, whatever it finds: **conventions** (`CLAUDE.md`, `AGENTS.md`,
`CONTRIBUTING.md`, the project's rules), without which a reviewer cannot see a convention breach
and will propose designs the project already rejected; **the change's design**, so "this
contradicts the decided design" is findable and a deliberate decision is not reported as a bug;
and **past findings** (`.codereviews/*/tasks.md`) as a pattern library of what has actually gone
wrong here, given as patterns to check for, never as a checklist to complete. Every reviewer is
told to read the code the diff touches (callers, callees, the other implementations of the same
fact), not the diff alone. That separates a mechanism-level finding from a line-level one, and
every escaped bug above needed it.

### Verification cost is a product of three terms, all bounded

The first full run cost 153 agents and 6.5M tokens on a 2,851-line diff, about 85% of it
verification, because its cost is a product: findings (69, about 6 per reviewer) × skeptics (3,
on every finding regardless of severity) × context per skeptic (a whole 5,271-line `main.rs`,
"open the file and read the real code"). Every term was unbounded and multiplied the others, so
fixing one would not have been enough. All three are bounded now: a per-reviewer finding cap,
which also improves the output (forced to pick five, a reviewer reports its five best rather than
padding); batching skeptics by file; and bounded reading (`grep -n` the enclosing function and
read a few hundred lines around it plus the per-file diff, never a large file end to end, the
largest single lever). Findings past the verify cap are reported `unverified`, never dropped, or a
budget limit would look like a clean review. Roughly, on a 1,000-line diff, `low` is about 6
agents, `medium` about 15, and `high` adds three agents per file carrying findings. Above about
2,000 changed lines, several smaller ranges are both cheaper and a better review: a reviewer
reasoning about 3,000 lines at once reasons worse about each.

### Acceptance is measured, never fed back

Findings become tasks, so their status is an accuracy signal: `done` means acted on, `cancelled`
means dismissed. Each run reports the running acceptance rate over settled findings; Propel's bar
is that an acting rate under 20% means the sensitivity is wrong. It is deliberately not used to
suppress a class. A class that keeps being dismissed might be noise, or a real problem the team
keeps deciding not to fix, and silently ceasing to report it would turn that decision into an
invisible one.

## Roles, grants and workflows

The pattern `/task-swarm` and hand-driven sessions converged on (a **coordinator** that spawns a
**designer**, then **implementers**, then **reviewers**, and a **systemic reviewer** when review
keeps finding the same areas) lived only in prompts. The operator typed "continue" and "run
another review round", and nothing stopped an agent skipping a step. This part makes the path
forward a fact jkb states, holds each agent to the steps its role allows, including an agent
trying not to, and lands a task only on a clean last review round.

### Where the pieces live

- **`crates/jkb-rbac`**: RBAC as a checkable table, the sibling of `jkb-fsm` and as
  dependency-free. Nothing in it is about jkb.
- **`jkb-core/src/roles.rs`**: the roles, who may grant whom, hashed grant tokens, the operator's
  `agent_type → role` map, and agent bindings.
- **`jkb-core/src/workflow/`**: the phase set, the graphs as `jkb-fsm` tables, strategies,
  presets, and the append-only workflow log.
- **`jkb-core/src/reviews.rs`** (migration `V023`): the land gate's review facts, kept out of tags.
- **`jkb-api/src/rbac.rs`**: `Request::permission` (exhaustive), `OP_GRANTS`, principals, the
  in-memory ticket store, `authorize` at the top of `LocalBackend::call` (the one dispatch the
  daemon, the MCP server and host-local mode share), and the `role.*`, `workflow.*` and `attest.*`
  ops.
- **`jkb-cli/src/rbac_cli.rs`**: `jkb role`, `jkb workflow` and `jkb attest hook`.

### RBAC is a checkable table

`jkb-rbac` has `RoleTable` (static) and `OwnedTable` (built at runtime) behind one `Grants`
trait. `check()` finds a role granted nothing, a permission nobody holds, and duplicate rows;
`matrix()` renders the table. `Authorizer` composes (`RoleBased`, `AllowAll`, `Both`,
`FnAuthorizer`), and a refusal names the roles that would have been allowed, so a refused agent
learns who to ask.

### Six roles, and who may grant whom

The roles are operator, coordinator, designer, implementer, reviewer and `systemic_reviewer`. Who
may grant whom is itself a `RoleTable<Role, Role>`: a coordinator grants a designer or an
implementer, scoped inside its own scope, and never a reviewer, because a token it mints is one it
holds, and a coordinator holding a reviewer token could review its own work. The table is checked
when a grant **resolves**, not only when it is minted, so a grant minted before a tightening
neither resolves nor lists as live. It stays unrevoked, and `role ls --all` shows it marked `NOT
GRANTABLE`. Reviewers come only from the operator: a grant, or the `agent_type → role` map an
attested subagent resolves through. Agent bindings are first-bind-wins.

### Grants are hashed tokens, revoked recursively, never changelogged

A grant is a 256-bit token stored only as its blake3 hash. Revocation is recursive: revoking a
grant revokes what it granted. Grants are deliberately not changelogged, because `jkb undo`
reviving a revoked grant would re-arm a credential.

### A strategy is a composition

There is one phase set: design, `design_review`, implement, review, `systemic_review`, landable,
landed, cancelled. Two graphs over it are `jkb-fsm` tables that pass `check()` and an `audit()`
over every combination of facts. A strategy is a composition (the operator's clarification): a
graph, permission **toggles** (`approves_design`, `lands`), each bounded by a domain so no toggle
can hand an operator power to a worker, and **attributes** (`repeated_area`). The presets are
`design-reviewed` (the default), `coordinated` and `autonomous`; operator definitions are
versioned. Phases were not folded into `TaskStatus`: the status is the claim and land axis every
frontier and synced checkbox reads, and the workflow is a second axis over it.

### A task pins its workflow at its first move

A task pins a snapshot of its strategy at its **first move**, whatever strategy it runs then,
including the default, so a redefinition never changes a task mid-flight. An unreadable stored
`default` is refused, never silently replaced by the preset. The workflow log is append-only, and
permission is checked in the callee (`store::fire`), not by each caller.

### The review loop is a graph, and only a clean round leaves it

A review round that finds a must-fix sends the task back to an implementer (`review_failed`),
through a systemic review first when must-fixes repeat an area (`review_repeated`; by default the
same file in two consecutive rounds). The systemic reviewer ends that one of two ways, with a
required written reason: `systemic_redesign`, back to design, when the fix changes how the
operator understands the system (under the default strategy that means back to the operator), or
`submit_systemic`, back to implementation, for a difficult code pattern. `jkb workflow observe`
takes the one reconciliation the facts call for, so after a round the task moves with no human
prompt.

### The Stop hook drives the next actor, only where a session opted in

`jkb workflow next --stop-hook` sends a Stop back, once per stop, while the next actor is one the
session drives, but only in a session launched with `JKB_DRIVE` set (`1`, or a task uid). The
hook is managed, so it fires in every container session, and without the opt-in it told an
interactive session in a task worktree to "continue" implementing on every turn.

### Reopen is the operator's, and follows the lifecycle

A workflow `reopen` is the operator's, and follows the task's lifecycle rather than leading it:
the task goes back to work first (`jkb task set <uid> --status open`), or the next observe would
put it straight back. Nothing reconciles a reopen automatically. A lifecycle reopened by a
coordinator or by a synced checkbox leaves the workflow parked where the operator acts next, and
`workflow show` names the operator's `reopen`. Nobody but the operator lands a task whose
workflow is parked.

### A landing parks the workflow in the lifecycle's own transaction

`store::follow_landing`, called from `transition::perform` on `land` and `observed_landed` (a
pull request merging included), parks the workflow at `landed` in the same transaction as the
landing, and a pull request closing without a merge, a landing with no destination, parks it too.
A cancellation still parks only on `observe`: neither `jkb undo` nor a `tasks.md` line that comes
back restores a parked landing, so parking a cancellation inside the cancel would make it one-way
for everyone but the operator when undo or sync restored the status. The operator's `reopen`
restores either.

### The same landing again is a no-op, and reported as already landed

Re-landing what a task's live landing already records is not refused: the merge queue re-running
a branch gets the lifecycle's no-op, not "held". That holds only for a workflow parked at
`landed`, and only for the branch, destination and head its live landing records; a cancelled
task ticked `done`, or a landing somewhere else, would otherwise record a landing it never had,
and new commits on the branch are new work. `jkb task landed` reports a task it may not land
again, but which is already `done`, as **already landed** rather than refused, because `task land
--keep-worktree` records the branch's pre-graft tip and the queue's own advice for a branch
already in its base otherwise failed. That applies only where the task's live landing records the
same branch onto the same destination (`task.facts` reports it) or a merged pull request (which
records no destination), whichever is newer, and neither once the task was put back to work
(`transition::current_landing`). On such a task the answer is true whoever asks; on any other, a
caller's refusal stays a refusal. `jkb task landed` reports a refused task as held and records the
branch's other tasks, and fails when the caller may land none of them rather than printing
`recorded:` over nothing. A task landed before workflows existed has no workflow rows, reads
`design`, and is not held by any of this.

### Deleting a task revokes its grants and keeps its workflow history

The workflow tables have no cascade from `items` (which is AUTOINCREMENT), so deleting a task
revokes its grants and keeps its workflow history, and `item rm` followed by `undo` gives back a
task still pinned to its strategy.

### The land gate needs a clean last round

The newest review round must itself have found no must-fix, on top of the task-lifecycle
design's review-gated landing. Fixing a round's findings is not a review of the fix, and
`must_fix` counts at any status. A daemon too old to report rounds refuses rather than skipping
the clause.

### A round is what it was when it was recorded

Rounds live in `review_rounds` and `review_round_findings` (`V023`). The first recording of a
namespace snapshots its findings (which are must-fix, and the file each names), and rounds are
ordered by recording. The gate's open must-fix count is the snapshot's must-fixes not yet
finished, and nothing live under a recorded round counts: a finding that matters after a round is
recorded is another round. A finding's priority, placement and `area=` are ordinary task content,
which is why nothing live may decide a round.

### Who may file and record a round

A round a **non-operator** records must be a namespace it filed itself with `task.review_file`
(`review_filings.filed_by`), and holds exactly what it filed: naming `tasks` as a round would put
every task in the recording task's scope, and recording another worker's filing would pull that
round's findings into its own. The operator's `/review-log`, whose findings arrive through a
mount, still records any namespace, on the host; from the container, a mounted folder is recorded
with `jkb task review record --branch <b> --findings <ns>`. **A coordinator neither files nor
records a round**: it drives the work, and one that could file an empty round and record it
against the branch it drove satisfied the gate with no reviewer involved. A round is a
reviewer's (a `reviewer`-typed subagent recording its own filing) or the operator's.

### A recorded round has one name, and filings never nest in rounds

A round's namespace is any filed or recorded one, any `repos/<repo>/codereviews/<folder>` (a
`/review-log` mount before it is recorded), and each through its `tasks/<repo>/…` mirror. A
recorded round is recorded under one name: recording another name for it, or a namespace in or
around it, is refused rather than snapshotted as a newer round, which would read today's
priorities and turn the gate's last-round verdict either way. A filing is refused into, above or
below any recorded round. A principal held to one task files only under
`repos/<repo>/codereviews/` of its task's repository (the nearest `repo=` up its parents), since
findings are ordinary open tasks on the shared frontier, and an attested subagent binds before it
files.

### Review facts are not tags

`reviewed=`, `review=` and `review-waived=` used to decide the gate, and a tag is content any
writer may set, the sync engine included, applying a `tasks.md` line an agent in the dev
container edited: `#review-waived=x` waived the gate. The answer is not a reserved facet; `tag.rs`
records that apparatus being tried for `base`, with six choke points failing to close it, and a
check in `task.tag` alone would guard one door of four (sync, quick-add and MCP write tags too).
The facts moved to an append-only `reviews` table, and the gate reads only that; a `reviewed=` tag
is now ordinary content nothing trusts. The live KB held 11 `reviewed=`, 35 `review=` and 28
`review-waived=` tags (measured). `V023` migrates a `review=` only beside a `reviewed=`:
`/review-log` tags a backlog finding `review=<ns>` as a trail with no head, and migrating those
made tasks the old gate called never-reviewed pass as reviewed. Those trails stay as ordinary
tags. `task.facts`, `task.staging` and `task.show` carry the facts as a `review` field.

### `landed` reads the landing transition, never `status = done`

Whether a task landed is read from its landing transition, never from `status = done`, which a
synced checkbox can write.

### Scope is enforced in the callee

A principal held to one task writes only that task, its subtasks and its findings.
`Request::target` is **exhaustive, with no wildcard**: each op writes one named task, or writes
nothing a scope protects (its callee holds it: filing, recording, revoking, attesting), or writes
**shared** state (a namespace, a lease, a worktree removal, an item outside the task tree), which
a scoped principal is refused. A scoped caller adds tasks only `--under` its task, and places
tasks (`task.add`'s home and mirrors, `task.place`) only where its own task is placed, and never
at or under a filed or recorded review namespace, even when its task is a finding placed there; a
subtask added under a finding falls back to the caller's own task's home. `review::record`
credits only in-scope tasks for a scoped caller. `--no-review` asks who the client is before
anything moves, whatever credential it presents.

### An attested subagent binds to its task

An attested subagent binds to its task on its first task-targeted write, or explicitly with `jkb
role bind <uid>`. A binding its first op made is undone if that op then fails, and one subagent's
calls are serialized from admission to that undo, so it never undoes a binding a concurrent call
of its own relied on. A reviewer must be bound before it records or files a review.

### The Land permission is asked before the target moves

`task.land_check` asks the Land permission as soon as the branch, target and head are known:
before the review gate, the graft, the gate command and the session archive. It carries the same
payload as `task.land` and is answered by the same `authorize`, so there is one rule rather than
a copy, and a merge queue re-landing a branch its live landing records is admitted here exactly as
there. It is not before every side effect: the `.git/info/exclude` entry, the land lease and
adopting the target from its remote come first. Pinned by
`a_caller_who_may_not_land_is_refused_before_the_target_moves`, which makes the task otherwise
landable (a clean review round, no gate), because without that the review gate refuses first,
before the graft, under the old code too, and the test would pass for the wrong reason.

### A new op needs the host daemon first

`task.land_check` is a new op, so a container client can be ahead of a host `jkb serve` that has
not been rebuilt, and that daemon answers `bad_request` ("unknown variant"). It fails closed, but
no `jkb task land` works until `./scripts/setup.sh` runs on the host. `op_error` says exactly
that, rather than the old daemon's list of every op it knows, for any op a stale daemon lacks,
keyed on the op actually sent (serde words an unknown value inside a request the same way).
Update the host before, or with, the container.

## Who is asking: credentials and harness attestation

Every request carries one bearer, and the daemon decides what that bearer may do. This part
records how each kind of caller gets its credential, and how Claude Code's harness vouches for
which agent made a tool call without the agent holding any secret.

### One bearer per request; no credential is not the operator

A request carries exactly one bearer: the root token (the operator, host only), the dev
container's credential (a coordinator grant, its ceiling), a role grant, or a harness **ticket**.
No credential no longer means operator. `remote::client` is the one place a client chooses: a
command uses its ticket or role token, else the container credential if it can read it (a person
at a container terminal can; the model's sandboxed tools cannot); a hook uses the container
credential; `jkb mcp` uses only an operator-configured role token, never the container's ceiling
on behalf of every agent it serves. A role header beside the root bearer was rejected in the
first draft, because omitting the header made the caller the operator.

### The daemon's grant cache

The daemon caches grant hashes so a wrong token costs at most one database read a second, claimed
under the lock, on the reader connection, and released if the read fails. A grant it mints
refreshes the cache at once, and every call re-resolves from the database so a revocation is never
served stale. A call that then fails `Unauthorized` evicts its hash and closes the connection, so a
grant revoked behind the daemon's back cannot hold slots open.

### The container credential is marked by rotation, and tickets are capped

The container credential is marked in its own column, set only by rotation: a grant the operator
merely labelled `container` mints no tickets. `setup.sh` rotates with `--keep-live`, because a
fresh container credential per daemon start would kill in-flight workers on every `setup.sh`. A
session holds at most 256 live tickets and the daemon 4096, expired in mint order.

### The harness vouches; the agent holds no secret

An in-process subagent cannot keep a secret from its parent: one process, one sandbox, one
transcript. So Claude Code's `PreToolUse` hook, which the harness feeds and the model does not,
mints a ticket per `jkb` tool call with the container credential, carrying the `session_id`,
`agent_id` and `agent_type` the harness reported, and rewrites the command to export it. Running
workers as separate `claude -p` processes was the second draft and was rejected: attestation holds
in-process subagents to their role without restructuring the swarm. A single-use ticket that
expires in seconds was rejected too, because it breaks multi-op commands; a ticket is bound to its
tool call's lifetime instead.

### What the harness reports, measured

Measured on Claude Code 2.1.276: `agent_id` and `agent_type` appear for Agent-tool and
Workflow-tool subagents and not for the main session; `updatedInput` reaches the shell;
`PostToolUseFailure`, timed-out commands and `SubagentStop` all fire; one command cannot read
another's `/proc/*/environ`; deny rules held from Bash, `Read`, a symlink and a subagent. Two
measurements changed the design. A workflow agent reports `workflow-subagent` unless its script
passes `agentType`, so roles come from explicit types and the generic ones map to nothing. And a
`PreToolUse` can be followed by no post-hook (a permission refusal after the hook ran), so tickets
are also released at `SubagentStop` and `SessionEnd`, with a 10-minute backstop. Only a ticketed
call is released at `PostToolUse`, and every hook client carries the hooks' deadlines and down
marker (`remote::client`).

### What the hook decides: allow, defer, or stay out

The hook mints the ticket and approves, or stays out of the way; it never forces a prompt. What a
ticketed `jkb` may do is the daemon's RBAC, held against the ticket on every request. Three
answers:

- **`allow`** for a line whose every command is `jkb` itself (the command word literally `jkb`) or
  one of a short `HARMLESS` allowlist: `cd`, `true`, `false`, `:`, `echo`, shell builtins with no
  path to run other code, read or write a file, or repoint `jkb`. So `jkb task show x` and `cd
  repo && jkb workflow next` never prompt, in any mode.
- **no `permissionDecision`** (deferred) for any other line that mentions `jkb`: one that also runs
  something else (`jkb ls && git status`), one the classifier cannot model (a redirect, `$`, a
  glob, a bare tilde), one that reaches `jkb` by a path, a prefix or a wrapper
  (`~/.cargo/bin/jkb`, `FOO=1 jkb`, `sh -c "jkb …"`). The line is ticketed, and the session's own
  rules judge it against the command as the model wrote it. Approving it would approve the rest of
  the line past whatever rule the person set for that.
- **nothing at all**, and no ticket, for a line that only mentions `jkb` as a harmless command's
  argument (`echo jkb`). A reader's mention (`grep -rn jkb src`) is deferred and ticketed, since no
  reader is on the list; the unused ticket costs nothing.

Only an `allow` overrides the session's rules, so only an approved line has to be understood, and
the property the tests hold is exactly that one: an approved line runs nothing but `jkb` and
harmless commands. Which `jkb` commands are deferred is `remote::beyond_rbac`, an exhaustive match
beside `remote::support`, asked of the words as `jkb`'s own clap parser reads them; words that
parser refuses are deferred too, except help and the version, which it reports as errors but
which read nothing. `HARMLESS` is pinned as a literal list, since growing it is the dangerous
direction. `sort`, `printf`, `sed`, `awk` and `uniq` are deliberately off it
(`--compress-program`, `printf -v PATH`, `e`, `system()`, `uniq IN OUT` overwriting a binary).
A ticket inherited by another program on a deferred line can do only what its role may.

### No reader is approved, and neither is a `jkb` that reads a named file

No reader is on the allowlist, by the user's decision (2026-10-02): with `cat`, `grep`, `head`,
`tail`, `wc` and `jq` on it, `jkb ls; cat ~/repos/other/.env` was approved and read a file past
any rule the person had for reads. So `jkb … | jq .status` is deferred: under the auto posture
still unprompted, under stricter rules the person's. The same decision covers `jkb` where it reads
a file the caller names and sends the daemon only the text: `jkb ingest <path>` (a URL is
rendered, and stays approved), `jkb mcp` (its `ingest_path` tool), `jkb task review file --from
<file>` (from `-` stays approved). RBAC judges the op, never which file fed it, so an approved `jkb
ingest ~/repos/other/.env` would read past a read rule exactly as `cat` did, and the container
sees every project under `~/repos`. An inline gate is deferred for the same reason: `jkb task land
x --gate '<cmd>'` runs a caller-written command with `sh -c`, so `--gate 'cat
~/repos/other/.env'` is `cat` again. `jkb task land x` with the gate the host stored stays
approved.

### Landing needs no prompt

Each prompt the hook once forced on landing stood in for something already covered. Who may land
is `may_land`, asked by `task.land_check` before the target moves. What an inline gate runs is
deferred to the person's rules, as above. A stored gate cannot be set from the container at all
(`remote.rs` refuses it), and `--gate-on-host` needs `task.ran_on_host`, which is operator-only.
And `main` changes only through a PR, whose CI is the verification that counts; the in-sandbox
gate is a fast pre-check a session can weaken only for itself.

### The ticket travels in a rewrite, because each Bash call has its own PID namespace

The rewrite goes out for every ticketed class, because that is how the ticket reaches `jkb`, and
it is a genuine per-tool-call secret: each Bash tool call runs in its own PID namespace (measured:
from a second call, the first call's processes are invisible to `ps` and its `/proc/*/environ`
unreadable). So the ticket cannot be left in a file for `jkb` to find: any Bash call could read the
file, and an in-process subagent could steal one minted for a different `agent_type`, which is
exactly what the harness vouching exists to prevent, since ancestry cannot tell a subagent from its
parent.

### A deferred line keeps its ticket, and rules see the original text

Measured on Claude Code 2.1.283, with the hook rebuilt and re-pinned, the two questions that gated
this design. `updatedInput` is applied with no `permissionDecision`: `cd /tmp &&
~/.cargo/bin/jkb role whoami`, deferred, arrived with `JKB_ATTEST` set, and the daemon answered
`coordinator` rather than `Unauthorized` (the schema marked the field optional, but that was read
from the installed bundle; this is the observation). Had it gone the other way, every deferred call
would have lost its ticket. And the rules match the original command, not the rewritten `export
JKB_ATTEST=…; cd … && jkb …`: a deny-rule probe (`Bash(export:*)` in `permissions.deny`, then one
deferred call) produced an approval prompt rather than a refusal, and in an interactive session
`cd /tmp && jkb role whoami` ran with no prompt at all. A `Bash(jkb:*)` rule is matched against the
text the model wrote. A trap met while measuring: a probe that itself contains `$` (`echo
${JKB_ATTEST:+present}`) is not approved by design, which looks like the fix failing and is the
guard working.

### Which characters keep a line from approval

The rule turns on whether the shell would read a character as syntax. `$`, a backtick and a
backslash keep a line from approval anywhere, quoted or not: they are what the word reader cannot
model (the first two substitute inside double quotes, the third escapes the quoting itself), so
with any of them present its output models nothing. Every other metacharacter matters only bare;
inside either kind of quote the shell passes it through as text. Bare, the separators `; & |` and
a line break split the line into commands, each judged on its own; `< > ( ) { } # * ? [ ]` leave
nothing the classifier can model, and `!` and a carriage return are read as word breaks, so a line
carrying any of these bare is not approved. A `~` has no quoted spelling that still expands: a word
holding an unquoted `~` anywhere is accepted only when it also carries a `/`, so whatever the tilde
expands to, the `/` survives. "Anywhere" is load-bearing, because bash expands a tilde after an
assignment's `=` and after a `:` in the value too (`a=~` becomes `a=$HOME`). This is why jkb's own
quick-add syntax (`!p<n>`, `#<facet>=<value>`, `?`) is approved when quoted: `jkb query
'status!=done'` and `jkb task add 'Fix it !p1 #area=hook'` are ordinary calls.

### Word separators are blanks, and a bare tilde is a variable

Two measurements are why the property is stated as the command word rather than as `PATH`. A word
separator is a blank, not Unicode whitespace: bash breaks a command line on space, tab and newline,
and the caller's `IFS` does not change that (`IFS` splits the result of an expansion; measured on
GNU bash, `IFS=x` still passes `axb` whole). While the reader split on `char::is_whitespace`,
`jkb<NBSP>./x` read as the two words `jkb ./x` and was approved, while bash ran the single relative
path `jkb<NBSP>./x`; a command word containing `/` is never searched on `PATH`, so a writable
working directory was enough to run an arbitrary program. And a bare tilde is an ordinary
variable: `~` is `$HOME`, `~+` is `$PWD`, `~-` is `$OLDPWD`, so under `HOME=land` a bare `~`
expands to exactly `land`, which would have carried `task land` and its `--gate` past the hook.
`cd` is harmless only because `PATH` holds no relative or empty entry, so the directory cannot
change which `jkb` runs: measured on the container's real `PATH`, eleven entries, all absolute. (A
first measurement, ten entries, was of the Mac's `PATH`, which a stray `env.PATH` in the shared
`.claude/settings.local.json` had substituted for the image's; same conclusion, wrong `PATH`. That
substitution is what a session sees when `jkb` will not resolve by name.) The residual: the hook
checks the command word, so a `jkb` shell function or alias in the invoking shell still shadows the
binary, and neither the hook nor its tests can see that.

### Approval is an allowlist, held by tests that run bash

Whether a line runs a `jkb` the model never saw cannot be read off its text: `sh -c "jkb task land
x"`, `… | xargs jkb task`, `env -S "…"`, a glob inside quotes, a script file, `make`, `jkb ls |
./evil.sh`, `LD_PRELOAD=` and `PATH=` prefixes, `hash -p`, `printf -v PATH`. Naming the dangerous
cases kept falling short; naming the safe ones did not, which is why approval is an allowlist. The
bash-backed tests run those lines to hold that none is ever approved:
`run_through_bash_a_line_that_runs_jkb_out_of_sight_is_never_approved`, whose oracle puts a fake
`jkb` program on `PATH` as well as a shell function, and
`run_through_bash_a_redirect_comment_or_heredoc_is_never_approved`.

### The hooks run a pinned binary

The hooks run `/usr/local/lib/jkb-hook/jkb`, root-owned, because they run outside the sandbox with
the credential readable, and `~/.cargo/bin` is writable from inside it (measured).
`pin-jkb-hook.sh` copies setup's build there through `sudo`, which a sandboxed command cannot use
(measured: `NoNewPrivs: 1`, `sudo -n` exits 1). The residual: it pins whatever `~/.cargo/bin/jkb`
is when it runs.

### Rollback is an environment variable

`JKB_ATTEST_DECISION=ask` forces the prompt on every ticketed class with no rebuild; the hook
binary is pinned and root-owned, so a rollback that needed one would not be a rollback. It is read
from the hook process's environment, and the harness spawns the hook, so on the host it goes in the
environment Claude Code is started with. Inside the dev container the only place is `containerEnv`
in `.container/container.json`, which is fixed at create, so it needs a recreate. Worth knowing
before the moment it is needed.

### What is not protected, stated

Agents on the host run as the operator. A genuine worker told to lie by a hostile coordinator (a
real reviewer filing a clean review on instruction) is mitigated, not prevented, by operator-owned
agent definitions, rounds recording the attested reviewer, and landing staying the operator's
under the default strategy. Git run by hand in a repository planted between reap passes is not
covered, nor are the hooks' binary as it was when last pinned. `/task-swarm`'s workflow agents
hold no role until its script passes `agentType` (its `.claude/workflows` file was read-only to
the session that built this); until then `jkb role map workflow-subagent coordinator` is the
explicit, visible way to keep it working with worker isolation off.

## Hooks-off git: the host never runs container-written code

Git on the host runs what a repository's own `.git/config` names, and the container can write
that file. So jkb's own git, which runs on the host, must never run what the container planted.
There are three layers, measured on Docker Desktop 29.7.2 and git 2.51.1. The audit is what
holds; the binds are a speed bump. Which mounts the container gets is the sandbox-and-container
design (and `.container/README.md`); this part records the git side.

### The container's view, in one place

The container no longer holds the daemon's root token or the knowledge base: only
`~/.jkb/{claude-memory,logs}` and a read-only coordinator credential are bound. Every
repository's `.git/config` and `.git/hooks` are bound read-only, because the host's git runs what
they name. The attestation hook runs outside the sandbox by design: it is the harness vouching for
which agent made a tool call, and it is the only reader of the credential.

### Read-only binds are a speed bump

`run.sh` binds each repo's `.git/config` and `.git/hooks` read-only, and a rename over the bind
fails with EBUSY. That does not make writes fail closed. `.git/` itself is writable around the
binds, so the container can rename `.git` away and put a writable copy in its place, or plant
`.git/commondir`, which redirects git's config and hooks to any directory (measured:
`rev-parse --git-common-dir` then answers the planted one). An empty directory there is no defence
either; git dies on it (measured). Submodule configs (`.git/modules/*/config`) are not bound at
all.

### Every jkb git call audits fresh and runs hooks-off

Every git call `gitrepo.rs` makes goes through one spawn (`git_in`), which passes `-c
core.hooksPath=/dev/null -c core.fsmonitor=false`, and audits the repository fresh, before every
call. That flag pair does not stop a planted filter, so the audit is what holds. It judges every
key of the repository's own config (local, worktree and included files) with `git config --list
--show-origin --show-scope`, which executes nothing.

### jkb's git never enters a submodule

jkb's git passes `-c diff.ignoreSubmodules=dirty` and friends, plus `--ignore-submodules=dirty` on
every `status` and `diff`, on the command line because a tracked `.gitmodules` can set
`submodule.<name>.ignore=none`, which outranks the `-c` (measured on git 2.51.1: a superproject
`status` ran a filter planted in `.git/modules/sub/config`, and with the option it did not, still
reporting a moved submodule commit). `checkout` and `switch` run `--quiet` too, because their
report of local changes enters every populated submodule, and a branch can bring a submodule only
the target tree's `.gitmodules` names (measured: the planted filter ran on `checkout --detach` and
`switch`, not with `--quiet`). And jkb's git runs only the subcommands measured not to enter a
submodule (`SAFE_SUBCOMMANDS`; `add -A`, `stash`, `cherry-pick` and `diff-index` did), refused at
runtime otherwise, so a new subcommand arrives with its own measurement. The cost: a submodule's
uncommitted edits no longer make a checkout read dirty, and a graft carries only the submodule's
commit anyway.

### The config allowlist names keys, over the repository's own scopes

The audit refuses a repository whose own config sets a key outside an allowlist of
repository-shape keys. The allowlist names keys, not sections: `core.worktree` points checkout at
any directory (`$HOME` included, measured) and `status.showUntrackedFiles=no` hides what a landing
left, so both are refused, except a submodule git directory's own `core.worktree` landing inside
its repository, which git writes itself. Only the repository's own scopes are judged: `local`,
`worktree`, and an `unknown` one whose file lies in the repository. Apple's git reads an extra,
Xcode-owned layer (`/Library/Developer/CommandLineTools/usr/share/git-core/gitconfig`,
`credential.helper=osxkeychain`) and lists it as `unknown` (measured on git 2.50.1, Apple
Git-155); judged as the repository's, it refused every repository on a Mac, found by the land
gate's tests on the host before anything landed.

### A git directory must take its config and hooks from its repository

The audit refuses a git directory that takes its config and hooks from anywhere but its
repository. A main repository's git directory is its own common directory; a linked worktree's is
`<common>/worktrees/<name>`; and a jkb session's common directory is its repository's `.git`, or,
for a repository nested in the session, one of that repository's own submodules. This is
`check_layout`, and it is what defeats a planted `commondir`.

### The reap scan covers what your git reaches

The reap service runs the same check on every repo and session worktree each pass, plus what only
your git reaches, and posts a sticky notification for what it finds. It is the only layer that
covers the git you run by hand. It judges each submodule git directory's own config (real git
directories only, top-level `config` only, symlinks reported, `core.worktree` judged through
symlinks) and each submodule checkout's `.git` link. It follows what your git follows (measured on
git 2.51.1): a symlinked `.git` is reported; each submodule's own `.gitmodules` is read in turn to
`MAX_MODULE_DEPTH`, deeper being reported; and an un-absorbed git directory's own `modules/` is
walked.

The `.gitmodules` it follows is hostile input. A path at or above its own checkout is reported,
not followed (`path = .` twenty times grew the scan as 20^depth); each checkout is read once; and a
`.git` file naming a git directory the walk does not reach (`modules/<a>/<b>` under `modules/<a>`)
has that directory judged directly. A `commondir` in a submodule's git directory is reported, not
read past (measured on git 2.51.1: a filter planted in the directory it named ran on `git -C sub
add`), the same redirect `check_layout` refuses for a repository jkb runs git in.

### Submodule hooks are reported unless they match your template

Any hook in a submodule git directory's own `hooks/` is reported (a planted `pre-commit` ran on
`git -C sub commit`), except git's `*.sample` files and hooks byte-identical to your
`init.templateDir`'s, which git copied in. The template directory is read from the environment or
the global or system config, following `[include]`s as git does, never from the repository's.
Each template hook is judged where it really is, through every symlink: one that resolves beside
the repositories is the container's to write, so it does not vouch for a submodule hook. Such a
hook's finding says it matches your template and where that template really lies, so the fix
reads as moving the template, not deleting your hook. A `hooks/` the scan cannot list is reported
too, since git needs only to search it to run a hook by name.

### Not covered: the repository's own `.git/hooks`

The container sees a repository's own `.git/hooks` read-only through the bind, but a container
that replaced `.git` wholesale could plant a hook there that neither the audit nor the reap scan
reports. Nothing can tell a planted hook from one you or `setup.sh` installed when the container
writes both the hook and anything it could be compared with.

### The land gate runs in the container

The land gate, which runs the candidate's own code, runs in the container through `docker exec`
(exit status and output measured intact), settled before the graft, for a repository under
`~/repos`. The decision is made on canonical paths: git reports a checkout by its physical path,
so with `~/repos` a symlink, the old textual prefix test never matched and every gate ran on the
host. A landing checkout the container cannot see, of a repository it shares, is refused rather
than run on the host. `--gate-on-host` is recorded.

### The installer's `post-merge` hook is unaffected

The audit lives in the same seam as the git-hooks-installer design's repository-selection scrub
(`gitrepo.rs`). The `post-merge` hook that installer places is unaffected: it is your `git
pull`'s hook, not jkb's, and it runs landed code.

## History

Superseded and reversed decisions, each with what replaced it and why.

### An LLM RESOLVER integrated every branch

The first swarm merged each branch through a standing RESOLVER agent (`resolverPrompt`), with
DESIGNER or RESOLVER re-dispatch on conflict, marking a task `done` the moment it merged into the
coordinating branch, with no review. Every integration spent tokens and latency even when the
branch merged cleanly and the gate was green, which is the common case, and a conflict was really
the implementer's to rebase away. Replaced by the deterministic merge queue and the REVIEWER stage.

### One implementer per task, behind a per-round barrier

The first loop fanned out one IMPLEMENTER per ready task and waited for every one with `await
parallel(...)` before integrating. Overlapping tasks collided at integration, and one slow
implementer stalled every fast one. Replaced by SCHEDULER work-groups and the pipeline.

### `needs_review` unblocked dependents

`TaskStatus::unblocks_dependents()` once returned true for `NeedsReview`, when it meant roughly
"merged, awaiting the operator". Once it came to mean "a reviewer is reviewing this branch", a
dependent could start against work that might still bounce. Replaced by the terminal set `{ Done,
Cancelled }`.

### Claims were logged but not undoable

The fleet design recorded claim, release and reclaim in the changelog "for inspection, not
auto-reverted", because undo then inverted only inserts. Undo since gained `undo::INVERSES`, which
inverts all three by restoring the logged claim columns.

### The merge queue's completion step closed tasks with `task set --status done`

The fleet design had the queue mark a group `done` with `jkb task set <uid> --status done`, and the
workflow ran a second, `haiku`-model agent (`completePrompt`) told to "mark every task in the
group done". Measured on 2026-10-08, twice in one chain: each time that agent ran after the queue
had already closed its group, and set the next task `done` (`open -> done`, an `override`, 42 s
and 75 s after the landing); the swarm then started that task's dependents on work that did not
exist, and a later implementer caught the second only because a note on its task told it to stop
and say `BLOCKED:`. The agent's `task set` had also been quietly closing groups past the default
strategy's rule that only the operator lands. Replaced by the queue recording the landing with
`jkb task landed` and a check-only step (`unclosedTasks`).

### `/review-log` wrapped the host's `/code-review`

`/review-log` ran the built-in review and persisted its findings. The built-in reports to the user
and returns nothing, so the middle step was a hole. Replaced by our own reviewer workflow with two
thin callers.

### Staged verification, described as plain 2-of-3

Verification was first described as plain "2 of 3" and then built staged: one decisive skeptic
(*does it reproduce?*) whose refutation was fatal, and only survivors that were neither nits nor
quality faced the other two, needing both to refute. Together with the burden-of-proof change,
the per-verdict refuted rate moved from 18% to 26%. Replaced by batching skeptics by file, where
each defect batch faces all three angles, making the vote a true 2-of-3 at roughly the cost of the
staged version.

### Two tiers and no `medium`

When verification proved to refute only 6% of findings, the reviewer had two tiers, `low` (lenses,
feature reviewers, consolidation, ranking, unverified) and `high` (plus the vote), with no middle,
because the natural middle (one skeptic) was the worst of both. That `low` was the full
fan-out (nine lens reviewers plus feature reviewers, about 3M tokens and an hour per run), with an
effort-scaled per-reviewer cap (3/5/8) quoted at about 30 agents per 1,000 diff lines. Replaced by three tiers on the axis of fan-out breadth:
the old fan-out became `medium`, and the new `low` asks every question from at most three
reviewers. There is still no single-skeptic tier.

### Quality findings capped at nit; severity by target proportion

The first reviewer capped every quality finding at nit to stop structure crowding out defects,
which is wrong in the case that matters: a structural problem sometimes is the most important
thing in a change. Replaced by pricing quality on evidence. A later ranking prompt priced
`concern` as meaningless when most of a run shared it and capped `must-fix` at "about a fifth", a
rule about the shape of the set rather than any finding in it, which pushed both ways (inflating a
finding so it got read, deflating one because its tier was crowded). Replaced by the per-finding
"would you hold the merge?" test and a strict order.

### Review rounds read live, ordered by the newest finding

A round was first read live and ordered by the highest item id among its findings. A finding's
priority, placement and `area=` are ordinary task content, so the implementer under review could
lower its own last round's must-fix, or file a line into an older clean round so it sorted newest.
Replaced by snapshotting a round at its first recording. A second pass also counted anything live
at `p1` under a recorded round's namespace, reachable by a line synced into a mounted round's
`tasks.md` or a task placed beside the findings; replaced by counting only the snapshot.

### A coordinator could file, record, or mint reviewers

Recording was first open to any principal over any namespace. A coordinator could file an empty
round and record it against the branch it drove, satisfying the gate with no reviewer involved;
once that was refused, it could undo the refusal with `role grant reviewer` to itself, since a
token it mints is one it holds. Replaced by rounds belonging to a reviewer's own filing or the
operator, and reviewers coming only from the operator. A recorded round could also be recorded
again under another name and snapshotted as newer, reading today's priorities; replaced by one
name per round.

### Workflow pinned only on `workflow set`, and reopen reconciled by any observe

A task first pinned its strategy only when `workflow set` named one, so a redefined `default`
changed a task already in `landable`; replaced by pinning at the first move. An
`observed_reopened` reconciliation let any observe follow a lifecycle reopen, so a coordinator
could reopen a landed task; removed, leaving reopen the operator's. Reopening the status and
landing again then re-landed a task with its workflow never reopened; replaced by refusing anyone
but the operator a landing while the workflow is parked.

### A landing parked the workflow only on observe; a cancel parked inside itself

A really landed task sat at `landable` until an explicit `workflow observe`, so the parked-workflow
refusal never fired, and a PR closing without a merge never parked at all; replaced by
`store::follow_landing` in the landing's transaction. Parking a cancellation, and revoking the
task's workers, inside the cancel made it one-way for all but the operator when `jkb undo` or a
returning `tasks.md` line restored the status; replaced by parking a cancellation on observe.

### Re-landing refused, and `task landed` aborting on the first refusal

The same landing run again was at first refused as "held", which broke the merge queue re-running
a branch; then admitted too broadly (a cancelled task ticked `done`, a landing elsewhere, an
operator landing while parked at `cancelled`). `task land --keep-worktree` recording the pre-graft
tip made the queue's own advice fail on a branch already in its base. A refusal aborted `jkb task
landed`'s loop over the branch's tasks, and a caller who could land none of them saw `recorded:`
over nothing. Each was replaced by the narrower rules above.

### `Request::target` had a wildcard arm

A `_ => None` arm in `Request::target` admitted `removal.add` naming another task's worktree and
`lease.take` displacing the merge queue as unscoped. Replaced by an exhaustive match with no
wildcard.

### The Land permission was asked only at the record

`task.land`, the record and last step of `jkb task land`, asked the Land permission first, so a
caller the strategy does not let land got its branch grafted, its gate (a caller-supplied `sh -c`)
run and its session archived, and was refused only the record: landed in git, not in jkb.
Measured, not inferred: with the new check removed, the regression test's target moves and its
worktree is archived. A task-scoped grant was already refused the land lease before the graft, but
an unscoped caller who may not land reached it, and under `design-reviewed` that is the
coordinator, the role a main session is attested as. Replaced by `task.land_check`.

### Emptying the grant cache on every grant or revocation

The daemon once emptied its whole grant cache on every grant or revocation, so every live token
missed at once, and the misses during the refresh were refused, the container credential's
attestation calls among them. Replaced by a minted grant refreshing at once and every call
re-resolving from the database.

### The hook forced prompts

The hook used to answer `ask`: on `task land` and `task gate`, on lines it could not model, and on
lines where a `jkb` might run out of sight. A `PreToolUse` `ask` overrides an allow rule, so a
mechanism meant to be invisible kept putting prompts in front of the person, including on commands
that only mentioned the repo's path (`cd /home/vscode/repos/jkb && … 2>&1`). Before that, the rule
was `ask` for anything that was not one plain invocation, which made every `cd repo && jkb …` and
`jkb … | jq` prompt; declining to answer was then applied to every non-plain line on the reasoning
that a ticket "costs nothing on the security axis", and refuted when review showed the ticket is
exported for the whole line, so every other program on it inherits the authorization. The user's
rule of 2026-10-02 reversed all of it: the ticket is authorization, RBAC decides, and the hook only
approves or stays out. Landing's prompt had been justified because `--gate` runs a caller-supplied
command; the reversal answers that with the sandbox and CI, and later with deferring an inline
gate. A residual from that era: approving an inline `--gate` because "it reaches nothing the agent
could not reach itself" used the sandbox's reach as the bar, the very bar the reader decision
rejected, which is why the inline gate is now deferred.

### No metacharacters, even inside quotes

The first approval rule refused any line with separators, pipes, redirects, substitutions or
expansions, even inside quotes. Reversed by a measurement: it put a prompt on jkb's own quick-add
syntax (`jkb query 'status!=done'`, `jkb task add 'Fix it !p1 #area=hook'`), and because an `ask`
overrides an allow rule, anyone with `Bash(jkb:*)` newly saw a prompt on the most ordinary calls.
Replaced by the rule that turns on whether the shell reads a character as syntax.

### Readers on the allowlist

`cat`, `grep`, `head`, `tail`, `wc` and `jq` were once on `HARMLESS`, so `jkb ls; cat
~/repos/other/.env` was approved and read a file past the person's read rules. Removed by the
user's decision of 2026-10-02, together with approving `jkb` commands that read a named file.

### The git audit ran once per directory per process

The audit first ran once per directory per process, so the reap service, one long process, never
re-read a config it had passed. Replaced by auditing fresh before every call. A later version
walked every file named `config` under `.git/modules` in the blocking audit, which read loose refs
as configs, missed per-worktree `modules/` and redirected submodule `.git` files, and was fooled by
symlinks; replaced by jkb's git never entering a submodule, leaving submodule git directories to
the reap scan.

### The whole template directory vouched for submodule hooks

The reap scan first judged the template directory as a whole, which a single hook symlinked into
`~/repos` stepped around, and the standing finding it then raised against every repository hid any
real one behind an unchanged summary. Replaced by judging each template hook where it really is.
