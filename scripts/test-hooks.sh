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
  live_id="jkb-hook-selftest-$$"
  "$live_bin" post --id "$live_id" --title "jkb self-test" --body "withdrawn immediately" \
    >/dev/null 2>&1
  check "a posted notification is delivered" \
    "$("$live_bin" list 2>/dev/null | grep -c "^$live_id$" | tr -d ' ')" "1"
  "$live_bin" remove --id "$live_id" >/dev/null 2>&1
  check "a withdrawn notification is gone" \
    "$("$live_bin" list 2>/dev/null | grep -c "^$live_id$" | tr -d ' ')" "0"
fi

if [ "$failures" -ne 0 ]; then
  printf '\n%d hook test(s) failed\n' "$failures" >&2
  exit 1
fi
echo "All hook tests passed."
