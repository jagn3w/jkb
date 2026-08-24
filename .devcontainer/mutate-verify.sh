#!/usr/bin/env bash
# Each case breaks ONE property the container is supposed to have. verify.sh must fail, and must
# fail naming that property — a guard nobody has watched fail is not a guard.
#
#   ./.devcontainer/mutate-verify.sh [image]
#
# Needs a Docker host and the image built (`docker build -t jkb-dev .devcontainer`), so it is this
# change's #[ignore] test and is never part of ./scripts/check.sh — the host-side static checks
# live in check-config.sh.
set -uo pipefail
REPO="${REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
IMAGE="${1:-jkb-dev}"
scratch="$(mktemp -d)"; trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/jkb" "$scratch/home/Documents"
SEC="$REPO/.devcontainer/seccomp-bwrap.json"
BASE=(-v "$REPO":/home/vscode/repos/jkb -v "$scratch/jkb":/home/vscode/.jkb -w /home/vscode/repos/jkb)
# A mutation is CAUGHT only when verify.sh both FAILS and says why. Matching the label alone was
# useless: `assert()` prints the same text on the ok and FAIL paths, so `grep "not a host mount"`
# matched `ok  ~/.claude is the container's own, not a host mount` — two of five mutations
# reported CAUGHT with the guard deleted, and the summary line said "every guard fired".
# Some properties cannot be broken with a docker flag — a sudoers grant lives in the image — so
# the mutation is a one-layer image built FROM it. Set RUN_IMAGE to use the variant.
RUN_IMAGE=""
MUTANT_FAILED=0
# A build failure must NOT fall through to the base image: the mutation would then run against an
# unmutated container, verify.sh would correctly pass, and the harness would report the guard as
# broken. A tooling failure reported as a guard failure sends you to read the wrong code.
mutant() { # mutant <tag> <root-shell-command>
  local err
  if err="$(printf 'FROM %s\nUSER root\nRUN %s\nUSER vscode\n' "$IMAGE" "$2" \
            | docker build -q -t "$1" - 2>&1)"; then
    RUN_IMAGE="$1"
  else
    RUN_IMAGE=""
    MUTANT_FAILED=1
    fails=$((fails+1))
    printf '  BUILD-FAILED  could not build mutant %s — the mutation below was NOT applied\n' "$1"
    sed 's/^/                /' <<<"$err" | tail -3
  fi
}

run() { # run <label> <expect-substring> <docker args...>
  local label="$1" expect="$2"; shift 2
  local out rc img="${RUN_IMAGE:-$IMAGE}"
  RUN_IMAGE=""
  # ...and if its mutant did not build, do NOT quietly test the base image instead: an unmutated
  # container passes verify.sh, which this would then report as the guard failing to fire.
  if [ "$MUTANT_FAILED" = 1 ]; then
    MUTANT_FAILED=0
    printf '  SKIPPED  %s  (mutant image did not build; deliberately NOT run against the base)\n' "$label"
    return
  fi
  out="$(docker run --rm "$@" "$img" bash -c '
      sudo -n /usr/local/bin/init-firewall.sh >/dev/null 2>&1
      . ./.devcontainer/lib.sh && dc_link_state /home/vscode
      ./scripts/auto-mode.sh install --force >/dev/null 2>&1
      ./.devcontainer/verify.sh' 2>&1)"
  rc=$?
  # The FAIL marker and the label on the SAME line: that rendering exists only on the fail path.
  if [ "$rc" -ne 0 ] && grep -E "FAIL.*$(sed 's/[][\.*^$/]/\\&/g' <<<"$expect")" <<<"$out" >/dev/null; then
    printf '  CAUGHT   %s\n' "$label"
  else
    fails=$((fails+1))
    printf '  MISSED   %s  (verify.sh exit %s; wanted a FAIL line mentioning: %s)\n' "$label" "$rc" "$expect"
    sed 's/^/           /' <<<"$out" | grep -E "FAIL|passed|failed" | head -3
  fi
}
fails=0
echo "=== mutations of the container's own guarantees (each must be CAUGHT) ==="
run "an undeclared host mount is added" "UNDECLARED mounts" \
    --security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user vscode "${BASE[@]}" \
    -v "$scratch/home/Documents":/home/vscode/Documents
# Outside /home/vscode entirely — the case the old target-prefix filter could not see at all,
# and the most valuable one: /var/run/docker.sock is root on the host.
run "a host mount OUTSIDE /home/vscode (docker.sock-shaped)" "UNDECLARED mounts" \
    --security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user vscode "${BASE[@]}" \
    -v "$scratch/home":/host
run "the host's ~/.claude is mounted in" "is a host mount" \
    --security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user vscode "${BASE[@]}" \
    -v "$scratch/home":/home/vscode/.claude
run "stock seccomp (nested sandbox cannot start)" "bubblewrap cannot create namespaces" \
    --cap-add=NET_ADMIN --user vscode "${BASE[@]}"
run "no NET_ADMIN (firewall cannot come up)" "NON-allowlisted host was permitted" \
    --security-opt seccomp="$SEC" --user vscode "${BASE[@]}"
run "runs as root" "runs as a non-root user" \
    --security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user root "${BASE[@]}"

# The base image ships /etc/sudoers.d/vscode with NOPASSWD:ALL, which makes the root-owned
# firewall, its snapshot and the pinned sudoers argument all bypassable with one sudo. The
# Dockerfile removes it; this puts it back and requires verify.sh to notice.
# The script sudo runs as root, and the allowlist beside it, are protected only by the directory
# they live in — which the base image owns, not this repo. `chmod 777` is the whole exploit.
mutant jkb-dev-writable-usrlocal "chmod 0777 /usr/local/bin /usr/local/share"
run "/usr/local is writable by the agent" "is writable by" \
    --security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user vscode "${BASE[@]}"

mutant jkb-dev-blanket-sudo "printf 'vscode ALL=(root) NOPASSWD:ALL\\n' > /etc/sudoers.d/vscode && chmod 0440 /etc/sudoers.d/vscode"
run "blanket passwordless root is restored" "may run more than the firewall as root" \
    --security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user vscode "${BASE[@]}"

# The harness's own negative control. If an UNMUTATED container is reported CAUGHT, the matcher
# is matching something that is present when nothing is wrong — which is precisely the defect
# this file exists to detect in verify.sh, and it had it too.
echo
echo "=== self-test: an unmutated container must be reported MISSED ==="
before="$fails"
run "control: nothing mutated (MISSED is correct here)" "runs as a non-root user" \
    --security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user vscode "${BASE[@]}"
if [ "$fails" -gt "$before" ]; then
  self_ok=1; fails="$before"      # a MISSED here is the expected result, not a failure
  echo "  (correct: a healthy container does not trip the matcher)"
else
  self_ok=0
  echo "  MATCHER IS BROKEN: a healthy container was reported CAUGHT"
fi

echo
[ "$self_ok" -eq 1 ] || { printf '\033[31mthe matcher reports CAUGHT for a healthy container — no result here is trustworthy\033[0m\n'; exit 1; }
[ "$fails" -eq 0 ] || { printf '\033[31m%d guard(s) did not fire\033[0m\n' "$fails"; exit 1; }
printf '\033[32mevery guard fired, and the matcher was shown to discriminate\033[0m\n'
