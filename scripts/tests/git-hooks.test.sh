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
    local want="exclude=added /.githooks/post-merge
exclude-file=changed"
    if [ "$got" = "$want" ]; then
        ok "a chainer inside the working tree: excluded locally"
    else
        fail "exclude: pattern" "expected '$(printf '%s' "$want" | tr '\n' '|')', got '$(printf '%s' "$got" | tr '\n' '|')'"
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
        fail "exclude: idempotence" "second run printed '$(printf '%s' "$got" | tr '\n' '|')' and the pattern appears $(grep -c '^/\.githooks/post-merge$' "$r/.git/info/exclude") time(s)"
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
        fail "exclude: retract" "printed '$(printf '%s' "$got" | tr '\n' '|')'; file still: $(tr '\n' '|' <"$r/.git/info/exclude")"
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
    local want="exclude=unowned /.githooks/post-merge
exclude-file=unchanged"
    if [ "$got" = "$want" ]; then
        ok "an unmarked exclude rule is reported as unowned"
    else
        fail "unowned: state" "expected '$(printf '%s' "$want" | tr '\n' '|')', got '$(printf '%s' "$got" | tr '\n' '|')'"
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
        fail "crlf: duplicate" "said '$(printf '%s' "$got" | tr '\n' '|')'; file: $(tr '\n' '|' <"$r/.git/info/exclude")"
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
        fail "crlf: retract" "said '$(printf '%s' "$got" | tr '\n' '|')'; file: $(tr '\n' '|' <"$r/.git/info/exclude")"
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
        || fail "glob: state" "got '$(printf '%s' "$got" | tr '\n' '|')'"
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
    # recurring defect, so EVERY call site is reverted in turn and each must be reported.
    #
    # The sites are DERIVED from the file, never listed here. A hand-written list went stale
    # the moment two readers of `core.hooksPath` were merged into one function — the named
    # command no longer existed, so a third of the coverage silently stopped being exercised.
    # It only surfaced because the probe asserts its own premise ("the revert did not apply,
    # so nothing was proven") rather than treating a no-op edit as a pass.
    probe="$work/wrapper-probe.sh"
    local n=0 caught=0 ln
    for ln in $(grep -n '_git -C' "$lib" | grep -v '^[0-9]*:_git()' | cut -d: -f1); do
        n=$((n + 1))
        awk -v L="$ln" 'NR==L { sub(/_git -C/, "git -C") } { print }' "$lib" >"$probe"
        if cmp -s "$lib" "$probe"; then
            fail "wrapper: probe" "the revert at line $ln did not apply, so nothing was proven"
        elif [ -n "$(_bare_git_calls "$probe")" ]; then
            caught=$((caught + 1))
        else
            fail "wrapper: missed" "a bare git call at line $ln was not detected"
        fi
    done
    [ "$n" -ge 4 ] \
        && ok "and there are $n wrapped call sites to check, not zero" \
        || fail "wrapper: none" "found $n call sites; the derivation is broken, not the code"
    [ "$caught" -eq "$n" ] \
        && ok "and it reports a revert at every one of them" \
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
    local v w rc bad=""
    # ASKS THE RENDERER. The first version matched the verdict WORD against a prefix pattern
    # (`unreadable*|unanchored*`), which is not the property it claims: `unreadableX` matches it
    # and has no render arm, and that mutation left this suite green. The renderer's own default
    # is the oracle for "is there an arm".
    #
    # The status list is derived from the function rather than written out beside it: two
    # hand-written lists that must agree is how the previous version came to omit a status
    # inside the very commit that added one.
    local n=0
    for rc in $(_override_statuses); do
        n=$((n + 1))
        v="$(_override_verdict "$rc")"
        case "$(printf 'dispatch=%s\n' "$v" | render_git_hooks_report 2>&1)" in
            *"unrecognised dispatch verdict"*) bad="$bad $rc" ;;
        esac
    done
    # A derived list that derives NOTHING passes a loop over it vacuously — the same lesson as
    # `check_shell_syntax`, where finding no files had to become a failure rather than idle.
    [ "$n" -ge 3 ] \
        && ok "the refusal statuses are derived from the function, and there are $n of them" \
        || fail "verdict: derived" "derived $n statuses; the derivation is broken, not the code"
    [ -z "$bad" ] \
        && ok "every refusal status maps to a verdict the renderer really has an arm for" \
        || fail "verdict: arms" "no render arm for status(es):$bad"

    # AND THE REMEDY MUST REACH THE OPERATOR, TRUE. An arm existing is not the claim; case10f's
    # loop above passed the whole time code 5 rendered through `unreadable`'s arm and told the
    # operator to run `git config --show-origin --get core.hooksPath` — which, on the very git
    # that could not be asked, prints a perfectly normal `file:.git/config<TAB>.githooks` and
    # appears to refute the warning. So this drives the WHOLE path: a real repository, a git
    # that refuses every scope option, `install_git_hooks`, and the rendered report a person
    # actually reads.
    local d5="$work/unaskable" report5 rendered5
    mkdir -p "$d5/bin"
    git_q init -q "$d5/r" >/dev/null 2>&1
    git_q -C "$d5/r" commit -q --allow-empty -m init
    git_q -C "$d5/r" config core.hooksPath .githooks
    printf '#!/bin/sh\necho hi\n' >"$d5/src"
    printf '%s\n' '#!/bin/sh' \
        'for a in "$@"; do case "$a" in --show-scope|--includes) exit 129;; esac; done' \
        "exec $(command -v git) \"\$@\"" >"$d5/bin/git"
    chmod 755 "$d5/bin/git"
    report5="$(PATH="$d5/bin:$PATH" install_git_hooks "$d5/r" "$d5/src" 2>/dev/null)"
    case "$report5" in
        *"dispatch=unaskable"*)
            ok "a git that cannot be asked reports its own dispatch word, not unreadable" ;;
        *"dispatch=unreadable"*)
            fail "unaskable: word" "code 5 still reports as unreadable, sharing code 2's sentence" ;;
        *) fail "unaskable: premise" "got: $(printf '%s' "$report5" | tr '\n' '|')" ;;
    esac
    rendered5="$(printf '%s\n' "$report5" | render_git_hooks_report 2>&1)"
    case "$rendered5" in
        *"--show-origin"*)
            fail "unaskable: remedy" \
                 "the repair line tells the operator to run a command that prints a normal value here" ;;
        *"upgrade git"*)
            ok "and its repair line names the git, which is the thing that refused" ;;
        *) fail "unaskable: render" "got: $(printf '%s' "$rendered5" | tr '\n' '|')" ;;
    esac
    # ...and it must not name a repair the fixture can be shown NOT to satisfy. The line used to
    # offer "or set core.hooksPath yourself and re-run": rc 5 is raised from the OPTION's 129, so
    # no repository state can change it. Demonstrated rather than argued — the repair is DONE
    # here, and the answer must be identical.
    mkdir -p "$d5/myhooks"
    git_q -C "$d5/r" config core.hooksPath "$d5/myhooks"
    after5="$(PATH="$d5/bin:$PATH" install_git_hooks "$d5/r" "$d5/src" 2>/dev/null)"
    case "$after5" in
        *"dispatch=unaskable"*)
            case "$rendered5" in
                *"set core.hooksPath yourself and re-run"*)
                    fail "unaskable: falseremedy" "the repair line offers a repair that was just \
performed and changed nothing" ;;
                *) ok "and it offers no repair the operator can perform and see refuted" ;;
            esac ;;
        *) fail "unaskable: repair-premise" "setting core.hooksPath changed the verdict, so this \
tested nothing: $(printf '%s' "$after5" | tr '\n' '|')" ;;
    esac
    git_q -C "$d5/r" config --unset core.hooksPath 2>/dev/null || :

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

# --- 10g. an environment-injected core.hooksPath is IGNORED, and the stored one serviced ----
# `_git` strips the three variables that select a REPOSITORY but deliberately not the two that
# inject CONFIGURATION: `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS` carry the `safe.directory`
# grants this project's dev container needs, and stripping those makes git refuse the checkout.
#
# So the read skips `command` scope instead. `--get` reports only the WINNING value, and a
# `git -c core.hooksPath=X pull` — which exports the setting into the hook environment, and the
# hook runs setup.sh — wins over everything stored. Measured before this: jkb installed its
# chainer at the injected path, added an exclude rule for it, and reported `dispatch=chained`,
# while the repository's own hooksPath kept none, so no later pull ran one.
#
# Refusing was the first fix and was worse than this one: a healthy repo pulled with `-c`
# printed three warnings and advised storing a value it had already stored, and a repo storing
# none was advised to set one — which would have killed `.git/hooks` dispatch outright.
case10g() {
    local d="$work/injected" out
    mkdir -p "$d"
    git_q init -q "$d/r" >/dev/null 2>&1
    git_q -C "$d/r" commit -q --allow-empty -m init
    git_q -C "$d/r" config core.hooksPath .stored
    printf '#!/bin/sh\necho hi\n' >"$d/src"

    out="$(GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0="$d/r/injected" \
           install_git_hooks "$d/r" "$d/src" 2>/dev/null)"

    case "$out" in
        *"chainer=installed $d/r/.stored/post-merge"*)
            ok "an injected core.hooksPath is ignored and the STORED path is serviced" ;;
        *) fail "injected: stored" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    [ -e "$d/r/injected" ] \
        && fail "injected: wrote" "a chainer was installed at the injected path" \
        || ok "and nothing is installed at the path the environment named"
    case "$out" in
        *"exclude=added /.stored/post-merge"*) ok "and the exclude rule names the stored path" ;;
        *"/injected/"*) fail "injected: exclude" "an exclude rule was written for the injected path" ;;
        *) fail "injected: exclude" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac

    # A repo that stores NOTHING must stay silent under an injection: it is healthy, git reads
    # `.git/hooks` itself, and there is nothing to report. This is the case the first fix got
    # loudly wrong — three warnings and a remedy that, followed, kills `.git/hooks` dispatch.
    local d2="$work/injected-none" out2 rendered
    mkdir -p "$d2"
    git_q init -q "$d2/r" >/dev/null 2>&1
    git_q -C "$d2/r" commit -q --allow-empty -m init
    printf '#!/bin/sh\necho hi\n' >"$d2/src"
    out2="$(GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0="$d2/r/injected" \
            install_git_hooks "$d2/r" "$d2/src" 2>/dev/null)"
    case "$out2" in
        *"dispatch=direct"*) ok "a repo storing no core.hooksPath reads as direct under an injection" ;;
        *) fail "injected: direct" "got: $(printf '%s' "$out2" | tr '\n' '|')" ;;
    esac
    rendered="$(printf '%s\n' "$out2" | render_git_hooks_report 2>&1)"
    case "$rendered" in
        *warning*) fail "injected: quiet" "warned about a healthy repo: $(printf '%s' "$rendered" | tr '\n' '|')" ;;
        *) ok "and nothing is warned about, because nothing is wrong" ;;
    esac
    [ -e "$d2/r/injected" ] \
        && fail "injected: wrote2" "installed into the injected path with nothing stored" \
        || ok "and again nothing is written where the environment pointed"

    # AND IT MUST NOT RETRACT THE REAL BLOCK. Measured before this was fixed: the override
    # refused the injected value while `git_hooks_exclude_pattern` read it separately and
    # happily derived from it — answering `none`, which the funnel turns into `want=no`, which
    # sweeps. So one environment variable retracted the exclude block for the repository's OWN
    # chainer, leaving that file untracked, the tree dirty and `jkb task land` refusing it,
    # unattended from the post-merge hook. Two readers of one fact, disagreeing.
    local d3="$work/injected-keeps" out3
    mkdir -p "$d3"
    git_q init -q "$d3/r" >/dev/null 2>&1
    git_q -C "$d3/r" commit -q --allow-empty -m init
    git_q -C "$d3/r" config core.hooksPath .githooks
    printf '#!/bin/sh\necho hi\n' >"$d3/src"
    install_git_hooks "$d3/r" "$d3/src" >/dev/null 2>&1
    if grep -q '^/\.githooks/post-merge$' "$d3/r/.git/info/exclude" 2>/dev/null; then
        ok "a normal run establishes the exclude block for the repository's own chainer"
    else
        fail "injected: premise" "the fixture never got an exclude block, so the next assertion proves nothing"
        return
    fi
    out3="$(GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/elsewhere/hooks \
            install_git_hooks "$d3/r" "$d3/src" 2>/dev/null)"
    grep -q '^/\.githooks/post-merge$' "$d3/r/.git/info/exclude" 2>/dev/null \
        && ok "and an injected core.hooksPath does not retract it" \
        || fail "injected: retracted" "one environment variable swept the repository's own exclude block"
    case "$out3" in
        *"exclude-file=unchanged"*) ok "and the run truthfully reports the file was not touched" ;;
        *) fail "injected: filefact" "got: $(printf '%s' "$out3" | tr '\n' '|')" ;;
    esac
    # The GIT_CONFIG_PARAMETERS form is what `git -c` actually exports into a hook.
    out3="$(GIT_CONFIG_PARAMETERS="'core.hooksPath=/elsewhere/hooks'" \
            install_git_hooks "$d3/r" "$d3/src" 2>/dev/null)"
    case "$out3" in
        *"$d3/r/.githooks/post-merge"*) ok "and the GIT_CONFIG_PARAMETERS form is ignored too" ;;
        *) fail "injected: params" "got: $(printf '%s' "$out3" | tr '\n' '|')" ;;
    esac
}

# --- 10h. a git without --show-scope still resolves normally --------------------------------
# `--show-scope` is git >= 2.26, and an older one exits 129 for the unknown option — which is
# NOT "cannot expand the value". Folded together, every repo on such a git would report
# `dispatch=unreadable` and jkb would stop installing chainers entirely. Nothing else exercises
# the fallback, so without this case it is a branch no run ever takes.
#
# A shim on PATH rather than an old git, and it asserts the shim really refuses first: a shim
# that quietly worked would make this case pass having tested the ordinary path twice.
case10h() {
    local d="$work/oldgit" out rc
    mkdir -p "$d/bin"
    printf '%s\n' '#!/bin/sh' \
        'for a in "$@"; do [ "$a" = "--show-scope" ] && { echo "error: unknown option" >&2; exit 129; }; done' \
        "exec $(command -v git) \"\$@\"" >"$d/bin/git"
    chmod 755 "$d/bin/git"

    git_q init -q "$d/r" >/dev/null 2>&1
    git_q -C "$d/r" commit -q --allow-empty -m init
    git_q -C "$d/r" config core.hooksPath .githooks
    printf '#!/bin/sh\necho hi\n' >"$d/src"

    # The premise must observe the SHIM, not an unset key: the first version asked
    # `--show-scope --get user.name`, which exits 1 under `isolate_git` because the key is
    # absent — so deleting the shim's refusal left this passing, and the case then exercised the
    # modern path twice, which its own comment says it exists to prevent. 129 is what git gives
    # for an unknown option, and it is distinguishable from every other outcome here.
    PATH="$d/bin:$PATH" git config --show-scope --get-all core.hooksPath >/dev/null 2>&1
    rc=$?
    if [ "$rc" -ne 129 ]; then
        fail "oldgit: premise" "the shim did not refuse --show-scope (exit $rc), so the fallback was never taken"
        return
    fi
    ok "the shim refuses --show-scope with git's own exit 129, as a git older than 2.26 does"

    out="$(PATH="$d/bin:$PATH" install_git_hooks "$d/r" "$d/src" 2>/dev/null)"
    case "$out" in
        *"dispatch=chained"*) ok "and jkb still resolves core.hooksPath and installs the chainer" ;;
        *"unreadable"*) fail "oldgit: verdict" "an unknown OPTION was reported as an unreadable VALUE" ;;
        *) fail "oldgit: verdict" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    # AN ASSERTION WHOSE ONLY DISCRIMINATOR IS A TOKEN NO PRODUCER CAN EMIT IS NOT AN ASSERTION.
    # This used to fail on `dispatch=transient` — deleted from lib.sh by the same commit — so
    # the `*)` arm ran unconditionally and reported `ok` while the old-git path was measurably
    # installing the chainer at the injected value. It asserts the property positively now: the
    # scoped fallback ignores an injected `core.hooksPath` exactly as `--show-scope` does, so
    # the chainer lands at the REPOSITORY'S `.githooks` and `dispatch=` names that path.
    out="$(PATH="$d/bin:$PATH" GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath \
           GIT_CONFIG_VALUE_0="$d/r/injected" install_git_hooks "$d/r" "$d/src" 2>/dev/null)"
    case "$out" in
        *"$d/r/injected"*)
            fail "oldgit: injected" "an injected core.hooksPath was serviced: $(printf '%s' "$out" | tr '\n' '|')" ;;
        *"dispatch=chained $d/r/.githooks/post-merge"*)
            ok "and an injected core.hooksPath is ignored there too, not just on a modern git" ;;
        *) fail "oldgit: injected" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    [ -f "$d/r/injected/post-merge" ] \
        && fail "oldgit: wrote" "a chainer was written at the injected path" \
        || ok "and nothing was written at the injected path"
}

# --- 10i. an unmeasurable file is `unknown`, never `unchanged` -----------------------------
# The false-reassurance defect one level down, inside the primitive added to prevent it.
# `_exclude_fingerprint` folded "could not measure" into "absent"; two failures compare equal,
# so with `cksum` unavailable jkb reported `exclude-file=unchanged` OVER A REAL WRITE. Measured
# with a cksum that exits 127 on PATH.
#
# `absent` stays a real, established answer with rc 0 — creating or removing the file must
# still register — so the case pins both halves, or "return 1 always" would pass it.
case10i() {
    local d="$work/nocksum" out
    mkdir -p "$d/bin"
    printf '#!/bin/sh\nexit 127\n' >"$d/bin/cksum"
    chmod 755 "$d/bin/cksum"

    git_q init -q "$d/r" >/dev/null 2>&1
    git_q -C "$d/r" commit -q --allow-empty -m init
    git_q -C "$d/r" config core.hooksPath .githooks
    printf '#!/bin/sh\necho hi\n' >"$d/src"

    if PATH="$d/bin:$PATH" cksum </dev/null >/dev/null 2>&1; then
        fail "nocksum: premise" "the shim did not break cksum, so nothing was tested"
        return
    fi
    ok "the shim breaks cksum, so the fingerprint genuinely cannot be taken"

    out="$(PATH="$d/bin:$PATH" install_git_hooks "$d/r" "$d/src" 2>/dev/null)"
    case "$out" in
        *"exclude-file=unknown"*)   ok "and an unmeasurable file is reported unknown" ;;
        *"exclude-file=unchanged"*) fail "nocksum: false" "reported unchanged over a write it could not measure" ;;
        *) fail "nocksum: state" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    case "$(printf '%s\n' "$out" | render_git_hooks_report 2>&1)" in
        *"unrecognised exclude-file state"*) fail "nocksum: arm" "unknown has no render arm" ;;
        *"could not tell whether"*) ok "and the operator is told, rather than reassured" ;;
        *) fail "nocksum: render" "the unknown state rendered nothing at all" ;;
    esac

    # The other half: an ABSENT file is an established answer, not an unmeasurable one, so
    # creating it still reports `changed`. Without this, `return 1` everywhere would pass above.
    rm -f "$d/r/.git/info/exclude"
    out="$(install_git_hooks "$d/r" "$d/src" 2>/dev/null)"
    case "$out" in
        *"exclude-file=changed"*) ok "while creating an absent exclude file still reports changed" ;;
        *) fail "nocksum: absent" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
}

# --- 10j. a git that cannot be asked is `unknown`, never `unchanged` ------------------------
# `_exclude_path` used to spell "not a git repository" and "could not ask git" the same way —
# both an empty answer — so a transient failure made both fingerprints `no-repo`, they compared
# equal, and the run reported `exclude-file=unchanged`. 128 is git's own "not a repository",
# which IS an established answer; anything else is not.
case10j() {
    local d="$work/nogit" out
    mkdir -p "$d/bin"
    printf '#!/bin/sh\nexit 127\n' >"$d/bin/git"
    chmod 755 "$d/bin/git"
    git_q init -q "$d/r" >/dev/null 2>&1
    printf '#!/bin/sh\necho hi\n' >"$d/src"

    if PATH="$d/bin:$PATH" git rev-parse --git-common-dir >/dev/null 2>&1; then
        fail "nogit: premise" "the shim did not break git, so nothing was tested"
        return
    fi
    ok "the shim makes git unaskable, without saying 'not a repository'"

    out="$(PATH="$d/bin:$PATH" reconcile_exclude "$d/r" "/p" yes 2>/dev/null)"
    case "$out" in
        *"exclude-file=unknown"*)   ok "and the run reports unknown rather than a definite answer" ;;
        *"exclude-file=unchanged"*) fail "nogit: false" "claimed unchanged after failing to locate the file" ;;
        *) fail "nogit: state" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    # The other side: genuinely not a git repository stays an established `none`/`unchanged`.
    out="$(reconcile_exclude "$d" "/p" yes 2>/dev/null)"
    case "$out" in
        *"exclude=none (not a git repository)"*"exclude-file=unchanged"*)
            ok "while a directory that really is no repository stays a definite answer" ;;
        *) fail "nogit: norepo" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
}

# --- 10k. the protocol header names every key the producer emits ---------------------------
# Three findings in this area were a header that had gone stale. Derived from the source, so
# adding a key forces the decision at the moment it is added.
case10k() {
    local lib key missing="" n=0
    lib="$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
    for key in $(grep -oE "printf '[a-z-]+=" "$lib" | sed "s/printf '//; s/=$//" | sort -u); do
        n=$((n + 1))
        grep -q "^#   $key=" "$lib" || missing="$missing $key"
    done
    [ "$n" -ge 4 ] \
        && ok "the producer's keys are derived from the source, and there are $n of them" \
        || fail "header: derived" "found $n keys; the derivation is broken"
    [ -z "$missing" ] \
        && ok "and the protocol header documents every one of them" \
        || fail "header: stale" "keys the header never mentions:$missing"
}

# --- 10l. the post-merge hook, driven by a real merge -------------------------------------
# THE FIXTURE MUST NOT DECIDE FOR ITSELF WHAT ENVIRONMENT GIT PRODUCES. The first version of
# this case built one by hand — cwd in the right repo, `GIT_DIR` naming the foreign one — which
# is the INVERSE of what git does, and under that inversion scrubbing the environment can only
# look like a win. Measured, git runs a hook with the cwd at the working tree it resolved and
# `GIT_DIR` naming the repository the merge was about (`GIT_WORK_TREE=.`). So the scrub the case
# was green for could not fix `--show-toplevel` at all, and DID lose the only pointer to the
# merged history: `ORIG_HEAD` stopped resolving and a merge touching `crates/` printed "no
# build-affecting changes pulled" — the exact failure the scrub was written to prevent.
#
# So this drives an actual merge and reads what the hook actually prints. Three layouts, and the
# middle one is the harm: without the guard an unrelated repository's setup.sh executes.
case10l() {
    local d="$work/hookenv" hook out
    hook="$(cd "$(dirname "$0")/../.." && pwd)/scripts/hooks/post-merge"
    [ -x "$hook" ] || { fail "hookenv: missing" "no hook at $hook"; return; }
    mkdir -p "$d"

    # build <name> — a repo with a `crates/` change on `feature`, a setup.sh that announces
    # which checkout ran it, and the real hook installed.
    _hookenv_build() {
        local r="$d/$1"
        mkdir -p "$r/scripts"
        git_q init -q "$r" >/dev/null 2>&1
        printf 'seed\n' >"$r/seed"
        # THE MARKER IS RESOLVED AT RUN TIME, not baked in at build time. Baked in, step 6's
        # fixture destroyed it: setting `core.worktree=$d/theirs` on `mine` and then running
        # `reset --hard` checks MINE's tree out INTO theirs, overwriting theirs/scripts/setup.sh
        # with mine's copy — so `SETUP-RAN-IN:theirs` could never be printed by anything, the arm
        # naming the harm was dead, and the case caught its regression only through the catch-all
        # and reported the wrong diagnosis. `dirname $0` cannot be relabelled by a checkout.
        printf '%s\n' '#!/bin/sh' 'echo "SETUP-RAN-IN:$(cd "$(dirname "$0")/.." && pwd -P)"' \
            >"$r/scripts/setup.sh"
        chmod +x "$r/scripts/setup.sh"
        git_q -C "$r" add -A >/dev/null; git_q -C "$r" commit -qm seed >/dev/null
        mkdir -p "$r/crates"; printf 'x\n' >"$r/crates/x.rs"
        git_q -C "$r" add -A >/dev/null; git_q -C "$r" commit -qm crates >/dev/null
        git_q -C "$r" branch -q feature 2>/dev/null
        git_q -C "$r" reset -q --hard HEAD~1
        cp "$hook" "$r/.git/hooks/post-merge"; chmod +x "$r/.git/hooks/post-merge"
    }
    # Seed an EXISTING checkout (one whose repository was created by the caller, because these
    # layouts are defined by how that creation was done) with the marker setup.sh, a crates/
    # change on `feature`, and HEAD one commit back.
    _hookenv_seed() {
        local r="$1"
        mkdir -p "$r/scripts"
        printf 'seed\n' >"$r/seed"
        printf '%s\n' '#!/bin/sh' 'echo "SETUP-RAN-IN:$(cd "$(dirname "$0")/.." && pwd -P)"' \
            >"$r/scripts/setup.sh"
        chmod +x "$r/scripts/setup.sh"
        git_q -C "$r" add -A >/dev/null; git_q -C "$r" commit -qm seed >/dev/null
        mkdir -p "$r/crates"; printf 'x\n' >"$r/crates/x.rs"
        git_q -C "$r" add -A >/dev/null; git_q -C "$r" commit -qm crates >/dev/null
        git_q -C "$r" branch -q feature 2>/dev/null
        git_q -C "$r" reset -q --hard HEAD~1
    }
    _hookenv_build mine
    _hookenv_build theirs

    # 1. The ordinary layout: the hook must SEE the crates/ change. This is what the scrub broke.
    out="$(git_q -C "$d/mine" merge --no-edit feature 2>&1)"
    case "$out" in
        *"running setup.sh"*"SETUP-RAN-IN:$(cd "$d/mine" && pwd -P)"*)
            ok "a merge touching crates/ runs setup.sh in its own checkout" ;;
        *"no build-affecting changes pulled"*)
            fail "hookenv: blind" "the hook missed a crates/ change — it lost ORIG_HEAD" ;;
        *) fail "hookenv: plain" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac

    # 2. `GIT_WORK_TREE` exported at an unrelated repo — the bare-dotfiles shell recipe. git
    #    chdirs THERE, so `--show-toplevel` names it however the environment is treated; the
    #    only safe move is to notice and stop. Building somebody else's checkout is the harm
    #    the whole repository-selection rule exists to prevent.
    git_q -C "$d/mine" reset -q --hard HEAD~0 >/dev/null 2>&1
    git_q -C "$d/mine" reset -q --hard "$(git_q -C "$d/mine" rev-parse feature~1)"
    out="$(GIT_WORK_TREE="$d/theirs" git_q -C "$d/mine" merge --no-edit feature 2>&1)"
    case "$out" in
        *"SETUP-RAN-IN:$(cd "$d/theirs" && pwd -P)"*)
            fail "hookenv: foreign" "the hook built an unrelated repository's checkout" ;;
        *"belongs to a different repository"*)
            ok "a redirected working tree is detected and named, and nothing is built" ;;
        *) fail "hookenv: redirect" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac

    # 3. A linked worktree must stay ORDINARY — the common dir is shared, so the guard in 2
    #    must not fire here. This is the layout every `jkb task work` session runs in.
    # From `feature~1`, or the worktree starts AT feature (step 2's merge advanced mine) and
    # the merge below is "Already up to date" — a case that exercises no hook at all.
    git_q -C "$d/mine" worktree add -q "$d/wt" -b wtb \
        "$(git_q -C "$d/mine" rev-parse feature~1)" >/dev/null 2>&1
    if [ -d "$d/wt" ]; then
        out="$(git_q -C "$d/wt" merge --no-edit feature 2>&1)"
        case "$out" in
            *"belongs to a different repository"*)
                fail "hookenv: worktree" "the guard fired inside a linked worktree" ;;
            *"running setup.sh"*) ok "and a linked worktree is treated as the ordinary case" ;;
            *) fail "hookenv: wt" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
    else
        fail "hookenv: wt-setup" "could not create a linked worktree"
    fi

    # 4. Reached through a SYMLINK — and ENTERED the way a user enters one. This is the pin on
    #    `pwd -P`, and it took two goes to make it one.
    #
    #    `git -C <symlink>` NORMALISES $PWD to the physical path before running the hook, so a
    #    fixture written that way is a byte-identical repeat of case 1: measured, `pwd -P` could
    #    be deleted from `common_of` with the whole suite green. Worse than a missing pin, that
    #    reading was then written into two comments as "defensive, not demonstrated", inviting a
    #    later round to delete it.
    #
    #    `cd` into the link, as `cd ~/repos/x && git pull` does, and the hook gets the SYMLINK
    #    path in $PWD while `--show-toplevel` answers the physical one — so the two `common_of`
    #    calls name one directory by two spellings. Measured: without `pwd -P` the hook prints
    #    "belongs to a different repository" and skips setup.sh and close-merged, permanently,
    #    for anyone whose checkout is reached through a link (`~/repos` a symlink, or macOS
    #    `/tmp` -> `/private/tmp`). A measurement whose variable you did not vary is not a
    #    measurement, and `git -C` was varying it back.
    _hookenv_build linked
    if ln -s "$d/linked" "$d/vialink" 2>/dev/null && [ -d "$d/vialink" ]; then
        out="$(cd "$d/vialink" && git_q merge --no-edit feature 2>&1)"
        case "$out" in
            *"belongs to a different repository"*)
                fail "hookenv: symlink" "the guard fired on a checkout reached through a symlink" ;;
            *"running setup.sh"*) ok "and a checkout ENTERED through a symlink is the ordinary case" ;;
            *) fail "hookenv: sym" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
    else
        fail "hookenv: sym-setup" "could not create a symlink"
    fi

    # 5. A checkout that has NO `.git` of its own, declared by `core.worktree` from a git dir
    #    outside it. `common_of "$repo_root"` scrubs the environment and then discovers from
    #    there — and finds nothing, so it returned empty, which the comparison read as "a
    #    different repository". Measured: an ordinary checkout in this layout was REFUSED, and
    #    an answer that could not be obtained is not the answer "foreign". The repository being
    #    merged declares the tree, so it is asked.
    local gd="$d/detached" wt="$d/dtree"
    mkdir -p "$wt/scripts"
    git_q init -q --bare "$gd" >/dev/null 2>&1
    git_q -C "$gd" config core.bare false
    git_q -C "$gd" config core.worktree "$wt"
    printf 'seed\n' >"$wt/seed"
    printf '%s\n' '#!/bin/sh' 'echo "SETUP-RAN-IN:$(cd "$(dirname "$0")/.." && pwd -P)"' \
        >"$wt/scripts/setup.sh"
    chmod +x "$wt/scripts/setup.sh"
    (
        cd "$wt" && export GIT_DIR="$gd"
        git_q add -A && git_q commit -qm seed
        mkdir -p crates && printf 'x\n' >crates/x.rs
        git_q add -A && git_q commit -qm crates
        git_q branch -q feature && git_q reset -q --hard HEAD~1
    ) >/dev/null 2>&1
    cp "$hook" "$gd/hooks/post-merge"
    chmod +x "$gd/hooks/post-merge"
    if [ -e "$wt/.git" ]; then
        fail "hookenv: cw-premise" "the fixture has a .git entry, so it is not the layout named"
    else
        out="$(cd "$wt" && GIT_DIR="$gd" git_q merge --no-edit feature 2>&1)"
        case "$out" in
            *"belongs to a different repository"*)
                fail "hookenv: coreworktree" "a declared working tree was refused as foreign" ;;
            *"running setup.sh"*)
                ok "and a core.worktree checkout with no .git of its own is the ordinary case" ;;
            *) fail "hookenv: cw" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
    fi

    # 6. ...and the arm that accepts a DECLARED working tree must not accept a REDIRECTED one.
    #    `core.worktree` and `GIT_WORK_TREE` answer the same question; the config one is the
    #    repository's answer, the environment one is the caller's, and that distinction is the
    #    whole guard. Found by probing the fix rather than by review: with `GIT_WORK_TREE`
    #    leaked AND `core.worktree` naming that same foreign tree, the arm took it and built
    #    somebody else's checkout — the arm re-opening the hole it sits above.
    git_q -C "$d/mine" config core.worktree "$d/theirs"
    git_q -C "$d/mine" reset -q --hard "$(git_q -C "$d/mine" rev-parse feature~1)"
    out="$(GIT_WORK_TREE="$d/theirs" git_q -C "$d/mine" merge --no-edit feature 2>&1)"
    case "$out" in
        *"SETUP-RAN-IN:$(cd "$d/theirs" && pwd -P)"*)
            fail "hookenv: declared-redirect" "a declared tree let a REDIRECTED one through" ;;
        *"belongs to a different repository"*)
            ok "and a declared working tree does not license a redirected one" ;;
        *) fail "hookenv: dr" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    git_q -C "$d/mine" config --unset core.worktree 2>/dev/null || :

    # 7. `core.worktree` may be "absolute or relative to the path to the .git directory"
    #    (git-config(5)). Resolving a relative one against the HOOK'S CWD refused a repository
    #    using git's documented form — the same false refusal step 5 exists to end, one layout
    #    over. It IS resolved now, against the git directory as git-config(5) documents — the
    #    round that wrote "nothing is resolved now: `--show-toplevel` already went through the
    #    declaration" was wrong, and step 8 below is the layout that disproves it.
    local rgd="$d/adm/gd" rwt="$d/rtree"
    mkdir -p "$d/adm" "$rwt/scripts"
    git_q init -q --bare "$rgd" >/dev/null 2>&1
    git_q -C "$rgd" config core.bare false
    git_q -C "$rgd" config core.worktree "../../rtree"
    printf 'seed\n' >"$rwt/seed"
    printf '%s\n' '#!/bin/sh' 'echo "SETUP-RAN-IN:$(cd "$(dirname "$0")/.." && pwd -P)"' \
        >"$rwt/scripts/setup.sh"
    chmod +x "$rwt/scripts/setup.sh"
    (
        cd "$rwt" && export GIT_DIR="$rgd"
        git_q add -A && git_q commit -qm seed
        mkdir -p crates && printf 'x\n' >crates/x.rs
        git_q add -A && git_q commit -qm crates
        git_q branch -q feature && git_q reset -q --hard HEAD~1
    ) >/dev/null 2>&1
    cp "$hook" "$rgd/hooks/post-merge"
    chmod +x "$rgd/hooks/post-merge"
    if [ "$(cd "$rwt" && GIT_DIR="$rgd" git_q rev-parse --show-toplevel 2>/dev/null)" \
         = "$(cd "$rwt" && pwd -P)" ]; then
        out="$(cd "$rwt" && GIT_DIR="$rgd" git_q merge --no-edit feature 2>&1)"
        case "$out" in
            *"belongs to a different repository"*)
                fail "hookenv: relworktree" "a RELATIVE core.worktree was refused as foreign" ;;
            *"running setup.sh"*)
                ok "and git's relative core.worktree form is the ordinary case too" ;;
            *) fail "hookenv: rel" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
    else
        fail "hookenv: rel-premise" "git does not resolve the relative form here, so this tested nothing"
    fi

    # 8. `GIT_COMMON_DIR` disables `core.worktree` — git takes the CWD as the toplevel while
    #    `git config` still reports the declaration (measured on 2.51.1). A round that read the
    #    arm above as "is it set at all" therefore let a leak at one repository build ANOTHER
    #    checkout: end to end, theirs/scripts/setup.sh ran for a merge in mine. The premise is
    #    checked first, so a git that stops ignoring the declaration reports "tested nothing"
    #    rather than passing silently.
    mkdir -p "$d/decl"
    # Step 2 redirected a merge INTO theirs: git wrote the files before the hook refused, so
    # `crates/x.rs` is sitting there untracked and this merge would abort before reaching the
    # hook at all. Cleaned, or the case tests git's overwrite check instead of the guard.
    git_q -C "$d/theirs" clean -qfd >/dev/null 2>&1 || :
    git_q -C "$d/mine" config core.worktree "$d/decl"
    git_q -C "$d/mine" reset -q --hard "$(git_q -C "$d/mine" rev-parse feature~1)"
    if [ "$(cd "$d/theirs" && GIT_DIR="$d/mine/.git" GIT_COMMON_DIR="$d/mine/.git" \
            git_q rev-parse --show-toplevel 2>/dev/null)" = "$(cd "$d/theirs" && pwd -P)" ]; then
        out="$(cd "$d/theirs" && GIT_DIR="$d/mine/.git" GIT_COMMON_DIR="$d/mine/.git" \
               git_q merge --no-edit feature 2>&1)"
        case "$out" in
            *"SETUP-RAN-IN:$(cd "$d/theirs" && pwd -P)"*)
                fail "hookenv: commondir" "a declaration git IGNORED licensed building theirs" ;;
            *"belongs to a different repository"*)
                ok "and a declaration git ignored does not license a foreign checkout" ;;
            *) fail "hookenv: cd" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
    else
        fail "hookenv: cd-premise" "GIT_COMMON_DIR no longer disables core.worktree, so this tested nothing"
    fi
    git_q -C "$d/mine" config --unset core.worktree 2>/dev/null || :

    # 9. THE CONFINEMENT ITSELF, which is the invariant the round-22 design pass added: the
    #    acceptance arm may promote "unestablished" to "same" and must never reach an
    #    established answer. Every earlier step pins a PREDICATE that keeps the arm off a case
    #    it should not take; this one removes the predicate and requires the refusal to survive
    #    anyway. That is the difference between testing the belt and testing the braces — and
    #    the round-20 defect was precisely an arm reaching a verdict that was never in doubt.
    #
    #    The hook is copied and the arm made MAXIMALLY PERMISSIVE in the copy: `declared` is
    #    seeded with `$repo_root`, so its declaration read always succeeds and its equality
    #    always holds. An arm that accepts unconditionally must STILL not change an established
    #    verdict — that is the invariant, stated as a mutation.
    #
    #    This replaced a sed that neutered the arm's `GIT_WORK_TREE` predicate. That predicate
    #    is gone (round 22 read the declaration from the scopes git honours instead, which
    #    closes the forge the predicate was gesturing at), and a mutation whose target no longer
    #    exists is a test of nothing — this one caught its own obsolescence through the premise
    #    check below, which is why the premise check is here. The seed is also the better
    #    mutation: it tests the CONFINEMENT rather than the absence of one predicate, so it
    #    survives any future rewording of the arm's internals.
    local stripped="$d/post-merge.unconfined"
    sed 's/^\( *\)declared=""$/\1declared="$repo_root"/' "$hook" >"$stripped"
    if ! grep -q 'declared="\$repo_root"' "$stripped"; then
        fail "hookenv: strip-premise" "the arm was not made permissive, so this tested nothing"
    else
        git_q -C "$d/theirs" clean -qfd >/dev/null 2>&1 || :
        git_q -C "$d/mine" config core.worktree "$d/theirs"
        git_q -C "$d/mine" reset -q --hard "$(git_q -C "$d/mine" rev-parse feature~1)"
        cp "$stripped" "$d/mine/.git/hooks/post-merge"
        chmod +x "$d/mine/.git/hooks/post-merge"
        out="$(GIT_WORK_TREE="$d/theirs" git_q -C "$d/mine" merge --no-edit feature 2>&1)"
        case "$out" in
            *"SETUP-RAN-IN:$(cd "$d/theirs" && pwd -P)"*)
                fail "hookenv: confinement" \
                     "an unconditionally-accepting arm overrode an ESTABLISHED verdict and built theirs" ;;
            *"belongs to a different repository"*)
                ok "and the arm cannot reach an established verdict even with its guard removed" ;;
            *) fail "hookenv: conf" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
        cp "$hook" "$d/mine/.git/hooks/post-merge"
        chmod +x "$d/mine/.git/hooks/post-merge"
        git_q -C "$d/mine" config --unset core.worktree 2>/dev/null || :
    fi

    # 10. The bare-dotfiles layout: a bare repo plus an exported `GIT_WORK_TREE`. `$repo_root`
    #     IS the correct tree of the repository being merged, but it has no `.git` of its own,
    #     so the scrubbed ask establishes nothing. This must not be spelled as "belongs to a
    #     different repository" — that sentence is false of it, and the remedy it implies would
    #     break the layout. It is the defect this round's design pass was asked to resolve.
    # The tree carries a SPACE on purpose: the refusal below prints a COMMAND, and an
    # unquoted `core.worktree $repo_root` splits there into a value plus a stray argument
    # git rejects. Step 11b runs the printed line, so the quoting is measured and not read.
    local bgd="$d/dotfiles.git" btree="$d/dot home"
    mkdir -p "$btree/scripts"
    git_q init -q --bare "$bgd" >/dev/null 2>&1
    printf 'seed\n' >"$btree/seed"
    printf '%s\n' '#!/bin/sh' 'echo "SETUP-RAN-IN:$(cd "$(dirname "$0")/.." && pwd -P)"' \
        >"$btree/scripts/setup.sh"
    chmod +x "$btree/scripts/setup.sh"
    (
        cd "$btree" && export GIT_DIR="$bgd" GIT_WORK_TREE="$btree"
        git_q add -A && git_q commit -qm seed
        mkdir -p crates && printf 'x\n' >crates/x.rs
        git_q add -A && git_q commit -qm crates
        git_q branch -q feature && git_q reset -q --hard HEAD~1
    ) >/dev/null 2>&1
    cp "$hook" "$bgd/hooks/post-merge"; chmod +x "$bgd/hooks/post-merge"
    out="$(cd "$btree" && GIT_DIR="$bgd" GIT_WORK_TREE="$btree" git_q merge --no-edit feature 2>&1)"
    case "$out" in
        *"belongs to a different repository"*)
            fail "hookenv: dotfiles" "an unestablished identity was reported as a definite foreign one" ;;
        *"could not establish which repository"*)
            case "$out" in
                *"config core.worktree"*)
                    ok "and an identity we could not establish says so, and names the remedy" ;;
                *) fail "hookenv: dotremedy" "the refusal names no remedy" ;;
            esac ;;
        *) fail "hookenv: dot" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac

    # 11a. ...and chore 2 must NOT run — REVERSED, and the reversal is the assertion. This step
    #      used to require the opposite, on the argument that `close-merged` "needs only the
    #      repository the merge was about, never in doubt here since `$hook_common` named it".
    #      MEASURED against the real binary in this very layout, it needs to DISCOVER that
    #      repository from its cwd with `GIT_DIR`/`GIT_WORK_TREE`/`GIT_COMMON_DIR` scrubbed —
    #      the ask that failed to produce a verdict in the first place — and prints
    #
    #          error: not inside a git repo — a task session is a git worktree, so run this from the repo
    #
    #      exiting 1, so the hook adds `jkb: close-merged failed (continuing)`. Two false lines
    #      under a refusal built to be true in every sentence. The stub is kept, and now proves
    #      the absence: a stub `jkb` on PATH reaches the hook, unlike a `git` stub — git prepends
    #      its own `GIT_EXEC_PATH`, which holds a real `git`, but nothing shadows `jkb`. So if
    #      the gate ever comes off, `JKB-RAN` appears and this fails.
    local jbin="$d/jkbbin"
    mkdir -p "$jbin"
    printf '%s\n' '#!/bin/sh' 'echo "JKB-RAN:$*"' >"$jbin/jkb"
    chmod +x "$jbin/jkb"
    ( cd "$btree" && GIT_DIR="$bgd" GIT_WORK_TREE="$btree" git_q reset -q --hard HEAD~1 ) \
        >/dev/null 2>&1
    git_q --git-dir="$bgd" config --unset core.worktree 2>/dev/null || :
    local out11
    out11="$(cd "$btree" && PATH="$jbin:$PATH" GIT_DIR="$bgd" GIT_WORK_TREE="$btree" \
             git_q merge --no-edit feature 2>&1)"
    case "$out11" in
        *"could not establish"*)
            case "$out11" in
                *"JKB-RAN:task close-merged"*)
                    fail "hookenv: closemerged" "close-merged ran in a tree it cannot resolve to \
a repository; measured, it answers 'not inside a git repo ... run this from the repo' and the \
hook prints a second false line under the refusal" ;;
                *"not closing merged tasks"*)
                    ok "and it does not run close-merged, which cannot name this repository either" ;;
                *) fail "hookenv: closemerged-silent" "chore 2 was skipped without saying so; in \
the layout the arm ACCEPTS the pull looks entirely ordinary and the chore silently stops" ;;
            esac ;;
        *) fail "hookenv: closemerged-premise" "the fixture no longer reaches the unestablished \
verdict: $(printf '%s' "$out11" | tr '\n' '|')" ;;
    esac

    # 11. ...and the same verdict must still SKIP. The two cases inside "unestablished" — this
    #     legitimate one and a leak into a directory no repository owns — are indistinguishable
    #     from the repository's own records, so the honest move is to build neither.
    case "$out" in
        *"SETUP-RAN-IN:"*) fail "hookenv: dotbuild" "it built a tree no repository vouches for" ;;
        *) ok "and it builds nothing, because nothing vouches for that tree" ;;
    esac

    # 11b. ...and the remedy must WORK, not merely appear. "Names the remedy" as a substring
    #      check passes on a line that does not parse, which is what an unquoted path with a
    #      space produces. So the printed line is taken from the output and RUN, and the next
    #      pull must reach the ordinary path and build THIS tree.
    #
    #      THE ALIAS IS KEPT THROUGHOUT, and that is the correction round 22 needed. This step
    #      used to drop `GIT_WORK_TREE` for the post-remedy pull, on the reasoning that the
    #      message's parenthetical made it a precondition. It made the step agree with the hook
    #      instead of testing it: with the drop in place the remedy appeared to work, while for
    #      every real user of this layout — whose alias sets the work tree on every command —
    #      it did nothing at all, for ever. Nobody sets up bare dotfiles and then stops using
    #      the alias.
    #      EVERY remedy line is run, in order, not just the first: on a bare repository the
    #      declaration needs `core.bare` cleared ahead of it, and a check that took line one
    #      would have called the incomplete remedy good.
    local remedy ranall=1
    remedy="$(printf '%s\n' "$out" | sed -n 's/^jkb:   //p')"
    if [ -z "$remedy" ]; then
        fail "hookenv: dotremedy-premise" "the refusal printed no remedy line to run"
    elif ! ( export GIT_DIR="$bgd" GIT_WORK_TREE="$btree"
             while IFS= read -r line; do
                 [ -n "$line" ] || continue
                 eval "$line" || exit 1
             done <<<"$remedy" ) >/dev/null 2>&1; then
        fail "hookenv: dotremedy-run" "a printed remedy line does not run: \
$(printf '%s' "$remedy" | tr '\n' '|')"
    else
        : "$ranall"
        ( cd "$btree" && GIT_DIR="$bgd" GIT_WORK_TREE="$btree" git_q reset -q --hard HEAD~1 ) \
            >/dev/null 2>&1
        out="$(cd "$btree" && GIT_DIR="$bgd" GIT_WORK_TREE="$btree" git_q merge --no-edit feature 2>&1)"
        case "$out" in
            *"SETUP-RAN-IN:$btree"*)
                ok "and after the remedy it is the ordinary case, building that very tree"
                # ...ordinary for setup.sh, and NOT for chore 2, which is the half this layout
                # had wrong before anyone looked. The declaration tells the HOOK which tree this
                # is; it tells `jkb` nothing, because jkb rediscovers the repository from the cwd
                # with the selection scrubbed and a `core.worktree` checkout has no `.git` to be
                # found. So the accepted layout must say it is not closing tasks rather than
                # print two false lines about task sessions — and must not do it silently, since
                # everything else about this pull looks completely ordinary.
                case "$out" in
                    *"not closing merged tasks"*)
                        ok "and it still says why it cannot close merged tasks in that layout" ;;
                    *"not inside a git repo"*|*"close-merged failed"*)
                        fail "hookenv: dotclose" "the accepted layout still runs close-merged, \
which cannot resolve this tree to a repository" ;;
                    *) fail "hookenv: dotclose-silent" "chore 2 vanished from an otherwise \
ordinary-looking pull without a word" ;;
                esac ;;
            *"could not establish"*|*"belongs to a different repository"*)
                fail "hookenv: dotremedy-effect" "the remedy ran and changed nothing: \
$(printf '%s' "$out" | tr '\n' '|')" ;;
            *) fail "hookenv: dotremedy-other" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
    fi

    # 12/13. Two layouts that WORK today and nothing pinned — which is how a plausible
    #        "improvement" removes them. The round-22 design pass evaluated replacing the two
    #        asks with `git worktree list` membership and measured that it names the GIT
    #        DIRECTORY rather than the working tree for exactly these layouts, so adopting it
    #        would have refused both while looking like a simplification. A layout with no case
    #        is a layout the next round is free to break.
    local sgd="$d/sep.git" stree="$d/septree"
    mkdir -p "$stree"
    git_q init -q --separate-git-dir="$sgd" "$stree" >/dev/null 2>&1
    if [ ! -f "$stree/.git" ]; then
        fail "hookenv: sep-premise" "this git did not produce a .git FILE, so the layout is not the one named"
    else
        _hookenv_seed "$stree"
        cp "$hook" "$sgd/hooks/post-merge"; chmod +x "$sgd/hooks/post-merge"
        out="$(git_q -C "$stree" merge --no-edit feature 2>&1)"
        case "$out" in
            *"SETUP-RAN-IN:$(cd "$stree" && pwd -P)"*)
                ok "and a checkout whose .git is a FILE is the ordinary case" ;;
            *"belongs to a different repository"*|*"could not establish"*)
                fail "hookenv: sepgit" "a --separate-git-dir checkout was refused: $(printf '%s' "$out" | tr '\n' '|')" ;;
            *) fail "hookenv: sep" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
    fi

    local sup="$d/super"
    mkdir -p "$sup"
    git_q init -q "$sup" >/dev/null 2>&1
    printf 'x\n' >"$sup/f"; git_q -C "$sup" add -A >/dev/null; git_q -C "$sup" commit -qm init >/dev/null
    if git_q -C "$sup" -c protocol.file.allow=always submodule add -q "$d/theirs" sub >/dev/null 2>&1 &&
       [ -f "$sup/sub/.git" ]; then
        _hookenv_seed "$sup/sub"
        local subgd
        subgd="$(git_q -C "$sup/sub" rev-parse --git-dir 2>/dev/null)"
        case "$subgd" in /*) ;; *) subgd="$sup/sub/$subgd" ;; esac
        cp "$hook" "$subgd/hooks/post-merge"; chmod +x "$subgd/hooks/post-merge"
        out="$(git_q -C "$sup/sub" merge --no-edit feature 2>&1)"
        case "$out" in
            *"SETUP-RAN-IN:$(cd "$sup/sub" && pwd -P)"*)
                ok "and a submodule, whose git dir lives under the superproject, is too" ;;
            *"belongs to a different repository"*|*"could not establish"*)
                fail "hookenv: submodule" "a submodule was refused: $(printf '%s' "$out" | tr '\n' '|')" ;;
            *) fail "hookenv: sub" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
    else
        fail "hookenv: sub-premise" "the submodule fixture did not build, so this tested nothing"
    fi

    # 14. THE FLAG SPELLING of the dotfiles alias — `git --git-dir=D --work-tree=T` — which is
    #     the canonical recipe, and which nothing here drove. Inside the hook it is BYTE-
    #     IDENTICAL to the exported form (measured: git rewrites both to `GIT_WORK_TREE=.`
    #     after chdir'ing to the tree), so this is not a second code path — it is a second way
    #     for a user to arrive, and the round-22 must-fix was that the printed remedy did
    #     nothing for either of them. Driven separately anyway, because "identical inside the
    #     hook" is a measurement that can stop being true, and because the alias is what the
    #     step keeps constant across the remedy.
    local fgd="$d/flag.git" ftree="$d/flagtree"
    mkdir -p "$ftree"
    git_q init -q --bare "$fgd" >/dev/null 2>&1
    ( cd "$ftree" && export GIT_DIR="$fgd" GIT_WORK_TREE="$ftree"
      printf 'seed\n' >seed
      mkdir -p scripts
      printf '%s\n' '#!/bin/sh' 'echo "SETUP-RAN-IN:$(cd "$(dirname "$0")/.." && pwd -P)"' \
          >scripts/setup.sh
      chmod +x scripts/setup.sh
      git_q add -A && git_q commit -qm seed
      mkdir -p crates && printf 'x\n' >crates/x.rs
      git_q add -A && git_q commit -qm crates
      git_q branch -q feature && git_q reset -q --hard HEAD~1 ) >/dev/null 2>&1
    cp "$hook" "$fgd/hooks/post-merge"; chmod +x "$fgd/hooks/post-merge"
    out="$(cd "$ftree" && git_q --git-dir="$fgd" --work-tree="$ftree" merge --no-edit feature 2>&1)"
    case "$out" in
        *"could not establish which repository"*)
            local fremedy
            fremedy="$(printf '%s\n' "$out" | sed -n 's/^jkb:   //p')"
            if [ -z "$fremedy" ]; then
                fail "hookenv: flag-premise" "the refusal printed no remedy line"
            elif ! ( while IFS= read -r line; do
                         [ -n "$line" ] || continue
                         eval "$line" || exit 1
                     done <<<"$fremedy" ) >/dev/null 2>&1; then
                fail "hookenv: flag-run" "a remedy line does not run: $(printf '%s' "$fremedy" | tr '\n' '|')"
            else
                ( cd "$ftree" && git_q --git-dir="$fgd" --work-tree="$ftree" reset -q --hard HEAD~1 ) \
                    >/dev/null 2>&1
                # THE SAME ALIAS. A user of this layout does not stop using it because jkb
                # printed something; if the remedy only works once they change how they invoke
                # git, it has not worked.
                out="$(cd "$ftree" && git_q --git-dir="$fgd" --work-tree="$ftree" \
                       merge --no-edit feature 2>&1)"
                case "$out" in
                    *"SETUP-RAN-IN:$(cd "$ftree" && pwd -P)"*)
                        ok "and the --work-tree spelling is fixed by the remedy too, alias unchanged" ;;
                    *"could not establish"*)
                        fail "hookenv: flag-effect" "the remedy ran and the flag form still \
refuses, for ever: $(printf '%s' "$out" | tr '\n' '|')" ;;
                    *) fail "hookenv: flag-other" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
                esac
            fi ;;
        *"SETUP-RAN-IN:"*) fail "hookenv: flag-accept" "an undeclared flag-form tree was built" ;;
        *) fail "hookenv: flag" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac

    # 15. FORGED TESTIMONY. The arm asks the repository whether it declares this tree; a bare
    #     `git config core.worktree` also answers from `-c`, `GIT_CONFIG_COUNT`, `--global` and
    #     `GIT_CONFIG_GLOBAL`, all of which are the CALLER. Measured against the pre-fix hook:
    #     five vectors, five builds of a marker `setup.sh` in a directory no repository owns.
    #
    #     Note what the caller is doing here: git does NOT redirect the work tree for a
    #     command-scope `core.worktree` (measured — the toplevel is unchanged). The redirect is
    #     the cwd; the `-c` only supplies the forged declaration that makes the hook accept it.
    #
    #     The prey directory is pre-populated with its marker BEFORE the merge, because a
    #     fast-forward does not rewrite unchanged paths and a fixture that writes it afterwards
    #     silently tests nothing.
    #     AND THE PREY IS REBUILT BETWEEN SPELLINGS. It is not tidiness: the first successful
    #     forge checks `crates/x.rs` OUT INTO the prey directory, and the next merge then dies
    #     with "untracked working tree files would be overwritten by merge" BEFORE the hook
    #     runs. Measured — with one shared prey, only the first of the four spellings ever
    #     reached the code under test and the other three reported clean having tested nothing,
    #     which is this suite's own recurring defect committed inside the case against it.
    local prey="$d/prey" forged="" reached=0
    _hookenv_prey() {
        rm -rf "$prey"
        mkdir -p "$prey/scripts"
        printf '%s\n' '#!/bin/sh' 'echo "SETUP-RAN-IN:$(cd "$(dirname "$0")/.." && pwd -P)"' \
            >"$prey/scripts/setup.sh"
        chmod +x "$prey/scripts/setup.sh"
    }
    for spelling in dashc envcount envparams global; do
        _hookenv_prey
        git_q -C "$d/mine" reset -q --hard "$(git_q -C "$d/mine" rev-parse feature~1)" >/dev/null 2>&1
        case "$spelling" in
            dashc)     out="$(cd "$prey" && git_q --git-dir="$d/mine/.git" \
                              -c core.worktree="$prey" merge --no-edit feature 2>&1)" ;;
            envcount)  out="$(cd "$prey" && GIT_DIR="$d/mine/.git" GIT_CONFIG_COUNT=1 \
                              GIT_CONFIG_KEY_0=core.worktree GIT_CONFIG_VALUE_0="$prey" \
                              git_q merge --no-edit feature 2>&1)" ;;
            envparams) out="$(cd "$prey" && GIT_DIR="$d/mine/.git" \
                              GIT_CONFIG_PARAMETERS="'core.worktree'='$prey'" \
                              git_q merge --no-edit feature 2>&1)" ;;
            global)    printf '[core]\n\tworktree = %s\n' "$prey" >>"$work/home/.gitconfig"
                       out="$(cd "$prey" && GIT_DIR="$d/mine/.git" git_q merge --no-edit feature 2>&1)"
                       : >"$work/home/.gitconfig"
                       git_q config --global user.email t@example.com
                       git_q config --global user.name "Test" ;;
        esac
        # The hook must have RUN. Without this, a merge that aborted before reaching it — the
        # untracked-file abort above, a fixture typo, anything — reads as "did not build",
        # which is the answer this step is looking for.
        case "$out" in
            *"jkb: "*) reached=$((reached + 1)) ;;
        esac
        case "$out" in
            *"SETUP-RAN-IN:$(cd "$prey" && pwd -P)"*) forged="$forged $spelling" ;;
        esac
    done
    _hookenv_prey
    git_q -C "$d/mine" reset -q --hard feature >/dev/null 2>&1
    if [ "$reached" -ne 4 ]; then
        fail "hookenv: forged-premise" "the hook ran for only $reached of the 4 forge spellings, \
so the rest tested nothing"
    elif [ -n "$forged" ]; then
        fail "hookenv: forged" "the caller forged a declaration and jkb built a tree no \
repository owns, via:$forged"
    else
        ok "and a declaration the CALLER supplied is not the repository declaring anything"
    fi

    # 16. `config.worktree`, which git honours when the LOCAL file turns the extension on. It
    #     is the second of the two files git reads for this key, so the arm reads it — and
    #     without a case that branch is dead code wearing the costume of completeness.
    local wgd="$d/wtc.git" wtree="$d/wtctree"
    mkdir -p "$wtree"
    git_q init -q --bare "$wgd" >/dev/null 2>&1
    ( cd "$wtree" && export GIT_DIR="$wgd" GIT_WORK_TREE="$wtree"
      printf 'seed\n' >seed
      mkdir -p scripts
      printf '%s\n' '#!/bin/sh' 'echo "SETUP-RAN-IN:$(cd "$(dirname "$0")/.." && pwd -P)"' \
          >scripts/setup.sh
      chmod +x scripts/setup.sh
      git_q add -A && git_q commit -qm seed
      mkdir -p crates && printf 'x\n' >crates/x.rs
      git_q add -A && git_q commit -qm crates
      git_q branch -q feature && git_q reset -q --hard HEAD~1 ) >/dev/null 2>&1
    git_q --git-dir="$wgd" config core.bare false
    git_q --git-dir="$wgd" config extensions.worktreeConfig true
    if git_q --git-dir="$wgd" config --worktree core.worktree "$wtree" 2>/dev/null; then
        cp "$hook" "$wgd/hooks/post-merge"; chmod +x "$wgd/hooks/post-merge"
        out="$(cd "$wtree" && GIT_DIR="$wgd" git_q merge --no-edit feature 2>&1)"
        case "$out" in
            *"SETUP-RAN-IN:$(cd "$wtree" && pwd -P)"*)
                ok "and a declaration in config.worktree is the repository's answer too" ;;
            *) fail "hookenv: wtcdecl" "a config.worktree declaration was not honoured: \
$(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
    else
        fail "hookenv: wtc-premise" "this git will not set a --worktree value, so this tested nothing"
    fi

    # 17. ...and an INCLUDED declaration must skip, because git itself ignores it for work-tree
    #     resolution — measured, `rev-parse --show-toplevel` is not redirected by a
    #     `core.worktree` reached through `[include]`. This is the one plausible future edit
    #     that re-opens the forge while looking like consistency: `_hooks_path_read` in lib.sh
    #     reads `core.hooksPath` WITH `--includes`, because git honours that key that way. The
    #     rule is "read it from where git honours it", and the two keys differ.
    local igd="$d/inc.git" itree="$d/inctree"
    mkdir -p "$itree"
    git_q init -q --bare "$igd" >/dev/null 2>&1
    ( cd "$itree" && export GIT_DIR="$igd" GIT_WORK_TREE="$itree"
      printf 'seed\n' >seed
      mkdir -p scripts
      printf '%s\n' '#!/bin/sh' 'echo "SETUP-RAN-IN:$(cd "$(dirname "$0")/.." && pwd -P)"' \
          >scripts/setup.sh
      chmod +x scripts/setup.sh
      git_q add -A && git_q commit -qm seed
      mkdir -p crates && printf 'x\n' >crates/x.rs
      git_q add -A && git_q commit -qm crates
      git_q branch -q feature && git_q reset -q --hard HEAD~1 ) >/dev/null 2>&1
    git_q --git-dir="$igd" config core.bare false
    printf '[core]\n\tworktree = %s\n' "$itree" >"$d/inc.cfg"
    git_q --git-dir="$igd" config include.path "$d/inc.cfg"
    cp "$hook" "$igd/hooks/post-merge"; chmod +x "$igd/hooks/post-merge"
    if [ "$(cd "$itree" && GIT_DIR="$igd" git_q config --includes --get core.worktree 2>/dev/null)" \
         = "$itree" ]; then
        out="$(cd "$itree" && GIT_DIR="$igd" GIT_WORK_TREE="$itree" git_q merge --no-edit feature 2>&1)"
        case "$out" in
            *"SETUP-RAN-IN:"*)
                fail "hookenv: included" "an INCLUDED core.worktree was believed, but git does \
not honour one — the read has drifted onto scopes git ignores" ;;
            *"could not establish"*)
                ok "and an included declaration is not one, because git does not honour it either" ;;
            *) fail "hookenv: inc" "unexpected: $(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
    else
        fail "hookenv: inc-premise" "the include fixture does not carry the value, so this \
tested nothing"
    fi
}

echo "==> scripts/lib.sh::git_hooks_dir + git_hooks_override + reconcile_exclude"
# --- 10m. the old-git fallback answers the same question --show-scope does -----------------
# Two ways the per-scope fallback disagreed with the `--show-scope` branch about ONE repository,
# both silent and both permanent, and neither reachable from any other case.
#
#   * `--worktree` is a distinct scope only with `extensions.worktreeConfig` on. Off, git
#     aliases it to `--local` — and hard-fails 128 in any repo with MORE THAN ONE working tree,
#     which D36 makes the normal state (this checkout has four). Read unconditionally, that
#     layout fact became "cannot be expanded", threw away the value `--local` had found, and
#     wrote no chainer.
#   * A scope flag turns include resolution OFF, while `--show-scope --get-all` leaves it on.
#     So a `core.hooksPath` reached through `[include] path =` — the standard split-gitconfig
#     recipe — read as "the repository stores none", which renders as `dispatch=direct` and
#     prints NOTHING, while git itself resolves the hook perfectly well.
case10m() {
    local d="$work/oldscope" rc v n rcs rcr rcg
    mkdir -p "$d/bin"
    printf '%s\n' '#!/bin/sh' \
        'for a in "$@"; do [ "$a" = "--show-scope" ] && exit 129; done' \
        "exec $(command -v git) \"\$@\"" >"$d/bin/git"
    chmod 755 "$d/bin/git"
    # The premise: the shim really refuses, or every case below tests the modern path twice.
    PATH="$d/bin:$PATH" git config --show-scope --get-all core.hooksPath >/dev/null 2>&1
    [ "$?" -eq 129 ] || { fail "oldscope: premise" "the shim did not refuse --show-scope"; return; }

    ask() { rc=0; v="$(PATH="$d/bin:$PATH" _hooks_path_read "$1" 2>/dev/null)" || rc=$?; }

    git_q init -q "$d/r" >/dev/null 2>&1
    git_q -C "$d/r" commit -q --allow-empty -m init
    git_q -C "$d/r" config core.hooksPath .githooks
    ask "$d/r"
    [ "$rc" = 0 ] && [ "$v" = .githooks ] \
        && ok "the fallback reads a stored core.hooksPath" \
        || fail "oldscope: base" "rc=$rc value='$v'"

    # A SECOND working tree, and nothing else changed.
    git_q -C "$d/r" worktree add -q "$d/w2" -b w2 >/dev/null 2>&1
    if [ -d "$d/w2" ]; then
        ask "$d/r"
        [ "$rc" = 0 ] && [ "$v" = .githooks ] \
            && ok "and a second working tree does not turn a layout fact into 'cannot expand'" \
            || fail "oldscope: worktree" "rc=$rc value='$v' — --worktree's 128 was read as the value's"
    else
        fail "oldscope: wt-setup" "could not add a second worktree"
    fi

    # ...and with the extension ON, --worktree is a real scope and must WIN.
    git_q -C "$d/r" config extensions.worktreeConfig true
    if git_q -C "$d/r" config --worktree core.hooksPath /wt 2>/dev/null; then
        ask "$d/r"
        [ "$rc" = 0 ] && [ "$v" = /wt ] \
            && ok "and with worktreeConfig on, --worktree wins as git resolves it" \
            || fail "oldscope: wtc" "rc=$rc value='$v'"
    else
        # A dropped premise is a dropped assertion, and the case still reported green — the
        # failure mode the two sub-cases above already guard against.
        fail "oldscope: wtc-premise" "could not set a --worktree core.hooksPath, so the arm that \
reads it was never exercised"
    fi

    # The extension is LOCAL-ONLY, and reading it with full precedence had no pin: deleting
    # `--local` left every suite green. A GLOBAL `extensions.worktreeConfig = true` is not this
    # repository's answer, and acting on it sends the loop into `git config --worktree`, which
    # then exits 128 in any repo with more than one working tree and collapses the whole read.
    git_q -C "$d/r" config --unset extensions.worktreeConfig 2>/dev/null || :
    git_q -C "$d/r" config --unset --worktree core.hooksPath 2>/dev/null || :
    git_q -C "$d/r" config core.hooksPath .githooks
    printf '[extensions]\n\tworktreeConfig = true\n' >>"$work/home/.gitconfig"
    ask "$d/r"
    [ "$rc" = 0 ] && [ "$v" = .githooks ] \
        && ok "and a GLOBAL worktreeConfig is not read as this repository's extension" \
        || fail "oldscope: wtc-scope" "rc=$rc value='$v' — the extension was read with full precedence"
    printf '[extensions]\n\tworktreeConfig = false\n' >>"$work/home/.gitconfig"

    # An included file is the same question, and a scope flag turns includes off by default.
    git_q init -q "$d/r2" >/dev/null 2>&1
    printf '[core]\n\thooksPath = /from-include\n' >"$work/home/inc.cfg"
    printf '[include]\n\tpath = %s/inc.cfg\n' "$work/home" >>"$work/home/.gitconfig"
    ask "$d/r2"
    [ "$rc" = 0 ] && [ "$v" = /from-include ] \
        && ok "and a core.hooksPath reached through [include] is found, not reported absent" \
        || fail "oldscope: includes" "rc=$rc value='$v' — --includes is missing from the scoped read"

    # A git that knows neither option answers 129 to EVERY scope. Absorbed into "not set in this
    # scope", a repo with a stored core.hooksPath would read as storing none — which the caller
    # renders as `dispatch=direct` and prints nothing for, the one verdict that is silent. So the
    # refusals are counted: every asked scope refusing means the read FAILED (rc 2), not that
    # nothing is stored.
    printf '%s\n' '#!/bin/sh' \
        'for a in "$@"; do case "$a" in --show-scope|--includes) exit 129;; esac; done' \
        "exec $(command -v git) \"\$@\"" >"$d/bin/git"
    chmod 755 "$d/bin/git"
    PATH="$d/bin:$PATH" git config --includes --get core.hooksPath >/dev/null 2>&1
    if [ "$?" -ne 129 ]; then
        fail "oldscope: nopremise" "the shim did not refuse --includes, so this case tested nothing"
    else
        ask "$d/r"
        # rc 5, its OWN code: "this git could not be asked" is a different fact from "this value
        # cannot be expanded" (rc 2) and needs a different remedy — `--show-origin` would print
        # something perfectly normal here, and the only fix is a newer git.
        [ "$rc" = 5 ] \
            && ok "and a git that refuses every option reads as unestablished, with its own code" \
            || fail "oldscope: allrefused" "rc=$rc value='$v' — want 5 (unaskable git), not 2 (unexpandable value)"
    fi

    # A LOSING scope that cannot expand must not abort the read. `~someuser/` for an account
    # absent on this machine is the ordinary state of a shared ~/.gitconfig, and returning on it
    # reported `dispatch=unreadable` for a repository whose own hooksPath git resolves AND RUNS —
    # measured, `git hook run post-merge` printed the hook's output while this returned 2. The
    # oracle is git itself, so both directions are checked against `rev-parse --git-path`.
    printf '%s\n' '#!/bin/sh' \
        'for a in "$@"; do [ "$a" = "--show-scope" ] && exit 129; done' \
        "exec $(command -v git) \"\$@\"" >"$d/bin/git"
    chmod 755 "$d/bin/git"
    git_q init -q "$d/r3" >/dev/null 2>&1
    git_q -C "$d/r3" config core.hooksPath .githooks
    printf '[core]\n\thooksPath = ~nosuchuser42/hooks\n' >>"$work/home/.gitconfig"
    if [ "$(git_q -C "$d/r3" rev-parse --git-path hooks/post-merge 2>&1)" = ".githooks/post-merge" ]; then
        ask "$d/r3"
        [ "$rc" = 0 ] && [ "$v" = .githooks ] \
            && ok "and a broken value in a LOSING scope does not abort a read git itself resolves" \
            || fail "oldscope: losing" "rc=$rc value='$v' — git resolves this repo's hooksPath, we did not"
    else
        fail "oldscope: losing-premise" "git does not resolve the fixture's hooksPath, so this tested nothing"
    fi
    # RESTORED. `$work/home/.gitconfig` is shared by every case after this one, and an
    # unexpandable global `core.hooksPath` left behind makes them all read rc 2 — a fixture that
    # silently rewrites its successors' premise. The `[include]` case below depends on this
    # being clean, and found it the hard way.
    : >"$work/home/.gitconfig"
    git_q config --global user.email t@example.com
    git_q config --global user.name "Test"

    # ...and a broken value that LOSES INSIDE ONE SCOPE is the same question again. `--path`
    # expands every value it returns, not just the winning one, so a single bad line anywhere in
    # a file failed the whole scope — including the split-config recipe `--includes` exists for:
    # a shared `~/.gitconfig` with an `[include]` whose file corrects it. Measured on 2.51.1:
    # `rev-parse --git-path` answers the good path, so git resolves it and will run hooks there.
    printf '[core]\n\thooksPath = ~nosuchuser42/hooks\n[include]\n\tpath = work.cfg\n' \
        >>"$work/home/.gitconfig"
    printf '[core]\n\thooksPath = %s/goodhooks\n' "$d" >"$work/home/work.cfg"
    mkdir -p "$d/goodhooks"
    git_q init -q "$d/r4" >/dev/null 2>&1
    if [ "$(git_q -C "$d/r4" rev-parse --git-path hooks/post-merge 2>&1)" = "$d/goodhooks/post-merge" ]; then
        ask "$d/r4"
        [ "$rc" = 0 ] && [ "$v" = "$d/goodhooks" ] \
            && ok "and a broken value LOSING INSIDE a scope does not abort it either" \
            || fail "oldscope: inscope" "rc=$rc value='$v' — want $d/goodhooks, as git resolves it"
    else
        fail "oldscope: inscope-premise" "git does not resolve the included hooksPath, so this tested nothing"
    fi

    # ...but when the scope's OWN WINNER is the unexpandable one, git fails and so must we.
    printf '[core]\n\thooksPath = ~nosuchuser42/hooks\n' >>"$work/home/work.cfg"
    if git_q -C "$d/r4" rev-parse --git-path hooks/post-merge >/dev/null 2>&1; then
        fail "oldscope: winner-premise" "git expands the fixture's winning value, so this tested nothing"
    else
        ask "$d/r4"
        [ "$rc" = 2 ] \
            && ok "and a winner git itself will not expand is still refused, with code 2" \
            || fail "oldscope: winner" "rc=$rc value='$v' — git refuses this value; we must too"
    fi
    : >"$work/home/.gitconfig"
    git_q config --global user.email t@example.com
    git_q config --global user.name "Test"

    # ...and the winner must come back INTACT. The arm above re-asks the scope raw and puts its
    # winning value back to git for expansion; the first version wrote that probe with `printf`,
    # and a git config file is not plain text. Measured on 2.51.1: `#` and `;` truncated the
    # value at the comment character, leading whitespace vanished, and a backslash exited 128 —
    # reporting as unexpandable a value git resolves perfectly well, which is the very defect
    # this arm exists to remove. The fixture uses `#` because that one fails SILENTLY, handing
    # the caller a shorter path that exists nowhere; the backslash at least failed loudly.
    local hashdir="$d/ho#oks"
    mkdir -p "$hashdir"
    printf '[core]\n\thooksPath = ~nosuchuser42/hooks\n[include]\n\tpath = work2.cfg\n' \
        >>"$work/home/.gitconfig"
    : >"$work/home/work2.cfg"
    git_q config --file "$work/home/work2.cfg" core.hooksPath "$hashdir"
    if [ "$(git_q -C "$d/r4" rev-parse --git-path hooks/post-merge 2>&1)" = "$hashdir/post-merge" ]; then
        ask "$d/r4"
        [ "$rc" = 0 ] && [ "$v" = "$hashdir" ] \
            && ok "and the scope's winner survives the expansion probe byte-for-byte" \
            || fail "oldscope: fidelity" "rc=$rc value='$v' — want '$hashdir'; the probe file mangled it"
    else
        fail "oldscope: fidelity-premise" "git does not resolve the fixture's '#' hooksPath, so \
this tested nothing"
    fi

    # ...and when the PROBE cannot be built, that is a fact about this machine and not about the
    # value. Collapsed into rc 2 it reported `unreadable core.hooksPath cannot be expanded on
    # this machine` for a value git expands perfectly well, wrote no chainer, and sent the
    # operator to `--show-origin`, which prints an ordinary path — the whole arm silently
    # reverting to the answer it was added to remove, on a full disk or a read-only temp mount.
    # Driven here, because two codes that are never both exercised are one code with two names.
    rc=0
    v="$(TMPDIR=/nonexistent-jkb-probe-dir PATH="$d/bin:$PATH" _hooks_path_read "$d/r4" 2>/dev/null)" \
        || rc=$?
    [ "$rc" = 6 ] \
        && ok "and a probe this machine cannot build is code 6, not 'git will not expand it'" \
        || fail "oldscope: unprobeable" "rc=$rc value='$v' — want 6; a temp-file failure is not \
a verdict about the value"

    # ...and 6 must not OUTLIVE the scope that raised it. The two facts were carried in two
    # flags, and a lower scope's probe failure stayed set while a HIGHER scope went on to find a
    # value git genuinely refuses: the read then answered 6 — "jkb could not create a temporary
    # file" — about a value that had in fact been tested and had failed. Wrong fact, wrong
    # remedy, and the operator is told to free disk space over a broken `~someuser/` path.
    #
    # Driven with a COUNTING `mktemp` shim, because the failure is per-call while `TMPDIR` is
    # not. TRACED, arm by arm, rather than assumed — the first description of this fixture named
    # the wrong decisive scope:
    #
    #   --system (via GIT_CONFIG_SYSTEM)  128, raw re-ask OK, probe 1 -> mktemp FAILS   "no temp file"
    #   --global                          128, raw re-ask OK, probe 2 -> git refuses it  a real break
    #   --local                           128, and the RAW re-ask fatals too             a real break
    #
    # The last line is why `--system` had to be the unprobeable one and is worth stating: a
    # repository whose EFFECTIVE `core.hooksPath` git cannot expand fails the raw re-ask as well,
    # because git expands that value during repository setup — so `--local` can never reach the
    # probe, and it is the highest-precedence break here. Which is the point. The stale flag was
    # set two scopes below and outlived BOTH breaks above it, and neither of those breaks is
    # about a temporary file. The answer must be 2; `$d/mktemp.n` says both probe arms were
    # reached, since a fixture where only one scope probes cannot see a stale flag at all.
    : >"$work/home/.gitconfig"
    git_q config --global user.email t@example.com
    git_q config --global user.name "Test"
    # The repository first, THEN the unexpandable values: `git init` copies the template hooks
    # and fails outright once a global `core.hooksPath` it cannot expand is in place, which left
    # the repo uncreated and every premise below reading a non-repository. Observed here.
    git_q init -q "$d/r5" >/dev/null 2>&1
    printf '[core]\n\thooksPath = ~nosuchuser42/hooks\n' >"$d/sys.cfg"
    printf '[core]\n\thooksPath = ~nosuchuser42/hooks\n' >>"$work/home/.gitconfig"
    cat >"$d/bin/mktemp" <<SHIM
#!/bin/sh
n=\$(cat "$d/mktemp.n" 2>/dev/null || echo 0)
n=\$((n + 1))
printf %s "\$n" >"$d/mktemp.n"
[ "\$n" = 1 ] && exit 1
exec $(command -v mktemp) "\$@"
SHIM
    chmod 755 "$d/bin/mktemp"
    rm -f "$d/mktemp.n"
    # Each status into its OWN variable before either is tested: `[ "$rcs" -ne 128 ]` sets `$?`
    # itself, so a second test reading `$?` reads the FIRST TEST's result and the premise passes
    # on whatever the second command did. Written that way once, in this very block.
    GIT_CONFIG_SYSTEM="$d/sys.cfg" git_q -C "$d/r5" config --system --includes --get-all --path \
        core.hooksPath >/dev/null 2>&1
    rcs=$?
    GIT_CONFIG_SYSTEM="$d/sys.cfg" git_q -C "$d/r5" config --system --includes --get-all \
        core.hooksPath >/dev/null 2>&1
    rcr=$?
    GIT_CONFIG_SYSTEM="$d/sys.cfg" git_q -C "$d/r5" config --global --includes --get-all --path \
        core.hooksPath >/dev/null 2>&1
    rcg=$?
    if [ "$rcs" -ne 128 ] || [ "$rcr" -ne 0 ] || [ "$rcg" -ne 128 ]; then
        fail "oldscope: stale-premise" "both scopes must reach the probe arm for this case to \
mean anything (system path=$rcs raw=$rcr, global path=$rcg)"
    else
        rc=0
        v="$(GIT_CONFIG_SYSTEM="$d/sys.cfg" PATH="$d/bin:$PATH" _hooks_path_read "$d/r5" 2>/dev/null)" \
            || rc=$?
        n="$(cat "$d/mktemp.n" 2>/dev/null || echo 0)"
        if [ "$n" -lt 2 ]; then
            fail "oldscope: stale-probes" "the probe was attempted $n time(s), so one scope never \
reached it and no flag could have gone stale"
        else
            [ "$rc" = 2 ] \
                && ok "and a lower scope's unbuildable probe does not outlive it: the winner git \
refuses is still code 2" \
                || fail "oldscope: stale" "rc=$rc value='$v' — want 2; 6 means the earlier scope's \
'no temp file' answered for a value that WAS tested and failed"
        fi
    fi
    rm -f "$d/bin/mktemp" "$d/mktemp.n" "$d/sys.cfg"
    : >"$work/home/.gitconfig"
    git_q config --global user.email t@example.com
    git_q config --global user.name "Test"
}

run_cases case1 case2 case3 case4 case5 case6 case6b case6c case6d case6p case6n case6g case6m case6k case6h case6j case6i case6e case6f case7 case8 case9 case10 case10b case10c case10d case10e case10f case10g case10h case10i case10j case10k case10l case10m

finish
