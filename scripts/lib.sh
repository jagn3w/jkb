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

# This file's own path, so `_override_statuses` can derive a list from the source rather than
# from a copy of it kept beside the test.
_JKB_LIB_SELF="${BASH_SOURCE[0]}"

# _git … — git, with the caller's repository selection stripped out.
#
# EVERY git call in this file goes through it. `GIT_DIR`, `GIT_WORK_TREE` and
# `GIT_COMMON_DIR` outrank `-C`, so with `GIT_WORK_TREE` exported — the standard bare-dotfiles
# shell recipe — `rev-parse --show-toplevel` answered somebody else's tree, and
# `install_git_hooks` then created `.githooks/` INSIDE that unrelated repository and reported
# `dispatch=chained`, the good verdict, while the repo it was actually asked about kept a dead
# hook. Measured on git 2.51.1.
#
# jkb runs inside other people's professional repositories and must not decorate them; that is
# the same rule that keeps it from writing a git ref (D46). A wrapper rather than a note at
# each of the six call sites, because the next one added would forget.
#
# It stops at this file's boundary, deliberately, and the REASON matters because it has been
# stated wrongly twice. A HOOK must honour the environment git hands it — not because a linked
# worktree needs `GIT_DIR` (it does not; git chdirs to the working tree top first, so discovery
# from the cwd answers the same), but because `GIT_DIR` is the only thing naming WHICH
# REPOSITORY the merge was about. A round stripped it on the worktree argument and measured the
# result: `ORIG_HEAD` stopped resolving, and a pull that changed `crates/` reported "no
# build-affecting changes pulled" — the failure the strip was written to prevent, caused by the
# strip. So `scripts/hooks/post-merge` and the `chainer_body` heredoc use bare `git`, and the
# hook detects a redirected working tree instead of fighting it (it cannot win: with `GIT_DIR`
# set and `GIT_WORK_TREE` unset, git regards the CWD as the work-tree top, and git has already
# chdir'd to the redirected one).
#
# Every caller HERE passes `-C "$repo_root"`, so stripping the inherited selection loses
# nothing: this file is asked ABOUT a repository, a hook is run BY one.
# SIX, not three — the same set `gitrepo::REPO_SELECTION_VARS` drops. The three that say WHICH
# repository were here from the start; the three that say which PART of one were added in round 28,
# after `jkb task work` was measured rewriting a foreign repository's index through an inherited
# `GIT_INDEX_FILE`. This wrapper is the shell half of that rule and was left at three for a round.
_git() {
    env -u GIT_DIR -u GIT_WORK_TREE -u GIT_COMMON_DIR \
        -u GIT_INDEX_FILE -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES \
        git "$@"
}

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
    common="$(_git -C "$repo_root" rev-parse --git-common-dir 2>/dev/null)" || return 1
    [ -n "$common" ] || return 1
    case "$common" in /*) ;; *) common="$repo_root/$common" ;; esac
    # `--git-common-dir` answers `.` in a bare repo, which made every reported path read
    # `<repo>/./hooks/post-merge`.
    printf '%s\n' "$(_real_dir "$common")/hooks"
}

# _real_dir <path> — a directory's physical path, or the path itself when it does not exist.
# Two spellings of one directory (a trailing slash, a symlink, a `..`) must compare equal.
_real_dir() {
    if [ -d "$1" ]; then (cd "$1" 2>/dev/null && pwd -P) || printf '%s\n' "$1"
    else printf '%s\n' "$1"; fi
}

# _hooks_path_read <repo_root> — the ONE read of `core.hooksPath`, and it reads the
# REPOSITORY'S OWN value.
#
# Prints the value and returns 0; returns 1 when the repository stores none, 2 when git will not
# expand the value this repository would actually use, 5 when THIS GIT could not be asked at all
# (it answered 129, unknown option, to every scope) — which is a fact about the binary, not about
# the value — and 6 when THIS MACHINE could not be asked, because the temporary file the
# expansion probe needs could not be created or written. Three different facts with three
# different remedies. Listed here and not only at the arms that return them: the header is what
# a reader consults, and one that stopped at 2 told them a 5 could not happen. Both consumers — `git_hooks_override`, which resolves it to a directory,
# and `git_hooks_exclude_pattern`, which needs the raw string — go through here, because they
# need DIFFERENT things from the SAME fact and reading it twice is how they came to disagree
# about it. They did, measurably: the override refused an environment-injected value while the
# derivation happily used it, so the derivation had the last word and jkb RETRACTED the
# legitimate exclude block for the repository's own chainer.
#
# **`--get-all`, and `command` scope is skipped.** `--get` reports only the WINNING value, and
# a `-c core.hooksPath=X` on the command line — or `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS`,
# which `git pull` exports into the hook environment — wins over everything stored. That value
# is the calling process's, not the repository's, and a chainer belongs where the NEXT pull
# will look. Reading only the winner meant a healthy repo pulled with `-c core.hooksPath=…`
# printed three warnings and advised storing a value it had already stored; a repo storing
# none was told to set one, which would have killed `.git/hooks` dispatch outright. Skipping
# those entries answers both correctly with no warning at all: nothing stored reads as
# `dispatch=direct`, and a stored value gets its chainer refreshed as usual.
#
# `--show-scope` is git >= 2.26. An older git exits 129 for the unknown option, which is not
# "cannot expand", so the read falls back to asking each STORED scope BY NAME — never to a
# plain `--get`, which reports the winner and so hands back precisely the environment-injected
# value the scope skip exists to ignore. (It did: chainer installed at the injected path,
# exclude rule written for it, `dispatch=chained` reported, while the repository's own path
# kept none.) Same answer on every git, not a degraded one on an old git.
#
# `|| rc=$?`, never a bare assignment: exit 1 here is the commonest case and a bare one aborts
# an `set -e` shell.
_hooks_path_read() {
    local rc=0 scoped line found=1 value=""
    scoped="$(_git -C "$1" config --show-scope --get-all --path core.hooksPath 2>/dev/null)" || rc=$?
    if [ "$rc" -eq 0 ]; then
        # Precedence order, lowest first, `command` last — so the last entry that is not
        # `command` is the repository's own winning value.
        while IFS= read -r line; do
            [ -n "$line" ] || continue
            case "${line%%$'\t'*}" in command) continue ;; esac
            value="${line#*$'\t'}"
            found=0
        done <<EOF
$scoped
EOF
        [ "$found" -eq 0 ] || return 1
        printf '%s' "$value"
        return 0
    fi
    [ "$rc" -eq 1 ] && return 1
    # No `--show-scope` (git < 2.26). Ask each STORED scope by name instead of falling back to
    # `--get`, which reports the WINNER — i.e. hands back exactly the environment-injected value
    # the scope skip above exists to ignore, so an old git would install the chainer at
    # `-c core.hooksPath=X`, write an exclude rule for it and report `dispatch=chained`, while
    # the repository's own path kept none. `--local`/`--global`/`--system` predate `--show-scope`
    # by a decade, so this is the same question on every git rather than a degraded answer on an
    # old one. (An earlier version of this paragraph went on to say that "an unsupported or
    # unusable `--worktree` just fails and is skipped". It did not — see directly below, which
    # is the correction; the claim is deleted rather than left standing above its own refutation.)
    #
    # `--worktree` is a DISTINCT scope only when `extensions.worktreeConfig` is on. With it off
    # git aliases it to `--local`, already asked — and, measured on 2.51.1, it hard-fails 128
    # ("cannot be used with multiple working trees") in any repo with more than one working
    # tree, which D36 makes the NORMAL state: this checkout has four. Read unconditionally, that
    # layout fact reached the `*)` arm below as "cannot be expanded", discarded the good value
    # `--local` had already found, wrote no chainer, and sent the operator to
    # `--show-origin --get core.hooksPath`, which prints a perfectly normal path — the
    # self-refuting remedy `--show-origin` exists to avoid.
    #
    # `--includes` because a scope flag turns include resolution OFF by default, while
    # `--show-scope --get-all` above leaves it ON. Without it a `core.hooksPath` reached through
    # `[include] path = …` or `includeIf` — the standard split-gitconfig recipe — reads as
    # "the repository stores none", which the caller renders as `dispatch=direct` and prints
    # nothing for, while git itself resolves the hook and jkb writes no chainer. The two
    # branches of this one function have to answer the same question about the same repository.
    # `broken` is THREE-VALUED, and one variable rather than two on purpose. It was a pair —
    # `broken` plus `unprobeable` — and a pair has a state neither arm means: a lower scope
    # setting `unprobeable=1` and a higher one then setting `broken=1` for a genuinely
    # unexpandable winner left the stale 1 standing, and the read returned 6 ("this machine
    # could not be asked") for a value git itself refuses. That is the wrong fact and the wrong
    # remedy — the exact class this file keeps producing — and it was reachable because two
    # flags must be assigned TOGETHER at five sites and one of them assigned only one. One
    # variable makes each arm a single write, so the stale combination cannot be spelled.
    #   0  this scope answered; nothing is broken
    #   1  git will not expand the value this scope resolves  -> rc 2
    #   2  jkb could not build the probe to find out          -> rc 6
    local scope out found2=1 value2="" wtc="" unsupported=0 asked=0 broken=0
    local raw last probe
    # `--local`, because `extensions.worktreeConfig` is a LOCAL-ONLY repository extension: git
    # stores it per repository, so a global one — or a `-c` on the command line, which this
    # function refuses for `core.hooksPath` three lines up — is not this repository's answer.
    # Read with full precedence, a stray global `true` sends the loop into `git config
    # --worktree`, which then exits 128 in any repo with more than one working tree and collapses
    # the whole read to `dispatch=unreadable` with no chainer written. The flag deciding HOW to
    # read must come from the same place as the value.
    wtc="$(_git -C "$1" config --local --bool extensions.worktreeConfig 2>/dev/null)" || wtc=false
    for scope in system global local worktree; do
        if [ "$scope" = worktree ] && [ "$wtc" != true ]; then continue; fi
        asked=$((asked + 1))
        rc=0
        out="$(_git -C "$1" config --"$scope" --includes --get-all --path core.hooksPath 2>/dev/null)" || rc=$?
        # Measured on git 2.51.1: 1 is "not set in this scope", 128 is "set, and git will not
        # expand it" (`~someuser/` for an absent account). 129 is an unknown OPTION, which is
        # how a git predating `--worktree` answers — a fact about the git, not about the value.
        # Anything else is unestablished and must not be spelled as "stores none", which the
        # caller renders as `dispatch=direct` and prints nothing for.
        case "$rc" in
            0) broken=0 ;;
            1) continue ;;
            # 129 is an unknown OPTION — a fact about this git, not about the value. It is how a
            # git predating `--worktree` answers, and it is ALSO how one predating `--includes`
            # would answer: absorbed into "not set in this scope", every scope reads empty and a
            # repo with a stored `core.hooksPath` gets `dispatch=direct`, which prints nothing.
            # So it is counted: all four scopes refusing means the read failed, not that nothing
            # is stored.
            129) unsupported=$((unsupported + 1)); continue ;;
            # 128 is "git will not expand SOMETHING in this scope" — which is not the same as
            # "this scope's answer is unusable". `--path` expands EVERY value it returns, not
            # just the winning one, so ONE broken line anywhere in the file fails the whole
            # read. Measured on 2.51.1 with the split-config recipe `--includes` exists for:
            # `~/.gitconfig` carrying `hooksPath = ~nosuchuser42/hooks` and then an
            # `[include]` whose file sets a good one. `rev-parse --git-path hooks/post-merge`
            # answers `/good/hooks/post-merge` — git resolves it and will run hooks there —
            # while this scope read exits 128 and the repository was reported
            # `dispatch=unreadable` with no chainer. The losing-scope fix one round ago named
            # this same harm one scope out and left it standing one scope in.
            #
            # So the scope is re-asked RAW, and only its LAST value — the one git would
            # actually use from here — is put back to git for expansion. Broken lines that are
            # not the scope's answer are as irrelevant as a broken value in a losing scope.
            128)
                raw="$(_git -C "$1" config --"$scope" --includes --get-all core.hooksPath 2>/dev/null)" \
                    || { broken=1; continue; }
                # The scope's winner, by the same last-entry-wins rule used below.
                last="${raw##*$'\n'}"
                # Asked of GIT, not modelled: `~/` and `~user/` differ only by whether the
                # account exists on THIS machine, which is not a thing to reimplement. A
                # one-key file is the narrowest way to ask about exactly one value.
                #
                # git WRITES the probe as well as reading it, because config values are not
                # plain text and hand-writing `hooksPath = $last` corrupts them. Measured on
                # 2.51.1, `printf` into the file: `/a/b#c` and `/a/b;c` came back truncated at
                # the comment character, leading whitespace was eaten, and `/a/back\slash`
                # exited 128 — a value git resolves perfectly well, reported by THIS arm as
                # unexpandable, which is the exact defect the arm exists to remove. `git config
                # --file <f> <key> <value>` quotes on the way in (`hooksPath = "/a/b#c"`), and
                # all six round-tripped, tilde expansion included.
                #
                # A probe we could not BUILD is its own fact, and rc 6 not rc 2. Both failures
                # here are about this machine — a full disk, a read-only or `noexec` temp mount,
                # a sandbox with a scoped `TMPDIR` — and reporting them as "git will not expand
                # this value" reverts the whole arm to the wrong answer it was added to remove,
                # with a remedy (`--show-origin`) that prints a perfectly ordinary path. `setup.sh`
                # runs unattended from `post-merge`, so the warning is all anyone sees.
                probe="$(mktemp "${TMPDIR:-/tmp}/.jkb-hookspath.XXXXXX")" \
                    || { broken=2; continue; }
                if ! _git config --file "$probe" core.hooksPath "$last" 2>/dev/null; then
                    rm -f "$probe"; broken=2; continue
                fi
                if out="$(_git config --file "$probe" --path --get core.hooksPath 2>/dev/null)"; then
                    rm -f "$probe"
                    broken=0
                else
                    rm -f "$probe"
                    # THIS scope's answer is the unexpandable one, so a lower scope's value is
                    # not what git would use either.
                    broken=1
                    continue
                fi
                ;;
            *) broken=1; continue ;;
        esac
        # Last entry within a scope wins, as git itself resolves it. Command substitution has
        # already eaten the trailing newline, so a single empty value arrives as "" — which is
        # `core.hooksPath` set to the empty string, a state `git_hooks_override` reports as
        # rc 3 and must not be confused with "no value here".
        value2="${out##*$'\n'}"
        found2=0
    done
    # Every scope we asked refused the options: we did not establish "stores none", we failed to
    # ask. Spelling that as "not set" is the one answer the caller renders silently.
    # rc 5, not 2. Both are "unestablished", but they are different facts with different
    # remedies: 2 is a VALUE this machine cannot expand (`~someuser/` for an absent account),
    # whose refusal sends the operator to `--show-origin` to find it; 5 is a GIT too old to be
    # asked, where `--show-origin` would print something perfectly normal and the only remedy is
    # a newer git. A refusal must name a remedy that is true of the thing refused.
    if [ "$found2" -ne 0 ] && [ "$asked" -gt 0 ] && [ "$unsupported" -eq "$asked" ]; then
        return 5
    fi
    # A break at the highest-precedence scope that answered: git would fail here too — unless
    # what broke was OUR probe rather than git's expansion, which is a different fact (6).
    # Read from the ONE variable the loop wrote, so the last scope to break is the one answered
    # for; a second flag read alongside it is what let a lower scope's failure win.
    case "$broken" in
        0) ;;
        2) return 6 ;;
        *) return 2 ;;
    esac
    [ "$found2" -eq 0 ] || return 1
    printf '%s' "$value2"
    return 0
}

# git_hooks_override <repo_root> — print the absolute `core.hooksPath` in effect for
# <repo_root>, or nothing when there is none. Returns 0 for both of those — "no override" is
# an answer, not a failure. The OTHER codes each name a way the setting yields no one hooks
# directory, and each is its own code because each needs its own sentence and its own repair —
# codes 2, 3 and 4 are ways the VALUE is unusable, code 5 is this git being unable to say:
#
#   2  git will not expand it (`~someuser` for an account this machine does not have)
#   3  it is set to the empty string (git resolves it to `/post-merge` and finds nothing)
#   4  it is relative and this repository has no working tree, so git anchors it on the
#      INVOKING PROCESS'S current directory and there is no one place at all
#   5  this git answered 129 (unknown option) to every scope we could ask, so nothing about
#      the repository's own value was established — a fact about the GIT, not about the value,
#      and the two need different remedies
#
# One code for all of them sent the operator a check that prints a perfectly normal value for
# code 4, which is the same failure `--show-origin` was introduced to fix for code 3 — and the
# same reason code 5 is not code 2: `--show-origin` prints something perfectly normal there too.
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
    # `|| rc=$?`, never a bare assignment. A bare `x="$(cmd)"` is a simple command, so under
    # `set -e` a non-zero substitution aborts the shell — and exit 1 here is the COMMONEST
    # case, `core.hooksPath` not set at all. This file's header promises every function
    # behaves the same with `set -e` on or off; that promise was being kept only by how the
    # one caller happens to spell the call. Same shape as the ERR-trap lesson in D50.
    local rc=0
    configured="$(_hooks_path_read "$repo_root")" || rc=$?
    case "$rc" in
        0) ;;
        1) return 0 ;;      # the repository stores none
        5) return 5 ;;      # this git could not be asked at all (see `_hooks_path_read`)
        6) return 6 ;;      # this MACHINE could not be asked: the expansion probe needs a temp file
        *) return 2 ;;      # set, and git will not expand it
    esac
    # Set to the empty string is NOT "not set". Measured: `config --get --path` exits 0
    # printing nothing, `rev-parse --git-path hooks/post-merge` answers `/post-merge`, and
    # `git hook run post-merge` says "cannot find a hook named post-merge" — the repo hook is
    # dead. Folded into "not set", the caller reported `dispatch=direct`, which the renderer
    # prints nothing for: the D34 post-merge automation silently off, which is the exact harm
    # `dispatch` was made many-valued to surface.
    [ -n "$configured" ] || return 3
    case "$configured" in
        /*) ;;
        *)
            # A relative value is anchored at the WORKING TREE TOP. With no working tree there
            # is no anchor at all: git resolves it against the invoking process's current
            # directory, so `git --git-dir=B rev-parse --git-path hooks/post-merge` answers
            # `<cwd>/.githooks/post-merge` and `git hook run post-merge` executes whatever
            # copy is under the cwd it happens to be run from. Measured on git 2.51.1 from
            # three different directories.
            #
            # A previous round called the git dir "git's own rule" for that case, on a
            # measurement taken with the cwd SET TO the git dir — which cannot tell the two
            # apart. jkb then installed a chainer there and reported the good verdict, while a
            # `git pull` in a linked worktree ran `<worktree>/.githooks/post-merge`, which did
            # not exist. There is nothing to resolve to, so rc 2 and say so; running setup.sh
            # against a worktree instead resolves normally and installs the chainer where that
            # worktree's pulls will find it.
            top="$(_git -C "$repo_root" rev-parse --show-toplevel 2>/dev/null)" || return 4
            [ -n "$top" ] || return 4
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
    local repo_root="$1" configured scoped rc rel line wt hit=""
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
$(_git -C "$repo_root" worktree list --porcelain 2>/dev/null)
EOF
    if [ -z "$main_top" ]; then
        printf 'undecided (the repository'"'"'s worktrees could not be listed)\n'
        return 0
    fi
    # The SAME read `git_hooks_override` makes, because it is the same fact — including its
    # skip of `command` scope. Reading it separately is what let this function derive a pattern
    # from an environment-injected path while the override refused it; an empty answer here
    # becomes `want=no` at the caller, so the derivation swept away the block for the
    # repository's own chainer.
    rc=0
    configured="$(_hooks_path_read "$repo_root")" || rc=$?
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

# _exclude_write <file> [line…] — replace <file> with the given lines, atomically.
#
# THE ONLY writer of `.git/info/exclude` in this file, used by the sweep and by the append
# alike. The append used to go straight in with two `>>` redirections — a separator newline,
# then marker+pattern — which is two writes with no rollback: a disk that fills between them
# leaves the user's file with a stray newline and half a marker line, under a report saying
# the step did not land. Through here that report is true of every failure.
#
# `cp -p` first so the replacement inherits the destination's mode rather than a fresh file's,
# and the write status is CHECKED — `printf` to a full disk fails after emitting part of its
# output, and dropping that status once renamed a truncated file over every rule the user owns
# under the word `retracted`.
_exclude_write() {
    local file="$1" tmp
    shift
    tmp="$file.jkb.$$"
    if [ -e "$file" ]; then
        cp -p "$file" "$tmp" 2>/dev/null || { rm -f "$tmp"; return 1; }
    fi
    if [ "$#" -eq 0 ]; then
        : >"$tmp" || { rm -f "$tmp"; return 1; }
    elif ! printf '%s\n' "$@" >"$tmp"; then
        rm -f "$tmp"
        return 1
    fi
    mv -f "$tmp" "$file" || { rm -f "$tmp"; return 1; }
}

# reconcile_exclude <repo_root> <pattern, or empty> <want> [empty_report] [undecided_reason] — make
# `.git/info/exclude` agree with the desired state, printing complete report lines:
#
#   exclude=added <pattern>          we wrote our marked block
#   exclude=kept <pattern>           a rule already excludes it (ours, or the user's)
#   exclude=retracted <pattern>      a block of ours was there and should not be; it is gone
#   exclude=deduplicated <pattern>   extra copies of the block we are keeping were removed
#   exclude=tidied <n> marker(s)     orphaned jkb marker lines were removed
#   exclude=unowned <pattern>        a rule excludes it that jkb cannot prove it wrote — kept
#   exclude=exposed <reason>         ours IS inside a working tree and cannot be hidden there
#   exclude=undecided <reason>       nothing was decided about THIS pattern
#   exclude=none <reason>            nothing to do
#   exclude=failed <reason>          a write was attempted and did not land — or was refused
#                                    before anything was read or written, which is the same
#                                    thing from the caller's side: the file is as it was
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
# THE SCOPE RULE. Every line above describes only its own STEP or its own PATTERN. Exactly one
# line describes the FILE, and it is emitted once per run, below every arm:
#
#   exclude-file=changed | unchanged | unknown   whether `.git/info/exclude` differs from
#                                     before this run; `unknown` when it could not be measured
#
# That key exists because three separate must-fixes were one shape: a per-pattern or per-step
# arm asserting a run-level fact it could not see. The clincher is that the report word
# `undecided` has TWO producers with opposite file semantics — `want=unknown` returns early and
# touches nothing, `want=undecided` runs the sweep first — so no wording of that arm could ever
# have been right. Rewording removed one false sentence and left the vacuum that invited it.
#
# It is measured, not bookkept: the wrapper fingerprints the file either side of the decision,
# so a write that some future arm forgets to record is still reported truthfully. Same rule as
# `dispatch=` — a claim about a file is asked of the file.
#
# Always returns 0 — `failed` is a word, not an exit status (see the header's `set -e` rule).
reconcile_exclude() {
    local repo_root="$1" before after path brc=0 arc=0 prc=0
    path="$(_exclude_path "$1")" || prc=$?
    [ "$prc" -eq 0 ] || brc=1     # could not even locate the file: nothing is established
    before="$(_exclude_fingerprint "$path")" || brc=$?
    # Contractually rc 0, so this wrapper behaves identically with `set -e` on or off.
    _reconcile_exclude_decide "$@"
    after="$(_exclude_fingerprint "$path")" || arc=$?
    # Below every arm, not inside one: the decision body has a dozen `return 0`s and none of
    # them can skip this. The property is structural, exactly as `install_git_hooks`' own
    # funnel is, so a new arm cannot forget to report what it did to the file.
    #
    # Three-valued, because either measurement can fail and a comparison of two failures is
    # not evidence of anything. `unknown` is never spelled `unchanged`.
    if [ "$brc" -ne 0 ] || [ "$arc" -ne 0 ]; then
        printf 'exclude-file=unknown\n'
    elif [ "$before" = "$after" ]; then
        printf 'exclude-file=unchanged\n'
    else
        printf 'exclude-file=changed\n'
    fi
    return 0
}

# _override_verdict <rc> / _override_why <rc> — how `git_hooks_override`'s refusal codes are
# reported. Extracted from `install_git_hooks` so the mapping can be CALLED with a status that
# does not exist yet: inline, the `*)` arm was behaviourally identical to `4)` — there is no
# fifth code today — so no test could tell an honest catch-all from an absorbing one, and a
# mutation reverting it stayed green.
#
# `*)` names an unrecognised status rather than absorbing it into a definite `unanchored`,
# whose remedy is about working trees and would be false of whatever the new code means. That
# is this project's house rule in miniature: an unestablished answer is never spelled as a
# definite one. The number is carried so a new code is diagnosable rather than anonymous.
# _override_statuses — the refusal codes `_override_verdict` maps, derived from its own `case`.
#
# Derived so a test cannot enumerate a stale list: the hand-written one omitted the status added
# by the very commit that added it.
_override_statuses() {
    _override_verdict --list
}

_override_verdict() {
    if [ "$1" = --list ]; then
        # Every numeric arm of THIS function's own case, in order. Scoped to the function by
        # awk rather than grepped file-wide: a file-wide pattern also matched the `0)`/`1)`
        # arms of unrelated `case "$rc"` blocks and `_override_why`'s duplicate arms, and the
        # test consuming it passed anyway — a derived list that derives the wrong thing is no
        # better than the stale hand-written one it replaced.
        #
        # The arm pattern is deliberately loose about LAYOUT and strict about POSITION: any
        # indentation, and no requirement that `printf` share the line. Pinned to eight spaces
        # plus a same-line `printf`, a reindented or wrapped arm dropped silently out of the
        # list — and a status missing from the list is a status nothing checks has a render
        # arm, which is the whole point of deriving it. The `case` line below it is what makes
        # the position unambiguous: only the arms of this one `case` are at this depth.
        awk '/^_override_verdict\(\) \{/ { inside = 1; next }
             inside && /^\}/             { exit }
             inside && /^[[:space:]]*[0-9|]+\)/ {
                 arm = $0; sub(/^[[:space:]]*/, "", arm); sub(/\).*/, "", arm)
                 n = split(arm, alts, "|")
                 for (i = 1; i <= n; i++) print alts[i]
             }' "$_JKB_LIB_SELF"
        return 0
    fi

    case "$1" in
        2) printf 'unreadable core.hooksPath cannot be expanded on this machine' ;;
        3) printf 'unreadable core.hooksPath is set to the empty string' ;;
        4) printf 'unanchored core.hooksPath' ;;
        5) printf 'unaskable core.hooksPath could not be read from this git' ;;
        6) printf 'unprobeable core.hooksPath could not be tested for expansion on this machine' ;;
        *) printf 'unreadable core.hooksPath could not be resolved (unrecognised status %s)' "$1" ;;
    esac
}

_override_why() {
    case "$1" in
        2) printf 'core.hooksPath cannot be expanded' ;;
        3) printf 'core.hooksPath is empty' ;;
        4) printf 'core.hooksPath is relative and this repository has no working tree' ;;
        5) printf 'this git is too old to report where core.hooksPath is set' ;;
        6) printf 'no temporary file could be created to ask git about core.hooksPath' ;;
        *) printf 'core.hooksPath could not be resolved' ;;
    esac
}

# _exclude_path <repo_root> — where `.git/info/exclude` is, or empty outside a repository.
#
# One derivation shared by the wrapper and the decision body. Two copies would drift, and the
# wrapper fingerprinting a different file from the one the body writes is a lie that reads
# exactly like the truth.
_exclude_path() {
    local common rc=0
    common="$(_git -C "$1" rev-parse --git-common-dir 2>/dev/null)" || rc=$?
    if [ "$rc" -ne 0 ] || [ -z "$common" ]; then
        # 128 is git's own "not a git repository" — an ESTABLISHED answer, and the one the
        # caller turns into `exclude=none`. Anything else (127: no git on PATH; a signal; a
        # broken object store) means we could not ask, which is a different fact: spelled the
        # same, a transient failure made both fingerprints `no-repo`, they compared equal, and
        # the run reported `exclude-file=unchanged` over whatever it had just done.
        [ "$rc" -eq 128 ] && return 0
        return 1
    fi
    case "$common" in /*) ;; *) common="$1/$common" ;; esac
    printf '%s/info/exclude' "$common"
}

# _exclude_fingerprint <path> — a value that changes whenever the file's CONTENT does.
#
# Existence is part of it: `absent` is a distinct fingerprint, so creating or removing the file
# both register — that is an ESTABLISHED state, and it returns 0.
#
# It returns **1 when it could not measure**: the path exists but cannot be read, or `cksum`
# will not run. That is not the same as `absent`, and folding the two together is the defect
# this whole key exists to prevent, reintroduced inside its own measurement. Measured: with
# `cksum` unavailable both calls answered `absent`, they compared equal, and jkb reported
# `exclude-file=unchanged` over a real write — the false reassurance, one level down.
#
# `set -e`-safe: every failure is consumed by `||` or by an explicit `return`.
_exclude_fingerprint() {
    [ -n "$1" ] || { printf 'no-repo'; return 0; }
    [ -e "$1" ] || { printf 'absent'; return 0; }
    # GROUPED. `cksum <"$1" 2>/dev/null` redirects cksum's stderr, but a failure to OPEN the
    # file is reported by the shell before cksum runs, so an unreadable exclude file leaked two
    # raw `bash: …: Permission denied` lines per run into setup.sh's output. The group's
    # redirection covers the open as well.
    { cksum <"$1"; } 2>/dev/null || return 1
}

# The decision half — everything the wrapper above measures. Its report words are unchanged.
_reconcile_exclude_decide() {
    local repo_root="$1" pattern="$2" want="$3" empty_report="${4:-}" why="${5:-the chainer install failed}"
    local common exclude tmp line nxt cur keep=""
    local -a lines=() out=() removed=() deduped=()
    local i n seen_keep=0 changed=0 probe_retracted=0 tidied=0

    # THE SAME derivation the wrapper fingerprints, called rather than copied. Two copies of
    # it would drift, and a wrapper measuring a different file from the one this body writes is
    # a lie that reads exactly like the truth — it would report `unchanged` over a real write,
    # which is the whole class `exclude-file=` was added to end.
    local prc=0
    exclude="$(_exclude_path "$repo_root")" || prc=$?
    if [ "$prc" -ne 0 ]; then
        printf 'exclude=failed (could not locate .git/info/exclude)\n'
        return 0
    fi
    if [ -z "$exclude" ]; then
        printf 'exclude=none (not a git repository)\n'
        return 0
    fi
    common="${exclude%/info/exclude}"

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
        if ! _exclude_write "$exclude" ${out[@]+"${out[@]}"}; then
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
        # The caller supplies the reason: `undecided` has two producers now — a chainer
        # install that failed, and a `core.hooksPath` the caller refused to anchor — and
        # naming the first for both reported a step that never ran.
        printf 'exclude=undecided (%s; nothing was decided about %s)\n' "$why" "$pattern"
        return 0
    fi
    if [ "$want" = yes ]; then
        if [ "$seen_keep" -eq 1 ] || _exclude_mentions "$exclude" "$pattern"; then
            printf 'exclude=kept %s\n' "$pattern"
            return 0
        fi
        mkdir -p "$common/info" 2>/dev/null \
            || { printf 'exclude=failed (cannot create %s/info)\n' "$common"; return 0; }
        # Appended through the SAME atomic rewrite the sweep uses, not with `>>`. Two direct
        # appends — a separator newline, then marker+pattern — are two writes with no rollback,
        # so a disk that fills between them leaves the user's file with a stray newline and
        # half a marker line, under a report claiming the step did not land. One write path
        # means the claim is true of every failure, and the separator special case disappears:
        # the file is rebuilt from lines that each end in one.
        #
        # (That special case existed because an exclude file not ending in a newline — a
        # hand-edited one often does not — had its last rule fused with our marker. Rebuilding
        # cannot produce that. `session::ensure_excluded` still appends and still needs its own
        # `sep`; the rule is one rule, with an implementation in each language.)
        out+=("$(exclude_marker)" "$pattern")
        if ! _exclude_write "$exclude" "${out[@]}"; then
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
#   exclude-file=<state>      changed | unchanged | unknown — whether `.git/info/exclude`
#                             differs from before this run. The ONLY line that describes the
#                             file; every other line describes one step or one pattern. Emitted
#                             once per run by `reconcile_exclude`, below every arm.
#   dispatch=<verdict> [detail] direct | chained | unknown | dead | unreadable | unanchored |
#                             unaskable | unprobeable — `chainer.test.sh` case14 checks this
#                             list against the RENDERER's own arms, because a header that lags
#                             says a verdict the code emits cannot happen. Against the renderer
#                             and not `_override_verdict`, which maps refusal codes and so knows
#                             only the four `un*` words: derived from that, the check let
#                             `unknown | dead` be deleted here with every suite still green.
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
# chainer may dispatch perfectly well and we cannot know, so it is never spelled `dead`. Nor
# is `unreadable` (a `core.hooksPath` that names no one hooks directory) nor `unanchored` (a
# relative one in a repository with no working tree) — and neither of those claims that no hook
# runs anywhere, because a worktree of such a repository resolves the value normally and does
# run one.
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
    local want=no ours=unknown verdict="" pat_line pattern="" pattern_reason="" undecided_why=""

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
    if [ "$override_rc" -ge 2 ]; then
        # `undecided` — nothing is known about THIS pattern — because that is the one thing
        # this branch does know: no chainer was attempted, so jkb cannot say whether its own
        # is at the derived path. The derivation still has the last word through the funnel
        # below, and every cause that reaches this branch then lands where it should:
        #
        #   unexpandable value  derivation says `undecided …` → want=unknown → touch nothing
        #   empty value         derivation says `none …`, no pattern → want=no → sweep, which
        #                       is right: nothing of ours is anywhere
        #   relative, no tree   derivation still yields a pattern (it is correct inside every
        #                       worktree of a bare repo), and `undecided` keeps that block
        #                       while sweeping the others
        #   unaskable git (5)   `_hooks_path_read` refuses there too, so the derivation says
        #                       `undecided …` → want=unknown → touch nothing, which is the
        #                       only honest move when the value was never established
        #
        # Leaving `want` at its `no` default made the third sweep the block a run from a
        # worktree had just added — the same configuration answering two ways depending on
        # which directory setup.sh was pointed at, which is the flip-flop the whole derivation
        # exists to prevent.
        want=undecided
        # The reason comes from the CALLER, which knows which cause fired. `reconcile_exclude`
        # used to word `undecided` as "the chainer install failed" — its only producer when
        # that text was written — so this path, where no chainer is attempted at all, reported
        # a step that never ran, on the exact configuration the refusal exists for.
        verdict="$(_override_verdict "$override_rc")"
        undecided_why="$(_override_why "$override_rc")"
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
        # The helper already worded the whole report line.
        "none "*|"exposed "*) pattern=""; pattern_reason="$pat_line" ;;
        # A word this caller does not know. `want=unknown` — touch nothing — because the
        # default here used to be `pattern=""`, which the funnel then collapsed to a perfectly
        # recognised `want=no`, so the callee's refusal never saw it and the sweep ran anyway:
        # jkb's own block retracted, and the renderer warning about the unknown state AFTER
        # the file had been changed. Adding a fifth word to the derivation is exactly the edit
        # made two rounds ago, so this is not hypothetical.
        *) pattern=""; pattern_reason="$pat_line"; want=unknown ;;
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
    reconcile_exclude "$repo_root" "$pattern" "$want" "$pattern_reason" "$undecided_why"

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
                    # ONE line, and it is a claim about this pattern only. The second line
                    # used to say "nothing in .git/info/exclude was changed" — which is false
                    # whenever `want=undecided`, because that want still sweeps every OTHER
                    # jkb block and writes the file. It printed directly beneath
                    # `excluded: X dropped from .git/info/exclude`: two lines about one file
                    # stating opposite facts, unattended, from the post-merge hook.
                    #
                    # No wording could have been right. This report word has two producers
                    # with opposite file semantics (`want=unknown` touches nothing;
                    # `want=undecided` sweeps first), so the fact belongs to `exclude-file=`,
                    # which is measured.
                    #
                    # A remedy usually rides on a sibling key — `chainer=failed` for one
                    # producer, `dispatch=unreadable|unanchored` for another. NOT ALWAYS: a
                    # derivation that could not list the worktrees emits this beside a
                    # perfectly healthy `dispatch=chained`, which renders nothing, so this line
                    # is on its own. It is why the reason is carried in `$detail` rather than
                    # left to a neighbour: the sentence has to stand alone.
                    undecided)  warn "could not work out what to hide from git: $detail" ;;
                    exposed)    warn "the chainer there is not hidden from git: $detail"
                                warn "  that working tree will read dirty, and \`jkb task land\` refuses a dirty target." ;;
                    # Only what is true of EVERY `failed`. The second line used to name the
                    # untracked-chainer consequence, which holds for the append-side failure
                    # and is false for a failed sweep (the block is still there, and the
                    # chainer may be nowhere near a working tree) and vacuous for a refusal
                    # that never opened the file. A false diagnosis handed to an operator on
                    # an unattended pull is worse than a vaguer true one.
                    # Scoped to the STEP, because `failed` is a per-step word and this
                    # function reports several lines per run. `reconcile_exclude` can retract
                    # (committed by `mv`) and then fail the append, so a whole-file claim of
                    # "nothing was changed" printed directly beneath `retracted …` — two lines
                    # about one file stating opposite facts. It is also untrue of a partial
                    # append, which writes to the user's file with no rollback.
                    failed)     warn "could not update .git/info/exclude $detail"
                                warn "  that step did not land; anything reported above it did." ;;
                    *)          warn "unrecognised exclude state: $line" ;;
                esac ;;
            # The ONLY line that describes the file. Both values render nothing: every
            # mutation is already itemised by its own `retracted`/`added`/`tidied` line, and
            # silence is not a claim — the same reason `dispatch=direct|chained` print
            # nothing. It exists so that no OTHER arm has to guess, and so that a reassurance,
            # if one is ever wanted again, has exactly one legal home where it is true by
            # measurement. The default arm warns, so a third value added at the producer
            # surfaces instead of vanishing.
            exclude-file=*)
                case "$state" in
                    changed|unchanged) : ;;
                    # The one value worth a line. Silence here would say "nothing to report
                    # about the file", which is exactly what could not be established — and
                    # the itemised lines above may or may not have landed.
                    unknown) warn "could not tell whether .git/info/exclude changed — check it by hand if a line above says one was added or dropped." ;;
                    *) warn "unrecognised exclude-file state: $line" ;;
                esac ;;
            dispatch=*)
                case "$state" in
                    # Both good outcomes: git reads .git/hooks itself, or our chainer sends it
                    # there. The lines above have already said so.
                    direct|chained) : ;;
                    unknown) warn "  if $detail does not exec \"\$(git rev-parse --git-common-dir)/hooks/post-merge\", the repo hook never runs." ;;
                    dead)    warn "core.hooksPath is set and nothing runnable is at $detail — git will NOT run the repo hook above." ;;
                    # True of both causes that reach here: a value git cannot expand (it
                    # fatals), and a value set to the empty string (git resolves it to
                    # `/post-merge` and finds nothing). It used to also cover a relative path
                    # in a repo with no working tree — which has no anchor at all and now has
                    # its own arm above, because git resolves it against the pulling process's
                    # cwd and this sentence was false of it.
                    # Its OWN arm and its own repair. Sharing `unreadable`'s sentence sent the
                    # operator to `git config --show-origin --get core.hooksPath`, which for
                    # this cause prints a perfectly normal `.githooks` and appears to refute
                    # the warning — the same failure `--show-origin` was introduced to fix one
                    # cause over.
                    unanchored)
                             warn "core.hooksPath is relative and this repository has no working tree, so git resolves it against whatever directory the pulling process is in — there is no one place to install a chainer."
                             warn "  run setup.sh from a working tree of this repository, or set an absolute core.hooksPath." ;;
                    unreadable)
                             warn "$detail, so git will not reliably run the repo hook above."
                             # `--show-origin`, because `--get` prints one empty line for an
                             # empty value and nothing for an unset one — visually identical,
                             # so the operator's own check appeared to refute the warning.
                             warn "  check it with: git config --show-origin --get core.hooksPath" ;;
                    # Its own arm, because the REMEDY differs — which is the whole reason code 5
                    # is not code 2. Sharing `unreadable`'s sentence sent the operator to
                    # `git config --show-origin --get core.hooksPath`, which on the very git
                    # that cannot be asked prints a perfectly normal `file:.git/config .githooks`
                    # and appears to refute the warning, with nothing pointing at the git binary.
                    # Measured under the all-refusing shim. A refusal must name a remedy that is
                    # true of the thing refused, and the thing refused here is the git.
                    unaskable)
                             warn "$detail, so jkb could not tell where git will look for hooks."
                             # ONE remedy, because the other one was false. It used to end "or
                             # set core.hooksPath yourself and re-run", which cannot change the
                             # answer: rc 5 is raised from the OPTION's 129, so `unsupported ==
                             # asked` holds whatever the repository stores. Measured — set it,
                             # re-ask, byte-identical warning. A half-true remedy is worse than
                             # a short one: the operator disproves the half they can test and
                             # stops trusting the half they cannot.
                             warn "  this git is too old to report where core.hooksPath is set (it needs --show-scope, or per-scope --includes); upgrade git — setting core.hooksPath will not change this answer." ;;
                    # Its own arm for its own remedy, again — and this one is about the MACHINE,
                    # not the git and not the value. jkb could not create the temporary file the
                    # expansion probe needs, so it never found out whether git would expand the
                    # value, and `--show-origin` would print a perfectly ordinary path here too.
                    # It sits BELOW `unaskable` because the six lines above belong to that arm:
                    # inserted between them and it, this arm wore code 5's rationale.
                    unprobeable)
                             warn "$detail, so jkb could not tell where git will look for hooks."
                             warn "  jkb could not create a temporary file (\$TMPDIR is ${TMPDIR:-/tmp}); free space or point TMPDIR at a writable directory and re-run." ;;
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

# --- a database on a shared filesystem ------------------------------------------------------
# The shell half of `crates/jkb-core/src/shared_fs.rs`, and the same rule. `sqlite3` opens a
# database read-write and checkpoints on close, so a script that "only reads" jkb.db from inside the
# dev container truncates a WAL the host is still writing — measured, a process on each side of the
# bind corrupted a database within 38 commits (.container/sqlite-share-probe.py). EVERY `sqlite3`
# under scripts/ goes through `jkb_sqlite`, which refuses first; case11 of dev-scripts.test.sh fails
# on a bare one, because the next script added would otherwise forget.

# Activate every unit `jkb service install` wrote, and set `watcher_state` to `running` or `failed`.
#
# EVERY unit, as the binary lists them (`jkb service labels`) — not a copy of the list here, which is
# how a third unit came to be written on both platforms and restarted on one. The reaper is what
# finishes a landing whose session could not archive its own worktree, so a unit written and never
# loaded means those worktrees accumulate for ever — visible only as `jkb doctor` output nobody reads.
#
# `enable` then `restart` on systemd, not `enable --now`: that leaves an already-running unit on the
# OLD binary, and a daemon older than the database it serves refuses every request (schema_newer).
# launchd's unload/load restarts each one already.
#
# Then it waits for `jkb serve` to prove it came up: the daemon writes its token only once its port
# is bound, so a token newer than a marker taken before activation is that proof. A unit that loads
# and then exits (the port taken) would otherwise read "loaded" and fail every container request.
# JKB_SERVE_READY_WAIT (seconds, default 10) bounds the wait.
activate_services() {
    local db="$1" labels label serve_token started_marker waited=0
    local wait_for="${JKB_SERVE_READY_WAIT:-10}"
    watcher_state=running
    if ! labels="$(jkb --db "$db" service labels)" || [ -z "$labels" ]; then
        warn "could not list the service units (jkb service labels)"
        watcher_state=failed
        return 0
    fi
    serve_token="$(dirname "$db")/daemon/token"
    started_marker="$(mktemp "${TMPDIR:-/tmp}/jkb-setup.XXXXXX")" || { watcher_state=failed; return 0; }
    # A token written within this second must still read as newer on a filesystem with 1 s mtimes.
    sleep 1
    case "$(uname -s)" in
        Darwin)
            for label in $labels; do
                local plist="$HOME/Library/LaunchAgents/$label.plist"
                launchctl unload "$plist" 2>/dev/null || true   # idempotent reload
                if launchctl load "$plist"; then echo "$label loaded (launchd)"; else
                    warn "could not load $label; activate manually: launchctl load $plist"
                    watcher_state=failed
                fi
            done ;;
        Linux)
            if command -v systemctl >/dev/null 2>&1; then
                systemctl --user daemon-reload || true
                for label in $labels; do
                    if systemctl --user enable "$label" && systemctl --user restart "$label"; then
                        echo "$label enabled (systemd)"
                    else
                        warn "could not enable $label; activate manually: systemctl --user enable --now $label"
                        watcher_state=failed
                    fi
                done
            else
                warn "systemctl not found; activate the printed units manually."
                watcher_state=failed
            fi ;;
        # Reachable only after `jkb service install` succeeded, and that refuses any platform but
        # macOS and Linux — so "unsupported OS" was the wrong diagnosis for the one state that gets
        # here: `uname` said something the two arms above did not recognise.
        *) warn "unrecognised platform '$(uname -s)'; the units were written — activate them manually."
           watcher_state=failed ;;
    esac
    if [ "$watcher_state" = running ]; then
        until [ "$serve_token" -nt "$started_marker" ] || [ "$waited" -ge "$wait_for" ]; do
            sleep 1; waited=$((waited + 1))
        done
        if [ "$serve_token" -nt "$started_marker" ]; then
            echo "jkb serve is up (token rotated)"
        else
            warn "jkb serve did not come up within ${wait_for}s (no new token at $serve_token); see serve.log beside the database"
            watcher_state=failed
        fi
    fi
    rm -f "$started_marker"
}

# shared_fs_kind <hex-magic> — the filesystem name when `stat -f -c %t` names one a database must not
# live on, empty otherwise. The same set as SHARED in shared_fs.rs: FUSE (measured on the container's
# binds), 9p, NFS, SMB2, CIFS.
shared_fs_kind() {
    case "$1" in
        65735546) printf 'FUSE (virtiofs, gRPC-FUSE, sshfs)\n' ;;
        1021997) printf '9p\n' ;;
        6969) printf 'NFS\n' ;;
        fe534d42) printf 'SMB2\n' ;;
        ff534d42) printf 'CIFS\n' ;;
        *) : ;;
    esac
    return 0
}

# refuse_shared_db <db-path> — 0 when the database may be opened here, non-zero (with the reason on
# stderr) when it, one of its files, or the directory that will hold them is on a shared filesystem,
# or that cannot be established. The same judgement as shared_fs.rs:
#   - a `file:` URI is refused outright: the database shell accepts one, and judging it as a relative
#     path would ask about the working directory instead of the database;
#   - symlinks are followed, DANGLING ones included (`readlink -m`), because SQLite creates the
#     database at the far end of a dangling link;
#   - the nearest existing ancestor of the resolved path is asked, and so are the file and its
#     -wal/-shm/-journal when they exist — a single file bind-mounted from the host into a local
#     directory is invisible to statfs of that directory.
# The filesystem half is Linux only — the host side of the boundary is a local disk.
refuse_shared_db() {
    local db="$1" target dir at magic kind asks=()
    # Before the platform check: shared_fs.rs refuses a URI on every platform, and two copies of one
    # rule that disagree on macOS are the defect this pair exists to avoid.
    case "$db" in
        file:*)
            printf 'refusing to open %s: a URI is not a path this guard can judge\n' "$db" >&2
            return 2 ;;
        *) : ;;
    esac
    if [ "$(uname -s)" != Linux ]; then return 0; fi
    if ! target="$(readlink -m -- "$db" 2>/dev/null)" || [ -z "$target" ]; then
        printf 'refusing to open %s: cannot resolve where it points\n' "$db" >&2
        return 2
    fi
    dir="$(dirname "$target")"
    while [ ! -d "$dir" ] && [ "$dir" != / ]; do
        dir="$(dirname "$dir")"
    done
    asks+=("$dir")
    for at in "$target" "$target-wal" "$target-shm" "$target-journal"; do
        if [ -e "$at" ]; then asks+=("$at"); fi
    done
    for at in "${asks[@]}"; do
        if ! magic="$(stat -f -c %t -- "$at" 2>/dev/null)"; then
            printf 'refusing to open %s: cannot tell what filesystem %s is on\n' "$db" "$at" >&2
            return 2
        fi
        kind="$(shared_fs_kind "$magic")"
        if [ -n "$kind" ]; then
            printf 'refusing to open %s: %s is on a %s filesystem shared with another kernel, ' \
                "$db" "$at" "$kind" >&2
            printf 'where SQLite locks and WAL do not work (see .container/sqlite-share-probe.py)\n' >&2
            return 3
        fi
    done
    return 0
}

# jkb_sqlite <db> <sql> [sqlite3 options…] — `sqlite3 [options] <db> <sql>`, after refuse_shared_db.
jkb_sqlite() {
    local db="$1" sql="$2"
    shift 2
    if ! refuse_shared_db "$db"; then return 3; fi
    sqlite3 "$@" "$db" "$sql"
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
