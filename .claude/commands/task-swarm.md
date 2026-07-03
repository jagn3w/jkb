---
description: Coordinator/implementer/resolver swarm — implement every unblocked jkb task under a path in parallel git worktrees, integrated by one serialized resolver
argument-hint: "<jkb-path | task-uids...>  [--dry-run]  (prefix the message with +<N>k to cap token spend)"
---

You are the **COORDINATOR** of a task swarm. Given a jkb path (a namespace with
subtasks) or an explicit list of task uids, you drive a workflow that: spawns one
**IMPLEMENTER** per *unblocked* task in an isolated git worktree, funnels each finished
task through a **single serialized RESOLVER** that integrates it into one branch (re-
dispatching to an IMPLEMENTER or DESIGNER on conflict), and loops — as completed tasks
unblock their dependents — until the frontier drains.

Argument: `$ARGUMENTS`

This command *deliberately* opts into multi-agent orchestration: it launches the
`task-swarm` **Workflow**, which spawns many sub-agents and can be expensive. Follow the
steps in order and **do not spawn anything until after the cost preview and confirmation
(steps 3–4).** Use `jkb` on PATH (fall back to `./target/debug/jkb`).

## 1. Parse the argument

- If `$ARGUMENTS` is empty, stop and ask for a jkb path or task uids.
- `--dry-run` present → do steps 2–3 only (preview, spawn nothing).
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
jkb task next --global --json '<SCOPE>' --limit 100      # ready (unblocked) tasks
jkb query    --global --json 'kind:task <SCOPE>' --limit 1000   # all tasks in scope
```

(For a uid list, filter these to the given uids.) From the results, print:
- the **ready** task count and titles, and the **total non-terminal** count (status not
  `done`/`cancelled`) — the latter is how many the swarm will eventually work as deps clear;
- a rough **agent estimate**: `~ ready × 2 (implement + resolve) + 1 scheduler/round`,
  noting retries (up to 3/task) can raise it;
- the **token budget**: if the user prefixed their message with a `+<N>k`/`+<N>` budget
  directive, state it as the hard ceiling; otherwise note there is **no cap** and they can
  re-run with one (e.g. start the message with `+500k`).

If `--dry-run`, stop here.

## 4. Confirm

Show the preview and **ask the user to confirm** before spawning (e.g. "This will run a
swarm over N ready / M total tasks, ~K agents, budget <…>. Proceed?"). Only continue on a
clear yes.

## 5. Set up the integration branch + worktree

```sh
INTEG="swarm/$BASE"
git show-ref --verify --quiet "refs/heads/$INTEG" || git branch "$INTEG" "$BASE"
mkdir -p .swarm
git worktree add .swarm/integration "$INTEG" 2>/dev/null || true   # reuse if present
```

`.swarm/` is git-ignored. The resolver does all its merges inside `.swarm/integration`
(checked out to `swarm/<BASE>`), so your `BASE` checkout is never touched.

## 6. Launch the workflow

Locate the installed workflow script (first that exists): `"$CLAUDE_CONFIG_DIR/workflows/task-swarm.js"`,
`"$HOME/.claude/workflows/task-swarm.js"`, or `./.claude/workflows/task-swarm.js`.

Call the **Workflow** tool with `scriptPath` = that path (or `name: "task-swarm"` if your
setup resolves saved workflows), and `args`:

```json
{
  "jkb": "jkb",
  "db": null,
  "scope": "<SCOPE or empty when using tasks>",
  "tasks": ["<uid>", "..."],
  "global": true,
  "repo": "<REPO abs path>",
  "integration": "swarm/<BASE>",
  "integrationWorktree": "<REPO>/.swarm/integration",
  "retryCap": 3,
  "roundCap": 25
}
```

Include only `scope` **or** `tasks` (whichever you resolved; omit the other). If the user
set a token budget, the workflow honors it (it stops a round early when the remaining
budget is low). The workflow runs in the background and notifies on completion.

## 7. Report + hand-off

When the workflow finishes, relay its result: which task uids **merged**, which it
**gave up** on (retry-capped), and how many rounds it ran. Then tell the user how to
finish:

- Review the integrated result: `git -C .swarm/integration log --oneline "$BASE".."swarm/$BASE"`
  and run the test suite there.
- When satisfied, merge it into your branch: `git switch "$BASE" && git merge "swarm/$BASE"`.
- Clean up: `git worktree remove .swarm/integration` and prune the per-task worktrees/branches
  (`git worktree prune`; `git branch -D` the merged `swarm-task/*` branches).

Note any tasks the swarm marked done in jkb (file-backed tasks had their checkboxes flipped
and were synced) versus managed tasks that need `task_update` via MCP to close.
