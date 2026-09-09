#!/usr/bin/env bash
# CAN THE NESTED SANDBOX START HERE? The one bubblewrap probe (D54.3).
#
#   ./.container/bwrap-probe.sh                 measure, and print three lines
#   ./.container/bwrap-probe.sh --print-invocation [ns|proc]
#   ./.container/bwrap-probe.sh --self-test
#
# Prints exactly three lines, so a caller parses one shape:
#
#   BWRAP-NS=OK|FAILED            can bubblewrap create its namespaces and set up its filesystem
#   BWRAP-PROC=OK|FAILED|not-reached   ...and then mount a fresh /proc inside them
#   BWRAP-WHY=<bwrap's own last line, or empty>
#
# TWO STEPS, BECAUSE THERE ARE TWO REFUSALS WITH DIFFERENT CAUSES. Creating the user namespace is
# what seccomp and AppArmor decide. Mounting proc INSIDE one is what docker's masked /proc paths
# decide, via the kernel's mount_too_revealing(): a MaskedPath is a submount, so /proc is not
# "fully visible" and a fresh procfs is refused whatever the syscall filter allows. Folding them
# into one column destroys the question -- with `--proc` every arm of a seccomp/AppArmor experiment
# fails, so the discrimination those arms exist for reads as a flat wall of FAILED.
#
# `not-reached` IS A THIRD VALUE ON PURPOSE. An arm that died at namespace creation never reached
# mount_too_revealing(), so it establishes NOTHING about the proc mount, and printing FAILED there
# spells "unestablished" exactly like "proven no".
#
# THE TWO INVOCATIONS DIFFER BY `--proc /proc` AND BY NOTHING ELSE, which is what makes a flip
# between the columns attributable to the proc mount rather than to whatever else drifted. Asserted
# by --self-test, because that is a defect this probe has already shipped twice (see the README
# section "/proc has to be unmasked" for both).
#
# WHOSE INVOCATION THIS IS. Claude Code's own, read from the 2.1.260 bundle, which builds:
#
#     --new-session --die-with-parent [--unshare-net …] <fs binds, starting --bind / />
#     --dev /dev --unshare-pid --unshare-user --cap-drop ALL --proc /proc -- <shell> -c <cmd>
#
# `--unshare-pid` is the flag that makes the proc mount depend on the unmask; the measurements are
# in .container/README.md, once, under "/proc has to be unmasked". NOTHING ENFORCES EQUALITY WITH
# THE BINARY: the real builder emits `--ro-bind / /` where a write config is supplied and the
# posture always supplies one, so this differs there already. That does not change the proc verdict
# -- a read-only root bind is still a bind, and the refusal is about the procfs -- and pinning it
# properly means extracting the argv builder from the installed bundle, which is filed, not built.
set -uo pipefail

# ONE LIST, TWO INVOCATIONS. The proc step is the ns step plus `--proc /proc`, expressed that way
# rather than as two literals, so they cannot drift apart into two different experiments.
BWRAP_NS_ARGS=(--new-session --die-with-parent --unshare-net --bind / /
               --dev /dev --unshare-pid --unshare-user --cap-drop ALL)
BWRAP_PROC_ARGS=("${BWRAP_NS_ARGS[@]}" --proc /proc)

if [ "${1:-}" = --print-invocation ]; then
    case "${2:-proc}" in
        ns)   printf '%s\n' "${BWRAP_NS_ARGS[@]}" ;;
        proc) printf '%s\n' "${BWRAP_PROC_ARGS[@]}" ;;
        *)    echo "bwrap-probe.sh: --print-invocation takes 'ns' or 'proc'" >&2; exit 2 ;;
    esac
    exit 0
fi

if [ "${1:-}" = --self-test ]; then
    fails=0
    eq() { # eq <what> <got> <want>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else printf '  \033[31mFAIL\033[0m %s\n       got:  %s\n       want: %s\n' "$1" "$2" "$3"; fails=$((fails+1)); fi
    }
    echo "==> bwrap-probe self-test: the two invocations differ by the proc mount alone"
    # THE DIFFERENCE IS COMPUTED, NOT ASSERTED AS A LITERAL PAIR. Writing the expectation as a
    # second copy of the construction passes for any construction, including a wrong one -- the
    # shape this repo has been correcting all branch. So: take the proc invocation, remove the
    # elements the ns one has, and require what is left to be exactly the proc mount.
    ns="$(printf '%s\n' "${BWRAP_NS_ARGS[@]}")"
    proc="$(printf '%s\n' "${BWRAP_PROC_ARGS[@]}")"
    eq "the proc invocation starts with the ns invocation" \
       "$(printf '%s\n' "$proc" | head -n "$(printf '%s\n' "$ns" | grep -c .)")" "$ns"
    eq "...and adds exactly --proc /proc" \
       "$(printf '%s\n' "$proc" | tail -n +"$(( $(printf '%s\n' "$ns" | grep -c .) + 1 ))" | tr '\n' ' ')" \
       "--proc /proc "
    # THE ONE FLAG THE WHOLE BRANCH TURNS ON. Measured: without --unshare-pid the probe passes in
    # exactly the state the /proc unmask exists to fix, on macOS and on Linux alike, and it did --
    # for two review rounds, which is how a wrong conclusion about the unmask reached the README.
    eq "the probe unshares the pid namespace (without it, it passes in the broken state)" \
       "$(printf '%s\n' "$ns" | grep -cx -- '--unshare-pid')" "1"
    eq "the ns step does NOT mount proc (or the two columns measure one thing)" \
       "$(printf '%s\n' "$ns" | grep -cx -- '--proc')" "0"
    echo
    [ "$fails" -eq 0 ] || { printf '\033[31mbwrap-probe self-test FAILED (%d)\033[0m\n' "$fails"; exit 1; }
    printf '\033[32mbwrap-probe self-test passed\033[0m\n'
    exit 0
fi

if ! command -v bwrap >/dev/null 2>&1; then
    # NOT a measurement of the kernel. bubblewrap absent is a fact about the image, and reporting it
    # as FAILED would send a reader to audit seccomp, AppArmor and the masked paths for a missing
    # package.
    echo "BWRAP-NS=not-installed"
    echo "BWRAP-PROC=not-reached"
    echo "BWRAP-WHY=bwrap is not on PATH in this container"
    exit 0
fi

ns=FAILED; proc=FAILED; why=
if why="$(bwrap "${BWRAP_NS_ARGS[@]}" -- /bin/sh -c true 2>&1)"; then
    ns=OK; why=
    if why="$(bwrap "${BWRAP_PROC_ARGS[@]}" -- /bin/sh -c true 2>&1)"; then proc=OK; why=; fi
else
    proc=not-reached
fi
echo "BWRAP-NS=$ns"
echo "BWRAP-PROC=$proc"
# The LAST line: bwrap prints its own diagnosis there, and earlier lines are usually noise from the
# shell it could not reach. Flattened, because a caller renders this as one table cell.
echo "BWRAP-WHY=$(printf '%s' "$why" | tail -1 | tr '\n' ' ')"
