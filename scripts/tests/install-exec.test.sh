#!/usr/bin/env bash
# Regression test for scripts/lib.sh::install_exec.
#
# The bug it exists for: setup.sh installed the post-merge hook with `cp`, and the
# post-merge hook is what runs setup.sh. `cp` truncates and rewrites the target inode in
# place, and bash reads a script lazily by byte offset — so replacing a running script
# with one of a different length makes the shell resume at a stale offset and execute
# whatever fragment lands there. Seen in the wild as
# "post-merge: line 35: i: command not found" on a line that is blank in both versions.
#
# Case 1 below is that exact shape. Swap `install_exec` back for `cp` in scripts/lib.sh
# and it fails (verified: stderr gains a "command not found" for a fragment of an
# earlier line).
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

work="$(new_workdir)"

# --- 1. a script survives being replaced, by a longer version, while it is running -------
case1() {
    local d="$work/running" out err status
    mkdir -p "$d"
    cat >"$d/new" <<'EOF'
#!/usr/bin/env bash
# a comment that is longer than the one it replaces
echo "start"
"$(dirname "$0")/installer"
echo "tail ran"
EOF
    # The installed (old) copy: same script, shorter by the length of that comment tail.
    sed 's/ than the one it replaces//' "$d/new" >"$d/script"
    chmod +x "$d/script"
    cat >"$d/installer" <<EOF
#!/usr/bin/env bash
. "$repo_root/scripts/lib.sh"
install_exec "$d/script" <"$d/new"
EOF
    chmod +x "$d/installer"

    out="$("$d/script" 2>"$d/stderr")"; status=$?
    err="$(cat "$d/stderr")"

    [ "$status" -eq 0 ] || fail "self-replacement: exit status" "expected 0, got $status"
    [ -z "$err" ] || fail "self-replacement: clean stderr" "the running shell executed a fragment: $err"
    if [ "$out" = "$(printf 'start\ntail ran')" ]; then
        ok "a running script replaced by a longer one still runs to completion"
    else
        fail "self-replacement: output" "expected 'start' then 'tail ran', got: $(printf '%s' "$out" | tr '\n' '|')"
    fi
    # And the replacement really did land.
    grep -q 'longer than the one it replaces' "$d/script" \
        || fail "self-replacement: install" "the new version was not installed"
}

# --- 2. the installed file is executable, and no temp files are left behind -------------
case2() {
    local d="$work/mode"
    mkdir -p "$d"
    # A heredoc, which is how setup.sh installs the global chainer.
    install_exec "$d/prog" <<'PROG' || fail "mode: install" "install_exec returned non-zero"
#!/bin/sh
echo hi
PROG
    if [ -x "$d/prog" ] && [ "$("$d/prog")" = "hi" ]; then
        ok "the installed file is executable"
    else
        fail "mode: exec bit" "$d/prog is not runnable (caller no longer chmods it)"
    fi
    if [ "$(entries_in "$d")" = "prog" ]; then
        ok "a successful install leaves the installed file and nothing else"
    else
        fail "mode: temp" "expected just 'prog', found: $(entries_in "$d" | tr '\n' ' ')"
    fi
}

# --- 3. a failed install reports it and leaves the destination untouched ----------------
# A partial write must be unobservable: the point of the temp+rename is that the
# destination is either the old file or the new one, never half of either.
case3() {
    local d="$work/failure"
    mkdir -p "$d"
    printf 'original\n' >"$d/prog"
    if [ "$(id -u)" = "0" ]; then
        skip "an unwritable directory cannot be simulated as root"
        return
    fi
    chmod 500 "$d"
    if printf 'replacement\n' | install_exec "$d/prog" 2>/dev/null; then
        chmod 700 "$d"
        fail "failure: status" "install_exec reported success though it could not write"
        return
    fi
    chmod 700 "$d"
    if [ "$(cat "$d/prog")" = "original" ]; then
        ok "a failed install leaves the destination untouched"
    else
        fail "failure: destination" "the destination was modified by a failed install"
    fi
}

# --- 4. a failure AFTER the temp file exists leaves no temp file ------------------------
# Case 3 above cannot pin this: an unwritable directory fails at `mktemp`, before there is
# anything to clean up, so deleting install_exec's `rm -f "$tmp"` left this suite green.
# Reading a directory is the one failure reachable after mktemp succeeds without fault
# injection — `cat` exits 1 with "Is a directory" on both BSD and GNU userland. (`mv` onto an
# existing directory is NOT a failure: it moves the file inside.)
case4() {
    local d="$work/stranded" before
    mkdir -p "$d/a-directory"
    printf 'original\n' >"$d/prog"
    before="$(entries_in "$d")"
    if install_exec "$d/prog" <"$d/a-directory" 2>/dev/null; then
        fail "stranded: status" "install_exec reported success though its input was unreadable"
        return
    fi
    if [ "$(entries_in "$d")" = "$before" ]; then
        ok "a failure after the temp file exists still leaves no temp file"
    else
        fail "stranded: temp" "the directory gained: $(entries_in "$d" | tr '\n' ' ')"
    fi
    [ "$(cat "$d/prog")" = "original" ] \
        || fail "stranded: destination" "the destination was modified by a failed install"
}

# --- 5. a destination that is a directory is refused, not filled ------------------------
# `mv file dir` moves the file INSIDE dir and exits 0, so without an explicit refusal this
# installs nothing, strands a temp file in a stranger's hook directory, and reports success.
# A directory-style hook manager keeps `post-merge/` as exactly such a folder. The Rust twin
# `atomic::write` fails here because `fs::rename` does; the two implementations must agree.
case5() {
    local d="$work/dirdest"
    mkdir -p "$d/post-merge"
    if printf '#!/bin/sh\n' | install_exec "$d/post-merge" 2>/dev/null; then
        fail "dirdest: status" "install_exec reported success installing onto a directory"
    elif [ -n "$(entries_in "$d/post-merge")" ]; then
        fail "dirdest: contents" "it wrote into the directory: $(entries_in "$d/post-merge" | tr '\n' ' ')"
    else
        ok "a destination that is a directory is refused, and nothing is written into it"
    fi
}

echo "==> scripts/lib.sh::install_exec"
case1
case2
case3
case4
case5

finish
