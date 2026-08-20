#!/usr/bin/env bash
# Notification hooks: make Claude Code's local notification STICKY, and clear it the moment
# you answer it.
#
# A macOS banner auto-hides after a few seconds, which is exactly backwards for "Claude needs
# your permission": the one notification you have to act on is the one most likely to be missed,
# and a session then sits blocked until you happen to look. Two halves, one script:
#
#   Notification                                       -> post, grouped by session
#   PostToolUse | UserPromptSubmit | Stop | SessionEnd  -> remove that session's group
#
# **The group key is derived from `session_id`, so dismissing owns nothing.** "Remove whatever
# this session posted" is a single call with no pid to track, no window handle and no cleanup
# pass — which is what a persistent alert window, the other way to get stickiness, would have
# needed. Parallel sessions (the D36 worktrees) each hold their own group and so cannot clear one
# another's notification, as a single global group or a shared pid file would. The one file this
# does keep is a marker, and it is an optimisation only: see `dismiss` for why a stale one costs
# nothing.
#
# **`PostToolUse` is the closest observable "permission was given".** `PreToolUse` runs *before*
# the permission prompt, so it cannot be the dismiss trigger; nothing fires at the moment you
# click Allow. The cost is one honest imprecision: granting a long-running command clears the
# notification when the command finishes, not when you granted it. `UserPromptSubmit` covers the
# idle-waiting notification and `Stop` sweeps a denial, which produces no `PostToolUse` at all.
#
# **Stickiness is a macOS setting, not something a notifier can force.** It needs
# terminal-notifier, whose alert style is set to "Alerts" once in System Settings; only then does
# a banner wait instead of hiding. Without it this hook still posts the ordinary auto-hiding
# banner through osascript, so the behaviour is never *worse* than before it existed — and
# `scripts/setup.sh` reports the missing piece rather than leaving a silently non-sticky hook.
#
# Fails OPEN and silent. Any failure here must not disturb the session, so it always exits 0 and
# writes nothing to stdout (a hook's stdout lands in the transcript).
#
# Env seams: `JKB_NOTIFIER` overrides the terminal-notifier path, `JKB_NOTIFY_STATE` the marker
# directory. `scripts/test-hooks.sh` drives both, which is what lets this be tested off macOS.

notifier=""
find_notifier() {
  if [ -n "${JKB_NOTIFIER:-}" ]; then
    [ -x "$JKB_NOTIFIER" ] && notifier="$JKB_NOTIFIER"
    return
  fi
  local c
  c=$(command -v terminal-notifier 2>/dev/null || true)
  # PATH first, then the places terminal-notifier actually installs itself. A hook inherits the
  # session's PATH, which for a GUI-launched terminal often omits /opt/homebrew/bin.
  for c in "$c" \
    /opt/homebrew/bin/terminal-notifier \
    /usr/local/bin/terminal-notifier \
    "$HOME/Applications/terminal-notifier.app/Contents/MacOS/terminal-notifier" \
    /Applications/terminal-notifier.app/Contents/MacOS/terminal-notifier; do
    if [ -n "$c" ] && [ -x "$c" ]; then
      notifier="$c"
      return
    fi
  done
}

# Quote a value as an AppleScript string literal. Line breaks are folded to spaces: AppleScript
# has no escape for a literal newline inside `"…"`, so one would end the statement mid-string —
# and a notification message is Claude's own text, which can carry any of these three characters.
as_applescript() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/ }
  s=${s//$'\r'/ }
  printf '"%s"' "$s"
}

# `scripts/setup.sh` asks THIS file where terminal-notifier is instead of keeping a second copy
# of the search list above: a copy that gained a location here and not there would report the
# notifier missing while the hook was happily using it. Answered before stdin is read, because a
# probe has none to give — `input=$(cat)` below would block on the terminal.
if [ "${1:-}" = "--find-notifier" ]; then
  find_notifier
  [ -n "$notifier" ] || exit 1
  printf '%s\n' "$notifier"
  exit 0
fi

show() {
  local message subtitle
  message=$(printf '%s' "$input" | jq -r '.message // ""' 2>/dev/null)
  [ -n "$message" ] || message="Claude Code needs you"
  # The working directory's leaf name, which under D36 is the session's worktree — the one thing
  # that tells two parallel sessions apart at a glance.
  subtitle=$(printf '%s' "$input" | jq -r '.cwd // ""' 2>/dev/null)
  subtitle=${subtitle##*/}

  # Written before posting, so the dismiss side never misses a notification that did go out.
  mkdir -p "$state_dir" 2>/dev/null && : > "$marker" 2>/dev/null

  find_notifier
  if [ -n "$notifier" ]; then
    # Same group as any earlier notification from this session, so a second one replaces the
    # first rather than stacking.
    "$notifier" -group "$group" -title "Claude Code" -subtitle "$subtitle" -message "$message" \
      >/dev/null 2>&1
    return
  fi

  command -v osascript >/dev/null 2>&1 || return
  osascript -e "display notification $(as_applescript "$message") with title \"Claude Code\" subtitle $(as_applescript "$subtitle")" \
    >/dev/null 2>&1
}

dismiss() {
  # `PostToolUse` runs after EVERY tool call, so the common case — nothing pending — must not
  # spawn a process. The marker is what makes that check a stat instead of an exec. A marker left
  # behind by a crashed session costs exactly one wasted `-remove` of a group that no longer
  # exists, so it needs no expiry.
  [ -f "$marker" ] || return
  rm -f "$marker" 2>/dev/null
  find_notifier
  [ -n "$notifier" ] || return
  "$notifier" -remove "$group" >/dev/null 2>&1
}

input=$(cat)

# One `jq` and no other subprocess before the marker test below: `PostToolUse` fires after EVERY
# tool call, so everything on the path to "nothing pending, exit" is a cost paid all day. Both
# fields are identifiers, so a tab join needs no quoting; the message and cwd are read separately,
# on the rare `Notification` path, precisely because they are free text.
IFS=$'\t' read -r event session <<<"$(
  printf '%s' "$input" | jq -r '[.hook_event_name // "", .session_id // ""] | @tsv' 2>/dev/null
)"

# The session id becomes both a notification group and a filename, so reduce it to characters
# that are inert in each — an id carrying `/` or `..` must not be able to name a path. An id we
# cannot use is not a reason to disturb the session, so it is a silent exit.
session=${session//[!A-Za-z0-9_-]/_}
[ -n "$session" ] || exit 0

group="jkb-claude-$session"
state_dir="${JKB_NOTIFY_STATE:-${TMPDIR:-/tmp}/jkb-claude-notify}"
marker="$state_dir/$session"

case "$event" in
  Notification) show ;;
  PostToolUse | UserPromptSubmit | Stop | SessionEnd) dismiss ;;
esac

exit 0
