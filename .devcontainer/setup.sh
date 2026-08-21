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

say "verify the container is what it claims to be"
"$repo/.devcontainer/verify.sh"
