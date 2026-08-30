#!/usr/bin/env bash
# Watch every check-config.sh assertion fail (design D49).
#
#   ./.container/mutate-config.sh
#
# WHY THIS EXISTS. verify.sh has mutate-verify.sh; check-config.sh had nothing, and three review
# rounds each found the same defect in it — an assertion that cannot fail. A guard matching text
# present on both the pass and fail paths; a regex that could not cross a shell quote and so never
# caught the exact code it existed to prevent; a rewrite that silently dropped the `type=volume`
# half of its own check while keeping the failure message about volumes. Each was found by a
# reviewer or by hand-mutating afterwards, which is a process that works right up until nobody
# does it. Needs no Docker, so unlike mutate-verify.sh this runs in ./scripts/check.sh.
#
# THE RULE, same as mutate-verify.sh: a mutation is CAUGHT only when check-config.sh exits non-zero
# AND prints a FAIL line matching the expectation. And the harness has a negative control — an
# unmutated tree must be reported MISSED, or the matcher is matching something that is present
# when nothing is wrong.
set -uo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
# The subject of this harness skips itself when jq is absent, so without the same precondition a
# machine without jq gets every mutation MISSED and a red shared gate — a security-shaped alarm for
# what is only a fact about the host. Every other machine-dependent gate step degrades to a named
# skip; this must too.
for t in jq python3; do
    command -v "$t" >/dev/null 2>&1 || { echo "==> container config guards"; echo "   (skipped: $t not installed; CI runs this gate)"; exit 0; }
done
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
fails=0

# check-config.sh reads $here/* and $here/../scripts/auto-mode-posture.json, so the copy has to
# preserve that shape.
seed() {
    rm -rf "$work/t"; mkdir -p "$work/t/scripts"
    cp -R "$repo/.container" "$work/t/.container"
    cp "$repo/scripts/auto-mode-posture.json" "$work/t/scripts/"
    # check-config.sh derives the locally-built extension's id from here, and skips when the repo
    # has none — so without this the mutation below could not be watched failing.
    mkdir -p "$work/t/ui/vscode"
    cp "$repo/ui/vscode/package.json" "$work/t/ui/vscode/"
}

DC() { printf '%s' "$work/t/.container/container.json"; }

EXPECTS=()
run() { # run <label> <expect-substring>
    local label="$1" expect="$2" out rc
    EXPECTS+=("$expect")
    out="$(cd "$work/t" && ./.container/check-config.sh 2>&1)"; rc=$?
    judge "$label" "$expect" "$out" "$rc"
}

# Separated from executing so the control can judge the SAME run it health-checked, rather than
# starting a second one that could differ from it. See mutate-verify.sh for the failure this
# prevents.
judge() { # judge <label> <expect> <output> <rc>
    local label="$1" expect="$2" out="$3" rc="$4"
    # A mutation is CAUGHT only when check-config.sh FAILS and says why, with both on the SAME line.
    # Fixed-string, because the regex form escaped only some ERE metacharacters and silently
    # mis-matched "host bind source(s) parsed"; `-e`, because an expect may start with a dash,
    # which grep would otherwise read as an option.
    if [ "$rc" -ne 0 ] && grep -F -e "$expect" <<<"$out" | grep -q "FAIL"; then
        printf '  CAUGHT   %s\n' "$label"
    else
        fails=$((fails+1))
        printf '  MISSED   %s  (exit %s; wanted a FAIL line mentioning: %s)\n' "$label" "$rc" "$expect"
        sed 's/^/           /' <<<"$out" | grep -E "FAIL|passed|failed" | head -3
    fi
}

# jq edit on the copied container.json, preserving its // comments by editing the raw text
# through a strip/emit cycle only where jq is genuinely needed.
jq_dc() { local f; f="$(DC)"; sed 's://.*$::' "$f" | jq "$1" > "$f.new" && mv "$f.new" "$f"; }
sub_dc() { local f; f="$(DC)"; python3 - "$f" "$1" "$2" <<'PY'
import sys
p, old, new = sys.argv[1], sys.argv[2], sys.argv[3]
s = open(p).read()
assert old in s, "mutation target not present: " + old
open(p, 'w').write(s.replace(old, new, 1))
PY
}

echo "==> mutations of the container config (each must be CAUGHT)"

seed; sub_dc '"remoteUser": "vscode"' '"remoteUser": "root"'
run "remoteUser becomes root" "remoteUser is root"

seed; jq_dc '.runArgs |= map(select(. != "--security-opt"))'
run "the --security-opt flag is dropped, leaving its value orphaned" "--security-opt"

seed; sub_dc '"--cap-add=NET_ADMIN",' ''
run "NET_ADMIN is dropped" "no longer declares --cap-add=NET_ADMIN"

# The generated profile: drop `unshare` from its unconditional allow group.
seed; python3 - "$work/t/.container/seccomp-bwrap.json" <<'PY'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
for e in d["syscalls"]:
    if e.get("action") == "SCMP_ACT_ALLOW" and not e.get("args") and "unshare" in e.get("names", []):
        e["names"].remove("unshare")
json.dump(d, open(p, "w"))
PY
run "a needed syscall is not allowed" "does not unconditionally allow"

# ...and the negative half: re-add a restriction naming one, so the allow is shadowed.
seed; python3 - "$work/t/.container/seccomp-bwrap.json" <<'PY'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["syscalls"].append({"names": ["pivot_root"], "action": "SCMP_ACT_ERRNO", "args": []})
json.dump(d, open(p, "w"))
PY
run "a restricted entry still names a needed syscall" "the removal loop missed"

seed; python3 - "$work/t/.container/generate-seccomp.sh" <<'PY'
import re, sys
p = sys.argv[1]; s = open(p).read()
# Break the extraction the two assertions above loop over.
s = s.replace('"unshare",', 'unshare').replace('"pivot_root",', 'pivot_root')
open(p, 'w').write(s)
PY
run "the generator's syscall list stops parsing" "no longer yields"

seed; jq_dc 'del(.mounts[] | select(test("/home/vscode/.jkb")))'
run "the knowledge-base mount is dropped" "declared mount set is missing"

# EVERY KEY IN container.json IS APPLIED BY SOMETHING. This replaced four mutations about
# workspaceFolder and initializeCommand, which were Dev Containers' rules and are gone with it —
# and it guards the risk the move introduced: VS Code no longer reads this file, so a key nobody
# applies is now possible and looks exactly like configuration.
seed; jq_dc '.postCreateCommand = "bash .container/setup.sh"'
run "a key is added that nothing applies" "which run.sh does not read"

# ...and both sides of it, pinned against a vacuous pass. An empty consumed list would report
# every key as unread (a different lie), and no declared keys would check nothing at all.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("consumed_keys() {\n    cat <<'KEYS'", "consumed_keys() {\n    return 0\n    cat <<'KEYS'", 1))
PYX
run "run.sh stops naming the keys it applies" "cannot tell an applied key from an ignored one"

# The firewall-argument guard now DERIVES its callers instead of naming setup.sh and run.sh, so it
# covers a caller added later — the entrypoint being the first one that would have been missed.
# Both halves of that derivation get watched failing, because a derived list that silently comes
# back empty is a guard reporting `ok` about nothing, which is the failure the hand-written list at
# least could not have.
seed; for f in "$work"/t/.container/*.sh; do
    case "$(basename "$f")" in
        init-firewall.sh|check-config.sh|mutate-config.sh) continue ;;
    esac
    # Every mention removed, so nothing looks like a caller any more.
    python3 - "$f" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("init-firewall.sh", "some-other-script.sh"))
PYX
done
run "no script reaches the firewall-argument guard" "the derivation below is checking nothing"

# ...and the completeness half: a caller that stops calling it must be reported, not quietly
# dropped from a list that then still prints `ok`.
seed; python3 - "$work/t/.container/entrypoint.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("init-firewall.sh", "some-other-script.sh"))
PYX
run "the entrypoint stops raising the firewall" "no longer reaches the firewall-argument guard"

# The setup-complete marker, spelled in two files that cannot share a variable. Drift there makes
# every run redo the whole of setup while reporting success — the kind of breakage that looks like
# slowness rather than like a bug, so nobody goes looking.
seed; python3 - "$work/t/.container/setup.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("/home/vscode/.jkb-container-setup-complete",
                             "/home/vscode/.jkb-setup-done"))
PYX
run "the two spellings of the setup marker drift apart" "setup would re-run for ever"

# ...and the half that pins the extraction itself: a guard that cannot find either value must say
# so rather than compare two empty strings and call them equal.
seed; python3 - "$work/t/.container/setup.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("touch /home/vscode/.jkb-container-setup-complete", ":"))
PYX
run "setup.sh stops writing the marker at all" "stopped naming it"

# ...and the second verifier put back, which is how the fix for "a fatal verify hides the attach
# instructions" comes to hold on one path and not the other again.
seed; python3 - "$work/t/.container/setup.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s + '\n"$repo/.container/verify.sh"\n')
PYX
run "setup.sh verifies as well as run.sh" "a second verifier there"

# ...and the half that guard's passing line CLAIMS but never checked: that run.sh still verifies.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("verify.sh", "nothing.sh"))
PYX
run "run.sh stops verifying at all" "nothing verifies the container"

# THE ENTRYPOINT LINE. One line in the Dockerfile is the whole of "the firewall is raised on every
# start"; deleting it leaves both config harnesses green because run.sh raises it too, and breaks
# only `docker start`, Docker Desktop and a daemon restart — where nothing else looks.
seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]
open(p, 'w').write("".join(l for l in open(p) if not l.startswith("ENTRYPOINT")))
PYX
run "the image stops running entrypoint.sh" "does not set ENTRYPOINT"

# The egress verdict path: one writer, two readers, three processes that cannot share a variable.
seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("/run/jkb-egress-verdict", "/run/jkb-egress-result"))
PYX
run "a reader drifts off the verdict path" "spelled 2 different ways"

seed; python3 - "$work/t/.container/entrypoint.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("/run/jkb-egress-verdict", "$SOMETHING_ELSE"))
PYX
run "a reader stops naming the verdict at all" "is not named by:"

seed; jq_dc '{}'
run "the declaration is emptied" "this check just certified nothing"

# A mutation harness whose expectation no longer matches its subject reports MISSED for ever.
# mutate-verify.sh needs Docker so this gate cannot run it; the strings are checkable statically.
seed; python3 - "$work/t/.container/mutate-verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('"is not inside any host BIND"', '"a string verify.sh never prints"', 1))
PYX
run "a mutate-verify expectation goes stale" "expects text verify.sh never prints"

# A refusal in the firewall that exits before installing any rule is not a refusal: iptables rules
# do not survive a restart, so the container comes up with unfiltered egress and a message.
seed; python3 - "$work/t/.container/init-firewall.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
s = s.replace('jq empty "$POSTURE" 2>/dev/null || fail_closed "$POSTURE is not valid JSON',
              'jq empty "$POSTURE" 2>/dev/null || exit 1 # "$POSTURE is not valid JSON', 1)
open(p, 'w').write(s)
PYX
run "a firewall refusal exits without installing a deny-all" "exits without installing a deny-all"

# The shape the Docker harness cannot reach: a bare command-substitution assignment, which under
# `set -eE` and the ERR trap takes the whole raise to deny-all the first time any one of the
# fifteen posture domains fails to resolve.
seed; python3 - "$work/t/.container/init-firewall.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace(
    's/sort -u)" || ips=""', 's/sort -u)"', 1) if False else s.replace(
    'sort -u)" || ips=""', 'sort -u)"', 1))
PYX
run "a firewall command substitution loses its fallback" "aborts the whole raise into fail_closed"

seed; jq_dc '.mounts += ["source=/var/run/docker.sock,target=/var/run/docker.sock,type=bind"]'
run "the docker socket is mounted in" "docker socket is root on the host"

seed; jq_dc '.mounts += ["source=${localEnv:HOME}/.claude,target=/home/vscode/.claude/settings.json,type=bind"]'
run "the posture file itself is mounted in" "inside the posture's own directory"

seed; jq_dc '.mounts += ["source=${localEnv:HOME},target=/home/vscode/elsewhere,type=bind"]'
run "the whole host HOME is bound under a benign name" "not on the reviewed bind allowlist"

seed; jq 'del(.publisher)' "$work/t/ui/vscode/package.json" > "$work/t/ui/vscode/pkg.new" \
        && mv "$work/t/ui/vscode/pkg.new" "$work/t/ui/vscode/package.json"
run "the locally-built extension loses its publisher" "no longer yields a publisher.name"

seed; sub_dc '"CARGO_TARGET_DIR": "/home/vscode/.cargo/target"' '"CARGO_TARGET_DIR": "/home/vscode/.cargo-target"'
run "CARGO_TARGET_DIR moves outside the allowlisted root" "is under no allowWrite root"

seed; sub_dc 'source=jkb-cargo-target,target=/home/vscode/.cargo/target,type=volume' \
             'source=${localWorkspaceFolder}/ct,target=/home/vscode/.cargo/target,type=bind'
run "the cargo target volume becomes a bind mount" "not declared type=volume"

seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PY'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
del d["require"]["sandbox"]["network"]["allowedDomains"]
json.dump(d, open(p, "w"))
PY
run "the posture loses its allowlist key" "the firewall would deny everything"

seed; python3 - "$work/t/.container/Dockerfile" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('init-firewall.sh ""', 'init-firewall.sh', 1))
PY
run "the sudoers grant stops pinning its argument" "does not pin the argument list"

seed; python3 - "$work/t/.container/Dockerfile" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('&& rm -f /etc/sudoers.d/vscode \\\n', '', 1))
PY
run "the blanket NOPASSWD:ALL removal is dropped" "the agent can sudo anything"

seed; python3 - "$work/t/.container/init-firewall.sh" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('takes no arguments', 'ignores arguments', 1))
PY
run "the firewall accepts a posture path again" "still accepts a posture path"

# Both spellings of the regression the caller guard exists for. The shell-quoted one is the case
# its first version could not match, so it is kept as its own mutation rather than folded in.
seed; python3 - "$work/t/.container/setup.sh" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('sudo -n /usr/local/bin/init-firewall.sh',
                             'sudo -n /usr/local/bin/init-firewall.sh "$repo/scripts/auto-mode-posture.json"', 1))
PY
run "a caller passes the workspace posture (shell-quoted)" "passes an argument to init-firewall.sh"

seed; python3 - "$work/t/.container/run.sh" <<'PYB'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('/usr/local/bin/init-firewall.sh',
                             '/usr/local/bin/init-firewall.sh /home/vscode/repos/jkb/scripts/auto-mode-posture.json', 1))
PYB
run "a caller passes the workspace posture (bare path)" "passes an argument to init-firewall.sh"

seed; printf '{oops' > "$(DC)"
run "container.json stops parsing" "does not parse"

seed; printf '{oops' > "$work/t/.container/seccomp-bwrap.json"
run "the seccomp profile stops parsing" "does not parse"

seed; sub_dc '"CARGO_TARGET_DIR"' '"CARGO_TARGET_DIR_TYPO"'
run "containerEnv.CARGO_TARGET_DIR disappears" "sets no containerEnv.CARGO_TARGET_DIR"

seed; jq_dc '.mounts |= map(select(test("jkb-cargo-target") | not))'
run "nothing is mounted at CARGO_TARGET_DIR" "nothing is mounted at CARGO_TARGET_DIR"

seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('mkdir -p /home/vscode/.cargo/target', 'mkdir -p /home/vscode/.unused', 1))
PYX
run "the Dockerfile stops pre-creating CARGO_TARGET_DIR" "does not pre-create"

seed; printf '\nif then fi\n' >> "$work/t/.container/setup.sh"
run "a container script gains a syntax error" "has a syntax error"

# An extension loses its version pin — the state VS Code resolves over the network, at connect
# time, against a raised egress firewall. It failed exactly this way on a real launch and said so
# only in a log line nothing gated on.
seed; jq_dc '.customizations.vscode.extensions |= map(split("@")[0])'
run "a VS Code extension loses its version pin" "unpinned VS Code extension(s)"

# ...and the same fail-open shape as the two below: the pinning check reads the list through
# lib.sh's dc_extensions, and a list it cannot find is an empty list. Nothing to check reads as
# nothing wrong — while the Dockerfile fetches no .vsix and setup.sh installs none.
seed; jq_dc 'del(.customizations.vscode.extensions)'
run "the extension list moves out from under dc_extensions" "this check just certified nothing"

# The source half of the mount review depends on lib.sh producing anything at all. mutate-config
# did not mutate lib.sh, so neither this nor the fail-open shape it protects was ever watched.
seed; python3 - "$work/t/.container/lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('dc_mount_sources() { # dc_mount_sources <container.json>',
                             'dc_mount_sources() { return 0; #', 1))
PYX
run "the bind-source derivation returns nothing" "host bind source(s) parsed"

# ...and the same fail-open shape one file over. The expectation check reads mutate-verify.sh's
# `run "<label>" "<expect>"` lines with a sed; change that shape and the sed matches nothing,
# `stale_expects` is empty, and the check printed `ok (0 checked)` — passing by having found
# nothing to check, in the assertion whose whole job is finding guards that cannot fire.
seed; python3 - "$work/t/.container/mutate-verify.sh" <<'PYX'
import re, sys
p = sys.argv[1]; s = open(p).read()
# Rename the helper, exactly as an ordinary refactor would.
open(p, 'w').write(re.sub(r'^run "', 'drive "', s, flags=re.M))
PYX
run "mutate-verify's run-line shape changes" "this check just certified nothing"

# COVERAGE, PINNED rather than claimed. The old summary said "every check-config assertion fired"
# while six of its failure paths had no mutation at all — so a 22nd assertion that cannot fail
# (this repo's most repeated defect, found in check-config.sh three rounds running) would have left
# the gate green under a line stating it had been watched failing.
#
# Pinned by COUNT, not by matching message text: three failure paths build their message in a
# variable, so the text a mutation matches does not appear at the `bad` call at all, and a matcher
# that cannot see them reports false gaps. A count cannot say WHICH path is unwatched, but it
# cannot be fooled either, and it forces the decision at the moment an assertion is added.
echo
echo "==> coverage"
bad_sites="$(grep -c 'bad "' "$repo/.container/check-config.sh")"
PINNED_BAD_SITES=41
if [ "$bad_sites" -ne "$PINNED_BAD_SITES" ]; then
    fails=$((fails+1))
    printf '  check-config.sh has %s failure paths, pinned at %s.\n' "$bad_sites" "$PINNED_BAD_SITES"
    echo "  Add a mutation for the new one (or drop the stale one) and update PINNED_BAD_SITES."
    echo "  An assertion nothing breaks is the defect this harness exists to catch."
else
    printf '  %s failure paths in check-config.sh, %s mutations, count pinned\n' \
        "$bad_sites" "${#EXPECTS[@]}"
fi

echo
echo "==> self-test: an unmutated tree must be reported MISSED"
before="$fails"
seed
# THE CONTROL MUST OBSERVE A HEALTHY SUBJECT, not merely fail to trip. `run` reports MISSED when
# check-config.sh passes cleanly AND when it cannot run at all — a missing tree, a missing tool —
# so accepting MISSED on its own blesses a run in which nothing was exercised. Same rule as
# mutate-verify.sh, which had the same hole; a harness that judges other guards has to hold itself
# to the standard it enforces.
control_out="$(cd "$work/t" && ./.container/check-config.sh 2>&1)"
control_rc=$?
if [ "$control_rc" -ne 0 ] || ! grep -q "container config checks passed" <<<"$control_out"; then
    printf '\033[31mthe unmutated config does not pass check-config.sh (exit %s) — every MISSED above is\n' "$control_rc"
    printf 'unattributable, because a subject that cannot run looks exactly like a guard that did not fire\033[0m\n'
    sed 's/^/    /' <<<"$control_out" | grep -E "FAIL|failed|not found" | head -5
    exit 1
fi

judge "control: nothing mutated (MISSED is correct here)" "remoteUser is root" \
      "$control_out" "$control_rc"
if [ "$fails" -gt "$before" ]; then
    self_ok=1; fails="$before"
    echo "  (correct: an unmutated config does not trip the matcher)"
else
    self_ok=0
    echo "  MATCHER IS BROKEN: an unmutated config was reported CAUGHT"
fi

echo
[ "$self_ok" -eq 1 ] || { printf '\033[31mthe matcher reports CAUGHT for a healthy config — no result here is trustworthy\033[0m\n'; exit 1; }
[ "$fails" -eq 0 ] || { printf '\033[31m%d check-config assertion(s) did not fire\033[0m\n' "$fails"; exit 1; }
# No `- 1`: the control is judged directly rather than through run(), so it no longer registers an
# expect, and EXPECTS is exactly the mutations.
printf '\033[32m%s mutations caught over %s failure paths, and the matcher was shown to discriminate\033[0m\n' "${#EXPECTS[@]}" "$bad_sites"
