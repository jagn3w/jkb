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
# DID $BASE MOVE. Those together are the whole contract — the swarm's coordinator reads the
# script's outcome line to decide whether a task group landed, and the base moving is what "landed"
# is supposed to mean. Every case also checks that the outcome line is printed, last, naming the
# code the script exited with; `dev-scripts.test.sh`'s case10 holds the other half of that seam,
# that every code the header documents is classified as this test expects.
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
    # THE BRANCH, NOT WHATEVER `trunk` RESOLVES TO: a bare name prefers a same-named tag, and the
    # tag cases below plant exactly that.
    pre="$(git -C "$r" rev-parse refs/heads/trunk)"
    : >"$r/.jkb-calls"
    out="$( cd "$r" && PATH="$work/bin:$PATH" JKB=jkb JKB_CALLS="$r/.jkb-calls" env "$@" \
        bash scripts/merge-queue.sh feat trunk "$r" 2>&1 )"
    rc=$?
    # The whole output, kept beside the repo for `queue_detail`; what this prints is unchanged.
    printf '%s\n' "$out" >"$r.out"
    post="$(git -C "$r" rev-parse refs/heads/trunk)"
    # `grep -c` PRINTS ITS ZERO AND THEN EXITS 1, so `|| echo 0` appends a second zero and every
    # field after it shifts. `|| true` keeps the count grep already printed.
    local calls detail
    # The landing records only: `--check` is the approval question asked before the graft.
    calls="$(grep -v -e '--check' "$r/.jkb-calls" 2>/dev/null | grep -c 'task landed' || true)"
    # ...and git's own text is multi-line, so the detail is flattened rather than trusted to be
    # one field — this harness is parsing it positionally.
    detail="$(tail -1 <<<"$out" | tr '\n' ' ')"
    printf '%s|%s|%s|%s\n' "$rc" "$([ "$pre" = "$post" ] && echo still || echo moved)" \
        "${calls:-0}" "$detail"
}

# queue_detail <repo> — the line before the outcome line of the last `queue` over <repo>: the
# detail a coordinator reports, which `queue`'s positional format does not carry.
queue_detail() { tail -2 "$1.out" | sed -n 1p; }

# check <label> <repo> <want-exit> <want-moved> <want-landed-calls> [VAR=VAL …]
check() {
    local label="$1" r="$2" want_rc="$3" want_mv="$4" want_jkb="$5"; shift 5
    local got rc mv jkb
    got="$(queue "$r" "$@")"
    IFS='|' read -r rc mv jkb detail <<<"$got"
    # THE OUTCOME LINE, ON EVERY PATH. The coordinator acts on it, so an exit that skips it (a
    # trap lost, an `exec` added) leaves it nothing to act on but a guess.
    if ! [[ "${detail% }" =~ ^merge-queue:\ (landed|eject|stall)\ \(exit\ $rc\)$ ]]; then
        fail "queue: $label outcome line" "the last line is not \`merge-queue: <outcome> (exit $rc)\`: ${detail}"
    fi
    if [ "$rc" = "$want_rc" ] && [ "$mv" = "$want_mv" ] && [ "$jkb" = "$want_jkb" ]; then
        ok "$label → exit $rc, base $mv, $jkb landing record(s)"
    else
        fail "queue: $label" "wanted exit $want_rc with the base $want_mv and $want_jkb \
\`jkb task landed\` call(s); got exit $rc, base $mv, $jkb call(s). A landing recorded over a base \
that did not move closes the whole group. Last line: $(cut -d' ' -f4- <<<"$got")"
    fi
}

# --- the seven states the queue can reach ---------------------------------------------------
case_land() {       # the ordinary path: green gate, base advances
    local r; r="$(mkrepo land)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk
    check "a green branch lands" "$r" 0 moved 1
}

# NO REVIEWER'S APPROVAL, NO GRAFT. jkb is asked before anything moves, because the landing is
# recorded only after the graft — refused then, the work would sit on the base with its tasks open.
case_unapproved() {
    local r; r="$(mkrepo unapproved)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk
    check "a head no reviewer approved stalls, and the base never moves" "$r" 6 still 0 APPROVE_RC=1
    local tip; tip="$(git -C "$r" rev-parse refs/heads/feat)"
    grep -qx "task landed feat --onto trunk --head $tip --check" "$r/.jkb-calls" \
        && ok "the queue asks jkb about exactly the branch, base and commit it would graft" \
        || fail "queue: approval question" "jkb was not asked \`task landed feat --onto trunk --head $tip --check\`: $(cat "$r/.jkb-calls")"
}

# THE HEAD IS READ ONCE. A commit the implementer makes after the approval question is not grafted:
# the stub's approval answer commits to the branch, as an implementer racing the queue would.
case_moved_after_approval() {
    local r tip; r="$(mkrepo moved)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk
    tip="$(git -C "$r" rev-parse refs/heads/feat)"
    check "a commit made after the approval question is not what lands" "$r" 0 moved 1 \
        RACE_REPO="$r"
    if git -C "$r" cat-file -e refs/heads/trunk:late 2>/dev/null; then
        fail "queue: head read once" "the commit made after the approval question was grafted"
    elif grep -q "^task landed feat --onto trunk --head $tip\$" "$r/.jkb-calls"; then
        ok "the landing is recorded at the commit that was asked about and grafted"
    else
        fail "queue: head read once" "the landing was not recorded at $tip: $(cat "$r/.jkb-calls")"
    fi
}

case_red_gate() {   # THE HEADLINE: a red gate must not have moved the base
    local r; r="$(mkrepo red)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk
    check "a red cargo gate ejects and the base never moves" "$r" 2 still 0 TEST_RC=1
}

case_red_suite() {  # the half of the gate that could not fail before this file's rework
    local r; r="$(mkrepo suite)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk
    check "a red shell suite ejects and the base never moves" "$r" 2 still 0 SUITE_RC=1
}

case_nothing_ahead() {
    local r; r="$(mkrepo ahead)"
    git -C "$r" branch feat
    check "a branch with no commits ahead of the base stalls for a person" "$r" 5 still 0
}

case_empty_work() { # commits, but a net diff of nothing: the phantom landing
    local r; r="$(mkrepo empty)"
    git -C "$r" checkout -qb feat
    git -C "$r" commit -q --allow-empty -m nothing
    echo g >"$r/g"; git -C "$r" add g; git -C "$r" commit -qm add
    git -C "$r" revert --no-edit HEAD >/dev/null
    git -C "$r" checkout -q trunk
    check "a diverged branch whose net diff is empty is handed back, not landed" "$r" 1 still 0
}

case_conflict() {
    local r; r="$(mkrepo conflict)"
    git -C "$r" checkout -qb feat; echo THEIRS >"$r/f"; git -C "$r" commit -qam theirs
    git -C "$r" checkout -q trunk; echo OURS >"$r/f"; git -C "$r" commit -qam ours
    check "a rebase conflict is handed back and the base never moves" "$r" 1 still 0
    # ...NAMING THE COMMIT AND THE PATH, and leaving nothing behind: the graft is built in the object
    # database, so there is no half-done rebase to abort and no switch back that could fail.
    local d bad=""; d="$(queue_detail "$r")"
    [[ "$d" =~ ^eject:\ rebase\ conflict\ onto\ trunk\ —\ [0-9a-f]+\ theirs:\ f\ \(see\ /tmp/merge-queue\.log\)$ ]] \
        || bad="$bad the detail does not name the commit and the path: $d;"
    [ "$(git -C "$r" symbolic-ref -q --short HEAD)" = trunk ] || bad="$bad the worktree is not on trunk;"
    [ -z "$(git -C "$r" status --porcelain --untracked-files=no)" ] || bad="$bad the worktree is not clean;"
    [ ! -e "$(git -C "$r" rev-parse --absolute-git-dir)/rebase-merge" ] || bad="$bad a rebase-merge directory was left;"
    [ -z "$bad" ] && ok "a conflict names the commit and the path, and leaves the worktree on trunk, clean" \
        || fail "queue: conflict detail" "$bad"
}

# THE REGRESSION: a pick that refuses a CLEAN tree. On the container's virtiofs repo mount, rebase's
# pick said "Your local changes ... would be overwritten" for exactly the files the commit changed
# (exec-99, exec-101). The shim makes every porcelain replay refuse that way, so a branch lands only
# if the queue never asks rebase, cherry-pick or am to touch the worktree.
case_phantom() {
    local r shim; r="$(mkrepo phantom)"; shim="$work/phantom-bin"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk
    mkdir -p "$shim"
    cat >"$shim/git" <<SHIM
#!/bin/sh
case "\$1" in rebase | cherry-pick | am)
    echo "error: Your local changes to the following files would be overwritten by merge:" >&2
    printf '\tf\n' >&2
    exit 1 ;;
esac
exec "$(command -v git)" "\$@"
SHIM
    chmod +x "$shim/git"
    check "a clean branch lands when every porcelain replay refuses a clean tree" "$r" 0 moved 1 \
        PATH="$shim:$work/bin:$PATH"
}

# PARITY WITH GIT'S OWN REBASE, on the inputs where a hand-written graft could differ: a normal edit,
# a commit that STARTED empty (rebase keeps it), one that BECOMES empty against the base (dropped),
# one whose patch the base already carries and that would conflict if replayed (skipped by
# --cherry-pick), an evil merge of a side branch (flattened, its own change lost, as rebase loses
# it), and a message whose bytes `git commit` would
# have cleaned up. The base moves on meanwhile. What `git rebase` makes of the same commits, in a
# scratch worktree, is the expectation, commit by commit.
case_parity() {
    local r rb pre want got i n bad=""; r="$(mkrepo parity)"
    printf 'one\n' >"$r/g"; git -C "$r" add g; git -C "$r" commit -qm "g"
    git -C "$r" checkout -qb feat
    echo a >>"$r/f"; git -C "$r" commit -qam "edit one"
    git -C "$r" commit -q --allow-empty -m "started empty"
    echo same >"$r/dup"; git -C "$r" add dup; git -C "$r" commit -qm "dup, which the base gets too"
    printf 'one\nX\n' >"$r/g"; git -C "$r" commit -qam "X, a patch the base carries"
    git -C "$r" checkout -qb side
    echo s >"$r/s"; git -C "$r" add s; git -C "$r" commit -qm "side work"
    # An EVIL merge: it adds a line no parent has. Rebase without --rebase-merges drops the merge
    # and that line with it, and so must the queue -- this is what makes --no-merges observable.
    git -C "$r" checkout -q feat; git -C "$r" merge -q --no-ff --no-commit side >/dev/null 2>&1
    echo evil >>"$r/s"; git -C "$r" add s; git -C "$r" commit -qm "merge side"
    echo b >>"$r/f"; git -C "$r" add f
    printf 'edit two  \n\n\nbody\t\n\n' | git -C "$r" commit -q --cleanup=verbatim -F -
    git -C "$r" checkout -q trunk; git -C "$r" branch -qD side
    echo u >"$r/u"; git -C "$r" add u; git -C "$r" commit -qm "unrelated"
    echo same >"$r/dup"; echo o >"$r/other"; git -C "$r" add dup other; git -C "$r" commit -qm "dup and other"
    printf 'one\nX\n' >"$r/g"; git -C "$r" commit -qam "X on the base"
    printf 'one\nY\n' >"$r/g"; git -C "$r" commit -qam "X becomes Y"
    pre="$(git -C "$r" rev-parse refs/heads/trunk)"
    rb="$work/parity-rebase"
    git -C "$r" worktree add -q --detach "$rb" refs/heads/feat
    if ! git -C "$rb" rebase -q refs/heads/trunk >/dev/null 2>&1; then
        fail "queue: parity" "git's own rebase of the parity branch failed, so there is nothing to compare against"
        return
    fi
    want="$(git -C "$rb" rev-parse HEAD)"
    git -C "$r" worktree remove --force "$rb"
    check "a branch of edits, empty commits, a cherry-pick and a merge lands" "$r" 0 moved 1
    # tree, author and raw message of each commit, oldest first
    describe() {
        local c
        for c in $(git -C "$r" rev-list --reverse "$pre..$1"); do
            printf '%s|%s|' "$(git -C "$r" rev-parse "$c^{tree}")" \
                "$(git -C "$r" cat-file commit "$c" | sed -n '1,/^$/s/^author //p')"
            git -C "$r" cat-file commit "$c" | sed '1,/^$/d' | od -An -c | tr -s ' \n' ' '
            echo
        done
    }
    want="$(describe "$want")"; got="$(describe refs/heads/trunk)"
    n="$(grep -c . <<<"$want")"
    [ "$n" -eq 4 ] || bad="$bad git's rebase made $n commits, where this case expects 4 (edit one, started empty, side work, edit two);"
    [ "$want" = "$got" ] || bad="$bad the queue's commits differ from git's rebase:
       rebase: $(tr '\n' '~' <<<"$want")
       queue:  $(tr '\n' '~' <<<"$got");"
    [ -z "$bad" ] && ok "the queue's graft is git's rebase, commit for commit: tree, author and message bytes" \
        || fail "queue: parity" "$bad"
}

# ...AND A BRANCH ALREADY ON THE BASE TIP LANDS AS ITS OWN COMMITS. Rebase fast-forwards a pick
# whose parent is the current tip, so the commits keep their ids, committer and signature, and the
# branch is an ancestor of the base afterwards. A graft that rewrote them would land copies.
case_parity_ff() {
    local r tip; r="$(mkrepo parity-ff)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam one
    git -C "$r" commit -q --allow-empty -m "started empty"
    echo y >>"$r/f"; git -C "$r" commit -qam two
    git -C "$r" checkout -q trunk
    git -C "$r" config user.name Queue; git -C "$r" config user.email queue@example.com
    tip="$(git -C "$r" rev-parse refs/heads/feat)"
    check "a branch cut from the base tip lands" "$r" 0 moved 1
    [ "$(git -C "$r" rev-parse refs/heads/trunk)" = "$tip" ] \
        && ok "a branch cut from the base tip lands as its own commits, as rebase leaves them" \
        || fail "queue: parity ff" "the base is at $(git -C "$r" rev-parse refs/heads/trunk), not the branch's own $tip: its commits were rewritten"
}

# A DIRTY QUEUE WORKTREE IS A STALL THAT NAMES THE FILE. `reset --hard` would discard the change
# silently; the rebase used to carry it into the gate, or eject it as the implementer's conflict.
case_dirty_tracked() {
    local r; r="$(mkrepo dirty)"
    git -C "$r" checkout -qb feat; echo w >"$r/work"; git -C "$r" add work; git -C "$r" commit -qm work
    git -C "$r" checkout -q trunk; echo dirty >>"$r/f"
    check "a tracked change in the queue's worktree stalls, and the base never moves" "$r" 3 still 0
    if [ "$(tail -1 "$r/f")" != dirty ]; then
        fail "queue: dirty tracked" "the tracked change in the queue's worktree was discarded"
    elif [[ "$(queue_detail "$r")" != "error: the queue's worktree has tracked changes (f);"* ]]; then
        fail "queue: dirty tracked detail" "the detail does not name the file: $(queue_detail "$r")"
    else
        ok "the tracked change is still there, and the stall names it"
    fi
}

# AN UNTRACKED FILE AT A PATH THE GRAFT ADDS. `reset --hard` would overwrite it, where checkout refused.
case_untracked_collision() {
    local r; r="$(mkrepo untracked)"
    git -C "$r" checkout -qb feat; echo branch >"$r/new.txt"; git -C "$r" add new.txt; git -C "$r" commit -qm new
    git -C "$r" checkout -q trunk; echo mine >"$r/new.txt"
    check "an untracked file the graft would overwrite stalls, and the base never moves" "$r" 3 still 0
    if [ "$(cat "$r/new.txt")" != mine ]; then
        fail "queue: untracked collision" "the untracked new.txt was overwritten: $(cat "$r/new.txt")"
    elif [[ "$(queue_detail "$r")" != "error: the queue's worktree has an untracked new.txt,"* ]]; then
        fail "queue: untracked collision detail" "the detail does not name the file: $(queue_detail "$r")"
    else
        ok "the untracked file is untouched, and the stall names it"
    fi
}

# THE AUTHOR IS KEPT, THE COMMITTER IS THE QUEUE: what rebase did, and what `git blame` relies on.
case_multi_author() {
    local r got; r="$(mkrepo author)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"
    GIT_AUTHOR_NAME="Other Person" GIT_AUTHOR_EMAIL=other@example.com \
        GIT_AUTHOR_DATE="981173106 +0130" git -C "$r" commit -qam "someone else's work"
    # The base moves on, so the commit is replayed rather than fast-forwarded (case_parity_ff).
    git -C "$r" checkout -q trunk; echo u >"$r/u"; git -C "$r" add u; git -C "$r" commit -qm unrelated
    git -C "$r" config user.name Queue; git -C "$r" config user.email queue@example.com
    check "a branch authored by someone else lands" "$r" 0 moved 1
    got="$(git -C "$r" log -1 --format='%an <%ae> %ad|%cn <%ce>' --date=raw refs/heads/trunk)"
    [ "$got" = "Other Person <other@example.com> 981173106 +0130|Queue <queue@example.com>" ] \
        && ok "the landed commit keeps its author and date, and the queue is its committer" \
        || fail "queue: multi author" "the landed commit reads: $got"
}

# GIT'S REASON WHEN THE BASE CANNOT BE SWITCHED TO. This said only "cannot switch to base", and a
# container run request stalled with "dubious ownership" swallowed (exec-97). A second worktree
# holding the base is the refusal reachable without another uid.
case_base_unswitchable() {
    local r d; r="$(mkrepo unswitchable)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" worktree add -q "$work/holder" trunk
    check "a base held by another worktree stalls, and the base never moves" "$r" 3 still 0
    d="$(queue_detail "$r")"
    [[ "$d" =~ ^error:\ cannot\ switch\ to\ base\ trunk\ —\ git\ said:\ fatal:\  ]] && [[ "$d" == *"$work/holder"* ]] \
        && ok "the stall carries git's own reason, naming the worktree that holds the base" \
        || fail "queue: base unswitchable detail" "the detail does not carry git's reason: $d"
    git -C "$r" worktree remove --force "$work/holder"
}

case_already_landed() {   # real content, already in the base under somebody else's commit
    local r; r="$(mkrepo already)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam mine
    git -C "$r" checkout -q trunk; echo x >>"$r/f"; git -C "$r" commit -qam theirs
    check "content already in the base is reported without moving it" "$r" 0 still 1
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
    check "a red gate that also wedges the worktree stalls for a human" "$r" 4 still 0
}

# --- the branch whose work is already in the base ------------------------------------------
# THE ONE THE GRAPH CANNOT ANSWER. An earlier entry landed this branch's content and the
# implementer then did exactly what the eject path tells them to — rebased onto the base — so the
# branch is now an ancestor of it. Indistinguishable, from the graph alone, from an implementer
# who reported ready and never committed. It must not be closed (that would mark a group done for
# work that may never have been written) and must not be handed back (that sends someone to fix a
# branch whose work is already in), so it stalls with no landing record.
case_already_merged() {
    local r; r="$(mkrepo merged)"
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk; git -C "$r" merge -q --ff-only feat
    echo y >>"$r/f"; git -C "$r" commit -qam "the base moved on"
    check "a branch already merged into the base stalls rather than closing or ejecting" "$r" 5 still 0
}

# --- a tag named like either branch -------------------------------------------------------
# BARE NAMES RESOLVE A TAG FIRST. git's DWIM order tries `refs/tags/<name>` before
# `refs/heads/<name>`, so while the queue read its two arguments bare, a tag named like the work
# branch was what it grafted, and a tag named like the base was what it rebased onto. Each tag below
# points at a DECOY commit carrying a file of its own, so the question is not only the exit code but
# WHAT landed: the branch's work on the base branch, and no decoy anywhere in it.
#
# landed_tree <label> <repo> — the base branch holds the work and the base's own commit, no decoy.
landed_tree() {
    local label="$1" r="$2" want=""
    git -C "$r" cat-file -e refs/heads/trunk:work 2>/dev/null || want="$want the branch's work is missing;"
    git -C "$r" cat-file -e refs/heads/trunk:later 2>/dev/null || want="$want the base's own later commit is missing;"
    ! git -C "$r" cat-file -e refs/heads/trunk:decoy 2>/dev/null || want="$want the tag's decoy commit landed;"
    if [ -z "$want" ]; then
        ok "$label: the base branch holds the branch's work and nothing from the tag"
    else
        fail "queue: $label tree" "after the queue ran, refs/heads/trunk is wrong:$want"
    fi
}

# mktagged <name> — trunk is base→later, feat is base→work, and a decoy commit off base adds
# `decoy`. The caller tags the decoy with the name it is testing.
mktagged() {
    local r; r="$(mkrepo "$1")"
    git -C "$r" switch -qc decoy-src; echo d >"$r/decoy"; git -C "$r" add decoy
    git -C "$r" commit -qm decoy
    git -C "$r" switch -q trunk; git -C "$r" switch -qc feat
    echo w >"$r/work"; git -C "$r" add work; git -C "$r" commit -qm work
    git -C "$r" switch -q trunk; echo l >"$r/later"; git -C "$r" add later; git -C "$r" commit -qm later
    printf '%s\n' "$r"
}

# THE WORK BRANCH'S TAG DECIDES THE ENTRY CHECKS, not the graft: `checkout --detach <name>` picks
# the branch, but `rev-parse`, `merge-base --is-ancestor` and `diff` all read the tag. So the two
# shapes that fail are the ones where the tag and the branch answer those checks differently.
#
# A stale tag at an ancestor of the base (a release tag, an earlier landing's marker): bare, the
# queue saw "already an ancestor" and stalled a branch with real work in it.
case_tag_like_branch() {
    local r; r="$(mktagged tag-branch)"
    git -C "$r" tag feat refs/heads/trunk~1; git -C "$r" branch -qD decoy-src
    check "a stale tag named like the work branch does not stall real work" "$r" 0 moved 1
    landed_tree "a tag named like the work branch" "$r"
}

# ...and the reverse, which is the phantom landing: the BRANCH is already an ancestor of the base
# (nothing to land — exit 5, a person decides) while a diverged tag of the same name carries work.
# Bare, the entry checks passed on the tag, the graft rebased the branch to a no-op, and the queue
# printed `landed:` and called `jkb task landed` with nothing of the branch's in the base.
case_tag_like_ancestor_branch() {
    local r; r="$(mktagged tag-ancestor)"
    git -C "$r" branch -qf feat refs/heads/trunk~1
    git -C "$r" tag feat decoy-src; git -C "$r" branch -qD decoy-src
    check "a diverged tag named like an already-merged branch does not close it" "$r" 5 still 0
}

case_tag_like_base() {     # bare, the queue rebased onto the tag and the fast-forward then failed
    local r; r="$(mktagged tag-base)"
    git -C "$r" tag trunk decoy-src; git -C "$r" branch -qD decoy-src
    check "a tag named like the base is not what the branch is grafted onto" "$r" 0 moved 1
    landed_tree "a tag named like the base" "$r"
}

# THE SUITE FLOOR IS READ FROM THE BASE BRANCH. A tag named like the base on a commit carrying
# FEWER suites lowered the floor the gate holds the integrated tree to: here the tag has four, the
# base branch five, and the branch deletes one, so only a floor read from the branch catches it.
# Bare, the floor was four, the deletion passed, and the queue landed it.
case_tag_like_base_suite_floor() {
    local r; r="$(mkrepo tag-floor)"
    git -C "$r" switch -qc d; git -C "$r" rm -q scripts/tests/s5.test.sh; git -C "$r" commit -qm "fewer suites"
    git -C "$r" switch -q trunk; git -C "$r" tag trunk d; git -C "$r" branch -qD d
    git -C "$r" switch -qc feat; git -C "$r" rm -q scripts/tests/s4.test.sh
    echo w >"$r/work"; git -C "$r" add work; git -C "$r" commit -qm "work, one suite fewer"
    git -C "$r" switch -q trunk 2>/dev/null     # picks the branch; the "ambiguous" warning is dropped
    check "a tag named like the base does not lower the suite floor" "$r" 2 still 0
}

# THE MERGE BASE IS ASKED OF THE BASE BRANCH. A tag named like the base on an ORPHAN commit shares
# no history with the work branch, so bare, `merge-base` found no common ancestor and the queue
# refused an ordinary branch as a setup error (exit 3).
case_tag_like_base_orphan() {
    local r; r="$(mkrepo tag-orphan)"
    git -C "$r" switch -q --orphan o; echo o >"$r/orphan"; git -C "$r" add orphan
    git -C "$r" commit -qm orphan
    # `switch` takes only branches, so it picks the branch; its "ambiguous" warning is dropped.
    git -C "$r" tag trunk o; git -C "$r" switch -qf trunk 2>/dev/null; git -C "$r" branch -qD o
    git -C "$r" switch -qc feat; echo w >"$r/work"; git -C "$r" add work; git -C "$r" commit -qm work
    git -C "$r" switch -q trunk 2>/dev/null
    check "a tag named like the base on an unrelated commit does not hide the merge base" "$r" 0 moved 1
}

echo "==> scripts/merge-queue.sh: what it exits, and whether the base moved"
# THE STUB RECORDS. Asserting the exit code and the base ref leaves out the half of a phantom
# landing that does the damage: `jkb task landed` is what closes every task in the group and
# unblocks its dependents. A run can report the right code, leave the base alone, and still have
# made that call — so the call is now evidence the cases check.
mkdir -p "$work/bin"
# `task landed --check` answers APPROVE_RC: whether jkb finds a reviewer's approval of the head.
# With RACE_REPO set, answering the question also commits to the branch, as a racing implementer.
cat >"$work/bin/jkb" <<'STUB'
#!/bin/sh
printf "%s\n" "$*" >> "$JKB_CALLS"
case "$*" in *--check*)
    if [ -n "${RACE_REPO:-}" ]; then
        git -C "$RACE_REPO" worktree add -q "$RACE_REPO.race" feat >/dev/null 2>&1
        echo late >"$RACE_REPO.race/late"; git -C "$RACE_REPO.race" add late
        git -C "$RACE_REPO.race" commit -qm late >/dev/null 2>&1
    fi
    exit "${APPROVE_RC:-0}" ;;
esac
exit 0
STUB
chmod +x "$work/bin/jkb"
case_signalled() {   # killed mid-gate by any signal: never `landed`, and the base never moves
    local r pid out sig pre bad="" want; r="$(mkrepo signalled)"
    printf '#!/bin/sh\nsleep 4\ntouch "$(dirname "$0")/../gate.done"\nexit 0\n' >"$r/scripts/build.sh"
    printf 'gate.done\n' >"$r/.gitignore"
    git -C "$r" add -A; git -C "$r" commit -qm slow-gate
    git -C "$r" checkout -qb feat; echo x >>"$r/f"; git -C "$r" commit -qam work
    git -C "$r" checkout -q trunk
    pre="$(git -C "$r" rev-parse trunk)"
    # TERM and HUP are what a timeout or a closed terminal sends, and are trapped: the gate finishes
    # first (no orphan in the worktree) and the stall names them. USR1 and USR2 are ones nothing
    # names, which is the point: the outcome must not depend on which signals someone trapped. Not
    # INT, and not PIPE: a signal ignored when bash starts can be neither trapped nor delivered, and
    # a non-interactive shell starts background jobs with SIGINT ignored, while some CI runners start
    # every job with SIGPIPE ignored (seen on CI: the PIPE case ran the queue to a real landing). Either
    # way the queue runs to its real end and the case tests nothing.
    for sig in TERM HUP USR1 USR2; do
        git -C "$r" checkout -q trunk 2>/dev/null; git -C "$r" reset -q --hard "$pre"; rm -f "$r/gate.done"
        ( cd "$r" && PATH="$work/bin:$PATH" JKB=jkb JKB_CALLS="$r/.jkb-calls" \
            exec bash scripts/merge-queue.sh feat trunk "$r" >"$work/signalled.out" 2>&1 ) &
        pid=$!
        sleep 2; kill -s "$sig" "$pid"; wait "$pid" 2>/dev/null
        out="$(tail -1 "$work/signalled.out")"
        case "$sig" in
            TERM | HUP) want="merge-queue: stall: killed by $sig, after the step it was running finished" ;;
            *) want="merge-queue: stall: ended without an outcome" ;;
        esac
        if [ "${out#"$want"}" = "$out" ]; then
            bad=1; fail "queue: signalled $sig" "a $sig mid-gate ended with: $out"; continue
        fi
        case "$sig" in TERM | HUP)
            [ -e "$r/gate.done" ] \
                || { bad=1; fail "queue: signalled $sig gate" "the queue exited on $sig before its gate finished, leaving it running in the worktree"; } ;;
        esac
        [ "$(git -C "$r" rev-parse trunk)" = "$pre" ] \
            || { bad=1; fail "queue: signalled $sig base" "a queue killed by $sig mid-gate moved the base"; }
    done
    sleep 3   # let an untrapped run's orphaned gate finish before the next case
    [ -n "$bad" ] || ok "a queue killed mid-gate by TERM, HUP, USR1 or USR2 reports a stall and leaves the base (TERM/HUP after the gate)"
}

run_cases case_signalled case_land case_unapproved case_moved_after_approval case_red_gate case_red_suite case_nothing_ahead case_empty_work \
          case_conflict case_phantom case_parity case_parity_ff case_dirty_tracked case_untracked_collision \
          case_multi_author case_base_unswitchable \
          case_already_landed case_wedged case_already_merged \
          case_tag_like_branch case_tag_like_ancestor_branch case_tag_like_base \
          case_tag_like_base_suite_floor case_tag_like_base_orphan

finish
