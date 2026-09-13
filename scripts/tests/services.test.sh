#!/usr/bin/env bash
# `activate_services` (scripts/lib.sh), the half of setup.sh that turns written units into running
# ones, against stub service managers.
#
# Why it exists: the activation was inline in setup.sh, which nothing can run without a cargo
# install, and review found it had drifted twice in one change — a third unit added to both label
# loops by hand, and restarted on Linux only after its unit was already running the old binary; and
# a daemon that loaded and then exited (its port taken) still reported "enabled".
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

new_workdir

# stubs <os> — a fresh bin dir of stub `jkb`, `uname`, `systemctl` and `launchctl` on PATH.
# `jkb service units` prints each of $STUB_LABELS with a path under $HOME/units (or fails with
# STUB_LABELS_FAIL=1), `jkb service token-path` a path under the stub KB, and `jkb mq topic ls` (the
# database check) fails with STUB_DB_REFUSES=1; every manager call is logged
# to $log; starting `com.jkb.serve` writes its token unless STUB_SERVE_DOWN=1, and a unit
# named in STUB_BROKEN fails to start. STUB_SERVE_DELAY (Linux) delays the token like a slow start.
stubs() {
    local os="$1"
    bin="$work/$os-$RANDOM/bin"; log="${bin%/bin}/calls.log"; db="${bin%/bin}/kb/jkb.db"
    mkdir -p "$bin" "$(dirname "$db")"; : >"$log"
    cat >"$bin/jkb" <<'EOF'
#!/usr/bin/env bash
[ "${STUB_LABELS_FAIL:-0}" = 1 ] && exit 1
case "$3 $4" in
    "service units") for l in $STUB_LABELS; do
        role=watcher; [ "$l" = com.jkb.serve ] && role=serve
        printf '%s\t%s\t%s\n' "$l" "$HOME/units/$l.unit" "$role"
    done ;;
    "mq topic") if [ "${STUB_DB_REFUSES:-0}" = 1 ]; then exit 1; fi ;;
    "service token-path") printf '%s\n' "$STUB_KB/daemon/token" ;;
esac
EOF
    printf '#!/usr/bin/env bash\necho %s\n' "$os" >"$bin/uname"
    cat >"$bin/systemctl" <<'EOF'
#!/usr/bin/env bash
echo "systemctl $*" >>"$STUB_LOG"
label="${3:-}"
case " ${STUB_BROKEN:-} " in *" $label "*) [ "$2" = restart ] && exit 1 ;; esac
if [ "$2" = restart ] && [ "$label" = com.jkb.serve ] && [ "${STUB_SERVE_DOWN:-0}" != 1 ]; then
    # Started in the background and up after STUB_SERVE_DELAY seconds, as a real daemon is.
    ( sleep "${STUB_SERVE_DELAY:-0}"; mkdir -p "$STUB_KB/daemon" && echo token >"$STUB_KB/daemon/token" ) \
        </dev/null >/dev/null 2>&1 &
fi
EOF
    cat >"$bin/launchctl" <<'EOF'
#!/usr/bin/env bash
echo "launchctl $*" >>"$STUB_LOG"
for broken in ${STUB_BROKEN:-}; do
    case "$1 $2" in "load "*"/$broken.unit") exit 1 ;; esac
done
case "$2" in *com.jkb.serve.unit)
    [ "$1" = load ] && [ "${STUB_SERVE_DOWN:-0}" != 1 ] && mkdir -p "$STUB_KB/daemon" && echo token >"$STUB_KB/daemon/token" ;;
esac
exit 0
EOF
    chmod +x "$bin"/*
    export STUB_LOG="$log" STUB_KB="$(dirname "$db")"
}

# activate — run activate_services under the stubs, in a subshell so PATH and state stay local;
# prints `<watcher_state> <serve_state>`.
activate() {
    (
        PATH="$bin:$PATH" HOME="${bin%/bin}"
        export JKB_SERVE_READY_WAIT=2
        activate_services "$db" >/dev/null 2>&1
        echo "$watcher_state $serve_state"
    )
}

# --- 1. every listed unit is enabled AND restarted, including one this file never names ------
case1() {
    stubs Linux
    local state
    state="$(STUB_LABELS="com.jkb.sync com.jkb.reap com.jkb.serve com.jkb.future" activate)"
    [ "$state" = "running up" ] && ok "all units up: running, serve up" || fail "linux: state" "got '$state'; $(cat "$log")"
    for label in com.jkb.sync com.jkb.reap com.jkb.serve com.jkb.future; do
        grep -qx "systemctl --user enable $label" "$log" && grep -qx "systemctl --user restart $label" "$log" \
            && ok "$label enabled and restarted" \
            || fail "linux: $label" "not enabled+restarted; calls: $(tr '\n' ';' <"$log")"
    done
}

# --- 2. a daemon that never comes up is a failure, not "enabled" -----------------------------
case2() {
    stubs Linux
    local state
    state="$(STUB_LABELS="com.jkb.sync com.jkb.serve" STUB_SERVE_DOWN=1 activate)"
    [ "$state" = "running failed" ] && ok "no fresh token within the wait: serve failed, the watcher still running" \
        || fail "linux: serve down" "got '$state'"
    # A token left by an EARLIER run does not count.
    stubs Linux
    mkdir -p "$STUB_KB/daemon" && echo old >"$STUB_KB/daemon/token"
    touch -t 200001010000 "$STUB_KB/daemon/token"
    state="$(STUB_LABELS="com.jkb.serve" STUB_SERVE_DOWN=1 activate)"
    [ "$state" = "running failed" ] && ok "a stale token from a previous start is not proof" \
        || fail "linux: stale token" "got '$state'"
    # ...nor a reason to stop waiting for the new one: the daemon comes up a second later.
    stubs Linux
    mkdir -p "$STUB_KB/daemon" && echo old >"$STUB_KB/daemon/token"
    touch -t 200001010000 "$STUB_KB/daemon/token"
    state="$(STUB_LABELS="com.jkb.serve" STUB_SERVE_DELAY=1 activate)"
    [ "$state" = "running up" ] && ok "a slow start behind a stale token is waited for" \
        || fail "linux: slow start" "got '$state'"
}

# --- 3. no label list: failed, and nothing is started ------------------------------------------
case3() {
    stubs Linux
    local state
    state="$(STUB_LABELS_FAIL=1 activate)"
    [ "$state" = "failed unchecked" ] && ok "units unavailable: failed, serve unchecked" || fail "labels: state" "got '$state'"
    [ ! -s "$log" ] && ok "no service manager call without a label list" \
        || fail "labels: calls" "$(cat "$log")"
}

# --- 4. one unit failing to start fails the step -----------------------------------------------
case4() {
    stubs Linux
    local state
    state="$(STUB_LABELS="com.jkb.sync com.jkb.serve" STUB_BROKEN=com.jkb.sync activate)"
    [ "$state" = "failed up" ] && ok "a watcher unit that will not restart: the watcher failed, serve still checked and up" || fail "broken: state" "got '$state'"
}

# --- 4b. the daemon's own failures are reported as the daemon's, never as the watcher's ----------
case4b() {
    stubs Linux
    local state
    state="$(STUB_LABELS="com.jkb.sync com.jkb.serve" STUB_BROKEN=com.jkb.serve activate)"
    [ "$state" = "running failed" ] && ok "a serve unit that will not restart: serve failed, the watcher running" \
        || fail "serve broken: state" "got '$state'"
    stubs Linux
    state="$(STUB_LABELS="com.jkb.sync com.jkb.serve" STUB_DB_REFUSES=1 activate)"
    [ "$state" = "running refusing" ] && ok "a daemon listening over a database it cannot open: refusing, not up" \
        || fail "serve refusing: state" "got '$state'"
}

# --- 5. launchd: every listed unit is loaded from the path jkb names ------------------------------------------------
case5() {
    stubs Darwin
    local state
    state="$(STUB_LABELS="com.jkb.sync com.jkb.serve com.jkb.future" activate)"
    [ "$state" = "running up" ] && ok "darwin: running, serve up" || fail "darwin: state" "got '$state'; $(cat "$log")"
    for label in com.jkb.sync com.jkb.serve com.jkb.future; do
        grep -qx "launchctl load ${bin%/bin}/units/$label.unit" "$log" \
            && ok "darwin: $label loaded" || fail "darwin: $label" "$(tr '\n' ';' <"$log")"
    done
    stubs Darwin
    state="$(STUB_LABELS="com.jkb.sync com.jkb.serve" STUB_BROKEN=com.jkb.serve activate)"
    [ "$state" = "running failed" ] && ok "darwin: a serve plist that will not load is serve's failure, not the watcher's" \
        || fail "darwin: serve broken" "got '$state'"
}

echo "==> activate_services: every unit, restarted, and serve proven up"
run_cases case1 case2 case3 case4 case4b case5

finish
