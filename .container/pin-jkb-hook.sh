#!/usr/bin/env bash
# Pin the `jkb` the harness hooks run (design D52.9, review finding on managed-settings.json).
#
# The managed hooks run OUTSIDE the sandbox, as vscode, with the container credential readable — so
# the binary they run is the credential's holder. `~/.cargo/bin/jkb` is writable from inside the
# sandbox (measured: a sandboxed `touch ~/.cargo/bin/x` succeeds), so a hook that found `jkb` on PATH
# ran whatever a tool call last put there. This copies the one setup just built to a root-owned path
# the managed hooks name absolutely, and nothing in the sandbox can replace it: a sandboxed command
# runs with no_new_privs (measured: `NoNewPrivs: 1`, and `sudo -n` exits 1), so it cannot run this.
#
# Run as root through sudoers, with no arguments (`sudo -n /usr/local/bin/pin-jkb-hook.sh` — the
# sudoers entry's `""` means none may be passed, and an empty string is one), by .container/setup.sh
# after it builds jkb — and again by you after rebuilding jkb in the container.
#
# The residual, stated: this pins what `~/.cargo/bin/jkb` is when it runs. setup builds it from the
# checkout you started the container on; a binary a tool call planted before you re-run this is what
# gets pinned. Re-run it only after a `cargo install` you ran yourself.
set -euo pipefail
src=/home/vscode/.cargo/bin/jkb
dest_dir=/usr/local/lib/jkb-hook
[ "$(id -u)" = 0 ] || { echo "pin-jkb-hook: must run as root (sudo -n $0)" >&2; exit 1; }
[ "$#" -eq 0 ] || { echo "pin-jkb-hook: takes no arguments; its source is fixed" >&2; exit 1; }
install -d -o root -g root -m 0755 "$dest_dir"
# READ AS vscode, WRITTEN AS root. The source is in a directory the sandbox can write, so checking it
# and then copying it as root is a race: swapped for a symlink in between, root would copy whatever it
# named — a root-only file — to a world-readable path. Read with vscode's own permissions, the worst a
# swap can do is pin a file vscode could already read. The destination is root's alone.
tmp="$(mktemp "$dest_dir/.jkb.XXXXXX")"
trap 'rm -f "$tmp"' EXIT
# Bounded: a FIFO planted at the source path would otherwise hold setup open for ever.
timeout 30 runuser -u vscode -- cat -- "$src" > "$tmp" \
    || { echo "pin-jkb-hook: could not read $src as vscode within 30s" >&2; exit 1; }
[ -s "$tmp" ] || { echo "pin-jkb-hook: $src is empty" >&2; exit 1; }
chown root:root "$tmp"
chmod 0755 "$tmp"
mv -f "$tmp" "$dest_dir/jkb"
trap - EXIT
"$dest_dir/jkb" --version
