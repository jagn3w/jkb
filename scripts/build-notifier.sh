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
# Flags: --prefix <dir> (default ~/Applications), --quiet, -h/--help.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
prefix="$HOME/Applications"
quiet=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --prefix) shift; prefix="$1" ;;
    --quiet) quiet=1 ;;
    -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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
app="$prefix/jkb Notifier.app"

# Built into a staging directory and moved into place only once every step has succeeded, so a
# failed build cannot leave a half-made bundle that Launch Services has already registered.
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
mkdir -p "$staging/app/Contents/MacOS"

swiftc -O -swift-version 5 \
  -o "$staging/app/Contents/MacOS/jkb-notifier" \
  "$src/main.swift"
cp "$src/Info.plist" "$staging/app/Contents/Info.plist"

codesign --force --sign - "$staging/app" >/dev/null 2>&1

mkdir -p "$prefix"
rm -rf "$app"
mv "$staging/app" "$app"

lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
[ -x "$lsregister" ] && "$lsregister" -f "$app"

note "  • notifier:   $app"
# Report rather than assume. `status` is the same read `setup.sh` and the hook rely on, so if it
# cannot answer here it could not have answered them either.
if state=$("$app/Contents/MacOS/jkb-notifier" status 2>/dev/null); then
  note "  • state:      $state"
else
  note "  • state:      unknown (run: '$app/Contents/MacOS/jkb-notifier' authorize)"
fi
