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

new_workdir
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
    local d="$work/mode" got before
    mkdir -p "$d"
    chainer_body >"$d/post-merge"
    chmod 644 "$d/post-merge"
    before="$(inode_of "$d/post-merge")"
    got="$(install_chainer "$d/post-merge")"
    # The replacement must be a rename. `cp`/`> "$dest"` leave the inode alone and rewrite it
    # underneath whoever is reading — or executing — the old file, and the mode and content
    # assertions below pass either way, which is how every call-site mutation survived.
    if [ -n "$before" ] && [ "$(inode_of "$d/post-merge")" != "$before" ]; then
        ok "the re-install replaced it by rename, not in place"
    else
        fail "mode: inode" "the destination kept inode $before — it was rewritten in place"
    fi
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
    local d="$work/legacy" got before
    mkdir -p "$d"
    chainer_body_v1 >"$d/post-merge"
    chmod 755 "$d/post-merge"
    before="$(inode_of "$d/post-merge")"
    got="$(install_chainer "$d/post-merge")"
    if [ -n "$before" ] && [ "$(inode_of "$d/post-merge")" != "$before" ]; then
        ok "the upgrade replaced it by rename, not in place"
    else
        fail "legacy: inode" "the destination kept inode $before — it was rewritten in place"
    fi
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
    local d="$work/block" main wt out hook before
    mkdir -p "$d"
    main="$d/main"
    git_q init -q "$main" >/dev/null 2>&1
    git_q -C "$main" commit -q --allow-empty -m init
    git_q -C "$main" worktree add -q "$d/wt" -b side >/dev/null 2>&1
    wt="$d/wt"
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"
    # An EXISTING hook, so the install is a replacement and the inode assertion below has a
    # "before". This is the headline bug's own call site: a pull runs the hook, the hook runs
    # setup.sh, and setup.sh installs that hook — `cp` there rewrites the running script's
    # inode and the shell resumes at a byte offset that no longer means what it meant.
    printf '#!/bin/sh\necho OLD\n' >"$(git_hooks_dir "$wt")/post-merge"
    chmod 755 "$(git_hooks_dir "$wt")/post-merge"
    before="$(inode_of "$(git_hooks_dir "$wt")/post-merge")"

    # Run it the way a session does: against the WORKTREE, which is where the bug lived.
    out="$(install_git_hooks "$wt" "$d/src")"
    hook="${out#repo-hook=}"; hook="${hook%%$'\n'*}"
    if [ "$hook" = "$(git -C "$wt" rev-parse --git-path hooks/post-merge)" ]; then
        ok "install_git_hooks puts the hook where git runs hooks from, from a worktree"
    else
        fail "block: path" "installed at '$hook', git runs '$(git -C "$wt" rev-parse --git-path hooks/post-merge)'"
    fi
    [ -x "$hook" ] && ok "and it is executable" || fail "block: mode" "$hook is not executable"

    if [ -n "$before" ] && [ "$(inode_of "$hook")" != "$before" ]; then
        ok "and replaced the old hook by rename, not in place"
    else
        fail "block: inode" "the hook kept inode $before — it was rewritten under any process running it"
    fi

    # No core.hooksPath here, so git reads .git/hooks itself: no chainer to install. There IS
    # an exclude= line — the sweep runs on every path, which is what stops a block outliving
    # the setting that created it — and it must report nothing to hide.
    case "$out" in
        *chainer=*) fail "block: extra" "reported a chainer with no core.hooksPath: $out" ;;
        *) ok "with no core.hooksPath it installs only the repo hook" ;;
    esac
    case "$out" in
        *"exclude=none (no core.hooksPath"*) ok "and says there is nothing of ours to hide" ;;
        *) fail "block: exclude" "expected exclude=none (no core.hooksPath…), got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    case "$out" in
        *"dispatch=direct"*) ok "and the verdict is that git runs it directly" ;;
        *) fail "block: verdict" "expected dispatch=direct, got: $(printf '%s' "$out" | tr '\n' '|')" ;;
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
    # `exclude=added`/`kept` here would take the opposite position on ownership from the line
    # above: jkb declaring the file not its to touch and then writing a permanent ignore rule
    # for it. Matched on the states, not on the ABSENCE of a key — `exclude=` is emitted on
    # every run that reaches a chainer, so "no exclude line" would be a guard that cannot fire.
    case "$out" in
        *"exclude=added"*|*"exclude=kept"*)
            fail "foreign: excluded" "jkb hid a chainer it had just declared not its own" ;;
        *exclude=*) ok "and it is not added to .git/info/exclude" ;;
        *) fail "foreign: no verdict" "no exclude= line at all: $(printf '%s' "$out" | tr '\n' '|')" ;;
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


# --- 10. the ordinary path, end to end: install -> up-to-date -> refresh -> foreign --------
# Pass 5's must-fix twice over. Nothing drove `install_git_hooks` down the arm that INSTALLS
# a chainer — case 7 has no core.hooksPath, case 8 starts foreign, case 9 is not a repo — so
# mutating lib.sh's `installed|up-to-date|refreshed)` to `never-matches-this)` left 40/40
# assertions ok. That arm is the whole exclusion composition, on the ordinary configuration
# of every machine that sets core.hooksPath globally.
#
# And step 4 is the other must-fix: the exclude rule was written once and never revisited, so
# a user who replaced jkb's chainer with their own hook had it git-ignored for ever — run 2
# said `foreign`, left run 1's rule in place, and `git status` went silent.
#
# Each step also pipes its own report through `render_git_hooks_report`, so the arms setup.sh
# used to hold inline are executed rather than re-parsed by the test.
case10() {
    local d="$work/lifecycle" r out rendered chainer before
    mkdir -p "$d"
    r="$d/repo"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    # RELATIVE, so git resolves the chainer inside the working tree and exclusion is in play.
    git_q -C "$r" config core.hooksPath .githooks
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"
    chainer="$r/.githooks/post-merge"

    # 1. first run — installs the chainer and hides it.
    out="$(install_git_hooks "$r" "$d/src")"
    case "$out" in
        *"chainer=installed $chainer"*) ok "an ordinary core.hooksPath: the chainer is installed" ;;
        *) fail "life: install" "got: $(printf '%s' "$out" | tr '\n' '|')"; return ;;
    esac
    case "$out" in
        *"exclude=added /.githooks/post-merge"*) ok "and hidden from git status" ;;
        *) fail "life: exclude" "expected exclude=added, got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    case "$out" in
        *"dispatch=chained"*) ok "and the verdict is that the repo hook will run" ;;
        *) fail "life: verdict" "expected dispatch=chained, got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    [ -z "$(git_q -C "$r" status --porcelain)" ] \
        && ok "so the tree is clean and a session can land" \
        || fail "life: dirty" "still dirty: $(git_q -C "$r" status --porcelain | tr '\n' ' ')"
    rendered="$(printf '%s\n' "$out" | render_git_hooks_report 2>&1)"
    case "$rendered" in
        *"core.hooksPath is set, so this is required"*"added to .git/info/exclude"*)
            ok "and setup.sh's rendering of it says both" ;;
        *) fail "life: render install" "rendered: $(printf '%s' "$rendered" | tr '\n' '|')" ;;
    esac

    # 2. second run — idempotent, and the rule is not duplicated.
    out="$(install_git_hooks "$r" "$d/src")"
    case "$out" in
        *"chainer=up-to-date"*"exclude=kept /.githooks/post-merge"*)
            ok "running it again: up-to-date, rule kept" ;;
        *) fail "life: idempotent" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    [ "$(grep -c '^/\.githooks/post-merge$' "$r/.git/info/exclude")" = "1" ] \
        && ok "and the pattern appears exactly once" \
        || fail "life: duplicate" "appears $(grep -c '^/\.githooks/post-merge$' "$r/.git/info/exclude") times"

    # 3. a chainer from before the worktree fix — the `refreshed` outcome word, by rename.
    chainer_body_v1 >"$chainer"; chmod 755 "$chainer"
    before="$(inode_of "$chainer")"
    out="$(install_git_hooks "$r" "$d/src")"
    case "$out" in
        *"chainer=refreshed"*) ok "an older chainer is refreshed through the block" ;;
        *) fail "life: refresh" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    [ -n "$before" ] && [ "$(inode_of "$chainer")" != "$before" ] \
        && ok "and by rename, so a running chainer keeps its bytes" \
        || fail "life: refresh inode" "the chainer kept inode $before"

    # 4. the user replaces it with their own hook. jkb must stop hiding it.
    printf '#!/bin/sh\n# my own post-merge\ndirenv reload\n' >"$chainer"
    out="$(install_git_hooks "$r" "$d/src")"
    case "$out" in
        *"chainer=foreign"*"exclude=retracted /.githooks/post-merge"*)
            ok "replacing the chainer: foreign, and the exclude rule is retracted" ;;
        *) fail "life: retract" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    if [ -n "$(git_q -C "$r" status --porcelain)" ]; then
        ok "so the user's own hook is visible to git again"
    else
        fail "life: still hidden" "the user's hook is still hidden by a rule jkb wrote"
    fi
    grep -q 'direnv reload' "$chainer" \
        && ok "and their file was not touched" \
        || fail "life: clobbered" "the user's hook was overwritten"
    # Exactly one exclude line: the retraction. Falling through to the "is anything else
    # hiding it?" question after retracting our own block would add a second, contradictory
    # line about the same pattern.
    [ "$(printf '%s\n' "$out" | grep -c '^exclude=')" = "1" ] \
        && ok "and says one thing about that pattern, not two" \
        || fail "life: two exclude lines" "$(printf '%s\n' "$out" | grep '^exclude=' | tr '\n' '|')"
    case "$out" in
        *"dispatch=unknown"*) ok "and the verdict is unknown, not dead — a foreign chainer may dispatch" ;;
        *) fail "life: foreign verdict" "expected dispatch=unknown, got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    rendered="$(printf '%s\n' "$out" | render_git_hooks_report 2>&1)"
    case "$rendered" in
        *"was not written by jkb"*"dropped from .git/info/exclude"*"the repo hook never runs"*)
            ok "and the rendering says all three" ;;
        *) fail "life: render foreign" "rendered: $(printf '%s' "$rendered" | tr '\n' '|')" ;;
    esac
}

# --- 10c. the sweep: a block is reconciled wherever core.hooksPath goes next ---------------
# Reconciling only the path being installed was not reconciliation. Change `core.hooksPath`
# and the old block stayed for ever; unset it and the early return meant no `exclude=` line
# was emitted at all, so nothing was ever retracted. A hook the user later wrote at either
# old path was then invisible to `git status` with nothing attributing that to jkb — which is
# the harm the whole function exists to end.
case10c() {
    local d="$work/sweep" r out
    mkdir -p "$d"
    r="$d/repo"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"

    git_q -C "$r" config core.hooksPath .githooks
    install_git_hooks "$r" "$d/src" >/dev/null

    # Moved.
    git_q -C "$r" config core.hooksPath .otherhooks
    out="$(install_git_hooks "$r" "$d/src")"
    case "$out" in
        *"exclude=retracted /.githooks/post-merge"*) ok "moving core.hooksPath retracts the old block" ;;
        *) fail "sweep: move" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    case "$out" in
        *"exclude=added /.otherhooks/post-merge"*) ok "and adds the new one" ;;
        *) fail "sweep: new" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    [ "$(grep -c '^/\.githooks/post-merge$' "$r/.git/info/exclude")" = "0" ] \
        && ok "and the old pattern is gone from the file" \
        || fail "sweep: stale" "file: $(tr '\n' '|' <"$r/.git/info/exclude")"

    # Unset entirely: git reads .git/hooks itself, so nothing of ours should remain.
    git_q -C "$r" config --unset core.hooksPath
    out="$(install_git_hooks "$r" "$d/src")"
    case "$out" in
        *"exclude=retracted /.otherhooks/post-merge"*) ok "unsetting it retracts the last block too" ;;
        *) fail "sweep: unset" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    if ! grep -q '^# jkb:' "$r/.git/info/exclude"; then
        ok "and no marker of ours is left in the file"
    else
        fail "sweep: marker" "file: $(tr '\n' '|' <"$r/.git/info/exclude")"
    fi
    # A hook the user now writes at an old path must be visible to them. Asserted against
    # `check-ignore` on THAT path, not against `git status` being non-empty: step 2 left
    # `.otherhooks/` untracked, so the porcelain output was already non-empty and the
    # assertion could not have failed.
    mkdir -p "$r/.githooks"; printf '#!/bin/sh\ndirenv reload\n' >"$r/.githooks/post-merge"
    if git_q -C "$r" check-ignore -q .githooks/post-merge; then
        fail "sweep: hidden" "a hook at the old path is still ignored"
    else
        ok "so a hook they write at an old path is visible to git again"
    fi
}

# --- 10d. an unresolvable core.hooksPath is not reported as the good verdict ---------------
# `git config --get` exits 1 for "not set" and 128 for "set to something I cannot expand" —
# a `~someuser/hooks` for an account absent on this machine, the ordinary state of a shared
# ~/.gitconfig. Folding the second into the first reported `dispatch=direct`, which the
# renderer prints nothing for: a clean bill of health for a repo in which git resolves no
# hooks path at all.
case10d() {
    local d="$work/unreadable" r out rendered user
    mkdir -p "$d"
    r="$d/repo"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"
    user="jkb-no-such-account-$$"
    git_q -C "$r" config core.hooksPath "~$user/hooks"
    if git -C "$r" config --get --path core.hooksPath >/dev/null 2>&1; then
        skip "this git expands ~$user without failing"
        return
    fi

    out="$(install_git_hooks "$r" "$d/src" 2>/dev/null)"
    case "$out" in
        *"dispatch=unreadable"*) ok "a core.hooksPath git cannot resolve: reported unreadable" ;;
        *) fail "unreadable: verdict" "got: $(printf '%s' "$out" | tr '\n' '|')"; return ;;
    esac
    case "$out" in
        *"dispatch=direct"*) fail "unreadable: direct" "reported the good verdict for a repo git runs no hooks in" ;;
        *) ok "and not as direct, which is rendered silently" ;;
    esac
    rendered="$(printf '%s\n' "$out" | render_git_hooks_report 2>&1)"
    case "$rendered" in
        *"git runs NO hooks in this repository"*) ok "and the rendering says so" ;;
        *) fail "unreadable: render" "rendered: $(printf '%s' "$rendered" | tr '\n' '|')" ;;
    esac
}

# --- 10e. the same directory, spelled differently -----------------------------------------
# "core.hooksPath points at git's own hooks directory" was literal string equality, so a
# trailing slash — or a symlink, or a `..` — put back the message this guard was added to
# remove: jkb calling the hook it wrote microseconds earlier foreign, and warning that the
# repo hook may never run, about a configuration whose true verdict is `direct`.
case10e() {
    local d="$work/spelling" r out
    mkdir -p "$d"
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"
    for spelling in trailing-slash dotdot; do
        r="$d/$spelling"
        git_q init -q "$r" >/dev/null 2>&1
        git_q -C "$r" commit -q --allow-empty -m init
        case "$spelling" in
            trailing-slash) git_q -C "$r" config core.hooksPath "$(git_hooks_dir "$r")/" ;;
            dotdot)         git_q -C "$r" config core.hooksPath "$(git_hooks_dir "$r")/../hooks" ;;
        esac
        out="$(install_git_hooks "$r" "$d/src" 2>/dev/null)"
        case "$out" in
            *"dispatch=direct"*) ok "core.hooksPath as $spelling: still direct" ;;
            *) fail "spelling: $spelling" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
        esac
        case "$out" in
            *chainer=foreign*) fail "spelling: $spelling foreign" "it called its own hook foreign" ;;
            *) ok "and it does not call its own hook foreign" ;;
        esac
    done
}

# --- 11. the report survives `set -e`, and never claims a hook was skipped -----------------
# Two findings. The report used to reach setup.sh only because the call happened to be
# written `… || true`, which disables `set -e` for the whole function body; without it the
# subshell died inside `install_chainer` and setup.sh printed the repo hook and nothing else,
# while core.hooksPath was set and that hook was therefore dead. And the failure was reported
# as `error=`, which setup.sh renders "skipping hook install" — directly under the line
# saying the hook was installed.
case11() {
    local d="$work/seterr" r out rendered
    if [ "$(id -u)" = "0" ]; then
        skip "an unwritable directory cannot be simulated as root"
        return
    fi
    mkdir -p "$d"
    r="$d/repo"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    mkdir -p "$d/ro"
    git_q -C "$r" config core.hooksPath "$d/ro"
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"
    # Exists, so `mkdir -p` succeeds; unwritable, so install_exec's mktemp fails.
    chmod 555 "$d/ro"

    # A CHILD shell with `set -euo pipefail`, which is what setup.sh runs under. Sourcing the
    # harness under `set -e` is not an option — `fail` increments and continues by design —
    # so the difference is covered here rather than assumed.
    out="$(bash -euo pipefail -c '. "$1/scripts/lib.sh"; install_git_hooks "$2" "$3"' _ \
        "$repo_root" "$r" "$d/src" 2>/dev/null)"
    chmod 755 "$d/ro"

    case "$out" in
        *repo-hook=*) ok "under set -e the repo hook is still reported" ;;
        *) fail "seterr: hook" "got: $(printf '%s' "$out" | tr '\n' '|')"; return ;;
    esac
    case "$out" in
        *"chainer=failed"*) ok "and so is the chainer failure, instead of the shell dying on it" ;;
        *) fail "seterr: chainer" "the report stopped early: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    case "$out" in
        *"dispatch=dead"*) ok "and the verdict says the hook just installed will not run" ;;
        *) fail "seterr: verdict" "expected dispatch=dead, got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    # `error=` means nothing was done, so it must never follow a line saying something was.
    case "$out" in
        *error=*) fail "seterr: error=" "a chainer failure was reported as error=, which renders 'skipping hook install'" ;;
        *) ok "and it is not reported as error=, which would say the install was skipped" ;;
    esac
    rendered="$(printf '%s\n' "$out" | render_git_hooks_report 2>&1)"
    case "$rendered" in
        *"skipping hook install"*)
            fail "seterr: render" "the rendering says the hook install was skipped, under a line saying it was installed" ;;
        *"git will NOT run the repo hook above"*)
            ok "and the rendering states the consequence" ;;
        *) fail "seterr: render" "rendered: $(printf '%s' "$rendered" | tr '\n' '|')" ;;
    esac
}

# --- 10b. the two verdicts the world can contradict ---------------------------------------
# `dispatch=` is derived from what is actually at the chainer path, and both of these were
# wrong when first written. `core.hooksPath` pointing at the directory git would have used
# anyway is not a chainer situation — the hook just installed is the one git runs — and
# reporting it as `foreign` warns that a file jkb wrote seconds earlier was not written by
# jkb. And `[ -x ]` is true of a DIRECTORY, so a directory-style hook manager's `post-merge/`
# was called `unknown` ("this may well dispatch") when it is the one case that provably
# cannot.
case10b() {
    local d="$work/verdicts" r out
    mkdir -p "$d"

    r="$d/selfpath"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"
    git_q -C "$r" config core.hooksPath "$(git_hooks_dir "$r")"
    out="$(install_git_hooks "$r" "$d/src")"
    case "$out" in
        *"dispatch=direct"*) ok "core.hooksPath aimed at git's own hooks directory: direct" ;;
        *) fail "verdict: selfpath" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    case "$out" in
        *chainer=*) fail "verdict: selfpath chainer" "it reported a chainer against the hook it had just installed" ;;
        *) ok "and no chainer is claimed against the hook it just installed" ;;
    esac

    r="$d/dirchainer"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    mkdir -p "$r/.githooks/post-merge"      # a directory-style hook manager owns the path
    out="$(install_git_hooks "$r" "$d/src" 2>/dev/null)"
    case "$out" in
        *"dispatch=dead"*) ok "a directory where the chainer goes: dead, not unknown" ;;
        *) fail "verdict: dirchainer" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
}

# --- 11b. the override directory itself cannot be created ---------------------------------
# Concern 4's own reproduction (`core.hooksPath=/etc/githooks-nope`, a root-owned parent).
# This arm printed `error=cannot create …` AFTER `repo-hook=`, and setup.sh renders `error=`
# as "skipping hook install" — so it said the hook was installed and then that the install
# was skipped, while the state that actually mattered (core.hooksPath is set, no chainer
# exists, so git will never run the hook just reported) was stated nowhere.
case11b() {
    local d="$work/nomkdir" r out rendered
    if [ "$(id -u)" = "0" ]; then
        skip "an uncreatable directory cannot be simulated as root"
        return
    fi
    mkdir -p "$d/parent"
    r="$d/repo"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath "$d/parent/githooks"
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"
    chmod 555 "$d/parent"

    out="$(install_git_hooks "$r" "$d/src" 2>/dev/null)"
    chmod 755 "$d/parent"

    case "$out" in
        *repo-hook=*"chainer=failed"*) ok "an uncreatable hooks directory: the chainer is reported failed" ;;
        *) fail "nomkdir: report" "got: $(printf '%s' "$out" | tr '\n' '|')"; return ;;
    esac
    case "$out" in
        *error=*) fail "nomkdir: error=" "reported error= after repo-hook=, which renders 'skipping hook install'" ;;
        *) ok "and not as error=, which claims nothing was done" ;;
    esac
    case "$out" in
        *"dispatch=dead"*) ok "and the verdict says the repo hook will not run" ;;
        *) fail "nomkdir: verdict" "expected dispatch=dead, got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    rendered="$(printf '%s\n' "$out" | render_git_hooks_report 2>&1)"
    case "$rendered" in
        *"skipping hook install"*) fail "nomkdir: render" "the rendering contradicts the line above it" ;;
        *"git will NOT run the repo hook above"*) ok "and the rendering says so where a person reads it" ;;
        *) fail "nomkdir: render" "rendered: $(printf '%s' "$rendered" | tr '\n' '|')" ;;
    esac
}

# --- 11c. the SUCCESS path under `set -e` too ---------------------------------------------
# case11 and case11b both make `install_chainer` fail, so `install_git_hooks` takes
# `want=skip` and never calls `reconcile_exclude` at all — including its retraction, which
# `cp -p`s and `mv`s over a file of the user's rules and is the newest, most write-dangerous
# code here. The parity case11's own comment claims ("covered here rather than assumed") did
# not cover it.
case11c() {
    local d="$work/seterrok" r out
    mkdir -p "$d"
    r="$d/repo"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"

    out="$(bash -euo pipefail -c '
        . "$1/scripts/lib.sh"
        install_git_hooks "$2" "$3"                       # install + add
        install_git_hooks "$2" "$3"                       # up-to-date + kept
        printf "#!/bin/sh\ndirenv reload\n" >"$2/.githooks/post-merge"
        install_git_hooks "$2" "$3"                       # foreign + RETRACTION
    ' _ "$repo_root" "$r" "$d/src" 2>/dev/null)"

    case "$out" in
        *"exclude=added"*"exclude=kept"*"exclude=retracted"*)
            ok "the whole lifecycle runs to completion under set -euo pipefail" ;;
        *) fail "seterrok: lifecycle" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    [ "$(printf '%s\n' "$out" | grep -c '^dispatch=')" = "3" ] \
        && ok "and every run emits its verdict rather than dying part-way" \
        || fail "seterrok: verdicts" "got $(printf '%s\n' "$out" | grep -c '^dispatch=') verdicts from 3 runs"
}

# --- 11d. a failed chainer install still sweeps the blocks it is not undecided about -------
# MF3. The `want=skip` arm returned without reconciling at all, so a stale block for a path
# that is no longer the chainer's survived for ever — and the failing precondition is itself
# persistent, so "the next successful run reconciles it" never comes. A failed install decides
# nothing about THIS pattern; it decides nothing about the others either, and they need no
# decision. The reconcile now sits below every arm rather than inside one.
case11d() {
    local d="$work/skipsweep" r out
    if [ "$(id -u)" = "0" ]; then
        skip "an unwritable directory cannot be simulated as root"
        return
    fi
    mkdir -p "$d"
    r="$d/repo"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"
    # A stale block from a hooksPath that is no longer in use.
    printf '%s\n/.oldhooks/post-merge\n' "$(exclude_marker)" >>"$r/.git/info/exclude"
    # A core.hooksPath INSIDE the tree — so there is a pattern, and `undecided` is reachable
    # — whose directory exists but cannot be written, so install_chainer fails.
    git_q -C "$r" config core.hooksPath .githooks
    mkdir -p "$r/.githooks"; chmod 555 "$r/.githooks"

    out="$(install_git_hooks "$r" "$d/src" 2>/dev/null)"
    chmod 755 "$r/.githooks"

    case "$out" in
        *"chainer=failed"*) ok "a chainer that cannot be installed is still reported failed" ;;
        *) fail "skipsweep: chainer" "got: $(printf '%s' "$out" | tr '\n' '|')"; return ;;
    esac
    case "$out" in
        *"exclude=retracted /.oldhooks/post-merge"*) ok "and the stale block is swept anyway" ;;
        *) fail "skipsweep: sweep" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    case "$out" in
        *"nothing was decided about /.githooks/post-merge"*)
            ok "while this run's own pattern is left undecided, not retracted" ;;
        *) fail "skipsweep: undecided" "got: $(printf '%s' "$out" | tr '\n' '|')" ;;
    esac
    grep -qxF '/.oldhooks/post-merge' "$r/.git/info/exclude" \
        && fail "skipsweep: file" "the stale rule is still in the file" \
        || ok "and it is gone from the file"
}

# --- 12. an exclusion that could not be written says so -----------------------------------
# It used to return success with no output — a failed exclusion spelled exactly like "nothing
# needed". The tree then read dirty, `jkb task land` refused it, and nothing attributed the
# dirt to jkb. `Unknown` is never spelled `no`, and neither is `failed`.
case12() {
    local d="$work/excludefail" r out out2 rendered
    if [ "$(id -u)" = "0" ]; then
        skip "an unwritable file cannot be simulated as root"
        return
    fi
    mkdir -p "$d"
    r="$d/repo"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    printf '#!/bin/sh\necho HOOK\n' >"$d/src"
    : >"$r/.git/info/exclude"
    chmod 444 "$r/.git/info/exclude"

    out="$(install_git_hooks "$r" "$d/src" 2>/dev/null)"
    chmod 644 "$r/.git/info/exclude"

    # And again with content and NO trailing newline, so the write that fails is the separator
    # rather than the block — a different arm, and the one that would otherwise fuse the
    # user's last rule with our marker if it silently carried on.
    printf '# my rules\n*.log' >"$r/.git/info/exclude"
    chmod 444 "$r/.git/info/exclude"
    out2="$(install_git_hooks "$r" "$d/src" 2>/dev/null)"
    chmod 644 "$r/.git/info/exclude"
    case "$out2" in
        *"exclude=failed"*) ok "and so is one whose separator cannot be written" ;;
        *) fail "excludefail: separator" "got: $(printf '%s' "$out2" | tr '\n' '|')" ;;
    esac
    [ "$(cat "$r/.git/info/exclude")" = "$(printf '# my rules\n*.log')" ] \
        && ok "and the user's rules are exactly as they were" \
        || fail "excludefail: damaged" "file is now: $(tr '\n' '|' <"$r/.git/info/exclude")"

    case "$out" in
        *"exclude=failed"*) ok "an exclude file that cannot be written is reported as failed" ;;
        *) fail "excludefail: state" "got: $(printf '%s' "$out" | tr '\n' '|')"; return ;;
    esac
    rendered="$(printf '%s\n' "$out" | render_git_hooks_report 2>&1)"
    case "$rendered" in
        *"could not update .git/info/exclude"*"jkb task land"*)
            ok "and the rendering names the consequence a session will hit" ;;
        *) fail "excludefail: render" "rendered: $(printf '%s' "$rendered" | tr '\n' '|')" ;;
    esac
}

# --- 12b. the other two write failures, on both sides of the reconciliation ---------------
# The add side has a second failure (the `.git/info` directory itself) and the retract side
# has its own (the temp copy it rewrites through). Both were arms nothing drove. The retract
# one matters most: it rewrites a file full of the USER'S rules, so "it did not land" and "it
# landed halfway" must be distinguishable, and only the first is acceptable.
case12b() {
    local d="$work/writefail" r got before_entries before_inode
    if [ "$(id -u)" = "0" ]; then
        skip "unwritable paths cannot be simulated as root"
        return
    fi
    mkdir -p "$d"

    # add side: .git/info is not a directory, so it cannot be created or written into.
    r="$d/noinfo"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    mkdir -p "$r/.githooks"; printf '#!/bin/sh\nexit 0\n' >"$r/.githooks/post-merge"
    rm -rf "$r/.git/info"; : >"$r/.git/info"
    got="$(reconcile_exclude "$r" "/.githooks/post-merge" yes 2>/dev/null)"
    case "$got" in
        exclude=failed*) ok "an exclude directory that cannot be created is reported as failed" ;;
        *) fail "writefail: add" "expected a failed state, got '$got'" ;;
    esac

    # retract side, the write itself: the directory is writable and the FILE is not, so
    # `cp -p` succeeds and hands the temp its 444 mode, and the `printf` that fills it fails.
    # That is the exact line whose status used to be dropped — a partial write (a full disk,
    # a quota) was renamed over every rule the user owns, under the word `retracted`.
    r="$d/rofile"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    mkdir -p "$r/.githooks"; printf '#!/bin/sh\nexit 0\n' >"$r/.githooks/post-merge"
    reconcile_exclude "$r" "/.githooks/post-merge" yes >/dev/null
    before_entries="$(entries_in "$r/.git/info")"
    before_inode="$(inode_of "$r/.git/info/exclude")"
    chmod 444 "$r/.git/info/exclude"
    got="$(reconcile_exclude "$r" "/.githooks/post-merge" no 2>/dev/null)"
    chmod 644 "$r/.git/info/exclude"
    case "$got" in
        exclude=failed*) ok "a rewrite whose write fails is reported as failed, not retracted" ;;
        *) fail "writefail: rewrite" "expected a failed state, got '$got'" ;;
    esac
    # NOT "the file still holds every rule": `cp -p` fills the temp with the old content, so
    # a dropped status would `mv` that same content back and the file would look untouched
    # either way. The state word is the assertion that can fail here, and it does — reverting
    # the checked write reports `retracted` above. What is asserted instead is the property
    # this fixture CAN establish: the destination was not replaced at all.
    [ "$before_inode" = "$(inode_of "$r/.git/info/exclude")" ] \
        && ok "and the destination was not replaced" \
        || fail "writefail: replaced" "the exclude file was swapped despite the failed write"
    [ "$(entries_in "$r/.git/info")" = "$before_entries" ] \
        && ok "and no temp file survives that either" \
        || fail "writefail: rewrite temp" "left: $(entries_in "$r/.git/info" | tr '\n' ' ')"

    # retract side, the copy: our block is there, and the directory is read-only.
    r="$d/roinfo"
    git_q init -q "$r" >/dev/null 2>&1
    git_q -C "$r" commit -q --allow-empty -m init
    git_q -C "$r" config core.hooksPath .githooks
    mkdir -p "$r/.githooks"; printf '#!/bin/sh\nexit 0\n' >"$r/.githooks/post-merge"
    reconcile_exclude "$r" "/.githooks/post-merge" yes >/dev/null
    before_entries="$(entries_in "$r/.git/info")"
    chmod 555 "$r/.git/info"
    got="$(reconcile_exclude "$r" "/.githooks/post-merge" no 2>/dev/null)"
    chmod 755 "$r/.git/info"
    case "$got" in
        exclude=failed*) ok "a retraction that cannot be written is reported as failed" ;;
        *) fail "writefail: retract" "expected a failed state, got '$got'" ;;
    esac
    if grep -qxF '/.githooks/post-merge' "$r/.git/info/exclude"; then
        ok "and the rule is left whole rather than half-removed"
    else
        fail "writefail: partial" "the file was rewritten anyway: $(tr '\n' '|' <"$r/.git/info/exclude")"
    fi
    # The directory's WHOLE contents, not a search for the template `reconcile_exclude`
    # happens to use — an assertion that knows the temp name passes however many files are
    # stranded once the name changes. harness.sh:88 records this exact lesson.
    [ "$(entries_in "$r/.git/info")" = "$before_entries" ] \
        && ok "and no temp file is left beside it" \
        || fail "writefail: temp" "before: $(printf '%s' "$before_entries" | tr '\n' ' ')/ after: $(entries_in "$r/.git/info" | tr '\n' ' ')"
}

# --- 13. a key with no render arm is not swallowed ----------------------------------------
# The default arms. A key added to the producer without an arm here used to vanish, which is
# how an `error=` line contradicting the line above it stayed invisible for three passes.
case13() {
    local rendered entry line want ok_all=1 missing=""
    # EVERY default arm, one per key. Covering only the outer one left three inner arms that
    # could be deleted with the suites green — which is the shape of the defect this whole
    # change is about, reintroduced in the guard against it.
    local -a table=(
        'invented=1|unrecognised report line: invented=1'
        'chainer=sideways /x|unrecognised chainer outcome'
        'exclude=sideways /x|unrecognised exclude state'
        'dispatch=sideways /x|unrecognised dispatch verdict'
    )
    for entry in "${table[@]}"; do
        line="${entry%%|*}"; want="${entry#*|}"
        rendered="$(printf '%s\n' "$line" | render_git_hooks_report 2>&1)"
        case "$rendered" in
            *"$want"*) ;;
            *) ok_all=0; missing="$missing [$line -> '$rendered']" ;;
        esac
    done
    [ "$ok_all" = 1 ] \
        && ok "an unknown key or value is warned about on every arm, not swallowed" \
        || fail "render: defaults" "swallowed:$missing"
}

# --- 14. every state in the protocol renders something that names it ----------------------
# The producer emits states the end-to-end cases above cannot all reach in one run — a `kept`
# rule, an `unowned` one, an `error=`. An arm nothing drives is a branch wearing the costume
# of a safeguard, and the whole reason both halves moved into lib.sh was that setup.sh's arms
# were reachable from nothing. Driven as a table so the closed protocol and the closed set of
# render arms are asserted to be the same set.
case14() {
    local line want rendered
    # Each entry is `<report line>|<a phrase only that arm produces>`.
    local -a table=(
        'repo-hook=/r/.git/hooks/post-merge|repo hook:  /r/.git/hooks/post-merge'
        'chainer=installed /c|core.hooksPath is set, so this is required'
        'chainer=up-to-date /c|(up to date)'
        'chainer=refreshed /c|(refreshed)'
        'chainer=foreign /c|was not written by jkb'
        'chainer=failed /c|could not install the chainer at /c'
        'exclude=added /p|added to .git/info/exclude'
        'exclude=kept /p|already in .git/info/exclude'
        'exclude=retracted /p|dropped from .git/info/exclude'
        'exclude=deduplicated /p|duplicate jkb entries for /p removed'
        'exclude=tidied 1 orphaned marker(s)|1 orphaned marker(s) removed from .git/info/exclude'
        'exclude=exposed (inside the worktree at /w)|that working tree will read dirty'
        'exclude=unowned /p|cannot prove it wrote'
        'exclude=failed (cannot write /e)|could not update .git/info/exclude'
        'dispatch=unknown /c|the repo hook never runs'
        'dispatch=dead /c|will NOT run the repo hook above'
        'dispatch=unreadable core.hooksPath|git runs NO hooks in this repository'
        'error=not a git repo|not a git repo; skipping hook install'
    )
    local entry ok_all=1 missing=""
    for entry in "${table[@]}"; do
        line="${entry%%|*}"; want="${entry#*|}"
        rendered="$(printf '%s\n' "$line" | render_git_hooks_report 2>&1)"
        case "$rendered" in
            *"$want"*) ;;
            *) ok_all=0; missing="$missing [$line -> '$rendered']" ;;
        esac
    done
    [ "$ok_all" = 1 ] \
        && ok "every state the producer can emit has a render arm that names it" \
        || fail "render: table" "no distinctive output for:$missing"

    # The two silent verdicts are silent ON PURPOSE — the lines above them already said the
    # hook will run — so assert the silence rather than leaving it unstated. Its OWN
    # accumulators: sharing the pair above meant one table failure also fired a second,
    # bogus "expected silence" failure listing arms that had behaved exactly as required.
    local quiet_ok=1 noisy=""
    for line in 'dispatch=direct' 'dispatch=chained /c' 'exclude=none (nothing is hiding it)'; do
        rendered="$(printf '%s\n' "$line" | render_git_hooks_report 2>&1)"
        [ -z "$rendered" ] || { quiet_ok=0; noisy="$noisy [$line said '$rendered']"; }
    done
    [ "$quiet_ok" = 1 ] \
        && ok "and the states with nothing to report say nothing" \
        || fail "render: silence" "expected silence:$noisy"
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
case10
case10b
case10c
case10d
case10e
case11
case11b
case11c
case11d
case12
case12b
case13
case14

finish
