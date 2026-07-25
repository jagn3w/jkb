#!/usr/bin/env bash
# Build, package, and install the jkb VS Code extension (ui/vscode) into VS Code.
# Repeatable: run it after pulling to refresh the installed extension. Uses pnpm only.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
ui_dir="$repo_root/ui"

# --- locate pnpm (installed via the standalone installer; not always on a bare PATH) ---
if ! command -v pnpm >/dev/null 2>&1; then
  for d in "$HOME/Library/pnpm" "$HOME/.local/share/pnpm"; do
    for candidate in "$d/bin/pnpm" "$d/pnpm"; do
      if [ -x "$candidate" ]; then
        export PNPM_HOME="$d"
        export PATH="$(dirname "$candidate"):$PATH"
        break 2
      fi
    done
  done
fi
command -v pnpm >/dev/null 2>&1 || {
  echo "error: pnpm not found. Install it (pnpm only, never npm): https://pnpm.io/installation" >&2
  exit 1
}

# --- locate the VS Code CLI ---
code_bin="$(command -v code || true)"
if [ -z "$code_bin" ]; then
  for c in \
    "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code" \
    "/Applications/VSCodium.app/Contents/Resources/app/bin/codium" \
    "/Applications/Cursor.app/Contents/Resources/app/bin/code"; do
    if [ -x "$c" ]; then code_bin="$c"; break; fi
  done
fi
[ -n "$code_bin" ] || {
  echo "error: VS Code 'code' CLI not found. In VS Code run: 'Shell Command: Install code in PATH'." >&2
  exit 1
}

echo "==> pnpm install + build (ui/)"
(cd "$ui_dir" && pnpm install --silent && pnpm run build)

echo "==> package .vsix"
vscode_dir="$ui_dir/vscode"
(cd "$vscode_dir" && pnpm dlx @vscode/vsce package --no-dependencies --allow-missing-repository >/dev/null)
vsix="$(ls -t "$vscode_dir"/*.vsix | head -1)"
echo "    $vsix"

echo "==> install into VS Code"
"$code_bin" --install-extension "$vsix" --force

if ! command -v jkb >/dev/null 2>&1; then
  echo "note: the extension calls 'jkb', which is not on your PATH."
  echo "      install it with: cargo install --path crates/jkb-cli"
fi
echo "Done. Reload VS Code ('Developer: Reload Window') to activate the latest build."
