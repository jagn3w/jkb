#!/usr/bin/env bash
# What "egress is bounded" means, defined ONCE and sourced by everything that needs to know.
# Design: openspec/changes/jkb-egress-liveness (D51).
#
# WHY A LIBRARY. Two scripts ask this question — init-firewall.sh, which raises the rules and
# records why, and egress-status.sh, which reads the live chains for the boot decision. Under D50
# the rule was spelled twice inside one script and that is exactly how the success path came to
# report success on unfiltered IPv6; spelling it twice across two scripts would be the same defect
# with a wider gap. It is sourced by path from the script's own directory, which resolves to
# .container/ in the repo and /usr/local/bin/ in the image, so there is no second install location.
#
# THE SPLIT THAT MAKES IT TESTABLE. Everything that DECIDES is pure and takes its inputs as
# arguments; everything that MEASURES runs a command or reads a file and is injectable. So the
# whole decision surface is exercisable on a laptop with no iptables, no /proc and no root — which
# is the only reason D50's writer went untested for a whole round.

# --- the decision (pure) ------------------------------------------------------------------------

# THE VOCABULARY, declared beside the function that produces it. A state a reader has no arm for
# falls into its `*` case and is read as unknown — which under D51.2 is a container that refuses to
# boot, for ever, over a word. Two things hold it: check-config.sh requires entrypoint.sh and
# verify.sh to carry an arm for every state named here, and the self-test below requires
# verdict_state to return nothing outside it. Neither half is decorative — adding a state without
# teaching the readers fails the gate, and inventing one here fails the self-test.
VERDICT_STATES="allowlisted denied unfiltered"

# WHAT THE RAISE ESTABLISHED, from each family's measured state (D50.2/D50.4). Two callers reach
# it, differing only in what "bounded" is called there: a blanket deny is `denied`, a raised
# allowlist is `allowlisted`. That is an argument, not a second rule.
#
# A family counts as closed when it is `denied`, or `absent` (no address and no route — denial by a
# second means). Everything else, INCLUDING a measurement that could not be taken, is open.
verdict_state() { # verdict_state <v4> <v6> <state-if-bounded> -> <state-if-bounded>|unfiltered
    case "$1" in denied|allowlisted) ;; *) printf 'unfiltered'; return ;; esac
    case "$2" in denied|absent) printf '%s' "$3" ;; *) printf 'unfiltered' ;; esac
}

# THE WHOLE ANSWER, from four measurements. Pure, so every combination is a self-test row rather
# than something only a container can reach.
#   v4chain    bounded | open      a terminal REJECT is in the v4 OUTPUT chain
#   v6chain    denied  | open      a REJECT is in the v6 OUTPUT chain
#   v6path     absent  | open      whether v6 has any way out at all (see v6_path_state)
#   allowlist  yes     | no        an allowlist ACCEPT rule is present
probe_state() { # probe_state <v4chain> <v6chain> <v6path> <allowlist> -> allowlisted|denied|unfiltered
    local v4 v6 bounded
    [ "$1" = bounded ] && v4=denied || v4=open
    if   [ "$2" = denied ]; then v6=denied
    elif [ "$3" = absent ]; then v6=absent
    else                         v6=open
    fi
    [ "$4" = yes ] && bounded=allowlisted || bounded=denied
    verdict_state "$v4" "$v6" "$bounded"
}

# --- the measurements (impure, injectable) ------------------------------------------------------

# IS THERE ANY WAY OUT OVER IPv6? (D51.3)
#
# This used to ask whether a non-loopback ADDRESS exists, which is a different question and was
# wrong in both directions. Every container on a default Docker bridge gets a link-local
# fe80::/64 when the host kernel has IPv6 — no global address, no v6 default route, nothing can
# leave — and that read as `open`, so an ordinary container refused to boot citing a bypassable
# allowlist and pushed its operator onto the permanent override. And an UNREADABLE table returned
# `absent`, which the rule reads as provably closed: the house defect, in the direction that
# matters, pinned as correct by a self-test row.
#
# So: a path needs an address that can source traffic off-link (anything but ::1 and fe80::/10) OR
# a default route. A read that FAILED is `open`, distinguished from a table positively read as
# holding nothing usable.
# GREP'S EXIT CODE IS THREE-VALUED AND ONLY TWO OF THEM ARE MEASUREMENTS: 0 found, 1 read it and
# found nothing, >=2 could not read it. The old code hid the third behind `2>/dev/null` and let it
# fall through as "found nothing", which here means "provably closed" — the whole finding.
v6_path_state() { # v6_path_state [if_inet6] [ipv6_route] -> absent|open
    local inet6="${1:-${JKB_INET6_PATH:-/proc/net/if_inet6}}"
    local route="${2:-${JKB_IPV6_ROUTE_PATH:-/proc/net/ipv6_route}}"
    local rc

    # 1. A default route is a way out on its own, whatever the addresses say. Destination ::/0 is
    #    32 zero hex digits then a zero prefix length, in the first two columns.
    grep -qE '^0{32} 00 ' "$route" >/dev/null 2>&1; rc=$?
    case $rc in
        0) printf 'open'; return ;;   # there is a default route
        1) : ;;                       # positively read: there is none
        *) # Could not read it. NOT PRESENT is evidence (no IPv6 stack at all); PRESENT BUT
           # UNREADABLE is not, and "absent" would mean provably closed.
           [ -e "$route" ] && { printf 'open'; return; } ;;
    esac

    # 2. No way out by route. Is there an address that can source off-link traffic — anything that
    #    is neither loopback (::1) nor link-local (fe80::/10)? Column 1 is 32 hex digits.
    grep -qivE '^(0{31}1|fe[89ab])' "$inet6" >/dev/null 2>&1; rc=$?
    case $rc in
        0) printf 'open' ;;                                        # an off-link address exists
        1) printf 'absent' ;;                                      # read it: only ::1 / fe80::, or empty
        *) [ -e "$inet6" ] && printf 'open' || printf 'absent' ;;  # unreadable vs no stack at all
    esac
}

# Is a terminal REJECT in the v4 OUTPUT chain? A failed read is `open` — never `bounded`.
v4_chain_state() { # -> bounded|open
    if command -v iptables >/dev/null 2>&1 \
       && iptables -w 5 -C OUTPUT -j REJECT --reject-with icmp-port-unreachable >/dev/null 2>&1; then
        printf 'bounded'
    else
        printf 'open'
    fi
}

# Is a REJECT in the v6 OUTPUT chain? A failed read is `open`.
v6_chain_state() { # -> denied|open
    if command -v ip6tables >/dev/null 2>&1 \
       && ip6tables -w 5 -C OUTPUT -j REJECT >/dev/null 2>&1; then
        printf 'denied'
    else
        printf 'open'
    fi
}

# Is the allowlist ACCEPT rule present? This is what separates a raised allowlist from a blanket
# deny-all; both are bounded, and both boot, so it decides the WORD rather than the decision.
allowlist_state() { # -> yes|no
    if command -v iptables >/dev/null 2>&1 \
       && iptables -w 5 -C OUTPUT -m set --match-set allowed-new dst -j ACCEPT >/dev/null 2>&1; then
        printf 'yes'
    else
        printf 'no'
    fi
}

# --- self-test ----------------------------------------------------------------------------------
# Sourced by two scripts and by nothing else that could exercise it, so it carries its own. Every
# assertion here is pure or file-injected: no iptables, no root, no /proc.
if [ "${1:-}" = "--self-test" ] && [ "$#" -eq 1 ] && [ "${BASH_SOURCE[0]}" = "$0" ]; then
    fails=0
    eq() { if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
           else fails=$((fails+1)); printf '  \033[31mFAIL\033[0m %s (got %s, wanted %s)\n' "$1" "$2" "$3"; fi; }
    # ...and neither function may invent a state the readers have no arm for. Checked on every row
    # rather than once, because the value on one branch is whatever a caller passed in.
    in_vocab() { # in_vocab <who> <state>
        case " $VERDICT_STATES " in *" $2 "*) ;;
            *) fails=$((fails+1)); printf '  \033[31mFAIL\033[0m %s returned %s, which is not in VERDICT_STATES\n' "$1" "$2" ;;
        esac
    }
    echo "==> egress-lib self-test: what bounded means"

    # THE RULE, as a literal table rather than a re-derivation. Writing the expectation as a second
    # copy of the condition passes for any condition, including the wrong one this replaced.
    while read -r v4 v6 bounded want; do
        [ -n "${v4:-}" ] || continue
        got="$(verdict_state "$v4" "$v6" "$bounded")"
        eq "verdict_state $v4 $v6 -> $want" "$got" "$want"
        in_vocab "verdict_state" "$got"
    done <<'TABLE'
denied      denied denied      denied
denied      absent denied      denied
denied      open   denied      unfiltered
denied      wat    denied      unfiltered
open        denied denied      unfiltered
open        absent denied      unfiltered
open        open   denied      unfiltered
wat         denied denied      unfiltered
allowlisted denied allowlisted allowlisted
allowlisted absent allowlisted allowlisted
allowlisted open   allowlisted unfiltered
allowlisted wat    allowlisted unfiltered
TABLE

    # ...and the whole answer, from the four things the probe measures.
    while read -r v4c v6c v6p al want; do
        [ -n "${v4c:-}" ] || continue
        got="$(probe_state "$v4c" "$v6c" "$v6p" "$al")"
        eq "probe_state $v4c $v6c $v6p allow=$al -> $want" "$got" "$want"
        in_vocab "probe_state" "$got"
    done <<'TABLE'
bounded denied open   yes allowlisted
bounded denied open   no  denied
bounded open   absent yes allowlisted
bounded open   absent no  denied
bounded open   open   yes unfiltered
bounded open   open   no  unfiltered
open    denied open   yes unfiltered
open    denied absent yes unfiltered
open    open   open   no  unfiltered
TABLE

    t="$(mktemp -d)"; trap 'rm -rf "$t"' EXIT
    : > "$t/no-route"
    printf '00000000000000000000000000000001 01 80 10 80       lo\n' > "$t/lo"
    printf '00000000000000000000000000000001 01 80 10 80       lo\nfe80000000000000042aff0fe4c0a010 02 40 20 80     eth0\n' > "$t/lo-linklocal"
    printf '20010db8000000000000000000000001 02 40 00 80     eth0\n' > "$t/global"
    : > "$t/empty"
    # A default route: destination ::/0, prefix length 00.
    printf '00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000001 00000000 00000003     eth0\n' > "$t/default-route"

    eq "loopback only is absent"                 "$(v6_path_state "$t/lo"           "$t/no-route")" "absent"
    eq "loopback + link-local is absent"         "$(v6_path_state "$t/lo-linklocal" "$t/no-route")" "absent"
    eq "a global address is a path"              "$(v6_path_state "$t/global"       "$t/no-route")" "open"
    eq "an empty table is absent"                "$(v6_path_state "$t/empty"        "$t/no-route")" "absent"
    eq "no table and no route is absent"         "$(v6_path_state "$t/nope"         "$t/nope")" "absent"
    # A default route is a way out whatever the addresses say.
    eq "a default route is a path"               "$(v6_path_state "$t/lo"           "$t/default-route")" "open"

    # THE DIRECTION THE OLD CODE GOT WRONG. A file that EXISTS and cannot be read has measured
    # nothing, and nothing is `open` — never `absent`, which the rule reads as provably closed.
    # Root can read anything, so the row would silently assert something else as root.
    if [ "$(id -u)" != 0 ]; then
        printf '00000000000000000000000000000001 01 80 10 80       lo\n' > "$t/unreadable"; chmod 000 "$t/unreadable"
        printf 'x\n' > "$t/unreadable-route"; chmod 000 "$t/unreadable-route"
        eq "an unreadable address table is open"  "$(v6_path_state "$t/unreadable" "$t/no-route")"  "open"
        eq "an unreadable route table is open"    "$(v6_path_state "$t/lo"         "$t/unreadable-route")" "open"
    else
        printf '  \033[33mskip\033[0m the unreadable-file rows (running as root, which can read them)\n'
    fi

    echo
    [ "$fails" -eq 0 ] || { printf '\033[31m%d failed\033[0m\n' "$fails"; exit 1; }
    printf '\033[32megress-lib self-test passed\033[0m\n'
    exit 0
fi
