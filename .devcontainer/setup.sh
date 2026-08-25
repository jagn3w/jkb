#!/usr/bin/env bash
# postCreate for the jkb dev container (design D49). Runs as `vscode`, once per container build.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

# FIRST, before anything else runs or reaches the network. The Dev Containers lifecycle is
# postCreate -> postStart, so leaving this to postStartCommand alone would run the whole of this
# script — including a toolchain download — with unrestricted egress. postStart raises it again
# on every later start, because iptables rules do not survive a container restart.
say "egress firewall"
sudo -n /usr/local/bin/init-firewall.sh

# Claude Code writes sessions, history and auto-memory under ~/.claude. That lives in a named
# volume so it survives a rebuild without putting anything on the host. NOTHING under ~/.claude is
# mounted from the host — not even the credential file — which is why you authenticate inside the
# container, and therefore why the credential and account-state files must be linked out here too.
# Without them, devcontainer.json's promise that a login survives a rebuild is false: they would
# sit in the container's writable layer and go with it.
say "claude state"
# shellcheck source=/dev/null
. "$repo/.devcontainer/lib.sh"
dc_link_state /home/vscode

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
# CARGO_TARGET_DIR is set in devcontainer.json to a volume, off the bind-mounted workspace: a bind
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

say "verify the container is what it claims to be"
"$repo/.devcontainer/verify.sh"
