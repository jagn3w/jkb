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
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

work="$(new_workdir)"
isolate_git "$work/home"

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

# --- 1b. right bytes, wrong mode ----------------------------------------------------------
# `up-to-date` is the only arm that skips install_exec, so it is the only one where the mode
# is not set as a side effect. A restore from backup or an `rsync` without `-p` lands here at
# 644; git skips a non-executable hook, so reporting "up to date" would mean the repo hook
# silently never runs — the exact failure the chainer exists to prevent.
case1b() {
    local d="$work/mode" got
    mkdir -p "$d"
    chainer_body >"$d/post-merge"
    chmod 644 "$d/post-merge"
    got="$(install_chainer "$d/post-merge")"
    if [ "$got" = "up-to-date" ]; then
        fail "mode: outcome" "reported up-to-date for a chainer git will not execute"
    else
        ok "a non-executable chainer is not reported healthy"
    fi
    [ -x "$d/post-merge" ] \
        && ok "and it is made executable" \
        || fail "mode: still 644" "the chainer is still not executable after install_chainer"
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

# --- 7. install_git_hooks: the whole block setup.sh runs, in a worktree ------------------
# Pass 4's coverage gap: every test drove the helpers, nothing drove the block that calls
# them, so reverting the hooks directory to `--git-dir` left the entire gate green while the
# headline fix was undone. The oracle is git's own answer.
case7() {
    local d="$work/block" main wt out hook
    mkdir -p "$d"
    main="$d/main"
    git_q init -q "$main" >/dev/null 2>&1
    git_q -C "$main" commit -q --allow-empty -m init
    git_q -C "$main" worktree add -q "$d/wt" -b side >/dev/null 2>&1
    wt="$d/wt"
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"

    # Run it the way a session does: against the WORKTREE, which is where the bug lived.
    out="$(install_git_hooks "$wt" "$d/src")"
    hook="${out#repo-hook=}"; hook="${hook%%$'\n'*}"
    if [ "$hook" = "$(git -C "$wt" rev-parse --git-path hooks/post-merge)" ]; then
        ok "install_git_hooks puts the hook where git runs hooks from, from a worktree"
    else
        fail "block: path" "installed at '$hook', git runs '$(git -C "$wt" rev-parse --git-path hooks/post-merge)'"
    fi
    [ -x "$hook" ] && ok "and it is executable" || fail "block: mode" "$hook is not executable"

    # No core.hooksPath here, so it must report no chainer and exclude nothing.
    case "$out" in
        *chainer=*|*excluded=*) fail "block: extra" "reported a chainer with no core.hooksPath: $out" ;;
        *) ok "with no core.hooksPath it installs only the repo hook" ;;
    esac
}

# --- 8. install_git_hooks does not hide a chainer it does not own -------------------------
# Two adjacent statements used to take opposite positions: `foreign` says the file is not
# ours to touch, and the exclude ran anyway — hiding the user's own hook from `git status`
# and `git add -A` permanently.
case8() {
    local d="$work/foreignblock" repo out
    mkdir -p "$d"
    repo="$d/repo"
    git_q init -q "$repo" >/dev/null 2>&1
    git_q -C "$repo" commit -q --allow-empty -m init
    git_q -C "$repo" config core.hooksPath .githooks
    mkdir -p "$repo/.githooks"
    printf '#!/bin/sh\n# my own hook\n' >"$repo/.githooks/post-merge"
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"

    out="$(install_git_hooks "$repo" "$d/src")"
    case "$out" in
        *"chainer=foreign"*) ok "a foreign chainer is reported as foreign" ;;
        *) fail "foreign: outcome" "expected chainer=foreign, got: $out" ;;
    esac
    case "$out" in
        *excluded=*) fail "foreign: excluded" "jkb hid a chainer it had just declared not its own" ;;
        *) ok "and it is not added to .git/info/exclude" ;;
    esac
    grep -q 'my own hook' "$repo/.githooks/post-merge" \
        && ok "the user's hook is untouched" \
        || fail "foreign: clobbered" "the user's hook was overwritten"
}

# --- 9. install_git_hooks outside a git repo ---------------------------------------------
# setup.sh renders `error=` as a warning and carries on. Untested, that arm was a branch
# wearing the costume of a safeguard.
case9() {
    local d="$work/norepo" out status
    mkdir -p "$d/plain"
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"
    out="$(install_git_hooks "$d/plain" "$d/src" 2>/dev/null)"; status=$?
    if [ "$status" -ne 0 ]; then
        ok "outside a git repo it fails"
    else
        fail "norepo: status" "reported success outside a repo"
    fi
    case "$out" in
        error=*) ok "and says why, in the form setup.sh renders" ;;
        *) fail "norepo: output" "expected an error= line, got: '$out'" ;;
    esac
    [ -e "$d/plain/hooks" ] \
        && fail "norepo: wrote" "it created a hooks directory outside a repo" \
        || ok "and writes nothing"
}

echo "==> scripts/lib.sh::install_chainer"
case1
case1b
case2
case3
case4
case5
case6
case7
case8
case9

finish
