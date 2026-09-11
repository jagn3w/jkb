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

# --- 2c. inode_of distinguishes a rename from an in-place rewrite -------------------------
# The whole point of the helper: every atomicity assertion in the other suites is built on it,
# so if it answered the same for both it would turn three real pins into three vacuous ones.
case2c() {
    local f="$work/inode" before
    printf 'one\n' >"$f"
    before="$(inode_of "$f")"
    printf 'two\n' >"$f"                       # in place: same inode
    if [ -n "$before" ] && [ "$(inode_of "$f")" = "$before" ]; then
        ok "inode_of is unchanged by an in-place rewrite"
    else
        fail "inode_of: in place" "before=$before after=$(inode_of "$f")"
    fi
    printf 'three\n' >"$f.tmp" && mv -f "$f.tmp" "$f"   # rename: new inode
    if [ "$(inode_of "$f")" != "$before" ]; then
        ok "and changes when the file is replaced by rename"
    else
        fail "inode_of: rename" "still $before after a rename"
    fi
}

# --- 2d. the runner refuses to name a case that does not exist ----------------------------
# `finish` asks only whether the FILE asserted something, so with a hundred passing assertions
# beside them two deleted case bodies cost nothing: bash printed `case6g: command not found`
# on stderr and the file exited 0. Two regression pins for bugs this repo has shipped once
# already went that way, with the whole gate green. That is this harness's own stated failure
# mode, one level down.
case2d() {
    local child="$work/child-missing.sh" status out
    cat >"$child" <<EOF
. "$harness"
new_workdir
present() { ok "ran"; }
run_cases present absent
finish
EOF
    out="$(bash "$child" 2>&1)"; status=$?
    if [ "$status" -ne 0 ]; then
        ok "naming a case that does not exist fails the file"
    else
        fail "runner: silent" "a missing case exited 0: $out"
    fi
    case "$out" in
        *"do not exist"*absent*) ok "and says which name it could not find" ;;
        *) fail "runner: mute" "no useful message: $out" ;;
    esac

    # The mirror: a body nothing calls looks like coverage, runs never, and can rot into
    # something that would fail if it were run. `finish` cannot see that either.
    local orphan="$work/child-orphan.sh"
    cat >"$orphan" <<EOF
. "$harness"
new_workdir
case1() { ok "ran"; }
case2() { ok "never called"; }
run_cases case1
finish
EOF
    out="$(bash "$orphan" 2>&1)"; status=$?
    if [ "$status" -ne 0 ]; then
        ok "and a case body the runner never names fails it too"
    else
        fail "runner: orphan" "an uncalled case exited 0: $out"
    fi
    case "$out" in
        *"never run"*case2*) ok "naming the one it found" ;;
        *) fail "runner: orphan mute" "no useful message: $out" ;;
    esac
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
# --- isolate_git really isolates -----------------------------------------------------------
# It had no case at all, and its `GIT_CONFIG_KEY_<n>`/`VALUE_<n>` sweep used `\|` alternation —
# a GNU BRE extension, so on BSD/macOS sed it matched nothing and unset nothing, silently, on
# the platform this project is developed on. The variables are live: this project's dev
# container exports `GIT_CONFIG_COUNT` to carry `safe.directory` grants.
# THE WHOLE SET, compared for EQUALITY against a list written down here.
#
# `case_isolate` below enumerates five names and reports "isolate_git unsets every variable that
# can outrank the empty configuration" — a claim about a class, checked against a sample. That is
# why the shell half could be left a round behind when the Rust half was widened: round 27 added
# three repository COMPONENT selectors to `MUST_DROP` after measuring that an exported
# `GIT_INDEX_FILE` lets a fixture rewrite somebody's real index, and nothing here noticed that the
# suites which build actual repositories still inherited it. Measured at the time: deleting the
# names again left this suite at 17 ok, 0 fail.
#
# The literal below is this test's own, never derived from `harness.sh` — the rule the Rust side
# arrived at over rounds 24-26 after getting it wrong in both directions. A name added to
# `isolate_git` and not here fails as an extra; a name dropped from `isolate_git` fails as a
# missing one; and `CONTROL_KEEP` must survive, so a function that simply unset everything would
# not pass either.
case_isolate_set() {
    local d="$work/isoset" lib got want
    lib="$(cd "$(dirname "$0")" && pwd)/harness.sh"
    mkdir -p "$d"
    want="GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_CONFIG_COUNT GIT_CONFIG_GLOBAL \
GIT_CONFIG_PARAMETERS GIT_CONFIG_SYSTEM GIT_DIR GIT_INDEX_FILE GIT_OBJECT_DIRECTORY \
GIT_TEMPLATE_DIR GIT_WORK_TREE XDG_CONFIG_HOME"
    want="$(printf '%s\n' $want | sort | tr '\n' ' ')"
    # DIFFED, not polled. Asking "which of the names I expected are gone" cannot see an unset
    # nobody expected — the first version of this case exported only the wanted names, so adding
    # `EDITOR` to `isolate_git` passed. `compgen -e` before and after gives the removals whatever
    # they are, which is what makes this equality rather than a checklist.
    #
    # `GIT_CONFIG_KEY_<n>`/`VALUE_<n>` are filtered out of the comparison because how many of them
    # exist is a fact about the machine — this project's dev container exports four to carry its
    # `safe.directory` grants — so they cannot be a written-down literal. That sweep is pinned by
    # `case_isolate`'s third assertion instead, which is the one that discriminates it.
    got="$(
        bash -c '
            . "$1" >/dev/null 2>&1
            for v in $3 CONTROL_KEEP EDITOR PAGER; do export "$v=probe"; done
            before="$(compgen -e | sort)"
            isolate_git "$2/home" >/dev/null 2>&1
            after="$(compgen -e | sort)"
            comm -23 <(printf "%s\n" "$before") <(printf "%s\n" "$after") \
                | grep -vE "^GIT_CONFIG_(KEY|VALUE)_[0-9]+$" | sort | tr "\n" " "
        ' _ "$lib" "$d" "$want"
    )"
    if [ "$got" = "$want" ]; then
        ok "and that is the WHOLE set it unsets, against a list this test writes down itself"
    else
        fail "isolate: set" "isolate_git unsets a different set than this test expects. Missing \
means a variable the developer's shell now reaches these suites' \`git\` through — add it to \
\`isolate_git\`, and to the Rust twin \`MUST_DROP\`. Extra means it unsets something nobody \
asked for. wanted [$want] got [$got]"
    fi
}

case_isolate() {
    local d="$work/iso" lib got
    lib="$(cd "$(dirname "$0")" && pwd)/harness.sh"
    mkdir -p "$d"
    got="$(
        GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/injected \
        GIT_CONFIG_PARAMETERS="'core.hooksPath=/injected2'" GIT_COMMON_DIR=/elsewhere \
        bash -c '. "$1" >/dev/null 2>&1; isolate_git "$2/home" >/dev/null 2>&1
                 printf "count=[%s] key=[%s] value=[%s] params=[%s] common=[%s]" \
                        "${GIT_CONFIG_COUNT-}" "${GIT_CONFIG_KEY_0-}" "${GIT_CONFIG_VALUE_0-}" \
                        "${GIT_CONFIG_PARAMETERS-}" "${GIT_COMMON_DIR-}"' _ "$lib" "$d"
    )"
    [ "$got" = "count=[] key=[] value=[] params=[] common=[]" ] \
        && ok "isolate_git unsets every variable that can outrank the empty configuration" \
        || fail "isolate: vars" "got: $got"

    # And the effect that matters: git must see no injected core.hooksPath afterwards.
    #
    # One assertion per injection ROUTE. Stated precisely, because the obvious claim is false:
    # neither of these dies on its own omission — `isolate: vars` above catches a missing unset
    # first, since it reads the variables directly. What they add is the FILE-based routes no
    # variable check can see (measured: dropping `isolate_git`'s HOME redirect leaves
    # `isolate: vars` green and fails both), and the third assertion below is the one that
    # discriminates the KEY_<n>/VALUE_<n> sweep specifically.
    _isolate_sees() {   # _isolate_sees <label> <env-prefix…> -- runs git after isolate_git
        local label="$1"; shift
        got="$(env "$@" bash -c '. "$1" >/dev/null 2>&1; isolate_git "$2/$3" >/dev/null 2>&1
                 git init -q "$2/r-$3" 2>/dev/null
                 git -C "$2/r-$3" config --get core.hooksPath 2>/dev/null || printf "<none>"' \
             _ "$lib" "$d" "$label")"
        [ "$got" = "<none>" ] \
            && ok "and git sees no core.hooksPath injected via $label" \
            || fail "isolate: $label" "git still reports core.hooksPath=$got"
    }
    _isolate_sees count GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath \
        GIT_CONFIG_VALUE_0=/injected
    _isolate_sees params GIT_CONFIG_PARAMETERS="'core.hooksPath=/injected2'"

    # The KEY_<n>/VALUE_<n> sweep itself is NOT observable through git once the count is gone —
    # git reads no pair without it — so the first assertion above is what pins it, by looking at
    # the variables. Stated rather than left as a gap: it is hygiene for a subprocess that sets
    # its own count, not a second lock on the same door.
    #
    # Nor are these two a restatement of that first assertion, which was the doubt about the
    # single one they replace. Measured: dropping `isolate_git`'s HOME redirect — a FILE-based
    # leak, invisible to any check that reads variables — leaves `isolate: vars` green and
    # fails both of these. The variable check pins the variable routes; these pin the effect.

    # ...and THIS is the one that drives the KEY_<n>/VALUE_<n> sweep, by putting the count back
    # afterwards — the sweep's own stated scenario ("a later export of COUNT by anything under
    # test would make them live again"). Without it the pair is inert to git and no assertion
    # here could tell a working sweep from a deleted one; with it, git reads KEY_0 again unless
    # the sweep really removed it. This is the BSD-sed regression's own pin.
    got="$(
        GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/injected \
        bash -c '. "$1" >/dev/null 2>&1; isolate_git "$2/home3" >/dev/null 2>&1
                 export GIT_CONFIG_COUNT=1
                 git init -q "$2/r-revive" 2>/dev/null
                 git -C "$2/r-revive" config --get core.hooksPath 2>/dev/null || printf "<none>"' \
             _ "$lib" "$d"
    )"
    [ "$got" = "<none>" ] \
        && ok "and the KEY_<n>/VALUE_<n> sweep survives the count being re-exported afterwards" \
        || fail "isolate: revive" "a swept pair came back live: core.hooksPath=$got"
}

# --- orphan detection sees a case whose name is not `case<digit>` ---------------------------
# The pattern `run_cases` derives defined cases with was `case[0-9][0-9a-z]*`, and `case_isolate`
# — the only such name in these four suites — fell outside it, so deleting it from a runner's
# argument list left the gate green with a BSD-sed portability pin gone. Widening it fixed that
# and pinned NOTHING: reverting the pattern was green again. This is the pin, and it is written
# against the shape rather than against the one name, so the next `case_`-prefixed case is
# covered without anyone remembering.
case_orphanname() {
    local out rc=0 lib
    lib="$(cd "$(dirname "$0")" && pwd)/harness.sh"
    out="$(
        bash -c '. "$1" >/dev/null 2>&1
                 case1() { :; }
                 case_a_named_one() { :; }
                 run_cases case1' _ "$lib" 2>&1
    )" || rc=$?
    case "$out" in
        *"case_a_named_one"*)
            ok "a defined-but-unrun case is reported whatever its name is spelled like" ;;
        *)  fail "orphanname" "run_cases did not report case_a_named_one (rc=$rc, said '$out')" ;;
    esac
}

run_cases case1 case2 case2b case2c case2d case3 case4 case_orphanname case_isolate \
           case_isolate_set

finish
