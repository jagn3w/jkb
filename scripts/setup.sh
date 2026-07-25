#!/usr/bin/env bash
# One-shot setup for jkb on a fresh machine (personal or work).
#
# Idempotent — safe to re-run after `git pull` to refresh everything. It:
#   1. installs the `jkb` binary to ~/.cargo/bin (cargo install)
#   2. scaffolds the standard KB namespace roots (repos/ tasks/ media/ references/ memory/)
#   3. builds + installs the VS Code extension (pnpm; skipped if VS Code/pnpm absent)
#   4. installs + activates the file-sync watcher as an OS service (launchd/systemd)
#
# Flags: --no-extension, --no-service, --no-scaffold, --db <path>, -h/--help.
# Everything is best-effort per step: a missing optional tool warns and continues.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
do_extension=1
do_service=1
do_scaffold=1
db="${JKB_DB:-$HOME/.jkb/jkb.db}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --no-extension) do_extension=0 ;;
    --no-service) do_service=0 ;;
    --no-scaffold) do_scaffold=0 ;;
    --db) shift; db="$1" ;;
    -h|--help)
      sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown flag: $1 (see --help)" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }

# --- 1. jkb binary -----------------------------------------------------------
say "install jkb binary"
# shellcheck disable=SC1090
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found. Install Rust via https://rustup.rs then re-run." >&2
  exit 1
fi
# --force so a re-run always refreshes from the current checkout; --locked for reproducibility.
(cd "$repo_root" && cargo install --path crates/jkb-cli --locked --force)

# The binary lands in $CARGO_HOME/bin; make sure that's reachable for the rest of this run.
cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
export PATH="$cargo_bin:$PATH"
if ! command -v jkb >/dev/null 2>&1; then
  echo "error: jkb not on PATH after install (expected in $cargo_bin)." >&2
  exit 1
fi
echo "installed: $(command -v jkb) ($(jkb --version))"

# --- 2. scaffold the KB ------------------------------------------------------
# The DB and its migrations are created on first open. `_sys/` is created by the
# migrations; these are the reserved semantic roots (design D32).
if [ "$do_scaffold" -eq 1 ]; then
  say "scaffold KB namespaces ($db)"
  mkdir -p "$(dirname "$db")"
  jkb --db "$db" ns mk repos tasks media references memory
else
  warn "skipping KB scaffold (--no-scaffold)"
fi

# --- 3. VS Code extension ----------------------------------------------------
if [ "$do_extension" -eq 1 ]; then
  say "build + install VS Code extension"
  if "$repo_root/scripts/install-extension.sh"; then :; else
    warn "extension install skipped/failed (VS Code or pnpm missing?) — continuing."
  fi
else
  warn "skipping VS Code extension (--no-extension)"
fi

# --- 4. file-sync watcher service -------------------------------------------
if [ "$do_service" -eq 1 ]; then
  say "install + activate file-sync watcher"
  jkb --db "$db" service install
  case "$(uname -s)" in
    Darwin)
      plist="$HOME/Library/LaunchAgents/com.jkb.sync.plist"
      launchctl unload "$plist" 2>/dev/null || true   # idempotent reload
      if launchctl load "$plist"; then echo "watcher loaded (launchd)"; else
        warn "could not load the launchd agent; activate manually: launchctl load $plist"
      fi ;;
    Linux)
      if command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload || true
        if systemctl --user enable --now com.jkb.sync; then echo "watcher enabled (systemd)"; else
          warn "could not enable the systemd unit; activate manually: systemctl --user enable --now com.jkb.sync"
        fi
      else
        warn "systemctl not found; activate the printed unit manually."
      fi ;;
    *) warn "unsupported OS for auto-activation; the unit was written — activate it manually." ;;
  esac
else
  warn "skipping watcher service (--no-service)"
fi

say "setup complete"
echo "  • jkb:        $(command -v jkb)"
echo "  • database:   $db"
echo "  • roots:      repos/ tasks/ media/ references/ memory/ (+ _sys/)"
[ "$do_extension" -eq 1 ] && echo "  • extension:  reload VS Code ('Developer: Reload Window') to activate"
[ "$do_service" -eq 1 ] && echo "  • watcher:    running; file edits under mounts auto-sync"
