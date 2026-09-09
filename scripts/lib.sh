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

# _real_dir <path> — a directory's physical path, or the path itself when it does not exist.
# Two spellings of one directory (a trailing slash, a symlink, a `..`) must compare equal.
_real_dir() {
    if [ -d "$1" ]; then (cd "$1" 2>/dev/null && pwd -P) || printf '%s\n' "$1"
    else printf '%s\n' "$1"; fi
}

# git_hooks_override <repo_root> — print the absolute `core.hooksPath` in effect for
# <repo_root>, or nothing when there is none. Returns 0 for both of those — "no override" is
# an answer, not a failure — and **2** when the setting exists and git will not resolve it,
# which is neither.
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
    # `git config --get` exits 1 for "not set" and something else for "could not read it" —
    # a `~someuser/hooks` for an account absent on this machine exits 128 (`failed to expand
    # user dir`), which is the ordinary state of a dotfiles-shared ~/.gitconfig. Folding the
    # second into the first reported the BEST verdict (`dispatch=direct`, rendered silently)
    # for a repo in which git cannot resolve its hooks path at all. `dispatch` is three-valued
    # exactly so an unestablished answer is not spelled as the good one.
    configured="$(git -C "$repo_root" config --get --path core.hooksPath 2>/dev/null)"
    case "$?" in
        0) ;;
        1) return 0 ;;      # genuinely not set
        *) return 2 ;;      # set to something git will not resolve
    esac
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
# It names its PURPOSE as well as its author, because jkb is not the only writer of marked
# blocks in this file: `session::ensure_excluded` (crates/jkb-cli/src/session.rs) writes
# `# jkb task sessions (git worktrees)` + `/.jkb/` from Rust. That block survives every sweep
# here precisely because its marker is not in `exclude_known_markers` — never add it.
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

# _exposure_answer <absolute dir> <tops> <skip, or empty> — the one answer for "this path is
# not anchorable to the main checkout": `exposed …` if it is inside some working tree anyway,
# `none …` if it is inside none.
#
# One function because there are two branches that ask it — bare and non-bare — and two copies
# of a four-line answer are two answers waiting to drift. The distinction they carry is the one
# this area keeps getting wrong: `none` is proven absence and licenses the sweep to retract;
# `exposed` is a real untracked file in a real tree that jkb is declining to hide.
_exposure_answer() {
    local dir="$1" tops="$2" skip="$3" hit
    hit="$(_worktree_containing "$dir" "$tops" "$skip")" || hit=""
    if [ -n "$hit" ]; then
        printf 'exposed (the chainer is inside the worktree at %s; an anchored rule would hide that path in every worktree, so it is left visible)\n' "$hit"
    else
        printf 'none (core.hooksPath is outside every working tree, so nothing of ours is hidden)\n'
    fi
}

# git_hooks_exclude_pattern <repo_root> — the ONE exclude pattern jkb wants in this
# repository, as exactly one of FOUR lines:
#
#   pattern <glob>       this is what must be excluded
#   none <reason>        PROVEN absence: nothing of ours is inside any working tree
#   exposed <reason>     ours IS inside a working tree and an anchored rule cannot hide it
#   undecided <reason>   git would not answer; nothing was established, so change nothing
#
# The last three are not interchangeable and the caller must not collapse them: `none` licenses
# the sweep to retract, `undecided` must not, and `exposed` is a warning. Reading two shapes
# from a header that described two is how `undecided` came to be spelled `none` in the first
# place.
#
# THE RULE THIS FUNCTION EXISTS FOR: the desired state is a pure function of facts every
# worktree shares — the raw `core.hooksPath` value, the common dir, the main checkout — and
# never of the worktree that happens to be running. `.git/info/exclude` lives in the common
# dir and applies to every worktree at once, so a desired state computed from
# `--show-toplevel` differs per run, and a sweep that enforces it turns that disagreement
# into a flip-flop: a main-checkout run added the block, the next run from a `jkb task work`
# session retracted it, and the main checkout read dirty in between — which is exactly the
# state `jkb task land` refuses. D36 makes that the normal case, not a corner one.
#
# So the pattern comes from the CONFIG STRING, not from resolving a path and stripping a
# toplevel back off it. A relative `core.hooksPath` is resolved by git against each
# worktree's own top (githooks(5)), so one anchored pattern is simultaneously right for all
# of them. An absolute one is inside at most one tree, and is only hidden when that tree is
# the main checkout: an anchored pattern applies to every worktree, so hiding a path that
# exists only inside one linked session would hide any same-named path in all the others.
#
# Residual, stated rather than detected: `extensions.worktreeConfig` can make
# `core.hooksPath` genuinely per-worktree, and then no shared desired state exists. Nothing
# here detects that; the failure is a pattern computed for one worktree's value, which is the
# pre-existing behaviour rather than a new one.
git_hooks_exclude_pattern() {
    local repo_root="$1" configured rc rel line wt hit=""
    local tops="" main_top="" bare=0 rec=0
    # Every worktree's top, main first, and whether the main entry is BARE. Asked of the
    # repository through the porcelain rather than `--is-bare-repository` of `$repo_root`:
    # in a bare-repo-plus-worktrees layout that question answers `false` from a worktree, the
    # bare git dir became the "main checkout", and a pattern derived from it marked the user's
    # own untracked file ignored while hiding nothing — and gave two different answers from
    # two places, falsifying this function's whole claim.
    # A worktree path containing a newline would break this parse; `--porcelain -z` is the
    # cure and needs git 2.36 plus `read -d ''`. Stated rather than built: the failure is a
    # wrong `none`, which hides nothing and destroys nothing.
    while IFS= read -r line; do
        case "$line" in
            "worktree "*)
                rec=$((rec + 1))
                wt="$(_real_dir "${line#worktree }")"
                if [ "$rec" -eq 1 ]; then main_top="$wt"; fi
                tops="$tops$wt
"
                ;;
            # Only the FIRST record's `bare`. The guard here used to test `first -eq 0`, true
            # of every record after the first, and a `$hit` that is always empty at this point.
            bare) if [ "$rec" -eq 1 ]; then bare=1; fi ;;
        esac
    done <<EOF
$(git -C "$repo_root" worktree list --porcelain 2>/dev/null)
EOF
    if [ -z "$main_top" ]; then
        printf 'undecided (the repository'"'"'s worktrees could not be listed)\n'
        return 0
    fi
    configured="$(git -C "$repo_root" config --get --path core.hooksPath 2>/dev/null)"
    rc=$?
    case "$rc" in
        0) ;;
        1) printf 'none (no core.hooksPath, so nothing of ours is inside the tree)\n'; return 0 ;;
        # `undecided`, not `none`: `none` means PROVEN absence, and the caller turns that
        # into want=no — so a transient git failure swept jkb's own block away and reported
        # "jkb no longer stands behind hiding it", which is false. jkb could not check.
        # `git_hooks_override` already answers rc 2 and `dispatch=unreadable` for this exact
        # condition; one fact must not have two verdicts.
        *) printf 'undecided (core.hooksPath could not be read)\n'; return 0 ;;
    esac
    [ -n "$configured" ] || { printf 'none (core.hooksPath is empty)\n'; return 0; }

    case "$configured" in
        /*)
            configured="$(_real_dir "$configured")"
            # Bare-ness disqualifies THIS branch only, and that is the whole of what it means
            # here: a bare repository has no checkout to anchor an absolute path against, so
            # `$main_top` is the bare git dir. Asked at the top of the function it answered for
            # every branch — and a RELATIVE `core.hooksPath` in a bare-repo-plus-worktrees
            # layout needs no main checkout at all. It resolves inside each linked worktree,
            # exactly as it does anywhere else, so returning `none` there dropped a working
            # exclusion and made jkb retract the block it had written itself.
            if [ "$bare" -eq 1 ]; then
                # Skip the main entry: `worktree list` reports the bare git dir as a record,
                # and it is not a working tree — naming it as one told the user their chainer
                # was inside a tree that does not exist.
                _exposure_answer "$configured" "$tops" "$main_top"
                return 0
            fi
            if [ "$configured" = "$main_top" ]; then
                rel=""
            else
                case "$configured/" in
                    "$main_top"/*) rel="${configured#"$main_top"/}" ;;
                    *)
                        # Not in the main checkout. If it is inside SOME other worktree the
                        # chainer is a real untracked file in a real tree, and saying `none`
                        # — which the renderer prints nothing for — left that tree dirty for
                        # ever with nothing attributing the file to jkb. An anchored rule
                        # applies to every tree at once, so hiding it is not available; being
                        # quiet about it is not the same trade.
                        # No skip: this arm is reached only when `$configured` did NOT
                        # match `"$main_top"/*`, so main cannot match here either.
                        _exposure_answer "$configured" "$tops" ""
                        return 0 ;;
                esac
            fi
            ;;
        *)
            rel="$(_normalize_rel "$configured")" \
                || { printf 'none (core.hooksPath resolves above the working tree)\n'; return 0; }
            ;;
    esac
    # An empty `rel` is the tree root, which is a real place to hide something, not a reason
    # to give up: `core.hooksPath = .` puts the chainer at `<root>/post-merge`.
    printf 'pattern /%s\n' "${rel:+$rel/}post-merge"
}

# _worktree_containing <absolute dir> <tops, newline-separated> <skip, or empty> — print the
# working tree that contains <absolute dir>, if any.
#
# A `read` loop, never `for x in $tops`: that word-splits on spaces, and a checkout under
# `~/My Projects/` is not exotic. One implementation because there are two callers — the bare
# and non-bare branches — and two would be two ideas of what "contains" means.
_worktree_containing() {
    local dir="$1" tops="$2" skip="$3" wt
    while IFS= read -r wt; do
        [ -n "$wt" ] || continue
        [ -n "$skip" ] && [ "$wt" = "$skip" ] && continue
        case "$dir/" in "$wt"/*) printf '%s' "$wt"; return 0 ;; esac
    done <<EOF
$tops
EOF
    return 1
}

# _normalize_rel <relative path> — the path as a clean sequence of segments, or non-zero if
# it escapes the tree. Empty output means "the tree root itself".
#
# Segment-wise, not a couple of prefix strips: `.`, `a/.`, `a//b`, `.//x` and `hooks/./x` are
# all legal `core.hooksPath` values that a pair of strips turns into `/./post-merge`,
# `/a/./post-merge`, `/a//b/post-merge` … — patterns git does not match. Nothing is destroyed,
# but the chainer stays visible and the tree reads dirty for ever, which is the whole failure
# this exclusion exists to prevent, arrived at quietly.
_normalize_rel() {
    local rest="$1" out="" seg
    while [ -n "$rest" ]; do
        seg="${rest%%/*}"
        case "$rest" in */*) rest="${rest#*/}" ;; *) rest="" ;; esac
        case "$seg" in
            ''|.) continue ;;
            ..)
                # An INTERIOR `..` resolves: `x/../y` is `y`, plainly inside the tree.
                # Refusing every `..` reported "escapes the working tree" for a path that
                # does not, and the reason was printed as a silent `none` — so the chainer
                # stayed visible and the tree read dirty, which is the failure this exists
                # to prevent, under a message that was false about why.
                [ -n "$out" ] || return 1
                # Pop one segment. `${out%/*}` alone is a no-op when `out` holds a single
                # segment with no `/` in it, which left `x/../y` as `x/y`.
                case "$out" in
                    */*) out="${out%/*}" ;;
                    *) out="" ;;
                esac
                continue
                ;;
        esac
        out="${out:+$out/}$seg"
    done
    printf '%s' "$out"
}

# _is_exclude_pattern_line <line> — can this line be the pattern half of a jkb block?
#
# Positive definition, so every bad shape follows from it instead of being a case to
# remember: non-empty, not itself a marker, and not a comment. A marker paired with a marker
# used to be read as a block whose "pattern" was the marker's own text — so jkb retracted
# BOTH marker lines, printed `retracted # jkb: …`, and left a bare pattern it would then
# report `unowned` and refuse to touch for ever. That is the silent-and-permanent harm the
# marked-block rule exists to end, caused by the parser.
_is_exclude_pattern_line() {
    local line
    line="$(_exclude_line "$1")"
    [ -n "$line" ] || return 1
    _is_exclude_marker "$line" && return 1
    case "$line" in '#'*) return 1 ;; esac
    return 0
}

# reconcile_exclude <repo_root> <pattern, or empty> <want> [reason] — make
# `.git/info/exclude` agree with the desired state, printing complete report lines:
#
#   exclude=added <pattern>          we wrote our marked block
#   exclude=kept <pattern>           a rule already excludes it (ours, or the user's)
#   exclude=retracted <pattern>      a block of ours was there and should not be; it is gone
#   exclude=deduplicated <pattern>   extra copies of the block we are keeping were removed
#   exclude=tidied <n> marker(s)     orphaned jkb marker lines were removed
#   exclude=unowned <pattern>        a rule excludes it that jkb cannot prove it wrote — kept
#   exclude=exposed <reason>         ours IS inside a working tree and cannot be hidden there
#   exclude=undecided <reason>       nothing could be established; nothing was changed
#   exclude=none <reason>            nothing to do
#   exclude=failed <reason>          a write was attempted and did not land
#
# <want> is four-valued, because there are two different unknowns here and sharing one word
# for them cost a stranded block. `yes` — we own this pattern and it must be excluded. `no` —
# nothing of ours may hide it. `undecided` — the chainer install failed, so nothing is known
# about THIS pattern and its block is left as found, while every other block is still swept.
# `unknown` — the derivation itself could not answer, so nothing at all is touched. Anything
# else is refused rather than falling through to a branch that sweeps.
#
# So the sweep of every OTHER jkb block runs on three of the four — `yes`, `no` and
# `undecided` — and not on `unknown`. It does not depend on the undecided fact: it is the
# condition, and a condition must dominate every arm rather than have one that opts out.
#
# WHAT JKB SWEEPS is only blocks bearing a marker in `exclude_known_markers` — its own, by
# byte identity. `session::ensure_excluded` (crates/jkb-cli/src/session.rs) writes a DIFFERENT
# marked block into this same file for `/.jkb/`, and it survives untouched precisely because
# its marker is not in that list. Every other line — bare patterns, other writers' blocks,
# the user's comments — belongs to the user and is never removed.
#
# Always returns 0 — `failed` is a word, not an exit status (see the header's `set -e` rule).
reconcile_exclude() {
    local repo_root="$1" pattern="$2" want="$3" empty_report="${4:-}"
    local common exclude tmp line nxt cur keep=""
    local -a lines=() out=() removed=() deduped=()
    local i n seen_keep=0 changed=0 probe_retracted=0 tidied=0

    common="$(git -C "$repo_root" rev-parse --git-common-dir 2>/dev/null)" || common=""
    if [ -z "$common" ]; then
        printf 'exclude=none (not a git repository)\n'
        return 0
    fi
    case "$common" in /*) ;; *) common="$repo_root/$common" ;; esac
    exclude="$common/info/exclude"

    # An unrecognised `want` refuses. The fall-through was the `no` branch, which SWEEPS: a
    # caller typo, or a fifth word added at one site and forgotten here, silently retracted
    # every block jkb owns and then printed a line the renderer reads as "nothing was changed".
    # This very change added a fourth word at the caller and a matching arm here; had that arm
    # been missed, that is what it would have done, unattended, from the post-merge hook. Every
    # renderer in this file has a warning default arm; the one consumer that can destroy the
    # user's file had none.
    case "$want" in
        yes|no|undecided|unknown) ;;
        *) printf 'exclude=failed (unrecognised want %s; nothing was changed)\n' "$want"; return 0 ;;
    esac

    # `unknown` — the DERIVATION could not establish anything — touches nothing at all:
    # sweeping with `keep=""` would retract every block jkb owns on the strength of an answer
    # git refused to give. It is its own word rather than a second meaning for `undecided`,
    # which says only "nothing is decided about THIS pattern" and must still sweep the others;
    # sharing one word made a failed chainer install with a decidably-empty pattern skip the
    # sweep too, stranding a stale block permanently.
    if [ "$want" = unknown ]; then
        printf 'exclude=%s\n' "${empty_report:-undecided (nothing could be established)}"
        return 0
    fi
    # `yes` and `undecided` both preserve this pattern's block; only `yes` may add or dedupe.
    case "$want" in yes|undecided) keep="$pattern" ;; esac

    if [ -f "$exclude" ]; then
        while IFS= read -r line || [ -n "$line" ]; do lines+=("$line"); done <"$exclude"
    fi
    n=${#lines[@]}
    i=0
    while [ "$i" -lt "$n" ]; do
        cur="${lines[$i]}"
        if _is_exclude_marker "$cur"; then
            nxt=""
            if [ "$((i + 1))" -lt "$n" ] && _is_exclude_pattern_line "${lines[$((i + 1))]}"; then
                nxt="$(_exclude_line "${lines[$((i + 1))]}")"
            fi
            if [ -n "$nxt" ]; then
                if [ -n "$keep" ] && [ "$nxt" = "$keep" ]; then
                    if [ "$seen_keep" -eq 0 ] || [ "$want" != yes ]; then
                        seen_keep=1
                        out+=("$cur" "${lines[$((i + 1))]}")
                    else
                        deduped+=("$nxt")
                        changed=1
                    fi
                else
                    removed+=("$nxt")
                    [ "$nxt" = "$pattern" ] && probe_retracted=1
                    changed=1
                fi
                i=$((i + 2))
                continue
            fi
            # An orphaned marker: ours by byte identity, inert to git (it is a comment), and
            # the seed of the mis-pairing above if it is left to meet a future block.
            tidied=$((tidied + 1))
            changed=1
            i=$((i + 1))
            continue
        fi
        out+=("$cur")
        i=$((i + 1))
    done

    if [ "$changed" -eq 1 ]; then
        # Through a temp file: this holds the USER'S rules and a partial in-place rewrite
        # destroys them. `cp -p` first so the replacement inherits the destination's mode.
        tmp="$exclude.jkb.$$"
        if ! cp -p "$exclude" "$tmp" 2>/dev/null; then
            rm -f "$tmp"
            printf 'exclude=failed (cannot write %s)\n' "$exclude"
            return 0
        fi
        # The write status is CHECKED: `printf` to a full disk fails after emitting part of
        # its output, and dropping that status renamed a truncated file over every rule the
        # user owns, under the word `retracted`.
        if [ "${#out[@]}" -eq 0 ]; then
            : >"$tmp" || { rm -f "$tmp"; printf 'exclude=failed (cannot write %s)\n' "$exclude"; return 0; }
        elif ! printf '%s\n' "${out[@]}" >"$tmp"; then
            rm -f "$tmp"
            printf 'exclude=failed (cannot write %s)\n' "$exclude"
            return 0
        fi
        if ! mv -f "$tmp" "$exclude"; then
            rm -f "$tmp"
            printf 'exclude=failed (cannot write %s)\n' "$exclude"
            return 0
        fi
        # Guarded: `"${arr[@]}"` on an EMPTY array under `set -u` is an unbound-variable error
        # on bash before 4.4, which is what macOS ships as /bin/bash.
        if [ "${#removed[@]}" -gt 0 ]; then
            for line in "${removed[@]}"; do printf 'exclude=retracted %s\n' "$line"; done
        fi
        if [ "${#deduped[@]}" -gt 0 ]; then
            for line in "${deduped[@]}"; do printf 'exclude=deduplicated %s\n' "$line"; done
        fi
        [ "$tidied" -eq 0 ] || printf 'exclude=tidied %s orphaned marker(s)\n' "$tidied"
    fi

    if [ -z "$pattern" ]; then
        printf 'exclude=%s\n' "${empty_report:-none (nothing of ours is inside the tree)}"
        return 0
    fi
    if [ "$want" = undecided ]; then
        # `undecided`, like the other way of reaching this: one condition, one word. It said
        # `none` — proven absence — for a situation whose whole point is that nothing was
        # established.
        printf 'exclude=undecided (the chainer install failed; nothing was decided about %s)\n' "$pattern"
        return 0
    fi
    if [ "$want" = yes ]; then
        if [ "$seen_keep" -eq 1 ] || _exclude_mentions "$exclude" "$pattern"; then
            printf 'exclude=kept %s\n' "$pattern"
            return 0
        fi
        mkdir -p "$common/info" 2>/dev/null \
            || { printf 'exclude=failed (cannot create %s/info)\n' "$common"; return 0; }
        # Separator first. An exclude file that does not end in a newline — hand-edited ones
        # often do not — would otherwise have its last rule fused with our marker (`*.log` +
        # `# jkb: …`), destroying a rule the user owns while our own pattern stayed inert,
        # under a success message. `session::ensure_excluded` computes the same `sep` for the
        # same reason — one rule with an implementation in each language.
        if [ -s "$exclude" ] && [ -n "$(tail -c 1 "$exclude")" ]; then
            printf '\n' >>"$exclude" 2>/dev/null \
                || { printf 'exclude=failed (cannot write %s)\n' "$exclude"; return 0; }
        fi
        # ONE simple command, not a `{ …; }` group: when a redirection fails, bash reports it
        # to `if !` for a simple command and a function call, but NOT for a group or subshell.
        if ! printf '%s\n%s\n' "$(exclude_marker)" "$pattern" >>"$exclude" 2>/dev/null; then
            printf 'exclude=failed (cannot write %s)\n' "$exclude"
            return 0
        fi
        printf 'exclude=added %s\n' "$pattern"
        return 0
    fi

    # want=no. Our own block, if there was one, was already reported `retracted`.
    [ "$probe_retracted" -eq 1 ] && return 0
    if _exclude_mentions "$exclude" "$pattern"; then
        # The one case this cannot repair: something hides the file and jkb cannot prove it
        # put it there, so it says so rather than deleting a rule that may be the user's.
        printf 'exclude=unowned %s\n' "$pattern"
    else
        printf 'exclude=none (nothing is hiding it)\n'
    fi
}

# install_git_hooks <repo_root> <hooks_src> — install the repo post-merge hook, plus a chainer
# when `core.hooksPath` redirects git away from it. Prints one `key=value` line per fact:
#
#   repo-hook=<path>          the hook, installed where git actually runs hooks from
#   chainer=<outcome> <path>  installed | up-to-date | refreshed | foreign | failed
#   exclude=<state> <detail>  added | kept | retracted | deduplicated | tidied | unowned |
#                             exposed | undecided | none | failed  (repeatable: the sweep
#                             reports one line per block). Three are easy to confuse: `none` is
#                             PROVEN absence, `exposed` means ours IS in a working tree and jkb
#                             is declining to hide it, and `undecided` means jkb could not
#                             establish anything and therefore changed nothing.
#   dispatch=<verdict> [detail] direct | chained | unknown | dead | unreadable
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
# a file that will dispatch and one that will not. `unknown` exists on purpose: a foreign
# chainer may dispatch perfectly well and we cannot know, so it is never spelled `dead` —
# and neither is `unreadable`, which is a `core.hooksPath` git itself will not resolve, so no
# hook runs in the repository at all.
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
    local repo_root="$1" hooks_src="$2"
    local hooks_dir chainer="" outcome override override_rc=0
    local want=no ours=unknown verdict="" pat_line pattern="" pattern_reason=""

    # The two `error=` arms stay AHEAD of the funnel below: `error=` means nothing was done,
    # it is contractually the only line, and there is no exclude question to answer before a
    # repo hook exists at all.
    hooks_dir="$(git_hooks_dir "$repo_root")" || { printf 'error=not a git repo\n'; return 1; }
    mkdir -p "$hooks_dir" || { printf 'error=cannot create %s\n' "$hooks_dir"; return 1; }
    # `install_exec`, never `cp`: the hook being replaced is very often the process that
    # invoked us, and `cp` rewrites its inode underneath the running shell.
    install_exec "$hooks_dir/post-merge" <"$hooks_src" || {
        printf 'error=could not install %s/post-merge\n' "$hooks_dir"; return 1; }
    printf 'repo-hook=%s\n' "$hooks_dir/post-merge"

    # --- plan: decide `want` and the dispatch verdict, and print the chainer line ----------
    # Every arm here SETS variables; none of them returns. The reconcile below is not inside
    # any of them, so a fifth arm added later cannot skip it — the property lives in the
    # structure rather than in each arm's memory. It had to: the `chainer install failed` arm
    # returned early, and a stale block therefore survived for ever whenever the failing
    # precondition was itself persistent.
    override="$(git_hooks_override "$repo_root")" || override_rc=$?
    if [ "$override_rc" -eq 2 ]; then
        verdict="unreadable core.hooksPath"
    elif [ -z "$override" ]; then
        verdict="direct"
    elif [ "$(_real_dir "$override")" = "$(_real_dir "$hooks_dir")" ]; then
        # `core.hooksPath` may legitimately point AT the directory git would have used anyway,
        # and then there is nothing to chain to: the hook just installed IS the one git runs.
        # Resolved directories, not literal strings — a trailing slash, a symlink and a `..`
        # are three spellings of one directory, and literal equality put back the warning that
        # jkb had not written its own hook.
        verdict="direct"
    else
        chainer="$override/post-merge"
        if mkdir -p "$override" 2>/dev/null; then
            outcome="$(install_chainer "$chainer")"
            [ -n "$outcome" ] || outcome=failed
        else
            outcome=failed
        fi
        printf 'chainer=%s %s\n' "$outcome" "$chainer"
        case "$outcome" in
            # `ours` is the ownership fact on its own, because `want` answers a different
            # question and gets collapsed on its way to the funnel. THREE-valued, like every
            # other answer here: a failed install leaves a file that may well be one jkb
            # wrote — three lines down this arm says exactly that — and spelling that unknown
            # as a definite `no` dropped the dirty-worktree warning and asserted "the file at
            # that path is not one jkb wrote" about a file jkb did write.
            installed|up-to-date|refreshed) want=yes; ours=yes ;;
            foreign) want=no; ours=no ;;
            # A failed install decides nothing about THIS pattern — the file there may be a
            # chainer jkb wrote before the refresh failed — but it decides nothing about the
            # other blocks either, and they need no decision.
            *) want=undecided ;;
        esac
        # Derived from the world, not from the outcome word: `foreign` and `failed` each cover
        # a file that will dispatch and one that will not. `-f` as well as `-x`, because a
        # directory is executable to `test` and unrunnable to git.
        # `ours`, not `want`: the same three outcomes, but `want` is collapsed to `no` by the
        # pattern-empty rule in the funnel, so reading it here is correct only because this
        # block happens to run first. Two live names for one fact, one carrying an unstated
        # ordering requirement — move this below the funnel and a healthy jkb chainer on an
        # `exposed` path reports `dispatch=unknown`, whose renderer warns that jkb's own
        # working chainer may never run.
        if [ "$ours" = yes ]; then
            verdict="chained $chainer"
        elif [ -f "$chainer" ] && [ -x "$chainer" ]; then
            verdict="unknown $chainer"
        else
            verdict="dead $chainer"
        fi
    fi

    # --- the funnel: unconditional, on every path above -----------------------------------
    pat_line="$(git_hooks_exclude_pattern "$repo_root")"
    case "$pat_line" in
        "pattern "*) pattern="${pat_line#pattern }" ;;
        # The derivation could not establish anything. `unknown`, not `undecided`: those are
        # two different unknowns and `reconcile_exclude` treats them differently — this one
        # touches nothing at all, rather than sweeping on the strength of an answer git
        # refused to give.
        "undecided "*) pattern=""; pattern_reason="$pat_line"; want=unknown ;;
        # Not a pattern: the helper already worded the whole report line — `none …`
        # (nothing of ours anywhere, rendered silently) or `exposed …` (ours IS in a tree
        # and we are declining to hide it, which the renderer warns about).
        *) pattern=""; pattern_reason="$pat_line" ;;
    esac
    # `exposed` says "the chainer JKB INSTALLED is visible in that tree". The derivation knows
    # only the path, so it cannot tell whether jkb wrote the file there — and for a `foreign`
    # chainer it warned, on every unattended pull, that jkb's file was dirtying a tree, about a
    # file the user wrote and jkb had refused to touch three lines earlier.
    #
    # Gated on `$ours`, NOT on `$want`: `want` is collapsed to `no` by the pattern-empty rule
    # below, and an exposed line is by definition pattern-empty — so reading `want` here made
    # the downgrade unconditional and the state unreachable. jkb's own chainer in a worktree
    # went back to being reported as nothing at all, which is what `exposed` was added to stop.
    # This runs BEFORE that rule, and asks the question it actually means.
    case "$pat_line" in
        "exposed "*)
            # TWO questions, and both must be yes. Ownership: suppressed on a PROVEN
            # `foreign`, which is the user's own file and none of jkb's business. And
            # EXISTENCE, asked of the world the way the dispatch verdict asks it — because
            # this is a claim about a file. A failed install can leave nothing at that path at
            # all, and then the report said "the chainer there is not hidden … that working
            # tree will read dirty" beside `dispatch=dead` ("nothing runnable is at …") about
            # an empty directory in a clean tree: two contradictory statements in one report.
            # The comment that used to sit here asserted the premise — "the file is untracked
            # in a real tree either way" — that is false in exactly that case.
            if [ "$ours" = no ]; then
                pattern_reason="none (the file at that path is not one jkb wrote)"
            elif [ ! -e "$chainer" ]; then
                pattern_reason="none (nothing was installed at that path, so nothing of ours is visible there)"
            fi
            ;;
    esac
    # Nothing to own means nothing to want, whatever the chainer did — unless the derivation
    # said it could not tell, in which case not knowing is the answer.
    if [ -z "$pattern" ] && [ "$want" != unknown ]; then want=no; fi
    reconcile_exclude "$repo_root" "$pattern" "$want" "$pattern_reason"

    printf 'dispatch=%s\n' "$verdict"
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
                    # One true sentence for all three causes — the chainer went foreign,
                    # core.hooksPath moved, core.hooksPath was unset. The old parenthetical
                    # named only the first and was false on the other two.
                    retracted)  printf '  • excluded:   %s dropped from .git/info/exclude (jkb no longer stands behind hiding it)\n' "$detail" ;;
                    # Not a retraction: extra copies of the block being KEPT. Reporting it as
                    # one printed `retracted P` and `kept P` about the same pattern.
                    deduplicated) printf '  • excluded:   duplicate jkb entries for %s removed\n' "$detail" ;;
                    tidied)     printf '  • excluded:   %s removed from .git/info/exclude\n' "$detail" ;;
                    unowned)    warn "$detail is excluded by a rule in .git/info/exclude that jkb cannot prove it wrote."
                                warn "  it is hiding that file from \`git status\` — remove the line yourself if you did not add it." ;;
                    none)       : ;;   # nothing to hide, and so nothing worth a line
                    undecided)  warn "could not work out what to hide from git: $detail"
                                warn "  nothing in .git/info/exclude was changed." ;;
                    exposed)    warn "the chainer there is not hidden from git: $detail"
                                warn "  that working tree will read dirty, and \`jkb task land\` refuses a dirty target." ;;
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
                    unreadable)
                             warn "$detail is set to something git will not resolve, so git runs NO hooks in this repository."
                             warn "  check it with: git config --get --path core.hooksPath" ;;
                    *)       warn "unrecognised dispatch verdict: $line" ;;
                esac ;;
            error=*) warn "$rest; skipping hook install" ;;
            '') : ;;
            *) warn "unrecognised report line: $line" ;;
        esac
    done
}

# --- what setup.sh finished with ----------------------------------------------------------
# render_setup_summary — turn setup.sh's closing `key=state [detail]` report on stdin into the
# lines a person reads.
#
# Here, not inline in setup.sh, for the third time and the same reason: NOTHING executes
# setup.sh, so an arm written there is reachable from no test. That is not a hypothetical cost
# — the summary has now produced a finding in three consecutive review rounds (the watcher line
# claiming "running" after activation failed, the roots line asserting roots the scaffold had
# just failed to create, the extension line telling you to reload for a build that was never
# made), and each was invisible to a green gate. `install_git_hooks` and
# `render_git_hooks_report` moved here for exactly this; the summary is the last block that
# had not.
#
# The protocol, closed:
#
#   jkb=<path>                 where the binary is
#   database=<path>            the db this run used
#   scaffold=<state> [detail]  created | untouched | skipped | failed
#   extension=<state>          installed | skipped | failed
#   watcher=<state>            running | skipped | failed
#
# Every `case` has a default arm that warns, so a state added to the producer with no arm here
# surfaces at runtime instead of vanishing.
render_setup_summary() {
    local line rest state detail
    while IFS= read -r line; do
        rest="${line#*=}"
        state="${rest%% *}"
        case "$rest" in *' '*) detail="${rest#* }" ;; *) detail="" ;; esac
        case "$line" in
            jkb=*)      printf '  • jkb:        %s\n' "$rest" ;;
            database=*) printf '  • database:   %s\n' "$rest" ;;
            scaffold=*)
                case "$state" in
                    created)   printf '  • roots:      repos/ tasks/ media/ references/ memory/ (+ _sys/)\n' ;;
                    # NOT an assertion that the roots exist. The database was already there, so
                    # this run created nothing and checked nothing — and the previous wording
                    # asserted five roots for a KB some other command may have made with one.
                    untouched) printf '  • roots:      not verified (an existing KB was left untouched)\n'
                               printf '                run: jkb --db '"'"'%s'"'"' ns mk repos tasks media references memory\n' "$detail" ;;
                    skipped)   printf '  • roots:      skipped (--no-scaffold)\n' ;;
                    # The remedy is the command that repairs it, not "re-run setup.sh": the
                    # failure leaves the db file behind, so a re-run takes the `existing KB —
                    # left untouched` arm and never retries.
                    failed)    printf '  • roots:      NOT created — run: jkb --db '"'"'%s'"'"' ns mk repos tasks media references memory\n' "$detail" ;;
                    *)         warn "unrecognised scaffold state: $line" ;;
                esac ;;
            extension=*)
                case "$state" in
                    installed) printf '  • extension:  reload VS Code ('"'"'Developer: Reload Window'"'"') to activate\n' ;;
                    skipped)   printf '  • extension:  skipped (--no-extension)\n' ;;
                    failed)    printf '  • extension:  NOT installed; see the warnings above\n' ;;
                    *)         warn "unrecognised extension state: $line" ;;
                esac ;;
            watcher=*)
                case "$state" in
                    running) printf '  • watcher:    running; file edits under mounts auto-sync\n' ;;
                    skipped) printf '  • watcher:    skipped (--no-service)\n' ;;
                    failed)  printf '  • watcher:    NOT running; see the warnings above to activate it\n' ;;
                    *)       warn "unrecognised watcher state: $line" ;;
                esac ;;
            '') : ;;
            *) warn "unrecognised summary line: $line" ;;
        esac
    done
}

# --- the repo's shell -----------------------------------------------------------------------
# shell_sources — every shell file in the repo, one per line.
#
# ONE list, because it is consumed by `scripts/check.sh` and by `.github/workflows/ci.yml` and
# a hand-written copy in each drifted the moment one gained a `*.md` skip the other lacked:
# green locally, red in CI, on the same tree. CI sources this file and calls the function.
#
# A shebang test, not a denylist of extensions: `scripts/hooks/post-merge` has no `.sh`, and
# `*.md|*.json` only names the two non-shell things that happen to be there today — the next
# `.txt` dropped into `.claude/hooks/` would be handed to `bash -n` and reported as a syntax
# error it does not have.
shell_sources() {
    local root="$1" f head
    # Every glob is `*`. Two of them were still `*.sh`, so an extensionless shell script in
    # `scripts/` or `.container/` — the exact shape that motivated selecting by shebang —
    # fell outside the gate. The `*.sh` short-circuit stays for the two files that ARE shell
    # and have no shebang, being sourced rather than run (`lib.sh`, `harness.sh`).
    for f in "$root"/scripts/* "$root"/scripts/tests/* "$root"/scripts/hooks/* \
             "$root"/.claude/hooks/* "$root"/.container/*; do
        [ -f "$f" ] || continue
        case "$f" in
            *.sh) printf '%s\n' "$f"; continue ;;
        esac
        # `read` returns non-zero at EOF on a file whose last line has no newline — and it
        # has ALREADY assigned. `|| head=""` therefore wiped the shebang of any one-line file
        # saved without a trailing newline, dropping it out of the gate silently, which is
        # precisely the class of file this gate exists for.
        head=""
        IFS= read -r head <"$f" || :
        case "$head" in
            # `zsh` is excluded on purpose: `bash -n` cannot parse it, so accepting it would
            # turn a valid script into a red gate — and a false red blocks a landing and
            # points at innocent code, which is worse here than a missed file.
            '#!'*zsh|'#!'*zsh\ *) : ;;
            '#!'*sh|'#!'*sh\ *) printf '%s\n' "$f" ;;
        esac
    done
}

# check_shell_syntax <repo root> — parse every shell file; fail if any does not, or if none
# was found.
#
# "None was found" is a failure, not a fact about the machine: the list is a fixed repo layout,
# so an empty match means the gate is broken. Unmatched globs were swallowed by the `[ -f ]`
# guard, so a zero-coverage run printed its header and then "All checks passed" — the vacuity
# the shell-tests stage grew a counter for, in the same file, one stage below.
check_shell_syntax() {
    local root="$1" f n=0
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        bash -n "$f" || { echo "   $f does not parse" >&2; return 1; }
        n=$((n + 1))
    done <<EOF
$(shell_sources "$root")
EOF
    if [ "$n" -eq 0 ]; then
        echo "   (no shell files found — this gate is broken, not idle)" >&2
        return 1
    fi
    echo "   $n shell file(s) parse"
}
