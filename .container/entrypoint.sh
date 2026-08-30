#!/usr/bin/env bash
# The container's own first act: raise the egress firewall, then become the command — but only if
# the raise established that egress is bounded. Design: openspec/changes/jkb-egress-verdict (D50).
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
# WHY IT READS A VERDICT RATHER THAN AN EXIT CODE. The first version of this file treated the
# presence of init-firewall.sh's failure marker as proof that egress was denied. It was not: that
# marker was written BEFORE any iptables call, and every one of those calls is `|| true`, so it
# meant "fail_closed ran" — true both when a deny-all was installed and when installing it failed
# and egress was left wide open. This printed "egress is DENIED" and booted the container on the
# second one. init-firewall.sh now records what it ESTABLISHED, and this decides on that.
set -uo pipefail

# Overridable ONLY so --self-test can point it somewhere writable: /run does not exist on the host
# this is developed on. It cannot be used to weaken anything — a container's environment is fixed
# at create and `docker start` reuses it, so nothing running inside can change what a later start
# sees — and the value only selects which file is read.
VERDICT="${JKB_EGRESS_VERDICT:-/run/jkb-egress-verdict}"

# THE ONE DELIBERATE ESCAPE (D50.6), and it is recorded rather than silent. Refusing to boot on an
# unfiltered verdict is right for an unattended agent, and it means a host with real IPv6 and no
# ip6tables cannot start the container at all — with no way forward but editing a root-owned script
# inside an image. A posture too tight to work is one that gets switched off, so there is a way
# through; and an override nobody can see is indistinguishable from a rule that does not exist, so
# verify.sh reports it as a FAILURE on every run for as long as it is set. Declared in
# container.json's containerEnv, which means it is fixed at create: turning it on is a deliberate,
# reviewable edit followed by a recreate, not something a session can do to itself.
ACCEPT="${JKB_EGRESS_ACCEPT_UNFILTERED:-0}"

verdict_field() { # verdict_field <key>  -> value, empty if unreadable
    [ -r "$VERDICT" ] || return 0
    sed -n "s/^$1=//p" "$VERDICT" 2>/dev/null | head -1
}

# --- self-test ------------------------------------------------------------------------------
# The boot decision had never been executed. It runs before anything else in the container and
# getting it wrong yields either a container that will not start or one running unprotected, so
# every state is exercised here against a stubbed raise. Only when it is the SOLE argument;
# run.sh always passes `sleep infinity`.
if [ "$#" -eq 1 ] && [ "$1" = "--self-test" ]; then
    fails=0
    eq() { # eq <label> <got> <want>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else fails=$((fails+1)); printf '  \033[31mFAIL\033[0m %s (got %s, wanted %s)\n' "$1" "$2" "$3"; fi
    }
    echo "==> entrypoint self-test: booting on the firewall's verdict"
    t="$(mktemp -d)"; trap 'rm -rf "$t"' EXIT
    mkdir -p "$t/bin"

    # A stub `sudo` standing in for init-firewall.sh: it writes the verdict the case is about and
    # exits how that ending would. `state=` empty means "record nothing", the aborted-early case.
    stub() { # stub <exit-code> <state-or-empty>
        cat > "$t/bin/sudo" <<STUB
#!/usr/bin/env bash
[ -n "$2" ] && printf 'state=%s\nv4=x\nv6=x\nreason=stub\n' "$2" > "\$JKB_EGRESS_VERDICT"
exit $1
STUB
        chmod +x "$t/bin/sudo"
    }
    run_ep() { # run_ep [accept]
        rm -f "$t/verdict"
        JKB_EGRESS_VERDICT="$t/verdict" JKB_EGRESS_ACCEPT_UNFILTERED="${1:-0}" \
            PATH="$t/bin:$PATH" bash "$0" echo BECAME-THE-COMMAND 2>"$t/err"
    }

    stub 0 allowlisted
    eq "an allowlisted verdict execs the command"   "$(run_ep)" "BECAME-THE-COMMAND"
    eq "...and says nothing on stderr"              "$(wc -c <"$t/err" | tr -d ' ')" "0"

    # fail_closed established blanket denial: no allowlist, so the container cannot work — but it
    # is SAFE, and staying up is what lets somebody attach and see why.
    stub 1 denied
    eq "a denied verdict still execs"               "$(run_ep)" "BECAME-THE-COMMAND"
    eq "...and says egress is denied"               "$(grep -c 'egress is DENIED' "$t/err")" "1"

    # The case the old marker could not tell apart from the one above.
    stub 1 unfiltered
    eq "an unfiltered verdict refuses"              "$(run_ep)" ""
    eq "...with a non-zero exit"                    "$(run_ep >/dev/null; echo $?)" "1"
    eq "...and says egress is NOT bounded"          "$(grep -c 'egress is NOT bounded' "$t/err")" "1"

    # ...including when the raise reported success. A green exit code is not the question.
    stub 0 unfiltered
    eq "an unfiltered verdict refuses even on exit 0" "$(run_ep >/dev/null; echo $?)" "1"

    # Nothing recorded: the raise never reached the point of saying what it left behind.
    stub 1 ""
    eq "no verdict at all refuses"                  "$(run_ep >/dev/null; echo $?)" "1"
    eq "...and says the state is unknown"           "$(grep -c 'left no verdict' "$t/err")" "1"

    # A verdict file that exists but says something nobody taught this about is not a pass.
    stub 0 wat
    eq "an unrecognised state refuses"              "$(run_ep >/dev/null; echo $?)" "1"

    # The override: boots, and is loud about it every single time.
    stub 1 unfiltered
    eq "the override boots an unfiltered container" "$(run_ep 1)" "BECAME-THE-COMMAND"
    eq "...and says it was accepted"                "$(grep -c 'ACCEPTED BY CONFIGURATION' "$t/err")" "1"
    # It is not a blanket skip: a missing verdict is still unknown, and unknown is not unfiltered.
    stub 1 ""
    eq "...but it does not cover an unknown state"  "$(run_ep 1 >/dev/null; echo $?)" "1"

    echo
    [ "$fails" -eq 0 ] || { printf '\033[31m%d failed\033[0m\n' "$fails"; exit 1; }
    printf '\033[32mentrypoint self-test passed\033[0m\n'
    exit 0
fi
# --------------------------------------------------------------------------------------------

# No arguments: sudoers grants `vscode` exactly `/usr/local/bin/init-firewall.sh ""`, and the
# script itself refuses any. Both are load-bearing — the allowlist it reads is the root-owned
# snapshot, never a path a caller names. Its exit code is deliberately NOT consulted: what decides
# is the state it left behind, which it records on every ending.
sudo -n /usr/local/bin/init-firewall.sh || true

state="$(verdict_field state)"
reason="$(verdict_field reason)"

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
            printf 'entrypoint: verify.sh reports this as a failure for as long as it is set.\n' >&2
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
        # Includes an unreadable file, an empty one, and a state this does not know. The raise
        # records on every ending it reaches, so nothing here means it did not get that far —
        # which is not evidence of anything, and unknown is never treated as denied.
        printf 'entrypoint: the firewall left no verdict this understands (state=%s) — refusing to run.\n' "${state:-<none>}" >&2
        printf 'entrypoint: it records one on every ending, so it aborted before reaching any of them.\n' >&2
        printf 'entrypoint:   diagnose: docker run --entrypoint bash -it <image>\n' >&2
        exit 1
        ;;
esac

exec "$@"
