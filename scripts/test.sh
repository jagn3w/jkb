#!/usr/bin/env bash
# Run the test suite, passing through extra args (e.g. `./scripts/test.sh -p jkb-sync`).
# Defaults to the whole workspace when given no args.
set -euo pipefail

# shellcheck disable=SC1090
source ~/.cargo/env

if [ "$#" -eq 0 ]; then
    cargo test --all
else
    cargo test "$@"
fi
