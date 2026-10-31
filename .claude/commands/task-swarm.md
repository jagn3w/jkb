---
description: Make this session the COORDINATOR of a task swarm. It starts a jkb-designer for each ready task still awaiting a design, groups overlapping ready jkb tasks, starts one jkb-implementer per group and a fresh jkb-reviewer per pass, and runs the deterministic merge queue (no agent), which lands approved branches on one feature branch and records each landing. Claim-guarded, and it keeps going as dependents unblock.
argument-hint: "<jkb-path | task-uids...>  [--branch <name>]  [--dry-run]  [--no-design-gate]"
---

This session becomes the **coordinator** of a task swarm. It works inside the tasks' lifecycle
and RBAC: jkb holds every rule the coordinator follows. The swarm has these parts:

- **Designer.** A task at `design` that the run's design gate holds back gets one `jkb-designer`
  subagent, which writes the task's design into it, with its open questions, and fires
  `submit_design`. The task then waits at `design_review` for your approval: it is *parked*, never
  built and never settled, so a design you approve during the run is built by a later frontier.
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
- **Workflow.** The coordinator routes a task by its workflow phase, and admits a group only if
  every task in it is admitted, and never a review finding. A task at `implement`, `review` or
  `landable` is a *workflow* task. A task still at `design` that the run's design gate let through (the `design=approved` tag, a
  hand-pick by uid, or `--no-design-gate`) is a *legacy* task: its design was never approved in
  jkb, so it is built on the legacy path, routed on the reviewer's verdict, and named as legacy in
  the report. A task at `design` that the gate holds back goes to a designer. A task at
  `design_review` is parked, and the report lists it as awaiting your design approval. A task at
  any other phase is left out and reported. A group never mixes legacy and workflow tasks. The workflow moves through
  the group's lead (its first task, recorded as `swarm-lead=<lead uid>` on every task of the
  group). For a workflow group, when the implementer reports ready the coordinator reads the lead's
  phase and fires `submit_work` from `implement`, nothing from `review` (already submitted), and
  `rework` then `submit_work` from `landable`; it stalls the group from any other phase, or on a
  refused fire. After a merge-queue eject it fires `rework`. After each review it runs `jkb workflow
  observe <lead>` and routes on the phase jkb reports, never on the reviewer's verdict word:
  `implement` goes back to the implementer, `systemic_review` stops the group for a systemic
  review, and `landable` goes to the merge queue.
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

The query DSL cannot test a workflow phase, so the gate is applied by the coordinator's step 1b
(ROUTE BY PHASE), not by the frontier query. Naming exact uids is a deliberate hand-pick, so
the tag gate does not apply to them (but see step 3). On every path the swarm leaves out review
findings (tasks under `repos/<repo>/codereviews/`): no run builds a finding.

## 2. Preflight

`git rev-parse --show-toplevel` gives `REPO`, and `git rev-parse --abbrev-ref HEAD` gives `BASE`. If
the working tree is dirty, warn the user: the swarm branches off `BASE`, and uncommitted work is
not in it. Ask whether to continue.

## 3. Scout and preview

```sh
jkb task next --global --json '<SCOPE>' --limit 100                         # ready
jkb task next --global --json '<SCOPE> tag:design=approved' --limit 100     # ready and tagged
jkb query    --global --json 'kind:task <SCOPE>' --limit 1000
```

For each ready task, `jkb workflow show <uid>` gives its phase, and `jkb task show <uid>` its
namespace. Route it as the coordinator's step 1b will: a finding is left out; `implement`, `review`
and `landable` are built; `design` is built when it is tagged, hand-picked, or the gate is off, and
otherwise goes to a designer; `design_review` is parked; everything else is left out.

Print:
- the ready tasks, and how many are not yet terminal;
- how many ready tasks are admitted on the workflow path, and how many on the legacy path;
- how many the gate holds back, each of which gets a designer, and how many are findings or at
  another phase;
- the parked tasks, awaiting your design approval (`design_review`), one `<uid> — <title>` line
  each: approving them before the run lets it build them;
- for named uids, any still at `design`, which go on the legacy path;
- a rough agent count: one designer per task the gate holds back, plus about two per group per
  attempt (an implementer and a reviewer), with up to 3 attempts per group.

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
  --var owner="$OWNER" --var group_cap=4 --var retry_cap=3 \
  --var scope_block="<see below>"
```

`scope_block` says how to read the frontier:
- **scope mode:** "Read the frontier with `jkb task next --global --json '<SCOPE>' --limit 100`.
  Only tasks it returns are yours. The design gate is ON: step 1b builds a task at `design` only if
  it is tagged `design=approved`, and starts a designer for every other task at `design`." With
  `--no-design-gate`, say instead "The design gate is OFF: step 1b builds a task at `design` on the
  legacy path."
- **uid mode:** "Your tasks are exactly: <uids>. A task is ready when `jkb task show <uid> --json`
  shows it open, unblocked and unclaimed. They are hand-picked: step 1b builds one at `design` on
  the legacy path."

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
