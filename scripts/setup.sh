#!/usr/bin/env bash
# One-shot setup for jkb on a fresh machine (personal or work).
#
# Idempotent — safe to re-run after `git pull` to refresh everything. It:
#   1. installs the `jkb` binary to ~/.cargo/bin (cargo install)
#   2. scaffolds the standard KB namespace roots (repos/ tasks/ media/ references/ memory/)
#   3. builds + installs the VS Code extension (pnpm; skipped if VS Code/pnpm absent)
#   4. installs + activates the background services (file-sync watcher and worktree
#      reaper) as OS services (launchd/systemd)
#   5. installs the repo's post-merge git hook into this repo's .git/hooks — and, when
#      core.hooksPath is set globally (which replaces .git/hooks), a chainer there too
#
# Flags: --no-extension, --no-service, --no-scaffold, --link-memory, --db <path>, -h/--help.
#
# --link-memory is opt-in, and deliberately not the default: it writes symlinks under
# ~/.claude/projects so the dev container and the host share one auto-memory store, and this
# script is re-run by the post-merge hook. A `git pull` must not quietly rearrange somebody's
# ~/.claude. See scripts/link-claude-memory.sh.
# Everything is best-effort per step: a missing optional tool warns and continues.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
# `install_exec` — every file this script installs is written atomically, because the
# post-merge hook runs this script and is itself one of them. See scripts/lib.sh.
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"
do_extension=1
do_service=1
do_scaffold=1
link_memory=0
# One state word per section, rendered at the end by `render_setup_summary` in lib.sh. Each
# means what actually happened, not what was attempted: `watcher=running` is set to `failed`
# by every arm that reports a failed or absent activation, not just by a failed write, and
# `scaffold` distinguishes "skipped by flag" from "an existing KB was left untouched" from
# "creating them failed" — a boolean could not, and the summary asserted five roots in two of
# the three.
scaffold_state=created
extension_state=installed
watcher_state=running
db="${JKB_DB:-$HOME/.jkb/jkb.db}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --no-extension) do_extension=0 ;;
    --no-service) do_service=0 ;;
    --no-scaffold) do_scaffold=0 ;;
    --link-memory) link_memory=1 ;;
    --db) shift; db="$1" ;;
    -h|--help)
      # Derived, not a pinned line range: the header grows, and a stale `2,18p` silently
      # truncates the help or prints a line of code as documentation. Both happened here.
      sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown flag: $1 (see --help)" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
# `warn` comes from lib.sh, which renders the git-hook report and needs it too.

# --- 1. jkb binary -----------------------------------------------------------
# Re-running this after `git pull` is the supported way to refresh the binary — the
# jkb.db is global across branches/worktrees, so a newer branch's migration can lock
# an older binary out; `--force` always reinstalls from the current checkout so you
# never run a stale binary against a migrated DB.
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
# ONLY on a fresh machine: if a KB already exists we leave it completely untouched
# (never mutate an existing knowledge base on setup — it may hold real data on a
# shared/cloud-synced path). The DB + migrations are created on first open; `_sys/`
# comes from the migrations, these are the reserved semantic roots (design D32).
if [ "$do_scaffold" -eq 0 ]; then
  scaffold_state=skipped
  warn "skipping KB scaffold (--no-scaffold)"
elif [ -f "$db" ]; then
  scaffold_state=untouched
  say "existing KB detected ($db) — left untouched"
else
  say "scaffold KB namespaces ($db)"
  # Wrapped for the same reason as the two steps below it. THE RULE, stated where it can be
  # checked against the file: everything after the binary is non-fatal, because the git-hooks
  # section is what installs the hook that re-runs this script after a later pull — a machine
  # that dies before it never installs the hook, and no `git pull` will then repair that. The
  # binary itself is the one hard precondition and is deliberately still fatal: every step
  # after it needs `jkb`, so skipping the hook section is the accepted cost there, and a
  # machine whose `cargo install` fails has nothing working to repair anyway. On the
  # unattended pull path the hook is already installed from an earlier run, so it survives.
  if mkdir -p "$(dirname "$db")" && jkb --db "$db" ns mk repos tasks media references memory; then :; else
    scaffold_state=failed
    warn "could not scaffold the KB at $db — continuing to the git hooks."
  fi
fi

# --- 3. VS Code extension ----------------------------------------------------
if [ "$do_extension" -eq 1 ]; then
  say "build + install VS Code extension"
  if "$repo_root/scripts/install-extension.sh"; then :; else
    extension_state=failed
    warn "extension install skipped/failed (VS Code or pnpm missing?) — continuing."
  fi
else
  extension_state=skipped
  warn "skipping VS Code extension (--no-extension)"
fi

# --- 4. file-sync watcher service -------------------------------------------
if [ "$do_service" -eq 1 ]; then
  say "install + activate background services (file sync, worktree reaper)"
  # Wrapped, like the extension step above it. A bare statement under `set -euo pipefail`
  # ends the script here — so a unit that could not be written (an uncreatable
  # ~/.config/systemd/user, a full disk, no HOME in the post-merge hook's environment) took
  # the git-hook section below with it, and the repo kept whatever stale or missing
  # post-merge hook it had. The hooks are the one section a partial setup must still reach.
  if jkb --db "$db" service install; then
  # BOTH units `service install` writes. The reaper is what finishes a landing whose session
  # could not archive its own worktree, so a unit that is written and never loaded means those
  # worktrees accumulate for ever — visible only as `jkb doctor` output nobody reads.
  case "$(uname -s)" in
    Darwin)
      for label in com.jkb.sync com.jkb.reap; do
        plist="$HOME/Library/LaunchAgents/$label.plist"
        launchctl unload "$plist" 2>/dev/null || true   # idempotent reload
        if launchctl load "$plist"; then echo "$label loaded (launchd)"; else
          warn "could not load $label; activate manually: launchctl load $plist"
          watcher_state=failed
        fi
      done ;;
    Linux)
      if command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload || true
        for label in com.jkb.sync com.jkb.reap; do
          if systemctl --user enable --now "$label"; then echo "$label enabled (systemd)"; else
            warn "could not enable $label; activate manually: systemctl --user enable --now $label"
            watcher_state=failed
          fi
        done
      else
        warn "systemctl not found; activate the printed units manually."
        watcher_state=failed
      fi ;;
    # Reachable only after `jkb service install` succeeded, and that refuses any platform but
    # macOS and Linux — so "unsupported OS" was the wrong diagnosis for the one state that
    # gets here: `uname` said something the two arms above did not recognise.
    *) warn "unrecognised platform '$(uname -s)'; the units were written — activate them manually."
       watcher_state=failed ;;
  esac
  else
    # A distinct variable, not `do_service=0`: that is the flag, and reusing it would make the
    # summary below report a failure as "--no-service" — the user's choice, which it was not.
    watcher_state=failed
    warn "could not write the service units — continuing to the git hooks."
  fi
else
  watcher_state=skipped
  warn "skipping watcher service (--no-service)"
fi

# --- git hooks -----------------------------------------------------------------------
# Install the repo-local post-merge hook (design D34.5): re-run setup.sh and close
# branch-completed tasks after a pull.
#
# The wrinkle: `core.hooksPath` set globally REPLACES .git/hooks entirely, so a repo-local
# hook is silently dead when one is configured. That is the same failure class as a build
# that "passes" without type-checking, so we detect it and install a chainer into the global
# directory rather than quietly doing nothing.
say "installing git hooks"
hooks_src="$repo_root/scripts/hooks/post-merge"
if [ -f "$hooks_src" ]; then
  # BOTH halves live in lib.sh — the installer and its rendering. Leaving the rendering
  # inline drew the seam one level too low: nothing runs setup.sh, so its `case` arms were
  # reachable from no test, and two review findings lived in them with the gate green.
  #
  # `< <(…)`, not a pipe and not `|| true`: a process substitution's exit status is never
  # checked, so a failing installer cannot kill this script, and nothing here is load-bearing
  # for the report arriving. `install_git_hooks` is itself `set -e`-safe (see lib.sh's header)
  # — it used to depend on an incidental `|| true` right here for that.
  render_git_hooks_report < <(install_git_hooks "$repo_root" "$hooks_src")
else
  # A header followed by nothing reads exactly like a stage that ran — the vacuity this file
  # and check.sh have both grown guards against. This is the one stage a partial setup must
  # reach, so it may not be the one that disappears quietly.
  warn "no hook source at $hooks_src — the post-merge hook was NOT installed."
fi

# --- shared claude memory (opt-in) -------------------------------------------
# Only when asked. This writes under ~/.claude/projects, and setup.sh is what the post-merge hook
# re-runs after a `git pull` — jkb does not rearrange other people's configuration behind them.
if [ "$link_memory" -eq 1 ]; then
  "$repo_root/scripts/link-claude-memory.sh" || warn "some repos could not be linked (see above)"
fi

say "setup complete"
# The report, rendered by lib.sh so the arms are reachable from a test. Inline, this block
# produced a finding in three consecutive review rounds and every one of them was invisible to
# a green gate. `< <(…)`, not a pipe: a process substitution's status is never checked, so
# nothing here can fail the script at its last statement.
render_setup_summary < <(
  printf 'jkb=%s\n' "$(command -v jkb)"
  printf 'database=%s\n' "$db"
  printf 'scaffold=%s %s\n' "$scaffold_state" "$db"
  printf 'extension=%s\n' "$extension_state"
  printf 'watcher=%s\n' "$watcher_state"
)
