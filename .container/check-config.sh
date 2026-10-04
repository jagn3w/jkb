#!/usr/bin/env bash
# Static checks on the dev container's configuration (design D49). No Docker required, so this is
# part of ./scripts/check.sh; the parts that need a container are verify.sh and mutate-verify.sh.
#
# What it is really guarding: the seccomp profile is GENERATED, and a generator whose patch
# silently no-ops against a changed upstream produces a profile that looks fine, applies fine, and
# leaves the nested sandbox unable to start. That failure is invisible until someone runs a
# command in a container.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
pass=0; fail=0
ok()  { pass=$((pass+1)); printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad() { fail=$((fail+1)); printf '  \033[31mFAIL\033[0m %s\n' "$1"; }

echo "==> container config"
command -v jq >/dev/null 2>&1 || { echo "   (skipped: jq not installed)"; exit 0; }

# THE `probe …` INVOCATIONS IN ci.yml, one per line, with `\`-continuations joined.
#
# ci.yml's bubblewrap ladder spells its flags in shell inside a `run:` block, and its arms are
# multi-line. A guard that greps the raw file cannot tell a flag ON an invocation from the same
# text in a comment, a label or an array assignment — which is exactly how the AppArmor-profile
# guard below came to read an assignment and stop establishing anything. Comment lines are dropped
# first, so a flag named in prose is never mistaken for one that is passed.
# COMMENT-STRIPPING, DEFINED BEFORE ANY USE. Bash resolves a function at call time, so using one
# above its definition is not a syntax error — it is an empty result, and an extraction that reads
# nothing looks exactly like a subject with nothing to find. That has now happened three times in
# this file with this very function, each caught only because the guard using it was pinned against
# an empty read. It lives at the TOP so the next guard cannot repeat it -- being first is the whole
# protection. A textual use-before-define check was considered and rejected: it cannot tell a
# top-level call from a call inside a function body (bash resolves at call time, so a helper naming
# a helper defined below it is correct), so it would redden this gate for correct code, which is
# the harm the pin below is written against.
dc_strip_comments() { sed 's/[[:space:]]#.*$//; s/^#.*$//' "$1"; }

# THE LINE NUMBER OF A `bash .container/<script>` STATEMENT in run.sh, as opposed to the same name
# appearing in a comment, a string or an array. DEFINED HERE, beside the other shared extractor,
# because the regex had been written out three times and the copy that single-sourced two of them
# was placed BELOW the third -- so the change that claimed to remove the duplication left it. The
# harm is prospective and specific: run.sh's reap line already uses `bash -lc '…'`, and the day
# verify.sh is invoked that way whoever widens the regex widens it at the site that went red and
# leaves the other, at which point this file prints `ok run.sh invokes verify.sh` beside a FAIL
# saying there is no verify.sh statement -- and the cheapest repair for the second is to delete the
# ordering branch that keeps the sweep from being disabled by an unrelated assertion.
dc_stmt_line() { # dc_stmt_line <stripped run.sh> <script basename regex> -> line number, or nothing
    # FROM THE KIT MIRROR: a statement running the checkout's copy is not the statement this asks for.
    grep -nE "^[[:space:]]*(in_container|docker exec)([[:space:]]+[^[:space:]]+)*[[:space:]]+(/usr)?(/bin/)?bash[[:space:]]+\"\\\$DC_CTR_KIT/\.container/$2" \
        <<<"$1" | sed -n '1s/^\([0-9]*\):.*/\1/p'
}

# NEVER PIPE A FILE-READER INTO `grep -q`, AND THIS IS NOT STYLE. `grep -q` exits at its FIRST
# match by design, so a producer still writing gets EPIPE -- and under `set -o pipefail` (line 9)
# that turns a SUCCESSFUL match into a FAILED pipeline. It is a race between how fast the producer
# writes and how early in the stream the match sits, so it passes on one machine and fails on
# another for the same bytes.
#
# That is not hypothetical: `dc_strip_comments run.sh | grep -qE '<the verify invocation>'` passed
# on macOS and failed on the CI runner with `sed: couldn't flush stdout: Broken pipe`, reporting
# "run.sh no longer runs verify.sh" about a line sitting at byte 23501 of 25810 -- the match is
# found, grep exits, and sed dies on the remaining 2.3KB. The guard did not merely misfire: it
# accused the file of the one thing it exists to prevent.
#
# Buffer size does not save it. The reader CLOSES the pipe; it is not a question of the 64KiB
# buffer filling. What is safe is a SINGLE-WRITE producer -- `printf "$var" | grep -q` completes
# its write before grep can exit -- so the rule is about multi-write producers, i.e. anything
# reading a FILE in chunks (`sed`, `grep`, `awk` over a path).
#
# Materialise first, match second. A here-string is ONE command, so pipefail has no second status
# to take. The rule lives here rather than at each call site, because a rule every call site must
# remember is itself the defect.
stripped_matches() { # stripped_matches <file> <extended-regex> -> 0 if the stripped file matches
    local text
    text="$(dc_strip_comments "$1")" || return 2
    grep -qE "$2" <<<"$text"
}
file_matches() { # file_matches <file> <grep args...> -> 0 if the raw file matches, no pipe
    local f="$1"; shift
    local text; text="$(cat "$f" 2>/dev/null)" || return 2
    grep -q "$@" <<<"$text"
}

# Sourced HERE rather than 80 lines down, so this file has one copy of the comment-stripping rule
# instead of a verbatim `strip()` beside the `dc_strip()` it later sources — two halves of one file
# parsing the same input through two copies that can disagree.
# shellcheck source=/dev/null
. "$here/lib.sh"
if dc_strip "$here/container.json" | jq empty 2>/dev/null; then ok "container.json parses"
else bad "container.json does not parse"; fi

dc="$(dc_strip "$here/container.json")"
for want in '"remoteUser": "vscode"' '--cap-add=NET_ADMIN'; do
    if grep -qF -e "$want" <<<"$dc"; then ok "declares $want"
    else bad "container.json no longer declares $want"; fi
done

# THE ONE EXTRACTION of "which values does runArgs pair with --security-opt". This was spelled
# three times within seventy lines — two near-verbatim jq `to_entries|any` blocks and this
# `range/select` — in the file that fails the build when `dc_require_apparmor_profile` or
# `dc_apparmor_mediates` gains a second definition. The next `--security-opt` the container
# declares would have needed a fourth spelling.
#
# ADJACENCY, not membership: an orphaned value reads as a declaration and applies nothing. Delete
# the `--security-opt` flag and leave `seccomp=…` behind, and Docker applies its DEFAULT profile —
# whose `mount` denial is what bubblewrap dies on — while the file still reads as declaring one.
# Found by mutate-config.sh, which is why this is a pair check and not a grep for the value.
declared_pairs="$(jq -r '[.runArgs // [] | .[]] as $a
                         | range(0; ($a | length))
                         | select($a[.] == "--security-opt")
                         | $a[.+1] // empty' <<<"$dc" 2>/dev/null)"
n_declared="$(printf '%s\n' "$declared_pairs" | grep -c . || true)"
if [ "$n_declared" -eq 0 ]; then
    bad "no --security-opt pairs could be read out of container.json's runArgs — every check over them below would pass having compared nothing"
fi
declares_security_opt() { grep -qxF -- "$1" <<<"$declared_pairs"; }

# WHAT IS DECLARED is a different question from what the control carries, and both are asked. This
# one goes red when the profile is removed from container.json; the control guard below cannot,
# because a declaration-less file passes it vacuously (the control would derive nothing to be
# missing). Sharing the extraction was the fix; merging the questions would have deleted a check.
if declares_security_opt "seccomp=\${localWorkspaceFolder}/.container/seccomp-bwrap.json"; then
    ok "runArgs pairs --security-opt with the seccomp profile"
else
    bad "container.json does not pair --security-opt with seccomp=\${localWorkspaceFolder}/.container/seccomp-bwrap.json — Docker would apply its default profile and bubblewrap could not start"
fi

# `systempaths=unconfined` is the second half of what bubblewrap needs, and it is separate from
# seccomp: Docker's masked /proc paths are SUBMOUNTS, so /proc is not "fully visible" and the
# kernel refuses a fresh proc mount inside a user namespace whatever the syscall filter allows.
# Without it the nested sandbox cannot start at all, and because the posture fails closed that
# surfaces as Bash erroring rather than as anything naming this flag.
if declares_security_opt "systempaths=unconfined"; then
    ok "runArgs pairs --security-opt with systempaths=unconfined"
else
    bad "container.json does not pair --security-opt with systempaths=unconfined — Docker's masked /proc paths would make the kernel refuse bubblewrap's proc mount, so Claude Code's nested sandbox could not start"
fi

# Non-root is load-bearing (root cannot create a mount namespace in a container), so a
# `"remoteUser": "root"` would break the nested sandbox while looking like a simplification.
if grep -q '"remoteUser": *"root"' <<<"$dc"; then bad "remoteUser is root — the nested sandbox cannot start"; fi

# THE CONTROL RUNS WHAT THE DECLARATION DECLARES — and this is no longer checked here, because
# there is no longer anything to check (D54.1).
#
# mutate-verify.sh used to re-derive its control's flags from container.json with the same readers
# run.sh uses, one file apart, and two guards lived here to keep the copies agreeing: a per-reader
# contiguous-block comparison, and a pin that no fourth reader could join the assembly without
# joining the table. Between them they produced five findings and three must-fixes across four
# review rounds — the comparison covered only the security-opt pairs, then only the runArgs third;
# the pin read a hand-picked source region, then matched only one spelling of its argument. Each
# fix was right and the next round found the next hole, because a guard over two copies cannot be
# complete: it has to enumerate what to compare, and whatever it fails to enumerate reads as
# agreement.
#
# The control asks `run.sh --print-args --posture` now. One derivation, no agreement to guard, so
# these are deleted rather than corrected a fourth time. What replaces them is not a better static
# check but different evidence, in two places that already existed:
#
#   * verify.sh asserts the DECLARATION'S EFFECTS from inside the running container — the /proc
#     unmask, the user, the environment — deriving each from container.json rather than from the
#     control's flag list. CI runs it on every push through `mutate-verify.sh --control`, so a
#     control that drifts from the declaration on any reader is red in the harness's own control.
#   * run.sh refuses to print a partial or empty assembly and mutate-verify.sh refuses an empty
#     one, so a truncated read cannot be certified as a container.
#
# What is still checked here about that script is its expectation strings (further down): those
# name lines verify.sh must print, and a stale one is MISSED-for-ever on a harness that needs
# Docker and therefore cannot run in this gate.
root="$(cd "$here/.." && pwd)"

# THE POSTURE HALF OMITS THE INSTANCE HALF (D54.1). This is the one property keeping the harness's
# containers off the real ~/repos and ~/.jkb -- mutate-verify.sh appends `--print-args --posture`
# straight into the control, so an instance flag leaking into it means every mutation runs
# bind-mounting the LIVE knowledge base and colliding on the live container's name. It was argued
# in a comment ("A mode cannot fail that way") and checked nowhere.
#
# ASKED BY RUNNING BOTH HALVES, not by reading docker_args' source. The property is about what the
# program emits; its `[ "$half" != posture ]` gates are one way to implement that and a third
# emission site outside a gate would satisfy any grep over them.
#
# BOTH DIRECTIONS, because the negative alone is vacuous: a docker_args that emitted no mounts at
# all would pass "posture carries none" while silently breaking the launcher. So the `all` half
# must carry each instance flag and the `posture` half must carry none of it -- the contrasting
# case that makes the check discriminate rather than merely not-fail.
INSTANCE_FLAGS='^--name$|^--detach$|^--workdir$|^--mount$'
pa_all="$("$here/run.sh" --print-args "$root" 2>/dev/null)" || pa_all=""
pa_posture="$("$here/run.sh" --print-args --posture "$root" 2>/dev/null)" || pa_posture=""
if [ -z "$pa_all" ] || [ -z "$pa_posture" ]; then
    bad "run.sh --print-args produced nothing for one or both halves — nothing establishes that the control the harness derives omits this host's mounts"
else
    n_all="$(grep -cE "$INSTANCE_FLAGS" <<<"$pa_all" || true)"
    leaked="$(grep -E "$INSTANCE_FLAGS" <<<"$pa_posture" | sort -u | tr '\n' ' ' || true)"
    if [ "$n_all" -eq 0 ]; then
        bad "run.sh --print-args emits no instance flag at all — the launcher would start a container with no name, no workdir and no mounts, and the posture check below would pass having compared nothing"
    elif [ -n "$leaked" ]; then
        bad "run.sh --print-args --posture carries instance flag(s) the harness must not inherit: $leaked — mutate-verify.sh appends this to its control, so every mutation would bind-mount the real ~/.jkb"
    else
        ok "the posture half carries none of the $n_all instance arguments the full half does"
    fi
fi

# The whole point of the profile: these must be unconditionally allowed. Checked against the
# generator's own list so the two cannot drift.
prof="$here/seccomp-bwrap.json"
if jq empty "$prof" 2>/dev/null; then ok "seccomp profile parses"
else bad "seccomp profile does not parse"; fi
needed="$(grep -o '"[a-z0-9_]*",' "$here/generate-seccomp.sh" | tr -d '",' | sort -u)"
missing=()
while IFS= read -r sc; do
    [ -n "$sc" ] || continue
    jq -e --arg s "$sc" 'any(.syscalls[]; .action == "SCMP_ACT_ALLOW" and (.names // [] | index($s)) and (.args // [] | length) == 0)' \
        "$prof" >/dev/null 2>&1 || missing+=("$sc")
done <<<"$needed"
if [ ${#missing[@]} -eq 0 ]; then
    ok "every syscall the generator names is unconditionally allowed ($(grep -c . <<<"$needed"))"
else
    bad "seccomp profile does not unconditionally allow: ${missing[*]} — regenerate it"
fi

# ...and the NEGATIVE half, which is the half that can actually fail. The generator appends one
# unconditional allow group, so "is it allowed somewhere" is true by construction and would stay
# true if the removal loop silently matched nothing against a changed upstream. What proves the
# loop ran is that no OTHER entry still names these syscalls under a restriction.
still_restricted=()
while IFS= read -r sc; do
    [ -n "$sc" ] || continue
    jq -e --arg s "$sc" \
        'any(.syscalls[]; (.names // [] | index($s)) and (.action != "SCMP_ACT_ALLOW" or ((.args // []) | length) > 0))' \
        "$prof" >/dev/null 2>&1 && still_restricted+=("$sc")
done <<<"$needed"
if [ ${#still_restricted[@]} -eq 0 ]; then
    ok "no restricted entry still names them (the removal loop ran)"
else
    bad "the removal loop missed: ${still_restricted[*]} — a restricted entry still matches, so the allow is shadowed"
fi

# Both assertions above loop over `needed`, and an empty or truncated loop reports success — so
# the extraction itself has to be checked. Named members, not a count: a threshold passes while
# silently losing names under it (10 of 14 cleared ">= 10" in testing), whereas losing `unshare`
# or `pivot_root` is losing the two the container demonstrably cannot start without.
missing_core=()
for core in clone unshare mount pivot_root; do
    grep -qx "$core" <<<"$needed" || missing_core+=("$core")
done
if [ ${#missing_core[@]} -eq 0 ]; then
    ok "the generator's syscall list parsed and names the load-bearing calls"
else
    bad "generate-seccomp.sh's list no longer yields: ${missing_core[*]} — the checks above are vacuous"
fi

# Every mount point the container declares, derived the same way verify.sh derives it. verify.sh
# no longer keeps a hand-written copy of this list — a transcribed one went stale the moment the
# mounts changed, dropping the cargo registry and failing a correctly-built container — so what is
# checked here is that the DERIVATION still yields the mounts the container cannot work without.
# An empty or truncated result would make verify.sh's boundary assertion meaningless.
mount_targets="$(dc_mount_targets "$here/container.json")"
missing_mounts=()
for m in /home/vscode/repos /home/vscode/.jkb/claude-memory /home/vscode/.jkb/logs /home/vscode/.jkb-container; do
    grep -qx "$m" <<<"$mount_targets" || missing_mounts+=("$m")
done
if [ ${#missing_mounts[@]} -eq 0 ]; then
    ok "the mount list parses and declares the load-bearing mounts ($(grep -c . <<<"$mount_targets") targets)"
else
    bad "the declared mount set is missing ${missing_mounts[*]} — verify.sh derives its boundary from this list"
fi

# EVERY TOP-LEVEL KEY of container.json is applied by something. This replaces the whole
# workspaceFolder/initializeCommand family of checks, which existed because Dev Containers decided
# where the container opened and could only express a BASENAME — a limitation that is gone now that
# you attach to the container and open any path inside it.
#
# What replaces it is the risk the rename introduces: this file is no longer read by VS Code, so
# nothing applies it except our own run.sh. A key added here that run.sh does not read is a
# declaration that does nothing while looking exactly like configuration — and the key most likely
# to be added is another `mounts`-shaped one, which is the security boundary. run.sh names what it
# reads (`--consumed-keys`) rather than this file guessing, so the two cannot disagree.
declared_keys=(); unread_keys=()
while IFS= read -r k; do [ -n "$k" ] && declared_keys+=("$k"); done < <(jq -r 'keys[]' <<<"$dc" 2>/dev/null)
consumed="$("$here/run.sh" --consumed-keys 2>/dev/null)"
for k in ${declared_keys[@]+"${declared_keys[@]}"}; do
    grep -qxF -e "$k" <<<"$consumed" || unread_keys+=("$k")
done
# PINNED AGAINST AN EMPTY EXTRACTION on BOTH sides, like the derived lists above. Either one
# yielding nothing makes the loop vacuous: no declared keys means nothing is checked, and no
# consumed list would instead report every key as unread, which is a different lie.
if [ ${#declared_keys[@]} -eq 0 ]; then
    bad "no top-level keys could be read out of container.json — this check just certified nothing"
elif [ -z "$consumed" ]; then
    bad "run.sh --consumed-keys printed nothing — this check cannot tell an applied key from an ignored one"
elif [ ${#unread_keys[@]} -eq 0 ]; then
    ok "every key in container.json is applied by run.sh (${#declared_keys[@]} checked)"
else
    bad "container.json declares ${unread_keys[*]} which run.sh does not read — nothing applies it; add it to consumed_keys() and to docker_args(), or remove it"
fi

# DERIVING THE EXPECTED SET MADE THE BOUNDARY SELF-CERTIFYING, and this is the other half of it.
# verify.sh can now only answer "does the running container match what it declares"; adding a
# mount to container.json makes it declared, so the runtime check would accept the two mounts
# verify.sh's own comment names as the reason it exists. What must still be answered is "is the
# declaration acceptable", and that belongs here, in the gate a human reads in a diff.
forbidden=()
while IFS= read -r t; do
    [ -n "$t" ] || continue
    case "$t" in
        # ~/.claude holds settings.json, which IS the posture. A process the posture bounds must
        # not read or write the file deciding whether it is bounded — at that path or under it.
        /home/vscode/.claude|/home/vscode/.claude/*) forbidden+=("$t (inside the posture's own directory)") ;;
        */docker.sock)                               forbidden+=("$t (the docker socket is root on the host)") ;;
    esac
done <<<"$mount_targets"
# ...and the source side, which the target cannot show: a bind may carry any host path in under an
# innocuous name. Volumes are container-managed and reach no host filesystem, so only binds count.
while IFS= read -r src; do
    [ -n "$src" ] || continue
    case "$src" in
        '${localEnv:HOME}/repos'|'${localEnv:HOME}/.jkb/claude-memory'|'${localEnv:HOME}/.jkb/logs'|'${localEnv:HOME}/.jkb-container') ;;
        # The whole of ~/.jkb carries the operator's database, its backups and the daemon's root
        # token: anything in the container could read the token that makes it the operator (D52.8).
        '${localEnv:HOME}/.jkb'|'${localEnv:HOME}/.jkb/') forbidden+=("host source $src (the whole ~/.jkb: the operator's database and root token, D52.8)") ;;
        '${localEnv:HOME}/.jkb/'*) forbidden+=("host source $src (only claude-memory and logs of ~/.jkb are reviewed, D52.8)") ;;
        *) forbidden+=("host source $src (not on the reviewed bind allowlist)") ;;
    esac
done <<<"$(dc_mount_sources "$here/container.json" | sed -n '/|volume$/!s/|[^|]*$//p')"
# ...and certify the source derivation produced something, the way the target list is certified
# above. An include-match on `|bind` skipped any other type spelling entirely, and an empty result
# made "every declared mount is acceptable" pass with the host's ~/.ssh bound in — a guard that
# fails OPEN. Excluding volumes instead means an unrecognised type is reviewed, not waved through.
bind_sources="$(dc_mount_sources "$here/container.json" | sed -n '/|volume$/!s/|[^|]*$//p' | grep -c .)"
if [ "$bind_sources" -lt 4 ]; then
    bad "only $bind_sources host bind source(s) parsed — the workspace, ~/.jkb's claude-memory and logs, and the container credential are all binds, so the review below saw less than the config declares"
fi
# THE CREDENTIAL IS READ-ONLY (D52.3). A writable bind would let anything in the container replace the
# credential the host's hooks and clients read — with a token it minted for itself, say.
cred_spec="$(dc_mount_specs "$here/container.json" | grep -F 'target=/home/vscode/.jkb-container' || true)"
case ",$cred_spec," in
    *,readonly,*|*,ro,*) ok "the container credential is bound read-only" ;;
    *) bad "the container credential mount is not read-only: ${cred_spec:-(missing)}" ;;
esac
if [ ${#forbidden[@]} -eq 0 ]; then
    ok "every declared mount is acceptable (no posture directory, no docker socket, binds from the reviewed set)"
else
    for f in "${forbidden[@]}"; do bad "container.json declares a mount that must not exist: $f"; done
fi

# CARGO_TARGET_DIR is named in three files that cannot reference one another (JSON has no
# variables), and it was already wrong once: it sat BESIDE the allowlisted ~/.cargo rather than
# under it, so denyRead blanketed every sandboxed build while both runtime guards reported the
# container healthy. The rule is generic — the path every site names must be the same one, and it
# must fall under a posture write root — so a future edit to any single site is caught here rather
# than by a build dying inside a container.
# EVERY named volume's target must be pre-created in the Dockerfile, not just CARGO_TARGET_DIR's.
# The rule was checked for that one path, and the next volume added (jkb-kb-local, for the
# container-local knowledge base, since retired) skipped it: Docker created it root-owned and `jkb` could not
# create its database. A rule checked for one call site is a rule the next one forgets.
precreated="$(awk '/^RUN mkdir -p /{on=1} on{print} on&&!/\\$/{on=0}' "$here/Dockerfile" \
    | tr ' \\' '\n\n' | grep '^/' | sort -u)"
unprecreated=()
while IFS= read -r t; do
    [ -n "$t" ] || continue
    [ "$(dc_type_for_target "$here/container.json" "$t")" = volume ] || continue
    grep -qx "$t" <<<"$precreated" || unprecreated+=("$t")
done <<<"$mount_targets"
if [ ${#unprecreated[@]} -eq 0 ]; then
    ok "every named volume's target is pre-created in the Dockerfile"
else
    bad "Dockerfile does not pre-create volume target(s) ${unprecreated[*]} — Docker creates them root-owned and the container user cannot write them"
fi

posture="$here/../scripts/auto-mode-posture.json"
user="$(jq -r '.remoteUser // "root"' <<<"$dc")"
home="/home/$user"
target="$(jq -r '.containerEnv.CARGO_TARGET_DIR // ""' <<<"$dc")"
if [ -z "$target" ]; then
    bad "container.json sets no containerEnv.CARGO_TARGET_DIR — cargo would write into the bind mount"
else
    sites_ok=1
    # Declared AND a volume. Rewriting this to use the derived target list dropped the
    # `type=volume` half, leaving a check whose own failure message is about volumes but which a
    # plain bind satisfies — and a bind is the case that breaks: it carries the host's uids, so
    # where the host uid is not 1000 the build dies with EACCES minutes in.
    grep -qx "$target" <<<"$mount_targets" || { bad "nothing is mounted at CARGO_TARGET_DIR ($target) — a named volume whose path the image lacks is created root-owned"; sites_ok=0; }
    [ "$(dc_type_for_target "$here/container.json" "$target")" = volume ] \
        || { bad "CARGO_TARGET_DIR ($target) is not declared type=volume — a bind mount carries the host's uids and the build dies with EACCES"; sites_ok=0; }
    grep -qF "mkdir -p $target" "$here/Dockerfile" || { bad "Dockerfile does not pre-create $target — Docker seeds volume ownership from the image, so this is what stops EACCES"; sites_ok=0; }
    [ "$sites_ok" -eq 1 ] && ok "every site names the same CARGO_TARGET_DIR ($target)"

    # `~` in the posture is the container user's home. Match at a component boundary: `~/.cargo`
    # must not be read as covering `~/.cargo-target`, which is the exact mistake being guarded.
    covered=0
    while IFS= read -r entry; do
        [ -n "$entry" ] || continue
        root="${entry/#\~/$home}"
        case "$target" in "$root"|"$root"/*) covered=1; break ;; esac
    done < <(jq -r '.require.sandbox.filesystem.allowWrite[]?' "$posture" 2>/dev/null)
    if [ "$covered" -eq 1 ]; then
        ok "CARGO_TARGET_DIR falls under a posture allowWrite root"
    else
        bad "CARGO_TARGET_DIR ($target) is under no allowWrite root — sandboxed builds in the container will be denied"
    fi
fi

# The firewall reads the SAME allowlist the sandbox posture uses. If that path or key moves, the
# firewall silently allowlists nothing and default-denies everything, which reads as "very secure"
# right up until nothing works.
if jq -e '.require.sandbox.network.allowedDomains | length > 0' "$here/../scripts/auto-mode-posture.json" >/dev/null 2>&1; then
    ok "the firewall's allowlist key exists in the posture"
else
    bad "posture has no .require.sandbox.network.allowedDomains — the firewall would deny everything"
fi

# The firewall is the layer that holds when the nested sandbox does not, so the party it bounds
# must not be able to choose what it enforces. Two halves, and BOTH are needed: a sudoers command
# naming no argument accepts every argument, so pinning it to none is what stops any readable JSON
# being passed; and the script must refuse an argument rather than merely ignore one, or the two
# statements disagree about which is authoritative.
if grep -qF 'init-firewall.sh ""' "$here/Dockerfile"; then
    ok "sudoers grants init-firewall.sh with no arguments permitted"
else
    bad "the sudoers grant does not pin the argument list — any readable JSON path would be accepted as the allowlist"
fi
# THE SECOND GRANT IS SUBJECT TO THE SAME RULE (D51.1). A command naming no argument accepts every
# argument, and this one runs as root — so it is pinned the same way, and egress-status.sh refuses
# any argument itself. Both halves, because either alone is a rule with one enforcer.
if grep -qE 'egress-status\.sh ""' "$here/Dockerfile" 2>/dev/null; then
    ok "sudoers grants egress-status.sh with no arguments permitted"
else
    bad "the sudoers entry for egress-status.sh no longer pins it to no arguments — a command naming no argument accepts every argument, and this one runs as root"
fi
# THE THIRD GRANT, THE SAME RULE (D52.9): pin-jkb-hook.sh installs the binary the harness hooks run
# outside the sandbox, so it takes no argument — its source is fixed in the script.
if grep -qE 'pin-jkb-hook\.sh ""' "$here/Dockerfile" 2>/dev/null; then
    ok "sudoers grants pin-jkb-hook.sh with no arguments permitted"
else
    bad "the sudoers entry for pin-jkb-hook.sh no longer pins it to no arguments — it installs, as root, the binary the harness hooks run"
fi
# Every managed hook names the pinned binary absolutely. A bare `jkb` is found on PATH, and
# ~/.cargo/bin is on PATH and writable from inside the sandbox — so the hooks, which run outside it
# with the container credential readable, ran whatever a tool call last put there.
hook_cmds="$(jq -r '.hooks[][].hooks[].command' "$here/managed-settings.json" 2>/dev/null)"
if [ -z "$hook_cmds" ]; then
    bad "no hook commands could be read from managed-settings.json — the check that they run the pinned binary examined nothing"
elif grep -qvE '^(/usr/local/lib/jkb-hook/jkb |/usr/local/bin/deny-transcripts\.sh$)' <<<"$hook_cmds"; then
    # TWO PINNED PATHS, NOT A RELAXED PATTERN. The rule is "absolute, root-owned, and not writable
    # from inside the sandbox", and both satisfy it: the Dockerfile installs each --chown=root:root
    # and the sandbox's allowOnly write list reaches neither. deny-transcripts.sh joined the list
    # when the transcript deny moved out of permissions.deny and into a hook, because a
    # `Read(...*.jsonl)` glob is named per-matching-file in the bubblewrap argv and blew it. A
    # THIRD entry here should be suspicious: each one is a program that runs outside the sandbox
    # with the container credential readable.
    bad "a managed hook runs neither pinned root-owned program — one found on PATH is replaceable from the sandbox: $(grep -m1 -vE '^(/usr/local/lib/jkb-hook/jkb |/usr/local/bin/deny-transcripts\.sh$)' <<<"$hook_cmds")"
else
    ok "every managed hook runs one of the two pinned, root-owned programs (jkb, deny-transcripts.sh)"
fi
# THE TWO THINGS A DENY RULE OVER THE TRANSCRIPT TREE CAN BREAK, and they pull in OPPOSITE
# directions. This is one check because treating either alone produces the other's failure.
#
#   ARGV. Claude Code compiles `permissions.deny` into the bubblewrap argv for the Bash sandbox.
#   A rule ending in a directory wildcard COLLAPSES -- `Read(~/.ssh/**)` becomes the single path
#   `~/.ssh`, as it does for ~/.aws, ~/Documents, ~/.pki and ~/.jkb-container. A rule ending in a
#   FILE pattern cannot: the sandbox names every match and binds /dev/null over each, so the argv
#   grows by one path per file on disk. Measured 2026-09-30, after a sweep had already run: the
#   two transcript rules expanded to 206 paths, 33,819 bytes per spelling, 67,638 across both,
#   against a MAX_ARG_STRLEN of 131,072 Linux does not let you raise -- 52% of the ceiling. Past
#   it EVERY Bash call in the container dies at spawn with E2BIG, including `:`, with nothing in
#   the message naming transcripts.
#
#   MEMORY. Claude Code keeps auto-memory at ~/.claude/projects/<slug>/memory/. That location is
#   not ours to choose, `scripts/link-claude-memory.sh` exists to put the link there, and
#   verify.sh FAILS when it is missing. A subtree rule over `projects/**` covers it -- and denied
#   memory does not error, it goes QUIET: MEMORY.md stops arriving in context, which reads like an
#   agent that forgot rather than a broken container.
#
# SO NO GLOB CAN BE THE TRANSCRIPT DENY, and that is a finding, not an oversight. `projects/**`
# collapses the argv to ~50 bytes and swallows auto-memory (verify.sh's memory_shadow rows caught it
# on both spellings); a bare `projects` does the same, because Claude Code applies a directory rule
# to its subtree (measured in a rebuilt container); and the per-file `**/*.jsonl` that spares memory
# is the O(files) term this whole check exists to refuse. So the transcript deny is a PreToolUse
# hook, .container/deny-transcripts.sh, and no rule here names the tree at all. The only named
# exceptions below are the `~/repos/**` Edit rules, which are a different, measured term. (This
# paragraph said the transcript rules "MUST end in a file pattern" and were the exception -- true
# of the design the branch tried first, and false of the one it shipped. Review round 2.)
# THE ONE DENY-RULE READER, loaded from sweep-transcripts.sh by name -- not a copy. This block used
# to parse rules itself (sed, `~` only) while verify.sh parsed them with jq and the sweep a third
# way, and each was wrong differently: this one missed Claude Code's absolute `//path` spelling, so
# `Read(//home/vscode/.claude/projects)` -- the exact bare-directory rule that silently drops
# MEMORY.md -- passed as clear. A rename in the sweep makes these calls "command not found" and the
# checks below fail loudly, which is the direction a shared definition must fail in.
# EVERY posture_* FUNCTION, by one pattern -- not a hand-kept list of names. The list form missed
# posture_canon when posture_rule_path began calling it: posture_rule_path then printed nothing, an
# empty pattern "covered" everything, and every rule read as swallowing auto-memory. Loud that time;
# the same miss elsewhere could be quiet. The `declare -F` guard below names what this file USES.
eval "$(sed -n '/^posture_[a-z_]*() {/,/^}/p' "$here/sweep-transcripts.sh")"
dc_reader_ok=0
declare -F posture_canon posture_deny_rules posture_rule_is_path posture_rule_path posture_rule_covers posture_rule_expands posture_rule_base posture_hook_matcher posture_transcript_roots >/dev/null && dc_reader_ok=1
# BOTH DENY LISTS, FROM BOTH FILES, through the one emitter: the image's managed settings, and the
# posture's `require` block that auto-mode writes into user settings. Each line is
# "<settings-dir><TAB><rule>", because a one-slash permission rule is relative to the file it is in.
# This read permissions.deny from the managed file alone, so a per-file transcript glob in
# sandbox.filesystem.denyRead -- which reaches the same argv -- passed (review round 4, reproduced).
dc_deny_raw=""
if [ "$dc_reader_ok" = 1 ]; then
    # "<settings-dir><TAB><list><TAB><rule>": `perm` lines govern the file tools, `argv` lines are
    # everything that reaches bubblewrap. The memory arm reads `perm`; the expansion arm reads `argv`.
    # The tab is printf's, so the separator is visible in the source.
    dc_tab="$(printf '\t')"
    dc_deny_raw="$( { posture_deny_rules "$here/managed-settings.json" . perm | sed "s|^|/etc/claude-code${dc_tab}perm${dc_tab}|"
                     posture_deny_rules "$posture" .require perm | sed "s|^|/home/vscode/.claude${dc_tab}perm${dc_tab}|"
                     posture_deny_rules "$here/managed-settings.json" . all | sed "s|^|/etc/claude-code${dc_tab}argv${dc_tab}|"
                     posture_deny_rules "$posture" .require all | sed "s|^|/home/vscode/.claude${dc_tab}argv${dc_tab}|"; } 2>/dev/null)"
fi
if [ "$dc_reader_ok" != 1 ]; then
    bad "the deny-rule reader could not be loaded from sweep-transcripts.sh, so nothing below can say whether a rule blows the argv or swallows auto-memory"
# THE MANAGED FILE'S OWN RULES, not the merged list: once the posture's rules joined it, the merged
# list was never empty, and an unreadable managed-settings.json passed in silence. The mutation
# harness caught that the same round it was introduced.
elif ! grep -q "^/etc/claude-code$(printf '\t')perm$(printf '\t')" <<<"$dc_deny_raw"; then
    bad "no permissions.deny rules could be read from managed-settings.json — the checks that none of them blows the argv or swallows auto-memory examined nothing"
else
    # Rules are read as the INSTALLED file reads them: home is the container's, and a single
    # leading `/` is relative to /etc/claude-code, where the image puts this file -- not $HOME,
    # which on CI is the runner's.
    # THE KNOWN EXPANDERS, named rather than tolerated by shape. These predate this guard and
    # keep agents from editing a repo's harness configuration; each `**` sits MID-PATH, so the
    # sandbox enumerates one argv entry per match on disk -- and every task worktree adds a set.
    # That is an O(worktrees) term, bounded by how many worktrees exist, measured in
    # .container/README.md. A new rule of this shape fails until it is added here, deliberately.
    dc_known_expanders='Edit(~/repos/**/.claude/settings.json)
Edit(~/repos/**/.claude/settings.local.json)
Edit(~/repos/**/.claude/hooks/**)
Edit(~/repos/**/.claude/agents/**)
Edit(~/repos/**/.claude/skills/**)
Edit(~/repos/**/.claude/workflows/**)
Edit(~/repos/**/.mcp.json)'
    dc_mem_hit=""; dc_expanding=""
    while IFS=$'\t' read -r dc_dir dc_list dc_rule; do
        [ -n "$dc_rule" ] || continue
        # Only FILE rules have paths: `Bash(curl:*)` fed through path semantics read as an expander.
        posture_rule_is_path "$dc_rule" || continue
        # No relative base: the managed file and the posture are not project layers, so a relative
        # rule in either meets a session's cwd -- a repo -- and covers no absolute path (round 9,
        # which reversed round 8's home reading here; verify.sh's memory arm reads it the same way).
        dc_pat="$(posture_rule_path "$dc_rule" /home/vscode "$dc_dir")"
        # 1. Nothing may cover auto-memory, with Claude Code's subtree semantics. Two spellings of
        #    the tree, one synthetic slug: no rule names a slug, so a probe answers for every repo.
        [ "$dc_list" = perm ] && for dc_probe in /home/vscode/.claude/projects/-probe-repo/memory/MEMORY.md \
                        /home/vscode/.claude-state/projects/-probe-repo/memory/MEMORY.md; do
            posture_rule_covers "$dc_pat" "$dc_probe" && { dc_mem_hit="$dc_mem_hit $dc_rule"; break; }
        done
        # 2. Nothing may be enumerated per match unless it is a named, measured exception.
        if [ "$dc_list" = argv ] && posture_rule_expands "$dc_pat" && ! grep -qxF -- "$dc_rule" <<<"$dc_known_expanders"; then
            dc_expanding="$dc_expanding $dc_rule"
        fi
    done <<<"$dc_deny_raw"
    if [ -n "$dc_mem_hit" ]; then
        bad "a deny rule covers ~/.claude/projects/<slug>/memory, so MEMORY.md stops reaching context with no error anywhere:$dc_mem_hit
       Auto-memory's location is Claude Code's, not ours, and verify.sh FAILS when it is not
       linked there — so a subtree rule over the transcript tree cannot also be the argv fix."
    elif [ -n "$dc_expanding" ]; then
        bad "a deny rule is enumerated per match — a file pattern at the end, or a ** mid-path — so the Bash sandbox must name every matching path and the argv grows with the tree until every Bash call in the container dies at spawn:$dc_expanding
       Only two shapes collapse to one argv entry: no wildcard, or a single trailing \`/**\` on a
       literal prefix. If it has to spare a sibling the way the transcript deny spares auto-memory,
       a glob cannot express that — make it a PreToolUse hook, as .container/deny-transcripts.sh
       is. .container/README.md carries the measurements."
    else
        ok "no deny rule (managed, or in the posture) swallows auto-memory, and none is enumerated per match beyond the $(grep -c . <<<"$dc_known_expanders") named ~/repos/** rules"
    fi
fi

# AND THE HOOK THAT REPLACED THEM MUST ACTUALLY BE WIRED. With the globs gone, deny-transcripts.sh
# is the ONLY thing stopping a file tool reading another session's transcript -- there is no
# permissions rule for the tree at all, because any rule broad enough to cover the transcripts
# covers auto-memory too (the memory arm above refuses it). Deleting the hook entry, or the COPY that installs
# it, leaves every gate here green and the confidentiality boundary simply absent. Three things
# have to hold together, so all three are asked: it is referenced, it is installed, and it is
# installed root-owned (a hook the sandbox can rewrite is a hook the agent controls).
dc_hook=/usr/local/bin/deny-transcripts.sh
if ! jq -e --arg h "$dc_hook" '[.hooks.PreToolUse[]?.hooks[]?.command] | index($h)' \
        "$here/managed-settings.json" >/dev/null 2>&1; then
    bad "managed-settings.json no longer runs $dc_hook as a PreToolUse hook — with the transcript globs gone, nothing else keeps a file tool out of another session's transcript"
# STRIPPED AND ANCHORED: a raw grep matched `# COPY --chown=root:root deny-transcripts.sh ...` and
# passed an image that ships without the hook. Review round 2.
elif ! stripped_matches "$here/Dockerfile" "^COPY --chown=root:root deny-transcripts\.sh ${dc_hook//./\\.}([[:space:]]|\$)"; then
    bad "the Dockerfile does not install deny-transcripts.sh root-owned at $dc_hook — the hook managed-settings.json names is either missing or writable by the agent it confines"
elif [ ! -f "$here/deny-transcripts.sh" ]; then
    bad "there is no .container/deny-transcripts.sh to install, so the transcript deny does not exist"
else
    # THE MATCHER MUST BE EVERY TOOL. An allowlist of file tools let built-ins it did not name --
    # Artifact, which reads a local file and uploads it -- skip the hook entirely (review round 3),
    # and a substring check on that list had already let `Edit` hide inside `NotebookEdit`
    # (round 1). One shape now, from one shared definition, compared exactly.
    dc_match="$(jq -r --arg h "$dc_hook" '.hooks.PreToolUse[]? | select([.hooks[]?.command] | index($h)) | .matcher' "$here/managed-settings.json" 2>/dev/null)"
    # ...AND ITS OWN DEADLINE ENDS BEFORE CLAUDE CODE'S TIMEOUT: the hook refuses a call it could not
    # judge in dt_deadline seconds, but Claude Code kills it at the managed timeout and then lets the
    # call through -- so a timeout at or under the deadline fails open (review round 27).
    dc_tmo="$(jq -r --arg h "$dc_hook" '[.hooks.PreToolUse[]? | .hooks[]? | select(.command == $h) | .timeout] | first // empty' "$here/managed-settings.json" 2>/dev/null)"
    dc_dl="$(sed -n 's/^    dt_deadline=\([0-9][0-9]*\)$/\1/p' "$here/deny-transcripts.sh" | head -1)"
    if [ "$dc_match" != "$(posture_hook_matcher)" ]; then
        bad "the transcript hook's matcher is [$dc_match], not [$(posture_hook_matcher)] — any tool it does not match never reaches the hook, and can read or upload another session's transcript"
    # An INTEGER timeout, or `[ -ge ]` errors, the elif is skipped and this reads as fine (round 28).
    elif ! [[ "$dc_tmo" =~ ^[0-9]+$ ]] || [ -z "$dc_dl" ] || [ "$dc_dl" -ge "$dc_tmo" ]; then
        bad "the transcript hook's own deadline [${dc_dl:-not found}s] does not end before its managed timeout [${dc_tmo:-not set}s] — Claude Code would kill it first and let the call through"
    # ...AND IT KNOWS WHERE IT IS INSTALLED: the hook trusts its environment and its test seams only when
    # it is NOT at DT_INSTALLED_PATH, so a hook command elsewhere would run the live hook in test mode
    # (review round 32).
    elif [ "$(sed -n 's/^DT_INSTALLED_PATH=\(.*\)$/\1/p' "$here/deny-transcripts.sh" | head -1)" != "$dc_hook" ]; then
        bad "the transcript hook's DT_INSTALLED_PATH is not the managed hook command [$dc_hook] — the live hook would take itself for a test copy, trusting its environment and its self-test seams"
    else
        ok "the transcript deny is a wired, root-owned hook that every tool call reaches"
    fi
fi
# THE SANDBOX IS PINNED ON IN THE IMAGE'S MANAGED SETTINGS (the user's decision after review round 36).
# Managed settings outrank every other layer, and without the pin a worktree's own settings.local.json
# -- which sandboxed Bash can create in a new worktree -- could set enabled:false, and Claude Code would
# start that session's Bash unsandboxed. allowUnsandboxedCommands:false removes the per-command escape, and
# failIfUnavailable:true makes Bash error, rather than run unconfined, when the sandbox cannot come up
# (measured: README, "`/proc` has to be unmasked"; the user's decision after review round 38).
# The host posture (scripts/auto-mode-posture.json) requires the same three on the Mac.
if [ "$(jq -r '.sandbox.enabled' "$here/managed-settings.json" 2>/dev/null)" = true ] \
   && [ "$(jq -r '.sandbox.failIfUnavailable' "$here/managed-settings.json" 2>/dev/null)" = true ] \
   && [ "$(jq -r '.sandbox.allowUnsandboxedCommands' "$here/managed-settings.json" 2>/dev/null)" = false ]; then
    ok "the image's managed settings pin the sandbox on (enabled, fail if unavailable, no unsandboxed commands), so no lower settings layer can switch it off"
else
    bad "the image's managed settings do not pin sandbox.enabled:true, failIfUnavailable:true and allowUnsandboxedCommands:false — a worktree's own settings.local.json, which sandboxed Bash can create, could switch the sandbox off for the next session there"
fi

# ...and that grant is decorative unless the base image's blanket one is gone. The devcontainers
# base ships /etc/sudoers.d/vscode = `NOPASSWD:ALL`, under which the agent can flush the firewall,
# delete the allowlist snapshot or rewrite the root-owned script. verify.sh asks sudo itself at
# runtime, which is the real check; this catches the removal being dropped from the Dockerfile.
if grep -qF 'rm -f /etc/sudoers.d/vscode' "$here/Dockerfile"; then
    ok "the base image's blanket NOPASSWD:ALL grant is removed"
else
    bad "the Dockerfile no longer removes /etc/sudoers.d/vscode — the agent can sudo anything, and every root-ownership guard here is bypassable"
fi

# AND THE SANDBOX'S OWN ALLOW LISTS MUST STAY CLEAR OF THE TREE. With no deny rule naming the
# transcripts any more, the one thing keeping SANDBOXED BASH out of them is the blanket `denyRead`
# of `~` -- which an allowRead or allowWrite entry carves back. The per-file `.jsonl` denies used to
# bind /dev/null over each transcript whatever the allow lists said; now an entry widened to
# `~/.claude` would expose every transcript to `cat`, with every other guard still green. Review
# round 3, filed as aggravated: the gap is old, this branch removed what was covering it. Compared in
# `~` space with the shared canonicaliser, both directions: an entry containing a root, or inside one.
# GATED ON THE LOADER'S RESULT, which reported its own failure above: without posture_canon this
# block would read every entry as empty and pass on nothing.
# ABSOLUTE ENTRIES COUNT TOO. Compared in `~` space alone, `/home/vscode/.claude` passed as clear
# (review round 4, reproduced). The posture applies on the host AND in the container, so an entry
# under any home -- /home/<user>/..., /Users/<user>/..., /root/... -- is mapped into `~` space, and
# `/`, `/home`, `/Users` are ancestors of every home.
# AN ALLOW ENTRY IN `~` SPACE, as the sandbox reads it: canonicalised, cut to its literal base (the
# sandbox collapses `~/.claude/**` to `~/.claude`), and with an absolute path under any home mapped to
# `~`. ONE helper for both allow guards: the kit's guard compared raw strings, so `~/.local/**` or
# `/Users/<u>/.local/share` read as not covering the kit (review round 14).
dc_tilde_base() { # dc_tilde_base <allow entry> -> its literal base in ~ space
    local a
    a="$(posture_rule_base "$(posture_canon "$1")")"
    case "$a" in
        /home/*/*|/Users/*/*) a="~/${a#/*/*/}" ;;
        /root/*)              a="~/${a#/root/}" ;;
        /home/*|/Users/*|/root|/home|/Users|/) a="~" ;;
    esac
    printf '%s\n' "$a"
}
dc_allow_hit=""
if [ "$dc_reader_ok" = 1 ]; then
    while IFS= read -r dc_a; do
        [ -n "$dc_a" ] || continue
        # A GLOB ENTRY IS ITS LITERAL BASE: the sandbox collapses `~/.claude/**` to `~/.claude`, and
        # compared as a string it passed as clear while opening every transcript (review round 6).
        dc_a="$(dc_tilde_base "$dc_a")"
        for dc_root in $(posture_transcript_roots); do
            if [ "$dc_a" = "$dc_root" ] || [ "${dc_root#"$dc_a"/}" != "$dc_root" ] || [ "${dc_a#"$dc_root"/}" != "$dc_a" ] || [ "$dc_a" = "~" ]; then
                dc_allow_hit="$dc_allow_hit $dc_a"; break
            fi
        done
    # FROM BOTH FILES, as the deny arm reads both: allow arrays MERGE across settings layers, so an
    # entry in managed-settings.json opens the tree exactly as one in the posture does. This read
    # the posture alone (review round 5).
    done <<<"$( { jq -r '.sandbox.filesystem | (.allowRead[]?, .allowWrite[]?)' "$here/managed-settings.json"
                 jq -r '.require.sandbox.filesystem | (.allowRead[]?, .allowWrite[]?)' "$posture"; } 2>/dev/null)"
    if [ -n "$dc_allow_hit" ]; then
        bad "a sandbox allowRead/allowWrite entry reaches the transcript tree, so sandboxed Bash can read or write other sessions' transcripts — nothing else stands in the way now that no deny rule names them:$dc_allow_hit"
    else
        ok "no sandbox allowRead/allowWrite entry reaches the transcript tree"
    fi
fi

# THE HOOK MUST NOT TRUST PATH. It runs UNSANDBOXED on every tool call, and the image puts the
# agent-writable ~/.local/bin and ~/.cargo/bin first on PATH, so a `jq` or a `bash` planted there ran
# outside the sandbox with the container credential readable (review round 4, measured; the guards
# above certified the hook "not replaceable" because they looked only at the script file). Required:
# an absolute shebang -- `#!/usr/bin/env bash` finds bash itself through PATH -- and PATH fixed to
# system directories before the first command the script runs.
dc_shebang="$(head -1 "$here/deny-transcripts.sh")"
dc_first_cmd="$(dc_strip_comments "$here/deny-transcripts.sh" | sed '1d' | grep -m1 -E '[^[:space:]]')"
# `-p` REQUIRED, not merely an absolute bash: privileged mode is what makes bash ignore BASH_ENV
# and functions exported through the environment, and a plain `#!/bin/bash` passed every gate here
# while losing both (review round 7). The self-test also plants a BASH_ENV and watches it not run.
case "$dc_shebang" in
    '#!/bin/bash -p'|'#!/bin/bash -p '*|'#!/usr/bin/bash -p'|'#!/usr/bin/bash -p '*)
        if [ "$dc_first_cmd" != "PATH=/usr/bin:/bin" ]; then
            bad "deny-transcripts.sh does not fix PATH before it runs anything (its first command is [$dc_first_cmd]) — it runs unsandboxed, and ~/.cargo/bin and ~/.local/bin, which the sandbox can write, come first on the image's PATH"
        else
            ok "the transcript hook has an absolute shebang and fixes PATH before running anything"
        fi ;;
    *) bad "deny-transcripts.sh's shebang is [$dc_shebang], not an absolute bash in privileged mode (\`#!/bin/bash -p\`) — an env shebang finds bash through a PATH the sandbox can write to, and without -p bash runs BASH_ENV and imported functions, all unsandboxed" ;;
esac

# THE SWEEP RUNS UNSANDBOXED TOO, and the same rule holds for it: the reaper pipes it into
# `docker exec`, run.sh runs it at start, and the image's PATH begins with directories the sandbox
# can write. Round 7 found a planted `jq` there running on the next reaper tick. Required: the
# sweep pins PATH as its first command, and every place that starts it -- or verify.sh, which also runs
# unsandboxed -- names `/bin/bash` by absolute path rather than letting docker exec look it up.
dc_unsb=""
# HERE-STRINGS, NEVER `producer | grep -q`: the note above dc_stmt_line is why. The first cut of
# this guard piped twice into grep -q, which is the race that once failed only on CI.
# THE FIRST COMMAND, not "somewhere in the real-run arm": the pin sat in that arm and the top-level
# `date` and `stat` ran a planted program before it (review round 8).
dc_sweep_first="$(dc_strip_comments "$here/sweep-transcripts.sh" | sed '1d' | grep -m1 -E '[^[:space:]]')"
[ "$dc_sweep_first" = '[ "${1:-}" = --self-test ] || { PATH=/usr/bin:/bin; export PATH; }' ] \
    || dc_unsb="$dc_unsb the sweep's first command does not pin PATH (it is [$dc_sweep_first]);"
grep -qF -- '"/bin/bash", "-s"' "$here/../crates/jkb-cli/src/transcripts.rs" \
    || dc_unsb="$dc_unsb the reaper's docker exec does not name /bin/bash;"
dc_run_stripped="$(dc_strip_comments "$here/run.sh")"
for dc_s in sweep-transcripts verify; do
    dc_ln="$(dc_stmt_line "$dc_run_stripped" "$dc_s\\.sh")"
    dc_line=""; [ -n "$dc_ln" ] && dc_line="$(sed -n "${dc_ln}p" <<<"$dc_run_stripped")"
    case "$dc_line" in
        *'/bin/bash "$DC_CTR_KIT/.container/'"$dc_s"'.sh"'*) ;;
        *) dc_unsb="$dc_unsb run.sh does not start $dc_s.sh with /bin/bash from the kit mirror;" ;;
    esac
done
# run.sh ITSELF runs as you on the host, and finds bash, jq and docker by name, from a shell an agent
# may have shaped. Since review round 27 it FILTERS NOTHING: its first command after `set` re-executes
# it under `env -i` with a PATH it builds from fixed directories and the kit home's path-keep file, and
# an allowlist of names. Rounds 11 to 26 filtered the inherited PATH and environment and each found the
# next inlet; these checks hold the construction, not a filter.
dc_run_cmds="$(dc_strip_comments "$here/run.sh" | sed '1d' | grep -E '[^[:space:]]' | head -2)"
dc_run_env="$(sed -n 2p <<<"$dc_run_cmds")"
# PRIVILEGED MODE, as the hook has: without -p, bash runs the launching terminal's BASH_ENV and imports
# its exported functions before any of this file runs (review round 21).
[ "$(head -1 "$here/run.sh")" = '#!/bin/bash -p' ] \
    || dc_unsb="$dc_unsb run.sh's shebang is not #!/bin/bash -p, so bash itself is found through PATH, or runs the launching terminal's BASH_ENV and exported functions;"
case "$dc_run_env" in
    *'jkb_home="$(/usr/bin/getent passwd "$(/usr/bin/id -u)" | /usr/bin/cut -d: -f6)"'*'jkb_path=/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin;'*'jkb_keepf="$jkb_home/.local/share/jkb-container-kit/path-keep"'*'jkb_env=("HOME=$jkb_home" "PATH=$jkb_path" '*'compgen -e'*'exec /usr/bin/env -i "${jkb_env[@]}" /bin/bash -p "$0" --jkb-clean-env "$@"'*) ;;
    *) dc_unsb="$dc_unsb run.sh does not rebuild its environment as its first command (env -i, a PATH built from fixed directories and path-keep, an allowlist), so it runs with what the launching terminal gave it;" ;;
esac
# The built PATH takes nothing from the inherited one, which travels only as JKB_USER_PATH for need_tool's
# message; and the allowlist names neither PATH nor a variable that steers what a child runs.
case "$dc_run_env" in
    *'jkb_path="$PATH'*|*'jkb_path=$PATH'*|*'jkb_path="${PATH'*|*'jkb_path=${PATH'*|*':$PATH'*|*':${PATH'*) dc_unsb="$dc_unsb run.sh builds its PATH from the inherited one;" ;;
esac
case "$dc_run_env" in
    *'in PATH|'*|*'|PATH|'*|*'|PATH)'*|*'in HOME|'*|*'|HOME|'*|*'|HOME)'*|*DOCKER_CONFIG*|*DOCKER_HOST*|*BASH_ENV*|*TAR_OPTIONS*|*LD_*|*DYLD_*|*TMPDIR*|*'|*)'*) dc_unsb="$dc_unsb run.sh's environment allowlist names a variable that steers what its children run;" ;;
esac
# ...AND THE ALLOWLIST NAMES EVERY JKB_ VARIABLE run.sh READS that it does not set itself: dropping
# JKB_CONTAINER_NAME made run.sh act on jkb-dev while the reaper looked for the override (review round
# 24). "Sets itself" is a statement-start assignment in the code (a comment or an echo exempted
# JKB_RUN_FROM_CHECKOUT, round 25), or a name the re-exec puts in its own environment.
dc_set_here="$({ dc_strip_comments "$here/run.sh"; dc_strip_comments "$here/lib.sh"; } | grep -oE '(^|;)[[:space:]]*(export |local )?JKB_[A-Z_]+=' | grep -oE 'JKB_[A-Z_]+'; grep -oE '"JKB_[A-Z_]+=' <<<"$dc_run_env" | tr -d '"=')"
for dc_v in $(grep -oE '[$][{]?JKB_[A-Z_]+' "$here/run.sh" | tr -d '${' | sort -u); do
    grep -qx -- "$dc_v" <<<"$dc_set_here" && continue
    case "$dc_run_env" in *"|$dc_v|"*|*"|$dc_v)"*) ;; *) dc_unsb="$dc_unsb run.sh reads $dc_v but its environment allowlist drops it;" ;; esac
done
grep -qF '[ "$IMAGE" != jkb-dev ]' <<<"$(dc_strip_comments "$here/run.sh")" \
    || dc_unsb="$dc_unsb run.sh may run an existing image named by JKB_CONTAINER_IMAGE without building it from the kit;"
# THE INSTALLED HOOK REFUSES ITS SELF-TEST, before the self-test stages any copy: run unsandboxed, its
# copies in agent-writable /tmp and ~/.cache could be swapped and run outside the sandbox (review
# rounds 34 and 35; nothing held the refusal).
dc_st="$(dc_strip_comments "$here/deny-transcripts.sh" | sed -n '/^if \[ "\${1:-}" = --self-test \]; then/,/mktemp/p')"
grep -qF 'if [ "$dt_installed" = 1 ]; then' <<<"$dc_st" && grep -qF 'exit 1' <<<"$dc_st" \
    || dc_unsb="$dc_unsb the installed transcript hook does not refuse --self-test before staging copies, which run unsandboxed from agent-writable directories;"
grep -q 'JKB_CONTAINER_KIT_HOME' <<<"$(dc_strip_comments "$here/lib.sh")" \
    && dc_unsb="$dc_unsb lib.sh lets JKB_CONTAINER_KIT_HOME move the kit, and a launching terminal sets it;"
# EVERY EXEC IN run.sh, not the two above: round 7 fixed the sweep and verify.sh and left seven
# others -- `bash -c` for the login, `bash -lc` for the reap, `sh`, `sudo` -- resolving through the
# same PATH (review round 8). Each `docker exec`/`in_container` statement must name its program
# absolutely, and pin PATH with `-e PATH=/usr/bin:/bin` unless the program is one that answers for
# its own: the sweep (pins as its first command), setup.sh (runs the toolchain by design, once),
# verify.sh (task verify-sh-runs-unsandboxed-with--18da6e4b5d893488), or sudo (secure_path).
# EVERY OCCURRENCE IS ACCOUNTED FOR, never skipped: a statement whose container or program the scan
# cannot resolve is a failure, so `in_container --user root "${NAME}" bash` cannot be added unseen
# (review round 9: the scan skipped anything not spelled `"$NAME"`, and the floor cannot see an
# addition). The only two occurrences that are not a statement are named: in_container's own
# `docker exec "$@"`, and the `"docker exec $*"` it prints when the container has died.
# run.sh AND lib.sh, whose hook mirror runs four execs -- one of them `sh -c` as root, which found
# `sh` on the agent-writable PATH until review round 8's self-review. lib.sh's container token is
# `"$name"` and its docker `"$docker"`. Each line is tagged with its file, so a failure says which.
# AND NOTHING FROM THE CHECKOUT: a `.container/` path in an exec that is not under "$DC_CTR_KIT" is
# the checkout's copy, which the agent writes (the kit's whole point; lib.sh's DC_KIT_DIR).
dc_exec_awk='
    {
        n = split($0, t, /[[:space:]]+/)
        l = $0; gsub(/"\$DC_CTR_KIT\/\.container\//, "", l); ck = (l ~ /\.container\//) ? 1 : 0
        for (i = 1; i <= n; i++) {
            if (t[i] ~ /(^|[("])in_container$/ || (t[i] == "exec" && i > 1 && t[i-1] ~ /docker"?$/)) {
                if (t[i] == "exec" && t[i-1] == "\"docker") continue
                j = i + 1; pin = 0
                while (j <= n && t[j] ~ /^-/) {
                    if ((t[j] == "-e" || t[j] == "--env") && t[j+1] == "PATH=/usr/bin:/bin") pin = 1
                    if (t[j] == "--env=PATH=/usr/bin:/bin") pin = 1
                    if (t[j] ~ /^(-e|-w|-u|--env|--workdir|--user)$/) j += 2; else j++
                }
                if (t[j] == "\"$@\"") continue
                if ((t[j] != "\"$NAME\"" && t[j] != "\"$name\"") || t[j+1] == "") { print F ":" NR "\tUNRESOLVED\t" t[j] "\t" t[j+1] "\t" ck; continue }
                print F ":" NR "\t" pin "\t" t[j+1] "\t" t[j+2] "\t" ck
            }
        }
    }'
dc_execs="$(awk -v F=run.sh "$dc_exec_awk" <<<"$dc_run_stripped"; awk -v F=lib.sh "$dc_exec_awk" <<<"$(dc_strip_comments "$here/lib.sh")")"
dc_nexec=0; dc_nlib=0
while IFS=$'\t' read -r dc_ln dc_pin dc_prog dc_arg dc_ck; do
    [ -n "$dc_ln" ] || continue
    dc_nexec=$((dc_nexec + 1))
    case "$dc_ln" in lib.sh:*) dc_nlib=$((dc_nlib + 1)) ;; esac
    [ "$dc_ck" = 1 ] && dc_unsb="$dc_unsb $dc_ln runs a script from the checkout's .container/, not from \"\$DC_CTR_KIT\";"
    if [ "$dc_pin" = UNRESOLVED ]; then
        dc_unsb="$dc_unsb $dc_ln has a container exec the scan cannot read (container [$dc_prog], program [$dc_arg]) -- spell it \`\"\$NAME\" /absolute/program\`;"
        continue
    fi
    case "$dc_prog" in
        /*) ;;
        *) dc_unsb="$dc_unsb $dc_ln starts [$dc_prog] by PATH lookup;"; continue ;;
    esac
    [ "$dc_pin" = 1 ] && continue
    case "$dc_prog $dc_arg" in
        '/bin/bash "$DC_CTR_KIT/.container/sweep-transcripts.sh"'|'/bin/bash "$DC_CTR_KIT/.container/setup.sh"'|'/bin/bash "$DC_CTR_KIT/.container/verify.sh"'|"/usr/bin/sudo "*) ;;
        *) dc_unsb="$dc_unsb $dc_ln runs [$dc_prog $dc_arg] without -e PATH=/usr/bin:/bin;" ;;
    esac
done <<<"$dc_execs"
# ~/.jq AS WELL AS PATH: jq sources $HOME/.jq into every program it runs, and the sandbox writes
# $HOME. The sweep and verify.sh each define ONE wrapper that runs jq with HOME=/dev/null, as their
# first mention of jq, so every call -- lib.sh's included, which round 9 found a per-call scan of
# these files could not see -- goes through it. Bypassing it takes `command jq` or an absolute
# `/usr/bin/jq`, and either is refused outside the wrapper line itself.
dc_jq_wrap='jq() { HOME=/dev/null command jq "$@"; }'
# run.sh too: its jq readers build the mount list handed to `docker run` (review round 12).
for dc_f in sweep-transcripts verify run; do
    dc_jq_text="$(dc_strip_comments "$here/$dc_f.sh")"
    dc_jq_first="$(grep -m1 -E '(^|[^A-Za-z0-9_.-])jq([^A-Za-z0-9_.-]|$)' <<<"$dc_jq_text")"
    [ "$dc_jq_first" = "$dc_jq_wrap" ] \
        || dc_unsb="$dc_unsb $dc_f.sh does not define the HOME=/dev/null jq wrapper before its first jq (first mention: [$dc_jq_first]), so an agent-written ~/.jq is sourced into its jq;"
    dc_jq_bypass="$(grep -vxF -- "$dc_jq_wrap" <<<"$dc_jq_text" | grep -cE 'command jq|/jq([[:space:]]|$)')"
    [ "$dc_jq_bypass" -eq 0 ] \
        || dc_unsb="$dc_unsb $dc_f.sh calls jq around its wrapper ($dc_jq_bypass time(s), by \`command jq\` or an absolute path);"
done
# A FLOOR, so a parser that stops matching reads as a failure and not as "nothing to check".
[ "$dc_nexec" -ge 13 ] && [ "$dc_nlib" -eq 4 ] \
    || dc_unsb="$dc_unsb only $dc_nexec container exec(s) found in run.sh and lib.sh ($dc_nlib in lib.sh), where there are nine and four -- the scan has stopped seeing them;"
if [ -n "$dc_unsb" ]; then
    bad "an unsandboxed script can be steered through PATH:$dc_unsb a program planted in ~/.cargo/bin would run outside the sandbox with the container credential readable"
else
    ok "the sweep pins PATH, and the reaper and run.sh start the unsandboxed scripts with /bin/bash"
fi

# THE KIT HOLDS EVERYTHING ITS SCRIPTS RUN. setup.sh and verify.sh run from the root-owned kit mirror
# and execute scripts from `scripts/` beside it; one that is not in lib.sh's dc_kit_paths would be
# missing from the mirror, and one reached through the CHECKOUT ($repo, $mem_repo) is the agent's
# copy, which is what the kit exists to stop (review round 8's self-review: verify.sh ran the
# checkout's link-claude-memory.sh and auto-mode.sh on every start). setup.sh's `cargo install` and
# the extension build are the named exception: they BUILD the checkout, which is their job.
dc_kit_list="$(bash -c '. "$1" && dc_kit_paths' _ "$here/lib.sh" 2>/dev/null)"
dc_kit_bad=""
[ -n "$dc_kit_list" ] || dc_kit_bad=" lib.sh's dc_kit_paths printed nothing;"
for dc_f in setup verify install-extensions; do
    dc_t="$(dc_strip_comments "$here/$dc_f.sh")"
    while IFS= read -r dc_ref; do
        [ -n "$dc_ref" ] || continue
        grep -qxF -- "scripts/$dc_ref" <<<"$dc_kit_list" || dc_kit_bad="$dc_kit_bad $dc_f.sh runs scripts/$dc_ref, which the kit does not carry;"
    done <<<"$(grep -oE '\$(kit|kit_dc)/scripts/[A-Za-z0-9._-]+' <<<"$dc_t" | sed 's,.*/scripts/,,' | sort -u)"
    # The one named exception: the extension's builder, run on the checkout it builds.
    dc_ck="$(grep -nE '\$\{?(repo|mem_repo)\}?/(scripts|\.container)/' <<<"$dc_t" \
        | grep -vF '"$repo/scripts/install-extension.sh" --build-in' | head -3 | tr '\n' ' ')"
    [ -z "$dc_ck" ] || dc_kit_bad="$dc_kit_bad $dc_f.sh reaches the checkout's scripts ($dc_ck);"
done
if [ -n "$dc_kit_bad" ]; then
    bad "the container kit does not hold what its scripts run:$dc_kit_bad what runs unsandboxed must come from the kit, never from the checkout the agent can write"
else
    ok "setup.sh, verify.sh and install-extensions.sh run scripts only from the kit, and the kit carries each one"
fi

# NO AGENT CAN WRITE THE KIT. It is what runs outside every sandbox, so it must be out of reach of
# the container (no bind reaches it), of sandboxed Bash on the host (no posture allowWrite covers
# it), and of the in-process file tools (a posture Edit deny names it). It sat under ~/.jkb first,
# which the posture's allowWrite grants, so any sandboxed host agent could rewrite the kit's run.sh
# (review round 10). The default comes from lib.sh with HOME pinned, in ~ space like the posture.
# The kit's HOME, not only the kit: the staging copies and the mirror's archive live under it too.
dc_kit_home="$(env -u JKB_CONTAINER_KIT_HOME HOME=/kit-home-probe bash -c '. "$1" && printf "%s" "$DC_KIT_HOME"' _ "$here/lib.sh" 2>/dev/null)"
dc_kit_tilde="~${dc_kit_home#/kit-home-probe}"
dc_kit_where=""
case "$dc_kit_home" in /kit-home-probe/?*) ;; *) dc_kit_where=" lib.sh's DC_KIT_HOME [$dc_kit_home] is not under the home;" ;; esac
dc_posture="$here/../scripts/auto-mode-posture.json"
# ONE REACH TEST over every place run.sh's trust rests on: the kit, the Homebrew prefixes it keeps
# on PATH, the directories /usr/local/bin's links point into, and ~/.docker, whose CLI plugins
# `docker info` runs. An allowWrite entry that is any of them, or an ancestor, lets sandboxed Bash
# plant code there; the Homebrew arm checked only the Edit deny (review round 15).
dc_protected="$dc_kit_tilde
/opt/homebrew
/usr/local
/Applications
~/.docker"
while IFS= read -r dc_aw; do
    [ -n "$dc_aw" ] || continue
    dc_aw="$(dc_tilde_base "$dc_aw")"
    while IFS= read -r dc_pp; do
        case "$dc_pp:$dc_aw" in "~"*":~"|*":/") dc_kit_where="$dc_kit_where the posture's allowWrite entry [$dc_aw] covers $dc_pp;"; continue ;; esac
        if [ "$dc_pp" = "$dc_aw" ] || [ "${dc_pp#"$dc_aw"/}" != "$dc_pp" ]; then
            dc_kit_where="$dc_kit_where the posture's allowWrite entry [$dc_aw] covers $dc_pp;"
        fi
    done <<<"$dc_protected"
done <<<"$(HOME=/dev/null jq -r '.require.sandbox.filesystem.allowWrite[]? // empty' "$dc_posture" 2>/dev/null)"
HOME=/dev/null jq -e --arg r "Edit($dc_kit_tilde/**)" '.require.permissions.deny | index($r)' "$dc_posture" >/dev/null 2>&1 \
    || dc_kit_where="$dc_kit_where the posture has no Edit($dc_kit_tilde/**) deny, so the in-process file tools can write it;"
dc_kit_nsrc=0
while IFS= read -r dc_src; do
    [ -n "$dc_src" ] || continue
    dc_kit_nsrc=$((dc_kit_nsrc + 1))
    # A named volume reaches no host path; only a ${localEnv:HOME} bind can hold the kit.
    case "$dc_src" in '${localEnv:HOME}'*) ;; *) continue ;; esac
    dc_src="~${dc_src#\$\{localEnv:HOME\}}"; dc_src="${dc_src%/}"
    if [ "$dc_kit_tilde" = "$dc_src" ] || [ "${dc_kit_tilde#"$dc_src"/}" != "$dc_kit_tilde" ]; then
        dc_kit_where="$dc_kit_where container.json binds [$dc_src], which holds it;"
    fi
done <<<"$(dc_strip "$here/container.json" 2>/dev/null | HOME=/dev/null jq -r '(.mounts // [])[] | split(",")[] | select(startswith("source=")) | ltrimstr("source=")' 2>/dev/null)"
[ "$dc_kit_nsrc" -gt 0 ] || dc_kit_where="$dc_kit_where container.json's mount sources could not be read, so whether a bind holds it is unchecked;"
# ...AND run.sh READS ITS PATH KEEP LIST FROM THIS SAME HOME. It spells the location itself, because
# its first command runs before lib.sh is sourced, so a move of DC_KIT_HOME that left it behind would have run.sh
# trust a file that is neither made 0700 nor Edit-denied, with every check here green (review round 23).
dc_keepf="$(sed -n 's/.*jkb_keepf="\([^"]*\)".*/\1/p' "$here/run.sh" | head -1)"
dc_keepf="${dc_keepf//\$jkb_home//kit-home-probe}"
[ -n "$dc_keepf" ] && [ "${dc_keepf%/*}" = "$dc_kit_home" ] \
    || dc_kit_where="$dc_kit_where run.sh reads its PATH keep list from [${dc_keepf:-nothing found}], not from the kit home [$dc_kit_home];"
# ...AND THE PATH run.sh BUILDS: path-keep's entries, then the system and Homebrew prefixes, where docker
# and jq live. Homebrew's are owned by the user,
# so the in-process Write tool, which no sandbox confines, could replace a jq there that run.sh then
# runs as the user (review round 14). The posture denies Edit on both.
# ~/.cargo/env too, a FILE: every login shell rustup set up sources it, unsandboxed, while the posture
# lets sandboxed agents write ~/.cargo for builds (review round 15). One Edit rule is both the Write
# tool's deny and, merged by Claude Code, the sandbox's denyWrite.
# ...AND EVERY OTHER FILE THAT BECOMES CODE LATER, outside every sandbox: the shell startup files (one
# `export BASH_ENV=...` in ~/.zshenv ran in run.sh before it rebuilt its environment), git's global config and
# hooks, ssh's config, and the per-user autostart directories (review round 20) -- and VS Code's user
# settings and extensions, whose terminal.integrated.env and extension code run in your next terminal
# or window (review round 24). Only the user edits these, so denying them costs nothing.
for dc_inlet in '~/.cargo/env' '~/.zshenv' '~/.zprofile' '~/.zshrc' '~/.zlogin' '~/.zlogout' '~/.bashrc' \
                '~/.bash_profile' '~/.bash_login' '~/.profile' '~/.bash_logout' '~/.gitconfig' '~/.config/git/**' \
                '~/.ssh/**' '~/Library/LaunchAgents/**' '~/.config/systemd/user/**' '~/.config/autostart/**' \
                '~/Library/Application Support/Code/User/**' '~/.config/Code/User/**' '~/.vscode/extensions/**'; do
    HOME=/dev/null jq -e --arg r "Edit($dc_inlet)" '.require.permissions.deny | index($r)' "$dc_posture" >/dev/null 2>&1 \
        || dc_kit_where="$dc_kit_where the posture has no Edit($dc_inlet) deny, so an agent can plant code that runs later as you, unsandboxed;"
done
for dc_pfx in //opt/homebrew //usr/local //Applications '~/.docker'; do
    HOME=/dev/null jq -e --arg r "Edit($dc_pfx/**)" '.require.permissions.deny | index($r)' "$dc_posture" >/dev/null 2>&1 \
        || dc_kit_where="$dc_kit_where the posture has no Edit($dc_pfx/**) deny, so the Write tool can replace a program run.sh runs from there;"
done
if [ -n "$dc_kit_where" ]; then
    bad "an agent can write the container kit ($dc_kit_tilde), which runs outside every sandbox:$dc_kit_where"
else
    ok "no agent can write the container kit ($dc_kit_tilde), the Homebrew prefixes run.sh keeps on PATH, or a shell, git, ssh or autostart file: no container bind, posture allowWrite or missing Edit deny reaches them"
fi

# THE FINGERPRINT STRIPS THE ROOT THE ARGUMENTS WERE ASSEMBLED FROM. run.sh assembled from the kit
# and fingerprinted with the checkout, so the kit's seccomp path entered the hash and every existing
# container read as stale, with `--rm` as the advice (review round 10). The self-test proves the
# function; this holds the two call sites to one root.
if stripped_matches "$here/run.sh" '^ARGS_OUT="\$\(assembled_args "\$args_root"\)"' \
   && stripped_matches "$here/run.sh" '^want_hash="\$\(fingerprint "\$args_root" '; then
    ok "run.sh fingerprints the container with the same root it assembles the arguments from"
else
    bad "run.sh's live fingerprint and its assembly do not both use \$args_root — a root the fingerprint does not strip enters the hash, and every existing container reads as created from a different container.json"
fi

# THE REPO'S OWN SETTINGS DO NOT REPLACE THE CONTAINER'S ENVIRONMENT. Claude Code puts a settings
# file's `env` into every session, over the image's ENV, and .claude/settings.json is committed and
# shared with the host. verify.sh checks every layer a container session loads, settings.local.json
# included; this holds the committed file at review time, before any container sees it.
if ! dc_envp_names="$(dc_protected_env "$here/Dockerfile" "$here/container.json" 2>&1)"; then
    bad "the environment names the container sets could not be derived ($dc_envp_names), so whether the repo's Claude settings replace one is unchecked"
else
    dc_envp_hit="$(settings_env_shadows "$dc_envp_names" "$here/../.claude/settings.json" | cut -f2 | tr '\n' ' ')"
    if [ -n "$dc_envp_hit" ]; then
        bad "the repo's .claude/settings.json sets env that the container itself sets ( $dc_envp_hit), so every container session gets the file's value instead of the image's — a PATH written for one machine breaks jkb's resolution on the other"
    else
        ok "the repo's committed Claude settings replace none of the $(grep -c . <<<"$dc_envp_names") environment names the container sets"
    fi
fi

# run.sh CHECKS THE KIT BEFORE IT MIRRORS IT. The mirror's tar dereferences, so a link in the kit
# would put its target in the container; dc_install_kit refuses one in the copy it makes, and run.sh
# holds the line for a kit made any other way. No test drives run.sh to that step, so this holds the
# call in place, ahead of the mirror (review round 15: deleting it left everything green).
dc_kit_chk="$(grep -n 'kit_odd="$(dc_unsafe_entries "$kit_src")"' <<<"$dc_run_stripped" | head -1 | cut -d: -f1)"
dc_kit_mir="$(grep -n 'dc_mirror_hooks "$kit_src" "$DC_CTR_KIT"' <<<"$dc_run_stripped" | head -1 | cut -d: -f1)"
if [ -n "$dc_kit_chk" ] && [ -n "$dc_kit_mir" ] && [ "$dc_kit_chk" -lt "$dc_kit_mir" ] \
   && grep -qF '[ -z "$kit_odd" ] || die' <<<"$dc_run_stripped"; then
    ok "run.sh refuses to mirror a kit holding a link, a special file or a hard link, before it mirrors one"
else
    bad "run.sh does not check the kit with dc_unsafe_entries, and refuse, before dc_mirror_hooks copies it into the container — a link in the kit would carry its target's bytes in"
fi

# ONE DERIVATION OF THE SERVED CHECKOUT. setup.sh, verify.sh and install-extensions.sh each spelled
# it, and two took the kit mirror for the checkout when run there (review round 18); each now calls
# lib.sh's dc_repo_root, and none spells `${JKB_REPO_ROOT:-` itself (review round 19: nothing held it).
dc_rr_bad=""
for dc_f in setup verify install-extensions; do
    dc_t="$(dc_strip_comments "$here/$dc_f.sh")"
    grep -q 'dc_repo_root "' <<<"$dc_t" || dc_rr_bad="$dc_rr_bad $dc_f.sh does not call dc_repo_root;"
    # A DEFAULT naming a directory is a derivation; `${JKB_REPO_ROOT:-}` only asks whether run.sh set it.
    grep -qE '\$\{JKB_REPO_ROOT:-[^}]' <<<"$dc_t" && dc_rr_bad="$dc_rr_bad $dc_f.sh derives the checkout itself;"
done
if [ -n "$dc_rr_bad" ]; then
    bad "the served checkout is derived outside lib.sh's dc_repo_root:$dc_rr_bad run from the kit mirror, such a script takes the mirror itself for the checkout"
else
    ok "setup.sh, verify.sh and install-extensions.sh derive the checkout through dc_repo_root alone"
fi

# THE HOOK AND THE SWEEP MUST AGREE ON WHERE THE TREE IS. The hook cannot load the shared reader --
# it is installed alone, root-owned, at /usr/local/bin -- so its roots are its own, and they drifted:
# it ignored CLAUDE_CONFIG_DIR while the sweep honoured it, leaving the real tree unguarded whenever
# that is set (review round 3). Held together by name here, on comment-stripped text: each must
# derive a root from CLAUDE_CONFIG_DIR and name both spellings.
dc_hook_roots="$(dc_strip_comments "$here/deny-transcripts.sh" | sed -n '/^root_list=()/,/^done/p')"
dc_sweep_roots="$(dc_strip_comments "$here/sweep-transcripts.sh" | grep -E '^[[:space:]]*proots=')"
dc_roots_missing=""
for dc_need in 'CLAUDE_CONFIG_DIR' '.claude-state/projects'; do
    grep -qF -- "$dc_need" <<<"$dc_hook_roots"  || dc_roots_missing="$dc_roots_missing hook:$dc_need"
    grep -qF -- "$dc_need" <<<"$dc_sweep_roots" || dc_roots_missing="$dc_roots_missing sweep:$dc_need"
done
# EVERY ROOT IN THE SHARED LIST is named by the hook, the archive included (review rounds 9 and 11).
for dc_need in $(posture_transcript_roots 2>/dev/null); do
    grep -qF -- "${dc_need#\~/}" <<<"$dc_hook_roots" || dc_roots_missing="$dc_roots_missing hook:${dc_need#\~/}"
done
[ -n "$(posture_transcript_roots 2>/dev/null)" ] || dc_roots_missing="$dc_roots_missing (the shared root list printed nothing)"
grep -qE '^TRANSCRIPT_ARCHIVE=.*/\.claude-state/transcript-archive' <<<"$(dc_strip_comments "$here/sweep-transcripts.sh")" \
    || dc_roots_missing="$dc_roots_missing sweep:TRANSCRIPT_ARCHIVE"
if [ -z "$dc_hook_roots" ] || [ -z "$dc_sweep_roots" ]; then
    bad "the transcript roots could not be found in deny-transcripts.sh and sweep-transcripts.sh, so whether they agree is unchecked"
elif [ -n "$dc_roots_missing" ]; then
    bad "the hook and the sweep disagree about where the transcript tree is — missing:$dc_roots_missing; a tree one of them does not know is guarded by the other alone, or by neither"
else
    ok "the hook and the sweep derive the transcript tree from the same spellings, CLAUDE_CONFIG_DIR included"
fi
if grep -qF 'takes no arguments' "$here/egress-status.sh"; then
    ok "egress-status.sh refuses arguments"
else
    bad "egress-status.sh no longer refuses arguments — it runs as root, and a command naming no argument accepts every argument"
fi
if grep -qF 'takes no arguments' "$here/init-firewall.sh"; then
    ok "init-firewall.sh refuses arguments (its allowlist is the root-owned snapshot)"
else
    bad "init-firewall.sh still accepts a posture path — the agent-writable workspace copy could be passed to it"
fi
# Match ANY argument, not a path spelled a particular way. The first version used `[^"]*` to reach
# `auto-mode-posture.json` on the same line, which cannot cross the double quote in setup.sh's own
# `init-firewall.sh "$repo/scripts/..."` — so reverting setup.sh wholesale to the code this guard
# exists to prevent still printed `ok`. It caught the JSON spelling and never the shell one.
#
# DERIVED, not enumerated. The hand-written list was `setup.sh run.sh`, in a directory this change
# gave a third caller — a rule every new call site has to be remembered into is the defect, and the
# guard that misses the newest caller is the one nobody notices. So: every script here that names
# the firewall, minus the ones that only QUOTE it. That exclusion is three files that exist today
# and is asserted non-empty and complete below, where adding a caller needs no edit at all.
# Shell comments, removed. Two guards below need it and had one copy between them; a second
# spelling of "what is a comment" is a second answer to the question they both ask.

# THE SETUP MARKER IS NO LONGER GUARDED HERE, because it is no longer spelled twice (D52.5). This
# compared run.sh's and setup.sh's spellings of the marker path, justified by a comment reading
# "setup.sh runs inside the container, run.sh on the host [so] they cannot share a variable". They
# can: run.sh sources lib.sh on the host, setup.sh sources the same file inside the container from
# the same bind-mounted checkout, and the path is JKB_SETUP_MARKER there. One spelling, nothing to
# compare -- the guard is deleted with the duplication rather than kept as a second model of it.

# ONE VERIFIER. verify.sh used to be setup.sh's last line as well as a run.sh step, and the split
# is what let "a failing assertion must not suppress the attach instructions" get fixed on one path
# and stay broken on the other. It also made a verify failure read as "setup did not complete", so
# the next run redid the toolchain because an extension was missing. Putting it back would restore
# both, silently and slowly, which is the kind of regression nobody goes looking for.
if stripped_matches "$here/setup.sh" '(^|[^-[:alnum:]])verify\.sh'; then
    bad "setup.sh runs verify.sh again — run.sh verifies after both arms, and a second verifier there is what made a failed check re-run the whole of setup"
else
    ok "setup.sh does not verify; run.sh does, once, after either arm"
fi

# ...AND THE OTHER HALF, which the line above claims and did not check. Asserting only that
# setup.sh does NOT verify, while the passing message says run.sh does, means deleting run.sh's
# call leaves every harness green and nothing verifying anything.
# ANCHORED ON THE INVOCATION, not on a mention of the name (D51.8). Grepping for the bare string
# passed on run.sh's three failure MESSAGES, which name verify.sh whether or not it is ever called
# — text present on the pass path and the fail path both, which is this directory's most-repeated
# defect. Deleting the actual `docker exec ... verify.sh` line left the guard green and nothing
# checking the mount boundary. And the mutation written to watch it fail rewrote every occurrence
# of the token, including those messages, so it never established which one the guard reads: a
# mutation that changes more than one thing proves nothing about any of them.
#
# So: require a statement-level exec of it — the shape the firewall-argument guard below already
# uses — and let mutate-config.sh delete only that line.
# THROUGH dc_stmt_line, which is now the ONLY spelling of "a `bash .container/<x>` statement in
# run.sh" in this file. It answers this question and both of the sweep's below, so widening it for
# one -- the day verify.sh is invoked as `bash -lc '…'`, the way the reap line already is -- widens
# it for all three, instead of leaving this site green while the sweep's ordering branch goes red
# and invites its own deletion as the cheaper repair.
if [ -n "$(dc_stmt_line "$(dc_strip_comments "$here/run.sh")" 'verify\.sh')" ]; then
    ok "run.sh invokes verify.sh (a statement, not a mention of the name)"
else
    bad "run.sh no longer runs verify.sh — nothing verifies the container, and the guard above says it does"
fi

# THE TRANSCRIPT SWEEP, and the properties of its enumeration that no runtime check can see.
#
# WHY IT IS GUARDED STATICALLY. The sweep runs on every container start and moves files out of
# ~/.claude/projects. Every way of getting it wrong is SILENT in both directions: widen the name
# filter and it archives the auto-memory the host also owns, through a symlink into the bind mount;
# cap the depth and it sweeps the cheap half of the population (depth 2) while the agent
# transcripts that are the bulk (depth 4 and 6) accumulate exactly as before, so the container
# still dies at spawn with a sweep in the log saying it worked. Neither state raises anything at
# run time. The self-test catches them — this is the half that catches them being edited out of
# the file the self-test does not run against, and the half that compares a name against the
# EXTERNAL harness that owns it, which no fixture of ours can do.
#
# ONE ASSERTION, composed message. They are one property — "the enumeration still enumerates the
# right set" — and splitting them would be a failure path each where one names the subject. No
# numeral is written here: the count is `PINNED_SWEEP_APPENDS` in mutate-config.sh, derived from
# this block, and a numeral in prose is a second copy of it that goes stale on the next condition.
# ANCHORED ON THE FUNCTION NAME, like the verify guard above is anchored on the invocation: the
# body is extracted by name, so a mutation that edits the real `find` is seen and the self-test's
# own `find` calls are not, and an extraction that reads nothing is a failure rather than a run of
# vacuous passes.
sweep_problems=""
run_stripped="$(dc_strip_comments "$here/run.sh")"
sweep_at="$(dc_stmt_line "$run_stripped" 'sweep-transcripts\.sh')"
verify_at="$(dc_stmt_line "$run_stripped" 'verify\.sh')"
if [ -z "$sweep_at" ]; then
    sweep_problems="$sweep_problems run.sh does not invoke it (a statement, not a mention of the name);"
elif [ -z "$verify_at" ]; then
    # PINNED AGAINST READING NOTHING, like every other extraction here. This one was the exception:
    # an unmatched verify line made the ordering test below `[ -n "$verify_at" ] && …`, i.e. skipped,
    # so the property that the sweep runs BEFORE the verify would have gone quiet rather than red --
    # and that property is the whole reason the sweep is not disabled by an unrelated assertion.
    sweep_problems="$sweep_problems run.sh has no verify.sh statement to order it against, so the sweep-before-verify property cannot be established;"
elif [ "$sweep_at" -gt "$verify_at" ]; then
    sweep_problems="$sweep_problems run.sh runs it AFTER verify.sh (line $sweep_at vs $verify_at), so one failing assertion about something else disables it;"
fi
# `|| true` IS THE WHOLE OF "NEVER FATAL", and it was the one pinned property whose only watcher
# was a mutation ANCHOR: both sweep mutations happened to carry the text inside their anchor
# strings, so removing it from run.sh reported `NO-OP the mutation changed nothing`, pointing the
# developer at the mutation rather than at the lost non-fatality -- whose natural repair (relax the
# anchor) greens the gate. It vanishes altogether on a host with no jq or python3, where
# mutate-config.sh exits early. run.sh is `set -euo pipefail`, and the sweep returns 1 on paths the
# script itself documents (an unwritable archive, a file that raced away), so without this one
# raced transcript aborts the start BEFORE verify.sh and before the attach instructions.
if [ -n "$sweep_at" ] \
   && ! grep -qE '\|\|[[:space:]]+true[[:space:]]*$' <<<"$(sed -n "${sweep_at}p" <<<"$run_stripped")"; then
    sweep_problems="$sweep_problems run.sh does not append \`|| true\` to it, so under set -euo pipefail one file it could not archive aborts the start before verify.sh runs;"
fi
if [ ! -f "$here/sweep-transcripts.sh" ]; then
    sweep_problems="$sweep_problems sweep-transcripts.sh is not there at all;"
else
    sweep_body="$(dc_strip_comments "$here/sweep-transcripts.sh")"
    sweep_enum="$(awk '/^transcript_records\(\)/ { inf = 1 } inf { print } inf && /^\}/ { exit }' \
        <<<"$sweep_body")"
    if [ -z "$sweep_enum" ]; then
        sweep_problems="$sweep_problems it has no transcript_records() to read, so the checks below establish nothing;"
    else
        grep -qF -- 'find -L ' <<<"$sweep_enum" \
            || sweep_problems="$sweep_problems it does not pass -L, so a symlinked root (which is how the container spells it) enumerates nothing;"
        grep -qF -- '-maxdepth' <<<"$sweep_enum" \
            && sweep_problems="$sweep_problems it caps the depth, which misses the nested agent transcripts that are the bulk of the population;"
        grep -qF -- "-name '*.jsonl'" <<<"$sweep_enum" \
            || sweep_problems="$sweep_problems it does not filter on *.jsonl, so auto-memory is in the plan;"
        # NOTHING IS HELD BACK IN THE WALK. This function feeds the PROJECTION as well as the
        # plan, and the projection is the sizing of the argv that overflows -- a path the sweep
        # cannot reclaim costs the kernel exactly what one it can costs. The round that introduced
        # the journal exclusion put it here, and the sweep then acted on 65,250 bytes of a real
        # 96,612 and printed "nothing to archive" while every Bash call went on dying at spawn.
        # The name is spared in transcript_plan; the walk counts everything.
        grep -qF -- '! -name' <<<"$sweep_enum" \
            && sweep_problems="$sweep_problems it holds a name back in the WALK, which feeds the projection as well as the plan, so bytes it can never reclaim are invisible to the budget;"
        grep -qF -- '-name memory -prune' <<<"$sweep_enum" \
            || sweep_problems="$sweep_problems it does not prune memory/, so under -L the walk follows that symlink out into the bind-mounted ~/.jkb;"
        # THE SPARED NAME IS DERIVED, NOT SPELLED. Its authority is an EXTERNAL harness -- Claude
        # Code's workflow runner -- and this repo has already been wrong about the name once (it
        # believed `wf_*.json`; there are zero such files anywhere). Pinning the literal here would
        # have kept every gate green through a rename: swarm-status.sh would break visibly and get
        # fixed, and the sweep would go on archiving the run state oldest-first with this guard,
        # the fixture and the mutation all still agreeing about a name nothing writes. So the name
        # comes out of swarm-status.sh's discovery predicate -- the one reader that defines it --
        # and an extraction that reads nothing is a failure rather than a vacuous pass, the same
        # arrangement this file uses for DEFAULT_ADDR and the extension id.
        # THROUGH dc_strip_comments, like every other extraction in this file. Read raw, the
        # authority for the name the sweep must spare could be a sentence ABOUT the predicate
        # rather than the predicate -- and swarm-status.sh has prose around that very line.
        swarm_journal="$(grep -oE -- "-name [A-Za-z0-9_.-]+ -path '\*/subagents/workflows/wf_\*'" \
            <<<"$(dc_strip_comments "$here/../scripts/swarm-status.sh" 2>/dev/null)" \
            | sed -n '1s/^-name \([^ ]*\).*/\1/p')"
        sweep_held="$(grep -oE '^HELD_NAME=[A-Za-z0-9_.-]+' <<<"$sweep_body" | sed -n '1s/^HELD_NAME=//p')"
        if [ -z "$swarm_journal" ]; then
            sweep_problems="$sweep_problems swarm-status.sh no longer discovers runs by \`-name <file> -path '*/subagents/workflows/wf_*'\`, so the name the sweep must spare cannot be read from the reader that defines it;"
        elif [ "$sweep_held" != "$swarm_journal" ]; then
            sweep_problems="$sweep_problems it spares HELD_NAME='$sweep_held' while swarm-status.sh finds runs by '$swarm_journal', so the sweep archives the harness's own run state oldest-first and every past run reads as \"no swarm run found\";"
        fi
        # Checked as a REFUSAL rather than as a comparison of the two default constants, so the
        # JKB_TRANSCRIPT_ARCHIVE override cannot reach the state either.
        grep -qF -- '"$phys_root"/*)' <<<"$sweep_body" \
            || sweep_problems="$sweep_problems it does not refuse an archive inside the root, where each sweep re-enumerates what the last one moved;"
        # AND THE REFUSAL IS ON RESOLVED PATHS. It was a string-prefix test on the CALLER'S
        # spelling while the walk is `-L`, so in the container -- where ~/.claude/projects is a
        # symlink into the state volume -- an archive squarely inside the enumerated tree was not a
        # prefix of the root as spelled and was accepted: the safety net was inoperative in the one
        # deployment it was written for, and the .archive/.archive/ nesting reproduced at
        # 530 -> 602 -> 674 deny bytes. `pwd -P` is what makes the two spellings comparable.
        grep -qE -- 'phys_root=.*pwd -P' <<<"$sweep_body" \
            || sweep_problems="$sweep_problems it compares the caller's spellings rather than resolved paths, so under -L a symlinked root accepts an archive inside itself;"
        # ...AND SO IS THE OTHER OPERAND, which the branch above does not establish. A comparison
        # has two sides, and the round that fixed this pinned one of them: `phys_archive="$archive"`
        # -- a plausible simplification, since the archive usually does not exist yet and plain
        # `cd`+`pwd -P` cannot resolve a path that is not there -- left check-config.sh green and
        # every mutation CAUGHT while re-admitting the nesting the branch above exists to refuse.
        # Two branches because they are two edits with two repairs, and each carries its own
        # mutation.
        grep -qE -- 'phys_archive=.*transcript_resolve' <<<"$sweep_body" \
            || sweep_problems="$sweep_problems it does not resolve the ARCHIVE side of that comparison, so an archive that does not exist yet is compared as the caller spelled it;"
        sweep_resolve="$(awk '/^transcript_resolve\(\)/ { inf = 1 } inf { print } inf && /^\}/ { exit }' \
            <<<"$sweep_body")"
        if [ -z "$sweep_resolve" ]; then
            sweep_problems="$sweep_problems it has no transcript_resolve() to read, so the branch above establishes nothing;"
        else
            grep -qF -- 'pwd -P' <<<"$sweep_resolve" \
                || sweep_problems="$sweep_problems transcript_resolve() does not reach a physical path, so resolving the archive resolves nothing;"
        fi
        # `CLAUDE_BASE=.*CLAUDE_CONFIG_DIR`, not a bare mention of the variable anywhere in the
        # file: the mention form was satisfied by the self-test's own `env -u CLAUDE_CONFIG_DIR`,
        # so the mutation that renames the variable IN THE ASSIGNMENT went from CAUGHT to MISSED
        # the moment a row was added that names it for an unrelated reason. What is being asserted
        # is that the config base is DERIVED from it. Whether that derivation is spelled correctly
        # is the self-test's, which runs the script with CLAUDE_CONFIG_DIR set and reads the root
        # back out of the message.
        # WHAT NO SWEEP CAN REMOVE IS BOTH HALVES. verify.sh decides exit 3 ("a condition to act on",
        # with a remedy) against exit 1 ("a broken boundary", and run.sh refuses to open a window) on
        # this one sentence, and the first version of it asked about the held set alone. The residual
        # after a full plan is held PLUS the newest KEEP_NEWEST, which crosses the budget at roughly
        # 161 run journals where a held-only test only speaks past about 199 -- and a container
        # reaches the first on its way to the second. The self-test stages a budget between the two;
        # this is the half that catches the term being edited out of the file it does not run against.
        sweep_irr="$(awk '/^transcript_irreducible\(\)/ { inf = 1 } inf { print } inf && /^\}/ { exit }' \
            <<<"$sweep_body")"
        if [ -z "$sweep_irr" ]; then
            sweep_problems="$sweep_problems it has no transcript_irreducible() to read, so nothing establishes what the sweep calls beyond its own help;"
        else
            # RECOGNISED **AND** COUNTED, on one line each. Asking only that the arm exists passed a
            # mutant whose held arm was `{ next }` -- it still recognised the set and contributed
            # nothing, which is the whole defect in miniature.
            grep -qE -- 'base == held.*tot \+= length' <<<"$sweep_irr" \
                || sweep_problems="$sweep_problems transcript_irreducible() does not count the held-back files toward what no sweep can remove, so a tree the run journals alone put beyond help is reported as a broken boundary;"
            grep -qE -- 'kept < keep.*tot \+= length' <<<"$sweep_irr" \
                || sweep_problems="$sweep_problems transcript_irreducible() does not count the newest KEEP_NEWEST the floor protects, so the window where the floor is what puts a tree beyond help is reported as a broken boundary;"
        fi
        # MATCHED ON THE SESSION DIRECTORY, not the leaf name. A session writes <slug>/<uuid>.jsonl
        # and everything under <slug>/<uuid>/subagents/…, and those nested agent transcripts are
        # the BULK of the population: matching only the basename protected the first and left the
        # majority to the recency window alone. The registry has no row for a Task-tool subagent,
        # so nothing else can cover them.
        grep -qF -- 'index(path, "/" id "/")' <<<"$sweep_body" \
            || sweep_problems="$sweep_problems the sweep does not spare what a live session writes BENEATH its own directory, which is the bulk of the population and has no registry row of its own;"
        grep -qF -- 'index(path, "/" id ".jsonl")' <<<"$sweep_body" \
            || sweep_problems="$sweep_problems the sweep does not skip the sessions it is told are live, so the reaper's keep-list is data nothing acts on;"
        grep -qF -- 'mtime > now - fresh' <<<"$sweep_body" \
            || sweep_problems="$sweep_problems the sweep does not spare recently-written transcripts, so a live session the registry cannot see is archived out from under it;"
        # ...AND BOTH READERS ASK THE SAME QUESTION. transcript_plan must not plan a protected file
        # and transcript_irreducible must count one; the day they disagree is the day a sweep
        # archives a live transcript while reporting itself unable to reclaim anything.
        # SUBSTITUTED, NOT PIPED, and every other check in this file is a here-string for the same
        # reason: `producer | grep -q` is a defect this repository has shipped before. `grep -q`
        # exits at its FIRST match, the producer dies on the unwritten tail, and under `pipefail`
        # the pipeline reports the status of the producer -- so a check that FOUND what it wanted
        # fails. Latent by size rather than wrong-by-construction: these function bodies fit the
        # 64KB pipe buffer today, so awk finishes before grep leaves and this passed every run,
        # which is exactly what makes it worth removing rather than watching.
        for dc_fn in transcript_plan transcript_irreducible; do
            dc_body="$(awk -v f="$dc_fn" '$0 ~ "^" f "\\(\\)" { inf = 1 } inf { print } inf && /^\}/ { exit }' \
                <<<"$sweep_body")"
            grep -qF -- 'protected($2, $1)' <<<"$dc_body" \
                || sweep_problems="$sweep_problems $dc_fn() does not ask whether a file is protected, so the plan and the irreducible measure no longer agree about what a live session holds;"
        done
        # THE POST-CONDITION'S CALL SITE. Its comparison is watched by the self-test, which drives
        # transcript_projection_fell from literals -- but the self-test cannot see the call being
        # deleted, because the state it guards (files moved, deny list no smaller) is refused
        # upstream and so never arises in the fixture. A helper nothing calls is a helper that
        # passes its own tests for ever.
        grep -qF -- 'transcript_projection_fell "$moved"' <<<"$sweep_body" \
            || sweep_problems="$sweep_problems it never asks whether the projection actually fell, so a sweep that moved files and shrank nothing exits 0;"
        # A FILE THAT VANISHED BETWEEN THE PLAN AND THE MOVE IS NOT A FAILURE. Pinned here and
        # nowhere else, and said plainly: no executable test in this repository reaches that state,
        # because it needs a file to disappear between two statements of one function. A live
        # session, Claude Code's own cleanupPeriodDays retention or a second run.sh all produce it,
        # and counting it made a healthy start print "N file(s) could not be archived" with no
        # cause. A static pin is the only watcher available, so it is the one that is here.
        grep -qF -- '[ -e "$f" ] || continue' <<<"$sweep_body" \
            || sweep_problems="$sweep_problems it counts a transcript that vanished between the plan and the move as a failure, which a live session produces routinely;"
        # THE SEAMS ARE FOR THE SELF-TEST, NOT FOR THE CONTAINER. Each exists so --self-test can run
        # the file as a PROGRAM against a tree it can build in a temp dir, and each can switch the
        # sweep off from a shipped file: a root that does not exist ("does not exist — nothing to
        # sweep", rc 0), an archive somewhere harmless, or a budget nothing reaches. Every one of
        # them then produces a start that reports success for ever while the deny list grows to the
        # E2BIG this script exists to end.
        #
        # READ FROM THE FILE THAT DEFINES THEM, not spelled here. The version that shipped named ONE
        # of the three, so the other two were refused by nothing while this guard's comment claimed
        # the property -- and the seam list was by then written in three places with three different
        # contents. An extraction that reads nothing is a failure, as everywhere else in this file.
        # STRIPPED OF COMMENTS BEFORE MATCHING, because a comment cannot set a variable: this file's
        # own habit is to name the identifier under discussion, so a line like "# nothing here sets
        # JKB_DENY_BUDGET_BYTES" above run.sh's invocation would otherwise redden the gate with a
        # false accusation whose natural repair is to delete the explanation.
        sweep_seams="$(grep -oE '^SEAMS="[^"]*"' <<<"$sweep_body" | sed -n '1s/^SEAMS="\(.*\)"/\1/p')"
        # AND IT MUST EQUAL WHAT THE SCRIPT ACTUALLY READS. SEAMS is hand-written; the overrides are
        # `${JKB_…:-}` expansions. They agree today, by hand. A fourth one -- `KEEP_NEWEST=
        # "${JKB_KEEP_NEWEST:-32}"` is the obvious next -- added without touching SEAMS would be
        # refused in no shipped file, move no pin and make no mutation MISSED, while this guard went
        # on reporting the whole property. Derived and compared, the way HELD_NAME already is
        # against swarm-status.sh.
        # EVERY INPUT, not only the refusable seams. JKB_KEEP_SESSIONS is production input that
        # both triggers pass, so it belongs to the agreement check and not to the blanket refusal.
        sweep_inputs="$(grep -oE '^INPUTS="[^"]*"' <<<"$sweep_body" \
            | sed -n '1s/^INPUTS="\(.*\)"/\1/p')"
        sweep_inputs="${sweep_inputs//\$SEAMS/$sweep_seams}"
        # EVERY `${JKB_X` READ, whatever its operator: matching only `:-` missed posture_layer_files'
        # `${JKB_REPO_ROOT:+...}`, so it was in neither list and this stayed green (review round 26).
        sweep_reads="$(grep -oE '\$\{JKB_[A-Z_]+' <<<"$sweep_body" \
            | sed -E 's/^\$\{//' | LC_ALL=C sort -u | tr '\n' ' ')"
        sweep_declared="$(printf '%s\n' $sweep_inputs | LC_ALL=C sort -u | tr '\n' ' ')"
        if [ -z "$sweep_reads" ]; then
            sweep_problems="$sweep_problems it reads no \${JKB_…:-} override at all, so the SEAMS declaration can no longer be checked against the code;"
        elif [ "$sweep_reads" != "$sweep_declared" ]; then
            sweep_problems="$sweep_problems INPUTS declares [$sweep_declared] while the script reads [$sweep_reads], so an input is covered by neither the shipped-file refusal nor the self-test neutralisation while this guard reports the whole property;"
        fi
        if [ -z "$sweep_seams" ]; then
            sweep_problems="$sweep_problems it has no SEAMS= line to read, so the check that no shipped file wires a self-test seam into the container establishes nothing;"
        else
            # EVERY SHIPPED FILE, DERIVED, not a list retyped once a round. The list named four and
            # missed verify.sh -- which the same change had just made a CALLER of the sweep, and
            # which runs INSIDE the container, where a seam takes effect immediately (in run.sh it
            # would not: `in_container` is plain `docker exec` with no `-e`, so a `VAR=… in_container`
            # prefix sets it for the docker CLI and never reaches the container). So the one file
            # where the harm was real was the one not scanned. The two harnesses are excluded
            # because guarding a name means writing it, and sweep-transcripts.sh because declaring
            # a seam means naming it.
            for dc_f_path in "$here"/*.sh "$here"/Dockerfile "$here"/container.json; do
                [ -f "$dc_f_path" ] || continue
                dc_f="${dc_f_path##*/}"
                # mutate-verify.sh joins the two harnesses: the record designates it for closing
                # the behavioural half of the verify gap, which means staging a budget in it, and
                # refusing that with "it silently disables the sweep" is false about that file.
                case "$dc_f" in sweep-transcripts.sh|check-config.sh|mutate-config.sh|mutate-verify.sh) continue ;; esac
                dc_f_body="$(dc_strip_comments "$dc_f_path")"
                for dc_seam in $sweep_seams; do
                    grep -qF -- "$dc_seam" <<<"$dc_f_body" \
                        && sweep_problems="$sweep_problems $dc_f sets $dc_seam, which is a self-test seam: in the container it silently disables the sweep while every start still reports success;"
                done
            done
        fi
        grep -qE -- 'CLAUDE_BASE=.*CLAUDE_CONFIG_DIR' <<<"$sweep_body" \
            || sweep_problems="$sweep_problems it does not honour CLAUDE_CONFIG_DIR, which commands.rs, auto-mode.sh and swarm-status.sh all do, so a second config dir sweeps an absent tree and reports success;"
    fi
fi
# THE HOST REAPER POKES THIS CONTAINER, and three names have to agree for that to reach anything.
# run.sh sweeps at container START and that is the only trigger it had, while transcripts are created
# continuously -- the container that produced the E2BIG reached 1,182 of them without ever being
# recreated. `jkb task reap --watch` on the host now sweeps it on every tick
# (crates/jkb-cli/src/transcripts.rs), which it can only do through Docker: ~/.claude-state is a
# named VOLUME with no host path, and the reaper knows a database, not a checkout.
#
# EVERY ONE OF THESE IS READ FROM THE FILE THAT OWNS IT. A reaper poking a container name nothing
# creates, or running a path the image does not carry, is silent for ever -- which is the same
# failure as having no second trigger at all, wearing a green log.
if [ ! -f "$here/../crates/jkb-cli/src/transcripts.rs" ]; then
    sweep_problems="$sweep_problems crates/jkb-cli/src/transcripts.rs is gone, so nothing sweeps the container between starts;"
else
    ctr_rs="$(dc_strip_comments "$here/../crates/jkb-cli/src/transcripts.rs")"
    rs_ctr_name="$(grep -oE 'DEV_CONTAINER_NAME: &str = "[^"]+"' <<<"$ctr_rs" \
        | sed -n '1s/.*"\(.*\)"/\1/p')"
    sh_ctr_name="$(grep -oE '^NAME="\$\{JKB_CONTAINER_NAME:-[^}]+\}"' <<<"$run_stripped" \
        | sed -n '1s/.*:-\(.*\)}"/\1/p')"
    # A LIVE SESSION IS NEVER PLANNED, and the two halves of that live in two languages. The
    # floor's argument -- "the live session is writing one of them right now" -- was written for
    # a sweep that ran at container START, when nothing is open; on the reaper's timer it runs
    # mid-flight, and during a swarm more than KEEP_NEWEST transcripts are touched inside one
    # window. So the reaper passes the ids the registry calls live and the sweep skips them,
    # with a recency window behind it for sessions the registry cannot see. A variable name
    # spelled differently at the two ends protects nothing while both files read correct.
    rs_keep_var="$(grep -oE 'KEEP_SESSIONS_VAR: &str = "[^"]+"' <<<"$ctr_rs" \
        | sed -n '1s/.*"\(.*\)"/\1/p')"
    sh_keep_var="$(grep -oE '^KEEP_SESSIONS="\$\{[A-Z_]+:-' <<<"$sweep_body" \
        | sed -n '1s/^KEEP_SESSIONS="\${\([A-Z_]*\):-/\1/p')"
    if [ -z "$rs_keep_var" ] || [ -z "$sh_keep_var" ]; then
        sweep_problems="$sweep_problems the never-archive list's variable cannot be read from both transcripts.rs and sweep-transcripts.sh, so nothing holds the reaper's keep-list to the one the sweep reads;"
    elif [ "$rs_keep_var" != "$sh_keep_var" ]; then
        sweep_problems="$sweep_problems the reaper sets '$rs_keep_var' while the sweep reads '$sh_keep_var', so a live session's transcript can be archived out from under it;"
    fi
    if [ -z "$rs_ctr_name" ] || [ -z "$sh_ctr_name" ]; then
        sweep_problems="$sweep_problems the container name cannot be read from both run.sh and transcripts.rs, so nothing holds the reaper to the container run.sh creates;"
    elif [ "$rs_ctr_name" != "$sh_ctr_name" ]; then
        sweep_problems="$sweep_problems the reaper pokes '$rs_ctr_name' while run.sh creates '$sh_ctr_name', so the only trigger between container starts reaches nothing and says nothing;"
    fi
    # THE SWEEP ITSELF IS EMBEDDED, not installed, and this is what holds that. The version this
    # replaced baked a copy into the image and exec'd it by path -- which no ALREADY-RUNNING
    # container has, since only a rebuilt image carries it and nothing forces a rebuild: the tick
    # would have exited 127 on every live container, been reported once into reap.log and deduped
    # for ever, with every gate green. `include_str!` leaves no second copy to drift, no rebuild to
    # require and no path to agree about, so the guard that compared two paths is gone with them.
    grep -qF -- 'include_str!("../../../.container/sweep-transcripts.sh")' <<<"$ctr_rs" \
        || sweep_problems="$sweep_problems the reaper no longer embeds the sweep, so it runs something other than the script this repository tests;"
    grep -qF -- '"exec", "-i", "-e", &keep, name, "/bin/bash", "-s"' <<<"$ctr_rs" \
        || sweep_problems="$sweep_problems the reaper no longer feeds the sweep in on stdin, so it depends on a copy inside the container that an already-running one does not have;"
fi

# ...AND IT CARRIES THE LIVE-SESSION LIST. Without it this trigger's only safety is "nothing is open
# at container start", which is false whenever `run.sh` is pointed at a container that is ALREADY
# running — up for days, or started from Docker Desktop. Skipping the sweep there was the first
# repair and it was worse: the container was then swept by nothing on that path, the window stayed
# refused, and re-running `run.sh` — the documented recovery for the very E2BIG this exists to
# prevent — stopped recovering. So the list is passed instead, from the same registry the reaper
# reads. `-e` and not a shell prefix: `in_container` is a plain `docker exec`, so a `VAR=… ` prefix
# sets the variable for the docker CLI and never enters the container.
if [ -n "$sweep_at" ]; then
    grep -qF -- '-e "JKB_KEEP_SESSIONS=' <<<"$(sed -n "${sweep_at}p" <<<"$run_stripped")" \
        || sweep_problems="$sweep_problems run.sh sweeps without passing the live-session list, so on a container that was already running a live transcript can be archived out from under its session;"
    # ...AND THE LIST IS ACTUALLY DERIVED. The flag alone pins nothing that matters: delete the
    # lines that fill `sweep_keep` and `-e "JKB_KEEP_SESSIONS=$sweep_keep"` still passes an empty
    # string for ever, with this guard reporting the protection it no longer has. Reproduced: the
    # composed assertion stayed `ok` and check-config's output was byte-identical to the unmutated
    # tree's.
    #
    # PINNED ON THE VERB, not on a JSON field. This used to read `--json` and pull `.[].session` out
    # with `jq`, so the guard had to hold that field name to `ClaudeSession`'s serde name — a
    # coupling that existed only because the rule was implemented twice. `--live-ids` applies the one
    # rule in crates/jkb-cli/src/transcripts.rs and prints one id per line, so there is no field to
    # agree about and nothing here to keep in step.
    grep -qF -- 'jkb notify sessions --live-ids' <<<"$run_stripped" \
        || sweep_problems="$sweep_problems run.sh never asks jkb which sessions are live, so the list it passes is empty on every start and the protection exists in name only;"
    # ...AND verify.sh MEASURES THE SAME TREE. It reports the budget by running the sweep's own
    # --dry-run, so without the ids it measures a DIFFERENT tree from the one the sweep just acted
    # on: a container held down by live sessions comes out `over` (exit 1) rather than `beyond`
    # (exit 3), with a FAIL naming causes that do not apply. That was a must-fix, and nothing pinned
    # it — deleting only that occurrence left every gate green.
    if [ -n "$verify_at" ]; then
        grep -qF -- '-e "JKB_KEEP_SESSIONS=' <<<"$(sed -n "${verify_at}p" <<<"$run_stripped")" \
            || sweep_problems="$sweep_problems verify.sh is measured without the live-session list, so it judges a different tree from the one the sweep acted on and names causes that cannot apply;"
    fi
    # ...AND THE TWO EMPTY STATES ARE TOLD APART. "no live sessions" and "could not ask the daemon"
    # both produce an empty list, and a sweep that ran UNPROTECTED must not look in the scroll-back
    # like one that had nothing to protect.
    grep -qF -- 'could not ask jkb which sessions are live' <<<"$run_stripped" \
        || sweep_problems="$sweep_problems run.sh does not say when it could not read the registry, so a sweep that ran with no live-session protection is indistinguishable from one with nothing to protect;"
fi

# AND THE OPERATOR IS TOLD. run.sh discards the sweep's exit code with `|| true` -- correctly, since
# a deny list slightly too long must not abort a start -- so the one state in which NO Bash tool call
# works was reported only by a line that scrolled past several steps before the verify the operator
# actually reads. verify.sh now asks, by running the sweep's own `--dry-run` (which moves nothing and
# returns non-zero when the tree as it stands is over budget) rather than re-deriving the
# budget a second time.
if [ ! -f "$here/verify.sh" ]; then
    sweep_problems="$sweep_problems verify.sh is not there to report the deny list at all;"
else
    verify_body="$(dc_strip_comments "$here/verify.sh")"
    { grep -qE -- 'sweep_sh=.*sweep-transcripts\.sh' <<<"$verify_body" \
      && grep -qF -- 'bash "$sweep_sh" --dry-run' <<<"$verify_body"; } \
        || sweep_problems="$sweep_problems verify.sh does not ask whether the deny list still fits in one argv, so the container's health report reads green in the one state where no Bash call works at all;"
    # ...AND REACHES A VERDICT WITH IT. Asking only that the sweep is CALLED let every arm be
    # deleted -- or, more realistically, demoted to a `note` by a refactor -- with this guard still
    # printing ok about a verify.sh that now says nothing. `accept_bad` specifically, because exit 3
    # against exit 1 is the fact run.sh reads to decide whether to open a window.
    # Anchored on the CODE, not on the comment heading above it: `$verify_body` is comment-stripped,
    # so a heading is not there to find. From the `sweep_sh=` assignment to the block's closing `fi`
    # at column 0 -- the inner arms are indented, so they cannot end the extraction early.
    verify_verdict="$(awk '/sweep_sh=/ { inf = 1 } inf { print } inf && /^fi$/ { exit }' \
        <<<"$verify_body")"
    if [ -z "$verify_verdict" ]; then
        sweep_problems="$sweep_problems verify.sh has no deny-list block to read, so the check above establishes only that the name appears;"
    else
        # The quote is appended rather than written into the list, so these NEEDLES are not counted
        # by mutate-config.sh's `grep -c 'bad "'` scan over this file -- that scan counts this
        # file's own FAILURE PATHS, and a string being searched for is not one. Left inline, the
        # loop moved PINNED_BAD_SITES by one and demanded a mutation for a pattern.
        # AND THE PHRASES IT CLASSIFIES ON MUST BE PHRASES THE SWEEP SAYS. verify.sh decides between
        # "over budget", "beyond any sweep's help" and "could not answer" by matching the sweep's own
        # wording -- two literals, living in two files, with nothing comparing them. Reword one and
        # verify silently reclassifies every future container: an over-budget tree becomes "the sweep
        # could not answer", or an unhelpable one becomes a broken boundary that refuses to open a
        # window. Derived rather than spelled here: every `*"…"*` pattern in that block must be text
        # the sweep actually emits.
        # BOTH WAYS IT BORROWS A PHRASE: a `*"…"*` case pattern, and a `grep -F '…'` that pulls a
        # line out of the sweep's output to quote back to the operator. The second arrived when the
        # accepted arm stopped keeping its own copy of the causes, and it is the same coupling — a
        # sentence living in two files with nothing comparing them.
        # FROM THE VERDICT FUNCTION AND THE REPORTING BLOCK BOTH. The classifying `case` arms moved
        # into `sweep_verdict()` when that chain was made pure and testable, and this extraction
        # went on reading only the reporting block — where there were then no `*"…"*` patterns
        # left, so the check established nothing and two mutations went MISSED. Extracted by
        # FUNCTION NAME, like the sweep's own, so it follows the code rather than a line range.
        verify_decide="$(awk '/^sweep_verdict\(\)/ { inf = 1 } inf { print } inf && /^\}/ { exit }' \
            <<<"$verify_body")"
        if [ -z "$verify_decide" ]; then
            sweep_problems="$sweep_problems verify.sh has no sweep_verdict() to read, so nothing establishes which outcome of the sweep reaches which verdict;"
        fi
        verify_markers="$(
            { grep -oE '\*"[^"]+"\*' <<<"$verify_decide$verify_verdict" | sed -E 's/^\*"(.*)"\*$/\1/'
              grep -oE "grep -F '[^']+'" <<<"$verify_verdict" | sed -E "s/^grep -F '(.*)'\$/\1/"
            } )"
        if [ -z "$verify_markers" ]; then
            sweep_problems="$sweep_problems verify.sh's deny-list block classifies on no phrase at all, so every outcome of the sweep reaches the same verdict;"
        else
            # MATCHED AGAINST WHAT THE SWEEP CAN PRINT, which is everything ABOVE its --self-test
            # block. Matched against the whole file, the self-test's own `grep -c '<phrase>'` rows
            # satisfy the check: rewording the real printf then left this guard green, because the
            # phrase was still in the file -- inside the suite that greps for it. Measured; the
            # mutation reported MISSED until this line existed.
            # PINNED AGAINST READING EVERYTHING, which is the failure mode an extraction bounded by
            # a literal has -- the mirror of the empty-read every other extraction here is pinned
            # against. Swap the two tests on that dispatch line (`[ "$#" -eq 1 ] && [ "${1:-}" = …`,
            # identical behaviour) and the address stops matching, `q` never fires, and this silently
            # becomes the whole file again: the state in which the self-test's own `grep -c '<phrase>'`
            # rows satisfy the marker check and rewording the real printf passes.
            sweep_emit="$(sed -n '/^if \[ "\${1:-}" = "--self-test" \]/q;p' <<<"$sweep_body")"
            if [ "$(grep -c . <<<"$sweep_emit")" -ge "$(grep -c . <<<"$sweep_body")" ]; then
                sweep_problems="$sweep_problems the --self-test dispatch line is no longer where the emitting half of sweep-transcripts.sh ends, so the check below reads the self-test's own grep patterns as things the sweep prints;"
                sweep_emit=""
            fi
            while IFS= read -r dc_marker; do
                [ -n "$dc_marker" ] || continue
                grep -qF -- "$dc_marker" <<<"$sweep_emit" \
                    || sweep_problems="$sweep_problems verify.sh classifies on \"$dc_marker\", which sweep-transcripts.sh never prints, so that verdict is unreachable and its cases fall to another;"
            done <<<"$verify_markers"
        fi
        # ANCHORED, because `accept_bad "` CONTAINS `bad "`. Unanchored, the accept_bad arm alone
        # satisfied the `bad` iteration, so both plain `bad` arms could be demoted to notes -- the
        # exact drift this loop was written against -- with the gate still printing 70/70 and the
        # single mutation here (which demotes accept_bad) still caught by the survivors. A guard
        # whose three checks were really two, in the round that added it to make three.
        #
        # WHAT THIS STILL CANNOT SEE, said plainly: it establishes that the block reaches each KIND
        # of verdict, never that the BUDGET arms are the ones reaching them. Demote two of the three
        # plain `bad` calls and the third satisfies the grep. A static read cannot do better; the
        # behavioural half -- an over-budget tree really exiting 1 and an unreclaimable one really
        # exiting 3, which is what run.sh reads to decide whether to open a window -- needs a
        # container, so it belongs in mutate-verify.sh and is not covered here.
        # The quote is appended rather than written into the pattern, so these NEEDLES are not
        # counted by mutate-config.sh's scan for this file's own failure paths.
        # THE REAPER'S CLASSIFIERS TOO. transcripts.rs decides "nothing happened" from the sweep's
        # stdout, on phrases living in two files -- the same coupling this block already holds
        # verify.sh to, and the same silent reclassification if one end is reworded.
        # FROM THE WHOLE DECLARATION, which spans lines once it has a comment in it — it did not
        # when this was written, and the day a third phrase arrived the extraction read a line with
        # no quoted strings on it and reported that the reaper declares none.
        ctr_markers="$(awk '/^const NOTHING_TO_DO/ { inf = 1 } inf { print } inf && /\];/ { exit }' \
            <<<"$ctr_rs" | grep -oE '"[^"]+"' | tr -d '"')"
        # ...AND THE OTHER DIRECTION. The phrases above are the reaper's list of what counts as a
        # quiet tick; the sweep is where quiet exits are ADDED. A fourth one with no marker is one
        # identical log line every quarter of an hour for ever, which is the noise the dedup exists
        # to prevent, on the one path it does not cover. Counting them forces the decision.
        sweep_quiet="$(awk '/^sweep_transcripts\(\)/ { inf = 1 } inf { print } inf && /^\}/ { exit }' \
            <<<"$sweep_body" | grep -c 'return 0')"
        if [ "$sweep_quiet" -ne 4 ]; then
            sweep_problems="$sweep_problems sweep_transcripts() has $sweep_quiet success exits, pinned at 4 — a new one needs a phrase in transcripts.rs NOTHING_TO_DO or the reaper logs it every tick for ever;"
        fi
        if [ -z "$ctr_markers" ]; then
            sweep_problems="$sweep_problems transcripts.rs declares no NOTHING_TO_DO phrases, so every tick reads as something happening;"
        else
            while IFS= read -r dc_marker; do
                [ -n "$dc_marker" ] || continue
                grep -qF -- "$dc_marker" <<<"$sweep_emit" \
                    || sweep_problems="$sweep_problems transcripts.rs treats \"$dc_marker\" as the sweep having nothing to do, which the sweep never prints, so a quiet tick is logged as an event;"
            done <<<"$ctr_markers"
        fi
        dc_q='"'
        for dc_verdict in ok bad accept_bad; do
            grep -qE -- "(^|[[:space:]])$dc_verdict $dc_q" <<<"$verify_verdict" \
                || sweep_problems="$sweep_problems verify.sh's deny-list block reaches no \`$dc_verdict\` verdict, so it runs the sweep and reports nothing a caller can act on;"
        done
    fi
fi

if [ -z "$sweep_problems" ]; then
    ok "run.sh sweeps transcripts before verifying, the sweep enumerates only transcripts at every depth, and verify.sh reports the budget"
else
    bad "the transcript sweep does not hold:$sweep_problems — see .container/sweep-transcripts.sh"
fi

# ...AND THE IDIOM THAT MADE THAT GUARD LIE is refused for the whole repository, in
# `scripts/tests/dev-scripts.test.sh`, not here. The scan that stood in this place covered
# `"$here"/*.sh` and matched only the `dc_strip_comments | grep -q` spelling, so it could not
# have caught the instance that shipped in `scripts/hooks/post-merge` — a different spelling in a
# directory it did not read. Two half-guards with the bug in the gap between them is what this
# repository's doc rules call the defect; one home, one glob (`shell_sources`, 41 files across all
# five script directories), one message.
#
# Its narrowing argument is also corrected there: this block exempted `printf "$var" | grep -q` as
# "a single write, safe", and post-merge was exactly that shape. Measured, bash 5.2.21, 30 trials:
# 0/30 failures at 16 KB and 28-30/30 at 32-82 KB, with `pipefail` set and 0/30 without it at any
# size. The deciding condition is `pipefail`, not the producer.

# THE ENTRYPOINT LINE ITSELF. `ENTRYPOINT [\"/usr/local/bin/entrypoint.sh\"]` appears exactly once
# and was referenced by no check: delete it in a rebase or a base-image bump and the build
# succeeds, both config harnesses stay green, and run.sh raises the firewall itself so its path
# looks identical — while `docker start`, Docker Desktop's start button and a daemon restart, the
# three routes the entrypoint exists for, come back with unrestricted egress.
if grep -qE '^ENTRYPOINT.*entrypoint\.sh' "$here/Dockerfile"; then
    ok "the image runs entrypoint.sh as its ENTRYPOINT"
else
    bad "the Dockerfile does not set ENTRYPOINT to entrypoint.sh — docker start would come up with no firewall"
fi

# THE REAPER PATH IS NOT GUARDED HERE EITHER, and for the same reason as the verdict path below:
# it is single-sourced as the Dockerfile's `ENV JKB_REAPER`, so there are no two spellings to keep
# in step. (Why a guard was tried first and deleted is in README.md, not here.)
#
# THE VERDICT PATH IS NO LONGER GUARDED HERE, because it is no longer duplicated (D52.5). This
# carried a check that init-firewall.sh, entrypoint.sh and verify.sh all named /run/jkb-egress-verdict
# identically, justified by a comment reading "three different processes [that] cannot share a
# variable". That premise was false: the writer already sourced egress-lib.sh, entrypoint.sh is
# installed BESIDE it in /usr/local/bin, and verify.sh runs from the checkout that carries it. The
# path is now `VERDICT_PATH` in that library and there is one spelling, so there is nothing for a
# guard to compare -- and consolidating surfaced a real inconsistency the guard could not see, that
# verify.sh alone ignored the JKB_EGRESS_VERDICT override.
#
# Removing the possibility beats guarding it; the guard went in the same commit as the duplication.

# ...and on the STATES that path carries. The path agreeing is not enough: a reader with no arm for
# a state the writer records drops it into `*`, which both readers treat as unknown — and unknown
# under D50.3 is a container that refuses to boot, permanently, over a word nobody taught them.
# Single-sourced from egress-lib.sh's VERDICT_STATES so this is not a fourth list to keep in
# step; egress-lib.sh's own self-test is what stops verdict_state returning something absent from it.
verdict_states="$(grep -oE '^(readonly )?VERDICT_STATES="[^"]*"' "$here/egress-lib.sh" 2>/dev/null \
                  | head -1 | sed 's/.*="//; s/"$//')"
if [ -z "$verdict_states" ]; then
    bad "egress-lib.sh no longer declares VERDICT_STATES — the check that both readers handle every verdict is now checking nothing"
else
    states_ok=1
    for st in $verdict_states; do
        for rf in entrypoint.sh verify.sh; do
            # A case arm for the state, i.e. the bare word followed by `)` — not a mention of it
            # in a comment or a message, which is how a reader can look like it handles a state
            # it only talks about.
            grep -qE "^[[:space:]]*(\*\|)?$st\)" "$here/$rf" 2>/dev/null \
                || { bad "$rf has no case arm for the '$st' verdict — it would read that state as unknown and refuse"; states_ok=0; }
        done
    done
    [ "$states_ok" -eq 1 ] && ok "both readers handle every verdict state ($verdict_states)"
fi

# THE HOST DAEMON'S OPENING (design r3.2 H5), four properties no container is needed to read.
#
# `repo_top`, not `root`: `root` is reassigned by the allowWrite loop above, and a check reading it
# here would look for the daemon's source under a posture path and report the port unreadable.
repo_top="$(cd "$here/.." && pwd)"

# 1. verify.sh handles every daemon state — the VERDICT_STATES rule, for the second vocabulary.
daemon_states="$(grep -oE '^(readonly )?DAEMON_STATES="[^"]*"' "$here/egress-lib.sh" 2>/dev/null \
                 | head -1 | sed 's/.*="//; s/"$//')"
if [ -z "$daemon_states" ]; then
    bad "egress-lib.sh no longer declares DAEMON_STATES — the check that verify.sh handles every daemon state is now checking nothing"
else
    dstates_ok=1
    for st in $daemon_states; do
        grep -qE "^[[:space:]]*(\*\|)?$st\)" "$here/verify.sh" 2>/dev/null \
            || { bad "verify.sh has no case arm for the '$st' daemon state — it would report it as unestablished"; dstates_ok=0; }
    done
    [ "$dstates_ok" -eq 1 ] && ok "verify.sh handles every daemon state ($daemon_states)"
fi

# 2. The port the firewall opens is the port the daemon binds. Two spellings in two languages, so
#    one is read out of each and compared; an empty read on either side is a failure, never a match.
fw_port="$(grep -oE '^DAEMON_PORT=[0-9]+$' "$here/egress-lib.sh" 2>/dev/null | head -1 | cut -d= -f2)"
rs_port="$(grep -oE 'DEFAULT_ADDR: &str = "127\.0\.0\.1:[0-9]+"' "$repo_top/crates/jkb-daemon/src/lib.rs" 2>/dev/null \
           | head -1 | sed 's/.*://; s/"$//')"
if [ -z "$fw_port" ] || [ -z "$rs_port" ]; then
    bad "could not read the daemon port from both egress-lib.sh (DAEMON_PORT='$fw_port') and jkb-daemon's DEFAULT_ADDR ('$rs_port') — the check that the firewall opens the port jkb serve binds is checking nothing"
elif [ "$fw_port" != "$rs_port" ]; then
    bad "egress-lib.sh opens DAEMON_PORT=$fw_port but jkb serve binds $rs_port (crates/jkb-daemon/src/lib.rs) — the container could not reach the daemon"
else
    ok "the firewall opens the port jkb serve binds ($fw_port)"
fi

# 2b. The daemon's file root is the container's bind. A task write from the container may have the
#    host's sync write a file only under $HOME/<CLIENT_FILE_ROOT> (jkb-daemon), because that is the
#    host directory the container sees — which is true only while container.json binds exactly that
#    directory at the same place under the container's home. Read out of the Rust source, not copied.
client_root="$(grep -oE 'pub const CLIENT_FILE_ROOT: &str = "[^"]+"' "$repo_top/crates/jkb-daemon/src/lib.rs" 2>/dev/null \
               | head -1 | sed 's/.*"\(.*\)"$/\1/')"
bind_srcs="$(dc_mount_sources "$here/container.json" | sed -n '/|volume$/!s/|[^|]*$//p')"
if [ -z "$client_root" ]; then
    bad "could not read CLIENT_FILE_ROOT from crates/jkb-daemon/src/lib.rs — the check that the daemon admits file-backed writes only where the container can see is checking nothing"
elif ! grep -qxF "\${localEnv:HOME}/$client_root" <<<"$bind_srcs" || ! grep -qxF "/home/vscode/$client_root" <<<"$mount_targets"; then
    bad "jkb serve admits a client's file-backed task writes under \$HOME/$client_root (CLIENT_FILE_ROOT), but container.json does not bind \${localEnv:HOME}/$client_root at /home/vscode/$client_root — the daemon would judge a directory the container does not see"
else
    ok "the daemon's client file root is the directory the container binds (~/$client_root)"
fi

# 3. The nested sandbox can reach it: its proxy tunnels only to allowedDomains, and the firewall
#    keeps that same entry out of the IP allowlist by address, so the one entry serves both layers.
fw_host="$(grep -oE '^DAEMON_HOST=[A-Za-z0-9.-]+$' "$here/egress-lib.sh" 2>/dev/null | head -1 | cut -d= -f2)"
if [ -z "$fw_host" ]; then
    bad "egress-lib.sh no longer declares DAEMON_HOST — the check that the sandbox may reach the daemon is checking nothing"
elif jq -e --arg h "$fw_host" '.require.sandbox.network.allowedDomains | index($h)' \
        "$repo_top/scripts/auto-mode-posture.json" >/dev/null 2>&1; then
    ok "the posture lets the nested sandbox's proxy reach the host daemon ($fw_host)"
else
    bad "scripts/auto-mode-posture.json's allowedDomains does not name $fw_host — jkb run from Bash in the nested sandbox could not reach the host daemon"
fi

# 3b. A Linux engine resolves that name only through --add-host, so the pinned flag must name the
#     host the firewall looks up; a rename on one side leaves CI's raise `unresolved` with every
#     static gate green.
dc_args_h="$(dc_run_args "$here/container.json" "$repo_top" 2>/dev/null)" || dc_args_h=""
add_host="$(sed -n 's/^--add-host=\([^:]*\):host-gateway$/\1/p' <<<"$dc_args_h" | head -1)"
if [ -z "$add_host" ]; then
    bad "container.json's runArgs pin no --add-host=<name>:host-gateway — a Linux engine would not resolve ${fw_host:-the daemon host}, and the firewall's daemon rule would hold no address"
elif [ "$add_host" != "${fw_host:-}" ]; then
    bad "container.json pins --add-host for $add_host but egress-lib.sh looks up DAEMON_HOST=${fw_host:-<unread>} — on a Linux engine the daemon rule would hold no address"
else
    ok "the pinned --add-host names the host the firewall looks up ($add_host)"
fi

# 4. VS Code does not forward the daemon's port. Measured on the Mac, 2026-09-14: after something in
#    here listened on 7117, VS Code held the HOST'S 127.0.0.1:7117, so com.jkb.serve crash-looped on
#    EADDRINUSE and connections hung. Attaching reads no `portsAttributes` from this file, so it is
#    carried as the `devcontainer.metadata` label on the container, which attaching does read.
dc_args="$(dc_run_args "$here/container.json" "$repo_top" 2>/dev/null)" || dc_args=""
dc_meta="$(grep -A1 -xF -- '--label' <<<"$dc_args" | sed -n 's/^devcontainer\.metadata=//p' | head -1)"
if [ -z "$dc_meta" ]; then
    bad "container.json's runArgs carry no devcontainer.metadata label — VS Code would auto-forward the daemon port and take the host's 127.0.0.1:${fw_port:-7117} from com.jkb.serve"
elif jq -e --arg p "${fw_port:-}" 'any(.[]; .portsAttributes[$p].onAutoForward == "ignore")' <<<"$dc_meta" >/dev/null 2>&1; then
    ok "VS Code is told not to forward the daemon port (${fw_port})"
else
    bad "the devcontainer.metadata label does not set portsAttributes.\"${fw_port:-?}\".onAutoForward to ignore — VS Code would auto-forward the daemon port and take the host's 127.0.0.1:${fw_port:-?} from com.jkb.serve"
fi

# 5. The container is in remote mode, at the address the firewall opens (tasks S6.5). Every `jkb` in
#    here and the notification hook reach the daemon through REMOTE_VAR; without it the hook looks
#    on the container's OWN loopback — where nothing listens, so every notification is lost with
#    nothing on screen to say so — and every other command opens a database of its own. Held to
#    the same two constants the firewall reads, not to a copy of them — and the variable's NAME is
#    read out of the binary's source, because the hook's address drifted once (JKB_DAEMON_URL in
#    the code, nothing in the config) with every check here green.
dc_env="$(dc_container_env "$here/container.json" "$repo_top" 2>/dev/null)" || dc_env=""
remote_var="$(grep -oE 'pub const REMOTE_VAR: &str = "[A-Z_]+"' "$repo_top/crates/jkb-cli/src/remote.rs" 2>/dev/null \
    | head -1 | sed 's/.*"\([A-Z_]*\)"/\1/')"
daemon_addr="$( [ -n "$remote_var" ] && sed -n "s/^$remote_var=//p" <<<"$dc_env" | head -1)"
if [ -z "$remote_var" ]; then
    bad "could not read REMOTE_VAR from crates/jkb-cli/src/remote.rs — the check that the container is in remote mode is checking nothing"
elif [ -z "$daemon_addr" ]; then
    bad "container.json's containerEnv sets no $remote_var (the variable that switches remote mode on) — jkb in the container would open a database of its own, and the notification hook would look for jkb serve on the container's own loopback"
elif [ "$daemon_addr" != "${fw_host:-?}:${fw_port:-?}" ]; then
    bad "container.json's $remote_var is $daemon_addr but the firewall opens ${fw_host:-<unread>}:${fw_port:-<unread>} (egress-lib.sh) — every jkb command and notification would be refused"
else
    ok "remote mode is pointed at the address the firewall opens ($daemon_addr)"
fi

# 6. ...and nothing gives it a database of its own. Remote mode refuses JKB_DB outright, so one left
#    in containerEnv fails every command; and a volume at the old container-local KB's path keeps a
#    second knowledge base alive for anything that is not jkb to write into.
if grep -q '^JKB_DB=' <<<"$dc_env"; then
    bad "container.json's containerEnv sets JKB_DB — remote mode refuses every jkb command with JKB_DB set, and the container must not name a database"
elif [ -n "$dc_env" ]; then
    ok "the container names no database of its own (no JKB_DB)"
fi
# JKB_VERIFY_NO_DAEMON is for a harness with no host daemon behind it (mutate-verify.sh). Declared
# here it turns "no jkb command in this container can reach the knowledge base" into a note in the
# real container, so it is refused. (An empty containerEnv already fails section 5.)
# Every place the image or the launcher can set a variable: containerEnv, runArgs, the Dockerfile.
if grep -q '^JKB_VERIFY_NO_DAEMON=' <<<"$dc_env"; then
    bad "container.json's containerEnv sets JKB_VERIFY_NO_DAEMON — that is the mutation harness's statement that no host daemon exists, and in the real container it hides a knowledge base nothing can reach"
elif grep -q 'JKB_VERIFY_NO_DAEMON' <<<"$dc_args"; then
    bad "container.json's runArgs set JKB_VERIFY_NO_DAEMON — the mutation harness's waiver, which in the real container hides a knowledge base nothing can reach"
elif grep -qE '^[[:space:]]*(ENV|ARG)[[:space:]].*JKB_VERIFY_NO_DAEMON' "$here/Dockerfile"; then
    bad "the Dockerfile sets JKB_VERIFY_NO_DAEMON — the mutation harness's waiver, baked into every container"
elif [ -n "$dc_env" ] && [ -n "$dc_args" ]; then
    ok "the container does not waive the daemon check (no JKB_VERIFY_NO_DAEMON)"
fi
# By source name AND by target: a renamed volume at the old path is the same second database.
if grep -q '^jkb-kb-local|' <<<"$(dc_mount_sources "$here/container.json")" \
    || grep -qx '/home/vscode/.local/state/jkb' <<<"$mount_targets"; then
    bad "container.json still mounts the container-local knowledge base (jkb-kb-local, or a mount at /home/vscode/.local/state/jkb) — it was retired at the cutover (tasks S6.5)"
else
    ok "the retired container-local knowledge base volume is not mounted"
fi

# THE PROBE LOOKS FOR THE RULE THE RAISE INSTALLS. init-firewall.sh installs the chain with
# `iptables -A OUTPUT <spec>` and egress-lib.sh reads it back with `iptables -C OUTPUT <spec>`;
# spelled separately those are two statements that have to agree, and they did not — the probe
# asked for `--match-set allowed-new`, the raise's STAGING set, which is swapped into `allowed` and
# destroyed before the raise returns. So `allowlist_state` could never answer `yes`: every healthy
# container reported `denied` rather than `allowlisted`, printed "egress is DENIED" at every boot,
# and drifted against its own record for ever.
#
# The specs are now single constants both sides expand, so drift is impossible while both sides
# USE them — which is what this asserts. It does not re-check the specs' contents (that would be a
# second copy of the thing being checked); it requires that no site spells one out inline.
rules_ok=1
for f in init-firewall.sh egress-lib.sh; do
    seen=0
    # Every OUTPUT-chain install or probe must expand a $RULE_* constant rather than name a match
    # or a target itself. `-F`/`-P` and the DNS/loopback/conntrack openings are not rules the probe
    # reads back, so they are not in scope: this is only about the specs that BOTH sides state.
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        seen=$((seen+1))
        case "$line" in
            *'$RULE_'*) ;;
            *'-j REJECT'*|*'--match-set'*)
                bad "$f spells an OUTPUT rule inline ($(printf '%s' "$line" | sed 's/^[[:space:]]*//')) instead of expanding a \$RULE_* constant — the probe and the raise can then disagree, which is how allowlist_state came to be unable to fire"
                rules_ok=0 ;;
        esac
    done <<EOF
$(dc_strip_comments "$here/$f" | grep -E '(iptables|ip6tables)[[:space:]].*[[:space:]]OUTPUT([[:space:]]|$)')
EOF
    # MATCHED ON THE CHAIN OPERAND, not on one spelling of the flag. This selected `-A OUTPUT ` and
    # `-C OUTPUT ` literally, so writing the probe as `iptables --check OUTPUT ...` -- iptables'
    # own long form for the same thing -- meant the line was never extracted and therefore never
    # examined, and the guard printed ok having looked at nothing. The shipped defect this exists
    # for was re-introducible one flag spelling along.
    #
    # Broadening is safe: lines that touch OUTPUT without stating a shared spec (-F, -P, the DNS
    # and loopback openings) carry neither REJECT nor --match-set and fall through the case arms.
    #
    # PINNED AGAINST AN EMPTY EXTRACTION, like every other derived list here. Routing the calls
    # through a wrapper variable, or a line continuation, yields no lines at all -- the loop never
    # runs and the same ok prints.
    if [ "$seen" -eq 0 ]; then
        bad "no OUTPUT-chain rule lines could be read out of $f — the check that the raise and the probe state one spec just certified nothing"
        rules_ok=0
    fi
done
[ "$rules_ok" -eq 1 ] && ok "the raise installs and the probe reads back one shared rule spec"

# THE APPARMOR PROFILE IS docker-default WITH ONE RULE CHANGED, and the point is the "one".
# Switching AppArmor off entirely would also have let bubblewrap start; what was chosen instead was
# a profile keeping every other docker-default restriction. That choice is worth nothing unless the
# file still reflects it, and it degrades silently: a profile edited into permissiveness has the
# same name, loads fine, and verify.sh's name check would still pass. (verify.sh also probes one
# restriction at runtime; this is the static half, and it can see the whole file.)
aa_file="$here/apparmor-jkb-dev"
aa_name="$(dc_apparmor_profile "$aa_file")"
if [ -z "$aa_name" ]; then
    bad "$aa_file declares no profile — run.sh, mutate-verify.sh and verify.sh all derive the profile name from it, so they would pass an empty one to docker"
else
    aa_ok=1
    # The deliberate difference: the mount FAMILY. `pivot_root` is a rule type of its own in
    # AppArmor and is NOT covered by `mount,` -- docker-default names no pivot_root rule, and an
    # unnamed rule type is denied. Both are asserted because they were needed in sequence and each
    # was measured: allowing `mount,` alone moved bwrap's failure from `Failed to make / slave` to
    # `pivot_root: Permission denied`, and a profile with only the first reads exactly like a
    # profile that works. `umount,` is upstream's own and is covered by the kept-restrictions list.
    for allow in mount pivot_root; do
        grep -qE "^[[:space:]]*$allow," "$aa_file" \
            || { bad "$aa_file does not allow \`$allow\` — bubblewrap cannot start under it, which is the whole reason the profile exists"; aa_ok=0; }
        # An explicit `deny` cannot be overridden later in AppArmor, so a deny must be ABSENT
        # rather than merely followed by an allow.
        grep -qE "^[[:space:]]*deny[[:space:]]+$allow," "$aa_file" \
            && { bad "$aa_file still denies \`$allow\` — an explicit deny wins over any later allow in AppArmor, so bubblewrap would still fail"; aa_ok=0; }
    done
    # ...and the restrictions it is supposed to KEEP. Named members, not a count, for the reason
    # the seccomp check gives: a threshold passes while silently losing the entries under it.
    # ANCHORED ON THE RULE, not on a mention of it, and with NO HAND-ROLLED REGEX ESCAPING --
    # both halves were defects the mutation caught. Grepping the whole file matched these words in
    # the profile's own header, where it explains what `apparmor=unconfined` would discard. The fix
    # for that then built a pattern with `sed 's/[]\/[.*^$]/\\&/g'`, which BSD sed REJECTS as
    # unbalanced brackets: the substitution produced nothing, the pattern collapsed to
    # `^[[:space:]]*deny[[:space:]].*`, and the guard matched any deny line at all -- passing with
    # every restriction below deleted. Selecting the deny rules and then matching FIXED-string
    # needs no escaping and cannot collapse. The first version grepped the whole file, and
    # every one of these words appears in the profile's own header where it explains what
    # `apparmor=unconfined` would have discarded -- so deleting the actual `deny` line left the
    # guard green. Caught by its mutation on the first run, which is the shape this directory
    # produces most often.
    # `network alg`, `network vsock` and `powercap` are on this list because the hand-written
    # profile OMITTED all three and the list as first written did not ask for them -- a check
    # derived from the same memory as the thing it checks agrees with it. They are named here so
    # that a regeneration silently losing them is caught even before the CI drift check runs.
    for r in 'sysrq-trigger' 'kcore' '/sys/firmware' '/sys/kernel/security' \
             'network alg' 'network vsock' 'powercap'; do
        grep -qF -e "$r" <<<"$(grep -E '^[[:space:]]*deny[[:space:]]' "$aa_file")" \
            || { bad "$aa_file no longer denies $r — it is supposed to be docker-default with ONE rule relaxed, not a permissive profile wearing its name"; aa_ok=0; }
    done
    # (That the profile is GENERATED rather than hand-maintained is asserted below, by the derived
    # check over every generator -- not here, where it would be a second rule about one of them.)
    #
    # THAT ci.yml NAMES THIS PROFILE IS NO LONGER A QUESTION. It used to spell the flag out in four
    # hand-written bubblewrap arms, and the guard for it -- "every probe invocation names the
    # declared profile" -- had to be made true twice, because the name first appeared only in an
    # arm's LABEL and then moved into a shared array a `grep -F` matched at the ASSIGNMENT. Both
    # times an arm claiming to be the shipped configuration was measuring something else while the
    # gate stayed green. ci.yml runs `mutate-verify.sh --ladder` now, whose rungs are the control
    # minus a named flag, so no file outside run.sh spells the profile at all (D54.3).
    [ "$aa_ok" -eq 1 ] && ok "the AppArmor profile is docker-default with only \`mount\` relaxed ($aa_name)"
fi

# THE CALL SHAPE OF `dc_require_apparmor_profile`, because the callee cannot enforce its own refusal.
#
# It ends with `exit 1` on a name it cannot read, and its comment claims that means "no future
# caller can forget". A caller CAN: inside a command substitution `exit` ends only the subshell, so
# `AA_ARGS=(--security-opt "apparmor=$(dc_require_apparmor_profile …)")` printed the five-line
# refusal and then handed docker `apparmor=`, which docker reads as its DEFAULT profile --
# docker-default, whose `mount` denial is exactly what the profile exists to lift. mutate-verify.sh
# did that, and being `set -uo pipefail` with no `-e`, nothing stopped it.
#
# There is no way to fix this in the callee, so the RULE IS CHECKED HERE: every call must be a plain
# scalar assignment, whose failure either trips `set -e` (run.sh) or is checked explicitly with
# `|| exit` (mutate-verify.sh, which has no `-e`). Anything else -- an array element, a nested
# expansion, an argument -- discards the status. A comment beside each call site is precisely what
# already failed.
#
# THREE FILES ARE EXEMPT, and no exemption can hide a real call, because NONE OF THE THREE RUNS
# DOCKER -- so none of them has anything to spend a profile name on:
#
#   lib.sh            defines the function.
#   check-config.sh   is this file: it names the function in its own pattern and its own message.
#   mutate-config.sh  contains the bad form ON PURPOSE, as the payload of the mutation that proves
#                     this guard fires. A harness that writes the defect it hunts will always match
#                     a grep for that defect.
#
# The same self-matching trap as the `GENERATED FILE` marker, which matched the two checkers looking
# for it, and it cost two rounds here as well: a guard that greps the tree has to say what it is not
# asking about, and "everything except the things that talk about it" is the answer every time.
shape_ok=1
while IFS= read -r hit; do
    f="${hit%%:*}"; rest="${hit#*:}"; n="${rest%%:*}"; line="${rest#*:}"
    case "$f" in */lib.sh|*/check-config.sh|*/mutate-config.sh) continue ;; esac
    case "$line" in
        *'#'*dc_require_apparmor_profile*) continue ;;   # prose about it, not a call
    esac
    if ! grep -qE '^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*="\$\(dc_require_apparmor_profile ' <<<"$line"; then
        bad "$f:$n calls dc_require_apparmor_profile somewhere its \`exit 1\` cannot stop the script — it must be a plain assignment (\`name=\"\$(dc_require_apparmor_profile …)\"\`), or the empty name it refuses reaches docker as \`apparmor=\`, which is docker-default"
        shape_ok=0
    fi
done < <(grep -rn 'dc_require_apparmor_profile' "$here" "$here/../scripts" "$here/../.github" 2>/dev/null || true)
[ "$shape_ok" -eq 1 ] && ok "every dc_require_apparmor_profile call is shaped so its refusal actually stops the caller"

# ONE DEFINITION OF "DOES APPARMOR MEDIATE". run.sh, verify.sh and mutate-verify.sh each had their
# own, and verify.sh's was NOT the same rule -- it preferred /proc/self/attr/apparmor/current, so
# the launcher that decides whether to pass `--security-opt` and the verifier that decides what it
# should see answered one question from different primary evidence about the same host. They call
# `dc_apparmor_mediates` in lib.sh now; this is what stops a fourth copy reappearing beside them.
# ci.yml is exempt: a workflow step cannot source shell, and its copy is asserted separately below.
# Exempt for the same three reasons as the call-shape guard above: lib.sh is the definition, this
# file names the path in its own pattern, and mutate-config.sh carries the second predicate as the
# payload of the mutation that proves this guard fires.
stray="$(grep -rln 'apparmor/parameters/enabled' "$here" 2>/dev/null \
           | grep -v -e '/lib.sh$' -e '/check-config.sh$' -e '/mutate-config.sh$' || true)"
if [ -n "$stray" ]; then
    bad "these read /sys/module/apparmor/parameters/enabled directly instead of calling dc_apparmor_mediates, so they can drift from the launcher's answer about the same host: $(printf '%s' "$stray" | tr '\n' ' ')"
else
    ok "\`does AppArmor mediate\` has one definition (dc_apparmor_mediates), and .container/ has no second copy"
fi

# EVERY VENDORED ARTIFACT IS GENERATED, AND CARRIES WHAT THE DRIFT CHECK NEEDS.
#
# `.container/` vendors files derived from moby's upstream policies. Vendoring is deliberate (the
# policy is reviewable in a diff, and a build works offline) but a vendored file can become a lie
# two ways -- hand-edited, or upstream moved -- and neither is visible by reading it. The AppArmor
# profile was first TRANSCRIBED BY HAND and was missing three deny rules, the ABI declaration and
# the runc/crun signal peers; every static guard passed, because every static guard was written
# from the same understanding as the file.
#
# What actually closes that is check-drift.sh, which regenerates from upstream and compares -- and
# it needs the network, so it runs in CI. THIS is the offline half: the preconditions that check
# has to have, asserted so a generator or artifact cannot quietly stop satisfying them between CI
# runs. DERIVED OVER THE GENERATORS, so a third one joins both checks by existing rather than by
# somebody remembering to add it to a list.
# NOTHING HERE EXECUTES A GENERATOR. The first version asked each one `--print-target`, which is
# how check-drift.sh discovers artifacts -- and that is right for a check that is about to run them
# anyway, and wrong here. This file runs 61 times inside mutate-config.sh, and a mutation that
# leaves the flag's grep satisfied but its branch broken would make the generator RUN: a network
# fetch and a rewritten policy file, as a side effect of a static check. It timed out on the first
# try. Both halves are derived from the text instead, and pairing is by recorded URL rather than by
# asking a generator which file is its.
gen_ok=1
gen_n=0

for gen in "$here"/generate-*.sh; do
    [ -e "$gen" ] || continue
    gen_n=$((gen_n+1))
    gname="$(basename "$gen")"
    [ -x "$gen" ] || { bad "$gname is not executable — nothing can regenerate its artifact, so drift in it is undetectable"; gen_ok=0; }
    grep -q -- '--print-target' "$gen" \
        || { bad "$gname does not support --print-target — check-drift.sh discovers artifacts by asking, so this generator sits outside the drift check while looking inside it"; gen_ok=0; }
    gurl="$(sed -n 's|^url="\([^"]*\)".*|\1|p' "$gen" | head -1)" || gurl=""
    # PAIRED FROM THE GENERATOR, which is the authority on what it writes. The first version found
    # artifacts by scanning .container/* for the "GENERATED FILE" marker -- and matched THIS FILE
    # and mutate-config.sh, which both contain that string in an assertion and a mutation. Same
    # defect as grepping the whole AppArmor profile and matching its own header: a guard that reads
    # a marker anywhere in a file reads its own description of the marker.
    gout="$(sed -n 's|^out="\$here/\([^"]*\)".*|\1|p' "$gen" | head -1)" || gout=""
    if [ -z "$gurl" ] || [ -z "$gout" ]; then
        bad "$gname does not declare both \`url=\` and \`out=\"\$here/...\"\` — its upstream and its artifact cannot be paired without running it"
        gen_ok=0; continue
    fi
    art="$here/$gout"
    if [ ! -f "$art" ]; then
        bad "$gname writes $gout, which does not exist — the drift check would have nothing to compare"; gen_ok=0; continue
    fi
    grep -qF -e 'GENERATED FILE -- DO NOT EDIT' "$art" \
        || { bad "$gout does not declare itself generated — a hand-edit would read as ordinary content"; gen_ok=0; }
    # The digest is what lets the drift check tell "upstream moved" from "somebody edited this",
    # which are repaired by looking at different diffs. Without it a difference is unattributable.
    grep -qE 'upstream-sha256: [0-9a-f]{64}' "$art" \
        || { bad "$gout records no upstream-sha256 — a difference from its generator could not be attributed to upstream or to a local edit"; gen_ok=0; }
    grep -qF -e "Source: $gurl" "$art" \
        || { bad "$gout does not record \`Source: $gurl\`, the URL $gname fetches — the drift check could not tell whose upstream it came from"; gen_ok=0; }
done

# AND NOTHING GENERATED IS ORPHANED. The loop above is over GENERATORS, so deleting one leaves its
# artifact in the tree with nothing checking it -- silently, because a smaller set still passes.
# Counting from the artifact side closes that. `*.sh` is excluded: a generated script is not a
# thing in this design, and both checkers contain the marker string in their own assertions, so
# scanning them found four "artifacts" and two generators the first time this was written.
art_n=0
for art in "$here"/*; do
    [ -f "$art" ] || continue
    case "$art" in *.sh) continue ;; esac
    grep -qF -e 'GENERATED FILE -- DO NOT EDIT' "$art" 2>/dev/null && art_n=$((art_n+1))
done
if [ "$art_n" -ne "$gen_n" ]; then
    bad "$gen_n generator(s) but $art_n generated artifact(s) in $here — one of them is outside the drift check"
    gen_ok=0
fi
# A loop that iterated over nothing must not report success — `generate-*.sh` matching no files is
# the state in which this check is most likely to be believed and least entitled to be.
if [ "$gen_n" -eq 0 ]; then
    bad "no generate-*.sh found in $here — the vendored policies would be hand-maintained with nothing checking them"
elif [ "$gen_ok" -eq 1 ]; then
    ok "every vendored artifact declares itself generated and records its upstream ($gen_n generator(s))"
fi

# THE TWO SELF-TEST LISTS AGREE (D51.9). scripts/check.sh and .github/workflows/ci.yml each
# enumerate the `.container/*.sh --self-test` invocations, because CI re-implements each gate step
# rather than running check.sh. Add a self-test to check.sh alone and it runs nowhere in CI; drop
# one from check.sh and CI exercises a script nobody runs locally. Derived from both files and
# compared, which is the rule this file already applies to the setup marker, the verdict path and
# run.sh's consumed keys.
# COMMENTS STRIPPED FIRST, because a commented-out self-test is not a self-test that runs. Both
# files are read raw before this, so commenting out `./.container/verify.sh --self-test` in ci.yml
# and the egress-status one in check.sh -- which is exactly how a step gets temporarily disabled --
# still had the two lists agreeing and both guards printing ok, while two self-tests ran nowhere.
# YAML and shell share `#`, so one stripper serves both. It is the same anchoring rule this file
# already applies a hundred lines above to the run.sh/verify.sh invocation guard, whose comment
# says that matching a MENTION rather than an INVOCATION is this directory's most-repeated defect.
selftests_of() { # selftests_of <file>  -> the .container scripts it runs --self-test on, sorted
    dc_strip_comments "$1" 2>/dev/null \
        | grep -oE '[A-Za-z0-9_.-]+\.sh" --self-test|[A-Za-z0-9_.-]+\.sh --self-test' \
        | sed 's/"* --self-test$//' | sed 's#.*/##' | sort -u
}
gate_selftests="$(selftests_of "$here/../scripts/check.sh")"
ci_selftests="$(selftests_of "$here/../.github/workflows/ci.yml")"
if [ -z "$gate_selftests" ] || [ -z "$ci_selftests" ]; then
    bad "could not extract the --self-test list from scripts/check.sh and/or ci.yml — this check is comparing nothing"
elif [ "$gate_selftests" = "$ci_selftests" ]; then
    ok "the gate and CI run the same container self-tests ($(grep -c . <<<"$gate_selftests"))"
else
    bad "scripts/check.sh and ci.yml disagree about which container self-tests to run — only in the gate: $(comm -23 <(printf '%s\n' "$gate_selftests") <(printf '%s\n' "$ci_selftests") | tr '\n' ' '); only in CI: $(comm -13 <(printf '%s\n' "$gate_selftests") <(printf '%s\n' "$ci_selftests") | tr '\n' ' ')"
fi

# EVERY SCRIPT HERE THAT HAS A --self-test IS RUN BY THE GATE. The check above pins the two lists
# to each other; without this, both could omit the same one and agree perfectly about running
# nothing. Found the same way as the derived caller list below: a set nobody compares to reality.
ungated=()
for f in "$here"/*.sh; do
    b="$(basename "$f")"
    case "$b" in check-config.sh|mutate-config.sh|mutate-verify.sh) continue ;; esac
    grep -qE '^\s*(if )?\[ "\$\{?1' "$f" 2>/dev/null || true
    if grep -qF -- '--self-test' "$f" 2>/dev/null; then
        grep -qF -- "$b\" --self-test" <<<"$(dc_strip_comments "$here/../scripts/check.sh")" \
            || grep -qF -- "$b --self-test" <<<"$(dc_strip_comments "$here/../scripts/check.sh")" \
            || ungated+=("$b")
    fi
done
if [ ${#ungated[@]} -eq 0 ]; then
    ok "every .container script with a --self-test is run by the gate"
else
    bad "these have a --self-test that no gate runs: ${ungated[*]} — a self-test nothing invokes is a test that has never run"
fi

callers_ok=1
callers=()
for f in "$here"/*.sh; do
    case "$(basename "$f")" in
        init-firewall.sh|pin-jkb-hook.sh|check-config.sh|mutate-config.sh) continue ;;  # the scripts themselves, and the two harnesses that quote them
    esac
    grep -qE 'init-firewall\.sh|pin-jkb-hook\.sh' "$f" && callers+=("$f")
done
[ "${#callers[@]}" -gt 0 ] || bad "no script here calls init-firewall.sh — the derivation below is checking nothing"
for want in setup.sh run.sh entrypoint.sh; do
    grep -qxF "$want" <<<"$(printf '%s\n' "${callers[@]##*/}")" \
        || bad "$want no longer reaches the firewall-argument guard (did it stop calling init-firewall.sh?)"
done
for caller in ${callers[@]+"${callers[@]}"}; do
    # Anything that STARTS a word after the command is an argument — including a quote, which is
    # how the old setup.sh spelled it. Only a redirect, pipe, separator or end of line is not.
    #
    # AN INVOCATION, NOT A MENTION, and deriving the caller list is what forced that distinction:
    # scanning only setup.sh and run.sh, every occurrence happened to be a call, so the bare name
    # was a good enough proxy. Over the whole directory it is not — the file name appears in a
    # comment, in an error message and in a list of paths to chmod, and all three read as calls
    # passing an argument. So the match is anchored on `sudo`, which is how it is invoked and the
    # only way it CAN be (it needs root, and sudoers grants `vscode` exactly this one path);
    # comments are stripped first, because a comment can mention sudoers too. A caller running it
    # as root without sudo would slip past, and that is deliberate: nothing here is root, and the
    # rule this guard enforces is a property of the sudoers grant.
    # The same rule for every sudoers grant pinned to no arguments (`cmd ""`): pin-jkb-hook.sh was
    # first called with `""`, which is an argument, so sudo would refuse it and setup would stop.
    if dc_strip_comments "$caller" | grep -nE 'sudo[^#]*(init-firewall|pin-jkb-hook)\.sh[[:space:]]+[^;|&>#[:space:]]' >/dev/null; then
        bad "$(basename "$caller") passes an argument to init-firewall.sh or pin-jkb-hook.sh — sudoers permits none to either, so sudo refuses the call"
        callers_ok=0
    fi
done
[ "$callers_ok" -eq 1 ] && ok "no caller passes an argument to the firewall"

# NO REFUSAL IN THE FIREWALL MAY BYPASS `fail_closed`. iptables rules do not survive a container
# restart, so a refusal that exits before installing any is not a refusal — it is unrestricted
# egress with a message. Two guards had that shape (an unparseable snapshot, an unidentifiable
# workspace posture), both in the file whose entire job is to be the layer that holds when the
# nested sandbox does not. `exit 1` inside `fail_closed` itself is the one legitimate site.
# Comments are stripped first and any NONZERO exit is matched ANYWHERE on the line, not just at
# its start. Two ways this was too narrow: an anchored pattern walked straight past `... || exit 1`
# (its own first version, reported MISSED by the mutation), and matching the literal `1` walked
# past the `exit 2` the argument refusal used — a refusal leaving no rules, which is the one thing
# this checks for.
stray_exits="$(dc_strip_comments "$here/init-firewall.sh" | awk '
    /^if \[ "\$#" -eq 1 \]/ { inself = 1 }
    inself && /^fi$/        { inself = 0; next }
    /^fail_closed\(\) \{/ { infn = 1 }
    infn && /^\}/           { infn = 0; next }
    !infn && !inself && /(^|[^[:alnum:]_])exit[[:space:]]+[1-9]/ { print FNR }
')"
if [ -z "$stray_exits" ]; then
    ok "every refusal in init-firewall.sh goes through fail_closed"
else
    bad "init-firewall.sh exits without installing a deny-all at line(s): $(tr '\n' ' ' <<<"$stray_exits")— a refusal that leaves no rules is unrestricted egress with a message"
fi

# UNDER `set -eE` PLUS THE ERR TRAP, A BARE COMMAND-SUBSTITUTION ASSIGNMENT ABORTS THE SCRIPT —
# and in this file an abort means `fail_closed`, which is deny-all with no allowlist. That is not
# hypothetical: `getent` exits 2 for a name with no A record, `pipefail` carries it out, and one
# unresolvable domain among the fifteen took the whole raise down, blaming "an unexpected failure
# at line 173" while the two arms written for exactly that state were unreachable.
#
# Measured on bash 3.2, EVERY shape this file uses, because the regex below looks narrower than
# the hazard and a later reader will otherwise widen it on suspicion:
#
#   x="$(cmd)"                     ABORTS — the trap fires and the raise becomes deny-all
#   x="$(cmd)" || fallback         safe
#   if x="$(cmd)"; then            safe
#   elif x="$(cmd)"; then          safe
#   printf ... "$(cmd)"            safe — errexit does not apply to an argument's expansion
#   cmd "... $(cmd) ..." after ||  safe, twice over
#   done <<<"$(cmd)"               safe
#
# So the bare assignment is the whole hazard, and matching it is the whole job. A review round
# reported this check as vacuously passing over two live violations (the `$(ls …)` inside the
# refusal message at init-firewall.sh:131, and the `elif` at :144); both were re-measured in the
# exact shapes the file uses and neither aborts — 131 sits in an argument of a command that is
# itself the right-hand side of `||`. Nothing was changed on the strength of that report, which
# is why the measurement is written down here instead.
#
# Checked here because the Docker harness cannot reach the DNS-failure path, and because the next
# substitution added to this file has the same trap waiting for it.
bare_subst="$(sed 's/[[:space:]]#.*$//; s/^#.*$//' "$here/init-firewall.sh" | awk '
    # The --self-test block is out of scope for BOTH this and the stray-exit rule above: it runs
    # above `set -E`, exits unconditionally before the ERR trap is ever installed, and is never
    # reached during a raise (the sudoers entry pins the script to no arguments). The exemption is
    # not taken on trust — the check below requires the block to end in an exit, so it cannot grow
    # a fall-through into the operational body and quietly take the exemption with it.
    /^if \[ "\$#" -eq 1 \]/ { inself = 1 }
    inself && /^fi$/        { inself = 0; next }
    inself                  { next }
    # An assignment whose value is a command substitution, at statement level.
    /^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*="?\$\(/ {
        if ($0 !~ /\|\|/) print FNR ": " $0
    }
')"
# WHAT MAKES THAT EXEMPTION SAFE. Both rules above skip the --self-test block because it exits
# before `set -E` installs the ERR trap. If it ever stopped exiting — an early `return`, a removed
# `exit 0`, a refactor that lets it fall through — its code would run during a real raise while
# still being exempt from the two rules that make a raise survivable. So: the last statement in
# the block must be an exit.
selftest_tail="$(dc_strip_comments "$here/init-firewall.sh" | awk '
    /^if \[ "\$#" -eq 1 \]/ { inself = 1; next }
    inself && /^fi$/        { print last; inself = 0 }
    inself && /[^[:space:]]/ { last = $0 }
')"
case "$selftest_tail" in
    *exit*) ok "init-firewall.sh's self-test block exits rather than falling into the raise" ;;
    "")     bad "init-firewall.sh has no --self-test block, but check.sh and CI run it — the writer's verdict logic is unexercised" ;;
    *)      bad "init-firewall.sh's --self-test block does not end in an exit (last statement: $selftest_tail) — it could fall through into a real raise while exempt from the two rules that make one survivable" ;;
esac

if [ -z "$bare_subst" ]; then
    ok "every command substitution in init-firewall.sh can fail without aborting the raise"
else
    bad "init-firewall.sh has a command-substitution assignment with no fallback, which aborts the whole raise into fail_closed under the ERR trap: $(tr '\n' ' ' <<<"$bare_subst")"
fi

# EVERY EXPECT STRING mutate-verify.sh greps for must be one verify.sh can actually print.
# That harness needs Docker, so it does not run in this gate — and a stale expect there is a
# mutation that reports MISSED for ever, which is a guard nobody has seen fire dressed as a guard.
# It went stale the moment a refusal was reworded, and nothing said so. Checked here, statically,
# because the strings are just text in two files.
stale_expects=()
expects=()
while IFS= read -r want; do
    [ -n "$want" ] || continue
    expects+=("$want")
    grep -qF -e "$want" "$here/verify.sh" || stale_expects+=("$want")
done < <(sed -n 's/^[[:space:]]*run "[^"]*" "\([^"]*\)".*/\1/p' "$here/mutate-verify.sh")
# ...AND THE EXTRACTION MUST HAVE FOUND THEM ALL, which an emptiness pin cannot tell you. The
# pattern was anchored at `^run`, so the first mutation to be indented -- one wrapped in an `if`
# for a host where it cannot discriminate -- silently dropped out and the guard went on reporting
# ok about the 13 it could still see. Derived rather than pinned to a number: count the `run "`
# calls the file actually makes and require the extractor to have matched every one, so this
# cannot go quiet again without saying so.
run_calls="$(dc_strip_comments "$here/mutate-verify.sh" | grep -cE '^[[:space:]]*run "' || true)"
if [ "${#expects[@]}" -ne "$run_calls" ]; then
    bad "the mutate-verify expectation check reads ${#expects[@]} of $run_calls run() calls — the rest are invisible to it, so their expectations are unchecked"
fi
# PINNED AGAINST AN EMPTY EXTRACTION, like the three other derived lists above. Without it this
# check passes by finding nothing to check: reword mutate-verify.sh's `run` line and the sed
# stops matching, `stale_expects` is empty, and it prints `ok (0 checked)` — the exact
# vacuous-pass shape it was written to stop existing elsewhere.
if [ ${#expects[@]} -eq 0 ]; then
    bad "no expectations could be read out of mutate-verify.sh — this check just certified nothing; has the 'run \"<label>\" \"<expect>\"' shape changed?"
elif [ ${#stale_expects[@]} -eq 0 ]; then
    ok "every mutate-verify expectation is a string verify.sh prints (${#expects[@]} checked)"
else
    bad "mutate-verify.sh expects text verify.sh never prints: ${stale_expects[*]} — those mutations can only ever report MISSED"
fi

# Every declared VS Code extension is VERSION-PINNED. Unpinned, VS Code resolves "latest" over
# the network when you connect — which is after postCreate raised the egress firewall, so the
# download is refused and the container comes up without the extension, non-fatally. The pin is
# what makes the .vsix staged into the image at build time match what VS Code asks for.
#
# Pinned against an empty extraction for the same reason as the lists above: an empty list would
# make this print `ok (0 checked)` while the Dockerfile fetched nothing and every downstream
# assertion was vacuously satisfied.
unpinned=()
ext_count=0
while read -r ext; do
    [ -n "$ext" ] || continue
    ext_count=$((ext_count+1))
    dc_extension_split "$ext" >/dev/null || unpinned+=("$ext")
done <<<"$(dc_extensions "$here/container.json")"
if [ "$ext_count" -eq 0 ]; then
    bad "no extensions could be read out of container.json — this check just certified nothing; has customizations.vscode.extensions moved?"
elif [ ${#unpinned[@]} -eq 0 ]; then
    ok "every declared VS Code extension is version-pinned ($ext_count checked)"
else
    bad "unpinned VS Code extension(s): ${unpinned[*]} — write publisher.name@version, or the connect-time download the firewall refuses is what installs them"
fi

# The extension this repo BUILDS is identified by ui/vscode/package.json, and verify.sh asks
# dc_local_extension for its id in order to assert it is installed. A rename that drops `publisher`
# or `name` fails nothing on its own: the helper returns nothing, verify.sh silently checks one
# fewer extension, and the missing side panel becomes invisible again — which is the state this
# whole path was added to end. Same vacuous-pass shape as the derived lists above, pinned the same
# way. Skipped where the repo builds no extension, since the container is meant to serve any repo.
if [ -f "$here/../ui/vscode/package.json" ]; then
    if local_ext="$(dc_local_extension "$(cd "$here/.." && pwd)")"; then
        ok "the locally-built extension has a derivable id ($local_ext)"
    else
        bad "ui/vscode/package.json no longer yields a publisher.name — verify.sh would silently stop checking that the jkb explorer is installed"
    fi
fi

# A PRODUCER THAT CAN REFUSE MUST NOT BE READ THROUGH `< <( )`. Bash discards a process
# substitution's exit status, so a refusal inside one kills the subshell alone and the reading loop
# keeps whatever was emitted before it. `dc_subst` refuses on an unset ${localEnv:…} precisely so a
# boundary cannot move because a variable was not set, and the readers built on it carry that
# refusal outward. They all emit as they go, so the caller is left holding not nothing — which
# every caller checks for — but a TRUNCATED list: a container started without some of the mounts or
# security flags it declares, a fingerprint over half a declaration, a control certified without a
# flag it was assembled to carry.
#
# AN ALLOWLIST, NOT A LIST OF PRODUCERS, and the direction is the whole point. This was a
# hand-maintained list of four names, and its own comment recorded that `dc_container_env` had
# "joined it late" after run.sh read its environment through the forbidden shape and a three-name
# list could not see it. A list of things to REFUSE fails open: the next producer nobody adds is
# unguarded, silently, which is how `dc_mount_specs` sat in run.sh's own mount loop — the security
# boundary — unseen by the guard written for exactly that. Naming what is SAFE fails closed
# instead: a producer nobody classified turns this gate red, which is a minute's work and an
# obvious message, rather than a hole.
#
# What is safe is a plain text filter reading a file or a string: it has no refusal to lose, and
# a caller that cares about its status is not using a loop like this. Anything that reads the
# DECLARATION -- every `dc_*`, `docker_args`, `assembled_args`, or a script invoked for one of its
# --print modes -- is not on this list and must go through `$( )`.
procsub_safe='jq|sed|awk|grep|cat|printf|echo|sort|tr|find|ls|comm|diff'
# Comment-stripped per file, so the file name and line number survive for the message -- and so
# that a comment QUOTING the forbidden shape (this block does, twice) is not itself a finding.
procsub=""; procsub_scanned=0; procsub_seen=0
for f in "$here"/*.sh; do
    stripped="$(dc_strip_comments "$f")" || continue
    [ -n "$stripped" ] || continue
    procsub_scanned=$((procsub_scanned+1))
    all="$(printf '%s\n' "$stripped" | grep -nE '< <\(' || true)"
    [ -n "$all" ] && procsub_seen=$((procsub_seen + $(printf '%s\n' "$all" | grep -c .)))
    hits="$(printf '%s\n' "$all" | grep -vE "< <\([[:space:]]*($procsub_safe)[[:space:]]" | grep . || true)"
    [ -n "$hits" ] && procsub="$procsub $(basename "$f"):$(printf '%s' "$hits" | cut -d: -f1 | tr '\n' ',')"
done
procsub="$(printf '%s' "$procsub" | sed 's/^ *//')"
# PINNED AGAINST HAVING READ NOTHING. "No producer is read unsafely" and "the scan found no files"
# are the same value here — an empty `procsub` — and this guard's whole subject is that a check can
# pass having observed nothing. So the count in the `ok` line comes from what was READ, and zero
# files read is a failure rather than a clean report.
if [ "$procsub_scanned" -eq 0 ]; then
    bad "no .container script could be read to check its process substitutions — this check certified nothing"
elif [ -z "$procsub" ]; then
    ok "every process substitution reads a plain text filter, never a producer that can refuse ($procsub_seen in $procsub_scanned files)"
else
    # THE MESSAGE MUST NOT SPELL THE SHAPE IT LOOKS FOR. Written out, this line matched the guard's
    # own scan of this file -- a check reporting itself, which reads as a real finding and cannot be
    # cleared by fixing anything.
    bad "a producer that can refuse is read through a process substitution, which discards its refusal and leaves a truncated list: $(tr '\n' ' ' <<<"$procsub") — read it through a command substitution instead, or add it to procsub_safe if it genuinely cannot refuse"
fi

for s in "$here"/*.sh; do
    if bash -n "$s" 2>/dev/null; then ok "$(basename "$s") parses"; else bad "$(basename "$s") has a syntax error"; fi
done

echo
if [ "$fail" -ne 0 ]; then printf '\033[31m%d failed\033[0m, %d passed\n' "$fail" "$pass"; exit 1; fi
printf '\033[32mall %d container config checks passed\033[0m\n' "$pass"
