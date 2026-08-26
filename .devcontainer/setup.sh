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
# The state it FOUND is recorded before it repairs anything, because the repair downgrades its own
# alarm: `link_one` removes a live link into a poisoned store, so `verify.sh` — which runs 25 lines
# later — would ask afterwards and see the harmless `unsafe` rather than the `exposed` it must fail
# on. Written where verify.sh reads it, and only ever appended to by this line.
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
