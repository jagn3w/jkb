#!/usr/bin/env bash
# One command that runs every gate we hold the codebase to. Run before every
# commit; CI runs the same thing.
set -euo pipefail

# Parse every shell file in the repo before running anything. NOTHING executes setup.sh, so a
# control-flow edit there reached the gate unchecked — and setup.sh is what the post-merge hook
# runs unattended after a pull. The same is true of the PreToolUse hooks, and there it is
# sharper: bash exits 2 on a syntax error, and a PreToolUse hook exiting 2 reads as DENY, so a
# broken hook does not fail open the way its header promises — it blocks every Bash tool call
# for everyone after a pull. First, because it needs no toolchain and takes a second.
#
# The list and the loop both live in lib.sh, so CI runs this exact code rather than a second
# hand-written copy of it — the two copies had already drifted by a `*.md` skip.
echo "==> shell syntax"
# shellcheck source=scripts/lib.sh
. "$(dirname "$0")/lib.sh"
check_shell_syntax "$(cd "$(dirname "$0")/.." && pwd)"

# The toolchain, the same way every other cargo wrapper here gets it. THIS SCRIPT DID NOT, and
# it is the one CLAUDE.md names as the gate to run before every commit: in a non-interactive
# shell — an agent's, a hook's, a `sh -c` — `cargo` is not on PATH, so line 21 exited 127 with
# `cargo: command not found` and NOTHING after the shell-syntax step ran. Not rustfmt, not
# clippy, not the shell tests, not `cargo test`, not cargo-deny, not the ui build. Measured: two
# review rounds were reported green over a `clippy -D warnings` that was already failing, which
# is precisely the "a header followed by nothing reads exactly like a gate that ran" failure
# this file's own comments were written to prevent — one step earlier than they were looking.
#
# Sourced only when present, because CI images install rust system-wide and this must not become
# a hard dependency. Written as an `if` rather than `[ -f … ] && source …`: measured, the `&&`
# form is safe under `set -e` here (a non-final command in an `&&` list is exempt), but it is
# safe by a rule you have to know and stops being safe the moment it is the last line of a
# block. The `if` needs no such argument.
if [ -f ~/.cargo/env ]; then
    # shellcheck disable=SC1090
    source ~/.cargo/env
fi

echo "==> rustfmt (check)"
cargo fmt --all -- --check

echo "==> clippy (warnings are errors)"
cargo clippy --all-targets --all-features -- -D warnings

# The shell under scripts/ is part of the codebase too, and setup.sh's installs are not
# reachable from a Rust test. Each *.test.sh is self-contained and runs in a temp dir.
echo "==> shell tests (scripts/tests)"
ran=0
for t in "$(dirname "$0")"/tests/*.test.sh; do
    [ -e "$t" ] || break
    bash "$t"
    ran=$((ran + 1))
done
# Say it, like the cargo-deny and pnpm branches below. A header followed by nothing, then
# "All checks passed", reads exactly like a gate that ran — which is what this file exists
# to stop. Unlike those two, nothing here is optional: CI has no guard and fails instead.
[ "$ran" -gt 0 ] || echo "   (skipped: no scripts/tests/*.test.sh found — CI treats this as a failure)"

echo "==> tests"
cargo test --all

echo "==> cargo-deny (advisories, licenses, sources)"
if command -v cargo-deny >/dev/null 2>&1; then
    # --all-features so feature-gated deps (e.g. the `fastembed` graph) are audited too;
    # this matches CI (the cargo-deny action defaults to --all-features). Without it a
    # vuln behind an optional feature is invisible locally.
    cargo deny --all-features check
else
    echo "   (skipped: cargo-deny not installed — 'cargo install cargo-deny')"
fi

# The `ui/` pnpm workspace is part of the codebase, so it is part of the gate. Each package's
# `build` script type-checks before emitting (the VS Code adapter bundles with esbuild, which
# strips types WITHOUT checking them — so `esbuild` alone would happily ship a type error).
# `pnpm -r` runs topologically, which also guarantees @jkb/core emits its .d.ts before the
# adapter type-checks against it.
echo "==> ui (typecheck + build + tests)"
# pnpm lives under PNPM_HOME, which ~/.zshrc only exports for interactive shells — put it on
# PATH here, the same way the other scripts self-source ~/.cargo/env, so this works when run
# directly.
export PNPM_HOME="${PNPM_HOME:-$HOME/Library/pnpm}"
case ":$PATH:" in
    *":$PNPM_HOME/bin:"*) ;;
    *) export PATH="$PNPM_HOME/bin:$PATH" ;;
esac
if command -v pnpm >/dev/null 2>&1; then
    # `test` after `build`: the tests bundle their own module, so they do not need dist — but
    # a type error is the cheaper failure to read, so it is the one reported first.
    (cd "$(dirname "$0")/../ui" && pnpm run build && pnpm run test)
else
    echo "   (skipped: pnpm not found — install it, or set PNPM_HOME; CI runs this gate)"
fi

# The auto-mode posture (design D48) is committed data that a script consumes, so it is part of
# the gate too: the tests are hermetic (a temp CLAUDE_CONFIG_DIR, no session, no network) and
# they generate their drift cases FROM the posture file, so a key added there is covered without
# anyone remembering to add a case. Skipped gracefully when jq is absent; CI always runs it.
echo "==> auto-mode posture (scripts/auto-mode.sh)"
if command -v jq >/dev/null 2>&1; then
    "$(dirname "$0")/auto-mode-test.sh"
else
    echo "   (skipped: jq not installed — 'brew install jq'; CI runs this gate)"
fi

# The dev container's configuration (design D49). Static only — no Docker — so it belongs in the
# gate; the parts that need a container are .container/verify.sh and mutate-verify.sh. It
# mainly guards the GENERATED seccomp profile, whose patch silently no-opping against a changed
# upstream yields a profile that parses, applies, and leaves the nested sandbox unable to start.
#
# ONE jq GUARD FOR THE WHOLE GROUP, not one per script. Every check below reads container.json
# through lib.sh's `dc_*` helpers, so they stand or fall together — and they did not: check-config.sh
# announced its own skip while run.sh --self-test went on to die red on the same missing tool, so a
# fact about the machine read as a broken container derivation. A skip decided per-assertion is not
# a skip; it is three different answers to one question.
echo "==> container config"
if ! command -v jq >/dev/null 2>&1; then
    echo "   (skipped: jq not installed — 'brew install jq'; CI runs these gates)"
else
"$(dirname "$0")/../.container/check-config.sh"

# ...and every assertion in it, watched failing. check-config.sh had no such harness while
# verify.sh did, and three review rounds each found the same defect in it — an assertion that
# cannot fail. Needs no Docker either, so it belongs in the gate rather than beside mutate-verify.
"$(dirname "$0")/../.container/mutate-config.sh"

# ...and the container's argument derivation. run.sh is the ONLY thing that applies
# container.json now that VS Code does not read it, so a mistake in the derivation is a container
# built to a different specification than the one every other check reads. The derivation is pure,
# so it is exercised here; the parts needing Docker are verify.sh and mutate-verify.sh.
"$(dirname "$0")/../.container/run.sh" --self-test

# ...and the marketplace URL derivation. It was the one --self-test in .container/ that no caller
# ran: the publisher/name split, the arm64/amd64 platform mapping and the refusal of an unknown
# architecture were exercised by nothing, so breaking any of them kept the gate green and surfaced
# as a 404 in somebody's `docker build`.
"$(dirname "$0")/../.container/fetch-extensions.sh" --self-test

# ...and what the container does when the firewall raise fails. It has three outcomes and getting
# the two failing ones the wrong way round yields either a container that will not boot or one
# running unprotected — decided by the first thing that executes in there, which nothing had run.
# Exercised against a stubbed sudo, so it needs no Docker and no privileges.
"$(dirname "$0")/../.container/entrypoint.sh" --self-test

# ...and the other half of that decision: what the raise RECORDS. The reader above had fourteen
# assertions and its writer had none, which is the asymmetry that matters least in the direction
# it was — a verdict is only as good as the measurement behind it, and "both families provably
# closed" was spelled twice, in two shapes, at the two sites that decide it. Pure and path-injected
# throughout: no iptables, no root, no /proc.
"$(dirname "$0")/../.container/init-firewall.sh" --self-test

# ...and the two halves D51 split that decision into. The library is what "egress is bounded" MEANS
# — one definition, sourced by the raise and by the probe, because spelling it twice inside one
# script is how the success path came to report success on unfiltered IPv6. The probe is what the
# entrypoint now boots on: it reads the live chains, so unlike the record it was reading before, it
# cannot describe a network that no longer exists.
"$(dirname "$0")/../.container/egress-lib.sh" --self-test
"$(dirname "$0")/../.container/egress-status.sh" --self-test

# ...and verify.sh's exclusion list. The rest of verify.sh needs a container, but RUNTIME_OWNED is
# a regex, and it is the one part of the mount boundary that widens by a typo instead of by an
# edit somebody reviews — an over-broad exclusion drops a real mount from the set and the
# assertion still prints `ok`. mutate-verify.sh covers it and needs Docker; this costs nothing.
"$(dirname "$0")/../.container/verify.sh" --self-test

# ...and the bubblewrap probe's SHAPE, which is pure and is the one thing about that probe a host
# with no Docker can establish. The measurement needs a container; that the two rungs differ by the
# proc mount and by nothing else does not — and a probe weaker than the mechanism it names is the
# defect this file's subject has shipped twice, passing `ok` in exactly the broken state both
# times.
"$(dirname "$0")/../.container/bwrap-probe.sh" --self-test
fi

# ...and the drift check's decision, which is pure. The check ITSELF needs the network — it fetches
# each generator's upstream and regenerates — so it runs in CI, not here: a gate that only works
# online is one that gets skipped. What runs here is the part that decides whether a difference is
# upstream drift, a hand-edit, or unattributable, plus the digest extractor against both artifact
# formats it has to read. Deliberately outside the jq group: it reads no container.json.
"$(dirname "$0")/../.container/check-drift.sh" --self-test

# ...and the AppArmor generator's own refusals, driven with fixture templates instead of the
# network. These are what stop a patch silently no-opping against changed upstream -- the exact
# failure check-config.sh already names for the seccomp profile -- and until this existed NOTHING
# exercised them: check-drift.sh compares our output against our own output, so a bug in the
# render/patch logic is invisible to it, and what it produces is a security policy.
# GUARDED, because it drives its fixtures through python3 and this file's own rule is that a fact
# about the machine degrades to a NAMED skip rather than a red gate. CI always has python3, so a
# local skip never becomes a CI skip. (check-drift.sh's --self-test above is pure shell.)
if command -v python3 >/dev/null 2>&1; then
    "$(dirname "$0")/../.container/generate-apparmor.sh" --self-test
else
    echo "   (skipped: python3 not installed; CI runs this gate)"
fi

# The host/container auto-memory link. Its slug rule is a guess about Claude Code's own private
# path encoding and its migration step is the only thing here that can lose a file, so both are
# exercised against a scratch HOME. No container, no Docker, no network — and deliberately OUTSIDE
# the jq group above: it reads no container.json, so a missing jq is no reason to skip it.
"$(dirname "$0")/link-claude-memory.sh" --self-test

echo "All checks passed."
