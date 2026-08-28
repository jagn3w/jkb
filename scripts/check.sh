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
# gate; the parts that need a container are .devcontainer/verify.sh and mutate-verify.sh. It
# mainly guards the GENERATED seccomp profile, whose patch silently no-opping against a changed
# upstream yields a profile that parses, applies, and leaves the nested sandbox unable to start.
echo "==> devcontainer config"
"$(dirname "$0")/../.devcontainer/check-config.sh"

# ...and every assertion in it, watched failing. check-config.sh had no such harness while
# verify.sh did, and three review rounds each found the same defect in it — an assertion that
# cannot fail. Needs no Docker either, so it belongs in the gate rather than beside mutate-verify.
"$(dirname "$0")/../.devcontainer/mutate-config.sh"

# The host/container auto-memory link. Its slug rule is a guess about Claude Code's own private
# path encoding and its migration step is the only thing here that can lose a file, so both are
# exercised against a scratch HOME. No container, no Docker, no network.
"$(dirname "$0")/link-claude-memory.sh" --self-test

# ...and the workspace preflight. It is the one guard between the widened container mount and a
# silent wrong-checkout open, and until now it was certified as wired (check-config.sh reads the
# JSON) but never as working.
"$(dirname "$0")/../.devcontainer/check-workspace.sh" --self-test

echo "All checks passed."
