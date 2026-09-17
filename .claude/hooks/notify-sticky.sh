#!/usr/bin/env bash
# Notification hooks: make Claude Code's local notification STICKY, and clear it the moment
# you answer it.
#
# A macOS banner auto-hides after a few seconds, which is exactly backwards for "Claude needs
# your permission": the one notification you have to act on is the one most likely to be missed,
# and a session then sits blocked until you happen to look.
#
#   Notification                                       -> post under this session's id
#   PostToolUse | UserPromptSubmit | Stop | SessionEnd  -> withdraw that id
#   SessionStart                                        -> withdraw what dead sessions left
#
# **This shim decides nothing and posts nothing** (design r3.2 N1,
# openspec/changes/jkb-message-queue/design-r3.md). It hands the payload to `jkb notify hook`, which
# sends it to `jkb serve` on the host; the daemon runs the notification lifecycle table against its
# own record of the session and puts posts and withdrawals on the `claude/notify` queue, and
# `jkb-notifier serve` on the Mac displays them. So a hook in the dev container — which can neither
# reach the Mac's notification centre nor open the host's database — works exactly as one on the host.
# The rules themselves are in crates/jkb-core/src/notify.rs; docs/notifications.md is the record.
#
# **The notification id is derived from `session_id`, so dismissing owns nothing**: parallel
# sessions (the D36 worktrees) each hold their own id and cannot clear one another's.
#
# **`PostToolUse` is the closest observable "permission was given".** `PreToolUse` runs *before*
# the permission prompt, so it cannot be the dismiss trigger; nothing fires at the moment you
# click Allow. Granting a long-running command clears the notification when the command finishes.
#
# **The notifier is ours** — `macos/notifier/`, built by `scripts/build-notifier.sh` into
# `jkb Notifier.app`, run by launchd as `com.jkb.notifier`. Withdrawing a delivered notification is
# the entire feature and only Apple's current `UserNotifications` framework offers it.
#
# Fails OPEN and silent. Any failure here must not disturb the session, so it always exits 0 and
# writes nothing to stdout (a hook's stdout lands in the transcript). `jkb notify hook` logs its
# own failures to ~/.jkb/logs/notify-hook.log.

# Where the notifier lives, spelled ONCE for the whole repo. `scripts/build-notifier.sh` installs
# to `--notifier-path` (the first entry) rather than composing a path of its own, and
# `scripts/setup.sh` reports on `--find-notifier`. This shim no longer runs the notifier itself.
# A per-user location first, because installing there needs no privileges.
NOTIFIER_PATHS=(
  "$HOME/Applications/jkb Notifier.app/Contents/MacOS/jkb-notifier"
  "/Applications/jkb Notifier.app/Contents/MacOS/jkb-notifier"
)

# Every event `.claude/settings.json` must register for this hook, and the ONLY place in shell they
# are enumerated. A registration that goes missing is silent in the worst way, so `--events` exists
# for the cross-check in `scripts/tests/notify-hook.test.sh` to diff this list against settings.json
# and against `jkb notify events`.
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
  local c
  for c in "${NOTIFIER_PATHS[@]}"; do
    if [ -x "$c" ]; then
      notifier="$c"
      return
    fi
  done
}

# `scripts/setup.sh` asks THIS file where the notifier is instead of keeping a second copy of the
# search list above. Answered before stdin is read, because a probe has none to give.
if [ "${1:-}" = "--find-notifier" ]; then
  find_notifier
  [ -n "$notifier" ] || exit 1
  printf '%s\n' "$notifier"
  exit 0
fi

# Where a notifier SHOULD be installed, whether or not one is there yet — which is why this is a
# separate question from `--find-notifier`. `scripts/build-notifier.sh` builds to exactly this.
if [ "${1:-}" = "--notifier-path" ]; then
  printf '%s\n' "${NOTIFIER_PATHS[0]}"
  exit 0
fi

# Every event this hook acts on, for the settings.json cross-check in scripts/tests/notify-hook.test.sh.
if [ "${1:-}" = "--events" ]; then
  printf '%s\n' "${HOOK_EVENTS[@]}"
  exit 0
fi

# `jkb` carries the request, but a hook inherits the session's PATH, and a GUI-launched terminal
# routinely lacks ~/.cargo/bin.
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
# rules in shell.
[ -n "$jkb_bin" ] || exit 0

# NOT `exec`. `exec` makes jkb's exit status the HOOK's, and a `PostToolUse` hook that exits
# non-zero blocks the tool call — so an older `jkb` without this subcommand, which clap rejects
# with status 2, stops the session dead. That is exactly what happened the first time this shim
# ran. Whatever jkb does, this exits 0.
#
# `$PPID` here is the process that invoked the hook, and measurement says that is `claude`
# itself — a process that outlives this shim by the whole session. jkb cannot ask for it: its own
# parent is THIS script, which exits milliseconds later, and recording that made every record read
# as provably dead, so the next session's sweep withdrew a live session's pending prompt.
JKB_HOOK_OWNER="$PPID" "$jkb_bin" notify hook >/dev/null 2>&1
exit 0
