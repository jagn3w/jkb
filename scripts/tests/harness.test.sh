#!/usr/bin/env bash
# Regression test for scripts/tests/harness.sh itself.
#
# Why a test file tests the test harness: `new_workdir` used to register its EXIT trap inside
# a command substitution, so both of the things it promises were false — the directory it
# returned had already been deleted by the subshell that made it, and the caller got no
# cleanup at all. Neither was observable from the suites, because every case `mkdir -p`s its
# own subdirectory before using it, and a leaked temp tree only shows up as disk. So the
# harness's contract is asserted here, from outside, rather than assumed by three suites.
#
# The cases that check cleanup spawn a CHILD shell that sources the harness and exits, because
# "removed on exit" is a claim about a process ending — it cannot be observed from inside the
# process making it.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
harness="$(dirname "$0")/harness.sh"
# shellcheck source=scripts/tests/harness.sh
. "$harness"

new_workdir

# --- 1. the directory exists, and is usable without mkdir --------------------------------
# The shape a case is written in the obvious way: `printf … > "$work/thing"`. Under the old
# version this failed with ENOENT on a path the harness had just handed out.
case1() {
    if [ -d "$work" ]; then
        ok "new_workdir leaves \$work on disk"
    else
        fail "exists: gone" "\$work ($work) does not exist after new_workdir"
        return
    fi
    if printf 'x\n' >"$work/direct" 2>/dev/null && [ -f "$work/direct" ]; then
        ok "and a case can write into it without creating it first"
    else
        fail "exists: unusable" "could not write $work/direct"
    fi
}

# --- 2. it is removed when the test file exits -------------------------------------------
case2() {
    local child="$work/child.sh" path
    cat >"$child" <<EOF
. "$harness"
new_workdir
printf '%s\n' "\$work"
EOF
    path="$(bash "$child")"
    if [ -z "$path" ]; then
        fail "cleanup: premise" "the child printed no path"
        return
    fi
    if [ -e "$path" ]; then
        fail "cleanup: leaked" "$path survived the child shell that created it"
    else
        ok "the work directory is removed when the shell that made it exits"
    fi
}

# --- 2b. even after a case has made something unwritable ----------------------------------
# The `chmod -R u+rwx` in the cleanup is there for install-exec.test.sh case 3, which drops a
# directory to mode 500 and could be interrupted before restoring it. Nothing exercised it.
case2b() {
    local child="$work/child-locked.sh" path
    if [ "$(id -u)" = "0" ]; then
        skip "an unwritable directory cannot be simulated as root"
        return
    fi
    cat >"$child" <<EOF
. "$harness"
new_workdir
mkdir -p "\$work/locked"
: >"\$work/locked/file"
chmod 500 "\$work/locked"
printf '%s\n' "\$work"
EOF
    path="$(bash "$child")"
    if [ -n "$path" ] && [ ! -e "$path" ]; then
        ok "and removed even when a case left a directory unwritable"
    else
        fail "cleanup: locked" "$path survived (mode 500 subdirectory)"
    fi
}

# --- 3. a file that asserts nothing fails -------------------------------------------------
# `finish`'s reason for existing: a suite whose every case bailed out on a failed premise used
# to exit 0, indistinguishable from one that checked everything and was happy.
case3() {
    local child="$work/child-empty.sh" status
    cat >"$child" <<EOF
. "$harness"
new_workdir
finish
EOF
    bash "$child" >/dev/null 2>&1; status=$?
    [ "$status" -ne 0 ] \
        && ok "a suite that asserted nothing fails" \
        || fail "finish: silent" "a suite with zero assertions exited 0"
}

# --- 4. a failed assertion fails the file -------------------------------------------------
case4() {
    local child="$work/child-fail.sh" status
    cat >"$child" <<EOF
. "$harness"
new_workdir
ok "one that passed"
fail "one that did not" "the reason"
finish
EOF
    bash "$child" >/dev/null 2>&1; status=$?
    [ "$status" -ne 0 ] \
        && ok "a suite with a failed assertion fails" \
        || fail "finish: green" "a suite containing a FAIL exited 0"
}

echo "==> scripts/tests/harness.sh"
case1
case2
case2b
case3
case4

finish
