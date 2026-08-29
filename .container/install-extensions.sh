#!/usr/bin/env bash
# Install this container's VS Code extensions. RUNS INSIDE THE CONTAINER.
#
#   ./.container/install-extensions.sh
#
# WHY IT IS ITS OWN SCRIPT, AND WHEN YOU RUN IT BY HAND. VS Code installs its server into the
# container when you ATTACH — which is after `run.sh` has finished, because attaching is something
# you do to a container that is already up. Under Dev Containers the order was the other way round
# (measured: server unpacked at 44s, symlinked at 51s, postCreate at 52s), so `setup.sh` always
# found a server and this was never a separate step.
#
# So on a fresh container `setup.sh` reports that it skipped this, correctly — there was nothing to
# install into yet. Attach, then run this from a terminal in the attached window.
#
# `run.sh` cannot do it for you: it drives Docker from the HOST, and the container deliberately has
# no Docker in it.
#
# Idempotent — `--force` reinstalls and the explorer rebuilds — so running it again is safe and is
# what you do after changing `ui/vscode` or the pinned extension list.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
# shellcheck source=/dev/null
. "$here/lib.sh"

say "vs code extensions"
code_server="$(ls -d "$HOME"/.vscode-server/bin/*/bin/code-server 2>/dev/null | head -1 || true)"
if [ -z "$code_server" ]; then
    # NOT an error when called from setup.sh, which is the create path and legitimately runs before
    # anything has attached — but it IS the whole point when called by hand, so it says which case
    # you are in rather than printing one word for both.
    echo "  no VS Code server in this container yet."
    echo "  That is expected during first-run setup: VS Code installs its server when you ATTACH."
    echo "  Attach (Command Palette -> 'Dev Containers: Attach to Running Container' -> jkb-dev),"
    echo "  then run this again from a terminal in that window."
    exit "${JKB_EXT_SKIP_RC:-0}"
fi

while read -r ext; do
    [ -n "$ext" ] || continue
    split="$(dc_extension_split "$ext")" || {
        echo "  '$ext' is not version-pinned in container.json — see check-config.sh" >&2
        exit 1
    }
    id="${split%%$'\t'*}"; version="${split##*$'\t'}"
    vsix="$HOME/.vsix/$id-$version.vsix"
    # A missing .vsix means the image predates this entry. Rebuild rather than reach for the
    # network: the download is exactly what cannot work from in here.
    [ -f "$vsix" ] || {
        echo "  $id@$version was not staged into this image — rebuild the container" >&2
        exit 1
    }
    # Same --server-data-dir VS Code itself passes, so this installs where the running server
    # will look rather than into a second default location.
    "$code_server" --server-data-dir "$HOME/.vscode-server" \
                   --install-extension "$vsix" --force >/dev/null
    echo "  installed $id@$version from disk"
done <<<"$(dc_extensions "$repo/.container/container.json")"

# ...and the one this repo BUILDS. The jkb explorer is not on the marketplace, so it is not in the
# list above and fetch-extensions.sh cannot stage it — which is why the side panel was missing from
# every container until this existed. Built from the workspace rather than baked into the image, so
# it matches the checkout you are actually working in. It needs registry.npmjs.org (pnpm, and vsce
# via `pnpm dlx`), which the posture allowlists, so it works behind the egress firewall.
#
# scripts/install-extension.sh is the HOST's installer, reused unchanged: one builder and one
# installer, or the container ships a different extension from the host for reasons nobody decided.
# It resolves code-server itself.
if [ -f "$repo/ui/vscode/package.json" ]; then
    say "jkb explorer extension (built from ui/vscode)"
    # --build-in, because ui/node_modules is in the bind mount and is NOT portable: esbuild's
    # native binary differs per platform and pnpm links only the current one, so building here
    # would break the HOST's `./scripts/check.sh`, which runs `pnpm run build` with no install in
    # front of it. See the flag's comment in that script.
    "$repo/scripts/install-extension.sh" --build-in "$HOME/.jkb-ui-build"
fi

say "done — reload the window ('Developer: Reload Window') to activate them"
