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
# STATED PLAINLY, because it is a hole in "the boundary is what you did not mount", and it is
# WIDER than the container question it was designed around. Two channels, not one:
#
#  1. container -> host. Memory is agent-writable prose injected into context, so a shared store
#     carries text from container sessions into the less-confined host ones. Same person's agents
#     at both ends, prose rather than code, through a directory that was already shared.
#
#  2. sandboxed Bash -> every future session, ON THE HOST. This one was measured, not predicted,
#     and it is the reason to read the paragraph twice. `~/.claude/projects` is under the
#     posture's blanket `denyRead` of `~` and in no allow list, so sandboxed Bash cannot read or
#     write auto-memory at all. `~/.jkb` is in BOTH `allowRead` and `allowWrite`, because jkb's
#     database lives there. Linking therefore moves memory from a place sandboxed Bash cannot
#     touch into one where a single auto-approved command can rewrite it — for this repo and, via
#     the same grant, for every other repo's store. Prose written there is injected into every
#     later session for that repo.
#
# The posture has no write-deny to carve `claude-memory` back out with (`filesystem` offers
# `denyRead`, `allowRead`, `allowWrite` and nothing else). Weighed against a dedicated store with
# its own declared bind, and `~/.jkb` was CHOSEN: both ends are the same person's agents, it is
# prose rather than code, and the host side is opt-in (`setup.sh --link-memory`), never created by
# a `git pull`.
#
# If you revisit it, start from this measured fact: file tools and Bash are bounded by different
# mechanisms — the sandbox's `filesystem` block governs Bash, the `permissions` rules govern
# Read/Edit/Write — so an agent writes memory through the Write tool wherever the store lives.
# Moving it out of `~/.jkb` would close the Bash channel and would NOT stop agents writing memory.
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
status_repo=""
status_file=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run)   dry=1 ;;
        --self-test) self_test=1 ;;
        # So verify.sh can find the link it asserts without keeping a second copy of the slug
        # rule. Two spellings of one derivation is the duplication .devcontainer/lib.sh exists
        # to remove, and this one is a guess about another program's encoding — the worst kind
        # to have two of.
        --print-slug) shift; slugify "${1:-}"; printf '\n'; exit 0 ;;
        --status)     shift; status_repo="${1:-}" ;;
        # Where to record the state each repo was in BEFORE this run repaired anything. The
        # repair downgrades its own alarm — removing a live link into a poisoned store turns
        # `exposed` into `unsafe` — so a check that runs afterwards must read what was found,
        # not what was left.
        --status-file) shift; status_file="${1:-}" ;;
        --home)      shift; home="${1:-}" ;;
        --repos)     shift; repos="${1:-}" ;;
        --store)     shift; store="${1:-}" ;;
        -h|--help)   sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)           echo "unknown flag: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done

# Progress goes to STDERR. `link_one`'s stdout is its state word and nothing else: `run` captures
# it, and with the notes mixed in the state never equalled `linked`, so every clean run exited 1
# with its own report eaten — including the "migrated N file(s)" line, the only word a user gets
# that their memory was moved.
note() { printf '  %s\n' "$*" >&2; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*" >&2; }


# One repo. Kept separate from the loop so the self-test can drive it directly.
#
# Prints one word — the STATE it left that repo in — and exits 0 for every state it recognises,
# because "the linker ran and correctly declined" is not a failure and must not read as one.
# `verify.sh` consumes that word: it used to infer breakage from the link's absence, which is a
# state this function produces on purpose, so a normal collision failed container creation.
#
#   linked     the memory directory is a symlink into the store
#   collision  a name exists on both sides; nothing was moved and no link was made
#   foreign    something else already owns that path; left exactly as it was
#   unsafe     the store holds something other than plain files, and nothing points at it
#   exposed    ...and a live link DID point at it, which this removed
#   error      the state could not be established
link_one() { # link_one <projects-dir> <store-dir> <repo-path>
    local projects="$1" store_dir="$2" path="${3%/}"
    local key slug link dest
    key="$(basename "$path")"
    slug="$(slugify "$path")"
    link="$projects/$slug/memory"
    dest="$store_dir/$key"

    if [ "$dry" -eq 1 ]; then
        note "would link $link -> $dest"
        echo linked
        return 0
    fi
    mkdir -p "$dest" || { warn "$key: cannot create $dest"; echo error; return 0; }

    # THE STORE MUST HOLD PLAIN FILES. It is written by agents on both sides of a boundary the
    # rest of this design spends its effort maintaining, and a symlink planted in it by either
    # side redirects the other's reads and writes wherever it points — including back into
    # ~/.claude, the one directory nothing here may share. Memory is prose in files; a link, a
    # socket or a device in there is not memory and is not something to quietly follow.
    if [ -L "$dest" ] || [ -n "$(find "$dest" ! -type d ! -type f -print -quit 2>/dev/null)" ]; then
        # AND BREAK THE LINK IF ONE IS LIVE. Refusing only to *create* one left the steady state
        # untouched: link a repo normally, plant a symlink in the store afterwards, and the
        # redirect — including back into ~/.claude, which nothing here may share — stays live on
        # both sides while this prints a refusal and `verify.sh` reports an ok line. Removing a
        # symlink destroys no memory (the files are in the store), and the repo simply stops being
        # shared until a person cleans it, which is the safer side of that trade.
        if [ -L "$link" ] && [ "$(readlink "$link")" = "$dest" ]; then
            rm -f "$link"
            warn "$key: $dest holds something that is not a plain file — REMOVED the live link into it"
            warn "  memory for $key is no longer shared until you clean $dest and re-run"
            echo exposed
            return 0
        fi
        warn "$key: $dest holds something that is not a plain file — refusing to link until it does"
        echo unsafe
        return 0
    fi

    if [ -L "$link" ]; then
        local current
        current="$(readlink "$link")"
        if [ "$current" = "$dest" ]; then
            note "$key: already linked"
            echo linked
            return 0
        fi
        warn "$key: $link already points at $current — left alone; remove it by hand to relink"
        echo foreign
        return 0
    fi

    # A real directory here is memory written before the store existed. DECIDED IN FULL BEFORE
    # ANYTHING MOVES: a scan first, and if any name exists on both sides nothing is moved and no
    # link is made. Migrating what it could and then refusing the link left that side holding only
    # the colliding file, with its MEMORY.md index pointing at notes it could no longer read —
    # which is worse than never having run, the one outcome this must not produce.
    if [ -d "$link" ]; then
        local collisions=() entry base moved=0
        shopt -s dotglob nullglob
        for entry in "$link"/*; do
            base="$(basename "$entry")"
            [ -e "$dest/$base" ] && collisions+=("$base")
        done
        if [ "${#collisions[@]}" -gt 0 ]; then
            shopt -u dotglob nullglob
            warn "$key: ${collisions[*]} exist(s) on both sides — nothing moved, nothing linked;"
            warn "  merge by hand, then re-run. Store: $dest  Local: $link"
            echo collision
            return 0
        fi
        # The store must hold plain files, and THIS is the path that fills it — checking only
        # what is already there left the one route that can plant a redirecting symlink
        # unguarded. Checked in the same all-or-nothing pass as the collisions.
        for entry in "$link"/*; do
            # THE SAME RECURSIVE QUESTION the store check asks. Inspecting only the top level let
            # a DIRECTORY containing a symlink migrate in, after which the link was created and
            # the run reported `linked` — two spellings of "plain files only" disagreeing about
            # what the store may hold, and the permissive one on the path that fills it.
            if [ -L "$entry" ] \
               || { [ ! -f "$entry" ] && [ ! -d "$entry" ]; } \
               || [ -n "$(find "$entry" ! -type d ! -type f -print -quit 2>/dev/null)" ]; then
                shopt -u dotglob nullglob
                warn "$key: $(basename "$entry") is not a plain file — nothing moved, nothing linked"
                echo unsafe
                return 0
            fi
        done
        for entry in "$link"/*; do
            base="$(basename "$entry")"
            if mv "$entry" "$dest/$base" 2>/dev/null; then
                moved=$((moved+1))
            else
                shopt -u dotglob nullglob
                warn "$key: could not move $base — stopping with the rest left in place"
                echo error
                return 0
            fi
        done
        shopt -u dotglob nullglob
        if ! rmdir "$link" 2>/dev/null; then
            warn "$key: $link could not be replaced by a link — left as it is"
            echo error
            return 0
        fi
        [ "$moved" -eq 0 ] || note "$key: migrated $moved file(s) into the store"
    elif [ -e "$link" ]; then
        warn "$key: $link exists and is not a directory — left alone"
        echo foreign
        return 0
    fi

    mkdir -p "$(dirname "$link")" || { warn "$key: cannot create $(dirname "$link")"; echo error; return 0; }
    ln -s "$dest" "$link" || { warn "$key: could not create the link"; echo error; return 0; }
    note "$key: linked $link -> $dest"
    echo linked
}

# Every repo, and the state each was left in. Returns non-zero when any repo needs a person —
# a caller that cannot tell a clean run from one that linked nothing has no reason to check.
run() { # run <home> <repos> <store>
    local h="$1" r="$2" s="$3" d state found rc=0
    local projects="$h/.claude/projects"
    if [ ! -d "$r" ]; then
        warn "no repos directory at $r — nothing to link"
        return 0
    fi
    for d in "$r"/*/; do
        [ -d "$d" ] || continue
        # Asked BEFORE `link_one` runs, because `link_one` repairs — see `--status-file`.
        found="$(status_of "$h" "$s" "$d")"
        state="$(link_one "$projects" "$s" "$d")"
        [ -z "$status_file" ] || printf '%s %s\n' "$(basename "${d%/}")" "$found" >> "$status_file"
        [ "$state" = linked ] || rc=1
    done
    return "$rc"
}

# The state of ONE repo, printed as a single word and nothing else. `verify.sh` asks this rather
# than inferring breakage from a missing symlink, so the container check and this script state the
# same rule instead of opposite ones.
status_of() { # status_of <home> <store> <repo-path>
    local h="$1" s="$2" path="${3%/}" key slug link dest
    key="$(basename "$path")"
    slug="$(slugify "$path")"
    link="$h/.claude/projects/$slug/memory"
    dest="$s/$key"
    # THE STORE FIRST, in the same order `link_one` asks — this used to test the link first, so a
    # linked repo whose store had been poisoned reported `linked`, and `verify.sh` consults this
    # one. Two orderings of one rule is two rules.
    if [ -L "$dest" ] || [ -n "$(find "$dest" ! -type d ! -type f -print -quit 2>/dev/null)" ]; then
        # `exposed` while a link into it is LIVE — an active redirect, which verify.sh fails on —
        # versus `unsafe`, an impure store nothing points at, which only wants a person.
        if [ -L "$link" ] && [ "$(readlink "$link")" = "$dest" ]; then echo exposed; return 0; fi
        echo unsafe; return 0
    fi
    if [ -L "$link" ]; then
        [ "$(readlink "$link")" = "$dest" ] && { echo linked; return 0; }
        echo foreign; return 0
    fi
    if [ -d "$link" ]; then
        local entry
        shopt -s dotglob nullglob
        for entry in "$link"/*; do
            if [ -e "$dest/$(basename "$entry")" ]; then
                shopt -u dotglob nullglob
                echo collision; return 0
            fi
        done
        shopt -u dotglob nullglob
        echo unlinked; return 0
    fi
    [ -e "$link" ] && { echo foreign; return 0; }
    echo unlinked
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
    # The CONTAINER key, by value. Asserting merely that two different inputs give two different
    # outputs cannot fail under a character-wise mapping — it was a test in the shape of a test.
    # This is the literal `verify.sh` and the README both name, so a change to either shows here.
    check "the container path keys to -home-vscode-repos-jkb" \
        "$([ "$(slugify /home/vscode/repos/jkb)" = "-home-vscode-repos-jkb" ] && echo yes || echo no)"

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
    check "--status agrees with what was done" \
        "$([ "$(status_of "$t" "$t/.jkb/claude-memory" "$t/repos/jkb")" = linked ] && echo yes || echo no)"
    # The other half of run()'s contract, and the half that was missing: a clean run must SUCCEED.
    # `note` used to write to stdout, which `run` captures, so the state never equalled `linked`
    # and every clean run exited 1 — the "needs attention" assertion below passed unconditionally
    # and could not have failed.
    check "a clean run succeeds" \
        "$(run "$t" "$t/repos" "$t/.jkb/claude-memory" >/dev/null 2>&1 && echo yes || echo no)"

    # Idempotent: the verb runs on every container create and on every host re-run.
    run "$t" "$t/repos" "$t/.jkb/claude-memory" >/dev/null 2>&1
    check "re-running changes nothing and keeps the content" \
        "$([ -L "$link" ] && [ "$(cat "$t/.jkb/claude-memory/jkb/one.md" 2>/dev/null)" = hello ] && echo yes || echo no)"

    # A COLLISION MOVES NOTHING. Migrating what it could and then refusing the link left this
    # side holding only the colliding file, with its index naming notes it could no longer read.
    rm "$link"; mkdir -p "$link"
    echo theirs > "$link/one.md"; echo new > "$link/two.md"
    run "$t" "$t/repos" "$t/.jkb/claude-memory" >/dev/null 2>&1
    check "a colliding name is not overwritten in the store" \
        "$([ "$(cat "$t/.jkb/claude-memory/jkb/one.md" 2>/dev/null)" = hello ] && echo yes || echo no)"
    check "the colliding copy is left where it is" \
        "$([ "$(cat "$link/one.md" 2>/dev/null)" = theirs ] && echo yes || echo no)"
    check "and so is every file beside it — nothing moved at all" \
        "$([ -f "$link/two.md" ] && [ ! -f "$t/.jkb/claude-memory/jkb/two.md" ] && echo yes || echo no)"
    check "the state is reported as a collision, not as breakage" \
        "$([ "$(status_of "$t" "$t/.jkb/claude-memory" "$t/repos/jkb")" = collision ] && echo yes || echo no)"
    check "run() reports that a repo needs attention" \
        "$(run "$t" "$t/repos" "$t/.jkb/claude-memory" >/dev/null 2>&1 && echo no || echo yes)"

    # A symlink in the store redirects the other side's reads and writes wherever it points.
    rm -rf "$link" "$t/.jkb/claude-memory/jkb"
    mkdir -p "$t/.jkb/claude-memory/jkb" "$t/elsewhere"
    ln -s "$t/elsewhere/leak" "$t/.jkb/claude-memory/jkb/one.md"
    run "$t" "$t/repos" "$t/.jkb/claude-memory" >/dev/null 2>&1
    check "a store holding a symlink is refused, not linked into" \
        "$([ ! -L "$link" ] && echo yes || echo no)"
    check "and says so" \
        "$([ "$(status_of "$t" "$t/.jkb/claude-memory" "$t/repos/jkb")" = unsafe ] && echo yes || echo no)"

    # The MIGRATION is the path that fills the store, so it must apply the same purity rule as
    # the store check — otherwise the one route that can plant a redirecting symlink is unguarded.
    rm -rf "$link" "$t/.jkb/claude-memory/jkb"; mkdir -p "$link"
    ln -s "$t/elsewhere/leak" "$link/one.md"
    run "$t" "$t/repos" "$t/.jkb/claude-memory" >/dev/null 2>&1
    check "a symlink is not migrated into the store" \
        "$([ ! -e "$t/.jkb/claude-memory/jkb/one.md" ] && echo yes || echo no)"
    check "and the local copy is left exactly as it was" \
        "$([ -L "$link/one.md" ] && echo yes || echo no)"

    # A poisoned store must not read as healthy just because the link happens to exist —
    # `verify.sh` consults `status_of`, so the two must ask in the same order.
    rm -rf "$link" "$t/.jkb/claude-memory/jkb"
    mkdir -p "$t/.jkb/claude-memory/jkb" "$(dirname "$link")"
    ln -s "$t/.jkb/claude-memory/jkb" "$link"
    ln -s "$t/elsewhere/leak" "$t/.jkb/claude-memory/jkb/one.md"
    check "a linked repo with a poisoned store reports EXPOSED, not linked" \
        "$([ "$(status_of "$t" "$t/.jkb/claude-memory" "$t/repos/jkb")" = exposed ] && echo yes || echo no)"
    # ...and the linker BREAKS that link rather than only declining to make another. Refusing to
    # create one left the live redirect — including back into ~/.claude — running untouched.
    run "$t" "$t/repos" "$t/.jkb/claude-memory" >/dev/null 2>&1
    check "and the live redirect is removed, not merely reported" \
        "$([ ! -e "$link" ] && echo yes || echo no)"
    check "while the memory in the store is left for a person to clean" \
        "$([ -L "$t/.jkb/claude-memory/jkb/one.md" ] && echo yes || echo no)"
    rm -rf "$t/.jkb/claude-memory/jkb"; mkdir -p "$t/.jkb/claude-memory/jkb"

    # A symlink somebody else made is reported, never retargeted.
    rm -rf "$t/.jkb/claude-memory/jkb"; mkdir -p "$t/.jkb/claude-memory/jkb"
    rm -rf "$link"; ln -s "$t/elsewhere" "$link"
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
if [ -n "$status_repo" ]; then
    status_of "$home" "$store" "$status_repo"
    exit 0
fi
[ -z "$status_file" ] || : > "$status_file"
echo "==> shared claude memory ($store)"
run "$home" "$repos" "$store"
