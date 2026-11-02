---
description: Make this session the COORDINATOR of a task swarm. It starts a jkb-designer for ready tasks still awaiting a design, up to the run's designer budget, groups overlapping ready jkb tasks, starts one jkb-implementer per group and a fresh jkb-reviewer per pass, starts a jkb-systemic-reviewer when review keeps failing in one area, and runs the deterministic merge queue (no agent), which lands approved branches on one feature branch and records each landing. Claim-guarded, and it keeps going as dependents unblock.
argument-hint: "<jkb-path | task-uids...>  [--branch <name>]  [--dry-run]  [--no-design-gate]  [--designers <n>]"
---

This session becomes the **coordinator** of a task swarm. It works inside the tasks' lifecycle
and RBAC: jkb holds every rule the coordinator follows. The swarm has these parts:

- **Designer.** A task at `design` that the run's design gate holds back gets one `jkb-designer`
  subagent, which writes the task's design into it, with its open questions, and fires
  `submit_design`. The task then waits at `design_review` for your approval. No frontier read
  offers a task at `design_review`, and nothing tags it: approved, it is at `implement` and the next
  read builds it; rejected, it is back at `design` for a designer. A rejected task's revised design
  is appended under a heading that supersedes the earlier one, and the last Design section governs.
  A run starts designers on at most `--designers <n>` tasks in all (default 3; `0` designs nothing),
  because each design costs your review at `design_review`. A paused group's redesign (below) is
  exempt: its work is already built and reviewed, and the budget is for starting new designs.
  Its role comes from the operator's agent-type map (`jkb-designer` → `designer`). The coordinator
  cannot submit a design itself: jkb gives it no `submit_design`.
- **Groups.** The coordinator reads the ready frontier and groups overlapping tasks, at most 4
  per group.
- **Implementer.** Each group gets one `jkb-implementer` subagent, which builds every task in the
  group on one branch. It stays with the group through every review and fix.
- **Reviewer.** Each pass gets a fresh `jkb-reviewer` subagent, which checks the branch against
  the whole group, then files its findings and records the round in jkb (`jkb role bind`, `jkb task
  review file`, `jkb task review record`). Its role comes from the operator's agent-type map
  (`jkb-reviewer` → `reviewer`), never from a grant the coordinator mints.
- **Workflow.** The coordinator reads the frontier by workflow phase, in up to four reads: first
  the tasks past design (`implement`, `review`, `landable`, `systemic_review`), then the tasks this
  run designed whose design you rejected (tagged `swarm-designed=<owner>`; a re-design uses no
  budget, so this read is never limited by it), then the tasks at `design` tagged
  `design=approved`, then the other tasks at `design`, limited to the designers the budget has
  left. Every read leaves out review findings (tagged `finding=review` when filed) and
  what the run has settled. It admits a group only if every task in it is admitted. A task at
  `implement`, `review` or `landable` is a *workflow* task. A task still at `design` that the run's design gate let through
  (the `design=approved` tag, a hand-pick by uid, or `--no-design-gate`), and whose design was never
  turned down, is a *legacy* task: its design was never approved in jkb, so it is built on the
  legacy path, routed on the reviewer's verdict, and named as legacy in the report. A task at `design` that the gate holds back goes to a designer, and so does one
  whose design was turned down (its last workflow move a `reject_design` or `systemic_redesign`),
  whatever the gate, the tag or a hand-pick says. A task at
  `design_review` is in no read, and the report lists it as awaiting your design approval. A task an
  earlier run left at `systemic_review` goes to a systemic reviewer. A task at any other phase is
  left out and reported. A group never mixes legacy and workflow tasks. The workflow moves through
  the group's lead (its first task, recorded as `swarm-lead=<lead uid>` on every task of the
  group). For a workflow group, when the implementer reports ready the coordinator reads the lead's
  phase and fires `submit_work` from `implement`, nothing from `review` (already submitted), and
  `rework` then `submit_work` from `landable`; it stalls the group from any other phase, or on a
  refused fire. After a merge-queue eject it fires `rework`. After each review it runs `jkb workflow
  observe <lead>` and routes on the phase jkb reports, never on the reviewer's verdict word:
  `implement` goes back to the implementer, `systemic_review` goes to a systemic reviewer, and
  `landable` goes to the merge queue.
- **Systemic reviewer.** When a round's must-fixes repeat an area an earlier round found, jkb moves
  the lead to `systemic_review`, and the coordinator starts one `jkb-systemic-reviewer` subagent on
  it instead of sending the work round the loop again. It finds the cause, records it in the task
  (`Systemic cause: …`), and fires exactly one exit with a written reason: `submit_systemic` sends
  the cause back to the same implementer, and `systemic_redesign` sends the task back to design. Then
  the group pauses with its claims, branch and implementer kept: a designer revises the design (always
  started, whatever the designer budget has left), the
  task waits at `design_review` for you, and once you approve it the same implementer carries on. A
  paused group still awaiting your approval when the run stops is released and listed as awaiting
  your design approval. Its role comes from the operator's agent-type map (`jkb-systemic-reviewer` →
  `systemic_reviewer`); the coordinator cannot fire either exit itself.
- **Findings.** Whoever verifies a fix closes its finding. When a round comes back clean, the
  coordinator closes the must-fix findings of the group's earlier rounds (`jkb task set <finding>
  --status done`) before the merge queue. Concerns and nits stay open as backlog.
- **Merge queue.** `scripts/merge-queue.sh` is deterministic, not an agent. It runs the gate on the
  rebased branch, lands it on the feature branch, records the landing, and names its own outcome.

Each worker's prompt comes from `jkb workflow agent show`, so an operator copy saved in Code
Factory's Workflows tab is what runs. The subagent types are installed by `jkb commands install`,
which runs automatically.

Argument: `$ARGUMENTS`

This starts many subagents and can be expensive. Follow the steps in order, and **start nothing
until after the cost preview and confirmation (steps 3–4).**

## 1. Parse the argument

- If `$ARGUMENTS` is empty, stop and ask for a jkb path or task uids.
- `--dry-run` → steps 2–3 only.
- `--branch <name>` → `<name>` is the integration branch (step 5).
- `--no-design-gate` → the design gate is off. Otherwise it is on.
- `--designers <n>` → `DESIGNERS=<n>`, the most tasks the run gives a designer, in all, not at once.
  Default 3; `0` designs nothing.
- The rest is either a **namespace path** (no spaces, no `#`), giving `SCOPE="ns:<path>/**"`, or
  one or more **task uids**, collected as `TASKS`.

**The design gate.** In scope mode the swarm builds only tasks whose design is approved, so
implementers never invent architecture they cannot ask you about. A task at `design` without an
approved design is not skipped: a designer writes its design, and you approve or reject it at
`design_review`. A design is approved in one of two ways:
- **In jkb.** The task's workflow phase is past design (`implement`, `review` or `landable`). It
  needs no tag, and it goes on the full workflow path.
- **By the tag (legacy).** The task is still at `design` but carries `design=approved` from
  `/jkb-design-pass`. It goes on the legacy path, routed on the reviewer's verdict, until operator
  approval in jkb is routine.

The gate is in the frontier reads, by phase (`phase:` in the query DSL): the coordinator's step 1
reads the approved tasks before any task still at `design`, so a broad scope's approved work is
never hidden past the limit behind hundreds of undesigned tasks, and step 1b (ROUTE BY PHASE) sends
each task down its path. Naming exact uids is a deliberate hand-pick, so
the tag gate does not apply to them (but see step 3). Neither the tag, a hand-pick nor
`--no-design-gate` builds a task whose design was turned down (its last workflow move a
`reject_design` or `systemic_redesign`): it always goes to a designer. On every path the swarm leaves out review
findings (tagged `finding=review`, which `jkb task review file` sets): no run builds a finding, and
a finding named by uid is refused and reported as one.

## 2. Preflight

`git rev-parse --show-toplevel` gives `REPO`, and `git rev-parse --abbrev-ref HEAD` gives `BASE`. If
the working tree is dirty, warn the user: the swarm branches off `BASE`, and uncommitted work is
not in it. Ask whether to continue.

## 3. Scout and preview

In scope mode, the coordinator's reads, as counts and lists (every read leaves out review
findings, and work an earlier run grafted whose landing was never recorded: `swarm-landing=unrecorded`). Its re-design read is empty before the run starts: it holds only tasks the run itself
designed, so the scout skips it.

```sh
jkb task next --global --json '<SCOPE> -tag:finding=review -tag:swarm-landing=unrecorded phase:implement,review,landable,systemic_review' --limit 100   # A, build
jkb task next --global --json '<SCOPE> -tag:finding=review -tag:swarm-landing=unrecorded phase:design tag:design=approved' --limit 100                  # B, legacy
jkb query --global --count 'kind:task is:ready <SCOPE> -tag:finding=review -tag:swarm-landing=unrecorded phase:design -tag:design=approved'             # C, design
jkb query --global --json 'kind:task <SCOPE> phase:design_review' --limit 1000                                            # awaiting you
```

For each task of reads A and B, `jkb workflow show <uid>` gives its phase and history. Route it as
the coordinator's step 1b will: `implement`, `review` and `landable` are built; `systemic_review`
goes to a systemic reviewer; `design` goes to a designer if its last workflow move turned its design
down (`reject_design` or `systemic_redesign`), and is otherwise built (tagged). Read C's tasks go to
designers with the gate on, as many as the budget allows, or are built with it off. In uid mode,
read each named task instead (`jkb task show <uid>`, `jkb workflow show <uid>`): a finding is
refused, and the phase routes the rest the same way.

Print:
- how many ready tasks are admitted on the workflow path, and how many on the legacy path;
- the designer budget (`DESIGNERS`), how many tasks at `design` the gate holds back (read C's
  count), and so how many of them this run will design and how many it will leave undesigned;
- the tasks awaiting your design approval (`design_review`), one `<uid> — <title>` line each:
  approving them before the run lets it build them;
- for named uids, any still at `design`, which go on the legacy path unless their design was
  turned down (then they go to a designer), and any review finding, which is refused;
- the tasks at `systemic_review`, each of which gets a systemic reviewer;
- a rough agent count: one designer per task designed, up to the budget, one systemic reviewer per
  task at `systemic_review`, plus about two per group per attempt (an implementer and a reviewer),
  with up to 3 attempts per group, and a systemic reviewer for a group whose review keeps failing in
  one area.

With `--dry-run`, stop here.

## 4. Confirm

Ask the user to confirm before starting anything. Continue only on a clear yes.

## 5. The integration branch and the run owner

The integration branch is an ordinary feature branch with no swarm artifact in its name. It
defaults to `fleet/<BASE>`.

```sh
INTEG="${BRANCH:-fleet/$BASE}"
git show-ref --verify --quiet "refs/heads/$INTEG" || git branch "$INTEG" "$BASE"
mkdir -p .swarm && git worktree add .swarm/integration "$INTEG" 2>/dev/null || true
```

Claims are owned by a live process, so a crashed run's claims can be told from this one's. Start a
keepalive with the Bash tool's `run_in_background: true`, and use its pid:

```sh
sleep 86400 & echo "OWNER=$(hostname):$!"
```

## 6. Become the coordinator

Fill the coordinator template and follow it as your instructions for the rest of this session:

```sh
jkb workflow agent show swarm-coordinator \
  --var repo="$REPO" --var integration="$INTEG" --var integration_worktree="$REPO/.swarm/integration" \
  --var owner="$OWNER" --var group_cap=4 --var retry_cap=3 --var designers="${DESIGNERS:-3}" \
  --var scope_block="<see below>"
```

`scope_block` says how to read the frontier:
- **scope mode:** "Your scope is `<SCOPE>`. Make each read with `jkb task next --global --json
  '<SCOPE> <the read's terms>' --limit <the read's limit>`. Only tasks it returns are yours.
  The design gate is ON: step 1b builds a task at `design` only if it is tagged `design=approved`
  (read B), and starts a designer for the others (read C), as many as the budget allows." With
  `--no-design-gate`, say instead "The design gate is OFF: step 1b builds a task at `design` on the
  legacy path, unless its design was turned down."
- **uid mode:** "Your tasks are exactly: <uids>. Instead of a read, check each one: it is ready when
  `jkb task show <uid> --json` shows it open, unblocked and unclaimed, and it belongs to the read
  whose terms its tags and its phase (`jkb workflow show <uid> --json`) match. One tagged
  `finding=review` is a review finding: no run builds it, so leave it and report it as a finding
  you named. They are hand-picked: step 1b builds one at `design` on the legacy path, unless its
  design was turned down."

## 7. When it is done

Stop the keepalive (`kill <its pid>`) and relay the coordinator's report, its tasks awaiting your
design approval first: approve each with `jkb workflow fire <uid> approve_design` (or reject it
with `reject_design --reason <notes>`, and a designer answers the notes). An approval or rejection
given while the run is going is picked up by its next frontier. Then tell the user how to finish:
- **Check the result.** `git -C .swarm/integration log --oneline "$BASE".."$INTEG"`, and run the
  tests there.
- **Merge it.** Merge or open a PR from `$INTEG` as with any feature branch.
- **Clean up.** `git worktree remove .swarm/integration`, then delete the local `swarm-task/*`
  branches. Never push them.
- **Leftover claims.** If a run crashed and left claims, `jkb doctor --fix` clears them.
