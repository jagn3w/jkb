#!/usr/bin/env bash
# Code Factory's installed copy (docs/code-factory.md, D53.3): the clean clone setup.sh keeps at
# origin/main (lib.sh's app_clone_refresh / app_clone_check), the builder that stages a build and
# refuses any tree but that clone (scripts/build-app.sh), the one step that swaps a staged copy in
# while nothing runs (scripts/install-app.sh, over lib.sh's app_swap), setup.sh's step
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
# Where the account's home is, for app_account_home: the tests' own, never the real one.
export JKB_APP_ACCOUNT_HOME="$work/home"

os="$(uname -s)"
# What electron-builder leaves on this platform, relative to ui/, and its executable inside it.
if [ "$os" = Darwin ]; then product="app/dist/mac-arm64/Code Factory.app"; else product="app/dist/linux-unpacked"; fi
product_exe="$(app_executable "$os" "$product")"

# A stub pnpm: logs its arguments and the repository selection / Electron variables it was given,
# `run build` leaves app/out/ as electron-vite would, and `run package` leaves a packaged app holding
# $STUB_ID, out/commit (electron-builder packs out/**), and an executable at the platform's place
# that, when run, appends "ran" to $STUB_RAN.
stub="$work/stub"
mkdir -p "$stub"
cat >"$stub/pnpm" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >>"$stub/log"
printf 'GIT_DIR=%s GIT_INDEX_FILE=%s ELECTRON_RUN_AS_NODE=%s\n' "\${GIT_DIR:-}" "\${GIT_INDEX_FILE:-}" "\${ELECTRON_RUN_AS_NODE:-}" >>"$stub/env"
case "\$*" in
    *"run package"*)
        mkdir -p "$product" "\$(dirname "$product_exe")" && printf '%s\n' "\${STUB_ID:-x}" >"$product/id" \\
            && { [ ! -f app/out/commit ] || cp app/out/commit "$product/commit"; } \\
            && printf '#!/bin/sh\necho ran >>"\${STUB_RAN:-/dev/null}"\n' >"$product_exe" && chmod +x "$product_exe" \\
            && { [ -z "\${STUB_UNREADABLE:-}" ] || { echo secret >"$product/secret" && chmod 000 "$product/secret"; }; } ;;
    *"run build"*) [ -z "\${STUB_FAIL_BUILD:-}" ] || exit 1; mkdir -p app/out
                   [ -z "\${STUB_HANG:-}" ] || { : >"\$STUB_HANG"; sleep 30; } ;;
esac
exit 0
EOF
chmod +x "$stub/pnpm"

# fixture <name> — sets $seed (where main is authored), $origin (bare), $checkout (a clone of origin
# on a feature branch with a commit main does not have), $app_home. main carries the real
# build-app.sh, install-app.sh and lib.sh unless NO_BUILDER is set.
fixture() {
    local d="$work/$1"
    seed="$d/seed" origin="$d/origin.git" checkout="$d/checkout" app_home="$d/app-home"
    git_q init -q -b main "$seed"
    mkdir -p "$seed/ui" "$seed/scripts"
    printf 'node_modules/\ndist/\nout/\n' >"$seed/ui/.gitignore"
    if [ -z "${NO_BUILDER:-}" ]; then
        cp "$repo_root/scripts/build-app.sh" "$repo_root/scripts/install-app.sh" "$repo_root/scripts/lib.sh" "$seed/scripts/"
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

# build <app-home> [flag…] — run the clone's build-app.sh with the stub pnpm (a test app home, which
# only JKB_APP_BUILD_TEST=1 allows).
build() {
    local home="$1"
    shift
    JKB_APP_BUILD_TEST=1 PNPM_HOME="$stub" /bin/bash "$home/src/scripts/build-app.sh" --app-home "$home" "$@"
}

# install <app-home> <dest> [flag…] — run the clone's install-app.sh at <dest>.
install() {
    local home="$1" dest="$2"
    shift 2
    JKB_APP_BUILD_TEST=1 XDG_DATA_HOME="$work/xdg" /bin/bash "$home/src/scripts/install-app.sh" --app-home "$home" --dest "$dest" "$@"
}

# run_as_app <dest> — start a process whose command line is the app's executable at <dest> (a copy
# of `sleep`), and set $app_pid; fails the case's premise loudly if ps does not show it.
run_as_app() {
    local exe
    exe="$(app_executable "$os" "$1")"
    mkdir -p "$(dirname "$exe")"
    rm -f "$exe" && cp "$(command -v sleep)" "$exe"
    "$exe" 30 & app_pid=$!
    sleep 0.2
    app_running "$os" "$1" || fail "premise: a copy runs from $1" "ps does not show $exe"
}

stop_app() { kill "$app_pid" 2>/dev/null; wait "$app_pid" 2>/dev/null; }

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


# --- 4. build-app.sh builds only in the clone, at main, and only stages --------------------------
case4() {
    fixture c4
    local src="$app_home/src" out rc main
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    # From the checkout (with main's builder checked out there too): refused, nothing staged.
    git -C "$checkout" checkout -q main 2>/dev/null
    if out="$(JKB_APP_BUILD_TEST=1 PNPM_HOME="$stub" /bin/bash "$checkout/scripts/build-app.sh" --app-home "$app_home" 2>&1)"; then
        fail "builder: from a checkout" "it built: $out"
    else
        case "$out" in
            *"not the app's clean clone"*JKB_APP_FROM_CHECKOUT=1*) ok "builder: refuses a checkout, naming the deliberate opt-in" ;;
            *) fail "builder: checkout refusal" "$out" ;;
        esac
    fi
    [ ! -e "$app_home/staged" ] && ok "builder: the refused run staged nothing" || fail "builder: refused run" "staged exists"
    # A test app home without the tests' variable: refused (the app runs only from the default).
    out="$(PNPM_HOME="$stub" /bin/bash "$src/scripts/build-app.sh" --app-home "$app_home" 2>&1)"; rc=$?
    [ "$rc" = 2 ] && ok "builder: an app home other than the default is refused outside the tests" \
        || fail "builder: --app-home gate" "rc=$rc $out"
    # The clone, dirty: refused.
    echo x >"$src/stray"
    build "$app_home" >/dev/null 2>&1 && fail "builder: dirty clone" "it built" \
        || ok "builder: a clone with a stray file refuses"
    rm "$src/stray"
    # The clone at main: built with the frozen lockfile and staged, with the caller's GIT_DIR and
    # ELECTRON_* reaching no step.
    : >"$stub/log"; : >"$stub/env"
    if ! out="$(GIT_DIR="$work/elsewhere.git" GIT_INDEX_FILE="$work/elsewhere.index" ELECTRON_RUN_AS_NODE=1 \
                STUB_ID=first build "$app_home" 2>&1)"; then fail "builder: clean clone" "$out"; return; fi
    if [ -s "$stub/env" ] && ! grep -qv '^GIT_DIR= GIT_INDEX_FILE= ELECTRON_RUN_AS_NODE=$' "$stub/env"; then
        ok "builder: pnpm runs without the caller's GIT_DIR, GIT_INDEX_FILE or ELECTRON_*"
    else
        fail "builder: environment scrub" "$(cat "$stub/env")"
    fi
    grep -qx 'install --frozen-lockfile' "$stub/log" && ok "builder: installs with --frozen-lockfile" \
        || fail "builder: frozen lockfile" "$(cat "$stub/log")"
    main="$(git -C "$seed" rev-parse main)"
    [ "$(cat "$app_home/staged/app/id" 2>/dev/null)" = first ] && [ "$(app_staged_commit "$app_home")" = "$main" ] \
        && ok "builder: stages the packaged app and its commit" \
        || fail "builder: staged" "$(ls -R "$app_home/staged" 2>&1)"
    [ "$(cat "$app_home/staged/app/commit" 2>/dev/null)" = "$main" ] \
        && ok "builder: the commit is built into the app (out/commit)" \
        || fail "builder: built-in commit" "$(cat "$app_home/staged/app/commit" 2>&1)"
    [ ! -e "$app_home/installed" ] && [ ! -e "$app_home/app" ] && ok "builder: installs nothing" \
        || fail "builder: installs nothing" "$(ls "$app_home")"
    [ ! -e "$app_home/lock" ] && ok "builder: releases its lock" || fail "builder: lock" "left behind"
    # A failed build leaves the last staged copy as it was.
    land two
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    STUB_FAIL_BUILD=1 build "$app_home" >/dev/null 2>&1 && fail "builder: failed build" "exit 0"
    [ "$(app_staged_commit "$app_home")" = "$main" ] && ok "builder: a failed build leaves the staged copy alone" \
        || fail "builder: failed build" "staged=$(app_staged_commit "$app_home")"
    # A copy into staging that fails (here an unreadable file; a full disk in life) keeps the last
    # staged copy whole, and leaves no half-made one.
    STUB_UNREADABLE=1 build "$app_home" >/dev/null 2>&1 && fail "builder: failed copy" "exit 0"
    [ "$(app_staged_commit "$app_home")" = "$main" ] && [ "$(cat "$app_home/staged/app/id" 2>/dev/null)" = first ] \
        && [ ! -e "$app_home/staged.new" ] && [ ! -e "$app_home/lock" ] \
        && ok "builder: a failed copy into staging keeps the last staged copy, and unlocks" \
        || fail "builder: failed copy" "staged=$(app_staged_commit "$app_home") $(ls "$app_home")"
    chmod -R u+rw "$src/ui/app/dist" 2>/dev/null
    # --update-to: the app's path. A commit that is no longer main's tip is refused and nothing moves.
    land three
    local head_before
    head_before="$(git -C "$src" rev-parse HEAD)"
    out="$(build "$app_home" --update-to "$main" 2>&1)"; rc=$?
    case "$rc:$out" in
        1:*"origin/main moved"*) [ "$(git -C "$src" rev-parse HEAD)" = "$head_before" ] \
            && ok "builder: --update-to a commit main has moved past is refused, the clone unmoved" \
            || fail "builder: --update-to stale" "the clone moved" ;;
        *) fail "builder: --update-to stale" "rc=$rc $out" ;;
    esac
    # main's tip: fetched, the clone moved and cleaned, and built by that commit's builder.
    echo planted >"$src/stray"
    main="$(git -C "$seed" rev-parse main)"
    out="$(STUB_ID=third build "$app_home" --update-to "$main" 2>&1)"; rc=$?
    [ "$rc" = 0 ] && [ "$(git -C "$src" rev-parse HEAD)" = "$main" ] && [ ! -e "$src/stray" ] \
        && [ "$(app_staged_commit "$app_home")" = "$main" ] && [ ! -e "$app_home/lock" ] \
        && ok "builder: --update-to main's tip moves the clone there, cleans it, stages it, and unlocks" \
        || fail "builder: --update-to" "rc=$rc head=$(git -C "$src" rev-parse HEAD) staged=$(app_staged_commit "$app_home") $out"
    # Another build or install holds the lock: 75, and nothing runs. No stale-lock breaking: a lock
    # whose pid is gone is reported with its path, not broken.
    local dead
    sh -c 'exit 0' & dead=$!; wait "$dead"
    mkdir -p "$app_home/lock" && echo "$dead" >"$app_home/lock/pid" && echo theirs >"$app_home/lock/token"
    : >"$stub/log"
    out="$(build "$app_home" 2>&1)"; rc=$?
    [ "$rc" = "$APP_EXIT_BUSY" ] && [ ! -s "$stub/log" ] && [ "$(cat "$app_home/lock/token")" = theirs ] \
        && case "$out" in *"$app_home/lock"*"remove it"*) true ;; *) false ;; esac \
        && ok "builder: a held lock (even a dead holder's) is busy (75), names its path, and is left alone" \
        || fail "builder: lock held" "rc=$rc $out"
    # The holder's own step proceeds under its token.
    JKB_APP_LOCK_TOKEN=theirs STUB_ID=fourth build "$app_home" >/dev/null 2>&1 \
        && [ "$(cat "$app_home/staged/app/id")" = fourth ] && [ "$(cat "$app_home/lock/token")" = theirs ] \
        && ok "builder: under its holder's token it builds, and leaves the holder's lock" \
        || fail "builder: holder's token" "$(cat "$app_home/staged/app/id" 2>&1)"
    rm -rf "$app_home/lock"
}

# --- 5. install-app.sh: the one swap, only while nothing runs --------------------------------------
case5() {
    fixture c5
    local src="$app_home/src" out rc main
    # A long destination, past BSD ps's default width (finding: `ps` without -ww cut it).
    local dest="$work/c5/a-destination-directory-whose-path-is-long-enough-to-overflow-eighty-columns/app"
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    main="$(git -C "$seed" rev-parse main)"
    out="$(install "$app_home" "$dest" 2>&1)"; rc=$?
    [ "$rc" = 1 ] && [ ! -e "$dest" ] && ok "install: nothing staged is a failure, and installs nothing" \
        || fail "install: nothing staged" "rc=$rc $out"
    out="$(PNPM_HOME="$stub" /bin/bash "$src/scripts/install-app.sh" --app-home "$(app_default_home)" --dest "$dest" 2>&1)"; rc=$?
    [ "$rc" = 2 ] && ok "install: --dest outside the tests is refused" || fail "install: --dest gate" "rc=$rc $out"
    STUB_ID=first build "$app_home" >/dev/null 2>&1
    if ! out="$(install "$app_home" "$dest" 2>&1)"; then fail "install: first" "$out"; return; fi
    [ "$(cat "$dest/id" 2>/dev/null)" = first ] && [ "$(app_installed_commit "$app_home")" = "$main" ] \
        && [ "$(app_installed_dest "$app_home")" = "$dest" ] && [ ! -e "$app_home/staged" ] && [ ! -e "$app_home/lock" ] \
        && ok "install: swaps the staged app in, stamps commit and dest, removes the staged copy, unlocks" \
        || fail "install: first" "id=$(cat "$dest/id" 2>&1) stamp=$(cat "$app_home/installed" 2>&1)"
    [ "$(cat "$app_home/install.result" 2>/dev/null)" = "$(printf 'status=0\ncommit=%s' "$main")" ] \
        && ok "install: records its outcome for the app (install.result)" \
        || fail "install: result" "$(cat "$app_home/install.result" 2>&1)"
    if [ "$os" = Linux ]; then
        grep -qF "Exec=\"$dest/code-factory\"" "$work/xdg/applications/jkb-code-factory.desktop" 2>/dev/null \
            && ok "install: writes a desktop entry for the installed app" \
            || fail "install: desktop entry" "$(cat "$work/xdg/applications/jkb-code-factory.desktop" 2>&1)"
    fi
    # A copy runs from dest: nothing is swapped, the staged copy is kept, and nothing is relaunched.
    land two
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    STUB_ID=second build "$app_home" >/dev/null 2>&1
    run_as_app "$dest"
    : >"$work/c5/ran"
    out="$(STUB_RAN="$work/c5/ran" install "$app_home" "$dest" --relaunch 2>&1)"; rc=$?
    stop_app
    [ "$rc" = "$APP_EXIT_RUNNING" ] && [ ! -s "$work/c5/ran" ] && [ "$(app_staged_commit "$app_home")" = "$(git -C "$seed" rev-parse main)" ] \
        && [ "$(app_installed_commit "$app_home")" = "$main" ] \
        && ok "install: a running copy (long path) is not swapped under; the staged copy is kept" \
        || fail "install: running" "rc=$rc ran=$(cat "$work/c5/ran") $out"
    # The app's way: started while the app runs, it waits for that process to exit, then swaps and
    # relaunches the new one.
    main="$(git -C "$seed" rev-parse main)"
    run_as_app "$dest"
    STUB_RAN="$work/c5/ran" install "$app_home" "$dest" --wait-pid "$app_pid" --relaunch >"$work/c5/wait.out" 2>&1 &
    local installer=$!
    sleep 0.5
    [ "$(cat "$dest/id")" = first ] && ok "install: --wait-pid waits while the app runs" \
        || fail "install: --wait-pid" "swapped while the app ran"
    stop_app
    wait "$installer"; rc=$?
    sleep 0.3
    [ "$rc" = 0 ] && [ "$(cat "$dest/id")" = second ] && [ "$(app_installed_commit "$app_home")" = "$main" ] \
        && grep -qx ran "$work/c5/ran" \
        && ok "install: once the app has exited it swaps, stamps and relaunches the new copy" \
        || fail "install: after wait" "rc=$rc id=$(cat "$dest/id") ran=$(cat "$work/c5/ran") $(cat "$work/c5/wait.out")"
    # A stamp that cannot be written: refused before anything moves; staged copy kept.
    land three
    app_clone_refresh "$checkout" "$src" 2>/dev/null
    STUB_ID=third build "$app_home" >/dev/null 2>&1
    mkdir -p "$app_home/installed.tmp/x"
    out="$(install "$app_home" "$dest" 2>&1)"; rc=$?
    rm -rf "$app_home/installed.tmp"
    [ "$rc" = 1 ] && [ "$(cat "$dest/id")" = second ] && [ -n "$(app_staged_commit "$app_home")" ] \
        && ok "install: a stamp it cannot write refuses the swap, keeping the app and the staged copy" \
        || fail "install: unwritable stamp" "rc=$rc id=$(cat "$dest/id") $out"
    # A held lock: 75, nothing swapped, the lock's path and holder named, and NOT relaunched — another
    # install is at work.
    mkdir -p "$app_home/lock" && echo 4242 >"$app_home/lock/pid" && echo theirs >"$app_home/lock/token"
    : >"$work/c5/ran"
    out="$(STUB_RAN="$work/c5/ran" install "$app_home" "$dest" --relaunch 2>&1)"; rc=$?
    rm -rf "$app_home/lock"
    sleep 0.3
    case "$out" in *"$app_home/lock"*"pid 4242"*"remove it"*) local named=1 ;; *) local named=0 ;; esac
    [ "$rc" = "$APP_EXIT_BUSY" ] && [ "$(cat "$dest/id")" = second ] && [ "$named" = 1 ] && [ ! -s "$work/c5/ran" ] \
        && ok "install: a held lock is busy (75): nothing swapped, the lock and holder named, no relaunch" \
        || fail "install: lock" "rc=$rc ran=$(cat "$work/c5/ran") $out"
    # A `set -e` failure after the lock is taken (stdout closed: the first echo fails) releases it,
    # through app_lock's EXIT trap, and records the failure.
    out="$(install "$app_home" "$dest" 2>&1 >&-)"; rc=$?
    [ "$rc" != 0 ] && [ ! -e "$app_home/lock" ] && [ "$(cat "$dest/id")" = second ] \
        && grep -qx "status=$rc" "$app_home/install.result" \
        && ok "install: a set -e failure after taking the lock releases it and records the failure" \
        || fail "install: set -e" "rc=$rc lock=$(ls "$app_home/lock" 2>&1) $(cat "$app_home/install.result" 2>&1) $out"
}

# --- 6. app_swap: only over jkb's app, and a failure leaves the installed app in place -------------
case6() {
    local d="$work/c6" h="$work/c6/home"
    mkdir -p "$d/old" "$h" && echo old >"$d/old/id"
    cp -R "$d/old" "$d/dest"
    # An app at dest that jkb's stamp does not name is not jkb's: refused, and nothing is moved.
    mkdir -p "$d/new" && echo new >"$d/new/id"
    app_swap "$d/new" "$d/dest" "$h" 2>/dev/null && fail "swap: foreign dest" "exit 0"
    [ "$(cat "$d/dest/id")" = old ] && [ ! -e "$h/previous" ] && [ ! -e "$h/installed" ] && ok "swap: an app jkb did not install is left alone" \
        || fail "swap: foreign dest" "dest=$(cat "$d/dest/id") home=$(ls "$h" 2>&1)"
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
    # A copy started while the new one was being copied (the Dock, a relaunch): app_swap checks again
    # right before moving the old bundle aside, refuses with 76, and leaves no temp copy.
    run_as_app "$d/dest"
    local swap_rc=0
    app_swap "$d/newer" "$d/dest" "$h" 2>/dev/null || swap_rc=$?
    stop_app
    rm -f "$(app_executable "$os" "$d/dest")"
    [ "$swap_rc" = "$APP_EXIT_RUNNING" ] && [ "$(cat "$d/dest/id")" = new ] && [ -z "$(ls -d "$d"/dest.new.* 2>/dev/null)" ] \
        && ok "swap: a copy running from dest when it comes to move it aside refuses (76)" \
        || fail "swap: running re-check" "rc=$swap_rc id=$(cat "$d/dest/id")"
    # A helper process counts: on macOS they run from Contents/Frameworks, not the main executable.
    local helper="$d/dest/Contents/Frameworks/Code Factory Helper (GPU).app/Contents/MacOS/Code Factory Helper (GPU)"
    mkdir -p "$(dirname "$helper")" && cp "$(command -v sleep)" "$helper"
    "$helper" 30 & app_pid=$!
    sleep 0.2
    app_running "$os" "$d/dest" && ok "running: a helper process anywhere in the bundle counts" \
        || fail "running: helper" "not seen"
    stop_app
    rm -rf "$d/dest/Contents"
    # A first install vouches for its dest BEFORE anything moves: if the final stamp then cannot be
    # written, the next install still replaces it rather than refusing it as foreign.
    local h2="$d/home2"
    mkdir -p "$h2"
    app_swap "$d/new" "$d/dest2" "$h2" 2>/dev/null
    [ "$(app_installed_dest "$h2")" = "$d/dest2" ] && [ -z "$(app_installed_commit "$h2")" ] \
        && app_swap "$d/newer" "$d/dest2" "$h2" 2>/dev/null && [ "$(cat "$d/dest2/id")" = newer ] \
        && ok "swap: the stamp names dest before the swap, so an unstamped first install is still jkb's" \
        || fail "swap: pre-stamp" "stamp=$(cat "$h2/installed" 2>&1) id=$(cat "$d/dest2/id" 2>&1)"
}

# --- 7. setup.sh's step: install_app and its states ----------------------------------------------
case7() {
    fixture c7
    local h="$work/c7/home" dest
    mkdir -p "$h"
    dest="$(app_default_dest "$os" "$h" "$app_home")"
    step() {
        (HOME="$h" JKB_APP_ACCOUNT_HOME="$h" JKB_APP_BUILD_TEST=1 PNPM_HOME="$stub" XDG_DATA_HOME="$work/xdg7" install_app "$checkout" "$app_home" >/dev/null 2>&1
         echo "$app_state" >"$work/c7/state")
        cat "$work/c7/state"
    }
    [ "$(step)" = installed ] && [ -d "$dest" ] && ok "install_app: a first run builds and installs" || fail "install_app: first" "$(cat "$work/c7/state")"
    [ "$(step)" = unchanged ] && ok "install_app: main's tip already installed is unchanged" \
        || fail "install_app: unchanged" "$(cat "$work/c7/state")"
    land two
    [ "$(step)" = installed ] && ok "install_app: a new main is built and installed again" || fail "install_app: new main" "$(cat "$work/c7/state")"
    # The stamp names the tip but the app is gone: installed again, not `unchanged` forever.
    rm -rf "$dest"
    [ "$(step)" = installed ] && [ -d "$dest" ] && ok "install_app: a stamped tip whose app was removed is installed again" \
        || fail "install_app: removed app" "$(cat "$work/c7/state")"
    # A copy is running: nothing is built or swapped, and the state says to quit it.
    land three
    run_as_app "$dest"
    : >"$stub/log"
    local state
    state="$(step)"
    stop_app
    [ "$state" = running ] && [ ! -s "$stub/log" ] && [ "$(app_installed_commit "$app_home")" != "$(git -C "$seed" rev-parse main)" ] \
        && ok "install_app: while the app runs nothing is built or swapped" \
        || fail "install_app: running" "$state; built: $(cat "$stub/log")"
    # Another build or install holds the lock: busy, and the clone is not touched.
    mkdir -p "$app_home/lock" && echo 1 >"$app_home/lock/pid" && echo other >"$app_home/lock/token"
    state="$(step)"
    [ "$state" = busy ] && [ "$(cat "$app_home/lock/token")" = other ] \
        && ok "install_app: a held lock is busy, and is left to its holder" \
        || fail "install_app: busy" "$state"
    rm -rf "$app_home/lock"
    [ "$(step)" = installed ] && [ ! -e "$app_home/lock" ] \
        && ok "install_app: installs once nothing runs or holds the lock, and releases it" \
        || fail "install_app: after" "$(cat "$work/c7/state"); lock: $(ls "$app_home/lock" 2>&1)"

    # Ctrl-C during the build: the whole foreground group gets SIGINT, and the lock is released by
    # app_lock's own trap in install_app's subshell (job control on, so the job takes SIGINT as an
    # interactive one would).
    land four
    rm -f "$work/c7/hang"
    set -m
    (HOME="$h" JKB_APP_ACCOUNT_HOME="$h" JKB_APP_BUILD_TEST=1 PNPM_HOME="$stub" STUB_HANG="$work/c7/hang" \
        install_app "$checkout" "$app_home" >/dev/null 2>&1) &
    local job=$!
    set +m
    local i=0
    while [ ! -e "$work/c7/hang" ] && [ "$i" -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
    [ -d "$app_home/lock" ] || fail "premise: the build holds the lock" "no lock while building"
    kill -INT -- "-$job" 2>/dev/null
    wait "$job" 2>/dev/null
    [ -e "$work/c7/hang" ] && [ ! -e "$app_home/lock" ] \
        && ok "install_app: Ctrl-C mid-build releases the lock" \
        || fail "install_app: SIGINT" "hang=$(ls "$work/c7/hang" 2>&1) lock=$(ls "$app_home/lock" 2>&1)"
    [ "$(step)" = installed ] && ok "install_app: and the next run installs" || fail "install_app: after SIGINT" "$(cat "$work/c7/state")"

    NO_BUILDER=1 fixture c7b
    [ "$(step)" = no-builder ] && ok "install_app: a main without the builder says so" \
        || fail "install_app: no builder" "$(cat "$work/c7/state")"

    fixture c7c
    git -C "$checkout" remote remove origin
    [ "$(step)" = failed ] && ok "install_app: no origin to clone is a failure" \
        || fail "install_app: no origin" "$(cat "$work/c7/state")"
    # Every state install_app sets has a summary arm.
    local s out
    for s in installed unchanged running busy no-builder skipped failed; do
        out="$(printf 'app=%s /x\n' "$s" | render_setup_summary 2>&1)"
        case "$out" in
            *unrecognised*|"") fail "summary: app=$s" "$out" ;;
            *) ok "summary: app=$s renders" ;;
        esac
    done
}

# --- 8. one home: the account's, from the user database, not $HOME -------------------------------
case8() {
    local expect u
    u="$(id -un)"
    if command -v getent >/dev/null 2>&1; then
        expect="$(getent passwd "$u" | cut -d: -f6)"
    else
        expect="$(dscl . -read "/Users/$u" NFSHomeDirectory 2>/dev/null | sed 's/^NFSHomeDirectory: //')"
    fi
    [ -n "$expect" ] || { fail "premise: the account's home" "no getent or dscl"; return; }
    local got
    got="$(env -u JKB_APP_ACCOUNT_HOME -u JKB_APP_BUILD_TEST HOME="$work/not-the-home" bash -c '. "$0"; app_account_home; app_default_home' "$repo_root/scripts/lib.sh")"
    [ "$got" = "$(printf '%s\n%s' "$expect" "$expect/.local/share/jkb-app")" ] \
        && ok "home: with HOME elsewhere, the app home is under the account's home ($expect)" \
        || fail "home: account" "got $got, want $expect"
    got="$(env -u JKB_APP_BUILD_TEST HOME="$work/not-the-home" JKB_APP_ACCOUNT_HOME=/elsewhere bash -c '. "$0"; app_account_home' "$repo_root/scripts/lib.sh")"
    [ "$got" = "$expect" ] && ok "home: the tests' override is ignored outside the tests" \
        || fail "home: override" "got $got"
}

run_cases case1 case2 case3 case4 case5 case6 case7 case8
finish
