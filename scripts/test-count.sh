#!/usr/bin/env bash
# Run the whole test suite and print the per-binary counts plus a grand total of
# non-doc tests (handy for keeping the count in CLAUDE.md accurate).
set -euo pipefail

# shellcheck disable=SC1090
source ~/.cargo/env

cargo test --all 2>&1 \
    | grep -E "Running|test result: ok" \
    | awk '
        /Running/ { name=$0 }
        /test result: ok/ {
            n=$4; total+=n;
            if (n+0 > 0) print n, "\t", name;
        }
        END { print "----"; print total, "\ttotal non-doc tests" }'
