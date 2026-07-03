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
    cargo deny check
else
    echo "   (skipped: cargo-deny not installed — 'cargo install cargo-deny')"
fi

echo "All checks passed."
