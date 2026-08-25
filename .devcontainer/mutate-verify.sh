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
# Without this, a host with no docker on PATH reports every guard as MISSED and exits 1 saying
# "N guard(s) did not fire" — a security-shaped alarm for what is only a fact about the shell.
# Docker Desktop installs to ~/.docker/bin, which an interactive profile may export and a plain
# shell may not, so this is the normal way to meet it rather than an exotic one.
# USABILITY, not presence. `command -v docker` succeeds on the far more common host state — Docker
# Desktop installed but not running — and every mutation then printed MISSED with exit 125 before
# the control finally said they were unattributable. Ten container starts to deliver the exact
# ten-broken-guards alarm this block exists to remove, for the same fact about the host that the
# PATH case reports as a clean skip.
if ! command -v docker >/dev/null 2>&1; then
    echo "=== container guards ==="
    echo "   (skipped: docker is not on PATH — try: export PATH=\"\$HOME/.docker/bin:\$PATH\")"
    echo "   Nothing was verified. This is NOT a passing result, and not a failing one either."
    exit 0
elif ! docker info >/dev/null 2>&1; then
    echo "=== container guards ==="
    echo "   (skipped: the Docker daemon is not reachable — is Docker Desktop running?)"
    echo "   Nothing was verified. This is NOT a passing result, and not a failing one either."
    exit 0
fi
IMAGE="${1:-jkb-dev}"
scratch="$(mktemp -d)"; trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/jkb" "$scratch/home/Documents"
printf '{}' > "$scratch/home/settings.json"
SEC="$REPO/.devcontainer/seccomp-bwrap.json"
BASE=(-v "$REPO":/home/vscode/repos/jkb -v "$scratch/jkb":/home/vscode/.jkb -w /home/vscode/repos/jkb)
# A mutation is CAUGHT only when verify.sh both FAILS and says why. Matching the label alone was
# useless: `assert()` prints the same text on the ok and FAIL paths, so `grep "not a host mount"`
# matched `ok  ~/.claude is the container's own, not a host mount` — two of five mutations
# reported CAUGHT with the guard deleted, and the summary line said "every guard fired".
# Some properties cannot be broken with a docker flag — a sudoers grant lives in the image — so
# the mutation is a one-layer image built FROM it. Set RUN_IMAGE to use the variant.
# ONE definition of what is run inside the container, and one of the healthy flag set. These were
# duplicated between run() and the control's pre-run, which meant the control could certify a
# configuration the mutations never used — a control that is not about the same thing is not a
# control.
# `--declare` is the nested-bind exception, and this harness is the reason it exists. BASE mounts
# the repo AT /home/vscode/repos/jkb, which devcontainer.json now declares only the parent of —
# and the parent cannot be mounted instead, because in a `jkb task work` session $REPO's parent is
# `.jkb/work`, so the checkout would land at /home/vscode/repos/<session>. verify.sh accepts the
# name only because it is strictly inside a declared target; see its comment for why nesting is
# not granted automatically. Read from the environment so a mutation can supply a BAD declaration
# and watch the refusal fire — one SUBJECT, as the comment above requires.
SUBJECT='
      sudo -n /usr/local/bin/init-firewall.sh >/dev/null 2>&1
      . ./.devcontainer/lib.sh && dc_link_state /home/vscode
      [ -n "${JKB_SKIP_MEMORY_LINK:-}" ] || ./scripts/link-claude-memory.sh >/dev/null 2>&1
      ./scripts/auto-mode.sh install --force >/dev/null 2>&1
      ./.devcontainer/verify.sh --declare "${JKB_VERIFY_DECLARE:-/home/vscode/repos/jkb}"'
# The baseline every ADDITIVE mutation runs in, and the control with it. The three subtractive
# mutations below deliberately spell a reduced set instead — that is what they are testing — and
# being the only sites that do so makes them visibly the odd ones. Before this, HEALTHY was used
# by the control alone while ten sites wrote the flags by hand, so tightening the baseline at the
# run sites would have left the control certifying a container the mutations never ran in.
HEALTHY=(--security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user vscode "${BASE[@]}")

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
    # Its OWN counter. Counting it in `fails` made the summary say "N guard(s) did not fire" for a
    # Docker registry outage — sending the reader to audit the container's guarantees over a
    # network blip. That is the rule the comment above states for the fall-through case, and it was
    # not applied one route over.
    build_failures=$((build_failures+1))
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
  out="$(docker run --rm "$@" "$img" bash -c "$SUBJECT" 2>&1)"
  rc=$?
  judge "$label" "$expect" "$out" "$rc"
}

# Judging is separated from executing precisely so ONE container run can be judged twice — the
# control needs to assert the container was healthy AND that the matcher stays quiet about it, and
# doing that as two runs let a transient fault land between them: the pre-run certified a healthy
# container, the second hit a DNS blip on verify.sh's live curl, and the matcher was then shown a
# BROKEN container while the harness printed "shown to discriminate".
judge() { # judge <label> <expect> <output> <rc>
  local label="$1" expect="$2" out="$3" rc="$4"
  # A mutation is CAUGHT only when the subject FAILS and says why, with both on the SAME line:
  # `assert()` prints the same label on its ok and fail paths, so matching the label alone reported
  # guards as caught while they were deleted. Fixed-string, because the regex form escaped only
  # some ERE metacharacters and silently mis-matched "host bind source(s) parsed"; `-e`, because an
  # expect may start with a dash, which grep would otherwise read as an option.
  if [ "$rc" -ne 0 ] && grep -F -e "$expect" <<<"$out" | grep -q "FAIL"; then
    printf '  CAUGHT   %s\n' "$label"
  else
    fails=$((fails+1))
    printf '  MISSED   %s  (verify.sh exit %s; wanted a FAIL line mentioning: %s)\n' "$label" "$rc" "$expect"
    sed 's/^/           /' <<<"$out" | grep -E "FAIL|passed|failed" | head -3
  fi
}
fails=0
build_failures=0
echo "=== mutations of the container's own guarantees (each must be CAUGHT) ==="
run "an undeclared host mount is added" "UNDECLARED mounts" \
    "${HEALTHY[@]}" \
    -v "$scratch/home/Documents":/home/vscode/Documents
# Outside /home/vscode entirely — the case the old target-prefix filter could not see at all,
# and the most valuable one: /var/run/docker.sock is root on the host.
run "a host mount OUTSIDE /home/vscode (docker.sock-shaped)" "UNDECLARED mounts" \
    "${HEALTHY[@]}" \
    -v "$scratch/home":/host
run "the host's ~/.claude is mounted in" "is a host mount" \
    "${HEALTHY[@]}" \
    -v "$scratch/home":/home/vscode/.claude
# ...and at a SUBPATH, which is the case the equality test waved through and the prefix match was
# added for. Mounting only at the exact path left that change unwatched: deleting the prefix clause
# would have kept the harness green. settings.json is the worst one — it IS the posture.
run "the host's ~/.claude/settings.json is mounted in" "is a host mount" \
    "${HEALTHY[@]}" \
    -v "$scratch/home/settings.json":/home/vscode/.claude/settings.json
run "stock seccomp (nested sandbox cannot start)" "bubblewrap cannot create namespaces" \
    --cap-add=NET_ADMIN --user vscode "${BASE[@]}"
run "no NET_ADMIN (firewall cannot come up)" "NON-allowlisted host was permitted" \
    --security-opt seccomp="$SEC" --user vscode "${BASE[@]}"
run "runs as root" "runs as a non-root user" \
    --security-opt seccomp="$SEC" --cap-add=NET_ADMIN --user root "${BASE[@]}"

# The nested-bind exception must not be usable as a general one. A `--declare` naming anything
# OUTSIDE every declared target is the shape that would turn it into a hole — `/host` is the
# docker.sock-shaped mount two cases above — so the refusal is watched here rather than argued for
# in a comment. No extra `-v` is needed: what is under test is that verify.sh refuses to ACCEPT
# the name, which it must do whether or not something is mounted there.
run "a --declare outside every declared target" "is not inside any host BIND" \
    -e JKB_VERIFY_DECLARE=/host "${HEALTHY[@]}"

# Auto-memory sharing is a README promise whose entire mechanism is one symlink, which is exactly
# the shape 3c exists for. Skip the linking step and the assertion must say so — otherwise the
# container reports healthy while everything an agent learns in here dies with it.
run "auto-memory is not linked into the shared store" "auto-memory is not linked" \
    -e JKB_SKIP_MEMORY_LINK=1 "${HEALTHY[@]}"

# The base image ships /etc/sudoers.d/vscode with NOPASSWD:ALL, which makes the root-owned
# firewall, its snapshot and the pinned sudoers argument all bypassable with one sudo. The
# Dockerfile removes it; this puts it back and requires verify.sh to notice.
# The script sudo runs as root, and the allowlist beside it, are protected only by the directory
# they live in — which the base image owns, not this repo. `chmod 777` is the whole exploit.
mutant jkb-dev-writable-usrlocal "chmod 0777 /usr/local/bin /usr/local/share"
run "/usr/local/{bin,share} are writable by the agent" "is writable by" \
    "${HEALTHY[@]}"

# The PARENT, which governs replacing them — the previous mutant could not see this, because it
# chmod'd the two paths already covered.
mutant jkb-dev-writable-usrlocal-parent "chmod 0777 /usr/local"
run "/usr/local itself is writable by the agent" "is writable by" \
    "${HEALTHY[@]}"

mutant jkb-dev-blanket-sudo "printf 'vscode ALL=(root) NOPASSWD:ALL\\n' > /etc/sudoers.d/vscode && chmod 0440 /etc/sudoers.d/vscode"
run "blanket passwordless root is restored" "may run more than the firewall as root" \
    "${HEALTHY[@]}"

# The harness's own negative control. If an UNMUTATED container is reported CAUGHT, the matcher
# is matching something that is present when nothing is wrong — which is precisely the defect
# this file exists to detect in verify.sh, and it had it too.
echo
echo "=== self-test: an unmutated container must be reported MISSED ==="
before="$fails"
# Run the base container directly first: the control below is satisfied by a MISSED, and a
# container that never started is ALSO a MISSED. Without this, a host with no docker — or a broken
# image — reported "a healthy container does not trip the matcher" having observed nothing at all.
# ONE execution, judged twice: health first, then the matcher over the same captured output.
control_out="$(docker run --rm "${HEALTHY[@]}" "$IMAGE" bash -c "$SUBJECT" 2>&1)"
control_rc=$?
if [ "$control_rc" -ne 0 ] || ! grep -q "container checks passed" <<<"$control_out"; then
    printf '\033[31mthe unmutated container does not pass verify.sh (exit %s) — every MISSED above is\n' "$control_rc"
    printf 'unattributable, because a container that cannot run looks exactly like a guard that did not fire\033[0m\n'
    sed 's/^/    /' <<<"$control_out" | grep -E "FAIL|failed|not found|Error" | head -5
    exit 1
fi

judge "control: nothing mutated (MISSED is correct here)" "runs as a non-root user" \
      "$control_out" "$control_rc"
if [ "$fails" -gt "$before" ]; then
  self_ok=1; fails="$before"      # a MISSED here is the expected result, not a failure
  echo "  (correct: a healthy container does not trip the matcher)"
else
  self_ok=0
  echo "  MATCHER IS BROKEN: a healthy container was reported CAUGHT"
fi

echo
[ "$self_ok" -eq 1 ] || { printf '\033[31mthe matcher reports CAUGHT for a healthy container — no result here is trustworthy\033[0m\n'; exit 1; }
[ "$build_failures" -eq 0 ] || printf '\033[33m%d mutation(s) could not be built — nothing was verified for them\033[0m\n' "$build_failures"
[ "$fails" -eq 0 ] || { printf '\033[31m%d guard(s) did not fire\033[0m\n' "$fails"; exit 1; }
[ "$build_failures" -eq 0 ] || exit 1
printf '\033[32mevery guard fired, and the matcher was shown to discriminate\033[0m\n'
