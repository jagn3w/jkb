#!/usr/bin/env bash
# Refuse to build the container for a folder the mount cannot represent (design D49).
#
#   .devcontainer/check-workspace.sh <the folder VS Code opened>
#
# Run on the HOST, from devcontainer.json's `initializeCommand`, before anything is created.
#
# WHY IT EXISTS. `workspaceMount` binds all of `~/repos` at `/home/vscode/repos`, so a folder's
# place inside the container is decided by its place under `~/repos` — and only folders that HAVE
# a place under `~/repos` can be opened. A `jkb task work` session lives at
# `<repo>/.jkb/work/<session>`, which is inside the mount but is not `~/repos/<name>`; before the
# mount was widened, opening one bound that worktree directly and it simply worked.
#
# Without this check the failure is silent and is the worst available: `${localWorkspaceFolder}`
# still resolves (so the seccomp profile is found and the container starts), the mount still
# succeeds, and `workspaceFolder` lands the agent in a DIFFERENT checkout — for a session, the
# main one, on whatever branch it is holding. Keeping two checkouts apart is the entire purpose of
# a session (D36), and every existing guard passes while it happens: `check-config.sh` only reads
# the JSON, and `verify.sh`'s workspace assertion is satisfied by the wrong repo.
#
# So it fails here, loudly, on the host, having created nothing.
set -uo pipefail

folder="${1:-}"
if [ -z "$folder" ]; then
    echo "check-workspace.sh: no folder given (devcontainer.json must pass \${localWorkspaceFolder})" >&2
    exit 2
fi

# Resolve both sides before comparing: on macOS `~/repos` may be reached through a symlink, and a
# textual prefix test then refuses a folder that is genuinely in the right place.
resolve() { (cd "$1" 2>/dev/null && pwd -P) || printf '%s' "$1"; }
folder="$(resolve "$folder")"
repos="$(resolve "${JKB_REPOS_DIR:-$HOME/repos}")"

parent="$(dirname "$folder")"
name="$(basename "$folder")"

if [ "$parent" = "$repos" ] && [ -n "$name" ] && [ "$name" != "/" ]; then
    exit 0
fi

cat >&2 <<EOF
This dev container mounts all of ~/repos at /home/vscode/repos, so it can only open a folder that
is directly inside ~/repos. It cannot open:

    $folder

because that is not \$HOME/repos/<something>. Opening it anyway would start the container in a
different checkout than the one you are looking at, silently.

  • Working a jkb task session? Sessions are worked on the host, in their own VS Code window
    (\`jkb task work <uid>\`). The container is for the main checkout.
  • Repo kept somewhere else? Point the mount at it by setting JKB_REPOS_DIR, or move the
    checkout under ~/repos.
EOF
exit 1
