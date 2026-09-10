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
# So the rule is machine-checked rather than remembered at each site (seven today), and case2
# pins the SYMPTOM rather than the spelling: a wrapper that finds its toolchain some other way
# is fine, one that cannot is not.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

new_workdir

# --- 1. no script calls cargo before it has arranged to have one ---------------------------
# Line-ordered, not merely "mentions it somewhere": a source line BELOW the first invocation
# reads as compliant and is not.
#
# `cargo` is found at a COMMAND POSITION, not anywhere on the line. The first version keyed on
# `^[[:space:]]*(\(cd [^)]*&&[[:space:]]*)?cargo[[:space:]]`, and a script whose spelling it
# could not parse fell out through `[ -n "$first_call" ] || continue` — SILENTLY EXEMPTED. Four
# forms already in this repository or its container missed it: `( cd "$r" && cargo … )` with a
# space after the paren, `RUSTFLAGS=… cargo build`, `exec cargo test`, and `if cargo build; then`.
# The guard was therefore off for exactly the author who did not already know the house idiom,
# which is the failure it exists to catch. It also failed in the other direction, matching
# `cargo` inside an `echo` string.
#
# So the line is split at command separators and each fragment is asked whether it BEGINS with
# cargo, after leading keywords and environment assignments. case0 drives that detector against
# every spelling above, both the six it must find and three it must not — because a detector
# nothing tests is the same silent exemption one level up.
_first_cargo_line() {
    # The env-assignment pattern is built at run time so it can name both quote characters
    # without fighting the shell over the single one. A QUOTED value with a space in it —
    # `RUSTFLAGS="-C link-arg=-s" cargo build`, which is how anyone sets more than one flag —
    # is not `[^[:space:]]*`, so the unquoted-only version stopped stripping there and the
    # line fell out as "mentions cargo, invokes nothing": silently exempt, the exact failure
    # case0 exists for. `env` joins the leading keywords for the same reason.
    awk '
        BEGIN {
            q = sprintf("%c", 39)
            envre = "^[A-Za-z_][A-Za-z0-9_]*=(\"[^\"]*\"|" q "[^" q "]*" q "|[^[:space:]]*)[[:space:]]+"
        }
        /^[[:space:]]*#/ { next }
        {
            line = $0
            gsub(/[;&|()]/, "\n", line)
            n = split(line, parts, "\n")
            for (i = 1; i <= n; i++) {
                p = parts[i]
                sub(/^[[:space:]]+/, "", p)
                while (p ~ /^(if|then|do|else|elif|exec|time|env)[[:space:]]/) {
                    sub(/^[A-Za-z]+[[:space:]]+/, "", p)
                }
                while (p ~ envre) { sub(envre, "", p) }
                if (p ~ /^cargo([[:space:]]|$)/) { print NR; exit }
            }
        }' "$1"
}

# The toolchain may be arranged ANY way that works — this suite's header says so, and case2
# pins the symptom rather than the spelling. Sourcing `~/.cargo/env` is the house idiom; a
# `PATH` export naming `.cargo/bin` or `$CARGO_HOME`, or `rustup run`, is equally fine. An
# earlier version accepted only the first and would have hard-failed the others.
_first_toolchain_line() {
    grep -nE '(\.|source)[[:space:]]+"?(~|\$HOME|\$\{HOME\})/\.cargo/env|PATH=[^#]*(\.cargo/bin|CARGO_HOME)|rustup[[:space:]]+run' "$1" \
        | grep -vE '^[0-9]+:[[:space:]]*#' | head -1 | cut -d: -f1
}

# --- 0. the detector itself, against every spelling it has to be right about ----------------
case0() {
    local probe="$work/forms.sh" n hit want_hit want_miss
    cat >"$probe" <<'FORMS'
( cd "$repo" && cargo install --path x )
RUSTFLAGS=-x cargo build
RUSTFLAGS="-C link-arg=-s" cargo build
env CARGO_TERM_COLOR=always cargo test
exec cargo test
if cargo build; then
(cd "$r" && cargo fmt)
    cargo clippy
echo "install it with: cargo install --path crates/jkb-cli"
if ! command -v cargo >/dev/null 2>&1; then
echo "(build the crate first so cargo extracts its source)" >&2
FORMS
    # Lines 1-8 are invocations; 9-11 mention cargo and invoke nothing. Asked one line at a
    # time, because `_first_cargo_line` stops at the first hit and would otherwise report only
    # line 1 whatever the other ten do.
    want_hit=""; want_miss=""
    n=0
    while IFS= read -r line; do
        n=$((n + 1))
        printf '%s\n' "$line" >"$work/one.sh"
        hit="$(_first_cargo_line "$work/one.sh")"
        if [ "$n" -le 8 ]; then
            [ -n "$hit" ] || want_hit="$want_hit $n"
        else
            [ -z "$hit" ] || want_miss="$want_miss $n"
        fi
    done <"$probe"
    [ "$n" -eq 11 ] || fail "toolchain: forms-premise" "read $n form(s), expected 11"
    if [ -n "$want_hit" ]; then
        fail "toolchain: forms-miss" "these real invocation spellings are not seen as cargo \
calls, so a script using one is silently exempted:$want_hit"
    elif [ -n "$want_miss" ]; then
        fail "toolchain: forms-false" "these lines invoke nothing and were read as calls:$want_miss"
    else
        ok "the cargo detector sees every invocation spelling in use, and no mere mention"
    fi
}

case1() {
    local f base first_call first_src bad="" seen=0
    for f in "$repo_root"/scripts/*.sh; do
        [ -f "$f" ] || continue
        base="$(basename "$f")"
        first_call="$(_first_cargo_line "$f")"
        [ -n "$first_call" ] || continue
        seen=$((seen + 1))
        first_src="$(_first_toolchain_line "$f")"
        if [ -z "$first_src" ]; then
            bad="$bad $base(never arranges one)"
        elif [ "$first_src" -gt "$first_call" ]; then
            bad="$bad $base(arranges at $first_src, calls at $first_call)"
        fi
    done
    # A FLOOR, because a loop that inspected nothing prints the same "ok" as one that inspected
    # everything — the shape this suite is here to remove. Seven scripts call cargo today; the
    # floor is lower so ordinary churn does not trip it, and a broken glob or detector does.
    if [ "$seen" -lt 5 ]; then
        fail "toolchain: coverage" "only $seen script(s) were inspected, so this case asserts \
almost nothing — the glob or the detector has regressed"
    elif [ -n "$bad" ]; then
        fail "toolchain: order" "script(s) call cargo without a toolchain first:$bad"
    else
        ok "every script that invokes cargo arranges a toolchain before it does ($seen inspected)"
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
run_cases case0 case1 case2

finish
