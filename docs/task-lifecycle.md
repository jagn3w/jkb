# The task lifecycle — subtasks, sessions, staging, landing

How a task moves from the frontier to merged: subtasks and containment (D34/D35), the
per-task git worktree (D36), the checkable state machine and the append-only transition
log (D48), the superseded `branch_records` table (D46), review-gated landing (D38), and
the design gate that keeps undecided work away from the swarm (D28).

Part of the jkb documentation set; see [CLAUDE.md](../CLAUDE.md) for the
conventions every session is expected to know.

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
- **A session owner names its worktree home-relative, and the Claude Code session that opened it**
  (tasks S6.4, decision E; `openspec/changes/jkb-message-queue/design-s6-4.md`). The form is now
  `session:<pid>[@<claude session>]:<worktree>`, and the worktree is written `~/…` when it lies
  under `$HOME`. The host and the dev container see the same `~/repos` under different homes, so an
  absolute path named a directory only one side had, and the other side's probe answered `Unknown`
  about every session it did not open. Each side now resolves `~` against its own home
  (`owner::session_worktree`), and old absolute owners still parse and are judged as before. The
  opener (`CLAUDE_CODE_SESSION_ID`) is provenance and never decides liveness. It does decide one
  thing: `task work` refuses to take over a checkout whose opener the session registry says is
  still **live**, unless it is that same session. That is how two Claude sessions stop ending up in
  one worktree. `ended` and `unknown` let the takeover through, as before, and the refusal names
  `jkb task release` for an opener that is gone but was never recorded as such. Pinned by
  `a_session_opened_by_a_running_claude_session_is_not_taken_over` and
  `a_session_owner_names_its_worktree_under_the_home`.
- **The session verbs' database steps are ops** (tasks S6.4, `jkb_api::sessions`), and every write
  they make is a compare-and-set on the owner the verb judged. `task.start` and `task.take` clear
  only the owner they were told about. `task.abandon` changes nothing when the claim is no longer
  the one observed before the git work. The release after a failed worktree add goes through
  `task.release` with the verb's own owner, where it used to clear the claim unconditionally.
  `task start` and `task gate` (show) run in the dev container through `jkb serve`. Storing a gate
  never does (decision A): it is a shell command the host later runs, so only the host stores one.
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
  **It is asked through `repo::work_for`**, which returns the session *and* the branch together, so
  a caller cannot take one and pick the other for itself — which is what `jkb task abandon` did as
  a third implementation, taking the first `branch=` value (`tag::applications` orders by value) and
  deleting a stale sibling under `--delete-branch` while the row the user clicked named the live
  one. The batched listing still calls `work_branch` directly with the sessions and refs it has
  already read once: same rule, not a second one.
- **A land target is a *branch*, not a revision that resolves** (`gitrepo::branch_name` →
  `Is`/`Unknown`/`NotABranch`). `branch_ref` maps a branch name to a ref you may hand to git;
  this maps an arbitrary string to the **key** `branch_refs` uses, and the two come apart on
  exactly the values that hurt — `origin/<batch>` and a tag both `rev-parse` fine, and both were
  accepted and stored, the first under a key `jkb staging ls` cannot look up. The canonicalization
  and the refusal live at `repo::record_land_target`, the single writer, so the next flag that
  accepts a branch cannot get it wrong; the CLI verbs ask the same question first only for the sake
  of a sentence the user can act on. Trunk is compared against the canonical name, not against two
  spellings guessed by hand.
- `scripts/merge-queue.sh` is still the swarm's queue and still a git/gate runner. It makes
  **one kind** of knowledge-base call — `jkb task landed <branch> --onto <target>`, recording the
  landing event (D46) — from two arms: after a genuine fast-forward, and when `<target>` already
  contains everything the branch adds. That makes it a jkb client, so its caller must export
  `JKB` and `JKB_DB` — `.claude/workflows/task-swarm.js`'s `QUEUE_ENV` does, and the script header
  states the contract. The CLI is the home of the human path because the UI calls it directly and
  it must work in any repo.
- **`jkb task land` is NO LONGER the same algorithm**, and the divergence is deliberate rather
  than drift, so it is recorded here instead of only in a Rust comment. Two differences, both
  introduced on 2026-09-12 when the queue was reworked:
  - **Ordering.** The queue gates the rebased commit while it is still detached and only then
    fast-forwards, so `<target>` never points at an ungated commit. `jkb task land` still
    fast-forwards first and rewinds with `reset --hard` on red — the window is the whole gate,
    and an implementer told to cut from the integration branch can carry ungated commits away
    inside it. Filed, not fixed: reordering the human path changes `graft`'s contract and
    `do_land`'s flow and wants its own change.
  - **What "nothing to land" asks.** The queue asks the content question — a branch whose net
    diff against its merge-base is empty is refused however many commits it carries — because it
    closes whole task groups unattended, and a phantom landing there unblocks dependents with
    nothing implemented. `gitrepo::graft` still asks the commit question (`ahead_count == 0`).
  Both are tracked as their own tasks. Until they close, a reader comparing the two must expect
  them to differ **here** and nowhere else.

## The lifecycle is a checkable state machine, and a landing is an event (D48)

**Reviewed in three ranges (`low`, ~18 agents): 36 findings, 8 must-fix, all fixed.** What the
reviewer caught that the machine's own checks did not, recorded because the pattern repeats:

- **Absorption discarded a plan.** The idempotence rule treated any event whose destination you
  were already in as a no-op — but arriving there another way leaves the plan unapplied.
  `abandon` on an operator-reopened task skipped its guard *and* its claim release, reported
  success, and the surviving claim held the task off every frontier. A row with a guard or a plan
  is never absorbed now; the domain declares the self-loop.
- **Two of my own tests could not fail.** One asserted `stdout contains uid` where the failure
  path prints the uid too; one asserted a defect that is structurally unreachable for its machine.
  Both are why `jkb task landed` never credited a swarm group for a whole branch.
- **A guard that only reports is not a guard.** The open-subtasks rule lived in the machine's
  `land` plan, which is applied *last* — so it narrated a landing that had already grafted and
  disposed of the session. It belongs in the preflight, beside every other precondition.
- **`Unknown` spelled as `No`, in the probe that protects every claim.** `pid_exists` folded "`ps`
  would not spawn" into "that process is gone" — the exact defect `Fact` exists to prevent.
- **A choke point with a third door.** `jkb task claim`, the verb the swarm runs on every task,
  still wrote the claim directly; swarm work had no `start` entry at all, and the two claim verbs
  answered `needs_review` oppositely.


The `staging-workflow` branch took 44 review passes and ~80 must-fix findings. Sorting the
task-lifecycle ones by *cause* rather than by site gives six groups, and each maps to a property
the code had no way to have. Design: `openspec/changes/jkb-state-machine/`.

- **The lifecycle was written down nowhere**, so about a dozen sites each derived the part their
  own question needed — `claim::claim`'s terminal pre-check, `staging::State::from_status`,
  `land_blocker`, `land_preflight`, `close-merged`, `task abandon`, `review record`,
  `merge-queue.sh`, the VS Code row. *Two of them answering one question differently* is the most
  common finding shape in the corpus, and the standing fix — make the two share a function — never
  reaches the thirteenth site.
- **`crates/jkb-fsm` is a dependency-free library where a lifecycle is a `&'static` table**, so it
  can be *walked*, and walking it is what makes these checkable at all: every state reaches a
  terminal one (`Wedged`), every state is reachable, no two rows compete (`Nondeterministic`),
  every reconciliation carries evidence (`UnguardedReconciliation`), every refusal's advice is an
  event the machine really accepts (`UnreachableRemedy`), every verb can be run twice
  (`Unrepeatable`), and under every observation something can still move the object (`DeadEnd`).
  `Machine::dot()` renders it — the artifact whose absence is the first item on the list.
- **`Unrepeatable` was found by the fix for the absorption bug below, and is the pair to it.**
  Correcting absorption — a row with a guard or a plan is never absorbed implicitly, because the
  object may have arrived by another route with that plan still owed — is right, and it silently
  turned five destinations into refusals: `land` on an already-landed task among them. That is
  S1.6's *the verb is re-runnable* lapsing, and a lapsed guarantee is worse than one never
  claimed, because the retry advice everywhere else assumes it. The two rules together say: **a
  verb you run is always answerable at its own destination, an observation only where somebody
  wrote down what re-seeing it means.** Satisfying it does not mean "make it a no-op" — a domain
  that wants the second run to fail declares a self-loop whose guard denies, and gets a sentence
  and a remedy instead of the silent absence of a row. Two rows here keep their guards on purpose
  (`abandon` from `open`, `observed_landed` from `done`): those verbs may still have work to do.
- **`Fact` is three-valued and has no method that collapses `Unknown` to a `bool`.** Nine
  must-fixes are one unobtainable answer spelled `false`: `ahead_count` returning `0` (which means
  *nothing to land*) for a branch it could not resolve; `has_own_commits` answering *no* when
  `rev-list` failed; a land gate that could not tell *no findings* from *the namespace resolved to
  nothing*. `is_yes` and `is_no` **both** mean *proven*, so a guard states its polarity in code:
  landing needs `work_dirty.is_no()` (an unreadable checkout refuses) and `has_commits.is_yes()`.
- **A transition yields its effects as one value.** `settle_landing` wrote the status, cleared the
  claim, then asked git to remove a worktree git refused — leaving a task `done`, unclaimed, with
  a live session. `Outcome::Moved` carries a `Vec<TaskEffect>` produced *with* the move, and
  `transition::perform` is the one seam that applies it. The ordering rule that makes a git
  failure survivable is stated once: **apply the plan last**, after every fallible external step,
  so a failure leaves the task where it was and the verb is re-runnable (which S1.6 guarantees is
  a no-op once it has worked).
- **A refusal names an *event*, not a sentence.** Passes 31 and 32 are the same finding one
  message apart — a printed remedy whose obvious argument froze the task permanently — and the
  fix each time was to reword. `Denial::remedy` holds a `TaskEvent`, and `Machine::audit`
  validates every remedy the machine can produce over a whole context matrix. **It caught a bad
  remedy in this change as it was written**, and a state passed beside the context that could
  disagree with it (fixed by `Stateful`: the state is read *out of* the observation).
- **`task::set_status` is not a hole beside the machine — it is the `override` event**, and a
  synced file's checkbox is `set_from_file`, a *guarded reconciliation* (the file may only speak
  for a task it backs). Both use `Dest::Stated`, a destination the caller names. One rule keeps
  the checks honest: **a `Stated` edge is excluded from the liveness walks**, because a state
  whose only exit is somebody naming a different state is still wedged.

### `branch_records` is gone; the history replaced it (V015, V016)

- **Two facts outlive git's memory**: where a branch was cut, and that it landed. `branch_records`
  stored them as *properties of a branch* — a mutable projection of the past, keyed by a name git
  lets you delete, recreate and reuse — so the row had to be kept in agreement with a moving
  world. The supersede clause, `landed_head`, the reflog instance anchor, `--forget`: every one
  was added after a defect, and all of them existed for that reconciliation.
- **`task_transitions` is append-only**, so it makes no claim about the present and there is
  nothing to reconcile. A name that changes hands appends a row rather than corrupting one;
  superseding stops being an operation. Deliberately **not** changelogged (the `blobs` precedent):
  it *is* an audit record, and a transition later reverted by `jkb undo` stays, which is the
  honest reading of a history.
- **Branch names became labels on events.** `land_target` is the last `onto` recorded — reset by
  an `abandon`, because where work lands is a property of the session doing it. Two tasks told
  different targets are two entries with timestamps, not one row keeping whichever wrote last.
- **`jkb task why <uid>`** prints it: every transition, who applied it, and the evidence each guard
  fired on. Fourteen must-fixes are "held for ever with no way to see why"; that is now one command.

### Evidence of a landing is spent once the task is put back to work

The log removed the reconciliation problem from **writing** — an append-only history makes no
claim about the present, so nothing has to be kept in agreement. It moved it to **reading**. Every
caller asks a present-tense question (*has this landed?*, *where does it land?*), and turning a
history into a present-tense answer needs a rule for when an older row stops counting — which was
written separately in each reader, and they disagreed. `land_target` stopped at `abandon`;
`landed` stopped at nothing. Five findings across two review rounds are that one gap.

- **The rule is asked of the status ORDER, not of a list of events.** `transition::resumed` is
  the one statement: the newest row that moved the task **backwards** through
  `open -> in_progress -> needs_review -> done` (`TaskStatus::stage`, the D27.7 lifecycle written
  down as an order). The obvious repair — give `landed` the same stop-list its sibling has — is a
  fourth private rule for a fifth reader to get wrong, and one a newly-added event has to be
  *remembered* and added to. Every row already records where it moved the task.
- **It took two goes, and the first was a narrower rule that looked identical.** *Moved out of a
  terminal status* is the same answer for the case it was written against — a landed task
  reopened — and misses the one that matters most: **`abandon` is `in_progress -> open`**, neither
  side terminal. A landing recorded while a task was held by an open subtask survived the abandon
  that destroyed its session, and the task auto-closed over live work. Asking the order covers
  both, plus `request_changes` and a resume out of `needs_review`, with no special case.
- **Nothing that stands still is a resumption**, which is what stops the row recording a held
  landing (`in_progress -> in_progress`) superseding **itself** and freezing its own task for
  ever. Found by running it.
- **`jkb undo` had to start recording what it did.** It restores `items.status` straight from the
  changelog, and `task_transitions` is deliberately not changelogged — so undoing a close left the
  landing looking live and the next `git pull` closed the task again, a loop undo could not break.
  It now appends an `undo` transition, from the statuses observed either side of the inversion
  rather than from what the entry claimed, and **only for a task that still exists**: inverting an
  insert deletes the item, and the history's foreign key onto `items` failed the whole undo.
- **A superseded landing is context, never a verdict** — and getting that wrong in *both*
  directions took two rounds. Spelling "spent" and "never landed" the same way sent `close-merged`
  off to ask GitHub about a pull request a locally-grafted branch never had, and reported that as
  the reason. Then treating "spent" as *the* answer, and returning on it, left a task whose work
  was redone and merged as a pull request permanently unclosable — printing *it will close when
  the new work lands* after the new work had landed. A stale local graft says nothing about
  whether the work reached its destination another way, so it falls through to the other evidence
  and only colours the reason when that proves nothing either.
- **`Landing` carries all of it from one read** — the landing, the resumption, the pull request
  number — because those three were fetched separately and the third re-derived a row the first
  had already found and thrown away, three history scans per task per `git pull`.
- **A review asks the present tense first, and the historical question only where it has no
  answer** — and getting that order wrong is the sharpest hole in the area, because it ends in
  landing unreviewed commits. `live()` credits; a task still aiming at this branch is *reported*,
  never credited, whatever it grafted before; and only a task aiming nowhere falls through to
  `recorded()`. That last case is what `abandon` leaves — it retires the land target — and a graft
  does not un-happen, so a session abandoned after its work reached the branch is covered.
  Asking `recorded()` first credited a task that landed, was reopened for a must-fix, and had its
  fix committed in a session the branch had never seen — recording that a review read work it did
  not read, and moving the task to `needs_review` under a live session. (It does **not** follow
  that refusing the credit stops an unreviewed landing in general: `gate_with` checks only that a
  `reviewed=` facet *exists*, never that it is current, and D38 declines to enforce staleness on
  purpose. It stops one only where the task had never been reviewed at all.) Asking
  `live()` alone dropped the abandoned case into `Credit::Unrelated`, which the loop discards. The
  discard is right and stays: that loop walks every task in the repo, so reporting `Unrelated`
  would list most of the backlog on every run.
- **The order is pinned where it is declared**, over all twenty-five status pairs plus the
  `None`/garbage cases. It had been rewritten twice, checked only through `close-merged`'s
  behaviour, and the one arguable rank — whether `cancelled` shares `done`'s — is exactly the edit
  a later reader would make.
- **It reaches the pull-request path too** (`pr::spent`), which is where it matters most: a merge
  reads as `MERGED` for ever, so reopening a landed task and running `git pull` closed it again —
  unattended, from the `post-merge` hook, over every task at once. That half **predates** the
  recorded-landing path and predates this branch.
- **Where it cannot be told, the task is held.** A merge with a known resumption it cannot be
  placed against is `Undecidable`, not "live" — closing there picks the burying direction on the
  strength of a missing field. `Live` is the default because *no resumption* is the normal case,
  not because a missed close is cheap: a missed close costs one command, a wrong one buries work
  in flight (D34.4).
- **The pure half is separated from the `gh` call** so it is testable at all — a rule exercisable
  only by shelling out to an authenticated network client is a rule nothing checks.

### Auto-close is a lookup on an id that is never reused

- **The inference was hard for one reason**: a squash or rebase merge rewrites the commits, so
  containment cannot be tested, and the weaker question `is_merged` asked — *does this branch add
  anything to trunk?* — cannot tell a branch squashed away from one that never started. Making
  that answerable needed the whole cut-point/anchor apparatus, and it produced roughly a quarter
  of the corpus's must-fixes.
- **A pull request number is minted by GitHub and never reused**, so there is nothing to
  disambiguate. `jkb task pr <uid> [number]` records or discovers it (refusing to guess when a
  reused branch name matches two); after that the branch name is never consulted. `close-merged`
  asks `gh`, and **everything degrades to `Fact::Unknown`, never to a `no`** — no `gh`, no
  network, no GitHub remote, an unrecognized state — so the task is *held with the reason printed*.
  It also produces an answer the inference could not: *closed without merging*. The field names,
  the flags and the **uppercase** state values are verified against `gh` itself rather than from
  memory (`gh pr view --json`'s own field list, `gh pr list --help`, and `gh`'s `display.go`); the
  one live call is an `#[ignore]` test beside the ollama and Chrome smokes.
- **Deleted:** `jkb-cli/src/base.rs` (932 lines), `jkb_core::branch`, `gitrepo::is_merged` /
  `MergeState` / `merge_base` / `has_own_commits` / `is_ancestor` / the reflog-anchor plumbing,
  `repo::landed_for_action` / `credited` / `clear_land_targets` / `measure_root_for`,
  `jkb task base`, and ~35 tests that pinned the mechanics rather than the rules. `V016` drops
  `branch_records` and migrates nothing, for `V013`'s own reason: importing values whose
  reliability was the problem defeats the store they are imported into.
- **What jkb performs, jkb records.** `jkb task land` writes a `land` transition after its gate is
  green; `scripts/merge-queue.sh` calls `jkb task landed <branch> --onto <target>`, which now
  closes every task on that branch. `jkb task review record` credits a task whose work jkb
  *grafted* onto the reviewed branch — a recorded event, where it used to be a containment probe
  that could not tell an empty session from a landed one.

### The second machine: the sync journal, and what it moved

`jkb-sync/src/lifecycle.rs` declares the per-file journal on the same library — a **reconciler**,
not a lifecycle: nothing finishes, every event is `Reconciled`, and the question is never *what
may I do next* but *which condition applies to what I just saw*. It moved the library three
times, which is the evidence that "it generalizes" is a claim worth making:

- **`is_terminal` → `is_settled`.** A synced file is never finished; it settles and is edited
  again. Under the old name the machine either had no terminal state — making `Wedged` vacuous —
  or had to lie about one. What the checks want is *rest*: the object owes the system nothing.
- **`State::awaits_input`** (default `false`). A conflicted file is waiting on a person, so no
  observation moves it; without this, `DeadEnd` fired on every such observation. A lifecycle
  keeps the default because an operator escape (`cancel`) is always available.
- **The initial state may be at rest.** A file an export-only mount holds no items for is nobody's
  business.
- What did **not** move is what carries the value — and `reconcile` refusing ambiguity turns out
  to be the *central* property here rather than a corner case, because evaluating every
  candidate's guard against one observation is exactly D45.5's *"a route is not a cause; the
  condition must dominate every arm"*.
- **The modelling found that `needs_attention` is two states** — a quarantine wants the file
  fixed, a blocked write wants the store fixed — which `Outcome::Refused`'s own doc already
  warned about in prose. And that a flag whose cause has gone is not always cleared (an
  import-only mount with a store-side-only change writes no row): modelled faithfully, filed, not
  fixed.
- `sync_state.status` now has **one writer** (`lifecycle::status_for`), replacing four
  hand-written spellings.

### The third machine: investigation units, where the rules are strategy-supplied

`jkb-core/src/nstype/lifecycle.rs` declares `items.resolution` on the same library — **two tables
over one state set**, which is the axis neither earlier machine had. It moved the library twice
more and found two rules that existed only in the shape of a function:

- **`debugging` concludes differently, twice.** A settled result can go **stale** and return to
  the frontier (an observation about a mutable system carries a `commit-range=`); and a tombstone
  is **not** revived by fresh evidence, where the base table's is. Both were already true — the
  first is one `if` in `debugging::resolution_rollup`, the second is that rollup's early return
  versus `default_rollup`'s fall-through — and neither was discoverable from anywhere else.
- **The strategy supplies the facts; the machine supplies the rules.** `resolution_rollup` (which
  returned a *conclusion*) became `unit_facts`. A rollup that concludes has to encode the priority
  of contradictory evidence in the order of its `if`s, where nothing can see it and nothing would
  notice a reorder. As guard clauses the priority is arguable and `audit` proves it exclusive. A
  strategy that merely *observes* differently — a `debugging` symptom is confirmed by a verified
  fix, not a `confirms` edge — now needs no table of its own.
- **Reachability counts a `Dest::Stated` edge; liveness still does not.** *Can the object be here*
  is answered yes by an operator override; *can the lifecycle get it out of here* is not.
  Collapsing them reported `abandoned` — which only a person ever sets — as unreachable dead code.
- **`Resolution::Unresolved` declares `awaits_input`.** Nothing the system can do moves it;
  evidence arrives from outside as an edge somebody links. Unlike a task's `open`, which always
  has `cancel`.
- **`UnusedEvent` is a per-machine statement**, and this is the first place two machines share one
  event enum. The domain filters it only where *another* machine in the family declares the event,
  and asserts the union separately — a narrow filter, so an event no table uses is still a defect.

### Claim keying: an owner id is a type, and `Unknown` is not `dead`

- **`jkb_types::AgentId`** replaces `split(':').nth(1)`: `Process { host, pid, run }` /
  `Session { pid, worktree }` / **`Agent { id }`** (new — an externally-minted identity from
  `JKB_AGENT_ID`, for a caller whose process and checkout are not the thing that persists) /
  `Unrecognized`. Each declares what would prove it via `Liveness`, a **closed enum**, so a new
  shape cannot be added without the compiler demanding a probe for it.
- **`owner::is_alive` returns `Fact`.** An `agent:` id and an id we cannot read are
  `Fact::Unknown` — *unestablished*, never *dead*. `transition::reclaim_dead` frees only claims
  **proven** gone and returns the rest in an `unverifiable` bucket that `jkb doctor` and
  `jkb task reclaim` report but never clear. That is a behaviour change: the old predicate treated
  an unreadable owner as reclaimable, which silently frees a live agent's task. Of the two ways to
  be wrong, the one that costs a command wins (D34.4).
- **The old objection to session ids answered a different question** — whether jkb could go and
  *ask* an agent something. A claim needs only a value stable for the life of the work; it does
  not need to be reachable. There is still no TTL and no heartbeat.
- **Reclaiming is a lifecycle transition** (`observed_owner_gone`, an effect-only self-loop), so it
  appears in the task's history and obeys the same evidence rule as everything else. Its effect is
  `ReclaimFrom(agent)`, distinct from `ReleaseClaim`, because the audit trail distinguishes the
  holder letting go from somebody else deciding it had.

## A branch is a record, not a tag value (D46) — SUPERSEDED by D48

**The `branch_records` table is gone** (`V016`), and with it the cut point, the instance anchor
and the landing columns. What survives is the *diagnosis* — an item-keyed, multi-valued, untyped,
open-write store cannot hold a per-branch fact — and the rule it produced: prefer an invariant the
schema enforces over one every caller must uphold. D48 applies it one level further, to the
question the table existed to answer. `branch=`/`repo=` stay facets, for the reasons below.


- **Re-founded by the B-series.** "Branch X was cut
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
    - **A landing onto the branch you are asking about *is* the answer** — `landed_for_action`
      stops there rather than walking on to ask "and is `S` contained in `S`?", which needs `S`'s
      own cut point. `jkb task review record` passes the *reviewed branch*, so without this it
      declined to credit work jkb had itself just grafted onto that branch. It is not the "landed
      onto a batch with no record" state, which is still **held**: there the target is a different
      branch, and whether it in turn reached trunk is a question the record genuinely cannot
      answer.
    - **The queue's verb reports, and deliberately does not measure.** The obvious fix for a
      landing onto a target with no cut point is to record one there — and it is wrong: a cut point
      is provable only while a branch is untouched, and a landing is exactly the moment the target
      stops being one. The queue's first entry fast-forwards the target onto commits its source
      branch still holds, so `has_own_commits` truthfully says "nothing of its own" and the **tip**
      gets stored for the whole batch, which is permanent. The record has to be made when the batch
      is *cut* (`--onto <batch>`); `jkb task landed` says so, on stderr and as `creditable: false`,
      and `merge-queue.sh` no longer swallows that.
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
  derived `state`: `implementing` / `review` / `landed` / `dropped` — `dropped` being a
  **cancelled** task that was on the branch, kept apart from `landed` because reporting the two
  as one would say a dropped task shipped. A branch adding nothing to trunk is
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
- **No review gate in `scripts/merge-queue.sh`** — deliberately, and that is the only sense in
  which D38 left it alone (it gained a `jkb task landed` call under D46).
  The swarm already runs a fresh REVIEWER before a group reaches the queue (D27.6) — that *is* its
  gate, and stricter.
  Requiring `reviewed=` there would make the REVIEWER write facets to satisfy a check its own
  approval already answered. **Review staleness** is recorded (`reviewed=<sha>`) but not
  enforced: making every post-review fixup force a re-review is the fastest way to make people
  reach for `--no-review` by reflex.

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
