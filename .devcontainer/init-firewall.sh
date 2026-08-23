#!/usr/bin/env bash
# Container egress allowlist (design D49). Runs as root at container start, via the single
# sudoers entry granting exactly this path.
#
# WHY THIS EXISTS WHEN THE SANDBOX ALREADY HAS strictAllowlist. Because that control lives in the
# layer that might not start. A container's default egress is unrestricted, so a container whose
# nested sandbox failed to initialise would be a DOWNGRADE on exfiltration versus running on the
# host — the one direction this whole exercise must not go. This is the layer that holds when the
# other one doesn't.
#
# IT IS COARSE, AND THAT IS THE DIVISION OF LABOUR. iptables filters by IP, so it can only pin
# domains that resolve to stable addresses, and it cannot see which host a TLS connection asked
# for. The nested sandbox filters by hostname at a proxy and is precise. Coarse-but-independent
# under precise-but-in-the-same-process is the point: they fail for different reasons.
#
# ONE SOURCE OF TRUTH: the allowlist comes from scripts/auto-mode-posture.json, the same file the
# sandbox posture comes from. Two egress lists that could disagree is how the tighter one ends up
# decorative. It is read at container CREATE and snapshotted (see below), so what this enforces is
# that file as it stood when the container was built, not as it stands now.
set -euo pipefail

say() { printf '[firewall] %s\n' "$*"; }

# THE ALLOWLIST THIS RUNS ON IS NOT THE ONE IN THE WORKSPACE. It used to be: the caller passed a
# path, and the path passed was the repo's own copy — bind-mounted, under allowWrite, writable by
# the agent this layer exists to bound. Appending a domain there and waiting for the next container
# start had root add it to the ipset. A backstop the bounded party can edit is not a backstop.
#
# So the workspace copy is read exactly ONCE, at container create, before any agent session exists,
# and snapshotted somewhere only root can write. Every later run reads the snapshot. A rebuild is
# what re-reads the repo — which is right, because widening egress should be a human act, and
# rebuilding is one. The argument is gone entirely rather than defaulted, so the sudoers entry can
# forbid arguments outright; a grant naming no argument accepts every argument.
readonly WORKSPACE_POSTURE=/home/vscode/repos/jkb/scripts/auto-mode-posture.json
readonly SNAPSHOT=/usr/local/share/jkb-egress-allowlist.json

[ "$#" -eq 0 ] || { echo "init-firewall: takes no arguments (the allowlist is $SNAPSHOT)" >&2; exit 2; }

if [ ! -e "$SNAPSHOT" ]; then
    [ -r "$WORKSPACE_POSTURE" ] || { echo "init-firewall: cannot read $WORKSPACE_POSTURE to snapshot" >&2; exit 1; }
    install -o root -g root -m 0444 "$WORKSPACE_POSTURE" "$SNAPSHOT"
    say "snapshotted the egress allowlist from the workspace (first run in this container)"
elif ! cmp -s "$WORKSPACE_POSTURE" "$SNAPSHOT" 2>/dev/null; then
    # Reported, never acted on. Divergence is the normal state after someone edits the posture,
    # and the honest response is to say the coarse layer is still on the old list — silence here
    # would let a legitimate edit look applied when the firewall never saw it.
    say "NOTE: $WORKSPACE_POSTURE differs from the snapshot this firewall runs on."
    say "      The coarse layer keeps the snapshot until the container is REBUILT."
fi
POSTURE="$SNAPSHOT"

# Idempotent regardless of whether an iptables rule still references the set. `destroy` cannot run
# while the OUTPUT rule points at it, and the subsequent `create` then failed "set with the same
# name already exists" under `set -e` — which aborted postStart on every fresh container create,
# because postCreate has already installed the rule by then.
ipset create -exist allowed hash:net
ipset flush allowed

skipped=()
resolved=0
while IFS= read -r domain; do
    [ -n "$domain" ] || continue
    case "$domain" in
        # A wildcard has no address to pin. Skipping is honest: the nested sandbox matches these
        # precisely by hostname, and inventing an IP range here would be a guess presented as a rule.
        \**) skipped+=("$domain"); continue ;;
        localhost|127.0.0.1|::1) continue ;;   # loopback never leaves the container
    esac
    ips="$(getent ahostsv4 "$domain" 2>/dev/null | awk '{print $1}' | sort -u)"
    if [ -z "$ips" ]; then skipped+=("$domain (no A record)"); continue; fi
    while IFS= read -r ip; do
        [ -n "$ip" ] && ipset add allowed "$ip" 2>/dev/null && resolved=$((resolved + 1))
    done <<<"$ips"
done <<<"$(jq -r '.require.sandbox.network.allowedDomains[]?' "$POSTURE")"

say "allowed $resolved addresses from $(jq -r '.require.sandbox.network.allowedDomains | length' "$POSTURE") domains"
[ ${#skipped[@]} -eq 0 ] || say "not pinned at the IP layer (the sandbox matches these by name): ${skipped[*]}"

# Flush first so a re-run is idempotent rather than additive.
iptables -F OUTPUT
iptables -F INPUT 2>/dev/null || true

# IPv6 is a SECOND, independent egress path. Filtering only v4 leaves the whole allowlist
# bypassable over AAAA while this script prints "default-deny is active" — a firewall that
# announces coverage it does not have. There is no v6 allowlist to build (the ipset above is
# hash:net over v4 addresses), so v6 is denied outright: the allowlisted hosts all resolve over
# v4, and a service reachable only over v6 fails closed and visibly.
have_v6=0
if command -v ip6tables >/dev/null 2>&1 && ip6tables -L OUTPUT >/dev/null 2>&1; then
    have_v6=1
    ip6tables -F OUTPUT
    ip6tables -A OUTPUT -o lo -j ACCEPT
    ip6tables -A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
    ip6tables -A OUTPUT -j REJECT
fi

# DNS must survive or nothing resolves, including the allowlisted hosts themselves.
iptables -A OUTPUT -p udp --dport 53 -j ACCEPT
iptables -A OUTPUT -p tcp --dport 53 -j ACCEPT
iptables -A OUTPUT -o lo -j ACCEPT
iptables -A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
iptables -A OUTPUT -m set --match-set allowed dst -j ACCEPT
iptables -A OUTPUT -j REJECT --reject-with icmp-port-unreachable

# Fail loudly rather than leave a half-applied policy that reads as protection.
if ! iptables -C OUTPUT -j REJECT --reject-with icmp-port-unreachable 2>/dev/null; then
    echo "init-firewall: default-REJECT rule is not in place — refusing to report success" >&2
    exit 1
fi
if [ "$have_v6" -eq 1 ]; then
    say "egress default-deny is active (IPv4 allowlisted, IPv6 denied outright)"
else
    # Not silently: an operator who believes v6 is filtered here would be wrong.
    say "egress default-deny is active (IPv4 only — ip6tables unavailable, so IPv6 is UNFILTERED;"
    say "  this is safe only because the container has no IPv6 route, which is not checked here)"
fi

# #13: the ipset is resolved once, at start. A CDN-fronted allowlisted host whose A records rotate
# stops working later and surfaces as a bare connection refusal with nothing pointing here. Say so
# where it will be read, since re-resolving on a timer is its own daemon and a worse trade.
say "addresses were resolved once, now. If an allowlisted host later fails to connect, its A"
say "  records may have rotated — re-run this script to re-resolve (it is idempotent)."
