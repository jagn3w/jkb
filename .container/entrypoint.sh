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
readonly FAILED_MARKER=/run/jkb-egress-failed

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
