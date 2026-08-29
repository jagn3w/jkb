#!/usr/bin/env bash
# Build, package, and install the jkb VS Code extension (ui/vscode) into VS Code.
# Repeatable: run it after pulling to refresh the installed extension. Uses pnpm only.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
ui_dir="$repo_root/ui"

build_in=""
while [ $# -gt 0 ]; do
  case "$1" in
    --build-in) build_in="${2:?--build-in needs a directory}"; shift 2 ;;
    *) echo "error: unknown argument '$1'" >&2; exit 1 ;;
  esac
done

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
# In a dev container there is no `code` CLI at all — the remote server ships `code-server`, which
# has to be TOLD where the running server keeps its data or it installs into a second default
# location the editor never reads. Resolved here rather than in a container-side copy of this
# script: one builder, one installer, so the container cannot ship a different extension from the
# host. The same server binary and the same --server-data-dir VS Code itself passes.
code_args=()
if [ -z "$code_bin" ]; then
  code_bin="$(ls -d "$HOME"/.vscode-server/bin/*/bin/code-server 2>/dev/null | head -1 || true)"
  [ -n "$code_bin" ] && code_args=(--server-data-dir "$HOME/.vscode-server")
fi
[ -n "$code_bin" ] || {
  echo "error: no VS Code CLI found ('code' on PATH, or a remote code-server under" >&2
  echo "       ~/.vscode-server/bin/*/bin/). In VS Code run: 'Shell Command: Install code in PATH'." >&2
  exit 1
}

# --- optionally build somewhere other than the workspace ---
# `ui/node_modules` is NOT portable across platforms: esbuild ships a native binary per platform
# and pnpm links only the current one. The dev container bind-mounts ~/repos, so it shares that
# directory with the host — a build in the container would leave linux links there, and the host's
# ./scripts/check.sh runs `pnpm run build` with no `pnpm install` in front of it, so the next host
# gate would fail with an esbuild platform error and no obvious cause. One shared mutable
# directory, two writers with incompatible requirements.
#
# So the container passes --build-in and gets its own copy, outside the mount. node_modules is
# excluded from the copy for the same reason it is the problem.
if [ -n "$build_in" ]; then
  echo "==> copy ui/ to $build_in (the workspace copy is shared with the host)"
  rm -rf "$build_in"
  mkdir -p "$build_in"
  tar -c --exclude node_modules -C "$repo_root" ui | tar -x -C "$build_in"
  ui_dir="$build_in/ui"
fi

echo "==> pnpm install + build ($ui_dir)"
(cd "$ui_dir" && pnpm install --silent && pnpm run build)

echo "==> package .vsix"
vscode_dir="$ui_dir/vscode"
(cd "$vscode_dir" && pnpm dlx @vscode/vsce package --no-dependencies --allow-missing-repository >/dev/null)
vsix="$(ls -t "$vscode_dir"/*.vsix | head -1)"
echo "    $vsix"

echo "==> install into VS Code"
"$code_bin" ${code_args[@]+"${code_args[@]}"} --install-extension "$vsix" --force

if ! command -v jkb >/dev/null 2>&1; then
  echo "note: the extension calls 'jkb', which is not on your PATH."
  echo "      install it with: cargo install --path crates/jkb-cli"
fi
echo "Done. Reload VS Code ('Developer: Reload Window') to activate the latest build."
