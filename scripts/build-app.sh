#!/usr/bin/env bash
# Build and install Code Factory, the jkb desktop app, from the app's clean clone (D53.3).
#
#   ~/.local/share/jkb-app/src/scripts/build-app.sh [--app-home DIR] [--dest DIR]
#
# The app runs unsandboxed on this machine and opens host terminals, so it is built only from the
# clone of origin/main under --app-home (default ~/.local/share/jkb-app; the clone is its `src/`),
# which no agent can write, and only with that clone at origin/main's tip and nothing else in its
# tree. Run from anywhere else — a checkout, a worktree — it refuses: run a checkout deliberately
# with `JKB_APP_FROM_CHECKOUT=1 pnpm --filter @jkb/app dev` instead.
#
# It installs the frozen lockfile, builds @jkb/core and the app, packages it (electron-builder
# --dir), copies it to --dest (default ~/Applications/Code Factory.app on macOS, <app-home>/app
# elsewhere) through lib.sh's `app_swap`, and records the commit in <app-home>/installed, which the
# app's *jkb ▸ Update from main…* counts from. On Linux it also writes a desktop entry.
#
# Who runs it: scripts/setup.sh, after moving the clone to origin/main (`app_clone_refresh`), and
# the installed app's update (ui/app/src/main/update.ts), after doing the same. Neither runs a
# checkout's copy of this file: they run the clone's.
#
# Flags: --app-home DIR, --dest DIR, -h/--help.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"

app_home="$HOME/.local/share/jkb-app"
dest=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --app-home) app_home="${2:?--app-home needs a directory}"; shift ;;
        --dest)     dest="${2:?--dest needs a directory}"; shift ;;
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
rm -rf app/dist
pnpm --filter @jkb/app run package
built="$(app_built_product "$repo_root/ui/app/dist" "$os")" || die "the package step left no app to install"

# --- install ------------------------------------------------------------------------------------
echo "==> installing $built at $dest"
app_swap "$built" "$dest" "$app_home/previous" || die "the installed app is unchanged"
# The stamp last, written whole: it says what is installed, so it changes only once that is true.
mkdir -p "$app_home"
printf 'commit=%s\n' "$commit" >"$app_home/installed.tmp" && mv -f "$app_home/installed.tmp" "$app_home/installed"

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
