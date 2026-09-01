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

# THE MEASUREMENT AND THE RULE COME FROM ONE PLACE (D51.1). `verdict_state`, `v6_path_state` and the
# chain probes used to live in this file; egress-status.sh needs the same answers for the boot
# decision, and D50's own history is what makes a second copy unacceptable — the rule was spelled
# twice inside THIS script and that is precisely how the success path came to report success on
# unfiltered IPv6.
# shellcheck source=egress-lib.sh
. "$(dirname "$0")/egress-lib.sh"

# WHY THIS RAISE ENDED AS IT DID, written on every ending (D50.1). Under D51 this is no longer what
# decides anything: the entrypoint and verify.sh ask the kernel (egress-status.sh) what the chains
# hold right now, because a record is an EVENT and they are asking a present-tense question. What
# only the record can supply is the REASON — the kernel can say egress is unfiltered, it cannot say
# DNS failed at 09:14 or that the allowlist snapshot was truncated.
#
# So a stale record is no longer dangerous, merely unhelpful: readers take the state from the probe
# and use this for the explanation, and a record whose state disagrees with the probe is reported as
# drift rather than obeyed.
#
# THE PATH COMES FROM THE LIBRARY, which is sourced above and where both READERS get it. This used
# to be a second `${JKB_EGRESS_VERDICT:-/run/jkb-egress-verdict}` declared here -- and the
# check-config guard that compared the spellings was deleted when egress-lib.sh was introduced, on
# the stated premise that there was now only one. There were two. Change the compiled-in default in
# the library alone and the raise would write the old file while `verdict_field` read the new one:
# the entrypoint gets an empty reason and prints "(the raise left no reason)" on every refusal,
# discarding the DNS remedy this file's own self-test exists to protect, with every static and
# mutation guard still green. They shared the JKB_EGRESS_VERDICT override, which is why only a
# change to the default would have split them -- and why nothing noticed.
#
# Overridable ONLY so --self-test can point it somewhere writable. It cannot weaken anything: sudo
# runs with env_reset, so the sudoers-granted path never sees a caller's value, and a non-root run
# that supplies one cannot write the real file anyway (it is root-owned).
VERDICT="$VERDICT_PATH"

# NEWLINES ARE FLATTENED HERE, which is the one place it can be enforced (D51.8). Every reason on
# the paths that matter is multi-line, and both readers parse a field with `sed | head -1` — so the
# DNS refusal lost "Fix DNS and re-run this, or restart the container, to restore it" and the
# posture refusal lost its whole remedy list. D50 chose refusing-to-boot on the argument that it
# prints its own escape, and the escape was being stripped in transit. The round-trip assertion
# passed only because it was fed a single-line reason the writer never emits.
record_verdict() { # record_verdict <state> <v4> <v6> <reason...>
    local state="$1" v4="$2" v6="$3"; shift 3
    local reason; reason="$(printf '%s' "$*" | tr '\n' ' ' | tr -s ' ')"
    { printf 'state=%s\n' "$state"
      printf 'v4=%s\n'    "$v4"
      printf 'v6=%s\n'    "$v6"
      printf 'reason=%s\n' "$reason"
    } > "$VERDICT" 2>/dev/null || true
}

# `v6_state` is NOT defined here any more — it lives in egress-lib.sh, sourced above, beside the
# measurements it composes and beside the two other callers that had each spelled the collapse for
# themselves (D52.5). A function defined in this file could not be reached by the library's own
# self-test, which is how a regression in it survived a green gate.

# EVERY REFUSAL MUST LEAVE MORE FILTERING THAN IT FOUND, NOT LESS. The refusals below used to
# `exit 1` before any rule was installed — and iptables rules do NOT survive a container restart,
# so a container that started offline kept UNFILTERED egress while the script printed "Refusing".
# The refusal path was less safe than the success path, which is the one direction this layer must
# never go. What it leaves is now also RECORDED, so a caller can act on it rather than infer it.
fail_closed() { # fail_closed <message...>
    printf 'init-firewall: %s\n' "$*" >&2
    # DNS and loopback stay open so the container can be diagnosed and can retry; everything else
    # is denied. Deliberately does not depend on the `allowed` set having any contents.
    iptables -w 5 -F OUTPUT 2>/dev/null || true
    iptables -w 5 -A OUTPUT -p udp --dport 53 -j ACCEPT 2>/dev/null || true
    iptables -w 5 -A OUTPUT -p tcp --dport 53 -j ACCEPT 2>/dev/null || true
    iptables -w 5 -A OUTPUT -o lo -j ACCEPT 2>/dev/null || true
    iptables -w 5 -A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT 2>/dev/null || true
    local v4 v6
    iptables -w 5 -A OUTPUT $RULE_V4_REJECT 2>/dev/null || true
    # MEASURED, not inferred from whether the install command succeeded. `iptables -A` returning 0
    # is a statement about one command; what the record and the probe both need is what the chain
    # HOLDS, and those come apart under a lost xtables lock or a concurrent flush. Same function the
    # boot probe uses, so the two cannot disagree about what bounded means.
    v4="$(v4_chain_state)" || v4=open
    [ "$v4" = bounded ] && v4=denied
    # IPv6 TOO, or the verdict is false on a first raise: the success path denies v6 outright
    # further down, so a refusal that rebuilt only the v4 chain left v6 egress entirely unfiltered
    # while printing "egress is DENIED". Best-effort like the rest of this function.
    if command -v ip6tables >/dev/null 2>&1; then
        ip6tables -w 5 -F OUTPUT 2>/dev/null || true
        ip6tables -w 5 -A OUTPUT -o lo -j ACCEPT 2>/dev/null || true
        ip6tables -w 5 -A OUTPUT $RULE_V6_REJECT 2>/dev/null || true
    fi
    # `|| v6=open` is not defensive noise: D50.4 says an unobtainable measurement is
    # `open`, and a bare assignment here would also abort the whole raise into
    # fail_closed under the ERR trap. The guard and the rule want the same line.
    v6="$(v6_state)" || v6=open

    # THE STATE IT LEAVES BEHIND, not the fact that it ran (D50.2). Both families must be provably
    # closed to call this denied; anything else is unfiltered, and under D50.3 the entrypoint
    # refuses to boot on it rather than printing that egress is denied and running anyway.
    record_verdict "$(verdict_state "$v4" "$v6" denied)" "$v4" "$v6" "$*"

    # NAMES WHAT IT ACTUALLY COVERED. "DENIED" unconditionally is the one sentence an operator
    # would act on, and it was asserting a state this function had not established.
    echo "  egress: IPv4=$v4, IPv6=$v6. Fix the cause and re-run to restore the allowlist." >&2
    if [ "$v4" != denied ]; then
        echo "  WARNING: the IPv4 deny-all chain could not be installed — egress is UNFILTERED." >&2
    fi
    if [ "$v6" = open ]; then
        echo "  WARNING: IPv6 egress is UNFILTERED (no ip6tables, and this container has IPv6)." >&2
    fi
    exit 1
}

# --- self-test --------------------------------------------------------------------------------
# What this script RECORDS no longer decides the boot (D51 moved that to egress-status.sh, which
# asks the kernel), but it is still the only source of the REASON a refusal prints — and D50 chose
# refusing-to-boot on the argument that it prints its own escape. So what is covered here is the
# half only this script owns: that a reason survives the trip to the reader intact.
#
# The rule itself and every measurement now live in egress-lib.sh and are covered by its own
# self-test; duplicating those rows here would be a second copy of the thing this change exists to
# have one of. Placed above `set -E`/the ERR trap deliberately: a failing assertion should report
# itself, not route into fail_closed. Only when it is the sole argument.
if [ "$#" -eq 1 ] && [ "${1:-}" = "--self-test" ]; then
    fails=0
    eq() { # eq <label> <got> <want>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else fails=$((fails+1)); printf '  \033[31mFAIL\033[0m %s (got %s, wanted %s)\n' "$1" "$2" "$3"; fi
    }
    echo "==> firewall self-test: the reason it records"
    t="$(mktemp -d)"; trap 'rm -rf "$t"' EXIT

    # A MULTI-LINE REASON, because that is what this script actually emits — every fail_closed
    # message and the unfiltered branch are multi-line, and both readers parse a field with
    # `sed | head -1`. The previous version of this assertion was named "the reason reaches the
    # reader intact" and passed only because it was handed a single-line reason the writer never
    # produces, so the remedy on every refusal that matters was being stripped in transit and
    # nothing noticed. record_verdict flattens newlines now; this is what holds it to that.
    VERDICT="$t/verdict" record_verdict denied denied absent "DNS resolved none of them.
  Fix DNS and re-run this, or restart the container, to restore the allowlist."
    got="$(sed -n 's/^reason=//p' "$t/verdict" | head -1)"
    case "$got" in
        *"Fix DNS and re-run this"*) printf '  \033[32mok\033[0m   a multi-line reason survives head -1\n' ;;
        *) fails=$((fails+1)); printf '  \033[31mFAIL\033[0m a multi-line reason loses its remedy (got: %s)\n' "$got" ;;
    esac
    eq "the record is still one field per line" "$(grep -c '=' "$t/verdict")" "4"

    # ...and against the REAL reader, which is the contract no static check can reach.
    ep="$(dirname "$0")/entrypoint.sh"
    if [ ! -r "$ep" ]; then
        fails=$((fails+1)); printf '  \033[31mFAIL\033[0m entrypoint.sh is not beside this script — the round-trip below checked nothing\n'
    else
        mkdir -p "$t/bin"
        # The raise is a no-op (the record is already written); the probe reports a bounded kernel,
        # so the entrypoint boots and prints the recorded reason for the `denied` arm.
        printf '#!/usr/bin/env bash\ncase "$*" in *egress-status*) printf "state=denied\\n";; esac\nexit 0\n' > "$t/bin/sudo"
        chmod +x "$t/bin/sudo"
        JKB_EGRESS_VERDICT="$t/verdict" PATH="$t/bin:$PATH" bash "$ep" echo BOOTED >/dev/null 2>"$t/err" || true
        eq "the remedy reaches the operator" "$(grep -c 'Fix DNS and re-run this' "$t/err")" "1"
    fi

    echo
    [ "$fails" -eq 0 ] || { printf '\033[31m%d failed\033[0m\n' "$fails"; exit 1; }
    printf '\033[32mfirewall self-test passed\033[0m\n'
    exit 0
fi
# ------------------------------------------------------------------------------------------------

# EVERY abort routes through it, not just the ones written as refusals. `set -e` aborts do not go
# near `fail_closed`, and one of them fires with the OUTPUT chain already flushed and empty —
# which is unrestricted egress reached by a path no refusal message describes. The static check in
# check-config.sh stays as second-line defence: it can see a stray `exit 1` and cannot see this.
set -E
trap 'fail_closed "an unexpected failure at line $LINENO — refusing to leave the chain as it is."' ERR

# ONE RAISE AT A TIME, SERIALISED IN THE CALLEE (D51.6). Two raises run concurrently on every fresh
# create — the entrypoint's, and run.sh's re-raise — because `docker run --detach` returns as soon
# as PID 1 starts, while this script is still inside ~15 DNS lookups. They share one `allowed-new`
# ipset, one OUTPUT chain and one record, and the interleavings are destructive: B's `ipset flush`
# wipes what A accumulated; A's `ipset destroy` makes B's adds fail silently into `resolved=0`,
# which calls fail_closed and installs a blanket deny on a machine with perfectly healthy DNS.
#
# In the CALLEE, because a rule every caller must remember is the defect — a third way to start a
# raise (a hand-run `docker exec`, a future verb) is covered without being told. `-w` on iptables
# closes only the xtables-lock half and is kept below as the narrower backstop.
#
# Best-effort: a container image without flock still raises, unserialised, exactly as before. That
# is the same trade as the rest of this function — never fail to raise because a helper is missing.
if command -v flock >/dev/null 2>&1; then
    exec 9>/run/jkb-egress-raise.lock 2>/dev/null || true
    flock 9 2>/dev/null || true
fi

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
    #
    # NAMES WHAT TO DO, and lists the candidates. Mounting all of ~/repos made "two checkouts
    # carrying a posture" the ordinary case rather than an exotic one — a second clone, a fork, a
    # colleague's copy — and this used to abort container creation with a count and no action,
    # which for the person who has just hit it is indistinguishable from a broken image.
    workspace_posture="$(find_workspace_posture)" || fail_closed "could not identify ONE workspace
  posture under /home/vscode/repos, and the egress allowlist is snapshotted from it once, at
  create. It must not be guessed, and this script cannot be told which to use: the sudoers grant
  forbids arguments (a grant naming no argument accepts every argument) and an environment
  variable would be agent-settable.

  Candidates found ($(ls -d /home/vscode/repos/*/scripts/auto-mode-posture.json 2>/dev/null | wc -l | tr -d ' ')):
$(ls -d /home/vscode/repos/*/scripts/auto-mode-posture.json 2>/dev/null | sed 's/^/    /' || true)

  To fix it, on the HOST, then rebuild the container:
    * none listed  — the checkout you opened has no scripts/auto-mode-posture.json. Open a repo
                     that has one, or restore it.
    * two or more  — move all but one out of ~/repos (they are mounted because ~/repos is mounted
                     whole). A checkout kept elsewhere is not seen by the container at all."
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
# reason is a dead resolver rather than a policy anyone chose. So refuse.
#
# What that COSTS changed when refusals were routed through `fail_closed`, and the message here
# had not caught up: it still promised "the previous allowlist is left untouched", which was true
# when a refusal exited without touching iptables and is false now that every refusal installs
# deny-all. Both are defensible — an unestablished allowlist should not leave egress open — but
# the operator has to be told which one happened, because "left untouched" and "you can reach
# nothing until this succeeds" call for different next moves.
if [ "$resolved" -eq 0 ]; then
    ipset destroy allowed-new 2>/dev/null || true
    fail_closed "$declared domain(s) declared but NONE resolved to an address — DNS is unavailable
  or every entry is a wildcard. Egress is now DENIED rather than left as it was: an allowlist that
  could not be established must not leave the container reachable. Fix DNS and re-run this, or
  restart the container, to restore it."
fi
# Swap the freshly built set into place. Until this line the live allowlist is untouched, but a
# refusal above still installs deny-all through `fail_closed`, so a running container IS cut off
# until the raise succeeds — deliberately, and said in the refusal. It used to be that a refusal
# changed nothing; that read better but left egress open on the one path where the allowlist could
# not be established, which is the opposite of what this layer is for.
ipset swap allowed-new allowed
ipset destroy allowed-new 2>/dev/null || true
say "allowed $resolved addresses from $declared domains"
[ ${#skipped[@]} -eq 0 ] || say "not pinned at the IP layer (the sandbox matches these by name): ${skipped[*]}"

# Flush first so a re-run is idempotent rather than additive.
iptables -w 5 -F OUTPUT
iptables -w 5 -F INPUT 2>/dev/null || true

# IPv6 is a SECOND, independent egress path. Filtering only v4 leaves the whole allowlist
# bypassable over AAAA while this script prints "default-deny is active" — a firewall that
# announces coverage it does not have. There is no v6 allowlist to build (the ipset above is
# hash:net over v4 addresses), so v6 is denied outright: the allowlisted hosts all resolve over
# v4, and a service reachable only over v6 fails closed and visibly.
have_v6=0
if command -v ip6tables >/dev/null 2>&1 && ip6tables -L OUTPUT >/dev/null 2>&1; then
    have_v6=1
    ip6tables -w 5 -F OUTPUT
    ip6tables -w 5 -A OUTPUT -o lo -j ACCEPT
    ip6tables -w 5 -A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
    ip6tables -w 5 -A OUTPUT $RULE_V6_REJECT
fi

# DNS must survive or nothing resolves, including the allowlisted hosts themselves.
iptables -w 5 -A OUTPUT -p udp --dport 53 -j ACCEPT
iptables -w 5 -A OUTPUT -p tcp --dport 53 -j ACCEPT
iptables -w 5 -A OUTPUT -o lo -j ACCEPT
iptables -w 5 -A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
iptables -w 5 -A OUTPUT $RULE_ALLOWLIST
iptables -w 5 -A OUTPUT $RULE_V4_REJECT

# Fail loudly rather than leave a half-applied policy that reads as protection.
if ! iptables -w 5 -C OUTPUT $RULE_V4_REJECT 2>/dev/null; then
    fail_closed "default-REJECT rule is not in place — refusing to report success on a
  half-applied policy."
fi
# THE SUCCESS PATH IS SUBJECT TO THE SAME RULE (D50.2/D50.4). It used to allowlist IPv4, print
# "IPv6 is UNFILTERED", and report success anyway — its own comment conceding that was "safe only
# because the container has no IPv6 route, which is not checked here". It is checked now: an
# unproven family is open, and a container with real IPv6 and no ip6tables has a bypassable
# allowlist, so the verdict says so and the entrypoint refuses to boot on it.
# See fail_closed: an unobtainable measurement is `open` (D50.4), and a bare assignment
# would abort into fail_closed under the ERR trap.
v6="$(v6_state)" || v6=open
# The check above established IPv4: the default-REJECT rule is in place behind the allowlist, so
# `allowlisted` here is measured, not assumed. Same rule as fail_closed's, same function.
if [ "$(verdict_state allowlisted "$v6" allowlisted)" = allowlisted ]; then
    record_verdict allowlisted allowlisted "$v6" "allowlist raised for $resolved addresses from $declared domains"
    if [ "$v6" = denied ]; then
        say "egress default-deny is active (IPv4 allowlisted, IPv6 denied outright)"
    else
        say "egress default-deny is active (IPv4 allowlisted; this container has no way out over"
        say "  IPv6 — no off-link address and no default route, measured rather than assumed)"
    fi
else
    record_verdict unfiltered allowlisted open "IPv4 is allowlisted but IPv6 is unfiltered:
  ip6tables is unavailable and this container has a way out over IPv6 (an off-link address or a
  default route), so the allowlist is bypassable over AAAA. Set JKB_EGRESS_ACCEPT_UNFILTERED=1 in
  container.json and recreate if you accept that; the container will not start otherwise."
    say "IPv4 is allowlisted, but IPv6 is UNFILTERED — ip6tables is unavailable and this container"
    say "  has IPv6, so every allowlisted-host rule is bypassable over AAAA. Recorded as unfiltered;"
    say "  the container will refuse to start. See JKB_EGRESS_ACCEPT_UNFILTERED in container.json."
fi

# #13: the ipset is resolved once, at start. A CDN-fronted allowlisted host whose A records rotate
# stops working later and surfaces as a bare connection refusal with nothing pointing here. Say so
# where it will be read, since re-resolving on a timer is its own daemon and a worse trade.
say "addresses were resolved once, now. If an allowlisted host later fails to connect, its A"
say "  records may have rotated — re-run this script to re-resolve (it is idempotent)."
