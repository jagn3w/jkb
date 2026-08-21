#!/usr/bin/env bash
# Notification hooks: make Claude Code's local notification STICKY, and clear it the moment
# you answer it.
#
# A macOS banner auto-hides after a few seconds, which is exactly backwards for "Claude needs
# your permission": the one notification you have to act on is the one most likely to be missed,
# and a session then sits blocked until you happen to look. Two halves, one script:
#
#   Notification                                       -> post under this session's id
#   PostToolUse | UserPromptSubmit | Stop | SessionEnd  -> withdraw that id
#
# **The notification id is derived from `session_id`, so dismissing owns nothing.** "Withdraw
# whatever this session posted" is a single call with no pid to track, no window handle and no
# cleanup pass — which is what a persistent alert window, the other way to get stickiness, would
# have needed. Parallel sessions (the D36 worktrees) each hold their own id and so cannot clear
# one another's notification, as a single global id or a shared pid file would. The one file this
# does keep is a marker, and it is an optimisation only: see `dismiss` for why a stale one costs
# nothing.
#
# **`PostToolUse` is the closest observable "permission was given".** `PreToolUse` runs *before*
# the permission prompt, so it cannot be the dismiss trigger; nothing fires at the moment you
# click Allow. The cost is one honest imprecision: granting a long-running command clears the
# notification when the command finishes, not when you granted it. `UserPromptSubmit` covers the
# idle-waiting notification and `Stop` sweeps a denial, which produces no `PostToolUse` at all.
#
# **The notifier is ours** — `macos/notifier/`, built by `scripts/build-notifier.sh` into
# `jkb Notifier.app`. Withdrawing a delivered notification is the entire feature and only Apple's
# current `UserNotifications` framework offers it: `osascript` cannot take back what it posted,
# and terminal-notifier can but was last released in 2017 on an API deprecated since macOS 11.
#
# **Stickiness itself is a per-app macOS setting no API can set.** `alert` waits for the user,
# `banner` hides itself; the user chooses in System Settings. What our notifier adds is that it
# can *read* that back (`jkb-notifier status`), so `scripts/setup.sh` reports a hook that is
# posting self-hiding banners instead of leaving it to look like a broken hook.
#
# Until the bundle is installed and authorized, `post` REFUSES and the hook falls back to the
# plain osascript banner — never worse than before this existed, and never a silent success for a
# notification the user cannot see.
#
# Fails OPEN and silent. Any failure here must not disturb the session, so it always exits 0 and
# writes nothing to stdout (a hook's stdout lands in the transcript).
#
# Env seams: `JKB_NOTIFIER` overrides the notifier binary, `JKB_NOTIFY_STATE` the marker
# directory. `scripts/test-hooks.sh` drives both, which is what lets this be tested off macOS.

# Where the notifier lives, spelled ONCE for the whole repo. `scripts/build-notifier.sh` installs
# to `--notifier-path` (the first entry) rather than composing a path of its own: the two used to
# be written out independently, and renaming the bundle in either one would have left the hook
# quietly falling back to plain banners forever — the failure that looks identical to this feature
# not existing. A per-user location first, because installing there needs no privileges.
NOTIFIER_PATHS=(
  "$HOME/Applications/jkb Notifier.app/Contents/MacOS/jkb-notifier"
  "/Applications/jkb Notifier.app/Contents/MacOS/jkb-notifier"
)

# The events this hook acts on, and the ONLY place they are enumerated. They also have to be
# registered in `.claude/settings.json`, and that is a second file this one cannot see — so
# `--events` exists for `scripts/test-hooks.sh` to diff the two. Losing a registration is silent
# in the worst way: drop `Stop` and a *denied* permission, which produces no `PostToolUse`, leaves
# its notification on screen until the session ends — which is the case `Stop` is here for.
SHOW_EVENTS=(Notification)
DISMISS_EVENTS=(PostToolUse UserPromptSubmit Stop SessionEnd)

in_list() {
  local needle=$1 x
  shift
  for x in "$@"; do
    [ "$x" = "$needle" ] && return 0
  done
  return 1
}

notifier=""
find_notifier() {
  if [ -n "${JKB_NOTIFIER:-}" ]; then
    [ -x "$JKB_NOTIFIER" ] && notifier="$JKB_NOTIFIER"
    return
  fi
  # The binary is invoked DIRECTLY rather than through `open`: `open` is asynchronous, so a hook
  # could not read its exit status to decide whether to fall back, and it is far too slow for a
  # per-tool-call dismiss. Launch Services registration is what makes the direct path work.
  local c
  for c in "${NOTIFIER_PATHS[@]}"; do
    if [ -x "$c" ]; then
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

# `scripts/setup.sh` asks THIS file where the notifier is instead of keeping a second copy of the
# search list above: a copy that gained a location here and not there would report the notifier
# missing while the hook was happily using it. Answered before stdin is read, because a probe has
# none to give — `input=$(cat)` below would block on the terminal.
if [ "${1:-}" = "--find-notifier" ]; then
  find_notifier
  [ -n "$notifier" ] || exit 1
  printf '%s\n' "$notifier"
  exit 0
fi

# Where a notifier SHOULD be installed, whether or not one is there yet — which is why this is a
# separate question from `--find-notifier`, whose answer is "nowhere" on the machine that most
# needs to install one. `scripts/build-notifier.sh` builds to exactly this.
if [ "${1:-}" = "--notifier-path" ]; then
  printf '%s\n' "${NOTIFIER_PATHS[0]}"
  exit 0
fi

# Every event this hook acts on, for the settings.json cross-check in scripts/test-hooks.sh.
if [ "${1:-}" = "--events" ]; then
  printf '%s\n' "${SHOW_EVENTS[@]}" "${DISMISS_EVENTS[@]}"
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

  # Same id as any earlier notification from this session, so a second one replaces the first
  # rather than stacking. A non-zero exit means the bundle is not installed or not yet authorized
  # — it refuses instead of posting something invisible, precisely so this can fall through to a
  # banner the user will actually see.
  find_notifier
  if [ -n "$notifier" ] &&
    "$notifier" post --id "$notif_id" --title "Claude Code" --subtitle "$subtitle" \
      --body "$message" >/dev/null 2>&1; then
    # The marker means "there is a notification of ours on screen that CAN be withdrawn", so it is
    # written only once posting actually succeeded — and only on this branch. An osascript banner
    # cannot be withdrawn, so marking one would buy `dismiss` nothing and cost every later tool
    # call a look at a marker it can never act on.
    mkdir -p "$state_dir" 2>/dev/null && : > "$marker" 2>/dev/null
    return
  fi

  command -v osascript >/dev/null 2>&1 || return
  osascript -e "display notification $(as_applescript "$message") with title \"Claude Code\" subtitle $(as_applescript "$subtitle")" \
    >/dev/null 2>&1
}

dismiss() {
  # `PostToolUse` runs after EVERY tool call, so the common case — nothing pending — must not
  # spawn a process. The marker is what makes that check a stat instead of an exec. A marker left
  # behind by a crashed session costs exactly one wasted `remove` of an id that is no longer on
  # screen, so it needs no expiry.
  [ -f "$marker" ] || return
  find_notifier
  # The marker is CONSUMED ONLY BY A WITHDRAW THAT WORKED. It used to be cleared first, on the
  # reasoning that it is "just an optimisation" — but its absence is authoritative for every later
  # dismiss, so one failed `remove` (bundle mid-reinstall, notifier timing out) silently disarmed
  # `Stop` and `SessionEnd` too, and the alert stayed up for the rest of the session. Leaving it
  # costs two path tests per tool call and buys a retry on every subsequent dismiss event.
  [ -n "$notifier" ] || return
  if "$notifier" remove --id "$notif_id" >/dev/null 2>&1; then
    rm -f "$marker" 2>/dev/null
  fi
}

input=$(cat)

# One `jq` and no other subprocess before the marker test below: `PostToolUse` fires after EVERY
# tool call, so everything on the path to "nothing pending, exit" is a cost paid all day. Both
# fields are identifiers, so a tab join needs no quoting; the message and cwd are read separately,
# on the rare `Notification` path, precisely because they are free text.
IFS=$'\t' read -r event session <<<"$(
  printf '%s' "$input" | jq -r '[.hook_event_name // "", .session_id // ""] | @tsv' 2>/dev/null
)"

# The session id becomes both a notification id and a filename, so reduce it to characters
# that are inert in each — an id carrying `/` or `..` must not be able to name a path. An id we
# cannot use is not a reason to disturb the session, so it is a silent exit.
session=${session//[!A-Za-z0-9_-]/_}
[ -n "$session" ] || exit 0

notif_id="jkb-claude-$session"
state_dir="${JKB_NOTIFY_STATE:-${TMPDIR:-/tmp}/jkb-claude-notify}"
marker="$state_dir/$session"

# Dispatched off the arrays above rather than a `case` arm spelling the events a second time.
if in_list "$event" "${SHOW_EVENTS[@]}"; then
  show
elif in_list "$event" "${DISMISS_EVENTS[@]}"; then
  dismiss
fi

exit 0
