#!/usr/bin/env bash
# PreToolUse hook: deny the file tools any session's transcript, while leaving auto-memory alone.
#
#   .container/deny-transcripts.sh            # hook mode: tool-call JSON on stdin
#   .container/deny-transcripts.sh --self-test
#
# WHY A HOOK AND NOT A DENY RULE. This replaces two `permissions.deny` globs, and the reason is a
# resource bound rather than a preference. Claude Code compiles `permissions.deny` into the
# bubblewrap argv for the Bash sandbox. A rule ending in a directory wildcard COLLAPSES to a single
# argv entry -- `Read(~/.ssh/**)` becomes `~/.ssh` -- but a rule ending in a FILE pattern cannot:
# the sandbox names every match and binds /dev/null over each, so the argv grows by one path per
# file on disk. Measured 2026-09-30 in jkb-dev, after a sweep had already run:
#
#     Read(~/.claude/projects/**/*.jsonl)  + its .claude-state spelling
#       206 .jsonl files -> 33,819 bytes of path text per spelling, 67,638 across both,
#       against a MAX_ARG_STRLEN of 131,072 that Linux does not let you raise.  52% of the
#       ceiling, spent by two rules. Past it EVERY Bash tool call in the container fails at
#       spawn with E2BIG -- not the one that overflowed, all of them, including `:` -- with
#       nothing in the message naming transcripts.
#
# AND THE COLLAPSING SHAPE IS UNAVAILABLE, which is the whole reason this file exists. It was
# tried and reverted: `Read(~/.claude/projects/**)` takes the argv to ~50 bytes and also covers
# `~/.claude/projects/<slug>/memory`, which is where Claude Code keeps auto-memory. That location
# is not ours to choose -- scripts/link-claude-memory.sh exists to put the link there and verify.sh
# FAILS when it is missing -- and denied memory does not error, it goes QUIET: MEMORY.md stops
# arriving in context, which reads like an agent that forgot rather than a broken container.
#
# A glob cannot separate `<slug>/memory/` from `<slug>/<uuid>.jsonl`; they are siblings. A hook
# can, and costs nothing in argv because it is code rather than a path list. That is the whole
# trade: O(files) of argv becomes O(1) of argv plus one process per file-tool call.
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
# non-blocking in Claude Code, and nothing inside the script can change that. It answers in ~7ms
# (measured, 50 runs) against the 10s budget managed-settings.json gives it.
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
# ANY TOOL THAT CAN NAME A PATH, not only the six built-in file tools. MCP tools run unsandboxed and
# take whatever arguments their schema says -- jkb's own server has an ingest_path -- so the
# matcher also covers `mcp__.*`, MultiEdit and LS, and for a tool whose fields this script does not
# know, EVERY string in its input is judged as a path. Capped at 100 strings, beyond which the call
# is refused: unbounded, a large payload could drive this past its timeout, and a timed-out hook
# FAILS OPEN. Measured at ~8ms a string (500 took 4.1s against the 10s budget), and this container
# has been seen running 5x slow while its VM was saturated -- so the cap leaves 5x headroom, not 2.
#
# CONSIDERED AND NOT VECTORS, measured 2026-10-01 rather than assumed:
#   hard links   sandboxed Bash cannot see ~/.claude-state/projects at all ("No such file or
#                directory"), so it has no source to name -- and ~/repos is a different
#                filesystem from the state volume (fuseblk vs a volume), so `ln` would be EXDEV.
#   case         ~/repos IS case-insensitive (macOS backing), but the tree lives on case-sensitive
#                Linux filesystems, and a case-variant symlink name under ~/repos is resolved by the
#                kernel inside `realpath -m`, which the physical check uses.
#   bind mounts  an unprivileged namespace the agent creates changes only its own view, never the
#                file tools' -- they run in Claude Code's process, outside it.
#
# THE RESIDUAL, stated: a TIME-OF-CHECK/TIME-OF-USE race. A background process from sandboxed Bash
# could repoint a symlink in an agent-writable directory between this check and the tool's open.
# Closing it would mean refusing every path through a symlink in a writable directory, which
# breaks ordinary repositories; it is left open and written down rather than half-closed.
set -uo pipefail

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
# error here is refusing a path that was harmless, never allowing one that was not.
resolve() { # resolve <path> <home> <cwd> -> normalised absolute path
    local p="$1"
    case "$p" in
        "~")      p="$2" ;;
        "~/"*)    p="$2/${p#\~/}" ;;
        "~"*/*)   p="$2/${p#*/}" ;;
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

# The literal leading part of a glob pattern: every segment before the first that holds a glob
# character. `**/*.jsonl` -> "" (the search root alone decides), `/a/b/*/c` -> /a/b.
glob_base() { # glob_base <pattern> -> literal prefix, possibly empty
    local pat="$1" out="" seg first=1 parts
    case "$pat" in /*) out="/" ;; esac
    split_path "$pat"
    for seg in ${parts[@]+"${parts[@]}"}; do
        [ -n "$seg" ] || continue
        case "$seg" in *[\*\?\[\{]*) break ;; esac
        if [ "$first" -eq 1 ] && [ "$out" != "/" ]; then out="$seg"; else out="${out%/}/$seg"; fi
        first=0
    done
    printf '%s\n' "$out"
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
    # The hook needs GNU `realpath -m`; macOS's BSD realpath has none. Where it is missing, every
    # program-level row would get the refusal and read green or red for the wrong reason, so they
    # are skipped and say so. The hook only ever runs in the container, which has it.
    if ! realpath -m / >/dev/null 2>&1; then
        printf '  \033[33mskip\033[0m hook-mode rows: no GNU realpath -m here (the container has it)\n'
        h() { :; }
    fi
    h() { # h <label> <want deny|allow> <stdin> [env...]
        local label="$1" want="$2" in="$3" out rc=0; shift 3
        out="$(printf '%s' "$in" | env "$@" "$BASH" "$self" 2>/dev/null)" || rc=$?
        local got=allow
        case "$out" in *'"permissionDecision":"deny"'*) got=deny ;; esac
        [ "$rc" -eq 2 ] && got=deny
        [ "$rc" -ne 0 ] && [ "$rc" -ne 2 ] && got="rc=$rc (non-blocking: the call would go through)"
        if [ "$got" = "$want" ]; then printf '  \033[32mok\033[0m   %s\n' "$label"
        else printf '  \033[31mFAIL\033[0m %s\n         got %s, wanted %s\n' "$label" "$got" "$want"; fails=$((fails+1)); fi
    }
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
    h "a symlink that points elsewhere is allowed" allow \
      '{"tool_name":"Read","cwd":"'"$sh"'/repos/r","tool_input":{"file_path":"'"$sh"'/repos/r/ok/f"}}' HOME="$sh"
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
    big="$(jq -cn '{tool_name:"mcp__x__y", cwd:"/h/repos/jkb", tool_input:{a:[range(101) | "/h/repos/jkb/f\(.)"]}}')"
    h "more path-like strings than the cap is a refusal, never a race with the timeout" deny "$big" HOME=/h
    many="$(jq -cn '{tool_name:"TodoWrite", cwd:"/h/repos/jkb", tool_input:{todos:[range(150) | {content:"do thing \(.)", status:"pending"}]}}')"
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
    long="$(jq -cn '{tool_name:"Read", cwd:"/h", tool_input:{file_path:("/" + ([range(6000)|"a/.."]|join("/")) + "/h/.claude/projects/-s/e.jsonl")}}')"
    t0=$(date +%s%N)
    h "a 24KB a/.. chain is refused" deny "$long" HOME=/h
    t1=$(( ($(date +%s%N) - t0) / 1000000 ))
    if [ "$t1" -lt 2000 ]; then printf '  \033[32mok\033[0m   ...in %sms, nowhere near the 10s timeout\n' "$t1"
    else printf '  \033[31mFAIL\033[0m ...but it took %sms, near enough the timeout to fail open\n' "$t1"; fails=$((fails+1)); fi

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
    h "no jq on PATH is a refusal, not a pass" deny \
      '{"tool_name":"Read","cwd":"/h/repos/jkb","tool_input":{"file_path":"/h/repos/jkb/x"}}' HOME=/h PATH=/nonexistent
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
deny() {
    decided=deny
    printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}\n' \
        '"Session transcripts are not readable, and neither is any directory containing them -- a search rooted there walks into them. They are other agents'"'"' working context, not a source of truth for this repository, and reading them is how one session inherits another'"'"'s mistakes. Auto-memory under <slug>/memory/ IS readable, and ~/.jkb/claude-memory holds the shared store. If you need what another session concluded, read the decision record it left in docs/ or the commit message."'
    exit 0
}

# HOME from the account database when the environment lost it: the roots are derived from it, and
# a hook that cannot name the tree it guards cannot classify anything. Still empty -> refuse.
home="${HOME:-$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6)}"
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
roots=""
for r in "$home/.claude/projects" "$home/.claude-state/projects" \
         ${CLAUDE_CONFIG_DIR:+"$CLAUDE_CONFIG_DIR/projects"}; do
    r="$(normalise "$r")"
    roots="$roots$r
"
    rp="$(realpath -m -- "$r" 2>/dev/null)" || exit 3
    [ "$rp" != "$r" ] && roots="$roots$rp
"
done

input="$(cat 2>/dev/null)"
command -v jq >/dev/null 2>&1 || exit 3
# One parse, @sh-quoted so `eval` assigns rather than executes. Every location a file tool can be
# pointed at: Read/Edit/Write carry file_path, Grep/Glob carry path, NotebookEdit notebook_path,
# and Glob's pattern can carry the location by itself.
# EVERY FIELD MUST BE A STRING OR ABSENT. `@sh` quotes a string as one word but an ARRAY as several,
# so `eval` of a file_path of ["/x","/bin/sh","-c","..."] RAN the command, outside the sandbox,
# before anything refused it. Reachable only past Claude Code's own schema validation, but a
# confidentiality hook must not be a command runner on any input: a wrong type is now a refusal.
assign="$(printf '%s' "$input" | jq -er '
    def s: if . == null then "" elif type == "string" then . else error("not a string") end;
    @sh "tool=\(.tool_name | s) cwd=\(.cwd | s) fp=\(.tool_input.file_path | s) pth=\(.tool_input.path | s) nb=\(.tool_input.notebook_path | s) pat=\(.tool_input.pattern | s)"' 2>/dev/null)" || exit 3
eval "$assign"
[ -n "$cwd" ] || cwd="$PWD"

check() { # check <path> [base]: deny on deny, return on allow, refuse on anything else
    local base="${2:-$cwd}" v abs raw phys
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
    raw="$(join_raw "$1" "$home" "$base")"
    abs="$(resolve "$1" "$home" "$base")"
    phys="$(realpath -m -- "$raw" 2>/dev/null)" || exit 3
    [ -n "$phys" ] || exit 3
    [ "$phys" = "$abs" ] && return 0
    v="$(verdict "$phys" "$roots" "$home" "$base")"
    case "$v" in deny) deny ;; allow) return 0 ;; *) exit 3 ;; esac
}

# BASH IS NOT JUDGED HERE. The matcher is `.*` -- every tool reaches this hook, so a built-in tool
# added tomorrow lands in the judge-every-string arm below instead of being exempt by default (the
# allowlist matcher let Artifact, which reads and uploads a local file, past it; review round 3).
# Bash alone is let through: the kernel sandbox confines it, its command text is not a path, and it
# is the tool a person repairs a broken container with.
[ "$tool" = Bash ] && allow

for p in "$fp" "$nb"; do [ -n "$p" ] && check "$p"; done
case "$tool" in
    Grep|Glob)
        # A search with no path searches the session's cwd.
        base="${pth:-$cwd}"
        check "$base"
        if [ "$tool" = Glob ] && [ -n "$pat" ]; then
            gb="$(glob_base "$pat")"
            if [ -n "$gb" ]; then
                case "$gb" in /*|"~"*) check "$gb" ;; *) check "$(resolve "$base" "$home" "$cwd")/$gb" ;; esac
            fi
        fi ;;
    Read|Edit|Write|NotebookEdit)
        [ -n "$pth" ] && check "$pth" ;;
    *)
        # A tool whose fields are not known here: every string it was given that COULD BE A PATH is
        # judged -- one holding a `/`, or starting `~` or `.`, and holding no newline. Not every
        # string: with every tool now routed here, a flat cap on all strings refused a TodoWrite or
        # an AskUserQuestion with a long list. The exception is a cwd that is itself in or above the
        # tree, where a bare word like `projects` is a path into it; then every string counts.
        # `strings` filters to strings, so @sh cannot produce an executable word here.
        scan_all=0
        [ "$(verdict "$cwd" "$roots" "$home" "$cwd")" = deny ] && scan_all=1
        leaves_sh="$(printf '%s' "$input" | jq -er --arg all "$scan_all" '
            [.tool_input | .. | strings
             | select($all == "1" or test("^[~.]") or contains("/"))
             | select(contains("\n") | not)]
            | if length > 100 then error("too many") else @sh "leaves=(\(.))" end' 2>/dev/null)" || exit 3
        leaves=()
        eval "$leaves_sh"
        # An MCP server resolves a relative path against ITS OWN cwd, which is not the session's:
        # the jkb server starts in the project root. So a relative string is judged against every
        # base it could plausibly mean. Review round 3.
        bases="$cwd"
        case "$tool" in mcp__*) bases="$cwd ${CLAUDE_PROJECT_DIR:-} $home" ;; esac
        for leaf in ${leaves[@]+"${leaves[@]}"}; do
            case "$leaf" in
                /*|"~"*) check "$leaf" ;;
                *) for b in $bases; do [ -n "$b" ] && check "$leaf" "$b"; done ;;
            esac
        done ;;
esac
allow
