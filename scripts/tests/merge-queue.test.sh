#!/usr/bin/env bash
# Behavioural regression test for scripts/merge-queue.sh — the swarm's landing step.
#
# WHY THIS FILE EXISTS. Every behaviour below was verified by hand, in scratch repositories, at
# the moment it was written — and none of it was written down. A review then observed that both
# behavioural fixes in that commit could be DELETED with every committed suite still green:
# nothing anywhere asserted the script's exit codes or whether the base ref actually moved. A
# verification you ran once and did not commit is a verification the next person does not have.
#
# The two questions asked of every case are the two the swarm acts on: WHAT CODE did it exit, and
# DID $BASE MOVE. Those together are the whole contract — `.claude/workflows/task-swarm.js` reads
# the code to decide whether a task group is marked done, and the base moving is what "done" is
# supposed to mean. `dev-scripts.test.sh`'s case10 holds the other half of that seam, that every
# code the header documents is one the workflow classifies.
#
# The gate is stubbed to exit codes we choose, because this suite is about the QUEUE, not about
# cargo: `build.sh`, `test.sh` and the planted suites read BUILD_RC/TEST_RC/SUITE_RC. `jkb` is
# stubbed too — the real one would write a landing into a live knowledge base.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

new_workdir
isolate_git "$work"

# A repository with the queue in it, a stubbed gate, and a stubbed jkb on PATH.
mkrepo() {
    local r="$work/$1" i
    rm -rf "$r"; mkdir -p "$r/scripts/tests"
    git init -q "$r"
    git -C "$r" config user.email t@t; git -C "$r" config user.name t
    cp "$repo_root/scripts/merge-queue.sh" "$r/scripts/"
    printf '#!/bin/sh\nexit ${BUILD_RC:-0}\n' >"$r/scripts/build.sh"
    printf '#!/bin/sh\nexit ${TEST_RC:-0}\n'  >"$r/scripts/test.sh"
    chmod +x "$r/scripts/build.sh" "$r/scripts/test.sh"
    # As many planted suites as the base will carry, so `_suite_floor` is satisfiable.
    for i in 1 2 3 4 5; do
        printf '#!/bin/sh\nexit ${SUITE_RC:-0}\n' >"$r/scripts/tests/s$i.test.sh"
        chmod +x "$r/scripts/tests/s$i.test.sh"
    done
    echo base >"$r/f"
    git -C "$r" add -A; git -C "$r" commit -qm base
    git -C "$r" branch -M trunk
    printf '%s\n' "$r"
}

# queue <repo> [VAR=VAL …] — run the queue over branch `feat`, print "<exit> <moved> <lastline>".
queue() {
    local r="$1"; shift
    local pre post rc out
    pre="$(git -C "$r" rev-parse trunk)"
    out="$( cd "$r" && PATH="$work/bin:$PATH" JKB=jkb env "$@" \
        bash scripts/merge-queue.sh feat trunk "$r" 2>&1 )"
    rc=$?
    post="$(git -C "$r" rev-parse trunk)"
    printf '%s %s %s\n' "$rc" "$([ "$pre" = "$post" ] && echo still || echo moved)" "$(tail -1 <<<"$out")"
}

# check <label> <repo> <want-exit> <want-moved> [VAR=VAL …]
check() {
    local label="$1" r="$2" want_rc="$3" want_mv="$4"; shift 4
    local got rc mv
    got="$(queue "$r" "$@")"
    rc="${got%% *}"; mv="$(cut -d' ' -f2 <<<"$got")"
    if [ "$rc" = "$want_rc" ] && [ "$mv" = "$want_mv" ]; then
        ok "$label → exit $rc, base $mv"
    else
        fail "queue: $label" "wanted exit $want_rc with the base $want_mv, got exit $rc with the \
base $mv. Last line: $(cut -d' ' -f3- <<<"$got")"
    fi
}

# --- the seven states the queue can reach ---------------------------------------------------
case_land() {       # the ordinary path: green gate, base advances
    local r; r="$(mkrepo land)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk
    check "a green branch lands" "$r" 0 moved
}

case_red_gate() {   # THE HEADLINE: a red gate must not have moved the base
    local r; r="$(mkrepo red)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk
    check "a red cargo gate ejects and the base never moves" "$r" 2 still TEST_RC=1
}

case_red_suite() {  # the half of the gate that could not fail before this file's rework
    local r; r="$(mkrepo suite)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk
    check "a red shell suite ejects and the base never moves" "$r" 2 still SUITE_RC=1
}

case_nothing_ahead() {
    local r; r="$(mkrepo ahead)"
    git -C "$r" branch feat
    check "a branch with no commits ahead is handed back" "$r" 1 still
}

case_empty_work() { # commits, but a net diff of nothing: the phantom landing
    local r; r="$(mkrepo empty)"
    git -C "$r" checkout -qb feat
    git -C "$r" commit -q --allow-empty -m nothing
    echo g >"$r/g"; git -C "$r" add g; git -C "$r" commit -qm add
    git -C "$r" revert --no-edit HEAD >/dev/null
    git -C "$r" checkout -q trunk
    check "a branch whose net diff is empty is handed back, not landed" "$r" 1 still
}

case_conflict() {
    local r; r="$(mkrepo conflict)"
    git -C "$r" checkout -qb feat; echo THEIRS >"$r/f"; git -C "$r" commit -qam theirs
    git -C "$r" checkout -q trunk; echo OURS >"$r/f"; git -C "$r" commit -qam ours
    check "a rebase conflict is handed back and the base never moves" "$r" 1 still
}

case_already_landed() {   # real content, already in the base under somebody else's commit
    local r; r="$(mkrepo already)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam mine
    git -C "$r" checkout -q trunk; echo x >>"$r/f"; git -C "$r" commit -qam theirs
    check "content already in the base is reported without moving it" "$r" 0 still
}

# --- and the one that wedges the worktree ---------------------------------------------------
# A RED GATE THAT ALSO DIRTIES THE TREE. The gate runs for minutes in this worktree, so a tracked
# file left modified is ordinary — an interrupted build, an operator opening a file to read the
# logs the eject message points at. `git switch` then refuses rather than overwriting it, and that
# refusal used to be swallowed by `|| true`: HEAD stayed detached at the gated commit and the NEXT
# run died at startup with the setup-error code, which the swarm reads as the implementer's fault.
# Every branch behind it would then eject for a wedged worktree.
#
# It must be 4, not 2: 2 says "your gate failed, fix it", and this worktree needs a person before
# anything else can run at all.
case_wedged() {
    local r; r="$(mkrepo wedged)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk
    # A gate that fails AND leaves a tracked file modified, which is what blocks the switch back.
    printf '#!/bin/sh\necho dirtied >> f\nexit 1\n' >"$r/scripts/test.sh"
    chmod +x "$r/scripts/test.sh"
    git -C "$r" add scripts/test.sh; git -C "$r" commit -qm "a gate that dirties the tree"
    git -C "$r" checkout -q feat; git -C "$r" rebase -q trunk >/dev/null 2>&1
    git -C "$r" checkout -q trunk
    check "a red gate that also wedges the worktree stalls for a human" "$r" 4 still
}

echo "==> scripts/merge-queue.sh: what it exits, and whether the base moved"
mkdir -p "$work/bin" && printf '#!/bin/sh\nexit 0\n' >"$work/bin/jkb" && chmod +x "$work/bin/jkb"
run_cases case_land case_red_gate case_red_suite case_nothing_ahead case_empty_work \
          case_conflict case_already_landed case_wedged

finish
