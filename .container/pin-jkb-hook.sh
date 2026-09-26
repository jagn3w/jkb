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
# Run as root through sudoers, with no arguments (`sudo -n pin-jkb-hook.sh ""`), by .container/setup.sh
# after it builds jkb — and again by you after rebuilding jkb in the container.
#
# The residual, stated: this pins what `~/.cargo/bin/jkb` is when it runs. setup builds it from the
# checkout you started the container on; a binary a tool call planted before you re-run this is what
# gets pinned. Re-run it only after a `cargo install` you ran yourself.
set -euo pipefail
src=/home/vscode/.cargo/bin/jkb
dest_dir=/usr/local/lib/jkb-hook
[ "$(id -u)" = 0 ] || { echo "pin-jkb-hook: must run as root (sudo -n $0 \"\")" >&2; exit 1; }
[ -f "$src" ] && [ ! -L "$src" ] || { echo "pin-jkb-hook: $src is missing or a symlink" >&2; exit 1; }
install -d -o root -g root -m 0755 "$dest_dir"
install -o root -g root -m 0755 "$src" "$dest_dir/jkb.new"
mv -f "$dest_dir/jkb.new" "$dest_dir/jkb"
"$dest_dir/jkb" --version
