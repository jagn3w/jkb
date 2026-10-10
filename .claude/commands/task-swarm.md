---
description: Make this session the COORDINATOR of a task swarm. It groups overlapping ready jkb tasks, starts one jkb-implementer per group and a fresh jkb-reviewer per pass, and runs the deterministic merge queue (no agent), which lands approved branches on one feature branch and records each landing. Claim-guarded, and it keeps going as dependents unblock.
argument-hint: "<jkb-path | task-uids...>  [--branch <name>]  [--dry-run]  [--no-design-gate]"
---

This session becomes the **coordinator** of a task swarm. It works inside the tasks' lifecycle
and RBAC: jkb holds every rule the coordinator follows. The swarm has four parts:

- **Groups.** The coordinator reads the ready frontier and groups overlapping tasks, at most 4
  per group.
- **Implementer.** Each group gets one `jkb-implementer` subagent, which builds every task in the
  group on one branch. It stays with the group through every review and fix.
- **Reviewer.** Each pass gets a fresh `jkb-reviewer` subagent, which checks the branch against
  the whole group.
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
- `--no-design-gate` → `GATE=""`. Otherwise `GATE="tag:design=approved"`.
- The rest is either a **namespace path** (no spaces, no `#`), giving `SCOPE="ns:<path>/**"`, or
  one or more **task uids**, collected as `TASKS`.

**The design gate.** In scope mode the swarm touches only tasks tagged `design=approved` by
`/jkb-design-pass`, so implementers never invent architecture they cannot ask you about. Naming
exact uids is a deliberate hand-pick, so the gate does not apply to them (but see step 3).

## 2. Preflight

`git rev-parse --show-toplevel` gives `REPO`, and `git rev-parse --abbrev-ref HEAD` gives `BASE`. If
the working tree is dirty, warn the user: the swarm branches off `BASE`, and uncommitted work is
not in it. Ask whether to continue.

## 3. Scout and preview

```sh
jkb task next --global --json '<SCOPE> <GATE>' --limit 100        # ready and approved
jkb query    --global --json 'kind:task <SCOPE> <GATE>' --limit 1000
jkb task next --global --json '<SCOPE>' --limit 100               # ready, ungated
```

Print:
- the ready tasks, and how many are not yet terminal;
- how many ready tasks the gate holds back (if many, suggest `/jkb-design-pass <path>` first);
- for named uids, any that lack `design=approved`;
- a rough agent count: about two per group per attempt (an implementer and a reviewer), with up to
  3 attempts per group.

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
- **scope mode:** "Read the frontier with `jkb task next --global --json '<SCOPE> <GATE>' --limit 100`.
  Only tasks it returns are yours."
- **uid mode:** "Your tasks are exactly: <uids>. A task is ready when `jkb task show <uid> --json`
  shows it open, unblocked and unclaimed."

## 7. When it is done

Stop the keepalive (`kill <its pid>`) and relay the coordinator's report. Then tell the user how
to finish:
- **Check the result.** `git -C .swarm/integration log --oneline "$BASE".."$INTEG"`, and run the
  tests there.
- **Merge it.** Merge or open a PR from `$INTEG` as with any feature branch.
- **Clean up.** `git worktree remove .swarm/integration`, then delete the local `swarm-task/*`
  branches. Never push them.
- **Leftover claims.** If a run crashed and left claims, `jkb doctor --fix` clears them.
