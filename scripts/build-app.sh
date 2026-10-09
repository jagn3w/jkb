#!/usr/bin/env bash
# Build Code Factory, the jkb desktop app, from the app's clean clone, and STAGE it (D53.3).
#
#   ~/.local/share/jkb-app/src/scripts/build-app.sh [--update-to SHA]
#
# The app runs unsandboxed on this machine and opens host terminals, so it is built only from the
# clone of origin/main under ~/.local/share/jkb-app (the clone is its `src/`), which no agent can
# write, and only with that clone at origin/main's tip and nothing else in its tree. Run from
# anywhere else — a checkout, a worktree — it refuses: run a checkout deliberately with
# `JKB_APP_FROM_CHECKOUT=1 pnpm --filter @jkb/app dev` instead.
#
# It installs the frozen lockfile, builds @jkb/core and the app (writing the commit into
# ui/app/out/commit, so the app knows what IT is), packages it (electron-builder --dir), and copies
# the result to <app-home>/staged/app with the commit in <app-home>/staged/commit. It installs
# NOTHING: scripts/install-app.sh is the one step that swaps a staged copy in, and only while no
# copy of the app is running.
#
# --update-to SHA (the app's *Update from main…*): first fetch main, require its tip to be SHA (the
# commit the user was shown), move the clone there and clean it, and build with THAT commit's copy of
# this script. setup.sh moves the clone itself (lib.sh's app_clone_refresh) and runs this without it.
#
# It holds lib.sh's app lock throughout (or proceeds under its caller's, passed in
# JKB_APP_LOCK_TOKEN), so two builds never share the clone or the staging directory; app_lock
# releases it however this ends. Exit status: 0 staged; 75 another build or install holds the lock
# (stderr names it and its holder); anything else, a failure.
#
# Repository selection (GIT_DIR and the rest) and Electron's variables are dropped first, for every
# caller: the post-merge hook runs setup.sh with GIT_DIR naming the merged repository, and pnpm's
# lifecycle scripts and git-hosted dependencies would act on it.
#
# Flags: --update-to SHA, -h/--help. (--app-home DIR other than the default is for the tests only,
# with JKB_APP_BUILD_TEST=1: the app identifies its installed copy by the default.)
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"
app_scrub_env

app_home="$(app_default_home)"
update_to=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --app-home)  app_home="${2:?--app-home needs a directory}"; shift ;;
        --update-to) update_to="${2:?--update-to needs a commit}"; shift ;;
        -h|--help)   sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)           echo "build-app.sh: unknown flag: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done

die() { printf 'build-app.sh: %s\n' "$*" >&2; exit 1; }

app_check_home "$app_home" || exit 2
os="$(uname -s)"

# --- this is the clone ------------------------------------------------------------------------
src="$app_home/src"
src_real="$(cd "$src" 2>/dev/null && pwd -P)" || src_real=""
if [ "$repo_root" != "$src_real" ]; then
    die "this is $repo_root, not the app's clean clone ($src). The app is built only from that clone
  at origin/main (scripts/setup.sh makes it). To run a checkout deliberately:
    JKB_APP_FROM_CHECKOUT=1 pnpm --filter @jkb/app dev"
fi
lock_rc=0
app_lock "$app_home" || lock_rc=$?
case "$lock_rc" in
    0) ;;
    1) exit "$APP_EXIT_BUSY" ;;
    *) exit 1 ;;
esac

if [ -n "$update_to" ]; then
    case "$update_to" in *[!0-9a-f]*) die "--update-to is not a commit id: $update_to" ;; esac
    [ "${#update_to}" -eq 40 ] || die "--update-to is not a full commit id: $update_to"
    GIT_TERMINAL_PROMPT=0 _git -C "$repo_root" fetch --quiet --no-tags origin "$APP_UPDATE_REFSPEC" \
        || die "could not fetch main"
    tip="$(_git -C "$repo_root" rev-parse --verify --quiet "$APP_UPDATE_REF^{commit}")" || die "no origin/main"
    [ "$tip" = "$update_to" ] || die "origin/main moved to ${tip:0:12} since ${update_to:0:12} was shown; check again"
    { _git -C "$repo_root" checkout --quiet --detach --force "$tip" && _git -C "$repo_root" clean -ffdq; } \
        || die "could not move $repo_root to ${tip:0:12}"
    [ -f "$repo_root/scripts/build-app.sh" ] || die "main at ${tip:0:12} has no scripts/build-app.sh"
    # That commit's builder, under this lock. This process keeps reading its own (older) copy of
    # this file, which git replaced rather than rewrote.
    rc=0
    JKB_APP_LOCK_TOKEN="$app_lock_token" /bin/bash "$repo_root/scripts/build-app.sh" --app-home "$app_home" || rc=$?
    exit "$rc"
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
# What this build is, inside it (electron-builder packs out/**): the app compares it with main.
printf '%s\n' "$commit" >app/out/commit
rm -rf app/dist
pnpm --filter @jkb/app run package
built="$(app_built_product "$repo_root/ui/app/dist" "$os")" || die "the package step left no app to stage"

# --- stage --------------------------------------------------------------------------------------
# Whole or not at all: built beside, and only once complete renamed over the last staged copy, so a
# failed copy (a full disk) leaves that copy as it was.
staged="$app_home/$APP_STAGED_IN_APP_HOME"
rm -rf "$staged.new" "$staged.old"
mkdir -p "$staged.new"
cp -pR "$built" "$staged.new/app" || { rm -rf "$staged.new"; die "could not copy $built into $staged.new"; }
printf '%s\n' "$commit" >"$staged.new/commit" || { rm -rf "$staged.new"; die "could not write $staged.new/commit"; }
if [ -e "$staged" ]; then mv "$staged" "$staged.old" || die "could not move the last staged copy aside"; fi
mv "$staged.new" "$staged" || { [ ! -e "$staged.old" ] || mv "$staged.old" "$staged"; die "could not move $staged.new to $staged"; }
rm -rf "$staged.old"
echo "staged Code Factory ${commit:0:12} at $staged; scripts/install-app.sh installs it"
