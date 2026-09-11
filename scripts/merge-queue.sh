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

# THE CALLER'S REPOSITORY SELECTION, DROPPED ONCE, BEFORE ANY GIT RUNS. This script is spawned by
# the swarm as `cd $INTEGRATION_WT && ./scripts/merge-queue.sh …`, inheriting the developer's
# environment whole, and every git call below is BARE — no `-C`, no `--git-dir`. An exported
# `GIT_WORK_TREE` outranks the working directory, so line 36's `git switch` checks the base branch
# out over whatever that variable names, and the two `git reset --hard "$PRE"` calls then force
# that tree to a commit.
#
# Measured on git 2.51.1, from inside a real repository with `GIT_WORK_TREE=<victim>` exported:
# `git switch feature` wrote the repository's tracked files INTO <victim>, and `git reset --hard`
# replaced a file there whose name collided with a tracked one — "MY UNSAVED WORK" became the
# repository's content, silently. That is somebody's home directory under the
# `export GIT_WORK_TREE=$HOME` dotfiles recipe this whole cluster (D46) exists for.
#
# ONCE, AT THE TOP, rather than `-C` on each of nine call sites: a rule every call site has to
# remember is the defect this repository keeps rediscovering, and a tenth git call added below
# would not have to remember this one. The six names are `gitrepo::REPO_SELECTION_VARS` — which
# repository, and which parts of one — kept in step with it by
# `git-hooks.test.sh`'s bare-git scan.
unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR \
      GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES

# Every `scripts/tests/*.test.sh`, derived rather than listed: a suite added to that directory and
# not to a hand-written list here would be a guard the queue silently does not run.
_shell_suites_pass() {
    local t rc=0
    for t in ./scripts/tests/*.test.sh; do
        [ -f "$t" ] || continue
        bash "$t" || rc=1
    done
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
#
# CHECKED. Step 1 detached HEAD to rebase, releasing $BASE for the whole of it, so the conclusion
# reached at line 57 — "this worktree holds $BASE" — is stale by the time we get here. If another
# worktree claimed the branch meanwhile this switch fails, HEAD stays detached AT $GRAFT, and the
# fast-forward below then trivially succeeds because it is already there: the script would print
# `landed:` and record the landing with jkb while $BASE never moved a commit.
if ! git switch "$BASE" >/tmp/merge-queue.log 2>&1; then
  echo "eject: cannot switch back to $BASE — another worktree holds it (see /tmp/merge-queue.log)"
  exit 1
fi
# HOOKS OFF FOR THIS MERGE. A fast-forward fires `post-merge`, and in this repository that hook
# runs setup.sh, which `cargo install`s the jkb binary, rebuilds the VS Code extension and
# reinstalls the watcher service. At this point in the script the gate has NOT run — it is step 3,
# below — so every graft was installing a binary built from a candidate that might fail the gate
# seconds later and be rolled back by step 4, leaving the operator's `jkb` newer than the branch
# they are on. Measured: the hook fires on `merge --ff-only`, and `-c core.hooksPath=/dev/null`
# suppresses it while the merge still happens.
if ! git -c core.hooksPath=/dev/null merge --ff-only "$GRAFT" >/tmp/merge-queue.log 2>&1; then
  git reset --hard "$PRE" >/dev/null 2>&1
  echo "eject: fast-forward failed"
  exit 1
fi

# 3. Run the gate on the integrated result.
# THE SHELL SUITES ARE PART OF THE GATE. It was `build.sh && test.sh` — cargo only — so nothing
# under `scripts/tests/` could block a landing, which is most of what this branch spent its rounds
# building: the quiet-grep refusal, the unscrubbed-git scan, the isolation oracles. A guard that
# cannot fail a landing is a guard the merge queue does not have.
#
# NOT `check.sh`, and the reason is stated rather than left as an omission: that gate also runs
# `clippy --all-features` and `cargo deny`, both of which fetch crates, and the swarm runs behind
# an egress firewall where they cannot complete — so making it the queue's gate would eject every
# candidate for a fact about the network. Those two stay CI's job. The suites added here need no
# network and take seconds.
start=$(date +%s)
if ./scripts/build.sh >/tmp/merge-queue-build.log 2>&1 \
   && ./scripts/test.sh >/tmp/merge-queue-test.log 2>&1 \
   && _shell_suites_pass >/tmp/merge-queue-shell.log 2>&1; then
  # Record that jkb itself grafted this branch (design D48), which is what closes the group's
  # tasks and what lets a later `jkb task review record` of $BASE credit them: a landing is an
  # EVENT jkb wrote, not something a reader infers from the commit graph.
  #
  # A failure here is reported and never fails the queue -- the commits ARE in $BASE either way,
  # and the repair is one command. stdout is noise; stderr is not: the verb names any task it
  # could not close (open subtasks, most often), and swallowing that would leave the caller
  # believing a whole group was done.
  "$JKB" task landed "$BRANCH" --onto "$BASE" >/dev/null \
    || echo "note: could not record the landing of $BRANCH (the graft itself is done)"
  echo "landed: $BRANCH → $BASE in $(( $(date +%s) - start ))s"
  exit 0
fi

# 4. Red gate → roll the base back to its pre-graft tip and eject.
git reset --hard "$PRE" >/dev/null 2>&1
echo "eject: gate failed after $(( $(date +%s) - start ))s (see /tmp/merge-queue-*.log)"
exit 2
