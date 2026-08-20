#!/usr/bin/env bash
# Tests for scripts/auto-mode.sh (design D48). Hermetic: a temp CLAUDE_CONFIG_DIR, no network,
# no Claude Code session, no writes outside the temp dir. Part of ./scripts/check.sh.
#
# The drift cases are GENERATED FROM THE POSTURE FILE, not listed here: every boolean is flipped
# in turn and every array element dropped in turn, and each must produce a refusal naming that
# path. A key added to auto-mode-posture.json is therefore covered the moment it is added —
# which is the whole reason `check` is one generic subset rule rather than a list of assertions
# that has to be kept in step with the posture beside it.
#
# `probe` is deliberately NOT exercised: it needs a live billed session, so it is this change's
# #[ignore] test.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
tool="$here/auto-mode.sh"
posture="$here/auto-mode-posture.json"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
export CLAUDE_CONFIG_DIR="$tmp/claude"
settings="$CLAUDE_CONFIG_DIR/settings.json"

pass=0
fail=0
ok()   { pass=$((pass + 1)); printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad()  { fail=$((fail + 1)); printf '  \033[31mFAIL\033[0m %s\n' "$1"; }
check_that() { if [ "$2" = "yes" ]; then ok "$1"; else bad "$1"; fi; }

# Run `check`, capturing output; sets $out and $rc rather than returning, so `set -e` does not
# abort on the (expected) non-zero exits.
run_check() {
    set +e
    out="$("$tool" check 2>&1)"
    rc=$?
    set -e
}

# Resolved BEFORE section 8 puts a stub `claude` on PATH: the schema check at the end must not
# end up asking the stub whether the posture is valid. (It did, and the mutation that proved the
# check inert is why this line is here rather than beside its use.)
real_claude="$(command -v claude || true)"
[ -n "$real_claude" ] || real_claude="$HOME/.local/bin/claude"

echo "==> auto-mode.sh"

# --- 1. a missing settings file is a named refusal, not a jq crash ------------------------
run_check
check_that "check refuses when no settings file exists" \
    "$([ "$rc" -ne 0 ] && grep -q 'no settings file' <<<"$out" && echo yes || echo no)"

# --- 2. install merges into an existing file without disturbing it ------------------------
mkdir -p "$CLAUDE_CONFIG_DIR"
cat > "$settings" <<'JSON'
{
  "theme": "dark",
  "skipWorkflowUsageWarning": true,
  "permissions": { "allow": ["Bash(jkb grep *)", "Bash(git diff *)"] }
}
JSON
set +e
"$tool" install >"$tmp/install.out" 2>&1
rc=$?
set -e
check_that "install succeeds on an existing settings file" \
    "$([ "$rc" -eq 0 ] && echo yes || echo no)"
[ "$rc" -eq 0 ] || sed 's/^/       /' "$tmp/install.out"

check_that "install preserves unrelated top-level keys" \
    "$(jq -e '.theme == "dark" and .skipWorkflowUsageWarning == true' "$settings" >/dev/null && echo yes || echo no)"
check_that "install preserves the existing permissions.allow list, in order" \
    "$(jq -e '.permissions.allow == ["Bash(jkb grep *)", "Bash(git diff *)"]' "$settings" >/dev/null && echo yes || echo no)"

run_check
check_that "check passes on freshly installed settings" \
    "$([ "$rc" -eq 0 ] && echo yes || echo no)"

# A present-and-correct `false` must READ as satisfied. jq's `//` treats false as empty, so the
# obvious spelling of "the value, or null if absent" turns every correct `false` into a failure —
# and this posture's strongest single setting (allowUnsandboxedCommands) is exactly that shape.
check_that "a correct false-valued setting satisfies check" \
    "$(jq -e '.sandbox.allowUnsandboxedCommands == false' "$settings" >/dev/null && [ "$rc" -eq 0 ] && echo yes || echo no)"

# --- 3. install is idempotent, byte for byte ----------------------------------------------
cp "$settings" "$tmp/first"
"$tool" install >/dev/null 2>&1 || true
check_that "install is idempotent (byte-identical on re-run)" \
    "$(cmp -s "$tmp/first" "$settings" && echo yes || echo no)"
check_that "an unchanged re-install writes no backup" \
    "$([ "$(find "$CLAUDE_CONFIG_DIR" -name 'settings.json.bak-*' | wc -l)" -eq 1 ] && echo yes || echo no)"

# The backup is the recovery path, so it has to hold what was there BEFORE the merge — not a
# second copy of the merged result, which is the way this silently becomes useless.
backup="$(find "$CLAUDE_CONFIG_DIR" -name 'settings.json.bak-*' | head -1)"
check_that "the backup holds the pre-merge contents, not the merged ones" \
    "$(jq -e '.sandbox == null and .theme == "dark" and (.permissions.deny | not)' "$backup" >/dev/null && echo yes || echo no)"

cp "$settings" "$tmp/good"

# --- 4. every boolean in the posture, flipped, is caught and named -------------------------
booleans="$(jq -c '.require | paths(type == "boolean")' "$posture")"
check_that "the posture declares at least one boolean to flip" \
    "$([ -n "$booleans" ] && echo yes || echo no)"
while IFS= read -r p; do
    [ -n "$p" ] || continue
    label=".$(jq -r 'join(".")' <<<"$p")"
    jq --argjson p "$p" 'setpath($p; getpath($p) | not)' "$tmp/good" > "$settings"
    run_check
    check_that "flipping $label is refused, by name" \
        "$([ "$rc" -ne 0 ] && grep -qF "$label" <<<"$out" && echo yes || echo no)"

    jq --argjson p "$p" 'delpaths([$p])' "$tmp/good" > "$settings"
    run_check
    check_that "deleting $label is refused, by name" \
        "$([ "$rc" -ne 0 ] && grep -qF "$label" <<<"$out" && echo yes || echo no)"
done <<<"$booleans"

# --- 5. every array element in the posture, dropped, is caught and named -------------------
dropped=0
while IFS= read -r p; do
    [ -n "$p" ] || continue
    n="$(jq --argjson p "$p" '.require | getpath($p) | length' "$posture")"
    for ((i = 0; i < n; i++)); do
        want="$(jq -c --argjson p "$p" --argjson i "$i" '.require | getpath($p)[$i]' "$posture")"
        jq --argjson p "$p" --argjson i "$i" 'setpath($p; getpath($p) | del(.[$i]))' "$tmp/good" > "$settings"
        run_check
        if [ "$rc" -ne 0 ] && grep -qF "$want" <<<"$out"; then
            dropped=$((dropped + 1))
        else
            bad "dropping $(jq -r 'join(".")' <<<"$p")[$i] ($want) was not refused by name"
        fi
    done
done <<<"$(jq -c '.require | paths(type == "array")' "$posture")"
check_that "every posture list entry ($dropped) is required individually" \
    "$([ "$dropped" -gt 0 ] && echo yes || echo no)"

# An array the user has EXTENDED is still intact: the posture is a subset, never an equality.
jq '.sandbox.network.allowedDomains += ["example.internal"]' "$tmp/good" > "$settings"
run_check
check_that "check tolerates extra entries the user added" \
    "$([ "$rc" -eq 0 ] && echo yes || echo no)"

# --- 6. malformed JSON is a named refusal, and install refuses to overwrite it --------------
printf '{oops\n' > "$settings"
run_check
check_that "check refuses malformed JSON, by name" \
    "$([ "$rc" -ne 0 ] && grep -q 'not valid JSON' <<<"$out" && echo yes || echo no)"
set +e
"$tool" install >/dev/null 2>&1
rc=$?
set -e
check_that "install refuses to merge into malformed JSON" \
    "$([ "$rc" -ne 0 ] && [ "$(cat "$settings")" = "{oops" ] && echo yes || echo no)"

# --- 6b. every `forbid` key: absent and empty pass, ANY entry is refused --------------------
# These are the "at most" assertions, and they are the reason `forbid` exists as a separate rule:
# under subset semantics a posture entry of `[]` would assert nothing, so a bypass list nobody
# checks is exactly the hole this whole change is about.
cp "$tmp/good" "$settings"
run_check
check_that "an absent forbid key passes" "$([ "$rc" -eq 0 ] && echo yes || echo no)"

forbid_checked=0
while IFS= read -r key; do
    [ -n "$key" ] || continue
    path="$(jq -c --arg k "$key" '$k | split(".")' <<<'null')"

    jq --argjson p "$path" 'setpath($p; [])' "$tmp/good" > "$settings"
    run_check
    check_that "an empty $key passes" "$([ "$rc" -eq 0 ] && echo yes || echo no)"

    jq --argjson p "$path" 'setpath($p; ["something"])' "$tmp/good" > "$settings"
    run_check
    check_that "a non-empty $key is refused, by name" \
        "$([ "$rc" -ne 0 ] && grep -qF ".$key" <<<"$out" && echo yes || echo no)"
    forbid_checked=$((forbid_checked + 1))
done <<<"$(jq -r '.forbid | keys[]' "$posture")"
check_that "the posture declares forbid keys, and each was exercised ($forbid_checked)" \
    "$([ "$forbid_checked" -gt 0 ] && echo yes || echo no)"
cp "$tmp/good" "$settings"

# --- 7. install creates the config dir when a fresh machine has none -----------------------
export CLAUDE_CONFIG_DIR="$tmp/fresh"
set +e
"$tool" install >/dev/null 2>&1
rc=$?
set -e
check_that "install bootstraps a missing config dir" \
    "$([ "$rc" -eq 0 ] && [ -f "$tmp/fresh/settings.json" ] && echo yes || echo no)"
check_that "a bootstrapped install leaves no backup to clean up" \
    "$([ "$(find "$tmp/fresh" -name 'settings.json.bak-*' | wc -l)" -eq 0 ] && echo yes || echo no)"

# --- 8. `run` gates on `check`, and hands claude the right argv ----------------------------
# A stub `claude` on PATH, so `run`'s exec is observable without a billed session. This pins
# three things at once: that `check` runs BEFORE the exec (so there is no window in which a
# drifted posture launches), that auto mode is what gets launched, and — the reason the stub
# exists at all — that the optional ssh-agent overlay expands correctly. bash 3.2 (what macOS
# ships, and what this runs under) treats "${arr[@]}" on an EMPTY array as an unbound-variable
# error under `set -u`, so the no-overlay path is the one that breaks first and silently.
mkdir -p "$tmp/bin"
cat > "$tmp/bin/claude" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$CLAUDE_ARGV_OUT"
STUB
chmod +x "$tmp/bin/claude"
export PATH="$tmp/bin:$PATH"
export CLAUDE_ARGV_OUT="$tmp/argv"

export CLAUDE_CONFIG_DIR="$tmp/claude"
cp "$tmp/good" "$settings"
rm -f "$CLAUDE_ARGV_OUT"
set +e
"$tool" run --model sonnet >/dev/null 2>&1
rc=$?
set -e
check_that "run launches auto mode and passes user args through" \
    "$([ "$rc" -eq 0 ] && [ "$(tr '\n' ' ' < "$CLAUDE_ARGV_OUT")" = "--permission-mode auto --model sonnet " ] && echo yes || echo no)"

rm -f "$CLAUDE_ARGV_OUT"
set +e
JKB_AUTO_MODE_SSH_AGENT=1 SSH_AUTH_SOCK="$tmp/agent.sock" "$tool" run >/dev/null 2>&1
set -e
check_that "the opt-in ssh-agent overlay names the socket and nothing else" \
    "$(grep -qF "{\"sandbox\":{\"network\":{\"allowUnixSockets\":[\"$tmp/agent.sock\"]}}}" "$CLAUDE_ARGV_OUT" && echo yes || echo no)"

rm -f "$CLAUDE_ARGV_OUT"
jq '.sandbox.enabled = false' "$tmp/good" > "$settings"
set +e
"$tool" run >/dev/null 2>&1
rc=$?
set -e
check_that "run refuses to launch a drifted posture, and execs nothing" \
    "$([ "$rc" -ne 0 ] && [ ! -e "$CLAUDE_ARGV_OUT" ] && echo yes || echo no)"
cp "$tmp/good" "$settings"

# --- 9. print is the posture, verbatim -----------------------------------------------------
check_that "print emits the posture file unchanged" \
    "$(diff -q <("$tool" print) "$posture" >/dev/null && echo yes || echo no)"
check_that "install writes .require and never the .forbid section" \
    "$(jq -e 'has("require") | not' "$settings" >/dev/null && jq -e 'has("forbid") | not' "$settings" >/dev/null && echo yes || echo no)"

# --- 9b. --help prints the whole header ----------------------------------------------------
# It is derived from the file rather than a line range, because the range was hardcoded, went
# stale the first time the header grew, and silently truncated the last paragraph.
help_out="$("$tool" --help)"
last_comment="$(awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); l = $0; next } NR > 1 { exit } END { print l }' "$tool")"
check_that "--help prints the header through its final line" \
    "$([ -n "$last_comment" ] && grep -qF "$last_comment" <<<"$help_out" && echo yes || echo no)"
check_that "--help strips the comment markers" \
    "$(grep -q '^#' <<<"$help_out" && echo no || echo yes)"

# --- 10. the posture validates against CLAUDE CODE'S OWN SCHEMA -----------------------------
# `claude doctor` reads settings in the current directory and reports schema violations, so it is
# a real validator for the committed file — a typo'd key or an out-of-range enum (`defaultMode`
# is a closed set) would otherwise install cleanly and be silently ignored at runtime, which is
# indistinguishable from a posture that is in force. Skipped when Claude Code is absent (CI has
# none); a skip is announced, never silent.
if [ -x "$real_claude" ]; then
    proj="$tmp/schema"
    mkdir -p "$proj/.claude"
    jq '.require' "$posture" > "$proj/.claude/settings.json"
    doctor="$(cd "$proj" && "$real_claude" doctor 2>&1 || true)"
    check_that "the posture validates against Claude Code's own settings schema" \
        "$(grep -q 'Invalid settings' <<<"$doctor" && echo no || echo yes)"
    grep -q 'Invalid settings' <<<"$doctor" && sed -n '/Invalid settings/,/^$/p' <<<"$doctor" | sed 's/^/       /'
else
    echo "  skip  schema validation (claude not installed)"
fi

echo
if [ "$fail" -ne 0 ]; then
    printf '\033[31m%d failed\033[0m, %d passed\n' "$fail" "$pass"
    exit 1
fi
printf '\033[32mall %d auto-mode checks passed\033[0m\n' "$pass"
