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

# A stub pnpm: logs its arguments and the repository selection / Electron variables it was given,
# `run build` leaves app/out/ as electron-vite would, and `run package` leaves a packaged app holding
# $STUB_ID and, as electron-builder packs out/**, out/commit.
stub="$work/stub"
mkdir -p "$stub"
cat >"$stub/pnpm" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >>"$stub/log"
printf 'GIT_DIR=%s GIT_INDEX_FILE=%s ELECTRON_RUN_AS_NODE=%s\n' "\${GIT_DIR:-}" "\${GIT_INDEX_FILE:-}" "\${ELECTRON_RUN_AS_NODE:-}" >>"$stub/env"
case "\$*" in
    *"run package"*) mkdir -p "$product" && printf '%s\n' "\${STUB_ID:-x}" >"$product/id" \
                     && { [ ! -f app/out/commit ] || cp app/out/commit "$product/commit"; } ;;
    *"run build"*) [ -z "\${STUB_FAIL_BUILD:-}" ] || exit 1; mkdir -p app/out ;;
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

# build <app-home> <dest> [script [flag…]] — run a build-app.sh (the clone's by default) with the stub
# pnpm, at <dest> (--dest, which only JKB_APP_BUILD_TEST=1 allows).
build() {
    local home="$1" dest="$2" script="${3:-$1/src/scripts/build-app.sh}"
    shift 3 2>/dev/null || shift $#
    JKB_APP_BUILD_TEST=1 PNPM_HOME="$stub" XDG_DATA_HOME="$work/xdg" /bin/bash "$script" --app-home "$home" --dest "$dest" "$@"
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
    : >"$stub/log"; : >"$stub/env"
    # A post-merge hook's GIT_DIR and a launching app's ELECTRON_* reach no step of the build.
    if ! out="$(GIT_DIR="$work/elsewhere.git" GIT_INDEX_FILE="$work/elsewhere.index" ELECTRON_RUN_AS_NODE=1 \
                STUB_ID=first build "$app_home" "$dest" 2>&1)"; then fail "builder: clean clone" "$out"; return; fi
    if [ -s "$stub/env" ] && ! grep -qv '^GIT_DIR= GIT_INDEX_FILE= ELECTRON_RUN_AS_NODE=$' "$stub/env"; then
        ok "builder: pnpm runs without the caller's GIT_DIR, GIT_INDEX_FILE or ELECTRON_*"
    else
        fail "builder: environment scrub" "$(cat "$stub/env")"
    fi
    grep -qx 'install --frozen-lockfile' "$stub/log" && ok "builder: installs with --frozen-lockfile" \
        || fail "builder: frozen lockfile" "$(cat "$stub/log")"
    [ "$(cat "$dest/id" 2>/dev/null)" = first ] && ok "builder: the packaged app is installed at --dest" \
        || fail "builder: installed" "no app at $dest"
    [ "$(app_installed_commit "$app_home")" = "$(git -C "$seed" rev-parse main)" ] \
        && ok "builder: stamps the commit it installed" || fail "builder: stamp" "$(cat "$app_home/installed" 2>&1)"
    [ "$(app_installed_dest "$app_home")" = "$dest" ] && ok "builder: stamps where it installed it" \
        || fail "builder: stamp dest" "$(cat "$app_home/installed" 2>&1)"
    [ "$(cat "$dest/commit" 2>/dev/null)" = "$(git -C "$seed" rev-parse main)" ] \
        && ok "builder: the commit is built into the app (out/commit)" \
        || fail "builder: built-in commit" "$(cat "$dest/commit" 2>&1)"
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
    # A stamp that cannot be written fails the build, rather than reporting an install it did not record.
    mkdir -p "$app_home/installed.tmp/x"
    out="$(STUB_ID=third build "$app_home" "$dest" 2>&1)"; local rc=$?
    case "$rc:$out" in
        "$APP_BUILD_EXIT_UNRECORDED:"*"could not be written"*)
            [ "$(cat "$dest/id" 2>/dev/null)" = third ] \
                && ok "builder: swapped but unstamped exits $APP_BUILD_EXIT_UNRECORDED, not plain failure" \
                || fail "builder: unwritable stamp" "the app was not swapped: $(cat "$dest/id" 2>&1)" ;;
        *) fail "builder: unwritable stamp" "rc=$rc $out" ;;
    esac
    rm -rf "$app_home/installed.tmp"
    # --dest is for the tests: without JKB_APP_BUILD_TEST=1 it is refused, and nothing is built.
    : >"$stub/log"
    out="$(PNPM_HOME="$stub" /bin/bash "$src/scripts/build-app.sh" --app-home "$app_home" --dest "$work/c4/other" 2>&1)"; rc=$?
    [ "$rc" = 2 ] && [ ! -s "$stub/log" ] && [ ! -e "$work/c4/other" ] \
        && ok "builder: --dest outside the tests is refused" || fail "builder: --dest gate" "rc=$rc $out"
    # A copy running from dest: nothing is swapped under it, unless the caller is that copy.
    land four
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    local pid
    cp "$(command -v sleep)" "$(app_executable "$(uname -s)" "$dest")"
    "$(app_executable "$(uname -s)" "$dest")" 30 & pid=$!
    out="$(STUB_ID=fourth build "$app_home" "$dest" 2>&1)"; rc=$?
    [ "$rc" = "$APP_BUILD_EXIT_RUNNING" ] && [ "$(cat "$dest/id")" = third ] \
        && ok "builder: a running copy at dest is not swapped under ($APP_BUILD_EXIT_RUNNING)" \
        || fail "builder: running" "rc=$rc id=$(cat "$dest/id") $out"
    out="$(STUB_ID=fourth build "$app_home" "$dest" "$src/scripts/build-app.sh" --replacing-running 2>&1)"; rc=$?
    kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    [ "$rc" = 0 ] && [ "$(cat "$dest/id")" = fourth ] \
        && ok "builder: --replacing-running (the app's own update) swaps under itself" \
        || fail "builder: --replacing-running" "rc=$rc id=$(cat "$dest/id") $out"
}

# --- 5. app_swap: a failure leaves the installed app in place ------------------------------------
case5() {
    local d="$work/c5" h="$work/c5/home"
    mkdir -p "$d/old" "$h" && echo old >"$d/old/id"
    cp -R "$d/old" "$d/dest"
    # An app at dest that jkb's stamp does not name is not jkb's: refused, and nothing is moved.
    mkdir -p "$d/new" && echo new >"$d/new/id"
    app_swap "$d/new" "$d/dest" "$h" 2>/dev/null && fail "swap: foreign dest" "exit 0"
    [ "$(cat "$d/dest/id")" = old ] && [ ! -e "$h/previous" ] && ok "swap: an app jkb did not install is left alone" \
        || fail "swap: foreign dest" "dest=$(cat "$d/dest/id") previous=$(ls "$h" 2>&1)"
    printf 'commit=%s\ndest=%s\n' "$(printf 'a%.0s' $(seq 40))" "$d/elsewhere" >"$h/installed"
    app_swap "$d/new" "$d/dest" "$h" 2>/dev/null && fail "swap: stamp names another dest" "exit 0"
    [ "$(cat "$d/dest/id")" = old ] && ok "swap: a stamp naming another place does not vouch for dest" \
        || fail "swap: stamp elsewhere" "dest=$(cat "$d/dest/id")"
    printf 'commit=%s\ndest=%s\n' "$(printf 'a%.0s' $(seq 40))" "$d/dest" >"$h/installed"
    app_swap "$d/missing" "$d/dest" "$h" 2>/dev/null && fail "swap: missing build" "exit 0"
    [ "$(cat "$d/dest/id")" = old ] && [ ! -e "$h/previous" ] && ok "swap: nothing to install leaves dest alone" \
        || fail "swap: missing build" "dest or prev changed"
    app_swap "$d/new" "$d/dest" "$h" 2>/dev/null
    [ "$(cat "$d/dest/id")" = new ] && [ "$(cat "$h/previous/id")" = old ] && [ -z "$(ls -d "$d"/dest.new.* 2>/dev/null)" ] \
        && ok "swap: installs, keeps the old one as previous, leaves no temp copy" \
        || fail "swap: install" "dest=$(cat "$d/dest/id") prev=$(cat "$h/previous/id" 2>&1)"
    # The final rename fails: the app that was installed is moved back, and no temp copy is left.
    mkdir -p "$d/newer" && echo newer >"$d/newer/id"
    (
        mv() { case "$1" in *.new.*) return 1 ;; esac; command mv "$@"; }
        app_swap "$d/newer" "$d/dest" "$h" 2>/dev/null
    ) && fail "swap: failed rename" "exit 0"
    [ "$(cat "$d/dest/id" 2>/dev/null)" = new ] && [ -z "$(ls -d "$d"/dest.new.* 2>/dev/null)" ] \
        && ok "swap: a failed rename puts the installed app back" \
        || fail "swap: rollback" "dest=$(cat "$d/dest/id" 2>&1) temp=$(ls -d "$d"/dest.new.* 2>&1)"
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
    # The stamp names the tip but the app is gone: installed again, not `unchanged` forever.
    local dest pid
    dest="$(app_default_dest "$(uname -s)" "$h" "$app_home")"
    rm -rf "$dest"
    (HOME="$h" PNPM_HOME="$stub" XDG_DATA_HOME="$work/xdg6" install_app "$checkout" "$app_home" >/dev/null 2>&1; echo "$app_state" >"$work/c6/state")
    [ "$(cat "$work/c6/state")" = installed ] && [ -d "$dest" ] && ok "install_app: a stamped tip whose app was removed is installed again" \
        || fail "install_app: removed app" "$(cat "$work/c6/state")"
    # A copy is running from dest: nothing is swapped under it.
    land three
    cp "$(command -v sleep)" "$(app_executable "$(uname -s)" "$dest")"
    "$(app_executable "$(uname -s)" "$dest")" 30 & pid=$!
    (HOME="$h" PNPM_HOME="$stub" XDG_DATA_HOME="$work/xdg6" install_app "$checkout" "$app_home" >/dev/null 2>&1; echo "$app_state" >"$work/c6/state")
    kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    [ "$(cat "$work/c6/state")" = running ] && [ "$(app_installed_commit "$app_home")" != "$(git -C "$seed" rev-parse main)" ] \
        && ok "install_app: a running app is not swapped under" \
        || fail "install_app: running" "$(cat "$work/c6/state")"
    # Another install holds the lock (a live pid): busy, and the clone is not touched.
    mkdir -p "$app_home/lock" && echo $$ >"$app_home/lock/pid" && echo other >"$app_home/lock/token"
    (HOME="$h" PNPM_HOME="$stub" XDG_DATA_HOME="$work/xdg6" install_app "$checkout" "$app_home" >/dev/null 2>&1; echo "$app_state" >"$work/c6/state")
    [ "$(cat "$work/c6/state")" = busy ] && [ "$(cat "$app_home/lock/token")" = other ] \
        && ok "install_app: a held lock is busy, and is left to its holder" \
        || fail "install_app: busy" "$(cat "$work/c6/state")"
    rm -rf "$app_home/lock"
    # The builder swapped but could not stamp: reported as such, not as a plain failure.
    mkdir -p "$app_home/installed.tmp/x"
    (HOME="$h" PNPM_HOME="$stub" XDG_DATA_HOME="$work/xdg6" install_app "$checkout" "$app_home" >/dev/null 2>&1; echo "$app_state" >"$work/c6/state")
    [ "$(cat "$work/c6/state")" = unrecorded ] && ok "install_app: swapped but unstamped is unrecorded" \
        || fail "install_app: unrecorded" "$(cat "$work/c6/state")"
    rm -rf "$app_home/installed.tmp"
    (HOME="$h" PNPM_HOME="$stub" XDG_DATA_HOME="$work/xdg6" install_app "$checkout" "$app_home" >/dev/null 2>&1; echo "$app_state" >"$work/c6/state")
    [ "$(cat "$work/c6/state")" = installed ] && [ ! -e "$app_home/lock" ] \
        && ok "install_app: installs once nothing runs or holds the lock, and releases it" \
        || fail "install_app: after" "$(cat "$work/c6/state"); lock: $(ls "$app_home/lock" 2>&1)"

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
    for s in installed unchanged running busy unrecorded no-builder skipped failed; do
        out="$(printf 'app=%s /x\n' "$s" | render_setup_summary 2>&1)"
        case "$out" in
            *unrecognised*|"") fail "summary: app=$s" "$out" ;;
            *) ok "summary: app=$s renders" ;;
        esac
    done
}

# --- 7. build-app.sh and the app lock --------------------------------------------------------------
case7() {
    fixture c7
    local src="$app_home/src" dest="$work/c7/installed" out dead
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    # Held by a live holder: refused with 75, and the build never starts.
    mkdir -p "$app_home/lock" && echo $$ >"$app_home/lock/pid" && echo theirs >"$app_home/lock/token"
    : >"$stub/log"
    out="$(build "$app_home" "$dest" 2>&1)"; local rc=$?
    [ "$rc" = "$APP_BUILD_EXIT_BUSY" ] && [ ! -s "$stub/log" ] && [ ! -e "$dest" ] && ok "lock: a held lock refuses the build (75) before it starts" \
        || fail "lock: held" "rc=$rc log=$(cat "$stub/log") $out"
    # The holder's own builder (its token) proceeds, and leaves the holder's lock alone.
    if JKB_APP_LOCK_TOKEN=theirs build "$app_home" "$dest" >/dev/null 2>&1 && [ -d "$dest" ] \
       && [ "$(cat "$app_home/lock/token" 2>/dev/null)" = theirs ]; then
        ok "lock: the holder's builder (its token) proceeds and leaves the lock to the holder"
    else
        fail "lock: holder's token" "dest=$(ls "$dest" 2>&1) lock=$(cat "$app_home/lock/token" 2>&1)"
    fi
    # The builder that recognised its holder's token recorded itself, so the lock outlives the holder.
    case "$(cat "$app_home/lock/builder" 2>/dev/null)" in
        ''|*[!0-9]*) fail "lock: builder pid" "$(ls "$app_home/lock")" ;;
        *) ok "lock: the holder's builder records its pid in the lock" ;;
    esac
    sh -c 'exit 0' & dead=$!; wait "$dead"
    # The holder died but the builder it started is still running (the app quit mid-update): live.
    sleep 30 & local builder=$!
    echo "$dead" >"$app_home/lock/pid"; echo "$builder" >"$app_home/lock/builder"
    : >"$stub/log"
    out="$(build "$app_home" "$dest" 2>&1)"; rc=$?
    kill "$builder" 2>/dev/null; wait "$builder" 2>/dev/null
    [ "$rc" = "$APP_BUILD_EXIT_BUSY" ] && [ ! -s "$stub/log" ] && [ "$(cat "$app_home/lock/token")" = theirs ] \
        && ok "lock: a dead holder whose builder still runs is busy, not stale" \
        || fail "lock: live builder" "rc=$rc $out"
    # Two runs break the same stale lock: the loser, finding the winner's fresh lock where the stale
    # one was, puts it back rather than leaving the winner unlocked.
    echo "$$" >"$app_home/lock/pid"; echo winner >"$app_home/lock/token"
    if _app_lock_break "$app_home/lock" "$dead" 2>/dev/null; then
        fail "lock: break race" "broke a lock that is no longer the dead holder's"
    elif [ "$(cat "$app_home/lock/token" 2>/dev/null)" = winner ] && [ -z "$(ls -d "$app_home"/lock.stale.* 2>/dev/null)" ]; then
        ok "lock: a lock taken since it was judged stale is put back, not left aside"
    else
        fail "lock: break race" "lock=$(cat "$app_home/lock/token" 2>&1) aside=$(ls -d "$app_home"/lock.stale.* 2>&1)"
    fi
    rm -f "$app_home/lock/builder"
    # Left by a holder that died: broken, and the run's own lock is released at its end.
    echo "$dead" >"$app_home/lock/pid"
    land two; app_clone_refresh "$checkout" "$src" 2>/dev/null
    if build "$app_home" "$dest" >/dev/null 2>&1 && [ ! -e "$app_home/lock" ] && [ -z "$(ls -d "$app_home"/lock.stale.* 2>/dev/null)" ]; then
        ok "lock: a dead holder's lock is broken, and the run releases its own"
    else
        fail "lock: stale" "$(ls -a "$app_home")"
    fi
}

run_cases case1 case2 case3 case4 case5 case6 case7
finish
