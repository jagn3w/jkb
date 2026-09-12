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
w9="$work/queue-gate"

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
# ONE of the four libraries rests on THIS CLAUSE ALONE: `.container/lib.sh`. Settled by running
# the code rather than reading it — disabling the clause and re-running names exactly that file
# and no other — because this sentence has now been wrong twice in opposite directions. It first
# said `egress-lib.sh` sets `pipefail` itself (it does not, in any sense that matters); corrected,
# it then said BOTH container libraries rest on this clause (they do not).
#
# The truth is stranger than either: `egress-lib.sh` IS admitted by the first clause, on a match
# its author never wrote as a shell option — line 366's `set -euo pipefail` inside a single-quoted
# `bash -c '…'` body in `under_trap`, which is script TEXT. So the file is in scope for a reason
# unrelated to what it does, and the clause that would cover it properly never gets consulted.
# Over-inclusion, therefore safe, and left alone — but not something to write down as though the
# detector understood the file.
#
# What made both wrong answers possible: I checked each with `_sets_pipefail`, the function under
# discussion. Verifying a claim about a detector with that detector is how the prose match two
# paragraphs up survived as long as it did, and it is why the claim above cites a command to run.
_sourced_by_scope() {
    local root="$1" f
    while IFS= read -r f; do
        _sets_pipefail "$f" || ! _has_shebang "$f" || continue
        # `-E`, BECAUSE THE ALTERNATION IS NOT PORTABLE IN A BASIC REGEX. This was written as a
        # BRE with an escaped alternation, and that is a GNU extension: BSD sed — every macOS
        # developer, and any BSD CI runner — reads it as a literal and the expression matches
        # NOTHING. `_sourced_by_scope` then returns the empty set, and every sourced library that
        # has a shebang and does not set `pipefail` itself silently leaves the racy-grep scan.
        #
        # MEASURED with GNU sed 4.9 `--posix`, which disables exactly the extensions BSD lacks:
        # the BRE form matched 0 of 3 real source lines, the `-E` form 3 of 3 both with and
        # without the flag. Found by `./scripts/check.sh` on macOS reporting `.container/lib.sh`
        # as outside the scan — and ONLY that file, because it is the one library of the four
        # that depends on this extraction: `scripts/lib.sh` and `harness.sh` have no shebang and
        # `.container/egress-lib.sh` sets `pipefail` itself, so all three stay in scope by other
        # arms. The membership assertion below is what caught it; the count floor could not.
        sed -E -n 's/^[[:space:]]*(\.|source)[[:space:]]+(.*)/\2/p' "$f" 2>/dev/null \
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

# --- a script that runs git must drop the caller's repository selection first ---------------
# `-C` DOES NOT PROTECT YOU, and that is the whole reason this case exists. An exported
# `GIT_WORK_TREE` outranks the working directory AND `-C`, so a script that carefully passes
# `-C "$REPO"` is still pointed wherever the caller's environment says.
#
# Measured on git 2.51.1, from inside a real repository with `GIT_WORK_TREE=<victim>` exported:
# `git switch feature` wrote the repository's tracked files into <victim>, and `git reset --hard`
# replaced a colliding file there — "MY UNSAVED WORK" became the repository's content, silently.
#
# `scripts/merge-queue.sh` had exactly that: nine bare git calls including two `reset --hard`,
# spawned by the swarm with the developer's environment inherited whole. Six review rounds of this
# branch went past it, because the only guard on this rule scanned `lib.sh` alone and matched only
# the spelling `git -C`. One home, one glob — the same correction the quiet-grep rule needed.
#
# A file satisfies the rule by dropping the selection itself (`unset`/`env -u`), by calling
# `isolate_git`, or by routing every git call through `lib.sh`'s `_git`. Those are the three
# spellings in the tree; a fourth would fail here and should, until somebody adds it deliberately.
_runs_git() {
    # COMMAND SUBSTITUTIONS ARE NOT QUOTED SPANS. The first version stripped `"[^"]*"` before
    # splitting, which removes `x="$(git rev-parse …)"` whole — so a script whose every git call is
    # written that way ran none, as far as this case could tell. MEASURED: a planted
    # `scripts/release.sh` doing `root="$(git rev-parse --show-toplevel)"` then
    # `rm -rf "$root/dist"` — two git calls, no scrub, a destructive operation on git's answer —
    # passed, and the `seen` floor did not move. The form is not exotic: `swarm-status.sh`'s
    # `REPO="$(git rev-parse …)"`, the very line the previous round scrubbed that file FOR, is
    # written exactly like it.
    #
    # So `$(`, backticks and `{` open a fragment before quotes are considered, and only then are
    # the remaining quoted spans blanked — which is still needed for the JSON string in
    # `auto-mode-test.sh`. The keyword strip covers the loop and grouping words, a leading
    # `VAR=value` assignment prefix, and the runners that take a command as an argument.
    #
    # A WRAPPER COUNTS AS AN INVOCATION. `harness.sh`'s `git_q() { git -c … "$@"; }` is how the
    # whole shell-test tree runs git, and by function-name alone that file looked like it never
    # touched git — exempting any future suite that used only the wrapper.
    local out rc
    out="$(awk '
        BEGIN { q = sprintf("%c", 39) }
        /^[[:space:]]*#/ { next }
        {
            # A TRAILING COMMENT IS NOT CODE, and it has to go before the backtick split. These
            # files are dense with prose about git, and a line ending
            #   echo done   # the tree comes from `git rev-parse --show-toplevel`
            # made `scripts/build.sh` — which runs no git and scrubs nothing — fail this case.
            # A false positive here reddens `check.sh` and CI for a comment, and will eject a
            # branch from the merge queue for one once the queue runs these suites.
            #
            # Quote-aware, because a pattern like grep -qE (caret)# is an ordinary line in this
            # tree and cutting at the first # anywhere would truncate it into nonsense. Walk the
            # line, track the quote state, cut at the first # outside quotes. `q` is built with
            # sprintf because this program is itself inside single quotes.
            cut = 0; inq = ""
            for (k = 1; k <= length($0); k++) {
                ch = substr($0, k, 1)
                if (inq == "") {
                    if (ch == "\"" || ch == q) { inq = ch }
                    else if (ch == "#" && (k == 1 || substr($0, k - 1, 1) ~ /[ \t]/)) {
                        cut = k; break
                    }
                } else if (ch == inq) { inq = "" }
            }
            if (cut > 0) { $0 = substr($0, 1, cut - 1) }

            # THREE PASSES, and the order is the whole trick. A command substitution opens a new
            # command CONTEXT while sitting inside quotes, so it must be split out BEFORE quoted
            # spans are blanked; but a JSON string like "Bash(git diff *)" must be blanked before
            # `(` is treated as a separator, or it becomes a fragment beginning `git `. Splitting
            # on `(` first breaks the second; blanking quotes first breaks the first.
            line = $0
            gsub(/\$\(/, "\n", line)
            gsub(/`/, "\n", line)
            n = split(line, outer, "\n")
            for (i = 1; i <= n; i++) {
                frag = outer[i]
                gsub(/"[^"]*"/, "", frag)
                gsub(/'"'"'[^'"'"']*'"'"'/, "", frag)
                gsub(/[;&|(){}]/, "\n", frag)
                m = split(frag, parts, "\n")
                for (j = 1; j <= m; j++) {
                    p = parts[j]
                    sub(/^[[:space:]]+/, "", p)
                    while (p ~ /^(if|then|do|done|else|elif|while|until|exec|time|env|sudo|command|xargs|!)[[:space:]]/) {
                        sub(/^[A-Za-z!]+[[:space:]]+/, "", p)
                        # `env` TAKES OPTIONS, and this loop used to stop at the first one.
                        # After the keyword is stripped from
                        # `env -u GIT_DIR -u GIT_INDEX_FILE git rev-parse`, what is left begins
                        # with `-u`, which is neither a keyword nor `git` — so a one-line
                        # scrubbed call was not a git call as far as this scan could tell, and
                        # its whole file was skipped before any verdict was reached: the
                        # file-level silent exemption this case has now closed three times.
                        # Nothing had shown it because the tree writes the multi-line form
                        # today, where `git` opens a line of its own. The `oneline` probe in
                        # case7 holds it. NO APOSTROPHES IN THIS BLOCK: the awk program around
                        # it is single-quoted, and one ends it.
                        while (p ~ /^-[uiS][[:space:]]+[A-Za-z_][A-Za-z0-9_]*[[:space:]]+/) {
                            sub(/^-[uiS][[:space:]]+[A-Za-z_][A-Za-z0-9_]*[[:space:]]+/, "", p)
                        }
                    }
                    while (p ~ /^[A-Za-z_][A-Za-z0-9_]*=[^[:space:]]*[[:space:]]+/) {
                        sub(/^[A-Za-z_][A-Za-z0-9_]*=[^[:space:]]*[[:space:]]+/, "", p)
                    }
                    if (p ~ /^git([[:space:]]|$)/) { print NR; exit }
                }
            }
        }' "$1")"
    rc=$?
    # A FILE THIS CANNOT READ IS NOT A FILE WITHOUT GIT. Every other arm of case6 keys off an
    # empty answer meaning "runs no git, nothing to check", so an awk that never ran produced
    # the most permissive verdict there is: a silent, file-level exemption from the only check
    # that asks this question at all. Measured on mawk 1.3.4: a file that cannot be opened exits 2
    # with an empty stdout, indistinguishable from a clean parse finding nothing. Binary content
    # is NOT this case; mawk reads it without complaint and returns 0.
    #
    # ONE ARRIVAL PATH, not the three an earlier version of this comment listed. `shell_sources`
    # guards every candidate with `[ -f "$f" ] || continue`, and `-f` follows symlinks, so a
    # dangling symlink never gets here; an extensionless file is opened again by the shebang
    # sniff. A name ending `.sh` short-circuits ahead of that sniff and is emitted unopened, so an
    # unreadable one is the single shape that reaches this function — and the one case7 probes.
    #
    # Reported as its own word rather than as "runs git", so the failure names the real problem
    # instead of sending somebody to add a scrub to a file they cannot open.
    if [ "$rc" -ne 0 ]; then printf 'unreadable\n'; return 0; fi
    printf '%s\n' "$out"
}

# The file must KNOW about the selection, by any of the three spellings the tree uses. Deliberately
# file-level and not per-call: `scripts/hooks/post-merge` makes BOTH kinds of ask on purpose — an
# ambient `git rev-parse --show-toplevel` to learn what tree it is standing in, and a scrubbed
# `env -u …` one to learn which repository that tree belongs to — and telling those apart is the
# entire subject of that file. A check that demanded every call be scrubbed would be wrong about
# the one script that has thought hardest about this.
# EVERY NAME, not one token. This asked only for `GIT_WORK_TREE`, so the six-name widening that
# round 30 made in three production shell files was pinned by nothing: measured, reverting it in
# `merge-queue.sh`, `swarm-status.sh` and `lib.sh`'s `_git` all at once left all five suites at 0
# FAIL. The three that say which PART of a repository are exactly the ones round 28 measured
# rewriting a victim's index, so a check that stops at the first name is a check for the harm this
# rule no longer considers the whole harm.
#
# The set is read from `gitrepo::REPO_SELECTION_VARS` rather than written here, so a seventh name
# added in Rust makes these shell files fail until they carry it too — the cross-language relation
# `case_rust_twin` gives the fixtures, applied to the production scripts.
_selection_vars() {
    sed -n '/^pub(crate) const REPO_SELECTION_VARS/,/^\];/p' \
        "$repo_root/crates/jkb-cli/src/gitrepo.rs" | grep -oE '"GIT_[A-Z_]+"' | tr -d '"'
}

# A COUNT PREMISE, because an empty extraction is a PASS here and not a failure. `_selection_vars`
# reads a Rust constant with a `sed` range anchored on `^pub(crate) const REPO_SELECTION_VARS`; a
# visibility change to `pub`, a rename, a module move or an attribute line above it makes that
# range match nothing, `_first_scrub_line`'s name loop runs zero times, its `END` block finds
# nothing missing and prints `0`, and every file scrubs "at line 0" — above its first git call,
# whatever that is. Measured: with that one-word edit and this guard removed, case6 reports
# "ok … (8)" while `merge-queue.sh` carries no scrub at all.
#
# The sibling `case_rust_twin` got exactly this premise in the same commit, for exactly this
# reason, and this one did not. It is no longer the only thing holding it: case7 builds its
# probes from the same extraction, so an empty one also makes `late.sh` and `bare.sh` report
# `scrubbed` and fails there too. Measured, with this guard disabled — two assertions red, not
# eight files silently exempt.
_require_selection_vars() {
    local n
    n="$(_selection_vars | grep -c .)"
    [ "$n" -ge 6 ] && return 0
    fail "gitenv: vars-premise" "read $n name(s) from REPO_SELECTION_VARS in gitrepo.rs, expected \
at least 6 — the extraction is broken, not the code, and an empty list would make every script \
below comply vacuously"
    return 1
}

# THE LINE BY WHICH THE SCRUB IS COMPLETE, or `none` if it never is. Prints the LAST of the six
# names' drop lines, because six names dropped is one event and it has not happened until the
# sixth one has: a file that drops one name at the top and the other five below its first git
# call is `late`, and taking the first line would call it scrubbed.
#
# CONTINUATIONS ARE JOINED, because `unset A B C \` + newline + `D E F` is how all three
# production scripts spell it and the second physical line carries no `unset` keyword — unjoined,
# five of the six names are never seen at all and the file reads as `exposed`. Measured: deleting
# the join line turns case7's `cont.sh` probe from `scrubbed` into `exposed`.
#
# The number reported for a joined line is where it BEGAN, and the previous answer — where it ends
# — was wrong in the one shape that matters most. I dismissed the distinction on the grounds that
# "a git call cannot sit between the two halves of a continuation". It can BE the second half:
#
#     root="$(env -u GIT_DIR ... \
#             -u GIT_INDEX_FILE ... \
#             git rev-parse --show-toplevel)"
#
# is the canonical inline scrub, and `_runs_git` reports the git call on the line the command
# appears on, which is also the line the joined scrub ends on. `[ n -lt n ]` is false, so a fully
# scrubbed call verdicts `late` — reported as exposed for scrubbing correctly. `scripts/lib.sh`'s
# `_git` and `post-merge`'s `common_of` are both this shape and escaped only because they reach an
# earlier arm. Taking the START line is right on its own terms too: the drop is what the reader is
# being asked about, and it starts where the `unset`/`env -u` keyword is.
#
# Comments are skipped where they are read, not by filtering the file first, because a filtered
# file has no line numbers and a line number is the entire point of this helper.
_first_scrub_line() {
    awk -v names="$(_selection_vars | tr '\n' ' ')" '
        BEGIN { nn = split(names, N, " ") }
        {
            if (acc == "" && $0 ~ /^[[:space:]]*#/) { next }
            if (acc == "") { start = NR }
            line = (acc == "") ? $0 : acc " " $0
            if (line ~ /\\$/) { sub(/\\$/, "", line); acc = line; next }
            acc = ""
            for (i = 1; i <= nn; i++) {
                if (i in seen) { continue }
                re = "(^|[ \t])(unset|-u)([ \t]+[A-Za-z_]+)*[ \t]+" N[i] "([ \t]|$)"
                if (line ~ re) { seen[i] = start }
            }
        }
        END {
            mx = 0
            for (i = 1; i <= nn; i++) {
                if (!(i in seen)) { print "none"; exit }
                if (seen[i] > mx) { mx = seen[i] }
            }
            print mx
        }' "$1"
}

# THE VERDICT, as a word, because "compliant" was hiding three different things and two of them
# were not compliance. `$2` is the line of the file's first git call, from `_runs_git`.
#
#   wrapper    the file defines `_git()` and that definition drops all six, or it routes its
#              calls through a wrapper checked elsewhere; ordering is not the question
#   leaky-wrapper  the file defines `_git()` and that definition does NOT drop all six — the one
#              failure every caller of it inherits silently
#   scrubbed   the file drops all six names itself, and does it BEFORE its first git call
#   ambient    the file declares that git itself hands it the selection and reading it is the job
#   late       drops all six, but only after a git call has already run under the caller's
#   exposed    never drops them at all
#
# `late` is the arm this was missing. The check asked whether the six names appear ANYWHERE, so a
# script that ran `git rev-parse --show-toplevel` at line 10 and scrubbed at line 200 passed — and
# line 10 is the call whose answer everything downstream is scoped to. case1 has made exactly this
# correction for `cargo` already ("a source line BELOW the first invocation reads as compliant and
# is not"); this is the same rule, arrived at from the other end.
# THE FILE THAT DEFINES THE WRAPPER IS THE ONE FILE THAT MUST NOT BE TAKEN ON TRUST. `wrapper`
# used to be granted by grepping for the wrapper's NAME, which is the proxy-versus-claim mistake
# `install_chainer` documents at length one directory over: matching `^_git()` says a wrapper is
# here, not that it scrubs. MEASURED: strip five of the six names from `scripts/lib.sh`'s `_git`
# and this whole suite stayed green — the one wrapper every other shell call site routes through
# was the only file nothing asked. That is exactly the round-30 drift this case exists to close,
# and the comment above claiming a seventh name "makes these shell files fail until they carry it
# too" was false of the most important one.
#
# So the definition is asked the same question every other file is asked: does it drop all six?
# `isolate_git` keeps the name-grep because its own scrub IS pinned, by `harness.test.sh`'s
# `case_rust_twin`, against Rust's `MUST_DROP`. A file that merely CALLS either wrapper is taken
# on trust deliberately — the trust is now backed by a check on the thing being trusted.
_defines_git_wrapper() { grep -qE '^_git\(\)' <<<"$(grep -vE '^[[:space:]]*#' "$1")"; }

_wrapper_scrubs() {
    local f="$1" body rc
    body="$work/wrapper-body.$$"
    sed -n '/^_git()/,/^}/p' "$f" >"$body"
    [ "$(_first_scrub_line "$body")" != none ]; rc=$?
    rm -f "$body"
    return "$rc"
}

_selection_verdict() {
    local f="$1" gitline="$2" scrubline
    # A LINE NUMBER OR NOTHING DOING. The order comparison below is `[ "$scrubline" -lt
    # "$gitline" ]`, and `test` handed a non-numeric operand writes to stderr and returns 2 —
    # which `&&`/`||` reads as false, so the file would be reported `late` on the strength of a
    # bad argument rather than a bad scrub. `_runs_git` answers with a number, an empty string
    # (no git here) or `unreadable`, and only the first is a question this function can answer.
    case "$gitline" in
        ''|*[!0-9]*) printf 'no-git-line\n'; return 0 ;;
    esac
    # A wrapper that scrubs, or a suite-wide isolation call, satisfies it for the whole file —
    # but only as CODE. Grepping the raw file meant a comment mentioning `isolate_git` or
    # `_git -C` exempted a script from the six-name requirement, and every one of these files is
    # heavily commented about exactly those names.
    if _defines_git_wrapper "$f"; then
        _wrapper_scrubs "$f" && printf 'wrapper\n' || printf 'leaky-wrapper\n'
        return 0
    fi
    if grep -qE 'isolate_git|_git[[:space:]]+-C' \
        <<<"$(grep -vE '^[[:space:]]*#' "$f")"; then printf 'wrapper\n'; return 0; fi
    scrubline="$(_first_scrub_line "$f")"
    [ "$scrubline" != none ] || { printf 'exposed\n'; return 0; }
    # THE ONE DECLARED EXCEPTION, and it has to be declared. `scripts/hooks/post-merge` is
    # INVOKED BY GIT with the repository selection already set, and reading it is that file's
    # whole subject: it makes an ambient ask to learn what tree it was handed and a scrubbed one
    # to learn which repository that tree belongs to. Scrubbing at its top would delete the first
    # question. It passed this case anyway — on the strength of `common_of`'s `env -u` list, a
    # helper that has nothing to do with its earlier bare calls — so the file was credited for a
    # scrub that covers one call site out of many, by accident rather than by decision.
    #
    # The marker exempts a file from the ORDER rule only. The six names must still all be
    # dropped somewhere in it, which is what keeps `post-merge` in step when a seventh name is
    # added to `REPO_SELECTION_VARS`. case6 counts these and says how many there are, so a second
    # file quietly acquiring the marker is visible in the ok line rather than inside the check.
    if grep -qE '^[[:space:]]*#[[:space:]]*case6-ambient:' "$f"; then printf 'ambient\n'; return 0; fi
    [ "$scrubline" -lt "$gitline" ] && { printf 'scrubbed\n'; return 0; }
    # SAME LINE IS NOT LATE, when the scrub is part of the call. `env -u GIT_DIR … git rev-parse`
    # written on one line puts both on line n, and strict `<` called it `late` — a call scrubbed
    # in the strongest way there is, reported as exposed. But the line number alone cannot settle
    # it, because `git rev-parse; unset GIT_DIR` is also one line and is genuinely late. So for
    # this one case the position within the line decides: the scrub must appear before the git
    # token with no command separator between them, which is the difference between `env -u X git`
    # and `git …; unset X`. Both spellings are probed in case7.
    [ "$scrubline" -eq "$gitline" ] && _scrub_precedes_on_line "$f" "$gitline" \
        && printf 'scrubbed\n' || printf 'late\n'
    return 0
}

# _scrub_precedes_on_line <file> <line> — true when, on that line, a selection drop appears to the
# left of the git token and nothing separates the two into different commands.
_scrub_precedes_on_line() {
    awk -v ln="$2" '
        NR != ln { next }
        {
            gp = match($0, /(^|[^A-Za-z_-])git[[:space:]]/)
            if (gp == 0) { exit 1 }
            pre = substr($0, 1, gp)
            if (pre !~ /(unset|-u)[ \t]+GIT_[A-Z_]+/) { exit 1 }
            if (pre ~ /[;&|]/) { exit 1 }      # a separator means a different command
            exit 0
        }
        END { if (NR < ln) exit 1 }' "$1"
}

# THE FILES ALLOWED TO DECLARE THEMSELVES AMBIENT, written down here rather than counted.
#
# The marker was introduced with a count in the ok line as its only control, and I claimed in a
# comment that the count WAS the control. It is not: a number in a string no assertion reads is
# not a check. MEASURED — move `merge-queue.sh`'s `unset` block to the end of the file, leave a
# `# case6-ambient:` line where it was, and the suite stays green printing "(8 script(s), 2
# declared ambient)". That file runs git against whatever branch and worktree the swarm hands it,
# and its whole trace of opting out would be one digit nobody diffs.
#
# So the exemption set is pinned the way this tree pins its other cross-file lists: literally, and
# by equality, so ACQUIRING the marker fails as loudly as losing it. Adding a file here is a
# deliberate edit in a review, which is the only way an exemption should ever be granted.
AMBIENT_ALLOWED="scripts/hooks/post-merge"

case6() {
    local f bad="" unread="" ambient="" seen=0 ordered=0 wrapped=0 gitline verdict
    _require_selection_vars || return
    while IFS= read -r f; do
        gitline="$(_runs_git "$f")"
        if [ "$gitline" = unreadable ]; then
            unread="$unread ${f#"$repo_root"/}"
            continue
        fi
        [ -n "$gitline" ] || continue
        seen=$((seen + 1))
        verdict="$(_selection_verdict "$f" "$gitline")"
        case "$verdict" in
            wrapper) wrapped=$((wrapped + 1)) ;;
            scrubbed) ordered=$((ordered + 1)) ;;
            ambient) ambient="$ambient ${f#"$repo_root"/}" ;;
            *) bad="$bad ${f#"$repo_root"/}:$gitline($verdict)" ;;
        esac
    done < <(shell_sources "$repo_root")
    if [ -n "$unread" ]; then
        fail "gitenv: unreadable" "these files are in the gate's own file list and could not be \
read, so every check below skipped them silently rather than failing:$unread"
    elif [ "$seen" -lt 5 ]; then
        fail "gitenv: coverage" "only $seen script(s) were found to run git at all, so this case \
asserts almost nothing — the glob or the git detector has regressed"
    elif [ -n "$bad" ]; then
        fail "gitenv: unscrubbed" "these scripts run git at the line shown without the caller's \
repository selection having been dropped first, so an exported GIT_WORK_TREE redirects them. A -C \
flag does not help, it is outranked. \`exposed\` never drops the six names; \`late\` drops them, but \
below a git call that has already answered about the wrong repository; \`leaky-wrapper\` means the \
file DEFINES _git and that definition does not drop all six, which every caller of it inherits \
without knowing. Move the unset above the first call, fix the wrapper where it is defined, or — \
if git itself hands this file the selection and reading it is the point — say so with a \
\`# case6-ambient:\` line and add the file to AMBIENT_ALLOWED:$bad"
    elif [ "${ambient# }" != "$AMBIENT_ALLOWED" ]; then
        fail "gitenv: ambient-set" "the files declaring \`# case6-ambient:\` are '${ambient# }' \
but the reviewed set is '$AMBIENT_ALLOWED'. That marker exempts a file from the ordering rule, so \
acquiring one is a decision, not a detail — add it to AMBIENT_ALLOWED in the same change that \
adds it to the file, and say in docs/git-hooks-installer.md why that file reads the selection git \
hands it"
    else
        # THE SENTENCE CLAIMS ONLY WHAT WAS ASKED. It used to say every script "drops the caller's
        # repository selection first, above that call" over a population of which most were never
        # order-checked at all — 5 of 8 satisfy the rule through a wrapper and 1 is ambient, so
        # the ordering comparison ran on two files while the sentence spoke for eight. The
        # breakdown is the honest form, and it also makes the shape visible: if `ordered` ever
        # reaches 0 because everything routed through `_git`, the line says so instead of reading
        # exactly as it does today. The arms stay covered either way — case7 drives each of them
        # against a planted file, which is where the logic is pinned, not here.
        ok "every script that runs git drops the caller's repository selection first ($seen: \
$wrapped via a checked wrapper, $ordered scrubbed above the call, $(printf '%s' "$ambient" | wc -w) \
declared ambient)"
    fi
}

# --- 7. every verdict case6 can reach, driven against a file that produces it ---------------
# Same reasoning as case0, which says it for the cargo detector: a detector nothing tests is a
# silent exemption one level up. case6 grew from a boolean to five words and three of those words
# are new, so three arms of it had never been observed to fire. `late` in particular was written
# to catch a shape no file in the tree has, which is exactly the arm that rots.
#
# The marker is spelled with a placeholder and substituted in, so this suite's own source never
# contains the literal — the same reason case3 writes `PIPE` where the pipe belongs. Without that,
# a file explaining the marker would claim it, which is the comment-versus-code confusion
# `_selection_verdict` already had to be corrected for once.
#
# The six names come from `_selection_vars`, not from a list written here: a probe that drops a
# hard-coded six would stop being a `scrubbed` probe the day a seventh name is added, and would
# fail as `late` while telling the reader nothing about ordering.
case7() {
    local d="$work/verdicts" names first_git v got want bad=""
    mkdir -p "$d"
    names="$(_selection_vars | tr '\n' ' ')"

    local g
    g="$(printf 'g%sit' '')"          # never the literal, for the reason in the header above
    # THE WRAPPER PROBE DEFINES THE WRAPPER, because a file that only CALLS `_git` and never
    # spells the command has no first-git line at all, and case6 skips it before any verdict is
    # reached — the first version of this probe was that file, and `_selection_verdict`'s refusal
    # of a missing line number is what said so. `lib.sh` is the real shape: it defines the
    # wrapper, so the bare call inside the definition is what the scan finds.
    #
    # AND IT SCRUBS, because the second version of this probe did not — `_git() { command git
    # "$@"; }` — and case7 asserted its verdict was `wrapper`, which pinned the hole that let a
    # `lib.sh` stripped to one name pass. A probe is an assertion about what is correct, so a
    # permissive probe does not merely miss a defect, it certifies one. Its leaky sibling below
    # is the same file with the scrub removed, and it must NOT come back `wrapper`.
    {
        printf '#!/bin/sh\n_%s() {\n    env \\\n' "$g"
        _selection_vars | sed 's/^/        -u /;s/$/ \\/'
        printf '        %s "$@"\n}\n_%s -C "$r" rev-parse --show-toplevel\n' "$g" "$g"
    } >"$d/wrap.sh"
    printf '#!/bin/sh\n_%s() { command %s "$@"; }\n_%s -C "$r" rev-parse --show-toplevel\n' \
        "$g" "$g" "$g" >"$d/wrap-leaky.sh"
    printf '#!/bin/sh\nunset %s\n%s rev-parse --show-toplevel\n' "$names" "$g" >"$d/top.sh"
    printf '#!/bin/sh\n%s rev-parse --show-toplevel\nunset %s\n' "$g" "$names" >"$d/late.sh"
    printf '#!/bin/sh\n%s rev-parse --show-toplevel\n' "$g" >"$d/bare.sh"
    sed -e 's/MARK/case6-ambient/' -e 's/GITCMD/git/' >"$d/amb.sh" <<'AMB'
#!/bin/sh
# MARK: GITCMD hands this one its selection on purpose.
GITCMD rev-parse --show-toplevel
AMB
    printf 'unset %s\n' "$names" >>"$d/amb.sh"
    # The marker excuses the ORDER and nothing else: a file claiming it while dropping none of
    # the names is still exposed. Otherwise the marker would be a way to turn the case off.
    sed -e 's/MARK/case6-ambient/' -e 's/GITCMD/git/' >"$d/amb-bare.sh" <<'AMBBARE'
#!/bin/sh
# MARK: claims the exemption but drops nothing.
GITCMD rev-parse --show-toplevel
AMBBARE
    # A CONTINUATION IS ONE EVENT, and the second line of one carries no `unset` keyword. This is
    # how all three production scripts spell the drop, so without the join five of the six names
    # go unseen and every one of them reads as `exposed`. Split at the name that must land last.
    {
        printf '#!/bin/sh\nunset %s \\\n' "${names%% *}"
        printf '      %s\n' "${names#* }"
        printf '%s rev-parse --show-toplevel\n' "$g"
    } >"$d/cont.sh"

    # THE INLINE SCRUB, where the git command IS the continuation's second half. This is the
    # shape `lib.sh`'s `_git` and `post-merge`'s `common_of` are both written in, and with
    # `_first_scrub_line` reporting the END of a joined line it verdicted `late` — a correctly
    # scrubbed call reported as exposed. Neither real file showed it, because both reach an
    # earlier arm; nothing covered it until this probe.
    {
        printf '#!/bin/sh\nroot="$(env \\\n'
        _selection_vars | sed 's/^/    -u /;s/$/ \\/'
        printf '    %s rev-parse --show-toplevel)"\n' "$g"
    } >"$d/inline.sh"

    # ...and the same scrub written on ONE line, which `_runs_git` did not see as a git call at
    # all until the `env -u NAME` strip above. Its verdict is not the point; that it reaches a
    # verdict is.
    printf '#!/bin/sh\nroot="$(env %s %s rev-parse --show-toplevel)"\n' \
        "$(_selection_vars | sed 's/^/-u /' | tr '\n' ' ')" "$g" >"$d/oneline.sh"

    # ...and the one-line form where the scrub comes AFTER the call, which is genuinely late and
    # must not be rescued by sharing a line number with it.
    printf '#!/bin/sh\n%s rev-parse --show-toplevel; unset %s\n' "$g" "$names" >"$d/sameline-after.sh"

    for v in wrap:wrapper wrap-leaky:leaky-wrapper top:scrubbed late:late bare:exposed \
             amb:ambient amb-bare:exposed cont:scrubbed inline:scrubbed oneline:scrubbed \
             sameline-after:late; do
        want="${v#*:}"
        first_git="$(_runs_git "$d/${v%%:*}.sh")"
        got="$(_selection_verdict "$d/${v%%:*}.sh" "$first_git")"
        [ "$got" = "$want" ] || bad="$bad ${v%%:*}.sh(want=$want got=$got)"
    done
    if [ -n "$bad" ]; then
        fail "gitenv: verdicts" "case6's verdict for these planted files is not the one the file \
was written to produce, so an arm of this check is not the arm it reports:$bad"
    else
        ok "every verdict case6 reaches is produced by a file written to produce it"
    fi

    # ...and the answer for a file that cannot be read at all, which used to be "runs no git".
    #
    # A `*.sh` AT MODE 000, which is the only shape that can actually arrive. The first version of
    # this probe used a dangling symlink, and `shell_sources` never emits one: every candidate is
    # guarded by `[ -f "$f" ] || continue` and `-f` follows symlinks. For an extensionless file
    # the shebang sniff opens it too, so that is filtered as well. What is NOT filtered is a file
    # ending `.sh` — the extension short-circuits ahead of the sniff and emits it unopened — so an
    # unreadable one reaches `_runs_git` and nothing else. Driving the arm with a file the gate
    # drops proved the arm worked on an input it will never see.
    #
    # Skipped as root, which can read a 000 file, so the probe would silently assert nothing.
    printf '#!/bin/sh\n%s status\n' "$g" >"$d/locked.sh"
    chmod 000 "$d/locked.sh"
    if [ "$(id -u)" = 0 ]; then
        ok "(skipped as root: mode 000 does not stop root reading, so this arm cannot be probed here)"
    else
        got="$(_runs_git "$d/locked.sh" 2>/dev/null)"
        if [ "$got" = unreadable ]; then
            ok "and a file the scan cannot open is named, not silently exempted"
        else
            fail "gitenv: unreadable-arm" "an unreadable .sh in the file list answered '$got', \
which case6 reads as 'runs no git' and skips — the file-level exemption this arm exists to close"
        fi
    fi
    chmod 644 "$d/locked.sh"
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
    # `export` IS NOT THE ONLY SPELLING — but only two of the other three actually leak, and the
    # difference was measured rather than assumed the second time. `bash -c 'set -uo pipefail;
    # <spelling>; ./child.sh'`, asking the child whether `pipefail` is on:
    #
    #     export SHELLOPTS       ON        readonly SHELLOPTS     off
    #     declare -x SHELLOPTS   ON        declare SHELLOPTS      off
    #     typeset -x SHELLOPTS   ON
    #
    # The first version of this paragraph said all four "set the export attribute identically —
    # measured", having measured `declare -x` alone and generalised. `readonly` sets no export
    # attribute at all.
    #
    # The pattern still matches all four keywords, DELIBERATELY: it is a line-shaped check, the
    # cost of a match is one loud failure a reader can dismiss in a second, and the cost of a miss
    # is an exemption that quietly stops being true. So the table below asserts what the PATTERN
    # does, not what leaks — `readonly SHELLOPTS` is a hit here and harmless in reality, and that
    # is written down rather than left for the next reader to re-derive.
    #
    # Still LINE-INITIAL, and that bound is stated rather than hidden: `[ -n "$x" ] && export
    # SHELLOPTS` is not matched. Widening to "anywhere on the line" would match the word inside
    # this very comment and inside the spellings table below, which is the self-matching problem
    # the PIPE placeholder exists for one guard over — so the bound is the honest trade, and the
    # message says what is detected instead of claiming completeness.
    # QUOTES AROUND THE NAME. `export "SHELLOPTS"` exports exactly as the bare form does —
    # measured, child pipefail ON for both `"` and `'` — and the prefix group must end in
    # whitespace, so a quote sitting immediately before the name could not be absorbed and the
    # spelling walked past. The round that added `readonly`, which measurably does NOT leak, left
    # one that does outside.
    #
    # The line-continuation form (`export FOO \` / newline / `SHELLOPTS`) also leaks and is still
    # not matched; that is stated in the ok message rather than papered over, because this guard's
    # green is an absence and an overstated claim about it is worth more than the gap.
    _shellopts_re() {
        printf '%s%s' '^[[:space:]]*(export|declare|typeset|readonly)[[:space:]]+' \
                      '([^#]*[[:space:]])?["'"'"']?(SHELL|BASH)OPTS["'"'"']?([[:space:]=]|$)'
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
hit|declare -x SHELLOPTS
hit|typeset -x BASHOPTS
hit|readonly SHELLOPTS
hit|declare SHELLOPTS
hit|export "SHELLOPTS"
hit|export 'SHELLOPTS'
miss|export FOO  # SHELLOPTS mentioned only in a trailing comment
miss|export MYSHELLOPTS
miss|export SHELLOPTSFOO
miss|# export SHELLOPTS
miss|echo export SHELLOPTS
SPELLINGS
    rm -f "$fake/scripts/exporter.sh"
    if [ "$n" -ne 16 ]; then
        fail "pipefail: shellopts-premise" "read $n spelling(s), expected 16"
    elif [ -n "$miss" ]; then
        fail "pipefail: shellopts-premise" "the SHELLOPTS pattern does not match these spellings it is \
meant to classify, and a guard satisfied by absence cannot tell that from a clean tree:$miss"
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
        && ok "and no line-initial, single-line export/declare/typeset/readonly gives it away" \
        || fail "pipefail: shellopts" "these files match the line-initial \
export/declare/typeset/readonly + SHELLOPTS shape. If it really exports (bare \`readonly\` and \
\`declare\` without -x do NOT), every script they run inherits pipefail and the per-file \
condition above is no longer the right question:$exporters"
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

# --- 8. no BASIC regex leans on a GNU extension --------------------------------------------
# The defect this exists for shipped on this branch and could only be found ON macOS:
# `_sourced_by_scope` extracted `source` lines with a BRE alternation, which is a GNU extension.
# BSD sed reads it as a literal, the expression matches nothing, `_sourced_by_scope` returns the
# empty set — so on every macOS machine the sourced-library half of the pipefail scan covered
# nothing at all, while this suite stayed green on Linux for four rounds.
#
# MEASURED with GNU sed 4.9 `--posix`, which turns off exactly the extensions BSD lacks: the BRE
# form matched 0 of 3 real source lines, the `-E` form 3 of 3 with and without the flag.
#
# A GUARD RATHER THAN A CORRECTED SITE, because the property is "this tree means the same thing
# on both seds" and one fixed expression does not hold it — the next person writing a basic
# regex has no way to know. The other two GNU-only constructs are flagged with it. A line that
# asks for an extended or Perl regex is exempt: there the escaped plus is a literal plus, which
# is legitimate and is what `.claude/hooks/block-raw-cargo.sh` uses. `grep -F` is exempt for the
# same reason — its backslash is data, and over-refusing innocent code is how a guard gets
# deleted rather than obeyed.
#
# Every construct is built from a variable rather than written, so this file never contains the
# shapes it scans for — case3's PIPE rule, applied again.
_gnu_bre_sites() {
    local root="$1" f
    while IFS= read -r f; do
        awk -v name="${f#"$root"/}" '
            /^[ \t]*#/ { next }
            {
                if ($0 !~ /(^|[^A-Za-z_-])(sed|grep)[ \t]/) { next }
                if ($0 ~ /(^|[^A-Za-z_-])(sed|grep)[ \t]+(-[A-Za-z]*[ErPF])/) { next }
                bs = sprintf("%c", 92)
                for (k = 1; k < length($0); k++) {
                    if (substr($0, k, 1) == bs) {
                        c = substr($0, k + 1, 1)
                        if (c == "|" || c == "+" || c == "?") { printf "%s:%d\n", name, NR; next }
                    }
                }
            }' "$f"
    done < <(shell_sources "$root")
}

case8() {
    local probe="$work/bre" sites n hit miss
    mkdir -p "$probe/scripts"
    # The detector first, against the spellings it has to be right about. BS is substituted in, so
    # this suite's own source carries none of them.
    sed 's/BS/\\/g' >"$probe/scripts/hits.sh" <<'HITS'
sed -n 's/^\(aBS|b\)/x/p' f
grep -n 'aBS|b' f
sed 's/xBS+/y/' f
sed -n 's/xBS?/y/p' f
HITS
    sed 's/BS/\\/g' >"$probe/scripts/miss.sh" <<'MISS'
sed -E -n 's/^(a|b)/x/p' f
grep -qE '(a|b)BS+c' f
grep -F 'aBS|b' f
sed -n 's/xBS{1,BS}/y/p' f
echo 'no command here at all'
MISS
    hit="$(_gnu_bre_sites "$probe" | grep -c 'hits\.sh' || true)"
    miss="$(_gnu_bre_sites "$probe" | grep -c 'miss\.sh' || true)"
    if [ "$hit" -lt 4 ]; then
        fail "bre: forms-miss" "the detector saw $hit of the 4 GNU-only spellings, so a basic \
regex that matches nothing on BSD sed would ship unreported"
    elif [ "$miss" != 0 ]; then
        fail "bre: forms-false" "$miss portable line(s) were read as GNU-only, which is how a \
guard gets deleted rather than obeyed"
    else
        ok "the GNU-extension detector sees every unportable spelling, and no portable neighbour"
    fi

    # ...and then the tree it is here to hold.
    sites="$(_gnu_bre_sites "$repo_root")"
    n="$(grep -c . <<<"$sites" || true)"
    [ -n "$sites" ] || n=0
    if [ "$n" = 0 ]; then
        ok "no basic regex in this tree depends on a GNU extension, so every scan means the same under BSD sed"
    else
        fail "bre: gnu-only" "$n site(s) use a GNU-only construct in a BASIC regex. BSD sed and \
BSD grep read it as a literal, so the expression matches nothing and whatever it feeds silently \
covers nothing — green on Linux, blind on macOS. Ask for an extended regex with -E: $(tr '\n' ' ' <<<"$sites")"
    fi
}

# --- 9. the gate half that had no oracle ----------------------------------------------------
# `merge-queue.sh`'s `_shell_suites_pass` is what makes a red shell suite able to block a landing
# — and nothing anywhere exercised it. `grep -rn merge-queue scripts/tests/` returned only case6's
# comments about the environment scrub. So a branch reverting the queue's gate to
# `build.sh && test.sh` passed every check in this repository and would have landed, restoring
# the cargo-only gate that function exists to replace. That is the same "guard with no oracle"
# this tree has already paid for once, in cb3254a.
#
# The functions are read out of the script rather than duplicated here, for the reason
# `_selection_vars` reads REPO_SELECTION_VARS out of the Rust: a copy is a second answer that
# drifts. Extraction is by name, so renaming either function reddens this case rather than
# silently exempting it — which is the premise `_require_selection_vars` exists to hold for case6,
# applied here.
_queue_fn() { sed -n "/^$1() {/,/^}/p" "$repo_root/scripts/merge-queue.sh"; }

case9() {
    local d="$w9" src bad=""
    # A LITERAL NEWLINE. `$(printf '\n')` is not one: command substitution strips trailing
    # newlines, so the two extracted functions were joined into `}_suite_floor() {` on one line
    # and the eval defined neither.
    src="$(_queue_fn _shell_suites_pass)
$(_queue_fn _suite_floor)"
    case "$src" in
        *_shell_suites_pass*_suite_floor*) ;;
        *) fail "queue-gate: premise" "could not read _shell_suites_pass and _suite_floor out of \
scripts/merge-queue.sh — they were renamed or moved, and everything below would pass vacuously"
           return ;;
    esac
    eval "$src"

    plant() {   # plant <dir> <count> <failing-index|0>
        local root="$1" n="$2" bad_i="$3" i
        rm -rf "$root"; mkdir -p "$root/scripts/tests"
        for i in $(seq 1 "$n"); do
            if [ "$i" = "$bad_i" ]; then printf '#!/bin/sh\nexit 1\n' > "$root/scripts/tests/s$i.test.sh"
            else printf '#!/bin/sh\nexit 0\n' > "$root/scripts/tests/s$i.test.sh"; fi
        done
    }
    try() {     # try <label> <count> <failing> <floor> <want-rc>
        local got
        plant "$d/$1" "$2" "$3" >/dev/null 2>&1
        ( cd "$d/$1" && _shell_suites_pass "$4" ) >/dev/null 2>&1
        got=$?
        [ "$got" = "$5" ] || bad="$bad $1(want=$5 got=$got)"
    }
    try all-green 5 0 5 0      # every suite passes and the floor is met
    try one-red   5 3 5 1      # a single red suite fails the gate — the whole point
    try too-few   3 0 5 1      # suites deleted: refused even though every one that ran passed
    try empty     0 0 1 1      # an empty glob is not a pass
    # STRICTLY BETWEEN THE OLD LITERAL AND THE PASSED FLOOR. Every arm above also passes with
    # `local floor=4` substituted for `local floor="$1"` — the exact hardcoded floor the change
    # abolished — because 3 and 0 are below 4 as well. So the argument could be ignored entirely
    # and this case stayed green: it certified the hole it was written to close. Four suites
    # against a floor of five is the one shape that tells the two apart.
    try ignores-arg 4 0 5 1

    if [ -n "$bad" ]; then
        fail "queue-gate: arms" "_shell_suites_pass did not answer as written for:$bad"
    else
        ok "the merge queue's shell-suite gate passes green, fails red, and refuses a thinned directory"
    fi

    # ...AND THE GATE ACTUALLY CALLS IT, which the arms above do not establish. Everything so far
    # would still pass with the invocation deleted from the gate conjunction and the function left
    # sitting there unused — which is precisely the revert this case exists to make impossible:
    # the gate back to `build.sh && test.sh`, cargo-only, with every guard in the repo green.
    if grep -qE '&&[[:space:]]*_shell_suites_pass[[:space:]]' \
        <<<"$(grep -vE '^[[:space:]]*#' "$repo_root/scripts/merge-queue.sh")"; then
        ok "and the queue's gate conjunction actually invokes it, so the shell half can fail a landing"
    else
        fail "queue-gate: uncalled" "scripts/merge-queue.sh defines _shell_suites_pass but its gate \
does not run it, so the landing gate is cargo-only again and nothing under scripts/tests can block \
a landing"
    fi

    # ...and the floor is COUNTED, against a tree whose answer is known. `-ge 4` against the live
    # repository passed with `_suite_floor`'s body replaced by `echo 4` — the literal the change
    # exists to abolish — so it asserted nothing about the derivation. A planted repository with a
    # count nobody can guess, checked for equality, is what makes the read real.
    #
    # The subdirectory arm is the second question: `_shell_suites_pass` runs the FLAT glob
    # `./scripts/tests/*.test.sh`, so a floor that counted recursively would be permanently
    # unreachable and would eject every branch queued after a suite was nested.
    local g="$w9/floorrepo" got want
    rm -rf "$g"; mkdir -p "$g/scripts/tests/nested"
    git init -q "$g" 2>/dev/null
    git -C "$g" config user.email t@t; git -C "$g" config user.name t
    : >"$g/scripts/tests/a.test.sh"; : >"$g/scripts/tests/b.test.sh"; : >"$g/scripts/tests/c.test.sh"
    : >"$g/scripts/tests/helper.sh"            # not a suite
    : >"$g/scripts/tests/nested/d.test.sh"     # a suite the flat runner cannot reach
    git -C "$g" add -A >/dev/null 2>&1; git -C "$g" commit -qm plant >/dev/null 2>&1
    want=3
    got="$(cd "$g" && _suite_floor HEAD)"
    if [ "$got" = "$want" ]; then
        ok "and its floor counts exactly the suites the runner can run ($want of 5 planted files)"
    else
        fail "queue-gate: floor" "_suite_floor counted '$got' where the planted tree has $want \
runnable suites (plus one helper and one nested). Counting the helper or the nested file makes the \
floor unreachable by the flat glob the gate actually runs, and every branch after it ejects"
    fi
}

# --- 10. the merge queue's exit contract has exactly one consumer, and it must know every code -
# THE DEFECT THIS EXISTS FOR SHIPPED IN THIS BRANCH. `merge-queue.sh` grew exit 4; the only thing
# that reads it — `.claude/workflows/task-swarm.js` — still enumerated 0/1/2/3 in a prompt and
# asked an agent to map the result to a boolean. Both answers it could give were wrong: one marks
# a whole task group done with the base never advanced, the other sends the implementer to fix a
# branch the script's own header says is not at fault.
#
# So the relation, not the function, is what is held here: every code the script DOCUMENTS must be
# one the workflow CLASSIFIES, and a code it does not document must fall to the unknown arm. Both
# sides are read from their home files — the header legend and the classifier source — so adding a
# code to one without the other reddens this case. That is `_selection_vars` reading
# `REPO_SELECTION_VARS` out of the Rust, applied across the shell/JS seam.
#
# Nothing else in this repository tests `.claude/workflows/*.js`, which is why the classifier was
# introduced with no oracle at all — the same "guard with no oracle" the queue's own gate half was
# just corrected for, one file over and in the same commit.
_queue_exit_codes() {
    sed -n '/^# Run inside the integration worktree/,/^set /p' "$repo_root/scripts/merge-queue.sh" \
        | grep -E '^#   [0-9]+ ' | sed -E 's/^#   ([0-9]+) .*/\1/'
}

_classify_merge_src() {
    sed -n '/^function classifyMerge(/,/^}/p' "$repo_root/.claude/workflows/task-swarm.js"
}

case10() {
    local codes src n bad="" out code
    codes="$(_queue_exit_codes)"
    src="$(_classify_merge_src)"
    n="$(grep -c . <<<"$codes" || true)"
    if [ "$n" -lt 4 ] || [ -z "$src" ]; then
        fail "queue-contract: premise" "read $n exit code(s) from merge-queue.sh's header and \
$( [ -n "$src" ] && echo "found" || echo "did NOT find") classifyMerge in task-swarm.js — one of \
the two extractions is broken, and everything below would pass vacuously"
        return
    fi

    # Every documented code must reach a real arm, not the default.
    while IFS= read -r code; do
        [ -n "$code" ] || continue
        out="$(node -e "$src
const v = classifyMerge($code)
console.log(v.outcome + '|' + (v.why || ''))" 2>&1)"
        case "$out" in
            *unknown\ exit*) bad="$bad $code(falls-to-default)" ;;
            landed\|*|eject\|*|stall\|*) ;;
            *) bad="$bad $code(bad-shape:$out)" ;;
        esac
    done <<<"$codes"

    # ...and a code the script does NOT document must stall as unknown, rather than being sorted
    # into the nearest bucket. This is the arm that makes the next code safe.
    out="$(node -e "$src
const v = classifyMerge(99)
console.log(v.outcome + '|' + (v.why || ''))" 2>&1)"
    case "$out" in
        stall\|*unknown\ exit\ 99*) ;;
        *) bad="$bad 99(undocumented-code-not-stalled:$out)" ;;
    esac

    if [ -n "$bad" ]; then
        fail "queue-contract: codes" "merge-queue.sh documents $n exit codes and task-swarm.js \
does not classify them all:$bad. A code the workflow has never heard of decides whether a task \
group is marked done"
    else
        ok "every exit code the merge queue documents is classified by its consumer ($n), and an unknown one stalls"
    fi

    # EVERY DOCUMENTED CODE HAS A NAMED EXPECTED OUTCOME, not just "reaches some arm". The first
    # version named 0/1/2/4 and omitted 3 — so the classifier still passed with exit 3 remapped to
    # `landed`, which is the exact harm this case's own failure message spells out. The expectation
    # is written down here and checked against the codes read from the header, so a code the header
    # grows arrives as a missing expectation rather than as silence.
    local want_map="0=landed 1=eject 2=eject 3=stall 4=stall"
    local code want got routing_bad=""
    while IFS= read -r code; do
        [ -n "$code" ] || continue
        want="$(tr ' ' '\n' <<<"$want_map" | sed -n "s/^$code=//p")"
        if [ -z "$want" ]; then
            routing_bad="$routing_bad $code(no-expectation-written-down)"
            continue
        fi
        got="$(node -e "$src
console.log(classifyMerge($code).outcome)" 2>&1)"
        [ "$got" = "$want" ] || routing_bad="$routing_bad $code(want=$want got=$got)"
    done <<<"$codes"
    if [ -z "$routing_bad" ]; then
        ok "and every documented code routes where this test says it must ($n checked)"
    else
        fail "queue-contract: routing" "classifyMerge sends these codes somewhere this test does \
not expect:$routing_bad. A landing reported as an eject burns the group's retry budget; an eject \
reported as a landing closes the group with nothing in the base; and a code with no expectation \
here means the header grew one and nobody decided what it means"
    fi
}

# ONE `run_cases`, because the harness requires the call to name every defined case — which is how
# it catches a case written and never wired up.
echo "==> scripts/*.sh: a reachable toolchain, and no pipe into a quiet grep"
run_cases case0 case1 case2 case3 case4 case5 case6 case7 case8 case9 case10

finish
