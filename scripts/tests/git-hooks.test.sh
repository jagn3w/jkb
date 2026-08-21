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

failures=0
ok()   { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n     %s\n' "$1" "$2"; failures=$((failures + 1)); }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# `--git-path hooks/…` honours core.hooksPath, and this developer has one set globally — so
# without an isolated config the oracle answers with the user's own hooks directory and the
# suite reddens over a regression that does not exist. HOME + GIT_CONFIG_NOSYSTEM works on
# every git version (GIT_CONFIG_GLOBAL needs 2.32); XDG_CONFIG_HOME has to go too, because
# git prefers `$XDG_CONFIG_HOME/git/config` over `$HOME/.gitconfig` and overriding only HOME
# leaves the real global config visible. GIT_DIR/GIT_WORK_TREE would point every command in
# here at somebody else's repository.
mkdir -p "$work/home"
export HOME="$work/home" GIT_CONFIG_NOSYSTEM=1
unset XDG_CONFIG_HOME GIT_DIR GIT_WORK_TREE
git_q() { git -c user.name=t -c user.email=t@example.com "$@"; }

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

echo "==> scripts/lib.sh::git_hooks_dir + git_hooks_override"
case1
case2
case3
case4
case5

if [ "$failures" -ne 0 ]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
