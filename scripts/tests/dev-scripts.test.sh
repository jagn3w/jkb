#!/usr/bin/env bash
# Every script here that invokes `cargo` must make the toolchain reachable first.
#
# The bug it exists for: scripts/check.sh — the gate CLAUDE.md tells you to run before every
# commit — called `cargo fmt` without sourcing `~/.cargo/env`, alone among the cargo wrappers.
# rustup does not install into a system PATH; it writes a line into the interactive shell rc
# files. So in ANY non-interactive shell — an agent's, a hook's, a `sh -c`, CI without a
# toolchain action — check.sh exited 127 at its first cargo line with `cargo: command not
# found`, and everything after the shell-syntax step never ran: rustfmt, clippy, the shell
# tests, `cargo test`, cargo-deny, the ui build.
#
# Measured cost: two review rounds were reported green over a `clippy -D warnings` that was
# already failing, because the gate never reached clippy to say so. That is the exact failure
# check.sh's own comments were written to prevent — "a header followed by nothing, then All
# checks passed, reads exactly like a gate that ran" — one step earlier than they were looking.
#
# So the rule is machine-checked rather than remembered at each of the eight sites, and case2
# pins the SYMPTOM rather than the spelling: a wrapper that finds its toolchain some other way
# is fine, one that cannot is not.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

new_workdir

# --- 1. no script calls cargo before it has arranged to have one ---------------------------
# Line-ordered, not merely "mentions it somewhere": a source line BELOW the first invocation
# reads as compliant and is not. `~` and `$HOME` are both spellings in use here.
case1() {
    local f base first_call first_src bad=""
    for f in "$repo_root"/scripts/*.sh; do
        [ -f "$f" ] || continue
        base="$(basename "$f")"
        first_call="$(grep -nE '^[[:space:]]*(\(cd [^)]*&&[[:space:]]*)?cargo[[:space:]]' "$f" \
            | head -1 | cut -d: -f1)"
        [ -n "$first_call" ] || continue
        # Anywhere on the line, not anchored at its start: `[ -f "$HOME/.cargo/env" ] &&
        # source "$HOME/.cargo/env"` is one of the spellings in use, and an anchored pattern
        # read it as "never sources". Comment lines are excluded so a note about the idiom
        # does not count as doing it.
        first_src="$(grep -nE '(\.|source)[[:space:]]+"?(~|\$HOME|\$\{HOME\})/\.cargo/env' "$f" \
            | grep -vE '^[0-9]+:[[:space:]]*#' | head -1 | cut -d: -f1)"
        if [ -z "$first_src" ]; then
            bad="$bad $base(never sources)"
        elif [ "$first_src" -gt "$first_call" ]; then
            bad="$bad $base(sources at $first_src, calls at $first_call)"
        fi
    done
    if [ -n "$bad" ]; then
        fail "toolchain: order" "script(s) call cargo without a toolchain first:$bad"
    else
        ok "every script that invokes cargo sources the toolchain before it does"
    fi
}

# --- 2. and check.sh actually gets past its first cargo line in a bare environment ---------
# The symptom, not the spelling. `env -i` with a PATH that has no cargo, and a HOME whose
# `.cargo/env` puts a STUB cargo on PATH: if check.sh sources it, the stub runs and check.sh
# dies on the stub's own exit status; if it does not, bash says `command not found` (127).
# The stub exits 1 at the first call, so nothing expensive runs and the shell-test loop below
# clippy is never reached — this suite does not re-enter itself.
case2() {
    local h="$work/home" out status
    mkdir -p "$h/.cargo" "$work/bin"
    printf '%s\n' '#!/bin/sh' 'echo "STUB-CARGO $*" >&2' 'exit 1' >"$work/bin/cargo"
    chmod +x "$work/bin/cargo"
    printf 'export PATH="%s:$PATH"\n' "$work/bin" >"$h/.cargo/env"
    out="$(env -i HOME="$h" PATH="/usr/bin:/bin" bash "$repo_root/scripts/check.sh" 2>&1)"
    status=$?
    case "$out" in
        *"cargo: command not found"*|*"cargo: No such file"*)
            fail "toolchain: check.sh" "check.sh cannot find cargo in a non-interactive shell (127)" ;;
        *"STUB-CARGO"*)
            ok "and check.sh reaches cargo itself when only ~/.cargo/env provides it" ;;
        *)
            fail "toolchain: premise" \
                 "check.sh neither found nor missed the stub (status=$status): $(printf '%s' "$out" | tr '\n' '|' | tail -c 200)" ;;
    esac
}

echo "==> scripts/*.sh toolchain availability"
run_cases case1 case2

finish
