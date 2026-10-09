#!/usr/bin/env bash
# Install the staged Code Factory: the ONE step that swaps the app (D53.3).
#
#   ~/.local/share/jkb-app/src/scripts/install-app.sh [--wait-pid PID] [--relaunch]
#
# scripts/build-app.sh stages a build at <app-home>/staged ({app/, commit}). This swaps it in at the
# one place the app runs from (~/Applications/Code Factory.app on macOS, <app-home>/app elsewhere) —
# but ONLY while no copy of the app is running (lib.sh's app_running, from `ps -ww`): a running
# Electron app loads its helpers from the bundle by path, so a swap under it would run the new
# bundle's helpers in the old process. If one is running, nothing is swapped and the staged copy is
# kept for next time (exit 76).
#
# The swap is lib.sh's app_swap (only over an app the stamp says jkb installed; the old one kept as
# <app-home>/previous; a failed rename rolled back), then the stamp <app-home>/installed
# (`commit=`, `dest=`), then the staged copy is removed. On Linux it writes a desktop entry.
#
# --wait-pid PID: first wait (up to two minutes) for PID — the app that started this on its way out —
# and every other copy of the app to exit. --relaunch: start the app afterwards, whatever happened,
# unless a copy is running. The app's *Update from main…* starts this detached with both; setup.sh
# runs it with neither, after build-app.sh, when the app is not running.
#
# It holds lib.sh's app lock (or proceeds under its caller's, JKB_APP_LOCK_TOKEN). Exit status: 0
# installed; 75 another build or install holds the lock; 76 a copy is running, nothing swapped;
# anything else, a failure, and the installed app is as it was.
#
# Flags: --wait-pid PID, --relaunch, -h/--help. (--app-home DIR other than the default, and --dest
# DIR, are for the tests only, with JKB_APP_BUILD_TEST=1.)
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"
app_scrub_env

app_home="$(app_default_home)"
dest=""
wait_pid=""
relaunch=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --app-home) app_home="${2:?--app-home needs a directory}"; shift ;;
        --wait-pid) wait_pid="${2:?--wait-pid needs a pid}"; shift ;;
        --relaunch) relaunch=1 ;;
        --dest)
            if [ "${JKB_APP_BUILD_TEST:-}" != 1 ]; then
                echo "install-app.sh: --dest is for the tests: the app installs only where it runs from" >&2; exit 2
            fi
            dest="${2:?--dest needs a directory}"; shift ;;
        -h|--help)  sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)          echo "install-app.sh: unknown flag: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done

app_check_home "$app_home" || exit 2
os="$(uname -s)"
[ -n "$dest" ] || dest="$(app_default_dest "$os" "$HOME" "$app_home")"
exe="$(app_executable "$os" "$dest")"

# end <status> <message> — release the lock, relaunch if asked and nothing runs, and exit.
end() {
    local status="$1"
    shift
    [ "$#" -eq 0 ] || printf 'install-app.sh: %s\n' "$*" >&2
    app_unlock "$app_home"
    if [ "$relaunch" = 1 ] && ! app_running "$os" "$dest" && [ -e "$exe" ]; then
        if [ "$os" = Darwin ]; then
            open "$dest" >/dev/null 2>&1 || :
        else
            nohup "$exe" >/dev/null 2>&1 &
        fi
    fi
    exit "$status"
}
trap 'end 143 "stopped"' TERM INT HUP

if [ -n "$wait_pid" ]; then
    case "$wait_pid" in *[!0-9]*) end 2 "--wait-pid is not a pid: $wait_pid" ;; esac
    tries=600
    while kill -0 "$wait_pid" 2>/dev/null || app_running "$os" "$dest"; do
        tries=$((tries - 1))
        [ "$tries" -gt 0 ] || end "$APP_EXIT_RUNNING" "Code Factory is still running; the staged copy is kept for the next install"
        sleep 0.2
    done
fi

lock_rc=0
app_lock "$app_home" || lock_rc=$?
case "$lock_rc" in
    0) ;;
    1) end "$APP_EXIT_BUSY" ;;
    *) end 1 ;;
esac

commit="$(app_staged_commit "$app_home")"
[ -n "$commit" ] || end 1 "nothing is staged in $app_home/$APP_STAGED_IN_APP_HOME (scripts/build-app.sh stages a build)"
running=0
app_running "$os" "$dest" || running=$?
case "$running" in
    0) end "$APP_EXIT_RUNNING" "Code Factory is running from $dest: quit it to install ${commit:0:12}; the staged copy is kept" ;;
    2) end 1 "cannot tell whether Code Factory is running (no ps), so nothing is swapped" ;;
esac

echo "==> installing Code Factory ${commit:0:12} at $dest"
app_swap "$app_home/$APP_STAGED_IN_APP_HOME/app" "$dest" "$app_home" || end 1 "the installed app is unchanged"
# The stamp names the commit now in place. If it cannot be written, app_swap's own stamp still names
# <dest> (with the commit before), so the next install sees a stale commit and replaces it.
app_stamp "$app_home" "$commit" "$dest" \
    || warn "Code Factory ${commit:0:12} is installed, but $app_home/installed could not be written; the next install re-stamps it"
rm -rf "${app_home:?}/$APP_STAGED_IN_APP_HOME"

if [ "$os" = Linux ]; then
    apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
    if mkdir -p "$apps" && cat >"$apps/.jkb-code-factory.desktop.tmp" <<EOF && mv -f "$apps/.jkb-code-factory.desktop.tmp" "$apps/jkb-code-factory.desktop"; then
[Desktop Entry]
Type=Application
Name=Code Factory
Comment=jkb: design, plan and implement with Claude Code
Exec="$exe" %U
Terminal=false
Categories=Development;
EOF
        echo "  • desktop entry: $apps/jkb-code-factory.desktop"
    else
        warn "could not write a desktop entry under $apps; start the app with $exe"
    fi
fi
echo "installed Code Factory ${commit:0:12} at $dest"
end 0
