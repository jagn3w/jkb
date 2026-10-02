#!/bin/bash
# Start the jkb container from .container/container.json, then ATTACH VS Code to it.
#
#   ./.container/run.sh                 build if needed, start, run the lifecycle, say how to attach
#   ./.container/run.sh --build         rebuild the image first
#   ./.container/run.sh --open [path]   ...and open a VS Code window attached to it
#   ./.container/run.sh --stop          stop the container (the volumes and image survive)
#   ./.container/run.sh --rm            stop AND remove it, so the next run redoes setup
#                                    (both move a login written since the start into the state
#                                    volume first; see lib.sh's dc_persist_login)
#   ./.container/run.sh --dry-run       print the docker command instead of running it
#   ./.container/run.sh --consumed-keys list the container.json keys this tooling reads
#   ./.container/run.sh --print-args [--posture] [<repo-root>]
#                                    the assembled docker arguments, one per line. `--posture`
#                                    prints only the security half (no name, no mounts, no
#                                    workdir); mutate-verify.sh's control is derived from it.
#   ./.container/run.sh --self-test     exercise the derivation; no Docker needed
#
# WHY THIS EXISTS RATHER THAN DEV CONTAINERS. Its `workspaceFolder` can only be built from
# `${localWorkspaceFolderBasename}` — there is no variable for a folder's path RELATIVE to the
# mount — so a folder nested inside the mount could not be opened, and the near-miss was worse
# than the miss: `~/repos/jkb/.jkb/work/sess` resolved to `/home/vscode/repos/sess`, and a literal
# fallback silently started the agent in a DIFFERENT checkout with every guard still passing.
# Attaching has no `workspaceFolder`: you open any path inside the container, at any depth, and
# `code <path>` from a terminal in there opens more windows on the SAME container.
#
# ONE CONTAINER, EVERY REPO. All of ~/repos is mounted, so attaching once reaches every checkout
# — including a `jkb task work` session inside one — instead of one container per opened folder.
set -euo pipefail

# NO PROGRAM FROM A PLACE AN AGENT CAN WRITE. This runs as you, on the host, unsandboxed, and it runs
# bash, jq, docker, tar and the rest by name. The host auto-mode posture lets a sandboxed agent write
# ~/.cargo (rustup puts ~/.cargo/bin first on PATH), ~/.jkb, ~/.cache, /tmp and the macOS temp roots,
# so a planted ~/.cargo/bin/jq or docker ran as you at the next start (review round 11; the
# kit had moved out of reach and this was the door left). So: an absolute shebang, `#!/usr/bin/env
# bash` having found bash itself through that PATH, and before anything runs, every PATH entry under
# $HOME, /tmp or /private, or relative, is dropped. System and Homebrew locations stay, which is
# where docker and jq live -- Homebrew's are user-owned, so the posture denies Edit on /opt/homebrew
# and /usr/local for the unsandboxed Write tool (review round 14; check-config.sh holds it); a docker under ~ (~/.docker/bin, ~/.orbstack/bin) has to be linked from
# one of them. JKB_RUN_PATH_KEEP is a colon-separated list of directories to keep anyway, and is safe
# only for one NO agent can write: outside the posture's sandbox allowWrite AND Edit-denied, since the
# in-process Write tool is not sandboxed (review round 13 -- ~/.docker/bin passes the first test and
# not the second unless the posture denies it). The tests' stubs in /tmp use it. A tool the filter
# hid is named, with the directory it was in, by `need_tool` below (review round 12).
# Not in --self-test, which check.sh runs and which starts nothing. check-config.sh holds this.
if [ "${1:-}" != --self-test ]; then [ -n "${HOME:-}" ] || { echo "run.sh: HOME is not set" >&2; exit 1; }; jkb_path=""; JKB_PATH_DROPPED=""; IFS=: read -r -a jkb_path_in <<<"$PATH"; for jkb_d in ${jkb_path_in[@]+"${jkb_path_in[@]}"}; do case ":${JKB_RUN_PATH_KEEP:-}:" in *":$jkb_d:"*) [ -n "$jkb_d" ] && { jkb_path="${jkb_path:+$jkb_path:}$jkb_d"; continue; } ;; esac; case "$jkb_d" in ""|[!/]*|"$HOME"|"$HOME"/*|/tmp|/tmp/*|/private|/private/*|/var/folders|/var/folders/*) JKB_PATH_DROPPED="${JKB_PATH_DROPPED:+$JKB_PATH_DROPPED:}$jkb_d"; continue ;; esac; jkb_path="${jkb_path:+$jkb_path:}$jkb_d"; done; PATH="${jkb_path:-/usr/bin:/bin}"; export PATH; fi
# ...and jq with HOME where no file can be: jq sources $HOME/.jq into every program, and the Write
# tool can create ~/.jq. This file's jq readers build the mount list handed to `docker run`, which
# README calls the security boundary (review round 12). check-config.sh holds the line in place.
jq() { HOME=/dev/null command jq "$@"; }

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
CONFIG="$here/container.json"
IMAGE="${JKB_CONTAINER_IMAGE:-jkb-dev}"
NAME="${JKB_CONTAINER_NAME:-jkb-dev}"
# Where the mount puts things. One statement of it, used by both the path mapping and the refusal.
HOST_REPOS="$HOME/repos"
HOST_REPOS_REAL="$(cd "$HOST_REPOS" 2>/dev/null && pwd -P || printf '%s' "$HOST_REPOS")"
CTR_REPOS="/home/vscode/repos"
# Written by setup.sh as its LAST act, so its presence means "setup finished", not "setup started".
# The path itself is JKB_SETUP_MARKER in lib.sh, which both this script and setup.sh source (D52.5).
# Assigned here from that constant rather than spelled again, so the two cannot drift.

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
# need_tool <program> <message> -- the program as a BINARY (`type -P`, never `command -v`, which finds
# the jq wrapper above), or a refusal. One the PATH filter at the top hid is named with the directory
# it was in: a per-user Docker in ~/.docker/bin otherwise failed as a bare "docker is not on PATH"
# (review round 12).
need_tool() {
    type -P "$1" >/dev/null 2>&1 && return 0
    local d hidden=""
    IFS=: read -r -a need_dropped <<<"${JKB_PATH_DROPPED:-}"
    for d in ${need_dropped[@]+"${need_dropped[@]}"}; do [ -z "$hidden" ] && [ -x "$d/$1" ] && hidden="$d"; done
    [ -z "$hidden" ] || die "$1 is in $hidden, which this script drops from PATH: it runs as you, and an agent can plant a program under your home. Keep $hidden only if NO agent can write it: the host posture's sandbox allowWrite must not cover it AND its permissions must deny Edit on it (the in-process Write tool is not sandboxed) -- scripts/auto-mode-posture.json is where both live. Then: JKB_RUN_PATH_KEEP=$hidden (for Docker Desktop, that directory also holds its credential helper)"
    die "$2"
}

# EVERY TOP-LEVEL KEY of container.json must appear here, with the thing that reads it. A key
# nobody reads is a declaration that does nothing while looking like configuration — and the one
# that matters is `mounts`, which is the security boundary. check-config.sh compares the file's
# actual keys against this list and fails on one that is missing, so adding a key forces the
# decision at the moment it is added rather than at the moment someone notices it never applied.
consumed_keys() {
    cat <<'KEYS'
name
build
remoteUser
runArgs
mounts
containerEnv
customizations
KEYS
}

# ---------------------------------------------------------------------------------------------
# Derivation. Pure functions, so --self-test can exercise them on a host with no Docker.
# ---------------------------------------------------------------------------------------------

# `dc_subst`, `dc_run_args` and `dc_remote_user` now live in lib.sh, which this sources before any
# call. They moved because mutate-verify.sh needs the same derivation: its control set was a hand
# copy of container.json's runArgs, and a copy is how the harness came to certify a container this
# script does not produce (D52.6). `dc_subst` returns 1 where this file's copy called `die` — lib.sh
# is sourced by scripts without `set -e` and must not exit for them — so every call here is wrapped.

# The container path of a host path under ~/repos. This is the ONLY thing left of the old host-side
# preflight, and it is a much smaller claim: not "which folder may you open" (attaching answers
# that — any of them) but "the checkout that PROVIDES this container has to be inside the mount",
# because run.sh has to hand the container a path to its own setup.sh.
# SYMLINKS ARE RESOLVED ON BOTH SIDES, which the deleted check-workspace.sh did deliberately and
# this dropped. `~/repos` being a symlink is ordinary — an external volume, a tidier home — and then
# the two spellings never match textually: `$repo` comes from `cd && pwd`, so it can be either form
# depending on how the script was invoked. Refusing there is the bad direction twice over, since the
# remedy printed ("move the checkout under ~/repos") is already satisfied. Both spellings of both
# operands are tried, so either form is accepted and the mount still decides what is reachable.
container_path() { # container_path <host-path>
    local p="$1" p_real root
    p_real="$(cd "$p" 2>/dev/null && pwd -P || printf '%s' "$p")"
    for p in "$1" "$p_real"; do
        for root in "$HOST_REPOS" "$HOST_REPOS_REAL"; do
            [ -n "$root" ] || continue
            case "$p" in
                "$root"/*) printf '%s%s' "$CTR_REPOS" "${p#"$root"}"; return 0 ;;
                "$root")   printf '%s' "$CTR_REPOS"; return 0 ;;
            esac
        done
    done
    return 1
}

# Carry a login written since the container started into the state volume, before the container
# stops or goes (lib.sh's dc_persist_login says why the link alone does not). Never fatal: a stop or
# remove you asked for still happens, and what could not be carried is said, not swallowed.
#
# A STOPPED CONTAINER IS STARTED FOR THIS. It used to be skipped, on the grounds that it had carried
# its login at its last start through here. That misses a token refreshed after the start, which is
# exactly what a container stopped by a reboot or Docker Desktop is holding. The recreate this
# script tells you to run (`--rm && run.sh`) is aimed at that container, so skipping it lost the
# refreshed token on the one path that promised to keep it. Starting raises the firewall and can be
# refused (the egress boot gate), in which case the exec fails and the warning says what was lost.
# The caller stops or removes the container straight after, so it does not stay up.
persist_login() {
    local state ctr
    if [ -z "$repo" ]; then
        echo "warning: the kit's recorded checkout (${KIT_GONE:-none}) is gone, so the login was not carried into the state volume first" >&2
        return 0
    fi
    state="$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" || return 0
    if ! ctr="$(container_path "$repo")"; then
        echo "warning: $repo is not under $HOST_REPOS, so the container cannot see lib.sh; a login written since $NAME started was not moved into the state volume" >&2
        return 0
    fi
    if [ "$state" != "true" ] && ! docker start "$NAME" >/dev/null 2>&1; then
        echo "warning: could not start $NAME to move its login into the state volume; if you are logged out after this, that is why" >&2
        return 0
    fi
    # From the root-owned kit mirror, never the checkout (lib.sh's DC_KIT_DIR says why). A container
    # whose mirror predates the kit has none, and this warns below.
    docker exec -e PATH=/usr/bin:/bin -w "$ctr" "$NAME" /bin/bash -c '. "$1" && dc_persist_login' _ "$DC_CTR_KIT/.container/lib.sh" \
        || echo "warning: could not move $NAME's login into the state volume; if a rebuild logs you out, this is why" >&2
}

# A fingerprint of the DERIVED arguments — not of the file, so a comment edit does not force a
# recreate while any change that actually reaches docker does. Stamped on the container as a label
# at create and compared on every later start, because `docker start` reuses the config the
# container was BUILT with: edit container.json, re-run this, and you get the old container
# silently, with verify.sh then asserting the new declaration against it. That is the declaration
# and the running container disagreeing with nothing to notice — the same shape this directory
# guards against everywhere else, one level over.
args_hash() { # args_hash <arg>...
    local sum
    if command -v shasum >/dev/null 2>&1; then sum="$(printf '%s\n' "$@" | shasum -a 256)"
    else sum="$(printf '%s\n' "$@" | sha256sum)"; fi
    printf '%s' "${sum%% *}"
}

file_sha() { # file_sha <path>
    local sum
    if command -v shasum >/dev/null 2>&1; then sum="$(shasum -a 256 "$1")"
    else sum="$(sha256sum "$1")"; fi
    printf '%s' "${sum%% *}"
}

# WHAT REACHES THE CONTAINER, not the checkout that produced it — and that distinction is the whole
# point of this function. `runArgs` carries `--security-opt seccomp=${localWorkspaceFolder}/…`, so
# the derived argument list is a function of WHERE the checkout lives. Hashing it raw meant a
# `jkb task work` session — the case this whole change exists to make possible — computed a
# different fingerprint from the main checkout, was told `jkb-dev was created from a different
# container.json` (naming a file that had not changed), and was advised `--rm && run.sh`, which
# destroys the shared container along with ~/.vscode-server, its extensions and ~/.jkb-ui-build,
# none of which are in a volume. The main checkout then refused identically. Two checkouts
# ping-ponging, from a declaration they agreed on completely.
#
# So the workspace root is normalised out, and the seccomp profile's CONTENT is folded in to
# replace the path just removed: a profile that really differs must still force a recreate, or
# normalising would have opened a hole where the check used to be.
fingerprint() { # fingerprint <repo-root> <arg>...
    local root="$1"; shift
    local a profile="" norm=()
    # ONLY THE SECCOMP ARGUMENT is normalised, and its content is hashed below instead. Stripping
    # the root from every argument as text moved a repo's own .git bind source to ${WORKSPACE} when
    # the root was the checkout and left it raw when the root was the kit, so switching between the
    # kit and JKB_RUN_FROM_CHECKOUT=1 recreated the container (review round 11).
    for a in "$@"; do
        case "$a" in
            *seccomp=*) profile="${a#*seccomp=}"; norm+=("${a//$root/\$\{WORKSPACE\}}") ;;
            *)          norm+=("$a") ;;
        esac
    done
    [ -n "$profile" ] && [ -f "$profile" ] && norm+=("seccomp-content=$(file_sha "$profile")")
    args_hash ${norm[@]+"${norm[@]}"}
}

# The fingerprint of a config, read the same way the live path reads it — one array element per
# line. `args_hash $(docker_args …)` would word-split instead, so a value with whitespace in it
# would be hashed as two arguments here and as one there, and the self-test would be checking a
# function of something other than what gets run. No declared value contains whitespace today,
# which is what makes this the cheap moment to fix it rather than the expensive one.
config_hash() { # config_hash <config> <repo-root>
    local a=() l out
    # `$( )`, not `< <( )`: see lib.sh. A `die` inside docker_args exits only the subshell, so
    # reading it that way fingerprinted whatever had been emitted before the refusal — half a
    # declaration, hashed as if it were the whole one, which then either matches a running
    # container it does not describe or advises recreating one that was fine.
    out="$(docker_args "$1" "$2")" || die "container.json could not be read; refusing to fingerprint half a declaration"
    while IFS= read -r l; do a+=("$l"); done <<<"$out"
    fingerprint "$2" ${a[@]+"${a[@]}"}
}

# TWO HALVES, ONE EMITTER (D54.1). Every flag is emitted at exactly one place here, and a caller
# chooses which halves it wants rather than re-deriving or subtracting:
#
#   INSTANCE  --name/--detach/--workdir and the mounts. What makes this container THIS container:
#             its identity and its binding to this host's data.
#   POSTURE   --user, runArgs and containerEnv (plus the AppArmor flag, added by assembled_args).
#             What makes it a jkb-dev container at all -- the security configuration.
#
# `posture` as the third argument omits the instance half. mutate-verify.sh's control uses it: the
# harness supplies its own name, its own scratch knowledge base and its own repo bind, and must NOT
# inherit this host's. Subtracting the instance flags from the full set instead would work until a
# pattern stopped matching, and the failure would be the harness's containers bind-mounting the
# REAL ~/.jkb -- mutations writing to the live store. A mode cannot fail that way: the mount lines
# are not emitted at all.
repo_git_mounts() { # -> `--mount <spec>` pairs, one argument per line, for $HOST_REPOS's repositories
    # Each repository's own .git/config and hooks, read-only (docker_args says why). Its own function
    # so the self-test can count what it generates: the declared-mount check once compared the
    # command line against container.json alone, and failed on any machine whose ~/repos held a
    # repository (a Mac with four: 15 mounts against 7 declared).
    local g rel
    for g in "$HOST_REPOS"/*/.git; do
        [ -d "$g" ] || continue
        rel="${g#"$HOST_REPOS"/}"
        case "$rel" in
            *,*) printf 'warning: %s has a comma in its path, which a --mount cannot carry, so its git config stays writable from the container\n' "$g" >&2
                 continue ;;
        esac
        [ -f "$g/config" ] && printf '%s\n' "--mount" "type=bind,source=$g/config,target=$CTR_REPOS/$rel/config,readonly"
        [ -d "$g/hooks" ] && printf '%s\n' "--mount" "type=bind,source=$g/hooks,target=$CTR_REPOS/$rel/hooks,readonly"
    done
    return 0
}

docker_args() { # docker_args <config> <repo-root> [all|posture]  -> one argument per line
    local cfg="$1" root="$2" half="${3:-all}" line sub
    # REFUSED, not defaulted. `[ "$half" = posture ] || <emit>` read every unrecognised value as
    # `all`, so a caller that misspelled the mode got the instance half -- the real ~/.jkb bind
    # among it -- with nothing to notice. The two halves are the security-relevant distinction
    # here, so an unknown one is an error.
    case "$half" in
        all|posture) ;;
        *) printf 'docker_args: unknown half %s (expected all or posture)\n' "$half" >&2; return 1 ;;
    esac

    [ "$half" = posture ] || printf '%s\n' "--name" "$NAME" "--detach" "--workdir" "$CTR_REPOS"

    local user
    user="$(dc_remote_user "$cfg")" || die "container.json's remoteUser could not be read"
    [ -n "$user" ] && printf '%s\n' "--user" "$user"

    # The security flags, from the shared reader mutate-verify.sh's control also uses.
    dc_run_args "$cfg" "$root" || die "container.json's runArgs could not be substituted"

    if [ "$half" != posture ]; then
        # `$( )`, not `< <( )`. The mount list is the security boundary, and a process substitution
        # discards its producer's exit status -- so a jq that failed part way would leave this loop
        # holding a TRUNCATED list and the container would start with some of its mounts, reporting
        # nothing wrong. This was the one `dc_*` reader still read that way; the checker's guard had
        # a hand-written list of producer names and could not see it.
        local specs
        specs="$(dc_mount_specs "$cfg")" || die "container.json's mounts could not be read"
        while IFS= read -r line; do
            [ -n "$line" ] || continue
            sub="$(dc_subst "$line" "$root")" || die "container.json's mounts could not be substituted"
            printf '%s\n' "--mount" "$sub"
        done <<<"$specs"

        # EVERY REPOSITORY'S OWN GIT CONFIG AND HOOKS, READ-ONLY (design D52.11, hole H5). ~/repos is
        # bound read-write, and git on the HOST runs whatever a repository's .git/config names
        # (core.fsmonitor, hooks, filters) — so a container that can write those files can run code
        # as the operator the next time they, or `jkb task land`, run git there. A read-only bind over
        # each stops the direct write: git writes config by lock-and-rename, and a rename over a
        # bind-mounted file fails (measured on Docker Desktop 29.7.2, 2026-09-25: EBUSY; the config
        # was unchanged). A SPEED BUMP, NOT THE BOUNDARY (review round 1): .git/ is writable around
        # these, so renaming .git itself or planting .git/commondir gets past them. jkb's audit before
        # every git call, and the reap scan, are what hold (docs/task-lifecycle.md, D52). Generated, not declared: container.json cannot list repositories. A
        # repository cloned after the container starts is covered at its next start (the fingerprint
        # changes); until then jkb's own git refuses it if it is planted, and the reap service reports
        # it. What this costs inside: `git config --local`, `remote add`, and `--set-upstream-to` /
        # `push -u` (which print an error and exit 0) — set those on the host.
        repo_git_mounts
    fi

    # Through the shared reader, read through `$( )` — the inline jq this replaces was itself a
    # `< <( )` over a producer that can fail, i.e. the shape lib.sh's rule forbids, sitting in the
    # file the rule was written for. mutate-verify.sh's control derives its environment from the
    # same function, so the two cannot disagree about what the container declares.
    local env_out
    env_out="$(dc_container_env "$cfg" "$root")" || die "container.json's containerEnv could not be read"
    if [ -n "$env_out" ]; then
        while IFS= read -r line; do
            [ -n "$line" ] || continue
            printf '%s\n' "--env" "$line"
        done <<<"$env_out"
    fi
}

# THE ONE ASSEMBLY (D54.1). The launcher below and `--print-args` -- which mutate-verify.sh's
# control is derived from -- both go through this, so the container people attach to and the
# container the harness certifies cannot be assembled differently.
#
# mutate-verify.sh used to re-derive its control from container.json with the same readers, one
# file apart. Twelve review findings and eight must-fixes are that second assembly, or the static
# guard written to keep the two agreeing: the guard compared only the security-opt pairs, then only
# the runArgs third, then read a hand-picked source region, then matched only one spelling of the
# argument. Each fix was correct and the next round found the next hole, because a guard over two
# copies cannot be complete. One derivation has no agreement to guard.
#
# THE APPARMOR FLAG IS PART OF IT, not something a caller adds afterwards. It is a host fact rather
# than a declared one -- whether AppArmor mediates depends on the machine, and `apparmor=` where it
# does not is an error, not a no-op -- so a caller reconstructing the flag set from container.json
# alone gets a DIFFERENT container on every Linux host. That is what the two hand-spelled AppArmor
# mutations drifted into twice.
assembled_args() { # assembled_args <repo-root> [posture] -> one docker argument per line
    docker_args "$CONFIG" "$1" "${2:-all}" || return 1
    dc_apparmor_mediates || return 0
    # Refused rather than allowed to be empty: `--security-opt apparmor=` reaches docker as its
    # DEFAULT profile, which is docker-default, whose `mount` denial is the silent state this
    # profile exists to lift. `dc_require_apparmor_profile` exits on an unreadable name, and inside
    # `$( )` that kills only the subshell -- so the emptiness is checked here too.
    local prof
    prof="$(dc_require_apparmor_profile "$here/apparmor-jkb-dev")" || return 1
    [ -n "$prof" ] || return 1
    printf '%s\n' --security-opt "apparmor=$prof"
}

# ---------------------------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------------------------
# WHAT ONE OBSERVATION OF THE CONTAINER MEANS. Pure, so every outcome is a self-test row rather
# than something only a Docker host can reach (D52.4). The entrypoint execs the command, so once
# PID 1 is no longer the entrypoint the boot decision has been made.
settle_step() { # settle_step <running:true|other> <ps output> -> gone|unreadable|waiting|settled
    [ "$1" = true ] || { printf 'gone'; return; }
    # Empty is NOT "the entrypoint has finished". It is `ps` missing from the image, or an exec
    # that failed while the container was still up -- neither of which is an observation of PID 1.
    [ -n "$2" ] || { printf 'unreadable'; return; }
    case "$2" in *entrypoint.sh*) printf 'waiting' ;; *) printf 'settled' ;; esac
}

if [ "${1:-}" = --self-test ]; then
    # shellcheck source=/dev/null
    . "$here/lib.sh"
    fails=0
    eq() { # eq <label> <got> <want>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else printf '  \033[31mFAIL\033[0m %s\n         got:  %s\n         want: %s\n' "$1" "$2" "$3"; fails=$((fails+1)); fi
    }
    rc_of() { ( "$@" >/dev/null 2>&1 ); printf '%s' "$?"; }

    echo "==> run.sh self-test: substitution"
    eq "the workspace folder is the checkout providing the container" \
       "$(dc_subst 'seccomp=${localWorkspaceFolder}/x.json' /a/b/jkb)" "seccomp=/a/b/jkb/x.json"
    eq "its basename is available too" \
       "$(dc_subst 'n=${localWorkspaceFolderBasename}' /a/b/jkb)" "n=jkb"
    eq "localEnv reads the environment" \
       "$(FOO=bar dc_subst 'x=${localEnv:FOO}' /r)" "x=bar"
    eq "several occurrences are all replaced" \
       "$(FOO=b dc_subst '${localEnv:FOO}/${localEnv:FOO}' /r)" "b/b"
    # THE ONE THAT MATTERS. Dev Containers substitutes empty here, which turns
    # `source=${localEnv:HOME}/repos` into `source=/repos` — a different host directory, mounted,
    # silently. An empty value is indistinguishable from a correct one once it is in the string,
    # so it has to be refused before it gets there. Asserted as rc AND message: a refusal that
    # does not name the variable leaves you looking for it in a 100-line JSON file.
    unset JKB_ABSENT_VAR
    if out="$(dc_subst 'x=${localEnv:JKB_ABSENT_VAR}' /r 2>&1)"; then rc=0; else rc=1; fi
    eq "an UNSET localEnv var is refused, not silently emptied" "$rc" "1"
    eq "  ...and the refusal names the variable" \
       "$(grep -c JKB_ABSENT_VAR <<<"$out" || true)" "1"

    echo "==> run.sh self-test: the container path of a host path"
    HOST_REPOS=/h/repos CTR_REPOS=/c/repos
    eq "a repo directly under ~/repos"  "$(container_path /h/repos/jkb)" "/c/repos/jkb"
    eq "a session worktree nested in one — the case Dev Containers could not express" \
       "$(container_path /h/repos/jkb/.jkb/work/sess)" "/c/repos/jkb/.jkb/work/sess"
    eq "~/repos itself"                 "$(container_path /h/repos)" "/c/repos"
    eq "a path outside the mount is refused" "$(rc_of container_path /elsewhere/jkb)" "1"
    # A prefix that merely SHARES CHARACTERS is not inside the mount. `/h/repos-backup` starts with
    # the string `/h/repos` and is a different directory; mapping it would hand the container a
    # path that resolves to something else entirely.
    eq "a sibling with a shared prefix is refused" "$(rc_of container_path /h/repos-backup/jkb)" "1"

    # A SYMLINKED ~/repos, which is ordinary and which the textual comparison refused. Both
    # spellings have to map, because which one you get depends on how the script was invoked: the
    # link name from `$HOME/repos`, the physical path from anything that resolved it on the way.
    lnk="$(mktemp -d)"; trap 'rm -rf "$lnk"' EXIT
    mkdir -p "$lnk/real/jkb"
    ln -s "$lnk/real" "$lnk/link"
    HOST_REPOS="$lnk/link"
    HOST_REPOS_REAL="$(cd "$HOST_REPOS" && pwd -P)"
    eq "a symlinked ~/repos, named through the link" \
       "$(container_path "$lnk/link/jkb")" "/c/repos/jkb"
    eq "...and named through what it resolves to" \
       "$(container_path "$HOST_REPOS_REAL/jkb")" "/c/repos/jkb"
    eq "...while something outside it is still refused" "$(rc_of container_path "$lnk/other")" "1"

    echo "==> run.sh self-test: each repository's git config and hooks, bound read-only"
    fx="$(mktemp -d)"
    mkdir -p "$fx/a/.git/hooks" "$fx/b/.git" "$fx/notrepo"
    : > "$fx/a/.git/config"; : > "$fx/b/.git/config"
    HOST_REPOS="$fx" CTR_REPOS=/c/repos
    mounts="$(repo_git_mounts)"
    eq "a config and hooks for a, a config for b, nothing for a plain directory" \
       "$(grep -cxF -- '--mount' <<<"$mounts" || true)" "3"
    eq "...each read-only" "$(grep -c ',readonly$' <<<"$mounts" || true)" "3"
    eq "...at the repository's container path" \
       "$(grep -cxF -- "type=bind,source=$fx/a/.git/hooks,target=/c/repos/a/.git/hooks,readonly" <<<"$mounts" || true)" "1"
    rm -rf "$fx"

    HOST_REPOS="$HOME/repos" CTR_REPOS=/home/vscode/repos
    HOST_REPOS_REAL="$(cd "$HOST_REPOS" 2>/dev/null && pwd -P || printf '%s' "$HOST_REPOS")"

    echo "==> run.sh self-test: derivation from the real container.json"
    args="$(docker_args "$CONFIG" "$repo")"
    # `-e`, because every pattern here starts with a dash and grep would read it as a flag.
    yes_no() { if grep -q -e "$1" <<<"$args"; then printf 'yes'; else printf 'no'; fi; }
    # ASSERTED AS A FLAG/VALUE PAIR, never as the flag alone. `--security-opt` on its own was the
    # old test, and container.json now carries three of them — so "carries the seccomp profile"
    # passed on finding ANY of them, including with no seccomp profile derived at all. Adjacency is
    # what makes it a pair: `args` is one argument per line, in order, so the value must be the
    # line after its flag. check-config.sh asserts the same pairing in container.json; this asserts
    # the derivation carries it through.
    pair() { awk -v f="$1" -v v="$2" 'p==f && $0 ~ v {n=1} {p=$0} END {print n?"yes":"no"}' <<<"$args"; }
    eq "runs as the non-root user (bubblewrap cannot make namespaces as root)" "$(yes_no '^--user$')" "yes"
    eq "carries the seccomp profile" \
       "$(pair '--security-opt' '^seccomp=.*/seccomp-bwrap\.json$')" "yes"
    # The second half of what bubblewrap needs: docker's masked /proc paths are submounts, so /proc
    # is not fully visible and the kernel refuses a proc mount inside the user namespace whatever
    # the syscall filter allows. Dropped here, run.sh starts a container whose nested sandbox
    # cannot start — which is the state the container shipped in.
    eq "carries the /proc unmask the nested sandbox needs" \
       "$(pair '--security-opt' '^systempaths=unconfined$')" "yes"
    eq "carries NET_ADMIN for the firewall" "$(yes_no '^--cap-add=NET_ADMIN$')" "yes"
    eq "mounts ~/repos"                     "$(yes_no "target=$CTR_REPOS,")" "yes"
    eq "no variable survives into the command line" "$(yes_no '\${local')" "no"
    # PINNED AGAINST AN EMPTY DERIVATION, the same way check-config.sh pins its derived lists.
    # Every assertion above is satisfied by FINDING a string, so a derivation that produced nothing
    # would fail them rather than pass — but the mount count is the one that could quietly shrink,
    # and it is the security boundary, so it is compared against the file rather than to a number.
    declared="$(dc_mount_specs "$CONFIG" | grep -c . || true)"
    generated="$(repo_git_mounts 2>/dev/null | grep -cxF -- '--mount' || true)"
    eq "every declared mount, and each repository's generated git binds, reach the command line" \
       "$(grep -cxF -- '--mount' <<<"$args" || true)" "$((declared + generated))"
    eq "...and there is at least one to reach it" "$([ "$declared" -gt 0 ] && echo yes || echo no)" "yes"

    # A DECLARATION THAT DECLARES NO FLAGS IS REFUSED, not started without them. `dc_run_args` used
    # to exit 0 with no output for an absent `runArgs` — a `while` loop that never runs its body
    # succeeds — so the `|| die` above could not fire and this script would have gone on to build a
    # `docker run` line with no seccomp profile, no /proc unmask, no NET_ADMIN and no pid limit,
    # reporting nothing wrong. mutate-verify.sh refused that state and this file did not, which is
    # a rule two callers had to remember; the refusal is `dc_run_args`' now. `rc_of` runs it in a
    # subshell, or the `die` would take this self-test down with it.
    # Removed inline rather than through a `trap … EXIT`, which replaces rather than adds: the two
    # already here mean only the last one runs.
    norunargs="$(mktemp)"
    dc_strip "$CONFIG" | jq 'del(.runArgs)' > "$norunargs"
    eq "a declaration with no runArgs is refused, not started without its security flags" \
       "$(rc_of docker_args "$norunargs" "$repo")" "1"
    rm -f "$norunargs"

    # ...AND AN UNREADABLE ONE, which is a different failure and used to be a silent success.
    # `dc_remote_user` was a bare pipe with its errors suppressed, so it answered 0-with-no-output
    # for a file it could not read -- "could not tell" spelled exactly like "declares nothing". A
    # verify.sh assertion written on top of that had an unreachable refusal branch and asserted
    # nothing about the running user on precisely the input it existed for.
    # ASSERTED OF THE READER, WITHOUT pipefail. Two earlier versions of these rows could not fail:
    # against `docker_args`, whose refusal is over-determined (dc_run_args and dc_container_env
    # refuse on the same input, so its status said nothing about the reader under test); then
    # against the reader under THIS script's options, where `pipefail` alone makes a bare pipe
    # refuse. lib.sh sets no options and is sourced by several scripts, so what matters is that the
    # reader refuses on its own. Both were found by reverting the fix and re-running -- the only
    # thing that finds a test which cannot fail.
    unparseable="$(mktemp)"; printf 'not json at all\n' > "$unparseable"
    eq "an unreadable declaration is refused by the reader itself, with no pipefail to do it" \
       "$(set +o pipefail; rc_of dc_remote_user /nonexistent-container.json 2>/dev/null)" "1"
    eq "...and so is an unparseable one" \
       "$(set +o pipefail; rc_of dc_remote_user "$unparseable" 2>/dev/null)" "1"
    # THE CONTRASTING CASE, or the two rows above pass for a reader that refuses EVERYTHING.
    nouser="$(mktemp)"; printf '{ "mounts": [] }\n' > "$nouser"
    eq "...while a declaration that genuinely omits remoteUser is not" \
       "$(rc_of dc_remote_user "$nouser" 2>/dev/null)" "0"
    eq "...and answers empty for it" "$(dc_remote_user "$nouser" 2>/dev/null)" ""
    rm -f "$unparseable" "$nouser"

    # The fingerprint that decides whether a running container is stale. The realistic way for it
    # to be useless is to be insensitive to the thing that matters, so it is tested against a
    # changed MOUNT rather than against an arbitrary edit — the mount list is the boundary, and a
    # hash that shrugs at a new mount would let `docker start` hand back a container built without
    # it while every check here read the new declaration.
    tmpcfg="$(mktemp)"; trap 'rm -f "$tmpcfg"' EXIT
    dc_strip "$CONFIG" | jq '.mounts += ["source=/tmp/x,target=/tmp/x,type=bind"]' > "$tmpcfg"
    same="$(config_hash "$CONFIG" "$repo")"
    again="$(config_hash "$CONFIG" "$repo")"
    other="$(config_hash "$tmpcfg" "$repo")"
    eq "the fingerprint is stable across two derivations" "$same" "$again"
    eq "...and a new mount changes it" "$([ "$same" != "$other" ] && echo differs || echo same)" "differs"

    # THE PROPERTY THE PING-PONG DEFECT NEEDED. `runArgs` names the seccomp profile by
    # ${localWorkspaceFolder}, so the raw argument list differs between two checkouts of the same
    # declaration — a session worktree and its main copy — and each then refused the other's
    # container and advised destroying it. Two roots, same content, must fingerprint the same.
    twin="$(mktemp -d)"; trap 'rm -f "$tmpcfg"; rm -rf "$twin"' EXIT
    mkdir -p "$twin/.container"
    cp "$CONFIG" "$twin/.container/container.json"
    cp "$here/seccomp-bwrap.json" "$twin/.container/seccomp-bwrap.json"
    eq "two checkouts of the same declaration agree" \
       "$(config_hash "$twin/.container/container.json" "$twin")" "$same"
    # ...and a root that appears in ANOTHER argument is not stripped from it: with the checkout as
    # root its own .git bind source turned into ${WORKSPACE}, with the kit as root it stayed raw, and
    # switching between them recreated the container (review round 11).
    fpk="$(mktemp -d)"; mkdir -p "$fpk/kit/.container" "$fpk/co/.container"
    printf '{}\n' > "$fpk/kit/.container/s.json"; printf '{}\n' > "$fpk/co/.container/s.json"
    eq "the kit and the checkout fingerprint one container the same" \
       "$(fingerprint "$fpk/kit" "seccomp=$fpk/kit/.container/s.json" "source=$fpk/co/.git/config")" \
       "$(fingerprint "$fpk/co" "seccomp=$fpk/co/.container/s.json" "source=$fpk/co/.git/config")"
    rm -rf "$fpk"
    # ...and normalising the path away must not have taken the profile's CONTENT with it, or the
    # check would be blind to the one file it exists to pin.
    printf '{"tampered":true}\n' > "$twin/.container/seccomp-bwrap.json"
    eq "...but a different seccomp profile does not" \
       "$([ "$(config_hash "$twin/.container/container.json" "$twin")" != "$same" ] && echo differs || echo same)" "differs"

    # THE LIFECYCLE LAYER, which had no harness at all -- and all three of review round 4's
    # must-fixes lived in it (the 125 sentinel consumed as an answer, the dead exit-3 branch, and
    # this function spelling three different unestablished outcomes `settled`). The decision is
    # pure and takes its observations as arguments, so every outcome is a row here rather than
    # something only a Docker host can reach (D52.4).
    #
    # A literal table, not a re-derivation: writing the expectation as a second copy of the
    # condition passes for any condition, including the one this replaced.
    while read -r running psout want; do
        # Blank lines and `#` comments are skipped, so a row can carry the reason it exists. A
        # comment read as a row would not be inert: it becomes `settle_step '#' 'WHAT'`, which
        # returns `gone` and fails against whatever the third field happened to be.
        case "${running:-}" in ''|\#*) continue ;; esac
        [ "$psout" = "-" ] && psout=""
        # Rows are whitespace-split, so a space in an argv is written `\x20` -- and `read -r`
        # does not decode it. Undecoded, the table fed settle_step a literal 34-character string
        # rather than the argv `ps -o args= -p 1` prints, which makes the fidelity claim above
        # false: a future settle_step that looked at the FIRST WORD would be pinned against a
        # string ps never emits. The decoded label is its own evidence -- the self-test prints
        # the row back with real spaces.
        psout="${psout//\\x20/ }"
        got="$(settle_step "$running" "$psout")"
        eq "settle_step $running '$psout' -> $want" "$got" "$want"
    done <<'TABLE'
false   -                     gone
<none>  -                     gone
true    -                     unreadable
true    /usr/local/bin/entrypoint.sh  waiting
true    /bin/bash             settled
true    sleep\x20infinity      settled
# WHAT PID 1 IS ONCE entrypoint.sh HANDS OVER. It execs tini so that PID 1 reaps -- `sleep` never
# wait()s, and every orphan reparented to it stayed a zombie for ever (README.md, "The
# measurements this is built on"). The argv has to stay readable BY THIS FUNCTION, which is the
# half that is easy to break silently.
true    /usr/bin/tini\x20--\x20sleep\x20infinity   settled
# ...AND WHY `--init` IS NOT HOW THAT IS DONE, as a row rather than only as a comment in
# entrypoint.sh. Docker's tini wraps this script instead of being exec'd by it, so PID 1's argv
# names entrypoint.sh for the whole life of the container: this function reads that as "not
# finished yet", settle() never returns 0, and every create and start fails on its 120s budget.
true    /sbin/docker-init\x20--\x20/usr/local/bin/entrypoint.sh\x20sleep\x20infinity   waiting
TABLE

    echo
    [ "$fails" -eq 0 ] || { printf '\033[31m%d failed\033[0m\n' "$fails"; exit 1; }
    printf '\033[32mrun.sh self-test passed\033[0m\n'
    exit 0
fi

# ---------------------------------------------------------------------------------------------
# Real work
# ---------------------------------------------------------------------------------------------
# shellcheck source=/dev/null
. "$here/lib.sh"

# RUN FROM THE KIT, NOT FROM A CHECKOUT (lib.sh's DC_KIT_DIR has the record). This script, lib.sh and
# everything they start used to run from the checkout, which the agent's sandbox can write, so an
# agent's edit here ran as you at the next start. In the kit, `repo` is the checkout recorded at
# install -- the repository the container is for -- and everything this script RUNS comes from the
# kit. From a checkout, only the modes that start nothing work (--self-test, --print-args,
# --dry-run, --consumed-keys) and --install-kit; JKB_RUN_FROM_CHECKOUT=1 is the developer's
# override, for iterating on this directory, and it says so.
KIT_ROOT=""
if [ -f "$here/../$DC_KIT_MARKER" ]; then
    KIT_ROOT="$(cd "$here/.." && pwd)"
    # A RECORDED CHECKOUT THAT IS GONE stops a start and an install, never a stop or a remove: dying
    # here, before the arguments were read, left a kit whose checkout had been deleted unable to
    # stop its own container (review round 10).
    KIT_GONE=""
    if ! repo="$(dc_kit_checkout "$KIT_ROOT")" || [ ! -d "$repo" ]; then
        KIT_GONE="${repo:-none}"; repo=""
    fi
fi
kit_need_checkout() {
    [ -z "${KIT_GONE:-}" ] && return 0
    die "the kit at $KIT_ROOT records a checkout that no longer exists ($KIT_GONE) -- reinstall it from one: <checkout>/.container/run.sh --install-kit"
}
require_kit() {
    [ -n "$KIT_ROOT" ] && return 0
    if [ "${JKB_RUN_FROM_CHECKOUT:-0}" = 1 ]; then
        echo "warning: running from the checkout ($here) because JKB_RUN_FROM_CHECKOUT=1 -- the agent can write every script this runs" >&2
        return 0
    fi
    die "this is the checkout's run.sh, which the agent's sandbox can write -- start the container from the installed kit instead:
    $DC_KIT_DIR/.container/run.sh${1:+ $1}
  Install or refresh the kit from this checkout, after reviewing what changed in .container/ and scripts/:
    $here/run.sh --install-kit"
}

BUILD=0 DRY=0 OPEN=0 open_path=""
while [ $# -gt 0 ]; do
    case "$1" in
        --build)         BUILD=1; shift ;;
        --dry-run)       DRY=1; shift ;;
        --open)          OPEN=1; shift; case "${1:-}" in -*|"") ;; *) open_path="$1"; shift ;; esac ;;
        --consumed-keys) consumed_keys; exit 0 ;;
        # Where the kit is: one answer, lib.sh's, for setup.sh's summary and anything else that
        # would otherwise spell the path again.
        --kit-path)      printf '%s\n' "$DC_KIT_DIR"; exit 0 ;;
        # COPIES, never runs: dc_install_kit copies the kit's paths from the checkout and executes
        # nothing in it. From the kit, the checkout is the one recorded at install, and what changed
        # since is listed first, so a refresh is a decision about named files.
        --install-kit)   kit_need_checkout
                         if [ -f "$DC_KIT_DIR/$DC_KIT_MARKER" ]; then
                             kit_changed="$(dc_kit_stale "$DC_KIT_DIR" "$repo")"
                             if [ -z "$kit_changed" ] && [ "$(dc_kit_checkout "$DC_KIT_DIR")" = "$(cd "$repo" && pwd -P)" ]; then
                                 echo "the kit at $DC_KIT_DIR already matches $repo"; exit 0
                             fi
                             [ -z "$kit_changed" ] || { echo "changed in $repo since the kit was installed:"; printf '    %s\n' $kit_changed; }
                         fi
                         dc_install_kit "$repo" "$DC_KIT_DIR" || die "the kit was not installed"
                         echo "start the container from it: $DC_KIT_DIR/.container/run.sh"; exit 0 ;;
        # THE CONTROL'S FLAGS COME FROM HERE (D54.1). Deliberately before the `container_path`
        # check below: that refuses a checkout outside ~/repos, which is right for STARTING a
        # container and wrong for printing what one would be started with -- CI checks out to
        # /home/runner/work, and mutate-verify.sh's control has to be derivable there.
        # The root is an argument for the same reason: it is the harness's, not this script's.
        --print-args)    shift
                         # PARSED AS A SET, NOT AS A FIXED ORDER, and an unconsumed argument is
                         # refused. This tested `--posture` in the next position ONLY, so the
                         # natural `--print-args <root> --posture` left the mode at `all`, exited
                         # 0, and printed the INSTANCE half -- `--name jkb-dev`, `--detach` and the
                         # real ~/.jkb bind. A caller deriving a control that way builds mutation
                         # containers bind-mounting the live knowledge base, which is the exact
                         # failure docker_args' own comment says a mode makes impossible. A silent
                         # wrong answer from an argument order nobody would call wrong.
                         pa_half=all; pa_root=""
                         while [ $# -gt 0 ]; do
                             case "$1" in
                                 --posture) pa_half=posture; shift ;;
                                 -*)        die "--print-args: unknown option '$1' (it takes --posture and an optional repo root)" ;;
                                 *)         [ -z "$pa_root" ] \
                                                || die "--print-args: two repo roots given ('$pa_root' and '$1')"
                                            pa_root="$1"; shift ;;
                             esac
                         done
                         pa_root="${pa_root:-$repo}"
                         # A ROOT THAT IS NOT A DIRECTORY IS REFUSED. It is substituted into every
                         # ${localWorkspaceFolder}, so a typo'd or flag-shaped value silently
                         # produced `seccomp=--oops/.container/seccomp-bwrap.json` -- a path docker
                         # would reject at run time, from a command that exited 0.
                         [ -d "$pa_root" ] \
                             || die "--print-args: '$pa_root' is not a directory, and it is substituted into every \${localWorkspaceFolder}"
                         need_tool jq "jq is required to read $CONFIG"
                         [ -f "$CONFIG" ] || die "no $CONFIG"
                         # `$( )`, not a bare call: a `die` inside assembled_args exits only the
                         # subshell, and a partial argument list printed as if it were whole is
                         # the control-missing-a-declared-flag state D54.1 exists to end.
                         args_out="$(assembled_args "$pa_root" "$pa_half")" \
                             || die "container.json could not be read; refusing to print a partial declaration"
                         [ -n "$args_out" ] || die "the assembly produced no arguments"
                         printf '%s\n' "$args_out"; exit 0 ;;
        --stop)          require_kit --stop; need_tool docker "docker is not on PATH"; persist_login
                         docker stop "$NAME" >/dev/null 2>&1 && echo "stopped $NAME" || echo "$NAME was not running"; exit 0 ;;
        --rm)            require_kit --rm; need_tool docker "docker is not on PATH"; persist_login
                         docker rm -f "$NAME" >/dev/null 2>&1 && echo "removed $NAME" || echo "$NAME did not exist"; exit 0 ;;
        *)               die "unknown argument '$1' (see the header of $0)" ;;
    esac
done

kit_need_checkout
need_tool jq "jq is required to read $CONFIG"
[ -f "$CONFIG" ] || die "no $CONFIG"

ctr_repo="$(container_path "$repo")" || die "this checkout ($repo) is not under $HOST_REPOS,
  and the container mounts $HOST_REPOS — so it would not be able to see its own setup.sh.
  Move the checkout under $HOST_REPOS. (Which folders you may OPEN is a different question, and
  the answer is any of them: you attach to the container and open any path inside it.)"

# Read with a plain loop, not `mapfile`: macOS ships bash 3.2, which does not have it, and this
# script's whole point is to be the way a Mac gets a container. Through `$( )` rather than
# `< <( )` so that docker_args' refusal reaches this script instead of dying in a subshell and
# leaving a truncated argument list here — a container started without the mounts or the security
# flags it declares. See lib.sh.
ARGS=()
# THE KIT'S ROOT substitutes ${localWorkspaceFolder}: docker reads the seccomp profile from that path
# on the host, and from a checkout it was a file the agent could rewrite. ONE VARIABLE for the
# assembly and the fingerprint below: the fingerprint strips this root out of the arguments, and
# handed `$repo` while the arguments came from the kit, it hashed the kit's absolute path, so every
# existing container read as "created from a different container.json" (review round 10).
args_root="${KIT_ROOT:-$repo}"
ARGS_OUT="$(assembled_args "$args_root")" || die "container.json could not be read; refusing to start a container from a partial declaration"
while IFS= read -r line; do ARGS+=("$line"); done <<<"$ARGS_OUT"

# THE APPARMOR PROFILE IS A HOST FACT, so it is decided here rather than declared in
# container.json: whether AppArmor mediates containers at all depends on the machine, and passing
# `--security-opt apparmor=...` where it is unavailable is an error rather than a no-op. macOS
# adds nothing; Docker Desktop's VM does not run AppArmor.
#
# WHY A PROFILE AT ALL. Docker's docker-default denies `mount`, so bubblewrap -- which Claude
# Code's Linux sandbox shells out to -- creates its namespaces and then fails at its first mount.
# The nested sandbox therefore never started on any AppArmor host, which is most Linux machines.
# `.container/apparmor-jkb-dev` is docker-default with that one rule allowed and every other
# restriction kept; see its header for why `apparmor=unconfined` was not the answer.
#
# INSIDE THE FINGERPRINT, deliberately: a container created without the profile is genuinely not
# the same container as one created with it, and should be reported stale rather than reused.
# The flag itself is appended by `assembled_args` above, which is the one assembly. This name is
# still needed for the preflight `docker run` further down, which reports a profile docker will not
# accept -- a different question from what the container is assembled with.
AA_PROFILE="$(dc_require_apparmor_profile "$here/apparmor-jkb-dev")"

# Hashed BEFORE the label is appended, or the value would have to contain itself.
want_hash="$(fingerprint "$args_root" "${ARGS[@]}")"
ARGS+=(--label "jkb.args-hash=$want_hash")

if [ "$DRY" -eq 1 ]; then
    printf 'docker run'
    printf ' %q' "${ARGS[@]}" "$IMAGE" sleep infinity
    printf '\n'
    exit 0
fi

require_kit
if [ -n "$KIT_ROOT" ]; then
    kit_changed="$(dc_kit_stale "$KIT_ROOT" "$repo")"
    [ -z "$kit_changed" ] || echo "note: the checkout has changed since the kit was installed ($(printf '%s ' $kit_changed)) -- this start uses the kit; to take the changes, review them, then: $KIT_ROOT/.container/run.sh --install-kit" >&2
fi

need_tool docker "docker is not on PATH"
docker info >/dev/null 2>&1 || die "the docker daemon is not reachable"

# The narrowed ~/.jkb binds and the credential's directory must exist on the host: a bind whose source
# is missing is a hard error (D52.8). The credential itself is written by `jkb role rotate-container
# --write` (setup.sh); without one, the container's jkb cannot reach the daemon at all.
mkdir -p "$HOME/.jkb/claude-memory" "$HOME/.jkb/logs" "$HOME/.jkb-container"
chmod 0700 "$HOME/.jkb-container"
[ -s "$HOME/.jkb-container/credential" ] \
    || echo "warning: no container credential at ~/.jkb-container/credential — run ./scripts/setup.sh on the host (jkb role rotate-container --write)" >&2

if [ "$BUILD" -eq 1 ] || ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    # `name` and `build.dockerfile` are READ here. They were listed as consumed keys while nothing
    # looked at either, so check-config.sh printed "every key in container.json is applied by
    # run.sh" about two declarations that did nothing — and the fix for the next inert key would
    # have been to add it to the list, which silences the check rather than satisfying it.
    say "build $IMAGE — $(dc_name "$CONFIG")"
    docker build -t "$IMAGE" -f "$here/$(dc_dockerfile "$CONFIG")" "$here"
fi

state="$(docker inspect -f '{{.State.Status}}' "$NAME" 2>/dev/null || true)"
fresh=0

# CAN DOCKER APPLY THE PROFILE? ASKED ABOVE THE DISPATCH, so it dominates every arm (D45.5).
#
# It used to live in the `*)` create arm alone, on the reasoning that only `docker run` passes
# `--security-opt`. True, and it misses that `docker start` REPLAYS the flags the container was
# created with: the profile has to be loaded again for the restart too. Nothing installs it under
# /etc/apparmor.d, so `apparmor_parser -r -W` loads it into the kernel and a REBOOT UNLOADS IT --
# after which the restart arm died on docker's raw error, ten lines below a remedy it could not
# reach. That arm was unreachable on an AppArmor host until the `--entrypoint true` fix landed,
# because the create probe could never pass and so no container was ever left behind; fixing the
# probe is what made the gap reachable, which is why it is fixed in the same breath.
#
# --entrypoint true, DELIBERATELY. Without it this runs the image's ENTRYPOINT, which refuses to
# boot when egress is unbounded -- and this probe passes no --cap-add NET_ADMIN, so the raise
# always fails and the probe always exits non-zero. It therefore reported "the profile is not
# loaded" on every AppArmor host, loaded or not, and the remedy it printed produced the identical
# failure: the create path was unusable on exactly the hosts the profile exists for. ci.yml's own
# bwrap probe had already learned this and says so in its comment ("the first version of this probe
# kept the image's entrypoint"). What is being asked is only whether DOCKER CAN APPLY THE PROFILE,
# so everything the entrypoint decides is an unrelated moving part.
#
# REFUSED, NEVER FALLEN BACK. Silently dropping the security-opt would start a container whose
# nested sandbox does not work and whose verifier reports docker-default, which is the state this
# whole change ends.
# The image exists by here unconditionally: the block above builds it when it is absent and `set
# -e` aborts if that fails, so this needs no image guard of its own -- one would be a condition
# that cannot be false, which is the shape this directory keeps having to delete.
if dc_apparmor_mediates; then
    # DOCKER'S OWN ERROR IS THE EVIDENCE, not discarded. This used to be `>/dev/null 2>&1` followed
    # by a `die` stating one cause -- so a dockerd built without AppArmor support, a transient
    # daemon error, or a daemon policy refusing `--security-opt apparmor=` was all reported as "the
    # profile is not loaded", with a remedy (`apparmor_parser -r -W`) that succeeds, changes
    # nothing, and leaves the next run failing identically. `container_died` below is this script's
    # own shape for the same problem: name the likely cause AND show what was actually said. A probe
    # that throws away the one line naming the real cause cannot be right about the cause more often
    # than its single guess happens to hold.
    aa_probe_err=""
    if ! aa_probe_err="$(docker run --rm --entrypoint true \
            --security-opt "apparmor=$AA_PROFILE" "$IMAGE" 2>&1 >/dev/null)"; then
        die "docker could not start a container under the AppArmor profile '$AA_PROFILE'.

  docker said:
$(printf '%s\n' "$aa_probe_err" | sed 's/^/      /')

  The usual cause is that the profile is not loaded on this host. Load it (it needs root, and a
  container cannot do it for itself) and run this again:

      sudo apparmor_parser -r -W $here/apparmor-jkb-dev

  It does NOT survive a reboot — nothing installs it under /etc/apparmor.d — so this is also what
  to run after restarting the machine. If docker's message above is about something else (a daemon
  built without AppArmor support, for instance), that is the thing to fix; loading the profile will
  not help.

  It is Docker's own docker-default profile with \`mount\` allowed, which bubblewrap needs; see
  $here/apparmor-jkb-dev for why the whole profile is not simply switched off."
    fi
fi

case "$state" in
    running|exited|created)
        # A container created before this label existed reports `<no value>`, and that is reported
        # as "cannot tell" rather than as "differs" — but it still refuses. Of the two ways to be
        # wrong, refusing costs one command and accepting runs a container built to a
        # specification nobody can see any more.
        have="$(docker inspect -f '{{index .Config.Labels "jkb.args-hash"}}' "$NAME" 2>/dev/null || true)"
        if [ "$have" != "$want_hash" ]; then
            case "$have" in
                ""|"<no value>") reason="$NAME carries no record of what it was created from" ;;
                *)               reason="$NAME was created from a different container.json or seccomp profile" ;;
            esac
            die "$reason.
  \`docker start\` reuses the configuration a container was BUILT with, so starting it would give
  you the old mounts and flags while every check here read the new declaration.

  Recreate it (the image and the volumes — cargo cache, Claude state — all survive):
      $0 --rm && $0"
        fi
        # ...AND THE IMAGE, by the staleness check's own argument. It says `docker start` reuses
        # the configuration a container was BUILT with; that is just as true of the image, and the
        # fingerprint covers only the argument list. So `run.sh --build` — which README.md
        # documents as exactly what you do after changing the Dockerfile or the pinned extension
        # list — built a new image, left the container on the old one, and install-extensions.sh
        # then failed with "was not staged into this image — rebuild the container", advice the
        # user had just followed.
        want_image="$(docker image inspect -f '{{.Id}}' "$IMAGE" 2>/dev/null || true)"
        have_image="$(docker inspect -f '{{.Image}}' "$NAME" 2>/dev/null || true)"
        if [ -n "$want_image" ] && [ -n "$have_image" ] && [ "$have_image" != "$want_image" ]; then
            die "$NAME is running an older build of $IMAGE than the one on disk.
  A container keeps the image it was created from, so the new one does not reach it by starting.

  Recreate it (the volumes — cargo cache, Claude state — survive):
      $0 --rm && $0"
        fi
        if [ "$state" = running ]; then say "container $NAME is already running"
        else say "start $NAME"; docker start "$NAME" >/dev/null; fi
        ;;
    *)
        say "create $NAME"
        # `sleep infinity` because nothing else keeps it alive: the image's job is to be a place to
        # attach to, not to run a program.
        #
        # No AppArmor probe here any more: it is asked above the dispatch, where it covers `docker
        # start` too (which replays the flags the container was created with, so the profile has to
        # be loaded for a restart as well).
        docker run "${ARGS[@]}" "$IMAGE" sleep infinity >/dev/null
        fresh=1
        ;;
esac

# The firewall is the IMAGE'S entrypoint now, so it is already up: it is raised on `docker run`
# and on `docker start`, by whoever issues them, which is what makes it a property of the container
# rather than of this caller. Re-raising here is not a second rule — the raise is idempotent by
# design — it is a SYNCHRONISATION POINT. `docker run` returns as soon as the container is started,
# so without this the exec below could race the entrypoint and do its work on a half-built chain.
# Running it synchronously is the cheapest way to know a raise has completed, and it keeps this
# script's behaviour identical to the version that was actually exercised.
# THE ENTRYPOINT CAN REFUSE TO START THE CONTAINER (D50.3), and `docker run --detach` still
# returns 0 when it does — the refusal is in the container's logs, not in docker's exit code. So
# the next `docker exec` failed with a bare "Container ... is not running" and `set -e` killed this
# script, leaving the actual explanation somewhere nothing pointed at.
# THE ENTRYPOINT'S DECISION TAKES TIME, so this waits for it rather than sampling once (D51.5).
# `docker run --detach` returns as soon as PID 1 starts, and the entrypoint then spends seconds in
# DNS lookups and ipset work before it decides. A single `docker inspect` here therefore saw `true`
# for a container that was about to refuse, the guard passed, and every step below misattributed
# the refusal: the setup probe recorded the definite answer "setup did not complete" and started a
# ten-minute rebuild, `verify_rc` took the daemon's exit 1 for verify's verdict, and the run ended
# by printing "the container is running and attachable" about a container that was not, from a
# verifier that never ran.
# EVERY UNESTABLISHED OUTCOME USED TO BE SPELLED `settled`. The loop fell out of `done` and
# `return 0` when the budget ran out, and the ps probe's `|| return 0` read "no output at all" the
# same way -- so a black-holed resolver (which keeps the raise inside ~15 getent calls at 5s x2 per
# name, well past this budget) and an image without `ps` both reported the entrypoint as finished,
# which is exactly the state this function was added to stop run.sh proceeding through.
# THE LAST PID 1 ARGV settle() SAW, carried out rather than discarded. It read this up to 120
# times and threw every one away, so the rc=3 arm had to send the operator looking for evidence
# elsewhere — and the evidence it named (a completion line in `docker logs`) is one entrypoint.sh
# never prints: its `allowlisted` arm is empty, so a healthy boot says nothing about itself at all.
# Reporting the observation the loop already made needs no new probe and no new failure mode.
SETTLE_PID1=""

settle() { # settle -> 0 settled | 1 container gone | 2 could not read PID 1 | 3 budget exhausted
    local i state argv
    for i in $(seq 1 120); do
        argv="$(docker exec -e PATH=/usr/bin:/bin "$NAME" /bin/sh -c 'ps -o args= -p 1 2>/dev/null || true' 2>/dev/null)"
        [ -z "$argv" ] || SETTLE_PID1="$argv"
        state="$(settle_step \
            "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" \
            "$argv")"
        case "$state" in
            gone)       return 1 ;;
            settled)    return 0 ;;
            # Not retried: `ps` absent is a property of the image, not a transient, and waiting
            # two minutes to say "could not tell" helps nobody. The caller decides what to do.
            unreadable) return 2 ;;
        esac
        sleep 1
    done
    return 3
}

# "Is the container alive, and if not why" DOMINATES every exec below, so it is asked in one place
# and every failing exec routes through it (D45.5's rule, applied here). It used to be asked once,
# up front, in a form that could not distinguish a dead container from a false probe.
container_died() { # container_died <what was being attempted>
    printf '\n\033[31merror:\033[0m %s is not running (while: %s).\n' "$NAME" "$1" >&2
    # OFFERED AS A LIKELY CAUSE, NOT STATED AS THE CAUSE. A container can be gone for reasons that
    # have nothing to do with egress -- an OOM kill, `docker stop`, a crash in something the
    # entrypoint exec'd -- and asserting the firewall refused sends the reader to audit a boundary
    # that is fine. The log below is the evidence; this line only says where to look first.
    printf 'The most likely cause is the entrypoint refusing to start a container whose firewall\n' >&2
    printf 'could not establish that egress is bounded. The log is the evidence:\n\n' >&2
    docker logs --tail 20 "$NAME" 2>&1 | sed 's/^/  /' >&2
}

# Runs a command in the container and tells a TRANSPORT failure from the command's own answer. They
# shared exit 1, so "the container is gone" and "the thing I asked about is false" were the same
# value.
#
# IT NO LONGER RETURNS A SENTINEL, and that is the fix. It used to report and return 125 with a
# comment saying no probe uses that as an answer -- but the two capturing call sites did exactly
# that: `verify_rc=$?` and `raise_rc=$?` took 125 as the command's own result, so a container that
# died during verify printed "verify.sh reported problems (exit 125) -- the container is running
# and attachable ... the failing lines above say what to do", with no failing lines, about a
# container that was gone and a verifier that never ran.
#
# A value every caller has to remember to check is the defect, not the callers. There is no call
# site here that could sensibly continue once the container is gone, so this reports and STOPS.
#
# The one caller that reads output through a command substitution is unaffected by design: `exit`
# there ends the subshell, the substitution yields nothing, and that site already treats "neither
# word came back" as its own condition rather than as an answer.
in_container() { # in_container <args...>
    local rc=0
    docker exec "$@" || rc=$?
    if [ "$rc" -ne 0 ] \
       && ! grep -q true <<<"$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)"; then
        container_died "docker exec $*"
        exit 1
    fi
    return "$rc"
}

settle_rc=0; settle || settle_rc=$?
case "$settle_rc" in
    0) ;;
    1) container_died "waiting for the entrypoint to settle"; exit 1 ;;
    2) # THE DECLARED LIMIT, from the arguments this script derived from container.json, so the
       # sentence below names a number the reader can compare against rather than a flag name.
       # `docker stats` prints PIDS as a bare count and never the limit, so without this the
       # operator is told to judge "near the --pids-limit" from output that does not contain it.
       pids_limit=""
       for _i in "${!ARGS[@]}"; do
           [ "${ARGS[$_i]}" = "--pids-limit" ] && { pids_limit="${ARGS[$((_i+1))]:-}"; break; }
       done
       # Reported, not continued through. Proceeding here is what the old code did for every one
       # of these, and it is what let the execs below race a half-built chain.
       printf '\n\033[31merror:\033[0m could not read PID 1 in %s, so this script cannot tell\n' "$NAME" >&2
       printf 'whether the entrypoint has finished deciding. Everything below would be racing a\n' >&2
       printf 'firewall that may still be coming up.\n\n' >&2
       # TWO CAUSES, AND THE SECOND ONE IS THIS REPO'S OWN BUG. The probe is `docker exec ... ps`,
       # so it needs to FORK INSIDE the container -- and a container whose PID 1 does not reap
       # eventually cannot, having spent every one of container.json's 4096 pids on zombies. That
       # is the exact end state verify.sh's reaping assertion exists to name, and naming only a
       # missing `ps` here pre-empted it: the reader was sent to audit the image while the actual
       # remedy, which is to recreate the container, was never printed because verify.sh never ran.
       #
       # ASK THE COUNTER THE LIMIT IS APPLIED TO, NOT A PROCESS LISTING. `--pids-limit` is enforced
       # by the pids cgroup controller, and `pids.current` -- which `docker stats` prints as PIDS --
       # is the number it bounds; a zombie is charged against it until it is reaped, which is
       # exactly why zombies exhaust it. `docker top` lists PROCESSES, from the host, needing no
       # fork inside -- and a listing is not the charge: whether a dying leader appears in it is a
       # kernel detail nobody here has measured, so a remedy resting on that count alone could read
       # empty in the one state it exists to name. Printing BOTH removes the dependency and is
       # strictly more informative, because the DISCREPANCY is the signature: the charge near the
       # limit while the listing shows a handful of live processes is what a zombie pile looks like
       # from outside, and no single number says that.
       printf '  `ps` may be missing from the image — or the container cannot fork at all,\n' >&2
       printf '  which is what a PID 1 that does not reap comes to after a day (every one of\n' >&2
       printf '  its pids spent on zombies). Tell them apart from outside the container:\n\n' >&2
       printf '    docker stats --no-stream %s   # PIDS = the counter --pids-limit bounds\n' "$NAME" >&2
       printf '    docker top %s                 # what is actually there\n' "$NAME" >&2
       # THE SUFFICIENT CONDITION, NOT A CONJUNCTION. This used to read "PIDS near the limit AND
       # only a handful of processes listed", which gates the remedy on an unmeasured premise --
       # and probably the wrong way round: a zombie is charged against pids.current precisely
       # BECAUSE it is still a task, so `docker top` most likely lists thousands of `<defunct>`
       # rows rather than a handful, the literal test fails, and the operator concludes it is not
       # the zombie pile and skips the recreate. PIDS at the limit is on its own grounds to
       # recreate, whatever the listing looks like; the listing is corroboration, worded as such.
       if [ -n "$pids_limit" ]; then
           printf '\n  PIDS at or near %s (the limit this container declares) is on its own enough\n' "$pids_limit" >&2
       else
           printf '\n  PIDS at or near the limit this container declares is on its own enough\n' >&2
       fi
       printf '  to recreate it — no second condition needed. If it IS zombies, `docker top`\n' >&2
       printf '  will be mostly `<defunct>` rows.\n\n' >&2
       printf '    %s --rm && %s --build\n' "$0" "$0" >&2
       exit 1 ;;
    3) printf '\n\033[31merror:\033[0m %s is still running its entrypoint after 120s.\n' "$NAME" >&2
       printf 'The firewall raise resolves the allowlist by DNS, so a black-holed resolver holds it\n' >&2
       printf 'here. The container log says where it is:\n\n' >&2
       docker logs --tail 20 "$NAME" 2>&1 | sed 's/^/  /' >&2
       # A SECOND CAUSE, READ OFF THE OBSERVATION RATHER THAN LOOKED FOR ELSEWHERE. `settle_step`
       # treats a match of *entrypoint.sh* in PID 1's argv as "not finished yet". Under Docker's
       # `--init` that is true for the container's whole life -- docker-init WRAPS this script
       # instead of being exec'd by it -- so settle() can never return 0. It is pinned as a
       # self-test row above.
       #
       # The argv IS the discriminator, and settle() has just read it 120 times. The first version
       # of this message instead asked the reader to look for a completion line in `docker logs`,
       # which entrypoint.sh never prints (its `allowlisted` arm is empty), and fell back to
       # `docker inspect .HostConfig.Init` -- the flag as REQUESTED, which is `<nil>` when unset and
       # says nothing about a daemon-level `init: true`. Printing what was observed needs neither.
       printf '\n  PID 1 is: %s\n' "${SETTLE_PID1:-<never readable>}" >&2
       case "$SETTLE_PID1" in
           *docker-init*|*/sbin/init*|*tini*entrypoint.sh*)
               printf '\n  That argv WRAPS the entrypoint rather than being exec"'"'"'d by it, so this is not a\n' >&2
               printf '  stuck boot: `--init` (or a daemon-level init) is making docker-init PID 1,\n' >&2
               printf '  whose argv names entrypoint.sh for ever. Drop `--init` from runArgs.\n' >&2 ;;
           *)
               printf '\n  That argv is the entrypoint itself, so it really is still deciding — the DNS\n' >&2
               printf '  cause above is the one to chase.\n' >&2 ;;
       esac
       exit 1 ;;
esac

# Already raised by the entrypoint, on this start and on every other. Re-raising here is not a
# second rule — the raise is idempotent by design — it is a SYNCHRONISATION POINT: `docker run`
# returns as soon as the container is started, so without this the execs below could race the
# entrypoint and do their work against a half-built chain.
#
# Its failure is RECORDED, not fatal. The entrypoint has already made the boot decision on the
# verdict; if the cause persists the re-raise fails the same way, and letting `set -e` kill this
# script here would skip the reap, skip verify.sh, and so skip the very reporting the design makes
# verify responsible for — for the one caller that runs it.
say "egress firewall"
raise_rc=0
in_container "$NAME" /usr/bin/sudo -n /usr/local/bin/init-firewall.sh || raise_rc=$?
if [ "$raise_rc" -ne 0 ]; then
    say "the raise reported a failure (exit $raise_rc) — verify.sh below reports what state it left"
fi

# WHICH ARM, and it is asked of whether SETUP FINISHED — not of whether this invocation created the
# container. `fresh=1` meant "docker run just succeeded", which is true a full minute before setup
# is done: interrupt the toolchain download, or let any step fail, and the container is left with
# no posture, no toolchain and no jkb, while every later run takes the verify arm instead — so
# setup.sh becomes unreachable for the life of the container and only `--rm` escapes. Dev
# Containers recorded postCreate completion and re-ran it after a failure; this is that. The marker
# is in the writable layer, not a volume, so recreating the container correctly redoes setup.
# PRINTS A WORD rather than relying on an exit code the daemon also uses (D51.5). `test -e`
# returning non-zero meant both "the marker is absent" and "the exec never ran", and the second was
# recorded as the definite answer `setup_done=0` — so a container whose setup had completed was
# told it had not and started the ten-minute toolchain rebuild. An answer that cannot be
# distinguished from a transport failure is not an answer.
# NO `2>/dev/null` ON THIS CALL. `in_container` dumps `docker logs --tail 20` through
# `container_died` when the container is gone, and redirecting the callee's stderr threw away the
# one diagnostic that would have named the reason -- while the arm below went on to say the
# container "is still running".
setup_probe="$(in_container -e PATH=/usr/bin:/bin "$NAME" /bin/sh -c "test -e '$JKB_SETUP_MARKER' && echo done || echo missing")" || setup_probe=""
case "$setup_probe" in
    done)    setup_done=1 ;;
    missing) setup_done=0 ;;
    *)       # Neither word came back: the exec failed. Never read as "setup did not complete" --
             # that answer would start a ten-minute toolchain rebuild on a container that had
             # already been set up.
             #
             # THE QUESTION IS ASKED, NOT INFERRED. This used to assert the container "is still
             # running" on the grounds that `in_container` reports and exits when it is gone -- but
             # that call is inside a COMMAND SUBSTITUTION, so its `exit 1` ends only the subshell
             # (its own comment says so). A container that died between `settle` and this probe --
             # an OOM kill, an external `docker stop`, a daemon restart -- therefore reached this
             # arm and was described as running. Announcing the wrong cause is the misattribution
             # this seam exists to remove, and inferring it from a guard that cannot stop us is how
             # it came back one level along.
             if [ "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" != "true" ]; then
                 container_died "asking whether first-run setup had completed"
                 exit 1
             fi
             printf '\n\033[31merror:\033[0m could not ask %s whether setup completed, and it IS\n' "$NAME" >&2
             printf 'running — so the exec failed for a reason this script has not established.\n' >&2
             printf 'Try again; if it persists, attach by hand and look at the container.\n' >&2
             exit 1 ;;
esac

# THE TRANSCRIPT SWEEP, BEFORE EVERYTHING THAT CAN FAIL. Same reason the reap below runs before
# the verify — a failure about something else must not disable it — but carried further, because
# the sweep is the more urgent job: the state it prevents is a container in which NO Bash tool
# call works at all (E2BIG at spawn, measured 2026-09-28; sweep-transcripts.sh carries the
# numbers). Never fatal: what it could not archive it says, and a deny list slightly too long is
# the state we were already in.
#
# ABOVE FIRST-RUN SETUP, which is the whole point of the position. `setup.sh` is called
# UNGUARDED under `set -e`, so a setup that fails takes the rest of this script with it — and
# the documented E2BIG recovery is `run.sh --rm && run.sh`, which DESTROYS the container and
# therefore always re-runs setup, while the transcripts it has to reclaim live in the state
# volume and survive the `--rm`. So the one path that exists to recover from a wedged container
# was also the one path where an unrelated setup failure meant the sweep never ran at all. It
# needs nothing setup provides: a running container and `$ctr_repo`, both settled far above, and
# a plain `bash` script that reads no `jkb` inside the container (the live list is asked on the
# HOST, below). The reap cannot move up here with it — that one does need the container's jkb.
#
# ...AND IT CARRIES THE LIVE-SESSION LIST, which is what lets it run on every path. It did not, and
# the repair for that was to skip the sweep when the container was ALREADY running — on the ground
# that "nothing is open at container start" is only true when the container just started. That
# traded a rare loss for a common one: a container up for days, or started from Docker Desktop, was
# then swept by nothing on this path, the window stayed refused, and the documented recovery for the
# very E2BIG this exists to prevent — re-running run.sh — no longer recovered. Worse, the skip
# asserted that the reaper's tick owned the container, which this script never checked and which is
# false whenever `setup.sh --no-service` was used or the unit is stopped.
#
# So the list comes from the same registry the reaper reads, through the daemon that already holds
# it. Empty is the safe direction and the one the reaper also takes when it cannot read: the recency
# window and the floor still stand. `-e` reaches `in_container` because that is a plain `docker
# exec`; a shell prefix would set the variable for the docker CLI and never enter the container.
#
# ASKED FOR THE IDS, NOT FOR THE ROWS. This read `--json` and pulled `.[].session` out with `jq` — a
# second answer to "which sessions count", in another language, with nothing comparing the two. They
# diverged immediately: when the reaper learned that an unclosable row is not a live session, this
# path went on handing out ids from rows that had been open for months, which is the state that makes
# the sweep reclaim nothing. `--live-ids` applies the one rule (crates/jkb-cli/src/transcripts.rs)
# and prints one id per line, so there is nothing here to keep in step and no JSON field to agree
# about. `sweep_ok` records whether the registry could be READ, which is not the same fact as its
# being empty — a sweep that ran unprotected must not look like one with nothing to protect.
#
# ASKED ONLY WHEN SOMETHING COULD BE RUNNING IN IT. A container this script just CREATED, or just
# started from stopped, has no sessions inside it — so the correct keep list there is empty, and
# asking the registry actively harms: nothing closes a session's row when its container is stopped
# or removed (no SessionEnd hook fires), so every session from the destroyed container still looks
# recently seen. Passing those made `run.sh --rm && run.sh` — the documented E2BIG recovery — hold
# three or four dead sessions' whole subagent subtrees, which the README measures as exceeding the
# entire budget: the recovery reclaimed nothing for up to six hours, where before this trigger
# carried any list at all it recovered in one pass.
# THE KIT, MIRRORED IN ROOT-OWNED, before anything below runs a script in the container. The sweep,
# setup.sh, the login step and verify.sh run unsandboxed in here, and they ran from the bind-mounted
# checkout, which the sandbox writes. Now they run from $DC_CTR_KIT, which only this step changes,
# through the same root step that copies the host's git hooks. Fatal, because the alternative is
# running them from the checkout.
say "container kit"
kit_src="$KIT_ROOT"; kit_stage=""
if [ -z "$kit_src" ]; then
    kit_stage="$(mktemp -d)"
    dc_install_kit "$repo" "$kit_stage/kit" >/dev/null || die "could not stage the kit from $repo"
    kit_src="$kit_stage/kit"
fi
# NO LINK IN WHAT IS MIRRORED: the mirror's tar dereferences (-h, for the host's hooks), so a link
# in the kit would put its target's bytes in the container. dc_install_kit refuses them in the copy
# it makes; this holds the line for a kit made any other way (review round 14).
kit_odd="$(find "$kit_src" ! -type f ! -type d 2>&1 | head -3)"
[ -z "$kit_odd" ] || die "the kit at $kit_src holds something that is not a regular file or a directory, so it is not mirrored: $kit_odd -- reinstall it: run.sh --install-kit"
kit_rc=0; dc_mirror_hooks "$kit_src" "$DC_CTR_KIT" "$NAME" docker "the container kit" || kit_rc=$?
[ -z "$kit_stage" ] || rm -rf "$kit_stage"
[ "$kit_rc" -eq 0 ] || die "could not install the container kit at $DC_CTR_KIT in $NAME -- the sweep, setup.sh and verify.sh run only from there, never from the checkout the agent can write"

sweep_keep=""
sweep_ok=yes
if [ "$state" = running ]; then
    sweep_ok=no
    # On the host `jkb` lives in ~/.cargo/bin, which the PATH filter at the top drops: an agent can
    # write it, so this no longer runs it, and the sweep holds sessions by its recency window and
    # floor instead (the arm below says so). A jkb on a system PATH is still asked.
    if command -v jkb >/dev/null 2>&1; then
        if sweep_ids="$(jkb notify sessions --live-ids 2>/dev/null)"; then
            sweep_ok=yes
            sweep_keep="$(printf '%s' "$sweep_ids" | tr '\n' ' ')"
        fi
    fi
fi
if [ "$sweep_ok" = no ]; then
    say "transcript sweep: could not ask jkb which sessions are live — sweeping with the recency window and the floor only"
elif [ "$state" = running ]; then
    say "transcript sweep: holding $(printf '%s' "$sweep_keep" | wc -w | tr -d ' ') live session(s)"
else
    say "transcript sweep: $NAME was not running, so nothing in it is live"
fi
# `/bin/bash` by absolute path: `docker exec` resolves a bare `bash` through the container's PATH,
# whose first entries the sandbox can write, and these scripts run UNSANDBOXED (review round 7).
# EVERY exec in this file names its program absolutely, and pins PATH for what that program runs
# unless it pins its own (the sweep), needs the toolchain (setup.sh), or is sudo, whose secure_path
# replaces it (round 8: the round-7 fix covered two of nine). check-config.sh holds all of them.
in_container -e "JKB_KEEP_SESSIONS=$sweep_keep" -e "JKB_REPO_ROOT=$ctr_repo" -w "$ctr_repo" "$NAME" /bin/bash "$DC_CTR_KIT/.container/sweep-transcripts.sh" || true

if [ "$setup_done" -eq 0 ]; then
    [ "$fresh" -eq 1 ] || say "setup did not complete last time — re-running it"
    say "first-run setup (this is the slow one — toolchain, jkb, extensions)"
    in_container -e "JKB_REPO_ROOT=$ctr_repo" -w "$ctr_repo" "$NAME" /bin/bash "$DC_CTR_KIT/.container/setup.sh"
fi

# THE LOGIN, CARRIED ON EVERY START. A container that was stopped outside this script (Docker
# Desktop, `docker stop`) may hold a token written since its last start; this is the first chance
# to move it into the state volume, and it happens before verify.sh looks at the links. Not fatal:
# verify.sh below reports whatever state it leaves.
say "login state"
in_container -e PATH=/usr/bin:/bin -w "$ctr_repo" "$NAME" /bin/bash -c '. "$1" && dc_persist_login' _ "$DC_CTR_KIT/.container/lib.sh" \
    || say "the login could not be moved into the state volume — verify.sh below reports what state it is in"

# THE HOST'S GIT HOOKS, COPIED IN ON EVERY START (lib.sh's dc_mirror_host_hooks says why a copy and
# not a mount). Every start, so an edit to a hook on the host arrives at the next one. Run here, on
# the host, because the host's hooks directory is exactly what the container cannot see. Never
# fatal: verify.sh below reports a hooks directory git cannot find.
say "git hooks"
dc_mirror_host_hooks "$NAME" "$CONFIG"

# THE REAP RUNS BEFORE THE VERIFY, and independently of it. It was after, and verify.sh exits 1 on
# any failing assertion under `set -e` — so one assertion about something else disabled the only
# reaper that can finish container-side archive records, whose /home/vscode/... paths the host's
# `com.jkb.reap` cannot see, while multi-gigabyte archives accumulate. The deleted postStartCommand
# ran it unconditionally and this is that shape back.
in_container -e PATH=/usr/bin:/bin -w "$ctr_repo" "$NAME" /usr/local/lib/jkb-hook/jkb task reap || true

# ONE VERIFIER, AFTER BOTH ARMS. It used to be the last line of setup.sh on the fresh path and a
# separate call here on the restart path — so the review's "a fatal verify suppresses everything
# after it" was fixed in one arm and left in the other, which is what fixing at a site rather than
# at the rule buys you. Its result is CARRIED rather than fatal: several assertions name a remedy
# you run from inside an attached window, and dying here printed the problem while withholding the
# way to fix it. The exit code is still verify's, at the very end.
say "verify"
verify_rc=0
# ...AND IT GETS THE SAME KEEP LIST THE SWEEP GOT. verify.sh measures the deny list by running
# the sweep's own `--dry-run`, and without the ids it measured a DIFFERENT tree from the one the
# real sweep had just acted on: a container held down by live sessions came out `over budget`
# (exit 1) instead of `beyond any sweep` (exit 3), and the FAIL named the floor, an unwritable
# archive and colliding destinations — none of which applied, while the actual cause, a live
# session holding its whole subagent subtree, was not among them. Reproduced against the real
# scripts: 120 subagent transcripts under one live session, 40 archivable, budget 30000 —
# `beyond` with the list, `over` without it.
in_container -e "JKB_KEEP_SESSIONS=$sweep_keep" -e "JKB_REPO_ROOT=$ctr_repo" -w "$ctr_repo" "$NAME" /bin/bash "$DC_CTR_KIT/.container/verify.sh" || verify_rc=$?

say "attached VS Code windows"
cat <<EOF
  Command Palette -> "Dev Containers: Attach to Running Container" -> $NAME
  then File -> Open Folder to any path inside, e.g.

    $CTR_REPOS/$(basename "$repo")
    $CTR_REPOS/<another repo>
    $ctr_repo

  From a terminal in an attached window, \`code <path>\` opens another window on the same
  container. There is no workspaceFolder here, so any depth works.
EOF

if [ "$OPEN" -eq 1 ] && [ "$verify_rc" -ne 0 ]; then
    # OPENING A WINDOW IS AN ACTION, NOT A NOTE — and it is the action of starting an agent session.
    # Carrying verify's result so the attach instructions still print is right; launching a window
    # into a container whose verifier just reported UNDECLARED mounts, permitted egress to a
    # non-allowlisted host, or a broken posture is not.
    #
    # BOOTING IS NOT ENDORSING (D51.7). Exit 3 means every failure is one this container TOLERATES
    # rather than a broken boundary. It read "a condition this container was CONFIGURED to accept —
    # in practice, the unfiltered-egress override", which was true while that override was the only
    # producer and stopped being true when the transcript deny list joined it: nobody configures a
    # container to accumulate run journals past what any sweep can reclaim. The branch below no
    # longer names a cause for the same reason, and this comment is the one a reader reaches first. That override exists so a
    # container BOOTS and can be attached to and diagnosed; it does not make that container a place
    # to run an agent unattended, which is precisely what this flag would do. So it is still
    # refused, and the message says something that can be acted on: the previous wording told you
    # to "fix them" about a condition the design REQUIRES to keep failing, which is advice with no
    # followable step.
    printf '\n\033[31mnot opening a window:\033[0m verify.sh reported problems (exit %s).\n' "$verify_rc" >&2
    if [ "$verify_rc" -eq 3 ]; then
        # WHAT VERIFY REPORTED, NOT WHAT EXIT 3 USED TO MEAN. This named the unfiltered-egress
        # override as the only thing exit 3 can be, and then told the operator to unset a variable
        # and recreate the container. Exit 3 gained a second producer the day verify.sh started
        # reporting the transcript deny list: past the point where the run journals and the kept
        # newest transcripts exceed the budget on their own, no sweep helps, which is a condition to
        # act on rather than a broken boundary -- and recreating the container changes nothing,
        # because those journals live in a volume. Two conditions with opposite remedies cannot
        # share one hard-coded sentence, so the remedy is read off verify's own output, which each
        # accepted arm already carries.
        printf 'Every failure it reported is one this container tolerates rather than a broken\n' >&2
        printf 'boundary. That is a container to attach to and diagnose, not one to run an agent in\n' >&2
        printf 'unattended, so no window is opened while it holds. The FAIL line(s) above say which\n' >&2
        printf 'condition it is and what to do about it — they want different things, and the list\n' >&2
        printf 'of them is not repeated here, because a second copy of it goes stale. Or attach by\n' >&2
        printf 'hand with the Command Palette route above.\n' >&2
    else
        printf 'Fix them, or attach by hand with the Command Palette route above if you know why.\n' >&2
    fi
elif [ "$OPEN" -eq 1 ]; then
    [ -n "$open_path" ] || open_path="$ctr_repo"
    # A HOST PATH IS THE NATURAL THING TO TYPE — you are standing in one — and appending it to the
    # container URI verbatim opened a window on a folder that does not exist in there, with no
    # error, because VS Code will happily attach to a path it then cannot list. Translate anything
    # under ~/repos; leave everything else alone, since a path already in container form (or
    # anywhere else inside the container) is equally legitimate and this cannot tell them apart
    # except by the mount, which is exactly the question container_path answers.
    if translated="$(container_path "$open_path" 2>/dev/null)"; then
        [ "$translated" = "$open_path" ] || say "opening the container's $translated (you named the host path)"
        open_path="$translated"
    fi
    need_tool code "the 'code' CLI is not on PATH (VS Code: 'Shell Command: Install code in PATH')"
    # Attached containers are addressed by a hex-encoded JSON authority. This spelling is VS Code's
    # and is not something this repo can verify from a test, so it is a convenience on top of the
    # Command Palette route above rather than the documented way in: if it stops working, the
    # instructions printed above still do.
    hex="$(printf '{"containerName":"/%s"}' "$NAME" | od -A n -t x1 | tr -d ' \n')"
    say "opening $open_path"
    code --folder-uri "vscode-remote://attached-container+$hex$open_path"
fi

# Carried from the verify above rather than exiting at it: the container IS up and attachable, and
# several assertions name a remedy you run from inside it, so the instructions had to be printed
# first. The exit code is still verify's, so a caller or CI cannot read a failed check as a pass.
if [ "${verify_rc:-0}" -ne 0 ]; then
    printf '\n\033[31mverify.sh reported problems (exit %s)\033[0m — the container is running and\n' "$verify_rc" >&2
    printf 'attachable, but it is not what it claims to be. The failing lines above say what to do.\n' >&2
    exit "$verify_rc"
fi
