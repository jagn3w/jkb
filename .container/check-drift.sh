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

    # ---------------------------------------------------------------- the loop, driven for real
    # THE `cmp` BELOW IS THE ENTIRE CHECK, and until now nothing drove it. check-config.sh covers
    # the preconditions (executable, --print-target, a recorded digest) and never that a DIFFERING
    # artifact is detected -- so inverting the `if`, or rewriting it as `cmp "$snapshot" "$snapshot"`,
    # would have CI printing "2 vendored artifact(s) match their generators" for a gutted profile,
    # permanently. This file's own header records that cost as already paid once: the first version
    # of this check compared against `git diff` and passed for a profile I had gutted. verify.sh has
    # mutate-verify.sh and check-config.sh has mutate-config.sh; the only check that can see a bad
    # transcription had neither.
    #
    # Driven against a FIXTURE generator in a scratch directory -- no network, no real policy -- by
    # re-invoking this script with $here pointed at it.
    d="$t/fix"; mkdir -p "$d"
    cat > "$d/generate-fixture.sh" <<'GEN'
#!/usr/bin/env bash
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$here/fixture-artifact"
url="https://fixture.invalid/thing"
if [ "${1:-}" = --print-target ]; then printf '%s\n' "$out"; exit 0; fi
# A GENERATOR THAT DIES PART-WAY, which is what a network failure looks like: it has already
# truncated the artifact when it gives up. The check must report UNCHECKED *and* put the file back.
if [ -n "${JKB_FIXTURE_FAIL:-}" ]; then
    printf 'half a policy\n' > "$out"
    printf 'fixture: could not fetch upstream\n' >&2
    exit 3
fi
{ printf '# GENERATED FILE -- DO NOT EDIT.\n'
  printf '# Source: %s\n' "$url"
  printf '# upstream-sha256: %s\n' "${JKB_FIXTURE_DIGEST:-1111111111111111111111111111111111111111111111111111111111111111}"
  printf 'body %s\n' "${JKB_FIXTURE_BODY:-original}"; } > "$out"
GEN
    chmod +x "$d/generate-fixture.sh"
    ( cd "$d" && ./generate-fixture.sh >/dev/null )   # lay down the in-sync artifact

    # A SECOND GENERATOR, so that "the loop went on to the next one" is observable at all. With one
    # fixture, every row below passes whether the loop continues or aborts — which is how the
    # `diff -u … | head -60` line sat in the DIFFER arm aborting the whole run on the first drifting
    # artifact: `diff` exits 1 on every difference, `head` exits early, and under `set -euo
    # pipefail` the script ended there, skipping every later generator AND the restore three lines
    # down. Measured: reverting that line leaves this self-test fully green with one fixture. Named
    # `generate-second.sh` because the glob is sorted and `f` sorts before `s`, so the drifting one
    # is processed first — the ordering the failure needs.
    cat > "$d/generate-second.sh" <<'GEN2'
#!/usr/bin/env bash
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$here/second-artifact"
if [ "${1:-}" = --print-target ]; then printf '%s\n' "$out"; exit 0; fi
{ printf '# GENERATED FILE -- DO NOT EDIT.\n'
  printf '# Source: %s\n' "https://fixture.invalid/second"
  printf '# upstream-sha256: %s\n' 3333333333333333333333333333333333333333333333333333333333333333
  printf 'second body\n'; } > "$out"
GEN2
    chmod +x "$d/generate-second.sh"
    ( cd "$d" && ./generate-second.sh >/dev/null )

    # THE ARTIFACT MUST BE BYTE-IDENTICAL AFTERWARDS, asserted on every drive that has one.
    #
    # This check REGENERATES IN PLACE and puts the file back, so its non-destructiveness is a
    # property of two lines -- the restore `cp` at the end of the loop and the copy inside
    # `restore_now` -- and nothing asserted it. Delete both and every row here still passed while a
    # real run reported drift AND overwrote the edit it had just detected: run it twice and the
    # second run prints "1 vendored artifact(s) match their generators" and exits 0. That is
    # verbatim the failure this file's header records as already paid once, and the header is the
    # only place it was written down. A harness that drives a check without asserting what the check
    # does to the tree is watching half of it.
    drive() { # drive <label> <expect ok|substring> [env assignments...]
        local label="$1" want="$2"; shift 2
        local out rc=0 before="" verdict=""
        [ -f "$d/fixture-artifact" ] && { before="$(mktemp)"; cp "$d/fixture-artifact" "$before"; }
        out="$(env "$@" "$0" --drift-dir "$d" 2>&1)" || rc=$?
        if [ "$want" = ok ]; then
            if [ "$rc" -eq 0 ]; then verdict=ok
            else verdict="reported drift: $out"; fi
        elif [ "$rc" -eq 0 ]; then
            verdict="reported NO drift, so the comparison cannot fire"
        elif grep -qF -e "$want" <<<"$out"; then
            verdict=ok
        else
            verdict="wrong reason: $out"
        fi
        # Only where there was an artifact to preserve; two rows below deliberately move it away.
        if [ -n "$before" ] && [ "$verdict" = ok ] && ! cmp -s "$before" "$d/fixture-artifact"; then
            verdict="the check MODIFIED the artifact it was only supposed to look at"
        fi
        [ -n "$before" ] && rm -f "$before"
        if [ "$verdict" = ok ]; then printf '  \033[32mok\033[0m   %s\n' "$label"
        else printf '  \033[31mFAIL\033[0m %s — %s\n' "$label" "$verdict"; fails=$((fails+1)); fi
    }

    # THE NEGATIVE CONTROL FIRST. An in-sync fixture must report ok, or every row below is CAUGHT by
    # something that is present when nothing is wrong.
    drive "an in-sync artifact reports no drift (the control)" ok
    drive "a hand-edited artifact is caught" "hand-edited" JKB_FIXTURE_BODY=different
    # ...AND THE LOOP CARRIES ON. The row above only needs the word "hand-edited", which is printed
    # BEFORE the diff — so it passes just as happily when the script dies immediately afterwards.
    # This one names the second artifact, which is only reached if the first one's arm returned.
    drive "and a later generator is still checked after an earlier one drifts" \
          "second-artifact" JKB_FIXTURE_BODY=different
    drive "a moved upstream is caught and named as such" "upstream moved" JKB_FIXTURE_DIGEST=2222222222222222222222222222222222222222222222222222222222222222
    # A GENERATOR THAT CANNOT RUN, which is the network-failure path and the one arm whose whole
    # value is that it does NOT report success. Deleting its `status=1` left every row here green
    # while a real run printed the red "drift is UNCHECKED, not absent" line AND THEN the green
    # summary, exiting 0 -- a CI pass asserting an artifact matches a generator that never ran. The
    # fixture truncates its artifact before giving up, so the `cmp` in `drive` is also what holds
    # the loop's "restore first: a generator that died part-way must not leave a half-written policy
    # behind" to its word.
    drive "a generator that dies part-way reports UNCHECKED, not absent" "drift is UNCHECKED" JKB_FIXTURE_FAIL=1
    chmod -x "$d/generate-fixture.sh"
    drive "a non-executable generator is caught" "is not executable"
    chmod +x "$d/generate-fixture.sh"
    mv "$d/fixture-artifact" "$d/gone"
    drive "a missing artifact is caught" "which does not exist"
    mv "$d/gone" "$d/fixture-artifact"
    # BOTH, now that there are two. Moving only the first left the second in place, so "no
    # generator at all" was driven against a directory that still had one — the row reported no
    # drift and failed on its own premise, which is the harness working.
    mv "$d/generate-fixture.sh" "$d/nope"; mv "$d/generate-second.sh" "$d/nope2"
    drive "no generator at all establishes nothing" "no generator was checked"
    mv "$d/nope" "$d/generate-fixture.sh"; mv "$d/nope2" "$d/generate-second.sh"

    if [ "$fails" -eq 0 ]; then printf '\033[32mcheck-drift self-test passed\033[0m\n'; exit 0; fi
    printf '\033[31mcheck-drift self-test: %s failed\033[0m\n' "$fails"; exit 1
fi

# --drift-dir <dir> points the loop below at a directory other than this script's own, which is how
# --self-test drives the real comparison against fixture generators. Deliberately not documented as
# a user-facing flag: it exists so the check can be watched failing.
if [ "${1:-}" = --drift-dir ]; then here="$2"; shift 2; fi

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
        # `sed -n 1,5p` rather than `head -5`, under `set -euo pipefail`: head exits at five
        # lines and the producer dies on the tail, so a generator whose stderr runs past 4 KB
        # would abort this loop while REPORTING that generator's failure.
        sed 's/^/         /' "$snapshot.err" | sed -n 1,5p
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
        # TWO failures in one line, both of which abort the loop under `set -euo pipefail`,
        # and this is the arm reached only when the files DIFFER. `diff` exits 1 on every
        # difference — so the first drifting artifact ended the run, skipping every later
        # generator and the restore three lines down (the EXIT trap covers the file, not the
        # coverage). And `head -60` exits early, killing the producer on a long diff. Measured:
        # the line after this one does not run, and the script exits 1.
        { diff -u "$snapshot" "$target" || true; } | sed 's/^/         /' | sed -n 1,60p
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
