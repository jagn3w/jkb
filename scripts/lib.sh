# Shared shell helpers. Sourced, never executed (no shebang, not +x).
#
#   . "$repo_root/scripts/lib.sh"
#
# TWO RULES HOLD FOR EVERY FUNCTION HERE.
#
# 1. `install_exec` is the only function permitted to write an executable destination.
#    `cp` and `> "$dest"` truncate and rewrite the destination's inode in place, and the
#    destination is routinely a script that is currently executing — see below. The primitive
#    having a test is not enough: the mutation that matters is at a CALL SITE, so each
#    replacement arm is pinned by asserting the destination's inode changed (`inode_of` in
#    scripts/tests/harness.sh), which a rename does and an in-place rewrite does not.
#
# 2. Every function behaves identically whether or not the caller ran `set -e`. A function
#    that REPORTS (install_chainer, reconcile_exclude, install_git_hooks on any path that has
#    already printed a line) returns 0 and speaks entirely through its printed words:
#    `failed` is an outcome, not an exit status. Every fallible command sits in a branch
#    (`if` / `||` / `&&`), never as a bare statement. This is not style. setup.sh's report
#    used to survive only because the call happened to be written `… || true`, which disables
#    `set -e` for the whole function body; without that the subshell died inside
#    `install_chainer`, and setup.sh printed the repo hook and nothing else while
#    `core.hooksPath` was set and that hook was therefore dead.

# warn <message> — a warning on stderr. Here rather than in setup.sh because
# `render_git_hooks_report` is here, and two copies of one line drift.
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }

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
#   failed      we should have written it and could not
#
# `failed` is a WORD, not an exit status, and this function always returns 0. See the
# `set -e` rule in this file's header: a reporter that dies mid-report takes its own report
# with it, and the caller then prints a success line and nothing else.
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
        chainer_body | install_exec "$dest" || { printf 'failed\n'; return 0; }
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
    # Anything else that is byte-for-byte a body jkb has written — the CURRENT one at the
    # wrong mode included, since `chainer_body` heads this list and the arm above required
    # `-x` as well as the bytes. There is deliberately no separate wrong-mode arm: it ran the
    # identical comparison and took the identical branch, so the `-x` distinction was
    # expressed in two places that had to agree, in the one function whose whole value is
    # that its ownership rule is legible.
    for body in $chainer_known_bodies; do
        if "$body" | cmp -s - "$dest"; then
            chainer_body | install_exec "$dest" || { printf 'failed\n'; return 0; }
            printf 'refreshed\n'
            return 0
        fi
    done
    printf 'foreign\n'
}

# --- hiding the chainer from `git status` -------------------------------------------------
# A RELATIVE `core.hooksPath` (`core.hooksPath = .githooks`) resolves inside the working tree,
# so the chainer is an untracked file there: every `jkb task work` session reads dirty, and
# `jkb task land` refuses a dirty target — recreated by the next pull, so deleting it does not
# help. `.git/info/exclude` is the local, unpushed write the project already sanctions for
# exactly this (D36 does it for `.jkb/`); editing the tracked `.gitignore` of someone's repo
# is not.
#
# The rule is RECONCILED on every run, not written once. Adding it on the run that installs a
# chainer and never revisiting it meant a user who later replaced that chainer with their own
# hook had their file git-ignored for ever: run 2 reported `chainer=foreign`, left run 1's
# rule in place, and `git status` went silent with nothing attributing it to jkb.
#
# Ownership is the chainer lesson applied a second time. jkb writes a MARKED two-line block,
# and only ever retracts that exact adjacent pair — so a bare pattern, which may be a rule the
# user wrote themselves, is reported and never deleted. Byte identity is the claim "we wrote
# this"; a pattern on its own cannot make it.

# exclude_marker — the comment line jkb writes above its own exclude pattern.
#
# Changing it means adding the outgoing text as the next `exclude_marker_vN` AND to
# `exclude_known_markers`, exactly as `chainer_body_v1` does. Forgetting is not silent: the
# old block stops being recognised, so it is reported as unowned rather than retracted.
exclude_marker() {
    printf '# jkb: the post-merge chainer, which core.hooksPath resolves inside this working tree\n'
}

exclude_known_markers="exclude_marker"

# _exclude_line <raw line> — the line as GIT reads it.
#
# git trims one trailing CR from every line of an ignore/exclude file (`dir.c`), so a CRLF
# file is perfectly functional to git and our own comparisons must agree with it. They did
# not: on such a file nothing matched, so `reconcile_exclude` neither recognised its own block
# nor saw the pattern at all, and appended a fresh one on EVERY qualifying pull — unbounded
# growth in a file of the user's rules, which is the harm this whole reconciliation exists to
# avoid. Comparisons are trimmed; what gets written back is the untrimmed original, so a CRLF
# file is not silently converted.
_exclude_line() { printf '%s' "${1%$'\r'}"; }

# _is_exclude_marker <line> — is this one of the marker lines jkb has ever written?
_is_exclude_marker() {
    local line marker
    line="$(_exclude_line "$1")"
    for marker in $exclude_known_markers; do
        [ "$line" = "$("$marker")" ] && return 0
    done
    return 1
}

# _exclude_mentions <file> <pattern> — does any line of <file> exclude <pattern>?
#
# Not `grep -qxF`: that is a second way of asking the question `_exclude_without_our_block`
# already asks, and the two came apart on exactly the input where it matters. One reading
# rule, applied by both.
_exclude_mentions() {
    local file="$1" pattern="$2" line
    [ -f "$file" ] || return 1
    while IFS= read -r line || [ -n "$line" ]; do
        [ "$(_exclude_line "$line")" = "$pattern" ] && return 0
    done <"$file"
    return 1
}

# _exclude_without_our_block <file> <pattern> — print <file> with every marked block for
# <pattern> removed. Returns 0 if it removed at least one, 1 if there was none to remove.
#
# One implementation answers both questions the reconciler asks — "is our block there?"
# (discard the output) and "give me the file without it" — so the two cannot disagree about
# what "ours" means.
_exclude_without_our_block() {
    local file="$1" pattern="$2" line i n found=1
    local -a lines=() keep=()
    [ -f "$file" ] || return 1
    # `|| [ -n "$line" ]` so a final line with no trailing newline is not dropped.
    while IFS= read -r line || [ -n "$line" ]; do lines+=("$line"); done <"$file"
    n=${#lines[@]}
    i=0
    while [ "$i" -lt "$n" ]; do
        if _is_exclude_marker "${lines[$i]}" \
            && [ "$((i + 1))" -lt "$n" ] \
            && [ "$(_exclude_line "${lines[$((i + 1))]}")" = "$pattern" ]; then
            i=$((i + 2))
            found=0
            continue
        fi
        keep+=("${lines[$i]}")
        i=$((i + 1))
    done
    [ "${#keep[@]}" -eq 0 ] || printf '%s\n' "${keep[@]}"
    return "$found"
}

# reconcile_exclude <repo_root> <path> <want> — make `.git/info/exclude` agree with <want>
# for <path>, and print the resulting state as `<state> <detail>`:
#
#   added <pattern>       we wrote our marked block
#   kept <pattern>        a rule already excludes it (ours, or one the user wrote)
#   retracted <pattern>   our block was there and <want> is no, so it was removed
#   unowned <pattern>     a rule excludes it that jkb cannot prove it wrote — reported, kept
#   none <reason>         nothing to do
#   failed <reason>       a write was attempted and did not land
#
# <want> is `yes` when the chainer at <path> is one jkb owns and `no` when it is not. Both
# directions are one function because they are one rule: excluding a `foreign` chainer would
# take the opposite position on ownership from the line that just refused to touch it.
#
# Always returns 0 — `failed` is a word, not an exit status (see the header's `set -e` rule).
reconcile_exclude() {
    local repo_root="$1" path="$2" want="$3" top common exclude pattern tmp
    top="$(git -C "$repo_root" rev-parse --show-toplevel 2>/dev/null)" \
        || { printf 'none (not a git working tree)\n'; return 0; }
    [ -n "$top" ] || { printf 'none (not a git working tree)\n'; return 0; }
    case "$path" in
        "$top"/*) pattern="/${path#"$top"/}" ;;
        *) printf 'none (outside the working tree, so nothing is hidden)\n'; return 0 ;;
    esac

    common="$(git -C "$repo_root" rev-parse --git-common-dir 2>/dev/null)" \
        || { printf 'none (not a git working tree)\n'; return 0; }
    case "$common" in /*) ;; *) common="$repo_root/$common" ;; esac
    exclude="$common/info/exclude"

    if _exclude_without_our_block "$exclude" "$pattern" >/dev/null; then
        # Ours.
        if [ "$want" = yes ]; then
            printf 'kept %s\n' "$pattern"
            return 0
        fi
        # Rewrite through a temp file: this file is full of the USER'S rules, and a partial
        # in-place rewrite destroys them. `cp -p` first so the replacement inherits the
        # destination's mode rather than mktemp's 600. Not `install_exec` — that installs an
        # executable, and this is data.
        tmp="$exclude.jkb.$$"
        if ! cp -p "$exclude" "$tmp" 2>/dev/null; then
            printf 'failed (cannot write %s)\n' "$exclude"
            return 0
        fi
        if ! _exclude_without_our_block "$exclude" "$pattern" >"$tmp" || ! mv -f "$tmp" "$exclude"; then
            rm -f "$tmp"
            printf 'failed (cannot write %s)\n' "$exclude"
            return 0
        fi
        printf 'retracted %s\n' "$pattern"
        return 0
    fi

    # Not ours. Is it excluded anyway — by a rule the user wrote?
    if _exclude_mentions "$exclude" "$pattern"; then
        if [ "$want" = yes ]; then
            # Already hidden, so there is nothing to do and nothing to own.
            printf 'kept %s\n' "$pattern"
        else
            # The harm this whole reconciliation exists to stop, in the one case we cannot
            # repair: something is hiding the file and jkb cannot prove it put it there, so
            # it says so instead of deleting a rule that may be the user's.
            printf 'unowned %s\n' "$pattern"
        fi
        return 0
    fi

    if [ "$want" != yes ]; then
        printf 'none (nothing is hiding it)\n'
        return 0
    fi

    mkdir -p "$common/info" 2>/dev/null || { printf 'failed (cannot create %s/info)\n' "$common"; return 0; }
    # Separator first. An exclude file that does not end in a newline — hand-edited ones often
    # do not — would otherwise have its last rule fused with our marker (`*.log` + `# jkb: …`),
    # destroying a rule the user owns while our own pattern stayed inert, under a success
    # message. `session::ensure_excluded` (crates/jkb-cli/src/session.rs) computes the same
    # `sep` for the same reason — one rule with an implementation in each language.
    if [ -s "$exclude" ] && [ -n "$(tail -c 1 "$exclude")" ]; then
        printf '\n' >>"$exclude" 2>/dev/null || { printf 'failed (cannot write %s)\n' "$exclude"; return 0; }
    fi
    # ONE simple command, not `{ exclude_marker; printf …; } >>"$exclude"`. When a redirection
    # fails, bash reports the failure to `if !` for a simple command (and for a function call)
    # but NOT for a group or a subshell — the group's status comes back 0 and the failure is
    # invisible. Written as a group, this arm printed `added` for a write that had just been
    # refused, which is the exact defect the state below it exists to report. Found by the
    # test, not by reading.
    if ! printf '%s\n%s\n' "$(exclude_marker)" "$pattern" >>"$exclude" 2>/dev/null; then
        printf 'failed (cannot write %s)\n' "$exclude"
        return 0
    fi
    printf 'added %s\n' "$pattern"
}

# install_git_hooks <repo_root> <hooks_src> — install the repo post-merge hook, plus a chainer
# when `core.hooksPath` redirects git away from it. Prints one `key=value` line per fact:
#
#   repo-hook=<path>          the hook, installed where git actually runs hooks from
#   chainer=<outcome> <path>  installed | up-to-date | refreshed | foreign | failed
#   exclude=<state> <detail>  added | kept | retracted | unowned | none | failed
#   dispatch=<verdict> [path] direct | chained | unknown | dead
#   error=<reason>            nothing was done; ALWAYS the only line, and the only rc 1
#
# Each key reports a STATE, not an action taken. That distinction is the whole design: while
# the keys were actions, every state that arises from *not* acting had no key, no render arm
# and no test — a stale exclude rule, a hook installed where git will never run it, an
# exclusion attempted and failed. Three review findings were that one shape.
#
# `dispatch=` is emitted on EVERY successful run and answers the only question this feature
# exists for: will git run the repo hook? It is derived from the world rather than from the
# chainer outcome word — `[ -x "$chainer" ]` — because `foreign` and `failed` each cover both
# a file that will dispatch and one that will not. Three-valued on purpose: a foreign chainer
# may dispatch perfectly well and we cannot know, so it is `unknown`, never `dead`.
#
# `error=` means nothing was done, so it is never printed after another key. The chainer half
# failing is not that: the repo hook WAS installed, and saying "skipping hook install" under a
# line that just reported installing it is two lies in three lines.
#
# `render_git_hooks_report` renders this; both halves live here so a shell test can drive the
# pair. Inline in setup.sh nothing could execute either — reverting the hooks directory to
# `--git-dir` left the whole gate green while every pull inside a worktree stopped running the
# repo hook, and check.sh and ci.yml both justify the shell-test stage on that reachability.
install_git_hooks() {
    local repo_root="$1" hooks_src="$2" hooks_dir chainer outcome override want

    hooks_dir="$(git_hooks_dir "$repo_root")" || { printf 'error=not a git repo\n'; return 1; }
    mkdir -p "$hooks_dir" || { printf 'error=cannot create %s\n' "$hooks_dir"; return 1; }
    # `install_exec`, never `cp`: the hook being replaced is very often the process that
    # invoked us, and `cp` rewrites its inode underneath the running shell.
    install_exec "$hooks_dir/post-merge" <"$hooks_src" || {
        printf 'error=could not install %s/post-merge\n' "$hooks_dir"; return 1; }
    printf 'repo-hook=%s\n' "$hooks_dir/post-merge"

    override="$(git_hooks_override "$repo_root")"
    if [ -z "$override" ]; then
        printf 'dispatch=direct\n'
        return 0
    fi
    chainer="$override/post-merge"
    # `core.hooksPath` can legitimately point AT the directory git would have used anyway, and
    # then there is nothing to chain to: the hook just installed IS the one git runs. Without
    # this, `install_chainer` compares the repo hook against `chainer_body`, calls it foreign,
    # and setup.sh warns that a file jkb wrote thirty microseconds earlier was not written by
    # jkb — advising the user to check a dispatch line that would be a loop.
    if [ "$chainer" = "$hooks_dir/post-merge" ]; then
        printf 'dispatch=direct\n'
        return 0
    fi

    if mkdir -p "$override" 2>/dev/null; then
        outcome="$(install_chainer "$chainer")"
        [ -n "$outcome" ] || outcome=failed
    else
        outcome=failed
    fi
    printf 'chainer=%s %s\n' "$outcome" "$chainer"

    case "$outcome" in
        installed|up-to-date|refreshed) want=yes ;;
        foreign) want=no ;;
        # A failed install decides nothing: the file at that path may be a chainer jkb wrote
        # before the refresh failed, so both hiding it and revealing it would be a position
        # taken on no evidence. The next successful run reconciles it.
        *) want=skip ;;
    esac
    if [ "$want" = skip ]; then
        printf 'exclude=none (the chainer install failed; nothing was decided)\n'
    else
        printf 'exclude=%s\n' "$(reconcile_exclude "$repo_root" "$chainer" "$want")"
    fi

    if [ "$want" = yes ]; then
        printf 'dispatch=chained %s\n' "$chainer"
    elif [ -f "$chainer" ] && [ -x "$chainer" ]; then
        # `-f` as well as `-x`: a directory is executable to `test` and unrunnable to git, and
        # a directory is exactly what sits there when a directory-style hook manager owns the
        # path (see `install_exec`'s own refusal). Calling that `unknown` would report "this
        # may well dispatch" about the one case that provably cannot.
        printf 'dispatch=unknown %s\n' "$chainer"
    else
        printf 'dispatch=dead %s\n' "$chainer"
    fi
    return 0
}

# render_git_hooks_report — turn install_git_hooks' report on stdin into what a person reads.
#
# It lives here, not inline in setup.sh, for the reason the installer itself moved: nothing
# runs setup.sh, so an inline arm is reachable from no test. Leaving the rendering behind was
# drawing the seam one level too low — two of pass 3's findings were in these arms, and
# reverting either left the whole gate green.
#
# Every `case` has a default arm that WARNS. A key added to the producer with no arm here then
# surfaces at runtime instead of vanishing, which is how an `error=` line contradicting the
# line above it stayed invisible.
render_git_hooks_report() {
    local line rest state detail
    while IFS= read -r line; do
        # `${rest#* }` returns `$rest` unchanged when there is no space, which would make the
        # detail a copy of the state. Split explicitly.
        rest="${line#*=}"
        state="${rest%% *}"
        case "$rest" in *' '*) detail="${rest#* }" ;; *) detail="" ;; esac
        case "$line" in
            repo-hook=*) printf '  • repo hook:  %s\n' "$rest" ;;
            chainer=*)
                case "$state" in
                    installed)  printf '  • chainer:    %s (core.hooksPath is set, so this is required)\n' "$detail" ;;
                    up-to-date) printf '  • chainer:    %s (up to date)\n' "$detail" ;;
                    refreshed)  printf '  • chainer:    %s (refreshed)\n' "$detail" ;;
                    # Not byte-for-byte something jkb wrote, so it is not ours to replace — it
                    # may be your own file, or ours with your edits in it.
                    foreign)    warn "$detail was not written by jkb (or has been edited) — left untouched." ;;
                    failed)     warn "could not install the chainer at $detail" ;;
                    *)          warn "unrecognised chainer outcome: $line" ;;
                esac ;;
            exclude=*)
                case "$state" in
                    added)      printf '  • excluded:   %s (inside the working tree; added to .git/info/exclude)\n' "$detail" ;;
                    kept)       printf '  • excluded:   %s (already in .git/info/exclude)\n' "$detail" ;;
                    retracted)  printf '  • excluded:   %s dropped from .git/info/exclude (that chainer is not jkb'"'"'s to hide)\n' "$detail" ;;
                    unowned)    warn "$detail is excluded by a rule in .git/info/exclude that jkb cannot prove it wrote."
                                warn "  it is hiding that file from \`git status\` — remove the line yourself if you did not add it." ;;
                    none)       : ;;   # nothing to hide, and so nothing worth a line
                    failed)     warn "could not update .git/info/exclude $detail"
                                warn "  the chainer will read as untracked, so the tree looks dirty and \`jkb task land\` refuses it." ;;
                    *)          warn "unrecognised exclude state: $line" ;;
                esac ;;
            dispatch=*)
                case "$state" in
                    # Both good outcomes: git reads .git/hooks itself, or our chainer sends it
                    # there. The lines above have already said so.
                    direct|chained) : ;;
                    unknown) warn "  if $detail does not exec \"\$(git rev-parse --git-common-dir)/hooks/post-merge\", the repo hook never runs." ;;
                    dead)    warn "core.hooksPath is set and nothing runnable is at $detail — git will NOT run the repo hook above." ;;
                    *)       warn "unrecognised dispatch verdict: $line" ;;
                esac ;;
            error=*) warn "$rest; skipping hook install" ;;
            '') : ;;
            *) warn "unrecognised report line: $line" ;;
        esac
    done
}
