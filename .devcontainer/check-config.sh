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

echo "==> devcontainer config"
command -v jq >/dev/null 2>&1 || { echo "   (skipped: jq not installed)"; exit 0; }

# Sourced HERE rather than 80 lines down, so this file has one copy of the comment-stripping rule
# instead of a verbatim `strip()` beside the `dc_strip()` it later sources — two halves of one file
# parsing the same input through two copies that can disagree.
# shellcheck source=/dev/null
. "$here/lib.sh"
if dc_strip "$here/devcontainer.json" | jq empty 2>/dev/null; then ok "devcontainer.json parses"
else bad "devcontainer.json does not parse"; fi

dc="$(dc_strip "$here/devcontainer.json")"
for want in '"remoteUser": "vscode"' '--cap-add=NET_ADMIN'; do
    if grep -qF -e "$want" <<<"$dc"; then ok "declares $want"
    else bad "devcontainer.json no longer declares $want"; fi
done

# The seccomp profile is asserted as a FLAG/VALUE PAIR in runArgs, not as a string present
# somewhere in the file. Grepping for the value alone passed when the `--security-opt` flag was
# deleted and the value left orphaned — Docker would then apply its default profile, bubblewrap
# would fail, and the config still read as declaring a profile. Found by mutate-config.sh.
if jq -e --arg v "seccomp=\${localWorkspaceFolder}/.devcontainer/seccomp-bwrap.json" \
      '[.runArgs // [] | to_entries[] | select(.value == "--security-opt") | .key]
       | any(. as $i | ($ARGS.named.v) == ($in_args[$i+1] // ""))' \
      --argjson in_args "$(jq -c '.runArgs // []' <<<"$dc")" <<<"$dc" >/dev/null 2>&1; then
    ok "runArgs pairs --security-opt with the seccomp profile"
else
    bad "devcontainer.json does not pair --security-opt with seccomp=\${localWorkspaceFolder}/.devcontainer/seccomp-bwrap.json — Docker would apply its default profile and bubblewrap could not start"
fi

# Non-root is load-bearing (root cannot create a mount namespace in a container), so a
# `"remoteUser": "root"` would break the nested sandbox while looking like a simplification.
if grep -q '"remoteUser": *"root"' <<<"$dc"; then bad "remoteUser is root — the nested sandbox cannot start"; fi

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
mount_targets="$(dc_mount_targets "$here/devcontainer.json")"
missing_mounts=()
for m in /home/vscode/repos /home/vscode/.jkb; do
    grep -qx "$m" <<<"$mount_targets" || missing_mounts+=("$m")
done
if [ ${#missing_mounts[@]} -eq 0 ]; then
    ok "the mount list parses and declares the load-bearing mounts ($(grep -c . <<<"$mount_targets") targets)"
else
    bad "the declared mount set is missing ${missing_mounts[*]} — verify.sh derives its boundary from this list"
fi

# The workspace folder must be INSIDE the workspace mount's target, and the folder that decides it
# must be refused when it cannot be. These are one rule in two halves and both are needed: a
# `workspaceFolder` that is a literal path under the target opens whatever repo happens to sit
# there — for a `jkb task work` session, the main checkout instead of the session — and every
# runtime guard still passes, because the wrong repo is a perfectly good repo. The `initializeCommand`
# is what makes the derived form safe, so its absence is a failure of this rule, not a separate one.
# Parsed through jq, handling BOTH the string and object forms of `workspaceMount` — the spec
# allows either and they are interchangeable at any time. A `target=` regex understood only the
# string form, and when it yielded nothing the case below degenerated to "does it start with a
# slash", which every plausible value passes: a guard that fails open on a purely cosmetic edit.
ws_target="$(jq -r 'if (.workspaceMount // "") | type == "string"
                    then ((.workspaceMount // "") | capture("target=(?<t>[^,]+)").t)
                    else (.workspaceMount.target // "") end' <<<"$dc" 2>/dev/null)"
ws_folder="$(jq -r '.workspaceFolder // ""' <<<"$dc")"
ws_ok=1
if [ -z "$ws_target" ] || [ -z "$ws_folder" ]; then
    bad "could not read workspaceMount's target and workspaceFolder from devcontainer.json — the checks below cannot mean anything"
    ws_ok=0
else
case "$ws_folder" in
    "$ws_target"/?*) ;;
    *) bad "workspaceFolder ($ws_folder) is not inside the workspaceMount target ($ws_target) — the container would open a directory the mount does not place there"; ws_ok=0 ;;
esac
# ...and DERIVED, which is the property that actually matters. Inside the target is not enough:
# a literal `/home/vscode/repos/jkb` is inside it and is exactly the harm the comment above
# names — it opens whichever checkout sits there, so a session worktree starts the agent in the
# main one. The folder has to follow the folder that was opened.
case "$ws_folder" in
    *'${localWorkspaceFolderBasename}'*) ;;
    *) bad "workspaceFolder ($ws_folder) is a literal path, not derived from \${localWorkspaceFolderBasename} — it would open whichever checkout happens to sit there rather than the one you opened"; ws_ok=0 ;;
esac
fi
if ! grep -qF 'check-workspace.sh' <<<"$(jq -r '.initializeCommand // ""' <<<"$dc")"; then
    bad "devcontainer.json has no initializeCommand running check-workspace.sh — a folder that is not \$HOME/repos/<name> would open a DIFFERENT checkout than the one you clicked, silently"
    ws_ok=0
fi
[ "$ws_ok" -eq 1 ] && ok "the workspace folder is inside the mount, and a folder that is not is refused before create"

# DERIVING THE EXPECTED SET MADE THE BOUNDARY SELF-CERTIFYING, and this is the other half of it.
# verify.sh can now only answer "does the running container match what it declares"; adding a
# mount to devcontainer.json makes it declared, so the runtime check would accept the two mounts
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
        '${localEnv:HOME}/repos'|'${localEnv:HOME}/.jkb') ;;
        *) forbidden+=("host source $src (not on the reviewed bind allowlist)") ;;
    esac
done <<<"$(dc_mount_sources "$here/devcontainer.json" | sed -n '/|volume$/!s/|[^|]*$//p')"
# ...and certify the source derivation produced something, the way the target list is certified
# above. An include-match on `|bind` skipped any other type spelling entirely, and an empty result
# made "every declared mount is acceptable" pass with the host's ~/.ssh bound in — a guard that
# fails OPEN. Excluding volumes instead means an unrecognised type is reviewed, not waved through.
bind_sources="$(dc_mount_sources "$here/devcontainer.json" | sed -n '/|volume$/!s/|[^|]*$//p' | grep -c .)"
if [ "$bind_sources" -lt 2 ]; then
    bad "only $bind_sources host bind source(s) parsed — the workspace and ~/.jkb are both binds, so the review below saw less than the config declares"
fi
if [ ${#forbidden[@]} -eq 0 ]; then
    ok "every declared mount is acceptable (no posture directory, no docker socket, binds from the reviewed set)"
else
    for f in "${forbidden[@]}"; do bad "devcontainer.json declares a mount that must not exist: $f"; done
fi

# CARGO_TARGET_DIR is named in three files that cannot reference one another (JSON has no
# variables), and it was already wrong once: it sat BESIDE the allowlisted ~/.cargo rather than
# under it, so denyRead blanketed every sandboxed build while both runtime guards reported the
# container healthy. The rule is generic — the path every site names must be the same one, and it
# must fall under a posture write root — so a future edit to any single site is caught here rather
# than by a build dying inside a container.
posture="$here/../scripts/auto-mode-posture.json"
user="$(jq -r '.remoteUser // "root"' <<<"$dc")"
home="/home/$user"
target="$(jq -r '.containerEnv.CARGO_TARGET_DIR // ""' <<<"$dc")"
if [ -z "$target" ]; then
    bad "devcontainer.json sets no containerEnv.CARGO_TARGET_DIR — cargo would write into the bind mount"
else
    sites_ok=1
    # Declared AND a volume. Rewriting this to use the derived target list dropped the
    # `type=volume` half, leaving a check whose own failure message is about volumes but which a
    # plain bind satisfies — and a bind is the case that breaks: it carries the host's uids, so
    # where the host uid is not 1000 the build dies with EACCES minutes in.
    grep -qx "$target" <<<"$mount_targets" || { bad "nothing is mounted at CARGO_TARGET_DIR ($target) — a named volume whose path the image lacks is created root-owned"; sites_ok=0; }
    [ "$(dc_type_for_target "$here/devcontainer.json" "$target")" = volume ] \
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
# ...and that grant is decorative unless the base image's blanket one is gone. The devcontainers
# base ships /etc/sudoers.d/vscode = `NOPASSWD:ALL`, under which the agent can flush the firewall,
# delete the allowlist snapshot or rewrite the root-owned script. verify.sh asks sudo itself at
# runtime, which is the real check; this catches the removal being dropped from the Dockerfile.
if grep -qF 'rm -f /etc/sudoers.d/vscode' "$here/Dockerfile"; then
    ok "the base image's blanket NOPASSWD:ALL grant is removed"
else
    bad "the Dockerfile no longer removes /etc/sudoers.d/vscode — the agent can sudo anything, and every root-ownership guard here is bypassable"
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
callers_ok=1
for caller in "$here/setup.sh" "$here/devcontainer.json"; do
    # Anything that STARTS a word after the command is an argument — including a quote, which is
    # how the old setup.sh spelled it. Only a redirect, pipe, separator or end of line is not.
    if grep -nE 'init-firewall\.sh[[:space:]]+[^;|&>#[:space:]]' "$caller" >/dev/null; then
        bad "$(basename "$caller") passes an argument to init-firewall.sh — sudoers permits none, and the allowlist is the root-owned snapshot"
        callers_ok=0
    fi
done
[ "$callers_ok" -eq 1 ] && ok "no caller passes an argument to the firewall"

# NO REFUSAL IN THE FIREWALL MAY BYPASS `fail_closed`. iptables rules do not survive a container
# restart, so a refusal that exits before installing any is not a refusal — it is unrestricted
# egress with a message. Two guards had that shape (an unparseable snapshot, an unidentifiable
# workspace posture), both in the file whose entire job is to be the layer that holds when the
# nested sandbox does not. `exit 1` inside `fail_closed` itself is the one legitimate site.
# Comments are stripped first and `exit 1` is matched ANYWHERE on the line, not just at its
# start: the shape this is about is `... || exit 1`, which an anchored pattern walks straight
# past — the guard's own first version did exactly that and reported the mutation as MISSED.
stray_exits="$(sed 's/[[:space:]]#.*$//; s/^#.*$//' "$here/init-firewall.sh" | awk '
    /^fail_closed\(\) \{/ { infn = 1 }
    infn && /^\}/           { infn = 0; next }
    !infn && /(^|[^[:alnum:]_])exit[[:space:]]+1([^[:alnum:]_]|$)/ { print FNR }
')"
if [ -z "$stray_exits" ]; then
    ok "every refusal in init-firewall.sh goes through fail_closed"
else
    bad "init-firewall.sh exits without installing a deny-all at line(s): $(tr '\n' ' ' <<<"$stray_exits")— a refusal that leaves no rules is unrestricted egress with a message"
fi

# EVERY EXPECT STRING mutate-verify.sh greps for must be one verify.sh can actually print.
# That harness needs Docker, so it does not run in this gate — and a stale expect there is a
# mutation that reports MISSED for ever, which is a guard nobody has seen fire dressed as a guard.
# It went stale the moment a refusal was reworded, and nothing said so. Checked here, statically,
# because the strings are just text in two files.
stale_expects=()
while IFS= read -r want; do
    [ -n "$want" ] || continue
    grep -qF -e "$want" "$here/verify.sh" || stale_expects+=("$want")
done < <(sed -n 's/^run "[^"]*" "\([^"]*\)".*/\1/p' "$here/mutate-verify.sh")
if [ ${#stale_expects[@]} -eq 0 ]; then
    ok "every mutate-verify expectation is a string verify.sh prints ($(sed -n 's/^run "[^"]*" "\([^"]*\)".*/\1/p' "$here/mutate-verify.sh" | grep -c .) checked)"
else
    bad "mutate-verify.sh expects text verify.sh never prints: ${stale_expects[*]} — those mutations can only ever report MISSED"
fi

for s in "$here"/*.sh; do
    if bash -n "$s" 2>/dev/null; then ok "$(basename "$s") parses"; else bad "$(basename "$s") has a syntax error"; fi
done

echo
if [ "$fail" -ne 0 ]; then printf '\033[31m%d failed\033[0m, %d passed\n' "$fail" "$pass"; exit 1; fi
printf '\033[32mall %d devcontainer config checks passed\033[0m\n' "$pass"
