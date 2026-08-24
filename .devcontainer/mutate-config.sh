#!/usr/bin/env bash
# Watch every check-config.sh assertion fail (design D49).
#
#   ./.devcontainer/mutate-config.sh
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
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
fails=0

# check-config.sh reads $here/* and $here/../scripts/auto-mode-posture.json, so the copy has to
# preserve that shape.
seed() {
    rm -rf "$work/t"; mkdir -p "$work/t/scripts"
    cp -R "$repo/.devcontainer" "$work/t/.devcontainer"
    cp "$repo/scripts/auto-mode-posture.json" "$work/t/scripts/"
}

DC() { printf '%s' "$work/t/.devcontainer/devcontainer.json"; }

run() { # run <label> <expect-substring>
    local label="$1" expect="$2" out rc
    out="$(cd "$work/t" && ./.devcontainer/check-config.sh 2>&1)"; rc=$?
    if [ "$rc" -ne 0 ] && grep -E "FAIL.*$(sed 's/[][\.*^$/]/\\&/g' <<<"$expect")" <<<"$out" >/dev/null; then
        printf '  CAUGHT   %s\n' "$label"
    else
        fails=$((fails+1))
        printf '  MISSED   %s  (exit %s; wanted a FAIL line mentioning: %s)\n' "$label" "$rc" "$expect"
        sed 's/^/           /' <<<"$out" | grep -E "FAIL|passed|failed" | head -3
    fi
}

# jq edit on the copied devcontainer.json, preserving its // comments by editing the raw text
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

echo "==> mutations of the devcontainer config (each must be CAUGHT)"

seed; sub_dc '"remoteUser": "vscode"' '"remoteUser": "root"'
run "remoteUser becomes root" "remoteUser is root"

seed; jq_dc '.runArgs |= map(select(. != "--security-opt"))'
run "the --security-opt flag is dropped, leaving its value orphaned" "--security-opt"

seed; sub_dc '"--cap-add=NET_ADMIN",' ''
run "NET_ADMIN is dropped" "no longer declares --cap-add=NET_ADMIN"

# The generated profile: drop `unshare` from its unconditional allow group.
seed; python3 - "$work/t/.devcontainer/seccomp-bwrap.json" <<'PY'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
for e in d["syscalls"]:
    if e.get("action") == "SCMP_ACT_ALLOW" and not e.get("args") and "unshare" in e.get("names", []):
        e["names"].remove("unshare")
json.dump(d, open(p, "w"))
PY
run "a needed syscall is not allowed" "does not unconditionally allow"

# ...and the negative half: re-add a restriction naming one, so the allow is shadowed.
seed; python3 - "$work/t/.devcontainer/seccomp-bwrap.json" <<'PY'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["syscalls"].append({"names": ["pivot_root"], "action": "SCMP_ACT_ERRNO", "args": []})
json.dump(d, open(p, "w"))
PY
run "a restricted entry still names a needed syscall" "the removal loop missed"

seed; python3 - "$work/t/.devcontainer/generate-seccomp.sh" <<'PY'
import re, sys
p = sys.argv[1]; s = open(p).read()
# Break the extraction the two assertions above loop over.
s = s.replace('"unshare",', 'unshare').replace('"pivot_root",', 'pivot_root')
open(p, 'w').write(s)
PY
run "the generator's syscall list stops parsing" "no longer yields"

seed; jq_dc 'del(.mounts[] | select(test("/home/vscode/.jkb")))'
run "the knowledge-base mount is dropped" "declared mount set is missing"

seed; jq_dc '.mounts += ["source=/var/run/docker.sock,target=/var/run/docker.sock,type=bind"]'
run "the docker socket is mounted in" "docker socket is root on the host"

seed; jq_dc '.mounts += ["source=${localEnv:HOME}/.claude,target=/home/vscode/.claude/settings.json,type=bind"]'
run "the posture file itself is mounted in" "inside the posture's own directory"

seed; jq_dc '.mounts += ["source=${localEnv:HOME},target=/home/vscode/elsewhere,type=bind"]'
run "the whole host HOME is bound under a benign name" "not on the reviewed bind allowlist"

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

seed; python3 - "$work/t/.devcontainer/Dockerfile" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('init-firewall.sh ""', 'init-firewall.sh', 1))
PY
run "the sudoers grant stops pinning its argument" "does not pin the argument list"

seed; python3 - "$work/t/.devcontainer/Dockerfile" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('&& rm -f /etc/sudoers.d/vscode \\\n', '', 1))
PY
run "the blanket NOPASSWD:ALL removal is dropped" "the agent can sudo anything"

seed; python3 - "$work/t/.devcontainer/init-firewall.sh" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('takes no arguments', 'ignores arguments', 1))
PY
run "the firewall accepts a posture path again" "still accepts a posture path"

# Both spellings of the regression the caller guard exists for. The shell-quoted one is the case
# its first version could not match, so it is kept as its own mutation rather than folded in.
seed; python3 - "$work/t/.devcontainer/setup.sh" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('sudo -n /usr/local/bin/init-firewall.sh',
                             'sudo -n /usr/local/bin/init-firewall.sh "$repo/scripts/auto-mode-posture.json"', 1))
PY
run "a caller passes the workspace posture (shell-quoted)" "passes an argument to init-firewall.sh"

seed; sub_dc '"postStartCommand": "sudo -n /usr/local/bin/init-firewall.sh"' \
             '"postStartCommand": "sudo -n /usr/local/bin/init-firewall.sh /home/vscode/repos/jkb/scripts/auto-mode-posture.json"'
run "a caller passes the workspace posture (bare path)" "passes an argument to init-firewall.sh"

echo
echo "==> self-test: an unmutated tree must be reported MISSED"
before="$fails"
seed
run "control: nothing mutated (MISSED is correct here)" "remoteUser is root"
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
printf '\033[32mevery check-config assertion fired, and the matcher was shown to discriminate\033[0m\n'
