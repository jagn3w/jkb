#!/usr/bin/env bash
# Format the tree, then run every gate (scripts/check.sh). One command so it can be
# allowlisted without chaining. Use during development; commit only when it's green.
set -euo pipefail

# shellcheck disable=SC1090
source ~/.cargo/env

echo "==> rustfmt (write)"
cargo fmt --all

exec "$(dirname "$0")/check.sh"
