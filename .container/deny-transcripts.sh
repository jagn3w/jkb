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
# was not, because naming a path in a deny rule is what EXPOSES it. managed-settings.json keeps one
# exact-path rule per spelling as the belt to that brace; each is a single argv entry, and neither
# matches a memory path, so verify.sh's memory_shadow stays clear.
#
# FAILS CLOSED, unlike .claude/hooks/block-raw-sqlite.sh, and deliberately. That hook steers an
# agent away from a better tool, so an error there must not wedge Bash. This one is a
# confidentiality boundary: if it cannot tell what is being read, the answer is no. The blast
# radius of failing closed is only this tree -- every path outside it is allowed without the hook
# having to decide anything.
set -uo pipefail

# The two spellings of one tree: ~/.claude/projects is a symlink into the state volume, so the
# same transcript is reachable by either name and a rule that knows only one knows neither.
ROOTS_DEFAULT="$HOME/.claude/projects $HOME/.claude-state/projects"

# Lexical, not `realpath`: the file may not exist yet (a Write), and a resolver that fails on a
# missing path would answer "cannot tell" for exactly the calls that create one. `..` is collapsed
# here because `<slug>/memory/../e1d7.jsonl` is a transcript wearing memory's prefix, and a check
# that compared the raw string would allow it.
normalise() { # normalise <path> -> lexically resolved absolute path
    local p="$1" out=() seg
    case "$p" in /*) ;; *) p="$PWD/$p" ;; esac
    local IFS=/
    for seg in $p; do
        case "$seg" in
            ''|.) ;;
            ..) [ "${#out[@]}" -gt 0 ] && unset 'out[${#out[@]}-1]' ;;
            *) out+=("$seg") ;;
        esac
    done
    printf '/%s' "${out[@]+${out[@]}}" | sed 's|^/$|/|'
    [ "${#out[@]}" -eq 0 ] && printf '/'
    printf '\n'
}

# THE DECISION, pure so --self-test can drive it with literals and no Claude Code.
verdict() { # verdict <path> <roots> -> allow|deny
    local p roots="$2" root
    p="$(normalise "$1")"
    for root in $roots; do
        case "$p/" in
            "$root"/*)
                # Auto-memory is the one child of a slug that is not a transcript, and it must stay
                # readable. Matched as a SEGMENT: `<slug>/memory/...` is memory, `<slug>/memoryX/`
                # is not, and `<slug>/x/memory` is not a store Claude Code would ever read.
                case "$p/" in
                    "$root"/*/memory/*) printf 'allow\n'; return ;;
                esac
                printf 'deny\n'; return ;;
        esac
    done
    printf 'allow\n'
}

if [ "${1:-}" = --self-test ]; then
    fails=0
    R="/h/.claude/projects /h/.claude-state/projects"
    t() { # t <label> <path> <want>
        local got; got="$(verdict "$2" "$R")"
        if [ "$got" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else printf '  \033[31mFAIL\033[0m %s\n         got %s, wanted %s\n' "$1" "$got" "$3"; fails=$((fails+1)); fi
    }
    echo "==> deny-transcripts self-test"
    t "a session transcript is denied"            /h/.claude/projects/-slug/e1d7.jsonl            deny
    t "a subagent transcript is denied"           /h/.claude/projects/-slug/e1d7/subagents/a.jsonl deny
    t "a workflow agent transcript is denied"     /h/.claude/projects/-slug/e1d7/subagents/workflows/wf_1/a.jsonl deny
    t "the state-volume spelling is denied too"   /h/.claude-state/projects/-slug/e1d7.jsonl      deny
    # The whole reason this is a hook and not a glob.
    t "auto-memory is ALLOWED"                    /h/.claude/projects/-slug/memory/MEMORY.md      allow
    t "a memory note is allowed"                  /h/.claude/projects/-slug/memory/foo.md         allow
    t "...in the state-volume spelling too"       /h/.claude-state/projects/-slug/memory/MEMORY.md allow
    # Traversal: a transcript wearing memory's prefix. Allowing this is the whole point of
    # normalising, and a string compare gets it wrong.
    t "a transcript reached through memory/.. is denied" \
      /h/.claude/projects/-slug/memory/../e1d7.jsonl deny
    t "a transcript reached through // is denied" /h/.claude/projects//-slug//e1d7.jsonl          deny
    t "a relative-looking .. inside the tree is denied" \
      /h/.claude/projects/-slug/e1d7/../e1d7.jsonl  deny
    # Near-misses that must NOT be swallowed.
    t "memoryX is not memory"                     /h/.claude/projects/-slug/memoryX/f.md          deny
    t "a repo file is allowed"                    /h/repos/jkb/src/main.rs                        allow
    t "the memory STORE outside the tree is allowed" /h/.jkb/claude-memory/jkb/MEMORY.md          allow
    t "a sibling directory is allowed"            /h/.claude/settings.json                        allow
    t "a path merely containing the root name is allowed" /h/x/.claude/projects-backup/a.jsonl    allow
    if [ "$fails" -eq 0 ]; then printf '\033[32mdeny-transcripts self-test passed\033[0m\n'; exit 0; fi
    printf '\033[31mdeny-transcripts self-test: %s failed\033[0m\n' "$fails"; exit 1
fi

# ---------------------------------------------------------------------------- hook mode
input="$(cat 2>/dev/null)"
# Both spellings of the field: Read/Edit/Write carry `file_path`, Grep/Glob carry `path`. Asking
# for both costs one jq and means a tool added later with either name is covered rather than
# silently exempt.
deny() {
    printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}\n' \
        '"Session transcripts are not readable. They are other agents'"'"' working context, they are not a source of truth for this repository, and reading them is how one session inherits another'"'"'s mistakes. Auto-memory under <slug>/memory/ IS readable, and ~/.jkb/claude-memory holds the shared store. If you need what another session concluded, read the decision record it left in docs/ or the commit message."'
    exit 0
}

jq_rc=0
paths="$(printf '%s' "$input" \
    | jq -r '[.tool_input.file_path?, .tool_input.path?, .tool_input.notebook_path?]
             | map(select(. != null and . != "")) | .[]' 2>/dev/null)" || jq_rc=$?

# THE FAIL-CLOSED ARM, and without it the claim in the header is false. If jq could not parse the
# call, `paths` is empty and the loop below allows -- so a malformed payload that NAMES a
# transcript would be the one input that gets through. There is no path to classify in that state,
# so the raw text is asked instead: it is a cruder question, but it is asked only when the precise
# one could not be, and the cost of a false deny here is one refused tool call with a reason.
if [ "$jq_rc" -ne 0 ] || ! command -v jq >/dev/null 2>&1; then
    for dc_root in $ROOTS_DEFAULT; do
        case "$input" in *"$dc_root"*) deny ;; esac
    done
fi


# NO PATH IS NOT A FAILURE. Most tool calls carry none, and a hook that denied those would block
# every Bash command in the container. The fail-closed rule applies to a path it cannot classify,
# not to a call that names no path.
[ -n "$paths" ] || exit 0

while IFS= read -r p; do
    [ -n "$p" ] || continue
    [ "$(verdict "$p" "$ROOTS_DEFAULT")" = deny ] && deny
done <<<"$paths"
exit 0
