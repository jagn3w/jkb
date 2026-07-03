#!/usr/bin/env bash
# Run clippy (warnings-as-errors) across all targets/features. Pass through extra
# args, e.g. `./scripts/clippy.sh -p jkb-sync`. Separate from check.sh so a single
# crate can be linted quickly during development.
set -euo pipefail

# shellcheck disable=SC1090
source ~/.cargo/env

cargo clippy --all-targets --all-features "$@" -- -D warnings
