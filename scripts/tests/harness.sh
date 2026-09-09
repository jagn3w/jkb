# Shared harness for scripts/tests/*.test.sh. Sourced, never run (no `.test.sh` suffix, so
# check.sh's glob does not pick it up).
#
#   . "$(dirname "$0")/harness.sh"
#   new_workdir            # sets $work
#   isolate_git "$work/home"
#   ok / fail / skip
#   finish
#
# It exists because each test file used to hand-roll its counters and epilogue. Two problems
# came out of that: the git isolation was written twice and was incomplete in both copies, and
# a file that asserted *nothing* — every case returning early on a failed premise — exited 0
# and was indistinguishable from one that passed. `finish` fails on zero assertions, so the
# gate can tell "checked and fine" from "never checked".

_asserts=0
_failures=0

ok()   { _asserts=$((_asserts + 1)); printf '  ok   %s\n' "$1"; }
fail() { _asserts=$((_asserts + 1)); _failures=$((_failures + 1)); printf '  FAIL %s\n     %s\n' "$1" "$2"; }
skip() { printf '  skip %s\n' "$1"; }

# finish — exit 1 if anything failed, or if nothing was asserted at all.
finish() {
    if [ "$_asserts" -eq 0 ]; then
        echo "no assertions ran — every case bailed out before asserting anything" >&2
        exit 1
    fi
    if [ "$_failures" -ne 0 ]; then
        echo "$_failures failure(s) of $_asserts" >&2
        exit 1
    fi
}

# new_workdir — create a temp directory and put its path in `$work`.
#
# It ASSIGNS rather than prints, and the EXIT trap is registered at file scope below rather
# than inside the function. Both are the same correction. Written as `work="$(new_workdir)"`,
# the body runs in a command-substitution subshell — so a `trap … EXIT` registered there fires
# the instant the subshell ends, deleting the directory it had just printed, and the caller is
# left with no cleanup at all. Neither half was visible: every case happens to `mkdir -p` its
# own subdirectory first (at 755, not mktemp's 700), so the suites passed while each gate run
# leaked a temp tree of git repos and linked worktrees. scripts/tests/harness.test.sh pins
# both halves.
work=""
new_workdir() {
    work="$(mktemp -d)" || { echo "harness: mktemp -d failed" >&2; exit 1; }
}

# `chmod` first: a case may make a directory unwritable, and an interrupt before it restores
# the mode would leave `rm -rf` unable to clean up.
_cleanup_workdir() {
    [ -n "$work" ] || return 0
    chmod -R u+rwx "$work" 2>/dev/null
    rm -rf "$work"
}
trap _cleanup_workdir EXIT

# isolate_git <home> — point git at an empty configuration, so a test measures the repos it
# builds and nothing about the machine it runs on.
#
# All five matter, and the precedence order is why. GIT_CONFIG_GLOBAL *replaces*
# `$HOME/.gitconfig`, and GIT_CONFIG_COUNT injects settings that outrank every file — so
# exporting HOME alone isolates nothing when either is set, and an ambient `core.hooksPath`
# then reddens this suite over a regression that does not exist. That is worse than a missed
# defect here: check.sh is the gate `jkb task land` and the merge queue trust, so a false red
# blocks a landing and the failure text points at innocent code.
isolate_git() {
    mkdir -p "$1" || return 1
    export HOME="$1" GIT_CONFIG_NOSYSTEM=1
    unset XDG_CONFIG_HOME GIT_DIR GIT_WORK_TREE GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_COUNT
}

# git_q … — git with an identity, so `commit` works under the isolated config.
git_q() { git -c user.name=t -c user.email=t@example.com "$@"; }

# inode_of <path> — the file's inode number.
#
# The faithful observable for "was this replaced safely?". A rename (or unlink+create) gives
# the destination a NEW inode and leaves the old one intact for whoever is reading — or
# executing — it; `cp` and `> "$dest"` truncate and rewrite the same inode underneath them.
# So an assertion on content cannot tell the two apart, and every mutation at a call site
# survived the suites until this was asserted. `ls -di` rather than `stat`, whose flags split
# GNU (`-c`) from BSD (`-f`).
inode_of() { ls -di "$1" 2>/dev/null | awk '{print $1}'; }

# entries_in <dir> — every entry, sorted. Tests assert a directory's whole contents rather
# than searching for a name they expect: looking for `.jkb-install.*` meant the assertion
# knew install_exec's temp template, so renaming it made those cases pass regardless.
entries_in() { ls -A "$1" 2>/dev/null | sort; }
