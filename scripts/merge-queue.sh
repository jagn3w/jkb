#!/usr/bin/env bash
# Deterministic merge-queue step (design D27.6): integrate ONE reviewer-approved branch
# into the base feature branch by rebase + fast-forward (linear history, no merge commit,
# no branch-name artifact), then run the gate. NO agent, NO reasoning — a merge train.
#
#   ./scripts/merge-queue.sh <branch> <base> <worktree>
#
# Run inside the integration worktree (checked out to <base>). Exit codes:
#   0  landed  — <branch> rebased onto the live <base> tip, gate green, <base> fast-forwarded.
#   1  eject   — rebase conflict (hand back to the implementer to rebase-and-fix).
#   2  eject   — gate failed on the integrated result; <base> never moved.
#   3  error   — setup problem (bad worktree/branch, or nothing to graft); nothing changed.
#   4  error   — the graft passed but <base> could not be advanced onto it (another worktree
#               holds the branch, or the fast-forward was refused). NOT the implementer's
#               problem, which is why it is not 1: the swarm hands 1 back as "rebase and fix
#               your branch", and there is nothing wrong with the branch.
#
# THE GATE RUNS BEFORE <base> MOVES, which is why 2 no longer says "reset to pre-graft": there is
# nothing to reset. It used to fast-forward first and roll back on red, and the window between
# those was minutes — long enough for a concurrent implementer, told to cut from the integration
# branch, to branch from commits that had passed nothing and carry them back in under its own
# name.
#
# Because the gate runs against the LIVE base tip, a branch green in isolation can still
# fail once an earlier queue entry landed — exactly the semantic/textual conflict the
# serial one-at-a-time queue exists to catch. The fix is the implementer rebasing on the
# new base, never a merger reconciling blind.
set -uo pipefail

# THE CALLER'S REPOSITORY SELECTION, DROPPED ONCE, BEFORE ANY GIT RUNS. This script is spawned by
# the swarm as `cd $INTEGRATION_WT && ./scripts/merge-queue.sh …`, inheriting the developer's
# environment whole, and every git call below is BARE — no `-C`, no `--git-dir`. An exported
# `GIT_WORK_TREE` outranks the working directory, so the `git switch` below checks the base branch
# out over whatever that variable names — and when this file still rolled the base back, the two
# `git reset --hard "$PRE"` calls it carried then forced that tree to a commit.
#
# Measured on git 2.51.1, from inside a real repository with `GIT_WORK_TREE=<victim>` exported:
# `git switch feature` wrote the repository's tracked files INTO <victim>, and `git reset --hard`
# replaced a file there whose name collided with a tracked one — "MY UNSAVED WORK" became the
# repository's content, silently. That is somebody's home directory under the
# `export GIT_WORK_TREE=$HOME` dotfiles recipe this whole cluster (D46) exists for.
#
# ONCE, AT THE TOP, rather than `-C` on each of the fourteen call sites below (counted, not
# estimated — it read "nine" for a round): a rule every call site has to remember is the defect
# this repository keeps rediscovering, and a fifteenth git call added below would not have to
# remember this one. The six names are `gitrepo::REPO_SELECTION_VARS` — which
# repository, and which parts of one — kept in step with it by
# `dev-scripts.test.sh`'s case6, which reads the six names out of `REPO_SELECTION_VARS` itself.
# (Not `git-hooks.test.sh`'s bare-git scan, which this line used to name: that one reads only
# `scripts/lib.sh` and knows nothing about variable NAMES — it checks that calls go through the
# wrapper, not what the wrapper strips.)
unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR \
      GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES

# Every `scripts/tests/*.test.sh`, derived rather than listed: a suite added to that directory and
# not to a hand-written list here would be a guard the queue silently does not run.
_shell_suites_pass() {
    local t rc=0 n=0
    for t in ./scripts/tests/*.test.sh; do
        [ -f "$t" ] || continue
        n=$((n + 1))
        bash "$t" || rc=1
    done
    # AN EMPTY GLOB IS NOT A PASS. Deriving the list from the directory means a branch that moves,
    # renames or deletes `scripts/tests/` silently restores the cargo-only gate this half exists to
    # replace — and it would do it by landing, which is the one moment nobody is watching. The
    # floor is low enough that ordinary churn does not trip it and high enough that an emptied
    # directory does.
    if [ "$n" -lt 4 ]; then
        echo "gate: only $n shell suite(s) found under scripts/tests — expected at least 4;" >&2
        echo "      the suites moved or were deleted, so this half of the gate checked nothing" >&2
        return 1
    fi
    return "$rc"
}

BRANCH="${1:?usage: merge-queue.sh <branch> <base> <worktree>}"
BASE="${2:?missing <base>}"
WT="${3:?missing <worktree>}"

# WHICH jkb, AND WHICH STORE. This script writes to the knowledge base (step 3 records the
# landing), and a swarm run can be configured with its own binary (`cfg.jkb`) and its own database
# (`cfg.db`). A bare `jkb` resolves neither: the landing event would be written into the user's
# PRODUCTION store under a `(repo, branch)` key nothing there asks about, while every task in the
# run's own database silently got none.
#
# Taken from the environment rather than as a fourth positional argument, so the caller sets it
# once for the whole invocation and `jkb`'s own `$JKB_DB` fallback does the rest -- there is no
# `--db` to thread through, and no second place for a future jkb call in this script to forget.
: "${JKB:=jkb}"

cd "$WT" 2>/dev/null || { echo "error: cannot cd to worktree $WT"; exit 3; }
git switch "$BASE" >/dev/null 2>&1 || { echo "error: cannot switch to base $BASE"; exit 3; }
git rev-parse --verify "$BRANCH" >/dev/null 2>&1 || { echo "error: no such branch $BRANCH"; exit 3; }
# AND IT MUST HAVE SOMETHING TO GRAFT. A branch sitting at the base tip rebases to a no-op, and
# `merge --ff-only` then answers "Already up to date." with exit 0 — so the queue printed `landed:`
# and called `jkb task landed`, which drives every task recording that branch to done with
# `landed_elsewhere: Fact::Yes`. A whole group closed, dependents unblocked, and not one commit in
# the base. Measured: base and branch at the same commit, the full step-1/step-2 sequence, exit 0,
# base tip unchanged.
#
# Reachable two ways. An IMPLEMENTER that reports `outcome: ready` without committing leaves the
# branch at the base tip and `rev-parse --verify` still succeeds, because the branch exists. And a
# branch whose commits the rebase drops as empty — the case step 1's own comment says it accepts —
# arrives here the same way, though that one has genuinely landed its content via an earlier entry
# and is reported separately below rather than refused.
if [ "$(git rev-list --count "$BASE..$BRANCH")" -eq 0 ]; then
  echo "eject: $BRANCH has no commits ahead of $BASE — nothing to graft"
  exit 3
fi

PRE=$(git rev-parse HEAD)   # the base tip before this graft, for a clean rollback

# 1. Rebase the branch's commits onto the CURRENT base tip WITHOUT moving the branch ref.
# `git rebase <base> <branch>` checks out <branch> first, which git REFUSES when <branch>
# is checked out in the implementer's worktree ("fatal: '<branch>' is already used by
# worktree at …") — a setup error the old code caught and misreported as a *content*
# conflict, ejecting every group forever. Detaching HEAD at the branch commit does not
# claim the branch ref, so it is allowed even when the branch is live elsewhere; we rebase
# that detached HEAD, then fast-forward the base to the grafted result (linear, no merge
# commit). Empty commits drop exactly as a normal rebase would.
if ! git checkout --detach "$BRANCH" >/tmp/merge-queue.log 2>&1; then
  git switch "$BASE" >/dev/null 2>&1
  echo "eject: cannot detach at $BRANCH (see /tmp/merge-queue.log)"
  exit 1
fi
if ! git rebase "$BASE" >/tmp/merge-queue.log 2>&1; then
  git rebase --abort >/dev/null 2>&1 || true
  git switch "$BASE" >/dev/null 2>&1
  echo "eject: rebase conflict onto $BASE"
  exit 1
fi
GRAFT=$(git rev-parse HEAD)   # the rebased commits (detached HEAD)

# 2. GATE THE GRAFT WHILE IT IS STILL DETACHED, before $BASE is allowed to move.
#
# The order used to be ff-then-gate, and the window between them is minutes: build.sh, test.sh and
# now the shell suites, with a container-wide CARGO_TARGET_DIR. task-swarm.js serialises only the
# merge stage, so Implement and Review run throughout it — and it tells each implementer to cut
# from the integration branch. So a group could branch from commits that had passed nothing, the
# gate could then go red, step 4 would `reset --hard "$PRE"`, and those commits would live on in
# somebody else's history and be carried back in under their name: the second group ejects for a
# defect it did not write, and if the first failure was intermittent the rejected code lands
# silently.
#
# The worktree at $GRAFT is byte-identical to what $BASE becomes, so gating here tests exactly the
# same thing and $BASE only ever advances to a commit that has already passed. That also deletes
# both `git reset --hard "$PRE"` rollbacks: there is nothing to roll back, because nothing moved.
# A rollback that never runs cannot fail silently, which is one of the two ways the old shape went
# wrong (the other was that its status was discarded).
#
# THE SHELL SUITES ARE PART OF IT. It was `build.sh && test.sh` — cargo only — so nothing under
# `scripts/tests/` could block a landing, which is most of what this branch built. NOT `check.sh`:
# that also runs `clippy --all-features` and `cargo deny`, both of which fetch crates, and the
# swarm runs behind an egress firewall where they cannot complete, so it would eject every
# candidate for a fact about the network. Those two stay CI's job.
start=$(date +%s)
if ! { ./scripts/build.sh >/tmp/merge-queue-build.log 2>&1 \
       && ./scripts/test.sh >/tmp/merge-queue-test.log 2>&1 \
       && _shell_suites_pass >/tmp/merge-queue-shell.log 2>&1; }; then
  git switch "$BASE" >/dev/null 2>&1 || true
  echo "eject: gate failed after $(( $(date +%s) - start ))s (see /tmp/merge-queue-*.log)"
  exit 2
fi

# 3. Green — put $BASE on the gated commit.
#
# CHECKED. Step 1 detached HEAD, releasing $BASE for the whole rebase and the whole gate, so the
# conclusion reached before that — "this worktree holds $BASE" — is long stale. If another
# worktree claimed the branch meanwhile this switch fails, HEAD stays detached AT $GRAFT and the
# fast-forward below trivially succeeds because it is already there: the script would print
# `landed:` and record the landing while $BASE never moved a commit.
if ! git switch "$BASE" >/tmp/merge-queue.log 2>&1; then
  echo "eject: cannot switch back to $BASE — another worktree holds it (see /tmp/merge-queue.log)"
  exit 4
fi
# HOOKS OFF. A fast-forward fires `post-merge`, which in this repository runs setup.sh —
# cargo-installing the jkb binary, rebuilding the extension, reinstalling the watcher service.
# Measured: the hook fires on `merge --ff-only`, and `-c core.hooksPath=/dev/null` suppresses it
# while the merge still happens. Even now that the gate has already passed, the queue is not the
# place to reinstall an operator's tooling mid-run.
if ! git -c core.hooksPath=/dev/null merge --ff-only "$GRAFT" >/tmp/merge-queue.log 2>&1; then
  echo "eject: fast-forward failed (see /tmp/merge-queue.log)"
  exit 4
fi
# ...AND THE BASE MUST HAVE MOVED. The entry check refuses a branch with nothing ahead; this
# catches the other arrival at the same state — every commit dropped as empty by the rebase,
# because an earlier queue entry landed the same content. Not a failure and not a graft: the work
# IS in the base, under somebody else's commit. Reported in its own words, and the tasks still
# close, because the content they asked for is there.
if [ "$(git rev-parse HEAD)" = "$PRE" ]; then
  "$JKB" task landed "$BRANCH" --onto "$BASE" >/dev/null \
    || echo "note: could not record the landing of $BRANCH"
  echo "landed: $BRANCH → $BASE (no new commits; its content was already in $BASE)"
  exit 0
fi

# 4. Record that jkb itself grafted this branch (design D48), which is what closes the group's
# tasks and what lets a later `jkb task review record` of $BASE credit them: a landing is an EVENT
# jkb wrote, not something a reader infers from the commit graph.
#
# A failure here is reported and never fails the queue -- the commits ARE in $BASE either way, and
# the repair is one command. stdout is noise; stderr is not: the verb names any task it could not
# close (open subtasks, most often), and swallowing that would leave the caller believing a whole
# group was done.
"$JKB" task landed "$BRANCH" --onto "$BASE" >/dev/null \
  || echo "note: could not record the landing of $BRANCH (the graft itself is done)"
echo "landed: $BRANCH → $BASE in $(( $(date +%s) - start ))s"
exit 0
