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
"$tool" install --force >"$tmp/install.out" 2>&1
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
"$tool" install --force >/dev/null 2>&1 || true
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
"$tool" install --force >/dev/null 2>&1
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

# --- 6c. an UNREADABLE settings file is not a missing one ----------------------------------
# Found by installing the posture for real: `denyRead: ["~"]` covered ~/.claude/settings.json, so
# `[ -f ]` was false, `check` reported "no settings file — run install", and install's
# fresh-machine branch would have merged the posture into {} — dropping 45 live permission rules,
# with no backup, because that branch believes there is nothing to back up. The write-deny is what
# stopped it, which is luck rather than design.
cp "$tmp/good" "$settings"
chmod 000 "$settings"
if [ -r "$settings" ]; then
    echo "  skip  unreadable-file cases (running as root — chmod cannot deny us)"
else
    run_check
    check_that "check refuses an unreadable settings file as DENIED, not missing" \
        "$([ "$rc" -ne 0 ] && grep -q 'permission denied' <<<"$out" && ! grep -q 'no settings file' <<<"$out" && echo yes || echo no)"
    check_that "that refusal does not advise running install" \
        "$(grep -q "run: .*install" <<<"$out" && echo no || echo yes)"

    set +e
    "$tool" install --force >"$tmp/inst.out" 2>&1
    rc=$?
    set -e
    check_that "install refuses rather than overwriting an unreadable file" \
        "$([ "$rc" -ne 0 ] && grep -q 'refusing to overwrite' "$tmp/inst.out" && echo yes || echo no)"
    chmod 644 "$settings"
    check_that "the unreadable file was left completely untouched" \
        "$(cmp -s "$tmp/good" "$settings" && echo yes || echo no)"
fi
chmod 644 "$settings" 2>/dev/null
cp "$tmp/good" "$settings"

# --- 6d. install's preflight gate, pinned deliberately -------------------------------------
# Every other install in this suite passes --force, because preflight asks a question about the
# MACHINE and the suite must be hermetic — on CI the checkout is /home/runner/work/..., under no
# allowWrite root, and without --force install wrote nothing while two assertions ("idempotent",
# "preserves allow") passed vacuously on the empty result. So the gate gets one test of its own
# rather than being exercised incidentally by all of them.
cp "$tmp/good" "$settings"
narrow="$tmp/posture-narrow.json"
jq '.require.sandbox.filesystem.allowWrite = ["/definitely-not-here"]' "$posture" > "$narrow"
mkdir -p "$tmp/gate/scripts"
cp "$here/auto-mode.sh" "$tmp/gate/scripts/"
cp "$narrow" "$tmp/gate/scripts/auto-mode-posture.json"
before="$(cat "$settings")"
set +e
"$tmp/gate/scripts/auto-mode.sh" install >"$tmp/gate.out" 2>&1
rc=$?
set -e
check_that "install refuses a posture preflight rejects" \
    "$([ "$rc" -ne 0 ] && grep -q 'preflight gaps' "$tmp/gate.out" && echo yes || echo no)"
check_that "a refused install writes nothing" \
    "$([ "$before" = "$(cat "$settings")" ] && echo yes || echo no)"
set +e
"$tmp/gate/scripts/auto-mode.sh" install --force >/dev/null 2>&1
rc=$?
set -e
check_that "--force installs the same posture the gate refused" \
    "$([ "$rc" -eq 0 ] && echo yes || echo no)"
cp "$tmp/good" "$settings"

# --- 6e. a forbidden key is REPAIRABLE by install ------------------------------------------
# It was not: `check` failed permanently and named `install` as the remedy, while install could
# only add keys and printed "already installed (no change)". The keys are declared must-be-empty,
# so emptying them is the defined repair and belongs in the same write.
jq '.sandbox.excludedCommands = ["bash"]' "$tmp/good" > "$settings"
run_check
check_that "a forbidden key makes check fail" "$([ "$rc" -ne 0 ] && echo yes || echo no)"
set +e
"$tool" install --force >/dev/null 2>&1
set -e
run_check
check_that "install clears the forbidden key it was pointed at" \
    "$([ "$rc" -eq 0 ] && echo yes || echo no)"
check_that "and the key is actually gone, not merely emptied around" \
    "$(jq -e '.sandbox | has("excludedCommands") | not' "$settings" >/dev/null && echo yes || echo no)"
cp "$tmp/good" "$settings"

# --- 7. install creates the config dir when a fresh machine has none -----------------------
export CLAUDE_CONFIG_DIR="$tmp/fresh"
set +e
"$tool" install --force >/dev/null 2>&1
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

# `run` hard-refuses on a Linux host without bubblewrap/socat — correctly, since the sandbox
# cannot start there — so the assertions that need it to REACH THE EXEC would fail for a reason
# about the machine, reddening a gate check.sh and CI both run. Decide once and skip those
# together. The dependency list is read out of auto-mode.sh rather than restated: two copies of
# "which tools does the sandbox need" is one more than can be kept in agreement.
run_deps="$(sed -n '/^missing_sandbox_deps()/,/^}/p' "$here/auto-mode.sh" | sed -n 's/.*for dep in \([^;]*\); do.*/\1/p')"
run_skip=""
if [ "$(uname -s)" = "Linux" ]; then
    for dep in $run_deps; do
        command -v "$dep" >/dev/null 2>&1 || run_skip="${run_skip:+$run_skip }$dep"
    done
fi
check_that "the dependency list was read out of auto-mode.sh, not guessed" \
    "$([ -n "$run_deps" ] && echo yes || echo no)"

if [ -n "$run_skip" ]; then
    echo "  skip  the exec-dependent \`run\` assertions — this host is missing: $run_skip (apt install bubblewrap socat)"
else
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
    # macOS passes the overlay; Linux must refuse to pretend. `allowUnixSockets` is documented
    # macOS-only, so an overlay emitted there would be a flag that reports success and does nothing.
    if [ "$(uname -s)" = "Darwin" ]; then
        check_that "the opt-in ssh-agent overlay names the socket and nothing else" \
            "$(grep -qF "{\"sandbox\":{\"network\":{\"allowUnixSockets\":[\"$tmp/agent.sock\"]}}}" "$CLAUDE_ARGV_OUT" && echo yes || echo no)"
    else
        # Absence of the overlay in a file that was never written is not evidence of anything, so
        # establish that `run` reached the exec before reading meaning into what is not there.
        check_that "run reached the exec, so the overlay's absence below means something" \
            "$([ -s "$CLAUDE_ARGV_OUT" ] && grep -q -- "--permission-mode" "$CLAUDE_ARGV_OUT" && echo yes || echo no)"
        check_that "the ssh-agent overlay is NOT emitted on Linux, where it would be inert" \
            "$(grep -q "allowUnixSockets" "$CLAUDE_ARGV_OUT" && echo no || echo yes)"
    fi

    cp "$tmp/good" "$settings"
fi

# OUTSIDE the skip group, deliberately: cmd_run runs cmd_check BEFORE the dependency gate, so a
# drifted posture is refused whether or not bubblewrap exists. Skipping this with the others put
# the one assertion guarding the check-before-exec ordering out of reach on exactly the hosts CI
# uses. The refusal TEXT is the discriminating half — a non-zero exit with no argv file is also
# what a missing dependency or a missing stub produces.
rm -f "$CLAUDE_ARGV_OUT"
jq '.sandbox.enabled = false' "$tmp/good" > "$settings"
set +e
"$tool" run >"$tmp/run.out" 2>"$tmp/run.err"
rc=$?
set -e
check_that "run refuses to launch a drifted posture, and execs nothing" \
    "$([ "$rc" -ne 0 ] && [ ! -e "$CLAUDE_ARGV_OUT" ] \
       && grep -q "posture NOT intact" "$tmp/run.err" && echo yes || echo no)"
cp "$tmp/good" "$settings"

# --- 6c. `retire`: the posture can WITHDRAW an entry it once installed ----------------------
# The merge is add-only for arrays so your own permissions.allow survives a re-install — which
# means an entry deleted from `require` would otherwise sit in an installed file for ever, with
# `check` tolerating it under subset semantics. Found live: three Write(...) deny rules that
# Claude Code reports as inert at every session start, uninstallable by any amount of re-running.
cp "$tmp/good" "$settings"
retire_posture="$tmp/posture-retire.json"
jq '.retire = {"permissions.allow": ["Bash(jkb grep *)"]}' "$posture" > "$retire_posture"
mkdir -p "$tmp/rt/scripts"; cp "$here/auto-mode.sh" "$tmp/rt/scripts/"; cp "$retire_posture" "$tmp/rt/scripts/auto-mode-posture.json"

check_that "a retired entry that IS installed is reported as drift" \
    "$(set +e; out="$("$tmp/rt/scripts/auto-mode.sh" check 2>&1)"; rc=$?; set -e
       [ "$rc" -ne 0 ] && grep -q 'retired' <<<"$out" && echo yes || echo no)"

"$tmp/rt/scripts/auto-mode.sh" install --force >/dev/null 2>&1 || true
check_that "install removes the retired entry" \
    "$(jq -e '[.permissions.allow[] | select(. == "Bash(jkb grep *)")] | length == 0' "$settings" >/dev/null && echo yes || echo no)"
check_that "install leaves the OTHER entries in that array alone" \
    "$(jq -e '.permissions.allow | index("Bash(git diff *)") != null' "$settings" >/dev/null && echo yes || echo no)"
check_that "check passes once the retired entry is gone" \
    "$("$tmp/rt/scripts/auto-mode.sh" check >/dev/null 2>&1 && echo yes || echo no)"

# Retiring something that was never installed must be a silent no-op, not drift.
jq '.retire = {"permissions.allow": ["Bash(never-installed *)"]}' "$posture" > "$tmp/rt/scripts/auto-mode-posture.json"
check_that "retiring an entry that is not present is a no-op" \
    "$("$tmp/rt/scripts/auto-mode.sh" check >/dev/null 2>&1 && echo yes || echo no)"
cp "$tmp/good" "$settings"

# --- 8d. the confinement verdict, over every class the canary can end in ---------------------
# Three rounds reported CONFINED for refusals that had nothing to do with the sandbox, each fix
# adding another observation to establish "a write to $HOME would otherwise have landed". That
# premise is not establishable from inside: the sandbox intercepts access(2) too, so `[ -w $HOME ]`
# reports policy rather than permissions. The canary's own errno answers it directly and subsumes
# every case — which is why this table is short where the previous one kept growing.
verdict() { bash -c 'a=$1; b=$2; set --; source "$0" >/dev/null 2>&1; confinement_verdict "$a" "$b"' \
                   "$tool" "$1" "$2"; }
check_that "the canary wrote                => NOT CONFINED" \
    "$([ "$(verdict yes wrote)" = UNCONFINED ] && echo yes || echo no)"
check_that "refused by POLICY               => CONFINED" \
    "$([ "$(verdict yes policy)" = CONFINED ] && echo yes || echo no)"
check_that "refused by PERMISSIONS (EACCES) => INCONCLUSIVE, not a counterfeit CONFINED" \
    "$([ "$(verdict yes permissions)" = INCONCLUSIVE ] && echo yes || echo no)"
check_that "no parent to write into (ENOENT)=> INCONCLUSIVE" \
    "$([ "$(verdict yes absent)" = INCONCLUSIVE ] && echo yes || echo no)"
check_that "unattributable                  => INCONCLUSIVE" \
    "$([ "$(verdict yes unattributable)" = INCONCLUSIVE ] && echo yes || echo no)"
check_that "the control dominates every class" \
    "$([ "$(verdict no policy)" = INCONCLUSIVE ] && [ "$(verdict no wrote)" = INCONCLUSIVE ] && echo yes || echo no)"

# ...and the classifier itself, against real kernel answers rather than named classes.
klass() { bash -c 'a=$1; set --; source "$0" >/dev/null 2>&1; canary_class_of "$a"' "$tool" "$1"; }
amh="$tmp/klass"; mkdir -p "$amh/ro" "$amh/ok" "$amh/dir/blocker"; chmod 555 "$amh/ro"
check_that "classifier: a writable path reads as 'wrote'" \
    "$([ "$(klass "$amh/ok/f")" = wrote ] && echo yes || echo no)"
check_that "classifier: a permission-denied path reads as 'permissions', never 'policy'" \
    "$([ "$(klass "$amh/ro/f")" = permissions ] && echo yes || echo no)"
check_that "classifier: an absent parent reads as 'absent'" \
    "$([ "$(klass "$amh/gone/f")" = absent ] && echo yes || echo no)"
check_that "classifier: a directory in the way is not 'policy'" \
    "$([ "$(klass "$amh/dir/blocker")" != policy ] && echo yes || echo no)"

# The live shapes that produced a false CONFINED in rounds two, three and four.
mkdir -p "$amh/sub/repos"; chmod 555 "$amh/sub"
for pair in "ro:a read-only \$HOME" "sub:a writable allowWrite subdir under an unwritable \$HOME" \
            "gone/nope:an absent \$HOME"; do
    d="${pair%%:*}"; what="${pair#*:}"
    check_that "$what is INCONCLUSIVE, not CONFINED" \
        "$(set +e; HOME="$amh/$d" "$tool" sandboxed >/dev/null 2>&1; [ $? -eq 2 ] && echo yes || echo no)"
done
chmod 755 "$amh/ro" "$amh/sub"

# --- 8c. no inert rules ---------------------------------------------------------------------
# Claude Code REPORTS these at session start: "Write(path) is not matched by file permission
# checks — only Edit(path) rules are. Use Edit(path) instead (Edit rules cover all file-editing
# tools)." Three were in the posture and it took a live session to surface them, because a schema
# check validates shape and not rule semantics. An inert rule in a security posture is worse than
# a missing one: it reads as protection, and the warning it prints on every start is how people
# learn to ignore warnings.
check_that "the posture declares no inert Write(...) deny rules" \
    "$(jq -e '[.require.permissions.deny[] | select(startswith("Write("))] | length == 0' "$posture" >/dev/null && echo yes || echo no)"
# ...and the paths those rules named must still be covered, by the form that works.
check_that "the posture's self-protecting paths are denied via Edit(...), which does bind" \
    "$(jq -e '[.require.permissions.deny[]] as $d
              | ["Edit(~/.claude/settings.json)","Edit(~/.claude/settings.local.json)","Edit(~/.claude/plugins/**)"]
              | all(. as $r | $d | index($r))' "$posture" >/dev/null && echo yes || echo no)"

# --- 9. print is the posture, verbatim -----------------------------------------------------
check_that "print emits the posture file unchanged" \
    "$(diff -q <("$tool" print) "$posture" >/dev/null && echo yes || echo no)"
check_that "install writes .require and never the .forbid section" \
    "$(jq -e 'has("require") | not' "$settings" >/dev/null && jq -e 'has("forbid") | not' "$settings" >/dev/null && echo yes || echo no)"

# --- 8b. preflight detects gaps, and is machine-specific by nature -------------------------
# Tested as LOGIC, not against this machine: whether the real posture covers the real paths
# depends on where the checkout lives ($PWD under ~/repos on a dev box, /home/runner/work on CI),
# so asserting "preflight passes here" would be a test of the machine. What is testable is that a
# posture covering nothing is refused, and one covering everything is not. Deliberately NOT part
# of ./scripts/check.sh for the same reason.
empty_posture="$tmp/posture-empty.json"
jq '.require.sandbox.filesystem.allowRead = [] | .require.sandbox.filesystem.allowWrite = []' \
   "$posture" > "$empty_posture"
wide_posture="$tmp/posture-wide.json"
jq '.require.sandbox.filesystem.allowRead = ["/"] | .require.sandbox.filesystem.allowWrite = ["/"]' \
   "$posture" > "$wide_posture"

run_preflight() { # run_preflight <posture-file>
    set +e
    out="$(cd "$tmp" && cp "$1" "$tmp/pf/scripts/auto-mode-posture.json" && "$tmp/pf/scripts/auto-mode.sh" preflight 2>&1)"
    rc=$?
    set -e
}
mkdir -p "$tmp/pf/scripts"
cp "$here/auto-mode.sh" "$tmp/pf/scripts/"

run_preflight "$empty_posture"
check_that "preflight reports gaps when the posture covers nothing" \
    "$([ "$rc" -ne 0 ] && grep -q 'GAP' <<<"$out" && echo yes || echo no)"
check_that "a gap names the RESOLVED path, which is the actionable form" \
    "$(grep -qE 'resolves to|->' <<<"$out" && echo yes || echo no)"

run_preflight "$wide_posture"
check_that "preflight passes when the posture covers everything" \
    "$([ "$rc" -eq 0 ] && echo yes || echo no)"

# A pass must not read as blanket coverage. `install` refuses on preflight's verdict, so an
# unstated blind spot in it is indistinguishable from a guarantee — and the setuid one has
# already cost another session a red gate that no tool here would have warned about.
check_that "even a passing preflight names what it cannot check" \
    "$(grep -q 'not checked here' <<<"$out" && grep -q 'setuid' <<<"$out" && echo yes || echo no)"
check_that "a passing preflight does not claim the machine is simply workable" \
    "$(grep -q 'no FILESYSTEM gaps' <<<"$out" && echo yes || echo no)"

# The DENY side must come from the posture, not from an assumption that it is $HOME. These two
# postures differ ONLY in denyRead and must disagree about the same paths; a preflight that
# hardcodes $HOME answers both identically and fails the second. Not hypothetical — the posture
# also denies /Volumes, /media, /mnt and /run/media, so a cargo home on an external volume, or
# anything under /mnt/c on WSL, was reported covered and install went ahead into the breakage.
nodeny_posture="$tmp/posture-nodeny.json"
jq '.require.sandbox.filesystem.allowRead = [] | .require.sandbox.filesystem.allowWrite = []
    | .require.sandbox.filesystem.denyRead = []' "$posture" > "$nodeny_posture"
alldeny_posture="$tmp/posture-alldeny.json"
jq '.require.sandbox.filesystem.allowRead = [] | .require.sandbox.filesystem.allowWrite = []
    | .require.sandbox.filesystem.denyRead = ["/"]' "$posture" > "$alldeny_posture"

# Matched with a regex spanning the colour reset, not the literal two spaces: the printf puts an
# ANSI escape between "GAP" and "readable", so `grep 'GAP  readable'` can never match and the
# assertion would hold whatever preflight printed.
run_preflight "$nodeny_posture"
check_that "a path under no denyRead root is not reported as a read gap" \
    "$(grep -q 'outside denyRead' <<<"$out" && ! grep -qE 'GAP.*readable' <<<"$out" && echo yes || echo no)"
run_preflight "$alldeny_posture"
check_that "the same path IS a read gap once the posture denies its root" \
    "$(grep -qE 'GAP.*readable' <<<"$out" && ! grep -q 'outside denyRead' <<<"$out" && echo yes || echo no)"

# The symlink case, which is the one that resolving both sides would hide: allow only the
# symlink spelling and require the tool to notice the real path is not listed.
if [ -L /tmp ] || [ "$(cd /tmp && pwd -P)" != "/tmp" ]; then
    sym_posture="$tmp/posture-sym.json"
    jq '.require.sandbox.filesystem.allowWrite = ["/tmp"] | .require.sandbox.filesystem.allowRead = ["/"]' \
       "$posture" > "$sym_posture"
    run_preflight "$sym_posture"
    check_that "preflight flags a symlinked path listed only by its link name" \
        "$([ "$rc" -ne 0 ] && grep -q 'covered only if the sandbox follows symlinks' <<<"$out" && echo yes || echo no)"
else
    echo "  skip  symlink case (/tmp is not a symlink on this platform)"
fi

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
