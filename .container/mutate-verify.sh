#!/usr/bin/env bash
# Each case breaks ONE property the container is supposed to have. verify.sh must fail, and must
# fail naming that property — a guard nobody has watched fail is not a guard.
#
#   ./.container/mutate-verify.sh [image]           every guard, each watched failing
#   ./.container/mutate-verify.sh --control [image]  ONE healthy run, for "is this container ok"
#
# `--control` exists because assembling that `docker run` by hand goes wrong: it needs the seccomp
# profile, NET_ADMIN, both binds, and a preamble that raises the firewall, links the state, links
# the memory store and installs the posture — and a command missing any of those produces a dozen
# FAILs that read as a broken container. That is the same "a container that cannot run looks
# exactly like a guard that did not fire" this file already guards its own control against, so the
# correct invocation is here, defined ONCE, rather than written out in prose somewhere.
#
# Needs a Docker host and the image built (`docker build -t jkb-dev .container`), so the mutation
# run is this change's #[ignore] test — the host-side static checks live in check-config.sh.
#
# ONE MODE IS AN EXCEPTION AND THE GATE DEPENDS ON IT: `--print-flags` assembles the control's flag
# set and prints it, needing no daemon, and check-config.sh runs it on every ./scripts/check.sh to
# compare that set against container.json. That is why the docker preflights below are skipped for
# it rather than merely tolerated — they exit 0 with a skip, so an empty read would have passed the
# gate on every machine without a running daemon.
set -uo pipefail
REPO="${REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
CONTROL_ONLY=0
SHELL_CMD=""
PRINT_FLAGS=0
if [ "${1:-}" = --control ]; then CONTROL_ONLY=1; shift; fi
# `--print-flags`: the assembled control flag set, one argument per line, and nothing else.
#
# It exists so check-config.sh can assert — on any host, in ./scripts/check.sh — that what the
# control actually RUNS still carries what container.json declares. Asserting that against the
# `HEALTHY=(…)` source line was the obvious version and is the wrong one: it greps a bash array
# literal, matches inside comments, is blind to a later reassignment, and once HEALTHY is derived
# it is a guard over one spelling rather than over the result. This prints the assembly.
#
# IT MUST NOT BE BEHIND THE DOCKER PREFLIGHTS, which is why the argument parse moved above them.
# Those exit 0 with a skip when Docker is absent or stopped — correct for a mutation run, fatal
# here: the check would read empty output as a pass on every machine without a running daemon,
# which is most of them.
if [ "${1:-}" = --print-flags ]; then PRINT_FLAGS=1; shift; fi
# `--shell <command>`: run one command in a healthy container, after the same preamble every
# mutation and the control run. It exists so a DIAGNOSTIC never has to hand-roll the docker flags
# -- the failure this file's header already records, where an assembled-by-hand `docker run`
# omitted the seccomp profile, NET_ADMIN, the binds and the preamble, and produced a dozen FAILs
# that read as a broken container instead of as a wrong command. It runs no assertions and its
# exit code is the command's.
if [ "${1:-}" = --shell ]; then
    shift; [ $# -gt 0 ] || { echo "mutate-verify.sh: --shell needs a command" >&2; exit 2; }
    SHELL_CMD="$1"; shift
fi
IMAGE="${1:-jkb-dev}"

if [ "$#" -gt 1 ]; then
    echo "=== container guards ==="
    echo "   usage: $(basename "$0") [--control|--print-flags|--shell <command>] [image]" >&2
    echo "   got $# arguments: $*" >&2
    echo "   (a command copied with trailing prose attached is the usual cause)" >&2
    exit 2
fi
# EVERY PREFLIGHT THAT NEEDS A DOCKER HOST, skipped as a group under --print-flags, which needs
# none: it derives and prints the flag set and exits.
#
# Without this, a host with no docker on PATH reports every guard as MISSED and exits 1 saying
# "N guard(s) did not fire" — a security-shaped alarm for what is only a fact about the shell.
# Docker Desktop installs to ~/.docker/bin, which an interactive profile may export and a plain
# shell may not, so this is the normal way to meet it rather than an exotic one.
# USABILITY, not presence. `command -v docker` succeeds on the far more common host state — Docker
# Desktop installed but not running — and every mutation then printed MISSED with exit 125 before
# the control finally said they were unattributable. Ten container starts to deliver the exact
# ten-broken-guards alarm this block exists to remove, for the same fact about the host that the
# PATH case reports as a clean skip.
#
# THE SUBJECT HAS TO EXIST, and be named on purpose, for the same reason: without the image check,
# an image that is not there produces nine MISSED lines and three BUILD-FAILED blocks before the
# control finally reports them all unattributable — thirteen alarming lines for a fact about the
# command. It happened with a stray em dash pasted as the image name, copied out of prose where one
# followed the command; `docker run … — bash -c …` exits 125 ("could not start the container"),
# which `judge` reads as a non-zero verify.sh and reports as a guard that did not fire.
if [ "$PRINT_FLAGS" -eq 0 ]; then
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
    if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
        echo "=== container guards ==="
        echo "   no image named '$IMAGE'. Nothing was verified — this is NOT a passing result." >&2
        echo "   Build it first:  docker build -t jkb-dev .container" >&2
        exit 2
    fi
fi
# CLEANUP HAS THE SAME UID PROBLEM AS THE MOUNT, one level along. The container writes into the
# scratch knowledge base as uid 1000, creating directories it owns; removing a file needs write
# permission on its CONTAINING directory, so on a host whose user is not 1000 the plain `rm -rf`
# fails partway with "Permission denied" and leaves the tree behind. Pre-creating a level does not
# help -- the tree is arbitrarily deep -- so the only user who can remove it is root, in a
# container, which is what the fallback does using the image already built. Every step is
# best-effort and the last resort is SAYING SO: a leaked temp directory is a known annoyance in
# this repo, and a confusing "Permission denied" at the end of a passing run is worse, because it
# trains people to read past the last lines of output.
scratch="$(mktemp -d)"
cleanup() {
    rm -rf "$scratch" 2>/dev/null && return 0
    docker run --rm --user root -v "$scratch":/s "$IMAGE" rm -rf /s/jkb /s/home >/dev/null 2>&1 || true
    rm -rf "$scratch" 2>/dev/null \
        || echo "note: $scratch holds files owned by the container's user and could not be removed" >&2
    return 0
}
trap cleanup EXIT
mkdir -p "$scratch/jkb" "$scratch/home/Documents"
# THE KNOWLEDGE-BASE BIND MUST BE WRITABLE BY THE CONTAINER'S USER, WHOEVER THAT IS ON THIS HOST.
# This directory is created here, on the host, and bind-mounted at /home/vscode/.jkb -- so it
# carries the HOST user's uid, while the container runs as vscode (uid 1000). Those agree on a
# developer's machine and on Docker Desktop for macOS, which maps ownership; they do NOT agree on a
# GitHub runner, whose user is uid 1001. There, every run failed with
#   mkdir: cannot create directory '/home/vscode/.jkb/claude-memory': Permission denied
# and verify.sh reported `auto-memory is not linked (state: unlinked)`, which it treats as fatal --
# so the control could never pass and every mutation verdict above it was unattributable.
#
# This harness had never run on a non-1000 host, so the whole class was invisible. 0777 rather than
# a chown: it is a throwaway mktemp directory removed on exit, and matching an arbitrary image uid
# from the host would need root.
chmod 0777 "$scratch/jkb"
printf '{}' > "$scratch/home/settings.json"
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
# the repo AT /home/vscode/repos/jkb, which container.json now declares only the parent of —
# and the parent cannot be mounted instead, because in a `jkb task work` session $REPO's parent is
# `.jkb/work`, so the checkout would land at /home/vscode/repos/<session>. verify.sh accepts the
# name only because it is strictly inside a declared target; see its comment for why nesting is
# not granted automatically. Read from the environment so a mutation can supply a BAD declaration
# and watch the refusal fire — one SUBJECT, as the comment above requires.
# Split so `--shell` can reuse the setup half verbatim rather than restating it: two spellings of
# what a healthy container has been through is two answers to the question the control asks.
PREAMBLE='
      sudo -n /usr/local/bin/init-firewall.sh >/dev/null 2>&1
      . ./.container/lib.sh && dc_link_state /home/vscode
      [ -n "${JKB_SKIP_MEMORY_LINK:-}" ] || ./scripts/link-claude-memory.sh >/dev/null 2>&1
      ./scripts/auto-mode.sh install --force >/dev/null 2>&1'
SUBJECT="$PREAMBLE"'
      ./.container/verify.sh --declare "${JKB_VERIFY_DECLARE:-/home/vscode/repos/jkb}"'
# The baseline for the control and for every mutation, additive or subtractive: NO run site spells
# a flag set. Additive ones pass "${HEALTHY[@]}" plus what they add; subtractive ones pass "${MUT[@]}",
# which `without` derives from HEALTHY by removing exactly the named unit. Hand-spelling a reduced
# set is what let a mutation differ from the control in two ways at once — three times, each after
# a new flag joined HEALTHY and the copies did not follow.
# JKB_ACCEPT_NO_BWRAP is propagated when the caller has set it, so the harness and the container
# agree about which failures this host is known to produce. Not defaulted here: the acceptance is
# an operator's statement about a host, and a harness that quietly assumed it would hide the very
# failure it exists to detect everywhere else.
ACCEPT_ENV=(); [ "${JKB_ACCEPT_NO_BWRAP:-0}" = 1 ] && ACCEPT_ENV=(-e JKB_ACCEPT_NO_BWRAP=1)
# The same host-conditional AppArmor decision run.sh makes, for the same reason: docker-default
# denies `mount`, so without this the healthy container fails bubblewrap and every mutation verdict
# is judged against a container that is not the one run.sh produces.
. "$REPO/.container/lib.sh"
AA_ARGS=()
if dc_apparmor_mediates; then
    # TWO STEPS, AND THE STATUS IS CHECKED. `dc_require_apparmor_profile` exits on an unreadable
    # profile name -- but this used to call it inside an array element's command substitution, and
    # `exit` there ends only the SUBSHELL. The refusal printed its five lines and the empty name it
    # exists to reject flowed straight into `--security-opt apparmor=`, which docker reads as its
    # DEFAULT profile: every mutation and the control then ran under docker-default, the exact
    # silent state this whole change ends. This script is `set -uo pipefail` with no `-e`, so
    # nothing else would have stopped it. The helper's own comment claims "no future caller can
    # forget"; the first new caller did, eleven lines from run.sh's comment describing the same
    # escape. check-config.sh now asserts the CALL SHAPE across the tree, because a callee cannot
    # force this and a comment beside each call site is what already failed.
    aa_name="$(dc_require_apparmor_profile "$REPO/.container/apparmor-jkb-dev")" || exit 1
    AA_ARGS=(--security-opt "apparmor=$aa_name")
fi
# ${A[@]+"${A[@]}"}, NOT "${A[@]}". Both arrays are legitimately empty -- AA_ARGS on any host
# without AppArmor, ACCEPT_ENV whenever the acceptance is unset, i.e. every macOS run -- and
# expanding an empty array under `set -u` is an UNBOUND VARIABLE ABORT on bash 3.2, which is what
# /usr/bin/env bash is on macOS (3.2.57). So `--control`, the one sanctioned "is my container
# healthy" command and the one run.sh's own refusals point at, died on this line naming nothing.
# run.sh:147 already uses this idiom for exactly this reason.
# THE CONFIG HALF OF THE CONTROL IS READ FROM container.json, NOT RE-TYPED (D52.6).
#
# It used to be a hand copy, and a copy of the declaration is a copy that goes stale: commit
# 8266a2b added `--security-opt systempaths=unconfined` to container.json, run.sh picked it up
# automatically because it derives, and this array did not — so for four commits CI's
# `mutate-verify.sh --control` step, the one titled "The container is what it claims to be",
# started a container WITHOUT the flag, reported it healthy, and judged every mutation verdict
# against a container run.sh does not produce. It had also silently omitted `--pids-limit 4096`
# since the day that was declared, which nobody noticed at all — the copy was never right, only
# right enough.
#
# `--user` comes from `remoteUser` for the same reason and is the same kind of fact: bubblewrap
# cannot create a namespace as root, so a control running as a different user from the real
# container is not a control either.
#
# What is still spelled by hand is BASE alone, deliberately: those binds are NOT container.json's
# mounts (a scratch knowledge base, the repo bind this harness `--declare`s) and are documented
# above as the harness's own.
# Read through `$( )`: a `< <( )` here would discard dc_run_args' refusal, and because it emits
# each argument as it substitutes it, a refusal PART WAY would leave a truncated set — a control
# missing one declared flag, which is the exact state this derivation exists to end. See lib.sh,
# and check-config.sh, which fails the gate on that shape.
#
# THE EMPTINESS REFUSAL IS THE CALLEE'S, not a block here. It used to be written out at this one
# call site, which meant run.sh — the launcher that starts the container people actually attach to
# — did not make it, and would start a container with none of its declared security flags while
# reporting nothing wrong. A rule two callers must remember is the defect; `dc_run_args` refuses.
CFG="$REPO/.container/container.json"
RUNARGS=()
_ra="$(dc_run_args "$CFG" "$REPO")" || {
    echo "mutate-verify: the control's security flags come from container.json's runArgs (above)." >&2
    echo "  Refusing to certify a container assembled without them." >&2
    exit 2
}
while IFS= read -r _l; do [ -n "$_l" ] && RUNARGS+=("$_l"); done <<<"$_ra"

USER_ARGS=(); _u="$(dc_remote_user "$CFG")"; [ -n "$_u" ] && USER_ARGS=(--user "$_u")

# containerEnv, from the same reader run.sh uses. It is on the DECLARATION side of the line lib.sh
# draws: the control copies what the declaration says and substitutes only what is a property of
# the host's data, which is the mount sources alone. Leaving it out meant the control ran without
# the environment the real container carries -- and `JKB_EGRESS_ACCEPT_UNFILTERED` is exactly the
# entry somebody will one day set to "1" (D50.6, the documented escape), after which verify.sh
# reports a FAILURE in the real container on every run while the control, not carrying it, went on
# printing "the container is what it claims to be". Every declared value happens to equal its own
# default today, which is why the omission has cost nothing so far and why nobody noticed.
ENV_ARGS=()
_ce="$(dc_container_env "$CFG" "$REPO")" || {
    echo "mutate-verify: container.json's containerEnv could not be read (above)." >&2
    exit 2
}
if [ -n "$_ce" ]; then
    while IFS= read -r _l; do [ -n "$_l" ] && ENV_ARGS+=(--env "$_l"); done <<<"$_ce"
fi

HEALTHY=("${RUNARGS[@]}" ${AA_ARGS[@]+"${AA_ARGS[@]}"} ${USER_ARGS[@]+"${USER_ARGS[@]}"} ${ENV_ARGS[@]+"${ENV_ARGS[@]}"} ${ACCEPT_ENV[@]+"${ACCEPT_ENV[@]}"} "${BASE[@]}")

# THE ASSEMBLED CONTROL, for check-config.sh to assert against the declaration. Printed here
# because this is where it is assembled: printing the derivation instead would compare
# container.json with itself.
if [ "$PRINT_FLAGS" -eq 1 ]; then printf '%s\n' "${HEALTHY[@]}"; exit 0; fi

# A MUTATION CHANGES EXACTLY ONE THING, and hand-spelling the reduced flag set is how that stopped
# being true — three times, the same way. The sets dropped $AA_ARGS when the AppArmor profile
# joined HEALTHY (so "stock seccomp" reported CAUGHT on an AppArmor host whether or not the seccomp
# profile was load-bearing, because removing the profile alone breaks bwrap); then they dropped
# `systempaths=unconfined` when the /proc unmask joined it; and when five sites were converted to
# `without`, the two AppArmor arms were left spelling their own and immediately drifted the same
# way. Not one of those was a mistake made AT a mutation — each was the consequence of writing the
# control's flags out a second time, which is why the answer is that no site does.
# `without <ere>...` sets $MUT to every element of HEALTHY matching none of the patterns, treating
# a flag and its value as one unit so a `--security-opt` can never survive the value it introduced.
#
# PAIRING IS A RULE, NOT A LIST OF FLAGS. It used to be `takes_value`, a hand-maintained set
# (-v -e -w --user --mount --security-opt) that had to be kept in step with a hand-written HEALTHY —
# and deriving HEALTHY from container.json immediately brought in `--pids-limit 4096`, which that
# list does not know. `without` would have dropped the flag and left `4096` behind as a stray
# argument docker reads as the image name: a harness bug, reported as a guard that did not fire.
# The rule needs no list, because it is true of every element HEALTHY can hold: AN ELEMENT THAT
# DOES NOT BEGIN WITH `-` IS THE VALUE OF THE ELEMENT BEFORE IT.
#
# Residual, stated rather than guarded: a docker option whose VALUE begins with `-` would pair
# wrongly. None exists here, and the guard for it is the unit count in the still-open nit — not
# added blind, since nothing on this host can watch it fail.
without() {
  local pats=("$@") i=0 n=${#HEALTHY[@]} e nxt p drop unit paired
  MUT=()
  while [ "$i" -lt "$n" ]; do
    e="${HEALTHY[$i]}"; paired=0; nxt=""; unit="$e"
    if [ "$((i+1))" -lt "$n" ]; then
      nxt="${HEALTHY[$((i+1))]}"
      case "$nxt" in -*) nxt="" ;; *) unit="$e $nxt"; paired=1 ;; esac
    fi
    drop=0
    for p in "${pats[@]}"; do [[ "$unit" =~ $p ]] && drop=1; done
    if [ "$drop" -eq 0 ]; then
      MUT+=("$e"); [ "$paired" -eq 1 ] && MUT+=("$nxt")
    fi
    i=$((i + 1 + paired))
  done
  # A PATTERN THAT MATCHED NOTHING IS A MUTATION THAT DID NOT HAPPEN, which would run the healthy
  # container and report the guard as broken. Refused rather than reported, because every caller
  # below is about to hand $MUT to `run` and there is nothing useful it could do with a set that
  # was not reduced.
  if [ "${#MUT[@]}" -eq "$n" ]; then
    echo "mutate-verify: 'without ${pats[*]}' matched nothing in HEALTHY — the mutation would be a no-op" >&2
    exit 1
  fi
}

# One healthy run, printed verbatim. Uses the SAME flags and the SAME preamble every mutation
# runs in, so "is my container ok" and "did this guard fire" cannot be answered about different
# containers.
if [ -n "$SHELL_CMD" ]; then
    echo "=== a healthy container, running your command after the standard preamble ==="
    docker run --rm "${HEALTHY[@]}" "$IMAGE" bash -c "$PREAMBLE
      $SHELL_CMD"
    exit $?
fi

if [ "$CONTROL_ONLY" -eq 1 ]; then
    echo "=== one healthy container (the same subject every mutation runs against) ==="
    docker run --rm "${HEALTHY[@]}" "$IMAGE" bash -c "$SUBJECT"
    rc=$?
    echo
    if [ "$rc" -eq 0 ]; then
        echo "the container is what it claims to be (verify.sh exit 0)"
    else
        echo "verify.sh exited $rc — the FAIL lines above are real, and about THIS container." >&2
        echo "  Note the knowledge base here is an empty scratch directory, not your ~/.jkb:" >&2
        echo "  this checks the container, not your data." >&2
    fi
    exit "$rc"
fi

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
    caught=$((caught+1))
    # The EXPECT, not the count. Coverage is a property of which failure paths in verify.sh were
    # driven, and several mutations legitimately share one — so counting mutations answers a
    # different question from the one the summary asks.
    caught_expects+=("$expect")
    printf '  CAUGHT   %s\n' "$label"
  else
    fails=$((fails+1))
    printf '  MISSED   %s  (verify.sh exit %s; wanted a FAIL line mentioning: %s)\n' "$label" "$rc" "$expect"
    sed 's/^/           /' <<<"$out" | grep -E "FAIL|passed|failed" | head -3
  fi
}
fails=0
build_failures=0
caught=0
caught_expects=()
echo "=== mutations of the container's own guarantees (each must be CAUGHT) ==="
run "an undeclared host mount is added" "UNDECLARED mounts" \
    "${HEALTHY[@]}" \
    -v "$scratch/home/Documents":/home/vscode/Documents
# Outside /home/vscode entirely — the case the old target-prefix filter could not see at all,
# and the most valuable one: /var/run/docker.sock is root on the host.
run "a host mount OUTSIDE /home/vscode (docker.sock-shaped)" "UNDECLARED mounts" \
    "${HEALTHY[@]}" \
    -v "$scratch/home":/host
# THE ONE SUBTRACTIVE CASE THAT WAS MISSING. `BASE` supplies the `~/.jkb` bind unconditionally
# and every mutation reuses it, so no run ever reached `kb_mounted=no` — the assertion added
# because the old `[ -d /home/vscode/.jkb ]` form passed in a container where the bind was ABSENT
# had itself never been watched failing. It isolates cleanly: a missing declared mount is not an
# extra one, so the boundary check still passes, and the memory linker reports `linked` against a
# container-local store that dies with the container. Unlike the paths the coverage note below
# excuses, this one needs a docker flag and nothing else.
without '/home/vscode/\.jkb$'
run "the knowledge base is NOT mounted" "knowledge base is mounted" "${MUT[@]}"
run "the host's ~/.claude is mounted in" "is a host mount" \
    "${HEALTHY[@]}" \
    -v "$scratch/home":/home/vscode/.claude
# ...and at a SUBPATH, which is the case the equality test waved through and the prefix match was
# added for. Mounting only at the exact path left that change unwatched: deleting the prefix clause
# would have kept the harness green. settings.json is the worst one — it IS the posture.
run "the host's ~/.claude/settings.json is mounted in" "is a host mount" \
    "${HEALTHY[@]}" \
    -v "$scratch/home/settings.json":/home/vscode/.claude/settings.json
# THE TWO FLAGS THE NESTED SANDBOX NEEDS, each watched failing on its own. They are separate
# refusals in separate subsystems: seccomp decides whether the namespace syscalls are allowed at
# all, and docker's masked /proc paths decide whether a proc mount is permitted INSIDE the
# namespace once created. Both end at the same verify.sh assertion, which is why that assertion had
# to grow `--proc /proc` before either of these could discriminate — without it the probe passed in
# the state the unmask exists to fix, and the container shipped that way.
#
# NEITHER CAN DISCRIMINATE WHILE BUBBLEWRAP IS A KNOWN FAILURE, so on such a host they are
# announced as skipped rather than run. Each requires verify.sh to fail naming bubblewrap -- and if
# the HEALTHY container already fails that same assertion, it would report CAUGHT whether or not
# the flag was load-bearing. That is the "guard that cannot fire" shape this whole directory keeps
# producing, and reporting CAUGHT for it would be the worst version: a green line asserting the
# flag is load-bearing, on a host where nothing tested it.
if [ "${JKB_ACCEPT_NO_BWRAP:-0}" = 1 ]; then
    printf '  SKIPPED  stock seccomp (nested sandbox cannot start)\n'
    printf '  SKIPPED  the /proc unmask is dropped (nested sandbox cannot start)\n'
    printf '           cannot discriminate while bubblewrap fails in the healthy container too;\n'
    printf '           they become meaningful again when that is fixed.\n'
else
    without 'seccomp='
    run "stock seccomp (nested sandbox cannot start)" "bubblewrap cannot create namespaces or mount /proc" "${MUT[@]}"
    without 'systempaths=unconfined'
    run "the /proc unmask is dropped (nested sandbox cannot start)" "bubblewrap cannot create namespaces or mount /proc" "${MUT[@]}"
fi
# NO NET_ADMIN, WITH THE OVERRIDE ARMED -- and the override is what makes this mutation
# judgeable at all. Without it the entrypoint (correctly) refuses to boot an unbounded container,
# so $SUBJECT never runs, verify.sh never runs, and the assertion this mutation names is
# unreachable: the harness printed MISSED for ever, which is a tooling outcome dressed as a guard
# that did not fire. Predicted by a reviewer, then confirmed by the first real run of this file.
#
# So the container is allowed to boot, and what is tested is what the label says: that verify.sh
# NOTICES the firewall never came up. The entrypoint's own refusal on this same configuration is a
# separate property and is covered by entrypoint.sh --self-test; it cannot be judged here, because
# `judge` requires the expectation and the word FAIL on one line and the refusal is not a verify.sh
# FAIL line.
#
# A REPLACEMENT, like `--user` below, and it had to become one the moment `containerEnv` joined the
# derived half: HEALTHY now carries the declared `--env JKB_EGRESS_ACCEPT_UNFILTERED=0`, so passing
# the override BEFORE $MUT left docker two values for one key and its last-wins rule chose the
# declared `0`. The entrypoint would then refuse to boot exactly as it does unarmed, $SUBJECT would
# never run, and this mutation would report MISSED for ever — the tooling outcome its own comment
# above says it exists to avoid. Dropped as a unit and re-added last instead, so there is one.
without 'NET_ADMIN' 'JKB_EGRESS_ACCEPT_UNFILTERED'
run "no NET_ADMIN, override armed (verify.sh must notice egress is unrestricted)" "NON-allowlisted host was permitted" \
    "${MUT[@]}" --env JKB_EGRESS_ACCEPT_UNFILTERED=1
# The one REPLACEMENT rather than a subtraction: `--user` is removed as a unit and re-added, so a
# second `--user` cannot be left for docker's last-wins rule to resolve.
without '^--user '
run "runs as root" "runs as a non-root user" "${MUT[@]}" --user root

# The nested-bind exception must not be usable as a general one. A `--declare` naming anything
# OUTSIDE every declared target is the shape that would turn it into a hole — `/host` is the
# docker.sock-shaped mount two cases above — so the refusal is watched here rather than argued for
# in a comment. No extra `-v` is needed: what is under test is that verify.sh refuses to ACCEPT
# the name, which it must do whether or not something is mounted there.
run "a --declare outside every declared target" "is not inside any host BIND" \
    -e JKB_VERIFY_DECLARE=/host "${HEALTHY[@]}"

# THE TWO AppArmor STATES THIS WHOLE CHANGE EXISTS TO DETECT, each one docker flag away from being
# watchable and neither watched until now. `docker-default` is the silent state: the container
# starts, the nested sandbox does not, and before this it was produced only as a SIDE EFFECT of the
# stock-seccomp mutation above and never judged. `unconfined` is the other way to lose the boundary
# -- it runs the sandbox with no profile at all, which is what the profile exists to avoid doing.
# Skipped as a group on a host without AppArmor: there both arms produce the no-profile `note`, so
# they would be MISSED for a fact about the machine.
if [ ${#AA_ARGS[@]} -eq 0 ]; then
    printf '  SKIPPED  the two AppArmor arms (no AppArmor on this host, so neither state is reachable)\n'
else
    # DERIVED, like every other subtractive mutation. These two were the last sites spelling a flag
    # set by hand, and they had already drifted: neither carried `systempaths=unconfined` once
    # container.json declared it, so each differed from the control in TWO ways and additionally
    # failed the bwrap assertion for a reason that is not AppArmor. The comment above HEALTHY
    # claimed no site could drop a flag by omission; it was true of the five `without` callers and
    # these two were not among them.
    without 'apparmor='
    run "no --security-opt apparmor (docker-default, the silent state)" "AppArmor is applying docker-default" \
        "${MUT[@]}"
    # A REPLACEMENT, the idiom the `--user root` case uses: the profile is removed as a unit and a
    # different one appended, so a second `--security-opt apparmor=` cannot be left for docker's
    # last-wins rule to resolve.
    without 'apparmor='
    run "apparmor=unconfined (the boundary is dropped entirely)" "AppArmor is not confining this container" \
        "${MUT[@]}" --security-opt apparmor=unconfined
fi

# Auto-memory sharing is a README promise whose entire mechanism is one symlink, which is exactly
# the shape 3c exists for. Skip the linking step and the assertion must say so — otherwise the
# container reports healthy while everything an agent learns in here dies with it.
run "auto-memory is not linked into the shared store" "auto-memory is not linked" \
    -e JKB_SKIP_MEMORY_LINK=1 "${HEALTHY[@]}"

# THE FIREWALL'S OWN REFUSAL PATH, which nothing else here drives. Every other case exercises the
# raise succeeding or never starting; this one makes it run and decide it cannot establish an
# allowlist, which is the only route through `fail_closed` — the deny-all it installs, its IPv6
# block, and its verdict line had never executed in a container.
#
# `--dns 127.0.0.1` rather than an unroutable address: nothing listens on the container's loopback
# port 53, so every lookup is refused immediately instead of timing out fifteen times over. With no
# domain resolvable the `resolved -eq 0` arm fires, `fail_closed` denies everything, and the
# container is then too tight to work in — which is exactly what verify.sh must say. A container
# that cannot reach its own allowlist is a real state (a laptop offline at container start), and
# the honest report is "the firewall is too tight", not silence.
run "the firewall cannot resolve any allowlisted domain" "the live chain denies everything and has no allowlist" \
    --dns 127.0.0.1 "${HEALTHY[@]}"

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
run "blanket passwordless root is restored" "may run more than the firewall and the egress probe as root" \
    "${HEALTHY[@]}"

# The harness's own negative control. If an UNMUTATED container is reported CAUGHT, the matcher
# is matching something that is present when nothing is wrong — which is precisely the defect
# this file exists to detect in verify.sh, and it had it too.
echo
echo "=== self-test: the matcher must stay quiet about a HEALTHY container ==="
# Run the base container directly first, and require it to be healthy: a container that never
# started produces output the matcher is also quiet about, so without this the harness reported
# "shown to discriminate" having observed nothing at all.
# ONE execution, judged twice: health first, then the matcher over the same captured output.
control_out="$(docker run --rm "${HEALTHY[@]}" "$IMAGE" bash -c "$SUBJECT" 2>&1)"
control_rc=$?
# Exit 3 is "every failure above is a condition this container was configured to accept" -- a
# healthy container for this harness's purposes, since the mutations are judged against the same
# baseline. Requiring exit 0 here would make the acceptance mechanism unusable: the control would
# refuse the very state an operator declared, and every verdict would read as unattributable.
control_ok=no
[ "$control_rc" -eq 0 ] && grep -q "container checks passed" <<<"$control_out" && control_ok=yes
[ "$control_rc" -eq 3 ] && grep -q "configured to accept" <<<"$control_out" && control_ok=yes
if [ "$control_ok" != yes ]; then
    printf '\033[31mthe unmutated container does not pass verify.sh (exit %s) — every MISSED above is\n' "$control_rc"
    printf 'unattributable, because a container that cannot run looks exactly like a guard that did not fire\033[0m\n'
    sed 's/^/    /' <<<"$control_out" | grep -E "FAIL|failed|not found|Error" | head -5
    exit 1
fi

# THE HALF THAT CAN ACTUALLY BE WRONG. Routing the control through `judge` proved nothing: the
# health check above has already established `control_rc` is 0, and `judge` reports CAUGHT only
# when rc is non-zero — so the MISSED branch was taken by construction and the `MATCHER IS BROKEN`
# arm was unreachable. A negative control that cannot fail is exactly the defect this file exists
# to find in verify.sh.
#
# What the matcher really claims is the FAIL filter: `assert()` prints the SAME label on its ok and
# its fail paths, so matching a label alone once reported two deleted guards as CAUGHT. Ask that
# directly, against a container known healthy — the label must be present (or the test is matching
# nothing) and must NOT be on a FAIL line.
self_ok=1
control_label="runs as a non-root user"
if ! grep -qF -e "$control_label" <<<"$control_out"; then
  self_ok=0
  echo "  MATCHER PROVES NOTHING: a healthy container's output never mentions \"$control_label\","
  echo "  so every CAUGHT above matched a string this harness cannot show discriminates."
elif grep -F -e "$control_label" <<<"$control_out" | grep -q "FAIL"; then
  self_ok=0
  echo "  MATCHER IS BROKEN: that label is on a FAIL line in a HEALTHY container, so a CAUGHT"
  echo "  above means nothing — the matcher fires when nothing is wrong."
else
  echo "  (correct: the label appears in a healthy container and never on a FAIL line)"
fi

echo
[ "$self_ok" -eq 1 ] || { printf '\033[31mthe matcher reports CAUGHT for a healthy container — no result here is trustworthy\033[0m\n'; exit 1; }
[ "$build_failures" -eq 0 ] || printf '\033[33m%d mutation(s) could not be built — nothing was verified for them\033[0m\n' "$build_failures"
[ "$fails" -eq 0 ] || { printf '\033[31m%d guard(s) did not fire\033[0m\n' "$fails"; exit 1; }
[ "$build_failures" -eq 0 ] || exit 1

# WHAT WAS DRIVEN, NOT "EVERY GUARD". This printed "every guard fired" while driving the mutations
# listed above and nothing else — so an assertion added to verify.sh tomorrow, with no mutation for
# it, was covered by that sentence without ever being watched failing. That is the same claim-more-
# than-was-established shape every guard in this directory exists to stop, in the summary line of
# the harness that judges them. No silent cap: say the number and say which are uncovered.
#
# COUNTED AS PATHS, NOT AS MUTATIONS, because those are two different quantities and the first
# version printed one as the other: 13 mutations sharing 10 expect strings drive 8 distinct
# failure paths, and "13 of 21" claimed half again as much coverage as existed. Worse, it was
# self-erasing — add five mutations of already-covered properties and `caught` reaches the
# denominator, at which point the whole "the rest are NOT covered" block disappears while a dozen
# paths have still never been watched failing. `assert` call sites are in the denominator too;
# they were not, so two paths that ARE driven were not even counted as existing.
V="$REPO/.container/verify.sh"
covered=""
if [ "${#caught_expects[@]}" -gt 0 ]; then
    covered="$(for want in "${caught_expects[@]}"; do
        grep -nF -e "$want" "$V" 2>/dev/null | cut -d: -f1
    done | sort -un)"
fi
all_paths="$(grep -nE '^[[:space:]]*(bad "|assert )' "$V" 2>/dev/null | cut -d: -f1 | sort -un)"
n_all="$(printf '%s' "$all_paths" | grep -c '^' || true)"
# NOT `comm`, which requires both inputs in ITS collating order — bytes — while these are line
# numbers sorted NUMERICALLY. The two orders disagree the moment the file passes 99 lines: `100`
# sorts after `12` here and before it there. Measured against this very verify.sh, with ten of
# its twenty-five paths marked covered: every one of the twenty-five came back uncovered and the
# summary read `0 of 25`. It errs towards claiming LESS than was established, which is the safe
# direction and is why it survived a round — but a coverage report that always says none is one
# nobody reads, and it buries the paths that genuinely have never been driven.
#
# `grep -vxF -f` asks set membership instead, which needs no ordering from either side. An empty
# `covered` leaves an empty pattern file, which matches nothing, so `-v` yields every path: the
# right answer for a run that caught nothing, and the reason this needs no special case.
uncovered="$(printf '%s\n' "$all_paths" | grep -v '^$' \
    | grep -vxF -f <(printf '%s\n' "$covered" | grep -v '^$') || true)"
n_cov=$(( n_all - $(printf '%s' "$uncovered" | grep -c '^' || true) ))
printf '\033[32m%d mutation(s) caught, and the matcher was shown to discriminate\033[0m\n' "$caught"
if [ "$n_all" -eq 0 ]; then
    # NOT the same as full coverage, though it renders identically without this arm: an empty
    # enumeration makes `uncovered` empty too, so the else branch below certified "all 0 failure
    # paths were driven" — the claim-more-than-was-established shape this whole block was
    # rewritten to remove, in the line that reports it. Reachable without any adversary: verify.sh
    # renamed or moved, `$REPO` pointing somewhere else, or the `bad "`/`assert ` shape refactored.
    # check-config.sh already refuses its own version of this ten lines from here, and unlike that
    # one nothing catches a regression here, because this harness needs Docker and is in no gate.
    printf '\033[31m  could not enumerate any failure paths in %s — this coverage report has\n' "$V"
    printf '  certified NOTHING. Check the path and the `bad "` / `assert ` shapes it looks for.\033[0m\n'
    # AND THE VERDICT, not only the rendering. This block is the last statement in the file, so
    # printing alone left `$?` at 0 and a harness that certified nothing reported success —
    # `check-config.sh` exits non-zero for its version of this. Deliberately NOT extended to the
    # partial-coverage arm below: that one is a READING of a working meter, and the file already
    # says why it is reported and not gated (several paths need a broken machine, not a docker
    # flag). This arm is a BROKEN meter, which is a different claim and must not pass.
    exit 1
elif [ -n "$uncovered" ]; then
    printf '  %d of %d failure paths in verify.sh were driven here. NOT covered by this run:\n' \
        "$n_cov" "$n_all"
    while IFS= read -r n; do
        [ -n "$n" ] || continue
        printf '    verify.sh:%s  %s\n' "$n" "$(sed -n "${n}p" "$V" | sed 's/^[[:space:]]*//' | cut -c1-88)"
    done <<<"$uncovered"
    printf '  Several need a broken machine (an unreadable mount table, a failed link) rather than a\n'
    printf '  docker flag, which is why this is reported and not a gate.\n'
else
    printf '  all %d failure paths in verify.sh were driven\n' "$n_all"
fi
