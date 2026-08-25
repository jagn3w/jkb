#!/usr/bin/env bash
# Share Claude Code's per-repo auto-memory between the host and the dev container (design D49).
#
#   ./scripts/link-claude-memory.sh [--dry-run] [--home <dir>] [--repos <dir>] [--store <dir>]
#   ./scripts/link-claude-memory.sh --self-test
#
# THE PROBLEM. Claude Code keeps auto-memory in `~/.claude/projects/<slug>/memory/`, one directory
# per project, where <slug> is the absolute path of the git repo root with every character outside
# [A-Za-z0-9-] replaced by `-`. That key is the ABSOLUTE PATH, so the same repo has two of them:
#
#     host       /Users/you/repos/jkb    ->  -Users-you-repos-jkb
#     container  /home/vscode/repos/jkb  ->  -home-vscode-repos-jkb
#
# Mounting all of ~/repos does not change that — the slug is derived from where the repo is, not
# from what is in it — so the two sides key to different directories and neither can see what the
# other learned.
#
# WHY NOT MOUNT IT. The obvious fix is a bind from the host's memory directory to the container's,
# and it is the one mount this whole design forbids: `~/.claude` also holds `settings.json`, which
# IS the posture, and a process the posture bounds must not read the file deciding whether it is
# bounded. check-config.sh refuses such a mount, verify.sh asserts against it, and
# mutate-verify.sh watches both fire. It would also collide with `dc_link_state`, which replaces
# ~/.claude/projects with a symlink into the state volume, and it would need `initializeCommand`
# to resolve a slug that no devcontainer.json substitution can express.
#
# WHAT THIS DOES INSTEAD. `~/.jkb` is ALREADY bind-mounted into the container, already declared and
# already reviewed, so it costs no boundary change at all: the shared store is
# `~/.jkb/claude-memory/<repo>/`, and each side symlinks its own slug's `memory` directory at it.
# Run it on the host once and in the container on every create; both sides derive the repo list by
# enumerating ~/repos, so a repo added later is picked up by re-running.
#
# STATED PLAINLY, because it is a hole in "the boundary is what you did not mount": memory is
# agent-WRITABLE prose that is injected into context, so a shared store is a channel from
# container sessions into the less-confined host ones. It is the same person's agents at both
# ends and it is prose rather than code, and it travels through a directory that was already
# shared — but it is a channel, and it is opt-in on the host for that reason (`setup.sh
# --link-memory`), never created by a `git pull`.
#
# NOTHING HERE OVERWRITES. An existing memory directory is migrated into the store file by file
# and the directory is then removed; a name that exists on both sides is left alone and reported,
# because picking a winner silently is how memory gets lost. A symlink already pointing somewhere
# else is reported and skipped, never retargeted.
set -uo pipefail

# The slug Claude Code derives from a project's absolute path. INFERRED FROM OBSERVATION, not from
# documentation: `/Users/jagnew/repos/jkb/.jkb/work/mount-all-of-repos-into-the-dev` is keyed
# `-Users-jagnew-repos-jkb--jkb-work-mount-all-of-repos-into-the-dev`, which fixes `/` and `.`
# and says nothing about the rest, so everything outside [A-Za-z0-9-] is mapped the same way.
# Getting it wrong for some repo name fails in the harmless direction: the link is made at a path
# Claude Code never reads, so that repo's memory is simply not shared and nothing is lost.
slugify() { printf '%s' "${1//[!A-Za-z0-9-]/-}"; }

home="$HOME"
repos=""
store=""
dry=0
self_test=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run)   dry=1 ;;
        --self-test) self_test=1 ;;
        # So verify.sh can find the link it asserts without keeping a second copy of the slug
        # rule. Two spellings of one derivation is the duplication .devcontainer/lib.sh exists
        # to remove, and this one is a guess about another program's encoding — the worst kind
        # to have two of.
        --print-slug) shift; slugify "${1:-}"; printf '\n'; exit 0 ;;
        --home)      shift; home="${1:-}" ;;
        --repos)     shift; repos="${1:-}" ;;
        --store)     shift; store="${1:-}" ;;
        -h|--help)   sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)           echo "unknown flag: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done

note() { printf '  %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*" >&2; }


# One repo. Kept separate from the loop so the self-test can drive it directly.
link_one() { # link_one <projects-dir> <store-dir> <repo-path>
    local projects="$1" store_dir="$2" path="${3%/}"
    local key slug link dest
    key="$(basename "$path")"
    slug="$(slugify "$path")"
    link="$projects/$slug/memory"
    dest="$store_dir/$key"

    if [ "$dry" -eq 1 ]; then
        note "would link $link -> $dest"
        return 0
    fi
    mkdir -p "$dest" || { warn "$key: cannot create $dest"; return 1; }

    if [ -L "$link" ]; then
        local current
        current="$(readlink "$link")"
        if [ "$current" = "$dest" ]; then
            note "$key: already linked"
            return 0
        fi
        warn "$key: $link already points at $current — left alone; remove it by hand to relink"
        return 1
    fi

    # A real directory here is memory written before the store existed. Move it in file by file,
    # never over something already in the store: a collision means both sides learned something
    # under one name, and choosing between them is not this script's call.
    if [ -d "$link" ]; then
        local moved=0 kept=0 entry base
        shopt -s dotglob nullglob
        for entry in "$link"/*; do
            base="$(basename "$entry")"
            if [ -e "$dest/$base" ]; then
                kept=$((kept+1))
            elif mv "$entry" "$dest/$base" 2>/dev/null; then
                moved=$((moved+1))
            else
                kept=$((kept+1))
            fi
        done
        shopt -u dotglob nullglob
        if ! rmdir "$link" 2>/dev/null; then
            warn "$key: migrated $moved file(s), but $kept could not be moved — $link is left as it is"
            return 1
        fi
        [ "$moved" -eq 0 ] || note "$key: migrated $moved file(s) into the store"
    elif [ -e "$link" ]; then
        warn "$key: $link exists and is not a directory — left alone"
        return 1
    fi

    mkdir -p "$(dirname "$link")" || { warn "$key: cannot create $(dirname "$link")"; return 1; }
    ln -s "$dest" "$link" || { warn "$key: could not create the link"; return 1; }
    note "$key: linked $link -> $dest"
}

run() { # run <home> <repos> <store>
    local h="$1" r="$2" s="$3" d
    local projects="$h/.claude/projects"
    if [ ! -d "$r" ]; then
        warn "no repos directory at $r — nothing to link"
        return 0
    fi
    for d in "$r"/*/; do
        [ -d "$d" ] || continue
        link_one "$projects" "$s" "$d"
    done
    return 0
}

# --- self-test ---------------------------------------------------------------
# The slug rule is a guess about another program's private encoding and the migration is the only
# step here that can lose something, so both are exercised against a scratch HOME rather than
# argued for. Runs in ./scripts/check.sh: no container, no network, no Docker.
if [ "$self_test" -eq 1 ]; then
    t="$(mktemp -d)"; trap 'rm -rf "$t"' EXIT
    fails=0
    check() { # check <label> <condition-result>
        if [ "$2" = yes ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else printf '  \033[31mFAIL\033[0m %s\n' "$1"; fails=$((fails+1)); fi
    }
    echo "==> link-claude-memory self-test"

    # The one observed slug, pinned. If Claude Code's encoding changes this is where it shows.
    got="$(slugify /Users/jagnew/repos/jkb/.jkb/work/mount-all-of-repos-into-the-dev)"
    want="-Users-jagnew-repos-jkb--jkb-work-mount-all-of-repos-into-the-dev"
    check "the slug rule reproduces the observed key" "$([ "$got" = "$want" ] && echo yes || echo no)"
    check "a container path keys separately from the host one" \
        "$([ "$(slugify /home/vscode/repos/jkb)" != "$(slugify /Users/you/repos/jkb)" ] && echo yes || echo no)"

    mkdir -p "$t/repos/jkb" "$t/repos/other"
    # Pre-existing memory, as an un-migrated host has.
    old="$t/.claude/projects/$(slugify "$t/repos/jkb")/memory"
    mkdir -p "$old"; echo hello > "$old/one.md"; echo idx > "$old/MEMORY.md"
    run "$t" "$t/repos" "$t/.jkb/claude-memory" >/dev/null 2>&1

    link="$t/.claude/projects/$(slugify "$t/repos/jkb")/memory"
    check "the repo's memory directory is now a symlink" "$([ -L "$link" ] && echo yes || echo no)"
    check "it points into the shared store" \
        "$([ "$(readlink "$link")" = "$t/.jkb/claude-memory/jkb" ] && echo yes || echo no)"
    check "existing memory was migrated, not dropped" \
        "$([ -f "$t/.jkb/claude-memory/jkb/one.md" ] && [ -f "$t/.jkb/claude-memory/jkb/MEMORY.md" ] && echo yes || echo no)"
    check "the content survived the migration" \
        "$([ "$(cat "$t/.jkb/claude-memory/jkb/one.md" 2>/dev/null)" = hello ] && echo yes || echo no)"
    check "every repo under the repos dir is linked" \
        "$([ -L "$t/.claude/projects/$(slugify "$t/repos/other")/memory" ] && echo yes || echo no)"

    # Idempotent: the verb runs on every container create and on every host re-run.
    run "$t" "$t/repos" "$t/.jkb/claude-memory" >/dev/null 2>&1
    check "re-running changes nothing and keeps the content" \
        "$([ -L "$link" ] && [ "$(cat "$t/.jkb/claude-memory/jkb/one.md" 2>/dev/null)" = hello ] && echo yes || echo no)"

    # A collision must be REPORTED and both copies kept — the whole point of not picking a winner.
    rm "$link"; mkdir -p "$link"; echo theirs > "$link/one.md"; echo new > "$link/two.md"
    run "$t" "$t/repos" "$t/.jkb/claude-memory" >/dev/null 2>&1
    check "a colliding name is not overwritten in the store" \
        "$([ "$(cat "$t/.jkb/claude-memory/jkb/one.md" 2>/dev/null)" = hello ] && echo yes || echo no)"
    check "the colliding copy is left where it is, not deleted" \
        "$([ "$(cat "$link/one.md" 2>/dev/null)" = theirs ] && echo yes || echo no)"
    check "the non-colliding file beside it still moved into the store" \
        "$([ -f "$t/.jkb/claude-memory/jkb/two.md" ] && echo yes || echo no)"

    # A symlink somebody else made is reported, never retargeted.
    rm -rf "$link"; mkdir -p "$t/elsewhere"; ln -s "$t/elsewhere" "$link"
    run "$t" "$t/repos" "$t/.jkb/claude-memory" >/dev/null 2>&1
    check "a foreign symlink is left pointing where it pointed" \
        "$([ "$(readlink "$link")" = "$t/elsewhere" ] && echo yes || echo no)"

    echo
    if [ "$fails" -ne 0 ]; then printf '\033[31m%d failed\033[0m\n' "$fails"; exit 1; fi
    printf '\033[32mlink-claude-memory self-test passed\033[0m\n'
    exit 0
fi

# --- the verb ----------------------------------------------------------------
repos="${repos:-$home/repos}"
store="${store:-$home/.jkb/claude-memory}"
echo "==> shared claude memory ($store)"
run "$home" "$repos" "$store"
