#!/usr/bin/env bash
# Tests for the Claude Code hooks under .claude/hooks/.
#
# These are shell, not Rust, so they are not reached by `cargo test` — `scripts/check.sh` and CI
# run this file directly. They are portable: `notify-sticky.sh` takes its notifier and its state
# directory from the environment, so the macOS-only half is exercised through a recorder stub and
# the suite passes on the Linux CI runner too.
set -uo pipefail

hook="$(cd "$(dirname "$0")/.." && pwd)/.claude/hooks/notify-sticky.sh"
[ -x "$hook" ] || { echo "missing or non-executable: $hook" >&2; exit 1; }

# Checked, not assumed: an unusable temp dir must stop the suite, never let it fall back to
# relative paths in the checkout. `rm -rf "$tmp"` with an empty $tmp is the other half of that.
tmp=$(mktemp -d 2>/dev/null) || tmp=""
if [ -z "$tmp" ] || [ ! -d "$tmp" ]; then
  echo "cannot create a temp directory — refusing to run tests from the working tree" >&2
  exit 1
fi
trap '[ -n "$tmp" ] && [ -d "$tmp" ] && rm -rf "$tmp"' EXIT

# A stand-in for jkb-notifier that records the argv of each call, one argument per line with a
# blank line between calls. Asserting on argv is the point: what this hook does is choose a
# command line. `NOTIFIER_FAILS` makes it refuse, which is how the real one reports "not installed
# or not yet authorized" — the state the osascript fallback exists for.
rec="$tmp/calls"
cat > "$tmp/notifier" <<'STUB'
#!/usr/bin/env bash
for a in "$@"; do printf '%s\n' "$a" >> "$REC"; done
printf '\n' >> "$REC"
exit "${NOTIFIER_FAILS:-0}"
STUB
chmod +x "$tmp/notifier"

failures=0
ok() { printf '  ok  %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1" >&2; failures=$((failures + 1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else fail "$1: expected [$3], got [$2]"; fi; }

# Run the hook on one event payload. Every case starts from an empty recorder, so an assertion
# about "did not call the notifier" is about this call and not a leftover.
run() {
  : > "$rec"
  JKB_NOTIFIER="$tmp/notifier" JKB_NOTIFY_STATE="$tmp/state" REC="$rec" \
    bash "$hook" <<<"$1"
}
calls() { tr '\n' ' ' < "$rec" | sed 's/  */ /g; s/^ //; s/ $//'; }

# payload <event> <session> <message> [tool_name]
payload() { printf '{"hook_event_name":"%s","session_id":"%s","message":"%s","tool_name":"%s","cwd":"/repos/jkb/.jkb/work/wt"}' "$1" "$2" "$3" "${4:-}"; }
PROMPT='Claude needs your permission to use Bash'

echo "==> notify-sticky.sh (the shim)"

# What is left here is what is genuinely SHELL. The rules — post, withdraw, which tool, the
# sweep — moved into `jkb notify` when they became a checkable table, and their assertions moved
# with them into crates/jkb-cli/src/notify/tests.rs. Testing them in both places would be two
# descriptions of one rule, which is the drift this whole change exists to end.
#
# The shim's own contract is not covered there, and it is the part that can wedge a session.

hook_run() { # hook_run <payload> <PATH> -> exit status, stdout captured in $hook_out
  hook_out=$(printf '%s' "$1" | PATH="$2" JKB_NOTIFY_STATE="$tmp/state" bash "$hook" 2>/dev/null)
  return $?
}

# A `jkb` that rejects the subcommand — an older binary that predates it, which clap exits 2 for.
mkdir -p "$tmp/oldjkb"
printf '#!/bin/sh\nexit 2\n' > "$tmp/oldjkb/jkb"
chmod +x "$tmp/oldjkb/jkb"

# A `jkb` that records how it was called.
mkdir -p "$tmp/goodjkb"
cat > "$tmp/goodjkb/jkb" <<STUB
#!/bin/sh
printf '%s ' "\$@" >> "$tmp/jkbcalls"
printf '\n' >> "$tmp/jkbcalls"
# Printed ONLY when the shim actually exported it. It used to print `notifier=none` otherwise,
# and the assertion just grepped for a line starting `notifier=` — so deleting the shim's export
# left the test green, which is the one thing it exists to catch.
[ -n "\${JKB_NOTIFIER:-}" ] && printf 'notifier=%s\n' "\$JKB_NOTIFIER" >> "$tmp/jkbcalls"
[ -n "\${JKB_HOOK_OWNER:-}" ] && printf 'owner=%s\n' "\$JKB_HOOK_OWNER" >> "$tmp/jkbcalls"
exit 0
STUB
chmod +x "$tmp/goodjkb/jkb"

PAYLOAD='{"hook_event_name":"PostToolUse","session_id":"s1","tool_name":"Bash"}'

# 1. THE CONTRACT THAT CAN WEDGE A SESSION. A PostToolUse hook exiting non-zero BLOCKS the tool
#    call. The shim first used `exec`, which makes jkb's status the hook's, and an older jkb
#    exiting 2 stopped the session dead — observed live, not hypothesised.
hook_run "$PAYLOAD" "$tmp/oldjkb:/usr/bin:/bin"
check "a jkb that rejects the subcommand still exits 0" "$?" "0"
# HOME and CARGO_HOME too: the shim falls back to `${CARGO_HOME:-$HOME/.cargo}/bin/jkb`, so
# overriding PATH alone found the developer's real binary and this branch was never taken.
hook_out=$(printf '%s' "$PAYLOAD" | PATH="/usr/bin:/bin" HOME="$tmp/nohome" \
  CARGO_HOME="$tmp/nohome/.cargo" JKB_NOTIFY_STATE="$tmp/state" bash "$hook" 2>/dev/null)
check "no jkb at all still exits 0" "$?" "0"
check "and it is really absent" \
  "$(PATH=/usr/bin:/bin HOME="$tmp/nohome" CARGO_HOME="$tmp/nohome/.cargo" \
     command -v jkb >/dev/null 2>&1 && echo found || echo absent)" "absent"
hook_run "$PAYLOAD" "$tmp/goodjkb:/usr/bin:/bin"
check "a working jkb still exits 0" "$?" "0"

# 2. Silent on stdout: a hook's stdout lands in the transcript, and on UserPromptSubmit it is
#    injected into the conversation as context.
for ev in Notification PostToolUse UserPromptSubmit Stop SessionEnd; do
  hook_run "$(payload "$ev" s1 'm')" "$tmp/goodjkb:/usr/bin:/bin"
  check "$ev is silent on stdout" "$hook_out" ""
done

# 3. It delegates, and hands over the notifier path rather than making jkb look it up — the
#    location stays spelled once, here, because setup.sh and build-notifier.sh already ask for it
#    and both run where jkb may not be built yet.
: > "$tmp/jkbcalls"
hook_run "$PAYLOAD" "$tmp/goodjkb:/usr/bin:/bin"
check "the shim delegates to jkb notify hook" \
  "$(head -1 "$tmp/jkbcalls" | sed 's/ *$//')" "notify hook"
check "and passes the notifier path it resolved" \
  "$(sed -n 's/^notifier=//p' "$tmp/jkbcalls" | head -1)" \
  "$(bash "$hook" --notifier-path)"

# The owner is the session's, and jkb CANNOT ask for it — its own parent is this shim, which
# exits milliseconds later. Recording that made every record read as provably dead, so the next
# session's sweep withdrew a live session's pending prompt.
check "and passes an owner that is not the shim itself" \
  "$(sed -n 's/^owner=//p' "$tmp/jkbcalls" | head -1 | grep -cE '^[0-9]+$' | tr -d ' ')" "1"

# 4. Malformed input still exits 0 and says nothing.
hook_run 'not json' "$tmp/goodjkb:/usr/bin:/bin"
check "malformed input exits 0" "$?" "0"
check "malformed input is silent" "$hook_out" ""

# 12b. The bundle search itself. Every test above injects `JKB_NOTIFIER`, so without this the
#      hard-coded install path is covered only by the opt-in live test — and a typo there would
#      leave the hook silently falling back to plain banners on every machine while the suite
#      stayed green. A fake HOME is enough: what is under test is the path, not the binary.
fake_home="$tmp/fakehome"
mkdir -p "$fake_home/Applications/jkb Notifier.app/Contents/MacOS"
cp "$tmp/notifier" "$fake_home/Applications/jkb Notifier.app/Contents/MacOS/jkb-notifier"
found=$(env -u JKB_NOTIFIER HOME="$fake_home" bash "$hook" --find-notifier </dev/null 2>/dev/null)
check "the notifier is found at its install path" \
  "$found" "$fake_home/Applications/jkb Notifier.app/Contents/MacOS/jkb-notifier"

# 12c. `--notifier-path` is the single place the install location is written down, and
#      `scripts/build-notifier.sh` builds to it. The two used to compose the path independently;
#      renaming the bundle in either would have left the hook falling back to plain banners, which
#      is indistinguishable from the feature not existing. So: the path the installer targets must
#      be one the hook actually searches, and the plist must name the same executable.
want=$(env -u JKB_NOTIFIER bash "$hook" --notifier-path </dev/null 2>/dev/null)
check "--notifier-path answers even with nothing installed" \
  "$([ -n "$want" ] && echo yes || echo no)" "yes"

# The decomposition build-notifier.sh performs, asserted here rather than trusted there.
check "the install path is one find_notifier searches" \
  "$(env -u JKB_NOTIFIER HOME="$tmp/pathcheck" bash "$hook" --notifier-path </dev/null 2>/dev/null)" \
  "$tmp/pathcheck/Applications/jkb Notifier.app/Contents/MacOS/jkb-notifier"
# Asked of build-notifier.sh, which owns the rule, rather than re-read here — and its --check arm
# runs before its own macOS gate and reads the plist with awk, so this assertion is live on Linux
# CI too. It used to call /usr/libexec/PlistBuddy directly, which does not exist on ubuntu-latest:
# the `|| echo MISSING` arm turned "cannot read" into a wrong VALUE and reddened CI on every push,
# in a suite whose header claims it is portable.
"$(cd "$(dirname "$0")/.." && pwd)/scripts/build-notifier.sh" --check --quiet >/dev/null 2>&1
check "the plist declares the executable the install path names" "$?" "0"

# 12d. The hook's events and `.claude/settings.json`'s registrations, diffed BOTH ways. The hook
#      can act only on events Claude Code is told to send it, and that registration lives in a file
#      the hook cannot see — so an event handled but unregistered does nothing, and one registered
#      but unhandled spawns a process per occurrence for no reason. Neither errors. Losing `Stop`
#      is the sharpest case: a *denied* permission produces no `PostToolUse`, so its notification
#      would sit on screen until the session ended, which is the case `Stop` exists for.
settings="$(cd "$(dirname "$0")/.." && pwd)/.claude/settings.json"
handled=$(env -u JKB_NOTIFIER bash "$hook" --events </dev/null 2>/dev/null | sort)
registered=$(jq -r --arg h "notify-sticky.sh" '
  .hooks | to_entries[]
  | .key as $event
  | .value[]?.hooks[]?
  | select(.command // "" | contains($h))
  | $event' "$settings" 2>/dev/null | sort -u)
check "every event the hook handles is registered in settings.json" \
  "$(comm -23 <(printf '%s\n' "$handled") <(printf '%s\n' "$registered") | tr '\n' ' ' | sed 's/ *$//')" ""
# The names also appear in the Rust dispatcher, which the shim cannot see. `jkb notify events`
# prints what it answers to, so all THREE spellings are diffed rather than two — renaming the
# `"SessionStart"` literal in `hook()` disabled the sweep permanently with every check green.
if [ -x "$(cd "$(dirname "$0")/.." && pwd)/target/debug/jkb" ]; then
  rust_events=$("$(cd "$(dirname "$0")/.." && pwd)/target/debug/jkb" notify events 2>/dev/null | sort)
  check "the hook's events and jkb's agree" \
    "$(comm -3 <(printf '%s\n' "$handled") <(printf '%s\n' "$rust_events") | tr -d '[:space:]')" ""
else
  printf '  --  %s\n' "jkb events cross-check (target/debug/jkb not built)"
fi

check "every event registered in settings.json is handled by the hook" \
  "$(comm -13 <(printf '%s\n' "$handled") <(printf '%s\n' "$registered") | tr '\n' ' ' | sed 's/ *$//')" ""

# 13. The live round-trip against the real notifier bundle. Withdrawing a delivered notification
#     is the one behaviour that justifies shipping our own notifier at all, and no stub can show
#     it works — only the notification centre can. Opt-in, mirroring how this repo treats its
#     other live smokes (the ollama and Chrome tests are #[ignore]d for the same reason): it puts
#     a real notification on screen, and a gate run before every commit must not flash one.
echo "==> live notifier round-trip"
live_bin=$(bash "$hook" --find-notifier </dev/null 2>/dev/null || true)
if [ "${JKB_HOOK_LIVE_TEST:-0}" != "1" ]; then
  printf '  --  %s\n' "skipped (set JKB_HOOK_LIVE_TEST=1 to run; posts a real notification)"
elif [ -z "$live_bin" ]; then
  fail "live round-trip: no notifier installed — run scripts/build-notifier.sh"
elif ! "$live_bin" status 2>/dev/null | grep -q "authorization=authorized"; then
  fail "live round-trip: notifier is not authorized — run: '$live_bin' authorize"
else
  # Driven through the REAL hook with the payloads Claude Code actually sends — nothing stubbed,
  # no injected notifier. Testing `jkb-notifier` on its own would leave the seam that matters
  # (hook -> notifier -> notification centre) uncovered, and that seam is the whole feature.
  live_sid="hook-selftest-$$"
  live_id="jkb-claude-$live_sid"
  delivered() { "$live_bin" list 2>/dev/null | grep -c "^$live_id\$" | tr -d ' '; }
  # Driven against THIS CHECKOUT's jkb, not whatever is installed. The shim delegates to the
  # first `jkb` on PATH, and an installed binary predating `notify` silently does nothing — which
  # is precisely what this test saw the first time it ran against the shim.
  repo_target="$(cd "$(dirname "$0")/.." && pwd)/target/debug"
  if [ ! -x "$repo_target/jkb" ]; then
    fail "live round-trip: $repo_target/jkb is not built — run ./scripts/build.sh"
    live_bin=""
  fi
  live() { printf '%s' "$1" | PATH="$repo_target:$PATH" bash "$hook"; }

  printf '  --  %s\n' "style: $("$live_bin" status 2>/dev/null)"

  # Claude needs permission.
  live "$(payload Notification "$live_sid" "$PROMPT")"
  check "the Notification hook posts a real notification" "$(delivered)" "1"

  # A DIFFERENT tool finishing first — the batched-call case — must leave it up.
  live "$(payload PostToolUse "$live_sid" '' Read)"
  check "a concurrent tool leaves the real notification up" "$(delivered)" "1"

  # You grant it; the tool runs; PostToolUse fires for THAT tool. The pair the change exists for.
  live "$(payload PostToolUse "$live_sid" '' Bash)"
  check "granting permission withdraws it" "$(delivered)" "0"

  # The other half: the idle-waiting notification, cleared by typing rather than by a tool.
  live "$(payload Notification "$live_sid" 'Claude is waiting for your input')"
  check "the idle notification posts" "$(delivered)" "1"
  live "$(payload UserPromptSubmit "$live_sid" '')"
  check "typing a prompt withdraws it" "$(delivered)" "0"

  # NOTE: `list` reports DELIVERED notifications, which includes one that has hidden its banner
  # and is resting in Notification Center. So nothing above proves the notification stayed
  # VISIBLE — no API exposes that. Stickiness is checked the only way it can be, by reading the
  # style back, and confirmed the only way it can be, by looking at the screen.
  case "$("$live_bin" status 2>/dev/null)" in
    *alert-style=alert*) ok "the alert style is sticky" ;;
    *) printf '  --  %s\n' "alert style is not 'alert' — banners will hide; set System Settings > Notifications > jkb Notifier > Alerts" ;;
  esac
fi

echo "==> post-merge (setup.sh rebuild trigger)"

# CANARY. These fixtures run a real git hook, and a hook that escapes its temp dir does not fail
# loudly — it silently rewrites the repository it escaped into. That happened once (see pm_run),
# so the suite records the repo's own state before and after and refuses to call itself green if
# anything moved. This detects an escape whatever its cause, rather than trusting the guards in
# pm_run to be exhaustive, and it is what makes those guards testable at all.
repo_root_for_canary="$(cd "$(dirname "$0")/.." && pwd)"
canary_before=$(git -C "$repo_root_for_canary" status --porcelain 2>/dev/null; \
                git -C "$repo_root_for_canary" rev-parse HEAD 2>/dev/null)

# post-merge is RUN here, not pattern-matched. These assertions used to re-implement its `grep`
# against setup.sh's answer and never execute the hook — so changing post-merge's own matching
# (say `grep -qE` to `grep -q`) would have made every pull silently skip setup.sh with all eleven
# of them still green. A test that reimplements the thing it tests measures the copy.
#
# Each case is a throwaway git repo with a stub setup.sh that records whether it was invoked.
# PATH deliberately excludes ~/.cargo/bin so `command -v jkb` fails and the hook's second step
# (`jkb task close-merged`) cannot touch the real knowledge base from a test.
pm_src="$(cd "$(dirname "$0")/.." && pwd)/scripts/hooks/post-merge"

pm_run() { # pm_run <changed-paths, space-separated> <skip-paths-answer> -> "ran" | "skipped"
  local d ans f
  ans=$2

  # THE TEMP DIR IS VALIDATED BEFORE ANYTHING IS WRITTEN, because `cd ""` SUCCEEDS in bash. When
  # `mktemp -d` failed (a sandbox denying $TMPDIR is enough), `d` was empty, `cd "$d" || exit` did
  # not fire, and this whole fixture ran in the REAL REPOSITORY: it `git init`-ed over the
  # worktree, committed, overwrote tracked sources with `x`, and executed the real post-merge —
  # which found the real scripts/setup.sh and ran it, reaching `cargo install` and `pnpm install`.
  # That is observed behaviour, not a hypothetical. Everything below is written so that no single
  # failure can reproduce it.
  d=$(mktemp -d 2>/dev/null) || d=""
  if [ -z "$d" ] || [ ! -d "$d" ]; then
    echo "fixture-error: could not create a temp dir" >&2
    echo fixture-error
    return
  fi
  : > "$d/.pm-fixture"          # sentinel: proves cwd is the fixture, not the repo

  mkdir -p "$d/scripts"
  cat > "$d/scripts/setup.sh" <<STUB
#!/bin/sh
[ "\$1" = "--skip-paths" ] && { printf '%s\\n' '$ans'; exit 0; }
echo ran > "$d/invoked"
STUB
  chmod +x "$d/scripts/setup.sh"

  # Every path is ABSOLUTE and every git call takes -C, so the destructive half does not depend on
  # the working directory at all — the second layer, independent of the check above.
  git -C "$d" init -q . && git -C "$d" config user.email t@t && git -C "$d" config user.name t
  echo seed > "$d/seed.txt"
  git -C "$d" add -A && git -C "$d" commit -qm one
  git -C "$d" rev-parse HEAD > "$d/.git/ORIG_HEAD"
  # An empty <changed-path> leaves ORIG_HEAD at HEAD, so `git diff` yields nothing — which is
  # also what an unresolvable range produces, since line 22 of the hook swallows the failure.
  if [ -n "$1" ]; then
    for f in $1; do
      mkdir -p "$d/$(dirname "$f")" && echo x > "$d/$f"
    done
    git -C "$d" add -A && git -C "$d" commit -qm two
  fi

  # The hook itself must run WITH the fixture as cwd (it calls `git rev-parse --show-toplevel`),
  # so this is the one place cwd matters. The sentinel is checked after the cd rather than
  # comparing $PWD to $d, which does not hold on macOS where /var is a symlink to /private/var.
  #
  # Invoked the way git does — through the shebang — NOT with `sh`. Ubuntu's /bin/sh is dash,
  # which rejects the hook's own `set -uo pipefail` and exits before doing anything.
  (
    cd "$d" || exit 1
    [ -f .pm-fixture ] || exit 1
    PATH=/usr/bin:/bin "$pm_src" >/dev/null 2>&1
  )

  [ -f "$d/invoked" ] && echo ran || echo skipped
  [ -n "$d" ] && [ -d "$d" ] && rm -rf "$d"
}

skip_paths=$("$(cd "$(dirname "$0")/.." && pwd)/scripts/setup.sh" --skip-paths 2>/dev/null)
check "setup.sh answers --skip-paths" "$([ -n "$skip_paths" ] && echo yes || echo no)" "yes"

# Everything that feeds a setup.sh step must rebuild. `.claude/` is here because
# build-notifier.sh takes its install path from .claude/hooks/notify-sticky.sh, which the previous
# inclusion-list version did not match — the second silent drift of the same kind.
for path in macos/notifier/main.swift .claude/hooks/notify-sticky.sh .claude/settings.json \
            crates/jkb-cli/src/main.rs ui/core/src/summary.ts scripts/build-notifier.sh \
            Cargo.toml; do
  check "a pull touching $path rebuilds" "$(pm_run "$path" "$skip_paths")" "ran"
done

# Only things that provably cannot change what any step installs may be skipped.
for path in openspec/changes/x/design.md README.md CLAUDE.md .codereviews/x/tasks.md; do
  check "a pull touching $path does not rebuild" "$(pm_run "$path" "$skip_paths")" "skipped"
done

# MIXED pulls, which are the common case — nearly every branch touches a top-level .md as well as
# code, and this one touches CLAUDE.md alongside crates/, macos/ and .claude/. Every case above
# changes exactly one path, so on its own the suite cannot tell "rebuild if ANY changed path is
# outside the skip list" from "rebuild only if ALL are": inverting that quantifier leaves all of
# them green while every real pull silently skips the rebuild.
check "a pull touching docs AND code rebuilds" \
  "$(pm_run "CLAUDE.md crates/jkb-cli/src/main.rs" "$skip_paths")" "ran"
check "a pull touching docs AND the notifier rebuilds" \
  "$(pm_run "README.md macos/notifier/main.swift" "$skip_paths")" "ran"
check "a pull touching only several doc paths still skips" \
  "$(pm_run "README.md CLAUDE.md openspec/changes/x/design.md" "$skip_paths")" "skipped"

# An unanswerable trigger degrades toward doing the work: a skipped rebuild leaves a stale
# artifact and says nothing, an unnecessary one only costs time.
check "an unanswerable --skip-paths rebuilds anyway" "$(pm_run README.md "")" "ran"

# A pattern grep CANNOT COMPILE is a third state, and as a bare `if` condition it was
# indistinguishable from "everything matched" — so the hook printed grep's error and then took the
# skip arm, the one direction its own comment forbids. The status is now read explicitly, and
# anything that is not a clean "all matched" rebuilds.
check "a skip pattern grep cannot compile rebuilds anyway" \
  "$(pm_run crates/f.rs '^(unbalanced')" "ran"

# The other unknown, and the one that used to degrade the wrong way: `git diff` produces an empty
# list both when nothing was pulled and when it could not resolve the range at all, and the hook
# cannot tell those apart — so it must not read either as "nothing to do".
check "an unreadable diff rebuilds anyway" "$(pm_run "" "$skip_paths")" "ran"

# ...and it must not rely on today's skip pattern happening to reject an empty line. `printf
# '%s\n' ""` emits one blank line, which no current alternative matches, so the inverted `grep -qv`
# rebuilds by luck as much as by design. An over-broad pattern removes that luck, and the explicit
# empty-`changed` guard is what still rebuilds — this is the case that makes it load-bearing
# rather than decorative.
check "an unreadable diff rebuilds even under an over-broad skip pattern" \
  "$(pm_run "" ".*")" "ran"

canary_after=$(git -C "$repo_root_for_canary" status --porcelain 2>/dev/null; \
               git -C "$repo_root_for_canary" rev-parse HEAD 2>/dev/null)
check "the post-merge fixtures did not touch the real repository" \
  "$([ "$canary_before" = "$canary_after" ] && echo intact || echo CHANGED)" "intact"

if [ "$failures" -ne 0 ]; then
  printf '\n%d hook test(s) failed\n' "$failures" >&2
  exit 1
fi
echo "All hook tests passed."
