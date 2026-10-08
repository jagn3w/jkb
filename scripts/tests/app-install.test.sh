#!/usr/bin/env bash
# Code Factory's installed copy (docs/code-factory.md, D53.3): the clean clone setup.sh keeps at
# origin/main (lib.sh's app_clone_refresh / app_clone_check), the builder that refuses any tree but
# that clone (scripts/build-app.sh), the swap that installs it (app_swap), setup.sh's step
# (install_app) and its summary line.
#
# Real repositories throughout: a bare `origin`, a "checkout" of it on a feature branch (what an
# agent can write), and the clone under a scratch app home. The build itself is a stub `pnpm` that
# leaves a packaged app where electron-builder would; packaging for real needs the Electron download.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

new_workdir
isolate_git "$work/home"

# What electron-builder leaves on this platform, relative to ui/.
if [ "$(uname -s)" = Darwin ]; then product="app/dist/mac-arm64/Code Factory.app"; else product="app/dist/linux-unpacked"; fi

# A stub pnpm: logs its arguments, and `run package` leaves a packaged app holding $STUB_ID.
stub="$work/stub"
mkdir -p "$stub"
cat >"$stub/pnpm" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >>"$stub/log"
case "\$*" in
    *"run package"*) mkdir -p "$product" && printf '%s\n' "\${STUB_ID:-x}" >"$product/id" ;;
    *"run build"*) [ -z "\${STUB_FAIL_BUILD:-}" ] || exit 1 ;;
esac
exit 0
EOF
chmod +x "$stub/pnpm"

# fixture <name> — sets $seed (where main is authored), $origin (bare), $checkout (a clone of origin
# on a feature branch with a commit main does not have), $app_home. main carries the real
# build-app.sh and lib.sh unless NO_BUILDER is set.
fixture() {
    local d="$work/$1"
    seed="$d/seed" origin="$d/origin.git" checkout="$d/checkout" app_home="$d/app-home"
    git_q init -q -b main "$seed"
    mkdir -p "$seed/ui" "$seed/scripts"
    printf 'node_modules/\ndist/\nout/\n' >"$seed/ui/.gitignore"
    if [ -z "${NO_BUILDER:-}" ]; then
        cp "$repo_root/scripts/build-app.sh" "$repo_root/scripts/lib.sh" "$seed/scripts/"
    fi
    git_q -C "$seed" add -A && git_q -C "$seed" commit -q -m one
    git_q clone -q --bare "$seed" "$origin"
    git_q -C "$seed" remote add origin "$origin"
    git_q clone -q "$origin" "$checkout"
    git_q -C "$checkout" switch -q -c feature
    echo agent >"$checkout/agent.txt"
    git_q -C "$checkout" add -A && git_q -C "$checkout" commit -q -m "an agent's commit"
}

# land <message> — a new commit on main, pushed to origin.
land() {
    echo "$1" >>"$seed/landed.txt"
    git_q -C "$seed" add -A && git_q -C "$seed" commit -q -m "$1" && git_q -C "$seed" push -q origin main
}

# build <app-home> <dest> [script] — run a build-app.sh (the clone's by default) with the stub pnpm.
build() {
    local script="${3:-$1/src/scripts/build-app.sh}"
    PNPM_HOME="$stub" XDG_DATA_HOME="$work/xdg" /bin/bash "$script" --app-home "$1" --dest "$2"
}

# --- 1. the clone follows origin/main, never the checkout's branch ------------------------------
case1() {
    fixture c1
    local src="$app_home/src"
    if ! app_clone_refresh "$checkout" "$src" 2>"$work/c1.err"; then
        fail "clone: created" "$(cat "$work/c1.err")"; return
    fi
    if [ "$(git -C "$src" rev-parse HEAD)" = "$(git -C "$seed" rev-parse main)" ]; then
        ok "clone: at origin/main, not the checkout's feature branch"
    else
        fail "clone: at origin/main" "HEAD is $(git -C "$src" rev-parse HEAD)"
    fi
    [ ! -e "$src/agent.txt" ] && ok "clone: the checkout's unlanded commit is not in it" \
        || fail "clone: unlanded commit" "agent.txt is in the clone"
    land two
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    [ "$(git -C "$src" rev-parse HEAD)" = "$(git -C "$seed" rev-parse main)" ] \
        && ok "clone: a refresh moves it to main's new tip" \
        || fail "clone: refresh" "HEAD did not follow main"
    # The clone's own origin is what it fetches once it exists: re-pointing the checkout's origin
    # at somewhere else redirects nothing.
    git -C "$checkout" remote set-url origin "$work/nowhere.git"
    if app_clone_refresh "$checkout" "$src" 2>/dev/null; then
        ok "clone: an existing clone ignores what the checkout's origin now says"
    else
        fail "clone: existing clone" "refresh failed after the checkout's origin changed"
    fi
}

# --- 2. a refresh throws away anything that is not main ------------------------------------------
case2() {
    fixture c2
    local src="$app_home/src"
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    echo planted >>"$src/scripts/build-app.sh"
    echo planted >"$src/new-file"
    mkdir -p "$src/ui/node_modules" && echo kept >"$src/ui/node_modules/kept"
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    cmp -s "$src/scripts/build-app.sh" "$repo_root/scripts/build-app.sh" \
        && ok "refresh: a tracked edit is reverted" || fail "refresh: tracked edit" "build-app.sh still differs"
    [ ! -e "$src/new-file" ] && ok "refresh: an untracked file is removed" || fail "refresh: untracked" "new-file survived"
    [ -f "$src/ui/node_modules/kept" ] && ok "refresh: ignored build state (node_modules) is kept" \
        || fail "refresh: ignored files" "node_modules was cleaned"
}

# --- 3. app_clone_check: exactly origin/main, or nothing -------------------------------------------
case3() {
    fixture c3
    local src="$app_home/src"
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    app_clone_check "$src" 2>/dev/null && ok "check: a fresh clone at main passes" || fail "check: fresh" "refused"
    echo x >"$src/stray"
    app_clone_check "$src" 2>/dev/null && fail "check: untracked" "passed with an untracked file" \
        || ok "check: an untracked file refuses"
    rm "$src/stray"
    git -C "$src" checkout -q --detach HEAD
    land later
    git -C "$src" fetch -q origin "$APP_UPDATE_REFSPEC"
    app_clone_check "$src" 2>/dev/null && fail "check: behind main" "passed at an old commit" \
        || ok "check: HEAD behind origin/main refuses"
}

# --- 4. build-app.sh builds only in the clone, at main, and installs ----------------------------
case4() {
    fixture c4
    local src="$app_home/src" dest="$work/c4/installed" out
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    # From the checkout (with main's builder checked out there too): refused, nothing installed.
    git -C "$checkout" checkout -q main 2>/dev/null
    if out="$(build "$app_home" "$dest" "$checkout/scripts/build-app.sh" 2>&1)"; then
        fail "builder: from a checkout" "it built: $out"
    else
        case "$out" in
            *"not the app's clean clone"*JKB_APP_FROM_CHECKOUT=1*) ok "builder: refuses a checkout, naming the deliberate opt-in" ;;
            *) fail "builder: checkout refusal" "$out" ;;
        esac
    fi
    [ ! -e "$dest" ] && ok "builder: the refused run installed nothing" || fail "builder: refused run" "$dest exists"
    # The clone, dirty: refused.
    echo x >"$src/stray"
    build "$app_home" "$dest" >/dev/null 2>&1 && fail "builder: dirty clone" "it built" \
        || ok "builder: a clone with a stray file refuses"
    rm "$src/stray"
    # The clone at main: built with the frozen lockfile, installed, stamped.
    : >"$stub/log"
    if ! out="$(STUB_ID=first build "$app_home" "$dest" 2>&1)"; then fail "builder: clean clone" "$out"; return; fi
    grep -qx 'install --frozen-lockfile' "$stub/log" && ok "builder: installs with --frozen-lockfile" \
        || fail "builder: frozen lockfile" "$(cat "$stub/log")"
    [ "$(cat "$dest/id" 2>/dev/null)" = first ] && ok "builder: the packaged app is installed at --dest" \
        || fail "builder: installed" "no app at $dest"
    [ "$(app_installed_commit "$app_home")" = "$(git -C "$seed" rev-parse main)" ] \
        && ok "builder: stamps the commit it installed" || fail "builder: stamp" "$(cat "$app_home/installed" 2>&1)"
    if [ "$(uname -s)" = Linux ]; then
        grep -qF "Exec=\"$dest/code-factory\"" "$work/xdg/applications/jkb-code-factory.desktop" 2>/dev/null \
            && ok "builder: writes a desktop entry for the installed app" \
            || fail "builder: desktop entry" "$(cat "$work/xdg/applications/jkb-code-factory.desktop" 2>&1)"
    fi
    # A second install keeps the first as the previous copy.
    land two
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    STUB_ID=second build "$app_home" "$dest" >/dev/null 2>&1
    [ "$(cat "$dest/id" 2>/dev/null)" = second ] && [ "$(cat "$app_home/previous/id" 2>/dev/null)" = first ] \
        && ok "builder: the replaced app is kept as previous" \
        || fail "builder: second install" "dest=$(cat "$dest/id" 2>&1) previous=$(cat "$app_home/previous/id" 2>&1)"
    # A failed build changes nothing.
    land three
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    STUB_FAIL_BUILD=1 STUB_ID=third build "$app_home" "$dest" >/dev/null 2>&1 && fail "builder: failed build" "exit 0"
    [ "$(cat "$dest/id" 2>/dev/null)" = second ] && [ "$(app_installed_commit "$app_home")" != "$(git -C "$seed" rev-parse main)" ] \
        && ok "builder: a failed build leaves the app and the stamp as they were" \
        || fail "builder: failed build" "dest=$(cat "$dest/id" 2>&1) stamp=$(app_installed_commit "$app_home")"
}

# --- 5. app_swap: a failure leaves the installed app in place ------------------------------------
case5() {
    local d="$work/c5"
    mkdir -p "$d/old" && echo old >"$d/old/id"
    cp -R "$d/old" "$d/dest"
    app_swap "$d/missing" "$d/dest" "$d/prev" 2>/dev/null && fail "swap: missing build" "exit 0"
    [ "$(cat "$d/dest/id")" = old ] && [ ! -e "$d/prev" ] && ok "swap: nothing to install leaves dest alone" \
        || fail "swap: missing build" "dest or prev changed"
    mkdir -p "$d/new" && echo new >"$d/new/id"
    app_swap "$d/new" "$d/dest" "$d/prev" 2>/dev/null
    [ "$(cat "$d/dest/id")" = new ] && [ "$(cat "$d/prev/id")" = old ] && [ -z "$(ls -d "$d"/dest.new.* 2>/dev/null)" ] \
        && ok "swap: installs, keeps the old one as previous, leaves no temp copy" \
        || fail "swap: install" "dest=$(cat "$d/dest/id") prev=$(cat "$d/prev/id" 2>&1)"
}

# --- 6. setup.sh's step: install_app and its states ----------------------------------------------
case6() {
    fixture c6
    local h="$work/c6/home"
    mkdir -p "$h"
    app_state=""
    (HOME="$h" PNPM_HOME="$stub" XDG_DATA_HOME="$work/xdg6" install_app "$checkout" "$app_home" >/dev/null 2>&1; echo "$app_state" >"$work/c6/state")
    [ "$(cat "$work/c6/state")" = installed ] && ok "install_app: a first run installs" || fail "install_app: first" "$(cat "$work/c6/state")"
    (HOME="$h" PNPM_HOME="$stub" install_app "$checkout" "$app_home" >/dev/null 2>&1; echo "$app_state" >"$work/c6/state")
    [ "$(cat "$work/c6/state")" = unchanged ] && ok "install_app: main's tip already installed is unchanged" \
        || fail "install_app: unchanged" "$(cat "$work/c6/state")"
    land two
    (HOME="$h" PNPM_HOME="$stub" XDG_DATA_HOME="$work/xdg6" install_app "$checkout" "$app_home" >/dev/null 2>&1; echo "$app_state" >"$work/c6/state")
    [ "$(cat "$work/c6/state")" = installed ] && ok "install_app: a new main is built again" || fail "install_app: new main" "$(cat "$work/c6/state")"

    NO_BUILDER=1 fixture c6b
    (HOME="$h" install_app "$checkout" "$app_home" >/dev/null 2>&1; echo "$app_state" >"$work/c6/state")
    [ "$(cat "$work/c6/state")" = no-builder ] && ok "install_app: a main without build-app.sh says so" \
        || fail "install_app: no builder" "$(cat "$work/c6/state")"

    fixture c6c
    git -C "$checkout" remote remove origin
    (HOME="$h" install_app "$checkout" "$app_home" >/dev/null 2>&1; echo "$app_state" >"$work/c6/state")
    [ "$(cat "$work/c6/state")" = failed ] && ok "install_app: no origin to clone is a failure" \
        || fail "install_app: no origin" "$(cat "$work/c6/state")"
    # Every state install_app sets has a summary arm.
    local s out
    for s in installed unchanged no-builder skipped failed; do
        out="$(printf 'app=%s /x\n' "$s" | render_setup_summary 2>&1)"
        case "$out" in
            *unrecognised*|"") fail "summary: app=$s" "$out" ;;
            *) ok "summary: app=$s renders" ;;
        esac
    done
}

run_cases case1 case2 case3 case4 case5 case6
finish
