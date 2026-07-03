#!/usr/bin/env bash
# Read-only inspection of a dependency's extracted registry source, so we can learn
# a crate's exact API for the resolved version instead of guessing.
# Usage: ./scripts/inspect-dep.sh <crate-dir-name> <grep-args...>
#   e.g. ./scripts/inspect-dep.sh rmcp-2.0.0 -n "pub fn enable_tools"
set -euo pipefail

crate="${1:?usage: inspect-dep.sh <crate-dir-name> <grep-args...>}"
shift

base=$(find "$HOME/.cargo/registry/src" -maxdepth 2 -type d -name "$crate" | head -1)
if [ -z "$base" ]; then
    echo "not found under ~/.cargo/registry/src: $crate" >&2
    echo "(build the crate first so cargo extracts its source)" >&2
    exit 1
fi

grep -rn "$@" "$base/src"
