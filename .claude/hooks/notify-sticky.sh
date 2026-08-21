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

# Every event `.claude/settings.json` must register for this hook, and the ONLY place they are
# enumerated. The shim no longer dispatches on them — `jkb notify hook` does — but a registration
# that goes missing is silent in the worst way, so `--events` exists for the cross-check in
# `scripts/test-hooks.sh` to diff this list against settings.json in both directions.
#
# `SessionStart` is not one of the lifecycle's events: it drives the sweep over records left by
# OTHER sessions, which is the only route by which a notification from a killed session ever
# comes down.
HOOK_EVENTS=(Notification PostToolUse UserPromptSubmit Stop SessionEnd SessionStart)

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
# Every event this hook acts on, for the settings.json cross-check in scripts/test-hooks.sh.
if [ "${1:-}" = "--events" ]; then
  printf '%s\n' "${HOOK_EVENTS[@]}"
  exit 0
fi

# `jkb` carries the decision, but a hook inherits the session's PATH, and a GUI-launched terminal
# routinely lacks ~/.cargo/bin. Looked up the same way the notifier is, and for the same reason.
find_jkb() {
  local c
  for c in "$(command -v jkb 2>/dev/null || true)" \
    "${CARGO_HOME:-$HOME/.cargo}/bin/jkb"; do
    if [ -n "$c" ] && [ -x "$c" ]; then
      printf '%s' "$c"
      return
    fi
  done
}

jkb_bin=$(find_jkb)
# Fail OPEN and silent. No `jkb` means no notification — never a second implementation of the
# rules in shell, which is the whole point of moving them into a table something can check.
[ -n "$jkb_bin" ] || exit 0

find_notifier
# The notifier's location stays OURS: `scripts/build-notifier.sh` and `scripts/setup.sh` already
# ask this file for it, and both run where `jkb` may not be built yet. It is handed over rather
# than looked up twice.
#
# NOT `exec`. `exec` makes jkb's exit status the HOOK's, and a `PostToolUse` hook that exits
# non-zero blocks the tool call — so an older `jkb` without this subcommand, which clap rejects
# with status 2, stops the session dead. That is exactly what happened the first time this shim
# ran. Whatever jkb does, this exits 0.
JKB_NOTIFIER="$notifier" "$jkb_bin" notify hook >/dev/null 2>&1
exit 0
