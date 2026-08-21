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
