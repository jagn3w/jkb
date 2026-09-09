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

    got="$(reconcile_exclude "$r" "/.githooks/post-merge" yes)"
    if [ "$got" = "exclude=added /.githooks/post-merge
exclude-file=changed" ]; then
        ok "a chainer inside the working tree: excluded locally"
    else
        fail "exclude: pattern" "expected 'exclude=added /.githooks/post-merge', got '$got'"
    fi
    if [ -z "$(git_q -C "$r" status --porcelain)" ]; then
        ok "the working tree is clean again, so a session can land"
    else
        fail "exclude: dirty" "still dirty: $(git_q -C "$r" status --porcelain | tr '\n' ' ')"
    fi
    # Twice must not duplicate the line — setup.sh runs on every qualifying pull.
    got="$(reconcile_exclude "$r" "/.githooks/post-merge" yes)"
    if [ "$got" = "exclude=kept /.githooks/post-merge
exclude-file=unchanged" ] \
        && [ "$(grep -c '^/\.githooks/post-merge$' "$r/.git/info/exclude")" = "1" ]; then
        ok "running it again adds nothing"
    else
        fail "exclude: idempotence" "second run printed '$got' and the pattern appears $(grep -c '^/\.githooks/post-merge$' "$r/.git/info/exclude") time(s)"
    fi

    # And the reverse decision retracts it, leaving the user's own rules alone. Adding the
    # rule on the run that installs a chainer and never revisiting it is how a user who later
    # replaced that chainer with their own hook had it git-ignored for ever.
    got="$(reconcile_exclude "$r" "/.githooks/post-merge" no)"
    if [ "$got" = "exclude=retracted /.githooks/post-merge
exclude-file=changed" ] \
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

    got="$(reconcile_exclude "$r" "/.githooks/post-merge" no)"
    if [ "$got" = "exclude=unowned /.githooks/post-merge
exclude-file=unchanged" ]; then
        ok "an unmarked exclude rule is reported as unowned"
    else
        fail "unowned: state" "expected 'exclude=unowned /.githooks/post-merge', got '$got'"
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

    reconcile_exclude "$r" "/.githooks/post-merge" yes >/dev/null

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

# --- 6d. a repository with no working tree ------------------------------------------------
# `reconcile_exclude` asks for the toplevel first, and a bare repo has none. Without a case
# the arm is unreachable-in-practice code that reads as a safeguard; with one it is a stated
# answer. Nothing can be hidden from a `git status` that cannot be run.
case6d() {
    local r="$work/bare.git" wt got_bare got_wt
    git_q init -q --bare "$r" >/dev/null 2>&1

    # A bare repo with no worktrees at all. An ABSOLUTE hooksPath has no checkout to be
    # relative to, so there is nothing to anchor and nothing to hide.
    git_q -C "$r" config core.hooksPath "$r/myhooks"
    got_bare="$(git_hooks_exclude_pattern "$r" 2>/dev/null)"
    case "$got_bare" in
        "none ("*) ok "a bare repository, absolute hooksPath: nothing to exclude" ;;
        *) fail "bare: absolute" "expected a none state, got '$got_bare'" ;;
    esac

    # A RELATIVE one still yields its pattern — the case a top-of-function bare gate broke.
    # It resolves inside each linked worktree exactly as anywhere else, so returning `none`
    # dropped a working exclusion AND made jkb retract the block it had written itself.
    # Inert when there are no worktrees; correct the moment there is one.
    git_q -C "$r" config core.hooksPath .githooks
    got_bare="$(git_hooks_exclude_pattern "$r" 2>/dev/null)"
    [ "$got_bare" = "pattern /.githooks/post-merge" ] \
        && ok "and a relative one still yields its pattern" \
        || fail "bare: relative" "got '$got_bare'"

    # Now with a worktree, which is where it matters: the pattern must survive, and must be
    # the same answer from the worktree and from the bare dir.
    git_q -C "$work" init -q "$work/bare-seed" >/dev/null 2>&1
    git_q -C "$work/bare-seed" commit -q --allow-empty -m init
    git_q -C "$work/bare-seed" push -q "$r" HEAD:refs/heads/main 2>/dev/null
    wt="$work/bare-wt"
    if ! git_q -C "$r" worktree add -q "$wt" main >/dev/null 2>&1; then
        skip "could not add a worktree to the bare repo"
        return
    fi
    got_wt="$(git_hooks_exclude_pattern "$wt" 2>/dev/null)"
    got_bare="$(git_hooks_exclude_pattern "$r" 2>/dev/null)"
    if [ "$got_wt" = "pattern /.githooks/post-merge" ] && [ "$got_wt" = "$got_bare" ]; then
        ok "a bare repo WITH a worktree keeps the relative exclusion, from either end"
    else
        fail "bare: worktree relative" "worktree='$got_wt' bare='$got_bare'"
    fi
    # And it really hides the chainer in that worktree.
    mkdir -p "$wt/.githooks"; printf '#!/bin/sh\nexit 0\n' >"$wt/.githooks/post-merge"
    reconcile_exclude "$wt" "/.githooks/post-merge" yes >/dev/null
    [ -z "$(git_q -C "$wt" status --porcelain)" ] \
        && ok "and the worktree is clean afterwards" \
        || fail "bare: dirty" "$(git_q -C "$wt" status --porcelain | tr '\n' ' ')"

    # An absolute path inside the BARE DIR is not inside any working tree — `worktree list`
    # reports the bare dir as a record, and naming it as a tree told the user their chainer
    # was inside one that does not exist.
    git_q -C "$r" config core.hooksPath "$r/myhooks"
    got_wt="$(git_hooks_exclude_pattern "$wt" 2>/dev/null)"
    case "$got_wt" in
        "none (core.hooksPath is outside every working tree"*)
            ok "an absolute path inside the bare dir is in no working tree" ;;
        *) fail "bare: absolute in bare dir" "got '$got_wt'" ;;
    esac
}

# --- 6p. an exported GIT_WORK_TREE does not redirect jkb into another repository ----------
# `GIT_DIR`/`GIT_WORK_TREE`/`GIT_COMMON_DIR` outrank `-C`, so with `GIT_WORK_TREE` exported —
# the standard bare-dotfiles shell recipe — `rev-parse --show-toplevel` answered somebody
# else's tree and `install_git_hooks` created `.githooks/` INSIDE that unrelated repository,
# reporting `dispatch=chained`, while the repo it was asked about kept a dead hook. jkb runs
# inside other people's repositories and must not decorate them.
#
# The harness unsets both variables for every other case, which is why no fixture could see
# this: it has to export them on purpose.
case6p() {
    local d="$work/envleak" out
    mkdir -p "$d"
    git_q init -q "$d/mine" >/dev/null 2>&1
    git_q -C "$d/mine" commit -q --allow-empty -m init
    git_q -C "$d/mine" config core.hooksPath .githooks
    git_q init -q "$d/theirs" >/dev/null 2>&1
    git_q -C "$d/theirs" commit -q --allow-empty -m init
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"

    out="$(GIT_WORK_TREE="$d/theirs" install_git_hooks "$d/mine" "$d/src" 2>/dev/null)"

    case "$out" in
        *"chainer=installed $d/mine/.githooks/post-merge"*)
            ok "an exported GIT_WORK_TREE does not move the chainer" ;;
        *) fail "envleak: chainer" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    [ -z "$(ls -A "$d/theirs" | grep -v '^\.git$')" ] \
        && ok "and nothing is created in the unrelated repository" \
        || fail "envleak: polluted" "wrote: $(ls -A "$d/theirs" | grep -v '^\.git$' | tr '\n' ' ')"
    # And the same for GIT_DIR, which splices another repo's config onto this one's tree.
    out="$(GIT_DIR="$d/theirs/.git" install_git_hooks "$d/mine" "$d/src" 2>/dev/null)"
    case "$out" in
        *"$d/mine/.git/hooks/post-merge"*) ok "and an exported GIT_DIR does not move the repo hook" ;;
        *) fail "envleak: gitdir" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
}

# --- 6n. the override agrees with git's own answer, in every layout ------------------------
# The oracle, not a snapshot. Two rounds asserted opposite rules for a relative `core.hooksPath`
# in a repository with no working tree, and BOTH were written into comments before either was
# measured properly. What git actually does — measured from three directories on 2.51.1 — is
# resolve it against the INVOKING PROCESS'S cwd, so there is no one place at all and jkb
# refuses (rc 4, `dispatch=unanchored`). The earlier claim that it resolves against the git dir
# came from a measurement taken with the cwd SET TO the git dir, which cannot tell the two
# apart. Asking git in each layout is what makes that unrepeatable — including the layouts
# nobody had measured: a `.git` gitfile, and a repo whose worktree has been removed.
case6n() {
    local d="$work/oracle" dir ours theirs bad="" probe
    mkdir -p "$d" "$d/probe-cwd"
    probe="$d/probe-cwd"                       # a cwd that is none of the repos below

    git_q init -q "$d/norm" >/dev/null 2>&1
    git_q -C "$d/norm" commit -q --allow-empty -m init
    git_q -C "$d/norm" config core.hooksPath .githooks
    git_q -C "$d/norm" worktree add -q "$d/wt" -b side >/dev/null 2>&1
    # repo_root, the git dir and the cwd are three different directories here. Without one
    # such fixture the oracle cannot fail: with repo_root == git dir == cwd, a mutant that
    # ignores git and resolves against `$repo_root` agrees with it everywhere.
    git_q init -q --separate-git-dir="$d/gitdir" "$d/sep" >/dev/null 2>&1
    git_q -C "$d/sep" commit -q --allow-empty -m init
    git_q -C "$d/sep" config core.hooksPath .githooks

    # Only repos WITH a working tree are oracled: for a relative value git anchors on the
    # invoking process's cwd, so with no working tree there is no fixed answer to compare to
    # (see the refusal asserted below).
    for dir in "$d/norm" "$d/wt" "$d/sep"; do
        ours="$(cd "$probe" && git_hooks_override "$dir" 2>/dev/null)/post-merge"
        theirs="$(cd "$dir" && git rev-parse --path-format=absolute --git-path hooks/post-merge 2>/dev/null)"
        [ "$ours" = "$theirs" ] || bad="$bad [$dir ours=$ours git=$theirs]"
    done
    rm -rf "$d/wt"                              # a worktree removed behind git's back
    ours="$(cd "$probe" && git_hooks_override "$d/norm" 2>/dev/null)/post-merge"
    theirs="$(cd "$d/norm" && git rev-parse --path-format=absolute --git-path hooks/post-merge 2>/dev/null)"
    [ "$ours" = "$theirs" ] || bad="$bad [pruned ours=$ours git=$theirs]"

    [ -z "$bad" ] \
        && ok "core.hooksPath resolves the way git resolves it, asked from an unrelated cwd" \
        || fail "oracle" "disagreed:$bad"

    # And the case git has no fixed answer for. Measured on git 2.51.1 from three cwds: with
    # no working tree a relative value resolves against the invoking process's directory, so
    # `git --git-dir=B rev-parse --git-path hooks/post-merge` answers `<cwd>/.githooks/…` and
    # `git hook run` executes whatever copy is under that cwd. A round called the git dir
    # "git's rule" here, on a measurement taken with the cwd set to the git dir — which cannot
    # tell the two apart. There is nothing to resolve to, so jkb refuses rather than guessing.
    git_q init -q --bare "$d/bare.git" >/dev/null 2>&1
    git_q -C "$d/bare.git" config core.hooksPath .githooks
    ( cd "$probe" && git_hooks_override "$d/bare.git" >/dev/null 2>&1 )
    # Captured, because `[` consumes `$?`: the failure diagnostic said "got 1" whatever the
    # subshell returned, pointing away from both the real status and the guessed path.
    local rc=$?
    [ "$rc" -eq 4 ] \
        && ok "and a relative one with no working tree is refused, not guessed at" \
        || fail "oracle: bare" "expected rc 4 (unanchored), got $rc"
    ours="$(cd "$d/bare.git" && git rev-parse --path-format=absolute --git-path hooks/post-merge)"
    theirs="$(cd "$probe" && git --git-dir="$d/bare.git" rev-parse --path-format=absolute --git-path hooks/post-merge)"
    [ "$ours" != "$theirs" ] \
        && ok "because git's own answer there moves with the caller's directory" \
        || fail "oracle: premise" "git gave one answer from two cwds: $ours"
}

# --- 6g. the desired state is the same from every worktree -------------------------------
# THE regression guard for the sweep. `.git/info/exclude` lives in the common dir and applies
# to every worktree at once, so a desired state computed from `--show-toplevel` differs per
# run — and a sweep that enforces it flip-flops: a main-checkout run added the block, the next
# run from a `jkb task work` session retracted it, and the main checkout read dirty in
# between, which is the state `jkb task land` refuses. D36 makes that the normal case.
#
# So the pattern comes from the raw config string and the shared facts. Asserting the two
# answers are byte-identical is what makes that structural rather than remembered: reverting
# the helper to resolve against the current tree fails this immediately.
case6g() {
    local m="$work/shared" wt got_main got_wt
    git_q init -q "$m" >/dev/null 2>&1
    git_q -C "$m" commit -q --allow-empty -m init
    wt="$work/shared-wt"
    git_q -C "$m" worktree add -q "$wt" -b side >/dev/null 2>&1

    # Relative: git resolves it inside EACH worktree's top, so one anchored pattern is right
    # for all of them at once.
    git_q -C "$m" config core.hooksPath .githooks
    got_main="$(git_hooks_exclude_pattern "$m")"
    got_wt="$(git_hooks_exclude_pattern "$wt")"
    if [ "$got_main" = "pattern /.githooks/post-merge" ] && [ "$got_wt" = "$got_main" ]; then
        ok "a relative core.hooksPath: the same pattern from the checkout and the worktree"
    else
        fail "shared: relative" "main='$got_main' worktree='$got_wt'"
    fi

    # Absolute, inside the main checkout: still one answer, and it is the main tree's.
    git_q -C "$m" config core.hooksPath "$m/.githooks"
    got_main="$(git_hooks_exclude_pattern "$m")"
    got_wt="$(git_hooks_exclude_pattern "$wt")"
    if [ "$got_main" = "pattern /.githooks/post-merge" ] && [ "$got_wt" = "$got_main" ]; then
        ok "an absolute one inside the checkout: the same answer from both"
    else
        fail "shared: absolute" "main='$got_main' worktree='$got_wt'"
    fi

    # Absolute, inside a LINKED worktree only: not hidden, and the reason says why — an
    # anchored rule would apply to every tree, including ones the path is not in.
    git_q -C "$m" config core.hooksPath "$wt/.githooks"
    got_main="$(git_hooks_exclude_pattern "$m")"
    got_wt="$(git_hooks_exclude_pattern "$wt")"
    # `exposed`, not `none`: the chainer really is an untracked file in a real tree. An
    # anchored rule applies to every worktree at once so hiding it is not available — but
    # `none` is rendered silently, which left that tree dirty for ever with nothing
    # attributing the file to jkb.
    case "$got_main" in
        "exposed ("*) [ "$got_wt" = "$got_main" ] \
            && ok "one inside a linked worktree only: reported exposed, same answer from both" \
            || fail "shared: linked" "main='$got_main' worktree='$got_wt'" ;;
        *) fail "shared: linked" "expected an exposed state, got '$got_main'" ;;
    esac
    case "$got_main" in
        *"$wt"*) ok "and it names the tree that will read dirty" ;;
        *) fail "shared: linked detail" "the reason does not name the worktree: $got_main" ;;
    esac

    # And end to end: the block a main run adds must survive a worktree run.
    git_q -C "$m" config core.hooksPath "$m/.githooks"
    printf '#!/bin/sh\necho HOOK\n' >"$work/shared-src"
    install_git_hooks "$m" "$work/shared-src" >/dev/null 2>&1
    install_git_hooks "$wt" "$work/shared-src" >/dev/null 2>&1
    if grep -qxF '/.githooks/post-merge' "$m/.git/info/exclude"; then
        ok "and a run from the worktree does not retract what the checkout added"
    else
        fail "shared: flip" "the worktree run removed the block: $(tr '\n' '|' <"$m/.git/info/exclude")"
    fi
    [ -z "$(git_q -C "$m" status --porcelain)" ] \
        && ok "so the main checkout is not left dirty by a session's pull" \
        || fail "shared: dirty" "$(git_q -C "$m" status --porcelain | tr '\n' ' ')"
}

# --- 6e. an exclude file git can read, we can read -----------------------------------------
# git trims one trailing CR from every ignore/exclude line, so a CRLF file is functional to
# git. Ours did not, so on such a file it recognised neither its own block nor the pattern,
# and appended a fresh one on every qualifying pull — unbounded growth in a file of the
# user's rules. Also pins that the surviving lines keep their original endings: agreeing with
# git about what a line MEANS is not licence to rewrite how it is spelled.
case6e() {
    local r="$work/crlf" chainer override got
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    override="$(git_hooks_override "$r")"
    chainer="$override/post-merge"
    mkdir -p "$override"; printf '#!/bin/sh\nexit 0\n' >"$chainer"
    # OUR OWN BLOCK, written with CRLF endings — as a Windows editor would leave it after any
    # visit to the file. Planting CRLF only on the user's lines does not exercise this: the
    # block we then append has LF endings and matches on the next run either way, so the test
    # passed with the trim reverted.
    printf '# my rules\r\n*.log\r\n%s\r\n/.githooks/post-merge\r\n' "$(exclude_marker)" \
        >"$r/.git/info/exclude"

    got="$(reconcile_exclude "$r" "/.githooks/post-merge" yes)"
    if [ "$got" = "exclude=kept /.githooks/post-merge
exclude-file=unchanged" ] \
        && [ "$(grep -c 'githooks/post-merge' "$r/.git/info/exclude")" = "1" ]; then
        ok "a CRLF exclude file: our own block is recognised, not appended a second time"
    else
        fail "crlf: duplicate" "said '$got'; file: $(tr '\n' '|' <"$r/.git/info/exclude")"
    fi
    if [ "$(head -1 "$r/.git/info/exclude" | od -c | grep -c '\\r')" = "1" ]; then
        ok "and the user's own line endings are left alone"
    else
        fail "crlf: rewritten" "the file's existing CRLF endings were changed"
    fi
    got="$(reconcile_exclude "$r" "/.githooks/post-merge" no)"
    if [ "$got" = "exclude=retracted /.githooks/post-merge
exclude-file=changed" ] \
        && ! grep -q 'githooks/post-merge' "$r/.git/info/exclude"; then
        ok "and a CRLF block is retracted, marker and all"
    else
        fail "crlf: retract" "said '$got'; file: $(tr '\n' '|' <"$r/.git/info/exclude")"
    fi
    [ "$(grep -c '^\*\.log' "$r/.git/info/exclude")" = "1" ] \
        && ok "and the user's rules survive the rewrite" \
        || fail "crlf: lost" "file: $(tr '\n' '|' <"$r/.git/info/exclude")"
}

# --- 6m. an answer git refused to give must not sweep --------------------------------------
# `none` means PROVEN absence and the caller turns it into want=no, so spelling "git could not
# answer" that way swept jkb's own block away and reported "jkb no longer stands behind hiding
# it" — false; jkb could not check. Both failing inputs run unattended from the post-merge
# hook, after which every worktree reads dirty until the next good setup.sh run.
case6m() {
    local r="$work/undecided" ex got
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    mkdir -p "$r/.githooks"; printf '#!/bin/sh\nexit 0\n' >"$r/.githooks/post-merge"
    ex="$r/.git/info/exclude"
    reconcile_exclude "$r" "/.githooks/post-merge" yes >/dev/null

    # EVERY arm of the helper that means "git would not answer" must use the one word. Only
    # one of the two was changed when this was fixed, and the other — an unlistable worktree
    # set — kept saying `none`, which the caller collapses to want=no and which therefore
    # retracts jkb's own block. The message on that line already said "could not be listed"
    # while the answer claimed proven absence.
    # BOTH arms, because a list of one is not a register of arms — the next person adding a
    # fifth `none`/`undecided` arm reads this as the place they are enumerated, adds nothing,
    # and the suite stays green.
    local arm got bad="" checked=0
    mkdir -p "$work/not-a-repo"                       # worktree list cannot answer
    local unresolvable="$work/unresolvable"           # config --get cannot answer
    git_q init -q "$unresolvable" >/dev/null 2>&1
    git_q -C "$unresolvable" commit -q --allow-empty -m init
    git_q -C "$unresolvable" config core.hooksPath "~jkb-no-such-account-$$/hooks"
    for arm in "$work/not-a-repo" "$unresolvable"; do
        # A git that resolves `~nosuchuser` without failing cannot exercise the second arm.
        if [ "$arm" = "$unresolvable" ] \
            && git -C "$arm" config --get --path core.hooksPath >/dev/null 2>&1; then
            skip "this git expands ~jkb-no-such-account-$$ without failing"
            continue
        fi
        got="$(git_hooks_exclude_pattern "$arm" 2>/dev/null)"
        checked=$((checked + 1))
        # The arm's OWN reason, not just the `undecided ` prefix: matching the prefix alone
        # means a fixture that quietly stops being what it was — a repo that failed to init,
        # say — answers through the other arm and the case still counts it.
        case "$arm:$got" in
            "$work/not-a-repo:undecided (the repository"*) ;;
            "$unresolvable:undecided (core.hooksPath could not be read)"*) ;;
            *) bad="$bad [$arm -> '$got']" ;;
        esac
    done
    # The count is in the message: one arm can legitimately skip (a git that resolves
    # `~nosuchuser` without failing), and "every arm" read as a claim about all of them while
    # the loop might have checked one. A loop whose arms can all skip reports success having
    # checked nothing.
    [ -z "$bad" ] && [ "$checked" -gt 0 ] \
        && ok "$checked arm(s) that mean git would not answer say undecided, not proven absence" \
        || fail "undecided: arms" "checked=$checked wrong:$bad"

    # `unknown` — the DERIVATION could not answer — leaves every block exactly as it was.
    # Distinct from `undecided`, which says only that THIS pattern is undecided and must still
    # sweep the others; sharing one word stranded a stale block permanently.
    got="$(reconcile_exclude "$r" "" unknown "undecided (core.hooksPath could not be read)")"
    case "$got" in
        "exclude=undecided ("*) ok "an unobtainable answer is reported undecided" ;;
        *) fail "undecided: state" "got '$got'" ;;
    esac
    grep -qxF '/.githooks/post-merge' "$ex" \
        && ok "and jkb's own block is left exactly where it was" \
        || fail "undecided: swept" "the block was retracted: $(tr '\n' '|' <"$ex")"
    case "$got" in
        *retracted*) fail "undecided: retracted" "it reported a retraction it did not make" ;;
        *) ok "and nothing is reported as retracted" ;;
    esac
    # The renderer must say it, not stay silent the way `none` does.
    case "$(printf '%s\n' "$got" | render_git_hooks_report 2>&1)" in
        *"could not work out what to hide"*) ok "and the rendering says so" ;;
        *) fail "undecided: render" "$(printf '%s\n' "$got" | render_git_hooks_report 2>&1 | tr '\n' '|')" ;;
    esac

    # And the sibling word is NOT that. `undecided` must sweep the OTHER blocks while leaving
    # its own — driven with a NON-EMPTY pattern, which is the only shape `install_git_hooks`
    # can actually emit: on an empty pattern the funnel collapses `want` to `no`, and
    # `keep=""` makes `undecided` byte-identical to `no`, so an empty-pattern fixture drives
    # an unreachable state and pins neither half.
    printf '%s\n/.stale/post-merge\n' "$(exclude_marker)" >>"$ex"
    got="$(reconcile_exclude "$r" "/.githooks/post-merge" undecided)"
    case "$got" in
        *"exclude=retracted /.stale/post-merge"*) ok "while undecided still sweeps the others" ;;
        *) fail "undecided: sibling" "got: $(printf '%s' "$got" | tr '\n' '|')" ;;
    esac
    grep -qxF '/.githooks/post-merge' "$ex" \
        && ok "and leaves its own block exactly where it was" \
        || fail "undecided: own block" "file: $(tr '\n' '|' <"$ex")"
    case "$got" in
        *"retracted /.githooks/post-merge"*)
            fail "undecided: own retracted" "it retracted the block it was undecided about" ;;
        *) ok "and reports no retraction for it" ;;
    esac
}

# --- 6k. every spelling of core.hooksPath normalizes to what git actually matches ---------
# A pair of prefix strips left `.`, `a/.`, `a//b`, `.//x` and `hooks/./x` as
# `/./post-merge`, `/a/./post-merge`, `/a//b/post-merge` … — patterns git does not match. It
# destroys nothing, so nothing failed loudly: the chainer simply stayed visible and the tree
# read dirty for ever, which is the exact failure the exclusion exists to prevent.
case6k() {
    local r="$work/spellings" entry v want got ok_all=1 bad=""
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    local -a table=(
        '.githooks|pattern /.githooks/post-merge'
        './githooks|pattern /githooks/post-merge'
        'githooks/|pattern /githooks/post-merge'
        '.|pattern /post-merge'
        './|pattern /post-merge'
        'a//b|pattern /a/b/post-merge'
        'a/.|pattern /a/post-merge'
        './/.githooks|pattern /.githooks/post-merge'
        'hooks/./x|pattern /hooks/x/post-merge'
        '..|none'
        '../x|none'
        'x/../y|pattern /y/post-merge'
        'a/b/../c|pattern /a/c/post-merge'
    )
    for entry in "${table[@]}"; do
        v="${entry%%|*}"; want="${entry#*|}"
        git_q -C "$r" config core.hooksPath "$v"
        got="$(git_hooks_exclude_pattern "$r")"
        case "$want" in
            none) case "$got" in "none ("*) ;; *) ok_all=0; bad="$bad [$v -> '$got']" ;; esac ;;
            *) [ "$got" = "$want" ] || { ok_all=0; bad="$bad [$v -> '$got' want '$want']"; } ;;
        esac
    done
    [ "$ok_all" = 1 ] \
        && ok "every core.hooksPath spelling normalizes to one git would match" \
        || fail "spellings" "wrong:$bad"

    # An absolute value naming the tree root itself: the same answer, by the other branch.
    git_q -C "$r" config core.hooksPath "$(cd "$r" && pwd -P)"
    got="$(git_hooks_exclude_pattern "$r")"
    [ "$got" = "pattern /post-merge" ] \
        && ok "and an absolute path naming the tree root is the tree root, not itself" \
        || fail "spellings: root" "got '$got'"
}

# --- 6h. an orphaned marker is tidied, never paired with the next marker ------------------
# The sweep's own must-fix. It paired a marker with the line after it without checking that
# line was not itself a marker: the user deletes only the pattern line, leaving the comment;
# the orphan is copied through and a fresh block appended below, giving marker/marker/pattern;
# the next run reads the pair as a block whose "pattern" is the marker's own text, retracts
# BOTH markers, prints `retracted # jkb: …`, and leaves the pattern bare — which jkb then
# reports `unowned` and refuses to touch for ever. Reproduced before the fix.
case6h() {
    local r="$work/orphan" got ex
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    mkdir -p "$r/.githooks"; printf '#!/bin/sh\nexit 0\n' >"$r/.githooks/post-merge"
    ex="$r/.git/info/exclude"
    # The exact shape the old parser produced for itself: an orphan, then a real block.
    printf '# my rules\n*.log\n%s\n%s\n/.githooks/post-merge\n' \
        "$(exclude_marker)" "$(exclude_marker)" >"$ex"

    got="$(reconcile_exclude "$r" "/.githooks/post-merge" yes)"
    case "$got" in
        *"exclude=tidied"*) ok "an orphaned jkb marker is tidied away" ;;
        *) fail "orphan: tidied" "got: $(printf '%s' "$got" | tr '\n' '|')" ;;
    esac
    case "$got" in
        *"retracted # jkb"*) fail "orphan: paired" "it read a marker as a pattern and retracted it" ;;
        *) ok "and never reported as a pattern" ;;
    esac
    [ "$(grep -c '^# jkb:' "$ex")" = "1" ] \
        && ok "exactly one marker survives" \
        || fail "orphan: count" "$(grep -c '^# jkb:' "$ex") markers: $(tr '\n' '|' <"$ex")"
    # And it is directly above the pattern — a bare pattern is the harm, not a tidy file.
    if [ "$(grep -A1 '^# jkb:' "$ex" | tail -1)" = "/.githooks/post-merge" ]; then
        ok "and it still heads the block, so the pattern is not left bare"
    else
        fail "orphan: bare" "file: $(tr '\n' '|' <"$ex")"
    fi
    [ "$(grep -c '^\*\.log$' "$ex")" = "1" ] \
        && ok "and the user's rules are untouched" \
        || fail "orphan: user" "file: $(tr '\n' '|' <"$ex")"
}

# --- 6j. a duplicate of the block we are keeping is deduplicated, not retracted ------------
# Reporting it as a retraction printed `retracted P` and then `kept P` — two contradictory
# lines about one pattern — and a substring assertion looking for both could be satisfied by
# the wrong lifecycle entirely.
case6j() {
    local r="$work/dupes" ex got
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    ex="$r/.git/info/exclude"
    printf '%s\n/.githooks/post-merge\n%s\n/.githooks/post-merge\n' \
        "$(exclude_marker)" "$(exclude_marker)" >>"$ex"

    got="$(reconcile_exclude "$r" "/.githooks/post-merge" yes)"
    case "$got" in
        *"exclude=deduplicated /.githooks/post-merge"*) ok "a duplicate block is deduplicated" ;;
        *) fail "dupes: state" "got: $(printf '%s' "$got" | tr '\n' '|')" ;;
    esac
    case "$got" in
        *"exclude=retracted"*) fail "dupes: retracted" "a copy of the kept block was called a retraction" ;;
        *) ok "and not called a retraction" ;;
    esac
    case "$got" in
        *"exclude=kept /.githooks/post-merge"*) ok "and the pattern is still kept" ;;
        *) fail "dupes: kept" "got: $(printf '%s' "$got" | tr '\n' '|')" ;;
    esac
    [ "$(grep -c '^/\.githooks/post-merge$' "$ex")" = "1" ] \
        && ok "exactly one copy survives" \
        || fail "dupes: count" "$(grep -c '^/\.githooks/post-merge$' "$ex") copies"
}

# --- 6i. jkb sweeps only its OWN markers ---------------------------------------------------
# `session::ensure_excluded` writes a different marked block into this same file for `/.jkb/`
# from Rust. It survives only because its marker is not in `exclude_known_markers`, which
# until now was a fact enforced by nobody.
case6i() {
    local r="$work/twowriters" ex
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    ex="$r/.git/info/exclude"
    printf '# jkb task sessions (git worktrees)\n/.jkb/\n' >>"$ex"
    # want=no with no pattern: the widest sweep there is.
    reconcile_exclude "$r" "" no "none (no core.hooksPath)" >/dev/null
    if grep -qxF '/.jkb/' "$ex" && grep -qxF '# jkb task sessions (git worktrees)' "$ex"; then
        ok "the sessions block another jkb writer owns survives the widest sweep"
    else
        fail "twowriters: eaten" "file: $(tr '\n' '|' <"$ex")"
    fi
}

# --- 6f. a working tree whose path contains glob metacharacters ---------------------------
# The pattern is derived by matching the chainer against the toplevel in a `case`, and a `[`
# or `*` in the path would be a glob if the operand were unquoted — silently taking the
# "outside the working tree" arm and excluding nothing, in a repo that reads dirty for ever.
case6f() {
    local r="$work/od[d]*name" chainer override got
    mkdir -p "$r"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init

    # The remaining unquoted-glob exposure is `case "$configured/" in "$main_top"/*)`, on the
    # ABSOLUTE branch of git_hooks_exclude_pattern — so that is what this must drive. It used
    # to hand `reconcile_exclude` a literal pattern and use a relative core.hooksPath, which
    # reaches neither the derivation nor that branch: the guard named an exposure it could no
    # longer fail on.
    git_q -C "$r" config core.hooksPath "$(cd "$r" && pwd -P)/.githooks"
    got="$(git_hooks_exclude_pattern "$r")"
    [ "$got" = "pattern /.githooks/post-merge" ] \
        && ok "a repo path with glob metacharacters: the absolute branch still matches it" \
        || fail "glob: derive" "got '$got'"

    # And end to end, so the exclusion really lands in a tree named like that.
    git_q -C "$r" config core.hooksPath .githooks
    override="$(git_hooks_override "$r")"
    chainer="$override/post-merge"
    mkdir -p "$override"; printf '#!/bin/sh\nexit 0\n' >"$chainer"
    got="$(reconcile_exclude "$r" "/.githooks/post-merge" yes)"
    [ "$got" = "exclude=added /.githooks/post-merge
exclude-file=changed" ] \
        && ok "and it is excluded" \
        || fail "glob: state" "got '$got'"
    [ -z "$(git_q -C "$r" status --porcelain)" ] \
        && ok "and the tree is clean" \
        || fail "glob: dirty" "$(git_q -C "$r" status --porcelain | tr '\n' ' ')"
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
    got="$(reconcile_exclude "$r" "" yes "none (outside the working tree)")"
    after="$(cat "$r/.git/info/exclude" 2>/dev/null)"
    case "$got" in
        "exclude=none (outside the working tree"*) got_ok=1 ;;
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

# --- 9. the shell-syntax gate finds files, refuses none, and asks the real question --------
# Three findings in one. The stage could not tell "parsed 40 files" from "parsed none" — every
# unmatched glob was swallowed by `[ -f ]`, so a broken gate printed its header and then "All
# checks passed". The list was hand-written twice (check.sh and ci.yml) and drifted by a `*.md`
# skip inside one commit. And the filter was a denylist of two extensions over unrestricted
# directory globs, so any non-shell file dropped beside a hook was handed to `bash -n`.
case9() {
    local d="$work/gate" out n
    mkdir -p "$d/scripts/tests" "$d/scripts/hooks" "$d/.claude/hooks" "$d/.container"
    printf '#!/usr/bin/env bash\ntrue\n' >"$d/scripts/a.sh"
    printf '#!/bin/sh\ntrue\n' >"$d/scripts/hooks/post-merge"     # no .sh, still shell
    # One extensionless file per glob that was widened from `*.sh` to `*`, so each has a file
    # only it can match. Without them, narrowing those globs back left this case green — the
    # only extensionless fixture lived in `scripts/hooks/`, which was already `*`.
    printf '#!/usr/bin/env bash\ntrue\n' >"$d/scripts/preflight"
    printf '#!/bin/sh\ntrue\n' >"$d/.container/entrypoint"
    printf '#!/usr/bin/env bash\ntrue\n' >"$d/scripts/tests/helper"
    printf 'not shell at all: [unclosed\n' >"$d/.claude/hooks/notes.txt"
    printf '# a readme\n' >"$d/scripts/hooks/README.md"
    printf '#!/usr/bin/python\nprint(1)\n' >"$d/.claude/hooks/probe.py"
    printf '#!/bin/bash -e\ntrue\n' >"$d/.claude/hooks/flags"
    # A shebang with NO trailing newline: `read` returns non-zero at EOF having already
    # assigned, and a `|| head=""` on that dropped the file out of the gate silently.
    printf '#!/bin/sh' >"$d/.claude/hooks/no-newline"
    # `bash -n` cannot parse zsh, so selecting it would turn a valid script into a red gate.
    printf '#!/usr/bin/env zsh\nsetopt extendedglob\n' >"$d/.claude/hooks/zshy"

    if out="$(check_shell_syntax "$d" 2>&1)"; then
        ok "a tree of valid shell parses"
    else
        fail "gate: valid" "$out"
        return
    fi
    n="$(printf '%s' "$out" | grep -o '[0-9]* shell file' | grep -o '[0-9]*')"
    # 7: the .sh, the extensionless hook, the three extensionless files in the widened globs,
    # the one with flags, and the one with no trailing newline. Out: the .txt, the .md, the
    # python, and the zsh.
    [ "$n" = "7" ] \
        && ok "and it counts what it parsed, selecting by shebang rather than by extension" \
        || fail "gate: count" "parsed $n files, expected 7: $out"
    printf '%s\n' "$(shell_sources "$d")" | grep -q 'no-newline' \
        && ok "including a shebang with no trailing newline" \
        || fail "gate: no-newline" "a one-line shebang file was dropped: $(shell_sources "$d" | tr '\n' ' ')"
    printf '%s\n' "$(shell_sources "$d")" | grep -q 'zshy' \
        && fail "gate: zsh" "zsh was selected; bash -n cannot parse it" \
        || ok "and not zsh, which bash -n cannot parse"

    # An empty tree is a broken gate, not an idle one.
    mkdir -p "$d/empty"
    if check_shell_syntax "$d/empty" >/dev/null 2>&1; then
        fail "gate: empty" "finding no shell files was reported as success"
    else
        ok "and finding nothing at all is a failure, not a quiet pass"
    fi

    # And it still fails on real breakage.
    printf '#!/usr/bin/env bash\nif true; then\n' >"$d/scripts/broken.sh"
    if check_shell_syntax "$d" >/dev/null 2>&1; then
        fail "gate: broken" "an unbalanced if parsed clean"
    else
        ok "and an unbalanced if fails it"
    fi
}

# --- 10. exclude-file= is measured against the file, in every arm ---------------------------
# THE ORACLE, not a snapshot. Three consecutive must-fixes were one shape: a per-pattern or
# per-step render arm asserting a run-level fact it could not see. The clincher is that the
# report word `undecided` has two producers with OPPOSITE file semantics — `want=unknown`
# returns early and touches nothing, `want=undecided` runs the sweep first — so no wording of
# that arm could ever have been right.
#
# So the test measures the file itself, either side of the call, and requires the emitted word
# to agree with its own measurement in EVERY arm. It enumerates the wants rather than checking
# one, because "fixed at the arm the reviewer found" is exactly how this defect survived three
# rounds. A hardcoded `changed` fails the untouched cells; a hardcoded `unchanged` fails the
# sweeping ones; a deleted emission fails all of them.
case10() {
    local d="$work/filefact" r want got line before after claimed measured
    for want in yes no undecided unknown bogus; do
        r="$d/$want"
        mkdir -p "$r"
        git_q init -q "$r" >/dev/null 2>&1
        git_q -C "$r" commit -q --allow-empty -m init
        # A jkb block for a DIFFERENT pattern, so the sweep has something to remove and the
        # arms genuinely differ in what they do to the file.
        { exclude_marker; printf '/stale/post-merge\n'; } >>"$r/.git/info/exclude"

        before="$(cksum <"$r/.git/info/exclude")"
        got="$(reconcile_exclude "$r" "/.githooks/post-merge" "$want" 2>/dev/null)"
        after="$(cksum <"$r/.git/info/exclude")"

        claimed=""
        while IFS= read -r line; do
            case "$line" in exclude-file=*) claimed="${line#exclude-file=}" ;; esac
        done <<EOF
$got
EOF
        [ "$before" = "$after" ] && measured=unchanged || measured=changed

        if [ -z "$claimed" ]; then
            fail "filefact: $want" "no exclude-file= line; the run-level fact was not reported"
        elif [ "$claimed" = "$measured" ]; then
            ok "want=$want reports exclude-file=$measured, which is what happened to the file"
        else
            fail "filefact: $want" "claimed $claimed, the file was $measured"
        fi
    done
}

# --- 10b. the reproduced contradiction, and the shape of its fix ---------------------------
# The exact fixture from the review: a bare repo with a relative core.hooksPath (so jkb refuses
# to anchor a chainer and reports `undecided`) plus a pre-existing jkb block for another
# pattern (so the sweep still runs and rewrites the file). It used to render
#
#   • excluded:   /old/post-merge dropped from .git/info/exclude …
#   warning:   nothing in .git/info/exclude was changed.
#
# two lines about one file stating opposite facts, unattended, from the post-merge hook.
#
# Asserted POSITIVELY — the `undecided` warning must be exactly one line — not as a denylist of
# the old sentence. Asserting a sentence's absence passes just as well when the arm can never
# fire, which is how the `exposed` downgrade stayed unreachable for a round.
case10b() {
    local d="$work/contradiction" out rendered n
    mkdir -p "$d"
    git_q init -q --bare "$d/bare.git" >/dev/null 2>&1
    git_q -C "$d/bare.git" config core.hooksPath .githooks
    mkdir -p "$d/bare.git/info"
    { exclude_marker; printf '/old/post-merge\n'; } >>"$d/bare.git/info/exclude"
    printf '#!/bin/sh\necho hi\n' >"$d/src"

    out="$(install_git_hooks "$d/bare.git" "$d/src" 2>/dev/null)"
    case "$out" in
        *"exclude=retracted /old/post-merge"*) ok "the sweep still runs under an undecided want" ;;
        *) fail "contradiction: sweep" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    case "$out" in
        *"exclude-file=changed"*) ok "and the run says so, because the file really was rewritten" ;;
        *) fail "contradiction: fact" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac

    rendered="$(printf '%s\n' "$out" | render_git_hooks_report 2>&1)"
    # The ARM IN ISOLATION, and its whole output — not a count of one phrase inside it.
    # Counting the phrase was the first version of this assertion and it could not fail:
    # restoring the false second line adds a DIFFERENT sentence, so the phrase count stays 1.
    # Feeding the renderer one line and pinning everything it emits is what makes any extra
    # claim about the file a failure, whatever words it is dressed in.
    n="$(printf 'exclude=undecided (a reason)\n' | render_git_hooks_report 2>&1 | wc -l | tr -d ' ')"
    [ "$n" -eq 1 ] \
        && ok "and the undecided arm emits one line: a claim about the pattern, not the file" \
        || fail "contradiction: arm" "the undecided arm emitted $n lines; only exclude-file= may describe the file"
    # The remedy must still reach the reader — it rides on dispatch=, not on the dropped line.
    case "$rendered" in
        *"run setup.sh from a working tree"*) ok "and the repair is still printed, via dispatch=" ;;
        *) fail "contradiction: remedy" "$(printf '%s' "$rendered" | tr '\n' '|')" ;;
    esac
}

# --- 10c. a value the renderer has no arm for surfaces --------------------------------------
# Every other key here carries this pin. Without it a third value added at the producer falls
# into a silent default, which for a security-adjacent report is indistinguishable from the
# state it was meant to describe.
case10c() {
    local out
    out="$(printf 'exclude-file=bogus\n' | render_git_hooks_report 2>&1)"
    case "$out" in
        *"unrecognised exclude-file state"*) ok "an unknown exclude-file value is warned about" ;;
        *) fail "filefact: default" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    out="$(printf 'exclude-file=changed\nexclude-file=unchanged\n' | render_git_hooks_report 2>&1)"
    [ -z "$out" ] \
        && ok "and the two real values render nothing — the itemised lines already said it" \
        || fail "filefact: quiet" "expected silence, got: $(printf '%s' "$out" | tr '\n' '|')"
}

# --- 10d. every git call in lib.sh goes through the wrapper ---------------------------------
# The header states this rule and, until now, nothing enforced it: four of the six call sites
# could be reverted to bare `git` with both suites green. That is precisely how the chainer's
# dispatch was once reverted to `--git-dir` while the whole gate stayed green.
#
# Anchored on `git -C`, which is the shape EVERY call in this file uses and the shape a
# regression takes. Deliberately not a command-position match: lib.sh legitimately names
# `git rev-parse` inside a warning string and inside the two emitted chainer heredocs, and a
# matcher that has to reason about quoting is a second parser to get wrong. The bound is
# stated rather than hidden — a new call written without `-C` is not caught, and there is no
# such call today.
case10d() {
    local lib hits probe
    lib="$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
    hits="$(_bare_git_calls "$lib")"
    [ -z "$hits" ] \
        && ok "no bare git -C outside the wrapper" \
        || fail "wrapper: live" "bare git call(s): $(printf '%s' "$hits" | tr '\n' '|')"

    # And the check itself must fire. A guard nobody has watched fail is this directory's
    # recurring defect, so every call site is reverted in turn and each must be reported.
    probe="$work/wrapper-probe.sh"
    local n=0 caught=0 target
    for target in 'rev-parse --git-common-dir' 'config --get --path core.hooksPath' \
                  'rev-parse --show-toplevel' 'worktree list --porcelain'; do
        n=$((n + 1))
        sed "s|_git -C \"\$repo_root\" $target|git -C \"\$repo_root\" $target|" "$lib" >"$probe"
        if cmp -s "$lib" "$probe"; then
            fail "wrapper: probe" "the $target revert did not apply, so nothing was proven"
        elif [ -n "$(_bare_git_calls "$probe")" ]; then
            caught=$((caught + 1))
        else
            fail "wrapper: missed" "a bare git call at '$target' was not detected"
        fi
    done
    [ "$caught" -eq "$n" ] \
        && ok "and it reports a revert at each of the $n call sites" \
        || fail "wrapper: coverage" "caught $caught of $n"
}

# Bare `git -C` outside the emitted chainer heredocs and outside `_git`'s own definition.
_bare_git_calls() {
    awk '
        /<<'"'"'CHAIN'"'"'/ { h = 1; next }
        h && /^CHAIN$/       { h = 0; next }
        h                    { next }
        /^_git\(\)/          { next }
        /(^|[^_[:alnum:]])git[ \t]+-C/ { printf "%d: %s\n", NR, $0 }
    ' "$1"
}

# --- 10e. the config readers survive `set -e` on their own ---------------------------------
# lib.sh's header promises every function behaves the same with `set -e` on or off. Both
# readers of `core.hooksPath` broke it: a bare `x="$(cmd)"` is a simple command, so a non-zero
# substitution aborts the shell — and exit 1 there is the COMMONEST case, the setting not being
# present at all. It was masked because the one production caller writes `… || override_rc=$?`,
# which disables errexit for the whole call; a second caller spelled the ordinary way would
# have killed setup.sh outright on any machine without the setting.
#
# Run in a `bash -euo pipefail` child, because the suites cannot be sourced under `set -e`
# (`fail` increments and continues by design). The premise is asserted first: if the fixture
# ever had a `core.hooksPath`, git would exit 0 and this case would pass having tested nothing.
case10e() {
    local d="$work/errexit" lib
    lib="$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
    mkdir -p "$d"
    git_q init -q "$d/r" >/dev/null 2>&1

    if git_q -C "$d/r" config --get core.hooksPath >/dev/null 2>&1; then
        fail "errexit: premise" "the fixture has a core.hooksPath, so the failing read never happens"
        return
    fi
    ok "the fixture genuinely has no core.hooksPath, so the read really does exit non-zero"

    if bash -euo pipefail -c '. "$1"; git_hooks_override "$2" >/dev/null' _ "$lib" "$d/r"; then
        ok "and git_hooks_override returns rather than killing an errexit shell"
    else
        fail "errexit: override" "the shell died (exit $?) reading an absent core.hooksPath"
    fi
    if bash -euo pipefail -c '. "$1"; git_hooks_exclude_pattern "$2" >/dev/null' _ "$lib" "$d/r"; then
        ok "and so does git_hooks_exclude_pattern"
    else
        fail "errexit: pattern" "the shell died (exit $?) reading an absent core.hooksPath"
    fi
}

# --- 10f. an unrecognised refusal status is named, never absorbed --------------------------
# Callable with a status that does not exist yet, which is the whole reason the mapping was
# lifted out of `install_git_hooks`: inline, `*)` and `4)` were behaviourally identical (there
# is no fifth code today), so a mutation collapsing them stayed green and no test could tell an
# honest catch-all from one that hands a future code a confident, wrong remedy.
case10f() {
    local v w
    for rc in 2 3 4; do
        v="$(_override_verdict "$rc")"
        case "$v" in
            unreadable*|unanchored*) ;;
            *) fail "verdict: $rc" "unexpected verdict '$v'" ; return ;;
        esac
    done
    ok "each known refusal status maps to a verdict the renderer has an arm for"

    v="$(_override_verdict 9)"
    w="$(_override_why 9)"
    case "$v" in
        *"unrecognised status 9"*) ok "an unknown status says so, and carries its number" ;;
        "unanchored"*) fail "verdict: absorb" "status 9 was absorbed into a definite '$v'" ;;
        *) fail "verdict: unknown" "got '$v'" ;;
    esac
    case "$w" in
        *"could not be resolved"*) ok "and its exclude reason claims nothing it cannot know" ;;
        *"no working tree"*) fail "why: absorb" "status 9 borrowed the unanchored reason" ;;
        *) fail "why: unknown" "got '$w'" ;;
    esac
    # The verdict must route to an arm that exists, or the operator gets the renderer's
    # "unrecognised dispatch verdict" instead of a diagnosis.
    case "$(printf 'dispatch=%s\n' "$v" | render_git_hooks_report 2>&1)" in
        *"unrecognised dispatch verdict"*) fail "verdict: arm" "no render arm for '$v'" ;;
        *"could not be resolved"*) ok "and the rendering names the cause it actually observed" ;;
        *) fail "verdict: render" "unexpected rendering of '$v'" ;;
    esac
}

# --- 10g. an environment-injected core.hooksPath is refused, not installed into -------------
# `_git` strips the three variables that select a REPOSITORY but deliberately not the two that
# inject CONFIGURATION: `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS` carry the `safe.directory`
# grants this project's dev container needs, and stripping those makes git refuse the checkout.
# So the transient case is detected instead, by asking git which scope the value came from.
#
# Measured without the refusal: jkb installs the chainer at the INJECTED path, adds an exclude
# rule for it, and reports `dispatch=chained` — the good verdict — while the repository's own
# core.hooksPath has no chainer, so no later pull runs one. Reachable unattended, because
# `git -c core.hooksPath=X pull` exports the setting into the hook environment and the hook
# runs setup.sh.
case10g() {
    local d="$work/injected" out
    mkdir -p "$d"
    git_q init -q "$d/r" >/dev/null 2>&1
    git_q -C "$d/r" commit -q --allow-empty -m init
    git_q -C "$d/r" config core.hooksPath "$d/r/persistent"
    printf '#!/bin/sh\necho hi\n' >"$d/src"

    out="$(GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0="$d/r/injected" \
           install_git_hooks "$d/r" "$d/src" 2>/dev/null)"

    case "$out" in
        *"dispatch=transient"*) ok "an environment-injected core.hooksPath is reported transient" ;;
        *"dispatch=chained"*)   fail "injected: verdict" "reported the GOOD verdict for a transient path" ;;
        *) fail "injected: verdict" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    [ -e "$d/r/injected" ] \
        && fail "injected: wrote" "a chainer was installed at the injected path" \
        || ok "and nothing is installed at the path the environment named"
    # The exclude file must not gain a rule for a path jkb declined to own.
    case "$out" in
        *"exclude=added"*) fail "injected: exclude" "an exclude rule was written for the injected path" ;;
        *) ok "and no exclude rule is written for it" ;;
    esac
    # The rendering, not just the wire report: without its own arm the verdict falls into the
    # renderer's "unrecognised dispatch verdict" default, so the operator is told the report is
    # broken rather than what is wrong with their configuration.
    case "$(printf '%s\n' "$out" | render_git_hooks_report 2>&1)" in
        *"unrecognised dispatch verdict"*)
            fail "injected: arm" "transient has no render arm" ;;
        *"set by the environment"*"re-run setup.sh without that setting"*)
            ok "and the rendering names the cause and a repair the operator can take" ;;
        *) fail "injected: render" "$(printf '%s\n' "$out" | render_git_hooks_report 2>&1 | tr '\n' '|')" ;;
    esac
    # And the same via GIT_CONFIG_PARAMETERS, which is the form `git -c` exports into a hook.
    out="$(GIT_CONFIG_PARAMETERS="'core.hooksPath=$d/r/injected2'" \
           install_git_hooks "$d/r" "$d/src" 2>/dev/null)"
    case "$out" in
        *"dispatch=transient"*) ok "and the GIT_CONFIG_PARAMETERS form is caught too" ;;
        *) fail "injected: params" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    # The premise: with no injection the same repo resolves normally, so the assertions above
    # are about the injection and not about something broken in the fixture.
    out="$(install_git_hooks "$d/r" "$d/src" 2>/dev/null)"
    case "$out" in
        *"dispatch=chained"*) ok "while the repository's own core.hooksPath still resolves normally" ;;
        *) fail "injected: premise" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
}

echo "==> scripts/lib.sh::git_hooks_dir + git_hooks_override + reconcile_exclude"
run_cases case1 case2 case3 case4 case5 case6 case6b case6c case6d case6p case6n case6g case6m case6k case6h case6j case6i case6e case6f case7 case8 case9 case10 case10b case10c case10d case10e case10f case10g

finish
