#!/usr/bin/env bash
# Archive Claude Code session transcripts until the Bash sandbox's deny list fits in one argv.
#
#   .container/sweep-transcripts.sh              sweep (run.sh runs this on every container start)
#   .container/sweep-transcripts.sh --dry-run    print what it would archive; move nothing
#   .container/sweep-transcripts.sh --self-test  exercise the derivation; no container, no Docker
#
# THE FAILURE, MEASURED IN THIS CONTAINER ON 2026-09-28. Claude Code's Bash sandbox profile
# enumerates every session transcript INDIVIDUALLY into its read-`denyOnly` list, and hands that
# profile to the shell as a single argv string. Linux caps one argument at MAX_ARG_STRLEN = 32
# pages = 131072 bytes. What was in here:
#
#   1182 transcript .jsonl files, 224KB of path text (~194 bytes per path)
#   ~/.claude/projects is a SYMLINK to ~/.claude-state/projects, so every file is listed under
#   BOTH spellings: ~2396 deny entries (2 x 1182, plus ~30 fixed security paths), 448KB of path
#   text, reported by the harness as `command line 498.8KB across 3 args`
#
# Every Bash tool call in every container session then failed at spawn with E2BIG. Not degraded:
# total, from the first call, with nothing in the message naming transcripts.
#
# WHY A BYTE BUDGET AND NOT A RETENTION PERIOD. The failing quantity is bytes of argv, so that is
# what this budgets. Claude Code's own `cleanupPeriodDays` is time-based and NEVER binds here: the
# budget is exhausted well inside any 30-day window, which is how a container with retention
# configured arrived at 1182 files. A count cutoff is closer but drifts — the path text per file
# grows as agents nest deeper — so a count that fits today silently stops fitting. Bytes are the
# thing that overflows, so bytes are the thing that is counted.
#
# WHY IT PERSISTS. .claude-state is a Docker volume, so transcripts survive every image rebuild
# and the count never resets on its own. Nothing else in the lifecycle was going to bound it.
#
# WHAT IT COSTS. Archived transcripts leave `~/.claude/projects`, so Claude Code stops listing
# them: `--resume` will not offer an archived session and `/resume` will not find it. The bytes
# are not deleted — they are one `mv` away, under ~/.claude-state/transcript-archive, in the same
# volume — so recovering one is copying a file back, but you have to know it is there. That is the
# whole price, and it is paid against a container in which no Bash call works at all.
set -uo pipefail

# ---------------------------------------------------------------------------------------------
# The budget. Pure constants, so --self-test can state them rather than rediscover them.
# ---------------------------------------------------------------------------------------------

# Where transcripts live, and where archived ones go. The archive is a SIBLING of `projects/` in
# the same volume, which buys three things at once: it survives a rebuild (same volume), the move
# is a rename rather than a copy (same filesystem), and it is outside the tree Claude Code walks,
# which is the only reason the sweep reduces anything.
# CLAUDE_CONFIG_DIR is honoured here because commands.rs, auto-mode.sh and swarm-status.sh all
# honour it, and one site spelling the config base differently is the whole bug. It is unset in
# the container today so this changes nothing now -- but a second config dir (the staging-login
# pattern auto-mode-test.sh already uses) would have pointed the sweep at an absent tree, where
# it prints "no transcripts", exits 0, and the real tree keeps growing while every Bash call goes
# on dying at spawn. A sweep that cannot find its subject must not look successful.
CLAUDE_BASE="${CLAUDE_CONFIG_DIR:-${HOME:-/home/vscode}/.claude}"
TRANSCRIPT_ROOT="${JKB_TRANSCRIPT_ROOT:-$CLAUDE_BASE/projects}"
TRANSCRIPT_ARCHIVE="${JKB_TRANSCRIPT_ARCHIVE:-${HOME:-/home/vscode}/.claude-state/transcript-archive}"

# MAX_ARG_STRLEN on Linux: 32 pages. Recorded for the reader; the budget below is derived from it.
ARGV_MAX_BYTES=131072
# Half of it. The other half is headroom for what this sweep does NOT count: the ~30 fixed
# security paths in the same deny list, the write-side allow/deny lists in the same argument, and
# the JSON quoting around every entry. Half is not a measurement, it is a margin — and a margin is
# what the previous arrangement (none) lacked.
DENY_BUDGET_BYTES=$((ARGV_MAX_BYTES / 2))
# Every transcript is listed TWICE, once per spelling of the root (see the measurement above).
# If that symlink ever goes, this becomes 1 and the sweep simply keeps twice as many files.
DENY_SPELLINGS=2
# KEPT UNCONDITIONALLY, whatever the budget says, because the live session is writing one of these
# right now and archiving it out from under Claude Code is data loss with a plausible-looking
# cause. This is a FLOOR, not a cap: the sweep stops as soon as the projection is under budget, so
# at ~194 bytes a path it keeps about 169 files in the ordinary case and this number never binds.
# It binds only if the newest 32 paths alone would blow the budget, and then keeping them is still
# the right answer — a container that cannot resume its own session is worse than one whose deny
# list is a little long.
KEEP_NEWEST=32

TAB="$(printf '\t')"

# GNU coreutils and BSD `stat` spell the same two fields differently, and this file is run inside
# the container (GNU) and self-tested on the host (which may be macOS). Probed once, against a
# path that always exists, rather than branched on `uname`.
if stat -c '%Y' . >/dev/null 2>&1; then STAT_FMT=(-c "%Y${TAB}%n"); else STAT_FMT=(-f "%m${TAB}%N"); fi

# ---------------------------------------------------------------------------------------------
# Derivation. Pure functions over "<mtime><TAB><path>" records, so --self-test can drive every one
# of them from a here-string with no filesystem at all.
# ---------------------------------------------------------------------------------------------

# The bytes of deny-list path text these records would contribute.
# LC_ALL=C so awk's length() counts BYTES: a repo path with any non-ASCII in it would otherwise be
# measured shorter than the kernel measures it, in the direction that overflows.
transcript_projection() { # transcript_projection < records -> bytes
    LC_ALL=C awk -F"$TAB" -v mult="$DENY_SPELLINGS" \
        '{ tot += length($2) } END { printf "%d\n", mult * tot }'
}

# Which files to archive, OLDEST FIRST, to bring the projection under budget — and no more than
# that. Stops at the first record that brings it under, so an ordinary start archives a handful.
#
# SORTED BY MTIME AND THEN BY PATH. The second key is not decoration: a task swarm writes a dozen
# agent transcripts in the same second, and without it the set chosen at a tie depends on the
# order find happened to walk the tree, which makes this function untestable and its behaviour
# unrepeatable on two containers with identical contents.
transcript_plan() { # transcript_plan < records -> paths to archive, oldest first
    LC_ALL=C sort -t"$TAB" -k1,1n -k2,2 \
    | LC_ALL=C awk -F"$TAB" -v budget="$DENY_BUDGET_BYTES" -v keep="$KEEP_NEWEST" \
                   -v mult="$DENY_SPELLINGS" '
        { n++; path[n] = $2; tot += length($2) }
        END {
            proj = mult * tot
            for (i = 1; i <= n - keep; i++) {
                if (proj <= budget) break
                print path[i]
                proj -= mult * length(path[i])
            }
        }'
}

# Every transcript under a root, as records.
#
# FOUR THINGS ABOUT THIS ONE `find`, each of which cost a real failure:
#
#   -L            the root is reachable through a symlink (~/.claude/projects -> the state
#                 volume), and find does not follow a symlinked STARTING POINT without it. The
#                 root is made absolute but deliberately NOT resolved: paths stay in the caller's
#                 spelling, which is the spelling the deny list is built from and the spelling
#                 `rel` is computed against. The move is still a rename on one filesystem, because
#                 the link lands in the volume the archive is in.
#   no depth cap  agent transcripts are <slug>/<uuid>/subagents/agent-*.jsonl (depth 4) and
#                 <slug>/<uuid>/subagents/workflows/wf_*/agent-*.jsonl (depth 6), and they are the
#                 BULK of the population — one per swarm implementer, reviewer and workflow agent.
#                 A depth cap here would sweep the cheap half and leave the expensive half.
#   -name filter  <slug>/memory/ holds auto-memory as .md files. Not transcripts, not ours to
#                 move, at any depth. The NAME is what guards them; depth never was.
#   journal.jsonl HELD BACK BY NAME, and the rule this bullet used to state was WRONG. It said the
#                 harness keeps `wf_*.json` run records; there are none -- measured, zero anywhere
#                 under the real root. What it actually writes is
#                 <slug>/<uuid>/subagents/workflows/wf_*/journal.jsonl (23 of them in this
#                 container), `*.jsonl` matches it, and swarm-status.sh DISCOVERS runs by that exact
#                 name (-name journal.jsonl -path '*/subagents/workflows/wf_*') and then requires
#                 the file. So the sweep archived the harness's own state oldest-first on every
#                 container start, and `swarm-status.sh <run>` printed "no swarm run found" for
#                 every past run. The agent transcripts in those SAME directories (agent-*.jsonl)
#                 are the bulk of the population and must still be swept, so `workflows` cannot be
#                 pruned the way `memory` is -- exactly one name is held back.
#   memory pruned belt to that brace: under -L, the per-repo `memory` symlink into the bind-mounted
#                 ~/.jkb/claude-memory is followed like a real directory, so the walk leaves the
#                 volume entirely. Pruned by name, which is portable (-prune/-o are POSIX, GNU's
#                 -xtype is not and this file self-tests on macOS).
#
# Errors are swallowed and partial output accepted on purpose: a live session is writing into this
# tree, so a file can vanish between the walk and the stat, and an empty result means "archive
# nothing", which is the safe direction.
transcript_records() { # transcript_records <root> -> "<mtime><TAB><path>" per transcript file
    local abs
    # `pwd`, not `pwd -P`: absolute (so no enumerated path can begin with a dash) but still the
    # caller's spelling, which is what makes -L above the thing doing the work.
    abs="$(cd "$1" 2>/dev/null && pwd)" || return 0
    [ -n "$abs" ] || return 0
    find -L "$abs" -type d -name memory -prune \
         -o -type f -name '*.jsonl' ! -name journal.jsonl -exec stat "${STAT_FMT[@]}" {} + 2>/dev/null
}

# ---------------------------------------------------------------------------------------------
# The sweep itself.
# ---------------------------------------------------------------------------------------------

# EVERY PROJECT SLUG BEGINS WITH `-`. Claude Code names a project directory after the absolute
# path with non-alphanumerics replaced, so a leading `/` becomes a leading `-`:
# `-home-vscode-repos-jkb`. A bare `dirname "$rel"` reads that as the `-h` option and dies, and
# this is every path in the tree rather than an edge case. So: the root is resolved to an absolute
# path first (which makes every enumerated path start with `/`), the relative directory comes from
# `${rel%/*}` parameter expansion rather than from dirname, and every external command that takes
# a path here is given `--`.
sweep_transcripts() { # sweep_transcripts <root> <archive> [--dry-run]
    local root="$1" archive="$2" dry="${3:-}"
    local abs records plan f rel dir before after moved=0 failed=0

    abs="$(cd "$root" 2>/dev/null && pwd)" || {
        printf 'transcript sweep: %s does not exist — nothing to sweep\n' "$root"
        return 0
    }
    # AN ARCHIVE INSIDE THE ROOT GROWS THE DENY LIST IT EXISTS TO SHRINK: the next walk finds
    # what this one moved, one directory deeper, for ever. The shipped constants are siblings, so
    # this is reachable only through JKB_TRANSCRIPT_ARCHIVE or a future edit -- which is precisely
    # why it is refused here rather than left to a check on the defaults.
    case "$archive/" in
        "$abs"/*)
            printf 'transcript sweep: archive %s is inside %s — each sweep would re-enumerate what the last one moved\n' \
                "$archive" "$abs" >&2
            return 1 ;;
    esac
    records="$(transcript_records "$abs")"
    if [ -z "$records" ]; then
        printf 'transcript sweep: no transcripts under %s\n' "$root"
        return 0
    fi
    before="$(printf '%s\n' "$records" | transcript_projection)"
    plan="$(printf '%s\n' "$records" | transcript_plan)"
    if [ -z "$plan" ]; then
        printf 'transcript sweep: %s deny bytes projected, budget %s — nothing to archive\n' \
            "$before" "$DENY_BUDGET_BYTES"
        return 0
    fi

    # A DRY RUN WRITES NOTHING, the archive directory included. It created it before this was
    # split out, which is a dry run with a side effect — small, but the whole value of the flag is
    # that you can run it to find out and not have found out by changing something.
    if [ "$dry" != "--dry-run" ]; then
        mkdir -p -- "$archive" || {
            printf 'transcript sweep: could not create %s — nothing archived\n' "$archive" >&2
            return 1
        }
    fi
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        rel="${f#"$abs"/}"
        dir="$archive"
        case "$rel" in */*) dir="$archive/${rel%/*}" ;; esac
        if [ "$dry" = "--dry-run" ]; then
            printf '  would archive %s\n' "$rel"
            moved=$((moved+1))
            continue
        fi
        mkdir -p -- "$dir" || { failed=$((failed+1)); continue; }
        # `-n` so an archived copy from an earlier sweep is never clobbered: the file then stays
        # where it is and the next sweep sees it again, which is visible rather than lossy.
        if mv -n -- "$f" "$dir/" 2>/dev/null; then moved=$((moved+1)); else failed=$((failed+1)); fi
    done <<<"$plan"

    after="$(transcript_records "$abs" | transcript_projection)"
    if [ "$dry" = "--dry-run" ]; then
        printf 'transcript sweep: would archive %s file(s), %s -> under %s deny bytes\n' \
            "$moved" "$before" "$DENY_BUDGET_BYTES"
    else
        printf 'transcript sweep: archived %s file(s) to %s (%s -> %s deny bytes, budget %s)\n' \
            "$moved" "$archive" "$before" "$after" "$DENY_BUDGET_BYTES"
    fi
    if [ "$failed" -gt 0 ]; then
        printf 'transcript sweep: %s file(s) could not be archived\n' "$failed" >&2
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------------------------
# Self-test. Runs on the host, in ./scripts/check.sh and in CI; needs no container and no Docker.
# ---------------------------------------------------------------------------------------------

if [ "${1:-}" = "--self-test" ] && [ "$#" -eq 1 ]; then
    fails=0
    eq() { # eq <label> <got> <want>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else printf '  \033[31mFAIL\033[0m %s\n         got:  %s\n         want: %s\n' "$1" "$2" "$3"; fails=$((fails+1)); fi
    }
    rc_of() { ( "$@" >/dev/null 2>&1 ); printf '%s' "$?"; }
    # `${1%/*}` and not `dirname`, here too: the fixtures are the dash-leading slugs the real tree
    # is made of, and a helper that cannot make them cannot test them.
    mk() { # mk <path> <YYYYMMDDhhmm>
        mkdir -p -- "${1%/*}"
        : > "$1"
        touch -t "$2" -- "$1"
    }

    echo "==> sweep-transcripts self-test: the byte projection"
    # Two spellings of every path, so the projection is twice the path text. 10 + 10 = 20 bytes of
    # path, 40 projected.
    recs="1${TAB}/aaaa/bbbb
2${TAB}/cccc/dddd"
    eq "a path is counted once per spelling of the root" \
       "$(printf '%s\n' "$recs" | transcript_projection)" "40"
    eq "no records project no bytes" "$(printf '' | transcript_projection)" "0"

    echo "==> sweep-transcripts self-test: what the budget chooses"
    # Five 10-byte paths: 100 projected. A budget of 60 needs 40 bytes gone, which is two paths.
    five="5${TAB}/eeee/eeee
1${TAB}/aaaa/aaaa
4${TAB}/dddd/dddd
2${TAB}/bbbb/bbbb
3${TAB}/cccc/cccc"
    DENY_BUDGET_BYTES=60 KEEP_NEWEST=0
    eq "it archives exactly as many as the budget needs, and no more" \
       "$(printf '%s\n' "$five" | transcript_plan | tr '\n' ' ')" "/aaaa/aaaa /bbbb/bbbb "
    DENY_BUDGET_BYTES=100
    eq "a projection already under budget archives nothing" \
       "$(printf '%s\n' "$five" | transcript_plan)" ""
    DENY_BUDGET_BYTES=0
    eq "oldest first, all the way down" \
       "$(printf '%s\n' "$five" | transcript_plan | tr '\n' ' ')" \
       "/aaaa/aaaa /bbbb/bbbb /cccc/cccc /dddd/dddd /eeee/eeee "
    # THE LIVE SESSION'S OWN TRANSCRIPT. With a budget of zero the arithmetic wants everything
    # gone; the floor is what stops it taking the file Claude Code is writing into right now.
    KEEP_NEWEST=2
    eq "the newest N survive a budget that wants everything" \
       "$(printf '%s\n' "$five" | transcript_plan | tr '\n' ' ')" \
       "/aaaa/aaaa /bbbb/bbbb /cccc/cccc "
    KEEP_NEWEST=99
    eq "a floor above the population archives nothing at all" \
       "$(printf '%s\n' "$five" | transcript_plan)" ""
    # Ties are ordinary: a swarm writes a dozen agent transcripts in the same second.
    DENY_BUDGET_BYTES=0 KEEP_NEWEST=0
    tied="7${TAB}/bbbb/bbbb
7${TAB}/aaaa/aaaa
7${TAB}/cccc/cccc"
    eq "files sharing an mtime are ordered by path, so the choice is repeatable" \
       "$(printf '%s\n' "$tied" | transcript_plan | tr '\n' ' ')" \
       "/aaaa/aaaa /bbbb/bbbb /cccc/cccc "

    echo "==> sweep-transcripts self-test: enumerating a real tree"
    work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
    root="$work/state/projects"
    arch="$work/state/transcript-archive"
    slug="$root/-home-vscode-repos-jkb"
    # Depth 2: an ordinary session. Depth 4 and 6: the agent transcripts that are the bulk.
    mk "$slug/1111.jsonl"                                          202601010000
    mk "$slug/2222.jsonl"                                          202601020000
    mk "$slug/2222/subagents/agent-a.jsonl"                        202601030000
    mk "$slug/2222/subagents/workflows/wf_x/agent-b.jsonl"         202601040000
    # ...and the things that are NOT transcripts and must never move.
    mk "$slug/2222/subagents/workflows/wf_x/journal.jsonl"         202601010000
    # AUTO-MEMORY IS A SYMLINK OUT OF THE TREE, exactly as the container has it: each slug's
    # `memory` points at ~/.jkb/claude-memory/<repo>, which is a bind mount of the HOST's
    # knowledge base. Under -L the walk follows it like a real directory and leaves the volume
    # entirely, so the fixture puts a .jsonl in there — a file that is not a transcript, that the
    # host also owns, and that only the prune keeps out of the plan.
    store="$work/jkb/claude-memory/jkb"
    mk "$store/MEMORY.md"                                          202601010000
    mk "$store/bash-e2big.md"                                      202601010000
    mk "$store/not-a-transcript.jsonl"                             202601010000
    ln -s "$store" "$slug/memory"
    # A second project, so the dash-leading slug is not the only directory in play.
    mk "$root/-tmp-d52harness-work/3333.jsonl"                     202601050000

    found="$(transcript_records "$root" | LC_ALL=C sort -t"$TAB" -k2,2 | cut -f2 | sed "s#^$root/##" | tr '\n' ' ')"
    eq "every .jsonl is found, at every depth, and nothing else is" "$found" \
       "-home-vscode-repos-jkb/1111.jsonl -home-vscode-repos-jkb/2222.jsonl -home-vscode-repos-jkb/2222/subagents/agent-a.jsonl -home-vscode-repos-jkb/2222/subagents/workflows/wf_x/agent-b.jsonl -tmp-d52harness-work/3333.jsonl "

    # THE SYMLINKED ROOT, which is how the container spells it: ~/.claude/projects is a link into
    # the state volume. Without `-L` this reads an empty tree and the sweep silently does nothing.
    ln -s "$root" "$work/projects-link"
    eq "a symlinked root enumerates the same files" \
       "$(transcript_records "$work/projects-link" | grep -c . )" "5"

    echo "==> sweep-transcripts self-test: a dry run changes nothing"
    DENY_BUDGET_BYTES=0 KEEP_NEWEST=2
    dry_out="$(sweep_transcripts "$work/projects-link" "$arch" --dry-run)"
    # `^  ` so the per-file lines are counted and the summary — which says "would archive N" too —
    # is not, which is how this row first read 4 for 3 files.
    eq "it names the three it would archive" "$(grep -c '^  would archive' <<<"$dry_out")" "3"
    eq "...and its summary agrees" "$(grep -c 'would archive 3 file' <<<"$dry_out")" "1"
    # Six, not five: the run journal is a .jsonl in the tree that is never in the plan.
    eq "...and moved none of them" \
       "$(find "$root" -type f -name '*.jsonl' | grep -c . )" "6"
    eq "...and did not even create the archive directory" \
       "$([ -e "$arch" ] && echo yes || echo no)" "no"

    echo "==> sweep-transcripts self-test: the sweep"
    DENY_BUDGET_BYTES=0 KEEP_NEWEST=2
    out="$(sweep_transcripts "$work/projects-link" "$arch")"; rc=$?
    eq "the sweep succeeds" "$rc" "0"
    eq "it archived the three oldest and kept the two newest" \
       "$(grep -c 'archived 3 file' <<<"$out")" "1"
    left="$(find "$root" -type f -name '*.jsonl' | sed "s#^$root/##" | LC_ALL=C sort | tr '\n' ' ')"
    # The two newest transcripts AND the run journal, which is the oldest file in the fixture and
    # would be first in the plan if the name were not held back. Its presence here is the assertion.
    eq "the newest two, and the run journal, are still where they were" "$left" \
       "-home-vscode-repos-jkb/2222/subagents/workflows/wf_x/agent-b.jsonl -home-vscode-repos-jkb/2222/subagents/workflows/wf_x/journal.jsonl -tmp-d52harness-work/3333.jsonl "
    # THE DASH-LEADING SLUG SURVIVES THE MOVE. If `${rel%/*}` had been `dirname "$rel"` this
    # directory would not exist and the files would be in the archive root, or nowhere.
    eq "the archive keeps the dash-leading slug as a directory" \
       "$([ -f "$arch/-home-vscode-repos-jkb/1111.jsonl" ] && echo yes || echo no)" "yes"
    eq "...and the nested agent transcript keeps its whole path" \
       "$([ -f "$arch/-home-vscode-repos-jkb/2222/subagents/agent-a.jsonl" ] && echo yes || echo no)" "yes"
    # THE POPULATIONS THAT ARE NOT TRANSCRIPTS. The memory store is the host's, reached through a
    # symlink, so moving anything out of it loses a file this container does not own.
    eq "the whole auto-memory store is untouched, .jsonl decoy included" \
       "$(find "$store" -type f | LC_ALL=C sort | sed "s#^$store/##" | tr '\n' ' ')" \
       "MEMORY.md bash-e2big.md not-a-transcript.jsonl "
    # THE HARNESS'S OWN RUN STATE, and the row that used to stand here asserted the survival of
    # `workflows/wf_x.json` -- a shape that exists nowhere, so it could not fail while the real
    # journals were being archived. It is the OLDEST file in the fixture, so it is first in line.
    eq "the workflow harness's run journal is untouched" \
       "$([ -f "$slug/2222/subagents/workflows/wf_x/journal.jsonl" ] && echo yes || echo no)" "yes"
    eq "...and no run journal reached the archive" \
       "$(find "$arch" -type f -name journal.jsonl | grep -c . )" "0"
    eq "nothing that is not a transcript reached the archive" \
       "$(find "$arch" -type f ! -name '*.jsonl' | grep -c . )" "0"

    # THE ARCHIVE MUST BE OUTSIDE THE ROOT, watched failing rather than assumed from the defaults.
    eq "an archive inside the root is refused" \
       "$(rc_of sweep_transcripts "$root" "$root/.archive")" "1"
    eq "...and nothing was created for it" \
       "$([ -e "$root/.archive" ] && echo yes || echo no)" "no"

    echo "==> sweep-transcripts self-test: running it again, and running it on nothing"
    archived_before="$(find "$arch" -type f | grep -c . )"
    eq "a second sweep of the same tree succeeds" \
       "$(rc_of sweep_transcripts "$work/projects-link" "$arch")" "0"
    eq "...and moves nothing that the first sweep left" \
       "$(find "$root" -type f -name '*.jsonl' | grep -c . )" "3"
    eq "...and adds nothing to the archive" \
       "$(find "$arch" -type f | grep -c . )" "$archived_before"
    DENY_BUDGET_BYTES=$((ARGV_MAX_BYTES / 2)) KEEP_NEWEST=32
    mkdir -p "$work/empty"
    eq "an empty root succeeds"  "$(rc_of sweep_transcripts "$work/empty" "$arch")" "0"
    eq "an absent root succeeds" "$(rc_of sweep_transcripts "$work/nothing-here" "$arch")" "0"
    eq "...and creates no archive for it" \
       "$([ -d "$work/nothing-here" ] && echo yes || echo no)" "no"

    echo
    [ "$fails" -eq 0 ] || { printf '\033[31msweep-transcripts self-test FAILED (%d)\033[0m\n' "$fails"; exit 1; }
    printf '\033[32msweep-transcripts self-test passed\033[0m\n'
    exit 0
fi

case "${1:-}" in
    ""|--dry-run) sweep_transcripts "$TRANSCRIPT_ROOT" "$TRANSCRIPT_ARCHIVE" "${1:-}" ;;
    *) printf 'usage: %s [--dry-run|--self-test]\n' "$0" >&2; exit 2 ;;
esac
