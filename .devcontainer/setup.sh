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
sudo -n /usr/local/bin/init-firewall.sh "$repo/scripts/auto-mode-posture.json"

# Claude Code writes sessions, history and auto-memory under ~/.claude. That lives in a named
# volume so it survives a rebuild without putting anything on the host; the credential file is the
# one thing bind-mounted in, so the symlink has to go around it rather than over it.
say "claude state"
mkdir -p /home/vscode/.claude-state
for d in projects sessions history file-history shell-snapshots todos statsig; do
    mkdir -p "/home/vscode/.claude-state/$d"
    ln -sfn "/home/vscode/.claude-state/$d" "/home/vscode/.claude/$d" 2>/dev/null || true
done

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
