#!/usr/bin/env bash
# Tests for the Claude Code hooks under .claude/hooks/.
#
# These are shell, not Rust, so they are not reached by `cargo test` — `scripts/check.sh` and CI
# run this file directly. They are portable: `notify-sticky.sh` takes its notifier and its state
# directory from the environment, so the macOS-only half is exercised through a recorder stub and
# the suite passes on the Linux CI runner too.
set -uo pipefail

hook="$(cd "$(dirname "$0")/../.." && pwd)/.claude/hooks/notify-sticky.sh"
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
# This stub is a direct child of the shim, so OUR parent IS the shim — the discriminator the
# owner assertion needs, and it is free.
printf 'shim=%s\n' "\$PPID" >> "$tmp/jkbcalls"
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

# 3. It delegates. It no longer hands over a notifier path: since r3.2 N1 `jkb notify hook` only
#    tells `jkb serve` what happened, and the notifier runs on its own as `jkb-notifier serve`.
: > "$tmp/jkbcalls"
hook_run "$PAYLOAD" "$tmp/goodjkb:/usr/bin:/bin"
check "the shim delegates to jkb notify hook" \
  "$(head -1 "$tmp/jkbcalls" | sed 's/ *$//')" "notify hook"

# The owner is the session's, and jkb CANNOT ask for it — its own parent is this shim, which
# exits milliseconds later. Recording that made every record read as provably dead, so the next
# session's sweep withdrew a live session's pending prompt.
# NOT merely "is a number" — that stayed green when the shim recorded `$$`, its own pid, which
# is the exact defect this guards: a process that exits milliseconds later makes every record
# read as provably dead, so the next session's sweep withdraws a live session's prompt.
owner_seen=$(sed -n 's/^owner=//p' "$tmp/jkbcalls" | head -1)
shim_seen=$(sed -n 's/^shim=//p' "$tmp/jkbcalls" | head -1)
check "and passes a numeric owner" "$(printf '%s' "$owner_seen" | grep -cE '^[0-9]+$' | tr -d ' ')" "1"
check "and it is NOT the shim itself" \
  "$([ -n "$owner_seen" ] && [ "$owner_seen" != "$shim_seen" ] && echo different || echo same)" \
  "different"

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
"$(cd "$(dirname "$0")/../.." && pwd)/scripts/build-notifier.sh" --check --quiet >/dev/null 2>&1
check "the plist declares the executable the install path names" "$?" "0"

# 12c'. The launchd agent build-notifier.sh installs, printed rather than installed — so it is
#      checked on the Linux runner too. It must run the binary at the install path (the bundle's, or
#      the notification centre refuses it), subscribe with the jkb it was given to the topic THAT jkb
#      names, and survive a database path that is not XML-safe.
mkdir -p "$tmp/topicjkb" "$tmp/oldtopicjkb"
printf '#!/bin/sh\n[ "$1 $2" = "notify topic" ] && echo claude/notify\n' > "$tmp/topicjkb/jkb"
printf '#!/bin/sh\nexit 2\n' > "$tmp/oldtopicjkb/jkb"
chmod +x "$tmp/topicjkb/jkb" "$tmp/oldtopicjkb/jkb"
builder="$(cd "$(dirname "$0")/../.." && pwd)/scripts/build-notifier.sh"
agent=$(env -u JKB_NOTIFIER HOME="$tmp/agenthome" "$builder" --print-agent --jkb "$tmp/topicjkb/jkb" \
  --db "$tmp/d b&<x>/jkb.db" 2>/dev/null)
check "the agent prints" "$?" "0"
check "the agent runs the bundle's binary" \
  "$(grep -c "<string>$tmp/agenthome/Applications/jkb Notifier.app/Contents/MacOS/jkb-notifier</string>" <<<"$agent" | tr -d ' ')" "1"
check "as serve, with the jkb, database and topic it was given" \
  "$(tr -d ' \n' <<<"$agent" | grep -c "<string>serve</string><string>--jkb</string><string>$tmp/topicjkb/jkb</string><string>--db</string><string>$tmp/db&amp;&lt;x&gt;/jkb.db</string><string>--topic</string><string>claude/notify</string>" | tr -d ' ')" "1"
check "and is kept alive" "$(grep -c '<key>KeepAlive</key>' <<<"$agent" | tr -d ' ')" "1"
HOME="$tmp/agenthome" "$builder" --print-agent --jkb "$tmp/oldtopicjkb/jkb" >/dev/null 2>&1
check "a jkb that cannot name the topic writes no agent" "$?" "1"
# launchd runs an agent from `/`: relative paths must be made absolute, or the notifier subscribes to
# a different database than the daemon sends on (stage-5 review).
rel_agent=$(cd "$tmp" && env -u JKB_NOTIFIER HOME="$tmp/agenthome" "$builder" --print-agent \
  --jkb ./topicjkb/jkb --db kb/jkb.db 2>/dev/null)
check "relative --jkb and --db are written absolute" \
  "$(grep -cE "<string>($tmp/topicjkb/jkb|$tmp/kb/jkb.db|$tmp/kb/notifier.log)</string>" <<<"$rel_agent" | tr -d ' ')" "4"

# 12d. The hook's events and `.claude/settings.json`'s registrations, diffed BOTH ways. The hook
#      can act only on events Claude Code is told to send it, and that registration lives in a file
#      the hook cannot see — so an event handled but unregistered does nothing, and one registered
#      but unhandled spawns a process per occurrence for no reason. Neither errors. Losing `Stop`
#      is the sharpest case: a *denied* permission produces no `PostToolUse`, so its notification
#      would sit on screen until the session ended, which is the case `Stop` exists for.
settings="$(cd "$(dirname "$0")/../.." && pwd)/.claude/settings.json"
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
# Under CARGO_TARGET_DIR when it is set — the dev container sets it — or this compared a stale binary.
built_jkb="${CARGO_TARGET_DIR:-$(cd "$(dirname "$0")/../.." && pwd)/target}/debug/jkb"
if [ -x "$built_jkb" ]; then
  rust_events=$("$built_jkb" notify events 2>/dev/null | sort)
  check "the hook's events and jkb's agree" \
    "$(comm -3 <(printf '%s\n' "$handled") <(printf '%s\n' "$rust_events") | tr -d '[:space:]')" ""
else
  printf '  --  %s\n' "jkb events cross-check ($built_jkb not built)"
fi

check "every event registered in settings.json is handled by the hook" \
  "$(comm -13 <(printf '%s\n' "$handled") <(printf '%s\n' "$registered") | tr '\n' ' ' | sed 's/ *$//')" ""

# 12e. The queue consumer's fold, stale marking and acks, and the fallback banner's quoting — the
#      parts of `jkb-notifier serve` that decide what reaches the screen. Compiled from source rather
#      than run from an installed bundle, which may be older than this checkout; `serve --dry-run`
#      touches no notification centre, so no bundle is needed. macOS only (swiftc, osacompile).
echo "==> jkb-notifier serve (dry run)"
if ! command -v swiftc >/dev/null 2>&1; then
  printf '  --  %s\n' "skipped (no swiftc — macOS with the Xcode Command Line Tools)"
elif ! swiftc -swift-version 5 -o "$tmp/jkb-notifier" \
    "$(cd "$(dirname "$0")/../.." && pwd)/macos/notifier/main.swift" 2>"$tmp/swiftc.err"; then
  fail "macos/notifier/main.swift does not compile: $(head -5 "$tmp/swiftc.err")"
else
  msg() { # msg <seq> <kind> <id> <enqueued_at> <expired>
    printf '{"event":"message","message":{"seq":%s,"key":"session/x","kind":"%s","payload":{"id":"%s","session":"x","title":"Claude Code","subtitle":"wt","body":"needs Bash"},"producer":"p","enqueued_at":%s,"expires_at":null,"expired":%s,"future_field":1}}\n' "$@"
  }
  served=$({
    msg 1 notify.post jkb-claude-a 1000 false
    msg 2 notify.withdraw jkb-claude-a 1000 false
    msg 3 notify.post jkb-claude-b 0 true
    msg 4 some.other_kind jkb-claude-z 0 false
    printf '{"event":"a_future_event"}\n'
    printf '{"event":"caught_up","seq":4}\n'
    msg 5 notify.post jkb-claude-c 50000000 false
    # No caught_up after it: the real subscriber sends nothing more until an unreadable row is acked,
    # so the consumer must end the batch here itself (the stage-5 review's deadlock).
    printf '{"event":"unreadable","seq":6,"reason":"x"}\n'
    msg 7 notify.post jkb-claude-d 50000000 false
  } | "$tmp/jkb-notifier" serve --dry-run --now-ms 50400000 2>/dev/null)
  check "a burst folds to its net effect, marks a stale post with its age, and is acked after" \
    "$served" "$(printf '%s\n' \
      'remove jkb-claude-a' \
      'post jkb-claude-b Claude Code | wt · stale, 14h ago | needs Bash' \
      '{"ack":4}' \
      'post jkb-claude-c Claude Code | wt | needs Bash' \
      '{"ack":6}')"
  script=$("$tmp/jkb-notifier" banner-script --title 'Claude Code' --subtitle 'w"t' \
    --body "$(printf 'say "hi" \\ now\nplease')")
  check "the fallback banner's AppleScript escapes and folds" \
    "$script" 'display notification "say \"hi\" \\ now please" with title "Claude Code" subtitle "w\"t"'
  if command -v osacompile >/dev/null 2>&1; then
    osacompile -o "$tmp/banner.scpt" -e "$script" 2>"$tmp/osacompile.err"
    check "and it compiles" "$?" "0"
  fi
fi

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
elif ! grep -q "authorization=authorized" <<<"$("$live_bin" status 2>/dev/null)"; then
  fail "live round-trip: notifier is not authorized — run: '$live_bin' authorize"
else
  # Driven through the REAL hook with the payloads Claude Code actually sends — nothing stubbed,
  # no injected notifier. Testing `jkb-notifier` on its own would leave the seam that matters
  # (hook -> notifier -> notification centre) uncovered, and that seam is the whole feature.
  live_sid="hook-selftest-$$"
  live_id="jkb-claude-$live_sid"
  # The path is asynchronous since r3.2 — hook, jkb serve, the queue, then the com.jkb.notifier agent
  # polling it — so each assertion waits up to 5 s for the state it expects before reading it once.
  delivered_now() { "$live_bin" list 2>/dev/null | grep -c "^$live_id\$" | tr -d ' '; }
  delivered() { # delivered <expected> -> the count, once it matches or 5 s have passed
    local i=0
    until [ "$(delivered_now)" = "$1" ] || [ "$i" -ge 50 ]; do sleep 0.1; i=$((i + 1)); done
    delivered_now
  }
  # Driven against THIS CHECKOUT's jkb, not whatever is installed. The shim delegates to the
  # first `jkb` on PATH, and an installed binary predating `notify` silently does nothing — which
  # is precisely what this test saw the first time it ran against the shim.
  repo_target="${CARGO_TARGET_DIR:-$(cd "$(dirname "$0")/../.." && pwd)/target}/debug"
  if [ ! -x "$repo_target/jkb" ]; then
    fail "live round-trip: $repo_target/jkb is not built — run ./scripts/build.sh"
    live_bin=""
  fi
  live() { printf '%s' "$1" | PATH="$repo_target:$PATH" bash "$hook"; }

  printf '  --  %s\n' "style: $("$live_bin" status 2>/dev/null)"

  # The path this drives is the INSTALLED daemon and agent: the lifecycle table runs in com.jkb.serve
  # (the jkb setup.sh last installed, not this checkout's) and display in com.jkb.notifier. So both are
  # checked first, and a failure below names what is missing rather than reading 'expected 1, got 0'.
  live_topic=$("$repo_target/jkb" notify topic 2>/dev/null)
  if ! grep -q '"name"' <<<"$("$repo_target/jkb" --json mq group ls "$live_topic" 2>/dev/null)"; then
    fail "live round-trip: $live_topic has no consumer group — is com.jkb.notifier running? (setup.sh)"
  fi
  live_hello=$(curl -s --max-time 2 \
    -H "Authorization: Bearer $(cat "$("$repo_target/jkb" service token-path 2>/dev/null)" 2>/dev/null)" \
    "$("$repo_target/jkb" service serve-url 2>/dev/null)/v1/hello")
  if ! grep -q '"protocol"' <<<"$live_hello"; then
    fail "live round-trip: jkb serve does not answer — is com.jkb.serve running? (setup.sh)"
  fi

  # Claude needs permission.
  live "$(payload Notification "$live_sid" "$PROMPT")"
  check "the Notification hook posts a real notification" "$(delivered 1)" "1"

  # A DIFFERENT tool finishing first — the batched-call case — must leave it up.
  live "$(payload PostToolUse "$live_sid" '' Read)"
  sleep 1  # long enough for a wrong withdrawal to have arrived
  check "a concurrent tool leaves the real notification up" "$(delivered_now)" "1"

  # You grant it; the tool runs; PostToolUse fires for THAT tool. The pair the change exists for.
  live "$(payload PostToolUse "$live_sid" '' Bash)"
  check "granting permission withdraws it" "$(delivered 0)" "0"

  # The other half: the idle-waiting notification, cleared by typing rather than by a tool.
  live "$(payload Notification "$live_sid" 'Claude is waiting for your input')"
  check "the idle notification posts" "$(delivered 1)" "1"
  live "$(payload UserPromptSubmit "$live_sid" '')"
  check "typing a prompt withdraws it" "$(delivered 0)" "0"

  # NOTE: `list` reports DELIVERED notifications, which includes one that has hidden its banner
  # and is resting in Notification Center. So nothing above proves the notification stayed
  # VISIBLE — no API exposes that. Stickiness is checked the only way it can be, by reading the
  # style back, and confirmed the only way it can be, by looking at the screen.
  case "$("$live_bin" status 2>/dev/null)" in
    *alert-style=alert*) ok "the alert style is sticky" ;;
    *) printf '  --  %s\n' "alert style is not 'alert' — banners will hide; set System Settings > Notifications > jkb Notifier > Alerts" ;;
  esac
fi

if [ "$failures" -ne 0 ]; then
  printf '\n%d hook test(s) failed\n' "$failures" >&2
  exit 1
fi
echo "All hook tests passed."
