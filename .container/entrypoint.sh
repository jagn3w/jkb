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

# The verdict path, the `key=value` parser and the verdict-state vocabulary all come from here --
# egress-lib.sh is installed BESIDE this script in /usr/local/bin, and sits beside it in the
# checkout too, so one `dirname $0` reaches it in both places (D52.5).
# shellcheck source=egress-lib.sh
. "$(dirname "$0")/egress-lib.sh"

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
    # THE REAPER STUB. The real one is tini, which does not exist on a macOS host, so the exec
    # that hands over PID 1 is path-injected. The stub is transparent — it drops the `--` and
    # execs the command — so every assertion below still reads the command's own output, and it
    # records that it ran so that "the reaper was skipped entirely" is distinguishable from "the
    # command ran". Without that marker this stub would turn the reaper into something the test
    # cannot see, which is how the bare `exec "$@"` went unnoticed in the first place.
    mkdir -p "$t/bin"
    cat >"$t/bin/reaper" <<'STUB'
#!/bin/sh
echo reaped >>"$JKB_REAPER_LOG"
[ "$1" = "--" ] && shift
exec "$@"
STUB
    chmod +x "$t/bin/reaper"
    export JKB_REAPER_LOG="$t/reaped"

    # A fake /proc/self/ns: two symlinks whose TARGETS stand in for the nsfs ids. macOS has no
    # /proc, so without this seam the marker write is the one line here that cannot run.
    mkdir -p "$t/ns"
    ln -sfn 'pid:[4026531836]' "$t/ns/pid"
    ln -sfn 'mnt:[4026532999]' "$t/ns/mnt"

    run_ep() { # run_ep [accept]
        rm -f "$t/verdict"
        JKB_EGRESS_VERDICT="$t/verdict" JKB_EGRESS_ACCEPT_UNFILTERED="${1:-0}" \
            JKB_REAPER="$t/bin/reaper" JKB_NS_MARKER="$t/nsmarker" JKB_NS_DIR="$t/ns" \
            PATH="$t/bin:$PATH" bash "$0" echo BECAME-THE-COMMAND 2>"$t/err"
    }
    export JKB_EGRESS_VERDICT="$t/verdict"

    stub allowlisted "allowlist raised"
    rm -f "$t/reaped"
    eq "an allowlisted kernel execs the command"    "$(run_ep)" "BECAME-THE-COMMAND"
    eq "...and says nothing on stderr"              "$(wc -c <"$t/err" | tr -d ' ')" "0"
    # THE HANDOVER GOES THROUGH A REAPER. `sleep` as PID 1 never wait()s, so every orphan
    # reparented to it stayed a zombie for ever (README.md, "The measurements this is built
    # on"). The command running proves nothing about this on its own: a bare `exec "$@"` satisfies
    # every other assertion here, which is exactly why it survived.
    eq "...and hands over THROUGH the reaper, not straight to the command" \
       "$(cat "$t/reaped" 2>/dev/null)" "reaped"
    # WHOSE NAMESPACES THESE ARE, recorded for verify.sh. Both ids, in one file, written only on a
    # boot that reached the handover.
    eq "...and records both namespace ids for verify.sh" \
       "$(tr '\n' ' ' <"$t/nsmarker" 2>/dev/null)" "pid=pid:[4026531836] mnt=mnt:[4026532999] "

    # DELETE-FIRST, WHICH IS THE HALF THAT CANNOT BE INFERRED FROM A HAPPY PATH. A marker in the
    # container's writable layer outlives `docker stop`, and namespace ids are REUSED once a
    # namespace dies -- so a marker left behind by a previous boot can be matched by a later,
    # unrelated namespace. The guarantee is that a boot which does not reach its handover leaves
    # ABSENCE. `unfiltered` is such a boot: it refuses long before the write.
    printf 'pid=pid:[1] mnt=mnt:[1]\n' > "$t/nsmarker"
    stub unfiltered "IPv6 is unfiltered"
    run_ep >/dev/null 2>&1
    eq "a boot that REFUSES leaves no stale marker behind" \
       "$([ -e "$t/nsmarker" ] && echo present || echo absent)" "absent"
    stub allowlisted "allowlist raised"

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

# THE NAMESPACE MARKER IS DELETED FIRST AND WRITTEN LAST, and the order is the whole guarantee.
#
# It records which pid and mount namespaces are THIS container's, so verify.sh can tell them from a
# nested sandbox's (see the write at the foot of this file). The failure to design against is the
# one D51 spent a round on: a record that outlives the state it describes. `/run` is the container's
# writable layer, not a tmpfs, so a marker survives `docker stop`/`docker start` exactly as the old
# egress marker did — and namespace ids are IDA-allocated and REUSED once a namespace dies, so a
# stale marker naming a dead namespace can be matched by a later, unrelated one.
#
# Deleting on entry and writing immediately before the handover makes that unrepresentable rather
# than guarded: a marker can only exist for the namespaces of a container that reached its exec, and
# a start that died anywhere in between leaves ABSENCE, which verify.sh refuses on. There is no
# window in which a marker describes a namespace this boot does not hold.
rm -f "${JKB_NS_MARKER:?the image must set JKB_NS_MARKER (see the Dockerfile)}"

# No arguments to either: sudoers grants `vscode` exactly these two paths with none, and both
# scripts refuse any. The allowlist the raise reads is the root-owned snapshot, never a path a
# caller names. The raise's exit code is deliberately NOT consulted — what decides is what the
# kernel holds afterwards.
sudo -n /usr/local/bin/init-firewall.sh || true

probe="$(sudo -n /usr/local/bin/egress-status.sh 2>/dev/null)" || probe=""
state="$(kv_field state "$probe")"
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

# PID 1 MUST REAP, AND `sleep` CANNOT. This script is the image's ENTRYPOINT and nothing in here
# runs a program, so run.sh keeps the container alive by passing `sleep infinity` as the command —
# which a bare `exec "$@"` would make the PID 1 of this namespace. A process whose parent exits is
# reparented to PID 1, and PID 1 must wait() on it or it stays a zombie for ever; `sleep` never
# wait()s. The leak is therefore unbounded, and it is not a corner case: it is one zombie per
# sandboxed Bash call (the zombies are bwrap/bash/sh/touch), so it tracks agent activity and an
# unattended session walks into it unaided, and the end state is not a slow degradation but every
# build, shell and tool call failing at once. README.md, "The measurements this is built on", has
# the numbers and the date they were taken; they are deliberately NOT restated here, because a
# count copied to a second place is a count that goes stale in one of them.
#
# WHY NOT `--init`. Docker's own tini does exactly this job, and it is the wrong shape HERE: it
# becomes PID 1 *wrapping* this script, so PID 1's argv is `/sbin/docker-init -- …/entrypoint.sh
# sleep infinity` for the whole life of the container. run.sh's `settle()` reads `ps -o args= -p 1`
# and treats a match of `*entrypoint.sh*` as "the entrypoint has not finished yet" — so under
# `--init` it never settles, exhausts its 120s budget and fails every create and start. Probing
# PID 1's children instead would have a race of its own: there is a window in which tini has not
# yet forked, and `settle_step` deliberately does not retry an unreadable probe.
#
# Exec'ing tini FROM here keeps PID 1's argv meaningful, which is what settle() actually reads:
# while this script runs PID 1 is `entrypoint.sh sleep infinity` (waiting), and the moment it
# hands over PID 1 is `tini -- sleep infinity` (settled, and reaping). No window, no new probe,
# and settle_step is unchanged. `-s` is not passed: tini reaps unconditionally when it IS PID 1.
#
# The egress boot gate above is untouched — it has already run and can still have refused.
#
# NO DEFAULT, DELIBERATELY. The path is the Dockerfile's `ENV JKB_REAPER`, which reaches this
# process because ENV persists into the image config — so writing a fallback here would put the
# path in two files again, which is the duplication a guard was briefly added to police instead of
# remove. `:?` turns a dropped ENV into a named failure at start rather than into `exec: not
# found` one level down, where run.sh reports it as the container dying and names the egress boot
# gate as the likeliest cause.
#
# The variable doubles as the self-test seam, as JKB_EGRESS_VERDICT and JKB_INET6_PATH do: the
# macOS self-test sets it to a transparent stub, since this is the one line in here that cannot
# otherwise run off Linux. What that test proves is that the script hands over THROUGH whatever
# JKB_REAPER names; that the named thing exists and reaps is the Dockerfile's `test -x` and
# verify.sh's runtime assertion respectively.
# RECORD WHOSE NAMESPACES THESE ARE, as the last act before handing over. verify.sh's assertions --
# the mount boundary and PID 1 reaping -- are only about this container if the /proc and the mount
# table it reads are this container's. Claude Code's sandbox wraps a Bash call in `bwrap ... --bind
# / / --unshare-pid --unshare-user --proc /proc`: the fresh procfs makes /proc/1 bwrap's init, which
# REAPS, so the reaping assertion passes on a container that does not. Measured, and reproduced
# against the pre-refusal commit (README.md).
#
# THE IDENTITY IS THE KERNEL'S, NOT A PROXY FOR IT. `/proc/self/ns/{pid,mnt}` resolve to nsfs inodes
# that ARE the namespaces' identities, so verify.sh comparing its own against these is the question
# asked rather than an inference from process topology. Two earlier discriminators inferred: a ppid
# walk (false for the container's own main command, which is how the mutation harness runs, so it
# refused all sixteen rows) and PID 1's starttime (blind to a sandbox that mounts a fresh /proc
# WITHOUT unsharing pid -- /proc/1 would still be tini while the mount table was bwrap's). Recording
# BOTH namespaces covers both assertions; a pid-only test guarding a mount-table check is the same
# mistake one axis over.
#
# `--bind / /` is recursive, so this file is readable from inside such a sandbox while its /proc is
# not the same -- which is exactly what makes the comparison discriminate.
#
# THE READ IS ns_pair, IN egress-lib.sh, SHARED WITH verify.sh -- one algorithm, covered by that
# library's own self-test, rather than this block and verify.sh's copy drifting apart. The values
# are collected BEFORE the file is touched, because `> "$MARKER"`
# truncates as soon as it is evaluated -- so writing through a failing pipeline would replace
# "nothing was recorded", which verify.sh refuses on, with "both namespaces are the empty string",
# which is a record making a claim. The directory is the seam the self-test injects, as
# JKB_EGRESS_VERDICT and JKB_REAPER are: there is no /proc/self/ns on the macOS host it runs on.
ns_pair "${JKB_NS_DIR:-/proc/self/ns}"
if [ -n "$NS_PID" ] && [ -n "$NS_MNT" ]; then
    printf 'pid=%s\nmnt=%s\n' "$NS_PID" "$NS_MNT" > "$JKB_NS_MARKER" || true
fi

exec "${JKB_REAPER:?the image must set JKB_REAPER (see the Dockerfile) — refusing to become PID 1 without a reaper}" -- "$@"
