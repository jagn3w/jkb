#!/usr/bin/env bash
# Build, bundle, sign and register `jkb Notifier.app` from macos/notifier/.
#
# Four steps, none of them optional — each was established by measurement, and skipping any one
# produces a notifier that runs, reports success and displays nothing:
#
#   1. compile   swiftc against the system UserNotifications framework (no third-party code)
#   2. bundle    the binary + Info.plist, because UNUserNotificationCenter.current() refuses a
#                process whose main bundle has no identifier
#   3. sign      ad-hoc (`-`). An Apple developer account is NOT needed; what ad-hoc costs is
#                distribution, which we do not do. Unsigned, the notification centre refuses it.
#   4. register  Launch Services. Without this the framework answers "Notifications are not
#                allowed for this application" no matter how the bundle is signed.
#
# Then it installs and (re)loads the launchd agent `com.jkb.notifier`, which runs the bundle's binary
# as `jkb-notifier serve`: the consumer of jkb's `claude/notify` queue (design r3.2 N2). The agent
# points at the BUNDLE's binary, never a copy, so the notification centre finds the identifier; it
# is given the `jkb` to subscribe with, the database, and the topic — asked of that `jkb`
# (`jkb notify topic`), so the name is spelled once, in jkb_core::notify::TOPIC.
#
# Idempotent: safe to re-run, and `scripts/setup.sh` does on every pull.
#
# **Where it installs is not this script's decision.** The destination comes from
# `.claude/hooks/notify-sticky.sh --notifier-path`, the one place the location is written down,
# because the consumer failing to find the bundle is invisible: the hook just falls back to plain
# banners and the feature silently is not there. There is deliberately no --prefix — a flag that
# installs somewhere the hook does not look is a way to produce exactly that state.
#
# Flags: --check (verify the plist/path agreement and stop; runs on any OS), --print-agent (print
# the launchd agent and stop; runs on any OS), --jkb <path> (default: `jkb` on PATH), --db <path>
# (default: $JKB_DB, else ~/.jkb/jkb.db), --no-agent, --quiet, -h/--help.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
quiet=0
check_only=0
print_agent=0
agent=1
jkb_bin=""
db="${JKB_DB:-$HOME/.jkb/jkb.db}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --quiet) quiet=1 ;;
    --check) check_only=1 ;;
    --print-agent) print_agent=1 ;;
    --no-agent) agent=0 ;;
    --jkb) jkb_bin="${2:?--jkb needs a path}"; shift ;;
    --db) db="${2:?--db needs a path}"; shift ;;
    -h|--help) sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown flag: $1 (see --help)" >&2; exit 2 ;;
  esac
  shift
done

note() { [ "$quiet" -eq 1 ] || echo "$@"; }

# Read a <string> value out of the Info.plist. Deliberately NOT PlistBuddy or plutil: the
# consistency check below is worth running on every machine, and a check that only exists on macOS
# is absent from CI, which is where it would actually catch someone. Handles the key and value on
# one line or on two, which are the two layouts a hand-edit and `plutil -convert xml1` produce.
plist_string() { # plist_string <file> <key>
  awk -v key="$2" '
    index($0, "<key>" key "</key>") {
      rest = substr($0, index($0, "<key>" key "</key>"))
      if (match(rest, /<string>[^<]*<\/string>/)) {
        print substr(rest, RSTART + 8, RLENGTH - 17); exit
      }
      seen = 1; next
    }
    seen && match($0, /<string>[^<]*<\/string>/) {
      print substr($0, RSTART + 8, RLENGTH - 17); exit
    }
  ' "$1"
}

src="$repo_root/macos/notifier"

# Asked of the hook, then decomposed — so the bundle name, its location and the executable name
# all come from one string that only one file spells.
target="$(bash "$repo_root/.claude/hooks/notify-sticky.sh" --notifier-path)"
if [ -z "$target" ]; then
  echo "could not determine the install path from notify-sticky.sh --notifier-path" >&2
  exit 1
fi
app="${target%/Contents/MacOS/*}"
exe="${target##*/}"

# Every fact about the bundle that is load-bearing AND checkable from here. Each has the same
# failure signature — the notifier stops working and nothing says why — so a gate that measured
# only one of them read as coverage it did not have.
#
#   CFBundleExecutable  must name the file we are about to write, or the bundle will not launch.
#   CFBundleIdentifier  is FIRST in the design's list: UNUserNotificationCenter.current() refuses
#                       a process whose main bundle has none, so `post` fails on every machine.
#   CFBundleName        is the string the user has to find in System Settings to set Alerts; an
#                       empty one leaves the sticky half unreachable with nothing to search for.
check_plist() {
  local declared rc=0
  declared="$(plist_string "$src/Info.plist" CFBundleExecutable)"
  if [ "$declared" != "$exe" ]; then
    echo "Info.plist CFBundleExecutable is '$declared' but the install path names '$exe'" >&2
    rc=1
  fi
  local key
  for key in CFBundleIdentifier CFBundleName; do
    if [ -z "$(plist_string "$src/Info.plist" "$key")" ]; then
      echo "Info.plist $key is missing or empty" >&2
      rc=1
    fi
  done
  return "$rc"
}

# --- the launchd agent -----------------------------------------------------------------------
agent_label=com.jkb.notifier
agent_plist="$HOME/Library/LaunchAgents/$agent_label.plist"

xml_escape() { # the five XML entities, for a value inside <string>
  # sed, not ${v//&/&amp;}: bash 5.2's patsub_replacement turns `&` in a replacement into the match.
  printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' \
    -e 's/"/\&quot;/g' -e "s/'/\\&apos;/g"
}

# agent_plist_for <notifier> <jkb> <db> <topic> — the agent, as text. `KeepAlive`, because a consumer
# that is not running is a permission prompt nobody sees; `jkb-notifier serve` restarts its own
# subscription, so launchd restarts only the notifier itself. Logs beside the database, like
# com.jkb.serve's serve.log.
agent_plist_for() {
  local log
  log="$(dirname "$3")/notifier.log"
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$agent_label</string>
    <key>ProgramArguments</key>
    <array>
        <string>$(xml_escape "$1")</string>
        <string>serve</string>
        <string>--jkb</string>
        <string>$(xml_escape "$2")</string>
        <string>--db</string>
        <string>$(xml_escape "$3")</string>
        <string>--topic</string>
        <string>$(xml_escape "$4")</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>$(xml_escape "$log")</string>
    <key>StandardErrorPath</key>
    <string>$(xml_escape "$log")</string>
</dict>
</plist>
PLIST
}

# The jkb the agent subscribes with, and the topic that jkb names. Both are asked, never assumed: an
# agent pointing at a jkb that predates `notify topic` would subscribe to nothing, for ever.
resolve_agent_inputs() {
  [ -n "$jkb_bin" ] || jkb_bin="$(command -v jkb 2>/dev/null || true)"
  if [ -z "$jkb_bin" ] || [ ! -x "$jkb_bin" ]; then
    echo "no jkb found (pass --jkb) — cannot write the $agent_label agent" >&2
    return 1
  fi
  if ! topic="$("$jkb_bin" notify topic 2>/dev/null)" || [ -z "$topic" ]; then
    echo "$jkb_bin does not answer 'jkb notify topic' (older than this checkout?) — cannot write the $agent_label agent" >&2
    return 1
  fi
}

if [ "$print_agent" -eq 1 ]; then
  resolve_agent_inputs || exit 1
  agent_plist_for "$target" "$jkb_bin" "$db" "$topic"
  exit 0
fi

# The consistency check on its own, buildable-or-not and macOS-or-not, so `scripts/tests/notify-hook.test.sh`
# can assert the real rule rather than re-implementing it in a second place. Answered before the
# Darwin gate below, which is the whole point — this is the arm CI reaches.
if [ "${check_only:-0}" -eq 1 ]; then
  check_plist || exit 1
  note "  • plist:      executable '$exe', identifier + name present"
  exit 0
fi

if [ "$(uname -s)" != "Darwin" ]; then
  echo "jkb-notifier is macOS-only; nothing to build on $(uname -s)" >&2
  exit 0
fi

if ! command -v swiftc >/dev/null 2>&1; then
  echo "swiftc not found — install the Xcode Command Line Tools: xcode-select --install" >&2
  exit 1
fi

check_plist || exit 1

# Built into a staging directory and moved into place only once every step has succeeded, so a
# failed build cannot leave a half-made bundle that Launch Services has already registered.
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
mkdir -p "$staging/app/Contents/MacOS"

swiftc -O -swift-version 5 -o "$staging/app/Contents/MacOS/$exe" "$src/main.swift"
cp "$src/Info.plist" "$staging/app/Contents/Info.plist"

codesign --force --sign - "$staging/app" >/dev/null 2>&1

mkdir -p "$(dirname "$app")"
rm -rf "$app"
mv "$staging/app" "$app"

# Step 4, and it must not be silent either way. `[ -x … ] && "$lsregister" …` let `set -e` decide
# the two cases in OPPOSITE wrong directions: a missing lsregister made the whole list return 1,
# which `set -e` ignores in a non-final position, so the script reported success for a bundle the
# framework will refuse — while a lsregister that RAN and failed was the list's final command, so
# `set -e` aborted after `mv` had already installed a good bundle and setup.sh then reported that
# the build failed, which was untrue.
lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
if [ ! -x "$lsregister" ]; then
  echo "warning: lsregister not found at $lsregister — the bundle is installed but NOT registered," >&2
  echo "  so the notification centre will refuse it however it is signed ('post' will fail)." >&2
elif ! "$lsregister" -f "$app"; then
  echo "warning: lsregister failed — the bundle is installed but may not be registered," >&2
  echo "  so the notification centre may refuse it ('post' will fail)." >&2
fi

note "  • notifier:   $app"

# Step 5, the agent. Written atomically — launchd may be reading it — and reloaded, not just loaded:
# `load` leaves an agent already running on the binary this build just replaced.
if [ "$agent" -eq 1 ]; then
  if resolve_agent_inputs; then
    mkdir -p "$(dirname "$agent_plist")" "$(dirname "$db")"
    agent_plist_for "$target" "$jkb_bin" "$db" "$topic" > "$agent_plist.tmp.$$"
    mv -f "$agent_plist.tmp.$$" "$agent_plist"
    launchctl unload "$agent_plist" 2>/dev/null || true
    if launchctl load "$agent_plist"; then
      note "  • agent:      $agent_label loaded ($agent_plist)"
    else
      echo "warning: could not load $agent_label; activate manually: launchctl load '$agent_plist'" >&2
    fi
  else
    echo "warning: the $agent_label agent was not installed, so nothing displays jkb's notifications" >&2
  fi
fi
# Report rather than assume. `status` is the same read `setup.sh` and the hook rely on, so if it
# cannot answer here it could not have answered them either.
if state=$("$target" status 2>/dev/null); then
  note "  • state:      $state"
else
  note "  • state:      unknown (run: '$target' authorize)"
fi
