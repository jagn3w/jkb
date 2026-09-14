#!/usr/bin/env bash
# One-shot setup for jkb on a fresh machine (personal or work).
#
# Idempotent — safe to re-run after `git pull` to refresh everything. It:
#   1. installs the `jkb` binary to ~/.cargo/bin (cargo install)
#   2. scaffolds the standard KB namespace roots (repos/ tasks/ media/ references/ memory/)
#   3. builds + installs the VS Code extension (pnpm; skipped if VS Code/pnpm absent)
#   4. installs + activates the background services (file-sync watcher, worktree reaper, and
#      the jkb serve daemon) as OS services (launchd/systemd)
#   5. installs the repo's post-merge git hook into this repo's .git/hooks — and, when
#      core.hooksPath is set globally (which replaces .git/hooks), a chainer there too
#   6. builds + installs the notifier behind sticky Claude Code notifications, and reports
#      the two things it cannot do for you: the one-time Allow, and the Alerts style
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
serve_state=unchecked
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
  say "install + activate background services (file sync, worktree reaper, jkb serve)"
  # Wrapped, like the extension step above it. A bare statement under `set -euo pipefail`
  # ends the script here — so a unit that could not be written (an uncreatable
  # ~/.config/systemd/user, a full disk, no HOME in the post-merge hook's environment) took
  # the git-hook section below with it, and the repo kept whatever stale or missing
  # post-merge hook it had. The hooks are the one section a partial setup must still reach.
  # The activation lives in lib.sh (`activate_services`) so a test can run it against stub
  # service managers — see scripts/tests/services.test.sh.
  if jkb --db "$db" service install; then
    activate_services "$db"
  else
    # A distinct variable, not `do_service=0`: that is the flag, and reusing it would make the
    # summary below report a failure as "--no-service" — the user's choice, which it was not.
    watcher_state=failed
    serve_state=unchecked
    warn "could not write the service units — continuing to the git hooks."
  fi
else
  watcher_state=skipped
  serve_state=skipped
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

# --- notifications -------------------------------------------------------------------
# `.claude/hooks/notify-sticky.sh` makes Claude Code's "needs your permission" notification stay
# on screen and withdraws it when you answer. The hook tells `jkb serve`, whose notification machine
# sends posts and withdrawals on a queue topic (design r3.2 N1); on macOS `jkb-notifier serve`
# consumes it. Withdrawing needs a notifier we own (macos/notifier, on Apple's UserNotifications
# framework), so this builds it — no third-party binary, no download.
#
# The TOPIC is created on every platform: a producer never creates one, and the hook in a Linux dev
# container still reaches a Mac's daemon. Nothing fills it where nobody consumes — the machine sends
# only to a topic with a consumer group, and the group is the notifier's, so only a Mac has one.
say "notification topic"
if notify_topic="$(jkb notify topic 2>/dev/null)" && [ -n "$notify_topic" ]; then
  jkb --db "$db" mq topic create "$notify_topic" >/dev/null \
    && echo "  • $notify_topic ready" \
    || warn "could not create the $notify_topic topic — notifications will not be sent"
else
  warn "this jkb does not name the notification topic (jkb notify topic) — notifications will not be sent"
fi
#
# The two things this CANNOT do for you are reported rather than assumed, because a hook that
# posts nothing, or posts self-hiding banners, looks exactly like a broken hook:
#   - authorization is a one-time user grant, and the prompt dies with the process that raised it,
#     so it must be requested interactively rather than in passing here;
#   - the sticky "Alerts" style is a per-app setting no API can set.
if [ "$(uname -s)" = "Darwin" ]; then
  say "sticky notifications"
  # "Did this build succeed" and "what notifier is installed, in what state" are separate
  # questions, and only the second is worth reporting. They used to be fused: a failed build
  # asserted that notifications would auto-hide — untrue whenever a working bundle is already
  # installed, which is the usual case, since build-notifier.sh bails at its `swiftc` guard before
  # touching the existing one — and it suppressed the two manual-step instructions, so a machine
  # that really was unauthorized was told nothing.
  # --jkb and --db: the agent it installs (com.jkb.notifier) subscribes with this jkb, to this
  # database — the one every other service here was installed against.
  "$repo_root/scripts/build-notifier.sh" --jkb "$(command -v jkb)" --db "$db" \
    || warn "could not rebuild the notifier (any existing one is untouched)"

  # Asked of the hook itself, so this reports on exactly the binary the hook will use. A second
  # copy of the search list here would eventually disagree with it, and the disagreement reads
  # as a broken notifier rather than as the drift it is.
  nb=$(bash "$repo_root/.claude/hooks/notify-sticky.sh" --find-notifier 2>/dev/null || true)
  state=$([ -n "$nb" ] && "$nb" status 2>/dev/null || true)
  if [ -z "$nb" ]; then
    warn "no notifier installed — permission notifications will not be shown (nothing consumes the queue)"
  else
    case "$state" in
      *authorization=authorized*) ;;
      *) warn "not yet allowed to notify — run once and click Allow:"
         echo "      '$nb' authorize" ;;
    esac
    case "$state" in
      *alert-style=alert*) ;;
      *) echo "  • for STICKY notifications, set: System Settings > Notifications >"
         echo "    jkb Notifier > Alerts  (banners auto-hide; only Alerts waits for you)" ;;
    esac
  fi
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
  printf 'serve=%s\n' "$serve_state"
)
