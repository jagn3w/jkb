#!/bin/bash -p
# PreToolUse hook: keep every tool out of any session's transcript, while leaving auto-memory alone.
#
#   .container/deny-transcripts.sh            # hook mode: tool-call JSON on stdin
#   .container/deny-transcripts.sh --self-test
#
# THE RECORD IS .container/README.md ("The transcript deny is a hook" and "The file tools are held to
# the sandbox's own boundary"): why this is a hook, what each review round found, the measurements.
# None of that is restated here. These are the invariants someone changing THIS FILE must keep:
#
# - EVERY TOOL REACHES IT (matcher `.*`). Bash and the text-carrying built-ins are let through first;
#   the file tools are judged by their path fields; every other tool is judged by THE FIELD TABLE at
#   the bottom -- its listed path fields, or let through as pathless, or, when it is not listed, the
#   fields whose NAMES say they are paths. Nothing scans free text: rounds 6 to 26 did, and could not
#   finish (the user's choices after rounds 26 and 27).
# - IT FAILS CLOSED. An EXIT trap turns every way of not reaching a verdict into exit 2, which blocks;
#   and the judging runs under a deadline (`timeout -s KILL 8`) whose expiry is a refusal, because
#   Claude Code lets a call through when it kills a hook at its own 10s timeout.
# - PATHS ARE RESOLVED AS THE TOOL WILL RESOLVE THEM: `~` is the home, a relative path is relative to
#   the session's cwd (and, for an MCP server, to the project dir it starts in).
# - AN ANCESTOR OF THE TREE IS DENIED, not only paths inside it: Grep and Glob walk what they are
#   rooted at. A Glob's literal prefix, per brace expansion, is judged the same way.
# - EVERY PATH IS JUDGED TWICE: as written, normalised lexically, and as `realpath -m` resolves it --
#   the kernel's view, symlinks followed -- and refused if either reading says so. /proc magic links
#   are refused outright.
# - THE SANDBOX'S OWN BOUNDARY: with the sandbox enabled, every path a tool is handed is also held to
#   the allow lists the sandbox enforces on Bash (`boundary`).
# - AUTO-MEMORY (`<root>/<slug>/memory/`) is the one readable and writable child of the tree, and
#   saved tool output (`<slug>/<session>/tool-results/`) the one readable one.
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
# BYTES, NOT CHARACTERS: under a UTF-8 locale every ${x#lit} that does not match re-decodes the string
# per candidate length, and one 1MB string took 907s against 7.6s under C (review round 20, measured
# in jkb-dev). It also makes ${#x} count bytes, which is what PATH_MAX counts.
LC_ALL=C
export LC_ALL
# WHERE THE INSTALLED COPY LIVES, once. The installed hook trusts none of the self-test seams and none
# of the environment's say over the boundary's locations; a copy anywhere else is the self-test's.
# check-config.sh holds this literal to the managed hook command (review round 32: four copies of it,
# tied to nothing, and a moved install would have put the live hook into test mode).
DT_INSTALLED_PATH=/usr/local/bin/deny-transcripts.sh
dt_installed=0; [ "$0" = "$DT_INSTALLED_PATH" ] && dt_installed=1
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

# A file: URI AS THE PATH IT NAMES, for every caller -- check() and the generic arm both, since round
# 7 found an over-PATH_MAX branch skipping a rewrite that lived in check() alone. The scheme is
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
# confidentiality check must not depend on it, so "starts with" compares bytes and nothing else: a
# SUBSTRING, `${x:0:${#prefix}}` = $prefix. It was `${x#"$prefix"} != $x` until review round 20 found
# that a strip which does not match is quadratic in bash, and over a long string ran the hook past the
# timeout, which fails open. `under <path> <dir>`: is <path> strictly inside <dir>?
under() { [ "${1:0:${#2}+1}" = "$2/" ]; }

verdict() { # verdict <path> <roots> <home> <cwd> -> allow|deny
    local p roots="$2" root rel rest anc
    # PROCFS MAGIC LINKS: a path through them is resolved in whichever process opens it, so it is
    # refused before it is normalised -- see through_magic.
    if through_magic "$(join_raw "$1" "$3" "$4")"; then printf 'deny\n'; return; fi
    p="$(resolve "$1" "$3" "$4")"
    while IFS= read -r root; do
        [ -n "$root" ] || continue
        # INSIDE the tree: the root itself, or anything under root/.
        if [ "$p" = "$root" ] || under "$p" "$root"; then
            # Auto-memory is the one child of a slug that is not a transcript. EXACTLY one slug
            # deep: `<slug>/memory/...`. A `case` `*` crosses `/`, so the first cut's
            # `"$root"/*/memory/*` matched a directory called memory at ANY depth --
            # `<slug>/<uuid>/subagents/memory/a.jsonl` was readable and writable.
            rel="${p:${#root}}"; rel="${rel#/}"
            case "$rel" in
                */*) rest="${rel#*/}"
                     case "$rest" in memory|memory/*) printf 'allow\n'; return ;; esac
                     # ...and Claude Code's own saved TOOL OUTPUT: what it could not show inline it
                     # writes to `<slug>/<session>/tool-results/` and tells the agent to Read. EXACTLY
                     # that depth, by string surgery as above; a transcript beside it stays denied
                     # (review round 16: this branch had made large outputs unreadable).
                     case "$rest" in
                         */*) local after="${rest#*/}"
                              case "$after" in tool-results|tool-results/*) printf 'allow\n'; return ;; esac ;;
                     esac ;;
            esac
            printf 'deny\n'; return
        fi
        # An ANCESTOR of the tree: anything rooted here walks into it. For p=/ this is "/".
        anc="$p"; [ "${anc: -1}" = / ] && anc="${anc:0:${#anc}-1}"; anc="$anc/"
        if [ "${root:0:${#anc}}" = "$anc" ]; then printf 'deny\n'; return; fi
    done <<<"$roots"
    printf 'allow\n'
}

if [ "${1:-}" = --self-test ]; then
    # THE INSTALLED COPY DOES NOT RUN IT. The self-test executes copies of this file staged in /tmp and
    # ~/.cache, which sandboxed agents write, and run from here -- unsandboxed, as verify.sh did -- a
    # swapped copy runs outside the sandbox (review round 34; round 33 had made the installed run
    # re-execute from such a copy). Run the checkout's copy instead, as check.sh and CI do.
    if [ "$dt_installed" = 1 ]; then
        echo "the installed hook does not run its self-test: it executes copies of itself from directories agents write. Run the checkout's .container/deny-transcripts.sh --self-test instead." >&2
        exit 1
    fi
    # Asked to behave as installed, the self-test's own rows still need their scratch homes honoured.
    unset DT_SELFTEST_AS_INSTALLED
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
      '{"tool_name":"Read","cwd":"/tmp","tool_input":{"file_path":["/x","/bin/sh","-c","touch '"$sh"'/PWNED"]}}' HOME="$sh"
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
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$sh"'/repos/r","tool_input":{"source":"~sync"}}' HOME="$sh"
    # ...and a NEWLINE in a path is refused: `$( )` drops a trailing one, so a link named "x\n" was
    # judged as "x" and the tool opened the link.
    ln -s "$sh/.claude-state/projects/-s" "$sh/repos/r/nl
"
    h "a Read whose path ends in a newline is refused" deny \
      '{"tool_name":"Read","cwd":"'"$sh"'/repos/r","tool_input":{"file_path":"nl\n/e.jsonl"}}' HOME="$sh"
    h "a Grep rooted at a link whose name ends in a newline is refused" deny \
      '{"tool_name":"Grep","cwd":"'"$sh"'/repos/r","tool_input":{"path":"nl\n","pattern":"p"}}' HOME="$sh"
    h "an MCP string naming that link is denied" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$sh"'/repos/r","tool_input":{"source":"./nl\n"}}' HOME="$sh"
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
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$bh"'/repos/w","tool_input":{"source":"'"$bh"'/.ssh/id"}}' HOME="$bh"
    h "boundary: a Grep rooted at the home is refused" deny \
      '{"tool_name":"Grep","cwd":"'"$bh"'/repos/w","tool_input":{"path":"'"$bh"'","pattern":"p"}}' HOME="$bh"
    h "boundary: a Write to the session's TMPDIR is allowed" allow \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$sh"'/tmpdir/x","content":""}}' HOME="$bh" TMPDIR="$sh/tmpdir"
    h "boundary: a plan file under ~/.claude/plans is writable" allow \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.claude/plans/p.md","content":""}}' HOME="$bh"
    # REVIEW ROUND 16. Claude Code's own saved tool output is readable; relative free text in an MCP
    # call is not judged as a path under the home; a Read() deny or a credential inside an allowed
    # tree is still denied; the settings layers the boundary reads are not writable through it.
    mkdir -p "$bh/.claude/projects/-w/sess1/tool-results" "$bh/.claude/projects/-w/sess1/subagents" "$bh/notes" "$bh/.cargo"
    : >"$bh/.claude/projects/-w/sess1/tool-results/out.txt"; : >"$bh/.cargo/credentials.toml"
    h "round 16: a saved tool result is readable" allow \
      '{"tool_name":"Read","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.claude/projects/-w/sess1/tool-results/out.txt"}}' HOME="$bh"
    h "round 16: ...while a subagent transcript beside it is not" deny \
      '{"tool_name":"Read","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.claude/projects/-w/sess1/subagents/a.jsonl"}}' HOME="$bh"
    h "round 16: an MCP namespace path is not read as a home path" allow \
      '{"tool_name":"mcp__jkb__task_create","cwd":"'"$bh"'/repos/w","tool_input":{"title":"t","place":"tasks/inbox"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 16: ...nor a URL" allow \
      '{"tool_name":"mcp__jkb__ingest_url","cwd":"'"$bh"'/repos/w","tool_input":{"url":"https://example.com/x"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 16: ...nor a word naming a directory in the home" allow \
      '{"tool_name":"mcp__jkb__search","cwd":"'"$bh"'/repos/w","tool_input":{"query":"notes"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    printf '%s\n' '{"permissions":{"deny":["Read(~/.cargo/credentials.toml)"]},"sandbox":{"enabled":true,"filesystem":{"denyRead":["~"],"allowWrite":["~/repos","~/.jkb","~/.cargo"]}}}' > "$bh/.claude/settings.json"
    h "round 16: a Read() deny inside an allowWrite tree still refuses an MCP read" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$bh"'/repos/w","tool_input":{"source":"'"$bh"'/.cargo/credentials.toml"}}' HOME="$bh"
    # REVIEW ROUND 17. Only the HOME-base reading of free text is a guess: a relative climb from the
    # project dir is what jkb's ingest_path opens. A `~name/` string's home reading is judged; a
    # directory holding a must-deny entry is refused to a walker; a padded over-long path still meets
    # the boundary.
    mkdir -p "$bh/.ssh"; : >"$bh/.ssh/id_rsa"
    h "round 17: a relative climb from the project into denyRead is refused to an MCP server" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$bh"'/repos/w","tool_input":{"source":"../../.ssh/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 17: ...and into a credential under an allowWrite root" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$bh"'/repos/w","tool_input":{"source":"../../.cargo/credentials.toml"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 17: a ~name/ path is judged at the home it names" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$bh"'/repos/w","tool_input":{"source":"~vscode/.cargo/credentials.toml"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 17: a walker handed a directory holding a credential is refused" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$bh"'/repos/w","tool_input":{"source":"'"$bh"'/.cargo"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    # Over PATH_MAX a path field is refused before it is walked, however it is padded.
    lrel="$(jqh -cn --arg h "$bh" '{tool_name:"mcp__jkb__ingest_path", cwd:($h + "/repos/w"), tool_input:{source:(([range(2100)|"./"]|join("")) + "../../.ssh/id_rsa")}}')"
    h "round 18: an over-long relative climb is refused" deny "$lrel" HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    ln -s "$bh/.ssh" "$bh/repos/w/k"
    padl="$(jqh -cn --arg h "$bh" '{tool_name:"mcp__jkb__ingest_path", cwd:($h + "/repos/w"), tool_input:{source:("k" + ([range(2100)|"/."]|join("")) + "/id_rsa")}}')"
    h "round 19: a cwd link padded with ./ past PATH_MAX is refused" deny "$padl" HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    # Prose in a jkb tool that takes no path is not judged at all.
    h "round 18: prose in a pathless jkb tool is allowed, ../ and all" allow \
      '{"tool_name":"mcp__jkb__task_create","cwd":"'"$bh"'/repos/w","tool_input":{"title":"t","description":"see ../docs and crates/a.rs\n///\n~ and \u0000"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 19: jkb's own ~\"term\" ns:a/b query is allowed" allow \
      '{"tool_name":"mcp__jkb__query","cwd":"'"$bh"'/repos/w","tool_input":{"query":"~\"merge conflict\" ns:repos/jkb"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    # REVIEW ROUND 20. The literal cwd reading of a `~`-led path is judged in full; a climb after a link
    # is held to the boundary where a normalising server lands, not only where the kernel does.
    ln -s "$bh/.ssh" "$bh/repos/w/~+"; ln -s "$bh/.ssh" "$bh/repos/w/~t"
    mkdir -p "$bh/repos/w/a/b"; ln -s "$bh/repos/w/a/b" "$bh/repos/w/l m"
    h "round 20: a cwd link named ~+ is followed and judged" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$bh"'/repos/w","tool_input":{"source":"~+/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 20: a climb after a link, behind a space, meets the boundary at its lexical landing" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$bh"'/repos/w","tool_input":{"source":"l m/../../../.ssh/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 20: ./~t through a cwd link is denied" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"'"$bh"'/repos/w","tool_input":{"source":"./~t/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    # ingest_url renders a file: URL from disk, so that one form of its source is a path field.
    h "round 27: ingest_url with a file: URL into denyRead is refused" deny \
      '{"tool_name":"mcp__jkb__ingest_url","cwd":"'"$bh"'/repos/w","tool_input":{"source":"file://'"$bh"'/.ssh/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: ...while an https URL is not a path" allow \
      '{"tool_name":"mcp__jkb__ingest_url","cwd":"'"$bh"'/repos/w","tool_input":{"source":"https://example.com/'"$bh"'/.ssh/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    # REVIEW ROUND 26, AND THE USER'S CHOICE AFTER IT: THE FIELD TABLE. A listed tool is judged on its
    # path fields only.
    # THE USER'S CHOICE AFTER ROUND 27: an unlisted tool is NOT refused. Its fields whose NAMES say they
    # are paths are judged; everything else passes. Refusing it broke StructuredOutput, which every
    # schema agent must call, and every connector.
    h "round 27: an unlisted MCP tool with no path-named field is allowed" allow \
      '{"tool_name":"mcp__claude_ai_Docs__batch","cwd":"'"$bh"'/repos/w","tool_input":{"q":"see '"$bh"'/.ssh/id_rsa","text":"../../x"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: ...while its path-named field is judged" deny \
      '{"tool_name":"mcp__fs__read_file","cwd":"'"$bh"'/repos/w","tool_input":{"path":"'"$bh"'/.ssh/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: ...however deep, and in an array" deny \
      '{"tool_name":"mcp__fs__read_many","cwd":"'"$bh"'/repos/w","tool_input":{"opts":{"paths":["'"$bh"'/repos/w/a","'"$bh"'/.ssh/id_rsa"]}}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: ...and a file: URL in a uri field" deny \
      '{"tool_name":"mcp__x__open","cwd":"/h/repos/jkb","tool_input":{"uri":"file:///h/.claude/projects/-s/e.jsonl"}}' HOME=/h
    h "round 27: ...while an https url field resolves to a harmless relative path" allow \
      '{"tool_name":"mcp__x__fetch","cwd":"'"$bh"'/repos/w","tool_input":{"url":"https://example.com'"$bh"'/.ssh/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    ln -s "$bh/.ssh" "$bh/repos/w/x:"
    h "round 28: an unlisted tool's x:// path through a cwd link named x: is judged" deny \
      '{"tool_name":"mcp__fs__read_file","cwd":"'"$bh"'/repos/w","tool_input":{"path":"x://id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 28: a claude.ai connector's path field is not a local path" allow \
      '{"tool_name":"mcp__claude_ai_Drive__list","cwd":"'"$bh"'/repos/w","tool_input":{"path":"/"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    ln -s "$bh/.ssh" "$bh/repos/w/https:"
    h "round 29: an https:// value through a cwd link named https: is judged" deny \
      '{"tool_name":"mcp__fs__read_file","cwd":"'"$bh"'/repos/w","tool_input":{"path":"https://id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: StructuredOutput passes, whatever its findings name" allow \
      '{"tool_name":"StructuredOutput","cwd":"'"$bh"'/repos/w","tool_input":{"findings":[{"file":"'"$bh"'/.ssh/id_rsa","summary":"x"}]}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: an unknown built-in with no path-named field is allowed" allow \
      '{"tool_name":"FutureTool","cwd":"'"$bh"'/repos/w","tool_input":{"note":"'"$bh"'/.ssh/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: EnterWorktree's path is judged" deny \
      '{"tool_name":"EnterWorktree","cwd":"'"$bh"'/repos/w","tool_input":{"path":"'"$bh"'/.ssh"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: a pathless built-in in the table is allowed" allow \
      '{"tool_name":"WebFetch","cwd":"'"$bh"'/repos/w","tool_input":{"url":"https://example.com","prompt":"read '"$bh"'/.ssh/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: Workflow's scriptPath is a path field" deny \
      '{"tool_name":"Workflow","cwd":"'"$bh"'/repos/w","tool_input":{"scriptPath":"'"$bh"'/.ssh/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: Artifact's {from} source is a path field" deny \
      '{"tool_name":"Artifact","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/repos/w/p.html","files":{"k":{"from":"'"$bh"'/.ssh/id_rsa"}}}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: ...while another artifact's published file is not a local one" allow \
      '{"tool_name":"Artifact","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/repos/w/p.html","files":{"k":{"artifact":"https://claude.ai/artifact/x","path":"'"$bh"'/.ssh/id_rsa"}}}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 27: Artifact's files are judged against its root too" deny \
      '{"tool_name":"Artifact","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/repos/w/p.html","root":"'"$bh"'","files":{"k":".ssh/id_rsa"}}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    # FROM A COPY: the installed hook ignores both test seams by its own path, so run from there (as
    # verify.sh runs it) these rows failed and verify.sh reported the hook broken (review round 27).
    dlc="$bh/dt-deadline.sh"; cp "$self" "$dlc" && chmod +x "$dlc"; dself="$self"; self="$dlc"
    t0=$(date +%s%N)
    h "round 27: a judge that runs past the deadline is refused, never let through" deny \
      '{"tool_name":"Read","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/repos/w/x.rs"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w" DT_SELFTEST_DEADLINE=1 DT_SELFTEST_SLOW=5
    case "$t0" in *N) t1=0 ;; *) t1=$(( ($(date +%s%N) - t0) / 1000000 )) ;; esac
    if [ "$t1" -lt 3000 ]; then printf '  \033[32mok\033[0m   ...at the deadline, in %sms\n' "$t1"
    else printf '  \033[31mFAIL\033[0m ...but it took %sms: the child was not killed at the deadline\n' "$t1"; fails=$((fails+1)); fi
    h "round 27: ...while the same call under the deadline is allowed" allow \
      '{"tool_name":"Read","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/repos/w/x.rs"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    self="$dself"
    h "round 24: a Skill's args are prose, and a ../ in them is not a path" allow \
      '{"tool_name":"Skill","cwd":"'"$bh"'/repos/w","tool_input":{"skill":"review","args":"see\n../docs/x.md\nfor context"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    me="$(jqh -cn --arg h "$bh" '{tool_name:"MultiEdit", cwd:($h + "/repos/w"), tool_input:{file_path:($h + "/repos/w/a.rs"), edits:[{old_string:"x", new_string:"/// doc\n///\nfn a() {}"}]}}')"
    h "round 25: a MultiEdit's code is not free text" allow "$me" HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    me="$(jqh -cn --arg h "$bh" '{tool_name:"MultiEdit", cwd:($h + "/repos/w"), tool_input:{file_path:($h + "/.bashrc"), edits:[{old_string:"x", new_string:"y"}]}}')"
    h "round 25: ...while its file_path is still held to the boundary" deny "$me" HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    mkdir -p "$bh/repos/w/.claude"
    h "round 16: the project's own settings layer is not writable through the boundary" deny \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/repos/w/.claude/settings.local.json","content":"{}"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    printf '%s\n' '{"sandbox":{"enabled":false}}' > "$bh/repos/w/.claude/settings.local.json"
    h "round 29: a local layer cannot turn the boundary off; only the image's managed layers can" deny \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.bashrc","content":""}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    rm -f "$bh/repos/w/.claude/settings.local.json"
    # REVIEW ROUND 30: ...nor widen it. A planted local allowWrite/allowRead of "/" let every path through.
    printf '%s\n' '{"sandbox":{"filesystem":{"allowWrite":["/"],"allowRead":["/"]}}}' > "$bh/repos/w/.claude/settings.local.json"
    h "round 30: a local layer cannot widen the boundary's allow lists" deny \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.bashrc","content":""}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    h "round 30: ...for reads either" deny \
      '{"tool_name":"Read","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.ssh/id_rsa"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    printf '%s\n' '{"permissions":{"deny":["Read(~/repos/w/secret/**)"]}}' > "$bh/repos/w/.claude/settings.local.json"
    mkdir -p "$bh/repos/w/secret"; : > "$bh/repos/w/secret/k"
    h "round 30: ...while its Read() deny still narrows it" deny \
      '{"tool_name":"Read","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/repos/w/secret/k"}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w"
    rm -f "$bh/repos/w/.claude/settings.local.json"
    # REVIEW ROUND 31: ...nor through the environment a layer's `env` sets. TMPDIR=/ made every path a
    # write root; a forged HOME or CLAUDE_CONFIG_DIR pointed the trusted user layer at an agent's file.
    h "round 31: a TMPDIR of / is not a write root" deny \
      '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"/opt/evil","content":""}}' HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w" TMPDIR=/
    mkdir -p "$bh/forged/.claude"
    printf '%s\n' '{"sandbox":{"enabled":true,"filesystem":{"allowWrite":["/"],"allowRead":["/"]}}}' > "$bh/forged/.claude/settings.json"
    aic="$bh/dt-as-installed.sh"; cp "$self" "$aic" && chmod +x "$aic"
    # A SCRATCH MANAGED LAYER enables the sandbox for these rows, so they run on any machine -- on CI,
    # with no user settings, they skipped, and reverting round 31 stayed green (review round 32).
    mkdir -p "$bh/aimgd"; printf '%s\n' '{"sandbox":{"enabled":true}}' > "$bh/aimgd/managed-settings.json"
    acct3="$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6)"
    ai() { printf '%s' "$1" | env -u CLAUDE_PROJECT_DIR -u CLAUDE_CONFIG_DIR DT_SELFTEST_AS_INSTALLED=1 DT_SELFTEST_MANAGED_DIR="$bh/aimgd" "${@:2}" "$BASH" "$aic" 2>/dev/null; }
    aiw='{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"/opt/evil","content":""}}'
    if [ -n "$acct3" ]; then
        case "$(ai "$aiw")" in
            *'"permissionDecision":"deny"'*)
                for aiv in "HOME=$bh/forged" "CLAUDE_CONFIG_DIR=$bh/forged/.claude"; do
                    case "$(ai "$aiw" "$aiv")" in *'"permissionDecision":"deny"'*) printf '  \033[32mok\033[0m   round 31: installed, a forged %s does not move the trusted user layer\n' "${aiv%%=*}" ;;
                        *) printf '  \033[31mFAIL\033[0m round 31: installed, a forged %s moved the trusted user layer\n' "${aiv%%=*}"; fails=$((fails+1)) ;; esac
                done
                # ...nor make an exception root of it: a forged home whose .claude/projects links to the
                # real ~/.claude made all of it "memory", and writable (round 32).
                mkdir -p "$bh/forged2/.claude"; ln -sfn "$acct3/.claude" "$bh/forged2/.claude/projects"
                aim='{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$acct3"'/.claude/commands/memory/x.md","content":""}}'
                case "$(ai "$aim" "HOME=$bh/forged2")" in *'"permissionDecision":"deny"'*) printf '  \033[32mok\033[0m   round 32: installed, a forged home linked into ~/.claude is no memory exception\n' ;;
                    *) printf '  \033[31mFAIL\033[0m round 32: installed, a forged home linked into ~/.claude made it writable as memory\n'; fails=$((fails+1)) ;; esac
                case "$(ai "$aiw" "CLAUDE_PROJECT_DIR=/")" in *'"permissionDecision":"deny"'*) printf '  \033[32mok\033[0m   round 32: a CLAUDE_PROJECT_DIR of / is not a write root\n' ;;
                    *) printf '  \033[31mFAIL\033[0m round 32: a CLAUDE_PROJECT_DIR of / made every path writable\n'; fails=$((fails+1)) ;; esac
                # ...nor one inside the home that is no ancestor of the cwd (round 33: only the inside-home
                # half of the bound was held by a row).
                aic2='{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$acct3"'/.claude/CLAUDE.md","content":""}}'
                case "$(ai "$aic2" "CLAUDE_PROJECT_DIR=$acct3/.claude")" in *'"permissionDecision":"deny"'*) printf '  \033[32mok\033[0m   round 33: a CLAUDE_PROJECT_DIR inside the home but not above the cwd is not a write root\n' ;;
                    *) printf '  \033[31mFAIL\033[0m round 33: a CLAUDE_PROJECT_DIR that is no ancestor of the cwd became a write root\n'; fails=$((fails+1)) ;; esac ;;
            *) printf '  \033[31mFAIL\033[0m round 31: with the sandbox enabled by a scratch managed layer, /opt was still writable\n'; fails=$((fails+1)) ;;
        esac
        # A relative entry resolves against the VALIDATED project dir: against the raw one,
        # CLAUDE_PROJECT_DIR=/ turned a trusted layer's `etc` into /etc (round 33).
        mkdir -p "$bh/aimgd2"; printf '%s\n' '{"sandbox":{"enabled":true,"filesystem":{"allowWrite":["etc"]}}}' > "$bh/aimgd2/managed-settings.json"
        aie='{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"/etc/evil","content":""}}'
        case "$(printf '%s' "$aie" | env -u CLAUDE_PROJECT_DIR -u CLAUDE_CONFIG_DIR DT_SELFTEST_AS_INSTALLED=1 DT_SELFTEST_MANAGED_DIR="$bh/aimgd2" CLAUDE_PROJECT_DIR=/ "$BASH" "$aic" 2>/dev/null)" in
            *'"permissionDecision":"deny"'*) printf '  \033[32mok\033[0m   round 33: a relative allowWrite entry is not resolved against a forged project dir\n' ;;
            *) printf '  \033[31mFAIL\033[0m round 33: a relative allowWrite entry resolved against CLAUDE_PROJECT_DIR=/\n'; fails=$((fails+1)) ;; esac
    else
        printf '  \033[33mskip\033[0m round 31/32: no passwd entry for this uid, so the installed home cannot be exercised\n'
    fi
    # REVIEW ROUND 17: MANAGED WINS for `enabled`, watched failing under the old order.
    mkdir -p "$bh/managed"
    printf '%s\n' '{"sandbox":{"enabled":true}}' > "$bh/managed/managed-settings.json"
    printf '%s\n' '{"sandbox":{"enabled":false}}' > "$bh/repos/w/.claude/settings.local.json"
    # RUN FROM A COPY: the installed hook ignores the managed-directory override by design, and
    # verify.sh runs THIS self-test from the installed path, where these rows failed on every start
    # and hid every probe after them (review round 18). A copy elsewhere honours it.
    sbq_self="$(cd "$(dirname "$self")" && pwd)/$(basename "$self")"
    dtc="$bh/dt-copy.sh"; cp "$sbq_self" "$dtc"
    out="$(printf '%s' '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.bashrc","content":""}}' \
           | env -u CLAUDE_CONFIG_DIR HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w" DT_SELFTEST_MANAGED_DIR="$bh/managed" "$BASH" "$dtc" 2>/dev/null)"
    case "$out" in *'"permissionDecision":"deny"'*) printf '  \033[32mok\033[0m   round 17: managed enabling beats a local disable\n' ;;
        *) printf '  \033[31mFAIL\033[0m round 17: managed enabling beats a local disable\n'; fails=$((fails+1)) ;; esac
    sbq="$(cd "$bh/repos/w" && env -u CLAUDE_CONFIG_DIR HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w" DT_SELFTEST_MANAGED_DIR="$bh/managed" "$BASH" "$dtc" --sandbox-enabled 2>/dev/null)"
    if [ "$sbq" = 1 ]; then printf '  \033[32mok\033[0m   round 17: --sandbox-enabled prints the merged answer (1)\n'
    else printf '  \033[31mFAIL\033[0m round 17: --sandbox-enabled printed [%s]\n' "$sbq"; fails=$((fails+1)); fi
    # ...and managed may turn it OFF, whatever a local layer says (round 29).
    printf '%s\n' '{"sandbox":{"enabled":false}}' > "$bh/managed/managed-settings.json"
    printf '%s\n' '{"sandbox":{"enabled":true}}' > "$bh/repos/w/.claude/settings.local.json"
    out="$(printf '%s' '{"tool_name":"Write","cwd":"'"$bh"'/repos/w","tool_input":{"file_path":"'"$bh"'/.bashrc","content":""}}' \
           | env -u CLAUDE_CONFIG_DIR HOME="$bh" CLAUDE_PROJECT_DIR="$bh/repos/w" DT_SELFTEST_MANAGED_DIR="$bh/managed" "$BASH" "$dtc" 2>/dev/null)"
    case "$out" in *'"permissionDecision":"deny"'*) printf '  \033[31mFAIL\033[0m round 29: managed disabling wins over a local enable\n'; fails=$((fails+1)) ;;
        *) printf '  \033[32mok\033[0m   round 29: managed disabling wins over a local enable\n' ;; esac
    rm -f "$bh/repos/w/.claude/settings.local.json"; rm -rf "$bh/managed"
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

    # ANY TOOL THAT CAN NAME A PATH. MCP tools run unsandboxed; a listed tool's path fields are judged,
    # and an unlisted tool's path-named fields (the field table, review round 27).
    h "an MCP tool given a transcript path is denied" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"/h/repos/jkb","tool_input":{"source":"/h/.claude/projects/-s/e.jsonl"}}' HOME=/h
    h "...or as an ancestor that a server would walk" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"/h/repos/jkb","tool_input":{"source":"/h"}}' HOME=/h
    h "an MCP tool with ordinary arguments is allowed" allow \
      '{"tool_name":"mcp__jkb__search","cwd":"/h/repos/jkb","tool_input":{"query":"hello world","limit":5}}' HOME=/h
    h "MultiEdit into the tree is denied" deny \
      '{"tool_name":"MultiEdit","cwd":"/h/repos/jkb","tool_input":{"file_path":"/h/.claude/projects/-s/e.jsonl","edits":[]}}' HOME=/h
    h "a Write whose CONTENT mentions the tree is allowed -- content is not a path" allow \
      '{"tool_name":"Write","cwd":"/h/repos/jkb","tool_input":{"file_path":"/h/repos/jkb/notes.md","content":"/h/.claude/projects/-s/e.jsonl"}}' HOME=/h
    many="$(jqh -cn '{tool_name:"TodoWrite", cwd:"/h/repos/jkb", tool_input:{todos:[range(150) | {content:"do thing \(.)", status:"pending"}]}}')"
    h "a TodoWrite with a long list -- no path-like strings -- is allowed" allow "$many" HOME=/h
    # EVERY TOOL reaches the hook now; Artifact's path fields are in the table.
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
    # REVIEW ROUND 6. Text that starts with `~` is not a home; a file:// URI is its path.
    h "jkb's own vector-search syntax is not a path" allow \
      '{"tool_name":"mcp__jkb__search","cwd":"/h/repos/jkb","tool_input":{"query":"~\"how does sync work\" kind:task"}}' HOME=/h
    h "a ~2h estimate is not a path" allow \
      '{"tool_name":"TaskCreate","cwd":"/h/repos/jkb","tool_input":{"description":"~2h of work"}}' HOME=/h
    h "a file:// URI to a transcript is denied" deny \
      '{"tool_name":"mcp__jkb__ingest_url","cwd":"/h/repos/jkb","tool_input":{"source":"file:///h/.claude/projects/-s/e.jsonl"}}' HOME=/h
    h "a percent-escaped file URI is refused, not decoded" deny \
      '{"tool_name":"mcp__jkb__ingest_url","cwd":"/h/repos/jkb","tool_input":{"source":"file:///h/%2eclaude/projects/-s/e.jsonl"}}' HOME=/h
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
      '{"tool_name":"mcp__jkb__ingest_url","cwd":"/h/repos/jkb","tool_input":{"source":"FILE:///h/.claude/projects/-s/e.jsonl"}}' HOME=/h
    lfile="$(jqh -cn '{tool_name:"mcp__jkb__ingest_url", cwd:"/h/repos/jkb", tool_input:{source:("file:///" + ([range(1500) | "a/.."] | join("/")) + "/h/.claude/projects/-s/e.jsonl")}}')"
    h "a long file:// chain that collapses into the tree is denied" deny "$lfile" HOME=/h
    h "a second brace group hiding an absolute path is denied" deny \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"{,}{/h/.claude/projects/**/*.jsonl,x}"}}' HOME=/h
    h "jkb's one-word vector search ~retry is not a home" allow \
      '{"tool_name":"mcp__jkb__query","cwd":"/h/repos/jkb","tool_input":{"dsl":"~retry"}}' HOME=/h
    h "a Glob whose .. is in the literal prefix is allowed" allow \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb/crates","tool_input":{"pattern":"../docs/*.md"}}' HOME=/h
    h "a brace of relative multi-segment paths is allowed" allow \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"{crates/jkb-core,crates/jkb-cli}/**/*.rs"}}' HOME=/h
    h "an ordinary brace pattern is allowed" allow \
      '{"tool_name":"Glob","cwd":"/h/repos/jkb","tool_input":{"pattern":"**/*.{rs,toml}"}}' HOME=/h
    # A bare ~name is a home, as a server applying expanduser would read it.
    h "a bare ~name root given to an MCP tool is the home, an ancestor" deny \
      '{"tool_name":"mcp__jkb__ingest_path","cwd":"/h/repos/jkb","tool_input":{"source":"~vscode"}}' HOME=/h
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
    # REVIEW ROUND 9.
    h "a leading brace that never closes is refused, not kept literal" deny \
      '{"tool_name":"Glob","cwd":"/h/repos","tool_input":{"pattern":"x}{{/.,/}./{.,}./.claude/projects/**"}}' HOME=/h
    h "an archived transcript is denied" deny \
      '{"tool_name":"Read","cwd":"/h/repos/jkb","tool_input":{"file_path":"/h/.claude-state/transcript-archive/-s/e.jsonl"}}' HOME=/h
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

# A DEADLINE THAT REFUSES (review round 27, from a structural review of why 26 rounds had not
# converged). Claude Code kills a hook at its timeout -- 10s, managed-settings.json -- and then lets the
# call through, so every input that could make this slow was a bypass, and rounds 9 to 22 answered
# each with a budget of its own. Instead the judging runs in a child under `timeout -s KILL`, and a
# child that does not finish is a refusal. 8s leaves the parent room inside the 10.
# The self-test shortens the deadline and slows the child, to watch a refusal; both seams are ignored
# in the installed copy, as DT_SELFTEST_MANAGED_DIR is.
if [ "${1:-}" != --judge ] && [ "${1:-}" != --sandbox-enabled ]; then
    dt_deadline=8
    [ -n "${DT_SELFTEST_DEADLINE:-}" ] && [ "$dt_installed" = 0 ] && dt_deadline="$DT_SELFTEST_DEADLINE"
    dt_rc=0; /usr/bin/timeout -s KILL "$dt_deadline" /bin/bash -p "$0" --judge || dt_rc=$?
    case "$dt_rc" in
        0|2) decided=child; exit "$dt_rc" ;;
        124|137) deny "The file-tool boundary could not decide this call within its ${dt_deadline}s deadline, so it is refused. A smaller call -- fewer paths, a shorter pattern -- decides in time." ;;
        *) exit 2 ;;
    esac
fi
[ -n "${DT_SELFTEST_SLOW:-}" ] && [ "$dt_installed" = 0 ] && sleep "$DT_SELFTEST_SLOW"

# BASH, AND THE BUILT-INS THAT TAKE NO PATH, ARE DECIDED FIRST -- before roots, realpath, or anything
# else that can fail. Bash is the repair tool, and a container broken in some other way must not
# refuse it too: it is refused now only if jq itself is missing, which verify.sh reports by name.
# The kernel sandbox confines Bash, and its command text is not a path. The pathless built-ins carry
# text, not locations -- a todo list, a question, a subagent's prompt -- and judging their strings as
# paths refused ordinary calls once every tool reached this hook: 40 todos from a home cwd, an empty
# activeForm read as "the home, an ancestor of the tree", a 4.6KB Agent prompt over the byte budget
# (review round 4, all reproduced). A subagent's own tool calls reach this hook in their own right.
# Read without `cat`: every tool call pays for this hook now, so a fork saved here is saved on every
# Bash call. `$(</dev/stdin)`, not `read -d ''`, which reads a pipe a byte at a time: 1s for a 300KB
# payload, before any judging (review round 20). The trailing newlines it drops are not JSON.
# `--sandbox-enabled` asks this hook's own merged answer (1 or 0) for the cwd, so verify.sh does not
# keep a second copy of the layer-precedence rule (review round 17). It runs the same path as a tool
# call, with a synthetic payload, and prints instead of deciding.
sb_query=0
if [ "${1:-}" = --sandbox-enabled ]; then
    sb_query=1
    input="$(printf '{"tool_name":"__sandbox_query__","cwd":%s,"tool_input":{}}' "$(printf '%s' "$PWD" | HOME=/dev/null jq -Rs .)")"
else
    input="$(</dev/stdin)"
fi
command -v jq >/dev/null 2>&1 || exit 3
tool="$(printf '%s' "$input" | jqh -er '.tool_name | strings' 2>/dev/null)" || exit 3
case "$tool" in
    # Skill too (review round 24): its `args` is a slash command's free text, a /review focus that
    # mentioned `../x` was refused as an MCP path, and the skill it names runs through these same tools.
    Bash|TodoWrite|AskUserQuestion|Agent|Task|ToolSearch|SendMessage|Skill) allow ;;
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
# mirror, and the transcript rule above still applies. An agent cannot turn it off or widen it: only
# managed settings may switch it off, and the allow lists come from managed and user settings, which
# are write-denied and Edit-denied. A project's or worktree's own layers -- which sandboxed Bash can
# CREATE in a new worktree -- may narrow it (denies, denyRead) and turn it on, never more (review
# rounds 29 and 30).
sb_on=0; sb_w=(); sb_r=(); sb_dr=(); sb_md=()
# IN PRECEDENCE ORDER, lowest first, because `enabled` takes the last layer's word: user, project,
# local, then managed and its drop-ins, which win in Claude Code. Managed came first, so a lower
# layer's `enabled:false` would have outranked it (review round 16).
# THE BOUNDARY'S HOME IS THE ACCOUNT'S, NOT $HOME (review round 31). A settings layer's `env` reaches
# this hook's environment, and a project or local layer is one sandboxed Bash can create in a new
# worktree: its env HOME or CLAUDE_CONFIG_DIR pointed the "user" layer -- the one trusted to widen the
# boundary -- at a file the agent wrote, and moved every `~` in the lists. So the installed hook finds
# the user layer and expands `~` from the passwd home. A copy elsewhere (the self-test's) keeps $HOME,
# so its rows can use a scratch home; DT_SELFTEST_AS_INSTALLED makes a copy behave as installed, which
# only ever tightens it.
sb_home="$home"; sb_cfg="${CLAUDE_CONFIG_DIR:-$home/.claude}"
sb_roots="$roots"
if { [ "$dt_installed" = 1 ] || [ -n "${DT_SELFTEST_AS_INSTALLED:-}" ]; } && [ -n "$acct_home" ]; then
    sb_home="$(normalise "$acct_home")"; sb_cfg="$sb_home/.claude"
    # ...and the roots its EXCEPTIONS come from (auto-memory, saved tool output), which ALLOW: built from
    # $HOME or CLAUDE_CONFIG_DIR, a forged home whose .claude/projects linked to ~/.claude made all of
    # ~/.claude a "memory" directory, writable (review round 32). The transcript rule may still use
    # every spelling: a root there only denies.
    sb_rl=("$sb_home/.claude/projects" "$sb_home/.claude-state/projects" "$sb_home/.claude-state/transcript-archive")
    sb_roots="$(printf '%s\n' "${sb_rl[@]}" "$(realpath -m -- "${sb_rl[@]}" 2>/dev/null)" | awk 'NF && !seen[$0]++')"
fi
sb_layers=("$sb_cfg/settings.json")
[ -n "${CLAUDE_PROJECT_DIR:-}" ] && sb_layers+=("$CLAUDE_PROJECT_DIR/.claude/settings.json" "$CLAUDE_PROJECT_DIR/.claude/settings.local.json")
# THE MANAGED DIRECTORY is fixed, except to this file's own --self-test, which points it at a scratch
# directory to watch managed precedence fire (review round 17). The INSTALLED copy never honours the
# override, whatever its environment holds -- a settings `env` must not be able to move it.
sb_mdir=/etc/claude-code
[ -n "${DT_SELFTEST_MANAGED_DIR:-}" ] && [ "$dt_installed" = 0 ] && sb_mdir="$DT_SELFTEST_MANAGED_DIR"
sb_nm_layers=${#sb_layers[@]}
sb_layers+=("$sb_mdir/managed-settings.json" "$sb_mdir"/managed-settings.d/*.json)
sb_args=(); sb_names=()
sb_i=0; sb_nm=0; sb_k=0; sb_untrusted=""; sb_sep=""
for f in "${sb_layers[@]}"; do
    sb_k=$((sb_k + 1))
    [ -f "$f" ] && [ -r "$f" ] || continue
    # THE PROJECT AND LOCAL LAYERS are files under the workspace, which sandboxed Bash can CREATE in a
    # new worktree (round 29). They may narrow the boundary, never widen it (below).
    if [ -n "${CLAUDE_PROJECT_DIR:-}" ] && { [ "$sb_k" -eq 2 ] || [ "$sb_k" -eq 3 ]; }; then
        sb_untrusted="$sb_untrusted$sb_sep$sb_i"; sb_sep=,
    fi
    sb_args+=(--rawfile "s$sb_i" "$f"); sb_names+=("\$s$sb_i"); sb_i=$((sb_i + 1))
    [ "$sb_k" -le "$sb_nm_layers" ] && sb_nm=$sb_i
done
sb_untrusted="[$sb_untrusted]"
if [ "$sb_i" -gt 0 ]; then
    # ONE jq, every layer as raw text parsed inside: a file that is not valid JSON contributes nothing,
    # as Claude Code skips it, instead of failing the whole read. `enabled` is the last layer's word
    # in the order read (user, project, local, managed last); the lists are unions, as Claude Code
    # merges arrays across layers -- except that allowWrite and allowRead skip the project and local
    # layers ($ut): a planted local allowWrite of "/" let every path through (review round 30).
    sb_names_csv="$(IFS=,; printf '%s' "${sb_names[*]}")"
    # ONLY THE IMAGE'S LAYERS MAY TURN IT OFF (review round 29). Managed settings and their drop-ins have
    # the last word when they set `enabled`; otherwise any layer may turn the boundary ON and none may
    # turn it off. A user, project or local layer's `enabled:false` won before, and sandboxed Bash can
    # CREATE a worktree's settings.local.json, which the ~/repos Edit rules cover only once it exists.
    # Claude Code itself would still start that session's Bash unsandboxed; pinning `enabled` in the
    # image's managed settings closes that, and is the user's decision (README).
    sb_sh="$(jqh -nr --argjson nm "$sb_nm" --argjson ut "$sb_untrusted" "${sb_args[@]}" '
        def p: try fromjson catch {};
        def str: if type == "string" then . else empty end;
        [ ('"$sb_names_csv"') | p | .sandbox // {} ] as $l
        | ([ $l[$nm:][] | .enabled | select(. != null) ] | last) as $mon
        | (if $mon != null then $mon else ([ $l[:$nm][] | .enabled == true ] | any) end) as $on
        | def dpath: if type == "string" then (capture("^Read\\((?<p>.*)\\)$").p // empty) else empty end;
          @sh "sb_on=\(if $on == true then 1 else 0 end)
               sb_md=(\([ ( [ '"$sb_names_csv"' ] | .[] | p | .permissions.deny[]? | dpath ),
                           ( $l[] | .credentials.files[]? | select(.mode == "deny") | .path | str ) ] | unique))
               sb_w=(\([ $l | to_entries[] | select(.key as $k | $ut | index($k) | not) | .value.filesystem.allowWrite[]? | str ] | unique))
               sb_r=(\([ $l | to_entries[] | select(.key as $k | $ut | index($k) | not) | .value.filesystem.allowRead[]? | str ] | unique))
               sb_dr=(\([ $l[] | .filesystem.denyRead[]? | str ] | unique))"' 2>/dev/null)" || exit 3
    eval "$sb_sh"
fi
if [ "$sb_on" = 1 ]; then
    # MUST-DENY entries (permissions.deny `Read(...)`, sandbox.credentials.files with mode deny) are
    # patterns: `//abs`, `~/x`, globs. Normalised to absolute here and matched as case patterns, so
    # `*` crosses `/`, the direction that errs towards refusing. A relative one is skipped: it is
    # Claude Code's to resolve against a settings file, and the native tools still enforce it.
    sb_md_abs=()
    for e in ${sb_md[@]+"${sb_md[@]}"}; do
        # `//x` is absolute and `/x` is RELATIVE to its settings file in Claude Code's rule syntax, so
        # a single-slash entry is skipped with the other relative ones (review round 17).
        case "$e" in "//"*) sb_md_abs+=("/${e#//}") ;; "~") sb_md_abs+=("$sb_home") ;; "~/"*) sb_md_abs+=("$sb_home/${e#\~/}") ;; esac
    done
    # `~` is the home; a relative entry (".") is the project's, as Claude Code reads it. Then every
    # entry resolved the way the path it is compared with is: physically.
    sb_abs() { case "$1" in "~") printf '%s\n' "$sb_home" ;; "~/"*) printf '%s\n' "$sb_home/${1#\~/}" ;; /*) printf '%s\n' "$1" ;;
               *) printf '%s\n' "${sb_proj:-$cwd}/${1#./}" ;; esac; }
    # Claude Code's own writable places, which the Bash sandbox also grants: the session's cwd and
    # project, the temp roots it hands tools, and ~/.claude/plans, where plan mode writes.
    # TMPDIR only where a temp root can be: a settings `env` set it to `/`, and every path became a write
    # root (review round 31).
    sb_w+=("$cwd" "/tmp/claude" "/tmp/claude-$UID" "~/.claude/plans")
    # CLAUDE_PROJECT_DIR only where a project can be: the cwd or an ancestor of it, strictly inside the
    # home. Whether a settings `env` can override the value Claude Code hands its hooks is unmeasured,
    # and set to `/` every path became a write root (review round 32), as TMPDIR's did in round 31.
    # The VALIDATED project dir is also the base a relative list entry (`.`, `etc`) resolves against:
    # resolved against the raw variable, CLAUDE_PROJECT_DIR=/ turned a trusted layer's `etc` into /etc
    # (review round 33). The raw value may still add a denial, never a location.
    sb_proj="$cwd"
    if [ -n "${CLAUDE_PROJECT_DIR:-}" ]; then
        sb_pd="$(realpath -m -- "$CLAUDE_PROJECT_DIR" 2>/dev/null)" || sb_pd=""
        sb_cw="$(realpath -m -- "$cwd" 2>/dev/null)" || sb_cw=""
        if [ -n "$sb_pd" ] && under "$sb_pd" "$(realpath -m -- "$sb_home" 2>/dev/null)" \
           && { [ "$sb_pd" = "$sb_cw" ] || under "$sb_cw" "$sb_pd"; }; then
            sb_w+=("$sb_pd"); sb_proj="$sb_pd"
        fi
    fi
    sb_tmp="$(realpath -m -- "${TMPDIR:-/tmp}" 2>/dev/null)" || sb_tmp=""
    case "$sb_tmp" in /tmp|/tmp/*) sb_w+=("$sb_tmp") ;; esac
    sb_resolve() { # sb_resolve <name of array> -- each entry, absolute and physical, in place
        local -n arr="$1"; local e out=()
        for e in "${arr[@]}"; do out+=("$(sb_abs "$e")"); done
        [ "${#out[@]}" -gt 0 ] || { arr=(); return 0; }
        # `$( )`, never `< <( )`: a resolver that fails must refuse, not hand back a short list.
        local res; res="$(realpath -m -- "${out[@]}" 2>/dev/null)" || exit 3
        mapfile -t arr <<<"$res"
    }
    sb_resolve sb_w; sb_resolve sb_r; sb_resolve sb_dr
    sb_layers_phys=(); for e in "${sb_layers[@]}"; do sb_layers_phys+=("$(realpath -m -- "$e" 2>/dev/null)"); done
fi
sb_under() { # sb_under <path> <entry>... -> rc 0 when <path> is an entry or lies inside one
    local p="$1" e; shift
    for e in "$@"; do
        [ -n "$e" ] || continue
        [ "$e" = / ] && return 0
        [ "$p" = "$e" ] || under "$p" "${e%/}" && return 0
    done
    return 1
}
# boundary <physical path> -- deny a path the sandbox would not let Bash reach in this tool's mode.
boundary() {
    [ "$sb_on" = 1 ] || return 0
    # AUTO-MEMORY IS THE ONE DELIBERATE DIFFERENCE from what Bash may reach: `<root>/<slug>/memory/`
    # is Claude Code's own memory, which its tools must read and write, linked into ~/.jkb or not.
    # Sandboxed Bash cannot see the tree at all. The transcript rule above draws the same line.
    local r rest e
    # MUST-DENY FIRST: a Read() deny or a credential file inside an allowed tree (~/.cargo holds
    # credentials.toml) was reachable through an MCP tool, because an allow match won before any deny
    # was looked at (review round 16). The native tools enforce these themselves; MCP servers do not.
    for e in ${sb_md_abs[@]+"${sb_md_abs[@]}"}; do
        # shellcheck disable=SC2254
        case "$1" in $e|$e/*) deny "$1 is denied by the Claude settings (a Read() permission deny or a credential file), and the file tools and MCP servers are held to it as the native Read is." ;; esac
        # ...and a READ of a directory that HOLDS one: a walker handed ~/.cargo reads credentials.toml
        # inside it (review round 17). The entry's literal base is compared, so a glob errs to refuse.
        if [ "$sb_mode" = read ]; then
            local eb="${e%%[\*\?\[\{]*}"; eb="${eb%/}"
            [ -n "$eb" ] && under "$eb" "${1%/}" \
                && deny "$1 holds $eb, which the Claude settings deny reading, and a tool rooted here would read it."
        fi
    done
    # THE SETTINGS LAYERS THIS READS are never writable through it: for a project outside ~/repos
    # the cwd is writable, and a Write of its .claude/settings.local.json could have switched the
    # boundary off (review round 16).
    if [ "$sb_mode" = write ]; then
        for e in ${sb_layers_phys[@]+"${sb_layers_phys[@]}"}; do
            [ "$1" = "$e" ] && deny "$1 is a Claude settings file this boundary reads; it is not writable by the tools it confines."
        done
    fi
    while IFS= read -r r; do
        [ -n "$r" ] || continue
        under "$1" "$r" || continue
        rest="${1:${#r}+1}"
        rest="${rest#*/}"
        [ "$rest" = memory ] || [ "${rest#memory/}" != "$rest" ] && return 0
        # Saved tool output, which Claude Code asks the agent to Read, is a read-only exception.
        if [ "$sb_mode" = read ]; then
            rest="${rest#*/}"
            [ "$rest" = tool-results ] || [ "${rest#tool-results/}" != "$rest" ] && return 0
        fi
    done <<<"$sb_roots"
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
if [ "$sb_query" = 1 ]; then printf '%s\n' "$sb_on"; decided=allow; exit 0; fi

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
    # The sandbox's boundary, on the path the kernel would open -- AND on the lexical one, which is
    # what a server that normalises first (path.resolve, normpath) opens: `l m/../../../.ssh/x` with
    # `l m` a link two deep climbs, physically, only to ~/repos, while a normalising server opens
    # ~/.ssh/x (review round 20). Refusing on either reading errs the safe way.
    boundary "$phys"
    [ "$abs" = "$phys" ] || boundary "$abs"
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
            [ $(( ${#brace_out[@]} * ${#pat} )) -le 12288 ] \
                || deny "This Glob pattern's brace expansions are too large to judge in time. Split it into smaller patterns."
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
    # MultiEdit too, as sb_mode above already counts it a write tool: left out here, its code was judged
    # as MCP free text and a `///` doc comment in new_string was refused (review round 25).
    Read|Edit|MultiEdit|Write|NotebookEdit)
        [ -n "$pth" ] && check "$pth" ;;
    *)
        # THE FIELD TABLE (the user's choices after review rounds 26 and 27, on a structural review of
        # why 26 rounds had not converged). A listed tool has its listed path fields judged as Read's
        # file_path is, and a tool listed as pathless is let through. ANY OTHER TOOL has the fields
        # whose NAMES say they are paths judged (file_path, path, paths, file, dir, root, source, uri,
        # ... -- at any depth, string or array), and everything else it carries passes. Rounds 6 to 26
        # judged every string an unknown tool carried as a possible path and could not finish: each
        # round found another reading of free text. A field's name is a much smaller guess than its
        # prose. Refusing unlisted tools outright (round 27) refused StructuredOutput, which every
        # schema agent must call, and every connector, which cannot open a local file at all.
        case "$tool" in
            mcp__jkb__ingest_path) dt_fields='.source' ;;
            # ingest_url renders its URL in a headless browser, which loads a file: URL from disk.
            mcp__jkb__ingest_url) dt_fields='.source | strings | select(test("^file:"; "i"))' ;;
            mcp__jkb__search|mcp__jkb__get_context|mcp__jkb__query|mcp__jkb__list_views|mcp__jkb__run_view|\
            mcp__jkb__task_next|mcp__jkb__task_create|mcp__jkb__task_update) dt_fields='empty' ;;
            # Artifact publishes local files: its page, its supporting files (a map of published path to
            # a source path or {from}, or a list of {path}; an {artifact, path} source is another
            # artifact's published file, not a local one), and asset uploads.
            Artifact) dt_fields='.file_path, .file_paths[]?, .root, .out_dir,
                ((.files // empty) | if type == "array" then .[].path?
                 elif type == "object" then (.[] | if type == "string" then .
                     elif type == "object" and (has("artifact") | not) then .from? else empty end)
                 else empty end)' ;;
            ArtifactData) dt_fields='.file_path' ;;
            Workflow) dt_fields='.scriptPath' ;;
            # Pathless built-ins: they carry text, URLs or ids, or run under the kernel sandbox as Bash
            # does (Monitor, the shell-output tools).
            WebFetch|WebSearch|Monitor|ScheduleWakeup|CronCreate|CronDelete|CronList|TaskStop|TaskOutput|\
            ListAgents|EnterPlanMode|ExitPlanMode|ReportFindings|PushNotification|RemoteTrigger|\
            ArtifactComments|KillShell|BashOutput|TaskCreate|TaskUpdate|TaskList|TaskGet|StructuredOutput|\
            ExitWorktree) dt_fields='empty' ;;
            # claude.ai connectors run on claude.ai and open no local file: a `path` there is a repo's or
            # a drive's, and judged here `path:"/"` was an ancestor of the tree (review round 28).
            mcp__claude_ai_*) dt_fields='empty' ;;
            # Everything else: the path-NAMED fields, at any depth, EVERY value judged as a path. A URL
            # is also the relative path a server that open()s it reads (`x:/...` from its cwd): a link
            # named `x:` reached the tree while schemes were skipped (round 28), and one named `https:`
            # while web schemes still were (round 29). A real URL resolves to a harmless relative path.
            *) dt_fields='.. | objects | to_entries[]
                | select(.key | test("^(file_?path|file_?paths|path|paths|file|files|file_?name|dir|directory|root|cwd|source|target|dest|destination|out_?dir|output_?path|input_?path|uri|url)$"; "i"))
                | .value | (strings, (arrays | .[] | strings))' ;;
        esac
        dt_paths_sh="$(printf '%s' "$input" | jqh -er "[.tool_input | ($dt_fields) | strings | select(. != \"\")] | @sh \"dt_paths=(\\(.))\"" 2>/dev/null)" || exit 3
        dt_paths=(); eval "$dt_paths_sh"
        # Artifact's supporting files are relative to its `root` when one is given.
        dt_root=""
        if [ "$tool" = Artifact ]; then
            dt_root="$(printf '%s' "$input" | jqh -r '.tool_input.root // "" | strings' 2>/dev/null)" || exit 3
            [ -z "$dt_root" ] || dt_root="$(join_raw "$dt_root" "$home" "$cwd")"
        fi
        for p in ${dt_paths[@]+"${dt_paths[@]}"}; do
            case "$p" in *$'\n'*) deny ;; esac
            check "$p"
            # A RELATIVE path is judged from every base the tool may resolve it from: an MCP server
            # resolves against its OWN cwd, and jkb's starts in the project root.
            [ -n "${CLAUDE_PROJECT_DIR:-}" ] && [ "$CLAUDE_PROJECT_DIR" != "$cwd" ] && check "$p" "$CLAUDE_PROJECT_DIR"
            [ -z "$dt_root" ] || check "$p" "$dt_root"
        done ;;
esac
allow
