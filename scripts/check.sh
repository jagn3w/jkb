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

echo "All checks passed."
