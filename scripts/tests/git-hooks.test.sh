#!/usr/bin/env bash
# Regression test for scripts/lib.sh's two answers to "where do this repo's hooks go":
# `git_hooks_dir` (the directory git runs hooks from) and `git_hooks_override` (the
# `core.hooksPath` that replaces it). They share a subject and an oracle, so they share a file.
#
# The bug it exists for: setup.sh derived the hook directory from `git rev-parse --git-dir`,
# which in a linked worktree is `<repo>/.git/worktrees/<name>` — a directory git never reads
# hooks from. So `./scripts/setup.sh` inside any `jkb task work` session installed the hook
# where nothing would run it, printed success, and left the stale hook in place.
#
# The oracle is git: `git rev-parse --git-path hooks/post-merge` is the path git itself will
# execute. Asserting against that rather than against a hand-written path is the point —
# a hard-coded expectation would just re-encode whichever rule the code happens to use.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

new_workdir
isolate_git "$work/home"

# Where git will actually run the post-merge hook from, as an absolute path.
oracle_dir() {
    local dir="$1" p d
    p="$(git -C "$dir" rev-parse --git-path hooks/post-merge)" || return 1
    d="$(dirname "$p")"
    case "$d" in /*) ;; *) d="$dir/$d" ;; esac
    (cd "$d" && pwd -P)
}
# Falls back to the raw path when it does not exist — which is itself the answer under the
# old rule, and makes the failure message say so instead of erroring on the `cd`.
abs_dir() { (cd "$1" 2>/dev/null && pwd -P) || printf '%s\n' "$1"; }

# A repo with one commit (worktrees need a born HEAD) plus a linked worktree.
main="$work/main"
git_q init -q "$main" >/dev/null 2>&1
git_q -C "$main" commit -q --allow-empty -m init
wt="$work/wt"
git_q -C "$main" worktree add -q "$wt" -b side >/dev/null 2>&1

# --- 1. an ordinary checkout ------------------------------------------------------------
case1() {
    local got
    got="$(git_hooks_dir "$main")" || { fail "plain: exit" "git_hooks_dir failed in a repo"; return; }
    if [ "$(abs_dir "$got")" = "$(oracle_dir "$main")" ]; then
        ok "an ordinary checkout: agrees with git"
    else
        fail "plain: path" "got $(abs_dir "$got"), git runs $(oracle_dir "$main")"
    fi
}

# --- 2. a linked worktree — the case the old code got wrong ------------------------------
case2() {
    local got wrong
    got="$(git_hooks_dir "$wt")" || { fail "worktree: exit" "git_hooks_dir failed in a worktree"; return; }
    if [ "$(abs_dir "$got")" = "$(oracle_dir "$wt")" ]; then
        ok "a linked worktree: agrees with git (the common dir, not the worktree's)"
    else
        fail "worktree: path" "got $(abs_dir "$got"), git runs $(oracle_dir "$wt")"
    fi
    # Name the old rule explicitly, so a revert to it fails here with the reason.
    wrong="$(git -C "$wt" rev-parse --git-dir)/hooks"
    if [ "$got" != "$wrong" ]; then
        ok "a linked worktree: not the per-worktree directory git ignores"
    else
        fail "worktree: regression" "resolved to \$(rev-parse --git-dir)/hooks, which git never reads"
    fi
}

# --- 3. not a git repo ------------------------------------------------------------------
# setup.sh branches on the empty output to warn and skip, so silence matters as much as the
# exit status.
case3() {
    local out
    mkdir -p "$work/plain"
    if out="$(git_hooks_dir "$work/plain" 2>/dev/null)"; then
        fail "non-repo: exit" "reported success outside a repo (printed '$out')"
    elif [ -n "$out" ]; then
        fail "non-repo: output" "printed '$out' while failing"
    else
        ok "outside a repo: fails, printing nothing"
    fi
}

# --- 4. core.hooksPath is read from the repo we asked about, not from the cwd ------------
# The must-fix this file grew for: setup.sh asked a bare `git config`, which answers for
# whatever repository the caller happens to be standing in.
case4() {
    local other="$work/other" got
    git_q init -q "$other" >/dev/null 2>&1
    git_q -C "$other" config core.hooksPath "$work/others-hooks"

    # Standing in the other repo, asking about ours: its setting must not leak.
    got="$(cd "$other" && git_hooks_override "$main")"
    if [ -z "$got" ]; then
        ok "core.hooksPath: another repo's setting does not leak into ours"
    else
        fail "override: leak" "reported '$got' for a repo that sets nothing"
    fi

    # And the converse — ours is found from outside it, which is the miss that leaves the
    # repo hook dead with no chainer written.
    git_q -C "$main" config core.hooksPath "$work/our-hooks"
    got="$(cd "$other" && git_hooks_override "$main")"
    if [ "$got" = "$work/our-hooks" ]; then
        ok "core.hooksPath: ours is found from outside the checkout"
    else
        fail "override: missed" "expected $work/our-hooks, got '$got'"
    fi
    git_q -C "$main" config --unset core.hooksPath
}

# --- 5. a relative core.hooksPath resolves the way git resolves it -----------------------
# githooks(5): git chdirs to the top of the working tree before running a hook, so a relative
# value is relative to THAT — not to wherever the installer was invoked from.
case5() {
    local got expected sub
    git_q -C "$main" config core.hooksPath .githooks
    mkdir -p "$main/.githooks" "$main/deep/nested"

    expected="$(oracle_dir "$main")"
    got="$(git_hooks_override "$main")"
    if [ "$(abs_dir "$got")" = "$expected" ]; then
        ok "a relative core.hooksPath: agrees with git"
    else
        fail "relative: path" "got $(abs_dir "$got"), git runs $expected"
    fi

    # Run from a subdirectory: the answer must not move with the caller.
    sub="$(cd "$main/deep/nested" && git_hooks_override "$main")"
    if [ "$(abs_dir "$sub")" = "$expected" ]; then
        ok "a relative core.hooksPath: does not move with the caller's cwd"
    else
        fail "relative: cwd" "from a subdirectory it resolved to $(abs_dir "$sub")"
    fi
    git_q -C "$main" config --unset core.hooksPath
}

# --- 6. a hooks path inside the working tree is excluded locally -------------------------
# A relative `core.hooksPath` resolves inside the tree, so the chainer is untracked there:
# every `jkb task work` session then reads dirty and `jkb task land` refuses it, and deleting
# the file does not help because the next pull recreates it. `.git/info/exclude` is the
# local, unpushed write the project already sanctions for this (D36 does it for `.jkb/`).
case6() {
    local r="$work/intree" chainer got override
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    # Guard the premise before building a path out of it. This used to be
    # `chainer="$(git_hooks_override "$r")/post-merge"` unguarded, so a regression that made
    # the function return nothing turned the target into `/post-merge` — writing at the
    # filesystem root — and the case then carried on to print a vacuous `ok`.
    # Canonicalised on both sides: `--show-toplevel` reports the real path, and on macOS the
    # temp dir is reached through the /var -> /private/var symlink.
    override="$(git_hooks_override "$r")"
    if [ "$(abs_dir "$override")" != "$(abs_dir "$r")/.githooks" ]; then
        fail "exclude: premise" "git_hooks_override returned '$override', not <repo>/.githooks"
        return
    fi
    chainer="$override/post-merge"
    mkdir -p "$override"
    printf '#!/bin/sh\nexit 0\n' >"$chainer"

    if [ -z "$(git_q -C "$r" status --porcelain)" ]; then
        fail "exclude: premise" "the chainer did not make the tree dirty to begin with"
        return
    fi

    got="$(reconcile_exclude "$r" "$chainer" yes)"
    if [ "$got" = "added /.githooks/post-merge" ]; then
        ok "a chainer inside the working tree: excluded locally"
    else
        fail "exclude: pattern" "expected 'added /.githooks/post-merge', got '$got'"
    fi
    if [ -z "$(git_q -C "$r" status --porcelain)" ]; then
        ok "the working tree is clean again, so a session can land"
    else
        fail "exclude: dirty" "still dirty: $(git_q -C "$r" status --porcelain | tr '\n' ' ')"
    fi
    # Twice must not duplicate the line — setup.sh runs on every qualifying pull.
    got="$(reconcile_exclude "$r" "$chainer" yes)"
    if [ "$got" = "kept /.githooks/post-merge" ] \
        && [ "$(grep -c '^/\.githooks/post-merge$' "$r/.git/info/exclude")" = "1" ]; then
        ok "running it again adds nothing"
    else
        fail "exclude: idempotence" "second run printed '$got' and the pattern appears $(grep -c '^/\.githooks/post-merge$' "$r/.git/info/exclude") time(s)"
    fi

    # And the reverse decision retracts it, leaving the user's own rules alone. Adding the
    # rule on the run that installs a chainer and never revisiting it is how a user who later
    # replaced that chainer with their own hook had it git-ignored for ever.
    got="$(reconcile_exclude "$r" "$chainer" no)"
    if [ "$got" = "retracted /.githooks/post-merge" ] \
        && ! grep -qxF '/.githooks/post-merge' "$r/.git/info/exclude"; then
        ok "and asking for it to be gone retracts it"
    else
        fail "exclude: retract" "printed '$got'; file still: $(tr '\n' '|' <"$r/.git/info/exclude")"
    fi
    if [ -n "$(git_q -C "$r" status --porcelain)" ]; then
        ok "so the file it was hiding is visible to git again"
    else
        fail "exclude: still hidden" "the chainer is still invisible to git status"
    fi
    # The marker goes with the pattern: a retraction that left its own comment behind would
    # accumulate one per cycle.
    if ! grep -q '^# jkb:' "$r/.git/info/exclude"; then
        ok "and takes its marker comment with it"
    else
        fail "exclude: marker" "the marker survived: $(tr '\n' '|' <"$r/.git/info/exclude")"
    fi
}

# --- 6c. a pattern jkb cannot prove it wrote is reported, never deleted -------------------
# The one case retraction must NOT repair. A bare pattern with no marker above it may be a
# rule the user wrote themselves; deleting it to fix our own mess would destroy something
# they own. Ownership is byte identity, exactly as it is for the chainer body — so this is
# reported instead, which turns a silent permanent harm into a visible one with a remedy.
case6c() {
    local r="$work/unowned" chainer override got
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    override="$(git_hooks_override "$r")"
    if [ "$(abs_dir "$override")" != "$(abs_dir "$r")/.githooks" ]; then
        fail "unowned: premise" "git_hooks_override returned '$override'"
        return
    fi
    chainer="$override/post-merge"
    mkdir -p "$override"; printf '#!/bin/sh\nexit 0\n' >"$chainer"
    # Their rule, written by hand: the pattern with no marker line above it.
    printf '# things I do not want to see\n/.githooks/post-merge\n' >>"$r/.git/info/exclude"

    got="$(reconcile_exclude "$r" "$chainer" no)"
    if [ "$got" = "unowned /.githooks/post-merge" ]; then
        ok "an unmarked exclude rule is reported as unowned"
    else
        fail "unowned: state" "expected 'unowned /.githooks/post-merge', got '$got'"
    fi
    if grep -qxF '/.githooks/post-merge' "$r/.git/info/exclude"; then
        ok "and is left exactly where the user put it"
    else
        fail "unowned: deleted" "jkb deleted a rule it could not prove it wrote"
    fi
}

# --- 6b. an exclude file that does not end in a newline keeps its last rule ---------------
# The pass-4 must-fix. Appending without a separator fused the user's last rule with ours
# (`*.log` + `/.githooks/post-merge`), destroying a rule they own and cannot recover, while
# our own pattern still did not take effect — under a success message.
case6b() {
    local r="$work/nonewline" chainer override
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    override="$(git_hooks_override "$r")"
    if [ "$(abs_dir "$override")" != "$(abs_dir "$r")/.githooks" ]; then
        fail "nonewline: premise" "git_hooks_override returned '$override', not <repo>/.githooks"
        return
    fi
    chainer="$override/post-merge"
    mkdir -p "$override"; printf '#!/bin/sh\nexit 0\n' >"$chainer"
    # No trailing newline, exactly as a hand-edited file often is.
    printf '# my rules\n*.log' >"$r/.git/info/exclude"

    reconcile_exclude "$r" "$chainer" yes >/dev/null

    if [ "$(grep -c '^\*\.log$' "$r/.git/info/exclude")" = "1" ]; then
        ok "an exclude file with no trailing newline keeps its last rule"
    else
        fail "nonewline: eaten" "the user's rule became: $(tail -2 "$r/.git/info/exclude" | tr '\n' '|')"
    fi
    if [ "$(git_q -C "$r" check-ignore "$chainer" >/dev/null 2>&1; echo $?)" = "0" ]; then
        ok "and our own pattern actually takes effect"
    else
        fail "nonewline: inert" "git does not ignore the chainer: $(cat "$r/.git/info/exclude" | tr '\n' '|')"
    fi
}

# --- 7. a hooks path outside the working tree is left alone -------------------------------
# The ordinary case: an absolute core.hooksPath is nobody's working tree, so there is nothing
# to hide and nothing should be written to .git/info/exclude.
case7() {
    local r="$work/outside" got before after got_ok
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    # `git init` ships a commented default exclude file, so "unchanged" is the assertion,
    # not "empty".
    before="$(cat "$r/.git/info/exclude" 2>/dev/null)"
    got="$(reconcile_exclude "$r" "$work/elsewhere/post-merge" yes)"
    after="$(cat "$r/.git/info/exclude" 2>/dev/null)"
    case "$got" in
        "none (outside the working tree"*) got_ok=1 ;;
        *) got_ok=0 ;;
    esac
    if [ "$got_ok" = 1 ] && [ "$before" = "$after" ]; then
        ok "a hooks path outside the tree: nothing excluded"
    else
        fail "outside: wrote" "printed '$got'; exclude file changed=$([ "$before" = "$after" ] && echo no || echo YES)"
    fi
}

# --- 8. a `~user/` core.hooksPath expands the way git expands it -------------------------
# git expands `~user/` through passwd, not through $HOME. The hand-rolled `${v/#~/$HOME}`
# this replaced stripped the tilde and concatenated, producing `/Users/jagnewjagnew/hooks`
# for `~jagnew/hooks` — a directory git never looks in, which setup.sh then created and
# reported success for. The plain `~/` form is NOT a discriminator: both spellings get it
# right. Compared as strings, never resolved, so nothing outside the sandbox is touched.
case8() {
    local r="$work/tilde" user got expected
    user="$(id -un)" || { skip "could not determine the current user name"; return; }
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath "~$user/jkb-hooks-probe"

    expected="$(dirname "$(git -C "$r" rev-parse --git-path hooks/post-merge)")"
    got="$(git_hooks_override "$r")"
    if [ "$got" = "$expected" ]; then
        ok "a ~user/ core.hooksPath: expands the way git expands it"
    else
        fail "tilde: path" "got '$got', git uses '$expected'"
    fi
}

echo "==> scripts/lib.sh::git_hooks_dir + git_hooks_override + reconcile_exclude"
case1
case2
case3
case4
case5
case6
case6b
case6c
case7
case8

finish
