#!/usr/bin/env bash
# The container's own first act: raise the egress firewall, then become the command — but only if
# the KERNEL says egress is bounded. Design: openspec/changes/jkb-egress-liveness (D51), which
# supersedes the decision half of openspec/changes/jkb-egress-verdict (D50).
#
# WHY THIS IS IN THE IMAGE AND NOT IN A CALLER. Under Dev Containers the raise was
# `postStartCommand`'s first act, so it happened on every start. Replacing that lifecycle with
# `run.sh` moved it into ONE caller — and `docker start jkb-dev`, Docker Desktop's start button,
# or a daemon restart bring the container back without it. iptables rules live in the network
# namespace and do not survive a stop, so those routes gave an unattended agent unrestricted
# egress, with nothing to notice: attaching runs no check. A boundary that depends on which
# caller you used is not one. As the image's ENTRYPOINT it runs on `docker run` AND on
# `docker start`, whoever issues them.
#
# WHY IT ASKS THE KERNEL RATHER THAN READING A RECORD. Two earlier versions read a file. The first
# treated the presence of a failure marker as proof of denial — but that marker was written BEFORE
# any iptables call, so it meant "fail_closed ran", true both when a deny-all went in and when
# installing it failed. The second read a verdict recording what the raise ESTABLISHED, which is a
# better fact and still the wrong KIND of fact: a verdict is an event ("at some moment a raise
# established X") and this is a present-tense question ("is egress bounded now?"). Nothing said
# when an old verdict stopped counting, and `docker stop` destroys every iptables rule while the
# file survives in the writable layer — so a raise that died before recording left the PREVIOUS
# start's `allowlisted` standing, and this booted the agent onto an empty OUTPUT chain in silence.
# The kernel cannot be stale about its own chains.
#
# The record still supplies the REASON, which the kernel cannot: that DNS failed, that the
# allowlist snapshot was truncated. State from the probe, explanation from the record.
set -uo pipefail

VERDICT="${JKB_EGRESS_VERDICT:-/run/jkb-egress-verdict}"

# THE ONE DELIBERATE ESCAPE (D50.6), and it is recorded rather than silent. Refusing to boot on an
# unfiltered network is right for an unattended agent, and it means a host with real IPv6 and no
# ip6tables cannot start the container at all — with no way forward but editing a root-owned script
# inside an image. A posture too tight to work is one that gets switched off, so there is a way
# through; and an override nobody can see is indistinguishable from a rule that does not exist, so
# verify.sh reads this variable directly and reports a FAILURE for as long as it is set. Declared in
# container.json's containerEnv, which means it is fixed at create: turning it on is a deliberate,
# reviewable edit followed by a recreate, not something a session can do to itself.
#
# It buys a container you can ATTACH TO AND DIAGNOSE. It does not make that container a place to run
# an agent, which is why run.sh's --open still refuses (D51.7).
ACCEPT="${JKB_EGRESS_ACCEPT_UNFILTERED:-0}"

verdict_field() { # verdict_field <key>  -> value, empty if unreadable
    [ -r "$VERDICT" ] || return 0
    sed -n "s/^$1=//p" "$VERDICT" 2>/dev/null | head -1
}

# --- self-test ------------------------------------------------------------------------------
# The boot decision runs before anything else in the container, and getting it wrong yields either
# a container that will not start or one running unprotected. Every state is exercised here against
# a stubbed raise and a stubbed probe. Only when it is the SOLE argument; run.sh always passes
# `sleep infinity`.
if [ "$#" -eq 1 ] && [ "$1" = "--self-test" ]; then
    fails=0
    eq() { # eq <label> <got> <want>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else fails=$((fails+1)); printf '  \033[31mFAIL\033[0m %s (got %s, wanted %s)\n' "$1" "$2" "$3"; fi
    }
    echo "==> entrypoint self-test: booting on what the kernel reports"
    t="$(mktemp -d)"; trap 'rm -rf "$t"' EXIT
    mkdir -p "$t/bin"

    # A stub `sudo` standing in for both root commands: the raise writes the REASON, the probe
    # prints the STATE. Empty state means the probe produced nothing — the case where it could not
    # run at all, which must never read as bounded.
    # The third argument is what the RECORD claims, which may disagree with the kernel — that
    # disagreement is the whole point of D51 and must be written by the stubbed raise itself, since
    # run_ep clears the file first (seeding it outside would be deleted before the entrypoint ran,
    # and the assertion would pass having tested nothing).
    stub() { # stub <probe-state-or-empty> <reason> [record-state]
        cat > "$t/bin/sudo" <<STUB
#!/usr/bin/env bash
case "\$*" in
    *egress-status*) [ -n "$1" ] && printf 'state=%s\nv4=x\nv6=x\n' "$1"; exit 0 ;;
    *init-firewall*) { [ -n "${3:-}" ] && printf 'state=%s\n' "${3:-}"
                       printf 'reason=%s\n' "$2"; } > "$JKB_EGRESS_VERDICT"; exit 0 ;;
esac
exit 0
STUB
        chmod +x "$t/bin/sudo"
    }
    run_ep() { # run_ep [accept]
        rm -f "$t/verdict"
        JKB_EGRESS_VERDICT="$t/verdict" JKB_EGRESS_ACCEPT_UNFILTERED="${1:-0}" \
            PATH="$t/bin:$PATH" bash "$0" echo BECAME-THE-COMMAND 2>"$t/err"
    }
    export JKB_EGRESS_VERDICT="$t/verdict"

    stub allowlisted "allowlist raised"
    eq "an allowlisted kernel execs the command"    "$(run_ep)" "BECAME-THE-COMMAND"
    eq "...and says nothing on stderr"              "$(wc -c <"$t/err" | tr -d ' ')" "0"

    # A blanket deny is SAFE but not working: no allowlist, so nothing but DNS and loopback. Staying
    # up is the point — this is the state you need to attach to in order to repair it.
    stub denied "DNS could not resolve any allowlisted domain"
    eq "a denied kernel still execs"                "$(run_ep)" "BECAME-THE-COMMAND"
    eq "...and says egress is denied"               "$(grep -c 'egress is DENIED' "$t/err")" "1"
    # ...and the REASON comes from the record, which is the half the kernel cannot supply.
    eq "...and prints the recorded reason"          "$(grep -c 'DNS could not resolve' "$t/err")" "1"

    stub unfiltered "IPv4 allowlisted but IPv6 unfiltered"
    eq "an unfiltered kernel refuses"               "$(run_ep)" ""
    eq "...with a non-zero exit"                    "$(run_ep >/dev/null; echo $?)" "1"
    eq "...and says egress is NOT bounded"          "$(grep -c 'egress is NOT bounded' "$t/err")" "1"

    # THE CASE THE RECORD-READING VERSION GOT WRONG, and the reason D51 exists. A record left by an
    # earlier start says `allowlisted`; the kernel says the chain is empty. The kernel wins.
    stub unfiltered "left over from a previous start" allowlisted
    eq "a stale allowlisted record does not boot"   "$(run_ep >/dev/null; echo $?)" "1"
    # ...and the converse, so the row above cannot pass just because everything refuses: a record
    # claiming `unfiltered` does not stop a container the kernel reports as bounded.
    stub allowlisted "left over from a previous start" unfiltered
    eq "a stale unfiltered record does not block"   "$(run_ep)" "BECAME-THE-COMMAND"

    # The probe produced nothing at all: it could not run, or sudo is broken. Unknown, never
    # bounded — the failure this whole design exists to avoid spelling as safe.
    stub "" "irrelevant"
    eq "no answer from the probe refuses"           "$(run_ep >/dev/null; echo $?)" "1"
    eq "...and says the state is unknown"           "$(grep -c 'could not establish' "$t/err")" "1"

    # A word nobody taught this about is not a pass.
    stub wat "irrelevant"
    eq "an unrecognised state refuses"              "$(run_ep >/dev/null; echo $?)" "1"

    # The override: boots, and is loud about it every single time.
    stub unfiltered "no ip6tables"
    eq "the override boots an unfiltered container" "$(run_ep 1)" "BECAME-THE-COMMAND"
    eq "...and says it was accepted"                "$(grep -c 'ACCEPTED BY CONFIGURATION' "$t/err")" "1"
    # It is not a blanket skip: an unknown state is not an unfiltered one, and only `unfiltered`
    # was accepted.
    stub "" "irrelevant"
    eq "...but it does not cover an unknown state"  "$(run_ep 1 >/dev/null; echo $?)" "1"

    echo
    [ "$fails" -eq 0 ] || { printf '\033[31m%d failed\033[0m\n' "$fails"; exit 1; }
    printf '\033[32mentrypoint self-test passed\033[0m\n'
    exit 0
fi
# --------------------------------------------------------------------------------------------

# No arguments to either: sudoers grants `vscode` exactly these two paths with none, and both
# scripts refuse any. The allowlist the raise reads is the root-owned snapshot, never a path a
# caller names. The raise's exit code is deliberately NOT consulted — what decides is what the
# kernel holds afterwards.
sudo -n /usr/local/bin/init-firewall.sh || true

probe="$(sudo -n /usr/local/bin/egress-status.sh 2>/dev/null)" || probe=""
state="$(printf '%s\n' "$probe" | sed -n 's/^state=//p' | head -1)"
reason="$(verdict_field reason)"
[ -n "$reason" ] || reason="(the raise left no reason)"

case "$state" in
    allowlisted)
        ;;
    denied)
        # Safe but not working: no allowlist, so nothing but DNS and loopback. Staying up is the
        # point — this is the state you need to be able to attach to in order to repair it.
        printf 'entrypoint: the firewall failed closed — egress is DENIED, and there is no\n' >&2
        printf 'entrypoint: allowlist, so nothing will reach the network. Reason:\n  %s\n' "$reason" >&2
        ;;
    unfiltered)
        if [ "$ACCEPT" = 1 ]; then
            printf 'entrypoint: egress is UNFILTERED and that was ACCEPTED BY CONFIGURATION\n' >&2
            printf 'entrypoint: (JKB_EGRESS_ACCEPT_UNFILTERED=1). Reason:\n  %s\n' "$reason" >&2
            printf 'entrypoint: verify.sh reports this as a failure for as long as it is set, and\n' >&2
            printf 'entrypoint: run.sh --open will not open a window on this container.\n' >&2
        else
            printf 'entrypoint: egress is NOT bounded — refusing to run. Reason:\n  %s\n' "$reason" >&2
            printf 'entrypoint: this container exists to run an agent unattended, so an unfiltered\n' >&2
            printf 'entrypoint: network is the one state it must not start in.\n' >&2
            printf 'entrypoint:   diagnose:  docker run --entrypoint bash -it <image>\n' >&2
            printf 'entrypoint:   accept it: set JKB_EGRESS_ACCEPT_UNFILTERED=1 in container.json and recreate\n' >&2
            exit 1
        fi
        ;;
    *)
        # No output, an unreadable answer, or a word this does not know. The probe reads the live
        # chains, so nothing here means it could not run — sudo broken, the script missing, the
        # image built without it. That is not evidence of anything, and unknown is never treated as
        # bounded: this is the exact spelling of the house defect the whole design exists to avoid.
        printf 'entrypoint: could not establish whether egress is bounded (state=%s) — refusing to run.\n' "${state:-<none>}" >&2
        printf 'entrypoint: egress-status.sh reads the live firewall chains; producing no answer means\n' >&2
        printf 'entrypoint: it could not run at all, which says nothing about the network. Last reason:\n  %s\n' "$reason" >&2
        printf 'entrypoint:   diagnose: docker run --entrypoint bash -it <image>\n' >&2
        exit 1
        ;;
esac

exec "$@"
