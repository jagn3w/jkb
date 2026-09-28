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
# THE ONE NAME THE SWEEP HOLDS BACK, and the rule it replaced was WRONG. That rule said the
# workflow harness keeps `wf_*.json` run records; there are none -- measured, zero anywhere under
# the real root. What it actually writes is <slug>/<uuid>/subagents/workflows/wf_*/journal.jsonl
# (23 of them in this container), `*.jsonl` matches it, and swarm-status.sh DISCOVERS runs by that
# exact name (-name journal.jsonl -path '*/subagents/workflows/wf_*') and then requires the file.
# So the sweep archived the harness's own state oldest-first on every container start, and
# `swarm-status.sh <run>` printed "no swarm run found" for every past run. The agent transcripts in
# those SAME directories (agent-*.jsonl) are the bulk of the population and must still be swept, so
# `workflows` cannot be pruned the way `memory` is -- exactly one name is spared.
# SPARED IN THE PLAN, NOT IN THE WALK; transcript_records says why.
# check-config.sh does not take this spelling on trust: it reads the name out of swarm-status.sh's
# discovery predicate and requires this line to agree, because the authority is that external
# harness and this repo has already been wrong about the name once.
HELD_NAME=journal.jsonl

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
#
# THE HELD NAME IS SPARED HERE, AND `tot` IS WHY. Every record's bytes go into the projection,
# including the ones no plan may ever contain, because the kernel counts a path the sweep cannot
# reclaim exactly like one it can. `keep` is a floor on the ARCHIVABLE population: a held file was
# never a candidate, so counting it toward the floor would reserve a slot no plan could have used.
transcript_plan() { # transcript_plan < records -> paths to archive, oldest first
    LC_ALL=C sort -t"$TAB" -k1,1n -k2,2 \
    | LC_ALL=C awk -F"$TAB" -v budget="$DENY_BUDGET_BYTES" -v keep="$KEEP_NEWEST" \
                   -v mult="$DENY_SPELLINGS" -v held="$HELD_NAME" '
        { tot += length($2)
          base = $2; sub(/^.*\//, "", base)
          if (base == held) next
          n++; path[n] = $2 }
        END {
            proj = mult * tot
            for (i = 1; i <= n - keep; i++) {
                if (proj <= budget) break
                print path[i]
                proj -= mult * length(path[i])
            }
        }'
}

# The records no plan may ever contain, so the sweep can say out loud when they alone exceed the
# budget -- the one state it cannot fix, in which archiving every transcript still leaves the argv
# over MAX_ARG_STRLEN and every Bash call still dies at spawn.
transcript_unreclaimable() { # transcript_unreclaimable < records -> the held-back records
    LC_ALL=C awk -F"$TAB" -v held="$HELD_NAME" \
        '{ base = $2; sub(/^.*\//, "", base); if (base == held) print }'
}

# Every .jsonl under a root, as records.
#
# THINGS ABOUT THIS ONE `find`, each of which cost a real failure:
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
#   nothing held  NOTHING IS HELD BACK HERE, and the round that introduced $HELD_NAME held it back
#                 in this very `find`. This function feeds the PROJECTION as well as the plan, and
#                 the projection is the sizing of the argv that overflows: a path the sweep cannot
#                 reclaim costs the kernel exactly what a path it can reclaim costs. Measured on a
#                 1,201-file fixture, the sweep acted on 65,250 bytes while the real path text was
#                 96,612 -- 48% more, all of it held-back journals -- and printed "nothing to
#                 archive" while every Bash call went on dying at spawn. Journals are never
#                 archived and .claude-state is a volume, so the unreclaimable set only grows: at
#                 ~165 bytes each, ~198 of them exceed the whole budget on their own. The walk
#                 counts everything; transcript_plan is where the name is spared, and
#                 transcript_unreclaimable is what lets the sweep say that state out loud.
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
         -o -type f -name '*.jsonl' -exec stat "${STAT_FMT[@]}" {} + 2>/dev/null
}

# ---------------------------------------------------------------------------------------------
# The sweep itself.
# ---------------------------------------------------------------------------------------------

# The PHYSICAL path of a path that need not exist yet. `cd` + `pwd -P` resolves only a directory
# that is already there, and the archive usually is not, so the nearest existing ancestor is
# resolved and the missing tail put back on. This also collapses any `..` in the caller's spelling,
# which is the second way into the containment the refusal below exists to refuse.
transcript_resolve() { # transcript_resolve <path> -> physical path
    local p="$1" tail="" base
    case "$p" in /*) ;; *) p="$PWD/$p" ;; esac
    while [ ! -d "$p" ]; do
        tail="/${p##*/}$tail"
        p="${p%/*}"
        [ -n "$p" ] || p="/"
    done
    base="$(cd "$p" && pwd -P)" || return 1
    [ "$base" != "/" ] || base=""
    printf '%s%s\n' "$base" "$tail"
}

# EVERY PROJECT SLUG BEGINS WITH `-`. Claude Code names a project directory after the absolute
# path with non-alphanumerics replaced, so a leading `/` becomes a leading `-`:
# `-home-vscode-repos-jkb`. A bare `dirname "$rel"` reads that as the `-h` option and dies, and
# this is every path in the tree rather than an edge case. So: the root is resolved to an absolute
# path first (which makes every enumerated path start with `/`), the relative directory comes from
# `${rel%/*}` parameter expansion rather than from dirname, and every external command that takes
# a path here is given `--`.
sweep_transcripts() { # sweep_transcripts <root> <archive> [--dry-run]
    local root="$1" archive="$2" dry="${3:-}"
    local abs phys_root phys_archive records plan held_bytes err
    local f rel dir before after moved=0 failed=0

    abs="$(cd "$root" 2>/dev/null && pwd)" || {
        printf 'transcript sweep: %s does not exist — nothing to sweep\n' "$root"
        return 0
    }
    # AN ARCHIVE INSIDE THE ROOT GROWS THE DENY LIST IT EXISTS TO SHRINK: the next walk finds
    # what this one moved, one directory deeper, for ever. The shipped constants are siblings, so
    # this is reachable only through JKB_TRANSCRIPT_ARCHIVE or a future edit -- which is precisely
    # why it is refused here rather than left to a check on the defaults.
    #
    # COMPARED AS PHYSICAL PATHS, because the walk is `-L` and the container's root is a SYMLINK
    # (~/.claude/projects -> ~/.claude-state/projects). The test that stood here compared the
    # caller's spellings, so `JKB_TRANSCRIPT_ARCHIVE=~/.claude-state/projects/.archive` -- a path
    # squarely inside the enumerated tree -- was not a string prefix of `~/.claude/projects` and
    # was accepted: the safety net was inoperative in the one deployment it was written for, and
    # the .archive/.archive/ nesting it exists to stop reproduces (530 -> 602 -> 674 deny bytes).
    # `abs` stays UNRESOLVED and is what the rest of the function walks, because that is the
    # spelling the deny list is built from and the spelling `rel` is computed against.
    phys_root="$(cd "$root" && pwd -P)" || return 1
    phys_archive="$(transcript_resolve "$archive")" || return 1
    case "$phys_archive/" in
        "$phys_root"/*)
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
    # SAID OUT LOUD, because it is the one state the sweep cannot fix. The harness's run journals
    # are never archived, so once their path text alone exceeds the budget no plan brings the tree
    # under it -- and every line the sweep prints below still reads as success while every Bash
    # call goes on dying at spawn. The count only ever grows: .claude-state is a volume.
    held_bytes="$(printf '%s\n' "$records" | transcript_unreclaimable | transcript_projection)"
    if [ "$held_bytes" -gt "$DENY_BUDGET_BYTES" ]; then
        printf 'transcript sweep: %s deny bytes are in %s files the sweep never archives, over the whole %s byte budget — archiving every transcript cannot bring this tree under it\n' \
            "$held_bytes" "$HELD_NAME" "$DENY_BUDGET_BYTES" >&2
    fi
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
        # A FILE THAT VANISHED BETWEEN THE PLAN AND THE MOVE IS NOT A FAILURE. transcript_records
        # already treats that race as ordinary -- a live session, Claude Code's own
        # cleanupPeriodDays retention, a second run.sh -- and counting it here printed a causeless
        # "N file(s) could not be archived" on a sweep that had done its job.
        [ -e "$f" ] || continue
        # `-n` so an archived copy from an earlier sweep is never clobbered: the file then stays
        # where it is and the next sweep sees it again, which is visible rather than lossy.
        # THE DIAGNOSTIC IS KEPT, not swallowed: ENOSPC, a read-only archive and a cross-device
        # rename all read identically from a number, and run.sh discards the return value anyway.
        if err="$(mv -n -- "$f" "$dir/" 2>&1)"; then
            moved=$((moved+1))
        else
            failed=$((failed+1))
            [ -z "$err" ] || printf 'transcript sweep: %s\n' "$err" >&2
        fi
    done <<<"$plan"

    after="$(transcript_records "$abs" | transcript_projection)"
    if [ "$dry" = "--dry-run" ]; then
        printf 'transcript sweep: would archive %s file(s), %s -> under %s deny bytes\n' \
            "$moved" "$before" "$DENY_BUDGET_BYTES"
    else
        printf 'transcript sweep: archived %s file(s) to %s (%s -> %s deny bytes, budget %s)\n' \
            "$moved" "$archive" "$before" "$after" "$DENY_BUDGET_BYTES"
    fi
    # A POST-CONDITION ON THE TWO NUMBERS IT ALREADY PRINTS. `before` and `after` sat side by side
    # in that summary with nothing comparing them, which is how a run that GREW the deny list --
    # an archive nested inside the root, re-enumerated one directory deeper on every start -- came
    # to exit 0 with every count reading as success. A dry run moves nothing, so its projection is
    # unchanged by definition and this is not asked of it.
    #
    # WHAT IT CANNOT TELL APART: a live session writing new transcripts faster than this one
    # archives them reads the same way. That is why it is a message and a non-zero return rather
    # than a refusal to continue -- run.sh discards the return value -- and why the wording says
    # what was observed rather than naming a cause with certainty. The nesting case is refused
    # outright above; this is the net that catches a route into it nobody has thought of yet.
    if [ "$dry" != "--dry-run" ] && [ "$moved" -gt 0 ] && [ "$after" -ge "$before" ]; then
        printf 'transcript sweep: archived %s file(s) and the projection did not fall (%s -> %s) — the archive is being re-enumerated under the root\n' \
            "$moved" "$before" "$after" >&2
        return 1
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
    # The script's own path, so the rows below can run it as a PROGRAM and not only as a library.
    case "${BASH_SOURCE[0]}" in
        */*) self="$(cd -- "${BASH_SOURCE[0]%/*}" && pwd)/${BASH_SOURCE[0]##*/}" ;;
        *)   self="$PWD/${BASH_SOURCE[0]}" ;;
    esac

    echo "==> sweep-transcripts self-test: the constants the rest is derived from"
    # STATED, NOT ASSUMED. The header two hundred lines up says "pure constants, so --self-test can
    # state them rather than rediscover them" -- and then none of the four was asserted anywhere,
    # here or in check-config.sh or in mutate-config.sh. All four were mutated against the
    # unmodified self-test and every gate stayed green: KEEP_NEWEST=0 (the harm scripts/check.sh
    # names by name -- with the floor gone the oldest-first plan takes the transcript the live
    # session is writing into), KEEP_NEWEST=3200, a 4x DENY_BUDGET_BYTES and a 10x ARGV_MAX_BYTES
    # (the other direction: a sweep that archives nothing while every Bash call goes on dying at
    # spawn and CI stays green). These rows run BEFORE the first override below, which is the only
    # place they can read the shipped values.
    eq "the argv cap is Linux's MAX_ARG_STRLEN, 32 pages" "$ARGV_MAX_BYTES" "131072"
    eq "the deny budget is half of it, the other half headroom" "$DENY_BUDGET_BYTES" "65536"
    eq "every path is listed once per spelling of the root" "$DENY_SPELLINGS" "2"
    eq "the floor that keeps the live session's own transcript is 32" "$KEEP_NEWEST" "32"
    # SET, but its VALUE is deliberately not pinned here. Its authority is swarm-status.sh's
    # discovery predicate, and check-config.sh reads it out of that file and requires agreement -- a
    # literal in this row as well would be a second copy of the same rule, which would have to be
    # edited in lockstep on a rename and would go red for the wrong reason if it were not.
    eq "a name is held back at all" "$([ -n "$HELD_NAME" ] && echo yes || echo no)" "yes"
    # Captured so the restore further down reads these and does not re-spell them as a second copy.
    DEFAULT_BUDGET="$DENY_BUDGET_BYTES"; DEFAULT_KEEP="$KEEP_NEWEST"

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
    #
    # MTIME ORDER DISAGREES WITH PATH ORDER, DELIBERATELY. Arranged the other way -- which is how
    # this fixture first read, every mtime ascending with its LC_ALL=C path -- "kept the two
    # newest" is satisfied by path order alone. Measured: replacing STAT_FMT's GNU format with
    # `-c "x${TAB}%n"`, the mtime field a constant, still printed `sweep-transcripts self-test
    # passed` including that row, because with a non-numeric key `sort -k1,1n` ties every record
    # and falls back to the path key for the identical set. So `%y` (a date string) or `%W` (0 on
    # ext4) shipped green, and in the container KEEP_NEWEST would have protected the 32
    # lexicographically-last paths rather than the newest -- the live session's own <uuid>.jsonl
    # archived out from under it. Here 1111.jsonl is the NEWEST and 2222.jsonl the OLDEST, so path
    # order would archive {1111, 2222, agent-a} and mtime order archives {2222, agent-b, agent-a}:
    # different sets, and only the mtime one satisfies the rows below.
    mk "$slug/1111.jsonl"                                          202601050000
    mk "$slug/2222.jsonl"                                          202601010000
    mk "$slug/2222/subagents/agent-a.jsonl"                        202601030000
    mk "$slug/2222/subagents/workflows/wf_x/agent-b.jsonl"         202601020000
    # ...and the things that are NOT transcripts and must never move. The run journal is the OLDEST
    # file in the tree by a margin, so it is first in any plan its name does not spare.
    mk "$slug/2222/subagents/workflows/wf_x/journal.jsonl"         200001010000
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
    mk "$root/-tmp-d52harness-work/3333.jsonl"                     202601040000

    found="$(transcript_records "$root" | LC_ALL=C sort -t"$TAB" -k2,2 | cut -f2 | sed "s#^$root/##" | tr '\n' ' ')"
    # THE RUN JOURNAL IS IN THIS LIST, and that is the assertion. It is never archived, but its
    # path text is in the argv the budget is sizing, and the round that introduced the exclusion
    # put it in this `find` -- which feeds the projection as well as the plan -- so the sweep
    # under-projected the very number it exists to bound. See transcript_records.
    eq "every .jsonl is found, at every depth, the run journal included, and nothing else is" "$found" \
       "-home-vscode-repos-jkb/1111.jsonl -home-vscode-repos-jkb/2222.jsonl -home-vscode-repos-jkb/2222/subagents/agent-a.jsonl -home-vscode-repos-jkb/2222/subagents/workflows/wf_x/agent-b.jsonl -home-vscode-repos-jkb/2222/subagents/workflows/wf_x/journal.jsonl -tmp-d52harness-work/3333.jsonl "

    # THE FIRST FIELD IS AN MTIME, not a placeholder that sorts the same way the paths do. The row
    # above passes on path order alone if this is not also asserted -- which is exactly what
    # happened. Shape first, then meaning.
    eq "every record leads with a numeric first field" \
       "$(transcript_records "$root" | LC_ALL=C awk -F"$TAB" '$1 !~ /^[0-9]+$/ { n++ } END { print n+0 }')" "0"
    mtime_of() { transcript_records "$root" | LC_ALL=C awk -F"$TAB" -v p="$1" '$2 ~ p { print $1 }'; }
    # 1111.jsonl is the newest file and sorts FIRST by path; 2222.jsonl is the oldest and sorts
    # second. A constant, a date string or a birth-time of 0 all fail this.
    eq "...and that field is the mtime, which here disagrees with the path order" \
       "$( [ "$(mtime_of '/1111\.jsonl$')" -gt "$(mtime_of '/2222\.jsonl$')" ] 2>/dev/null && echo yes || echo no)" "yes"

    # THE SYMLINKED ROOT, which is how the container spells it: ~/.claude/projects is a link into
    # the state volume. Without `-L` this reads an empty tree and the sweep silently does nothing.
    ln -s "$root" "$work/projects-link"
    eq "a symlinked root enumerates the same files" \
       "$(transcript_records "$work/projects-link" | grep -c . )" "6"

    echo "==> sweep-transcripts self-test: what the sweep cannot reclaim still costs argv"
    held_p="$(transcript_records "$root" | transcript_unreclaimable | transcript_projection)"
    recl_p="$(transcript_records "$root" | LC_ALL=C grep -v "/$HELD_NAME\$" | transcript_projection)"
    eq "the run journal projects bytes of its own" \
       "$([ "$held_p" -gt 0 ] && echo yes || echo no)" "yes"
    # THE ROW THAT REDDENS IF THE WALK EVER HOLDS THE JOURNAL BACK AGAIN. The budget is set to what
    # the RECLAIMABLE files alone project. The true projection is higher by the journal's bytes, so
    # a sweep that counts what it cannot reclaim still has work to do; one that does not prints
    # "nothing to archive" and leaves the tree over budget. That is the shape that shipped; on the
    # real tree the projection it acted on was 65,250 against 96,612 of actual path text.
    DENY_BUDGET_BYTES="$recl_p" KEEP_NEWEST=0
    eq "a budget that the held-back bytes alone overrun still yields a plan" \
       "$([ -n "$(transcript_records "$root" | transcript_plan)" ] && echo yes || echo no)" "yes"
    eq "...and the run journal is never in that plan" \
       "$(transcript_records "$root" | transcript_plan | grep -c "/$HELD_NAME\$")" "0"
    # AND IT IS SAID OUT LOUD when the unreclaimable set alone is over budget: no plan can bring
    # the tree under, so a silent success there is a container in which no Bash call works.
    # `$root` and not `$work/projects-link` for these two, because the budget is being set from
    # bytes measured through `$root`: the two spellings are different lengths, so measuring under
    # one and sweeping under the other compares numbers that are not about the same argv. That is
    # the whole subject of this file in miniature.
    DENY_BUDGET_BYTES=$((held_p - 1)) KEEP_NEWEST=0
    eq "an unreclaimable set that alone exceeds the budget is reported" \
       "$(sweep_transcripts "$root" "$arch" --dry-run 2>&1 | grep -c 'cannot bring this tree under it')" "1"
    DENY_BUDGET_BYTES="$DEFAULT_BUDGET" KEEP_NEWEST="$DEFAULT_KEEP"
    eq "...and the shipped budget, which it is nowhere near, reports nothing of the kind" \
       "$(sweep_transcripts "$root" "$arch" --dry-run 2>&1 | grep -c 'cannot bring this tree under it')" "0"

    echo "==> sweep-transcripts self-test: a dry run changes nothing"
    DENY_BUDGET_BYTES=0 KEEP_NEWEST=2
    # 2>/dev/null: a budget of 0 is under the run journal's own bytes, so the unreclaimable
    # warning fires on every row below. It is asserted on two rows of its own above; here it is
    # noise in the gate's output, and a gate nobody can read is a gate nobody reads.
    dry_out="$(sweep_transcripts "$work/projects-link" "$arch" --dry-run 2>/dev/null)"
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
    out="$(sweep_transcripts "$work/projects-link" "$arch" 2>/dev/null)"; rc=$?
    eq "the sweep succeeds" "$rc" "0"
    eq "it archived the three oldest and kept the two newest" \
       "$(grep -c 'archived 3 file' <<<"$out")" "1"
    left="$(find "$root" -type f -name '*.jsonl' | sed "s#^$root/##" | LC_ALL=C sort | tr '\n' ' ')"
    # THE SURVIVORS ARE THE TWO NEWEST BY MTIME, and they are NOT the two that survive if the sort
    # falls back to path order: that set is {agent-b, 3333}, this one is {1111, 3333}. This row is
    # where the fixture's deliberate disagreement (see the mk block) pays for itself.
    # The run journal is here too -- the oldest file in the tree, first in any plan its name does
    # not spare, so its presence is the assertion that the plan spares it.
    eq "the newest two by mtime, and the run journal, are still where they were" "$left" \
       "-home-vscode-repos-jkb/1111.jsonl -home-vscode-repos-jkb/2222/subagents/workflows/wf_x/journal.jsonl -tmp-d52harness-work/3333.jsonl "
    # THE DASH-LEADING SLUG SURVIVES THE MOVE. If `${rel%/*}` had been `dirname "$rel"` this
    # directory would not exist and the files would be in the archive root, or nowhere.
    eq "the archive keeps the dash-leading slug as a directory" \
       "$([ -f "$arch/-home-vscode-repos-jkb/2222.jsonl" ] && echo yes || echo no)" "yes"
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
    #
    # AND THROUGH EVERY SPELLING, not just the one the fixture happens to pass. Only the first row
    # here stood before, and it is the row that passes either way: the refusal was a string-prefix
    # test on the CALLER'S spelling, so with the root spelled through a symlink -- the only way the
    # container spells it -- the same containment was accepted, and the .archive/.archive/ nesting
    # the refusal exists to stop reproduced at 530 -> 602 -> 674 deny bytes. The last row is the
    # `..` route into the same state.
    eq "an archive inside the root is refused" \
       "$(rc_of sweep_transcripts "$root" "$root/.archive")" "1"
    eq "...with the root spelled through a symlink, as the container spells it" \
       "$(rc_of sweep_transcripts "$work/projects-link" "$root/.archive")" "1"
    eq "...with the archive spelled through that symlink instead" \
       "$(rc_of sweep_transcripts "$root" "$work/projects-link/.archive")" "1"
    eq "...and by way of a .. that climbs back in" \
       "$(rc_of sweep_transcripts "$root" "$arch/../projects/.archive")" "1"
    eq "...and nothing was created for any of them" \
       "$([ -e "$root/.archive" ] && echo yes || echo no)" "no"
    # A SIBLING ARCHIVE IS STILL ACCEPTED, so the four rows above are a refusal and not a function
    # that refuses everything -- which would pass all four and archive nothing for ever.
    eq "a sibling archive, which is what ships, is accepted" \
       "$(rc_of sweep_transcripts "$work/projects-link" "$arch")" "0"

    echo "==> sweep-transcripts self-test: running it again, and running it on nothing"
    archived_before="$(find "$arch" -type f | grep -c . )"
    left_before="$(find "$root" -type f -name '*.jsonl' | LC_ALL=C sort | tr '\n' ' ')"
    # THE FLOOR IS OFF AND THE BUDGET IS EXACTLY WHAT THE TREE ALREADY PROJECTS, so an empty plan
    # is the sweep deciding it is under budget. The rows that stood here left KEEP_NEWEST=2 over a
    # population of 2, so `n - keep` was zero and the loop never ran: they re-measured the floor
    # that is already asserted above, and any regression in the re-run path -- re-enumerating the
    # archive, nesting, a plan recomputed against moved files -- reported ok under the heading
    # written to catch it. Inserting KEEP_NEWEST=0 here turned both of them red, which is the proof.
    DENY_BUDGET_BYTES="$(transcript_records "$root" | transcript_projection)" KEEP_NEWEST=0
    eq "a second sweep of a tree already under budget succeeds" \
       "$(rc_of sweep_transcripts "$work/projects-link" "$arch")" "0"
    eq "...and moves nothing, with no floor doing the work" \
       "$(find "$root" -type f -name '*.jsonl' | LC_ALL=C sort | tr '\n' ' ')" "$left_before"
    eq "...and adds nothing to the archive" \
       "$(find "$arch" -type f | grep -c . )" "$archived_before"
    # AND A RE-RUN THAT DOES MOVE SOMETHING, which is the path the nesting defect lived on: the
    # second sweep must archive from the TREE and never re-enumerate its own archive. One file, so
    # the count is exact rather than "at least".
    DENY_BUDGET_BYTES=0 KEEP_NEWEST=1
    out2="$(sweep_transcripts "$work/projects-link" "$arch" 2>/dev/null)"; rc2=$?
    eq "a re-run that binds succeeds" "$rc2" "0"
    eq "...and archives from the tree, not from the archive the last one wrote" \
       "$(grep -c 'archived 1 file' <<<"$out2")" "1"
    eq "...growing the archive by exactly that one" \
       "$(find "$arch" -type f | grep -c . )" "$((archived_before + 1))"
    eq "...and leaving the newest transcript and the run journal behind" \
       "$(find "$root" -type f -name '*.jsonl' | sed "s#^$root/##" | LC_ALL=C sort | tr '\n' ' ')" \
       "-home-vscode-repos-jkb/1111.jsonl -home-vscode-repos-jkb/2222/subagents/workflows/wf_x/journal.jsonl "
    DENY_BUDGET_BYTES="$DEFAULT_BUDGET" KEEP_NEWEST="$DEFAULT_KEEP"
    mkdir -p "$work/empty"
    eq "an empty root succeeds"  "$(rc_of sweep_transcripts "$work/empty" "$arch")" "0"
    eq "an absent root succeeds" "$(rc_of sweep_transcripts "$work/nothing-here" "$arch")" "0"
    eq "...and creates no archive for it" \
       "$([ -d "$work/nothing-here" ] && echo yes || echo no)" "no"

    echo "==> sweep-transcripts self-test: the script as a program"
    # EVERYTHING ABOVE CALLS THE FUNCTIONS. Nothing above runs the FILE, so the root resolution at
    # the top and the argument dispatch at the bottom were executed by no test at all: writing
    # `CLAUDE_BASE="${CLAUDE_CONFIG_DIR:-$HOME}/.claude"` -- one brace out of place -- made the
    # sweep walk `$CLAUDE_CONFIG_DIR/.claude/projects`, print "does not exist", exit 0, and leave
    # this self-test green, and check-config.sh's static guard green too -- it can see that the
    # config base is DERIVED from CLAUDE_CONFIG_DIR, never that the derivation is spelled right.
    # That is exactly the "a sweep that cannot find its subject must not look successful" failure
    # the comment at the top claims to have closed. These rows read the root back out of the
    # message, which is the only place it is observable from outside.
    #
    # `prog <VAR=val>... -- <script arg>...`. Two things in one helper:
    #   the `--`   without it the script's own flags land among `env`'s options, where `--dry-run`
    #              is an unknown option and not a run.
    #   the `-u`s  whatever ran this self-test may have these set, and a row that silently reads
    #              the developer's own environment establishes nothing.
    # `${envs[@]+...}` because bash 3.2 (which is what a Mac ships, and this file self-tests on
    # macOS) treats an empty array under `set -u` as unbound.
    prog() {
        local envs=()
        while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do envs+=("$1"); shift; done
        [ "$#" -eq 0 ] || shift
        env -u CLAUDE_CONFIG_DIR -u JKB_TRANSCRIPT_ROOT -u JKB_TRANSCRIPT_ARCHIVE \
            ${envs[@]+"${envs[@]}"} bash "$self" "$@"
    }
    phome="$work/phome"; mkdir -p "$phome/.claude/projects"   # exists, empty
    palt="$work/palt"                                          # $palt/projects absent
    proot="$work/proot"; mkdir -p "$proot"                     # exists, empty
    eq "with only HOME set it sweeps \$HOME/.claude/projects" \
       "$(prog HOME="$phome" -- 2>&1)" \
       "transcript sweep: no transcripts under $phome/.claude/projects"
    eq "CLAUDE_CONFIG_DIR moves the root, as every other site in this repo honours it" \
       "$(prog HOME="$phome" CLAUDE_CONFIG_DIR="$palt" -- 2>&1)" \
       "transcript sweep: $palt/projects does not exist — nothing to sweep"
    eq "JKB_TRANSCRIPT_ROOT overrides both" \
       "$(prog HOME="$phome" CLAUDE_CONFIG_DIR="$palt" JKB_TRANSCRIPT_ROOT="$proot" -- 2>&1)" \
       "transcript sweep: no transcripts under $proot"
    eq "--dry-run is accepted"  "$(rc_of prog HOME="$phome" -- --dry-run)" "0"
    eq "a bare run is accepted" "$(rc_of prog HOME="$phome" --)" "0"
    eq "an unknown argument is a usage error" "$(rc_of prog HOME="$phome" -- --wat)" "2"
    # `--self-test` guards on `$# -eq 1`, so a second argument must fall through to that usage
    # error rather than quietly running the suite with an argument nobody reads.
    eq "--self-test with a trailing argument is a usage error" \
       "$(rc_of prog HOME="$phome" -- --self-test extra)" "2"

    echo
    [ "$fails" -eq 0 ] || { printf '\033[31msweep-transcripts self-test FAILED (%d)\033[0m\n' "$fails"; exit 1; }
    printf '\033[32msweep-transcripts self-test passed\033[0m\n'
    exit 0
fi

case "${1:-}" in
    ""|--dry-run) sweep_transcripts "$TRANSCRIPT_ROOT" "$TRANSCRIPT_ARCHIVE" "${1:-}" ;;
    *) printf 'usage: %s [--dry-run|--self-test]\n' "$0" >&2; exit 2 ;;
esac
