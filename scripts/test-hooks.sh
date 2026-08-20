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

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

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

payload() { printf '{"hook_event_name":"%s","session_id":"%s","message":"%s","cwd":"/repos/jkb/.jkb/work/wt"}' "$1" "$2" "$3"; }

echo "==> notify-sticky.sh"

# 1. A Notification posts under an id derived from the session, carrying the message and the
#    worktree name.
run "$(payload Notification s1 'Claude needs your permission to use Bash')"
check "Notification posts under the session's id" \
  "$(calls)" \
  "post --id jkb-claude-s1 --title Claude Code --subtitle wt --body Claude needs your permission to use Bash"

# 2. The dismissing events each withdraw that same id. All four are asserted: they fire on
#    different paths through a turn (grant, type, deny, quit) and any one of them silently
#    dropping out of the case arm leaves a notification stuck on screen.
for ev in PostToolUse UserPromptSubmit Stop SessionEnd; do
  run "$(payload Notification s1 'needs permission')" >/dev/null
  run "$(payload "$ev" s1 '')"
  check "$ev withdraws the notification" "$(calls)" "remove --id jkb-claude-s1"
done

# 3. With nothing posted, a dismiss must not spawn the notifier — PostToolUse runs after every
#    single tool call, so this is the whole reason the marker exists.
run "$(payload PostToolUse s1 '')"
check "dismiss with nothing pending is free" "$(calls)" ""

# 4. Dismissing twice does not re-run it either: the marker is consumed, not just read.
run "$(payload Notification s1 'needs permission')" >/dev/null
run "$(payload PostToolUse s1 '')" >/dev/null
run "$(payload PostToolUse s1 '')"
check "a second dismiss is free" "$(calls)" ""

# 5. Parallel sessions (D36 worktrees) hold separate ids, so one session answering a prompt
#    must not clear another session's notification.
run "$(payload Notification s1 'a')" >/dev/null
run "$(payload Notification s2 'b')" >/dev/null
run "$(payload PostToolUse s1 '')"
check "one session's dismiss targets only its own id" "$(calls)" "remove --id jkb-claude-s1"
run "$(payload PostToolUse s2 '')"
check "the other session is still pending" "$(calls)" "remove --id jkb-claude-s2"

# 6. An id is a group name and a filename, so it must not be able to name a path.
run '{"hook_event_name":"Notification","session_id":"../../etc/passwd","message":"m","cwd":"/x"}'
check "a path-like session id is sanitised" "$(calls)" \
  "post --id jkb-claude-______etc_passwd --title Claude Code --subtitle x --body m"
# The marker it wrote is a direct child of the state directory under the sanitised name, so the
# separators were neutralised rather than merely renamed somewhere deeper.
check "the marker stays a direct child of the state directory" \
  "$(cd "$tmp/state" && find . -mindepth 2 | wc -l | tr -d ' ')" "0"
check "the marker carries the sanitised name" \
  "$([ -f "$tmp/state/______etc_passwd" ] && echo yes || echo no)" "yes"

# 7. An event the hook has no business in does nothing at all.
run "$(payload PreToolUse s1 '')"
check "an unhandled event is a no-op" "$(calls)" ""

# 8. Malformed input fails open rather than disturbing the session.
: > "$rec"
out=$(JKB_NOTIFIER="$tmp/notifier" JKB_NOTIFY_STATE="$tmp/state" REC="$rec" bash "$hook" <<<'not json' 2>/dev/null)
check "malformed input exits 0" "$?" "0"
check "malformed input posts nothing" "$(calls)" ""

# 9. Nothing reaches stdout: a hook's stdout lands in the transcript, and on UserPromptSubmit it
#    is injected into the conversation as context.
out=$(run "$(payload Notification s9 'permission')")
check "the hook is silent on stdout" "$out" ""

# 10. The fallback path. With no terminal-notifier installed this is what actually runs, so it is
#     not a degraded corner — on a stock macOS it is the only path there is. `JKB_NOTIFIER` naming
#     a file that does not exist forces it deterministically.
osa="$tmp/osacalls"
cat > "$tmp/osascript" <<'STUB'
#!/usr/bin/env bash
for a in "$@"; do printf '%s\n' "$a" >> "$OSA_REC"; done
STUB
chmod +x "$tmp/osascript"
osa_calls() { tr '\n' ' ' < "$osa" | sed 's/  */ /g; s/^ //; s/ $//'; }

# No notifier bundle at all.
fallback() {
  : > "$rec"; : > "$osa"
  PATH="$tmp:$PATH" JKB_NOTIFIER="$tmp/does-not-exist" JKB_NOTIFY_STATE="$tmp/state" \
    REC="$rec" OSA_REC="$osa" bash "$hook" <<<"$1"
}

# Bundle present but REFUSING, which is what the real notifier does when it is not yet authorized.
refusing() {
  : > "$rec"; : > "$osa"
  PATH="$tmp:$PATH" JKB_NOTIFIER="$tmp/notifier" NOTIFIER_FAILS=2 JKB_NOTIFY_STATE="$tmp/state" \
    REC="$rec" OSA_REC="$osa" bash "$hook" <<<"$1"
}

fallback '{"hook_event_name":"Notification","session_id":"s10","message":"needs permission","cwd":"/repos/wt"}'
check "without the notifier bundle it still posts a banner" \
  "$(osa_calls)" \
  '-e display notification "needs permission" with title "Claude Code" subtitle "wt"'

# A notification message is Claude's text, so it can carry the three characters that end an
# AppleScript string early. Getting this wrong means no notification at all, silently.
fallback '{"hook_event_name":"Notification","session_id":"s11","message":"say \"hi\" \\ now\nplease","cwd":"/repos/wt"}'
script=$(sed -n '2p' "$osa")
check "quotes, backslashes and newlines are escaped" \
  "$script" \
  'display notification "say \"hi\" \\ now please" with title "Claude Code" subtitle "wt"'

# ...and that escaping is checked against a real AppleScript compiler where one exists, rather
# than only against our own idea of it. Absent on Linux, so the suite skips it there.
if ! command -v osacompile >/dev/null 2>&1; then
  printf '  --  %s\n' "AppleScript compile check (osacompile not present)"
elif [ -z "$script" ]; then
  # `osacompile -e ""` falls back to READING STDIN and blocks forever, so an assertion that
  # already failed above must not be allowed to hang the suite behind it. (`</dev/null` below is
  # the same guard for the non-empty case.)
  fail "the escaped script compiles as AppleScript: nothing was posted to compile"
elif osacompile -o "$tmp/probe.scpt" -e "$script" >/dev/null 2>&1 </dev/null; then
  ok "the escaped script compiles as AppleScript"
else
  fail "the escaped script compiles as AppleScript"
fi

# 11. `--find-notifier` is how `scripts/setup.sh` reports readiness without keeping its own copy
#     of the search list, so it has to answer for the same binary the hook would actually run. It
#     also must not read stdin: a probe has none, and `input=$(cat)` would block on the terminal.
found=$(JKB_NOTIFIER="$tmp/notifier" bash "$hook" --find-notifier </dev/null 2>/dev/null)
check "--find-notifier reports the notifier the hook would use" "$found" "$tmp/notifier"

found=$(JKB_NOTIFIER="$tmp/does-not-exist" bash "$hook" --find-notifier </dev/null 2>/dev/null)
status=$?
check "--find-notifier says nothing when there is none" "$found" ""
check "--find-notifier exits non-zero when there is none" "$([ "$status" -ne 0 ] && echo yes || echo no)" "yes"

# 12. The refusal path, which is the whole reason `post` reports failure instead of quietly
#     succeeding: an unauthorized notification is accepted by macOS, displays nothing, and returns
#     no error, so a notifier that did not refuse would be indistinguishable from a working one.
refusing '{"hook_event_name":"Notification","session_id":"s12","message":"needs permission","cwd":"/repos/wt"}'
check "a refusing notifier is still tried first" \
  "$(calls)" \
  "post --id jkb-claude-s12 --title Claude Code --subtitle wt --body needs permission"
check "a refusing notifier falls back to a banner" \
  "$(osa_calls)" \
  '-e display notification "needs permission" with title "Claude Code" subtitle "wt"'

# ...and the dismiss side still runs, so the notification is withdrawn if it did reach the screen
# by some other route. Refusing to post must not also disable clearing.
: > "$rec"; : > "$osa"
PATH="$tmp:$PATH" JKB_NOTIFIER="$tmp/notifier" JKB_NOTIFY_STATE="$tmp/state" \
  REC="$rec" OSA_REC="$osa" bash "$hook" <<<"$(payload PostToolUse s12 '')"
check "a refused post still leaves the session dismissable" "$(calls)" "remove --id jkb-claude-s12"

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
  live() { printf '%s' "$1" | bash "$hook"; }

  printf '  --  %s\n' "style: $("$live_bin" status 2>/dev/null)"

  # Claude needs permission.
  live "$(payload Notification "$live_sid" 'Claude needs your permission to use Bash')"
  check "the Notification hook posts a real notification" "$(delivered)" "1"

  # You grant it; the tool runs; PostToolUse fires. This is the pair the whole change exists for.
  live "$(payload PostToolUse "$live_sid" '')"
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

# The trigger that decides whether a `git pull` rebuilds what setup.sh installs. It kept its own
# copy of the path list and drifted the first time it mattered — setup.sh gained the notifier step
# (built from macos/) and this pattern did not, so pulling a notifier change left the stale bundle
# in place. It now asks setup.sh, and these assert the property that failed: every path setup.sh
# builds FROM must fire the trigger.
setup_sh="$(cd "$(dirname "$0")/.." && pwd)/scripts/setup.sh"
pm="$(cd "$(dirname "$0")/.." && pwd)/scripts/hooks/post-merge"
build_paths=$("$setup_sh" --build-paths 2>/dev/null)
check "setup.sh answers --build-paths" "$([ -n "$build_paths" ] && echo yes || echo no)" "yes"

fires() { printf '%s\n' "$1" | grep -qE "$build_paths" && echo fire || echo skip; }
for path in macos/notifier/main.swift macos/notifier/Info.plist crates/jkb-cli/src/main.rs \
            ui/core/src/summary.ts scripts/build-notifier.sh Cargo.toml Cargo.lock; do
  check "a pull touching $path rebuilds" "$(fires "$path")" "fire"
done
for path in openspec/changes/x/design.md README.md CLAUDE.md .codereviews/x/tasks.md; do
  check "a pull touching $path does not rebuild" "$(fires "$path")" "skip"
done

# post-merge must not keep a pattern of its own — that is the whole fix, and a re-added literal
# would pass every assertion above while drifting again on the next step.
check "post-merge holds no path pattern of its own" \
  "$(grep -cE "crates/\|ui/|\^\(crates" "$pm" | tr -d ' ')" "0"

# An unanswerable trigger must degrade toward running setup.sh: a skipped rebuild is silent, an
# unnecessary one only costs time.
check "an empty pattern is not treated as 'matches nothing'" \
  "$(grep -c 'if \[ -z "$build_paths" \] ||' "$pm" | tr -d ' ')" "1"

if [ "$failures" -ne 0 ]; then
  printf '\n%d hook test(s) failed\n' "$failures" >&2
  exit 1
fi
echo "All hook tests passed."
