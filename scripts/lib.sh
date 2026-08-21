# Shared shell helpers. Sourced, never executed (no shebang, not +x).
#
#   . "$repo_root/scripts/lib.sh"

# install_exec <dest> — install stdin as an executable file at <dest>, ATOMICALLY.
#
# Writes a temp file in <dest>'s own directory and `mv`s it into place. This is
# load-bearing, not tidiness: `mv` within a directory is a rename, which swaps the
# directory entry and leaves the old inode alone, so a process **currently executing**
# the old file keeps reading the bytes it started with.
#
# `cp`/`> "$dest"` instead truncate and rewrite that inode in place. Bash reads a script
# lazily, by byte offset — so overwriting a running script with one of a different length
# makes the shell resume at an offset that no longer means what it meant, and execute
# whatever fragment it lands on. Observed on the 2026-08-21 pull, where the post-merge
# hook ran setup.sh, setup.sh `cp`d a 2-byte-longer hook over it, and the hook died with
#
#     .git/hooks/post-merge: line 35: i: command not found
#
# on a line that is blank in both versions. That failure was harmless by luck; the same
# shift could silently skip the rest of the hook, which is why the burden belongs here, in
# the one installer, rather than on every script that might be replaced while it runs.
#
# A partial write is also impossible to observe: on any failure the temp file is removed
# and <dest> is left exactly as it was.
install_exec() {
    local dest="$1" dir tmp
    dir="$(dirname "$dest")"
    # The temp name is deliberately unrelated to <dest> (and hidden) so a half-written file
    # is never mistaken for the thing being installed — e.g. a git hook.
    tmp="$(mktemp "$dir/.jkb-install.XXXXXX")" || return 1
    if cat >"$tmp" && chmod 755 "$tmp" && mv -f "$tmp" "$dest"; then
        return 0
    fi
    rm -f "$tmp"
    return 1
}

# git_hooks_dir <repo_root> — print the directory git runs hooks from. Fails if <repo_root>
# is not a git repo.
#
# NOT `--git-dir`. In a linked worktree that is `<repo>/.git/worktrees/<name>`, which holds
# no hooks: git resolves `hooks/` against the COMMON dir, so a hook installed under
# `--git-dir` goes where git never looks, the installer reports success, and the stale hook
# keeps running. `jkb task work` puts every session in a worktree, so that is the normal
# case here rather than a corner one. `git rev-parse --git-path hooks/post-merge` is the
# authority, and scripts/tests/git-hooks-dir.test.sh checks this against it.
#
# The path comes back relative to <repo_root> for an ordinary checkout (`.git`) and absolute
# for a worktree, so it is normalised here rather than at each call site.
git_hooks_dir() {
    local repo_root="$1" common
    common="$(git -C "$repo_root" rev-parse --git-common-dir 2>/dev/null)" || return 1
    [ -n "$common" ] || return 1
    case "$common" in /*) ;; *) common="$repo_root/$common" ;; esac
    printf '%s\n' "$common/hooks"
}

# git_hooks_override <repo_root> — print the absolute `core.hooksPath` in effect for
# <repo_root>, or nothing when there is none. Always succeeds: "no override" is an answer,
# not a failure.
#
# Both halves are corrections of a cwd-scoped read, and both fail silently:
#
#   * `git config --get core.hooksPath` answers for whatever repository the CALLER happens to
#     be standing in. Read from another project, setup.sh wrote jkb's chainer into *that*
#     project's hooks directory; missed because the caller stood outside jkb while jkb itself
#     sets the key, it wrote no chainer at all — leaving the repo hook dead, which is the one
#     thing the chainer exists to prevent, under a success message.
#   * a relative value is resolved by git against the top of <repo_root>'s working tree
#     (githooks(5): git chdirs there before running a hook), NOT against `$PWD` — so letting
#     `mkdir -p` resolve it put the chainer one directory per subdirectory you ran from.
#
# `git rev-parse --git-path hooks/post-merge` honours this setting, which is what lets
# scripts/tests/git-hooks.test.sh check the result against git's own answer.
git_hooks_override() {
    local repo_root="$1" configured top
    configured="$(git -C "$repo_root" config --get core.hooksPath 2>/dev/null)" || return 0
    [ -n "$configured" ] || return 0
    configured="${configured/#\~/$HOME}"
    case "$configured" in
        /*) ;;
        *)
            top="$(git -C "$repo_root" rev-parse --show-toplevel 2>/dev/null)" || return 0
            [ -n "$top" ] || return 0
            configured="$top/$configured"
            ;;
    esac
    printf '%s\n' "$configured"
}
