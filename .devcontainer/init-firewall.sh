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

# EVERY REFUSAL MUST LEAVE MORE FILTERING THAN IT FOUND, NOT LESS. The refusals below used to
# `exit 1` before any rule was installed — and iptables rules do NOT survive a container restart
# (that is why postStart re-raises them), so a container that started offline kept UNFILTERED
# egress while the script printed "Refusing". The refusal path was less safe than the success path,
# which is the one direction this layer must never go.
fail_closed() { # fail_closed <message...>
    printf 'init-firewall: %s\n' "$*" >&2
    # DNS and loopback stay open so the container can be diagnosed and can retry; everything else
    # is denied. Deliberately does not depend on the `allowed` set having any contents.
    iptables -F OUTPUT 2>/dev/null || true
    iptables -A OUTPUT -p udp --dport 53 -j ACCEPT 2>/dev/null || true
    iptables -A OUTPUT -p tcp --dport 53 -j ACCEPT 2>/dev/null || true
    iptables -A OUTPUT -o lo -j ACCEPT 2>/dev/null || true
    iptables -A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT 2>/dev/null || true
    local v4=no v6=no
    iptables -A OUTPUT -j REJECT --reject-with icmp-port-unreachable 2>/dev/null && v4=yes
    # IPv6 TOO, or the verdict below is false on a first raise: the success path denies v6
    # outright further down, so a refusal that rebuilt only the v4 chain left v6 egress entirely
    # unfiltered while printing "egress is DENIED". Best-effort like the rest of this function —
    # a kernel without ip6tables is a normal state, and it is reported rather than assumed.
    if command -v ip6tables >/dev/null 2>&1; then
        ip6tables -F OUTPUT 2>/dev/null || true
        ip6tables -A OUTPUT -o lo -j ACCEPT 2>/dev/null || true
        ip6tables -A OUTPUT -j REJECT 2>/dev/null && v6=yes
    else
        v6=absent
    fi
    # NAMES WHAT IT ACTUALLY COVERED. "DENIED" unconditionally is the one sentence an operator
    # would act on, and it was asserting a state this function had not established.
    echo "  egress: IPv4 denied=$v4, IPv6 denied=$v6. Fix the cause and re-run to restore the allowlist." >&2
    [ "$v4" = yes ] || echo "  WARNING: the IPv4 deny-all chain could not be installed — egress may be unfiltered." >&2
    exit 1
}

# EVERY abort routes through it, not just the ones written as refusals. `set -e` aborts do not go
# near `fail_closed`, and one of them fires with the OUTPUT chain already flushed and empty —
# which is unrestricted egress reached by a path no refusal message describes. The static check in
# check-config.sh stays as second-line defence: it can see a stray `exit 1` and cannot see this.
set -E
trap 'fail_closed "an unexpected failure at line $LINENO — refusing to leave the chain as it is."' ERR

# Defined FIRST, above every refusal in this file, because that is the only way the rule it
# states can hold. It used to sit below two of them: the unparseable-snapshot guard and the
# posture-discovery guard both `exit 1` before a single rule was installed, so a truncated
# snapshot — the very state that guard's own comment describes as real — left the container
# with an empty OUTPUT chain and unrestricted egress on every later start, permanently, since
# the snapshot is root-owned and 0444. Two refusal styles in one security-critical script is
# the defect, not either message.

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
readonly SNAPSHOT=/usr/local/share/jkb-egress-allowlist.json

# WHICH workspace, now that all of ~/repos is mounted. This was `/home/vscode/repos/jkb/...`, and
# under the widened mount that literal is a statement about whichever checkout happens to sit
# there: open `~/repos/jkb-wip` and the firewall would snapshot the OTHER checkout's posture, on
# another branch, and install it as the list every later start runs on — while `verify.sh`'s own
# posture check looked at the right repo and passed. Where nothing is named `jkb` it was simply
# unreadable and container creation died.
#
# It cannot be told which one: the sudoers grant forbids arguments outright (a grant naming no
# argument accepts every argument), and an environment variable is agent-settable, which is the
# same hole one step over. So it is DISCOVERED, and ambiguity refuses rather than picks: a repo
# carrying `scripts/auto-mode-posture.json` is the marker, one match is the answer, and two — two
# jkb checkouts, or a planted decoy — is a question only a human can settle.
find_workspace_posture() {
    local d hits=()
    for d in /home/vscode/repos/*/; do
        [ -r "$d/scripts/auto-mode-posture.json" ] && hits+=("$d/scripts/auto-mode-posture.json")
    done
    [ "${#hits[@]}" -eq 1 ] || return 1
    printf '%s' "${hits[0]}"
}

[ "$#" -eq 0 ] || fail_closed "takes no arguments (the allowlist is $SNAPSHOT)."

if [ ! -e "$SNAPSHOT" ]; then
    # FIRST RAISE ONLY. Refusing here fails container creation, which is the correct direction:
    # an egress allowlist that cannot be established must not be guessed at, and nothing has run
    # in the container yet.
    workspace_posture="$(find_workspace_posture)" || fail_closed "could not identify one workspace
  posture under /home/vscode/repos (looked for */scripts/auto-mode-posture.json; found
  $(ls -d /home/vscode/repos/*/scripts/auto-mode-posture.json 2>/dev/null | wc -l | tr -d ' ')
  matches, and exactly one is needed). The egress allowlist is snapshotted from it once, at
  create, and must not be guessed."
    install -o root -g root -m 0444 "$workspace_posture" "$SNAPSHOT"
    say "snapshotted the egress allowlist from $workspace_posture (first run in this container)"
# EVERY LATER RAISE reads only the snapshot, so the lookup below is for the drift NOTE alone and
# must never refuse: exiting non-zero here would leave the rules unapplied, and unapplied rules
# mean unrestricted egress — the opposite of failing closed.
elif workspace_posture="$(find_workspace_posture)"; then
    if ! cmp -s "$workspace_posture" "$SNAPSHOT" 2>/dev/null; then
        # Reported, never acted on. Divergence is the normal state after someone edits the
        # posture, and the honest response is to say the coarse layer is still on the old list —
        # silence here would let a legitimate edit look applied when the firewall never saw it.
        say "NOTE: $workspace_posture differs from the snapshot this firewall runs on."
        say "      The coarse layer keeps the snapshot until the container is REBUILT."
    fi
else
    say "NOTE: could not identify one workspace posture to compare the snapshot against."
fi
POSTURE="$SNAPSHOT"

# Parse it HERE, once, rather than letting the first `jq` in the loop below fail under `set -e` —
# that exited 5 with no output at all, which in postCreate is an unexplained abort. A snapshot
# that will not parse is a real state (a truncated write, a half-copied file), and the operator
# needs to be told which file to look at.
jq empty "$POSTURE" 2>/dev/null || fail_closed "$POSTURE is not valid JSON — the egress
  allowlist cannot be read. Rebuild the container to re-snapshot it from the workspace posture."


# Validated BEFORE any live state is touched. `|| declared=0`: a snapshot that is valid JSON but
# not an object makes jq exit non-zero, and a bare assignment from a failing command substitution
# aborts the whole script under `set -e` — the unexplained no-output exit the `jq empty` guard
# above was added to eliminate, one line further down.
declared="$(jq -r '.require.sandbox.network.allowedDomains | length' "$POSTURE" 2>/dev/null)" || declared=0
case "$declared" in ''|*[!0-9]*) declared=0 ;; esac
if [ "$declared" -eq 0 ]; then
    fail_closed "$POSTURE declares no allowed domains (empty, truncated, or missing
  .require.sandbox.network.allowedDomains). Rebuild the container to re-snapshot it."
fi

# `allowed` must exist for the match-set rule to reference; the new contents are built in a SCRATCH
# set and swapped in only once they are known good, so a refusal leaves the live allowlist intact.
# `-exist` rather than destroy/create: `destroy` cannot run while the OUTPUT rule points at the set,
# and the subsequent `create` then failed "set with the same name already exists" under `set -e`,
# aborting postStart on every fresh create because postCreate has already installed the rule.
ipset create -exist allowed hash:net
ipset create -exist allowed-new hash:net
ipset flush allowed-new

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
    # `|| ips=""` IS THE WHOLE POINT of this line, not tidiness. `getent` exits 2 for a name with
    # no A record and `pipefail` carries that out of the pipeline; a BARE assignment is a simple
    # command in no conditional context, so `errexit` aborts — and with `set -E` the ERR trap sends
    # that to `fail_closed`. Measured: one unresolvable domain took the whole raise to deny-all
    # with no allowlist at all, blaming "an unexpected failure at line 173", while the two arms
    # written for exactly this (the per-domain skip below, and the "NONE resolved" refusal) became
    # unreachable. Reopen a container offline, or with any one of the fifteen posture domains
    # temporarily NXDOMAIN, and nothing works until the next container start.
    #
    # The other two substitutions in this file already carry a fallback, and the one at line 122 is
    # in a conditional context; measured, those shapes do not trip the trap.
    ips="$(getent ahostsv4 "$domain" 2>/dev/null | awk '{print $1}' | sort -u)" || ips=""
    if [ -z "$ips" ]; then skipped+=("$domain (no A record)"); continue; fi
    while IFS= read -r ip; do
        [ -n "$ip" ] && ipset add allowed-new "$ip" 2>/dev/null && resolved=$((resolved + 1))
    done <<<"$ips"
done <<<"$(jq -r '.require.sandbox.network.allowedDomains[]?' "$POSTURE")"

# Resolving nothing yields an empty set, and the rules below would still install and still print
# "default-deny is active" — technically true and useless: the container can reach nothing, and the
# reason is a dead resolver rather than a policy anyone chose. Refuse, fail closed, and leave the
# previous allowlist in place so a re-run at a better moment restores it.
if [ "$resolved" -eq 0 ]; then
    ipset destroy allowed-new 2>/dev/null || true
    fail_closed "$declared domain(s) declared but NONE resolved to an address — DNS is unavailable
  or every entry is a wildcard. The previous allowlist, if any, is left untouched."
fi
# Swap the freshly built set into place. Until this line the LIVE allowlist is untouched, so a
# refusal above cannot black-hole a running container — which the script's own advice to re-run it
# would otherwise do at exactly the moment resolution is flaky.
ipset swap allowed-new allowed
ipset destroy allowed-new 2>/dev/null || true
say "allowed $resolved addresses from $declared domains"
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
    fail_closed "default-REJECT rule is not in place — refusing to report success on a
  half-applied policy."
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
