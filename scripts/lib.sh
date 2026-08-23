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
    # `mv file dir` does not fail — it moves the file INSIDE the directory. So without this
    # a <dest> that is a directory (a directory-style hook manager keeps `post-merge/` as a
    # folder of scripts) installs nothing, strands a hidden temp file in there, and returns
    # success. Refuse before writing; the Rust twin `jkb_cli::atomic::write` errors here
    # because `fs::rename` does, and the two must agree.
    if [ -e "$dest" ] && [ ! -f "$dest" ]; then
        printf 'install_exec: %s exists and is not a regular file\n' "$dest" >&2
        return 1
    fi
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
# authority, and scripts/tests/git-hooks.test.sh checks this against it.
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
    # `--path` makes git do its own expansion: `~/x` via $HOME and `~user/x` via passwd,
    # which a hand-rolled `${v/#\~/$HOME}` gets wrong for the second form — it produced
    # `/Users/jagnewjagnew/hooks` for `~jagnew/hooks`, a directory git never looks in, and
    # setup.sh then created it and reported success. A relative value stays relative, so the
    # branch below is unaffected.
    configured="$(git -C "$repo_root" config --get --path core.hooksPath 2>/dev/null)" || return 0
    [ -n "$configured" ] || return 0
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

# --- the global post-merge chainer -------------------------------------------------------
# `core.hooksPath` REPLACES .git/hooks, so a repo-local hook is silently dead whenever one is
# configured. setup.sh therefore also installs a chainer there that dispatches back to the
# repo hook. Both the body and the install arms live here, not inline in setup.sh, so
# scripts/tests/chainer.test.sh can drive them: while they were a heredoc plus three inline
# arms, reverting the dispatch line below to `--git-dir` left the entire gate green — and
# check.sh and ci.yml both justify the shell-test stage on exactly that reachability claim.

# chainer_body — the chainer jkb installs today.
chainer_body() {
    cat <<'CHAIN'
#!/bin/sh
# Global post-merge chainer. `core.hooksPath` bypasses .git/hooks, so dispatch to the
# repo-local hook if one exists (mirrors the commit-msg chainer).
#
# `--git-common-dir`, not `--git-dir`: in a linked worktree the latter is the per-worktree
# directory, which holds no hooks. Git resolves `hooks/` against the common dir.
#
# Written by jkb's scripts/setup.sh. Edit it and jkb will stop updating it — by design: it
# can only recognise its own work byte-for-byte, and refuses to overwrite anything else.
common_dir="$(git rev-parse --git-common-dir 2>/dev/null)" || exit 0
[ -n "$common_dir" ] || exit 0
repo_hook="$common_dir/hooks/post-merge"
[ -x "$repo_hook" ] && exec "$repo_hook" "$@"
exit 0
CHAIN
}

# chainer_body_v1 — FROZEN. The chainer shipped before the `--git-common-dir` fix; it
# dispatches to `--git-dir`, so under it a pull inside any worktree finds no repo hook.
# Kept verbatim so `install_chainer` can recognise one and upgrade it.
#
# Changing `chainer_body` means adding the outgoing body here as the next `_vN`, and to
# `chainer_known_bodies`. Skipping that step is not silent: the old chainer stops being
# recognised and is reported as foreign — it is never overwritten.
chainer_body_v1() {
    cat <<'CHAIN'
#!/bin/sh
# Global post-merge chainer. `core.hooksPath` bypasses .git/hooks, so dispatch to the
# repo-local hook if one exists (mirrors the commit-msg chainer).
repo_hook="$(git rev-parse --git-dir 2>/dev/null)/hooks/post-merge"
[ -x "$repo_hook" ] && exec "$repo_hook" "$@"
exit 0
CHAIN
}

chainer_known_bodies="chainer_body chainer_body_v1"

# install_chainer <path> — install/refresh the chainer, printing exactly one word:
#
#   installed   there was nothing there
#   up-to-date  already byte-identical to chainer_body
#   refreshed   byte-identical to a KNOWN OLDER body, so jkb wrote it and upgraded it
#   foreign     anything else — left completely alone
#
# Ownership is byte equality against a body jkb actually wrote, never a marker inside the
# file. A grep for the comment line was the first attempt and it is a proxy for the claim
# rather than the claim: a user who adds a line to *our* chainer still matches it, and the
# refresh arm then replaced their file, unattended, on an ordinary `git pull`, with no
# backup and the same "(refreshed)" message as the intended upgrade. Byte equality IS the
# claim "we wrote every byte of this", so it cannot be satisfied by a file we did not write.
install_chainer() {
    local dest="$1" body
    if [ ! -e "$dest" ]; then
        chainer_body | install_exec "$dest" || return 1
        printf 'installed\n'
        return 0
    fi
    if [ ! -f "$dest" ]; then
        printf 'foreign\n'
        return 0
    fi
    # `-x` as well as the bytes: git skips a hook that is not executable, so "the bytes match"
    # is not the claim being made — "git will run our chainer" is. This is the only arm that
    # skips install_exec, so it is the only one where the mode is not set as a side effect;
    # a restore from backup or an `rsync` without `-p` lands here at mode 644 and would be
    # reported healthy while the repo hook silently never runs.
    if [ -x "$dest" ] && chainer_body | cmp -s - "$dest"; then
        printf 'up-to-date\n'
        return 0
    fi
    # Right bytes, wrong mode: ours, so re-install it (install_exec chmods).
    if chainer_body | cmp -s - "$dest"; then
        chainer_body | install_exec "$dest" || return 1
        printf 'refreshed\n'
        return 0
    fi
    for body in $chainer_known_bodies; do
        if "$body" | cmp -s - "$dest"; then
            chainer_body | install_exec "$dest" || return 1
            printf 'refreshed\n'
            return 0
        fi
    done
    printf 'foreign\n'
}

# git_exclude_locally <repo_root> <absolute path> — add <path> to the repo's
# `.git/info/exclude` if it sits inside the working tree and is not excluded already.
# Idempotent; prints the pattern it added, nothing if there was no need.
#
# For the chainer under a RELATIVE `core.hooksPath` (`core.hooksPath = .githooks`), which
# git resolves inside the working tree. Untracked, it makes every `jkb task work` session
# read dirty, and `jkb task land` refuses a dirty target — recreated by the next pull, so
# deleting it does not help. `.git/info/exclude` is the local, unpushed write the project
# already sanctions for exactly this (D36 does it for `.jkb/`); editing the tracked
# `.gitignore` of someone's repo is not.
git_exclude_locally() {
    local repo_root="$1" path="$2" top common rel exclude
    top="$(git -C "$repo_root" rev-parse --show-toplevel 2>/dev/null)" || return 0
    [ -n "$top" ] || return 0
    case "$path" in
        "$top"/*) rel="/${path#"$top"/}" ;;
        *) return 0 ;;   # outside the working tree: nothing to hide
    esac
    common="$(git -C "$repo_root" rev-parse --git-common-dir 2>/dev/null)" || return 0
    case "$common" in /*) ;; *) common="$repo_root/$common" ;; esac
    exclude="$common/info/exclude"
    mkdir -p "$common/info" || return 0
    if [ -f "$exclude" ] && grep -qxF "$rel" "$exclude"; then
        return 0
    fi
    # Separator first. An exclude file that does not end in a newline — hand-edited ones
    # often do not — would otherwise have its last rule fused with ours (`*.log` +
    # `/.githooks/post-merge` = `*.log/.githooks/post-merge`), destroying a rule the user
    # owns and cannot get back, while our own pattern still does not take effect. The next
    # run appends a correct second line, so it self-heals for jkb and never for them.
    # `session::ensure_excluded` (crates/jkb-cli/src/session.rs) computes the same `sep` for
    # the same reason — this is one rule with an implementation in each language.
    if [ -s "$exclude" ] && [ -n "$(tail -c 1 "$exclude")" ]; then
        printf '\n' >>"$exclude" || return 0
    fi
    printf '%s\n' "$rel" >>"$exclude" || return 0
    printf '%s\n' "$rel"
}

# install_git_hooks <repo_root> <hooks_src> — install the repo post-merge hook, plus a chainer
# when `core.hooksPath` redirects git away from it. Prints one `key=value` line per action:
#
#   repo-hook=<path>          the hook, installed where git actually runs hooks from
#   chainer=<outcome> <path>  installed | up-to-date | refreshed | foreign | failed
#   excluded=<pattern>        the chainer sat inside the working tree and was hidden locally
#   error=<reason>            nothing was done
#
# setup.sh renders those; the machine-readable form is what lets a shell test drive the whole
# block. It is here rather than inline in setup.sh for the reason the chainer body already
# moved: nothing runs setup.sh, so an inline arm is reachable from no test, and reverting the
# hooks directory to `--git-dir` left the entire gate green while every pull inside a worktree
# stopped running the repo hook.
install_git_hooks() {
    local repo_root="$1" hooks_src="$2" hooks_dir chainer outcome excluded override

    hooks_dir="$(git_hooks_dir "$repo_root")" || { printf 'error=not a git repo\n'; return 1; }
    mkdir -p "$hooks_dir" || { printf 'error=cannot create %s\n' "$hooks_dir"; return 1; }
    # `install_exec`, never `cp`: the hook being replaced is very often the process that
    # invoked us, and `cp` rewrites its inode underneath the running shell.
    install_exec "$hooks_dir/post-merge" <"$hooks_src" || {
        printf 'error=could not install %s/post-merge\n' "$hooks_dir"; return 1; }
    printf 'repo-hook=%s\n' "$hooks_dir/post-merge"

    override="$(git_hooks_override "$repo_root")"
    [ -n "$override" ] || return 0

    mkdir -p "$override" || { printf 'error=cannot create %s\n' "$override"; return 1; }
    chainer="$override/post-merge"
    outcome="$(install_chainer "$chainer")"
    [ -n "$outcome" ] || outcome=failed
    printf 'chainer=%s %s\n' "$outcome" "$chainer"

    # Only hide a file we own. Excluding a `foreign` chainer would take the opposite position
    # on ownership from the line above it: jkb would declare the file not its to touch and
    # then write a permanent ignore rule for it, hiding the user's own hook from `git status`
    # and `git add -A` for good.
    case "$outcome" in
        installed|up-to-date|refreshed)
            excluded="$(git_exclude_locally "$repo_root" "$chainer")"
            # `if`, not `[ … ] && printf` — the rule this file states one function above.
            if [ -n "$excluded" ]; then
                printf 'excluded=%s\n' "$excluded"
            fi
            ;;
    esac
    return 0
}
