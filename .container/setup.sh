#!/usr/bin/env bash
# First-run setup for the jkb container (design D49). Runs as `vscode`, once per container, from
# run.sh — which is what starts the container now that this is not a Dev Containers config. It is
# also safe to run by hand inside an attached window.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

# FIRST, before anything else runs or reaches the network — otherwise the whole of this script,
# including a toolchain download, runs with unrestricted egress. run.sh also raises it before
# calling this, and again on every later start, because iptables rules live in the container's
# network namespace and do not survive a restart. Raised here as well rather than relied upon:
# this script is runnable by hand, and a firewall that is only raised by one caller is a boundary
# that depends on which caller you used.
say "egress firewall"
sudo -n /usr/local/bin/init-firewall.sh

# Claude Code writes sessions, history and auto-memory under ~/.claude. That lives in a named
# volume so it survives a rebuild without putting anything on the host. NOTHING under ~/.claude is
# mounted from the host — not even the credential file — which is why you authenticate inside the
# container, and therefore why the credential and account-state files must be linked out here too.
# Without them, container.json's promise that a login survives a rebuild is false: they would
# sit in the container's writable layer and go with it.
say "claude state"
# shellcheck source=/dev/null
. "$repo/.container/lib.sh"
dc_link_state /home/vscode

# ...and auto-memory, which the volume alone does NOT solve. Claude Code keys memory by the
# project's absolute path, so this container's `/home/vscode/repos/jkb` is a different key from the
# host's `/Users/.../repos/jkb` and the two sides would never see each other's. The shared store is
# `~/.jkb/claude-memory/<repo>/` — inside the bind mount that already exists, so no mount is added
# and the `~/.claude` prohibition is untouched. See the script's header for why the obvious bind is
# refused. A repo the linker DECLINES to link — a name that exists on both sides, a store holding
# something that is not a plain file — is a state it recognises and reports, and `verify.sh` passes
# on those: they want a person, not a rebuild. What is not tolerated is the linker failing to run
# at all, which `verify.sh` fails on, so this must not hide a non-zero exit behind a comment
# claiming the opposite. It reports and continues; verify.sh decides.
say "shared claude memory"
# The state each repo was LEFT in is recorded here, and `verify.sh` reads it as an extra alarm
# beside the live answer it asks for itself. The record earns its place because `link_one` repairs:
# it removes a live link into a poisoned store, so a live question asked afterwards sees the
# harmless `unsafe` and not the `exposed` that was true at create. It is not a substitute for
# asking — that made the store guard unfirable after create — and verify.sh consumes an `exposed`
# record once it has reported it, so the remedy it prints can clear it.
"$repo/scripts/link-claude-memory.sh" --status-file /home/vscode/.claude-state/memory-status \
    || echo "  (some repos need attention — see above)" >&2

say "auto-mode posture"
# The same posture file the host uses. It ends by running `check`, so a merge that did not take
# fails here rather than at the first unattended command.
"$repo/scripts/auto-mode.sh" install

say "rust toolchain (pinned by rust-toolchain.toml)"
( cd "$repo" && rustup show >/dev/null && cargo --version )

# The `jkb` binary has to exist INSIDE the container: the VS Code explorer extension spawns it,
# and so does every workflow verb (`task work`, `staging ls`). Without this the container looks
# fine and the tooling fails at first use. Slow on first create; the cargo registry volume makes
# a rebuild cheap. Deliberately fatal rather than `|| true` — a swallowed failure here is a
# container that looks ready and is not.
# CARGO_TARGET_DIR is set in container.json to a volume, off the bind-mounted workspace: a bind
# mount carries the HOST's uids, so where the host uid is not 1000 the container user cannot
# create target/ at all. Assert it rather than discover it three minutes into a build.
[ -w "${CARGO_TARGET_DIR:-/home/vscode/.cargo/target}" ] || {
    echo "CARGO_TARGET_DIR (${CARGO_TARGET_DIR:-unset}) is not writable — check the volume mount" >&2
    exit 1
}

say "install jkb into the container"
( cd "$repo" && cargo install --path crates/jkb-cli --locked --force )
command -v jkb >/dev/null || { echo "jkb is not on PATH after install" >&2; exit 1; }
jkb --version || true

# The .vsix files were staged into the image by fetch-extensions.sh, because a connect-time
# download is refused by the firewall this script raised in its first act. Installing from disk
# needs no network at all.
#
# SKIPPED, not failed, when there is no VS Code server: `run.sh` before anything attaches,
# a plain `docker run` and
# mutate-verify.sh all produce a perfectly good container with no VS Code in it, and extensions
# are meaningless there. Said out loud rather than passed over silently — verify.sh applies the
# same condition to its own assertion, so the two cannot disagree about whether this ran.
say "vs code extensions"
code_server="$(ls -d "$HOME"/.vscode-server/bin/*/bin/code-server 2>/dev/null | head -1 || true)"
if [ -z "$code_server" ]; then
    echo "  no VS Code server in this container — skipping (nothing to install extensions into)"
else
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

    # ...and the one this repo BUILDS. The jkb explorer is not on the marketplace, so it is not in
    # the list above and fetch-extensions.sh cannot stage it — which is why the side panel was
    # missing from every container until this line existed. Built from the workspace rather than
    # baked into the image, so it matches the checkout you are actually working in. It needs
    # registry.npmjs.org (pnpm, and vsce via `pnpm dlx`), which the posture allowlists, so it
    # works behind the firewall this script raised in its first act.
    #
    # scripts/install-extension.sh is the HOST's installer, reused unchanged: one builder and one
    # installer, or the container ships a different extension from the host for reasons nobody
    # decided. It resolves code-server itself.
    #
    # Fatal, like the `jkb` install above and for the same reason — the binary and the panel are
    # the two things that make this a jkb container rather than a generic one, and a swallowed
    # failure is a container that looks ready and is not. verify.sh asserts the result.
    if [ -f "$repo/ui/vscode/package.json" ]; then
        say "jkb explorer extension (built from ui/vscode)"
        # --build-in, because ui/node_modules is in the bind mount and is NOT portable: esbuild's
        # native binary differs per platform and pnpm links only the current one, so building here
        # would break the HOST's `./scripts/check.sh`, which runs `pnpm run build` with no install
        # in front of it. See the flag's comment in that script.
        "$repo/scripts/install-extension.sh" --build-in "$HOME/.jkb-ui-build"
    fi
fi

say "verify the container is what it claims to be"
"$repo/.container/verify.sh"
