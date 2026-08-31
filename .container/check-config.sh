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

# The seccomp profile is asserted as a FLAG/VALUE PAIR in runArgs, not as a string present
# somewhere in the file. Grepping for the value alone passed when the `--security-opt` flag was
# deleted and the value left orphaned — Docker would then apply its default profile, bubblewrap
# would fail, and the config still read as declaring a profile. Found by mutate-config.sh.
if jq -e --arg v "seccomp=\${localWorkspaceFolder}/.container/seccomp-bwrap.json" \
      '[.runArgs // [] | to_entries[] | select(.value == "--security-opt") | .key]
       | any(. as $i | ($ARGS.named.v) == ($in_args[$i+1] // ""))' \
      --argjson in_args "$(jq -c '.runArgs // []' <<<"$dc")" <<<"$dc" >/dev/null 2>&1; then
    ok "runArgs pairs --security-opt with the seccomp profile"
else
    bad "container.json does not pair --security-opt with seccomp=\${localWorkspaceFolder}/.container/seccomp-bwrap.json — Docker would apply its default profile and bubblewrap could not start"
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
mount_targets="$(dc_mount_targets "$here/container.json")"
missing_mounts=()
for m in /home/vscode/repos /home/vscode/.jkb; do
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
        '${localEnv:HOME}/repos'|'${localEnv:HOME}/.jkb') ;;
        *) forbidden+=("host source $src (not on the reviewed bind allowlist)") ;;
    esac
done <<<"$(dc_mount_sources "$here/container.json" | sed -n '/|volume$/!s/|[^|]*$//p')"
# ...and certify the source derivation produced something, the way the target list is certified
# above. An include-match on `|bind` skipped any other type spelling entirely, and an empty result
# made "every declared mount is acceptable" pass with the host's ~/.ssh bound in — a guard that
# fails OPEN. Excluding volumes instead means an unrecognised type is reviewed, not waved through.
bind_sources="$(dc_mount_sources "$here/container.json" | sed -n '/|volume$/!s/|[^|]*$//p' | grep -c .)"
if [ "$bind_sources" -lt 2 ]; then
    bad "only $bind_sources host bind source(s) parsed — the workspace and ~/.jkb are both binds, so the review below saw less than the config declares"
fi
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
# ...and that grant is decorative unless the base image's blanket one is gone. The devcontainers
# base ships /etc/sudoers.d/vscode = `NOPASSWD:ALL`, under which the agent can flush the firewall,
# delete the allowlist snapshot or rewrite the root-owned script. verify.sh asks sudo itself at
# runtime, which is the real check; this catches the removal being dropped from the Dockerfile.
if grep -qF 'rm -f /etc/sudoers.d/vscode' "$here/Dockerfile"; then
    ok "the base image's blanket NOPASSWD:ALL grant is removed"
else
    bad "the Dockerfile no longer removes /etc/sudoers.d/vscode — the agent can sudo anything, and every root-ownership guard here is bypassable"
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
dc_strip_comments() { sed 's/[[:space:]]#.*$//; s/^#.*$//' "$1"; }

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
if dc_strip_comments "$here/setup.sh" | grep -qE '(^|[^-[:alnum:]])verify\.sh'; then
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
if dc_strip_comments "$here/run.sh" | grep -qE '^[[:space:]]*(in_container|docker exec)([[:space:]]+[^[:space:]]+)*[[:space:]]+bash[[:space:]]+\.container/verify\.sh'; then
    ok "run.sh invokes verify.sh (a statement, not a mention of the name)"
else
    bad "run.sh no longer runs verify.sh — nothing verifies the container, and the guard above says it does"
fi

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
        init-firewall.sh|check-config.sh|mutate-config.sh) continue ;;  # the script itself, and the two harnesses that quote it
    esac
    grep -qF 'init-firewall.sh' "$f" && callers+=("$f")
done
[ "${#callers[@]}" -gt 0 ] || bad "no script here calls init-firewall.sh — the derivation below is checking nothing"
for want in setup.sh run.sh entrypoint.sh; do
    printf '%s\n' "${callers[@]##*/}" | grep -qxF "$want" \
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
    if dc_strip_comments "$caller" | grep -nE 'sudo[^#]*init-firewall\.sh[[:space:]]+[^;|&>#[:space:]]' >/dev/null; then
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

for s in "$here"/*.sh; do
    if bash -n "$s" 2>/dev/null; then ok "$(basename "$s") parses"; else bad "$(basename "$s") has a syntax error"; fi
done

echo
if [ "$fail" -ne 0 ]; then printf '\033[31m%d failed\033[0m, %d passed\n' "$fail" "$pass"; exit 1; fi
printf '\033[32mall %d container config checks passed\033[0m\n' "$pass"
