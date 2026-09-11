#!/usr/bin/env bash
# Every script here that invokes `cargo` must make the toolchain reachable first.
#
# The bug it exists for: scripts/check.sh — the gate CLAUDE.md tells you to run before every
# commit — called `cargo fmt` without sourcing `~/.cargo/env`, alone among the cargo wrappers.
# rustup does not install into a system PATH; it writes a line into the interactive shell rc
# files. So in ANY non-interactive shell — an agent's, a hook's, a `sh -c`, CI without a
# toolchain action — check.sh exited 127 at its first cargo line with `cargo: command not
# found`, and everything after the shell-syntax step never ran: rustfmt, clippy, the shell
# tests, `cargo test`, cargo-deny, the ui build.
#
# Measured cost: two review rounds were reported green over a `clippy -D warnings` that was
# already failing, because the gate never reached clippy to say so. That is the exact failure
# check.sh's own comments were written to prevent — "a header followed by nothing, then All
# checks passed, reads exactly like a gate that ran" — one step earlier than they were looking.
#
# So the rule is machine-checked rather than remembered at each site (seven today), and case2
# pins the SYMPTOM rather than the spelling: a wrapper that finds its toolchain some other way
# is fine, one that cannot is not.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"
# ...and lib.sh, for `shell_sources` — the ONE file list `check.sh` and `ci.yml` already share.
# A second glob here would be a second answer to "which files are scripts", and the defect the
# newest case exists for lived in a directory the other scan's glob did not reach. Sourcing it is
# definitions plus two harmless assignments; `git-hooks.test.sh` already does the same.
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"

new_workdir

# --- 1. no script calls cargo before it has arranged to have one ---------------------------
# Line-ordered, not merely "mentions it somewhere": a source line BELOW the first invocation
# reads as compliant and is not.
#
# `cargo` is found at a COMMAND POSITION, not anywhere on the line. The first version keyed on
# `^[[:space:]]*(\(cd [^)]*&&[[:space:]]*)?cargo[[:space:]]`, and a script whose spelling it
# could not parse fell out through `[ -n "$first_call" ] || continue` — SILENTLY EXEMPTED. Four
# forms already in this repository or its container missed it: `( cd "$r" && cargo … )` with a
# space after the paren, `RUSTFLAGS=… cargo build`, `exec cargo test`, and `if cargo build; then`.
# The guard was therefore off for exactly the author who did not already know the house idiom,
# which is the failure it exists to catch. It also failed in the other direction, matching
# `cargo` inside an `echo` string.
#
# So the line is split at command separators and each fragment is asked whether it BEGINS with
# cargo, after leading keywords and environment assignments. case0 drives that detector against
# every spelling above, both the six it must find and three it must not — because a detector
# nothing tests is the same silent exemption one level up.
_first_cargo_line() {
    # The env-assignment pattern is built at run time so it can name both quote characters
    # without fighting the shell over the single one. A QUOTED value with a space in it —
    # `RUSTFLAGS="-C link-arg=-s" cargo build`, which is how anyone sets more than one flag —
    # is not `[^[:space:]]*`, so the unquoted-only version stopped stripping there and the
    # line fell out as "mentions cargo, invokes nothing": silently exempt, the exact failure
    # case0 exists for. `env` joins the leading keywords for the same reason.
    awk '
        BEGIN {
            q = sprintf("%c", 39)
            envre = "^[A-Za-z_][A-Za-z0-9_]*=(\"[^\"]*\"|" q "[^" q "]*" q "|[^[:space:]]*)[[:space:]]+"
        }
        /^[[:space:]]*#/ { next }
        {
            line = $0
            gsub(/[;&|()]/, "\n", line)
            n = split(line, parts, "\n")
            for (i = 1; i <= n; i++) {
                p = parts[i]
                sub(/^[[:space:]]+/, "", p)
                while (p ~ /^(if|then|do|else|elif|exec|time|env)[[:space:]]/) {
                    sub(/^[A-Za-z]+[[:space:]]+/, "", p)
                }
                while (p ~ envre) { sub(envre, "", p) }
                if (p ~ /^cargo([[:space:]]|$)/) { print NR; exit }
            }
        }' "$1"
}

# The toolchain may be arranged ANY way that works — this suite's header says so, and case2
# pins the symptom rather than the spelling. Sourcing `~/.cargo/env` is the house idiom; a
# `PATH` export naming `.cargo/bin` or `$CARGO_HOME`, or `rustup run`, is equally fine. An
# earlier version accepted only the first and would have hard-failed the others.
_first_toolchain_line() {
    grep -nE '(\.|source)[[:space:]]+"?(~|\$HOME|\$\{HOME\})/\.cargo/env|PATH=[^#]*(\.cargo/bin|CARGO_HOME)|rustup[[:space:]]+run' "$1" \
        | grep -vE '^[0-9]+:[[:space:]]*#' | head -1 | cut -d: -f1
}

# --- 0. the detector itself, against every spelling it has to be right about ----------------
case0() {
    local probe="$work/forms.sh" n hit want_hit want_miss
    cat >"$probe" <<'FORMS'
( cd "$repo" && cargo install --path x )
RUSTFLAGS=-x cargo build
RUSTFLAGS="-C link-arg=-s" cargo build
env CARGO_TERM_COLOR=always cargo test
exec cargo test
if cargo build; then
(cd "$r" && cargo fmt)
    cargo clippy
echo "install it with: cargo install --path crates/jkb-cli"
if ! command -v cargo >/dev/null 2>&1; then
echo "(build the crate first so cargo extracts its source)" >&2
FORMS
    # Lines 1-8 are invocations; 9-11 mention cargo and invoke nothing. Asked one line at a
    # time, because `_first_cargo_line` stops at the first hit and would otherwise report only
    # line 1 whatever the other ten do.
    want_hit=""; want_miss=""
    n=0
    while IFS= read -r line; do
        n=$((n + 1))
        printf '%s\n' "$line" >"$work/one.sh"
        hit="$(_first_cargo_line "$work/one.sh")"
        if [ "$n" -le 8 ]; then
            [ -n "$hit" ] || want_hit="$want_hit $n"
        else
            [ -z "$hit" ] || want_miss="$want_miss $n"
        fi
    done <"$probe"
    [ "$n" -eq 11 ] || fail "toolchain: forms-premise" "read $n form(s), expected 11"
    if [ -n "$want_hit" ]; then
        fail "toolchain: forms-miss" "these real invocation spellings are not seen as cargo \
calls, so a script using one is silently exempted:$want_hit"
    elif [ -n "$want_miss" ]; then
        fail "toolchain: forms-false" "these lines invoke nothing and were read as calls:$want_miss"
    else
        ok "the cargo detector sees every invocation spelling in use, and no mere mention"
    fi
}

case1() {
    local f base first_call first_src bad="" seen=0
    for f in "$repo_root"/scripts/*.sh; do
        [ -f "$f" ] || continue
        base="$(basename "$f")"
        first_call="$(_first_cargo_line "$f")"
        [ -n "$first_call" ] || continue
        seen=$((seen + 1))
        first_src="$(_first_toolchain_line "$f")"
        if [ -z "$first_src" ]; then
            bad="$bad $base(never arranges one)"
        elif [ "$first_src" -gt "$first_call" ]; then
            bad="$bad $base(arranges at $first_src, calls at $first_call)"
        fi
    done
    # A FLOOR, because a loop that inspected nothing prints the same "ok" as one that inspected
    # everything — the shape this suite is here to remove. Seven scripts call cargo today; the
    # floor is lower so ordinary churn does not trip it, and a broken glob or detector does.
    if [ "$seen" -lt 5 ]; then
        fail "toolchain: coverage" "only $seen script(s) were inspected, so this case asserts \
almost nothing — the glob or the detector has regressed"
    elif [ -n "$bad" ]; then
        fail "toolchain: order" "script(s) call cargo without a toolchain first:$bad"
    else
        ok "every script that invokes cargo arranges a toolchain before it does ($seen inspected)"
    fi
}

# --- 2. and check.sh actually gets past its first cargo line in a bare environment ---------
# The symptom, not the spelling. `env -i` with a PATH that has no cargo, and a HOME whose
# `.cargo/env` puts a STUB cargo on PATH: if check.sh sources it, the stub runs and check.sh
# dies on the stub's own exit status; if it does not, bash says `command not found` (127).
# The stub exits 1 at the first call, so nothing expensive runs and the shell-test loop below
# clippy is never reached — this suite does not re-enter itself.
case2() {
    local h="$work/home" out status
    mkdir -p "$h/.cargo" "$work/bin"
    printf '%s\n' '#!/bin/sh' 'echo "STUB-CARGO $*" >&2' 'exit 1' >"$work/bin/cargo"
    chmod +x "$work/bin/cargo"
    printf 'export PATH="%s:$PATH"\n' "$work/bin" >"$h/.cargo/env"
    out="$(env -i HOME="$h" PATH="/usr/bin:/bin" bash "$repo_root/scripts/check.sh" 2>&1)"
    status=$?
    case "$out" in
        *"cargo: command not found"*|*"cargo: No such file"*)
            fail "toolchain: check.sh" "check.sh cannot find cargo in a non-interactive shell (127)" ;;
        *"STUB-CARGO"*)
            ok "and check.sh reaches cargo itself when only ~/.cargo/env provides it" ;;
        *)
            fail "toolchain: premise" \
                 "check.sh neither found nor missed the stub (status=$status): $(printf '%s' "$out" | tr '\n' '|' | tail -c 200)" ;;
    esac
}

# --- a pipe into a quiet grep, under pipefail ----------------------------------------------
# `grep -q` exits at its FIRST match. A producer with more to write then dies on EPIPE, and
# `set -o pipefail` makes the pipeline report THAT — so a SUCCESSFUL match comes back as a failed
# test. Every occurrence is an inverted answer waiting for its input to grow.
#
# Two real instances, in two directories, found six days apart:
#
#   .container/run.sh          CI reported "run.sh no longer runs verify.sh" about a file that
#                              did, the invocation sitting at byte 23501 of 25810.
#   scripts/hooks/post-merge   announced "no build-affecting changes pulled — skipping setup.sh"
#                              on a pull that changed every crate in the repository.
#
# The first was fixed with a scan local to `.container/*.sh` matching only the
# `dc_strip_comments | grep -q` spelling. It could not have found the second: different spelling,
# different directory. Two half-guards with the bug in the gap between them is the shape this
# repository's own doc rules name as the defect, so there is ONE home, ONE glob and one message,
# and the container-local copy is gone (a pointer stands where it was).
#
# MEASURED, bash 5.2.21 on Linux, 30 trials per cell, match on the first line:
#
#                    4 KB   8 KB   16 KB   32 KB   64 KB   82 KB
#     pipefail       0/30   0/30    0/30   28/30   30/30   30/30
#     no pipefail    0/30   0/30    0/30    0/30    0/30    0/30
#
# Three things follow, and each shapes the rule. It is PROBABILISTIC — a band, not a threshold —
# so "our producer is small" is a claim about today's input. It is governed by the 64 KiB pipe
# buffer rather than by write sizes: `printf` writes in 120-290 byte pieces, and what decides it
# is whether the producer must BLOCK. And `pipefail` is the whole hazard: without it the
# producer's death changes nothing, because `grep -q`'s own status is what the shell reports.
#
# So the rule is conditioned on `pipefail` rather than blanket. That is not a softening — it is
# what makes the guard SELF-MAINTAINING. The three sites that survive it, both `.claude/hooks`
# scripts, are safe only because those files set no shell options, and the day somebody adds
# `set -o pipefail` to one this case fails and names the line. A blanket rule would have had to
# exempt them by directory instead, which is the same fact written where nothing checks it.
#
# The fix is always a here-string. `<<<` is a pipe at or below 65536 bytes and a temp file above
# (measured — the switch lands exactly on the pipe buffer), and it is safe either way for a reason
# unrelated to which: the shell finishes the write before the consumer is exec'd, and there is ONE
# command in the pipeline, so `pipefail` has no second status to take.
# ANCHORED TO A `set` COMMAND. Unanchored (`\<set\>[^#]*pipefail`) this matched PROSE: a comment
# reading "...because every caller happens to set `pipefail` --" satisfied it, which is how
# `.container/lib.sh` — shebang'd, sourced, and setting no options of its own — appeared to be in
# scope for a reason that had nothing to do with the code. Over-inclusion is the safe direction,
# so nothing was wrong; but the file was in scope by accident, and fixing the accident without
# fixing the rule would have dropped it out.
#
# It matches a line that BEGINS with `set`, which is not the same as "a `set` command": one inside
# a here-doc body or a quoted `bash -c '…'` script still counts. That is over-inclusion and so
# safe — the file is scanned, nothing is exempted — but it is not what the word "sets" suggests,
# and reading it that way is what produced the wrong account further down.
_sets_pipefail() { grep -qE '^[[:space:]]*set[[:space:]]+[^#]*pipefail' "$1"; }

# ASSEMBLED FROM HALVES so this file cannot match its own detector. A check that fails on an
# unmutated tree is the first thing a reader deletes, and this file has to spell the shape in
# order to look for it.
_racy_re() {
    printf '%s%s' '(^|[^|])\|&?[[:space:]]*(command[[:space:]]+)?[ef]?g' \
                  'rep([[:space:]]+-(-[a-z-]+|[A-Za-z]+))*[[:space:]]+(-[A-Za-z]*q[A-Za-z]*|--quiet|--silent)([[:space:]]|$)'
}

# The files the rule applies to: those that set `pipefail`, plus every SOURCED library, which runs
# under its caller's options and sets none of its own. `scripts/lib.sh` and
# `scripts/tests/harness.sh` are both in that second group and both are sourced by every suite
# here — a racy line added to either would run under pipefail while a file-local check saw a file
# with no `set` line at all. Membership is asserted by name below, because this half was wrong
# once already and nothing noticed.
# A file with NO SHEBANG cannot be run as a program — it can only be sourced, so it executes
# under its caller's shell options and is in scope whatever it does or does not `set` itself.
# That is a property of the file rather than a fact about who sources it, which is the whole
# reason to test it this way: the first version of this function parsed the `.`/`source` lines of
# every pipefail script to collect library basenames, and on
#
#     . "$(dirname "$0")/harness.sh"
#
# — how all five suites source the harness — it captured `$(dirname` as the filename. So
# `harness.sh` was outside the scope of the guard that names it, while the doc claimed the
# opposite and a planted line in `lib.sh` (sourced by an absolute path, hence parsed correctly)
# made it look verified. `shell_sources` already distinguishes the two kinds of file, so ask it
# rather than re-deriving the answer from a path.
_has_shebang() { case "$(head -c 2 "$1" 2>/dev/null)" in "#!") return 0 ;; *) return 1 ;; esac; }

# ...AND A SOURCED FILE THAT HAS ONE. "No shebang" identifies the libraries meant only to be
# sourced, but it is not the whole set: `.container/lib.sh` and `egress-lib.sh` carry a shebang
# (they are `--self-test`-able) and are sourced by scripts that set `pipefail`, so they run under
# it too. That is the dangerous direction — a racy line there would go unreported — and the earlier
# attempt at it is what shipped the `$(dirname` bug, so this time the basename is taken off the
# WHOLE argument rather than the first quote-free run, and the result is checked by name below.
#
# BY BASENAME, which over-includes and does not follow a variable. `lib.sh` and `setup.sh` each
# name two files in `shell_sources`, so sourcing either puts both in scope — the safe direction, a
# file scanned for nothing costs nothing. A source written through a variable cannot be followed at
# all; the one in this tree, `egress-lib.sh`'s `LIB="${BASH_SOURCE[0]}"`, is the file re-sourcing
# ITSELF in a child shell, so there is no second hop to miss.
#
# TWO of the four libraries rest on THIS CLAUSE ALONE — `.container/lib.sh` and
# `.container/egress-lib.sh`, both of which carry a shebang and neither of which sets `pipefail`.
# An earlier version of this paragraph said `egress-lib.sh` set it itself, and concluded that
# `.container/lib.sh` was the only one depending on the clause. Wrong, and wrong in an instructive
# way: I checked it with `_sets_pipefail`, which is the function under discussion. Its only match
# in that file is line 366 — `set -euo pipefail` inside a single-quoted `bash -c '…'` body in
# `under_trap`, script TEXT rather than a command this shell runs. Verifying a claim about a
# detector with that detector is how the prose match two paragraphs up survived as long as it did.
_sourced_by_scope() {
    local root="$1" f
    while IFS= read -r f; do
        _sets_pipefail "$f" || ! _has_shebang "$f" || continue
        sed -n 's/^[[:space:]]*\(\.\|source\)[[:space:]]\{1,\}\(.*\)/\2/p' "$f" 2>/dev/null \
            | sed 's/[[:space:]]*#.*//; s/.*\///; s/["'"'"']*[[:space:]]*$//'
    done < <(shell_sources "$root") | grep -E '^[A-Za-z0-9._-]+$' | sort -u
}

_pipefail_scope() {
    local root="$1" f libs
    libs="$(_sourced_by_scope "$root")"
    while IFS= read -r f; do
        if _sets_pipefail "$f" || ! _has_shebang "$f" \
           || grep -qxF "${f##*/}" <<<"$libs"; then
            printf '%s\n' "$f"
        fi
    done < <(shell_sources "$root")
}

# A pipeline may be broken across lines in EITHER direction, and a line-oriented match sees only
# one of them. `  | grep -q x` on a continuation line is caught by the `(^|[^|])` alternation;
# `cat f |` followed by `    grep -q x` is the same pipeline written the other way and was not.
# So lines are joined on a trailing `|` before matching, and the reported number is the line the
# pipeline STARTS on, which is the one a reader has to go and edit.
_joined() {
    # A COMMENT NEVER JOINS, and this is not tidiness. `scripts/lib.sh` — in scope, no shebang —
    # has two comment lines ending in `|` at 1191 and 1201, the exclude and dispatch vocabulary
    # lists, in the very file whose code those comments describe. A racy pipeline written under one
    # of them would be joined into the comment, and the joined record then STARTS with `#`, so the
    # comment filter downstream drops both: the line-joining added to WIDEN this detector would
    # have narrowed it instead.
    #
    # Dropped rather than flushed, because bash allows a comment BETWEEN the halves of a continued
    # pipeline and the pipeline really does continue past it. Skipping keeps `cat f |` / `# why` /
    # `grep -q x` joined; flushing would break it in two and miss the hit. A comment can never be a
    # racy site itself, so losing it costs nothing.
    awk '
        /^[ \t]*#/ { next }
        {
            if (held == "") { ln = NR; cur = $0 } else { cur = held " " $0 }
            if (cur ~ /\|[ \t]*$/) { held = cur; next }
            held = ""
            printf "%d:%s\n", ln, cur
        }
        END { if (held != "") printf "%d:%s\n", ln, held }' "$1"
}

# One file, so case3 can drive it with a multi-line probe — a form the per-line loop cannot express.
_racy_in_file() {
    local re
    re="$(_racy_re)"
    # No trailing comment filter: `_joined`'s own `/^[ \t]*#/ { next }` already guarantees that no
    # emitted record begins with one, so the filter that used to sit here could not fire — measured
    # by deleting it, every assertion including the two comment FORMS still passed. Two
    # implementations of "a comment is not code", one unreachable, is one more than the next reader
    # should have to reason about.
    _joined "$1" 2>/dev/null | grep -E "$re"
}

_racy_sites() {
    local f
    while IFS= read -r f; do
        _racy_in_file "$f" | sed "s|^|${f}:|"
    done < <(_pipefail_scope "$1")
}

# --- 3. the detector, against every spelling it has to be right about ----------------------
# Same reasoning as case0: a detector nothing tests is a silent exemption one level up. The probe
# is written with `PIPE` where the character belongs and substituted in, so this suite's own source
# never contains the shape and the real scan can never report this file.
case3() {
    local probe="$work/racy.sh" n hit want_hit want_miss re
    re="$(_racy_re)"
    sed 's/PIPE/|/g' >"$probe" <<'FORMS'
  PIPE grep -qE 'x'
printf '%s' "$1" PIPE grep -Eq "$RE"
docker inspect x 2>/dev/null PIPE grep -q true
cat f PIPEgrep -q x
cat f PIPE egrep -q x
cat f PIPE fgrep -q x
cat f PIPE command grep -q x
cat f PIPE& grep -q x
cat f PIPE grep --quiet x
cat f PIPE grep --silent x
printf '%s' "${a[@]}" PIPE grep -qxF -- "$w"
sed -n 1p f PIPE grep -q -- --flag
grep -qE 'x' <<<"$changed"
  PIPEPIPE grep -qF -- "$b" <<<"$(cat x)"
procsub_safe='jqPIPEsedPIPEawkPIPEgrepPIPEcat'
grep -q -- '--print-target' "$gen"
  PIPE grep -vE '^[0-9]+:' PIPE head -1
cat f PIPE grep -c .
cat f PIPE grep -E 'x'
# a comment about PIPE grep -q here
  # PIPE grep -qE 'indented comment'
cat f PIPE sed -n 1p
FORMS
    want_hit=""; want_miss=""
    n=0
    while IFS= read -r line; do
        n=$((n + 1))
        printf '%s\n' "$line" >"$work/one.sh"
        hit="$(_racy_in_file "$work/one.sh")"
        if [ "$n" -le 12 ]; then
            [ -n "$hit" ] || want_hit="$want_hit $n"
        else
            [ -z "$hit" ] || want_miss="$want_miss $n"
        fi
    done <"$probe"
    [ "$n" -eq 22 ] || fail "pipefail: forms-premise" "read $n form(s), expected 22"
    if [ -n "$want_hit" ]; then
        fail "pipefail: forms-miss" "these real spellings of the idiom are not seen, so a script \
using one is silently exempted:$want_hit"
    elif [ -n "$want_miss" ]; then
        fail "pipefail: forms-false" "these safe lines were read as the idiom, which is how a \
guard gets deleted:$want_miss"
    else
        ok "the quiet-grep detector sees every spelling of the idiom, and no safe neighbour"
    fi

    # ...and BOTH continuation directions, which the per-line loop above cannot express.
    # PIPE placeholders here too, for the same reason the FORMS heredoc uses them — and not as
    # hypothetical caution: writing these three probes with literal pipes made the joined detector
    # report THIS FILE, which is the "a check that fails on an unmutated tree" failure the
    # placeholder exists to prevent, arriving the moment the detector got strong enough to see it.
    sed 's/PIPE/|/g' >"$work/two.sh" <<'TWO'
cat f PIPE
    grep -q x
TWO
    sed 's/PIPE/|/g' >"$work/two2.sh" <<'TWO2'
cat f \
    PIPE grep -q x
TWO2
    sed 's/PIPE/|/g' >"$work/two3.sh" <<'TWO3'
cat f PIPE
    sed -n 1p
TWO3
    # A COMMENT ENDING IN `|` DIRECTLY ABOVE A RACY LINE. lib.sh has two such comments, in scope,
    # and the line-joining added this round would have swallowed the code beneath them into a
    # record starting with `#` — narrowing the detector while claiming to widen it.
    sed 's/PIPE/|/g' >"$work/two4.sh" <<'TWO4'
#   states: added PIPE kept PIPE retracted PIPE
cat f PIPE grep -q x
TWO4
    # ...and a comment BETWEEN the halves of a continued pipeline, which bash permits and which a
    # rule that FLUSHED on comments rather than skipping them would break in two and miss.
    sed 's/PIPE/|/g' >"$work/two5.sh" <<'TWO5'
cat f PIPE
    # why we do this
    grep -q x
TWO5
    if [ -z "$(_racy_in_file "$work/two.sh")" ]; then
        fail "pipefail: forms-multiline" "a pipeline whose \`|\` ends the line is not seen, so the \
same pipeline written the other way is exempt"
    elif [ -z "$(_racy_in_file "$work/two2.sh")" ]; then
        fail "pipefail: forms-multiline" "a pipeline continued with a backslash is not seen"
    elif [ -n "$(_racy_in_file "$work/two3.sh")" ]; then
        fail "pipefail: forms-multiline-false" "joining lines made a safe two-line pipeline match"
    elif [ -z "$(_racy_in_file "$work/two4.sh")" ]; then
        fail "pipefail: forms-comment-above" "a racy line under a comment ending in a pipe is not \
seen — the comment swallows it and the record reads as a comment"
    elif [ -z "$(_racy_in_file "$work/two5.sh")" ]; then
        fail "pipefail: forms-comment-within" "a comment between the halves of a continued \
pipeline breaks the join, so the pipeline is not seen"
    else
        ok "and a pipeline split across lines, whichever side of the break the pipe sits on"
    fi
}

# --- 4. ...and the glob reaches all five script directories --------------------------------
# A FLOOR plus a planted file per directory. The floor alone is the mistake case1 already made
# once: a scan pointed at the wrong list reports the same clean result as one pointed at
# everything. `scripts/hooks/` held the real defect and is the directory the container-local scan
# could not see, so "it reaches every directory" is the premise that matters here.
case4() {
    local fake="$work/fake" d n planted found
    for d in scripts scripts/tests scripts/hooks .claude/hooks .container; do
        mkdir -p "$fake/$d"
        printf '%s\n' '#!/usr/bin/env bash' 'set -uo pipefail' \
            "cat f $(printf '%s' '|') grep -q x" >"$fake/$d/planted.sh"
    done
    planted=5
    # ...and one racy file per directory that sets NO shell options, which must NOT be reported.
    # The rule's whole condition is `pipefail`, and a condition nothing tests is the way this
    # guard would quietly become the blanket rule it deliberately is not — or, worse, stay
    # blanket-shaped while reading as conditional. Today's three surviving sites are both
    # `.claude/hooks` scripts, safe for exactly this reason.
    for d in scripts scripts/tests scripts/hooks .claude/hooks .container; do
        printf '%s\n' '#!/usr/bin/env bash' 'set -u' \
            "cat f $(printf '%s' '|') grep -q x" >"$fake/$d/nopipefail.sh"
    done
    found="$(_racy_sites "$fake" | grep -c . || true)"
    [ "$found" = "$planted" ] \
        && ok "and the scan reaches all five script directories, and only where pipefail is set" \
        || fail "pipefail: reach" "planted $planted racy files under pipefail (one per directory) \
and $planted more without it, and the scan found $found rather than $planted — either the glob \
does not cover what this rule claims to, or the pipefail condition is not being applied"

    n="$(_pipefail_scope "$repo_root" | grep -c . || true)"
    [ "$n" -ge 30 ] \
        || fail "pipefail: coverage" "only $n file(s) are in scope, so the scan below asserts \
almost nothing — shell_sources or the pipefail detection has regressed"

    # MEMBERSHIP BY NAME for the sourced half. The floor above is cleared by the three dozen files
    # that declare `pipefail` themselves, so it said nothing at all about the libraries — and that
    # half was broken and silent: it parsed `. "$(dirname "$0")/harness.sh"` down to `$(dirname`,
    # leaving `harness.sh` outside the guard while the doc said otherwise. A floor cannot notice a
    # missing member; only naming it can.
    local missing="" want scope
    scope="$(_pipefail_scope "$repo_root")"
    for want in scripts/lib.sh scripts/tests/harness.sh .container/lib.sh .container/egress-lib.sh; do
        grep -qxF "$repo_root/$want" <<<"$scope" || missing="$missing $want"
    done
    [ -z "$missing" ] \
        && ok "and the sourced libraries are in scope, which is the half that was silently absent" \
        || fail "pipefail: sourced" "these sourced libraries are outside the scan, so a racy line \
in one would run under every suite's pipefail and go unreported:$missing"

    # THE ONE WAY THE CONDITION CAN BE WRONG, checked rather than trusted. An executed script does
    # not inherit its parent's shell options — measured: a `set -uo pipefail` parent running
    # `./child.sh` leaves the child with pipefail OFF — which is what lets this rule exempt the
    # two `.claude/hooks` scripts. But `export SHELLOPTS` propagates them, measured on the same
    # bash: the child then reports pipefail ON. Nothing here exports it and the hooks are invoked
    # as `bash "<path>"`, so the exemption is sound today; a single `export SHELLOPTS` anywhere
    # would make it wrong everywhere at once, silently. That is the shape this whole round is
    # about, so it is a check and not a sentence.
    # PORTABLE BOUNDARIES. This was `\<(SHELL|BASH)OPTS\>`, which is a GNU extension: POSIX ERE
    # leaves a backslash before an ordinary character undefined, and the BSD regex macOS grep uses
    # spells boundaries `[[:<:]]`/`[[:>:]]` and reads `\<` as a literal `<`. The pattern would
    # therefore match nothing on the platform this repo is developed on — and because this guard is
    # satisfied by ABSENCE, matching nothing is indistinguishable from passing. Pattern 8 in this
    # file's own record, paid for three times before; the sibling `_sets_pipefail` was rewritten
    # away from `\<set\>` in this same range, and this was the one that got left.
    #
    # So it is also DRIVEN, against a planted export, rather than only asked. A check whose green
    # is the absence of a match must be shown capable of a match, or it is a guard that cannot
    # fire — which is what the whole of round 24 was about.
    local exporters miss="" false_hit="" n=0 form want
    _shellopts_re() {
        printf '%s' '^[[:space:]]*export[[:space:]]+([^#]*[[:space:]])?(SHELL|BASH)OPTS([[:space:]=]|$)'
    }
    # DRIVEN ON BOTH SIDES, like the grep detector above. One planted spelling proved the pattern
    # was not inert, which is what it was added for — but six of the seven forms it was rewritten
    # to classify, and every must-NOT-match case, could still break with that premise green. This
    # is the one pattern in this file that has already been broken twice: once by the GNU-only
    # `\<`/`\>` boundaries, and once by my own portable rewrite putting `[^#]*` where the variable
    # name starts. A guard satisfied by ABSENCE cannot report its own deadness, so it is the one
    # that most needs its pattern exercised rather than trusted.
    while IFS='|' read -r want form; do
        [ -n "$form" ] || continue
        n=$((n + 1))
        printf '%s\n' "$form" >"$fake/scripts/exporter.sh"
        if [ -n "$(grep -rlE "$(_shellopts_re)" "$fake/scripts/exporter.sh" 2>/dev/null)" ]; then
            [ "$want" = hit ] || false_hit="$false_hit [$form]"
        else
            [ "$want" = miss ] || miss="$miss [$form]"
        fi
    done <<'SPELLINGS'
hit|export SHELLOPTS
hit|export BASHOPTS
hit|  export SHELLOPTS
hit|export SHELLOPTS=posix
hit|export FOO SHELLOPTS
miss|export MYSHELLOPTS
miss|export SHELLOPTSFOO
miss|# export SHELLOPTS
miss|echo export SHELLOPTS
SPELLINGS
    rm -f "$fake/scripts/exporter.sh"
    if [ "$n" -ne 9 ]; then
        fail "pipefail: shellopts-premise" "read $n spelling(s), expected 9"
    elif [ -n "$miss" ]; then
        fail "pipefail: shellopts-premise" "the exported-SHELLOPTS pattern does not match these \
real spellings, and a guard satisfied by absence cannot tell that from a clean tree:$miss"
    elif [ -n "$false_hit" ]; then
        fail "pipefail: shellopts-false" "these lines export nothing and were read as doing so, \
which is how a guard gets loosened until it means nothing:$false_hit"
    fi

    # ...over `shell_sources`, the ONE file list this suite already uses. A hand-written list of
    # three directories is a second answer to "which files are scripts" — sitting beside the guard
    # that exists because two half-lists once disagreed with the defect in the gap. They agree
    # today; they stop agreeing the moment a sixth directory joins `shell_sources`, and this
    # guard's silence would then mean "not looked at" rather than "nothing found".
    exporters=""
    while IFS= read -r form; do
        grep -qE "$(_shellopts_re)" "$form" 2>/dev/null \
            && exporters="$exporters $form"
    done < <(shell_sources "$repo_root")
    [ -z "$exporters" ] \
        && ok "and nothing exports SHELLOPTS, a pattern driven against nine spellings" \
        || fail "pipefail: shellopts" "these files export SHELLOPTS/BASHOPTS, so every script they \
run inherits pipefail and the per-file condition above is no longer the right question:$exporters"
}

# --- 5. the rule itself --------------------------------------------------------------------
case5() {
    local sites count
    sites="$(_racy_sites "$repo_root")"
    count="$(grep -c . <<<"$sites" || true)"
    [ -n "$sites" ] || count=0
    if [ "$count" = 0 ]; then
        ok "no script running under pipefail pipes into a quiet grep"
    else
        fail "pipefail: quiet grep" "$count site(s) feed a quiet grep through a pipe: it exits at \
its first match, the producer dies on the unwritten tail, and pipefail reports the FOUND match as \
a failure (28/30 at 32 KB; scripts/hooks/post-merge shipped it as 'no build-affecting changes'). \
Match a here-string instead: $(sed "s|^$repo_root/||" <<<"$sites" | tr '\n' ' ')"
    fi
}

# ONE `run_cases`, because the harness requires the call to name every defined case — which is how
# it catches a case written and never wired up.
echo "==> scripts/*.sh: a reachable toolchain, and no pipe into a quiet grep"
run_cases case0 case1 case2 case3 case4 case5

finish
