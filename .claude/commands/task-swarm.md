---
description: SCHEDULER groups overlapping ready jkb tasks; one IMPLEMENTER builds each group; a fresh REVIEWER checks it; a deterministic merge queue (no agent) rebase/fast-forwards approved branches into one feature branch and marks the group done. Pipelined, claim-guarded, looping as dependents unblock.
argument-hint: "<jkb-path | task-uids...>  [--branch <name>]  [--dry-run]  (prefix the message with +<N>k to cap token spend)"
---

You are the **COORDINATOR** of a task swarm. Given a jkb path (a namespace with
subtasks) or an explicit list of task uids, you drive a workflow whose roles are (design
D27):

- **SCHEDULER** — reads the ready frontier and clusters *overlapping* ready tasks into
  small **work-groups** (≤~4 tasks; non-overlapping tasks stay singletons).
- **IMPLEMENTER** — one per group, builds **all** the group's tasks on one clean branch,
  and stays with the group through review/fix.
- **REVIEWER** — a **fresh** reviewer per pass checks the branch against the **whole
  group**; `approve` → merge queue, `request_changes` → back to the same implementer.
- **merge queue** — *deterministic, no agent* (`scripts/merge-queue.sh`): rebase +
  fast-forward each approved branch onto the feature branch, run the gate, and on green
  mark the group **`done`**; on conflict/red **eject** back to the implementer to rebase.

The pipeline runs Implement → Review → merge **without a per-round barrier**; the merge
queue is the one serial stage; newly-ready groups feed in as dependents unblock.

Argument: `$ARGUMENTS`

This command *deliberately* opts into multi-agent orchestration: it launches the
`task-swarm` **Workflow**, which spawns many sub-agents and can be expensive. Follow the
steps in order and **do not spawn anything until after the cost preview and confirmation
(steps 3–4).** Use `jkb` on PATH (fall back to `./target/debug/jkb`).

## 1. Parse the argument

- If `$ARGUMENTS` is empty, stop and ask for a jkb path or task uids.
- `--dry-run` present → do steps 2–3 only (preview, spawn nothing).
- `--branch <name>` present → use `<name>` as the integration/feature branch (see step 5).
- Otherwise interpret the non-flag argument as either:
  - a **namespace/path** (no spaces, no `#`) → scope `SCOPE="ns:<path>/**"`, or
  - one or more **task uids** (contain `:` or `#`, or a space-separated list) → collect them as `TASKS`.

## 2. Preflight

- Confirm you're in a git repo: `git rev-parse --show-toplevel`. Record `REPO` (abs path)
  and `BASE=$(git rev-parse --abbrev-ref HEAD)`.
- If the working tree is dirty (`git status --porcelain` non-empty), warn the user — the
  swarm branches off `BASE`; uncommitted changes won't be included and could confuse
  merges. Suggest committing/stashing, and ask whether to continue.

## 3. Scout + cost preview (the guardrail)

Fetch the frontier without changing anything:

```sh
jkb task next --global --json '<SCOPE>' --limit 100      # ready (unblocked, unclaimed) tasks
jkb query    --global --json 'kind:task <SCOPE>' --limit 1000   # all tasks in scope
```

(For a uid list, filter these to the given uids.) From the results, print:
- the **ready** task count and titles, and the **total non-terminal** count (status not
  `done`/`cancelled`) — the latter is how many the swarm will eventually work as deps clear;
- a rough **agent estimate**: the SCHEDULER groups tasks (≤~4/group), so roughly
  `~ groups × (implement + review + a few mechanical claim/status/merge steps) + 1 scheduler/pass`,
  noting retries (up to 3/group) and re-reviews can raise it;
- the **token budget**: if the user prefixed their message with a `+<N>k`/`+<N>` budget
  directive, state it as the hard ceiling; otherwise note there is **no cap** and they can
  re-run with one (e.g. start the message with `+500k`).

If `--dry-run`, stop here.

## 4. Confirm

Show the preview and **ask the user to confirm** before spawning (e.g. "This will run a
swarm over N ready / M total tasks, ~K agents, budget <…>. Proceed?"). Only continue on a
clear yes.

## 5. Set up the integration branch + worktree

The integration/feature branch is an **ordinary feature branch** — **no** `swarm/` prefix,
so the PR you eventually open and its branch name carry **no swarm artifact** (status,
task ids, and the fact a swarm ran are all KB-local, D27.7). Default its name to
`fleet/<BASE>` (or take `--branch <name>`); pick anything that reads like a normal branch.

```sh
INTEG="${BRANCH:-fleet/$BASE}"          # a normal feature-branch name (override with --branch)
git show-ref --verify --quiet "refs/heads/$INTEG" || git branch "$INTEG" "$BASE"
mkdir -p .swarm
git worktree add .swarm/integration "$INTEG" 2>/dev/null || true   # reuse if present
```

`.swarm/` is git-ignored. The merge queue does all its rebase/fast-forwards inside
`.swarm/integration` (checked out to `$INTEG`), so your `BASE` checkout is never touched.
Ephemeral per-group `swarm-task/*` branches are **local-only** — they never enter history
(the merge queue rebase/fast-forwards, so no merge commits or branch-name artifacts) and
**must not be pushed**; prune them after landing.

### Run owner + the 60s reclaimer sidecar (the claim liveness authority)

The periodic owner-existence reclaim (D27.1/D27.6.6b) is a **true wall-clock timer**, which
the workflow JS engine can't provide (no clock, no background timer). So run it **here**, as
a detached sidecar the command owns for the swarm's lifetime — this is the coordinator's
liveness authority for *crashed prior runs*, independent of the workflow's scheduling cadence:

```sh
HOST="$(hostname)"
RUN_OWNER_FILE="$(mktemp)"                 # the sidecar reads the run owner from here
# Sidecar: every ~60s clear claims whose owner PROCESS is gone (crashed prior runs),
# always --keep-ing THIS run's owner so it never touches our own in-flight claims.
( while :; do sleep 60; jkb task reclaim --keep "$(cat "$RUN_OWNER_FILE")" >/dev/null 2>&1; done ) &
RECLAIMER_PID=$!                           # a real, live pid for the whole run
OWNER="$HOST:$RECLAIMER_PID"               # host:pid — liveness-checkable via `kill -0`
printf '%s' "$OWNER" > "$RUN_OWNER_FILE"
```

Launch this with the Bash tool's **`run_in_background: true`** so it detaches and keeps
running across turns. `OWNER = host:<sidecar pid>` is a genuinely alive process for the run,
so an external `jkb doctor --fix` never mistakes this live run for a crashed one. Pass `OWNER`
as `owner` below; the workflow's implementers claim with it and the sidecar keeps it.

## 6. Launch the workflow

Locate the installed workflow script (first that exists): `"$CLAUDE_CONFIG_DIR/workflows/task-swarm.js"`,
`"$HOME/.claude/workflows/task-swarm.js"`, or `./.claude/workflows/task-swarm.js`.

Call the **Workflow** tool with `scriptPath` = that path (or `name: "task-swarm"` if your
setup resolves saved workflows), and `args`.

> **Gotcha:** pass `args` as an **actual JSON object**, not a JSON-encoded string. The
> script does `const cfg = args || {}` and reads `cfg.integration` etc.; a stringified
> value makes `cfg` a string, so every field is `undefined` and the script throws
> `task-swarm requires args.integration and args.integrationWorktree` on launch.

```json
{
  "jkb": "jkb",
  "db": null,
  "scope": "<SCOPE or empty when using tasks>",
  "tasks": ["<uid>", "..."],
  "global": true,
  "repo": "<REPO abs path>",
  "integration": "<INTEG>",
  "integrationWorktree": "<REPO>/.swarm/integration",
  "owner": "<OWNER, e.g. host:pid>",
  "retryCap": 3,
  "roundCap": 40,
  "groupCap": 4
}
```

Include only `scope` **or** `tasks` (whichever you resolved; omit the other). If the user
set a token budget, the workflow honors it (it stops feeding new groups when the remaining
budget is low, then drains in-flight work). The workflow runs in the background and
notifies on completion.

## 7. Report + hand-off

**Stop the reclaimer sidecar first** — it's no longer needed once the swarm is done:

```sh
kill "$RECLAIMER_PID" 2>/dev/null; rm -f "$RUN_OWNER_FILE"
```

When the workflow finishes, relay its result: which task uids **completed** (landed on the
feature branch and marked `done`), which it **gave up** on (retry-capped), how many groups
and passes it ran, and the merge-queue **landed/eject** counts. Each completed task was
marked **`done`** in jkb (file-backed tasks got a `- [x]` + sync; managed tasks via
`jkb task set … --status done`) once its group's branch landed — which also unblocked its
dependents. Nothing about the swarm reached git: commits are ordinary professional
messages, history is linear, and no `swarm-task/*` branch entered it.

Then tell the user how to finish:

- Review the integrated result: `git -C .swarm/integration log --oneline "$BASE".."$INTEG"`
  and run the test suite there. Completed tasks are already `done`
  (`jkb query --global 'kind:task status:done'`); revert any you reject.
- When satisfied, merge/PR the feature branch normally: `git switch "$BASE" && git merge "$INTEG"`
  (or open a PR from `$INTEG`). It looks like any other feature branch.
- Clean up: `git worktree remove .swarm/integration`, `git worktree prune`, and delete the
  local `swarm-task/*` branches (`git branch -D`); do **not** push them.
- If a prior run crashed and left claims, `jkb doctor` reports orphaned claims and
  `jkb doctor --fix` clears them (owner-existence reclaim, D27.2).
