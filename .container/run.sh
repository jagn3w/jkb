#!/usr/bin/env bash
# Start the jkb container from .container/container.json, then ATTACH VS Code to it.
#
#   ./.container/run.sh                 build if needed, start, run the lifecycle, say how to attach
#   ./.container/run.sh --build         rebuild the image first
#   ./.container/run.sh --open [path]   ...and open a VS Code window attached to it
#   ./.container/run.sh --stop          stop the container (the volumes and image survive)
#   ./.container/run.sh --rm            stop AND remove it, so the next run redoes setup
#   ./.container/run.sh --dry-run       print the docker command instead of running it
#   ./.container/run.sh --consumed-keys list the container.json keys this tooling reads
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

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
CONFIG="$here/container.json"
IMAGE="${JKB_CONTAINER_IMAGE:-jkb-dev}"
NAME="${JKB_CONTAINER_NAME:-jkb-dev}"
# Where the mount puts things. One statement of it, used by both the path mapping and the refusal.
HOST_REPOS="$HOME/repos"
CTR_REPOS="/home/vscode/repos"

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

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

# Dev Containers' variable syntax, with ONE deliberate difference: an unset ${localEnv:VAR} is a
# hard error here, where Dev Containers substitutes the empty string. That default is how
# `source=${localEnv:HOME}/repos` quietly becomes `source=/repos` — a different host directory,
# mounted into the container, with nothing to notice it. A boundary must not be able to move
# because a variable was not set.
dc_subst() { # dc_subst <string> <repo-root>
    local s="$1" root="$2" var val
    s="${s//\$\{localWorkspaceFolderBasename\}/$(basename "$root")}"
    s="${s//\$\{localWorkspaceFolder\}/$root}"
    while [[ "$s" =~ \$\{localEnv:([A-Za-z_][A-Za-z0-9_]*)\} ]]; do
        var="${BASH_REMATCH[1]}"
        [ -n "${!var+set}" ] || die "container.json references \${localEnv:$var}, which is not set"
        val="${!var}"
        s="${s//\$\{localEnv:$var\}/$val}"
    done
    printf '%s' "$s"
}

# The container path of a host path under ~/repos. This is the ONLY thing left of the old host-side
# preflight, and it is a much smaller claim: not "which folder may you open" (attaching answers
# that — any of them) but "the checkout that PROVIDES this container has to be inside the mount",
# because run.sh has to hand the container a path to its own setup.sh.
container_path() { # container_path <host-path>
    local p="$1"
    case "$p" in
        "$HOST_REPOS"/*) printf '%s%s' "$CTR_REPOS" "${p#"$HOST_REPOS"}" ;;
        "$HOST_REPOS")   printf '%s' "$CTR_REPOS" ;;
        *) return 1 ;;
    esac
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

# The fingerprint of a config, read the same way the live path reads it — one array element per
# line. `args_hash $(docker_args …)` would word-split instead, so a value with whitespace in it
# would be hashed as two arguments here and as one there, and the self-test would be checking a
# function of something other than what gets run. No declared value contains whitespace today,
# which is what makes this the cheap moment to fix it rather than the expensive one.
config_hash() { # config_hash <config> <repo-root>
    local a=() l
    while IFS= read -r l; do a+=("$l"); done < <(docker_args "$1" "$2")
    args_hash ${a[@]+"${a[@]}"}
}

docker_args() { # docker_args <config> <repo-root>  -> one argument per line
    local cfg="$1" root="$2" line stripped
    stripped="$(dc_strip "$cfg")"

    printf '%s\n' "--name" "$NAME" "--detach" "--workdir" "$CTR_REPOS"

    local user; user="$(jq -r '.remoteUser // empty' <<<"$stripped")"
    [ -n "$user" ] && printf '%s\n' "--user" "$user"

    while IFS= read -r line; do
        [ -n "$line" ] || continue
        printf '%s\n' "$(dc_subst "$line" "$root")"
    done < <(jq -r '(.runArgs // [])[]' <<<"$stripped")

    while IFS= read -r line; do
        [ -n "$line" ] || continue
        printf '%s\n' "--mount" "$(dc_subst "$line" "$root")"
    done < <(dc_mount_specs "$cfg")

    while IFS= read -r line; do
        [ -n "$line" ] || continue
        printf '%s\n' "--env" "$(dc_subst "$line" "$root")"
    done < <(jq -r '(.containerEnv // {}) | to_entries[] | "\(.key)=\(.value)"' <<<"$stripped")
}

# ---------------------------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------------------------
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
    HOST_REPOS="$HOME/repos" CTR_REPOS=/home/vscode/repos

    echo "==> run.sh self-test: derivation from the real container.json"
    args="$(docker_args "$CONFIG" "$repo")"
    # `-e`, because every pattern here starts with a dash and grep would read it as a flag.
    yes_no() { if grep -q -e "$1" <<<"$args"; then printf 'yes'; else printf 'no'; fi; }
    eq "runs as the non-root user (bubblewrap cannot make namespaces as root)" "$(yes_no '^--user$')" "yes"
    eq "carries the seccomp profile"        "$(yes_no '^--security-opt$')" "yes"
    eq "carries NET_ADMIN for the firewall" "$(yes_no '^--cap-add=NET_ADMIN$')" "yes"
    eq "mounts ~/repos"                     "$(yes_no "target=$CTR_REPOS,")" "yes"
    eq "no variable survives into the command line" "$(yes_no '\${local')" "no"
    # PINNED AGAINST AN EMPTY DERIVATION, the same way check-config.sh pins its derived lists.
    # Every assertion above is satisfied by FINDING a string, so a derivation that produced nothing
    # would fail them rather than pass — but the mount count is the one that could quietly shrink,
    # and it is the security boundary, so it is compared against the file rather than to a number.
    declared="$(dc_mount_specs "$CONFIG" | grep -c . || true)"
    eq "every declared mount reaches the command line" \
       "$(grep -cxF -- '--mount' <<<"$args" || true)" "$declared"
    eq "...and there is at least one to reach it" "$([ "$declared" -gt 0 ] && echo yes || echo no)" "yes"

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

BUILD=0 DRY=0 OPEN=0 open_path=""
while [ $# -gt 0 ]; do
    case "$1" in
        --build)         BUILD=1; shift ;;
        --dry-run)       DRY=1; shift ;;
        --open)          OPEN=1; shift; case "${1:-}" in -*|"") ;; *) open_path="$1"; shift ;; esac ;;
        --consumed-keys) consumed_keys; exit 0 ;;
        --stop)          docker stop "$NAME" >/dev/null 2>&1 && echo "stopped $NAME" || echo "$NAME was not running"; exit 0 ;;
        --rm)            docker rm -f "$NAME" >/dev/null 2>&1 && echo "removed $NAME" || echo "$NAME did not exist"; exit 0 ;;
        *)               die "unknown argument '$1' (see the header of $0)" ;;
    esac
done

command -v jq >/dev/null 2>&1 || die "jq is required to read $CONFIG"
[ -f "$CONFIG" ] || die "no $CONFIG"

ctr_repo="$(container_path "$repo")" || die "this checkout ($repo) is not under $HOST_REPOS,
  and the container mounts $HOST_REPOS — so it would not be able to see its own setup.sh.
  Move the checkout under $HOST_REPOS. (Which folders you may OPEN is a different question, and
  the answer is any of them: you attach to the container and open any path inside it.)"

# Read with a plain loop, not `mapfile`: macOS ships bash 3.2, which does not have it, and this
# script's whole point is to be the way a Mac gets a container.
ARGS=()
while IFS= read -r line; do ARGS+=("$line"); done < <(docker_args "$CONFIG" "$repo")

# Hashed BEFORE the label is appended, or the value would have to contain itself.
want_hash="$(args_hash "${ARGS[@]}")"
ARGS+=(--label "jkb.args-hash=$want_hash")

if [ "$DRY" -eq 1 ]; then
    printf 'docker run'
    printf ' %q' "${ARGS[@]}" "$IMAGE" sleep infinity
    printf '\n'
    exit 0
fi

command -v docker >/dev/null 2>&1 || die "docker is not on PATH"
docker info >/dev/null 2>&1 || die "the docker daemon is not reachable"

if [ "$BUILD" -eq 1 ] || ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    say "build $IMAGE"
    docker build -t "$IMAGE" "$here"
fi

state="$(docker inspect -f '{{.State.Status}}' "$NAME" 2>/dev/null || true)"
fresh=0
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
                *)               reason="$NAME was created from a different container.json" ;;
            esac
            die "$reason.
  \`docker start\` reuses the configuration a container was BUILT with, so starting it would give
  you the old mounts and flags while every check here read the new declaration.

  Recreate it (the image and the volumes — cargo cache, Claude state — all survive):
      $0 --rm && $0"
        fi
        if [ "$state" = running ]; then say "container $NAME is already running"
        else say "start $NAME"; docker start "$NAME" >/dev/null; fi
        ;;
    *)
        say "create $NAME"
        # `sleep infinity` because nothing else keeps it alive: the image's job is to be a place to
        # attach to, not to run a program.
        docker run "${ARGS[@]}" "$IMAGE" sleep infinity >/dev/null
        fresh=1
        ;;
esac

# ORDER, and it is the same rule Dev Containers' lifecycle comment used to state: the firewall
# FIRST, before anything else runs or reaches the network — otherwise setup.sh's toolchain
# download happens with unrestricted egress. It is raised on every start, not just at create,
# because iptables rules live in the container's network namespace and do not survive a restart.
say "egress firewall"
docker exec "$NAME" sudo -n /usr/local/bin/init-firewall.sh

if [ "$fresh" -eq 1 ]; then
    say "first-run setup (this is the slow one — toolchain, jkb, extensions)"
    docker exec -w "$ctr_repo" "$NAME" bash .container/setup.sh
else
    # setup.sh ends by running verify.sh, so this is the arm that would otherwise never check.
    say "verify"
    docker exec -w "$ctr_repo" "$NAME" bash .container/verify.sh
fi

# THE CONTAINER IS WHERE DEFERRAL IS NORMAL: a session cannot archive its own checkout, so every
# `jkb task land` in here records one for later, and the host's `com.jkb.reap` cannot finish it
# (the record names /home/vscode/... paths the host cannot see). Best-effort: a reaper that could
# not run must never stop the firewall from having been raised.
docker exec -w "$ctr_repo" "$NAME" bash -lc 'jkb task reap || true' || true

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

if [ "$OPEN" -eq 1 ]; then
    [ -n "$open_path" ] || open_path="$ctr_repo"
    command -v code >/dev/null 2>&1 || die "the 'code' CLI is not on PATH (VS Code: 'Shell Command: Install code in PATH')"
    # Attached containers are addressed by a hex-encoded JSON authority. This spelling is VS Code's
    # and is not something this repo can verify from a test, so it is a convenience on top of the
    # Command Palette route above rather than the documented way in: if it stops working, the
    # instructions printed above still do.
    hex="$(printf '{"containerName":"/%s"}' "$NAME" | od -A n -t x1 | tr -d ' \n')"
    say "opening $open_path"
    code --folder-uri "vscode-remote://attached-container+$hex$open_path"
fi
