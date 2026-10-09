<!-- generated from jkb design design:task-lifecycle-18dcdeffaafbcfd87c069f, edit there (version 112.b_2I6IndsJEGAYGdu_yU_awDAYKo9cTruwUBgbCvyvXfvwIBhIWS9IeapA0Bhae59vPx-AoBhqHwycuzxAoBh4fWg8q79Q0Bh8rc4biU9QIB_dakx6mh3w0Bit-6l4Wprw0B_9yx8p6JmwoBjOSnvIrrzQ4BjdHWvb2O-A4Bjvz45fSu2A0Bj5fqg9GohQcBjcfDyc_llg8BkMil2eDEzAMBjLzKt5Cd2AUBkrivg62IuQ4Bk7Hmg4uU2AkBk5PmlsyXlgkBk7-h2qSg3ggBlqmrgaP4qgKs5wWWksvI8uSYAQGWlPaCtJjSBAGZqdfqnNn-CAGX8pONqO27CQGYn8LK3t-nCwGPlMv7uIeDBgGa8eSp-v2-BAGe4NTBkYbyAwGXpeGpjb_VBAGg1bamxrlnAaH25Z3ihbgNAaKftbHfki4Bo4PB_PfpsAwBotCO-afV7wgBpdmYyJPIvgsBjdLbv9f-9wQBm4berKa5_gYBqImgkqXz8Q0BqOjc06jB-QIBqaOkiNPXpwoBqbGkpNyznAIBrJKai5vD2gIBrcLivemFyQQBrvHo_pyBeAGup5_Mj4LBCAGyhauIiO-UDwGzvNf8sufoBQG02eeoz_eAAgG1_tTRkKm8DAG25o6mx7-gCQG32bXx-an3DAGyqqPxusaJDAG0yZ7Jn8PFCAG2yqOFramlBAG7kcGOj47YDgG2mIydl8-BCwG-pfjsvNPRAwG-9rHhkZbbBgG-9aO5xPbECwHB_NywyJvEDAHC5au0qq-dCAHDqtq2y9cIAcTUju2Aq-wOAcPo1bOl-6kJAcamob7b7MYJAb_Hr8rb67YOAcjJhK_2u_IOAcfkoqiMzCIBzMPftPCwyQgBzZHw3LGP4A0B0Iis5ryurwkB0Lu07_eSWAHUrJDtv9W8BQHVxoS9sZu3DwHVjsmlu4SCAwHan4mk1JWmBwHbht3ZyYi0BAHc27ajqYWtAwHfh9baiOzxCgHgnaay18bLAwHh2r2Nyo2bCwHi2qKqqc-iCQHg5ubBg-q1BwHgjs3o_5PNAgHlwZbF2s7CCwHljaWUkvyeCQHflfjBpdb3CwHqzM_yv_eFAQHqivWquYT5BgHsu82uhsiICwHur6L-yN7zDgHw95Dh9o-xAwHx0avxlrPECAHw0vjVq-XJCwHzsrTDhuLeBAHz18zIqq7BCgH0tLer_qPwDQH1_q7ZrfrxBAH3196ovJTtCwH2osPmhaWADQH27JXrzch6AfiFw7jkwtkMAfjdwoL3kssCAfq3mcTenPAIAfLLiMue2qUKAf6569Hd14YJAfmPho6hqscHAQ, blake3 f78cadd481ecfdd42a25823d883f49b2f16bdbc0e351fa8b6bf18c3ce370de9e) -->
# Task lifecycle

How a task moves from the frontier to merged. A task is decomposed into subtasks, so the
frontier hands out leaves. Work on one runs in its own git worktree session, on a branch
that lands on a staging branch before trunk. Landing is gated on a recorded review, and
closing a task whose work reached trunk is automatic but conservative. Underneath all of it
the lifecycle is a declared, checkable state machine (`crates/jkb-fsm`), and every move a
task makes is appended to a transition log that never has to be reconciled with the world.

Two rules run under every decision here. **Of the two ways to be wrong, the recoverable one
wins**: a missed close costs one command, a wrong close buries unfinished work, so every
inference about git degrades towards holding a task, never towards closing it. And **a fact
git owns is asked of git**, never copied into the store and then reconciled.

Neighbouring subjects live in other designs. Roles, RBAC, task workflows, review rounds and
the design gate that keeps undecided work away from the swarm are the agents-and-roles
design. The land lock's lease rules are the daemon-and-messaging design. Installing the git
hooks is the git-hooks-installer design. The sync journal's own rules are the file-sync
design, and investigation units are the namespaces design; both appear here only as the
second and third users of the state-machine library. The changelog and `jkb undo` are the
foundation design.

## Subtasks and containment

Two chores were manual: re-running `setup.sh` after a pull, and closing the tasks whose pull
request had landed. Both were forgotten, and the second silently rotted the backlog, since a
task that shipped weeks earlier still sat on the frontier. Automating the close needs a task
to know which branch would complete it, and automating it correctly needs a way to say "this
branch only does part of this task". That is what subtasks are for, so the three landed
together.

### A subtask is a `parent_of` edge, and the frontier hands out leaves

A subtask is a `parent_of` edge, parent to child. The edge type already existed and the
`tasks` serializer already wrote it from indentation; nothing read it. The load-bearing rule:
**a task with a non-terminal child is not on the frontier.** You work the leaves; the parent
is a container, not a unit of work. This is one anti-join (`SUBTASK_CLAUSE`), the same shape
as the existing `depends_on` one.

It is added to **both** `is:ready` and `is:frontier`, identically. Those two must stay
equivalent for tasks (a task's `resolution` is always NULL, so the frontier clauses collapse
to the ready clauses; see the namespaces design), and putting the rule in only one would break
that the moment a task had a child. `jkb task add --under <uid>` creates a subtask, inheriting
the parent's home; `jkb task show` lists the subtasks and says why the parent is held.

### No status rollup: closing is a separate, git-triggered decision

A parent does not flip to `done` when its last child does. Auto-close is a separate,
git-triggered decision (see "Auto-close is conservative by construction"), and two mechanisms
racing to close one task is how a task closes for the wrong reason.

### Containment is a relationship between items

`containment(child_item_id PRIMARY KEY, parent_item_id, position)` is its own table, keyed on
the **child**, because "X is contained by Y" is a property of X and not of one of X's several
placements (a home plus the `tasks/<repo>` mirror). The primary key makes *at most one
container* structural rather than conventional; the rows are sparse; a separate table keeps
the hot `items` table narrow; and depth is free, since the same adjacency list nests three
levels or thirty.

`placements` is untouched and still carries `namespace_id`, because namespace scoping
(`ns:tasks/**`, which `task next` resolves through) must keep finding a contained item.
Listing and scoping ask different questions and both stay right: a subtask **is** in
`tasks/jkb`, but it is **listed** under its parent. Accepted deliberately: a contained item
is listed under its container and nowhere else, even when it is homed in another namespace. It
is never unreachable, since expanding the container always reaches it, and a test pins both
halves of that.

Rejected: the parent becoming a namespace. It derives a path from a mutable title, the
identity failure the sync `prose` blocks had already taught ("that identity cannot survive an
edit"). It grows the organizational tree with content: 607 ingested episodes would become 607
namespaces, mixing folders a person chose with folders a parser produced. And the `tasks`
serializer already turns `##` headers into namespaces, so parent tasks as namespaces would put
two meanings in one file for the three-way merge to disentangle. The other rejected model,
`placements.parent_item_id`, was built and replaced; see History.

### The edges survive beside the containment row

The `parent_of` and `derived_from` edges carry what a containment row cannot: `edge::link`'s
cycle guard, `jkb related` traversal, `derived_from` as the provenance search reads for
`source_document`, and the `tasks` serializer's indentation round-trip and three-way merge
`Sig`. `task::add_subtask` writes the edge and the containment row in one call so they cannot
drift.

### Containment is a behaviour, not a node kind

A *pure namespace* is a node that only contains; a parent task both **is** a task and
**contains** its subtasks; a document **contains** its chunks. So `jkb ls <path-or-uid>` is the
one container read: it resolves a namespace first (the historical meaning) and falls back to
an item uid. `jkb tree` descends into **any** child with `has_children`. `jkb task subtasks` is
a thin alias for discoverability, not a second implementation, and the UI passes a node's
address and does not branch on kind.

The two containment edges stay distinct where it matters: a task *decomposes into* subtasks
(`parent_of`, authored), a document is *fragmented into* chunks (`derived_from`, generated and
rebuildable). A chunk of a document is not a subtask of it.

### A contained node is listed once

`--under` homes a subtask beside its parent, and ingest places chunks beside their document,
so either would otherwise appear both as a namespace sibling and nested under its container.
`ls` hides it **only where its container is in the same listing**. One homed elsewhere keeps
its own row, because hiding it there would make it unreachable rather than merely
un-duplicated.

### Chunks are nested, not flag-hidden

Chunks used to be dropped from listings unless `--all`. Now they are reached by expanding
their document (`jkb ls <document-uid>`, in document order via the `chunk` placement's
`position`). `--all` no longer re-flattens them, which would reintroduce the duplicate; it
governs terminal tasks and whether chunks count toward per-folder totals, a separate question
from where they are listed.

### The explorer shows the hold

A task carries `subtask_count` and `open_subtask_count`, and the explorer row reads `2 of 4
subtasks open`, with a hover saying the parent is held. Without it a container renders
identically to the pickable tasks beside it, which is worse than having no subtasks at all.

### Creating a task from the node you are standing on

Filing a task used to mean leaving the tree, finding the namespace path and typing it back
into `jkb task add +<path>`. Two explorer commands are thin calls to the `jkb task add` that
already exists: **New Task Here** on a namespace runs `jkb task add "<text>" +<path> --json`,
and **New Subtask** on a task runs `jkb task add "<text>" --under <uid> --json`. The input box
takes the **raw quick-add line**, not just a title, so `!p1 @2026-08-12 #area=ui` work exactly
as in the terminal. The UI is a client of the CLI, and a UI that accepted only a title would be
a second, poorer task-creation grammar. `--json` returns the uid, so the tree refreshes and
reveals the new node instead of leaving the user to find it.

Offered on namespaces and tasks only. A document is a container, but `--under` writes a
`parent_of` edge and a containment row, and a chunk is not a subtask.

### Measured: the containment migration takes a second

An early report that `ALTER TABLE` was pathologically slow on the 584 MB database was wrong:
a measurement artifact from concurrent processes contending on copies, with a 584 MB `cp`
counted inside the timing. Measured cleanly, every variant took about 3s and the final `V009`
ran in **1s**. Recorded so the slow-migration claim is not re-made from the same mistake.

## Where the work is happening

A task records the branch and repo its work is on, so the system can find the session and
later decide whether the work landed. This section is about how that location is written; how
landing is detected is in "Auto-close".

### A task's branch is a facet, not a column

`branch=<name>` and `repo=<key>` are ordinary facet tags. The mechanism already existed, needed
no migration, and is immediately queryable as `tag:branch=fix-embed-backfill`. `branch=` is
genuinely item-keyed ("which branch is *this task* on"), legitimately multi-valued, and
round-trips through a synced `tasks.md` line, which is a feature.

The tag is a *hint about where the work is happening*, deliberately not a source of truth: git
and the landing record decide what merged. A stale or wrong tag causes a task not to
auto-close, never a wrong close.

Rejected: dedicated `items` columns (a migration, and two columns meaningful only for tasks on
a table shared by every item kind), and a first-class `branch` item with a typed edge (the
most jkb-native, but it invents a lifecycle to manage for something whose authority lives in
git). Moving `branch=` into table rows was rejected a second time, when the branch-record
table was built (see History): the findings about it were choice-rule defects, and a table
permits two rows just as a facet permits two values.

### `jkb task start` records the location in one command

`jkb task start <uid>` claims the task *and* records `branch=` and `repo=` from the ambient git
repo, plus its land target when `--onto` is given. One moment, one command, so the tag is
never missing on exactly the tasks that needed it. It refuses the trunk branch: a task on
trunk would read as landed the moment it started. It is re-runnable: a second run on a task
already started is a no-op, not a refusal.

### Location facets are set, not added

`branch=` and `repo=` go through `set_facet`, which clears the facet's other values first.
`tag::apply` is additive, which is right for open-ended facets (`design=approved`,
`size=small`) and wrong here: a task carrying two `branch=` values is not better described, it
is contradictory, and readers that collapse the multi-map pick one and mint a second session
for a task that already has one, after which `land` could find neither.

Correspondingly `task_tags` returns **every** value per facet rather than collapsing to one
(collapsing silently picked the lexicographically last), and the session lookup matches a
task's recorded branches against the worktrees that actually exist on disk, so a task that
picked up a stale branch tag still resolves to its real session.

### `jkb task tag set` makes a value a facet's only one

`set` is the sibling of `add` and `rm`. `add` stays additive, honest to its name, since an
open-ended facet legitimately holds several values. `set` is for `branch=` and `repo=`, where a
second value is a contradiction. It is load-bearing because `/task-swarm` re-tags a group on
every pass.

It **refuses `onto=`**, and so does quick-add's `#onto=`: where a branch lands is not a facet
(it is the last `onto` the transition log recorded), so a facet of that name would reach no
reader. Both redirect to `jkb task work --onto` and `jkb task start --onto`. A synced line's
`#onto=` stays inert content on purpose: a stray `onto=` cannot close anything, and rebuilding a
reserved-facet apparatus for it would recreate the machinery the branch-record work deleted.

### The `post-merge` hook runs `setup.sh` only when it would change something

A `post-merge` hook fires on every `git pull` that merges. `setup.sh` is idempotent but does a
full `cargo install` (about 1 to 2 minutes), so running it unconditionally taxes every pull,
including the many that touch only `openspec/` or docs. It runs when the merge touched
`crates/`, `ui/` or `scripts/`, the things `setup.sh` installs; anything else skips it with a
one-line note. The same hook runs `jkb task close-merged`.

Installing the hook so that it actually fires (a global `core.hooksPath` bypasses `.git/hooks`
entirely, and a repo-local hook with no chainer is silently dead) is the git-hooks-installer
design.

## Worktree sessions

The manual counterpart of `/task-swarm`: the same isolation and the same merge queue, driven by
a person instead of a coordinator. Before it, clicking "Work this task with Claude" twice gave
two agents one checkout. They edited the same files, one switched branches under the other,
whichever finished second committed a mixture of both, and neither claimed its task, so a
swarm run or a third click could start the same task again. The swarm had already solved this
for itself (a worktree per implementer, a claim before dispatch, `scripts/merge-queue.sh`
integrating one branch at a time behind a gate), but as agent tooling a person could not use.

### The CLI owns sessions, not a script

A person driving tasks by hand is the ordinary way to use jkb, so sessions belong in the
binary: the UI calls it directly (`jkb … --json`; the UI is a CLI client), it is tested in Rust
with the rest of the CLI, and it works in any repo without copying scripts around. The git work
shells out to `git`, as `gitrepo.rs` does everywhere: jkb does not link a git library, so `git`
with the user's own config and refs stays the authority.

### A session is a git worktree

One task, one worktree, one branch: `<repo>/.jkb/work/<session>` on branch `task/<session>`,
plus `<repo>/.jkb/base/`, a checkout of the land target when it lives nowhere else. Worktrees
share the repo's object store, so a session costs a checkout, not a clone, and every branch is
visible from the main copy. The session name is the task uid's slug, truncated and made
path-safe, with a counter on collision; naming it after the task rather than a random id is
what makes `jkb task sessions`, the directory listing and `git branch` all say the same thing.

`.jkb/` is added to `.git/info/exclude` on first use: locally, not by editing someone else's
`.gitignore`. Otherwise the first session makes the tree dirty, and `land` refuses a dirty
target. Everything a session needs to be found again lives in git and the KB; there is no
session state file to drift.

### The session verbs

`jkb task work <uid>` opens a session or *returns* the existing one. It is idempotent, so the
button cannot fork the work. `jkb task land` integrates it (below). `jkb task abandon` drops it.
`jkb task sessions` lists what is in flight. `jkb task gate` shows or sets the verify command.

### The land target: the branch you started from, or one cut for the batch

`land` fast-forwards a *target* branch, resolved in order:

1. `--onto <branch>`, if given.
2. The land target the task already has, if any: a resumed session lands where it was always
   going to land.
3. The branch checked out in `.jkb/base`, if that worktree exists. This is what makes a second
   session started from trunk join the first one's batch instead of opening its own.
4. Otherwise the branch you invoked from, **unless that is trunk**, in which case a branch is cut
   from trunk, named after this task (the first of the batch), and the session branches off it.

```
main
  └─ fix-ls-counts          ← cut from trunk, named for the first task; the land target
       ├─ task/fix-ls-counts
       └─ task/tree-descends
```

Landing onto trunk directly is refused for the reason `jkb task start` refuses trunk. Cutting
the batch branch also keeps the eventual pull request ordinary: one feature branch, linear
history, no evidence a fleet of sessions produced it. The land target is recorded as the `onto`
of a transition (see "The transition log") and reset by an `abandon`, because where work lands
is a property of the session doing it.

### A land target is a branch, not a revision that resolves

`gitrepo::branch_name` answers `Is`, `Unknown` or `NotABranch`. `branch_ref` maps a branch name
to a ref you may hand to git; `branch_name` maps an arbitrary string to the **key**
`branch_refs` uses, and the two come apart on exactly the values that hurt: `origin/<batch>`
and a tag both `rev-parse` fine, and both were accepted and stored, the first under a key
`jkb staging ls` cannot look up. The canonicalization and the refusal live at
`repo::record_land_target`, the single writer, so the next flag that accepts a branch cannot get
it wrong; the CLI verbs ask the same question first only to print a sentence the user can act
on. Trunk is compared against the canonical name, not against two spellings guessed by hand.

### Landing is the merge queue, one at a time

`jkb task land` is the merge queue's algorithm in Rust, with the same reason for each step:

1. **Rebase detached, never checked out.** `git rebase <onto> <branch>` checks the branch out
   first, which git refuses while the session worktree holds it. Detaching at the branch commit
   does not claim the ref, so it is allowed. `merge-queue.sh` carries the scar from getting this
   wrong: the failure reported as a content conflict and ejected every group.
2. **Fast-forward the target** to the rebased result: linear, no merge commit.
3. **Run the gate on the integrated result.** This earns the whole design: two sessions each
   green alone can break once both are in, and the only place that is visible is after the
   second is grafted.
4. **Red gate or conflict: roll the target back** to its pre-graft tip and eject, telling you to
   rebase in your session worktree. Never resolve a conflict on the user's behalf; the person
   with the context is the one who wrote the branch.

Two sessions finishing at once must graft one after the other, or the second one's gate result
is meaningless, so landing holds the land lock. On green the task is moved to `done` through
the machine's `land` event, its claim released, the session worktree removed and the session
branch deleted, since it has been rebased into the target and the ref is a duplicate.

`graft` rebases a **detached HEAD**, so it never moves the session's branch ref. That is
invisible in the normal path, which deletes the branch, but `--keep-worktree` must point the
branch at the commit that landed, or the kept session reads as N commits ahead of a target that
already contains its work, and landing again re-runs the whole graft.

### The land lock is taken before the checks, not just before the graft

The lock is a database lease (its rules are the daemon-and-messaging design). Taking it before
the preflight is what lets "is the target checkout dirty?" be asked **once**, by
`staging::target_dirty_reason`, the same function the In Flight row renders. It used to be
asked twice, in two wordings, on either side of the lock. The second copy did not close the
window it justified itself with (it had the same gap to the graft), and because both wordings
shared a phrase, the test asserting on that phrase stayed green with *either* one disabled. A
redundant guard that reads as protection is worse than none. `land_dir_for` keeps a dirty check
of its own: it guards the `git switch` it is about to perform across branches, with its own
remedy, and is not a second copy of the land rule.

### `.jkb/base` is a reusable cache, released when its batch is spent

The land happens in whichever worktree already holds the target, else in `.jkb/base`, which is
switched to whatever branch a land needs. `git worktree add` refuses an existing path, so a
second base checkout would wedge landing the moment a batch landed onto a different branch than
the last, until the directory was deleted by hand. Once its batch has merged, `.jkb/base` is
removed: holding a checkout of a dead branch both attracts new sessions onto it and stops
`git branch -d` from deleting it.

### The gate is remembered per repo

jkb runs in any repo, so it cannot assume `./scripts/check.sh`. Resolution order: `--no-gate`
(land unverified, deliberately); `--gate '<cmd>'` (run it, and remember it); the command stored
for this repo; autodetect (`./scripts/check.sh`, `./scripts/test.sh`, `make test`, the first
that exists, **and stored**, so the guess is made once and visible afterwards); nothing found,
land with no gate, saying so. The chosen command is always printed. A gate that silently did not
run is worse than none, because the landing reads as verified.

Storage is `namespaces.metadata.gate` on `repos/<repo>`, via `ns::set_metadata`: jkb-native, no
new table, no dotfile, and per-machine, which is what a build command is. `jkb task gate [cmd]
[--clear]` shows, sets or clears it. Showing a gate runs in the dev container through
`jkb serve`; storing one never does, because it is a shell command the host later runs, so only
the host stores one.

Rejected: a `.jkb/gate` file in the repo (a second source of truth that would want to be
committed, at which point it is per-repo build config, which is not jkb's business), and asking
interactively (the UI calls this non-interactively).

### A session's claim is owned by the worktree; the pid is provenance

A claim is stale when its owner no longer exists, and `jkb task work` exits in under a second,
so a plain `host:pid` owner would be dead on arrival and `jkb doctor --fix` would free the task
while you worked it. A session owner is therefore judged **only** by whether its worktree
exists. Closing the terminal does not mean the task is finished: the half-written branch is
still there, and freeing the claim then is how a swarm run or a second click starts the same
task on a second branch, the exact collision sessions exist to prevent. A stale claim is one
command to clear; a wrong un-claim costs a duplicated branch.

The pid is not consulted, not even as a fallback. It belongs to a process that exited before
anyone could read the claim, so it can only ever be wrong: falsely dead (the original bug) or,
once the OS recycles it, falsely alive for a session `land` already removed. Only `land` and
`abandon`, the two commands that remove the worktree, free a session's claim. Resuming (running
`jkb task work` again on a live session) hands the claim over: the owner is matched on its
worktree, and the claim is re-taken in one transaction. Any *other* live owner refuses, and says
who holds it. `sessions` and `doctor` report what is observable: uncommitted work and commits
ahead.

### A session owner names its worktree home-relative, and the Claude session that opened it

The owner form is `session:<pid>[@<claude session>]:<worktree>`, with the worktree written
`~/repos/…` when it lies under `~/repos`, and only there. The host and the dev container see the
same `~/repos` under different homes, so an absolute path named a directory only one side had,
and the other side's probe answered `Unknown` about every session it did not open. Each side now
resolves `~` against its own home (`owner::session_worktree`), and old absolute owners still
parse and are judged as before. A checkout elsewhere under the home keeps its absolute path,
because the other side's `~/src` is a different directory, where an absent checkout would read
as proven gone. A home reached through a symlink still gives a `~/` owner: git reports the
physical path, so the mapping also compares resolved paths.

The opener (`CLAUDE_CODE_SESSION_ID`) is provenance and never decides liveness. It decides one
thing: `task work` refuses to take over a checkout when **both** its opener and the session
asking are `live` in the session registry and are different sessions. That is how two top-level
Claude sessions stop ending up in one worktree. Anything less established lets the takeover
through as before: a process with no session, a session the registry does not know, an opener
that ended. The unknown-session case covers a subagent, which has its own
`CLAUDE_CODE_SESSION_ID` (measured), while `CLAUDE_CODE_CHILD_SESSION` is set in a top-level
session's shell as well, so nothing tells a subagent apart. Whether a subagent's start ever
reaches the registry is not measured; if it does, a subagent resuming its parent's checkout is
refused. A resume by something that is not itself a running, registered session keeps the opener
it found rather than clearing it, or one terminal resume would lift the protection. The refusal
names `jkb task release` for an opener that is gone but was never recorded as such. Pinned by
`a_session_opened_by_a_running_claude_session_is_not_taken_over`,
`a_session_owner_names_its_worktree_under_the_home` and
`a_home_relative_owner_is_reclaimed_from_another_home`.

### The session verbs' database steps are compare-and-set ops

The session verbs' database steps are ops in `jkb_api::sessions`, and every write they make is
a compare-and-set on the owner the verb judged. `task.start` and `task.take` clear only the owner
they were told about, and write nothing when a claim appeared that the caller did not see.
`task.start` keeping a claim writes only while that claim is still there. `task.abandon` changes
nothing when the claim is no longer the one observed before the git work, and reports
`released`, so a caller whose task was taken meanwhile says so instead of "abandoned". A failed
worktree add, and a pending removal that could not be cancelled, release only the verb's own
claim, through `task.release`; the first used to clear the claim unconditionally, the second
left it held.

`task.take` **judges** `task work`'s location before any git work: it writes the facets for trial
in a savepoint, checks the task's `tasks.md` line, and rolls the trial back, so a refusal (a line
that could not carry the place) leaves nothing to undo. `task.locate` records the location once
the worktree exists, and only while the run still holds the claim, so a displaced run cannot
overwrite what its successor recorded. The take's `start` entry carries no branch or land
target; `task.locate` adds a `note` carrying them when they changed. Writing the location with
the claim, as the first fix did, left a run that then failed its git work pointing the task at a
branch nobody made, which `close-merged` would wait on for ever.

### A run stopped between the claim and the location is found through the claim

A run stopped between the claim and `task.locate` leaves a claim on a checkout with no `branch=`
naming it. `task work` and `task abandon` then find the session **through the claim**
(`session_cli::claimed_session`), so a re-run resumes that checkout rather than forking a second,
and abandon can still remove it. A `task.locate` that fails keeps the claim for the same reason.
Both verbs use the one rule, whatever other branches the task records, and accept the claimed
checkout only when no other task records its branch: names are minted from slugs, so two tasks
can reach one path. Such a checkout's land target was never recorded (one the task carries
belongs to an earlier checkout), so `task work` always requires `--onto` for it rather than
guessing.

### Owners are taken in one spelling and compared as stored

An owner a claim is **taken** as (`task.claim`, `task.take`, `task.start`'s `take`) must already
be in `AgentId`'s spelling (`tasks::check_new_owner`): the claim is stored that way, so an owner
sent in another spelling would never match its own claim again. Claims are compared **as stored**
(`claim::holder`): `task.facts` reports the stored string, so an owner in a spelling `AgentId`
would render differently still matches when it is sent back.

### Branch existence counts the remote-tracking copy, and creating is not adopting

`gitrepo::branch_ref(dir, branch, prefer)` is the one answer to "does this branch exist, and
under what name". A branch living only on `origin/` is the ordinary state after a merged pull
request deletes the local copy, and a bare `refs/heads/` probe called that gone and advised
deleting the tag tracking live work. The create side is deliberately **two** functions, chosen
per caller: `ensure_branch` prefers an existing remote copy (the caller is *referring to* a
branch: an explicit `--onto <batch>`, a session branch whose commits may be pushed), and
`create_branch` takes `start` literally (the caller is *making* one, and a stale namesake on the
remote must not be adopted in its place). Folding both into the primitive made it ignore its own
`start` argument.

### Which recorded branch a task's work is on is one rule

`repo::work_branch` is shared by the In Flight row and `jkb task land`. Sharing the existence
*predicate* was not enough: the row preferred a branch that resolves while the command took
whichever `tag::applications` returned first, the lexicographically smallest, so a task carrying
a stale `a-gone` beside a live `z-live` got two opposite explanations from one shared blocker,
and the command's advice for the branch it picked (`jkb task work`) cut a *second* branch and
detached the task from its batch. A live session wins outright: it is the branch with a checkout
on disk.

It is asked through `repo::work_for`, which returns the session *and* the branch together, so a
caller cannot take one and pick the other for itself. That is what `jkb task abandon` did as a
third implementation, taking the first `branch=` value and deleting a stale sibling under
`--delete-branch` while the row the user clicked named the live one. The batched listing calls
`work_branch` directly with the sessions and refs it has already read once: the same rule, not a
second one.

### The explorer button launches a session

`jkb.workTask` runs `jkb task work <uid> --json`, then opens a terminal whose cwd is the returned
worktree and starts Claude there with a seeded prompt. The prompt says it is in an isolated
worktree on a named branch, to commit there, and to leave landing to the person: an implementer
that lands its own work has no reviewer. A second click on the same task returns the same
worktree. `jkb: Land this task` sits beside it, and the tree labels claimed tasks so a session
in flight is visible without running a command.

### What sessions deliberately do not do

No reviewer of their own: the swarm needs one because nothing human sees the branch, and here
you are it (the land gate below still requires a recorded review). No automatic landing: landing
is a decision, and driving by hand is making it. No cross-repo sessions: a session is a worktree
in one repo. The batch branch is not pushed or turned into a pull request by jkb; it is an
ordinary branch, finished the way you finish any other.

### The disposal verdict, and when it earns a state machine

Disposing of a spent session (`archive::observe_pending`, `verdict_pending`) took three review
rounds in which each found a must-fix inside the previous round's fix. Twice the level was
judged, and twice the answer was **no fourth `jkb-fsm` machine**, for the same argument, so it is
written down rather than re-derived by feel.

The class: the observation vocabulary was coarser than the remedy vocabulary keyed on it, and
the collapse happened *upstream* of the pure seam. `verdict_pending` is pure and its whole
product is walked by an audit, so its power is exactly the product space of `Observed`: any two
world states needing different advice must be distinguishable there. Four were not
(`worktree_head`'s `Ok(None)` covering wreck and absent; `worktrees`' `Ok(vec![])` covering a
failed git; `deletions_only`'s `None` covering an unanswered probe; `Presence`'s single `Unknown`
covering two causes). It is "one unobtainable answer spelled `false`" one level up: causes
spelled as one value.

A `Machine` table would not help. It would formalize `Observed -> Verdict`, which is already
pure, already a choke point (`pending_verdict`) and already audited; `Machine::audit` would walk
the *same* collapsed vocabulary and be exactly as blind. And these remedies are operator actions
whose effects happen in git and the filesystem, so an effect model is needed either way, and the
model is the weak joint either way. The table is earned when **any** of these holds: a second or
third site independently re-derives the verdict rule (the task machine's motivation was about
thirteen); remedies become events the machine itself applies, with effects on the record rather
than the world; or the state set grows reconciliation paths between several writers. A rising
*count* of findings is not a trigger; the finding **kind** is. Findings in `world -> Observed`
or in the effect model are the kind a table does not catch.

### Three audit properties guard the disposal verdict

The exit audit ("every hold names a remedy that moves the verdict") cannot police the
recoverable-wins rule: an operator deleting a live checkout is a perfectly good escape by its
lights. So `nothing_irreversible_is_advised_on_an_unproven_observation` asserts separately that a
destructive remedy is offered only where every licensing fact was **proven**.

Both walks inspected only `Verdict::Hold`, so the guarantee was about *advice*. `RemoveByHand` is
a sentence to a person who will look before acting, and was gated; `Verdict::Stow` renames a live
checkout, force-deletes its branch and hands the tree to `remove_dir_all` after the retention
window, unattended, from a service, and nothing gated it. **The stronger act had the weaker
check.** Widening `Foreign` into an unconditional `Wreck => Stow` immediately put the module's
most destructive outcome behind an observation nobody had established. The third property,
`nothing_is_acted_on_without_the_facts_that_license_it`: an acting verdict requires the tree
proven present, and its identity either proven (`Matches`) or unprovable in principle for a
reason itself established (`Wreck`: git *answered* and named an enclosing repo). `DropRecord`
asserts its own licence (`Presence::Gone`). With `worktree_identity` split last (its `Foreign`
had covered "or it could not say"), the vocabulary split is complete across the family.

### An honest effect model is a finder, not a formality

`applied()` is a hand-written model of what each remedy achieves, and it twice granted a remedy
an effect it does not have, which proved the property about the model rather than the code.
Three rules: an arm may *settle* a fact only where that fact IS the remedy's success criterion,
and must otherwise yield the **set** of answers the advice might produce, all of which must
escape; each arm names the mechanism that delivers the effect, so a claim about another command
is checkable; and the harness has a **negative control**, an inert remedy that must be reported
as a dead end. The control found a defect in the harness on its first run, and applying the
first rule to `FixGitAccess` (whose criterion is *git answers*) made the exit audit fail on a
state that really was a permanent hold. The closed set of remedies is a macro, not a list:
`remedies!` declares each variant beside a sample, so one without a sample does not parse, and
`advice`, `is_destructive` and `applied` are exhaustive over it. Its doc had claimed the set was
"made true rather than asserted" while being two hand-written lists, and the variant added in the
same round skipped the audit.

The round's remaining concerns were "a rule every call site must remember" one level out, and
each moved into a callee: the identity fence into `deletions_only`, the head check into
`delete_branch_if_any` (`DropRecord` deletes branches too), and `target_dirty_reason` took the
`Option` instead of two call sites each carrying an argument for why collapsing it was safe,
arguments that for that consumer were both wrong.

## Staging branches and review-gated landing

A staging branch is the branch a batch of tasks lands on before trunk. It is the **same thing**
`/task-swarm` calls its integration branch (cut from trunk, sub-branches rebase and
fast-forward into it linearly, the gate runs on the integrated result), reached by hand instead
of by a coordinator. Sessions made landing work, but left three things invisible and one
unenforced. The land target was resolved by a fallback chain the user never saw, with no way to
ask which staging branches existed or to say "put this one on that batch". What was in flight was
a flat `jkb task sessions` list that could not show a staging branch whose sessions had all
landed, which is precisely the branch you open a pull request from. And nothing required a
review: you could run `/review-log`, ignore every must-fix it filed, and land.

### A staging branch is derived, never stored

A staging branch is any git branch some task's land target names that still exists in git.
There is no `kind='staging'` item and nothing to reconcile. Which branches exist comes from git;
which tasks are on them comes from the transition log; sessions live in git worktrees; whether a
session has work to land is `gitrepo::ahead_count`; what state a task is in is `items.status`.
A staging *item* would add a title and a pull request URL and would then need reconciling
against git: a branch deleted by hand leaves a stale item claiming to be live, and the UI shows
work that does not exist. Sessions refused a session state file for the same reason.

What this rule actually protects is narrower than its first wording, which forbade any table: a
stored entity that **copies a fact git owns** (branch existence) needs reconciling. Facts git
does not own (where a branch lands, that jkb landed it, whether a review ran) have to be stored
somewhere, and storing them is not a violation. The branch-record episode in History is what
that distinction cost to learn.

### `jkb staging ls` is the one read

`jkb staging ls [--all] [--json]` is the one read behind both the explorer's branch picker and
its In Flight view, so the two cannot disagree about what is live. Each row is a staging branch
with its tasks nested; each task carries its session, `dirty`, `commits`, its review, and a
derived `state`:

- **`implementing`**: a session worktree exists and the task is not `needs_review`.
- **`review`**: the status is `needs_review`.
- **`landed`**: the task is `done` and its work landed on the staging branch.
- **`dropped`**: a **cancelled** task that was on the branch, kept apart from `landed` because
  reporting the two as one would say a dropped task shipped.

Spent batches are omitted unless `--all`: joining one both attracts new work onto a dead branch
and blocks `git branch -d`. Spent-ness is read from the tasks' statuses and the transition log,
not inferred from refs. A branch adding nothing to trunk is either landed *or* freshly cut and
still empty, and refs cannot tell those apart, so **live work is the tie-break**; otherwise the
branch cut by the very first `task work` is hidden from the picker that exists to offer it.

**One database pass.** `tasks_by_branch` used to issue a query, then a `tag::applications` read
*and* an `item::get` per task, each a round-trip serialized on the writer thread, over
`kind:task tag:repo=<key>`, which grows with every task ever worked in the repo. Fine at three
sessions; not fine for a view that redraws on every database write. The read fetches items and
tags in one pass and filters in Rust. The git calls stay per staging branch, of which there are
a handful.

### "Does this branch exist" is answered with a ref, not a boolean

`gitrepo::branch_refs` is one `for-each-ref` over `refs/heads` and `refs/remotes/origin`, local
winning. Counting the remote-tracking copy admitted a pruned batch to the listing, and every
count was then still taken with its bare short name, which resolves to nothing: `rev-list`
exited non-zero, the failure read as **zero commits**, and the row refused a landing the command
performed. Membership answers "may I show this", not "may I ask git about it", and the second
was the question every consumer had. So `ahead_count` **refuses** an operand it cannot resolve
rather than returning zero, since zero is a load-bearing answer ("nothing to land") and a count
that could not be taken must not be spelled the same way. `land_preflight` asks `branch_ref` for
the same reason: it asked `has_branch` while the row asked remote-inclusively, so one shared
blocker printed two opposite explanations of the same task.

### The staging branch is chosen, not inferred

"Work this task with Claude" is two steps: pick the staging branch, then open the session, with
the picker built from `jkb staging ls`. It offers each live staging branch, described by what is
on it (`3 tasks · 7 commits ahead`); **New staging branch…**, which prompts for a name and cuts it
from trunk; and **Let jkb decide**, which passes no `--onto` and keeps the fallback chain
exactly. "Let jkb decide" is listed first when there is already a batch to join, because the
chain is usually right: the point is that the choice is *visible and overridable*, not that it
becomes mandatory. A picker with no default turns a one-click action into a decision made every
time, which is how people stop using it. Cancelling the picker opens no session.

`resolve_onto` is unchanged; the picker only feeds it a value it could always take. Rejected:
folding the picker into `resolve_onto` as a prompt. The CLI must stay non-interactive, since the
UI, workflows and `setup.sh` all call it; the choice belongs to the caller.

### Review state belongs to the task, keyed by branch

A review records the branch HEAD it ran against and the folder of its findings, so the findings
are one `jkb ls` away. It is the one fact here with nowhere authoritative to live: git does not
know, and the reviewer is a Claude workflow the CLI cannot run, so the CLI can only *require a
record*. `jkb task review record [--branch <b>] [--sha <s>] --findings <ns>` writes it, and
`/review-log` calls it after mounting and syncing its findings, so they exist before anything
points at them, then says whether the branch can now land and names the open must-fixes if not.

Recording is keyed by **branch**, since that is what a review knows; the task is found through
the `branch=` index. A staging-branch-wide review records against every task on it. A review run
on trunk, or on a branch no task claims, matches nothing and says so: a note, not an error,
because reviewing an arbitrary range is legitimate.

It lives on the task, not on the review folder's namespace. `/review-log` mounts each run at
`repos/<repo>/codereviews/<folder>`, and that namespace's `metadata` is *owned by the sync
engine* (`layout`, `header_line`, `position`, `prose`); a second writer there is the class of bug
that collapsed `openspec/`. Nor is it derived from the folder name: `/review-log` names folders
`<datetime>-<branch>-<N>`, but the branch is `tr '/' '-'`-mangled on the way in, so `task/fix-ls`
and `task-fix-ls` are one folder, and the SHA is not there at all. A boolean instead of a SHA was
rejected: the SHA costs nothing more, and without it staleness could never even be reported.

The record was first two facets, `reviewed=<sha>` and `review=<ns>`, written with `set_facet`.
It is now an append-only `reviews` table, with review rounds snapshotted when recorded, because a
tag is content any writer may set (see History; the round rules are the agents-and-roles
design). `task.facts`, `task.staging` and `task.show` carry it as a `review` field.

### The land gate: reviewed, and no open must-fix

`jkb task land` refuses a task with no recorded review, or whose review has a `!p1` finding that
is neither `done` nor `cancelled`. Open must-fixes are counted with `kind:task
ns:<review_ns>/** priority<=1`, filtering terminal statuses in Rust: the DSL has `status:<s>` but
no `-status:`, and `is:ready` is the wrong instrument because a *blocked* must-fix must still
block landing. The gate later gained a last-round clause (the newest round must itself be clean),
recorded in the agents-and-roles design.

The check runs **before the graft**, beside the dirty and ahead checks, so a refusal has moved
nothing. Concerns and nits never block: a gate everything trips is a gate nobody keeps, and a
previous run put 34 of 45 findings on `concern`, so blocking on those would make the override the
normal path within a week.

`--no-review` overrides and is recorded as a waiver at the reviewed SHA, surfaced by `staging ls`
and the In Flight view. An override nobody can see is indistinguishable from a rule that does not
exist; one that leaves a mark is a decision someone made, readable as such afterwards.

### `needs_review` is the display state; the findings are the gate

Recording a review moves the task `in_progress` to `needs_review`, and is the **only** author of
that transition (in the machine, `submit_for_review`). The status and the gate are deliberately
not fused: a task in `needs_review` with nothing outstanding lands, and one moved back to
`in_progress` with an open must-fix does not. Fusing them would make `jkb task set --status`, an
ordinary bookkeeping command, the bypass. The status tells a person where the work is; the
findings decide whether it may land.

### Review staleness is recorded, not enforced

Commits after the reviewed SHA do not invalidate the review. The SHA is provenance (it lets
`staging ls` say "reviewed, 3 commits ago"), and promoting it to a hard rule is a one-line change
once it is known whether that is annoying. Doing it now would make every post-review fixup force
a full re-review, the fastest way to make people reach for `--no-review` by reflex. Likewise
`-status:` was not added to the query DSL for the gate's sake: filtering two statuses in Rust is
not worth an operator; if a second caller wants it, that is the time.

### The In Flight view

A second tree view in the `jkb` container, beside the explorer. It is separate because it is a
different axis: the explorer organizes by *where things live*, this by *what is being worked*,
and the same task legitimately appears in both.

```
▾ ui-and-staging            7 commits · 3 tasks
    ● Create tasks from the tree      implementing · 3 commits
    ◐ Staging branch picker           review · 2 must-fix open
    ✓ jkb staging ls                  landed
▾ code-review-workflow      merged
```

Backed entirely by `jkb staging ls --json`, refreshed on the database-write signal the explorer
uses. Row actions are the ones that exist: land, abandon, open the session's terminal, open the
review's findings. A task held by must-fix findings says so in its row, because a held row that
looks identical to a landable one is worse than no row at all. The portable part, deriving the
row label and state from the JSON, lives in `ui/core` beside `summary.ts`, so a future web host
renders the same thing.

### The swarm records where it is working

`/task-swarm` sets `repo=` at claim, and runs `jkb task start --branch <group-branch> --onto
<integration>` once the implementer has a branch, recording `branch=`, `repo=` and the land
target in one write, so the swarm supplies no value it could get wrong. The land target cannot be
recorded at claim, because the group has no branch yet and the target is a fact about a branch.
`staging ls` then shows swarm work and hand-driven work in one view rather than the half it was
told about.

### The merge queue has no review gate, and is a jkb client for one call

`scripts/merge-queue.sh` is still the swarm's queue and still a git and gate runner. It has no
review gate, deliberately: the swarm runs a fresh REVIEWER before a group reaches the queue, and
that *is* its gate, stricter than this one, because no branch reaches the queue without an
approving reviewer. Requiring a recorded review there would make the REVIEWER write review
records to satisfy a check its own approval already answered. Each path keeps its own gate, and
each design note points at the other.

It makes **one kind** of knowledge-base call, `jkb task landed <branch> --onto <target>`, which
records the landing and closes every task on that branch, from two arms: after a genuine
fast-forward, and when `<target>` already contains everything the branch adds. That makes it a
jkb client, so its caller must export `JKB` and `JKB_DB`; `.claude/workflows/task-swarm.js`'s
`QUEUE_ENV` does, and the script header states the contract. The CLI is the home of the human
path because the UI calls it directly and it must work in any repo.

### `jkb task land` and the merge queue deliberately differ in two places

They are no longer the same algorithm, and the divergence is deliberate rather than drift, so it
is recorded here and not only in a Rust comment. Both differences date from the 2026-09-12 queue
rework.

- **Ordering.** The queue gates the rebased commit while it is still detached and only then
  fast-forwards, so `<target>` never points at an ungated commit. `jkb task land` still
  fast-forwards first and rewinds with `reset --hard` on red: the window is the whole gate, and an
  implementer told to cut from the integration branch can carry ungated commits away inside it.
  Filed, not fixed: reordering the human path changes `graft`'s contract and `do_land`'s flow and
  wants its own change.
- **What "nothing to land" asks.** The queue asks the content question (a branch whose net diff
  against its merge-base is empty is refused however many commits it carries), because it closes
  whole task groups unattended, and a phantom landing there unblocks dependents with nothing
  implemented. `gitrepo::graft` still asks the commit question (`ahead_count == 0`).

Both are tracked as their own tasks. Until they close, a reader comparing the two must expect them
to differ **here** and nowhere else.

## Auto-close

`jkb task close-merged`, run by the `post-merge` hook on every pull, closes tasks whose work
reached its destination. The inference this needed was the hardest problem in the area: a squash
or rebase merge rewrites the commits, so containment cannot be tested, and the weaker question
(*does this branch add anything to trunk?*) cannot tell a branch squashed away from one that never
started. Making that answerable took a stored cut point per branch and an instance anchor to
protect it, and produced roughly a quarter of the review corpus's must-fixes (see History). The
decisions below replaced it.

### Auto-close is conservative by construction

`close-merged` closes an open task only when **both** hold: its work is proven to have reached its
destination, and every subtask is terminal (`done` or `cancelled`). Anything failing the second is
*reported, not closed*: the branch landed but the task is not finished, which is precisely the
case subtasks exist to express, and what makes them load-bearing rather than decorative. A merged
branch is evidence, not proof. A missed auto-close costs one `jkb task set --status done`; a wrong
one silently buries unfinished work. The open-subtask rule is checked in the preflight, beside
every other precondition, not in the `land` plan, which is applied last and would only narrate a
landing that had already grafted.

### What jkb performs, jkb records

Where jkb itself grafts a branch, the landing is an event, not an inference. `jkb task land`
appends a `land` transition after its gate is green, so a rolled-back land leaves no event.
`scripts/merge-queue.sh`, which is bash, records through `jkb task landed <branch> --onto
<target>`. `jkb task review record` credits a task whose work jkb *grafted* onto the reviewed
branch: a recorded event, where it used to be a containment probe that could not tell an empty
session from a landed one.

### Auto-close is a lookup on an id that is never reused

A pull request number is minted by GitHub and never reused, so there is nothing to disambiguate.
`jkb task pr <uid> [number]` records it or discovers it (refusing to guess when a reused branch
name matches two: `Discovery::Ambiguous`); after that the branch name is never consulted.
`close-merged` asks `gh`, and **everything degrades to `Fact::Unknown`, never to a no**: no `gh`,
no network, no GitHub remote, an unrecognized state, a parse failure (which is `Unavailable`
rather than an empty list, since an empty list reads as *no pull request* and would let a task
close on the strength of a schema change). The task is then *held with the reason printed*. It
also gives an answer the inference never could: *closed without merging*. `close-merged` reports
two buckets where it had six, each held task carrying the guard's own reason, and `--trunk` is
gone.

### `gh`'s interface is verified against `gh`, not memory

The field names, the flags and the **uppercase** state values were checked against `gh` itself:
`gh` 2.97.0, without auth, against `gh pr view --json`'s and `gh pr list --json`'s own field
lists, `gh pr list --help` (including that `--state` takes `all`), and `gh`'s `display.go`, which
switches on `"OPEN"`, `"CLOSED"` and `"MERGED"`. The whole failure path ran against a real
unauthenticated `gh`: `close-merged` holds the task and `task pr --json` reports `merged:
"unknown"` with `gh`'s message as the reason. Found by running it: `gh`'s errors are multi-line,
and reasons print one task per line, so they are collapsed where the string is built. The one
live call, a real merged pull request reading `MERGED`, is
`pr::live::live_a_merged_pull_request_reads_as_merged`, an `#[ignore]` test beside the ollama and
Chrome smokes; it needs `gh auth login` and names the auth failure without it.

### The pure half is separated from the `gh` call

The rule that turns a pull request's state and the task's history into a verdict is pure, and the
`gh` call only gathers its input. A rule exercisable only by shelling out to an authenticated
network client is a rule nothing checks.

### Where it cannot be told, the task is held

A merge with a known resumption it cannot be placed against is `Undecidable`, not "live": closing
there picks the burying direction on the strength of a missing field. `Live` is the default
because *no resumption* is the normal case, not because a missed close is cheap.

### A landing jkb did not perform and no pull request records is not detected

Stated rather than covered over. Such a task is *reported*, not guessed at: `close-merged` names
it and says it has no proof, and the repair is one command. Given that a missed close costs a
command while a wrong one buries work in flight, that is the right side to fail on.

### Three identities exist already, and a branch name is none of them

The task uid identifies the subject (jkb mints it); the agent id identifies who is acting (the
caller's `JKB_AGENT_ID`, or a pid, or a worktree); the pull request number identifies that work
reached its target (GitHub mints it). None is ever reused. A branch name is not a fourth
identity; it is a **label on an event**. Recycle one and nothing breaks, because nothing is keyed
by it. A jkb-minted `attempts` table, with the branch as an attribute, was proposed first: a
better shape than keying by `(repo, branch)`, and still unnecessary once the pull request answers
the question directly.

## The lifecycle is a checkable state machine

The `staging-workflow` branch took 44 review passes and about 80 must-fix findings. Every one
names a file and a line and every fix is in the tree, so "the same defect recurred" could be
checked rather than asserted; the 44 `.codereviews/*staging-workflow-*` folders were read in full.
Sorting the task-lifecycle must-fixes by *cause* rather than by site gives six groups, and each
maps to a property the code had no way to have. `crates/jkb-fsm` and the task machine declared in
it are the answer.

### The lifecycle was written down nowhere

No artifact said which states a task has, which transitions exist, and what each requires. About
a dozen sites each derived the part their own question needed: `claim::claim`'s terminal
pre-check, `task::set_status`, `staging::State::from_status`, `land_blocker`, `land_preflight`,
`close-merged`, `task abandon`, `task work`, `review record`, `merge-queue.sh`, the VS Code row.
*Two sites answering one question differently* is the most common finding shape in the corpus
(the In Flight row offering Abandon on a landed task that `task abandon` then reopened;
`land_preflight`'s own terminal bail shadowing `land_blocker`'s arm). Each was fixed by making the
two sites share a function, which works and does not generalize: the thirteenth site is written by
whoever adds the next verb, and nothing tells them the list exists.

### A lifecycle is a walkable static table

`crates/jkb-fsm` is a dependency-free library (`serde` optional and off, no `std::process`, no
I/O): `fact.rs`, `machine.rs`, `check.rs`. A `Machine<S, E, C, X>` is a `&'static` table of
`Transition { from, event, to, kind, guard, plan }` plus an initial state, where `S` is states, `E`
events, `C` the observation a guard reads and `X` the effect the domain performs. Because the table
is data it can be printed, checked, exhaustively tested and drawn: `Machine::dot()` renders it as
Graphviz, reconciliations dashed and stated destinations to a `*` node, the artifact whose absence
was the first item on the list. Twelve sites deriving a lifecycle become twelve sites asking one.

Rejected: a `statig`, `sm` or typestate crate. Those model states as types and transitions as
consuming methods, right for a protocol whose state is in memory and wrong here: the state is a
database column known only at runtime, the transition set must be **walkable** for the checks, and
a guard must refuse *with a reason*, where a typestate machine simply does not offer the method.
The table would end up maintained separately in order to check it. Also rejected: generating the
table from a macro (the `db_enum!` precedent): the table is about 20 rows and its value is being
*read* by a reviewer, which a macro invocation obstructs.

### `Fact` is three-valued, and nothing collapses `Unknown`

Nine must-fixes were one unobtainable answer spelled `false`: `ahead_count` returning `0` (which
means *nothing to land*) for a branch it could not resolve; `has_own_commits` answering *no* when
`rev-list` failed; a land gate that could not tell *no findings* from *the namespace resolved to
nothing*; `base_is_usable` asking git the non-verifying question, so any 40-hex string read as a
commit. Each was fixed at its site; the type system was never enlisted, and the next fact would
have been a `bool` too.

`Fact { Yes, No, Unknown }` has `is_yes` and `is_no`, **both** meaning *proven*, and no method that
collapses `Unknown` to a `bool` in either direction. So a guard states its polarity in code:
landing needs `work_dirty.is_no()` (an unreadable checkout refuses) and `has_commits.is_yes()`; a
close needs `merged.is_yes()` (a failed git holds). Kleene `and`, `or` and `not` keep a composite
three-valued, and the constructors `observed(Result)` and `maybe(Option<bool>)` turn "I ran git and
it failed" into `Unknown` at the boundary, not three lines later.

### A transition yields its effects as one value, applied last

`settle_landing` wrote `done`, cleared the claim, then asked git to remove a worktree git refused,
leaving a task `done`, unclaimed, with a live session. `task start` wrote `branch=` without the
rest of the location for a whole feature's life. "Two independent writes that must agree will
eventually not, so there is one write" was learned at three sites and enforced at none.

`Machine::apply` returns `Outcome::Moved { from, event, to, effects }`: the state change and
everything that must accompany it as one value. `transition::perform` is the one seam: ask the
machine, apply the whole plan or none of it in one `write_txn`, append one history row;
`apply_effects` is private, so a caller cannot apply half a transition. `TaskEffect` is
`SetStatus`, `Claim`, `ReclaimFrom` and `ReleaseClaim`: the two fields `settle_landing`
desynchronized, both on one row, and nothing else.

The graft, worktree removal and branch deletion stay **outside** the plan. They are git operations
that can fail after the transaction commits, and pretending a `Vec<TaskEffect>` could hold them
would recreate the bug with more ceremony. What the machine gives those callers is the ordering
rule, stated once: **apply the plan last**, after every fallible external step, so a git failure
leaves the task where it was and the verb is re-runnable. A test that the gate leaves the checkout
dirty caught this change applying the plan before the disposal. Rejected: effects as trait objects
that perform themselves, which reads well and makes it natural to put the graft in the plan.

### A refusal names an event, and the machine checks it

Review passes 31 and 32 were the same finding one message apart: a printed remedy
(`jkb task base <uid> <branch> <sha>`) whose obvious argument, the branch tip, froze the task
permanently. Another refusal told the user to edit a file, and the edit routed it into an arm with
no guard. The fix each time was to reword, and there are only so many messages.

`Denial { reason, remedy }` holds a `Remedy { event, how }`: a guard that refuses says what to do in
the machine's own vocabulary. If the named event is neither accepted nor idempotent from the
refused state, the outcome carries `Defect::UnreachableRemedy`. `Machine::audit(&[C])` validates
every remedy the machine can produce over a context matrix, and the task machine's matrix is
generated from the cross-product of its facts. It caught a bad remedy in this change as it was
written: `land`'s "no work to land" refusal offered `start`, which is not accepted from
`needs_review`. It also caught the state being passed beside the context, free to disagree with it;
`Stateful` now reads the state *out of* the observation.

### No wedges: `check` and `audit`

The words *permanently*, *forever* and *unrepairable* appear in 14 must-fix summaries. Nothing had
asked "from this state, can the user still get anywhere?". `Machine::check()` is the static pass,
run in a test: `Nondeterministic` (two rows for one `(from, event)`, never last-one-wins),
`UnreachableState`, `Wedged` (no path to a settled state), `NoSettledState`,
`UnguardedReconciliation`, `UnusedEvent`. An undeclared pair is `Outcome::Undefined`, a named
refusal, so no arm falls through to "do nothing quietly", which is how `--onto` was once silently
ignored. `Machine::audit` is the dynamic half: `UnreachableRemedy`, `DeadEnd` (under some
observable context a non-settled state accepts nothing: "held for ever", which static liveness
cannot see because every path out is guarded), `AmbiguousReconciliation` and `UncoveredState`.

`tests/machine.rs` has a working toy lifecycle plus **one deliberately broken machine per defect**,
so every check is shown to fire rather than merely to exist.

### `Dest::Stated` counts for reachability and never for liveness

Some events take a destination the caller names (`Dest::Stated`): an operator override, a synced
file's checkbox. Such an edge is excluded from the liveness walks, because a state whose only exit
is somebody naming a different state is still wedged; an escape hatch is not an exit. It does count
for reachability: *can the object be here* is answered yes by an override. Collapsing the two
reported `abandoned` investigation units, which only a person sets, as unreachable dead code.

### Every verb is answerable at its own destination

The destination of a transition accepts that transition's own event as a no-op
(`Outcome::Idempotent`), unless the table declares otherwise. Pass 4's must-fix was `jkb task start`
refusing its own second run: not a verb that forgot a check, a machine rule that did not exist.

Absorption is never applied to a row with a guard or a plan. The first rule absorbed any event whose
destination you were already in, but arriving there another way leaves the plan unapplied: `abandon`
on an operator-reopened task skipped its guard *and* its claim release, reported success, and the
surviving claim held the task off every frontier. Correcting that silently turned five destinations
into refusals, `land` on an already-landed task among them, which is the re-run guarantee lapsing,
worse than one never claimed because the retry advice everywhere assumes it. `Unrepeatable` is the
check that found it, and the pair of rules is: **a verb you run is always answerable at its own
destination; an observation only where somebody wrote down what re-seeing it means.** That does not
mean "make it a no-op": a domain that wants the second run to fail declares a self-loop whose guard
denies, and gets a sentence and a remedy instead of a missing row. Two rows keep their guards on
purpose (`abandon` from `open`, `observed_landed` from `done`): those verbs may still have work to do.

### Applied versus reconciled events

**Applied** events are somebody asking (`jkb task land`, `abandon`, the merge queue).
**Reconciled** events are the world moving and the system detecting it (the work reached trunk, the
holder no longer exists). A reconciled transition must be guarded, fires only through
`Machine::reconcile(state, &C)`, and **refuses ambiguity**: if two are allowed at once, `reconcile`
returns `Ambiguous` and does nothing rather than taking the first in declaration order. The
reconciliation paths existed before (`close-merged`, `doctor --fix`, `review record`) but as
commands, not a modelled kind, so nobody had asked whether two could fire on one task at once.

### The state is `items.status`, and nothing else

The states are exactly `TaskStatus`: `open`, `in_progress`, `needs_review`, `done`, `cancelled`.
`jkb_fsm::State` is implemented for `TaskStatus` itself in `jkb-types` (the orphan rule, and a
parallel enum would be a fourth list over the same five strings); `staging::State` became a
rendering of it. The richer set that folds in the claim (`Unstarted`, `Claimed`, `Implementing`) is
wrong: claim and status were split precisely because "is anyone holding this" and "how far along is
the work" are different questions (the agents-and-roles design). The claim is **context**, and a
claim change is an **effect**. `blocked` stays derived and absent, and so do subtasks: both are
properties of the graph, and a task's machine must not carry a state another task's status changes
under it. Storing the machine's history as items and edges was rejected as scope.

### Events

Applied: `start`, `submit_for_review`, `request_changes`, `land`, `abandon`, `cancel`, `reopen`,
`override`. Reconciled: `set_from_file`, `observed_owner_gone`, `observed_landed`.

| event | from → to | fired by |
| --- | --- | --- |
| `start` | open → in_progress | `task work`, `task start`, `task claim` |
| `submit_for_review` | in_progress → needs_review | `jkb task review record` |
| `request_changes` | needs_review → in_progress | a review with open must-fix findings |
| `land` | in_progress, needs_review → done | `jkb task land` |
| `abandon` | in_progress, needs_review → open | `jkb task abandon` |
| `cancel` | non-terminal → cancelled | `jkb task set --status cancelled`, sync `[-]` |
| `reopen` | done, cancelled → open | `jkb task set --status open` |
| `observed_landed` | in_progress, needs_review → done | `close-merged`, `jkb task landed` |
| `observed_owner_gone` | self-loop | `task reclaim`, `doctor --fix` |

`jkb task claim`, the verb the swarm runs on every task, wrote the claim directly until review
found it: a choke point with a third door, so swarm work had no `start` entry at all and the two
claim verbs answered `needs_review` oppositely. It now goes through `start`.

### `set_status` is the `override` event, and a checkbox is `set_from_file`

`task::set_status` is not a hole beside the machine. An operator stating a status is `override`
(applied, unguarded, since status and the land gate are not fused); a synced file's checkbox is
`set_from_file` (reconciled, guarded on the task actually being backed by that file;
`task::set_status_from_file`, used by `jkb-sync`). Both use `Dest::Stated`, and being rows they
carry every transition's obligations, including the claim release a terminal status entails, which
had been a hidden tail on one function that a second writer of the column silently skipped. Wiring
the sync path exposed a latent ordering bug: `create_item` wrote a task's columns before its
binding, so a new file-backed task's first status came from an authority the store could not yet
see. The binding now goes first.

### `TaskFacts` is the only input to a guard

Every outside-world field is a `Fact`: whether the claimant is alive, a branch is recorded and
resolves, a session checkout exists, it is dirty, it has commits the target lacks, the target
checkout is ready, a review is recorded and clean or waived, a subtask is open, the work landed.
`task::observe` gathers the database facts; the CLI gathers the git and review facts. Filling the
struct is where the git reads live, and the machine performs none, which keeps `jkb-core` free of
git and the *rules* testable without a repository (the same rules had been exercisable only by
string-matching `assert_cmd` output against a scratch checkout).

### The regression set and the generated audit

`tests.rs` runs `check()` clean and `audit()` over a generated matrix (11 three-valued facts, 5
statuses, 3 claim shapes, walked as a base-3 counter on a coprime stride), plus one test per real
must-fix from the corpus: `apply(done, abandon)` is refused with remedy `reopen`; `apply(in_progress,
start)` is idempotent; `land`'s plan is one vector applied last; a zero-commit branch cannot read
as landed; every produced remedy is accepted where produced; no context leaves a non-settled state
accepting nothing; an `Unknown` review does not pass the gate; one `land` guard serves both callers;
two simultaneous reconciliations are `Ambiguous`. If a guard cannot express one of these, the model
is wrong.

### What review caught that the machine's own checks did not

The change was reviewed in three ranges (about 18 agents): 36 findings, 8 must-fix, all fixed.
Recorded because the pattern repeats. The absorption bug above. Two tests that could not fail: one
asserted `stdout contains uid` where the failure path prints the uid too, one asserted a defect
structurally unreachable for its machine; both are why `jkb task landed` never credited a swarm
group for a whole branch. A guard that only reports is not a guard: the open-subtasks rule sat in
`land`'s plan. `pid_exists` folded "`ps` would not spawn" into "that process is gone", in the probe
that protects every claim. And `jkb task claim` as a third door.

### Cost, and why the library is worth one user

`jkb-fsm` was new code with one caller, and a library justified by one user is usually a premature
abstraction. What answers it is that the *checks*, not the abstraction, are the deliverable, and they
cannot be written against a hand-rolled `match`: a `match` has no table to walk, no place to hang a
remedy validation, and no way to be audited over a context matrix. 44 review passes was the price of
not having them. The second and third machines below are the evidence that it generalizes.

### The second machine: the sync journal

`jkb-sync/src/lifecycle.rs` declares the per-file journal on the same library: `FileState`
(`Untracked`, `Settled`, `Conflicted`, `Quarantined`, `Blocked`), ten events, **all** reconciled,
since a file has no applied events. It is a reconciler, not a lifecycle: nothing finishes, and the
question is never *what may I do next* but *which condition applies to what I just saw*. `Policy` is
a plain enum, not a `Fact`: stored configuration is never unestablished, and spelling it
three-valued would make the discipline decorative. The guards are a **partition** of the observation
space rather than ordered arms, so two that both apply are `Ambiguous`, which is the file-sync
design's "a route is not a cause; the condition must dominate every arm" stated as a rule.
`lifecycle::status_for` is now the one writer of `sync_state.status`, replacing four hand-written
spellings; deleting a table row made `malformed_file_is_quarantined_then_recovers` fail. Audited
over 7,488 **modelled** observations, derived from what a pass really varies.

It moved the library three times. `is_terminal` became `is_settled`: a synced file settles and is
edited again, and under the old name the machine either had no terminal state (making `Wedged`
vacuous) or lied about one. `State::awaits_input` (default `false`): a conflicted file waits on a
person, and without it `DeadEnd` fired on every such observation and would have trained its reader
to ignore the check. And the initial state may be at rest. The modelling found that
`needs_attention` is two states (a quarantine wants the file fixed, a blocked write the store), now
separated in the model and collapsed only at the column boundary; and that a flag whose cause has
gone is not always cleared (an import-only mount with a store-side-only change writes no row),
modelled faithfully, filed, not fixed.

### The third machine: investigation units, with strategy-supplied rules

`jkb-core/src/nstype/lifecycle.rs` declares `items.resolution` as **two tables over one state set**
(`BASE` and `DEBUGGING`), the axis neither earlier machine had. `debugging` differs twice, and both
differences already existed only in the shape of a function: a settled result can go **stale** and
return to the frontier, and a tombstone is **not** revived by fresh evidence, where the base
table's is (that asymmetry is modelled as found, `default_rollup` lacking an early return, and filed
for a design pass). The strategy supplies the facts, the machine the rules: `resolution_rollup`,
which returned a *conclusion* and encoded the priority of contradictory evidence in the order of its
`if`s, became `unit_facts`, and `investigation::roll_up` drives `machine.reconcile`, so the rules
have one derivation. As guard clauses the priority is arguable and `audit` proves it exclusive.
Both tables are audited over the whole 5 × 3⁴ space, each strategy difference asserted in both
directions. `Resolution::Unresolved` declares `awaits_input`: evidence arrives from outside, unlike a
task's `open`, which always has `cancel`. `UnusedEvent` became a per-machine statement, filtered
only where another machine in the family declares the event, with the union asserted separately.
The machine's semantics are the namespaces design.

### What the checks cannot do, stated

`DeadEnd` is a lifecycle check: both reconcilers mark the states it would fire on `awaits_input`, so
for them it is vacuous and `Wedged` does the work; it remains a regression guard for the task
machine, where an operator escape always exists. The remedy check caught one bad remedy in **each**
of the three machines, every time the same mistake, a remedy named from the *reader's* point of view
rather than the machine's: `land` from `needs_review` offering `start`; `adopted` from `untracked`
offering `exported`, which was not declared there and whose premise was false anyway; `confirmed`
refused for staleness offering `went_stale`, which only `debugging`'s table has. Twice the bad remedy
was the symptom of a bad rule underneath, the more useful half.

## The transition log

### `task_transitions` is append-only, and makes no claim about the present

`V015__task_transitions.sql` is an append-only history: one row per transition, with the actor and
the evidence each guard fired on, indexed by item and by branch, `ON DELETE CASCADE`. Two facts
outlive git's memory, where a branch landed and that it landed, and an append-only history makes no
claim about the present, so there is nothing to reconcile: a branch name that changes hands appends
a row rather than corrupting one, and superseding stops being an operation. `transition::note`
records a fact that is not a transition (learning a pull request number), spelled `note` in the
`event` column. Reads: `history`, `latest_with_branch`, `land_target`, `landed`, `pull_request`.

It is deliberately **not** changelogged (the `blobs` precedent): it *is* an audit record, and a
transition later reverted by `jkb undo` stays, which is the honest reading of a history.

### Branch names are labels on events

`land_target` is the last `onto` recorded, reset by an `abandon`, because where work lands is a
property of the session doing it. Two tasks told different targets are two entries with timestamps,
not one row keeping whichever wrote last.

### `jkb task why` prints the history

`jkb task why <uid>` prints every transition, who applied it, and the evidence each guard fired on.
Fourteen must-fixes were "held for ever with no way to see why"; that is now one command.

### Evidence of a landing is spent once the task is put back to work

The log removed reconciliation from **writing** and moved it to **reading**. Every caller asks a
present-tense question (*has this landed?*, *where does it land?*), and turning a history into one
needs a rule for when an older row stops counting. Each reader wrote its own, and they disagreed:
`land_target` stopped at `abandon`, `landed` stopped at nothing. Five findings across two rounds were
that one gap.

The rule is asked of the status **order**, not a list of events. `transition::resumed` is the one
statement: the newest row that moved the task **backwards** through `open -> in_progress ->
needs_review -> done` (`TaskStatus::stage`). Giving `landed` its sibling's stop-list would be a
fourth private rule for a fifth reader to get wrong, and one each new event would have to be
*remembered* into; every row already records where it moved the task. It took two goes: the first,
*moved out of a terminal status*, answered the reopened-task case and missed that **`abandon` is
`in_progress -> open`**, neither side terminal, so a landing recorded while an open subtask held the
task survived the abandon that destroyed its session, and the task auto-closed over live work.
Asking the order covers both, plus `request_changes` and a resume out of `needs_review`.

### Nothing that stands still is a resumption

A row recording a held landing (`in_progress -> in_progress`) would otherwise supersede **itself**
and freeze its own task for ever. Found by running it.

### `jkb undo` appends an `undo` transition

`jkb undo` restores `items.status` from the changelog, and the log is not changelogged, so undoing a
close left the landing looking live and the next `git pull` closed the task again, a loop undo could
not break. It now appends an `undo` transition, from the statuses observed either side of the
inversion rather than what the entry claimed, and **only for a task that still exists**: inverting
an insert deletes the item, and the history's foreign key failed the whole undo.

### A superseded landing is context, never a verdict

Getting this wrong in both directions took two rounds. Spelling "spent" and "never landed" the same
way sent `close-merged` to ask GitHub about a pull request a locally-grafted branch never had, and
reported that as the reason. Then treating "spent" as *the* answer left a task whose work was redone
and merged as a pull request permanently unclosable, printing *it will close when the new work
lands* after it had. A stale local graft says nothing about whether the work reached its destination
another way, so it falls through to the other evidence and only colours the reason when that proves
nothing either. `Landing` carries the landing, the resumption and the pull request number from one
read; they had been fetched separately, three history scans per task per pull.

### A review asks the present tense first

`live()` credits; a task still aiming at this branch is *reported*, never credited, whatever it
grafted before; and only a task aiming nowhere falls through to `recorded()`. That last case is what
`abandon` leaves (it retires the land target), and a graft does not un-happen, so a session abandoned
after its work reached the branch is covered. Asking `recorded()` first credited a task that landed,
was reopened for a must-fix and had its fix committed in a session the branch had never seen:
recording that a review read work it did not read, and moving the task to `needs_review` under a live
session. Asking `live()` alone dropped the abandoned case into `Credit::Unrelated`, which the loop
discards; the discard stays, since the loop walks every task in the repo. Refusing the credit does
not stop unreviewed landings in general (the gate does not enforce staleness, on purpose); it stops
one only where the task had never been reviewed.

### The order is pinned where it is declared

All twenty-five status pairs plus the `None` and garbage cases. It had been rewritten twice, checked
only through `close-merged`'s behaviour, and the one arguable rank (whether `cancelled` shares
`done`'s) is exactly the edit a later reader would make. The rule reaches the pull-request path too
(`pr::spent`), where it matters most: a merge reads `MERGED` for ever, so reopening a landed task and
pulling closed it again, unattended, from the `post-merge` hook, over every task at once. That half
predated the recorded-landing path.

## Claims

### An owner id is a type

`jkb_types::AgentId` replaced `split(':').nth(1)`: `Process { host, pid, run }`, `Session { pid,
worktree }`, `Agent { id }` and `Unrecognized`, round-tripping every stored value. `Agent` is an
externally-minted identity from `JKB_AGENT_ID`, for a caller whose process and checkout are not the
thing that persists (a subagent, a resumed session, a cloud run); `owner::preferred_owner` prefers
it, so a subagent's claim outlives the process that took it. Each variant declares what would prove
it through `Liveness` (`Process`, `Worktree`, `External`), a closed enum, so a new shape cannot be
added without the compiler demanding a probe.

### `is_alive` returns `Fact`, and `Unknown` is not dead

`Process` is `Yes` or `No` from `ps -p`; `Session` from whether the worktree exists; `Agent` is always
`Unknown`, since nothing local can know; an unreadable id is `Unknown`. Every takeover site refuses
unless the holder is **proven** gone. `jkb doctor` and `jkb task reclaim` probe each owner where they
run (`crates/jkb-cli/src/doctor.rs`, `probe`) and send only owners proven gone to `task.reclaim`; the
rest form the `unverifiable` bucket they report and never clear (`jkb task reclaim --force` is the
explicit escape). This was a behaviour change: the old predicate treated an unreadable owner as
reclaimable so a malformed claim could never wedge a task, which silently freed live agents' tasks.
Wedging is now checkable and the escape explicit; of the two ways to be wrong, the one that costs a
command wins.

The old objection to session ids answered a different question: whether jkb could go and *ask* an
agent something. A claim needs only a value stable for the life of the work. There is still no TTL
and no heartbeat.

### Reclaiming is a lifecycle transition

`reclaim_dead` moved from `claim.rs` into the transition seam as `observed_owner_gone`, an
effect-only self-loop, so a reclaim appears in the task's history and obeys the same evidence rule;
it writes only the claim columns, so it can never clash with a status transition. Its effect is
`ReclaimFrom(agent)`, distinct from `ReleaseClaim`, because the audit trail distinguishes the holder
letting go from somebody else deciding it had. It returns `Reclaimed { cleared, unverifiable }`.

## History

Superseded and reversed decisions, with what replaced them and why. The largest is the branch-record
table, whose design ran to more than a thousand lines and five audits; it is compressed here to its
lesson.

### Containment as `placements.parent_item_id`

Containment was first simulated at read time, placing a child in its container's namespace and
hiding it again on the way out. It was then stored as `placements.parent_item_id` (migration `V009`,
`ON DELETE SET NULL` so deleting a container returned children to the namespace), which made listing
one query over one table. Replaced by the `containment` table keyed on the child, because an item
has several placements, so the parent was stored once per placement: one fact, N rows, free to
disagree, with `subtasks()` needing `SELECT DISTINCT` to paper over it. The justification for
per-placement containment (the mirror index should list subtasks flat) turned out to be invented.

### Merge detection by asking whether a branch adds anything

A task closed when its `branch=` named a branch merged into trunk. Ancestry is wrong for two of
GitHub's three strategies; measured on a scratch repo exercising all three:

| method | merge commit | squash | rebase | unmerged |
| --- | --- | --- | --- | --- |
| `git merge-base --is-ancestor` | yes | **no** | yes | no |
| `git cherry` (patch-id) | yes | **no** | yes | no |
| `git merge-tree --write-tree` | yes | **yes** | yes | no |

So `gitrepo::is_merged` asked instead whether re-merging the branch produced trunk's own tree
(`merge-tree --write-tree trunk branch == trunk^{tree}`), on git 2.38 or later, falling back to
`--is-ancestor` and saying so. It could not tell a branch squashed away from one just created, nor
a rebase-merged branch (GitHub fast-forwards, leaving it byte-identical to trunk) from a fresh one,
which forced a recorded cut point per branch and everything below. Replaced by recorded landings
and the pull request lookup; `is_merged`, `MergeState`, `merge_base`, `has_own_commits`,
`is_ancestor` and `supports_merge_tree` were deleted.

### Cut points, land targets and landings as facets, then as `branch_records`

"Branch X was cut from Y", "X lands on Y" and "jkb merged X into Y" were stored as tag applications
(`base=<branch>:<sha>`, `onto=`) on whichever tasks named the branch. Tag applications are
item-keyed, multi-valued, untyped and writable from any route, and each property produced its own
defect family across fifteen review passes (47 storage findings, 100 in the wider cluster, 20
must-fix): the encoding leaked to about 12 sites; the documented repair `jkb task tag set base=`
deleted other branches' records; `HEAD` and 40-hex non-commits were stored; and five write routes
were found one at a time, the fifth after a reserved-facet guard was added for the other four, with
the guard's own asymmetry a must-fix. Six ascending choke points did not close it.

The response applied the rule other clusters had converged on (vector id reuse, the sync document
structure): **prefer an invariant the schema enforces over one every caller must uphold.**
`branch_records` (`V013`, `jkb_core::branch`), keyed `(repo, branch)`, with CHECKs on the cut point's
form and on paired columns. `branch=` deliberately did not move (its defects were choice rules);
`onto=` did, to `land_target`; existing `base=` and `onto=` values were deleted, not back-filled,
since back-filling imports exactly the values proven unreliable. The staleness rule (a name outlives
the branch that held it) became the `WHERE` of one `INSERT … ON CONFLICT DO UPDATE` rather than a
forget-then-insert sequence. Because three states (rebase-merged externally, merge-commit-merged
externally, a recycled name) present one identical git signature, and the first must close while the
last must not, an **instance anchor** was added: the branch's creation reflog entry, `(anchor_sha,
anchor_ts)`, verified on git 2.50 as written once per instance and forged by no verb, with exact-ref
`gc.refs/heads/<branch>.reflogExpire = never` retention. `landed_head` stopped a landing event
re-creating the staleness one column over. The audits predicted the table would remove about 10 of
20 must-fixes and cost 2 or 3 of its own.

Replaced whole by the transition log and the pull request lookup (`V016` drops the table and
migrates nothing, for `V013`'s reason). The diagnosis survives: an item-keyed, multi-valued,
untyped, open-write store cannot hold a per-branch fact, and a schema invariant beats a choke point.
The state machine applied it one level further, to the question the table existed to answer: keyed
by a name, the record still had to be kept in agreement with a moving world, and every column added
after a defect (the supersede clause, `landed_head`, the anchor, `--forget`) existed for that
reconciliation. An append-only history and a never-reused pull request number leave nothing to
reconcile. Deleted with it: `jkb-cli/src/base.rs` (932 lines), `jkb_core::branch` (974), the reflog
plumbing, `repo::landed_for_action`, `credited`, `clear_land_targets`, `measure_root_for`, `jkb task
base`, and about 35 tests that pinned mechanics rather than rules (five were rewritten where the rule
survived). `V013` and `V012` had locked older binaries out of `~/.jkb/jkb.db`, accepted at the time.

### A merge-queue verb that refused unless the branch tip was an ancestor of the target

The queue's landing verb was specified to refuse unless the branch's tip was an ancestor of the
target, so it could not fabricate a landing. The queue rebases a **detached** HEAD and never moves
the branch ref, so after the first entry every branch's commits are rewritten and the check would
have refused every entry but the first, the case a serial queue exists for. Replaced by asking the
content question readers asked. A related lesson: recording a cut point for a batch at the moment of
a landing is wrong, because a landing is exactly when the target stops being provably untouched;
measuring there stored the tip and froze the whole batch.

### A hand-repair verb that accepted a commit id

`jkb task base <uid> <branch> <sha>` produced three findings across three passes, all one shape: the
sha nearest a user's hand is the branch tip, and a cut point equal to the tip froze the task with no
repair path. Each was fixed by rewording a message. Replaced by `jkb task base --forget <branch>`
(dropping the cut point, not the row, so the task stayed in `staging ls`), then deleted with the
table. The general rule it taught is the refusal-names-an-event rule of the state machine.

### Refs and git hooks as stores of branch facts

A git ref per branch (`refs/jkb/base/<branch>`) was rejected and stays rejected: jkb runs inside
other people's professional repositories and must not decorate them with refs, notes or tags the
user never asked for. A `reference-transaction` hook calling `forget` was rejected on four grounds
verified on git 2.50: a repo-local `core.hooksPath` (husky's standard install) silently masks the
global hook; a missed firing loses the event permanently; libgit2 and JGit tools bypass CLI hooks;
and a pruned-then-repushed `origin/<branch>` arrives as a creation. A git shim blocking deletion was
rejected as silently evadable. A `.git/config` instance witness (`branch.<name>.<key>`, which
`branch -m` moves, `branch -D` removes, a recreated branch lacks, and `branch -f` keeps; local and
unpushed like `.git/info/exclude`) was verified and judged better than the reflog anchor on four
counts, but not built, since with the cut point gone there is no per-branch record to protect.
Recorded so none is re-proposed.

### Review facts as tags

`reviewed=<sha>`, `review=<ns>` and `review-waived=<sha>` were facets on the task, written with
`set_facet`. A tag is content any writer may set, the sync engine included, so a `tasks.md` line
edited in the dev container to carry `#review-waived=x` waived the gate. A reserved facet was not the
answer (the branch-record episode had six choke points fail). Replaced by an append-only `reviews`
table that the gate alone reads; the migration and the round rules are the agents-and-roles design.
Likewise `landed` now reads the landing transition, never `status = done`, which a synced checkbox
can write.

### Staging state from `is_merged`

`staging ls` derived `merged` and a task's `landed` state from `gitrepo::is_merged` against trunk,
and its staging set from `onto=` facets, then from `branch_records.land_target`. Replaced by batches
from the transition log and spent-ness from task status, when the inference was deleted.

### The `land.lock` pid file

Landing was serialized by `.jkb/land.lock` holding the holder's pid, a lock whose pid was gone being
stale and taken over. Replaced by a database lease (the daemon-and-messaging design), which the host
and the dev container can both see.

### Absolute session owners

The session owner was `session:<pid>:<abs-worktree-path>`, with `owner_pid` reading field 1 so every
existing probe parsed it unchanged. Replaced by the home-relative form with the opener, because the
host and the dev container mount `~/repos` under different homes and each answered `Unknown` about
the other's sessions. Old absolute owners still parse.

### An attended/unattended axis

A flag built on the session owner's pid reported *every* session as unattended, including the one
being sat in, and advised abandoning it. Removed: nothing observable distinguishes a session being
worked from one walked away from, so the honest report is the observable facts, uncommitted work and
commits ahead. A real attendance signal would need a process living as long as the session (the
terminal `exec`ing the agent), a separate change rather than a flag.

### Unreadable owners reclaimed automatically

Claim liveness was a `bool`, and an owner id that could not be parsed was treated as reclaimable, so
a malformed claim could never wedge a task. Replaced by `Fact`-valued liveness, where `Unknown` is
reported and cleared only with `--force`: an automatic sweep that frees a live agent's task is a
silent wrong action, and a reported hold is a visible one. `pid_exists` had also spelled "`ps` would
not spawn" as "gone".

### The changelog and undo findings met along the way

Rounds 4 to 8 of the branch-record work produced a must-fix family in the changelog and `jkb undo`:
a typed entity with an untyped op, upserts logged as `insert`, `undo_last` skipping what it could not
invert and reverting an unrelated older transaction instead, and inverses that failed only at apply
time. Its resolution (ops derived, never chosen; undo refuses rather than retargets; any apply-time
error becomes a named refusal) belongs to the foundation design, which records it.
