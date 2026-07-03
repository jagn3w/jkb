#!/usr/bin/env bash
# PreToolUse (Bash) hook: block raw `cargo build|test|clippy|fmt|check` and steer to the
# repo's ./scripts/*.sh wrappers (they self-source ~/.cargo/env, pin the toolchain, and
# gate lints). Fails OPEN — any error here allows the command rather than wedging Bash.
#
# Blocks:   cargo test, cargo +1.96.1 clippy, FOO=1 cargo build, source ~/.cargo/env && cargo check
# Allows:   ./scripts/test.sh …, cargo install, cargo run, cargo add, cargo tree, cargo deny

input=$(cat)
cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // ""' 2>/dev/null)

# Anything already routed through the repo scripts is fine.
case "$cmd" in
  *"./scripts/"* | *"/scripts/"*) exit 0 ;;
esac

# Match `cargo [+toolchain] <build|test|clippy|fmt|check>` only when `cargo` sits at a
# COMMAND position: the line start or just after a shell separator (; & | ( { newline),
# optionally preceded by env-var assignments (FOO=bar cargo …). This keeps `cargo test`
# inside a quoted string (e.g. a commit message) from tripping the guard. `source … &&`
# prefixes match because cargo then follows `&&`. Other subcommands (install/run/add/
# tree/deny/…) don't match and pass through.
if printf '%s' "$cmd" \
  | grep -qE '(^|[;&|(){}])[[:space:]]*([A-Za-z_][A-Za-z0-9_]*=[^[:space:]]*[[:space:]]+)*cargo[[:space:]]+(\+[^[:space:]]+[[:space:]]+)?(build|test|clippy|fmt|check)([[:space:]]|$)'; then
  cat <<'JSON'
{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"Don't run cargo directly in this repo — use the ./scripts wrappers (they self-source ~/.cargo/env, pin the toolchain, and gate lints): ./scripts/build.sh, ./scripts/test.sh, ./scripts/clippy.sh, ./scripts/fix.sh (fmt+check), ./scripts/test-count.sh. They pass args through, e.g. ./scripts/test.sh -p jkb-core needs_review. (cargo install/run/add/tree/deny are still fine.)"}}
JSON
fi
exit 0
