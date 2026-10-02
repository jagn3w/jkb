#!/bin/bash -p
# PreToolUse hook: keep every tool out of any session's transcript, while leaving auto-memory alone.
#
#   .container/deny-transcripts.sh            # hook mode: tool-call JSON on stdin
#   .container/deny-transcripts.sh --self-test
#
# THE RECORD IS .container/README.md, "The transcript deny is a hook": why this is a hook and not a
# deny rule (a per-file `Read(...*.jsonl)` glob was half of MAX_ARG_STRLEN in the Bash sandbox's
# argv, measured; the collapsing `projects/**` swallows auto-memory), and what each review round
# found. The notes below are the ones someone changing THIS FILE needs beside the code: the
# invariants, and why each guard is shaped the way it is. The design is not restated here, so the
# two cannot disagree -- they did once, about multi-line strings.
#
# A glob cannot separate `<slug>/memory/` from `<slug>/<uuid>.jsonl`; they are siblings. A hook
# can, and costs nothing in argv because it is code rather than a path list: one process per TOOL
# CALL (the matcher is `.*`) in exchange for O(files) of argv.
#
# WHAT STILL COVERS BASH. The sandbox's blanket `denyRead` of `~` already hides this tree from
# Bash -- observable in one listing, where `~/.claude/todos` is invisible while `~/.claude/projects`
# was not, because naming a path in a deny rule is what EXPOSES it.
#
# THIS HOOK IS THE ONLY FILE-TOOL RULE FOR THE TREE, AND THAT IS DELIBERATE. The first cut kept
# `Read(~/.claude/projects)` and its .claude-state spelling beside it "as the belt to this brace",
# on the theory that naming a directory names only the directory. Measured in the rebuilt container
# (2026-09-30), Claude Code applies a directory rule to its whole subtree: this hook ALLOWED
# <slug>/memory/MEMORY.md and the permission rule behind it denied it anyway ("File is in a
# directory that is denied by your permission settings"). Any permissions rule broad enough to
# cover the transcripts covers memory too -- that is the property that made this a hook in the
# first place -- so there is no belt to add.
#
# FAILS CLOSED, unlike .claude/hooks/block-raw-sqlite.sh, and deliberately. That hook steers an
# agent away from a better tool, so an error there must not wedge Bash. This one is a
# confidentiality boundary, so EVERY way of not reaching a verdict is a refusal: a payload jq
# cannot parse, a missing jq, an unset variable, any crash. An EXIT trap turns anything that ends
# the script without an explicit allow into exit 2, which Claude Code treats as blocking. The first
# cut only closed the jq-parse case and said "fails closed" about all of it; with HOME unset,
# `set -u` aborted at rc=1, which Claude Code reads as a NON-blocking error and lets the call
# through.
#
# THE ONE OPEN EDGE, stated rather than hidden: a hook KILLED for exceeding its timeout is
# non-blocking in Claude Code, and nothing inside the script can change that. Per call, measured
# 2026-10-01 in jkb-dev as 50 sequential invocations under `date +%s%N`: 13ms for a Bash call (let
# through right after the parse) and 50ms for a judged Read, against the 10s budget
# managed-settings.json gives it. Re-measured after round 8 the same way: 11ms Bash, 29ms Read,
# 38ms for a Glob whose braces expand to the 64-way cap, 222ms for an MCP call carrying 2000 bare
# words, each tested on disk, and 270ms for the worst Glob found: 64 expansions of a pattern at the
# 4096-byte budget (2.8s before the expander skipped finished expansions). Round 12 judges each
# expansion's own literal prefix: a Glob with 64 short distinct prefixes, every one checked, took
# 1.05s. That made glob_base's quadratic prefix-building reachable, and 64 distinct ~4KB prefixes
# then took 30s (round 13). glob_base is linear now, and expansions x pattern bytes is capped. At
# 32768 a `~`-led 8-way 4KB pattern still took 2.9s, because a `~name` prefix is judged twice
# (round 14), so the cap is 12288: the slowest Glob under it, `~{a0..a7}/` and 745 segments, takes
# 0.82s. It refuses `{d0,...,d63}/**/*.rs` (17KB of work), the price of the margin. The MCP arm caps
# path segments x bases at 12000 (round 14: 100 leaves of 2000 segments reached 10s) AND check()
# calls at 32 (round 15: 600 calls passed the segment cap and ran 4.6s idle, 20s with 10 busy
# loops on 10 cores). The slowest call under both, five `~name/` strings from three bases (30
# calls), takes 0.26s idle and 2.14s with 10 busy loops, measured that way on 2026-10-02. It was ~7ms before five review rounds added guards; the number is
# re-measured rather than carried, because this hook now runs on EVERY tool call.
#
# PATHS ARE RESOLVED THE WAY THE TOOL WILL RESOLVE THEM, not the way this process would. A leading
# `~` is the user's home, and a relative path is relative to the SESSION'S cwd (the payload's
# `.cwd`), not to wherever the harness happened to start this script. Both were missed at first,
# and the `~` one was confirmed live (2026-10-01): a Read of `~/.claude/projects/<slug>/x.jsonl`
# went straight past this hook, which had resolved it under its own $PWD, and was stopped only by a
# permissions rule that a later commit removed.
#
# AND AN ANCESTOR OF THE TREE IS DENIED, not only paths inside it. Grep and Glob walk whatever they
# are rooted at, so `Grep path=~/.claude-state` -- or `path=$HOME`, or no path at all from a home
# cwd -- read transcripts while naming no transcript. The per-file `.jsonl` deny rules this hook
# replaced had also been doing that job, as an ignore glob ripgrep honoured, so removing them
# silently dropped it: the second time in this change that a rule turned out to have a job nobody
# had written down. A Glob whose PATTERN carries the location (`/home/.../projects/**`,
# `../../.claude/projects/**`) has its literal prefix checked the same way.
#
# AND THE PHYSICAL PATH IS JUDGED, NOT ONLY THE LEXICAL ONE. The file tools run unsandboxed and the
# kernel follows symlinks, so a path that does not SPELL the tree can still land in it: a symlink
# an agent makes from sandboxed Bash (`ln -s ~/.claude-state/projects ~/repos/jkb/x` needs no read
# access to the target), or /proc/self/root/home/... Found by review round 2, reproduced. Every
# path is now judged twice -- as written, and as `realpath -m` resolves it (symlinks in every
# existing component, a missing tail kept lexically, which is the kernel's view for a Write too)
# -- and refused if either says so. /proc/*/{root,cwd,fd,...} and /dev/fd are refused outright:
# `/proc/self` is the HOOK's process when this resolves it and Claude Code's when the tool does,
# so no resolution from in here can be trusted for them, and nothing legitimate reads through them.
#
# EVERY TOOL REACHES THIS HOOK: the matcher is `.*`. It was an allowlist of file tools, and built-ins
# it did not name -- Artifact reads a local file and uploads it -- skipped the hook (review round 3).
# Bash and the built-ins that carry text rather than locations (TodoWrite, AskUserQuestion, Agent,
# Task, ToolSearch, SendMessage) are let through first. The six file tools are judged by their
# path fields. Anything else -- MCP tools run unsandboxed, and jkb's own server has an ingest_path
# -- has every distinct, non-empty string that COULD be a path judged: one holding a `/`, or
# starting `~` or `.`. A relative one is judged against each distinct base an MCP server might
# resolve it from: the session cwd, CLAUDE_PROJECT_DIR, the home.
#
# BOUNDED, because a timed-out hook FAILS OPEN. At most 100 path-shaped candidates and 2000 bare
# words (each tested on disk; round 9 timed 20000 at 2s), then refused; a Glob pattern and a path field
# over PATH_MAX (4096) is refused before it is walked, and an over-long free-text string gets the
# linear lexical verdict alone. THE MEASUREMENT, kept here and nowhere else: 100 distinct relative
# strings to an MCP tool, `date +%s%N` around one invocation in jkb-dev on 2026-10-01 -- ~1.0s when
# the cwd is the project dir (bases deduplicated), 1.4s with three distinct bases. This container
# has been seen running 5x slow under a saturated VM, and 5x the worst case is still inside 10s.
#
# AND THE SANDBOX'S OWN BOUNDARY: when the Claude settings enable the sandbox, every path a tool is
# handed is also held to the allow lists the sandbox enforces on Bash (`boundary` below; design A,
# chosen after review round 15). .container/README.md, "The file tools are held to the sandbox's
# own boundary", has the decision and its cost.
#
# WHAT IS RULED OUT AND WHAT IS LEFT OPEN -- hard links, case folding and bind mounts measured as
# not vectors; the symlink-swap race between this check and the tool's open left open -- is recorded
# once, in .container/README.md ("The transcript deny is a hook"), and not restated here.
#
# A FIXED PATH, BEFORE ANYTHING ELSE RUNS, and an absolute shebang in privileged mode. This hook
# runs UNSANDBOXED on every tool call, and the image puts the agent-writable ~/.local/bin and
# ~/.cargo/bin first on PATH: sandboxed Bash could drop a `jq` there and have it run outside the
# sandbox, with the container credential readable, on the next call -- measured by review round 4
# with a fake jq that touched a marker. `#!/usr/bin/env bash` found bash itself through that PATH.
# `-p` makes bash ignore BASH_ENV and functions exported through the environment, which are the
# other two ways to put code in front of this script.
PATH=/usr/bin:/bin
export PATH
set -uo pipefail

# EVERY jq CALL GOES THROUGH HERE, with HOME pointed where no file can be. jq SOURCES $HOME/.jq into
# every program it runs, and the Write tool can create ~/.jq -- nothing denies it. Planted there,
# `def unique: [];` turned a deny into an allow, and `def error(m): .;` defeated the type guard so
# that `eval` RAN an array-valued field as a command outside the sandbox (review round 5, both
# reproduced). Round 4's PATH fix closed one door into this unsandboxed process; this is the same
# class through another. /dev/null is a character device, so $HOME/.jq is never a file. The other
# programs this script runs -- realpath, sed, cat, getent, id, cut -- load nothing from the home,
# and `bash -p` skips BASH_ENV.
jqh() { HOME=/dev/null jq "$@"; }

# Lexical, not `realpath`: the file may not exist yet (a Write), and a resolver that fails on a
# missing path would answer "cannot tell" for exactly the calls that create one. `..` is collapsed
# here because `<slug>/memory/../e1d7.jsonl` is a transcript wearing memory's prefix, and a check
# that compared the raw string would allow it.
# SPLIT WITH `read -a`, NEVER WITH AN UNQUOTED EXPANSION. `for seg in $p` with IFS=/ also does
# pathname expansion, so a `*` segment became the names of files in whatever directory this ran in:
# `/h/.claude/projects/*` normalised to `/h/.claude/projects/AGENTS.md/CLAUDE.md/...`. A hook that
# rewrites the path it is judging into an unrelated one can be steered past itself. Caught by the
# glob_base rows, which tripped on the same loop. `-d ''` so a newline in a path is a byte of it.
split_path() { # split_path <path> -> sets the array `parts`
    parts=()
    IFS=/ read -r -d '' -a parts < <(printf '%s' "$1") || true
}

normalise() { # normalise <absolute path> -> lexically resolved absolute path
    local out=() seg parts
    split_path "$1"
    for seg in ${parts[@]+"${parts[@]}"}; do
        case "$seg" in
            ''|.) ;;
            ..) [ "${#out[@]}" -gt 0 ] && unset 'out[${#out[@]}-1]' ;;
            *) out+=("$seg") ;;
        esac
    done
    if [ "${#out[@]}" -eq 0 ]; then printf '/\n'; else printf '/%s' "${out[@]}"; printf '\n'; fi
}

# `~` and `~/x` are the home; `~name/x` is over-approximated as the home too, because the safe
# error here is refusing a path that was harmless, never allowing one that was not. A SLASHLESS
# `~word` is a home only when it is shaped like a user name (`~vscode`, as expanduser reads it):
# anything else -- `~"how does sync work"`, jkb's own vector-search syntax, or `~2h of work` -- is
# text, and resolving it to the home refused ordinary calls as transcript reads (review round 6).
resolve() { # resolve <path> <home> <cwd> -> normalised absolute path
    local p="$1"
    case "$p" in
        "~")      p="$2" ;;
        "~/"*)    p="$2/${p#\~/}" ;;
        "~"*/*)   p="$2/${p#*/}" ;;
        "~"[A-Za-z0-9._-]*) case "${p#\~}" in *[!A-Za-z0-9._-]*) p="$3/$p" ;; *) p="$2" ;; esac ;;
        /*)       ;;
        *)        p="$3/$p" ;;
    esac
    normalise "$p"
}

# The same join as resolve, WITHOUT normalising: the magic-link walk below needs the `..` segments
# still in place, because the kernel applies them AFTER following a link, not before.
join_raw() { # join_raw <path> <home> <cwd> -> absolute, un-normalised
    case "$1" in
        "~")    printf '%s\n' "$2" ;;
        "~/"*)  printf '%s\n' "$2/${1#\~/}" ;;
        "~"*/*) printf '%s\n' "$2/${1#*/}" ;;
        "~"[A-Za-z0-9._-]*) case "${1#\~}" in *[!A-Za-z0-9._-]*) printf '%s\n' "$3/$1" ;; *) printf '%s\n' "$2" ;; esac ;;
        /*)     printf '%s\n' "$1" ;;
        *)      printf '%s\n' "$3/$1" ;;
    esac
}

# Does the path PASS THROUGH a procfs magic link at any point of its walk? Checked step by step,
# because normalising first lets `..` cancel the link it follows: `/proc/self/cwd/../../x`
# normalises to `/proc/x`, which looks harmless, while the kernel follows cwd and only then climbs.
through_magic() { # through_magic <absolute un-normalised path> -> rc 0 if it passes through one
    # LINEAR, with no subshell per segment. This built `$(printf '/%s' "${out[@]}")` for every
    # segment -- quadratic, and a fork each time -- so one 24KB path of `a/..` repeated took 15.3s
    # against a 10s budget, and a timed-out hook FAILS OPEN. Found by review round 3.
    local seg parts cur=""
    split_path "$1"
    for seg in ${parts[@]+"${parts[@]}"}; do
        case "$seg" in
            ''|.) continue ;;
            ..) cur="${cur%/*}"; continue ;;
        esac
        cur="$cur/$seg"
        case "$cur" in
            /proc/*/root|/proc/*/cwd|/proc/*/fd|/proc/*/fdinfo|/proc/*/map_files|/proc/*/exe|/dev/fd) return 0 ;;
        esac
    done
    return 1
}

# A file: URI AS THE PATH IT NAMES, for every caller -- check() and the over-PATH_MAX branch both,
# since round 7 found the long branch skipping a rewrite that lived in check() alone. The scheme is
# compared CASE-INSENSITIVELY: `FILE:///h/.claude/...` is the same URI under RFC 3986, and a
# lowercase-only match let it through. Prints the path, or nothing if the string is not a file URI;
# returns 1 for a file URI it will not judge (percent-escapes: a second parser of the same string
# is a second place to be wrong, and the agent can send the plain path).
uri_path() { # uri_path <string> -> path on stdout, rc 0; empty for not-a-URI; rc 1 to refuse
    local p="$1" lc
    lc="$(printf '%s' "${p:0:17}" | tr 'A-Z' 'a-z')"
    case "$lc" in
        file://localhost/*) p="/${p:17}" ;;
        file:///*)          p="/${p:8}" ;;
        file:/*)            p="/${p:6}" ;;
        *) return 0 ;;
    esac
    case "$p" in *%*) return 1 ;; esac
    printf '%s\n' "$p"
}

# The literal leading part of a glob pattern: every segment before the first that holds a glob
# character. `**/*.jsonl` -> "" (the search root alone decides), `/a/b/*/c` -> /a/b.
glob_base() { # glob_base <pattern> -> literal prefix, possibly empty
    # LINEAR: one substring of the pattern, up to the end of its last literal segment. It rebuilt the
    # prefix by appending each segment to a growing string, which is quadratic, and with every brace
    # expansion now judged that ran 64 x 4KB prefixes past the timeout (review round 13). The prefix
    # is also exactly the pattern's own text now, so stripping it from an expansion cannot miss.
    local pat="$1" seg pos=0 end=0 parts
    case "$pat" in /*) end=1 ;; esac
    split_path "$pat"
    for seg in ${parts[@]+"${parts[@]}"}; do
        case "$seg" in *[\*\?\[\{]*) break ;; esac
        pos=$((pos + ${#seg}))
        [ -n "$seg" ] && end=$pos
        pos=$((pos + 1))
    done
    printf '%s\n' "${pat:0:end}"
}

# WHAT A GLOB'S BRACES EXPAND TO, nested groups included, into the global array `brace_out`. Judging
# the raw alternatives let `{x,{/abs,y}}` and a `..` composed across a boundary (`.{.,}`) through
# (review round 8). In this shell, not a subshell: no fork per pattern. Returns 1 past 64 expansions
# or on unbalanced braces -- a pattern this cannot expand is a pattern it cannot judge.
brace_expand() { # brace_expand <pattern>
    local s i c depth open close pre body post cur a o="${1//[^\{]/}" x="${1//[^\}]/}"
    local -a todo=("$1") alts
    brace_out=()
    [ "${#o}" -eq "${#x}" ] || return 1
    while [ "${#todo[@]}" -gt 0 ]; do
        s="${todo[0]}"; todo=("${todo[@]:1}")
        # Walked from the first `{` only, and not at all when there is none: a finished expansion
        # walked end to end, 64 times, was most of a 2.8s worst case at the byte budget.
        case "$s" in *"{"*) ;; *) brace_out+=("$s"); continue ;; esac
        pre="${s%%\{*}"
        open=-1; close=-1; depth=0
        for ((i = ${#pre}; i < ${#s}; i++)); do
            c="${s:i:1}"
            if [ "$c" = "{" ]; then
                [ "$depth" -eq 0 ] && open=$i
                depth=$((depth + 1))
            elif [ "$c" = "}" ] && [ "$depth" -gt 0 ]; then
                depth=$((depth - 1))
                [ "$depth" -eq 0 ] && { close=$i; break; }
            fi
        done
        # An OPEN that never closes is refused, not kept literal: a later group would still expand
        # in a real engine, and keeping the string whole hid a `..` built across that group (round 9).
        if [ "$close" -lt 0 ]; then
            return 1
        else
            pre="${s:0:open}"; body="${s:open+1:close-open-1}"; post="${s:close+1}"
            alts=(); cur=""; depth=0
            for ((i = 0; i < ${#body}; i++)); do
                c="${body:i:1}"
                case "$c" in
                    "{") depth=$((depth + 1)); cur+="$c" ;;
                    "}") depth=$((depth - 1)); cur+="$c" ;;
                    ,) if [ "$depth" -eq 0 ]; then alts+=("$cur"); cur=""; else cur+="$c"; fi ;;
                    *) cur+="$c" ;;
                esac
            done
            alts+=("$cur")
            for a in "${alts[@]}"; do todo+=("$pre$a$post"); done
        fi
        [ $(( ${#todo[@]} + ${#brace_out[@]} )) -le 64 ] || return 1
    done
}

# THE DECISION, pure so --self-test can drive it with literals and no Claude Code.
#
# PREFIX TESTS ARE STRING SURGERY, NOT `case` PATTERNS. The ancestor test was first written
# `case "$root/" in "${p%/}"/*)`, and for p=/ -- where the quoted part is EMPTY -- it did not match
# `/h/.claude/projects/` on bash 5.2.21, while the literal pattern `/*` did. Reproduced in a fresh
# shell; the dot after a slash is part of it. Whatever bash's matcher is doing there, a
# confidentiality check must not depend on it, so "starts with" is `${x#"$prefix"} != $x`, which
# compares bytes and nothing else.
verdict() { # verdict <path> <roots> <home> <cwd> -> allow|deny
    local p roots="$2" root rel rest anc
    # PROCFS MAGIC LINKS: a path through them is resolved in whichever process opens it, so it is
    # refused before it is normalised -- see through_magic.
    if through_magic "$(join_raw "$1" "$3" "$4")"; then printf 'deny\n'; return; fi
    p="$(resolve "$1" "$3" "$4")"
    while IFS= read -r root; do
        [ -n "$root" ] || continue
        # INSIDE the tree: the root itself, or anything under root/.
        if [ "$p" = "$root" ] || [ "${p#"$root"/}" != "$p" ]; then
            # Auto-memory is the one child of a slug that is not a transcript. EXACTLY one slug
            # deep: `<slug>/memory/...`. A `case` `*` crosses `/`, so the first cut's
            # `"$root"/*/memory/*` matched a directory called memory at ANY depth --
            # `<slug>/<uuid>/subagents/memory/a.jsonl` was readable and writable.
            rel="${p#"$root"}"; rel="${rel#/}"
            case "$rel" in
                */*) rest="${rel#*/}"
                     case "$rest" in memory|memory/*) printf 'allow\n'; return ;; esac ;;
            esac
            printf 'deny\n'; return
        fi
        # An ANCESTOR of the tree: anything rooted here walks into it. For p=/ this is "/".
        anc="${p%/}/"
        if [ "${root#"$anc"}" != "$root" ]; then printf 'deny\n'; return; fi
    done <<<"$roots"
    printf 'allow\n'
}

if [ "${1:-}" = --self-test ]; then
    fails=0
    R="/h/.claude/projects
/h/.claude-state/projects"
    t() { # t <label> <path> <want> [cwd]
        local got; got="$(verdict "$2" "$R" /h "${4:-/h/repos/jkb}")"
        if [ "$got" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else printf '  \033[31mFAIL\033[0m %s\n         got %s, wanted %s\n' "$1" "$got" "$3"; fails=$((fails+1)); fi
    }
    echo "==> deny-transcripts self-test: the decision"
    t "a session transcript is denied"            /h/.claude/projects/-slug/e1d7.jsonl            deny
    t "a subagent transcript is denied"           /h/.claude/projects/-slug/e1d7/subagents/a.jsonl deny
    t "a workflow agent transcript is denied"     /h/.claude/projects/-slug/e1d7/subagents/workflows/wf_1/a.jsonl deny
    t "the state-volume spelling is denied too"   /h/.claude-state/projects/-slug/e1d7.jsonl      deny
    # The whole reason this is a hook and not a glob.
    t "auto-memory is ALLOWED"                    /h/.claude/projects/-slug/memory/MEMORY.md      allow
    t "a memory note is allowed"                  /h/.claude/projects/-slug/memory/foo.md         allow
    t "the memory directory itself is allowed"    /h/.claude/projects/-slug/memory                allow
    t "...in the state-volume spelling too"       /h/.claude-state/projects/-slug/memory/MEMORY.md allow
    # Memory is exactly one slug deep.
    t "a memory directory two levels down is NOT memory" \
      /h/.claude/projects/-slug/e1d7/subagents/memory/a.jsonl deny
    t "...nor one level down under a uuid"        /h/.claude/projects/-slug/e1d7/memory/x.jsonl   deny
    t "memoryX is not memory"                     /h/.claude/projects/-slug/memoryX/f.md          deny
    # Traversal: a transcript wearing memory's prefix.
    t "a transcript reached through memory/.. is denied" \
      /h/.claude/projects/-slug/memory/../e1d7.jsonl deny
    t "a memory/../../other/memory chain lands where it lands" \
      /h/.claude/projects/-slug/a/memory/../../b/memory/x.jsonl deny
    t "a transcript reached through // is denied" /h/.claude/projects//-slug//e1d7.jsonl          deny
    # A glob character in a path is a byte of the path, never expanded against this process's cwd.
    t "a * in a path is not expanded against the cwd" "/h/.claude/projects/*"                     deny
    t "...nor a * that would match files here"    "/h/.claude/projects/-slug/memory/../*"         deny
    # Resolved the way the TOOL resolves it.
    t "a ~ path is the home, not this process's cwd"  "~/.claude/projects/-slug/e1d7.jsonl"       deny
    t "~ memory is still allowed"                 "~/.claude/projects/-slug/memory/MEMORY.md"     allow
    t "a ~name path is over-approximated to the home" "~vscode/.claude/projects/-slug/e.jsonl"   deny
    t "a relative path resolves against the SESSION cwd" \
      ".claude/projects/-slug/e1d7.jsonl" deny /h
    t "...so from a repo cwd it is a repo path"   ".claude/projects/-slug/e1d7.jsonl"             allow /h/repos/jkb
    t "a relative climb out of the repo into the tree is denied" \
      "../../.claude-state/projects/-s/e.jsonl" deny /h/repos/jkb
    # Ancestors: a search rooted here walks into the tree.
    t "the state volume root is an ancestor, denied" /h/.claude-state                             deny
    t "~/.claude is an ancestor, denied"          /h/.claude                                      deny
    t "the home is an ancestor, denied"           /h                                              deny
    t "/ is an ancestor, denied"                  /                                               deny
    t "~ alone is the home, denied"               "~"                                             deny
    # Near-misses that must NOT be swallowed.
    t "a repo file is allowed"                    /h/repos/jkb/src/main.rs                        allow
    t "the repos directory is not an ancestor"    /h/repos                                        allow
    t "the memory STORE outside the tree is allowed" /h/.jkb/claude-memory/jkb/MEMORY.md          allow
    t "a sibling file is allowed"                 /h/.claude/settings.json                        allow
    t "a path merely containing the root name is allowed" /h/x/.claude/projects-backup/a.jsonl    allow
    t "a sibling whose name starts like the root is not an ancestor" /h/.claude-statement         allow
    # Procfs and /dev/fd magic links resolve in whichever process opens them.
    t "/proc/self/root into the tree is refused"  /proc/self/root/h/.claude-state/projects/-s/e.jsonl deny
    t "/proc/self/cwd is refused, whatever follows" /proc/self/cwd/../../.claude/projects/-s/e.jsonl deny
    t "another pid's root is refused too"         /proc/1/root/h/.claude/projects                 deny
    t "an open fd through /proc is refused"       /proc/self/fd/7                                 deny
    t "...and through /dev/fd"                    /dev/fd/7                                       deny
    t "an ordinary /proc file is allowed"         /proc/cpuinfo                                   allow

    echo "==> deny-transcripts self-test: the literal prefix of a Glob pattern"
    g() { local got; got="$(glob_base "$2")"
          if [ "$got" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
          else printf '  \033[31mFAIL\033[0m %s\n         got [%s], wanted [%s]\n' "$1" "$got" "$3"; fails=$((fails+1)); fi; }
    g "a wholly relative glob has no prefix"      '**/*.jsonl'                         ''
    g "an absolute glob keeps its literal part"   '/h/.claude/projects/**/*.jsonl'     '/h/.claude/projects'
    g "a relative climb is kept to be resolved"   '../../.claude/projects/*/x'         '../../.claude/projects'
    g "a ~ glob keeps the ~"                      '~/.claude-state/projects/*'         '~/.claude-state/projects'
    g "a bracket is a glob character"             '/h/a/[bc]/d'                        '/h/a'

    # THROUGH THE PROGRAM: the fail-closed contract is about how the script EXITS, which no call to
    # `verdict` can show. Run as Claude Code runs it: JSON on stdin, decision on stdout or rc 2.
    echo "==> deny-transcripts self-test: hook mode, as Claude Code runs it"
    self="$0"
    h() { # h <label> <want deny|allow> <stdin> [env...]
        local label="$1" want="$2" in="$3" out rc=0; shift 3
        # The session's own CLAUDE_PROJECT_DIR and CLAUDE_CONFIG_DIR are not inherited: they would
        # point the boundary at this machine's real settings. A row that is about them sets them.
        out="$(printf '%s' "$in" | env -u CLAUDE_PROJECT_DIR -u CLAUDE_CONFIG_DIR "$@" "$BASH" "$self" 2>/dev/null)" || rc=$?
        local got=allow
        case "$out" in *'"permissionDecision":"deny"'*) got=deny ;; esac
        [ "$rc" -eq 2 ] && got=deny
        [ "$rc" -ne 0 ] && [ "$rc" -ne 2 ] && got="rc=$rc (non-blocking: the call would go through)"
        if [ "$got" = "$want" ]; then printf '  \033[32mok\033[0m   %s\n' "$label"
        else printf '  \033[31mFAIL\033[0m %s\n         got %s, wanted %s\n' "$label" "$got" "$want"; fails=$((fails+1)); fi
    }
    # The hook needs GNU `realpath -m`; macOS's BSD realpath has none. Where it is missing, every
    # program-level row would get the refusal and read green or red for the wrong reason, so they
    # are skipped and say so. DEFINED AFTER h(), or the real h() replaced this stub and every row ran
    # (review round 5). The hook only ever runs in the container, which has GNU realpath.
    # EVERY hook-mode row, not only the h() calls: the hostile-PATH, BASH_ENV and timing rows run the
    # hook too, and with h() stubbed they failed on the refusal or passed on nothing, so check.sh went
    # red on a Mac (review round 11). The probe uses the hook's OWN pinned PATH, which is what it runs
    # with; a GNU realpath elsewhere on the caller's PATH proves nothing about it.
    # ...and jq IN THAT PATH: the hook runs `jq` from /usr/bin:/bin, so a jq only in ~/.local/bin,
    # nix or Linuxbrew passed check.sh's gate and then failed every allow row (review round 12).
    # Decided here, in the callee, so every caller of the self-test gets it.
    if ! PATH=/usr/bin:/bin realpath -m / >/dev/null 2>&1 || ! PATH=/usr/bin:/bin type -P jq >/dev/null 2>&1; then
        printf '  \033[33mskip\033[0m every hook-mode row: no GNU realpath -m, or no jq, in /usr/bin:/bin here (the container has both)\n'
        if [ "$fails" -eq 0 ]; then printf '\033[32mdeny-transcripts self-test passed (pure rows only)\033[0m\n'; exit 0; fi
        printf '\033[31mdeny-transcripts self-test: %s failed\033[0m\n' "$fails"; exit 1
    fi
    h "a Read of a transcript is denied" deny \
      '{"tool_name":"Read","cwd":"/h/repos/jkb","tool_input":{"file_path":"/h/.claude/projects/-s/e.jsonl"}}' HOME=/h
    h "a Read of memory is allowed" allow \
      '{"tool_name":"Read","cwd":"/h/repos/jkb","tool_input":{"file_path":"/h/.claude/projects/-s/memory/MEMORY.md"}}' HOME=/h
    h "a Read with a ~ path is denied" deny \
      '{"tool_name":"Read","cwd":"/h/repos/jkb","tool_input":{"file_path":"~/.claude/projects/-s/e.jsonl"}}' HOME=/h
    h "a Grep rooted at the state volume is denied" deny \
      '{"tool_name":"Grep","cwd":"/h/repos/jkb","tool_input":{"path":"/h/.claude-state","pattern":"x"}}' HOME=/h
    h "a Grep with NO path from a home cwd is denied" deny \
      '{"tool_name":"Grep","cwd":"/h","tool_input":{"pattern":"x"}}' HOME=/h
    h "...and from a repo cwd is allowed" allow \
      '{"tool_name":"Grep","cwd":"/h/repos/jkb","tool_input":{"pattern":"x"}}' HOME=/h
    h "a Glob whose pattern names the tree is denied" deny \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"/h/.claude/projects/**/*.jsonl"}}' HOME=/h
    h "a Glob climbing into the tree from its path is denied" deny \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"path":"/h/repos/jkb","pattern":"../../.claude-state/projects/*"}}' HOME=/h
    h "an ordinary Glob in a repo is allowed" allow \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"**/*.rs"}}' HOME=/h
    h "a call naming no path at all, to a non-search tool, is allowed" allow \
      '{"tool_name":"Read","cwd":"/h/repos/jkb","tool_input":{}}' HOME=/h
    # SYMLINKS, with real files: the kernel follows a link the lexical check cannot see. A temp HOME
    # holds the tree, and an agent-made link in a repo points into it.
    # PHYSICAL, so a temp dir that sits under a symlink (macOS's /var/folders) is not a false red.
    # And NEVER derived from `cd` alone: `$(cd "$(mktemp -d)" && pwd -P)` prints the CURRENT directory
    # when mktemp fails (`cd ""` stays put), and the `rm -rf` below would then delete the checkout
    # this ran from. Claude Code's own removal guard refused exactly that pattern. So mktemp must
    # succeed and name a directory, and the cleanup refuses anything that is not that directory.
    sh_tmp="$(mktemp -d)" && [ -n "$sh_tmp" ] && [ -d "$sh_tmp" ] \
        || { printf '  \033[31mFAIL\033[0m mktemp -d failed; the symlink rows cannot run\n'; exit 1; }
    sh="$(cd -- "$sh_tmp" && pwd -P)" && [ -n "$sh" ] && [ "$sh" != / ] && [ "$sh" != "$PWD" ] \
        || { printf '  \033[31mFAIL\033[0m could not resolve the temp dir\n'; exit 1; }
    mkdir -p "$sh/.claude/projects/-s/memory" "$sh/.claude-state/projects/-s" "$sh/repos/r" "$sh/elsewhere"
    : >"$sh/.claude/projects/-s/e.jsonl"; : >"$sh/.claude/projects/-s/memory/MEMORY.md"
    ln -s "$sh/.claude-state/projects" "$sh/repos/r/x"
    ln -s "$sh/.claude/projects/-s/memory" "$sh/repos/r/mem"
    ln -s "$sh/elsewhere" "$sh/repos/r/ok"
    h "a Read through a symlink into the tree is denied" deny \
      '{"tool_name":"Read","cwd":"'"$sh"'/repos/r","tool_input":{"file_path":"'"$sh"'/repos/r/x/-s/e.jsonl"}}' HOME="$sh"
    h "a Grep rooted at a symlink into the tree is denied" deny \
      '{"tool_name":"Grep","cwd":"'"$sh"'/repos/r","tool_input":{"path":"x","pattern":"p"}}' HOME="$sh"
    h "a Write through a symlink, to a file not yet there, is denied" deny \
      '{"tool_name":"Write","cwd":"'"$sh"'/repos/r","tool_input":{"file_path":"'"$sh"'/repos/r/x/-s/new.jsonl"}}' HOME="$sh"
    h "a symlink to auto-memory still reads as memory" allow \
      '{"tool_name":"Read","cwd":"'"$sh"'/repos/r","tool_input":{"file_path":"'"$sh"'/repos/r/mem/MEMORY.md"}}' HOME="$sh"
    # A symlink FOLLOWED BY `..`: the kernel follows the link, then climbs. Review round 3.
    ln -s "$sh/.claude-state/projects/-s" "$sh/repos/r/l2"
    h "a symlink followed by .. lands where the kernel lands" deny \
      '{"tool_name":"Grep","cwd":"'"$sh"'/repos/r","tool_input":{"path":"l2/..","pattern":"p"}}' HOME="$sh"
    h "...for a Read through it too" deny \
      '{"tool_name":"Read","cwd":"'"$sh"'/repos/r","tool_input":{"file_path":"'"$sh"'/repos/r/l2/../-s/e.jsonl"}}' HOME="$sh"
    # A HOME that passes through a symlink: the roots must be resolved as well.
    ln -s "$sh" "$sh.link"
    h "with a symlinked HOME, a direct read of the real tree is denied" deny \
      '{"tool_name":"Read","cwd":"/tmp","tool_input":{"file_path":"'"$sh"'/.claude/projects/-s/e.jsonl"}}' HOME="$sh.link"
    # A symlinked BASE with a climbing Glob PATTERN: the kernel follows l, then climbs.
    mkdir -p "$sh/.claude/plugins/cache"
    ln -s "$sh/.claude/plugins/cache" "$sh/repos/r/l"
    h "a Glob from base l/.. with a climbing pattern lands where the kernel lands" deny \
      '{"tool_name":"Glob","cwd":"'"$sh"'/repos/r","tool_input":{"path":"l/..","pattern":"../projects/-s/*.jsonl"}}' HOME="$sh"
    # A HOSTILE ~/.jq: jq would source it into every program, so the hook must not let it.
    printf '%s\n' 'def unique: [];' 'def error(m): .;' > "$sh/.jq"
    h "with a planted ~/.jq, an Artifact of a transcript is still denied" deny \
      '{"tool_name":"Artifact","cwd":"'"$sh"'/repos/r","tool_input":{"files":{"a":"'"$sh"'/.claude/projects/-s/e.jsonl"}}}' HOME="$sh"
    h "...and an array-valued field still refused" deny \
      '{"tool_name":"mcp__x__y","cwd":"/tmp","tool_input":{"file_path":["/x","/bin/sh","-c","touch '"$sh"'/PWNED"]}}' HOME="$sh"
    if [ -e "$sh/PWNED" ]; then printf '  \033[31mFAIL\033[0m a planted ~/.jq let an array field EXECUTE\n'; fails=$((fails+1))
    else printf '  \033[32mok\033[0m   ...and nothing executed\n'; fi
    rm -f -- "$sh/.jq"
    h "a symlink that points elsewhere is allowed" allow \
      '{"tool_name":"Read","cwd":"'"$sh"'/repos/r","tool_input":{"file_path":"'"$sh"'/repos/r/ok/f"}}' HOME="$sh"
    # REVIEW ROUND 8. A BARE LEAF that names a link in the cwd: an MCP server opens it relative to
    # its own cwd, so `t` -- or `~t`, which is no account -- reached the tree with nothing judged.
    ln -s "$sh/.claude-state/projects/-s" "$sh/repos/r/t"
    ln -s "$sh/.claude-state/projects/-s" "$sh/repos/r/~t"
    h "a bare word naming a symlink into the tree is denied" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$sh"'/repos/r","tool_input":{"source":"t"}}' HOME="$sh"
    h "...and a ~word that is no account, naming one, is denied" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$sh"'/repos/r","tool_input":{"source":"~t"}}' HOME="$sh"
    h "a bare word naming a symlink elsewhere is allowed" allow \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$sh"'/repos/r","tool_input":{"source":"ok"}}' HOME="$sh"
    h "a bare word naming nothing on disk is allowed" allow \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$sh"'/repos/r","tool_input":{"source":"nothing-here"}}' HOME="$sh"
    # REVIEW ROUND 13. A cwd link named after an EXISTING account: judged only at that account's
    # home (/bin for sync), it reached the tree. Both readings, from every base.
    ln -s "$sh/.claude-state/projects/-s" "$sh/repos/r/~sync"
    # REVIEW ROUND 14. A Glob with path= and a ~name/ pattern: the literal ./~name reading must be
    # taken from the Glob's path, not the session cwd.
    mkdir -p "$sh/repos/b"; ln -s "$sh/.claude-state/projects" "$sh/repos/b/~x"
    h "a Glob ~x/ pattern under path= reaches the tree through a link there, and is denied" deny \
      '{"tool_name":"Glob","cwd":"'"$sh"'/repos/r","tool_input":{"path":"'"$sh"'/repos/b","pattern":"~x/-s/*.jsonl"}}' HOME="$sh"
    h "a cwd link named ~sync, an existing account, is denied" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$sh"'/repos/r","tool_input":{"path":"~sync"}}' HOME="$sh"
    # ...and a NEWLINE in a path is refused: `$( )` drops a trailing one, so a link named "x\n" was
    # judged as "x" and the tool opened the link.
    ln -s "$sh/.claude-state/projects/-s" "$sh/repos/r/nl
"
    h "a Read whose path ends in a newline is refused" deny \
      '{"tool_name":"Read","cwd":"'"$sh"'/repos/r","tool_input":{"file_path":"nl\n/e.jsonl"}}' HOME="$sh"
    h "a Grep rooted at a link whose name ends in a newline is refused" deny \
      '{"tool_name":"Grep","cwd":"'"$sh"'/repos/r","tool_input":{"path":"nl\n","pattern":"p"}}' HOME="$sh"
    h "an MCP string naming that link is denied" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$sh"'/repos/r","tool_input":{"path":"./nl\n"}}' HOME="$sh"
    h "a Glob with a newline is refused" deny \
      '{"tool_name":"Glob","cwd":"'"$sh"'/repos/r","tool_input":{"pattern":"x\n/../../.claude/projects/*"}}' HOME="$sh"
    # REVIEW ROUND 9. `~t/...` read only as a home let the cwd's `~t` link through.
    : >"$sh/.claude-state/projects/-s/e.jsonl"
    h "a ~name/ path through a cwd link of that name is denied, to an MCP tool" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$sh"'/repos/r","tool_input":{"source":"~t/e.jsonl"}}' HOME="$sh"
    h "...and to Read" deny \
      '{"tool_name":"Read","cwd":"'"$sh"'/repos/r","tool_input":{"file_path":"~t/e.jsonl"}}' HOME="$sh"
    # DESIGN A: THE FILE TOOLS HELD TO THE SANDBOX'S OWN BOUNDARY. A scratch home whose user settings
    # enable the sandbox with the posture's shape of lists.
    # OUTSIDE THE TEMP ROOTS: /tmp and $TMPDIR are writable to the sandbox, so a scratch home there
    # passes every write test whatever the lists say. ~/.cache is writable here and on CI.
    mkdir -p "$HOME/.cache" 2>/dev/null
    bh="$(mktemp -d "$HOME/.cache/jkb-boundary.XXXXXX" 2>/dev/null)" && [ -d "$bh" ] \
        || { printf '  \033[31mFAIL\033[0m could not make a scratch home under ~/.cache for the boundary rows\n'; fails=$((fails+1)); bh="$sh/bhome"; }
    mkdir -p "$bh/.claude" "$bh/repos/w" "$bh/.ssh" "$bh/.jkb/claude-memory/w" "$bh/.claude/projects/-w"
    : >"$bh/.ssh/id"; : >"$bh/.jkb/claude-memory/w/MEMORY.md"
    ln -s "$bh/.jkb/claude-memory/w" "$bh/.claude/projects/-w/memory"
    ln -s "$bh/.ssh" "$bh/repos/w/keys"
    printf '%s\n' '{"sandbox":{"enabled":true,"filesystem":{"denyRead":["~"],"allowRead":["~/.claude/settings.json"],"allowWrite":["~/repos","~/.jkb"]}}}' > "$bh/.claude/settings.json"
    h "boundary: a Write inside allowWrite is allowed" allow \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/repos/w/x.rs","content":""}}' HOME="$bh"
    h "boundary: a Write outside allowWrite is denied" deny \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.bashrc","content":""}}' HOME="$bh"
    h "boundary: an Edit of a system file is denied" deny \
      '{"tool_name":"Edit","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"/etc/hosts","old_string":"a","new_string":"b"}}' HOME="$bh"
    h "boundary: a Read under denyRead is denied" deny \
      '{"tool_name":"Read","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.ssh/id"}}' HOME="$bh"
    h "boundary: ...and through a link in the workspace" deny \
      '{"tool_name":"Read","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/repos/w/keys/id"}}' HOME="$bh"
    h "boundary: a Read outside every deny root is allowed" allow \
      '{"tool_name":"Read","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"/etc/hosts"}}' HOME="$bh"
    h "boundary: a Read of allowRead inside denyRead is allowed" allow \
      '{"tool_name":"Read","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.claude/settings.json"}}' HOME="$bh"
    h "boundary: auto-memory through its link into ~/.jkb is readable" allow \
      '{"tool_name":"Read","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.claude/projects/-w/memory/MEMORY.md"}}' HOME="$bh"
    mkdir -p "$bh/.claude/projects/-u/memory"
    h "boundary: an UNLINKED auto-memory directory is writable too" allow \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.claude/projects/-u/memory/n.md","content":""}}' HOME="$bh"
    h "boundary: ...while the tree around it is not" deny \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.claude/projects/-u/notes.md","content":""}}' HOME="$bh"
    h "boundary: ...and writable" allow \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.claude/projects/-w/memory/new.md","content":""}}' HOME="$bh"
    h "boundary: an MCP server handed a denied path is refused" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$bh"'/repos/w","tool_input":{"path":"'"$bh"'/.ssh/id"}}' HOME="$bh"
    h "boundary: a Grep rooted at the home is refused" deny \
      '{"tool_name":"Grep","cwd":"'"$bh"'/repos/w","tool_input":{"path":"'"$bh"'","pattern":"p"}}' HOME="$bh"
    h "boundary: a Write to the session's TMPDIR is allowed" allow \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$sh"'/tmpdir/x","content":""}}' HOME="$bh" TMPDIR="$sh/tmpdir"
    h "boundary: a plan file under ~/.claude/plans is writable" allow \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.claude/plans/p.md","content":""}}' HOME="$bh"
    printf '%s\n' '{"sandbox":{"enabled":false,"filesystem":{"denyRead":["~"],"allowWrite":["~/repos"]}}}' > "$bh/.claude/settings.json"
    h "boundary: with the sandbox disabled there is no boundary to mirror" allow \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.bashrc","content":""}}' HOME="$bh"
    printf '%s\n' 'not json {' > "$bh/.claude/settings.json"
    h "boundary: an unparseable settings layer contributes nothing, as Claude Code skips it" allow \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.bashrc","content":""}}' HOME="$bh"
    case "$bh" in */jkb-boundary.*) rm -rf -- "$bh" ;; esac
    h "a HOME with a trailing slash still finds its tree" deny \
      '{"tool_name":"Read","cwd":"/tmp","tool_input":{"file_path":"'"$sh"'/.claude/projects/-s/e.jsonl"}}' HOME="$sh/"
    # A field of the wrong TYPE is refused, never interpolated: `eval` of @sh output RUNS an array.
    h "an array-valued file_path is refused and runs nothing" deny \
      '{"tool_name":"Read","cwd":"/tmp","tool_input":{"file_path":["/x","/bin/sh","-c","touch '"$sh"'/INJECTED"]}}' HOME="$sh"
    if [ -e "$sh/INJECTED" ]; then printf '  \033[31mFAIL\033[0m an array-valued field EXECUTED a command\n'; fails=$((fails+1))
    else printf '  \033[32mok\033[0m   ...and nothing was executed\n'; fi
    rm -f -- "$sh.link"
    case "$sh" in */tmp.*) rm -rf -- "$sh" ;; *) printf '  \033[33mnote\033[0m left %s in place: not a mktemp path\n' "$sh" ;; esac

    # ANY TOOL THAT CAN NAME A PATH. MCP tools run unsandboxed; for a tool whose fields this does
    # not know, every string in its input is judged.
    h "an MCP tool given a transcript path is denied" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"/h/repos/jkb","tool_input":{"path":"/h/.claude/projects/-s/e.jsonl"}}' HOME=/h
    h "...however deep in its input the path sits" deny \
      '{"tool_name":"mcp__x__y","cwd":"/h/repos/jkb","tool_input":{"opts":{"files":["/h/repos/a","~/.claude-state/projects/-s/e.jsonl"]}}}' HOME=/h
    h "...or as an ancestor that a server would walk" deny \
      '{"tool_name":"mcp__x__index","cwd":"/h/repos/jkb","tool_input":{"root":"/h"}}' HOME=/h
    h "an MCP tool with ordinary arguments is allowed" allow \
      '{"tool_name":"mcp__jkb__search","cwd":"/h/repos/jkb","tool_input":{"query":"hello world","limit":5}}' HOME=/h
    h "MultiEdit into the tree is denied" deny \
      '{"tool_name":"MultiEdit","cwd":"/h/repos/jkb","tool_input":{"file_path":"/h/.claude/projects/-s/e.jsonl","edits":[]}}' HOME=/h
    h "a Write whose CONTENT mentions the tree is allowed -- content is not a path" allow \
      '{"tool_name":"Write","cwd":"/h/repos/jkb","tool_input":{"file_path":"/h/repos/jkb/notes.md","content":"/h/.claude/projects/-s/e.jsonl"}}' HOME=/h
    # Built with jq, not python3: without python3 the payload was empty, the empty-payload refusal
    # answered, and the row passed without ever reaching the cap. Review round 3.
    big="$(jqh -cn '{tool_name:"mcp__x__y", cwd:"/h/repos/jkb", tool_input:{a:[range(101) | "/h/repos/jkb/f\(.)"]}}')"
    h "more path-like strings than the cap is a refusal, never a race with the timeout" deny "$big" HOME=/h
    many="$(jqh -cn '{tool_name:"TodoWrite", cwd:"/h/repos/jkb", tool_input:{todos:[range(150) | {content:"do thing \(.)", status:"pending"}]}}')"
    h "a TodoWrite with a long list -- no path-like strings -- is allowed" allow "$many" HOME=/h
    # EVERY TOOL reaches the hook now; one it does not know is judged by its strings.
    h "an Artifact publish of a transcript is denied" deny \
      '{"tool_name":"Artifact","cwd":"/h/repos/jkb","tool_input":{"action":"publish","file_path":"~/.claude/projects/-s/e.jsonl"}}' HOME=/h
    h "...and through its files map" deny \
      '{"tool_name":"Artifact","cwd":"/h/repos/jkb","tool_input":{"files":{"a.json":"/h/.claude-state/projects/-s/e.jsonl"}}}' HOME=/h
    h "Bash is let through -- the kernel sandbox confines it" allow \
      '{"tool_name":"Bash","cwd":"/h","tool_input":{"command":"cat /h/.claude/projects/-s/e.jsonl"}}' HOME=/h
    # An MCP server resolves relative paths against its OWN cwd, the project root.
    h "an MCP relative path is judged against the project dir too" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"/h/repos/jkb/a/b","tool_input":{"source":"../../.claude-state/projects/-s/e.jsonl"}}' HOME=/h CLAUDE_PROJECT_DIR=/h/repos/jkb
    # CLAUDE_CONFIG_DIR moves the transcript tree, and the hook follows it.
    h "with CLAUDE_CONFIG_DIR set, its projects tree is denied" deny \
      '{"tool_name":"Read","cwd":"/h/repos/jkb","tool_input":{"file_path":"/cfg/projects/-s/e.jsonl"}}' HOME=/h CLAUDE_CONFIG_DIR=/cfg
    # A BYTE BUDGET: an `a/..` chain built to be slow is refused before any walk, and fast.
    long="$(jqh -cn '{tool_name:"Read", cwd:"/h", tool_input:{file_path:("/" + ([range(6000)|"a/.."]|join("/")) + "/h/.claude/projects/-s/e.jsonl")}}')"
    # Timed only where `date +%N` gives nanoseconds: macOS prints a literal N, and the arithmetic
    # would fail rather than measure.
    t0=$(date +%s%N)
    h "a 24KB a/.. chain is refused" deny "$long" HOME=/h
    case "$t0" in *N) t1=0 ;; *) t1=$(( ($(date +%s%N) - t0) / 1000000 )) ;; esac
    if [ "$t1" -lt 2000 ]; then printf '  \033[32mok\033[0m   ...in %sms, nowhere near the 10s timeout\n' "$t1"
    else printf '  \033[31mFAIL\033[0m ...but it took %sms, near enough the timeout to fail open\n' "$t1"; fails=$((fails+1)); fi

    # REVIEW ROUND 4. A cwd or project dir with a SPACE must still be judged whole.
    h "a relative climb from a cwd with a space is denied" deny \
      '{"tool_name":"Artifact","cwd":"/h/repos/a b/c","tool_input":{"files":{"x":"../../../.claude/projects/-s/e.jsonl"}}}' HOME=/h
    h "an MCP relative path from a project dir with a space is denied" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"/tmp","tool_input":{"source":"../../../.claude-state/projects/-s/e.jsonl"}}' HOME=/h "CLAUDE_PROJECT_DIR=/h/repos/a b/c"
    # Pathless built-ins are not refused for carrying text -- from a HOME cwd, with an empty field.
    todos="$(jqh -cn '{tool_name:"TodoWrite", cwd:"/h", tool_input:{todos:[range(40) | {content:"fix crates/a\(.).rs", status:"pending", activeForm:""}]}}')"
    h "40 todos from a home cwd, one with an empty activeForm, are allowed" allow "$todos" HOME=/h
    agent="$(jqh -cn '{tool_name:"Agent", cwd:"/h/repos/jkb", tool_input:{prompt:([range(130) | "Review the change in crates/jkb-core."] | join(" "))}}')"
    h "a 4.6KB Agent prompt with slashes is allowed" allow "$agent" HOME=/h
    # Over PATH_MAX in an unknown tool's input: prose passes, a chain that collapses into the tree
    # does not -- the lexical verdict does the collapse a server would.
    prose="$(jqh -cn '{tool_name:"mcp__x__note", cwd:"/h/repos/jkb", tool_input:{text:([range(200) | "see docs/a b.md here"] | join(" "))}}')"
    h "a 5KB prose string with slashes in an MCP call is allowed" allow "$prose" HOME=/h
    chain="$(jqh -cn '{tool_name:"mcp__x__read", cwd:"/h/repos/jkb", tool_input:{p:("/" + ([range(1500) | "a b/.."] | join("/")) + "/h/.claude/projects/-s/e.jsonl")}}')"
    h "...while a 10KB chain with spaces that collapses into the tree is denied" deny "$chain" HOME=/h
    # REVIEW ROUND 6. Text that starts with `~` is not a home; a file:// URI is its path.
    h "jkb's own vector-search syntax is not a path" allow \
      '{"tool_name":"mcp__jkb__search","cwd":"/h/repos/jkb","tool_input":{"query":"~\"how does sync work\" kind:task"}}' HOME=/h
    h "a ~2h estimate is not a path" allow \
      '{"tool_name":"TaskCreate","cwd":"/h/repos/jkb","tool_input":{"description":"~2h of work"}}' HOME=/h
    h "a file:// URI to a transcript is denied" deny \
      '{"tool_name":"mcp__x__open","cwd":"/h/repos/jkb","tool_input":{"uri":"file:///h/.claude/projects/-s/e.jsonl"}}' HOME=/h
    h "a percent-escaped file URI is refused, not decoded" deny \
      '{"tool_name":"mcp__x__open","cwd":"/h/repos/jkb","tool_input":{"uri":"file:///h/%2eclaude/projects/-s/e.jsonl"}}' HOME=/h
    h "a Glob climbing after a wildcard is denied" deny \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"*/../../../.claude/projects/*/*.jsonl"}}' HOME=/h
    h "a Glob hiding an absolute path in braces is denied" deny \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"{/h/.claude/projects/**/*.jsonl,**/*.rs}"}}' HOME=/h
    # A REDIRECTED HOME adds roots, never removes them: the account's real tree stays guarded.
    acct2="$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6)"
    if [ -n "$acct2" ] && [ "$acct2" != /h ]; then
        h "with HOME redirected, the account home's real tree is still denied" deny \
          '{"tool_name":"Read","cwd":"/tmp","tool_input":{"file_path":"'"$acct2"'/.claude-state/projects/-s/e.jsonl"}}' HOME=/h
    fi
    # REVIEW ROUND 7.
    h "an UPPER-CASE FILE:// URI to a transcript is denied" deny \
      '{"tool_name":"mcp__x__open","cwd":"/h/repos/jkb","tool_input":{"uri":"FILE:///h/.claude/projects/-s/e.jsonl"}}' HOME=/h
    lfile="$(jqh -cn '{tool_name:"mcp__x__read", cwd:"/h/repos/jkb", tool_input:{p:("file:///" + ([range(1500) | "a/.."] | join("/")) + "/h/.claude/projects/-s/e.jsonl")}}')"
    h "a long file:// chain that collapses into the tree is denied" deny "$lfile" HOME=/h
    h "a second brace group hiding an absolute path is denied" deny \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"{,}{/h/.claude/projects/**/*.jsonl,x}"}}' HOME=/h
    h "jkb's one-word vector search ~retry is not a home" allow \
      '{"tool_name":"mcp__jkb__query","cwd":"/h/repos/jkb","tool_input":{"dsl":"~retry"}}' HOME=/h
    h "a ~2h with no space is not a home either" allow \
      '{"tool_name":"mcp__x__note","cwd":"/h/repos/jkb","tool_input":{"text":"~2h"}}' HOME=/h
    h "a Glob whose .. is in the literal prefix is allowed" allow \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb/crates","tool_input":{"pattern":"../docs/*.md"}}' HOME=/h
    h "a brace of relative multi-segment paths is allowed" allow \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"{crates/jkb-core,crates/jkb-cli}/**/*.rs"}}' HOME=/h
    h "free text that starts with file: is text, not a refusal" allow \
      '{"tool_name":"mcp__x__note","cwd":"/h/repos/jkb","tool_input":{"text":"file: see crates/a.rs"}}' HOME=/h
    h "an ordinary brace pattern is allowed" allow \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"**/*.{rs,toml}"}}' HOME=/h
    # A bare ~name is a home, as a server applying expanduser would read it.
    # A MULTI-LINE string a lexically-normalising server would collapse into the tree.
    h "a multi-line string that collapses into the tree is denied" deny \
      '{"tool_name":"mcp__x__read","cwd":"/h/repos/jkb","tool_input":{"p":"/h/.claude\n/../.claude/projects/-s/e.jsonl"}}' HOME=/h
    h "a bare ~name root given to an MCP tool is the home, an ancestor" deny \
      '{"tool_name":"mcp__x__index","cwd":"/h/repos/jkb","tool_input":{"root":"~vscode"}}' HOME=/h
    # REVIEW ROUND 8. What a brace group EXPANDS to, not its raw alternatives: a nested group and a
    # `..` composed across a group boundary both passed the per-alternative test.
    h "a nested brace hiding an absolute path is denied" deny \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"{x,{/h/.claude/projects/**/*.jsonl,y}}"}}' HOME=/h
    h "...nested first, absolute second" deny \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"{{a,b},/h/.claude/projects/*}"}}' HOME=/h
    h "a .. composed from .{.,} is denied" deny \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":".{.,}/.{.,}/.claude-state/projects/*/*.jsonl"}}' HOME=/h
    h "an ordinary empty alternative is allowed" allow \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"src/*.rs{,.orig}"}}' HOME=/h
    h "an ordinary nested brace is allowed" allow \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"{src,tests/{unit,e2e}}/**/*.rs"}}' HOME=/h
    blow="$(jqh -cn '{tool_name:"Glob", cwd:"/h/repos/jkb", tool_input:{pattern:([range(8) | "{a,b,c}"] | join(""))}}')"
    h "a brace product too large to expand is refused, never a race with the timeout" deny "$blow" HOME=/h
    # REVIEW ROUND 14. The MCP arm had no budget on total work: 99 padding strings of ~4KB and ~2000
    # segments each pushed a transcript path past the timeout, which fails open.
    pad="$(jqh -cn '{tool_name:"mcp__jkb__ingest_path", cwd:"/h/repos/jkb", tool_input:{path:"/h/.claude/projects/-s/e.jsonl", pad:[range(99) as $i | ("/a\($i)/" + ([range(2000)|"a/"]|join("")))]}}')"
    t0=$(date +%s%N)
    h "an MCP call padded to run past the timeout is refused" deny "$pad" HOME=/h
    case "$t0" in *N) t1=0 ;; *) t1=$(( ($(date +%s%N) - t0) / 1000000 )) ;; esac
    if [ "$t1" -lt 2000 ]; then printf '  \033[32mok\033[0m   ...in %sms\n' "$t1"
    else printf '  \033[31mFAIL\033[0m ...but it took %sms, near enough the timeout to fail open\n' "$t1"; fails=$((fails+1)); fi
    # REVIEW ROUND 15. The cost is check() calls, not segments: 100 short `~name/` strings from three
    # bases passed the segment cap and ran 4.6s idle, 20s under load. Refused on a count, and fast.
    many="$(jqh -cn '{tool_name:"mcp__x__y", cwd:"/h/repos/jkb/a", tool_input:{p:[range(100) as $i | ("~u\($i)/" + ([range(37)|"a/"]|join("")))]}}')"
    t0=$(date +%s%N)
    h "100 ~name strings from three bases are refused on a check count" deny "$many" HOME=/h CLAUDE_PROJECT_DIR=/h/repos/jkb
    case "$t0" in *N) t1=0 ;; *) t1=$(( ($(date +%s%N) - t0) / 1000000 )) ;; esac
    if [ "$t1" -lt 1500 ]; then printf '  \033[32mok\033[0m   ...in %sms\n' "$t1"
    else printf '  \033[31mFAIL\033[0m ...but it took %sms\n' "$t1"; fails=$((fails+1)); fi
    # REVIEW ROUND 13. 64 long, distinct prefixes made glob_base's per-prefix work run past the
    # timeout, which fails open: refused on a budget before any prefix is walked, and fast.
    slow="$(jqh -cn '{tool_name:"Glob", cwd:"/h/repos/jkb", tool_input:{pattern:("~/.claude/projects/" + ([range(6)|"{.,./}"]|join("")) + ([range(1960)|"./"]|join("")) + "*/*.jsonl")}}')"
    t0=$(date +%s%N)
    h "64 long distinct Glob prefixes are refused" deny "$slow" HOME=/h
    case "$t0" in *N) t1=0 ;; *) t1=$(( ($(date +%s%N) - t0) / 1000000 )) ;; esac
    if [ "$t1" -lt 2000 ]; then printf '  \033[32mok\033[0m   ...in %sms\n' "$t1"
    else printf '  \033[31mFAIL\033[0m ...but it took %sms, near enough the timeout to fail open\n' "$t1"; fails=$((fails+1)); fi
    # REVIEW ROUND 12. A ~ before the first brace: its literal prefix is empty, and only the cwd was
    # judged while the first expansion is the denied tree.
    h "a Glob ~{,x}/.claude/projects/** is denied like its first expansion" deny \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"~{,x}/.claude/projects/**"}}' HOME=/h
    h "a Glob whose alternatives are ordinary relative paths is still allowed" allow \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"{src,tests/{unit,e2e}}/**/*.rs"}}' HOME=/h
    # ...and ~word in free text is an account only BY NAME, judged at the home it really has: getent
    # also answers a uid (`~5`) and system accounts (`~sync`, home /bin), all denied as $HOME.
    h "jkb's vector search ~sync is not the home of the sync account's tree" allow \
      '{"tool_name":"mcp__jkb__query","cwd":"/h/repos/jkb","tool_input":{"dsl":"~sync"}}' HOME=/h
    h "a ~5 is not uid 5's home" allow \
      '{"tool_name":"mcp__x__note","cwd":"/h/repos/jkb","tool_input":{"text":"~5"}}' HOME=/h
    # REVIEW ROUND 9.
    h "a leading brace that never closes is refused, not kept literal" deny \
      '{"tool_name":"Glob","cwd":"/h/repos","tool_input":{"pattern":"x}{{/.,/}./{.,}./.claude/projects/**"}}' HOME=/h
    h "an archived transcript is denied" deny \
      '{"tool_name":"Read","cwd":"/h/repos/jkb","tool_input":{"file_path":"/h/.claude-state/transcript-archive/-s/e.jsonl"}}' HOME=/h
    words="$(jqh -cn '{tool_name:"mcp__x__y", cwd:"/h/repos/jkb", tool_input:{w:[range(2001) | "w\(.)"]}}')"
    h "more bare words than the cap is a refusal, never a race with the timeout" deny "$words" HOME=/h
    words="$(jqh -cn '{tool_name:"mcp__x__y", cwd:"/h/repos/jkb", tool_input:{w:[range(2000) | "w\(.)"]}}')"
    h "...while 2000 is allowed" allow "$words" HOME=/h
    huge="$(jqh -cn '{tool_name:"Glob", cwd:"/h/repos/jkb", tool_input:{pattern:([range(3000) | "ab"] | join("") | "{" + . + ",x}")}}')"
    h "a Glob pattern over the byte budget is refused before it is walked" deny "$huge" HOME=/h

    # FAIL CLOSED: every way of not reaching a verdict is a refusal.
    h "an unparseable payload is denied" deny 'not json {' HOME=/h
    h "an empty payload is denied" deny '' HOME=/h
    # With HOME unset the hook falls back to the ACCOUNT's home, so the probe is a transcript there.
    acct="$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6)"
    if [ -n "$acct" ]; then
        h "HOME unset falls back to the account home and still denies -- never rc 1" deny \
          '{"tool_name":"Read","cwd":"/tmp","tool_input":{"file_path":"'"$acct"'/.claude/projects/-s/e.jsonl"}}' -u HOME
    else
        # Without getent the fallback cannot be exercised, and the row would pass on the empty-home
        # refusal instead -- green for the wrong reason. Say so rather than count it.
        printf '  \033[33mskip\033[0m HOME-unset fallback: no getent here, so it cannot be exercised\n'
    fi
    # A HOSTILE PATH CANNOT CHOOSE WHAT RUNS. This hook is unsandboxed, and the image puts the
    # agent-writable ~/.cargo/bin first on PATH: a planted `jq` (or `bash`, via an env shebang) ran
    # outside the sandbox (review round 4, measured). Planted here for every tool the hook calls,
    # and the hook executed DIRECTLY, so its own shebang is what is tested, not `bash "$self"`.
    evil="$(mktemp -d)" && [ -d "$evil" ] || { printf '  \033[31mFAIL\033[0m mktemp -d failed\n'; exit 1; }
    for prog in bash jq realpath getent id cut cat dirname sed env; do
        printf '#!/bin/sh\n: > "%s/RAN-%s"\nexit 0\n' "$evil" "$prog" > "$evil/$prog"; chmod +x "$evil/$prog"
    done
    out="$(printf '%s' '{"tool_name":"Read","cwd":"/h/repos/jkb","tool_input":{"file_path":"/h/.claude/projects/-s/e.jsonl"}}' \
           | HOME=/h PATH="$evil:/usr/bin:/bin" "$self" 2>/dev/null)"
    ran="$(ls "$evil" | grep '^RAN-' | tr '\n' ' ')"
    if [ -n "$ran" ]; then printf '  \033[31mFAIL\033[0m a planted program ran: %s\n' "$ran"; fails=$((fails+1))
    else printf '  \033[32mok\033[0m   with a hostile PATH, no planted program runs\n'; fi
    case "$out" in *'"permissionDecision":"deny"'*) printf '  \033[32mok\033[0m   ...and the verdict is still a deny\n' ;;
        *) printf '  \033[31mFAIL\033[0m ...but the verdict was not a deny: %s\n' "$out"; fails=$((fails+1)) ;; esac
    # PRIVILEGED MODE: a BASH_ENV must not run. Without `-p` in the shebang, bash sources it before
    # the first line of this script -- unsandboxed (review round 7).
    printf '#!/bin/sh\n: > "%s/RAN-BASH_ENV"\n' "$evil" > "$evil/bash_env"
    printf '%s' '{"tool_name":"Read","cwd":"/h/repos/jkb","tool_input":{"file_path":"/h/repos/jkb/x"}}' \
        | HOME=/h BASH_ENV="$evil/bash_env" "$self" >/dev/null 2>&1
    if [ -e "$evil/RAN-BASH_ENV" ]; then printf '  \033[31mFAIL\033[0m BASH_ENV ran: the shebang has lost -p\n'; fails=$((fails+1))
    else printf '  \033[32mok\033[0m   a planted BASH_ENV does not run\n'; fi
    case "$evil" in */tmp.*) rm -rf -- "$evil" ;; esac
    if [ "$fails" -eq 0 ]; then printf '\033[32mdeny-transcripts self-test passed\033[0m\n'; exit 0; fi
    printf '\033[31mdeny-transcripts self-test: %s failed\033[0m\n' "$fails"; exit 1
fi

# ---------------------------------------------------------------------------- hook mode
# The trap is set BEFORE anything that can fail, so nothing below can end this script without a
# decision: `decided` is set only by allow/deny, and any other exit -- set -u, a crash, a helper
# missing -- becomes exit 2, which blocks.
decided=""
on_exit() {
    local rc=$?
    [ -n "$decided" ] && exit "$rc"
    printf 'deny-transcripts.sh could not reach a decision (exit %s), so it refuses: this hook guards other sessions'"'"' transcripts and an unclassified call is treated as one.\n' "$rc" >&2
    exit 2
}
trap on_exit EXIT

allow() { decided=allow; exit 0; }
# deny [reason] -- the transcript reason unless another is given; the boundary has its own (below).
deny() {
    decided=deny
    if [ -n "${1:-}" ]; then
        printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}\n' "$(printf '%s' "$1" | jqh -Rs .)"
        exit 0
    fi
    printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}\n' \
        '"Session transcripts are not readable, and neither is any directory containing them -- a search rooted there walks into them. They are other agents'"'"' working context, not a source of truth for this repository, and reading them is how one session inherits another'"'"'s mistakes. Auto-memory under <slug>/memory/ IS readable, and ~/.jkb/claude-memory holds the shared store. If you need what another session concluded, read the decision record it left in docs/ or the commit message."'
    exit 0
}

# BASH, AND THE BUILT-INS THAT TAKE NO PATH, ARE DECIDED FIRST -- before roots, realpath, or anything
# else that can fail. Bash is the repair tool, and a container broken in some other way must not
# refuse it too: it is refused now only if jq itself is missing, which verify.sh reports by name.
# The kernel sandbox confines Bash, and its command text is not a path. The pathless built-ins carry
# text, not locations -- a todo list, a question, a subagent's prompt -- and judging their strings as
# paths refused ordinary calls once every tool reached this hook: 40 todos from a home cwd, an empty
# activeForm read as "the home, an ancestor of the tree", a 4.6KB Agent prompt over the byte budget
# (review round 4, all reproduced). A subagent's own tool calls reach this hook in their own right.
# Read with the builtin, not `cat`: every tool call pays for this hook now, so a fork saved here is
# saved on every Bash call.
IFS= read -r -d '' input || true
command -v jq >/dev/null 2>&1 || exit 3
tool="$(printf '%s' "$input" | jqh -er '.tool_name | strings' 2>/dev/null)" || exit 3
case "$tool" in
    Bash|TodoWrite|AskUserQuestion|Agent|Task|ToolSearch|SendMessage) allow ;;
esac

# HOME from the account database when the environment lost it: the roots are derived from it, and
# a hook that cannot name the tree it guards cannot classify anything. Still empty -> refuse.
# THE ENVIRONMENT CAN ADD A ROOT, NEVER REMOVE ONE. The roots come from the union of $HOME and the
# account's home in the password database: a settings layer can set `env.HOME`, and roots taken from
# $HOME alone then guarded /tmp/x/.claude while the real tree sat unguarded (review round 6).
acct_home="$(getent passwd "$UID" 2>/dev/null | cut -d: -f6)"
home="${HOME:-$acct_home}"
[ -n "$home" ] || exit 3
# NORMALISED, so a HOME of `/home/vscode/` or `/home//vscode` still names the tree: built raw, the
# roots became `/home/vscode//.claude/projects`, which no normalised path ever starts with, and
# every transcript read was allowed.
home="$(normalise "$home")"
# EVERY SPELLING OF THE TREE, one per line: the home's, the state volume's, CLAUDE_CONFIG_DIR's when
# it is set (Claude Code writes transcripts there then, and the sweep already honoured it while this
# did not), and each of those as the kernel resolves it -- a HOME that passes through a symlink
# gave roots no `realpath -m` output ever starts with, so direct reads went through. Review round 3.
# Not shared with the sweep by loading it: this file is installed alone, root-owned, at
# /usr/local/bin, with no sibling to load from. check-config.sh holds the two to the same set.
# ONE realpath for every root, not one per root: this runs on every tool call.
root_list=()
# THE ARCHIVE IS A ROOT TOO: the sweep moves transcripts to ~/.claude-state/transcript-archive, and
# outside every root they were one plain Read away (review round 9). check-config.sh holds this list
# and the sweep's TRANSCRIPT_ARCHIVE to the same spelling.
for r in "$home/.claude/projects" "$home/.claude-state/projects" "$home/.claude-state/transcript-archive" \
         ${acct_home:+"$acct_home/.claude/projects" "$acct_home/.claude-state/projects" "$acct_home/.claude-state/transcript-archive"} \
         ${CLAUDE_CONFIG_DIR:+"$CLAUDE_CONFIG_DIR/projects"}; do
    root_list+=("$(normalise "$r")")
done
root_phys="$(realpath -m -- "${root_list[@]}" 2>/dev/null)" || exit 3
roots="$(printf '%s\n' "${root_list[@]}" "$root_phys" | awk 'NF && !seen[$0]++')
"

# One parse, @sh-quoted so `eval` assigns rather than executes. Every location a file tool can be
# pointed at: Read/Edit/Write carry file_path, Grep/Glob carry path, NotebookEdit notebook_path,
# and Glob's pattern can carry the location by itself.
# EVERY FIELD MUST BE A STRING OR ABSENT. `@sh` quotes a string as one word but an ARRAY as several,
# so `eval` of a file_path of ["/x","/bin/sh","-c","..."] RAN the command, outside the sandbox,
# before anything refused it. Reachable only past Claude Code's own schema validation, but a
# confidentiality hook must not be a command runner on any input: a wrong type is now a refusal.
assign="$(printf '%s' "$input" | jqh -er '
    def s: if . == null then "" elif type == "string" then . else error("not a string") end;
    @sh "tool=\(.tool_name | s) cwd=\(.cwd | s) fp=\(.tool_input.file_path | s) pth=\(.tool_input.path | s) nb=\(.tool_input.notebook_path | s) pat=\(.tool_input.pattern | s)"' 2>/dev/null)" || exit 3
eval "$assign"
[ -n "$cwd" ] || cwd="$PWD"

# THE SANDBOX'S OWN BOUNDARY, FOR THE TOOLS IT DOES NOT CONFINE. Claude Code's file tools and MCP
# servers run in its own process, outside the Bash sandbox, and were held only by deny rules: a list
# of what is FORBIDDEN, to which every review round added the place nobody had thought of
# (~/.docker, /Applications, ~/.cargo/env...). So every path a tool is handed is also judged against
# the same allow lists the sandbox enforces on Bash, read from the same settings layers: a write
# must land under allowWrite; a read must not land under denyRead unless allowRead or allowWrite
# covers it. One list, two enforcers (design A, chosen after review round 15). Judged on the PHYSICAL
# path, as the kernel sandbox judges it, so auto-memory reached through its ~/.claude/projects link
# into ~/.jkb is readable, and a link in ~/repos pointing at ~/.ssh is not.
# Mirrored only when the merged settings ENABLE the sandbox; with no sandbox there is no boundary to
# mirror, and the transcript rule above still applies. An agent cannot turn it off: every layer read
# here is write-denied to the sandbox and Edit-denied to the tools.
sb_on=0; sb_w=(); sb_r=(); sb_dr=()
sb_layers=(/etc/claude-code/managed-settings.json /etc/claude-code/managed-settings.d/*.json
           "${CLAUDE_CONFIG_DIR:-$home/.claude}/settings.json")
[ -n "${CLAUDE_PROJECT_DIR:-}" ] && sb_layers+=("$CLAUDE_PROJECT_DIR/.claude/settings.json" "$CLAUDE_PROJECT_DIR/.claude/settings.local.json")
sb_args=(); sb_names=()
sb_i=0
for f in "${sb_layers[@]}"; do
    [ -f "$f" ] && [ -r "$f" ] || continue
    sb_args+=(--rawfile "s$sb_i" "$f"); sb_names+=("\$s$sb_i"); sb_i=$((sb_i + 1))
done
if [ "$sb_i" -gt 0 ]; then
    # ONE jq, every layer as raw text parsed inside: a file that is not valid JSON contributes nothing,
    # as Claude Code skips it, instead of failing the whole read. `enabled` is the last layer's word
    # in the order read (user, project, local, managed last); the lists are unions, as Claude Code
    # merges arrays across layers.
    sb_names_csv="$(IFS=,; printf '%s' "${sb_names[*]}")"
    sb_sh="$(jqh -nr "${sb_args[@]}" '
        def p: try fromjson catch {};
        def str: if type == "string" then . else empty end;
        [ ('"$sb_names_csv"') | p | .sandbox // {} ] as $l
        | ([ $l[] | .enabled | select(. != null) ] | last // false) as $on
        | @sh "sb_on=\(if $on == true then 1 else 0 end)
               sb_w=(\([ $l[] | .filesystem.allowWrite[]? | str ] | unique))
               sb_r=(\([ $l[] | .filesystem.allowRead[]? | str ] | unique))
               sb_dr=(\([ $l[] | .filesystem.denyRead[]? | str ] | unique))"' 2>/dev/null)" || exit 3
    eval "$sb_sh"
fi
if [ "$sb_on" = 1 ]; then
    # `~` is the home; a relative entry (".") is the project's, as Claude Code reads it. Then every
    # entry resolved the way the path it is compared with is: physically.
    sb_abs() { case "$1" in "~") printf '%s\n' "$home" ;; "~/"*) printf '%s\n' "$home/${1#\~/}" ;; /*) printf '%s\n' "$1" ;;
               *) printf '%s\n' "${CLAUDE_PROJECT_DIR:-$cwd}/${1#./}" ;; esac; }
    # Claude Code's own writable places, which the Bash sandbox also grants: the session's cwd and
    # project, the temp roots it hands tools, and ~/.claude/plans, where plan mode writes.
    sb_w+=("$cwd" "${CLAUDE_PROJECT_DIR:-$cwd}" "${TMPDIR:-/tmp}" "/tmp/claude" "/tmp/claude-$UID" "~/.claude/plans")
    sb_resolve() { # sb_resolve <name of array> -- each entry, absolute and physical, in place
        local -n arr="$1"; local e out=()
        for e in "${arr[@]}"; do out+=("$(sb_abs "$e")"); done
        [ "${#out[@]}" -gt 0 ] || { arr=(); return 0; }
        # `$( )`, never `< <( )`: a resolver that fails must refuse, not hand back a short list.
        local res; res="$(realpath -m -- "${out[@]}" 2>/dev/null)" || exit 3
        mapfile -t arr <<<"$res"
    }
    sb_resolve sb_w; sb_resolve sb_r; sb_resolve sb_dr
fi
sb_under() { # sb_under <path> <entry>... -> rc 0 when <path> is an entry or lies inside one
    local p="$1" e; shift
    for e in "$@"; do
        [ -n "$e" ] || continue
        [ "$e" = / ] && return 0
        [ "$p" = "$e" ] || [ "${p#"${e%/}"/}" != "$p" ] && return 0
    done
    return 1
}
# boundary <physical path> -- deny a path the sandbox would not let Bash reach in this tool's mode.
boundary() {
    [ "$sb_on" = 1 ] || return 0
    # AUTO-MEMORY IS THE ONE DELIBERATE DIFFERENCE from what Bash may reach: `<root>/<slug>/memory/`
    # is Claude Code's own memory, which its tools must read and write, linked into ~/.jkb or not.
    # Sandboxed Bash cannot see the tree at all. The transcript rule above draws the same line.
    local r rest
    while IFS= read -r r; do
        [ -n "$r" ] || continue
        rest="${1#"$r"/}"; [ "$rest" != "$1" ] || continue
        rest="${rest#*/}"
        [ "$rest" = memory ] || [ "${rest#memory/}" != "$rest" ] && return 0
    done <<<"$roots"
    if [ "$sb_mode" = write ]; then
        sb_under "$1" ${sb_w[@]+"${sb_w[@]}"} && return 0
        deny "$1 is outside the sandbox's writable paths (allowWrite in the Claude settings), and the file tools are held to the same boundary as Bash. Write inside the workspace or another allowWrite directory."
    fi
    sb_under "$1" ${sb_w[@]+"${sb_w[@]}"} ${sb_r[@]+"${sb_r[@]}"} && return 0
    sb_under "$1" ${sb_dr[@]+"${sb_dr[@]}"} || return 0
    deny "$1 is under a path the sandbox denies reading (denyRead in the Claude settings), and the file tools are held to the same boundary as Bash."
}
# Writes for the tools that write; everything else -- Read, Grep, Glob, MCP servers, unknown tools --
# is judged as a read, the weaker test, since what an unknown tool does with a path is unknown.
case "$tool" in Write|Edit|MultiEdit|NotebookEdit) sb_mode=write ;; *) sb_mode=read ;; esac

check() { # check <path> [base]: deny on deny, return on allow, refuse on anything else
    local base="${2:-$cwd}" v abs raw phys p="$1"
    # A file:// URI IS ITS PATH. Joined onto the cwd as a relative string, `file:///h/.claude/...`
    # judged as /cwd/file:/h/... and an MCP server handed a transcript as a URI was allowed (review
    # round 6). Percent-escapes are refused rather than decoded: a decoder here is a second parser
    # of the same string, and the agent can always send the plain path.
    local up
    up="$(uri_path "$p")" || exit 3
    [ -n "$up" ] && p="$up"
    set -- "$p" "${2:-}"
    # A BYTE BUDGET, before any walk: PATH_MAX is 4096, so no path the kernel can open is longer,
    # and anything longer is either garbage or a `a/..`-chain built to be slow. Refused up front so
    # no input can push this into its timeout.
    [ "${#1}" -le 4096 ] || exit 3
    v="$(verdict "$1" "$roots" "$home" "$base")"
    case "$v" in deny) deny ;; allow) ;; *) exit 3 ;; esac
    # ...and AS THE KERNEL WILL RESOLVE IT, from the UN-normalised join. Resolving the lexically
    # collapsed path let a symlink followed by `..` through: `l2/..` with l2 -> the tree collapsed
    # to the repo before realpath saw the link, while the kernel follows l2 first and then climbs.
    # Review round 3, reproduced. A resolver that fails cannot say where the path lands: refuse.
    # SENTINEL CAPTURES: `$( )` drops every trailing newline, so a path ending in one -- a link
    # named "x\n" -- was judged as "x" while the tool opened the link (review round 13). The `.`
    # keeps the path's own newlines; only the one the producer adds is removed.
    raw="$(join_raw "$1" "$home" "$base"; printf .)"; raw="${raw%.}"; raw="${raw%$'\n'}"
    abs="$(resolve "$1" "$home" "$base")"
    phys="$(realpath -m -- "$raw" 2>/dev/null && printf .)" || exit 3
    phys="${phys%.}"; phys="${phys%$'\n'}"
    [ -n "$phys" ] || exit 3
    # The sandbox's boundary, on the path the kernel would open.
    boundary "$phys"
    if [ "$phys" != "$abs" ]; then
        v="$(verdict "$phys" "$roots" "$home" "$base")"
        case "$v" in deny) deny ;; allow) ;; *) exit 3 ;; esac
    fi
    # `~name/...` HAS TWO READINGS, and both are judged. A shell or a server that expands it opens a
    # home; one that does not -- jkb's ingest_path, Rust's fs::read -- opens a directory literally
    # named `~name` in its cwd. Judged as the home alone, a cwd link named `~t` pointing into the
    # tree was allowed (review round 9, reproduced). `./` makes the second reading relative.
    case "$1" in "~/"*|"~") ;; "~"?*) check "./$1" "$base" ;; esac
    return 0
}

# The matcher is `.*` -- every tool reaches this hook, so a built-in tool added tomorrow lands in the
# judge-every-string arm below instead of being exempt by default (the allowlist matcher let
# Artifact, which reads and uploads a local file, past it; review round 3). Bash and the pathless
# built-ins were already let through above.

# A NEWLINE IN A PATH FIELD OR A GLOB PATTERN is refused outright: no real path needs one, and every
# helper here passes paths through lines (review round 13). Free text in other tools keeps its
# multi-line judging, with check()'s sentinel captures.
case "$fp$nb$pth" in *$'\n'*) deny ;; esac
[ "$tool" = Glob ] && case "$pat" in *$'\n'*) deny ;; esac
for p in "$fp" "$nb"; do [ -n "$p" ] && check "$p"; done
case "$tool" in
    Grep|Glob)
        # A search with no path searches the session's cwd.
        base="${pth:-$cwd}"
        check "$base"
        if [ "$tool" = Glob ] && [ -n "$pat" ]; then
            # ONLY THE LITERAL PREFIX IS JUDGED, so the rest must not steer the walk: a `..` after a
            # wildcard (`*/../../../.claude/projects/*`) or an absolute alternative in braces
            # (`{/h/.claude/projects/**,**/*.rs}`) went unjudged (review round 6). Both are refused;
            # ordinary patterns -- `**/*.{rs,toml}`, `{src,tests}/**` -- hold neither.
            # Judged on EVERY EXPANSION of the braces, never on their raw alternatives: round 6 read
            # the first group alone, round 7 every group's alternatives as text, and both let a
            # nested group or a `..` built across a group boundary (`.{.,}`) through (round 8).
            # The byte budget check() applies to a path, applied to the pattern BEFORE the expander
            # walks it a character at a time: unbounded, a megabyte pattern was a walk into the
            # timeout, which fails open.
            [ "${#pat}" -le 4096 ] || exit 3
            brace_expand "$pat" || exit 3
            # EACH EXPANSION'S OWN LITERAL PREFIX is judged, not the raw pattern's. The raw prefix of
            # `~{,x}/.claude/projects/**` is empty -- glob_base stops at the first segment holding a
            # brace -- so only the cwd was judged while the first expansion is the denied tree
            # (review round 12, reproduced). A `..` is allowed IN a prefix, which is judged, and
            # refused only past one (round 7: `../docs/*.md` is ordinary); an expansion may not turn
            # absolute when the pattern is not. Ordinary patterns -- `**/*.{rs,toml}`,
            # `{crates/a,crates/b}/**` -- share one prefix, so this judges each distinct prefix once.
            # A BUDGET ON THE WORK, before any prefix is walked: 64 distinct ~4KB prefixes ran past
            # the 10s timeout, which fails open (review round 13, measured at 30s). Expansions times
            # pattern bytes; an ordinary pattern is a few hundred.
            [ $(( ${#brace_out[@]} * ${#pat} )) -le 12288 ] || exit 3
            # AN ARRAY of distinct prefixes, never newline-joined text: a newline in an expansion
            # split one prefix into two harmless ones (round 13; newlines are refused above too).
            gbs=()
            for e in "${brace_out[@]}"; do
                case "$pat" in /*|"~"*) ;; *) case "$e" in /*|"~"*) deny ;; esac ;; esac
                gb="$(glob_base "$e"; printf .)"; gb="${gb%.}"; gb="${gb%$'\n'}"
                rest="${e#"$gb"}"
                case "/$rest/" in */../*) deny ;; esac
                seen=0; for g in ${gbs[@]+"${gbs[@]}"}; do [ "$g" = "$gb" ] && seen=1; done
                [ "$seen" -eq 1 ] || gbs+=("$gb")
            done
            for gb in ${gbs[@]+"${gbs[@]}"}; do
                [ -n "$gb" ] || continue
                # Joined onto the UN-normalised base, so check()'s realpath meets any link in the
                # base before the pattern's `..` segments: resolving the base first collapsed
                # `l/..` to the cwd, and a pattern climbing from there was judged from the wrong
                # directory (review round 5, reproduced) -- round 3's symlink-then-`..` defect in
                # a composition check() itself never saw.
                # A `~name` prefix's literal reading (check's ./~name) is taken from the Glob's own
                # `path`, not the session cwd: aimed at the cwd, a `~x` link under `path` reached the
                # tree (review round 14).
                case "$gb" in /*|"~"*) check "$gb" "$(join_raw "$base" "$home" "$cwd")" ;; *) check "$(join_raw "$base" "$home" "$cwd")/$gb" ;; esac
            done
        fi ;;
    Read|Edit|Write|NotebookEdit)
        [ -n "$pth" ] && check "$pth" ;;
    *)
        # A tool whose fields are not known here: every string it was given that COULD BE A PATH is
        # judged -- one holding a `/`, or starting `~` or `.` (multi-line strings included, see below),
        # or a bare word that names an existing entry in a base (round 8, below). Not every
        # string: with every tool now routed here, a flat cap on all strings refused a TodoWrite or
        # an AskUserQuestion with a long list. The exception is a cwd that is itself in or above the
        # tree, where a bare word like `projects` is a path into it; then every string counts.
        # `strings` filters to strings, so @sh cannot produce an executable word here.
        # EVERY STRING scanned only where a bare word can reach the tree: a cwd that IS a root's
        # parent (~/.claude, where `projects` is a root) or lies inside one. It used to be "any
        # ancestor", which made a home cwd scan everything and refuse a long todo list.
        scan_all=0
        while IFS= read -r r; do
            [ -n "$r" ] || continue
            if [ "$cwd" = "${r%/*}" ] || [ "$cwd" = "$r" ] || [ "${cwd#"$r"/}" != "$cwd" ]; then scan_all=1; fi
        done <<<"$roots"
        # UNIQUE, NON-EMPTY candidates: the cap counts distinct strings, and an empty string is never
        # a location -- judged as one it resolved to the cwd. MULTI-LINE STRINGS ARE JUDGED TOO, on
        # purpose: a server that normalises a path lexically turns `/h/.claude\n/../.claude/projects/x`
        # into a transcript path, and the lexical verdict here does the same collapse.
        leaves_sh="$(printf '%s' "$input" | jqh -er --arg all "$scan_all" '
            [.tool_input | .. | strings
             | select(. != "")
             | select($all == "1" or test("^[~.]") or contains("/"))]
            | unique
            | if length > 100 then error("too many") else @sh "leaves=(\(.))" end' 2>/dev/null)" || exit 3
        # ...and every OTHER string short enough to be one name (NAME_MAX): a bare word is a path
        # the moment it names something in a base, and a link named `t` there reached the tree with
        # nothing judged (review round 8, reproduced). Tested on disk below, with no fork per word.
        bare_sh="$(printf '%s' "$input" | jqh -er --arg all "$scan_all" '
            [.tool_input | .. | strings
             | select(. != "" and length <= 255)
             | select(($all == "1" or test("^[~.]") or contains("/")) | not)]
            | unique
            | if length > 2000 then error("too many") else @sh "bare=(\(.))" end' 2>/dev/null)" || exit 3
        leaves=(); bare=()
        eval "$leaves_sh"
        eval "$bare_sh"
        # An MCP server resolves a relative path against ITS OWN cwd, which is not the session's:
        # the jkb server starts in the project root. So a relative string is judged against every
        # base it could plausibly mean. AN ARRAY, iterated quoted: `for b in $bases` split a cwd
        # with a space into fragments, the real base was never judged, and a relative climb into
        # the tree was allowed (review round 4, reproduced).
        bases=("$cwd")
        if [ "${tool#mcp__}" != "$tool" ]; then
            # DISTINCT bases only: the session cwd is usually the project dir, and judging every
            # string twice against one base doubled the worst case for nothing.
            bases=("$cwd")
            for b in "${CLAUDE_PROJECT_DIR:-}" "$home"; do
                [ -n "$b" ] || continue
                dup=0; for e in "${bases[@]}"; do [ "$e" = "$b" ] && dup=1; done
                [ "$dup" -eq 0 ] && bases+=("$b")
            done
        fi
        for w in ${bare[@]+"${bare[@]}"}; do
            for b in "${bases[@]}"; do
                if [ -n "$b" ] && { [ -e "$b/$w" ] || [ -L "$b/$w" ]; }; then leaves+=("./$w"); break; fi
            done
        done
        [ "${#leaves[@]}" -le 100 ] || exit 3
        # A BUDGET ON THE WORK, as the Glob arm has: path segments, times the bases each relative
        # one is judged from. 100 leaves of ~2000 segments ran a transcript path past the 10s
        # timeout, which fails open (review round 14, reproduced at 10s; the leaf cap alone allowed
        # it). Ordinary calls are a few hundred segment-bases.
        segs=0
        for leaf in ${leaves[@]+"${leaves[@]}"}; do sl="${leaf//[^\/]/}"; segs=$((segs + ${#sl} + 1)); done
        [ $(( segs * ${#bases[@]} )) -le 12000 ] || exit 3
        # ...AND ON THE NUMBER OF check() CALLS, which is what the time goes on: each forks realpath
        # and walks the path. Under the segment cap, 100 short `~name/` strings from three bases
        # made 600 calls and ran 4.6s idle, 20s under load (review round 15). An absolute path is
        # one call, a relative one one per base, a `~name` one two per base (both readings).
        nchk=0
        for leaf in ${leaves[@]+"${leaves[@]}"}; do
            case "$leaf" in
                /*|"~/"*|"~") nchk=$((nchk + 1)) ;;
                "~"*) nchk=$((nchk + 2 * ${#bases[@]})) ;;
                *) nchk=$((nchk + ${#bases[@]})) ;;
            esac
        done
        [ "$nchk" -le 32 ] || exit 3
        for leaf in ${leaves[@]+"${leaves[@]}"}; do
            # A SLASHLESS ~word in free text is a home only if that account exists. `~retry` is jkb's
            # own one-word vector search and `~2h` an estimate; reading them as home directories
            # refused ordinary calls (round 7). Otherwise it is judged as what a server that does
            # not expand it opens: a relative name. Skipping it let a link named `~t` reach the tree
            # (round 8). A path field keeps the over-approximation.
            case "$leaf" in
                "~"*/*|"~") ;;
                # BY NAME, AT ITS REAL HOME: getent also answers a uid (`~5`) and system accounts
                # (`~sync`, whose home is /bin), and reading every one as $HOME refused jkb's own
                # `~sync` search (review round 12). An account whose first field is the word is
                # judged at the home it has; anything else is a relative name.
                "~"*) ent="$(getent passwd "${leaf#\~}" 2>/dev/null | head -1)"
                      ent_home="$(printf '%s' "$ent" | cut -d: -f6)"
                      # BOTH READINGS for an account: its home, and the literal name a server that
                      # does not expand it opens. Judged at the home alone, a cwd link named `~sync`
                      # reached the tree (review round 13).
                      [ "${ent%%:*}" = "${leaf#\~}" ] && [ -n "$ent_home" ] && check "$ent_home"
                      leaf="./$leaf" ;;
            esac
            # A free-text string that merely STARTS `file:` is text, not a refusal (round 7); only a
            # `file:/...` URI is rewritten, and the same helper serves the long branch below.
            case "$(printf '%s' "${leaf:0:6}" | tr 'A-Z' 'a-z')" in
                file:/) up="$(uri_path "$leaf")" || exit 3; [ -n "$up" ] && leaf="$up" ;;
                file:*) continue ;;
            esac
            # Over PATH_MAX is either prose or a chain built to be slow. A path field refuses it
            # (check does); here it may be prose, so it gets the lexical verdict alone -- linear now,
            # and the collapse a server would do to get under PATH_MAX is the collapse it mirrors.
            if [ "${#leaf}" -gt 4096 ]; then
                for b in "${bases[@]}"; do
                    [ -n "$b" ] || continue
                    v="$(verdict "$leaf" "$roots" "$home" "$b")"
                    case "$v" in deny) deny ;; allow) ;; *) exit 3 ;; esac
                done
                continue
            fi
            case "$leaf" in
                /*|"~/"*|"~") check "$leaf" ;;
                # Both readings of `~name...` (check does the second), from every base.
                "~"*) for b in "${bases[@]}"; do [ -n "$b" ] && check "$leaf" "$b"; done ;;
                *) for b in "${bases[@]}"; do [ -n "$b" ] && check "$leaf" "$b"; done ;;
            esac
        done ;;
esac
allow
