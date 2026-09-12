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

# run_cases <name>… — run each named case, refusing a name that is not a declared function.
#
# The runner was a hand-written second list of case names, and bash treats an unknown one as a
# command: `case6g: command not found` on stderr, and the file still exits 0. Two regression
# pins — the cross-worktree agreement set and the whole CRLF exclude set, both for bugs this
# repo has shipped once already — were deleted that way with the gate green. `finish` cannot
# catch it, because it asks only whether the FILE asserted something and the surviving cases
# assert plenty. That is this harness's own stated failure mode, one level down.
# BOTH directions. A name with no body is the bug above; a body no name calls is the same
# class the other way round — a case that looks like coverage, runs never, and can rot into
# something that would fail if it were run. Neither is catchable by `finish`.
#
# The orphan half is only as wide as its pattern, and `case[0-9][0-9a-z]*` — `case` then a
# DIGIT — silently excluded `case_isolate`, the one name in these four suites that does not
# start with a number. Deleting it from a runner's argument list, which is exactly the edit
# this check exists to catch, left the gate green with a BSD-sed portability pin gone. A
# pattern claiming "both directions" must not have names it cannot see, so it admits `_` in
# both positions: a future case cannot fall outside it by being spelled reasonably.
run_cases() {
    local c defined missing="" orphaned=""
    for c in "$@"; do
        declare -F "$c" >/dev/null 2>&1 || missing="$missing $c"
    done
    while IFS= read -r defined; do
        case " $* " in *" $defined "*) ;; *) orphaned="$orphaned $defined" ;; esac
    done <<EOF
$(declare -F | sed -n 's/^declare -f \(case[0-9_][0-9a-z_]*\)$/\1/p')
EOF
    if [ -n "$missing" ] || [ -n "$orphaned" ]; then
        [ -z "$missing" ]  || echo "the runner names cases that do not exist:$missing" >&2
        [ -z "$orphaned" ] || echo "these cases are defined but never run:$orphaned" >&2
        exit 1
    fi
    for c in "$@"; do "$c"; done
}

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
    local d
    d="$(mktemp -d)" || { echo "harness: mktemp -d failed" >&2; exit 1; }
    # PHYSICAL path. `mktemp -d` returns the logical one, and on macOS `$TMPDIR` lives under
    # /var, a symlink to /private/var — while `git rev-parse --show-toplevel`, which
    # `git_hooks_override` and `reconcile_exclude` both call, returns the physical one. A case
    # that builds an expected path out of `$work` and compares it to git's answer then fails
    # on every Mac, and because a failed premise `return`s, the rest of that case silently
    # stops running: 13 assertions vanished from one case alone. check.sh is the gate
    # `jkb task land` and the merge queue trust, so a false red there blocks a landing and
    # points at innocent code.
    work="$(cd "$d" && pwd -P)" || { echo "harness: cannot resolve $d" >&2; exit 1; }
}

# abs_dir <path> — a directory's physical path, or the path itself if it does not exist.
#
# Here rather than in one suite: `$work` is already physical, but a case that asks git for a
# path and compares it to one it built still needs both sides resolved the same way.
abs_dir() {
    if [ -d "$1" ]; then (cd "$1" && pwd -P); else printf '%s\n' "$1"; fi
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
# The list is defined by a CLASS, not by a count: every variable that can outrank the empty
# configuration this points git at, plus every variable that selects a repository or a part of
# one. A count in the
# prose ("all five") is a fact about the line below it that goes stale the first time somebody
# extends it, and this one already had.
#
# The precedence order is why it matters. GIT_CONFIG_GLOBAL *replaces* `$HOME/.gitconfig`;
# GIT_CONFIG_COUNT and GIT_CONFIG_PARAMETERS inject settings that outrank every file — so
# exporting HOME alone isolates nothing when any of them is set, and an ambient
# `core.hooksPath` then reddens this suite over a regression that does not exist. That is
# worse than a missed defect here: check.sh is the gate `jkb task land` and the merge queue
# trust, so a false red blocks a landing and the failure text points at innocent code.
#
# GIT_CONFIG_PARAMETERS is not hypothetical: this project's own dev container exports it, and
# GIT_CONFIG_COUNT with it, to carry `safe.directory` grants.
isolate_git() {
    mkdir -p "$1" || return 1
    export HOME="$1" GIT_CONFIG_NOSYSTEM=1
    # ...AND THE DEVELOPER'S `jkb`, which is not a git variable but is the same class of thing: a
    # real binary these fixtures reach by accident. `scripts/hooks/post-merge` runs `jkb task
    # close-merged` as its second chore, gated only by `command -v jkb`, and five cases here run
    # that hook for real inside fixture repositories. Measured with a spy first on PATH: five
    # invocations of the write verb, cwd set to a throwaway directory.
    #
    # Harmless while only a human ran `check.sh` against their own store. Not harmless the
    # moment anything runs these suites with a `JKB_DB` pointing at a live knowledge base — CI
    # does, and `merge-queue.sh` will once `task/merge-queue-gates-the-graft` lands and puts the
    # suites in the landing gate with the swarm's `JKB_DB` exported. `close-merged` scopes itself
    # by the repository key it derives from its cwd, so
    # the only thing standing between a fixture and real tasks being closed is that no fixture
    # directory happens to share a name with a repo that has `repo=` tags in that database.
    #
    # A stub rather than an empty PATH: the hook's `command -v jkb` must still find something, or
    # the cases that assert what it says about chore 2 would be asserting the absence instead.
    mkdir -p "$1/bin" && printf '%s\n' '#!/bin/sh' 'exit 0' >"$1/bin/jkb" \
        && chmod +x "$1/bin/jkb" && export PATH="$1/bin:$PATH"
    # THE SAME LIST AS THE RUST SIDE'S `MUST_DROP`, and they move together. These suites build
    # real repositories — `git_q init`/`add`/`commit` in the work dir — so everything true of
    # `crates/jkb-cli/tests/common/mod.rs` is true here. Round 27 widened that list after
    # measuring the harm and this one was left behind for a round: with
    # `GIT_INDEX_FILE=<victim>/.git/index` exported, these suites' `add`/`commit` write into the
    # victim's index and `GIT_OBJECT_DIRECTORY` leaks loose objects out of them — and the thing
    # that runs them is `./scripts/check.sh`, the gate `jkb task land` and the merge queue trust.
    # Two implementations of one rule, and only one of them widened, is the drift this file's own
    # header warns about.
    #
    # `GIT_TEMPLATE_DIR` is here for a different route to the same kind of harm: it is copied into
    # every `git init`, so the developer's own hooks land inside each fixture repo and run on its
    # first commit. Pointing the config files at /dev/null cannot cover it — that neutralizes
    # `init.templateDir`, not the environment spelling.
    unset XDG_CONFIG_HOME GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR \
          GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES \
          GIT_TEMPLATE_DIR \
          GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS
    # KEY_<n>/VALUE_<n> are inert once COUNT is gone, but they are the machine's values and a
    # later export of COUNT by anything under test would make them live again. Swept by
    # pattern so the sweep cannot drift from however many the machine happens to set.
    # A `case` glob, not sed: `\|` alternation in a BRE is a GNU extension, so the expression
    # this replaced matched NOTHING on BSD/macOS sed — silently sweeping nothing on the very
    # platform this project is developed on. Measured with `sed --posix`.
    local line v
    for line in $(env); do
        v="${line%%=*}"
        case "$v" in
            GIT_CONFIG_KEY_*|GIT_CONFIG_VALUE_*) unset "$v" ;;
        esac
    done
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
