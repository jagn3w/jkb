#!/usr/bin/env bash
# PreToolUse (Bash) hook: block raw `sqlite3` against a jkb database and steer to the
# `jkb` CLI (which routes every write through the audited writer-actor + changelog +
# undo). The CLI now covers every read/write an agent needs (design D27.3), so reaching
# into raw SQL is both unnecessary and unsafe. Fails OPEN — any error here allows the
# command rather than wedging Bash.
#
# Blocks:   sqlite3 ~/.jkb/jkb.db "…", JKB_DB=x sqlite3 "$JKB_DB", sqlite3 ./jkb.db
# Allows:   sqlite3 /tmp/other.db …, jkb task …, `sqlite3` inside a quoted commit message

input=$(cat)
cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // ""' 2>/dev/null)

# Match `sqlite3` only when it sits at a COMMAND position: the line start or just after a
# shell separator (; & | ( { newline), optionally preceded by env-var assignments
# (FOO=bar sqlite3 …). This keeps the string `sqlite3` inside a quoted argument (e.g. a
# commit message) from tripping the guard.
if printf '%s' "$cmd" \
  | grep -qE '(^|[;&|(){}])[[:space:]]*([A-Za-z_][A-Za-z0-9_]*=[^[:space:]]*[[:space:]]+)*sqlite3([[:space:]]|$)'; then
  # …and only deny when it targets a jkb database: the default `jkb.db`, anything under
  # `~/.jkb/`, or the `$JKB_DB` env var. `sqlite3` on an unrelated database passes through.
  if printf '%s' "$cmd" | grep -qE 'jkb\.db|\.jkb/|\$\{?JKB_DB'; then
    cat <<'JSON'
{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"Don't run sqlite3 against a jkb database — use the jkb CLI (it routes every write through the audited writer-actor with changelog + undo). Reads: jkb task show|next, jkb query, jkb search, jkb ns/tag ls. Writes: jkb task set|tag|depend|undepend|place|bind|claim|release, jkb undo, jkb doctor --fix. See --help for each. (sqlite3 against a non-jkb database is still fine.)"}}
JSON
  fi
fi
exit 0
