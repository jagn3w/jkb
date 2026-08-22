#!/usr/bin/env bash
# Regression test for scripts/lib.sh's global post-merge chainer: `install_chainer`'s four
# outcomes, and the installed chainer's actual dispatch.
#
# Why it exists: while the body and the three install arms were a heredoc inline in setup.sh,
# nothing could execute them. Reverting the dispatch line to `--git-dir` left the whole gate
# green while every `git pull` inside a worktree silently stopped running the repo hook —
# and check.sh and ci.yml both justify the shell-test stage on the claim that setup.sh's
# installs are not reachable from a Rust test. They were not reachable from anything.
#
# Case 4 is the must-fix from review pass 3: ownership used to be `grep -q` for a comment
# line, so a user's edits to jkb's own chainer still matched and the refresh arm replaced
# their file unattended, with no backup, on an ordinary `git pull`.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"

failures=0
ok()   { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n     %s\n' "$1" "$2"; failures=$((failures + 1)); }

work="$(mktemp -d)"
trap 'chmod -R u+rwx "$work" 2>/dev/null; rm -rf "$work"' EXIT

mkdir -p "$work/home"
export HOME="$work/home" GIT_CONFIG_NOSYSTEM=1
unset XDG_CONFIG_HOME GIT_DIR GIT_WORK_TREE
git_q() { git -c user.name=t -c user.email=t@example.com "$@"; }

# --- 1. install, then re-install ---------------------------------------------------------
case1() {
    local d="$work/fresh" got
    mkdir -p "$d"
    got="$(install_chainer "$d/post-merge")"
    if [ "$got" = "installed" ] && [ -x "$d/post-merge" ]; then
        ok "a fresh directory: installed"
    else
        fail "fresh: outcome" "got '$got', executable=$([ -x "$d/post-merge" ] && echo yes || echo no)"
    fi
    got="$(install_chainer "$d/post-merge")"
    [ "$got" = "up-to-date" ] \
        && ok "running it again: up-to-date" \
        || fail "fresh: idempotence" "second run reported '$got'"
}

# --- 2. a chainer from before the worktree fix is upgraded -------------------------------
case2() {
    local d="$work/legacy" got
    mkdir -p "$d"
    chainer_body_v1 >"$d/post-merge"
    chmod 755 "$d/post-merge"
    got="$(install_chainer "$d/post-merge")"
    if [ "$got" = "refreshed" ] && chainer_body | cmp -s - "$d/post-merge"; then
        ok "a known older body: refreshed to the current one"
    else
        fail "legacy: outcome" "got '$got'; body updated=$(chainer_body | cmp -s - "$d/post-merge" && echo yes || echo no)"
    fi
}

# --- 3. somebody else's file is never touched --------------------------------------------
case3() {
    local d="$work/stranger" got before
    mkdir -p "$d"
    printf '#!/bin/sh\n# my own hook\necho mine\n' >"$d/post-merge"
    before="$(cat "$d/post-merge")"
    got="$(install_chainer "$d/post-merge")"
    if [ "$got" = "foreign" ] && [ "$(cat "$d/post-merge")" = "$before" ]; then
        ok "a stranger's chainer: foreign, untouched"
    else
        fail "stranger: outcome" "got '$got'; file changed=$([ "$(cat "$d/post-merge")" = "$before" ] && echo no || echo YES)"
    fi
}

# --- 4. OUR chainer WITH THE USER'S EDITS IN IT is never touched either -------------------
# The must-fix. `grep -q 'Global post-merge chainer'` matched this file, because the comment
# it looked for is still there — so the refresh arm replaced it and printed success.
case4() {
    local d="$work/edited" got before
    mkdir -p "$d"
    { chainer_body | sed '$d'; printf 'direnv reload 2>/dev/null || true\nexit 0\n'; } >"$d/post-merge"
    chmod 755 "$d/post-merge"
    before="$(cat "$d/post-merge")"
    got="$(install_chainer "$d/post-merge")"
    if [ "$got" != "foreign" ]; then
        fail "edited: outcome" "our body plus a user's line reported '$got' — it is not ours to replace"
    elif [ "$(cat "$d/post-merge")" != "$before" ]; then
        fail "edited: destructive" "the user's edits were overwritten"
    else
        ok "our body with a user's edits: foreign, their line survives"
    fi
    grep -q 'direnv reload' "$d/post-merge" \
        || fail "edited: lost" "the user's line is gone from the file"
}

# --- 5. a destination that is a directory -------------------------------------------------
# A directory-style hook manager keeps `post-merge/` as a folder of scripts. `mv` would move
# our temp file inside it and report success.
case5() {
    local d="$work/dirdest" got
    mkdir -p "$d/post-merge"
    got="$(install_chainer "$d/post-merge" 2>/dev/null)"
    if [ "$got" = "foreign" ] && [ -z "$(ls -A "$d/post-merge")" ]; then
        ok "a directory where the chainer goes: foreign, nothing written into it"
    else
        fail "dirdest: outcome" "got '$got'; directory now holds: $(ls -A "$d/post-merge" | tr '\n' ' ')"
    fi
}

# --- 6. the installed chainer really dispatches, including from a worktree -----------------
# The dispatch line itself: `--git-dir` here is the pass-2 bug, and in a worktree it resolves
# to a directory that holds no hooks.
case6() {
    local d="$work/dispatch" hooks main wt out
    mkdir -p "$d"
    hooks="$d/globalhooks"; mkdir -p "$hooks"
    install_chainer "$hooks/post-merge" >/dev/null

    main="$d/main"
    git_q init -q "$main" >/dev/null 2>&1
    git_q -C "$main" commit -q --allow-empty -m init
    # The repo hook the chainer must find, in the directory git actually runs hooks from.
    printf '#!/bin/sh\necho REPO-HOOK-RAN\n' >"$(git_hooks_dir "$main")/post-merge"
    chmod 755 "$(git_hooks_dir "$main")/post-merge"

    out="$(cd "$main" && "$hooks/post-merge" 2>&1)"
    [ "$out" = "REPO-HOOK-RAN" ] \
        && ok "the installed chainer dispatches to the repo hook" \
        || fail "dispatch: checkout" "expected REPO-HOOK-RAN, got '$out'"

    wt="$d/wt"
    git_q -C "$main" worktree add -q "$wt" -b side >/dev/null 2>&1
    # git runs a hook from the top of the working tree, so that is where the chainer runs.
    out="$(cd "$wt" && "$hooks/post-merge" 2>&1)"
    [ "$out" = "REPO-HOOK-RAN" ] \
        && ok "the installed chainer dispatches from inside a worktree" \
        || fail "dispatch: worktree" "expected REPO-HOOK-RAN, got '$out'"
}

echo "==> scripts/lib.sh::install_chainer"
case1
case2
case3
case4
case5
case6

if [ "$failures" -ne 0 ]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
