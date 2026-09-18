#!/usr/bin/env bash
# Assert the container is what it claims to be (design D49). Run inside the container.
#
# A configuration nobody checks is a configuration nobody knows the state of, and every property
# here is one somebody could remove by editing a single line of container.json — a mount added,
# `remoteUser` dropped, the seccomp profile path typo'd (Docker fails loudly on a missing profile,
# but not on one that no longer contains what it should). Each assertion below fails for exactly
# one such edit.
set -uo pipefail

# The egress verdict path, its `key=value` parser and the verdict-state vocabulary come from here.
# This script runs from the checkout (`./.container/verify.sh`), which carries egress-lib.sh beside
# it, so the same `dirname $0` idiom the installed scripts use reaches it here too (D52.5).
# shellcheck source=egress-lib.sh
. "$(dirname "$0")/egress-lib.sh"

pass=0; fail=0; accepted_failure=0
ok()  { pass=$((pass+1)); printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad() { fail=$((fail+1)); printf '  \033[31mFAIL\033[0m %s\n' "$1"; }
# A failure that is a CONSEQUENCE of a condition this container was configured to accept. Reported
# at full volume like any other -- what it changes is the exit code, so a caller can tell "this
# container is misconfigured" from "this container is in a state its operator chose".
#
# It is a COUNT, and that is the fix. `accepted_failure` used to be a flag set to 1, compared
# against `fail` at the end -- but arming the egress override produces THREE failures (the
# disarmed-gate notice, the unfiltered chain, and the reachability check that follows from having
# no firewall), so `fail` was 3 against an accepted count of 1 and the exit-3 branch could never be
# taken. The one state exit 3 exists for was the one state it could not report, and run.sh's
# handler for it was dead code.
accept_bad() { bad "$1"; accepted_failure=$((accepted_failure+1)); }
note() { printf '  \033[33mnote\033[0m %s\n' "$1"; }
assert() { if [ "$2" = yes ]; then ok "$1"; else bad "$1"; fi; }

# `--declare <mount-point>`: a mount point the CALLER declares, for the one case container.json
# cannot express — a bind NESTED inside a declared target. `mutate-verify.sh` is that case: it
# spells its own docker flags, and it must mount the repo at /home/vscode/repos/jkb because in a
# `jkb task work` session the repo's parent directory is `.jkb/work`, not a repos dir, so mounting
# the parent would put the checkout at /home/vscode/repos/<session>.
#
# Nesting is NOT granted automatically, and that is the whole design decision here. A mount point
# and a mount SOURCE are independent: `-v ~/.ssh:/home/vscode/repos/jkb/secrets` sits inside a
# declared region and is still exfiltration. Nor can the source be checked from in here — on
# Docker Desktop for macOS /proc/self/mountinfo reports the path inside the VM, not the host path,
# which is why lib.sh's `dc_mount_sources` is used only by check-config.sh, on the host, where the
# sources are literal strings in the JSON.
#
# So the exception is NAMED rather than inferred, and it is bounded two ways. It only ADDS to the
# derived set, so it can never switch a check off; and it is refused unless the value is a strict
# descendant of a target container.json declares AS A BIND — not a volume, which reaches no
# host filesystem and is therefore a region nobody reviewed sources for — so `--declare /host`,
# `--declare /var/run/docker.sock` and `--declare /home/vscode/.claude/settings.json` — the exact
# mutations mutate-verify.sh exists to catch — cannot be waved through by it. The count is printed
# in the ok line below, because an override nobody can see is indistinguishable from a rule that
# does not exist (D38).
DECLARED_EXTRA=()
SELF_TEST=no
while [ $# -gt 0 ]; do
    case "$1" in
        --declare)   shift; [ $# -gt 0 ] || { echo "verify.sh: --declare needs a mount point" >&2; exit 2; }
                     DECLARED_EXTRA+=("$1"); shift ;;
        --declare=*) DECLARED_EXTRA+=("${1#--declare=}"); shift ;;
        --self-test) SELF_TEST=yes; shift ;;
        *)           echo "usage: verify.sh [--declare <mount-point>]... | --self-test" >&2; exit 2 ;;
    esac
done

# What every container has regardless of configuration. Anything outside this and EXPECTED is
# something a human added to container.json and must be looked at.
#
# Anchored at a component boundary — `(/|$)` — not as bare prefixes. `^/dev` also matched
# `/devtools`, `^/proc` matched `/procdata` and `^/sys` matched `/sysroot`, so a host mount at any
# of those was silently dropped from the set this check calls exhaustive. A list of exclusions
# that quietly grows is the shape this assertion exists to avoid having.
#
# `/vscode` is the one entry here the CONTAINER RUNTIME does not own: VS Code's remote launcher
# mounts a named volume there to hold the server, unpacks it, and symlinks ~/.vscode-server/bin at
# it. It comes from the launcher's own docker flags, so it is expressible neither in
# container.json (which would be a false claim: we do not create it, and under `run.sh` or a plain
# docker run it is simply absent) nor as `--declare` (refused for anything not nested inside a
# declared bind). Without this entry the container failed outright on `UNDECLARED mounts:
# /vscode`, which is why the supported path was the one path nothing had exercised.
#
# It is kept now that the container is started by `run.sh` and ATTACHED to rather than created by
# Dev Containers, because whether a given VS Code version stages its server through that volume is
# the launcher's business and not something this file should have an opinion about. An exclusion
# for a mount that never appears costs nothing; its absence costs a container that cannot start.
#
# Anchored `^/vscode$`, deliberately not `(/|$)`: only the mount point itself is the launcher's.
# A bind at /vscode/anything is somebody putting a host path inside it, and must still fail.
# The cost, stated rather than hidden: from in here a volume and a bind are indistinguishable, so
# this one path is a spot where a host bind would pass. Bounded to one path, and it is not the
# threat this check is for — a careless line in container.json is, and that is still covered.
RUNTIME_OWNED='^/$|^/proc(/|$)|^/sys(/|$)|^/dev(/|$)|^/etc/hosts$|^/etc/hostname$|^/etc/resolv\.conf$|^/run/\.containerenv$|^/var/run/secrets(/|$)|^/vscode$'

# Which declared extension ids are absent from a `code-server --list-extensions` listing.
#
# Pure, and separated from the call for one reason: in EVERY harness this repo has there is no VS
# Code server — `run.sh`, a plain docker run and mutate-verify.sh all build a correct
# container with no VS Code in it — so the only arm of assertion 7 that can run there is the skip.
# Left inline, its FAIL arm would be unreachable code wearing the costume of a guard, in a change
# whose entire subject is a check that could not fire.
#
# Case-insensitive: the marketplace treats extension ids that way, and `--list-extensions` prints
# the publisher's own casing rather than the casing container.json happens to use.
missing_extensions() { # missing_extensions <declared, one per line> <installed, one per line>
    local declared="$1" installed="$2" ext id out=""
    while read -r ext; do
        [ -n "$ext" ] || continue
        id="${ext%@*}"
        grep -qiFx "$id" <<<"$installed" || out="$out $id"
    done <<<"$declared"
    printf '%s' "$out"
}

# WHAT AN ORPHAN'S FATE MEANS. Pure, taking its observations as arguments, for the reason
# settle_step is (D52.4): the arm that matters most here is the one no healthy container can
# reach, and a decision reachable only by running a container is a decision nothing in the gate
# checks. Gathering the observations needs /proc; judging them does not.
#
# THE PROPERTY IS MEASURED, NOT THE NAME. `--init` makes PID 1 `docker-init`, which IS tini and
# DOES reap, so "PID 1 is /usr/bin/tini" would fail a correct container; and "PID 1 is not sleep"
# would pass any non-reaping program that is not sleep. What leaked was that PID 1 did not wait()
# on what it adopted, so that is what is asked. A zombie is removed from /proc by nothing but a
# wait, so `reaped` cannot be produced by a PID 1 that does not reap: no false pass exists.
#
# Everything that is not an observation of reaping is its own verdict and none of them is `ok` --
# an unobtainable measurement must never be spelled as a definite answer.
# /proc/<pid>/stat, split at the LAST ") ". comm is arbitrary — it can contain spaces and
# parentheses — so splitting at the first is wrong for any process that chooses to be awkward.
#
# starttime (field 22 overall, position 20 after the comm) is what gives the orphan an IDENTITY.
# Gating on comm being `(sleep)` would defeat pid recycling, but it also fails for the window
# between fork() and execve() when the child's comm is still `bash` — so a healthy container would
# report `vanished`. Identity by construction plus starttime removes the window instead of sizing
# it: the pid is our own child, forked milliseconds ago; a DIFFERENT starttime on it later means
# the pid was recycled, which can only happen after the original was reaped.
proc_stat_fields() { # proc_stat_fields <stat line> -> sets PS_STATE PS_PPID PS_START; 1 if short
    local rest
    rest="${1##*) }"
    # Deliberate word splitting: every field after the comm is a single token.
    # shellcheck disable=SC2086
    set -- $rest
    [ "$#" -ge 20 ] || return 1
    PS_STATE="$1"; PS_PPID="$2"; PS_START="${20}"
}

# AS FEW FORKS AS THE MEASUREMENT CAN BE MADE WITH -- not none, and the comment used to say none.
# `mktemp`, the subshell, the orphan itself and the two `sleep`s all fork; what went is the
# previous version's poll, which read through `$( )` sixty times with `sleep 0.1` for up to ~120
# forks inside a probe whose own premise is that fork may be failing at the --pids-limit. Every
# one of the remaining forks fails into `fork-failed`, which is a refusal, so the count is a cost
# rather than a correctness argument. Worse, it spelled four different unobtainable observations (process gone, fork
# failed, /proc unreadable, a line too short to parse) as `gone`, which reaper_verdict reads as
# `reaped`: the PASS direction. So a container near the limit could be leaking and be certified.
#
# This sets globals and returns three ways instead: 0 present, 1 GONE, 2 UNREADABLE. `gone` is
# claimed only when the process directory is observably absent -- an absence is proof only where
# the place it would be is visible, which is the archive sweep's rule one level down. A read that
# fails with the directory still there establishes nothing and must not pass.
# ONE SPELLING, AND NOT AN ENVIRONMENT OVERRIDE. This used to read `${JKB_PROC:-/proc}` under a
# comment claiming `--self-test` drove every arm below through it. It did not: the self-test exits
# ~60 lines above the gathering block, so the variable was never a test seam at runtime -- only a
# live override that could point the one assertion this file exists for at a fabricated /proc. The
# self-test injects by assigning PROC directly, which is what it already did.
PROC=/proc

PS_STATE=""; PS_PPID=""; PS_START=""
pstat() { # pstat <pid> -> 0 present (sets PS_*) | 1 gone | 2 unreadable
    local l=""
    # NOT `if read`: read returns non-zero at EOF WITHOUT a trailing newline while still having
    # set the variable, so its exit code is not "did I get a line". The content is.
    # 2>/dev/null FIRST: redirections are processed left to right, so with the input redirect
    # first the shell reports "No such file or directory" on the REAL stderr before stderr is
    # suppressed. An absent stat file is this function's ordinary GONE answer — it happens on every
    # healthy run, the moment the orphan is reaped — so the wrong order printed a scary line under
    # a passing check, which is how people learn to ignore warnings.
    IFS= read -r l 2>/dev/null <"$PROC/$1/stat"
    if [ -n "$l" ]; then
        proc_stat_fields "$l" || return 2
        return 0
    fi
    # `gone` is claimed ONLY where the place it would be is observably absent.
    [ -d "$PROC/$1" ] && return 2
    return 1
}

# WHOSE NAMESPACES IS THIS /proc AND THIS MOUNT TABLE FROM? Every assertion in this file is about
# THE CONTAINER, and two of them -- the mount boundary and PID 1 reaping -- read /proc/self/mountinfo
# and /proc/1. Claude Code's sandbox wraps a Bash tool call in `bwrap --new-session --die-with-parent
# --unshare-net --bind / / --dev /dev --unshare-pid --unshare-user --cap-drop ALL --proc /proc`
# (bwrap-probe.sh records the invocation). `--proc /proc` mounts a FRESH procfs, so inside it
# /proc/1 is bwrap's init and mountinfo is bwrap's table -- and bwrap's init reaps unconditionally,
# so the reaping assertion PASSES on a container that does not reap. Measured, and reproduced
# against the pre-refusal commit (README.md).
#
# THE QUESTION IS ASKED OF THE KERNEL, NOT INFERRED FROM PROCESS TOPOLOGY. `/proc/self/ns/pid` and
# `/proc/self/ns/mnt` resolve to nsfs inodes that ARE those namespaces' identities; entrypoint.sh
# records the container's own pair immediately before handing over, and this compares. Equality is
# namespace identity by definition, so there is no polarity to get backwards and no premise about
# who started whom.
#
# TWO EARLIER DISCRIMINATORS INFERRED, AND BOTH WERE WRONG -- recorded because the next reader will
# reach for one of them:
#   * A PPID WALK ("PID 1 is an ancestor => nested"). True for `docker exec`, whose parent is
#     outside the namespace and reads as ppid 0; FALSE for the container's own main command, which
#     is a direct child of PID 1. mutate-verify.sh runs verify.sh exactly that way, so the walk
#     refused its control and all sixteen mutations, and CI with them. It also refuses on any
#     ordinary Linux host, where systemd is an ancestor of every shell.
#   * PID 1's STARTTIME, compared against a recorded one. Sound on the pid axis (starttime survives
#     exec, so entrypoint.sh's and tini's are the same), and BLIND on the mount axis: a sandbox that
#     mounts a fresh /proc WITHOUT unsharing pid leaves /proc/1 as the real tini -- starttime
#     matches, the gate opens -- while mountinfo is still the sandbox's. A pid-only test guarding a
#     mount-table assertion is the same mistake one axis over, which is why BOTH ids are recorded.
#
# Pure, taking its four observations as arguments, for the reason settle_step and reaper_verdict are:
# the arm that matters most is the one no healthy container can reach.
ns_verdict() { # ns_verdict <recorded-pid> <recorded-mnt> <observed-pid> <observed-mnt>
    # THE OBSERVATION IS TESTED FIRST, AND THE ORDER IS THE WHOLE OF IT. Both reads are forks --
    # `readlink` for the observation, `cat` for the record -- so a container that cannot fork
    # returns EMPTY FOR BOTH, and whichever test runs first decides what such a container is told.
    # That is not a corner case: it is the measured end state of the leak the reaping assertion
    # below exists to name, ~4083 of 4096 pids spent on zombies (README.md).
    #
    # Asked record-first, it answered `no-marker` and sent the operator to rebuild the image, while
    # this arm's own comment claimed -- in so many words -- that a pid-exhausted container lands
    # HERE, and its `docker stats` remedy was unreachable. A comment asserting what the code beside
    # it does not do, in the file whose subject is guards that cannot fire. `reaper_verdict` below
    # has always had this precedence right, which is what makes the disagreement a defect rather
    # than a choice.
    #
    # It is also right on its own terms: whether our own namespaces are readable is a fact about
    # THIS process, and it dominates whatever a file on disk says. A record can only be interpreted
    # by something that knows what it is holding.
    { [ -n "$3" ] && [ -n "$4" ]; } || { printf 'unreadable'; return; }
    # Absence of the record is not evidence of anything. entrypoint.sh deletes the marker on entry
    # and writes it only on reaching the handover, so absence means this container did not start
    # through entrypoint.sh (`--entrypoint bash`) or did not finish starting -- never "it matches".
    # Reached only once the observation IS readable, which is the ordinary Linux host case: /proc
    # is there, /run/jkb/ns is not, and "no marker" is the honest answer.
    { [ -n "$1" ] && [ -n "$2" ]; } || { printf 'no-marker'; return; }
    { [ "$1" = "$3" ] && [ "$2" = "$4" ]; } || { printf 'nested'; return; }
    printf 'ours'
}

reaper_verdict() { # reaper_verdict <pid1-argv> <orphan-pid> <adopted-by-pid> <final-state>
    [ -n "$1" ] || { printf 'pid1-unreadable'; return; }
    # A container at its --pids-limit fails exactly here -- which is the SYMPTOM of a PID 1 that
    # does not reap, so this arm is not noise on the machine this assertion exists for.
    [ -n "$2" ] || { printf 'fork-failed'; return; }
    [ -n "$3" ] || { printf 'vanished'; return; }
    [ "$3" = 1 ] || { printf 'not-adopted'; return; }
    case "$4" in
        gone)             printf 'reaped' ;;
        Z)                printf 'not-reaped' ;;
        proc-unreadable)  printf 'proc-unreadable' ;;
        unreadable)       printf 'orphan-unreadable' ;;
        *)                printf 'never-exited' ;;
    esac
}
# One login file's state, for assertion 3c: <link> <file in the state volume> -> one word.
#   linked     a link to the volume file (it may still dangle: nobody has logged in yet)
#   pending    a regular file: a login or refresh replaced the link, and run.sh has not yet moved it
#   elsewhere  a link to anywhere else
#   notfile    exists but is neither (a directory): dc_persist_login refuses it
#   missing    nothing there at all: setup never linked it
# A function rather than inline so that --self-test reaches every arm; no container harness can
# produce most of them on purpose.
login_link_state() { # login_link_state <link> <want>
    if [ -L "$1" ]; then
        if [ "$(readlink "$1")" = "$2" ]; then printf 'linked'; else printf 'elsewhere'; fi
    elif [ -f "$1" ]; then printf 'pending'
    elif [ -e "$1" ]; then printf 'notfile'
    else printf 'missing'
    fi
}
# Where git in here looks for hooks, for assertion 3e. Pure over the facts the caller gathers, so
# --self-test reaches every arm:
#   <read>      set | unset | unreadable: what lib.sh's dc_global_hooks_path answered
#   <raw>       the value as stored;  <expanded> git's own expansion of it ("" if it has none)
#   <mount>     bind | volume | none: lib.sh's dc_hooks_mount_verdict for the expanded path
#   <root-uid>  the uid that must own the mirror (0; the self-test passes its own)
# ->
#   unset       no global value: each repository's own .git/hooks runs
#   unreadable  git could not read the global config at all
#   empty       set to "": git reads that as "/", and runs no hooks from there
#   in-repo     relative: git resolves it inside each repository, which is mounted too
#   shared      inside a bind: the host's own directory, live. Nothing is copied, nothing is wrong
#               with it, and it is writable wherever that bind is
#   in-volume   inside one of the container's volumes: not the host's hooks, and never mirrored
#   unexpandable git cannot expand it in here (~user, with no such user)
#   missing     nothing there: git runs NO hooks, silently (the defect this exists for)
#   unmarked    a directory run.sh did not make (no marker, or a symlink)
#   not-root    carries the marker but is not owned by root: a marker can be written by anyone who
#               can write the directory, so this is a forgery or a copy that lost its owner
#   writable    the mirror, but writable from here
#   mirrored    the host's hooks, copied in by run.sh, owned by root and read-only here
hooks_dir_state() { # hooks_dir_state <read> <raw> <expanded> <mount> <root-uid>
    case "$1" in unset) printf 'unset'; return ;; set) ;; *) printf 'unreadable'; return ;; esac
    case "$2" in
        "")       printf 'empty'; return ;;
        /*|"~"*)  ;;   # ~user too: git expands it or fails, and either way the arms below answer
        *)        printf 'in-repo'; return ;;
    esac
    case "$4" in bind) printf 'shared'; return ;; volume) printf 'in-volume'; return ;; esac
    if [ -z "$3" ]; then printf 'unexpandable'
    elif [ ! -d "$3" ]; then printf 'missing'
    elif [ -L "$3" ] || [ ! -e "$3/$DC_HOOKS_MIRROR_MARKER" ]; then printf 'unmarked'
    elif [ "$(stat -c %u "$3" 2>/dev/null || stat -f %u "$3")" != "$5" ]; then printf 'not-root'
    elif [ -w "$3" ]; then printf 'writable'
    else printf 'mirrored'
    fi
}
# What git in here uses, against what run.sh recorded APPLYING, for assertion 3e. run.sh sets
# core.hooksPath in ~/.config/git/config on every start, and the record says to what (<applied>);
# VS Code's copy of ~/.gitconfig names the same thing while it is current, and wins when present.
#   <host-state> none | unset | unreadable | value (lib.sh's dc_read_host_record)
#   <applied>    what run.sh set (`-` when it set nothing)
#   <read> <raw> what git in here answered (set | unset | unreadable) and its raw value
#   <actual>     git's expansion in here, trailing slashes dropped
# An absolute <applied> is compared with <actual> (the copy may spell the same place ~/...); any
# other value is compared raw. Round 5 found a relative host value applied as nothing, and this
# function reading "nothing expected" as agreement.
# ->
#   none         nothing to compare against (no record from this start, the host could not read,
#                or its value means nothing in here)
#   agree        git in here uses what run.sh applied
#   not-applied  run.sh applied a value and git in here reads NONE. A FAILURE whose remedy is
#                always open: re-run run.sh.
#   apply-failed run.sh could not set the key at all (round 6: a leftover config.lock). A FAILURE:
#                git in here may run no hooks, and before this word it read as "the host sets none".
#   stale        the host sets none, and git in here still reads one: VS Code's copy predates that
#   diverged     git in here reads something else: VS Code's copy predates a change on the host
# `stale` and `diverged` run hooks the host ran until recently and heal without a restart, so they
# are reported, not failed: a failure would make `run.sh --open` refuse the window you fix them in.
# Rounds 3 and 4 found the states before these (`awaiting`, `not-seen`) failing on every fresh
# container and on a copy that merely predated a host edit, each with a remedy run.sh --open refused.
hooks_host_compare() { # hooks_host_compare <host-state> <applied> <read> <raw> <actual>
    case "$1" in none|unreadable) printf 'none'; return ;; esac
    case "$3" in set|unset) ;; *) printf 'none'; return ;; esac
    if [ "$1" = unset ]; then
        if [ "$3" = unset ]; then printf 'agree'; else printf 'stale'; fi
        return
    fi
    case "$2" in -|kept) printf 'none'; return ;; failed) printf 'apply-failed'; return ;; esac
    [ "$3" = set ] || { printf 'not-applied'; return; }
    case "$2" in
        /*) [ "$5" = "$2" ] && printf 'agree' || printf 'diverged' ;;
        *)  [ "$4" = "$2" ] && printf 'agree' || printf 'diverged' ;;
    esac
}
# hooks_record_verdict <record> <start marker> <read> <raw> <actual> -- the record read and the
# comparison together, as 3e uses them: prints <verdict> <host> <origin> <mirror> <applied>,
# separated by 0x1f (lib.sh's dc_read_host_record says why not tabs). ONE function, so --self-test
# drives the whole path from a real record file to a verdict: round 6 found the live glue (the field
# order of the read, the variable names) exercised by nothing, since no test container starts
# through run.sh and so none has a record.
hooks_record_verdict() { # hooks_record_verdict <record> <marker> <read> <raw> <actual>
    local state host origin mirror applied
    IFS="$(printf '\037')" read -r state host origin mirror applied <<REC
$(dc_read_host_record "$1" "$2")
REC
    printf '%s\037%s\037%s\037%s\037%s\n' \
        "$(hooks_host_compare "$state" "$applied" "$3" "$4" "$5")" "$host" "$origin" "$mirror" "$applied"
}
# --self-test: the exclusion list, exercised with no container. Run by ./scripts/check.sh.
#
# It is the one part of the mount boundary that widens by a TYPO rather than by an edit anyone
# reviews: every entry is an exclusion, so a pattern matching more than it names silently stops
# the exhaustive check from being exhaustive, and the assertion still prints `ok`. That is not
# hypothetical — the `^/dev` / `/devtools` class above is the same file's own history.
# mutate-verify.sh would catch it, but mutate-verify.sh needs a Docker host and is not in the
# gate. This is, and it costs nothing.
if [ "$SELF_TEST" = yes ]; then
    st_fail=0
    st() { # st <mount point> <owned|checked>
        if grep -Eq "$RUNTIME_OWNED" <<<"$1"; then got=owned; else got=checked; fi
        if [ "$got" = "$2" ]; then printf '  \033[32mok\033[0m   %-24s %s\n' "$1" "$2"
        else printf '  \033[31mFAIL\033[0m %-24s is %s, wanted %s\n' "$1" "$got" "$2"; st_fail=$((st_fail+1)); fi
    }
    echo "==> verify.sh self-test: RUNTIME_OWNED"
    for p in / /proc /proc/sys /sys /sys/fs/cgroup /dev /dev/shm /dev/pts \
             /etc/hosts /etc/hostname /etc/resolv.conf /run/.containerenv \
             /var/run/secrets /var/run/secrets/kubernetes.io /vscode; do
        st "$p" owned
    done
    # Every one of these is a real mount point somebody could add, and each is a near-miss for an
    # entry above. A regex that swallows one of them is a hole with no symptom.
    for p in /vscodex /vscode-evil /vscode/secrets /devtools /procdata /sysroot /etchosts \
             /etc/hostnamex /etc/resolv.conf.bak /run/containerenv /var/run/secretsx \
             /home/vscode/repos /home/vscode/.jkb /host; do
        st "$p" checked
    done

    # Assertion 7's judgement, whose FAIL arm no container harness can reach (see
    # missing_extensions). `st` compares strings, so these read as the mount cases do.
    echo "==> verify.sh self-test: missing_extensions"
    declared="$(printf 'rust-lang.rust-analyzer@0.3.3025\nanthropic.claude-code@2.1.250\n')"
    st2() { # st2 <label> <got> <want>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else printf '  \033[31mFAIL\033[0m %s\n         got:  [%s]\n         want: [%s]\n' "$1" "$2" "$3"; st_fail=$((st_fail+1)); fi
    }
    st2 "both installed is nothing missing" \
        "$(missing_extensions "$declared" "$(printf 'rust-lang.rust-analyzer\nanthropic.claude-code\n')")" ""
    st2 "the one this container exists for, absent, is named" \
        "$(missing_extensions "$declared" "rust-lang.rust-analyzer")" " anthropic.claude-code"
    st2 "an empty listing names every declared one" \
        "$(missing_extensions "$declared" "")" " rust-lang.rust-analyzer anthropic.claude-code"
    # The version suffix is ours, not the listing's: `--list-extensions` prints bare ids, so
    # comparing the pinned string would report every extension missing, always.
    st2 "the @version pin is stripped before comparing" \
        "$(missing_extensions "anthropic.claude-code@2.1.250" "anthropic.claude-code")" ""
    st2 "publisher casing does not decide the answer" \
        "$(missing_extensions "$declared" "$(printf 'rust-lang.rust-analyzer\nAnthropic.Claude-Code\n')")" ""
    # A DIFFERENT extension whose id contains this one must not satisfy it. The direction matters
    # and the first version of this case had it backwards — it asked whether a listing SHORTER
    # than the id matched, which no grep would have said yes to, so it passed with the anchoring
    # deleted. Caught by mutating the guard rather than by reading it.
    st2 "a longer id containing this one is not a match" \
        "$(missing_extensions "anthropic.claude-code@2.1.250" "anthropic.claude-code-preview")" " anthropic.claude-code"
    # The dot in an extension id is a literal. Unfixed, `anthropic.claude-code` is a pattern whose
    # `.` matches any character, so an unrelated id differing only there would read as installed.
    st2 "the dot in an id is literal, not a wildcard" \
        "$(missing_extensions "anthropic.claude-code@2.1.250" "anthropicXclaude-code")" " anthropic.claude-code"

    # THE STAT PARSER, against real-shaped lines. It splits at the LAST ") " because comm is
    # arbitrary; the pre-exec row is the one that matters, since gating on comm being `(sleep)`
    # would make a healthy container report `vanished` between fork() and execve().
    echo "==> verify.sh self-test: proc_stat_fields"
    st_line() { printf '%s (%s) %s 1 1 1 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 %s 0 0\n' "$1" "$2" "$3" "$4"; }
    psf() { PS_STATE=; PS_PPID=; PS_START=; proc_stat_fields "$1" || return 1; printf '%s %s %s' "$PS_STATE" "$PS_PPID" "$PS_START"; }
    st2 "a sleeping orphan yields state, ppid and starttime" \
        "$(psf "$(st_line 42 sleep S 987654)")" "S 1 987654"
    st2 "a pre-exec child still named bash parses identically — no comm gate" \
        "$(psf "$(st_line 42 bash R 987654)")" "R 1 987654"
    st2 "a zombie is read as such" \
        "$(psf "$(st_line 42 sleep Z 987654)")" "Z 1 987654"
    st2 "a comm containing a space and a paren does not shift the fields" \
        "$(psf "$(st_line 42 'we (ird) name' S 987654)")" "S 1 987654"
    st2 "a truncated line is refused rather than answered from short fields" \
        "$(psf '42 (sleep) S 1 1' >/dev/null 2>&1; echo $?)" "1"

    # pstat's THREE-WAY RETURN, which is the whole of finding 2's fix: `gone` is claimed only where
    # the process directory is observably absent, and a read that fails with the directory still
    # there establishes nothing. Spelling those the same is what let a leaking container be
    # certified as reaping. /proc is injected, so all three are reachable on a host without one.
    echo "==> verify.sh self-test: pstat's three-way return"
    PROC="$(mktemp -d)"
    mkdir -p "$PROC/100" "$PROC/101" "$PROC/102"
    st_line 100 sleep S 555 > "$PROC/100/stat"
    printf '101 (sleep) S 1 1' > "$PROC/101/stat"          # too short to parse
    # 102 has a directory and no stat file at all.
    pstat 100; st2 "a readable stat is present (0)"            "$?" "0"
    st2 "...and its fields are set"                            "$PS_STATE $PS_PPID $PS_START" "S 1 555"
    pstat 101; st2 "an unparseable stat is UNREADABLE (2), not gone" "$?" "2"
    pstat 102; st2 "a directory with no stat is UNREADABLE (2), not gone" "$?" "2"
    pstat 999; st2 "an absent process directory is GONE (1)"   "$?" "1"
    rm -rf "$PROC"; PROC=/proc

    # WHOSE NAMESPACES, as a literal table. This is the guard that decides whether any assertion in
    # this file means anything -- so it is pinned against fixture values rather than reasoned about,
    # and every arm including the ones no healthy container reaches. The two rows that matter most
    # are the two topologies the PREVIOUS discriminator got wrong: `docker exec` (how run.sh runs
    # this) and the container's own main command (how mutate-verify.sh does). Under a namespace-id
    # comparison they are the SAME case -- both are in the container's namespaces -- which is the
    # point: the question stopped depending on who started the process.
    echo "==> verify.sh self-test: whose namespaces are these?"
    P='pid:[4026531836]'; M='mnt:[4026532999]'
    st2 "the container's own namespaces are recognised (docker exec, and its own command alike)" \
        "$(ns_verdict "$P" "$M" "$P" "$M")" "ours"
    st2 "a nested sandbox's fresh procfs is REFUSED, not trusted" \
        "$(ns_verdict "$P" "$M" 'pid:[4026533111]' 'mnt:[4026533112]')" "nested"
    st2 "...and so is a fresh MOUNT namespace alone, which a pid-only test cannot see" \
        "$(ns_verdict "$P" "$M" "$P" 'mnt:[4026533112]')" "nested"
    st2 "...and a fresh pid namespace alone" \
        "$(ns_verdict "$P" "$M" 'pid:[4026533111]' "$M")" "nested"
    st2 "no marker establishes nothing — it is not a match" \
        "$(ns_verdict "" "" "$P" "$M")" "no-marker"
    st2 "a half-written marker is no marker" \
        "$(ns_verdict "$P" "" "$P" "$M")" "no-marker"
    st2 "an unreadable observation establishes nothing either" \
        "$(ns_verdict "$P" "$M" "" "")" "unreadable"
    st2 "...including when only one of the two could be read" \
        "$(ns_verdict "$P" "$M" "$P" "")" "unreadable"
    # THE ROW THAT PINS THE PRECEDENCE, and its absence is why the table was satisfied by either
    # order. A container that cannot fork fails BOTH reads, so this is the only input that
    # distinguishes record-first from observation-first — and it is the input a pid-exhausted
    # container actually produces, which is the state the reaping assertion exists to name.
    st2 "a container that cannot fork fails BOTH reads, and is told THAT — not to rebuild" \
        "$(ns_verdict "" "" "" "")" "unreadable"

    # Assertion 1b's judgement. Only ONE of these arms is reachable in a healthy container, so
    # without this the rest are unreachable code in a change whose whole subject is a check that
    # could not fire. A literal table, not a re-derivation of the conditions.
    echo "==> verify.sh self-test: reaper_verdict"
    st2 "a reaped orphan is the only ok arm" \
        "$(reaper_verdict '/usr/bin/tini -- sleep infinity' 42 1 gone)" "reaped"
    st2 "docker-init reaps too — the verdict is the property, not the name" \
        "$(reaper_verdict '/sbin/docker-init -- /usr/local/bin/entrypoint.sh sleep infinity' 42 1 gone)" "reaped"
    st2 "a lingering zombie is the leak itself" \
        "$(reaper_verdict 'sleep infinity' 42 1 Z)" "not-reaped"
    st2 "an unreadable PID 1 establishes nothing" \
        "$(reaper_verdict '' 42 1 gone)" "pid1-unreadable"
    st2 "a failed fork establishes nothing (and is the leak's own symptom)" \
        "$(reaper_verdict 'sleep infinity' '' '' '')" "fork-failed"
    st2 "an unreadable /proc establishes nothing — it must not read as a reap" \
        "$(reaper_verdict 'sleep infinity' 42 1 proc-unreadable)" "proc-unreadable"
    st2 "an orphan whose entry cannot be read establishes nothing either" \
        "$(reaper_verdict '/usr/bin/tini -- sleep infinity' 42 1 unreadable)" "orphan-unreadable"
    st2 "an orphan that vanished before it was observed establishes nothing" \
        "$(reaper_verdict '/usr/bin/tini -- sleep infinity' 42 '' '')" "vanished"
    st2 "an orphan a subreaper took establishes nothing about PID 1" \
        "$(reaper_verdict '/usr/bin/tini -- sleep infinity' 42 77 gone)" "not-adopted"
    st2 "an orphan still running was never there to be reaped" \
        "$(reaper_verdict '/usr/bin/tini -- sleep infinity' 42 1 S)" "never-exited"


    echo "==> verify.sh self-test: login_link_state"
    lt="$(mktemp -d)"; want="$lt/volume/.credentials.json"; mkdir -p "$lt/volume"
    ln -s "$want" "$lt/linked";            st2 "a link to the volume file, even dangling" "$(login_link_state "$lt/linked" "$want")" linked
    printf x > "$lt/pending";              st2 "a regular file"                           "$(login_link_state "$lt/pending" "$want")" pending
    ln -s "$lt/other" "$lt/elsewhere";     st2 "a link to anywhere else"                  "$(login_link_state "$lt/elsewhere" "$want")" elsewhere
    mkdir "$lt/dir";                       st2 "a directory"                              "$(login_link_state "$lt/dir" "$want")" notfile
                                           st2 "nothing at all"                           "$(login_link_state "$lt/none" "$want")" missing
    rm -rf "$lt"

    echo "==> verify.sh self-test: hooks_dir_state"
    # shellcheck source=lib.sh
    . "$(dirname "$0")/lib.sh"   # the marker's name; the main path sources it further down
    ht="$(mktemp -d)"; me="$(id -u)"
    mkdir -p "$ht/mirror" "$ht/bare"; : > "$ht/mirror/$DC_HOOKS_MIRROR_MARKER"; ln -s "$ht/mirror" "$ht/link"
    hs() { hooks_dir_state "$@"; }
    st2 "no global hooksPath"                    "$(hs unset "" "" none 0)" unset
    st2 "git could not read the global config"   "$(hs unreadable "" "" none 0)" unreadable
    st2 "set to the empty string"                "$(hs set "" "" none 0)" empty
    st2 "a relative hooksPath"                   "$(hs set .githooks "" none 0)" in-repo
    st2 "inside a bind: the host's own"          "$(hs set /home/vscode/repos/d/h /home/vscode/repos/d/h bind 0)" shared
    st2 "inside a volume"                        "$(hs set ~/.claude-state/h /home/vscode/.claude-state/h volume 0)" in-volume
    st2 "another user's home, unexpandable"      "$(hs set "~nosuchuser/hooks" "" none 0)" unexpandable
    st2 "a path with nothing there"              "$(hs set "$ht/none" "$ht/none" none "$me")" missing
    st2 "a directory run.sh did not make"        "$(hs set "$ht/bare" "$ht/bare" none "$me")" unmarked
    st2 "a symlink to a marked directory"        "$(hs set "$ht/link" "$ht/link" none "$me")" unmarked
    if [ "$me" = 0 ]; then
        printf '  \033[33mskip\033[0m a marker in a directory root does not own (running as root: every directory is root'"'"'s)\n'
    else
        st2 "a marker in a directory root does not own" "$(hs set "$ht/mirror" "$ht/mirror" none 0)" not-root
    fi
    st2 "the mirror, writable from here"         "$(hs set "$ht/mirror" "$ht/mirror" none "$me")" writable
    chmod 555 "$ht/mirror"
    if [ -w "$ht/mirror" ]; then
        printf '  \033[33mskip\033[0m the mirror, read-only (running as root: nothing is unwritable)\n'
    else
        st2 "the mirror, read-only from here"    "$(hs set "$ht/mirror" "$ht/mirror" none "$me")" mirrored
    fi
    chmod 755 "$ht/mirror"; rm -rf "$ht"

    echo "==> verify.sh self-test: hooks_host_compare"
    hc() { hooks_host_compare "$@"; }
    st2 "no record from this start"              "$(hc none - set /h /h)" none
    st2 "the host could not read its own"        "$(hc unreadable - unset "" "")" none
    st2 "neither side sets one"                  "$(hc unset - unset "" "")" agree
    st2 "the host dropped it, the copy has it"   "$(hc unset - set /h /h)" stale
    st2 "a value that means nothing in here"     "$(hc value - unset "" "")" none
    st2 "applied, and git reads none"            "$(hc value /U/h unset "" "")" not-applied
    st2 "a relative value, applied and read"     "$(hc value .githooks set .githooks "")" agree
    st2 "a relative value, applied but not read" "$(hc value .githooks unset "" "")" not-applied
    st2 "an empty value, applied and read"       "$(hc value "" set "" "")" agree
    st2 "the same directory, spelled ~/ here"    "$(hc value /home/vscode/h set "~/h" /home/vscode/h)" agree
    st2 "the copy predates a host change"        "$(hc value /U/new set /U/old /U/old)" diverged
    st2 "a relative value that differs"          "$(hc value .githooks set .hooks "")" diverged
    st2 "the write failed"                       "$(hc value failed unset "" "")" apply-failed
    st2 "the key was kept (host unknown)"        "$(hc value kept set /h /h)" none

    echo "==> verify.sh self-test: hooks_record_verdict (a real record file, end to end)"
    rt="$(mktemp -d)"; : > "$rt/marker"; sleep 1
    rv() { hooks_record_verdict "$rt/rec" "$rt/marker" "$@" | tr '\037' '|'; }
    printf 'state=value\nvalue=~/h\norigin=/U/.gitconfig\nmirror=ok\napplied=/home/vscode/h\n' > "$rt/rec"
    st2 "a mirrored ~/ value, read back in here"  "$(rv set "~/h" /home/vscode/h)" "agree|~/h|/U/.gitconfig|ok|/home/vscode/h"
    st2 "the same record, git reads none"          "$(rv unset "" "")" "not-applied|~/h|/U/.gitconfig|ok|/home/vscode/h"
    printf 'state=value\nvalue=/U/h\norigin=\nmirror=failed\napplied=failed\n' > "$rt/rec"
    st2 "a failed write, empty origin kept in place" "$(rv unset "" "")" "apply-failed|/U/h||failed|failed"
    sleep 1; : > "$rt/marker"
    st2 "a record from before this start"          "$(rv set /x /x)" "none||||-"
    rm -rf "$rt"

    echo
    [ "$st_fail" -eq 0 ] || { printf '\033[31m%d failed\033[0m\n' "$st_fail"; exit 1; }
    printf '\033[32mverify.sh self-test passed\033[0m\n'
    exit 0
fi

# INSIDE THE CONTAINER, OR NOT AT ALL. Every assertion below is about a Linux container's kernel
# state, and run anywhere else they do not fail — they answer about a machine that was never the
# subject. On the macOS host this printed fourteen confident FAILs (no `~/.claude` links, sudo
# wants a password, the knowledge base "not mounted") and, worse, two `ok` lines: `/proc/self/
# mountinfo` does not exist there, so the mount-boundary check compared an EMPTY set and passed.
# A report an operator could act on, about nothing.
#
# The mount table is the load-bearing input rather than a proxy for the platform: if it cannot be
# read, the one assertion this file exists for cannot mean anything, whatever else is true.
if [ ! -r /proc/self/mountinfo ]; then
    echo "verify.sh asserts what a running container is, and must run INSIDE one." >&2
    echo "  /proc/self/mountinfo is not readable here, so the mount boundary — the assertion this" >&2
    echo "  file exists for — could not be checked at all." >&2
    echo >&2
    echo "  In VS Code:  ./.container/run.sh, attach to the container, then run this" >&2
    echo "  With Docker: ./.container/mutate-verify.sh --control   (one healthy run)" >&2
    echo "               ./.container/mutate-verify.sh             (every guard, watched failing)" >&2
    echo >&2
    echo "  Do NOT hand-roll the docker run: it needs the seccomp profile, NET_ADMIN, both binds" >&2
    echo "  and a preamble that raises the firewall and installs the posture. A command missing" >&2
    echo "  any of those prints a dozen FAILs that read as a broken container." >&2
    exit 2
fi

# ...AND THEY MUST BE THIS CONTAINER'S NAMESPACES, WHICH IS A SECOND QUESTION. The refusal above
# establishes that a container is the subject; this establishes that THIS one is. See ns_verdict
# for what is compared and for the two inferred discriminators that were wrong before it.
#
# IT DOMINATES EVERY ASSERTION BELOW rather than living inside one of them -- the rule this repo
# keeps arriving at (D45.5): a condition that applies to every arm belongs above the dispatch. Both
# the mount boundary and PID 1 reaping depend on it, so siting it inside either would leave the
# other unguarded, and inside the reaping `case` it would be one more verdict word to forget.
#
# THE REFUSAL PRINTS A `FAIL` LINE, WHICH IS NOT COSMETIC. mutate-verify.sh's `judge` reports a
# mutation CAUGHT only on a non-zero exit AND a line carrying both the expected text and `FAIL`.
# A refusal that only wrote to stderr would be invisible to the harness -- so the two mutations
# that break this marker could never be watched firing, in the guard whose whole subject is checks
# that cannot fire. `bad` is what every other failure here uses; the guidance goes to stderr after.
# NO SECOND SPELLING OF THE PATH. `${JKB_NS_MARKER:-/run/jkb/ns}` would put it in two files again,
# which is the duplication this branch deleted for JKB_REAPER rather than guarded. Unset is not an
# error here either: `cat ""` fails quietly and lands on `no-marker`, whose message is the right one
# for a plain Linux host — where the variable is absent precisely because there is no container.
ns_rec="$(cat "${JKB_NS_MARKER:-}" 2>/dev/null)" || ns_rec=""
rec_pid="$(kv_field pid "$ns_rec")"; rec_mnt="$(kv_field mnt "$ns_rec")"
# ns_pair is egress-lib.sh's, shared with the entrypoint that WRITES the record this reads, so the
# two halves cannot drift. Its own self-test covers the transposition a single `readlink a b` can
# produce; this gathering is otherwise exercised only by running it in a container.
ns_pair "$PROC/self/ns"; obs_pid="$NS_PID"; obs_mnt="$NS_MNT"

case "$(ns_verdict "$rec_pid" "$rec_mnt" "$obs_pid" "$obs_mnt")" in
    ours) ;;
    nested)
        bad "these are NOT this container's namespaces — every assertion below would describe a nested sandbox instead, and the PID-1 reaping check would PASS on a container that does not reap, because the sandbox's own init does"
        {
            echo
            echo "  recorded by the container's entrypoint:  pid=$rec_pid  mnt=$rec_mnt"
            echo "  observed by this process:                pid=$obs_pid  mnt=$obs_mnt"
            echo
            echo "  You are almost certainly inside Claude Code's own sandbox, which wraps a Bash"
            echo "  tool call in \`bwrap --bind / / --unshare-pid --unshare-user --proc /proc\`."
            echo "  The fresh procfs is why /proc/1 and the mount table are not this container's."
            echo
            echo "  Run it from a plain terminal in the attached container, or from the host:"
            echo "    ./.container/run.sh                       (runs this for you, via docker exec)"
            echo "    ./.container/mutate-verify.sh --control   (one healthy run)"
        } >&2
        exit 2
        ;;
    no-marker)
        bad "this container recorded no namespace identity, so verify.sh cannot tell whether what it is about to measure is this container at all"
        {
            echo
            echo "  ${JKB_NS_MARKER:-(JKB_NS_MARKER is not set)} is absent or incomplete. entrypoint.sh"
            echo "  deletes it on entry and writes it only on reaching its handover, so this means"
            echo "  one of:"
            echo "    * the container was started with --entrypoint, bypassing entrypoint.sh;"
            echo "    * its start did not finish (check \`docker logs\`);"
            echo "    * the image predates the marker — rebuild: ./.container/run.sh --rm && ./.container/run.sh --build"
            echo
            echo "  On an ordinary Linux host there is no marker either, and that is the honest"
            echo "  answer: this script asserts what a CONTAINER is and has no subject here."
        } >&2
        exit 2
        ;;
    unreadable)
        bad "verify.sh could not read its own namespace ids from $PROC/self/ns, so whether these are this container's namespaces is unknown — and every assertion below depends on the answer"
        {
            echo
            echo "  \`readlink\` is a fork, so this is also where a container that cannot fork lands —"
            echo "  which is the end state of a PID 1 that does not reap, every one of its pids spent"
            echo "  on zombies. Tell them apart from OUTSIDE the container:"
            echo
            echo "    docker stats --no-stream <name>   # PIDS = the counter --pids-limit bounds"
            echo
            echo "  If that is near the limit, recreate it:"
            echo "    ./.container/run.sh --rm && ./.container/run.sh --build"
        } >&2
        exit 2
        ;;
    # A VERDICT WITH NO ARM MUST NOT OPEN THE GATE. `set -uo pipefail` is on and `set -e` is not, so
    # an unmatched `case` is a no-op returning 0 -- which here means falling through into every
    # assertion below with the subject UNVERIFIED, the one failure direction this gate exists to
    # prevent. The verdict words and these arms are two lists, and the next edit to ns_verdict is
    # the one that forgets this one. reaper_verdict's `case` carries the identical arm for the
    # identical reason; this one was missing it, found by asking who else implements the rule.
    *)
        bad "could not establish whose namespaces these are: unrecognised verdict '$(ns_verdict "$rec_pid" "$rec_mnt" "$obs_pid" "$obs_mnt")' — ns_verdict gained a word this case has no arm for, and an unhandled verdict must refuse rather than let every assertion below run against an unverified subject"
        exit 2
        ;;
esac

echo "==> container posture"

# READ ONCE, AND AN EMPTY TABLE IS NOT A ZERO. This count feeds a pass/fail verdict below, and it
# used to be a byte-identical second copy of the diagnostic beside bwrap's error — two copies of one
# predicate, free to drift. Worse, it spelled a readable-but-empty /proc/self/mountinfo as "no
# submounts", i.e. as a PASS, while this file's other mountinfo consumer calls that same state
# unreadable and reports "the mount boundary was not checked, which is not the same as finding it
# clean". One run, two readers, opposite verdicts about one observation. The empty case is what the
# comment there records as having already shipped here once.
if proc_mountinfo="$(cat /proc/self/mountinfo 2>/dev/null)" && [ -n "$proc_mountinfo" ]; then
    proc_submounts="$(awk '$5 ~ "^/proc/" {n++} END {print n+0}' <<<"$proc_mountinfo")"
else
    proc_submounts=""
fi

# 1. Non-root. Load-bearing, not hygiene: root in a container cannot create a mount namespace
#    directly even with seccomp relaxed, so bubblewrap fails and the nested sandbox with it.
assert "runs as a non-root user (uid $(id -u))" "$([ "$(id -u)" -ne 0 ] && echo yes || echo no)"

# 1b. PID 1 REAPS WHAT IT ADOPTS. run.sh keeps this container alive with `sleep infinity`, and a
#     bare `exec "$@"` in entrypoint.sh made THAT PID 1 -- `sleep` never wait()s, so every orphan
#     reparented to it stayed a zombie for ever — see README.md, "The measurements this is
#     built on", for the numbers and the date they were taken.
#
#     WHY IT IS ASSERTED HERE AND NOT ONLY AT BUILD. The fix is an IMAGE change, and nothing else
#     observes the running container: `config_hash` covers the derived docker arguments and the
#     seccomp profile's content, not entrypoint.sh or the Dockerfile, and the container.json edit
#     that carried the fix was comment-only, which `dc_strip` removes before it is hashed. So
#     `run.sh` without `--build` finds the args-hash and the image id both matching, starts the
#     PRE-TINI container, settle() reads `sleep infinity` and returns settled, and the run reports
#     "running and attachable" while the leak continues. This is the assertion that goes red there.
#     The self-test proves the SCRIPT execs its reaper and the Dockerfile proves the reaper existed
#     AT BUILD TIME; neither is an observation of the container you are about to attach to.
#
#     SO THIS IS THE IMAGE-STALENESS CHECK, re-entering as a direct measurement — worth saying
#     because a separate branch was built to compare image inputs against a fingerprint, and the
#     next person to notice the gap will reach for that again. Measuring the property the stale
#     image LACKS beats comparing ids: it needs no fingerprint to keep current, it cannot be
#     defeated by an input nobody thought to hash (the container.json edit that carried this fix
#     was comment-only, which `dc_strip` removes before hashing), and what it reports is the thing
#     that is actually wrong rather than a hash mismatch the reader has to interpret.
# WHAT IS AND IS NOT COVERED BY `--self-test`, said plainly because the previous wording claimed
# more than it delivered: the JUDGEMENT is covered -- proc_stat_fields, pstat's three-way return,
# ns_verdict and reaper_verdict all run against fixtures, which is where the arms no
# healthy container can reach live. The GATHERING below is not: it is exercised only by running it
# in a container, and only ever takes its healthy path there.
pid1_argv=""
while IFS= read -r -d '' w; do pid1_argv="$pid1_argv$w "; done 2>/dev/null <"$PROC/1/cmdline"
pid1_argv="${pid1_argv% }"


# THE PREMISE, ESTABLISHED BEFORE IT IS USED: if our own stat is unreadable then nothing below
# observes anything, and every later "gone" would be a fact about /proc rather than about PID 1.
orphan=""; adopted=""; final=""; born=""
if ! pstat "$$"; then
    final=proc-unreadable
else
    # The pid comes back through a FILE, not a command substitution: `$( )` would fork again, and
    # the substitution also has to wait for the subshell. These two forks (the subshell and the
    # orphan) are the only ones before the measurement, and a failure of either shows up as an
    # empty pid, which reaper_verdict reads as fork-failed -- the leak's own end state.
    pidfile="$(mktemp)"
    ( sleep 1 >/dev/null 2>&1 </dev/null & echo $! >"$pidfile" ) 2>/dev/null
    IFS= read -r orphan 2>/dev/null <"$pidfile" || orphan=""
    rm -f "$pidfile"
    case "$orphan" in ''|*[!0-9]*) orphan="" ;; esac

    if [ -n "$orphan" ] && pstat "$orphan"; then
        adopted="$PS_PPID"; born="$PS_START"
        # ONE WAIT, NOT A POLL. The orphan lives 1s; three seconds is well past it, and a `sleep`
        # that cannot fork is itself the answer rather than sixty chances to be wrong.
        if sleep 3; then
            pstat "$orphan"; rc=$?
            case "$rc" in
                # A DIFFERENT starttime on the same pid is a recycled pid, and a pid is recycled
                # only after the process holding it was reaped -- evidence of a reap, not a miss.
                0) if [ "$PS_START" != "$born" ]; then final=gone; else final="$PS_STATE"; fi ;;
                2) final=unreadable ;;
                *) final=gone ;;
            esac
        else
            orphan=""     # fork-failed
        fi
    fi
    # THERE IS DELIBERATELY NO ARM FOR A FAILED FIRST OBSERVATION. Whether the orphan was already
    # gone or its entry could not be read, `adopted` is unset -- and reaper_verdict answers
    # `vanished` on that before it ever reads the final state. An arm setting `final` here would be
    # dead code: there is nothing to say without an adopter, because we do not know whether PID 1
    # was ever in the picture. The `vanished` message names both.
fi

case "$(reaper_verdict "$pid1_argv" "$orphan" "$adopted" "$final")" in
    reaped)     ok  "PID 1 reaps the orphans it adopts (PID 1 is: $pid1_argv)" ;;
    not-reaped) bad "PID 1 does not reap: an orphan it adopted is still a zombie (PID 1 is: $pid1_argv) — so every orphan becomes one and ordinary use spends the --pids-limit. This container predates the tini handover; recreate it: ./.container/run.sh --rm && ./.container/run.sh --build" ;;
    fork-failed)      bad "could not establish whether PID 1 reaps: the fork for the test orphan failed, which is how a container at its --pids-limit fails — the end state of a PID 1 that does not reap (PID 1 is: $pid1_argv)" ;;
    pid1-unreadable)  bad "could not establish whether PID 1 reaps: $PROC/1/cmdline could not be read, so nothing here observed what PID 1 even is" ;;
    proc-unreadable)  bad "could not establish whether PID 1 reaps: this process's own $PROC entry is unreadable, so an absent orphan would say nothing about reaping" ;;
    orphan-unreadable) bad "could not establish whether PID 1 reaps: the test orphan's $PROC entry exists but could not be read or parsed (PID 1 is: $pid1_argv)" ;;
    not-adopted)      bad "could not establish whether PID 1 reaps: the test orphan was adopted by pid $adopted rather than PID 1, so a subreaper is in the way (PID 1 is: $pid1_argv)" ;;
    vanished)         bad "could not establish whether PID 1 reaps: the test orphan could not be observed after being spawned — it was already gone, or its $PROC entry could not be read (PID 1 is: $pid1_argv)" ;;
    never-exited)     bad "could not establish whether PID 1 reaps: the test orphan is still in state '$final' after 3s, so it never exited to be reaped (PID 1 is: $pid1_argv)" ;;
    # A VERDICT WITH NO ARM MUST NOT BE SILENCE. `set -uo pipefail` is on and `set -e` is not, so
    # an unmatched `case` is a no-op returning 0: neither `pass` nor `fail` would move, no line
    # would print, and verify would report every check passed having asserted NOTHING about PID 1.
    # The verdict words and these arms are two lists, and the next edit to reaper_verdict is the
    # one that forgets this one.
    *)                bad "could not establish whether PID 1 reaps: unrecognised verdict '$(reaper_verdict "$pid1_argv" "$orphan" "$adopted" "$final")' — reaper_verdict gained a word this case has no arm for" ;;
esac

# 2. THE NESTED SANDBOX'S MECHANISM, measured by the one probe (D54.3).
#
# `bwrap-probe.sh` runs Claude Code's own bubblewrap invocation in two steps -- namespaces and
# filesystem, then the /proc mount -- and prints three lines. It is a separate script because
# ci.yml needed the same measurement and had its own copy of it: the two were byte-equivalent
# until this branch strengthened one, after which CI's "why bubblewrap can or cannot start" table
# reported the mechanism working in exactly the state the gate beside it was failing on. See that
# script's header for whose invocation it is and what is not enforced about it.
#
# THE ERROR IS REPORTED, NOT DISCARDED. This used to send stderr to /dev/null and advise "check the
# seccomp profile is applied" -- one cause among several, and the wrong one wherever the profile
# demonstrably IS applied, which docker guarantees by failing outright on a missing profile file. A
# remedy that does not fit the failure is worse than no remedy (D48).
probe_out="$("$(dirname "$0")/bwrap-probe.sh" 2>&1)"
bwrap_ns="$(printf '%s\n' "$probe_out"   | sed -n 's/^BWRAP-NS=//p')"
bwrap_proc="$(printf '%s\n' "$probe_out" | sed -n 's/^BWRAP-PROC=//p')"
bwrap_err="$(printf '%s\n' "$probe_out"  | sed -n 's/^BWRAP-WHY=//p')"

if [ "$bwrap_ns" = OK ] && [ "$bwrap_proc" = OK ]; then
    ok "bubblewrap creates its namespaces and mounts /proc (the nested sandbox's mechanism works)"
elif [ -z "$bwrap_ns" ]; then
    # THE PROBE ITSELF DID NOT ANSWER, which is not a measurement of the kernel. Reported as a bad
    # rather than folded into the refusal below, or "the probe is missing" reads as "the sandbox
    # cannot start" and sends a reader to audit the host.
    bad "the bubblewrap probe produced no verdict -- nothing establishes whether the nested sandbox can start"
    [ -n "$probe_out" ] && printf '       it said: %s\n' "$(printf '%s' "$probe_out" | head -2 | tr '\n' ' ')"
else
    # ACCEPTED ONLY WHEN AN OPERATOR SAID SO, by name, for this host class. Still reported at full
    # volume and still naming the reason -- what changes is the exit code, so a caller can tell a
    # container whose nested sandbox is broken by surprise from one running on a host where it is
    # known not to start. Never inferred from the AppArmor setting: a check that quietly excused
    # itself whenever it found the condition it suspects would be assuming the answer.
    bw_bad=bad; [ "${JKB_ACCEPT_NO_BWRAP:-0}" = 1 ] && bw_bad=accept_bad
    $bw_bad "bubblewrap cannot create namespaces or mount /proc — the nested sandbox cannot start"
    printf '       namespaces: %s, /proc mount: %s\n' "$bwrap_ns" "$bwrap_proc"
    [ -n "$bwrap_err" ] && printf '       bwrap said: %s\n' "$bwrap_err"
    # THE CAUSE THAT IS VISIBLE FROM INSIDE. The two sysctls below explain a refusal to create the
    # user namespace; a submount over /proc explains a refusal to mount proc INSIDE one, which is
    # what `systempaths=unconfined` removes. Counted rather than asserted: a container legitimately
    # has some, and what matters is that a reader sees the number beside bwrap's own message
    # rather than being sent to audit two sysctls when neither is the cause.
    [ -n "$proc_submounts" ] \
        && printf '       submounts under /proc = %s (any at all defeat the proc mount; see systempaths=unconfined)\n' "$proc_submounts"
    # Reported as facts, with no cause asserted. `apparmor_restrict_unprivileged_userns=1` is the
    # Ubuntu 24.04+ default and restricts exactly this; `max_user_namespaces=0` disables it
    # outright. Neither is fixable from inside the container — they are the host's.
    for f in kernel/apparmor_restrict_unprivileged_userns user/max_user_namespaces; do
        v="$(cat "/proc/sys/$f" 2>/dev/null)" \
            && printf '       host %s = %s\n' "$(basename "$f")" "$v"
    done
    # The profile confining THIS process, the one AppArmor fact observable from in here.
    # `docker-default (enforce)` means a profile is mediating our syscalls; `unconfined` rules
    # AppArmor out and leaves seccomp or the kernel as the remaining candidates.
    v="$(cat /proc/self/attr/current 2>/dev/null)" \
        && printf '       apparmor profile in force: %s\n' "$v"
    printf '       the seccomp profile is applied by the run flags, so it is not usually the cause;\n'
    printf '       for the flag-by-flag discrimination run, from the host:\n'
    printf '           ./.container/mutate-verify.sh --ladder\n'
fi

# 2b. WHICH AppArmor PROFILE IS IN FORCE, and that relaxing one rule did not relax the rest.
#
# docker-default denies `mount`, which is why bubblewrap could not start; the answer was a profile
# that is docker-default with that ONE rule allowed, rather than `apparmor=unconfined`. That choice
# is worth nothing unless it is checked, and it fails silently in the direction that matters: a
# container started without the security-opt gets docker-default and simply cannot run the nested
# sandbox, while one started with `unconfined` runs it with no profile at all.
#
# So this asserts the profile by NAME, and then asserts that a restriction the relaxed profile is
# supposed to have KEPT still bites. Reading the name alone would pass for a profile called
# jkb-dev that had been edited into permitting everything -- the name is a label, not the policy.
# lib.sh FIRST: the mediation predicate and the profile name both live there, and this block needs
# the first of them. It used to be sourced six lines below, which is why the predicate got written
# out again here -- and the copy was not the same rule as run.sh's, so the launcher deciding whether
# to pass `--security-opt` and the verifier deciding what it should see disagreed about one host.
. "$(dirname "$0")/lib.sh" 2>/dev/null || true

# 2c. THE DECLARED /proc UNMASK IS IN FORCE — asserted as the flag's DIRECT EFFECT, which is the
# whole point of this assertion existing (D53.1).
#
# `systempaths=unconfined` does exactly one thing: it removes docker's submounts over /proc, and
# that is observable from in here on EVERY host. Whether the kernel then refuses a nested proc
# mount is a CONSEQUENCE, mediated by mount_too_revealing(), and assertion 2 above is what measures
# it. Splitting them is the point: the mutation that removes this flag used to be judged on the
# consequence, and a probe too weak to see the consequence then reported the flag as inert.
#
# DERIVED FROM THE DECLARATION, never assumed, so it follows container.json rather than having to
# be remembered: a declaration that stops carrying the flag makes this a note asserting nothing
# instead of a failure on every run.
# READ THROUGH `$( )` FIRST. Piping the reader into grep discards its refusal, so an unreadable or
# unparseable container.json -- or a jq that is not installed -- would take the `else` branch and
# print a note, asserting nothing: "the declaration could not be read" spelled the same as "the
# declaration says no". Same rule as lib.sh's, one file along.
if ! declared_run_args="$(dc_run_args "$(dirname "$0")/container.json" "$(cd "$(dirname "$0")/.." && pwd)" 2>/dev/null)"; then
    bad "container.json's runArgs could not be read from inside the container — nothing establishes whether the declared /proc unmask is in force (${proc_submounts:-an unreadable count of} submounts under /proc)"
elif grep -qxF 'systempaths=unconfined' <<<"$declared_run_args"; then
    if [ -z "$proc_submounts" ]; then
        bad "/proc/self/mountinfo could not be read, so whether the declared /proc unmask is in force was not established — which is not the same as finding it in force"
    elif [ "$proc_submounts" -eq 0 ]; then
        ok "the declared /proc unmask is in force (no submounts under /proc)"
    else
        bad "the declared /proc unmask is not in force: $proc_submounts submounts under /proc — container.json passes systempaths=unconfined and this container did not get it, so bubblewrap may be unable to mount proc"
    fi
else
    note "container.json declares no /proc unmask; $proc_submounts submounts under /proc (asserting nothing)"
fi

# 2d. THE DECLARATION'S OTHER TWO READERS, asserted the same way (D54.2).
#
# container.json is read by three declaration readers -- runArgs (2c above), remoteUser and
# containerEnv -- and until now only the first had any evidence that its effect reached the running
# container. That asymmetry was load-bearing: the static gate compared the harness's control
# against the declaration for all three, so the effects of the other two were "checked" only by a
# guard over a copy, and dropping `--user` or the environment from the assembly left every check
# green. The guard over the copy is gone (D54.1) and the copy with it; these are what replace it,
# and they are evidence of a different kind -- what the container IS, not what a flag list SAYS.
#
# Each derives from the declaration, so a declaration that stops saying something makes the
# assertion a note rather than a failure nobody can clear.
cfg_path="$(dirname "$0")/container.json"
cfg_root="$(cd "$(dirname "$0")/.." && pwd)"

# Read through `$( )` so the reader's refusal reaches this branch: piping into a comparison would
# spell "the declaration could not be read" exactly like "the declaration says nothing".
if ! declared_user="$(dc_remote_user "$cfg_path" 2>/dev/null)"; then
    bad "container.json's remoteUser could not be read from inside the container — nothing establishes which user this container runs as"
elif [ -z "$declared_user" ]; then
    note "container.json declares no remoteUser (asserting nothing about the running user)"
elif [ "$(id -un)" = "$declared_user" ]; then
    ok "running as the declared user ($declared_user)"
else
    # Not the same assertion as #1 above, which asks only for non-root. A container running as some
    # OTHER non-root user passes that and is still not the container that was declared: the state
    # volumes, the cargo caches and ~/.jkb are all owned by the declared user, so it would fail on
    # first write with an error naming a path rather than a user.
    bad "this container runs as '$(id -un)', but container.json declares remoteUser '$declared_user' — the volumes and caches are owned by the declared user"
fi

if ! declared_env="$(dc_container_env "$cfg_path" "$cfg_root" 2>/dev/null)"; then
    bad "container.json's containerEnv could not be read from inside the container — nothing establishes that its environment reached this container"
elif [ -z "$declared_env" ]; then
    note "container.json declares no containerEnv (asserting nothing about the environment)"
else
    env_missing=""; env_wrong=""; env_armed=""; env_n=0
    while IFS= read -r decl; do
        [ -n "$decl" ] || continue
        env_n=$((env_n+1))
        k="${decl%%=*}"; want="${decl#*=}"
        # `printenv` rather than `[ -z "${!k}" ]`: an empty declared value is a legitimate
        # declaration, and indirect expansion cannot tell it from an unset name.
        if ! got="$(printenv "$k")"; then env_missing="$env_missing $k"
        elif [ "$k" = JKB_EGRESS_ACCEPT_UNFILTERED ] && [ "$got" = 1 ]; then
            # The documented escape (D50.6) is armed at `docker run`, which is the only way to arm
            # it: an ACCEPTED failure, like the disarmed boot gate it causes, or a container run
            # with the override exits 1 and is told to fix the condition it chose.
            env_armed="$env_armed $k(=$got, declared $want)"
        elif [ "$got" != "$want" ]; then env_wrong="$env_wrong $k(=$got, declared $want)"
        fi
    done <<<"$declared_env"
    if [ -n "$env_missing" ]; then
        bad "container.json declares these environment entries and this container does not carry them:$env_missing — it was not started from the declaration"
    elif [ -n "$env_wrong" ]; then
        # JKB_EGRESS_ACCEPT_UNFILTERED is the entry that will not stay at its default (D50.6), and
        # an override set at `docker run` beats containerEnv silently. Reported as a difference
        # rather than as an absence, because those have different causes and different repairs.
        bad "container.json's environment reached this container with different values:$env_wrong — something overrode the declaration at run time"
    elif [ -z "$env_armed" ]; then
        ok "every declared environment entry reached this container ($env_n checked)"
    fi
    # Reported beside a real difference as well as alone, so arming the override never hides one.
    if [ -n "$env_armed" ]; then
        accept_bad "container.json's environment reached this container with different values:$env_armed — the egress override was armed at run time"
    fi
fi

# WHETHER APPARMOR MEDIATES IS ASKED FIRST, of the host, via the one shared predicate. Only then is
# the process label read -- and only to learn WHICH profile is in force, which is a different
# question from whether the LSM is active at all.
#
# Prefer the AppArmor-specific attr file where the kernel offers it: it cannot return another LSM's
# label, so there is nothing to misread. `/proc/self/attr/current` is the GENERIC LSM interface and
# on Fedora/RHEL returns an SELinux context like `system_u:system_r:container_t:s0:c103,c771`,
# which matched no arm below and FAILed with "an unexpected AppArmor profile is in force" -- a
# confident wrong verdict on a correctly configured container, after which run.sh declines to open
# a window.
if ! dc_apparmor_mediates; then
    aa_profile="__no_apparmor__"
elif [ -r /proc/self/attr/apparmor/current ]; then
    aa_profile="$(sed 's/ (.*//' /proc/self/attr/apparmor/current 2>/dev/null)"
else
    aa_profile="$(sed 's/ (.*//' /proc/self/attr/current 2>/dev/null)"
fi
# The name below is derived from the profile file in the checkout, never a literal here: that
# comparison is the whole assertion, and a name that drifted from the file would check the wrong
# thing.
aa_want="$(dc_apparmor_profile "$(dirname "$0")/apparmor-jkb-dev" 2>/dev/null)"
# NO LITERAL FALLBACK. This used to be `[ -n "$aa_want" ] || aa_want=jkb-dev`, which turns "I could
# not read the profile's name" into "the name is jkb-dev" -- an unestablished answer spelled as a
# definite one, two lines under a comment saying never to use a literal here. It also MASKED a real
# defect: dc_apparmor_profile could not read upstream's quoted `profile "jkb-dev"` form and returned
# empty, so the assertion below was comparing against the fallback and passing for the wrong reason.
if [ -z "$aa_want" ]; then
    bad "cannot read the profile name from .container/apparmor-jkb-dev — every AppArmor assertion below compares against it, and run.sh passes it to \`docker run --security-opt apparmor=\`"
fi
case "$aa_profile" in
    __no_apparmor__)
                   note "AppArmor is not the mediating LSM on this host (SELinux, or none) — the container correctly declares no AppArmor profile, and there is nothing here to check" ;;
    # REACHED ONLY WHEN APPARMOR DOES MEDIATE AND THE LABEL WOULD NOT READ -- a host without it is
    # `__no_apparmor__` above. So this says what was OBSERVED and claims nothing about the host:
    # the macOS example this used to carry now belongs to the arm above, and printing it here
    # spelled an unread label as a definite answer, in the file whose whole subject is that those
    # are different things.
    "")            note "AppArmor mediates on this host, but this process's profile label could not be read — nothing was established about what is confining the container" ;;
    unconfined)    bad "AppArmor is not confining this container (unconfined) — the container ships a profile that keeps every docker-default restriction except \`mount\`; running unconfined discards all of them" ;;
    docker-default)
                   bad "AppArmor is applying docker-default, which denies \`mount\` — bubblewrap cannot start under it, so the nested sandbox is not running. Load the container's profile: sudo apparmor_parser -r -W .container/apparmor-jkb-dev" ;;
    "$aa_want")
        # THE POLICY, NOT THE LABEL -- a name is a label, and a profile called jkb-dev that had
        # been edited into permitting everything would pass a name check.
        #
        # THE PREVIOUS PROBE COULD NOT FIRE, and its own comment contained the refutation: it wrote
        # to /proc/sysrq-trigger, "root-writable, and this container is not root". A write that is
        # refused by the permission bits is refused whether or not AppArmor is loaded, so the
        # strongest assurance in this file was printed for a gutted profile. The rule the whole
        # directory keeps relearning: assert on a signal that DISCRIMINATES, and a denial only
        # discriminates when nothing else could have produced it.
        #
        # /proc/kcore does. Docker masks it by bind-mounting /dev/null over it, so under a
        # permissive profile the read SUCCEEDS (0 bytes from a world-readable char device), while
        # docker-default's `deny @{PROC}/kcore rwklx` -- which this profile keeps verbatim --
        # refuses it. Two things are established before the answer is believed:
        #
        #   the mask   `-c` says it is the char device runc substituted. Without the mask (a
        #              `systempaths=unconfined` container) the real /proc/kcore is 0400 root:root
        #              and DAC would refuse it, which is the old defect exactly.
        #   a control  a read that must succeed. If reads are failing wholesale the denial says
        #              nothing about policy, so that is reported as unestablished, not as a pass.
        # NOW PERMANENTLY THE FIRST ARM, because container.json passes `systempaths=unconfined` --
        # the nested sandbox cannot start without it. The mask was what made this profile testable:
        # docker-default's denials otherwise overlap what DAC already restricts to root, so a
        # denial discriminates nothing. Kept rather than deleted so the check returns if the mask
        # ever does. What replaces it is the bwrap probe above, which now mounts `proc`:
        # docker-default denies `mount`, so on an AppArmor host a pass there separates this profile
        # from the stock one. Named residual: it does NOT separate this profile from
        # `apparmor=unconfined`, which passes the same probe, so an edited profile that still
        # allows `mount` is undetectable from in here.
        if [ ! -c /proc/kcore ]; then
            note "cannot check the profile's POLICY: /proc/kcore is not the masked char device this probe needs, so a denial could come from the permission bits rather than from AppArmor ($aa_want is in force by name)"
        elif ! head -c1 /proc/self/cmdline >/dev/null 2>&1; then
            note "cannot check the profile's POLICY: the control read failed too, so a denial here would establish nothing ($aa_want is in force by name)"
        elif head -c1 /proc/kcore >/dev/null 2>&1; then
            bad "the AppArmor profile $aa_want is in force but a docker-default restriction it is supposed to keep (deny @{PROC}/kcore) did not apply — /proc/kcore was readable, so the loaded profile is not the one in this repo"
        else
            ok "AppArmor is applying the container's own profile ($aa_want), and a restriction it keeps (deny @{PROC}/kcore) still bites while a control read succeeds"
        fi ;;
    *)             bad "an unexpected AppArmor profile is in force ($aa_profile) — this container declares $aa_want" ;;
esac

# 3. THE MOUNT BOUNDARY IS THE POINT, so assert it EXHAUSTIVELY rather than by listing paths
#    that ought to be absent. A list of absences can never be complete — it is the same
#    "enumerate the secrets" shape the host posture has to settle for because permission rules
#    cannot express default-deny. Here the kernel can, so ask it: every host path mounted in shows
#    up in /proc/self/mountinfo, and the set must be exactly what container.json declares.
#    A mount added in a hurry fails this line by name, including one nobody thought to forbid.
# EVERY mount point the kernel reports, minus the runtime's own fixed set — NOT the ones that
# happen to live under /home/vscode. Filtering by target prefix made this a list of absences with
# two entries: mounting /var/run/docker.sock (root on the host) or $HOME at /host passed cleanly.
#
# The expected set is DERIVED from container.json rather than transcribed beside it. A
# hand-kept copy went stale the first time the mounts changed: renaming the cargo target volume
# deleted the registry line with it, so a correctly-built container failed this very assertion
# after a full toolchain build. Two lists that must agree is the defect; there is now one list.
# Deriving does not weaken it — the question this asks is "does the running container match what
# it declares", and editing the declaration changes nothing until a human rebuilds.
# Derived through .container/lib.sh, the SAME function check-config.sh uses on the host, so
# the gate that reviews the boundary and the check that enforces it cannot read it differently.
here_dc="$(cd "$(dirname "$0")" && pwd)"
DC="$here_dc/container.json"
# The checkout being verified: the one this script is in. Every assertion below that used to name
# /home/vscode/repos/jkb reads this instead — with all of ~/repos mounted, that literal is a
# statement about whichever repo happens to sit there, which is not necessarily this one.
mem_repo="$(cd "$here_dc/.." && pwd)"
# shellcheck source=/dev/null
. "$here_dc/lib.sh"
EXPECTED="$(dc_mount_targets "$DC")"
# RUNTIME_OWNED — the exclusion list — is defined at the top of this file, above the
# inside-the-container refusal, so `--self-test` can exercise it on a host with no Docker.
# The READ is kept separate from the result, because "there were no mounts" and "the table could
# not be read" are different facts and the second must never be spelled like the first. `EXPECTED`
# has been guarded that way since it was derived; `actual` was not, so an unreadable table made
# the boundary check pass having compared nothing.
if mountinfo="$(cat /proc/self/mountinfo 2>/dev/null)" && [ -n "$mountinfo" ]; then
    mounts_readable=yes
    actual="$(awk '{print $5}' <<<"$mountinfo" | sort -u | grep -Ev "$RUNTIME_OWNED" || true)"
else
    mounts_readable=no
    actual=""
fi
# A failed derivation would make every real mount look undeclared — a true failure, but reported
# as the wrong thing. Say which it is, so the fix is not looked for in container.json's mounts.
#
# The caller's own declarations are folded in HERE, after the derivation and never instead of it,
# and each is refused unless it is strictly inside something already derived. `$d/*` is a strict
# descendant test on purpose: `--declare` cannot restate a declared target (which would be a
# no-op) and cannot name one of its ancestors (which would widen the set upwards).
extra=0
for t in ${DECLARED_EXTRA[@]+"${DECLARED_EXTRA[@]}"}; do
    nested=no
    while IFS= read -r d; do
        [ -n "$d" ] || continue
        case "$t" in "$d"/?*) ;; *) continue ;; esac
        # ...and only inside a BIND. A named volume is container-managed and reaches no host
        # filesystem, which is exactly why check-config.sh reviews bind sources and waves volumes
        # through — so "inside a declared region" cannot mean inside a volume: a bind nested under
        # ~/.cargo/target would be a host mount at a place nobody reviewed. `dc_type_for_target`
        # is lib.sh's own answer to that question, used here rather than derived a second way.
        [ "$(dc_type_for_target "$DC" "$d")" = bind ] || continue
        nested=yes; break
    done <<<"$EXPECTED"
    if [ "$nested" = yes ]; then
        EXPECTED="$(printf '%s\n%s\n' "$EXPECTED" "$t" | sort -u)"
        extra=$((extra+1))
    else
        bad "--declare $t is not inside any host BIND $DC declares — only a mount nested in a declared bind may be named here (a named volume reaches no host filesystem, so nothing under one is reviewable this way)"
    fi
done
if [ "$mounts_readable" != yes ]; then
    bad "could not read /proc/self/mountinfo — the mount boundary was not checked, which is not the same as finding it clean"
elif [ -z "$EXPECTED" ]; then
    bad "could not derive the declared mounts from $DC — the check below cannot mean anything"
else
    unexpected="$(comm -23 <(printf '%s\n' "$actual") <(printf '%s\n' "$EXPECTED"))"
    if [ -z "$unexpected" ]; then
        ok "every mount point is declared or runtime-owned ($(printf '%s\n' "$actual" | grep -c . ) checked against $(printf '%s\n' "$EXPECTED" | grep -c . ) declared$([ "$extra" -gt 0 ] && printf ', %s of them nested and named with --declare' "$extra"))"
    else
        bad "UNDECLARED mounts: $(tr '\n' ' ' <<<"$unexpected")"
    fi
fi

#    ...and the one that would quietly undo the whole posture: ~/.claude must NOT be a host mount.
#    The container writes its own settings.json there (that is the posture, installed by setup.sh);
#    what must never appear is the HOST's, which the agent would then be able to read and which is
#    the file that decides whether it is sandboxed at all. Nothing under ~/.claude is mounted from
#    the host at all — not even the credential file, which is why you log in inside the container.
#    Matched as a PREFIX, not for equality: a bind at ~/.claude/settings.json is the posture file
#    itself and an equality test waves it through, which is the one mount that matters most here.
# From the table read ONCE and guarded above, not a second open: re-reading it here meant an
# unreadable mountinfo printed `ok  nothing under ~/.claude is a host mount` having compared
# nothing — the same defect fixed one assertion over, still live in its neighbour.
claude_mounts="$(awk '$5 == "/home/vscode/.claude" || index($5, "/home/vscode/.claude/") == 1 {print $5}' <<<"$mountinfo")"
assert "nothing under ~/.claude is a host mount${claude_mounts:+ (found: $(tr '\n' ' ' <<<"$claude_mounts"))}" \
    "$([ -z "$claude_mounts" ] && echo yes || echo no)"

# 3b. ROOT IS REACHABLE OR IT IS NOT, and everything above depends on which. The mount boundary,
#     the root-owned firewall, its allowlist snapshot and the pinned sudoers argument are all
#     protections against a process that cannot become root — and the devcontainers base image
#     ships /etc/sudoers.d/vscode granting `NOPASSWD:ALL`, so until that is removed the agent can
#     undo every one of them with a single sudo. Asked of sudo itself rather than of the file,
#     because what matters is the policy in force: every command vscode may run as root must be
#     the firewall. A blanket grant re-added by any route fails here.
#     The grant is only as good as the file it names. Replacing the script, or the snapshot beside
#     it, is a root shell by the sudoers entry's own permission — and unlink/replace is governed by
#     the containing DIRECTORY, so the directories are asserted, not just the files.
unwritable_ok=1
# /usr/local is in the list because it governs REPLACING bin and share: `mv /usr/local/bin aside
# && mkdir /usr/local/bin && cp evil .../init-firewall.sh` needs write on the PARENT, not on the
# two directories, and every path below would still test unwritable while the sudoers entry ran the
# agent's script as root. Debian ships /usr/local as root:staff drwxrwsr-x, so this is one group
# membership away on a floating base tag.
# The FILES come from the Dockerfile's own root-owned COPY lines, so a script added there is
# covered without being remembered into this list -- which is how entrypoint.sh, egress-status.sh
# and egress-lib.sh came to have no ownership check while the Dockerfile claimed they did. The
# DIRECTORIES stay explicit below: they are not COPY targets, and they are what governs replacing
# the files inside them.
root_installed="$(dc_root_installed "$here_dc/Dockerfile")"
# PINNED AGAINST AN EMPTY DERIVATION, like every other derived list here. A grep that silently
# matched nothing would leave this loop checking only the directories and still print its ok --
# a guard reporting success having examined none of the files it exists for.
if [ -z "$root_installed" ]; then
    bad "no root-owned COPY targets could be derived from the Dockerfile — the check that the agent cannot replace the scripts sudo runs as root is examining nothing"
    unwritable_ok=0
fi
for path in /usr/local /usr/local/bin /usr/local/share \
            /usr/local/share/jkb-egress-allowlist.json /etc/sudoers.d $root_installed; do
    # A path that does not exist yet (the snapshot, before the first raise) cannot be replaced
    # either, so absence is fine; what must never be true is that it exists AND is writable.
    if [ -e "$path" ] && [ -w "$path" ]; then
        bad "$path is writable by $(id -un) — the firewall sudo runs as root can be replaced"
        unwritable_ok=0
    fi
done
[ "$unwritable_ok" -eq 1 ] && ok "the root-owned firewall and its allowlist cannot be replaced from here"

#     `sudo -n -l` failing and `sudo -n -l` listing nothing are different facts, and collapsing
#     them reported the friendliest one: sudo missing, PAM broken or sudoers unparseable all
#     produced an empty list and read as "no passwordless root". Worse, an unparseable sudoers is
#     a state in which the FIREWALL cannot run either, so silence there is the wrong direction.
sudo_raw="$(sudo -n -l 2>&1)"; sudo_rc=$?
sudo_entries="$(grep -E '^[[:space:]]*\(' <<<"$sudo_raw" || true)"
if [ "$sudo_rc" -ne 0 ] && [ -z "$sudo_entries" ]; then
    # `sudo -n -l` exits non-zero when the user may run NOTHING, which is a legitimate hardened
    # state — but only if sudo is actually working. Tell that apart from a broken sudo.
    if grep -qiE 'not allowed to run sudo|may not run sudo' <<<"$sudo_raw"; then
        ok "sudo works and grants this user nothing at all"
    else
        bad "sudo -n -l failed for a reason that is not 'nothing granted' (rc $sudo_rc): $(head -1 <<<"$sudo_raw") — the firewall cannot run either"
    fi
elif [ -z "$sudo_entries" ]; then
    bad "sudo -n -l succeeded but listed no grants — cannot establish what this user may run as root"
else
# THE ALLOWED SET IS NAMED, and it is exactly two: the firewall, which raises the rules, and the
# status probe, which reads them (D51.1). The probe is a second grant and therefore a second thing
# to justify — it is read-only, so a grant to run it is not a grant to change the boundary, and
# both are pinned to no arguments. Anything else at all fails here, whatever added it.
sudo_extra="$(grep -vE '/usr/local/bin/(init-firewall|egress-status)\.sh' <<<"$sudo_entries" || true)"
if [ -z "$sudo_extra" ]; then
    ok "the only commands permitted as root are the firewall and the egress probe ($(grep -c . <<<"$sudo_entries") grant(s))"
else
    bad "vscode may run more than the firewall and the egress probe as root: $(tr -s ' ' <<<"$sudo_extra" | tr '\n' ';')"
fi
fi

# 3c. The login-state links. container.json and the README both promise a login survives a
#     rebuild, and that promise is entirely these two symlinks — without them the credentials sit
#     in the writable layer and go with it. A promise made in two documents and checked nowhere is
#     the shape this change keeps finding, so it is checked here.
links_ok=1
#     Every link, not only the two whole-file ones. A real directory at a link site is silently
#     declined by `ln -sfn`, so asserting the files alone left seven ways for state to stay in the
#     container layer and vanish on rebuild with nothing reporting it.
for d in projects sessions history file-history shell-snapshots todos statsig; do
    if [ ! -L "/home/vscode/.claude/$d" ]; then
        bad "~/.claude/$d is not a link into the state volume — that state would not survive a rebuild"
        links_ok=0
    fi
done
#     The two whole-file ones come from lib.sh, the same list dc_persist_login links.
#     A REGULAR FILE at a link site is NOT a failure, and that was a reversal. Claude Code replaces
#     the credential link when it saves a login or a refreshed token (see dc_persist_login), so
#     after any login this state is the normal one, and failing it meant every healthy container
#     failed this check once someone logged in. The login is not lost: run.sh moves the file back
#     into the volume on its next start, `--stop` and `--rm`. What stays a failure is state that
#     will not be carried: a link missing entirely, a directory, or a link to anywhere else.
login_pending=""
while IFS= read -r pair; do
    link="${pair%%:*}"; want="${pair##*:}"
    case "$(login_link_state "$link" "$want")" in
        linked)    ;;
        pending)   login_pending="$login_pending $link" ;;
        elsewhere) bad "$link points at $(readlink "$link"), not $want"; links_ok=0 ;;
        notfile)   bad "$link is not a file or a link — dc_persist_login cannot carry it into the state volume"; links_ok=0 ;;
        *)         bad "$link is missing, not a link into the state volume — a login here would not survive a rebuild"; links_ok=0 ;;
    esac
done <<LOGIN_FILES
$(dc_login_files /home/vscode)
LOGIN_FILES
#     ...but "pending" is only true while the mover can still move it. dc_persist_login records
#     a failed move, so a file it could not carry is reported as the failure it is, and not as a
#     note that repeats, green, on every start.
if [ -f "/home/vscode/$DC_LOGIN_CARRY_FAILED" ]; then
    bad "the last attempt to move the login into the state volume failed: $(tr '\n' ' ' < "/home/vscode/$DC_LOGIN_CARRY_FAILED")— it stays in the writable layer until that is fixed"
    links_ok=0
elif [ -n "$login_pending" ]; then
    note "written since this container started, so not yet in the state volume:$login_pending — run.sh moves it there on its next start, --stop or --rm; removing the container any other way loses it"
fi
[ "$links_ok" -eq 1 ] && ok "login state is linked into the persistent volume${login_pending:+, or waiting for run.sh to move it there}"

# 3d. Auto-memory reaches the host, and the state volume does NOT give you this: Claude Code keys
#     memory by the project's ABSOLUTE PATH, so this container's /home/vscode/repos/jkb is a
#     different key from the host's, and widening the workspace mount does not change that. The
#     shared store lives inside the ~/.jkb bind that already exists, so nothing under ~/.claude is
#     mounted and the assertion above still holds. Asserted here for the reason 3c is: it is a
#     promise the README makes, and a promise checked nowhere is how this one would rot.
#     The slug comes from the linking script itself rather than being spelled again here — one
#     guess about another program's private encoding is enough.
#     ASKED OF THE LINKER, not inferred from the link's absence. The linker deliberately leaves
#     the link absent in states it recognises — a name that exists on both sides is a collision it
#     refuses to resolve, and a store holding a symlink is one it refuses to follow — so reading
#     "no link" as "the mechanism is broken" made a state its own design calls normal fail this
#     check, and with it postCreate, and with that the container. One rule, stated in one place.
# ASKED LIVE, EVERY TIME — and the create-time record is an ADDITIONAL alarm, never a substitute.
#
# It used to be the other way round, and that made the store guard unfirable after `postCreate`:
# the record says `linked`, so a redirect planted in the store the next day — the only time that
# state can arise, and exactly the sequence `link_one`'s comment describes — reported `ok`. The
# mirror case was a false FAIL that could not be cleared: after an `exposed` create, the printed
# remedy is a hand re-run of the linker, which writes no status file, so verify.sh kept reading
# `exposed` until a full rebuild.
#
# The record still earns its place: `link_one` REPAIRS, removing a live link into a poisoned store,
# so a live question asked afterwards sees the harmless `unsafe` and not the `exposed` that was
# true at create. So both are consulted, the worse one decides, and an `exposed` record is
# consumed once reported — which is what makes the documented remedy actually clear it.
mem_key="$(basename "$mem_repo")"
mem_status_file=/home/vscode/.claude-state/memory-status
mem_live="$("$mem_repo/scripts/link-claude-memory.sh" --status "$mem_repo" 2>/dev/null)"
mem_recorded="$(awk -v k="$mem_key" '$1 == k { print $2 }' "$mem_status_file" 2>/dev/null | tail -1)"
# The two states ONLY THE RUN can know, so only the record can carry them. `exposed` because the
# repair clears its own alarm; `error` because it means the run stopped part-way — a migration that
# moved some files and then failed leaves a directory the live observer reads as an ordinary
# `unlinked`, and this check then blamed "the linker did not run", the one cause it is not. The
# `error)` arm below was unreachable until now: `status_of` never emits that word.
case "$mem_recorded" in
    exposed|error)
        mem_state="$mem_recorded"
        # Consumed: the alarm describes one create, and leaving it makes the remedy — a hand
        # re-run, which writes no status file — unable to clear it.
        if [ -w "$mem_status_file" ]; then
            awk -v k="$mem_key" '$1 != k' "$mem_status_file" > "$mem_status_file.new" 2>/dev/null \
                && mv "$mem_status_file.new" "$mem_status_file" 2>/dev/null || true
        fi ;;
    *)  mem_state="$mem_live" ;;
esac
case "$mem_state" in
    linked)
        ok "auto-memory is shared with the host through ~/.jkb" ;;
    exposed)
        # A live redirect out of the store, which the linker has just broken. FATAL: the container
        # is not in a state its own design permits, and saying so on the create that discovers it
        # is the only moment anyone reads this.
        bad "auto-memory was pointing into a store holding something that is not a plain file — the link has been removed; clean ~/.jkb/claude-memory and re-run scripts/link-claude-memory.sh" ;;
    unmanaged)
        # Not a FAIL, and distinct from `unlinked` on purpose: the linker walks `~/repos/*/`, so it
        # never visited this path — it did not fail at anything, it was never asked. Reporting that
        # as `unlinked` (which is fatal, and rightly) said the linker had broken.
        #
        # This became reachable when the container stopped being opened by Dev Containers: that
        # could only open a top-level repo, so every workspace was managed by construction. Now a
        # `jkb task work` session is an ordinary thing to open, and its memory really is its own —
        # on the host too, because Claude Code keys memory by absolute path. Whether a worktree's
        # memory should be merged into its repo's single store, where several sessions would then
        # collide on MEMORY.md, is filed as a decision rather than guessed at here.
        ok "auto-memory is not shared for $(basename "$mem_repo") (a session worktree, not a top-level repo under ~/repos) — memory written here stays in this workspace"
        ;;
    collision|unsafe|foreign)
        # Not a FAIL: the container works, one repo's memory just is not shared until a person
        # settles it. Said plainly, because a state nobody is told about is one nobody fixes.
        ok "auto-memory is not shared for $(basename "$mem_repo") ($mem_state) — run scripts/link-claude-memory.sh to see what it wants"
        ;;
    broken)
        # The link is live and points at a store directory that is GONE, so every memory write
        # into it fails. Not one of the declining states: nothing here wants a decision, the
        # linker recreates the directory. FATAL because until it is re-run, memory is silently
        # not being recorded — the failure mode with no symptom.
        bad "auto-memory points at a store directory that no longer exists — re-run scripts/link-claude-memory.sh" ;;
    error)
        # The linker ran and could not finish for this repo — from the RECORD, since a live
        # observation cannot see a run that stopped part-way. Distinct from `unlinked`, which is
        # "no link and nothing recorded", and from the declining states above, which want a person.
        bad "auto-memory could not be linked (state: error) — see scripts/link-claude-memory.sh's output in the create log" ;;
    unlinked|"")
        # FATAL, and setup.sh's comment now says the same. `unlinked` is not the linker declining
        # — a collision and a poisoned store have their own words above and pass — it means the
        # linker did not run or could not answer, and the README promises this works. The two
        # files used to state opposite rules about the same state.
        # Says what was OBSERVED, not why. The previous wording asserted "the linker did not run",
        # which this cannot establish: a hand re-run that fails part-way writes no status file and
        # lands here too.
        bad "auto-memory is not linked into the shared store (state: ${mem_state:-unknown}) — memory written in here would be invisible on the host; run scripts/link-claude-memory.sh and read what it says" ;;
    *)
        bad "scripts/link-claude-memory.sh --status answered '$mem_state', which this check does not recognise" ;;
esac

# 3e. Git runs the hooks the host runs. VS Code copies the host's ~/.gitconfig in on attach, so a
#     global core.hooksPath names a host directory; run.sh mirrors it (lib.sh's
#     dc_mirror_host_hooks). If the mirror is missing, git runs NO hooks and says nothing, which is
#     how commits made in here slipped past the host's hooks for as long as nobody looked. Asked of
#     git, not of a file: whatever ~/.gitconfig says, `--path` is where git will look.
# The reads go through lib.sh's dc_global_hooks_path, which run.sh's mirror uses too, so the two
# cannot disagree about what the setting is (a review found them reading it differently: this one
# without `--includes`). It scrubs the caller's repository selection itself.
hooks_read=set
hooks_raw="$(dc_global_hooks_path)" || { [ $? -eq 1 ] && hooks_read=unset || hooks_read=unreadable; }
hooks_dir="$(dc_global_hooks_path --path 2>/dev/null)" || hooks_dir=""
hooks_dir="$(dc_strip_slashes "$hooks_dir")"   # the mirror acts on the stripped path; so must this
# What run.sh recorded the host using, and what this start did with it, compared with what git in
# here reads (hooks_record_verdict; the record counts only if written since this start).
IFS="$(printf '\037')" read -r hooks_cmp hooks_host hooks_origin hooks_mirror hooks_applied <<HOOKREC
$(hooks_record_verdict "$DC_HOST_HOOKS_RECORD" "${JKB_NS_MARKER:-}" "$hooks_read" "$hooks_raw" "$hooks_dir")
HOOKREC
case "$hooks_cmp" in
    apply-failed) bad "run.sh could not set core.hooksPath in $DC_HOOKS_XDG_CONFIG (the host's is $hooks_host), so git in here may run none of the host's hooks — a leftover $DC_HOOKS_XDG_CONFIG.lock is one cause; remove it if no git is running, then re-run run.sh" ;;
    not-applied) bad "the host sets core.hooksPath ($hooks_host, from ${hooks_origin:-an unknown file}) and run.sh set it to '$hooks_applied' in $DC_HOOKS_XDG_CONFIG, but git in here uses none — re-run run.sh and read its 'git hooks' step" ;;
    stale)       note "the host no longer sets core.hooksPath, but git in here still reads $hooks_raw from the ~/.gitconfig VS Code copied: it runs hooks the host has retired. To stop now: git config --global --unset core.hooksPath (in here); a reattach also refreshes the copy" ;;
    diverged)    note "the host's core.hooksPath is $hooks_host, but git in here reads $hooks_raw from the ~/.gitconfig VS Code copied, which predates the change. To use the host's now: git config --global --unset core.hooksPath (in here), and the value run.sh wrote to $DC_HOOKS_XDG_CONFIG applies; a reattach also refreshes the copy" ;;
esac
hooks_mount=none
[ -n "$hooks_dir" ] && hooks_mount="$(dc_hooks_mount_verdict "$hooks_dir" "$DC")"
case "$(hooks_dir_state "$hooks_read" "$hooks_raw" "$hooks_dir" "$hooks_mount" 0)" in
    unset)      ok "no global core.hooksPath: each repository's own .git/hooks runs" ;;
    in-repo)    ok "the global core.hooksPath ($hooks_raw) resolves inside each repository, which is mounted" ;;
    mirrored)   ok "git's hooks directory ($hooks_dir) is the host's, mirrored, owned by root and read-only here"
                # Mirrored by SOME start. When this one's refresh failed, it is an earlier copy.
                [ "$hooks_mirror" != failed ] \
                    || note "this start's mirror did not refresh it (see run.sh's 'git hooks' step), so it is an earlier start's copy of the host's hooks"
                # Read-only is only as strong as its PARENT: whoever can write that (and it is not
                # sticky) can rename the mirror away and put another directory in its place. That is
                # the container user for a ~/ path. The sandbox can do it only where its posture
                # grants writes, which ~/.config does not; said, since it cannot be fixed from here.
                hooks_parent="$(dirname "$hooks_dir")"
                if [ -w "$hooks_parent" ] && [ ! -k "$hooks_parent" ]; then
                    note "$hooks_parent is writable by $(id -un), so anything running as $(id -un) could replace the mirror; the sandbox can only where its posture grants writes to that directory"
                fi ;;
    shared)     ok "git's hooks directory ($hooks_dir) is inside a bind: the host's own, live"
                note "it is as writable in here as that bind is, so a sandboxed command can change hooks the host also runs — that is the host's configuration, not the mirror's" ;;
    unreadable) bad "git could not read its config, so which hooks run in here is unknown — run: $DC_HOOKS_SHOW_ORIGIN" ;;
    empty)      bad "git runs NO hooks in here: the global core.hooksPath is the empty string, which git reads as '/' — fix it on the host" ;;
    in-volume)  bad "the global core.hooksPath ($hooks_raw) is inside one of the container's volumes, so it is not the host's hooks and run.sh does not mirror it — point it elsewhere on the host" ;;
    unexpandable) bad "git cannot expand the global core.hooksPath ($hooks_raw) in here, so it runs NO hooks — use an absolute path or ~/ on the host" ;;
    missing)    if [ "$hooks_cmp" = diverged ]; then
                    bad "git runs NO hooks in here: the ~/.gitconfig VS Code copied says $hooks_raw, where nothing exists, and overrides the host's $hooks_host — run: git config --global --unset core.hooksPath (in here), and the mirror run.sh made applies"
                else
                    bad "git runs NO hooks in here: core.hooksPath is $hooks_raw and $hooks_dir does not exist, and run.sh mirrors the host's on start — re-run it, and read its 'git hooks' step"
                fi ;;
    unmarked)   if [ "$hooks_cmp" = diverged ]; then
                    bad "$hooks_dir is not the host's hooks: the ~/.gitconfig VS Code copied points there, overriding the host's $hooks_host — run: git config --global --unset core.hooksPath (in here), and the mirror run.sh made applies"
                else
                    bad "$hooks_dir is not the host's hooks: run.sh did not put it there, so it refuses to replace it — move it aside and re-run run.sh"
                fi ;;
    not-root)   bad "$hooks_dir carries the mirror's marker but is not owned by root, so it is not the mirror: a marker can be forged by anything that can write there — move it aside and re-run run.sh" ;;
    writable)   bad "$hooks_dir is writable from here, so a sandboxed command could plant a hook that runs unsandboxed on your next commit — re-run run.sh, which makes it root-owned" ;;
    *)          bad "the hooks-directory check answered something it does not recognise" ;;
esac

# 4. ...and these must be present, or the container is merely empty rather than confined.
# THE MOUNT, asked of the kernel — and asked about THIS checkout rather than about a path.
#
# Three versions of this, and the middle two are why the wording matters. A hard-coded
# /home/vscode/repos/jkb asserted something about whichever checkout sat there once ~/repos is
# mounted whole. Deriving it from where this script lives went too far the other way and asserted
# that the directory containing the running script contains a Cargo.toml — true by construction on
# every path that can execute this. Then requiring the DECLARED target to itself appear in
# mountinfo failed the harness outright: mutate-verify.sh mounts the repo AT
# /home/vscode/repos/jkb (it cannot mount the parent — in a `jkb task work` session $REPO's parent
# is `.jkb/work`), so /home/vscode/repos is never a mount point there. Measured: every mutation
# reported CAUGHT and then the control failed, so the harness judged nothing.
#
# What actually has to hold is that this checkout sits inside a mount point that is BOTH really
# mounted and declared. Both halves carry weight: mounted, so a repo baked into the image layer
# with the bind dropped fails rather than passing as "confined"; declared, so it is a mount the
# boundary knows about. `EXPECTED` is used deliberately — it has `--declare` folded in by now,
# which is the one mechanism that exists to name exactly this nested case.
ws_mounted=no
while IFS= read -r m; do
    [ -n "$m" ] || continue
    grep -qx "$m" <<<"$EXPECTED" || continue
    case "$mem_repo" in "$m"|"$m"/?*) ws_mounted=yes; break ;; *) ;; esac
done <<<"$actual"
assert "$mem_repo is inside a declared mount point" "$ws_mounted"
# ASKED OF THE KERNEL TOO, with the table already in hand. `[ -d /home/vscode/.jkb ]` was a test
# that `jkb` itself satisfies: `create_dir_all` runs on the parent of JKB_DB at three sites in
# main.rs, so any verb materialises the directory and this printed `ok` in a container where the
# bind was simply absent — a container-local database, a container-local memory store the linker
# happily reports `linked` for, and every write lost on the next rebuild. The boundary check above
# cannot cover it either: `comm -23 actual EXPECTED` reports mounts that are EXTRA, never a
# declared one that is missing.
kb_mounted=no
while IFS= read -r m; do
    [ "$m" = /home/vscode/.jkb ] && { kb_mounted=yes; break; }
done <<<"$actual"
assert "knowledge base is mounted" "$kb_mounted"

# REMOTE MODE, NOT A DATABASE OF ITS OWN (tasks S6.5). The container reaches the knowledge base only
# through jkb serve on the host. It had its own once — JKB_DB on the jkb-kb-local volume, which
# came up root-owned the first time so every `jkb` verb died creating it — and before that JKB_DB
# pointed through the ~/.jkb bind at the host's database, which a process on each kernel corrupts
# (.container/sqlite-share-probe.py). Both are gone; what is left to hold is that nothing brought
# either back. Remote mode refuses JKB_DB outright, so one set here fails every command anyway —
# asserted by name so the failure says why.
assert "JKB_REMOTE names the host daemon (${JKB_REMOTE:-unset})" \
    "$([ -n "${JKB_REMOTE:-}" ] && echo yes || echo no)"
assert "JKB_DB is not set (the container has no database of its own)" \
    "$([ -z "${JKB_DB:-}" ] && echo yes || echo no)"
# Behind remote mode, jkb-core's own refusals. Asked of the INSTALLED jkb, not of a third copy of
# the filesystem rule: jkb-core refuses a database on a filesystem shared with another kernel and
# any `file:` URI (crates/jkb-core/src/shared_fs.rs). A binary built before a given refusal still
# opens the host's database from in here — a review measured one — so the binary itself is asked.
# Those two probes run with remote mode off for that one process (`env -u JKB_REMOTE`): with it on,
# `--db` is refused before jkb-core sees the path, and they would pass testing nothing.
#
# Two things are conditional, and each for a stated reason:
#   - no `jkb` on PATH is a NOTE: the image does not install one (setup.sh does), and the mutation
#     harness runs this file against that bare image, where two FAILs made its control fail;
#   - the bind refusal is required only when the bind IS a shared filesystem, asked of lib.sh's
#     `shared_fs_kind` (the list shared_fs.rs is checked against). On a native Linux Docker host the
#     bind is same-kernel ext4, where opening is correct and refusing would be the bug.
# The `file:` refusal is required everywhere, because Rust refuses URIs on every filesystem.
kb_lib="$(cd "$(dirname "$0")/.." && pwd)/scripts/lib.sh"
kb_probe_dir="/home/vscode/.jkb/.verify-refusal-probe-$$"
if ! command -v jkb >/dev/null 2>&1; then
    note "no jkb installed here, so the installed-binary refusal checks did not run (setup.sh installs it)"
else
    kb_uri_refused=no
    if ! kb_out="$(env -u JKB_REMOTE jkb --db "file:$kb_probe_dir/uri.db" ns ls 2>&1)" \
        && grep -q "never \`file:\` URIs" <<<"$kb_out"; then
        kb_uri_refused=yes
    fi
    assert "the installed jkb refuses a file: URI database (rebuild it if not: setup.sh)" "$kb_uri_refused"

    kb_magic="$(stat -f -c %t /home/vscode/.jkb 2>/dev/null || true)"
    kb_kind="$(bash -c '. "$1" && shared_fs_kind "$2"' _ "$kb_lib" "$kb_magic" 2>/dev/null || true)"
    if [ -n "$kb_kind" ]; then
        kb_refused=no
        if ! kb_out="$(env -u JKB_REMOTE jkb --db "$kb_probe_dir/jkb.db" ns ls 2>&1)" \
            && grep -q "refusing to open a database" <<<"$kb_out" \
            && [ ! -e "$kb_probe_dir/jkb.db" ]; then
            kb_refused=yes
        fi
        assert "the installed jkb refuses a database on the $kb_kind host bind (rebuild it if not: setup.sh)" "$kb_refused"
    else
        note "the ~/.jkb bind is not a shared filesystem here (magic ${kb_magic:-unreadable}), so there is nothing for jkb to refuse on it"
    fi
    # ...and the installed jkb is in remote mode: it refuses to name a database at all. Asked without
    # the daemon, which this check must not need (mutate-verify.sh's containers have none).
    # Only with JKB_REMOTE set and JKB_DB unset (asserted above; `env -u` besides): otherwise this jkb
    # is not in remote mode, and the probe would open a database rather than be refused.
    kb_remote=no
    if [ -n "${JKB_REMOTE:-}" ] \
        && ! kb_out="$(env -u JKB_DB jkb --db "$kb_probe_dir/remote.db" ns ls 2>&1)" \
        && grep -q "refused with JKB_REMOTE set" <<<"$kb_out" \
        && [ ! -e "$kb_probe_dir/remote.db" ]; then
        kb_remote=yes
    fi
    assert "the installed jkb is in remote mode and refuses to open a database (rebuild it if not: setup.sh)" "$kb_remote"
fi
rm -rf -- "$kb_probe_dir" "$PWD/file:"

# 5. Egress default-deny. Asserted in BOTH directions: a firewall that blocks everything passes a
#    one-sided test while having broken the container.
# WHAT THE KERNEL HOLDS RIGHT NOW, not what egress happens to do and not what some past raise
# recorded. The two curl probes below both resolve a name, so a dead resolver produces the same
# answers as a deny-all — which is why this does not infer the boundary from them. And it no longer
# reads the RECORD for the state either (D51.1): a record is an event, this is a present-tense
# question, and the file outlives the rules it describes — `docker stop` destroys every chain while
# the file survives, so a container restarted without a successful raise reported the previous
# start's `allowlisted` as healthy. The probe reads the live chains and cannot be stale.
#
# The record still supplies the REASON, which the kernel cannot: that DNS failed, that the snapshot
# was truncated.
# The path and the parser come from egress-lib.sh, which sits beside this script in the checkout
# it runs from (D52.5). Spelling the path here ALSO meant this file ignored the
# JKB_EGRESS_VERDICT override the writer and the other reader both honour.
eg_probe="$(sudo -n /usr/local/bin/egress-status.sh 2>/dev/null)" || eg_probe=""
eg_state="$(kv_field state "$eg_probe")"
eg_v6="$(kv_field v6 "$eg_probe")"
eg_reason="$(verdict_field reason)"
[ -n "$eg_reason" ] || eg_reason="(the raise left no reason)"

# THE OVERRIDE IS READ, NEVER INFERRED (D51.4). This used to be deduced from an `unfiltered` state
# on the reasoning that the entrypoint refuses that state otherwise — which failed both ways. An
# operator who armed it to get past a refusal, then fixed the host, got `allowlisted` and silence:
# nothing reported that the boot gate was still disarmed, which is exactly the condition the
# override's own justification says must never be invisible. And an `unfiltered` state reached by
# any other route (a re-raise on a running container) was BLAMED on a variable that may be 0.
# `docker exec` inherits containerEnv, so the value is right here to be read.
eg_accept="${JKB_EGRESS_ACCEPT_UNFILTERED:-0}"
if [ "$eg_accept" = 1 ]; then
    accept_bad "the egress boot gate is DISARMED: JKB_EGRESS_ACCEPT_UNFILTERED=1, so this container will
       start even with unfiltered egress. Reported every run for as long as it is set — an override
       nobody can see is indistinguishable from a rule that does not exist. Unset it in
       container.json and recreate. (--open will not open a window while this holds.)"
fi

case "$eg_state" in
    allowlisted) ok "the firewall has an allowlist in the live chain (IPv4 bounded, IPv6 $eg_v6)" ;;
    denied)      bad "the live chain denies everything and has no allowlist: $eg_reason" ;;
    # Worded from the evidence this has, not from a cause it assumed. Whether the override is set is
    # reported separately, above, because it is a separate fact.
    # Accepted only when the override is what let it run: without it the entrypoint would have
    # refused, so an unfiltered chain HERE is the operator's stated choice rather than a defect.
    # With the override unset it stays an ordinary failure.
    unfiltered)  eg_bad=bad; [ "$eg_accept" = 1 ] && eg_bad=accept_bad
                 $eg_bad "egress is UNFILTERED in the live chain (IPv6 $eg_v6) and this container is
       running anyway: $eg_reason" ;;
    # No answer at all. The probe reads the live chains, so producing nothing means it could not
    # run — which says nothing about the network and is never read as bounded. This is also what a
    # container started with --entrypoint bash looks like, which is exactly when you most want to
    # be told the firewall's state is unestablished.
    *)           bad "could not establish whether egress is bounded (state=${eg_state:-<none>}).
       egress-status.sh reads the live chains; no answer means it could not run — sudo, the
       sudoers grant, or the image. Last recorded reason: $eg_reason" ;;
esac

# DRIFT between the two is worth saying and is never acted on. The record is what a past raise
# concluded; the chain is what is there. They disagree when a raise died after recording, when two
# raises fought, or when something changed the chain afterwards — each of which an operator wants
# to know about, and none of which changes the answer above.
eg_recorded="$(verdict_field state)"
if [ -n "$eg_recorded" ] && [ -n "$eg_state" ] && [ "$eg_recorded" != "$eg_state" ]; then
    note "the last raise recorded '$eg_recorded' but the live chain is '$eg_state' — the chain
       decides; the record is stale or something changed it afterwards"
fi
if curl -sS -m 6 -o /dev/null https://example.com 2>/dev/null; then
    reach_bad=bad; [ "$eg_accept" = 1 ] && reach_bad=accept_bad
    $reach_bad "egress to a NON-allowlisted host was permitted (example.com)"
else
    ok "egress to a non-allowlisted host is refused"
fi
if curl -sS -m 15 -o /dev/null https://api.anthropic.com/ 2>/dev/null \
   || [ "$(curl -sS -m 15 -o /dev/null -w '%{http_code}' https://api.anthropic.com/ 2>/dev/null)" != "000" ]; then
    ok "egress to an allowlisted host still works"
else
    bad "egress to an allowlisted host is blocked — the firewall is too tight to work in"
fi

# 5b. The one opening to the host: `jkb serve` (design r3.2 H5). The kernel's answer first, for the
#     same reason as above: a daemon that is down and a rule that is missing look identical from a
#     curl.
#
# `wide` is the one that matters most and the one no curl to the daemon's port can see: the daemon
# answers either way, and every OTHER port on the host's loopback answers too. That is also why
# there is no "another host port is refused" probe here. One was written (curl to :7118 expecting a
# refusal) and could not be shown to fire: on a Linux engine a closed host port answers with a
# refusal whether or not the rule is wide, so it passed in exactly the state it existed for. The
# other direction is asserted where it can be — the `wide` state, read from the live sets.
#
# WHERE THE IMAGE SAYS THE DAEMON IS. egress-status.sh runs the image's egress-lib.sh, and this
# script runs the checkout's; after a pull without a rebuild the two can name different ports, and a
# probe of the checkout's port would then blame com.jkb.serve for a rule on another one. So the
# address probed below is the one the kernel's answer was about.
eg_daemon="$(kv_field daemon "$eg_probe")"
daemon_at="$(kv_field daemon_at "$eg_probe")"
[ -n "$daemon_at" ] || daemon_at="$DAEMON_HOST:$DAEMON_PORT"

# THE OVERRIDE, AGAIN (D51.5). With JKB_EGRESS_ACCEPT_UNFILTERED=1 and no firewall at all there is no
# daemon rule either, and that absence is a consequence of the state the operator accepted — so it is
# an accepted failure like the reachability check above, or verify.sh exits 1 and run.sh tells the
# operator to fix something they chose.
dm_bad=bad; [ "$eg_accept" = 1 ] && [ "$eg_state" = unfiltered ] && dm_bad=accept_bad
case "$eg_daemon" in
    port)       ok "the firewall opens the host daemon's port ($daemon_at) and no other host port beyond DNS" ;;
    unresolved) $dm_bad "${daemon_at%:*} did not resolve when the firewall was raised, so no address is open for jkb serve on the host — re-run init-firewall.sh; on Linux add --add-host=${daemon_at%:*}:host-gateway" ;;
    absent)     $dm_bad "the firewall has no rule for jkb serve on the host ($daemon_at), so this container cannot reach the knowledge base" ;;
    wide)       $dm_bad "the host's address is in the egress allowlist, which opens EVERY port on the host's loopback to this container — jkb serve must be reached through its port-only rule alone" ;;
    *)          $dm_bad "could not establish the firewall's opening for jkb serve on the host (daemon=${eg_daemon:-<none>}) — egress-status.sh did not report it; an image built before the opening existed does not, so rebuild: ./.container/run.sh --rm && ./.container/run.sh --build" ;;
esac

# ...and what actually answers. The token is read from the ~/.jkb bind, where the host's daemon
# writes it, which is also what the container's `jkb` in remote mode will read — so this is the
# whole path a client takes, not just the port. `--noproxy '*'`: this runs outside the nested
# sandbox, and a proxy variable in the environment would test the proxy instead of the rule.
# The header goes through a file descriptor so the token is never in this process's argv.
# Keyed by the daemon's port (`~/.jkb/daemon/<port>/token`), which is how a client finds it.
daemon_token="${JKB_REMOTE_TOKEN_FILE:-$HOME/.jkb/daemon/${daemon_at##*:}/token}"
# NO TOKEN IS A FAILURE since the cutover (tasks S6.5): every jkb command in here goes to the daemon,
# so a container that cannot authenticate to it has no knowledge base at all. The one exception is
# a harness that builds a correct container with no host daemon behind it — mutate-verify.sh's
# scratch ~/.jkb, a CI runner — which says so with JKB_VERIFY_NO_DAEMON=1; the kernel's answer
# above is still asserted there. A NOTE then, never silence.
#
# ABSENT, not unreadable. A token that exists but this user cannot read (a uid mismatch across the
# bind) is a failure either way: remote mode cannot authenticate.
if [ ! -e "$daemon_token" ] && [ "${JKB_VERIFY_NO_DAEMON:-0}" = 1 ]; then
    note "there is no daemon token at $daemon_token and JKB_VERIFY_NO_DAEMON=1 says no host daemon is expected, so jkb serve was not asked"
elif [ ! -e "$daemon_token" ]; then
    bad "there is no daemon token at $daemon_token, so no jkb command in this container can reach the knowledge base — run ./scripts/setup.sh on the host to install com.jkb.serve"
elif [ ! -r "$daemon_token" ]; then
    bad "the daemon token at $daemon_token exists but this user cannot read it, so remote mode could not authenticate to jkb serve on the host ($(stat -c '%U:%G %a' "$daemon_token" 2>/dev/null || echo 'owner unreadable'))"
else
    daemon_hello="$(curl -sS --noproxy '*' -m 5 \
        -H @<(printf 'Authorization: Bearer %s\n' "$(cat "$daemon_token" 2>/dev/null)") \
        "http://$daemon_at/v1/hello" 2>/dev/null)" || daemon_hello=""
    case "$daemon_hello" in
        *'"protocol"'*)
            ok "jkb serve on the host answers this container, authenticated by the token on the ~/.jkb bind"
            # ...and the INSTALLED jkb gets there too. curl proves the path, not the binary: one built
            # before `JKB_REMOTE` accepted bare host:port reads the address as a URL scheme and fails
            # every command, while the `--db` refusal above passes on it. A read no database is needed
            # for, as setup.sh's activation asks it.
            # Never without JKB_REMOTE: this jkb would then open the database behind the bind — the
            # host's live one, on a native-Linux engine where nothing refuses it. And JKB_REMOTE must
            # name the address curl just reached: the firewall's opening is the IMAGE's, the variable
            # the checkout's, and after a port change without a rebuild they differ — which a
            # reinstall of jkb does not fix.
            jkb_remote_at="${JKB_REMOTE:-}"; jkb_remote_at="${jkb_remote_at#http://}"; jkb_remote_at="${jkb_remote_at%/}"
            if [ -z "$jkb_remote_at" ] || ! command -v jkb >/dev/null 2>&1; then
                :   # asserted above: remote mode unset fails there, no jkb is a note there
            elif [ "$jkb_remote_at" != "$daemon_at" ]; then
                $dm_bad "JKB_REMOTE (${JKB_REMOTE}) is not the address this image's firewall opens ($daemon_at) — the checkout and the image disagree; rebuild the image: ./.container/run.sh --rm && ./.container/run.sh --build"
            else
                if jkb_answer="$(env -u JKB_DB jkb --json mq topic ls 2>&1)"; then
                    ok "the installed jkb reaches jkb serve through JKB_REMOTE"
                else
                    $dm_bad "the installed jkb cannot reach jkb serve through JKB_REMOTE=${JKB_REMOTE:-unset} although curl can — rebuild it (setup.sh): $(head -c 300 <<<"$jkb_answer")"
                fi
            fi
            ;;
        *) $dm_bad "jkb serve on the host does not answer at http://$daemon_at with the token from $daemon_token — is com.jkb.serve running there? Its log is ~/.jkb/serve.log on the host; a VS Code port forward holding ${daemon_at##*:} on the host is one measured cause" ;;
    esac
fi

# 6. The inner posture. `check` is the drift rule from D48; here it also proves the posture
#    survived being installed into a fresh container HOME.
if "$mem_repo/scripts/auto-mode.sh" check >/dev/null 2>&1; then
    ok "Claude Code posture is intact"
else
    bad "Claude Code posture is NOT intact (scripts/auto-mode.sh check)"
fi

# 7. The declared extensions are actually installed. This is the assertion the failure that
#    prompted it did not have: VS Code's own install failed with ECONNREFUSED against the egress
#    firewall, logged it, and carried on — so `postCreate` went green with neither extension
#    present and the Claude Code extension, which is most of what this container is for, simply
#    absent. A non-fatal log line is not a guard.
#
#    Asked of the server, not of a directory listing: `--list-extensions` is what VS Code itself
#    considers installed, where a directory can be left behind by a failed install.
#
#    SKIPPED where there is no VS Code server — `run.sh`, a plain docker run and
#    mutate-verify.sh all build a correct container with no VS Code in it. The skip is printed
#    rather than silent.
#
#    THE TWO CONDITIONS ARE NOT THE SAME ONE, and the comment here used to claim they were. That
#    was true under Dev Containers, where the server was unpacked BEFORE postCreate. Now VS Code
#    installs its server when you ATTACH, which is after setup.sh has run — so setup.sh legitimately
#    finds no server and installs nothing, and by the time this runs on a later `run.sh` the server
#    exists and has nothing in it. They evaluate at different times and routinely disagree, which is
#    the ordinary state of every container between attaching and running install-extensions.sh.
#    Failing there aborted `run.sh` under `set -e` and printed "rebuild the container" — advice that
#    returns you to exactly that state. So the never-installed case REPORTS; a server that has
#    extensions but is missing a declared one is still a failure, because that is the original bug.
code_server="$(ls -d "$HOME"/.vscode-server/bin/*/bin/code-server 2>/dev/null | head -1 || true)"
if [ -z "$code_server" ]; then
    echo "  skip no VS Code server in this container — extensions not checked (not a VS Code launch)"
else
    installed="$("$code_server" --server-data-dir "$HOME/.vscode-server" --list-extensions 2>/dev/null || true)"
    declared="$(dc_extensions "$here_dc/container.json")"
    # ...plus the extension this repo builds itself, which is not in that list and cannot be: it is
    # not on the marketplace. It was absent from every container ever built precisely because
    # nothing declared it, so nothing checked it. Appended rather than checked separately so the
    # one matcher the self-test exercises covers it too.
    if local_ext="$(dc_local_extension "$(cd "$here_dc/.." && pwd)")"; then
        declared="$declared"$'\n'"$local_ext"
    fi
    missing="$(missing_extensions "$declared" "$installed")"
    present="$(printf '%s\n' "$installed" | grep -c . || true)"
    if [ -z "$missing" ]; then
        ok "every declared VS Code extension is installed ($present present)"
    elif [ "$present" -eq 0 ]; then
        # Nothing at all has been installed into this server, which is what attaching leaves behind
        # — not a broken install. The remedy is a command, and it is the command that exists for it.
        echo "  note this VS Code server has no extensions yet — attaching does not install them."
        echo "       Run  ./.container/install-extensions.sh  from a terminal in the attached window."
    else
        bad "declared extensions are not installed:$missing — $present other(s) are, so this is not the
       never-installed state. Run ./.container/install-extensions.sh from an attached terminal; if it
       reports one was not staged into the image, rebuild: ./.container/run.sh --rm && ./.container/run.sh --build"
    fi
fi

echo
echo "  note: that the sandbox actually ENGAGES for a tool call is not asserted here — it needs a"
echo "  live session. Inside one, run:  ./scripts/auto-mode.sh sandboxed   (control + canary, no"
echo "  cost; do NOT use printenv CLAUDE_CODE_SANDBOXED, which was unset on a host whose sandbox"
echo "  was provably enforcing), or"
echo "  ./scripts/auto-mode.sh probe   for the full write/egress/credential probe."
echo
# TWO KINDS OF FAILURE, TWO EXIT CODES (D51.5). One code meant both "the boundary is broken" and
# "this container is in a state you deliberately configured", and run.sh gated --open on it — so a
# host using the documented escape could never open a window, and the message told it to fix a
# condition the design REQUIRES to keep failing. The failure is still reported every run, at full
# volume; what changes is that a caller can tell the two apart.
#   1  a real failure
#   3  the only failures are conditions this container was configured to accept
if [ "$fail" -ne 0 ]; then
    printf '\033[31m%d failed\033[0m, %d passed\n' "$fail" "$pass"
    if [ "$accepted_failure" -ne 0 ] && [ "$fail" -eq "$accepted_failure" ]; then
        printf 'every failure above is a condition this container was configured to accept.\n'
        exit 3
    fi
    exit 1
fi
printf '\033[32mall %d container checks passed\033[0m\n' "$pass"
