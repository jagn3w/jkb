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

# THE SEAMS, NAMED ONCE AND IN ONE PLACE: every JKB_ input this file reads. Most exist for
# --self-test, which cannot drive this file as a program without them; JKB_KEEP_SESSIONS is the
# host reaper's, and belongs here for the same reason — every one of them can switch the sweep off
# (a keep-list naming everything protects everything): a root that does
# not exist, an archive somewhere harmless, or a budget nothing ever reaches all produce a start
# that reports success for ever while the deny list grows. check-config.sh refuses every name on
# this line in run.sh, the Dockerfile, container.json and entrypoint.sh, and reads the list from
# HERE rather than spelling it -- the guard that shipped covered one of the three, which is the
# same half-a-guard shape this file has now recorded four times.
# CLAUDE_CONFIG_DIR is deliberately NOT here: it is a legitimate thing for a shipped file to set,
# and the whole point of honouring it is that the sweep follows it.
# HAND-WRITTEN, AND CHECKED AGAINST REALITY BY check-config.sh, which derives the same set from
# every `${JKB_…:-}` this file actually reads and requires the two to agree. A declaration nothing
# compares to the code is a fourth seam waiting to be refused by nothing.
SEAMS="JKB_TRANSCRIPT_ROOT JKB_TRANSCRIPT_ARCHIVE JKB_DENY_BUDGET_BYTES JKB_NOW_SECS"
# ...AND EVERY JKB_ INPUT, which is a longer list than the seams. JKB_KEEP_SESSIONS is NOT a
# seam: it is the sweep's live production input, the sessions a caller knows to be running, and
# both triggers pass it. Refusing it in shipped files -- the blanket rule the four above earn --
# would refuse the thing run.sh is supposed to do, with a message saying it silently disables the
# sweep, which is false for this one variable. It still needs the rest: check-config compares
# this list against what the script actually reads, and --self-test neutralises all of it so a
# developer's exported value cannot reach a row.
INPUTS="$SEAMS JKB_KEEP_SESSIONS"

# MAX_ARG_STRLEN on Linux: 32 pages. Recorded for the reader; the budget below is derived from it.
ARGV_MAX_BYTES=131072
# Half of it. The other half is headroom for what this sweep does NOT count: the security paths in
# the same deny list, the write-side allow/deny lists in the same argument, and the JSON quoting
# around every entry. Half is not a measurement, it is a margin — and a margin is what the previous
# arrangement (none) lacked.
#
# THAT SET IS NOT FIXED, and this comment used to call it "the ~30 fixed security paths". Measured
# in a session on 2026-09-28: 89 deny paths, SIX of them registered git worktrees — and D36 gives
# every task its own worktree, so the uncounted half grows with exactly the workload this sweep was
# written for. No sweep can reclaim a worktree path. A few hundred bytes against 65,536 today, but
# the error runs in the direction that overflows, and it compounds with the spelling lean below:
# both have to be closed before anyone tightens this margin.
# JKB_DENY_BUDGET_BYTES is a TEST SEAM, alongside JKB_TRANSCRIPT_ROOT and JKB_TRANSCRIPT_ARCHIVE,
# and the only way --self-test can drive this file as a PROGRAM against a tree small enough to
# build in a temp directory -- without it, every program-level row has to point at an empty root,
# where the sweep returns before it reads its own arguments. Nothing sets it in the container.
DENY_BUDGET_BYTES="${JKB_DENY_BUDGET_BYTES:-$((ARGV_MAX_BYTES / 2))}"
# Every transcript is listed TWICE, once per spelling of the root (see the measurement above).
# If that symlink ever goes, this becomes 1 and the sweep simply keeps twice as many files.
# IT LEANS LOW, DELIBERATELY BUT KNOWABLY. The two spellings are not the same length --
# ~/.claude/projects/… and ~/.claude-state/projects/… differ by 6 bytes per path -- and what is
# multiplied here is the CALLER'S spelling, which in the container is the shorter one. So the real
# deny list is larger than this projection by 6 bytes a file: ~7KB at 1,182 files, about 11% of the
# budget. The half-of-MAX_ARG_STRLEN margin is what absorbs it. Anyone tightening that margin has
# to close this gap first, because the error is in the direction that overflows.
DENY_SPELLINGS=2
# KEPT UNCONDITIONALLY, whatever the budget says, because the live session is writing one of these
# right now and archiving it out from under Claude Code is data loss with a plausible-looking
# cause. This is a FLOOR, not a cap: the sweep stops as soon as the projection is under budget, so
# at ~194 bytes a path it keeps about 169 files in the ordinary case and this number never binds.
# It binds only if the newest 32 paths alone would blow the budget, and then keeping them is still
# the right answer — a container that cannot resume its own session is worse than one whose deny
# list is a little long.
KEEP_NEWEST=32
# ...AND TWO GUARDS THAT ARE ABOUT LIVENESS, NOT RETENTION, added when the sweep gained a SECOND
# trigger. KEEP_NEWEST's whole argument was "the live session is writing one of them right now", and
# it was written for a sweep that ran at container START, when nothing is open. On the host reaper's
# timer it runs mid-flight, and during a swarm more than 32 transcripts are touched inside one
# window -- so the newest-32 floor stops being a statement about live sessions and a running
# session's transcript can be archived out from under it, at which point `/resume` cannot find it.
#
# THE REGISTRY IS THE PRECISE ANSWER and the window is the belt to its brace. jkb knows which
# Claude sessions are live (every hook in the container posts to the host daemon), and a transcript
# is named for its session, so the reaper passes those ids in and they are never planned. The
# window then covers what the registry cannot see: a session that predates the registry, one whose
# hooks are not reporting, a container not in remote mode. Neither is time-based RETENTION, which
# this file rejects above and still rejects -- what bounds the sweep is bytes. These say only that a
# file written moments ago is probably open, which is a different claim from "old files may go".
KEEP_MODIFIED_WITHIN_SECS=3600
# Space-separated session ids that must not be archived whatever the arithmetic says. Empty unless
# a caller knows better; the reaper fills it from the registry, `run.sh` leaves it alone.
KEEP_SESSIONS="${JKB_KEEP_SESSIONS:-}"
# Overridable so --self-test can place the fixture's mtimes relative to a fixed present; the
# freshness window is meaningless against a clock the suite does not control.
NOW_SECS="${JKB_NOW_SECS:-$(date +%s)}"
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
# THE LAST FIELD, so this takes records (`<mtime><TAB><path>`) and bare paths (a plan, piped
# straight in) alike. One counter for both, because the alternative is writing the multiplication
# out a second time for the plan's residual, and two copies of the arithmetic that decides whether
# Bash can spawn is the defect this file keeps rediscovering in other forms.
transcript_projection() { # transcript_projection < records|paths -> bytes
    LC_ALL=C awk -F"$TAB" -v mult="$DENY_SPELLINGS" \
        '{ tot += length($NF) } END { printf "%d\n", mult * tot }'
}

# IS THE TREE STILL OVER BUDGET? Nothing asked this until round 3, which is remarkable for a script
# whose entire subject is a budget: `before`, `after` and DENY_BUDGET_BYTES were printed in one
# line with nothing comparing any pair of them. Two reproductions, both rc 0 and both "success":
# an EMPTY PLAN over budget (3 sessions + 3 journals, 702 projected against 500, KEEP_NEWEST=32 --
# `n - keep` is negative, so the plan is empty however far over the tree is), and an EXHAUSTED PLAN
# over budget (43 sessions, budget 500, `archived 11 file(s) (4086 -> 3076 deny bytes, budget
# 500)`). On the real container the second is reachable on the journals' growth alone: the residual
# after a full plan is held_bytes plus the newest KEEP_NEWEST, which passes 65,536 at ~161 journals
# while the unreclaimable warning only fires past ~199. In that window every Bash call dies at spawn
# and the sweep says it worked.
transcript_over_budget() { # transcript_over_budget <projection> <what> -> rc 1 + message if over
    [ "$1" -gt "$DENY_BUDGET_BYTES" ] || return 0
    printf 'transcript sweep: %s deny bytes %s, over the %s byte budget — Bash may still fail at spawn with E2BIG\n' \
        "$1" "$2" "$DENY_BUDGET_BYTES" >&2
    return 1
}

# ...and did this sweep reduce anything at all? Pure, over the three numbers the summary already
# prints, so --self-test can drive it from literals rather than only through a filesystem it cannot
# easily put into the failing state. See the call site for what it is watching for.
transcript_projection_fell() { # transcript_projection_fell <moved> <before> <after>
    [ "$1" -gt 0 ] || return 0
    [ "$3" -lt "$2" ]
}

# Which files to archive, OLDEST FIRST, to bring the projection under budget — and no more than
# that. Stops at the first record that brings it under, so an ordinary start archives a handful.
#
# SORTED BY MTIME AND THEN BY PATH. A task swarm writes a dozen agent transcripts in the same
# second, and a tie broken by the order `find` happened to walk the tree makes this function
# untestable and its behaviour different on two containers with identical contents.
#
# WHAT THE SECOND KEY ACTUALLY BUYS, stated honestly because the row below cannot show it: POSIX,
# GNU and BSD `sort` all fall back to comparing the WHOLE LINE when every key ties, and a tied
# record's mtime field is byte-identical, so path order arrives either way -- removing `-k2,2`
# leaves the self-test green, measured. The key earns its place by surviving `-s`, which disables
# that last resort and would hand the choice back to walk order, and by saying out loud which
# ordering this function promises. The row below asserts the PROMISE, not the flag.
#
# THE HELD NAME IS SPARED HERE, AND `tot` IS WHY. Every record's bytes go into the projection,
# including the ones no plan may ever contain, because the kernel counts a path the sweep cannot
# reclaim exactly like one it can. `keep` is a floor on the ARCHIVABLE population: a held file was
# never a candidate, so counting it toward the floor would reserve a slot no plan could have used.
# IS THIS FILE ONE A LIVE SESSION MAY BE WRITING? Asked by transcript_plan, which must not plan
# one, and by transcript_irreducible, which must count one -- and the day those two disagree is the
# day a sweep archives a live transcript while reporting itself unable to reclaim anything. So it is
# one text, handed to both awks, rather than a rule each of them remembers.
AWK_PROTECTED='
function protected(path, mtime,   i, id) {
    if (now > 0 && fresh > 0 && mtime > now - fresh) return 1
    for (i = 1; i <= nlive; i++) {
        id = LIVE[i]
        if (id == "") continue
        # BY SESSION DIRECTORY, NOT BY LEAF NAME. A session writes <slug>/<uuid>.jsonl AND
        # everything under <slug>/<uuid>/subagents/... -- and those nested agent transcripts are
        # the BULK of the population, as this file says of the walk. Matching only the basename
        # protected the first and left the majority to the recency window alone, which a subagent
        # sitting an hour on one tool call or a permission prompt walks straight out of. The
        # registry has no row for a Task-tool subagent, so no id of its own ever appears -- but the
        # session it belongs to has one, and every file it writes is under that session directory.
        if (index(path, "/" id ".jsonl") > 0) return 1
        if (index(path, "/" id "/") > 0) return 1
    }
    return 0
}
BEGIN { nlive = split(live, LIVE, " ") }
'

transcript_plan() { # transcript_plan < records -> paths to archive, oldest first
    LC_ALL=C sort -t"$TAB" -k1,1n -k2,2 \
    | LC_ALL=C awk -F"$TAB" -v budget="$DENY_BUDGET_BYTES" -v keep="$KEEP_NEWEST" \
                   -v mult="$DENY_SPELLINGS" -v held="$HELD_NAME" \
                   -v live="$KEEP_SESSIONS" -v now="$NOW_SECS" -v fresh="$KEEP_MODIFIED_WITHIN_SECS" \
        "$AWK_PROTECTED"'
        { tot += length($2)
          base = $2; sub(/^.*\//, "", base)
          if (base == held) next
          # A LIVE SESSION IS NEVER PLANNED. Skipped like the held name and before the floor,
          # because a file that may not be archived was never a candidate: letting it consume a
          # `keep` slot would reserve protection for something already protected.
          if (protected($2, $1)) next
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

# THE SMALLEST DENY LIST ANY SWEEP COULD LEAVE: everything it may never archive, PLUS the newest
# KEEP_NEWEST it will never archive. If that is over budget, no number of sweeps helps.
#
# THE FLOOR IS HALF OF IT, and asking only about the held set left a window where the tree was
# equally beyond help and did not say so. This file's own numbers give it: the residual after a full
# plan is held plus the newest KEEP_NEWEST, which passes 65,536 at roughly 161 run journals, while a
# held-only test only speaks past about 199. A container reaches the first on its way to the second,
# so the unhelpable state that gets REPORTED as unhelpable was the second one it arrives at.
# Sorted newest-first so the first `keep` reclaimable records are the ones the floor protects.
transcript_irreducible() { # transcript_irreducible < records -> bytes no sweep can remove
    LC_ALL=C sort -t"$TAB" -k1,1nr -k2,2r \
    | LC_ALL=C awk -F"$TAB" -v keep="$KEEP_NEWEST" -v mult="$DENY_SPELLINGS" -v held="$HELD_NAME" \
                   -v live="$KEEP_SESSIONS" -v now="$NOW_SECS" -v fresh="$KEEP_MODIFIED_WITHIN_SECS" \
        "$AWK_PROTECTED"'
        { base = $2; sub(/^.*\//, "", base)
          if (base == held)        { tot += length($2); next }
          # ...AND WHAT A LIVE SESSION HOLDS, which this did not count. A container recreated after
          # a heavy swarm -- the standard recovery from an E2BIG -- has every transcript inside the
          # recency window, so no plan reaches the budget while this reported a number under it:
          # verify.sh then fell past `accept_bad` into plain `bad`, exit 1 instead of 3, run.sh
          # refused to open a window, and the causes it printed were all wrong. Protection here is
          # about NOW: the share of it the recency window contributes lapses, and the message says so.
          if (protected($2, $1)) { tot += length($2); next }
          if (kept < keep)       { tot += length($2); kept++ } }
        END { printf "%d\n", mult * tot }'
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
    local p="$1" tail="" base out seg
    case "$p" in /*) ;; *) p="$PWD/$p" ;; esac
    while [ ! -d "$p" ]; do
        tail="/${p##*/}$tail"
        p="${p%/*}"
        [ -n "$p" ] || p="/"
    done
    base="$(cd "$p" && pwd -P)" || return 1
    [ "$base" != "/" ] || base=""
    # AND THE TAIL IS COLLAPSED LEXICALLY, because `pwd -P` never saw it. Putting the tail back
    # verbatim left any `..` in it intact, and on the FIRST sweep -- when nothing of the archive
    # exists yet, which is the only state that matters, since the sweep is what creates it -- the
    # whole of `…/transcript-archive/../projects/.archive` is tail. The prefix test then compared a
    # path still containing `..` and did not match, so the archive was accepted INSIDE the
    # enumerated root (deny bytes 96 -> 114 on the probe), and every later sweep refused for ever
    # because mkdir had since made the `..` collapsible. Lexical is sound here and only here: a tail
    # component that existed as a directory would have stopped the loop above, so there is no
    # symlink left in it for `..` to mean something else about.
    out="$base"
    while [ -n "$tail" ]; do
        tail="${tail#/}"
        seg="${tail%%/*}"
        case "$tail" in */*) tail="/${tail#*/}" ;; *) tail="" ;; esac
        case "$seg" in
            ''|.) ;;
            ..)   out="${out%/*}" ;;
            *)    out="$out/$seg" ;;
        esac
    done
    printf '%s\n' "${out:-/}"
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
    local abs phys_root phys_archive records plan planned irreducible err
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
    # SAID OUT LOUD, because it is the one state the sweep cannot fix, and verify.sh reads this
    # sentence to tell "this needs a decision" from "this boundary is broken". The run journals are
    # never archived and the newest KEEP_NEWEST are never archived; .claude-state is a volume, so
    # the first of those only grows. Once their sum is over budget every line below still reads as
    # success while every Bash call goes on dying at spawn.
    irreducible="$(printf '%s\n' "$records" | transcript_irreducible)"
    if [ "$irreducible" -gt "$DENY_BUDGET_BYTES" ]; then
        # ALL THREE TERMS, and the third is usually the largest. This named the held journals and
        # the floor only — so half an hour after a swarm, when the recency window covers nearly the
        # whole tree, the operator was told to delete journals worth a few hundred bytes and never
        # told that the answer is to wait. The one term that LAPSES is the one worth saying.
        printf 'transcript sweep: %s deny bytes are in files no sweep can remove right now (%s files, the newest %s kept for the live session, and anything written in the last %ss or held by a live session — that last part lapses), over the whole %s byte budget — archiving every transcript cannot bring this tree under it\n' \
            "$irreducible" "$HELD_NAME" "$KEEP_NEWEST" "$KEEP_MODIFIED_WITHIN_SECS" \
            "$DENY_BUDGET_BYTES" >&2
    fi
    plan="$(printf '%s\n' "$records" | transcript_plan)"
    if [ -z "$plan" ]; then
        printf 'transcript sweep: %s deny bytes projected, budget %s — nothing to archive\n' \
            "$before" "$DENY_BUDGET_BYTES"
        # "NOTHING TO ARCHIVE" IS NOT "NOTHING IS NEEDED", and for two rounds this branch said the
        # first and meant to be read as the second. An empty plan only means the arithmetic chose
        # no file -- including when `n - keep` is zero or negative, so a tree entirely inside the
        # floor takes this branch at any distance over budget.
        transcript_over_budget "$before" "are projected and none of them can be archived" || return 1
        return 0
    fi
    # The residual a dry run WOULD leave. The plan is bare paths; transcript_projection takes them.
    planned="$(printf '%s\n' "$plan" | transcript_projection)"

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

    # `after` IS ONLY ASKED FOR ON A REAL SWEEP. It was computed unconditionally, which meant every
    # `--dry-run` walked the whole tree a second time to produce a number the dry-run path never
    # reads -- on the tree that prompted this file, 1,182 files and two `stat` batches.
    if [ "$dry" = "--dry-run" ]; then
        # `%s -> %s`, not `-> under %s`: the old wording ASSERTED the outcome the rest of this
        # function never checked, which is how a plan that cannot reach the budget read as one that
        # does. The number is stated and then tested below.
        printf 'transcript sweep: would archive %s file(s), %s -> %s deny bytes, budget %s\n' \
            "$moved" "$before" "$((before - planned))" "$DENY_BUDGET_BYTES"
    else
        after="$(transcript_records "$abs" | transcript_projection)"
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
    if [ "$dry" != "--dry-run" ] && ! transcript_projection_fell "$moved" "$before" "$after"; then
        # BOTH CAUSES NAMED, because the comment above admits the check cannot tell them apart and
        # the message used to assert one of them as fact. An operator who reads "the archive is
        # being re-enumerated" and finds a correctly-sited archive has been sent to the wrong place.
        printf 'transcript sweep: archived %s file(s) and the projection did not fall (%s -> %s) — either the archive is being re-enumerated under the root, or transcripts are arriving faster than this sweep archives them\n' \
            "$moved" "$before" "$after" >&2
        return 1
    fi
    if [ "$failed" -gt 0 ]; then
        printf 'transcript sweep: %s file(s) could not be archived\n' "$failed" >&2
        return 1
    fi
    # THE RESIDUAL, LAST, because the two above name a cause and this one names the state the whole
    # script exists to prevent: a deny list still too long for one argv.
    #
    # A DRY RUN IS JUDGED ON THE TREE AS IT IS, NOT ON THE ONE ITS PLAN IMAGINES. This tested
    # `before - planned` -- a hypothetical -- so it exited 0 whenever the PLAN would have fit,
    # regardless of whether anything had ever been archived. verify.sh reads that code, and the
    # failure case is exactly the one it was added for: with the archive unusable (ENOSPC, a regular
    # file where the directory must go, a refused containment) the real sweep at container start
    # moves nothing and returns 1, run.sh discards that by design, and verify then printed
    # `ok  the transcript deny list fits in one argv` over a tree where not one file had moved and
    # every Bash call still died at spawn. Both codes now answer one question -- is the deny list,
    # as it stands on disk right now, too long for one argv -- and `before - planned` stays in the
    # summary above as information.
    if [ "$dry" = "--dry-run" ]; then
        transcript_over_budget "$before" "are in this tree now, and a dry run moves nothing" || return 1
    else
        transcript_over_budget "$after" "remain after this sweep" || return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------------------------
# Self-test. Runs on the host, in ./scripts/check.sh and in CI; needs no container and no Docker.
# ---------------------------------------------------------------------------------------------

if [ "${1:-}" = "--self-test" ] && [ "$#" -eq 1 ]; then
    # ONE SEAM IS REFUSED HERE, and only one: JKB_DENY_BUDGET_BYTES, whose value the constants rows
    # below actually read. Exported, it gives a red gate for a correct script, or -- worse -- a
    # green one for rows that have quietly stopped measuring the shipped number.
    #
    # THE OTHERS ARE NOT REFUSED, AND THAT IS THE POINT. The first version of this refused all three
    # plus CLAUDE_CONFIG_DIR, which made `./scripts/check.sh` exit red on a correct checkout for any
    # developer using the second-config-dir posture line 48 of this file cites approvingly -- and
    # `check.sh` stops at the first failing gate, so every step after this one silently stopped
    # running. A machine-dependent gate step degrades to a named skip in this repository; it never
    # reddens. The rest cannot affect a row anyway: the function rows pass explicit paths, and the
    # program rows below neutralise every seam with `env -u`.
    [ -z "${JKB_DENY_BUDGET_BYTES:-}" ] || {
        printf 'sweep-transcripts --self-test: JKB_DENY_BUDGET_BYTES is set in this environment; unset it and re-run\n' >&2
        exit 2
    }
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
    eq "a file written within the hour is treated as open" "$KEEP_MODIFIED_WITHIN_SECS" "3600"
    # Set here rather than asserted from the environment: unlike the others this is a caller's
    # input, so its shipped value is "whatever JKB_KEEP_SESSIONS said", and what the rows below
    # need is a known starting point.
    KEEP_SESSIONS=""
    eq "and nothing is held live unless a caller says so" "$KEEP_SESSIONS" ""
    # SET, but its VALUE is deliberately not pinned here. Its authority is swarm-status.sh's
    # discovery predicate, and check-config.sh reads it out of that file and requires agreement -- a
    # literal in this row as well would be a second copy of the same rule, which would have to be
    # edited in lockstep on a rename and would go red for the wrong reason if it were not.
    eq "a name is held back at all" "$([ -n "$HELD_NAME" ] && echo yes || echo no)" "yes"
    # Captured so the restore further down reads these and does not re-spell them as a second copy.
    DEFAULT_BUDGET="$DENY_BUDGET_BYTES"; DEFAULT_KEEP="$KEEP_NEWEST"
    DEFAULT_FRESH="$KEEP_MODIFIED_WITHIN_SECS"

    echo "==> sweep-transcripts self-test: the byte projection"
    # Two spellings of every path, so the projection is twice the path text. 10 + 10 = 20 bytes of
    # path, 40 projected.
    recs="1${TAB}/aaaa/bbbb
2${TAB}/cccc/dddd"
    eq "a path is counted once per spelling of the root" \
       "$(printf '%s\n' "$recs" | transcript_projection)" "40"
    eq "no records project no bytes" "$(printf '' | transcript_projection)" "0"
    # THE SAME FUNCTION OVER A PLAN, which is bare paths with no mtime field. The sweep pipes its
    # plan through this to work out the residual a dry run would leave, so the two must agree.
    eq "bare paths are counted the same way records are" \
       "$(printf '/aaaa/bbbb\n/cccc/dddd\n' | transcript_projection)" "40"

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
    # Ties are ordinary: a swarm writes a dozen agent transcripts in the same second. This asserts
    # the promise -- scrambled input, path order out -- and not the presence of `-k2,2`, which
    # sort's last-resort whole-line comparison makes redundant for exactly this input shape.
    DENY_BUDGET_BYTES=0 KEEP_NEWEST=0
    tied="7${TAB}/bbbb/bbbb
7${TAB}/aaaa/aaaa
7${TAB}/cccc/cccc"
    eq "files sharing an mtime are ordered by path, so the choice is repeatable" \
       "$(printf '%s\n' "$tied" | transcript_plan | tr '\n' ' ')" \
       "/aaaa/aaaa /bbbb/bbbb /cccc/cccc "

    echo "==> sweep-transcripts self-test: did the sweep reduce anything"
    # DRIVEN FROM LITERALS, because the state this exists to catch -- files moved and the deny list
    # no smaller -- is refused outright upstream, so the fixture cannot easily be put into it.
    # Reproduced by hand against the real script with ONLY the containment refusal neutered:
    # `archived 2 file(s) and the projection did not fall (408 -> 444)`, rc 1, on the second sweep.
    eq "moving nothing is not a failure"      "$(rc_of transcript_projection_fell 0 100 100)" "0"
    eq "a projection that fell is fine"       "$(rc_of transcript_projection_fell 3 100 40)"  "0"
    eq "a move that changed nothing is not"   "$(rc_of transcript_projection_fell 3 100 100)" "1"
    eq "and one that GREW is the nesting signature" \
       "$(rc_of transcript_projection_fell 2 408 444)" "1"

    echo "==> sweep-transcripts self-test: a live session is never planned"
    # THE FLOOR IS NOT A STATEMENT ABOUT LIVE SESSIONS once the sweep runs on a timer. These two rows
    # are what makes it one again. `KEEP_NEWEST=0` throughout, so nothing here is the floor doing the
    # work: the only thing standing between these files and the plan is that they are live.
    live_recs="100${TAB}/p/-s/aaaaaaaa.jsonl
200${TAB}/p/-s/bbbbbbbb.jsonl
300${TAB}/p/-s/cccccccc.jsonl"
    DENY_BUDGET_BYTES=0 KEEP_NEWEST=0 NOW_SECS=0 KEEP_SESSIONS=""
    eq "with nothing live and no clock, every file is a candidate" \
       "$(printf '%s\n' "$live_recs" | transcript_plan | tr '\n' ' ')" \
       "/p/-s/aaaaaaaa.jsonl /p/-s/bbbbbbbb.jsonl /p/-s/cccccccc.jsonl "
    # BY NAME, from the registry: a transcript is named for its session, and the reaper passes the
    # ids of every session jkb knows to be live. This is the precise half.
    KEEP_SESSIONS="bbbbbbbb"
    eq "a session the registry calls live is not planned, whatever the budget wants" \
       "$(printf '%s\n' "$live_recs" | transcript_plan | tr '\n' ' ')" \
       "/p/-s/aaaaaaaa.jsonl /p/-s/cccccccc.jsonl "
    KEEP_SESSIONS="aaaaaaaa cccccccc"
    eq "...and more than one of them" \
       "$(printf '%s\n' "$live_recs" | transcript_plan | tr '\n' ' ')" "/p/-s/bbbbbbbb.jsonl "
    # ...AND EVERYTHING THAT SESSION WROTE BENEATH ITSELF, which is the half that was missing. A
    # Task-tool subagent opens no session, so the registry has no row of its own for
    # <slug>/<uuid>/subagents/agent-X.jsonl -- and those nested transcripts are the BULK of the
    # population. Matched on the leaf name alone they were left to the recency window, which a
    # subagent sitting an hour on one tool call or a permission prompt walks straight out of.
    nested_recs="100${TAB}/p/-s/uuuu.jsonl
100${TAB}/p/-s/uuuu/subagents/agent-a.jsonl
100${TAB}/p/-s/uuuu/subagents/workflows/wf_1/agent-b.jsonl
100${TAB}/p/-s/vvvv/subagents/agent-c.jsonl"
    DENY_BUDGET_BYTES=0 KEEP_NEWEST=0 NOW_SECS=0 KEEP_SESSIONS="uuuu"
    eq "a live session protects its nested agent transcripts, not just its own file" \
       "$(printf '%s\n' "$nested_recs" | transcript_plan | tr '\n' ' ')" \
       "/p/-s/vvvv/subagents/agent-c.jsonl "
    # ...and what it protects is counted as beyond any sweep, or verify.sh reports a tree no plan
    # can shrink as a broken boundary and prints causes that do not apply.
    eq "what a live session holds counts toward what no sweep can remove" \
       "$([ "$(printf '%s\n' "$nested_recs" | transcript_irreducible)" -gt 0 ] && echo yes || echo no)" "yes"
    KEEP_SESSIONS=""
    eq "...and with nothing live, none of it is" \
       "$(printf '%s\n' "$nested_recs" | transcript_irreducible)" "0"

    # A PREFIX IS NOT A MATCH. The list is searched with its separators, so a session whose id is a
    # substring of a live one is still archivable -- ids are uuids and this is cheap to get wrong.
    KEEP_SESSIONS="bbbb"
    eq "an id that merely contains a live one is still planned" \
       "$(printf '%s\n' "$live_recs" | transcript_plan | tr '\n' ' ')" \
       "/p/-s/aaaaaaaa.jsonl /p/-s/bbbbbbbb.jsonl /p/-s/cccccccc.jsonl "
    # BY RECENCY, which covers what the registry cannot see: a session predating it, one whose hooks
    # are not reporting, a container not in remote mode. `now` is fixed here because a window
    # measured against a clock the suite does not control asserts nothing.
    # now 1000 less a 850s window is a cutoff of 150, so the 200 and 300 files are inside it.
    KEEP_SESSIONS="" NOW_SECS=1000 KEEP_MODIFIED_WITHIN_SECS=850
    eq "a file written inside the window is not planned" \
       "$(printf '%s\n' "$live_recs" | transcript_plan | tr '\n' ' ')" "/p/-s/aaaaaaaa.jsonl "
    KEEP_MODIFIED_WITHIN_SECS=1
    eq "...and once it is outside, it is a candidate again" \
       "$(printf '%s\n' "$live_recs" | transcript_plan | tr '\n' ' ')" \
       "/p/-s/aaaaaaaa.jsonl /p/-s/bbbbbbbb.jsonl /p/-s/cccccccc.jsonl "
    # AND NEITHER SKIP CONSUMES A FLOOR SLOT. A file that may not be archived was never a candidate,
    # so letting it hold a `keep` place would reserve protection for something already protected.
    KEEP_SESSIONS="aaaaaaaa" KEEP_MODIFIED_WITHIN_SECS=0 KEEP_NEWEST=1
    eq "the floor still protects a real candidate, not a slot spent on a live one" \
       "$(printf '%s\n' "$live_recs" | transcript_plan | tr '\n' ' ')" "/p/-s/bbbbbbbb.jsonl "
    # ...and their bytes are still PROJECTED: the argv counts what the sweep may not touch.
    KEEP_SESSIONS="aaaaaaaa bbbbbbbb cccccccc" KEEP_NEWEST=0
    eq "a tree that is entirely live still projects its bytes" \
       "$(printf '%s\n' "$live_recs" | transcript_projection)" "120"
    eq "...and plans nothing" "$(printf '%s\n' "$live_recs" | transcript_plan)" ""
    DENY_BUDGET_BYTES="$DEFAULT_BUDGET" KEEP_NEWEST="$DEFAULT_KEEP"
    KEEP_SESSIONS="" NOW_SECS=0 KEEP_MODIFIED_WITHIN_SECS="$DEFAULT_FRESH"

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
    # A NON-TRANSCRIPT OUTSIDE memory/, which the fixture did not have. Every other non-.jsonl file
    # here lives in the auto-memory store, reached through the `memory` symlink and so already
    # behind the -prune -- so both rows that looked like coverage of the `-name '*.jsonl'` filter
    # were satisfied by the prune alone, and widening the filter to `-name '*'` left the whole
    # suite green. This file is what makes the name half of the walk discriminate.
    mk "$slug/2222/subagents/notes.md"                             202601010000
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

    # THE FLOOR IS HALF OF "NO SWEEP CAN FIX THIS", and the rows above cover only the other half:
    # both hold KEEP_NEWEST at 0, so deleting the floor term from transcript_irreducible left every
    # gate green while verify.sh silently reclassified over-budget trees from accept_bad (exit 3, a
    # condition to act on, with a remedy) to bad (exit 1, a broken boundary, and run.sh refuses to
    # open a window). This is the 161-to-199-journal window the README says this closed.
    #
    # The budget sits BETWEEN the two, so only counting both reaches it: above what the run journal
    # projects alone, below what it projects plus the newest KEEP_NEWEST.
    floor_only_p="$(transcript_records "$root" | transcript_unreclaimable | transcript_projection)"
    KEEP_NEWEST=2
    both_p="$(transcript_records "$root" | transcript_irreducible)"
    eq "the floor adds bytes the held set alone does not account for" \
       "$([ "$both_p" -gt "$floor_only_p" ] && echo yes || echo no)" "yes"
    DENY_BUDGET_BYTES=$(( (floor_only_p + both_p) / 2 ))
    eq "a tree the floor alone puts beyond any sweep's help says so" \
       "$(sweep_transcripts "$root" "$arch" --dry-run 2>&1 | grep -c 'cannot bring this tree under it')" "1"
    eq "...and the same tree with no floor is merely over budget, not beyond help" \
       "$(KEEP_NEWEST=0; sweep_transcripts "$root" "$arch" --dry-run 2>&1 | grep -c 'cannot bring this tree under it')" "0"
    DENY_BUDGET_BYTES="$DEFAULT_BUDGET" KEEP_NEWEST="$DEFAULT_KEEP"

    echo "==> sweep-transcripts self-test: a sweep that cannot reach the budget says so"
    # NOTHING COMPARED THE PROJECTION TO THE BUDGET until round 3 -- in a script whose whole subject
    # is a budget, with `before`, `after` and DENY_BUDGET_BYTES printed side by side in one line.
    # Both branches reported success over budget; both are watched here, through --dry-run so the
    # fixture is not disturbed.
    #
    # THE EXHAUSTED PLAN: budget 0 is unreachable by construction, so the sweep plans everything the
    # floor allows and must still say what remains instead of printing a clean summary.
    DENY_BUDGET_BYTES=0 KEEP_NEWEST=2
    over_out="$(sweep_transcripts "$work/projects-link" "$arch" --dry-run 2>&1)"; over_rc=$?
    eq "a plan that cannot reach the budget returns non-zero" "$over_rc" "1"
    # THE TREE AS IT IS, not the tree the plan imagines. A dry run moves nothing, so a verdict about
    # `before - planned` is a verdict about a state that does not exist -- and verify.sh reads this
    # exit code, so with the archive unusable it printed `ok the transcript deny list fits in one
    # argv` over a tree where nothing had moved.
    eq "...and says how much is in the tree now, not what a plan would leave" \
       "$(grep -c 'are in this tree now, and a dry run moves nothing' <<<"$over_out")" "1"
    eq "...and its summary no longer claims the residual is \"under\" the budget" \
       "$(grep -c -- '-> under' <<<"$over_out")" "0"
    # THE EMPTY PLAN, which is the branch that read as success for two rounds: with the floor above
    # the whole population `n - keep` is negative and the plan is empty however far over the tree is.
    DENY_BUDGET_BYTES=1 KEEP_NEWEST=99
    empty_out="$(sweep_transcripts "$work/projects-link" "$arch" --dry-run 2>&1)"; empty_rc=$?
    eq "an empty plan over budget returns non-zero too" "$empty_rc" "1"
    eq "...so \"nothing to archive\" cannot stand as the whole answer" \
       "$(grep -c 'none of them can be archived' <<<"$empty_out")" "1"

    echo "==> sweep-transcripts self-test: a dry run changes nothing"
    # A BUDGET THIS TREE CAN ACTUALLY REACH, derived from the fixture rather than guessed: what the
    # two newest transcripts and the run journal project -- which is exactly what a correct sweep
    # leaves. The 0 that stood here is unreachable by construction, so with the residual now
    # checked every row below would have been asserting the failure path under a happy-path label.
    # MEASURED THROUGH THE SPELLING THESE ROWS SWEEP. `$root` and `$work/projects-link` are
    # different lengths, so a budget measured under one and applied under the other is a number
    # about a different argv -- the whole subject of this file, in miniature.
    survivors_p="$(transcript_records "$work/projects-link" \
        | LC_ALL=C grep -E "/(1111\.jsonl|3333\.jsonl|$HELD_NAME)\$" | transcript_projection)"
    DENY_BUDGET_BYTES="$survivors_p" KEEP_NEWEST=2
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
    DENY_BUDGET_BYTES="$survivors_p" KEEP_NEWEST=2
    out="$(sweep_transcripts "$work/projects-link" "$arch")"; rc=$?
    eq "the sweep succeeds, having actually brought the tree under budget" "$rc" "0"
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
    # ASSERTED ON THE REFUSAL, NOT ON THE EXIT CODE. `rc_of` was the whole of these rows, and by
    # round 4 three of the four returned 1 for an unrelated reason: they sweep `$root` while the
    # budget around them is measured through `$work/projects-link`, the two spellings differ by a
    # byte per path, and the new residual check returns 1 on its own. Measured: deleting the entire
    # containment `case` block turned exactly ONE of the four red. So the executable half of the
    # guard against the nesting defect had come to rest on a single row balanced on an exact budget
    # equality that any fixture edit breaks in silence -- an assertion passing for the wrong reason,
    # which is the defect this file has now recorded at four different sites.
    refused() { # refused <root> <archive> -> yes when the containment refusal is what spoke
        local out
        # Captured and then matched, never `| grep -q`: grep exits at its first match, the producer
        # gets EPIPE, and `set -o pipefail` two hundred lines up turns a SUCCESSFUL match into a
        # failed pipeline. This repository has a shell test for that idiom.
        out="$(sweep_transcripts "$1" "$2" 2>&1 >/dev/null)"
        case "$out" in *"is inside"*) echo yes ;; *) echo no ;; esac
    }
    eq "an archive inside the root is refused" \
       "$(refused "$root" "$root/.archive")" "yes"
    eq "...with the root spelled through a symlink, as the container spells it" \
       "$(refused "$work/projects-link" "$root/.archive")" "yes"
    eq "...with the archive spelled through that symlink instead" \
       "$(refused "$root" "$work/projects-link/.archive")" "yes"
    eq "...and it is a refusal, not a success" \
       "$(rc_of sweep_transcripts "$root" "$root/.archive")" "1"
    eq "...and nothing was created for any of them" \
       "$([ -e "$root/.archive" ] && echo yes || echo no)" "no"

    # THE `..` ROUTE, ON A TREE WHOSE ARCHIVE DOES NOT EXIST YET, which is the only state the
    # question is ever asked in: the sweep is what creates the archive, so the first run of any
    # container has nothing there. The row that stood here ran after a real sweep had already made
    # `$arch`, so the `..` was collapsible by `pwd -P` and the refusal fired for a reason no first
    # run has -- it passed while the first-run path accepted the archive INSIDE the root and wrote
    # the nesting (96 -> 114 deny bytes on the probe).
    dd_root="$work/dd/state/projects"
    mk "$dd_root/-slug/a.jsonl" 202601010000
    eq "a .. that climbs back in is refused before the archive exists" \
       "$(refused "$dd_root" "$work/dd/state/transcript-archive/../projects/.archive")" "yes"
    eq "...and nothing was written inside the root" \
       "$([ -e "$dd_root/.archive" ] && echo yes || echo no)" "no"
    # ...and the sibling it resolves to WITHOUT the .. is still accepted, so the row above is a
    # refusal and not a resolver that refuses everything it cannot parse.
    eq "...while the plain sibling it was climbing out of is accepted" \
       "$(refused "$dd_root" "$work/dd/state/transcript-archive")" "no"
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
    DENY_BUDGET_BYTES="$(transcript_records "$work/projects-link" | transcript_projection)" KEEP_NEWEST=0
    eq "a second sweep of a tree already under budget succeeds" \
       "$(rc_of sweep_transcripts "$work/projects-link" "$arch")" "0"
    eq "...and moves nothing, with no floor doing the work" \
       "$(find "$root" -type f -name '*.jsonl' | LC_ALL=C sort | tr '\n' ' ')" "$left_before"
    eq "...and adds nothing to the archive" \
       "$(find "$arch" -type f | grep -c . )" "$archived_before"
    # AND A RE-RUN THAT DOES MOVE SOMETHING, which is the path the nesting defect lived on: the
    # second sweep must archive from the TREE and never re-enumerate its own archive. One file, so
    # the count is exact rather than "at least".
    # Again a reachable budget, measured through the spelling swept: what the newest transcript and
    # the run journal project, which is what one more archived file leaves.
    DENY_BUDGET_BYTES="$(transcript_records "$work/projects-link" \
        | LC_ALL=C grep -E "/(1111\.jsonl|$HELD_NAME)\$" | transcript_projection)" KEEP_NEWEST=1
    out2="$(sweep_transcripts "$work/projects-link" "$arch")"; rc2=$?
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

    # ...AND ON A REAL SWEEP, not only a dry run. They are two call sites, and the rows above watch
    # one of them: deleting the non-dry-run check left the whole suite green. A tree of its own, so
    # the fixture the sections above depend on is not disturbed.
    res_root="$work/res/projects"; res_arch="$work/res/archive"
    mk "$res_root/-slug/a.jsonl" 202601010000
    mk "$res_root/-slug/b.jsonl" 202601020000
    DENY_BUDGET_BYTES=1 KEEP_NEWEST=1
    res_out="$(sweep_transcripts "$res_root" "$res_arch" 2>&1)"; res_rc=$?
    eq "a real sweep still over budget when its plan runs out returns non-zero" "$res_rc" "1"
    eq "...and says how much remains" "$(grep -c 'remain after this sweep' <<<"$res_out")" "1"
    eq "...having archived the one file it could" \
       "$([ -f "$res_arch/-slug/a.jsonl" ] && echo yes || echo no)" "yes"
    DENY_BUDGET_BYTES="$DEFAULT_BUDGET" KEEP_NEWEST="$DEFAULT_KEEP"

    echo "==> sweep-transcripts self-test: when a move cannot happen"
    # `mv -n`, AND THE ONLY COPY. A session whose <slug>/<uuid>.jsonl was archived and is then
    # resumed by id recreates that same relative path, which the next sweep plans oldest-first --
    # and ~/.claude-state is a volume, so the archived copy is the only one there is. Without `-n`
    # that copy is silently overwritten by a newer file of the same name.
    #
    # WHAT IS ASSERTED IS PORTABLE AND WHAT IS NOT IS NOT. Measured on GNU coreutils 9.4, `mv -n`
    # onto an existing destination prints `mv: not replacing '…'` and exits 1; BSD/macOS `mv -n`
    # skips silently and exits 0. So the return code and the message are NOT asserted here -- this
    # file self-tests on macOS, and a row that is red on a Mac for a correct script is worse than no
    # row. Both platforms agree on the two things that matter, which are the two things below.
    coll_root="$work/coll/projects"; coll_arch="$work/coll/archive"
    mk "$coll_root/-slug/dup.jsonl" 202601010000
    printf 'the live file\n' > "$coll_root/-slug/dup.jsonl"
    mkdir -p "$coll_arch/-slug"; printf 'already archived\n' > "$coll_arch/-slug/dup.jsonl"
    DENY_BUDGET_BYTES=1 KEEP_NEWEST=0
    sweep_transcripts "$coll_root" "$coll_arch" >/dev/null 2>&1
    eq "an already-archived copy is never clobbered" \
       "$(cat "$coll_arch/-slug/dup.jsonl")" "already archived"
    eq "...and the source stays in the tree, where the next sweep will see it again" \
       "$([ -f "$coll_root/-slug/dup.jsonl" ] && echo yes || echo no)" "yes"

    # A DESTINATION THAT CANNOT BE MADE: a FILE where the archive's sub-directory has to go. Chosen
    # over `chmod a-w` because root ignores a write bit and CI may run as root, so the chmod form is
    # a row that quietly stops asserting on exactly the machine nobody watches.
    blk_root="$work/blk/projects"; blk_arch="$work/blk/archive"
    mk "$blk_root/-slug/x.jsonl" 202601010000
    mkdir -p "$blk_arch"; : > "$blk_arch/-slug"
    blk_out="$(sweep_transcripts "$blk_root" "$blk_arch" 2>&1)"; blk_rc=$?
    eq "a destination that cannot be created is a failure, not a silent skip" "$blk_rc" "1"
    eq "...counted and reported" "$(grep -c 'could not be archived' <<<"$blk_out")" "1"
    eq "...and the transcript is still in the tree" \
       "$([ -f "$blk_root/-slug/x.jsonl" ] && echo yes || echo no)" "yes"
    DENY_BUDGET_BYTES="$DEFAULT_BUDGET" KEEP_NEWEST="$DEFAULT_KEEP"

    # THE CONTRACT verify.sh RESTS ON, watched in the state it exists for: the archive unusable, the
    # tree over budget, a plan that WOULD have fitted. A regular file where the archive's parent
    # directory must go, so `mkdir -p` fails for every planned file and not one moves -- ENOSPC and
    # a read-only volume are the same shape. Before the dry run was judged on the tree as it is, it
    # exited 0 here (`7200 -> 3960 deny bytes, budget 3960`) and verify.sh printed
    # `ok  the transcript deny list fits in one argv` over a container where every Bash call dies.
    # THE MARGIN IS DERIVED, NOT WRITTEN TWICE. The surplus over the floor and the fraction of the
    # projection the budget keeps are one relationship: the budget must be reachable by archiving
    # SOME of the surplus and not all of it, or `before - planned` lands on the budget and the old
    # verdict passes for a different reason. Written as two literals (`+ 28`, `* 55 / 100`) the
    # margin was one file, and raising KEEP_NEWEST to 48 left both of these rows green under the
    # reverted behaviour. SURPLUS files are candidates; the budget keeps KEEP_FRACTION of the whole,
    # so the plan needs roughly (1 - KEEP_FRACTION) of the projection gone and there must be more
    # candidates than that demands.
    blocked_root="$work/blocked/projects"
    blocked_surplus=$((DEFAULT_KEEP * 3))
    i=0
    while [ "$i" -lt "$((DEFAULT_KEEP + blocked_surplus))" ]; do
        mk "$blocked_root/-slug/t$(printf '%03d' "$i").jsonl" 202601010000
        i=$((i + 1))
    done
    : > "$work/blocked/not-a-dir"
    blocked_proj="$(transcript_records "$blocked_root" | transcript_projection)"
    # 80%: the plan must remove about a fifth of the projection, which is well inside the surplus
    # (three times the floor) and nowhere near exhausting it -- so `before - planned` sits under the
    # budget while `before` sits over it, which is the whole difference the rows below measure.
    DENY_BUDGET_BYTES=$((blocked_proj * 80 / 100)) KEEP_NEWEST="$DEFAULT_KEEP"
    blocked_real="$(sweep_transcripts "$blocked_root" "$work/blocked/not-a-dir/arch" 2>&1)"; blocked_rc=$?
    eq "a sweep whose archive cannot be created archives nothing and says so" "$blocked_rc" "1"
    eq "...leaving every transcript where it was" \
       "$(find "$blocked_root" -type f -name '*.jsonl' | grep -c . )" "$((DEFAULT_KEEP + blocked_surplus))"
    eq "...and nothing claims to have archived" \
       "$(grep -c 'could not create' <<<"$blocked_real")" "1"
    # AND THE DRY RUN, which is the code verify.sh actually reads.
    blocked_dry="$(sweep_transcripts "$blocked_root" "$work/blocked/not-a-dir/arch" --dry-run 2>&1)"
    blocked_dry_rc=$?
    eq "a dry run over the same tree refuses to call it healthy" "$blocked_dry_rc" "1"
    eq "...naming the projection that is really there, not the one its plan imagines" \
       "$(grep -c "$blocked_proj deny bytes are in this tree now" <<<"$blocked_dry")" "1"
    DENY_BUDGET_BYTES="$DEFAULT_BUDGET" KEEP_NEWEST="$DEFAULT_KEEP"

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
        local envs=() unset_args=(-u CLAUDE_CONFIG_DIR) seam
        # DERIVED FROM $INPUTS -- every JKB_ the script reads, not only the refusable ones --
        # and not retyped. The list named three of four, so an exported JKB_KEEP_SESSIONS
        # reached every row below, and nothing but this comment would have stopped the next
        # one repeating it.
        for seam in $INPUTS; do unset_args+=(-u "$seam"); done
        while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do envs+=("$1"); shift; done
        [ "$#" -eq 0 ] || shift
        env "${unset_args[@]}" ${envs[@]+"${envs[@]}"} bash "$self" "$@"
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
    # THE NEUTRALISATION ITSELF, watched. `prog` derives its `env -u` list from $SEAMS after that
    # list named three of four — and nothing saw the derivation: replacing the loop with `:` left
    # the whole suite green, while a developer with any seam exported got program rows asserting
    # exact messages against the wrong tree, and check.sh stopping at this gate. Exported here on
    # purpose, for one command.
    eq "an exported seam does not reach a program row" \
       "$(JKB_TRANSCRIPT_ROOT="$work/nowhere-at-all" prog HOME="$phome" -- 2>&1)" \
       "transcript sweep: no transcripts under $phome/.claude/projects"
    eq "--dry-run is accepted"  "$(rc_of prog HOME="$phome" -- --dry-run)" "0"
    eq "a bare run is accepted" "$(rc_of prog HOME="$phome" --)" "0"
    eq "an unknown argument is a usage error" "$(rc_of prog HOME="$phome" -- --wat)" "2"
    # `--self-test` guards on `$# -eq 1`, so a second argument must fall through to that usage
    # error rather than quietly running the suite with an argument nobody reads.
    eq "--self-test with a trailing argument is a usage error" \
       "$(rc_of prog HOME="$phome" -- --self-test extra)" "2"

    # ...AND AGAINST A TREE THAT HAS SOMETHING IN IT, which is what makes `"${1:-}"` in the dispatch
    # load-bearing. Every row above points at an EMPTY root, where sweep_transcripts returns at
    # "no transcripts" before it ever reads its third argument -- so the flag they were added to
    # cover was still exercised by nothing. Measured: dropping `"${1:-}"` from the dispatch left
    # --self-test green and check-config.sh green at 70/70, while `sweep-transcripts.sh --dry-run`
    # archived for real.
    #
    # KEEP_NEWEST + 2 files, derived rather than spelled, so the BUDGET is what chooses here and the
    # row follows if the floor ever moves.
    prog_root="$work/prog/projects"; prog_arch="$work/prog/archive"
    i=0
    while [ "$i" -lt "$((DEFAULT_KEEP + 2))" ]; do
        mk "$prog_root/-slug/$(printf '%04d' "$i").jsonl" "20260101$(printf '%02d' $((i / 60)))$(printf '%02d' $((i % 60)))"
        i=$((i + 1))
    done
    prog_tree="$(find "$prog_root" -type f | LC_ALL=C sort | tr '\n' ' ')"
    dry_prog="$(prog HOME="$phome" JKB_TRANSCRIPT_ROOT="$prog_root" \
        JKB_TRANSCRIPT_ARCHIVE="$prog_arch" JKB_DENY_BUDGET_BYTES=1 -- --dry-run 2>&1)"
    eq "--dry-run through the CLI names the two the floor leaves over" \
       "$(grep -c '^  would archive' <<<"$dry_prog")" "2"
    eq "...and moves nothing" \
       "$(find "$prog_root" -type f | LC_ALL=C sort | tr '\n' ' ')" "$prog_tree"
    eq "...and creates no archive directory" \
       "$([ -e "$prog_arch" ] && echo yes || echo no)" "no"
    # The same invocation WITHOUT the flag must move them, or the rows above are about a budget that
    # happened to choose nothing rather than about the flag.
    prog HOME="$phome" JKB_TRANSCRIPT_ROOT="$prog_root" \
        JKB_TRANSCRIPT_ARCHIVE="$prog_arch" JKB_DENY_BUDGET_BYTES=1 -- >/dev/null 2>&1
    eq "...while the same run without it archives them" \
       "$(find "$prog_arch" -type f | grep -c . )" "2"

    echo
    [ "$fails" -eq 0 ] || { printf '\033[31msweep-transcripts self-test FAILED (%d)\033[0m\n' "$fails"; exit 1; }
    printf '\033[32msweep-transcripts self-test passed\033[0m\n'
    exit 0
fi

case "${1:-}" in
    ""|--dry-run) sweep_transcripts "$TRANSCRIPT_ROOT" "$TRANSCRIPT_ARCHIVE" "${1:-}" ;;
    *) printf 'usage: %s [--dry-run|--self-test]\n' "$0" >&2; exit 2 ;;
esac
