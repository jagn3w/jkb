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
# The checkout whose extension is built: run.sh's setup names it in JKB_REPO_ROOT, because this
# runs from the kit mirror (lib.sh's DC_KIT_DIR); run by hand from a checkout, it is that one.
repo="${JKB_REPO_ROOT:-$(cd "$here/.." && pwd)}"
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
    # Plain 0. There was a JKB_EXT_SKIP_RC knob here so a caller could make this state fatal, and
    # nothing anywhere set it — so the by-hand-versus-setup.sh distinction the text above describes
    # existed only in the text, while every caller got the same exit code regardless. A knob with no
    # user is not a seam, it is a claim the code does not honour; the two cases differ in what they
    # PRINT, which is where the difference actually is.
    exit 0
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
done <<<"$(dc_extensions "$here/container.json")"

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
    # FROM THE CHECKOUT, deliberately, and the one place this script runs checkout code: it BUILDS
    # ui/ from the checkout (pnpm runs its package scripts), so the builder is part of what is
    # built. First run only, from setup.sh; .container/README.md records it with cargo install.
    "$repo/scripts/install-extension.sh" --build-in "$HOME/.jkb-ui-build"
fi

# The server's Machine settings, merged — not replaced — with vscode-machine-settings.json. Here
# rather than in setup.sh for the reason this whole script exists: the file belongs to the server,
# and the server arrives on ATTACH. Why these settings: dc_machine_settings_path in lib.sh.
say "vs code machine settings"
settings="$(dc_machine_settings_path)"
if ! merged="$(dc_merge_machine_settings "$settings" "$here/vscode-machine-settings.json")"; then
    # Refused, not overwritten: VS Code accepts comments in that file and jq does not, so a file
    # we cannot parse is most likely one a person edited, and rewriting it would lose their edits.
    echo "  $settings is not plain JSON (comments?) — left untouched." >&2
    echo "  Merge $here/vscode-machine-settings.json into it by hand (Preferences: Open Remote Settings)." >&2
    exit 1
fi
mkdir -p "$(dirname "$settings")"
# Same directory, so the rename is atomic and the server's watcher never reads half a file.
tmp="$(mktemp "$settings.XXXXXX")"
printf '%s\n' "$merged" > "$tmp"
mv "$tmp" "$settings"
echo "  merged $(jq -r 'keys | join(", ")' "$here/vscode-machine-settings.json") into $settings"

say "done — reload the window ('Developer: Reload Window') to activate them"
