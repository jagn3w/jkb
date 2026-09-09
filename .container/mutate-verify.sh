#!/usr/bin/env bash
# Each case breaks ONE property the container is supposed to have. verify.sh must fail, and must
# fail naming that property — a guard nobody has watched fail is not a guard.
#
#   ./.container/mutate-verify.sh [image]            every guard, each watched failing
#   ./.container/mutate-verify.sh --control [image]  ONE healthy run, for "is this container ok"
#   ./.container/mutate-verify.sh --ladder [image]   WHY bubblewrap can or cannot start here
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
# NO MODE HERE RUNS WITHOUT DOCKER any more. `--print-flags` existed so check-config.sh could
# compare the control's assembled flags against container.json on every ./scripts/check.sh -- and
# under D54.1 the control no longer assembles anything: it asks `run.sh --print-args --posture`,
# which IS offline-observable. An offline guard that wants to know what the control runs with asks
# run.sh, which is the one place the answer is produced.
set -uo pipefail
REPO="${REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
CONTROL_ONLY=0
SHELL_CMD=""
LADDER=0
if [ "${1:-}" = --control ]; then CONTROL_ONLY=1; shift; fi
# `--ladder`: WHY bubblewrap can or cannot start on this host, as a table (D54.3).
#
# The rungs are the control minus one security flag at a time, so the top rung IS the container
# that ships and a rung cannot lose a flag by omission. This replaced four hand-spelled `docker
# run` invocations in ci.yml, which drifted from the shipped set in every review round they
# survived -- and which nobody could run by hand, being YAML.
if [ "${1:-}" = --ladder ]; then LADDER=1; shift; fi
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
    echo "   usage: $(basename "$0") [--control|--ladder|--shell <command>] [image]" >&2
    echo "   got $# arguments: $*" >&2
    echo "   (a command copied with trailing prose attached is the usual cause)" >&2
    exit 2
fi
# EVERY PREFLIGHT THAT NEEDS A DOCKER HOST. Every mode does now, so there is no exemption.
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
if true; then
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
# THE CONTROL'S SECURITY CONFIGURATION IS run.sh's, ASKED FOR (D54.1) -- not derived again here.
#
# `run.sh --print-args --posture` prints the half of the assembly that makes a container a jkb-dev
# container: --user, container.json's runArgs, containerEnv, and the host-conditional AppArmor
# flag. It omits the instance half -- the name, --detach, the workdir and the MOUNTS -- because the
# harness supplies its own scratch knowledge base and its own repo bind and must not inherit this
# host's.
#
# WHY IT IS ASKED FOR RATHER THAN DERIVED. This block used to re-read container.json with the same
# readers run.sh uses, one file apart, and then a static guard in check-config.sh kept the two
# agreeing. Twelve review findings and eight must-fixes are that arrangement: the guard compared
# only the security-opt pairs, then only the runArgs third, then read a hand-picked region of this
# file, then matched only one spelling of the argument -- each fix correct, each round finding the
# next hole, because a guard over two copies cannot be complete. There is one copy now, so the
# block table, the reader pin and their mutations are gone rather than fixed a fourth time.
#
# THE FAILURE DIRECTION IS SAFE. A flag added to run.sh flows in here (the control carries MORE of
# the launcher, never less). A flag DROPPED from run.sh is caught by verify.sh, whose assertions
# derive from container.json itself and not from this set -- so the two things that could go wrong
# are covered by different evidence than the thing that produces them.
. "$REPO/.container/lib.sh"
POSTURE=()
_pa="$("$REPO/.container/run.sh" --print-args --posture "$REPO")" || {
    echo "mutate-verify: run.sh could not print the container's security configuration (above)." >&2
    echo "  Refusing to certify a container assembled without it." >&2
    exit 2
}
while IFS= read -r _l; do [ -n "$_l" ] && POSTURE+=("$_l"); done <<<"$_pa"
# An empty set is a refusal, not an empty control. run.sh refuses this itself, so this is the
# second reader of one fact -- kept because the cost of being wrong is a container certified with
# no security flags at all, and because `$( )` of a script that died still yields empty here.
[ "${#POSTURE[@]}" -gt 0 ] || {
    echo "mutate-verify: run.sh printed no security configuration -- refusing to certify." >&2
    exit 2
}

# ${A[@]+"${A[@]}"}, NOT "${A[@]}". ACCEPT_ENV is legitimately empty whenever the acceptance is
# unset, i.e. every macOS run, and expanding an empty array under `set -u` is an UNBOUND VARIABLE
# ABORT on bash 3.2, which is what /usr/bin/env bash is on macOS (3.2.57). So `--control`, the one
# sanctioned "is my container healthy" command, died on this line naming nothing.
HEALTHY=("${POSTURE[@]}" ${ACCEPT_ENV[@]+"${ACCEPT_ENV[@]}"} "${BASE[@]}")

# WHETHER AN AppArmor PROFILE IS IN THE CONTROL is read off the control, not decided a second time.
# It used to be `${#AA_ARGS[@]} -eq 0`, i.e. this file's own copy of run.sh's host decision; the two
# AppArmor mutations below are skipped as a group when there is none, and skipping them on a
# DIFFERENT answer from the one the container was started with is a guard reporting about another
# machine.
control_has() { printf '%s\n' "${HEALTHY[@]}" | grep -qF -- "$1"; }
control_has_apparmor() { control_has 'apparmor='; }

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
    # THE FLAGS THIS RUN USES, printed BY this run rather than by a second process -- which mints
    # its own scratch directory, so a separately-derived report named a ~/.jkb bind that this run's
    # EXIT trap had already deleted. Nothing parses this: it is here so a reader of the output can
    # see what the container was started with.
    printf '  flags: %s\n' "${HEALTHY[*]}"
    docker run --rm "${HEALTHY[@]}" "$IMAGE" bash -c "$PREAMBLE
      $SHELL_CMD"
    exit $?
fi

# WHY BUBBLEWRAP CAN OR CANNOT START HERE (D54.3). The rungs are the control MINUS one security
# flag at a time, so the top rung IS the container that ships and no rung can lose a flag by
# omission -- which is what four hand-spelled `docker run` arms in ci.yml did, in every review
# round they survived. Each row prints the flags of the array it actually ran, so attribution is
# read off the run rather than off the label.
#
# SUBTRACTIVE, NOT ADDITIVE. An additive ladder has to spell a starting set, and that set is a
# second copy of the shipped configuration -- the copy this whole design exists to delete. Reading
# it is the same: rung N is the container minus the flags named on its own line.
#
# It runs no assertions and returns 0 whatever it measures: it is an experiment, and a host where
# the nested sandbox cannot start is a result rather than a failure of this command.
if [ "$LADDER" -eq 1 ]; then
    echo "=== why bubblewrap can or cannot start here ==="
    echo
    uname -a
    echo
    # THE IMAGE'S OWN ENTRYPOINT IS KEPT, so the top rung really is the container as it ships. The
    # arms this replaced used `--entrypoint bash` because they granted no NET_ADMIN, so the
    # entrypoint refused to boot on unbounded egress and bwrap never ran. These rungs subtract only
    # the three security flags under test and keep NET_ADMIN, so that cause does not apply -- and a
    # rung the entrypoint does refuse reports `did-not-run` with its reason rather than a verdict.
    ladder_row() { # ladder_row <label> <flags...>
        local label="$1"; shift
        local out ns proc why
        # RELATIVE TO THE WORKDIR $BASE SETS, not an absolute path spelled again here: the bind
        # target is BASE's to choose, and a second copy of it would break this silently -- the
        # container would start, the probe would not be found, and the row would read did-not-run
        # as though the kernel had refused something.
        out="$(docker run --rm "$@" "$IMAGE" bash -c './.container/bwrap-probe.sh' 2>&1)"
        ns="$(printf   '%s\n' "$out" | sed -n 's/^BWRAP-NS=//p')"
        proc="$(printf '%s\n' "$out" | sed -n 's/^BWRAP-PROC=//p')"
        why="$(printf  '%s\n' "$out" | sed -n 's/^BWRAP-WHY=//p')"
        # AN ARM THAT DID NOT RUN SAYS SO, rather than being read as a result: "no BWRAP-NS=" and
        # "the probe never executed" are different facts and must not print the same way.
        [ -n "$ns" ] || { ns="did-not-run"; proc="did-not-run"; why="$(printf '%s' "$out" | tail -1)"; }
        printf '  %-46s namespaces=%-12s proc-mount=%s\n' "$label" "$ns" "$proc"
        [ -n "$why" ] && printf '  %-46s   why: %s\n' "" "$why"
        printf '  %-46s   flags: %s\n' "" "$*"
    }
    ladder_row "[shipped]  the container as it ships" "${HEALTHY[@]}"

    # THE RUNGS ARE BUILT FROM WHAT THE CONTROL ACTUALLY CARRIES, so `without` is never asked to
    # match nothing. Its no-op refusal is an `exit 1` -- correct for a mutation run, FATAL for a
    # diagnostic: a declaration that stops carrying the unmask (a state verify.sh 2c supports, with
    # a `note` branch saying so) would have killed this table after its first row, with a message
    # naming a mutation this command does not perform, in the CI step you read when the control
    # fails. Only the AppArmor rung was guarded this way; the other two flags were not, and the
    # guard is now the loop rather than one arm's special case.
    #
    # SKIPPED WITH A REASON, never silently: a rung subtracting a flag the control does not carry
    # would be identical to the row above it and would read as that flag making no difference.
    acc=(); label=""
    for spec in 'systempaths=unconfined|/proc unmask|unmask' 'apparmor=|AppArmor profile|aa' 'seccomp=|seccomp profile|seccomp'; do
        pat="${spec%%|*}"; rest="${spec#*|}"; name="${rest%%|*}"; short="${rest#*|}"
        if ! control_has "$pat"; then
            printf '  %-46s (skipped: the control carries no %s, so it is in no rung)\n' "[-$short]" "$pat"
            continue
        fi
        acc+=("$pat"); label="$label -$short"
        without "${acc[@]}"
        ladder_row "[${label# }]  ...minus the $name" "${MUT[@]}"
    done
    echo
    echo "  Read DOWN: the first row is what ships. A column that flips on a row names the flag"
    echo "  that row removed. 'not-reached' means the namespace step failed first, so that row"
    echo "  establishes nothing about the proc mount."
    exit 0
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
# THE TWO FLAGS THE NESTED SANDBOX NEEDS, each watched failing on its own -- and they are now
# judged by DIFFERENT verify.sh assertions, which is what decides whether each can be skipped.
#
# The /proc unmask is judged on its DIRECT effect: verify.sh counts the submounts docker puts over
# /proc and fails when the declaration says there should be none (D53.1). That is a mountinfo read,
# so it fires whether or not bubblewrap works, and this arm therefore runs on every host.
#
# Seccomp is judged on bubblewrap failing, because that IS its effect here -- it decides whether the
# namespace syscalls are permitted at all, and nothing else observable changes. So this one arm
# cannot discriminate on a host where bubblewrap is a known failure: if the HEALTHY container
# already fails that assertion, the mutation would report CAUGHT whether or not the flag was
# load-bearing. A green line asserting a flag is load-bearing on a host where nothing tested it is
# the worst version of the "guard that cannot fire" shape this directory keeps producing, so on
# such a host it is announced as skipped.
#
# BOTH ARMS USED TO SIT IN THAT SKIP, which was right while both ended at the bubblewrap assertion
# and wrong the moment the unmask arm stopped: on exactly the hosts the acceptance exists for, a
# newly added guard was discarded under a printed reason that no longer applied to it.
without 'systempaths=unconfined'
run "the /proc unmask is dropped (the masks come back)" "the declared /proc unmask is not in force" "${MUT[@]}"
if [ "${JKB_ACCEPT_NO_BWRAP:-0}" = 1 ]; then
    printf '  SKIPPED  stock seccomp (nested sandbox cannot start)\n'
    printf '           cannot discriminate while bubblewrap fails in the healthy container too;\n'
    printf '           it becomes meaningful again when that is fixed. The /proc unmask arm above\n'
    printf '           is unaffected: it is judged on a mountinfo count, not on bubblewrap.\n'
else
    without 'seccomp='
    run "stock seccomp (nested sandbox cannot start)" "bubblewrap cannot create namespaces or mount /proc" "${MUT[@]}"
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

# THE OTHER TWO DECLARATION READERS, watched (D54.2). Until this branch only `runArgs` had any
# evidence that its effect reached the running container; `remoteUser` and `containerEnv` were
# covered by a static guard over the harness's own copy of the declaration, which is the
# arrangement D54.1 deletes. These are what replace it, so they are watched failing like the rest.
#
# THE SAME SINGLE CHANGE, WATCHED BY THE OTHER GUARD IT TRIPS. `judge` takes one expect, and
# running as root fails two assertions: "not root" (above) and "not the user the declaration names"
# (2d). A second run of the identical mutation is how both are watched without either being
# asserted on a container broken in some other way.
#
# TRIED FIRST AS `--user 4242`, a non-root user that is not the declared one -- which reads better
# and is a WORSE MUTATION, because it changes more than one thing: that uid has no passwd entry, is
# not in sudoers, and cannot write /home/vscode, so it also breaks the firewall raise, the preamble
# and the memory linker. It reported MISSED, and whichever of those preempted the assertion, a
# mutation whose verdict depends on that is not evidence about the guard. Root changes one thing
# the harness already models.
without '^--user '
run "runs as a user the declaration does not name" "container.json declares remoteUser" \
    "${MUT[@]}" --user root

# A DECLARED ENVIRONMENT ENTRY, OVERRIDDEN. Appended rather than subtracted, because docker's rule
# is last-wins: this is the shape the escape hatch really takes (JKB_EGRESS_ACCEPT_UNFILTERED set
# at `docker run` beats containerEnv silently), and subtracting the flag would test a container
# nobody starts.
run "a declared environment entry is overridden at run time" "reached this container with different values" \
    "${HEALTHY[@]}" --env JKB_DB=/tmp/not-the-declared-path

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
if ! control_has_apparmor; then
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
