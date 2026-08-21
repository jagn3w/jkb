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
run() { # run <label> <expect-substring> <docker args...>
  local label="$1" expect="$2"; shift 2
  local out; out="$(docker run --rm "$@" "$IMAGE" bash -c '
      sudo -n /usr/local/bin/init-firewall.sh /home/vscode/repos/jkb/scripts/auto-mode-posture.json >/dev/null 2>&1
      ./scripts/auto-mode.sh install >/dev/null 2>&1
      ./.devcontainer/verify.sh' 2>&1)"
  if grep -qF "$expect" <<<"$out"; then printf '  CAUGHT   %s\n' "$label"
  else fails=$((fails+1)); printf '  MISSED   %s  (expected a failure mentioning: %s)\n' "$label" "$expect"
       sed 's/^/           /' <<<"$out" | grep -E "FAIL|passed|failed" | head -3; fi
}
fails=0
echo "=== mutations of the container's own guarantees (each must be CAUGHT) ==="
run "an undeclared host mount is added" "UNDECLARED host paths" \
    --security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user vscode "${BASE[@]}" \
    -v "$scratch/home/Documents":/home/vscode/Documents
run "the host's ~/.claude is mounted in" "not a host mount" \
    --security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user vscode "${BASE[@]}" \
    -v "$scratch/home":/home/vscode/.claude
run "stock seccomp (nested sandbox cannot start)" "bubblewrap cannot create namespaces" \
    --cap-add=NET_ADMIN --user vscode "${BASE[@]}"
run "no NET_ADMIN (firewall cannot come up)" "NON-allowlisted host was permitted" \
    --security-opt seccomp="$SEC" --user vscode "${BASE[@]}"
run "runs as root" "runs as a non-root user" \
    --security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user root "${BASE[@]}"

echo
[ "$fails" -eq 0 ] || { printf '\033[31m%d guard(s) did not fire\033[0m\n' "$fails"; exit 1; }
printf '\033[32mevery guard fired\033[0m\n'
