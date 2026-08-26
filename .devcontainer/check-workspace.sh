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

# Exercised by ./scripts/check.sh. A guard nothing runs is a guard nobody has seen work, and this
# one is the only thing standing between the widened mount and a silent wrong-checkout open.
if [ "${1:-}" = --self-test ]; then
    t="$(mktemp -d)"; trap 'rm -rf "$t"' EXIT
    fails=0
    check() { # check <label> <got-rc> <want-rc>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else printf '  \033[31mFAIL\033[0m %s (rc %s, wanted %s)\n' "$1" "$2" "$3"; fails=$((fails+1)); fi
    }
    echo "==> check-workspace self-test"
    mkdir -p "$t/repos/jkb/.jkb/work/sess" "$t/repos/other" "$t/elsewhere/jkb"
    run_it() { HOME="$t" bash "$0" "$1" >/dev/null 2>&1; printf '%s' "$?"; }
    check "a repo directly under ~/repos is accepted"      "$(run_it "$t/repos/jkb")" 0
    check "so is any other repo there"                     "$(run_it "$t/repos/other")" 0
    check "a session worktree is refused"                  "$(run_it "$t/repos/jkb/.jkb/work/sess")" 1
    check "a subdirectory of a repo is refused"            "$(run_it "$t/repos/jkb/.jkb")" 1
    check "the repos directory itself is refused"          "$(run_it "$t/repos")" 1
    check "a checkout outside ~/repos is refused"          "$(run_it "$t/elsewhere/jkb")" 1
    check "a path that does not exist is refused"          "$(run_it "$t/repos/nope/deeper")" 1
    check "no argument is a usage error"                   "$(HOME="$t" bash "$0" >/dev/null 2>&1; printf '%s' "$?")" 2
    echo
    [ "$fails" -eq 0 ] || { printf '\033[31m%d failed\033[0m\n' "$fails"; exit 1; }
    printf '\033[32mcheck-workspace self-test passed\033[0m\n'
    exit 0
fi

folder="${1:-}"
if [ -z "$folder" ]; then
    echo "check-workspace.sh: no folder given (devcontainer.json must pass \${localWorkspaceFolder})" >&2
    exit 2
fi

# Resolve both sides before comparing: on macOS `~/repos` may be reached through a symlink, and a
# textual prefix test then refuses a folder that is genuinely in the right place.
# ONE statement of where repos live, and it is devcontainer.json's `source=${localEnv:HOME}/repos`.
# There used to be a `JKB_REPOS_DIR` override here, read by nothing else in the tree — so taking
# the refusal's advice switched this guard off without moving the mount, and the container then
# opened whatever sat at ~/repos/<name> instead. A remedy the machine does not accept is worse
# than no remedy.
# `pwd -P` resolves symlinks, which is why a symlinked checkout is refused rather than accepted:
# the container binds ~/repos itself, so a link pointing out of it dangles in there. That is the
# right answer, and the refusal below says so rather than advising a symlink — which it did, and
# which this same resolution then rejected.
resolve() { (cd "$1" 2>/dev/null && pwd -P) || printf '%s' "$1"; }
folder="$(resolve "$folder")"
repos="$(resolve "$HOME/repos")"

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
  • Repo kept somewhere else? MOVE the checkout under ~/repos. A symlink there does not work
    and is refused for the same reason: only ~/repos itself is bind-mounted, so the link would
    dangle inside the container even though it resolves fine out here.
EOF
exit 1
