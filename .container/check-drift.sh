#!/usr/bin/env bash
# Every vendored artifact is still exactly what its generator produces (design D52.11).
#
# WHY THIS EXISTS. `.container/` vendors two files derived from moby's upstream policies: the
# seccomp profile and the AppArmor profile. Vendoring is deliberate -- the policy the container
# runs under is then reviewable in a diff, and a build works offline -- but a vendored file has two
# ways to become a lie, and neither is visible by reading it:
#
#   * it was hand-edited, so it no longer matches its generator; or
#   * upstream moved, so it no longer matches the world.
#
# The AppArmor profile was first written BY HAND and was missing three deny rules, the ABI
# declaration and the runc/crun signal peers. Every static guard passed, because every static guard
# was written from the same understanding as the file. Regenerating and comparing is the only check
# that can see that class of defect at all: it is the one that consults the authoritative source.
#
# WHY IT IS DERIVED, not two checks. The set of artifacts is discovered by asking each
# `generate-*.sh` what it writes (`--print-target`), so a third generator joins this check by
# existing. A hand-maintained list of artifacts beside a set of generators is the exact shape this
# directory keeps finding as a defect: two lists that must agree, where the one nobody updates is
# the one that silently checks nothing.
#
# WHY IT IS NOT IN scripts/check.sh. It needs the network. The gate must be runnable offline, and a
# check that is skipped when the network is down is worse than one that lives where the network is
# guaranteed. `--self-test` (which is pure) runs in the gate; the fetching half runs in CI.
#
# NON-DESTRUCTIVE. The artifact is snapshotted, regenerated in place, compared, and the snapshot is
# put back -- byte for byte, including uncommitted edits. Nothing here consults git, which is
# deliberate: the first version of this check compared against `git diff` and PASSED for a profile
# I had gutted, because regeneration had already overwritten my edit before the diff was taken. The
# question worth asking is "is the file on disk what the generator makes", and that needs no git.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"

# ---------------------------------------------------------------------------------- the decision
# Pure, so the three-way outcome is testable without a network or a generator. `unknown` is a real
# answer and is never spelled as one of the other two: an artifact recording no upstream digest
# cannot be attributed, and saying "hand-edited" there would send a reader to the wrong diff.
drift_kind() { # drift_kind <recorded-digest> <fetched-digest> -> upstream|local|unknown
    if [ -z "$1" ] || [ -z "$2" ]; then printf 'unknown'
    elif [ "$1" != "$2" ]; then printf 'upstream'
    else printf 'local'; fi
}

# One extraction rule for every artifact, whatever its format: the generators agree on a token
# rather than on a file layout. In the AppArmor profile it is a comment line; in the seccomp
# profile it is inside the JSON `comment` string, because adding an unknown top-level key to a
# security profile is not something to discover the parser's opinion about at run time.
#
# NEVER FAILS, and that is load-bearing rather than tidiness. `grep` exits 1 when it matches
# nothing, `pipefail` carries that out of the pipeline, and the call sites below are BARE
# assignments from a command substitution -- which is a simple command in no conditional context,
# so `errexit` fires and the script dies. It did: an artifact with no recorded digest made this
# whole check exit 1 having printed NOTHING, which reads as a failed check with no reason given,
# for the one input the `unknown` arm exists to handle. Same shape as `v6_path_state` in
# egress-lib.sh earlier on this branch.
#
# The self-test above could not catch it: it calls this inside an ARGUMENT, where a non-zero
# status does not trip errexit. So there is a second self-test below that calls it exactly as the
# real code does -- a bare assignment, under the same shell options.
recorded_digest() { # recorded_digest <artifact> -> the 64-hex digest, or empty
    local m=""
    m="$(grep -oE 'upstream-sha256: [0-9a-f]{64}' "$1" 2>/dev/null | head -1)" || m=""
    printf '%s' "${m##*: }"
    return 0
}

if [ "${1:-}" = --self-test ]; then
    fails=0
    check() { # check <label> <got> <want>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else printf '  \033[31mFAIL\033[0m %s (got %s, wanted %s)\n' "$1" "$2" "$3"; fails=$((fails+1)); fi
    }
    A=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    B=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
    check "differing digests are upstream drift"      "$(drift_kind "$A" "$B")" upstream
    check "matching digests mean the file was edited" "$(drift_kind "$A" "$A")" local
    check "no recorded digest is not attributable"    "$(drift_kind ""   "$B")" unknown
    check "no fetched digest is not attributable"     "$(drift_kind "$A" ""  )" unknown
    check "neither digest is not attributable"        "$(drift_kind ""   ""  )" unknown

    # The extractor, against each format it really has to read -- not against a string written here
    # to match it. A rule shared by two file formats is exactly where a regex quietly reads one.
    t="$(mktemp -d)"; trap 'rm -rf "$t"' EXIT
    printf '# Source: https://x/y\n# upstream-sha256: %s\nprofile "p" flags=() {\n}\n' "$A" > "$t/aa"
    check "reads the digest from a profile comment line" "$(recorded_digest "$t/aa")" "$A"
    printf '{"comment": "GENERATED FILE. Source: https://x/y upstream-sha256: %s", "syscalls": []}\n' "$B" > "$t/sc"
    check "reads the digest from inside a JSON string"   "$(recorded_digest "$t/sc")" "$B"
    printf 'no digest here at all\n' > "$t/none"
    check "an artifact with no digest reads as empty"    "$(recorded_digest "$t/none")" ""

    # CALLED THE WAY THE REAL CODE CALLS IT. Every check above passes the result as an ARGUMENT,
    # where a non-zero exit does not trip errexit -- so all of them stayed green while the real
    # path, a bare assignment, aborted the whole script silently on the no-digest input. A test
    # that exercises a function in a gentler context than production is not a test of production.
    #
    # IN A CHILD PROCESS, and that is not incidental. The obvious spelling --
    # `( set -e; v="$(recorded_digest ...)" ) && r=ok || r=aborted` -- CANNOT FAIL: bash suppresses
    # errexit for any command in a `&&`/`||` list, and the suppression propagates INTO the subshell,
    # so the construct disables the very behaviour it is written to observe. That version passed
    # against the broken implementation, which is how this was found. A separate process has its own
    # errexit state and does not inherit the parent's conditional context. `declare -f` hands it the
    # REAL function rather than a copy that could drift from it.
    fn="$(declare -f recorded_digest)"
    errexit_case() { # errexit_case <file> <expected value> -> ok | ABORTED | WRONG
        local out
        if out="$(bash -c "set -euo pipefail; $fn; v=\"\$(recorded_digest \"\$1\")\"; printf '%s' \"\$v\"" \
                  _ "$1" 2>/dev/null)"; then
            [ "$out" = "$2" ] && printf 'ok' || printf 'WRONG(%s)' "$out"
        else
            printf 'ABORTED'
        fi
    }
    check "a bare assignment under errexit survives no match" "$(errexit_case "$t/none" "")" ok
    check "a bare assignment under errexit still returns the digest" "$(errexit_case "$t/aa" "$A")" ok
    # A truncated digest is not a digest. Without the length anchor this returned a prefix, and a
    # prefix compares unequal to the real one -- reporting UPSTREAM DRIFT for a corrupt local file.
    printf '# upstream-sha256: abc123\n' > "$t/short"
    check "a truncated digest is not accepted"           "$(recorded_digest "$t/short")" ""

    if [ "$fails" -eq 0 ]; then printf '\033[32mcheck-drift self-test passed\033[0m\n'; exit 0; fi
    printf '\033[31mcheck-drift self-test: %s failed\033[0m\n' "$fails"; exit 1
fi

# ------------------------------------------------------------------------------------- the check
# The artifact is regenerated IN PLACE and put back afterwards, so an interrupt between those two
# moments would leave a policy file rewritten by a check that is supposed to only look. Ctrl-C
# during a curl is not a hypothetical -- these fetch the network.
RESTORE_FROM=""; RESTORE_TO=""
restore_now() {
    [ -n "$RESTORE_TO" ] && [ -f "$RESTORE_FROM" ] && cp "$RESTORE_FROM" "$RESTORE_TO"
    RESTORE_FROM=""; RESTORE_TO=""
    return 0
}
trap 'restore_now' EXIT INT TERM

status=0
checked=0
for gen in "$here"/generate-*.sh; do
    [ -e "$gen" ] || continue
    name="$(basename "$gen")"
    if [ ! -x "$gen" ]; then
        printf '  \033[31mFAIL\033[0m %s is not executable, so nothing can regenerate its artifact\n' "$name"
        status=1; continue
    fi
    # ASKED STATICALLY FIRST. A generator that does not implement the flag does not refuse it -- it
    # ignores an unknown argument, fetches, REWRITES ITS ARTIFACT, and hands back its own progress
    # output as the "target". A probe must not have the side effect of the thing it probes, and
    # discovering that only from the mangled message afterwards is too late.
    if ! grep -q -- '--print-target' "$gen"; then
        printf '  \033[31mFAIL\033[0m %s does not support --print-target, so its artifact cannot be discovered\n' "$name"
        printf '         Add it, or this generator sits outside the drift check while looking inside it.\n'
        status=1; continue
    fi
    target=""; rc=0
    target="$("$gen" --print-target 2>/dev/null)" || rc=$?
    # ONE LINE, AND A PATH IN THIS DIRECTORY. The value is used as a path, so a generator whose flag
    # is implemented wrongly must be reported as that rather than as a missing artifact with a
    # multi-line name -- which is what the mangled message above actually said.
    # `$'\n'`, never `"$(printf '\n')"`: command substitution strips trailing newlines, so the
    # latter is the EMPTY string and the pattern collapses to `**`, rejecting every target and
    # failing the healthy tree. Same collapse as the BSD-sed escaping bug in check-config.sh.
    case "$target" in
        *$'\n'*)   target="" ;;
        "$here"/*) : ;;
        *)         target="" ;;
    esac
    if [ "$rc" -ne 0 ] || [ -z "$target" ]; then
        printf '  \033[31mFAIL\033[0m %s implements --print-target but did not print one path under %s\n' "$name" "$here"
        status=1; continue
    fi
    if [ ! -f "$target" ]; then
        printf '  \033[31mFAIL\033[0m %s says it writes %s, which does not exist\n' "$name" "$target"
        status=1; continue
    fi

    checked=$((checked+1))
    snapshot="$(mktemp)"
    cp "$target" "$snapshot"
    RESTORE_FROM="$snapshot"; RESTORE_TO="$target"
    recorded="$(recorded_digest "$target")"

    rc=0
    "$gen" >/dev/null 2>"$snapshot.err" || rc=$?
    if [ "$rc" -ne 0 ]; then
        # Restore first: a generator that died part-way must not leave a half-written policy behind.
        cp "$snapshot" "$target"
        printf '  \033[31mFAIL\033[0m %s could not run (exit %s) — drift is UNCHECKED, not absent\n' "$name" "$rc"
        sed 's/^/         /' "$snapshot.err" | head -5
        status=1; RESTORE_FROM=""; RESTORE_TO=""; rm -f "$snapshot" "$snapshot.err"; continue
    fi

    fetched="$(recorded_digest "$target")"
    if cmp -s "$snapshot" "$target"; then
        printf '  \033[32mok\033[0m   %s is exactly what %s produces (upstream %s)\n' \
            "$(basename "$target")" "$name" "${fetched:0:12}"
    else
        status=1
        case "$(drift_kind "$recorded" "$fetched")" in
            upstream)
                printf '  \033[31mFAIL\033[0m %s: upstream moved (%s -> %s)\n' \
                    "$(basename "$target")" "${recorded:0:12}" "${fetched:0:12}"
                printf '         Run ./.container/%s, REVIEW the diff, and commit it. This is a\n' "$name"
                printf '         security policy: a change upstream made is a change to look at.\n' ;;
            local)
                printf '  \033[31mFAIL\033[0m %s is not what %s produces from the upstream it records\n' \
                    "$(basename "$target")" "$name"
                printf '         It was hand-edited, or the generator changed and it was not regenerated.\n'
                printf '         Never hand-edit it: that is how the AppArmor profile lost three deny rules.\n' ;;
            unknown)
                printf '  \033[31mFAIL\033[0m %s differs from what %s produces, and records no upstream\n' \
                    "$(basename "$target")" "$name"
                printf '         digest — so this cannot be attributed to upstream or to a local edit.\n'
                printf '         Make the generator record `upstream-sha256: <hex>` in what it writes.\n' ;;
        esac
        diff -u "$snapshot" "$target" | sed 's/^/         /' | head -60
    fi
    cp "$snapshot" "$target"
    RESTORE_FROM=""; RESTORE_TO=""
    rm -f "$snapshot" "$snapshot.err"
done

# A run that checked nothing must not report success. `generate-*.sh` matching no files, or every
# generator failing its preconditions, is the state in which this script is most likely to be
# believed and least entitled to be.
if [ "$checked" -eq 0 ]; then
    printf '  \033[31mFAIL\033[0m no generator was checked — this run establishes nothing about drift\n'
    status=1
fi

if [ "$status" -eq 0 ]; then
    printf '\033[32m%s vendored artifact(s) match their generators and their recorded upstream\033[0m\n' "$checked"
fi
exit "$status"
