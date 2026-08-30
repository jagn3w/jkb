#!/usr/bin/env bash
# The container's own first act: raise the egress firewall, then become the command.
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
# FAIL-CLOSED HAS TWO CASES AND THEY DIFFER. `init-firewall.sh`'s own `fail_closed` installs a
# deny-all chain and records why in the marker before exiting non-zero — so a failure THERE has
# already denied egress, and the container is safe to run and worth keeping up: verify.sh reports
# the marker loudly, and a person can attach and repair it. Exiting would remove that ability and
# protect nothing further. A failure that left no marker never reached `fail_closed`, so no rule
# was installed and nothing is denied; that one refuses to run at all.
#
# There is deliberately no third case for "sudo is broken" — that surfaces as a non-zero exit with
# no marker, which is the refusing arm, which is correct.
set -uo pipefail

# The marker init-firewall.sh's own `fail_closed` writes. Overridable ONLY so --self-test can put
# it somewhere writable: /run does not exist on the host this is developed on, so without this the
# staying-up arm could not be exercised anywhere but inside a broken container. It cannot be used
# to weaken anything — a container's environment is fixed at create and `docker start` reuses it,
# so nothing running INSIDE can change what a later start sees, and the value only decides which
# of two messages is printed and whether we exec. The rules themselves are iptables' business.
FAILED_MARKER="${JKB_EGRESS_FAILED_MARKER:-/run/jkb-egress-failed}"

# --- self-test ------------------------------------------------------------------------------
# THREE OUTCOMES, and getting the two failing ones the wrong way round means either a container
# that will not boot or one running unprotected. This decides that, it runs before anything else
# in the container, and it had never been executed — so it is exercised here against a stubbed
# `sudo`. Only when it is the SOLE argument; run.sh always passes `sleep infinity`.
if [ "$#" -eq 1 ] && [ "$1" = "--self-test" ]; then
    fails=0
    eq() { # eq <label> <got> <want>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else fails=$((fails+1)); printf '  \033[31mFAIL\033[0m %s (got %s, wanted %s)\n' "$1" "$2" "$3"; fi
    }
    echo "==> entrypoint self-test: what happens when the firewall raise fails"
    t="$(mktemp -d)"; trap 'rm -rf "$t"' EXIT
    mkdir -p "$t/bin"

    # A stub `sudo` whose exit code and side effect the case decides. It stands in for
    # init-firewall.sh, whose own contract is: fail_closed installs deny-all AND writes the marker.
    stub() { # stub <exit-code> <touch-marker: yes|no>
        cat > "$t/bin/sudo" <<STUB
#!/usr/bin/env bash
[ "$2" = yes ] && : > "\$JKB_EGRESS_FAILED_MARKER"
exit $1
STUB
        chmod +x "$t/bin/sudo"
    }
    # The command the entrypoint should become. Its output is the evidence it was exec'd at all.
    run_ep() { rm -f "$t/marker"; JKB_EGRESS_FAILED_MARKER="$t/marker" PATH="$t/bin:$PATH" \
                   bash "$0" echo BECAME-THE-COMMAND 2>"$t/err"; }

    stub 0 no
    eq "a clean raise execs the command"            "$(run_ep)" "BECAME-THE-COMMAND"
    eq "...and says nothing on stderr"              "$(wc -c <"$t/err" | tr -d ' ')" "0"

    # fail_closed ran: deny-all is installed, so the container is SAFE and worth keeping up.
    stub 1 yes
    eq "a raise that failed closed still execs"     "$(run_ep)" "BECAME-THE-COMMAND"
    eq "...and says egress is denied"               "$(grep -c 'egress is DENIED' "$t/err")" "1"

    # Nothing was installed, so nothing is denied. This one must NOT run the command.
    stub 1 no
    eq "a raise that installed no rules refuses"    "$(run_ep)" ""
    eq "...with a non-zero exit"                    "$(run_ep >/dev/null; echo $?)" "1"
    eq "...and says why"                            "$(grep -c 'installed no rules' "$t/err")" "1"

    echo
    [ "$fails" -eq 0 ] || { printf '\033[31m%d failed\033[0m\n' "$fails"; exit 1; }
    printf '\033[32mentrypoint self-test passed\033[0m\n'
    exit 0
fi
# --------------------------------------------------------------------------------------------

# No arguments: sudoers grants `vscode` exactly `/usr/local/bin/init-firewall.sh ""`, and the
# script itself refuses any. Both are load-bearing — the allowlist it reads is the root-owned
# snapshot, never a path a caller names.
if ! sudo -n /usr/local/bin/init-firewall.sh; then
    if [ -e "$FAILED_MARKER" ]; then
        printf 'entrypoint: the firewall failed closed — egress is DENIED (%s):\n  %s\n' \
               "$FAILED_MARKER" "$(head -1 "$FAILED_MARKER" 2>/dev/null)" >&2
        printf 'entrypoint: staying up so it can be diagnosed; verify.sh reports this.\n' >&2
    else
        printf 'entrypoint: init-firewall.sh failed and installed no rules — refusing to run.\n' >&2
        printf 'entrypoint: to debug, start a shell that skips this: docker run --entrypoint bash ...\n' >&2
        exit 1
    fi
fi

exec "$@"

