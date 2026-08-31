#!/usr/bin/env bash
# What the kernel says about egress RIGHT NOW. Design: openspec/changes/jkb-egress-liveness (D51).
#
#   sudo -n /usr/local/bin/egress-status.sh
#   state=allowlisted
#   v4=bounded
#   v6=denied
#
# WHY THIS EXISTS. D50 made the raise record what it established, in /run/jkb-egress-verdict, and
# the entrypoint booted on that record. But a verdict is an EVENT — "at some moment, a raise
# established X" — and every reader asks a PRESENT-TENSE question: is egress bounded now? Nothing
# said when an old verdict stops counting, and the file outlives what it describes: `docker stop`
# destroys every iptables rule while the file survives in the writable layer. So a raise that died
# before recording anything left the previous start's `allowlisted` standing, and the entrypoint
# read it, printed nothing, and exec'd the agent onto an empty OUTPUT chain — unrestricted egress,
# silently, on exactly the `docker start` path the entrypoint was added to cover.
#
# Making the record trustworthy needs three mechanisms (a marker for interrupted raises, a token
# naming the network namespace, and a rule for when an old record stops counting) to reconstruct
# something the kernel will simply tell you. So: ask the kernel. There is no stale answer to
# detect, because there is no answer being stored — and this also covers the case none of those
# mechanisms could, where NO raise runs at all (`--entrypoint bash`, the escape D50's own refusal
# message recommends), and the case where two concurrent raises fought and the file records
# whichever finished last rather than what is in the chain.
#
# WHAT IT IS NOT FOR. The kernel cannot say WHY egress is not allowlisted — that DNS failed, or the
# snapshot was truncated. That is what /run/jkb-egress-verdict still carries. State from here,
# explanation from there.
#
# WHY IT IS ROOT-OWNED, ARGUMENT-LESS AND READ-ONLY. It is granted by sudoers, and D49's rule is
# that a command naming no argument accepts every argument — so it takes none and refuses any. It
# only inspects chains and prints: it must never install, flush or modify a rule, so that a grant
# to run it is not a grant to change the boundary.
set -uo pipefail

# shellcheck source=egress-lib.sh
. "$(dirname "$0")/egress-lib.sh"

# Reports what was MEASURED alongside the state it implies. `v6=absent` and `v6=denied` both make a
# container bounded, and an operator needs to be able to tell them apart — one is a rule, the other
# is a network with nowhere to go. Defined above the self-test because that is what exercises it.
report() { # report <v4chain> <v6chain> <v6path> <allowlist>
    local v6
    if   [ "$2" = denied ]; then v6=denied
    elif [ "$3" = absent ]; then v6=absent
    else                         v6=open
    fi
    printf 'state=%s\n' "$(probe_state "$1" "$2" "$3" "$4")"
    printf 'v4=%s\n'    "$1"
    printf 'v6=%s\n'    "$v6"
}

if [ "${1:-}" = "--self-test" ] && [ "$#" -eq 1 ]; then
    fails=0
    eq() { if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
           else fails=$((fails+1)); printf '  \033[31mFAIL\033[0m %s (got %s, wanted %s)\n' "$1" "$2" "$3"; fi; }
    echo "==> egress-status self-test: the report it prints"

    # The composition and the measurements are covered by egress-lib.sh --self-test. What is left
    # here is the REPORT: that a reader can parse it, and that it refuses arguments.
    out="$(report bounded denied absent yes)"
    eq "state is the first line"      "$(printf '%s\n' "$out" | sed -n 's/^state=//p')" "allowlisted"
    eq "v4 is reported"               "$(printf '%s\n' "$out" | sed -n 's/^v4=//p')"    "bounded"
    eq "v6 is reported as measured"   "$(printf '%s\n' "$out" | sed -n 's/^v6=//p')"    "denied"
    out="$(report open open open no)"
    eq "an unbounded chain is unfiltered" "$(printf '%s\n' "$out" | sed -n 's/^state=//p')" "unfiltered"
    # v6 is reported as what was MEASURED, not as the word the rule collapsed it to: an operator
    # needs to know whether v6 was denied or merely had nowhere to go.
    out="$(report bounded open absent no)"
    eq "no v6 path is reported as absent" "$(printf '%s\n' "$out" | sed -n 's/^v6=//p')" "absent"
    eq "...and the state is denied"       "$(printf '%s\n' "$out" | sed -n 's/^state=//p')" "denied"

    ( "$0" some-argument >/dev/null 2>&1 ); eq "an argument is refused" "$?" "2"

    echo
    [ "$fails" -eq 0 ] || { printf '\033[31m%d failed\033[0m\n' "$fails"; exit 1; }
    printf '\033[32megress-status self-test passed\033[0m\n'
    exit 0
fi

# NO ARGUMENTS, and it says so rather than ignoring them. The sudoers entry pins this to none; a
# caller passing one has misunderstood something and should be told, not silently obeyed.
if [ "$#" -ne 0 ]; then
    printf 'egress-status.sh takes no arguments (got %s). It reads the live chains and prints them.\n' "$#" >&2
    exit 2
fi

report "$(v4_chain_state)" "$(v6_chain_state)" "$(v6_path_state)" "$(allowlist_state)"
