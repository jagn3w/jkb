#!/usr/bin/env bash
# Build and install Code Factory, the jkb desktop app, from the app's clean clone (D53.3).
#
#   ~/.local/share/jkb-app/src/scripts/build-app.sh [--app-home DIR] [--replacing-running]
#
# The app runs unsandboxed on this machine and opens host terminals, so it is built only from the
# clone of origin/main under --app-home (default ~/.local/share/jkb-app; the clone is its `src/`),
# which no agent can write, and only with that clone at origin/main's tip and nothing else in its
# tree. Run from anywhere else — a checkout, a worktree — it refuses: run a checkout deliberately
# with `JKB_APP_FROM_CHECKOUT=1 pnpm --filter @jkb/app dev` instead.
#
# It installs the frozen lockfile, builds @jkb/core and the app, packages it (electron-builder
# --dir), copies it to the one place the app runs from (~/Applications/Code Factory.app on macOS,
# <app-home>/app elsewhere: lib.sh's `app_default_dest`, which the app's own identity check agrees
# with) through lib.sh's `app_swap`, and records the commit and the destination in
# <app-home>/installed. The commit is also built into the app (ui/app/out/commit), so the app knows
# what IT is, which the stamp cannot say once something else has been swapped in under it. On Linux it
# also writes a desktop entry.
#
# It holds lib.sh's app lock throughout (or recognises its caller's, passed in JKB_APP_LOCK_TOKEN),
# so two installs never share the clone, app/dist or the swap.
#
# It does not swap under a running copy (lib.sh's `app_running`, checked right before the swap): a
# running Electron app loads its helpers from the bundle by path. Only the app's own update passes
# --replacing-running, because it is that copy and relaunches the moment this returns.
#
# Exit status: 0 installed; 75 another install holds the lock; 76 a copy is running, nothing swapped;
# 77 the app WAS swapped in but the stamp could not be written (relaunch it; the next install
# re-stamps); anything else, the installed app is unchanged. lib.sh's APP_BUILD_EXIT_* and
# @jkb/core's BUILD_EXIT name the same codes.
#
# Repository selection (GIT_DIR and the rest) and Electron's variables are dropped first, for every
# caller: the post-merge hook runs setup.sh with GIT_DIR naming the merged repository, and pnpm's
# lifecycle scripts and git-hosted dependencies would act on it.
#
# Who runs it: scripts/setup.sh, after moving the clone to origin/main (`app_clone_refresh`), and
# the installed app's update (ui/app/src/main/update.ts), after doing the same. Neither runs a
# checkout's copy of this file: they run the clone's.
#
# Flags: --app-home DIR, --replacing-running, -h/--help. (--dest DIR exists for the tests only, with
# JKB_APP_BUILD_TEST=1: an app installed anywhere else refuses to start and the stamp would vouch for
# a place no update installs to.)
set -euo pipefail

unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES
for _var in $(compgen -e); do
    case "$_var" in ELECTRON_*) unset "$_var" ;; esac
done
unset _var

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"

app_home="$HOME/.local/share/jkb-app"
dest=""
replacing_running=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --app-home) app_home="${2:?--app-home needs a directory}"; shift ;;
        --replacing-running) replacing_running=1 ;;
        --dest)
            if [ "${JKB_APP_BUILD_TEST:-}" != 1 ]; then
                echo "build-app.sh: --dest is for the tests: the app installs only where it runs from" >&2; exit 2
            fi
            dest="${2:?--dest needs a directory}"; shift ;;
        -h|--help)  sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)          echo "build-app.sh: unknown flag: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done

die() { printf 'build-app.sh: %s\n' "$*" >&2; exit 1; }

case "$app_home" in /*) ;; *) die "--app-home must be an absolute path: $app_home" ;; esac
os="$(uname -s)"
[ -n "$dest" ] || dest="$(app_default_dest "$os" "$HOME" "$app_home")"
case "$dest" in /*) ;; *) die "--dest must be an absolute path: $dest" ;; esac

# --- this is the clone, at main ---------------------------------------------------------------
src="$app_home/src"
src_real="$(cd "$src" 2>/dev/null && pwd -P)" || src_real=""
if [ "$repo_root" != "$src_real" ]; then
    die "this is $repo_root, not the app's clean clone ($src). The app is built only from that clone
  at origin/main (scripts/setup.sh makes it). To run a checkout deliberately:
    JKB_APP_FROM_CHECKOUT=1 pnpm --filter @jkb/app dev"
fi
app_lock "$app_home" || { printf 'build-app.sh: busy: another install holds the app lock\n' >&2; exit "$APP_BUILD_EXIT_BUSY"; }
trap 'app_unlock "$app_home"' EXIT
app_clone_check "$repo_root" || die "refusing to build: $repo_root is not exactly origin/main"
commit="$(_git -C "$repo_root" rev-parse HEAD)"

# --- build --------------------------------------------------------------------------------------
# A GUI app (the update) and the post-merge hook both start this with a short PATH. pnpm lives under
# PNPM_HOME, which shells export only interactively (check.sh does the same); Homebrew's node and
# pnpm are in /opt/homebrew/bin or /usr/local/bin.
export PNPM_HOME="${PNPM_HOME:-$HOME/Library/pnpm}"
export PATH="$PNPM_HOME:$HOME/.local/share/pnpm:/opt/homebrew/bin:/usr/local/bin:$PATH"
command -v pnpm >/dev/null 2>&1 || die "pnpm not found (install it, or set PNPM_HOME)"

echo "==> building Code Factory at ${commit:0:12} (origin/main) in $repo_root"
cd "$repo_root/ui"
# FROZEN: the lockfile is what was reviewed and landed with this commit; an install that would
# change it fails rather than resolving something new.
pnpm install --frozen-lockfile
# The app and what it depends on (@jkb/core), in order: each type-checks before it emits.
pnpm --filter "@jkb/app..." run build
# What this build is, inside it (electron-builder packs out/**): the app compares it with main.
printf '%s\n' "$commit" >app/out/commit
rm -rf app/dist
pnpm --filter @jkb/app run package
built="$(app_built_product "$repo_root/ui/app/dist" "$os")" || die "the package step left no app to install"

# --- install ------------------------------------------------------------------------------------
echo "==> installing $built at $dest"
if [ "$replacing_running" = 0 ]; then
    case "$(app_running "$os" "$dest"; echo $?)" in
        0) printf 'build-app.sh: Code Factory is running from %s; quit it and re-run, or use its jkb ▸ Update from main…\n' "$dest" >&2
           exit "$APP_BUILD_EXIT_RUNNING" ;;
        2) warn "could not tell whether Code Factory is running (no ps); if it is, restart it after this" ;;
    esac
fi
app_swap "$built" "$dest" "$app_home" || die "the installed app is unchanged"
# The stamp last, written whole: it says what is installed and where, so it changes only once that is
# true. A failure here is the script's failure (an && list is exempt from set -e).
{ printf 'commit=%s\ndest=%s\n' "$commit" "$dest" >"$app_home/installed.tmp" \
    && mv -f "$app_home/installed.tmp" "$app_home/installed"; } \
    || { printf 'build-app.sh: the app at %s is %s, but the stamp %s/installed could not be written\n' "$dest" "${commit:0:12}" "$app_home" >&2
         exit "$APP_BUILD_EXIT_UNRECORDED"; }

if [ "$os" = Linux ]; then
    apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
    if mkdir -p "$apps" && cat >"$apps/.jkb-code-factory.desktop.tmp" <<EOF && mv -f "$apps/.jkb-code-factory.desktop.tmp" "$apps/jkb-code-factory.desktop"; then
[Desktop Entry]
Type=Application
Name=Code Factory
Comment=jkb: design, plan and implement with Claude Code
Exec="$dest/code-factory" %U
Terminal=false
Categories=Development;
EOF
        echo "  • desktop entry: $apps/jkb-code-factory.desktop"
    else
        warn "could not write a desktop entry under $apps; start the app with $dest/code-factory"
    fi
fi
echo "installed Code Factory ${commit:0:12} at $dest"
