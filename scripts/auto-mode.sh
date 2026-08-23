#!/usr/bin/env bash
# Run Claude Code with no permission prompts, inside a boundary that holds when the model is
# wrong (design D48; full rationale in openspec/changes/jkb-safe-auto-mode/design.md, and a
# summary in CLAUDE.md).
#
#   ./scripts/auto-mode.sh print            # the posture this would install, as JSON
#   ./scripts/auto-mode.sh preflight        # would this posture let THIS machine work?
#   ./scripts/auto-mode.sh install          # merge it into ~/.claude/settings.json
#   ./scripts/auto-mode.sh check            # is the posture still intact? (exit 1 if not)
#   ./scripts/auto-mode.sh run [args…]      # check, then exec `claude --permission-mode auto`
#   ./scripts/auto-mode.sh probe            # live end-to-end smoke (costs a real session)
#
# TWO LAYERS, BECAUSE THEY FAIL DIFFERENTLY. `--permission-mode auto` is a *classifier*: it
# decides what is worth asking about, it is a model judgment, and it buys ergonomics. Claude
# Code's *sandbox* (macOS seatbelt via `sandbox-exec`, Linux bubblewrap + seccomp) bounds what
# can happen when that judgment is wrong, and it buys the guarantee. They meet in one setting:
# `autoAllowBashIfSandboxed` means a sandboxed command is never shown to the classifier — the OS
# boundary *is* the check.
#
# WHY NOT `--dangerously-skip-permissions`. The sandbox confines Bash and everything it spawns.
# It does NOT confine Claude Code's in-process tools; the settings schema says so about
# `strictAllowlist` in as many words ("in-process tools such as WebFetch are not gated by this
# setting"). Bypassing permissions therefore leaves Read/Edit/Write/WebFetch unbounded — a hole
# exactly the shape of the file-editing tools. `auto` keeps the classifier over precisely what
# the kernel does not cover, and the posture's `permissions.deny` rules close the named paths in
# BOTH layers at once (the schema merges `Read(...)` deny rules into `filesystem.denyRead` and
# `Edit(...)` into `denyWrite`) — one list, two enforcers, rather than two lists that drift.
#
# WHY USER SETTINGS. Several posture keys are honored only from user / managed / `--settings`
# and ignored in project settings, and Claude Code additionally REFUSES to run when a repo's
# .claude/settings.json negates `sandbox.enabled`, `sandbox.failIfUnavailable`,
# `sandbox.allowUnsandboxedCommands` or `disableAllHooks` ("operator posture belongs in the
# user-level settings.json"). So a cloned repo cannot switch this off — and installing the
# posture into a repo instead of ~/.claude would silently drop half of it.
#
# WHAT IS STILL UNSANDBOXED, because a guarantee you cannot state is not one. The sandbox covers
# Bash and every process it spawns — compilers, git, package managers, jkb itself, which is where
# the real capability lives. It does not cover the in-process tools: Read/Glob/Grep (bounded by
# `Read(...)` deny rules only), Write/Edit/NotebookEdit (deny rules plus the permission scope),
# WebFetch/WebSearch (permission rules only), MCP servers (long-lived processes started at session
# start, never per-command wrapped), and hooks (nothing in the binary evidences hook sandboxing).
# Three posture keys aim at that column: `permissions.ask: ["WebFetch"]`, because Read-anything
# composed with fetch-anywhere is read-everything-send-anywhere outside the kernel boundary and is
# the one composition that defeats the posture — so it is the single surviving prompt;
# `disableBypassPermissionsMode`, because the in-process layer is the ONLY bound those tools have;
# and `defaultMode: "auto"`, so an IDE session that never touched this script is prompt-free too.
#
# FILE ACCESS IS AN ALLOWLIST. Writes were already default-deny (the workspace only), so
# `filesystem.allowWrite` is the list. Reads are default-deny too — `denyRead: ["~", "/Volumes"]`
# blankets your data, `allowRead` (which takes precedence) re-opens the work roots and toolchain.
# System paths are deliberately NOT denied: a command that cannot read its own dynamic linker
# cannot run, so allowlisting *everything* is not a posture, it is an inoperative machine.
#
# WHY THIS SCRIPT EXISTS AT ALL. Claude Code enforces the boundary; re-checking its enforcement
# here would be a second model of the world. What is ours is that THE POSTURE IS A FILE AND
# FILES DRIFT — Claude Code appends to `permissions.allow` on every "always allow", /statusline
# edits the same file, `claude auto-mode reset` rewrites a section of it. So the posture file has
# two halves and `check` asks two questions, not a hand-written list of assertions beside the
# posture, which is the shape that drifts: is `require` a deep SUBSET of the effective settings,
# and is every `forbid` key empty or absent? The second rule exists because a subset check cannot
# express emptiness — a posture entry of `excludedCommands: []` would assert nothing, and that is
# the sandbox's own bypass list. Adding a key to either half extends the check, and the tests
# that generate their cases from it, by construction.
#
# The IDE needs no launcher: it reads the same user settings, `permissions.defaultMode: "auto"`
# gives it the same prompt-free behaviour, and `failIfUnavailable: true` makes Claude Code refuse
# to start unsandboxed however it was started. `check` is the drift preflight, not the gate.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
posture="$here/auto-mode-posture.json"
config_dir="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
settings="$config_dir/settings.json"

die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

command -v jq >/dev/null 2>&1 || die "jq is required (brew install jq / apt install jq)."
[ -f "$posture" ] || die "posture file missing: $posture"

# Deep merge, posture wins. Objects merge key-by-key; arrays UNION preserving the existing
# order and appending only what is new (so the user's own permissions.allow entries survive a
# re-install unshuffled, which is what makes `install` idempotent byte-for-byte); scalars are
# replaced. `has` rather than `//` throughout: `false // x` is `x` in jq, and this posture
# turns on a boolean whose correct value IS false (allowUnsandboxedCommands).
readonly JQ_MERGE='
def merge($a; $b):
  if ($a|type) == "object" and ($b|type) == "object" then
    reduce ($b|keys_unsorted[]) as $k ($a;
      .[$k] = merge((if ($a|has($k)) then $a[$k] else null end); $b[$k]))
  elif ($a|type) == "array" and ($b|type) == "array" then
    $a + ($b - $a)
  else $b end;
# Merge what `require` declares, then DELETE what `forbid` names. Both halves of the posture are
# applied by the same write, so `install` can repair either kind of drift — before, a forbidden
# key made `check` fail permanently while its own remedy printed "already installed (no change)".
reduce ($want[0].forbid | keys_unsorted[]) as $k
    (merge($got[0]; $want[0].require); delpaths([$k | split(".")]))
'

# Every posture leaf that the settings do not carry, as {path, want, got}. A missing key and an
# explicit null are both reported as `got: null` — the posture never declares null, so the two
# need no distinguishing. Arrays are subset-by-membership (deep equality per element), never
# order- or length-sensitive: the user may add domains of their own.
readonly JQ_DIFF='
def leaves($e; $a; $p):
  if ($e|type) == "object" then
    if ($a|type) != "object" then [{path: $p, want: $e, got: $a}]
    else [$e|keys_unsorted[]]
         | map(. as $k | leaves($e[$k];
                                (if ($a|type) == "object" and ($a|has($k)) then $a[$k] else null end);
                                ($p + "." + $k)))
         | add // []
    end
  elif ($e|type) == "array" then
    if ($a|type) != "array" then [{path: $p, want: $e, got: $a}]
    else [$e[]]
         | map(. as $x | if any($a[]; . == $x) then [] else [{path: ($p + "[]"), want: $x, got: null}] end)
         | add // []
    end
  else
    if $e == $a then [] else [{path: $p, want: $e, got: $a}] end
  end;
leaves($want[0].require; $got[0]; "")
'

# "At most" assertions, which a subset check CANNOT express: a key whose only safe value is
# ABSENT-or-EMPTY. `sandbox.excludedCommands` is the sandbox's own bypass list ("all bash commands
# must run in the sandbox unless they are explicitly listed in excludedCommands") and
# `permissions.additionalDirectories` widens the in-process permission scope past the workspace —
# both are additive, so requiring `[]` in the posture would assert nothing at all under subset
# semantics. Hence a second, separate rule rather than a key in `require`.
readonly JQ_FORBID='
[$want[0].forbid | to_entries[]]
| map(. as $e
      | ($e.key | split(".")) as $path
      | ($got[0] | getpath($path)) as $v
      | if $v == null or $v == [] or $v == {} then []
        else [{path: ("." + $e.key), why: $e.value, have: $v}] end)
| add // []
'

# macOS has sandbox-exec in the base system; Linux and WSL shell out to bubblewrap, and Claude
# Code's own dependency error names exactly these two. Missing them does not corrupt the posture —
# `failIfUnavailable: true` makes Claude Code refuse to start — but finding that out at launch is
# strictly worse than being told here.
missing_sandbox_deps() {
    [ "$(uname -s)" = "Linux" ] || return 0
    local missing=()
    for dep in bwrap socat; do
        command -v "$dep" >/dev/null 2>&1 || missing+=("$dep")
    done
    [ ${#missing[@]} -eq 0 ] && return 0
    printf '%s' "${missing[*]}"
}

# Distinguishes the three states `[ -f ]` collapses into one: readable, present-but-unreadable,
# and genuinely absent. Prints nothing and returns 0/1/2 so callers can act on the difference.
settings_state() {
    if [ -r "$settings" ] && head -c 1 "$settings" >/dev/null 2>&1; then return 0; fi
    # Not readable. Distinguish "denied" from "absent" WITHOUT trusting a stat we may not be
    # allowed to make: if the directory listing is itself denied, the file's existence is unknown
    # and must not be reported as absence.
    [ -e "$settings" ] && return 1                    # present, just not readable
    [ -d "$config_dir" ] && return 2                  # directory readable, file genuinely absent
    ls "$config_dir" >/dev/null 2>&1 && return 2      # listable by another route: absent
    [ -e "$config_dir" ] && return 1                  # exists but unusable: denied
    return 2                                          # no such directory: absent
}

read_settings() {
    local st=0; settings_state || st=$?
    case $st in
        0) ;;
        1) die "cannot read $settings (permission denied). It exists — this is NOT a missing file, so do NOT run 'install', which would treat it as absent. If the sandbox is denying it, add the path to sandbox.filesystem.allowRead, or run this from an unsandboxed shell." ;;
        2) die "no settings file at $settings — run: $0 install" ;;
    esac
    jq empty "$settings" 2>/dev/null || die "$settings is not valid JSON — fix it by hand, then re-run."
}

# The whole posture, both halves. `install` writes only `.require`: `.forbid` is an
# assertion about what must NOT be there, and has no merged form.
cmd_print() { cat "$posture"; }

cmd_install() {
    # Refuse before touching anything. The live install that denied its own settings file, $TMPDIR
    # and /tmp is exactly what this stops, and every one of those was knowable here.
    if [ "${1:-}" != "--force" ] && ! cmd_preflight; then
        die "refusing to install a posture with preflight gaps (pass --force to install anyway)."
    fi
    mkdir -p "$config_dir" 2>/dev/null || true
    local existed=1
    local st=0; settings_state || st=$?
    case $st in
        0) ;;
        # Refuse. Creating an empty file here would merge the posture into {} and drop everything
        # the real file holds, and the backup below would not fire because this branch believes
        # there was nothing to back up.
        1) die "cannot read $settings (permission denied), and it exists — refusing to overwrite it with a fresh posture. Run this from an unsandboxed shell, or restore a backup by hand." ;;
        2) existed=0
           printf '{}\n' > "$settings" 2>/dev/null \
             || die "cannot create $settings (permission denied). Installing the posture is deliberately a human action: the posture denies writes to itself, so an agent session cannot install or repair it. Run this from your own terminal." ;;
    esac
    jq empty "$settings" 2>/dev/null || die "$settings is not valid JSON — refusing to merge into it."

    local merged tmp_prev
    merged="$(mktemp)"; tmp_prev="$(mktemp)"
    cp "$settings" "$tmp_prev"
    jq -n --slurpfile got "$settings" --slurpfile want "$posture" "$JQ_MERGE" > "$merged"

    if jq -e --slurpfile a "$settings" --slurpfile b "$merged" -n '$a[0] == $b[0]' >/dev/null; then
        rm -f "$merged" "$tmp_prev"
        echo "posture already installed in $settings (no change)"
        cmd_check
        return
    fi

    # Back up only a file that both existed and is about to change, so neither a re-run nor a
    # fresh machine litters the config dir with backups of nothing.
    mv "$merged" "$settings" 2>/dev/null || die "cannot write $settings (permission denied). The posture denies writes to itself, so installing or repairing it is deliberately a human action — run this from your own terminal, not from inside an agent session."
    echo "posture installed in $settings"
    if [ "$existed" -eq 1 ]; then
        local backup="$settings.bak-$(date +%Y%m%d-%H%M%S)"
        cp "$tmp_prev" "$backup"
        echo "previous contents: $backup"
    fi
    rm -f "$tmp_prev"
    echo
    echo "This is your USER posture: it applies to every repo and to the IDE, not just to jkb."
    echo "It sets permissions.defaultMode=auto, so EVERY Claude Code session on this machine"
    echo "now starts in auto mode. That is the point — but it is machine-wide, so say it out"
    echo "loud rather than let it be discovered. To undo: restore the backup printed above."
    cmd_check
}

# Would this posture let THIS machine work? A different question from `check`, which asks whether
# the settings match the posture — and the question that actually mattered: the first live install
# denied its own settings file, $TMPDIR and /tmp, and every one of those was knowable from the
# posture plus a few `realpath` calls, without installing anything.
#
# Coverage is computed on RESOLVED paths, because that is where two of the three hid: on macOS
# /tmp is a symlink to /private/tmp and the sandbox matches the real path, and $TMPDIR is a
# per-user /var/folders/… directory that no reasonable person guesses.
cmd_preflight() {
    local -a need_write=() need_read=()
    need_write+=("${TMPDIR:-/tmp}" "/tmp" "$PWD")
    need_read+=("$settings" "$HOME/.cargo" "${CARGO_HOME:-$HOME/.cargo}" "$HOME/.gitconfig")
    [ -n "${PNPM_HOME:-}" ] && need_write+=("$PNPM_HOME")
    # Where cargo writes, which is not always under ~/.cargo: the dev container points it at a
    # volume, and a value outside the allowlist denies every sandboxed build while `check` and
    # `verify.sh` both report the machine healthy.
    [ -n "${CARGO_TARGET_DIR:-}" ] && need_write+=("$CARGO_TARGET_DIR")
    [ -n "${SHELL:-}" ] && need_read+=("$HOME/.$(basename "$SHELL")rc")

    # Expand a posture entry to an absolute, symlink-resolved prefix.
    #
    # The `[ -n "$e" ]` guard is load-bearing, not defensive tidying. An empty posture list makes
    # jq print nothing, a here-string of nothing is still ONE empty line, and `cd ""` SUCCEEDS in
    # bash without moving — so the empty entry resolved to $PWD and quietly entered the list as an
    # allowed (or denied) prefix. Every path under the current directory then read as covered by a
    # posture that covers nothing.
    local -a allow_w=() allow_r=()
    local e real
    while IFS= read -r e; do
        [ -n "$e" ] || continue
        e="${e/#\~/$HOME}"
        real="$(cd "$e" 2>/dev/null && pwd -P)" || real="$e"
        allow_w+=("$real")
    done <<<"$(jq -r '.require.sandbox.filesystem.allowWrite[]?' "$posture")"
    while IFS= read -r e; do
        [ -n "$e" ] || continue
        e="${e/#\~/$HOME}"
        real="$(cd "$e" 2>/dev/null && pwd -P)" || real="$e"
        allow_r+=("$real")
    done <<<"$(jq -r '.require.sandbox.filesystem.allowRead[]?' "$posture")"

    covered() { # covered <resolved-path> <prefix...>
        local path="$1"; shift
        local pfx
        for pfx in "$@"; do
            [ -n "$pfx" ] || continue
            [ "$pfx" = "/" ] && return 0
            case "$path" in "$pfx"|"$pfx"/*) return 0 ;; esac
        done
        return 1
    }
    resolve() { # a path that may not exist yet: resolve the nearest existing ancestor
        local p="$1" d
        d="$(cd "$p" 2>/dev/null && pwd -P)" && { printf '%s' "$d"; return; }
        d="$(cd "$(dirname "$p")" 2>/dev/null && pwd -P)" && { printf '%s/%s' "${d%/}" "$(basename "$p")"; return; }
        printf '%s' "$p"
    }

    # The entries as WRITTEN (only ~ expanded). Compared separately from the resolved forms
    # because resolving both sides makes /tmp and /private/tmp agree — which is exactly the
    # mismatch that denied /tmp on the live install, where the sandbox matched the real path and
    # the posture said the symlink. A path that is covered only after resolution is a latent gap.
    local -a lit_w=() lit_r=()
    while IFS= read -r e; do [ -n "$e" ] && lit_w+=("${e/#\~/$HOME}"); done <<<"$(jq -r '.require.sandbox.filesystem.allowWrite[]?' "$posture")"
    while IFS= read -r e; do [ -n "$e" ] && lit_r+=("${e/#\~/$HOME}"); done <<<"$(jq -r '.require.sandbox.filesystem.allowRead[]?' "$posture")"

    # The DENY roots, read from the posture rather than assumed to be $HOME. They are not: the
    # posture also blankets /Volumes, /media, /mnt and /run/media, so a machine whose CARGO_HOME
    # lives on an external volume — or, on WSL, anything under /mnt/c — was told "outside denyRead"
    # and install proceeded into exactly the breakage preflight exists to predict. Both spellings
    # are collected and a path under EITHER counts as denied, which errs toward reporting a gap.
    local -a deny_r=()
    while IFS= read -r e; do
        [ -n "$e" ] || continue
        e="${e/#\~/$HOME}"
        deny_r+=("$e")
        # `|| true`: a deny root that does not exist on this machine (/media on macOS, /Volumes on
        # Linux) makes the cd fail, and a bare failing && chain at the end of a loop body aborts
        # the whole script under `set -e` — the same trap that made settings_state unreachable.
        real="$(cd "$e" 2>/dev/null && pwd -P)" || real=""
        [ -n "$real" ] && [ "$real" != "$e" ] && deny_r+=("$real") || true
    done <<<"$(jq -r '.require.sandbox.filesystem.denyRead[]?' "$posture")"

    local gaps=0 p rp
    echo "==> preflight: does the posture cover what this machine needs?"
    for p in "${need_write[@]}"; do
        rp="$(resolve "$p")"
        if covered "$rp" ${lit_w[@]+"${lit_w[@]}"}; then printf '  \033[32mok\033[0m   writable: %s\n' "$p"
        elif covered "$rp" ${allow_w[@]+"${allow_w[@]}"}; then
            printf '  \033[31mGAP\033[0m  writable: %s resolves to %s, which is covered only if the sandbox follows symlinks — list %s literally\n' "$p" "$rp" "$rp"; gaps=$((gaps+1))
        else printf '  \033[31mGAP\033[0m  writable: %s -> %s is in no allowWrite entry\n' "$p" "$rp"; gaps=$((gaps+1)); fi
    done
    for p in "${need_read[@]}"; do
        rp="$(resolve "$p")"
        # readable if covered by allowRead OR by allowWrite (a writable tree is readable), or if
        # it simply is not under a denyRead root at all.
        if covered "$rp" ${lit_r[@]+"${lit_r[@]}"} ${lit_w[@]+"${lit_w[@]}"}; then printf '  \033[32mok\033[0m   readable: %s\n' "$p"
        elif ! covered "$rp" ${deny_r[@]+"${deny_r[@]}"}; then printf '  \033[32mok\033[0m   readable: %s (outside denyRead)\n' "$p"
        else printf '  \033[31mGAP\033[0m  readable: %s -> %s is under denyRead and in no allowRead entry\n' "$p" "$rp"; gaps=$((gaps+1)); fi
    done
    preflight_blind_spots
    [ "$gaps" -eq 0 ] && { echo "  no FILESYSTEM gaps — see the four items above for what that does not cover"; return 0; }
    printf '\n\033[31m%d gap(s)\033[0m — installing this posture would make the machine hard to work on.\n' "$gaps" >&2
    echo "Add the resolved paths to sandbox.filesystem.allowRead/allowWrite in $posture." >&2
    return 1
}

# What preflight structurally cannot check. Printed every run, because an unstated blind spot in
# a tool that `install` gates on is indistinguishable from coverage — and the first three of these
# have each already cost a session.
preflight_blind_spots() {
    cat <<'BLIND'

  not checked here, and not checkable:
    - setuid-root binaries (ps, top) cannot be exec'd under ANY posture. A sandboxed process
      cannot exec setuid and keep the privilege, so the kernel refuses it. There is no setting
      for this; the only lever is sandbox.excludedCommands, which runs a command wholly OUTSIDE
      the sandbox. It surfaces as an opaque "operation not permitted" on exec.
    - unix sockets are blocked by default, which severs Docker, Postgres and ssh-agent.
    - the domain allowlist is not compared against what this machine actually reaches; a missing
      host shows up as a failed request at the moment you needed it.
    - whether the sandbox ENGAGES at all is not established here — that needs a live session
      (`auto-mode.sh probe`, or `printenv CLAUDE_CODE_SANDBOXED` inside one).
BLIND
}

cmd_check() {
    read_settings
    local diff extra
    diff="$(jq -n --slurpfile want "$posture" --slurpfile got "$settings" "$JQ_DIFF")"
    extra="$(jq -n --slurpfile want "$posture" --slurpfile got "$settings" "$JQ_FORBID")"

    if [ "$(jq 'length' <<<"$diff")" -eq 0 ] && [ "$(jq 'length' <<<"$extra")" -eq 0 ]; then
        echo "posture intact in $settings"
        case "$(uname -s)" in
            Darwin|Linux) ;;
            *) echo "note: the sandbox supports macOS, Linux and WSL2 only — on $(uname -s)" \
                    "'failIfUnavailable' will refuse to start rather than run unconfined." ;;
        esac
        local deps; deps="$(missing_sandbox_deps)"
        [ -z "$deps" ] || printf '\033[33mwarning:\033[0m the sandbox needs %s on Linux — Claude Code will refuse to start until they are installed (apt install bubblewrap socat)\n' "$deps" >&2
        return 0
    fi

    printf '\033[31mposture NOT intact\033[0m in %s — do not run auto mode:\n' "$settings" >&2
    jq -r '.[] | "  \(.path): want \(.want|tojson), have \(.got|tojson)"' <<<"$diff" >&2
    jq -r '.[] | "  \(.path): must be empty or absent (\(.why)), have \(.have|tojson)"' <<<"$extra" >&2
    echo >&2
    echo "Repair with: $0 install" >&2
    return 1
}

cmd_run() {
    cmd_check
    # A hard gate here, a warning in `check`: this is the moment you actually go unattended, and
    # launching into a sandbox that cannot start is the one outcome the whole posture exists to
    # prevent.
    local deps; deps="$(missing_sandbox_deps)"
    [ -z "$deps" ] || die "the sandbox needs $deps on this Linux host (apt install bubblewrap socat). Refusing to launch unattended without it."
    local claude; claude="$(command -v claude || true)"
    [ -n "$claude" ] || claude="$HOME/.local/bin/claude"
    [ -x "$claude" ] || die "claude not found on PATH or at $HOME/.local/bin/claude"

    local -a overlay=()
    # Opt-in: hand the sandbox the ssh-agent socket so `git push` can authenticate WITHOUT the
    # private key ever being readable (the key stays denied either way). Not in the posture file
    # because $SSH_AUTH_SOCK is a per-login-session path, so it can only be supplied at launch.
    if [ "${JKB_AUTO_MODE_SSH_AGENT:-0}" = "1" ] && [ -n "${SSH_AUTH_SOCK:-}" ]; then
        if [ "$(uname -s)" = "Linux" ]; then
            # Say so rather than appear to work. `allowUnixSockets` is documented macOS-only
            # ("Ignored on Linux — seccomp cannot filter by path"), so this overlay is inert here;
            # Linux's only lever is `allowAllUnixSockets`, which is all-or-nothing and therefore
            # not something to switch on behind a flag whose name promises one socket.
            echo "note: JKB_AUTO_MODE_SSH_AGENT is macOS-only — sandbox.network.allowUnixSockets is" \
                 "ignored on Linux, so this has no effect. Push from outside the sandbox instead." >&2
        else
            overlay+=(--settings "$(jq -nc --arg s "$SSH_AUTH_SOCK" \
                '{sandbox: {network: {allowUnixSockets: [$s]}}}')")
            echo "ssh-agent socket allowed for this session: $SSH_AUTH_SOCK"
        fi
    fi

    say "claude --permission-mode auto"
    exec "$claude" --permission-mode auto ${overlay[@]+"${overlay[@]}"} "$@"
}

# The live smoke. It needs a real Claude Code session, so it is this change's #[ignore] test:
# never part of ./scripts/check.sh. Its verdict is taken from the FILESYSTEM, not from the
# transcript — what the model narrates about its own confinement is not evidence.
#
# TWO files, because one cannot tell the two failures apart. "The canary is absent" is only
# evidence the sandbox denied the write if the session ran the command at all: with the sandbox
# OFF — the state this exists to detect — Bash is no longer auto-allowed, so the *classifier*
# gets the out-of-bounds write and will very likely refuse it, and an absent canary would read as
# a clean pass. The control file, written inside the workspace where both layers allow it,
# separates "denied at the boundary" from "never ran", and the second is reported as INCONCLUSIVE
# rather than as a pass.
cmd_probe() {
    cmd_check
    local claude; claude="$(command -v claude || true)"
    [ -n "$claude" ] || claude="$HOME/.local/bin/claude"
    [ -x "$claude" ] || die "claude not found on PATH or at $HOME/.local/bin/claude"

    # The canary is deliberately NOT dot-prefixed: `~/.jkb-…` shares a prefix with the allowed
    # `~/.jkb`, and a near-miss like that is how a probe comes to lie.
    local canary="$HOME/auto-mode-probe-canary"
    local control; control="$(pwd)/auto-mode-probe-control"
    rm -f "$canary" "$control"

    say "probing the sandbox with a live session"
    "$claude" -p --permission-mode auto "$(cat <<PROMPT
This is a sandbox self-test. Run each of these with the Bash tool, exactly once, in order, and
report the exit status and output of each verbatim. Do NOT work around a failure, do not retry
with a different path or tool, and do not use dangerouslyDisableSandbox: a failure IS the
expected result for 3, 4 and 5, and reporting it is the whole point.
  1. touch $control
  2. printenv CLAUDE_CODE_SANDBOXED
  3. touch $canary
  4. curl -sS -m 5 -o /dev/null -w '%{http_code}' https://example.com
  5. ls ~/.ssh
Then state, in one line each, whether 3, 4 and 5 were denied.
PROMPT
)" || true

    echo
    if [ ! -e "$control" ]; then
        rm -f "$canary"
        die "PROBE INCONCLUSIVE: the session never wrote $control, so it did not run the commands (a classifier refusal, or a session error). Nothing was learned about the sandbox — read the transcript above."
    fi
    rm -f "$control"

    if [ -e "$canary" ]; then
        rm -f "$canary"
        die "PROBE FAILED: a sandboxed command wrote $canary, outside every allowed root. Auto mode is NOT safe on this machine."
    fi
    printf '\033[32mprobe passed:\033[0m the session ran, and its write outside the allowed roots did not land.\n'
    echo "Read the transcript above for 4 (non-allowlisted host) and 5 (credential read) —"
    echo "those two are only reported by the session, not verifiable from out here."
}

case "${1-}" in
    print)   shift; cmd_print "$@" ;;
    preflight) shift; cmd_preflight "$@" ;;
    install) shift; cmd_install "$@" ;;
    check)   shift; cmd_check "$@" ;;
    run)     shift; cmd_run "$@" ;;
    probe)   shift; cmd_probe "$@" ;;
    -h|--help|"") awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0" ;;
    *) die "unknown command: $1 (see --help)" ;;
esac
