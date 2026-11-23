#!/usr/bin/env bash
# scripts/lib.sh::pnpm_onto_path — the one place build-app.sh, check.sh and install-extension.sh
# find pnpm when their caller's PATH lacks it.
#
# The bug it exists for: pnpm's current standalone installer puts the binary in PNPM_HOME/bin, and
# build-app.sh put only PNPM_HOME itself on PATH, so Code Factory's *Update from main…* stopped at
# "pnpm not found" on a Mac where check.sh (which looked only in PNPM_HOME/bin) found it. Each case
# runs in a subshell with a scratch HOME and a PATH holding only the directories it names, so the
# real pnpm (CI has one on PATH) is never what a case finds.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

new_workdir

# stub_pnpm <dir> — an executable pnpm in <dir>.
stub_pnpm() { mkdir -p "$1" && printf '#!/bin/sh\n' >"$1/pnpm" && chmod +x "$1/pnpm"; }

# lookup <home> <path> [PNPM_HOME] — run pnpm_onto_path with HOME=<home>, PATH=<path> and, when
# given, PNPM_HOME; print its status, the pnpm it left on PATH, and the PNPM_HOME it left.
lookup() {
    (
        export HOME="$1" PATH="$2"
        if [ "$#" -ge 3 ]; then export PNPM_HOME="$3"; else unset PNPM_HOME; fi
        rc=0
        pnpm_onto_path || rc=$?
        printf '%s %s %s\n' "$rc" "$(command -v pnpm || echo none)" "${PNPM_HOME:-unset}"
    )
}

empty="$work/empty"
mkdir -p "$empty"
# Homebrew's prefixes are the lookup's last resort; a machine with pnpm there would make the
# "nowhere" case find it.
brew=""
for d in /opt/homebrew/bin /usr/local/bin; do [ ! -x "$d/pnpm" ] || brew="$d/pnpm"; done

# The regression: PNPM_HOME as pnpm's installer leaves it, the binary in bin/.
case1() {
    local h="$work/c1"; stub_pnpm "$h/pnpm/bin"
    local got; got="$(lookup "$h" "$empty" "$h/pnpm")"
    [ "$got" = "0 $h/pnpm/bin/pnpm $h/pnpm" ] && ok "PNPM_HOME/bin/pnpm is found" \
        || fail "bin layout" "$got"
}

# The older layout, the binary in PNPM_HOME itself.
case2() {
    local h="$work/c2"; stub_pnpm "$h/pnpm"
    local got; got="$(lookup "$h" "$empty" "$h/pnpm")"
    [ "$got" = "0 $h/pnpm/pnpm $h/pnpm" ] && ok "PNPM_HOME/pnpm is found" \
        || fail "flat layout" "$got"
}

# A PNPM_HOME the caller set wins over a pnpm already on PATH (app-install.test.sh's stub relies on it).
case3() {
    local h="$work/c3"; stub_pnpm "$h/pnpm/bin"; stub_pnpm "$h/other"
    local got; got="$(lookup "$h" "$h/other" "$h/pnpm")"
    [ "$got" = "0 $h/pnpm/bin/pnpm $h/pnpm" ] && ok "a set PNPM_HOME wins over PATH" \
        || fail "PNPM_HOME precedence" "$got"
}

# No PNPM_HOME: a pnpm on PATH is used as it is, and the default home is not consulted.
case4() {
    local h="$work/c4"; stub_pnpm "$h/other"; stub_pnpm "$h/Library/pnpm/bin"
    local got; got="$(lookup "$h" "$h/other")"
    [ "$got" = "0 $h/other/pnpm unset" ] && ok "with no PNPM_HOME, PATH's pnpm is used" \
        || fail "PATH" "$got"
}

# No PNPM_HOME and nothing on PATH — the GUI app's case: the default homes, bin layout first.
case5() {
    local h="$work/c5"; stub_pnpm "$h/Library/pnpm/bin"; stub_pnpm "$h/Library/pnpm"
    local got; got="$(lookup "$h" "$empty")"
    [ "$got" = "0 $h/Library/pnpm/bin/pnpm $h/Library/pnpm" ] && ok "~/Library/pnpm/bin is found, and PNPM_HOME set to its home" \
        || fail "default home" "$got"
    h="$work/c5b"; stub_pnpm "$h/.local/share/pnpm"
    got="$(lookup "$h" "$empty")"
    [ "$got" = "0 $h/.local/share/pnpm/pnpm $h/.local/share/pnpm" ] && ok "~/.local/share/pnpm is found" \
        || fail "XDG home" "$got"
}

# Nowhere at all: status 1, so each caller's own message names the problem.
case6() {
    if [ -n "$brew" ]; then skip "nowhere: this machine has $brew"; return; fi
    local h="$work/c6"; mkdir -p "$h"
    local got; got="$(lookup "$h" "$empty" "$h/missing")"
    [ "$got" = "1 none $h/missing" ] && ok "no pnpm anywhere returns 1" || fail "nowhere" "$got"
}

# Every caller goes through it: none keeps a lookup of its own.
case7() {
    local s missing=""
    for s in build-app.sh check.sh install-extension.sh; do
        grep -q '^[^#]*pnpm_onto_path' "$repo_root/scripts/$s" || missing="$missing $s"
    done
    local own; own="$(grep -lnE '^[^#]*\$\{?PNPM_HOME' "$repo_root"/scripts/{build-app,check,install-extension}.sh || true)"
    [ -z "$missing" ] && [ -z "$own" ] && ok "build-app, check and install-extension all use pnpm_onto_path, and none reads PNPM_HOME itself" \
        || fail "callers" "missing:${missing:- none}; own lookup: ${own:-none}"
}

echo "==> scripts/lib.sh::pnpm_onto_path"
run_cases case1 case2 case3 case4 case5 case6 case7

finish
