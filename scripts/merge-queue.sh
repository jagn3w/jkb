#!/usr/bin/env bash
# Deterministic merge-queue step (design D27.6): integrate ONE reviewer-approved branch
# into the base feature branch by rebase + fast-forward (linear history, no merge commit,
# no branch-name artifact), then run the gate. NO agent, NO reasoning — a merge train.
#
#   ./scripts/merge-queue.sh <branch> <base> <worktree>
#
# Run inside the integration worktree (checked out to <base>). Exit codes:
#   0  landed  — <branch> rebased onto the live <base> tip, fast-forwarded, gate green.
#   1  eject   — rebase conflict (hand back to the implementer to rebase-and-fix).
#   2  eject   — gate (build/test) failed on the integrated result; base reset to pre-graft.
#   3  error   — setup problem (bad worktree/branch); nothing changed.
#
# Because the gate runs against the LIVE base tip, a branch green in isolation can still
# fail once an earlier queue entry landed — exactly the semantic/textual conflict the
# serial one-at-a-time queue exists to catch. The fix is the implementer rebasing on the
# new base, never a merger reconciling blind.
set -uo pipefail

BRANCH="${1:?usage: merge-queue.sh <branch> <base> <worktree>}"
BASE="${2:?missing <base>}"
WT="${3:?missing <worktree>}"

cd "$WT" 2>/dev/null || { echo "error: cannot cd to worktree $WT"; exit 3; }
git switch "$BASE" >/dev/null 2>&1 || { echo "error: cannot switch to base $BASE"; exit 3; }
git rev-parse --verify "$BRANCH" >/dev/null 2>&1 || { echo "error: no such branch $BRANCH"; exit 3; }

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

# 2. Fast-forward the base to the rebased result — linear graft, no merge commit.
git switch "$BASE" >/dev/null 2>&1
if ! git merge --ff-only "$GRAFT" >/tmp/merge-queue.log 2>&1; then
  git reset --hard "$PRE" >/dev/null 2>&1
  echo "eject: fast-forward failed"
  exit 1
fi

# 3. Run the gate on the integrated result.
start=$(date +%s)
if ./scripts/build.sh >/tmp/merge-queue-build.log 2>&1 \
   && ./scripts/test.sh >/tmp/merge-queue-test.log 2>&1; then
  # Record that jkb itself grafted this branch (design B4), so `close-merged` asks one question
  # per BATCH instead of one per task -- and asks it about the branch whose cut point is provable.
  # The verb refuses unless the work really is in $BASE, so this cannot claim a landing that did
  # not happen; a failure here is reported and never fails the queue, since the commits ARE in
  # $BASE either way and the ref-based inference still covers the task.
  jkb task landed "$BRANCH" --onto "$BASE" >/dev/null 2>&1 \
    || echo "note: could not record the landing of $BRANCH (the graft itself is done)"
  echo "landed: $BRANCH → $BASE in $(( $(date +%s) - start ))s"
  exit 0
fi

# 4. Red gate → roll the base back to its pre-graft tip and eject.
git reset --hard "$PRE" >/dev/null 2>&1
echo "eject: gate failed after $(( $(date +%s) - start ))s (see /tmp/merge-queue-*.log)"
exit 2
