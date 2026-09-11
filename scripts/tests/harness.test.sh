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
# can outrank the empty configuration" — a claim about a class, checked against a sample. So a
# NARROWING of `isolate_git` went unnoticed, which is what this case fixes.
#
# It is NOT why the shell half fell a round behind the Rust half, and the first version of this
# paragraph said it was. Round 27 widened `MUST_DROP` and nothing here was wrong — both sides of
# this comparison live in one file and move together, so this case would have passed then too. The
# drift was between the LANGUAGES, and that needs an artifact reading the other one; `case_rust_twin`
# below is it.
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
    # A CANDIDATE SET WIDER THAN THE EXPECTATION, or the "extra" half of the equality is a
    # tautology: the probe used to export only `$want`, so the diff could contain nothing else and
    # an added `unset` was invisible. I "verified" that direction with EDITOR — which was in the
    # probe list, so the check was circular. These four are the names `MUST_DROP`'s own comment
    # nominates as the next plausible widening, which makes them exactly the shape this oracle has
    # to be able to see.
    local candidates="GIT_CEILING_DIRECTORIES GIT_DISCOVERY_ACROSS_FILESYSTEM GIT_NAMESPACE \
GIT_INDEX_VERSION"
    # DIFFED, not polled. Asking "which of the names I expected are gone" cannot see an unset
    # nobody expected — the first version of this case exported only the wanted names, so adding
    # `EDITOR` to `isolate_git` passed. `compgen -e` before and after gives the removals whatever
    # they are, which is what makes this equality rather than a checklist.
    #
    # `GIT_CONFIG_KEY_<n>`/`VALUE_<n>` are filtered out of the comparison because how many of them
    # exist is a fact about the machine — this project's dev container exports four to carry its
    # `safe.directory` grants — so they cannot be a written-down literal. That sweep is pinned by
    # the `isolate: revive` assertion in `case_isolate` instead, named rather than counted: an
    # ordinal cross-reference is read off whichever assertion the next reader starts counting from.
    got="$(
        bash -c '
            . "$1" >/dev/null 2>&1
            for v in $3 $4 CONTROL_KEEP EDITOR PAGER; do export "$v=probe"; done
            before="$(compgen -e | sort)"
            isolate_git "$2/home" >/dev/null 2>&1
            after="$(compgen -e | sort)"
            comm -23 <(printf "%s\n" "$before") <(printf "%s\n" "$after") \
                | grep -vE "^GIT_CONFIG_(KEY|VALUE)_[0-9]+$" | sort | tr "\n" " "
        ' _ "$lib" "$d" "$want" "$candidates"
    )"
    # ...AND WHAT IT SETS. `comm -23` is removals only, so `GIT_CONFIG_NOSYSTEM=1` was unpinned —
    # and it is the only thing holding /etc/gitconfig out of these suites, because `isolate_git`
    # UNSETS `GIT_CONFIG_SYSTEM` rather than pointing it at /dev/null the way the Rust twin's
    # `MUST_SET` does. Measured: cutting `isolate_git` down to `export HOME="$1"` left all
    # assertions green while this container's live /etc/gitconfig came back into a suite that is
    # entirely about `core.hooksPath`. A case headed "THE WHOLE SET" that checks half of one is
    # worse than no case, because a reader stops looking.
    local added
    added="$(
        bash -c '
            . "$1" >/dev/null 2>&1
            # A DIFF SEES NOTHING WHERE THE VALUE WAS ALREADY THERE. On a machine that already
            # exports GIT_CONFIG_NOSYSTEM — a plausible developer setting, and this assertion is
            # about the variable that holds /etc/gitconfig out — `isolate_git` would add nothing
            # and this would blame it for a setting it makes correctly.
            unset GIT_CONFIG_NOSYSTEM
            before="$(compgen -e | sort)"
            isolate_git "$2/home" >/dev/null 2>&1
            after="$(compgen -e | sort)"
            comm -13 <(printf "%s\n" "$before") <(printf "%s\n" "$after") | sort | tr "\n" " "
        ' _ "$lib" "$d"
    )"
    [ "$added" = "GIT_CONFIG_NOSYSTEM " ] \
        && ok "and the only variable it ADDS is GIT_CONFIG_NOSYSTEM, which holds /etc/gitconfig out" \
        || fail "isolate: added" "isolate_git sets a different set than expected — \
GIT_CONFIG_NOSYSTEM is what keeps the system config out of these suites, since GIT_CONFIG_SYSTEM \
is unset rather than pointed at /dev/null. wanted [GIT_CONFIG_NOSYSTEM ] got [$added]"

    if [ "$got" = "$want" ]; then
        ok "and that is the WHOLE set it unsets, against a list this test writes down itself"
    else
        fail "isolate: set" "isolate_git unsets a different set than this test expects. Missing \
means a variable the developer's shell now reaches these suites' \`git\` through — add it to \
\`isolate_git\`, and to the Rust twin \`MUST_DROP\`. Extra means it unsets something nobody \
asked for. wanted [$want] got [$got]"
    fi
}

# THE RUST TWIN, READ FROM ITS OWN SOURCE.
#
# `isolate_git` and `crates/jkb-cli/tests/common/mod.rs`'s `MUST_DROP` are one rule in two
# languages. Nothing related them, and the cost was measured: round 27 widened `MUST_DROP` after
# reproducing a fixture rewriting somebody's real index, and the shell half — whose suites run
# `git init`/`add`/`commit` in a work dir — kept inheriting the same variables for a round, with
# the whole gate green. `case_isolate_set` cannot see that; both sides of its comparison live in
# this file.
#
# SUBSET, not equality, and in this direction: everything the Rust fixtures refuse, the shell ones
# must refuse too. `isolate_git` may drop more (it takes `XDG_CONFIG_HOME`, which means nothing to
# a `Command`), and the message says to widen `isolate_git` rather than trim `MUST_DROP` — the
# lesson of rounds 24-26, where "the lists disagree" read as an instruction to sync them and the
# sync was the edit that reopened a hole.
#
# This is the parity test this branch deleted twice, and it earns its place here for a reason those
# did not have: there is no `#[path]` trick across a language boundary, and the drift it names has
# actually happened.
case_rust_twin() {
    local must_drop shell_unset missing="" v
    must_drop="$(sed -n '/^pub const MUST_DROP/,/^\];/p' \
        "$repo_root/crates/jkb-cli/tests/common/mod.rs" \
        | grep -oE '"[A-Z_]+"' | tr -d '"' | sort -u)"
    # FROM THE STATEMENT, NOT THE BODY. This grepped names out of the whole function including its
    # COMMENTS — and that body explains, in prose, why `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY` and
    # `GIT_TEMPLATE_DIR` are there. Measured: deleting those three from the real `unset` and
    # narrowing `case_isolate_set`'s literal to match left this case reporting ok. They are exactly
    # the three the round-27 drift was about, so the pin was blind to the one state it exists for.
    shell_unset="$(sed -n '/^isolate_git()/,/^}/p' "$repo_root/scripts/tests/harness.sh" \
        | grep -vE '^[[:space:]]*#' \
        | sed -e :a -e '/\\$/N; s/\\\n//; ta' \
        | grep -E '^[[:space:]]*unset[[:space:]]' \
        | grep -oE '\bGIT_[A-Z_]+\b|\bXDG_[A-Z_]+\b' | sort -u)"
    # A COUNT PREMISE, because both halves of this comparison are extracted by regex and an
    # extraction that returns nothing compares equal to nothing.
    if [ "$(printf '%s' "$shell_unset" | grep -c .)" -lt 8 ]; then
        fail "isolate: twin-premise" "extracted only \
$(printf '%s' "$shell_unset" | grep -c .) name(s) from isolate_git's unset statement; the \
extraction is broken, not the code"
        return
    fi
    if [ -z "$must_drop" ] || [ -z "$shell_unset" ]; then
        fail "isolate: twin-premise" "could not read one of the two lists — MUST_DROP had \
$(printf '%s' "$must_drop" | grep -c .) name(s), isolate_git had \
$(printf '%s' "$shell_unset" | grep -c .); the extraction is broken, not the code"
        return
    fi
    for v in $must_drop; do
        grep -qxF "$v" <<<"$shell_unset" || missing="$missing $v"
    done
    [ -z "$missing" ] \
        && ok "and everything the Rust fixtures refuse, isolate_git refuses too" \
        || fail "isolate: twin" "these are in MUST_DROP but not in isolate_git, so the shell \
suites — which build real repositories — still inherit them:$missing. Widen isolate_git (and this \
file's own want list); do not trim MUST_DROP."
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
           case_isolate_set case_rust_twin

finish
