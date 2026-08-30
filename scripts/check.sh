#!/usr/bin/env bash
# One command that runs every gate we hold the codebase to. Run before every
# commit; CI runs the same thing.
set -euo pipefail

echo "==> rustfmt (check)"
cargo fmt --all -- --check

echo "==> clippy (warnings are errors)"
cargo clippy --all-targets --all-features -- -D warnings

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
echo "==> ui (typecheck + build)"
# pnpm lives under PNPM_HOME, which ~/.zshrc only exports for interactive shells — put it on
# PATH here, the same way the other scripts self-source ~/.cargo/env, so this works when run
# directly.
export PNPM_HOME="${PNPM_HOME:-$HOME/Library/pnpm}"
case ":$PATH:" in
    *":$PNPM_HOME/bin:"*) ;;
    *) export PATH="$PNPM_HOME/bin:$PATH" ;;
esac
if command -v pnpm >/dev/null 2>&1; then
    (cd "$(dirname "$0")/../ui" && pnpm run build)
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

# ...and verify.sh's exclusion list. The rest of verify.sh needs a container, but RUNTIME_OWNED is
# a regex, and it is the one part of the mount boundary that widens by a typo instead of by an
# edit somebody reviews — an over-broad exclusion drops a real mount from the set and the
# assertion still prints `ok`. mutate-verify.sh covers it and needs Docker; this costs nothing.
"$(dirname "$0")/../.container/verify.sh" --self-test
fi

# The host/container auto-memory link. Its slug rule is a guess about Claude Code's own private
# path encoding and its migration step is the only thing here that can lose a file, so both are
# exercised against a scratch HOME. No container, no Docker, no network — and deliberately OUTSIDE
# the jq group above: it reads no container.json, so a missing jq is no reason to skip it.
"$(dirname "$0")/link-claude-memory.sh" --self-test

echo "All checks passed."
