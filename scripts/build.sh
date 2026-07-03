#!/usr/bin/env bash
# Build, passing through extra args (e.g. `./scripts/build.sh -p jkb-cli`).
# Defaults to the whole workspace when given no args.
set -euo pipefail

# shellcheck disable=SC1090
source ~/.cargo/env

if [ "$#" -eq 0 ]; then
    cargo build --all
else
    cargo build "$@"
fi
