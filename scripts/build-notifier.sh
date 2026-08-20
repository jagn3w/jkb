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
# Idempotent: safe to re-run, and `scripts/setup.sh` does on every pull.
#
# **Where it installs is not this script's decision.** The destination comes from
# `.claude/hooks/notify-sticky.sh --notifier-path`, the one place the location is written down,
# because the consumer failing to find the bundle is invisible: the hook just falls back to plain
# banners and the feature silently is not there. There is deliberately no --prefix — a flag that
# installs somewhere the hook does not look is a way to produce exactly that state.
#
# Flags: --quiet, -h/--help.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
quiet=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --quiet) quiet=1 ;;
    -h|--help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown flag: $1 (see --help)" >&2; exit 2 ;;
  esac
  shift
done

note() { [ "$quiet" -eq 1 ] || echo "$@"; }

if [ "$(uname -s)" != "Darwin" ]; then
  echo "jkb-notifier is macOS-only; nothing to build on $(uname -s)" >&2
  exit 0
fi

if ! command -v swiftc >/dev/null 2>&1; then
  echo "swiftc not found — install the Xcode Command Line Tools: xcode-select --install" >&2
  exit 1
fi

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

# CFBundleExecutable must name the file we are about to write, or the bundle will not launch and
# every later symptom (no notifications, no error) points somewhere else entirely. Checked here
# because this is the only place that knows both halves.
declared="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$src/Info.plist" 2>/dev/null || true)"
if [ "$declared" != "$exe" ]; then
  echo "Info.plist CFBundleExecutable is '$declared' but the install path names '$exe'" >&2
  exit 1
fi

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

lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
[ -x "$lsregister" ] && "$lsregister" -f "$app"

note "  • notifier:   $app"
# Report rather than assume. `status` is the same read `setup.sh` and the hook rely on, so if it
# cannot answer here it could not have answered them either.
if state=$("$target" status 2>/dev/null); then
  note "  • state:      $state"
else
  note "  • state:      unknown (run: '$target' authorize)"
fi
