#!/usr/bin/env bash
# Assert the container is what it claims to be (design D49). Run inside the container.
#
# A configuration nobody checks is a configuration nobody knows the state of, and every property
# here is one somebody could remove by editing a single line of devcontainer.json — a mount added,
# `remoteUser` dropped, the seccomp profile path typo'd (Docker fails loudly on a missing profile,
# but not on one that no longer contains what it should). Each assertion below fails for exactly
# one such edit.
set -uo pipefail
pass=0; fail=0
ok()  { pass=$((pass+1)); printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad() { fail=$((fail+1)); printf '  \033[31mFAIL\033[0m %s\n' "$1"; }
assert() { if [ "$2" = yes ]; then ok "$1"; else bad "$1"; fi; }

echo "==> container posture"

# 1. Non-root. Load-bearing, not hygiene: root in a container cannot create a mount namespace
#    directly even with seccomp relaxed, so bubblewrap fails and the nested sandbox with it.
assert "runs as a non-root user (uid $(id -u))" "$([ "$(id -u)" -ne 0 ] && echo yes || echo no)"

# 2. The nested sandbox's mechanism. This is the exact shape Claude Code invokes.
if bwrap --new-session --die-with-parent --bind / / --unshare-net /bin/true 2>/dev/null; then
    ok "bubblewrap can create its namespaces (nested sandbox can start)"; pass=$pass
else
    bad "bubblewrap cannot create namespaces — check the seccomp profile is applied"
fi

# 3. THE MOUNT BOUNDARY IS THE POINT, so assert it EXHAUSTIVELY rather than by listing paths
#    that ought to be absent. A list of absences can never be complete — it is the same
#    "enumerate the secrets" shape the host posture has to settle for because permission rules
#    cannot express default-deny. Here the kernel can, so ask it: every host path mounted in shows
#    up in /proc/self/mountinfo, and the set must be exactly what devcontainer.json declares.
#    A mount added in a hurry fails this line by name, including one nobody thought to forbid.
# EVERY mount point the kernel reports, minus the runtime's own fixed set — NOT the ones that
# happen to live under /home/vscode. Filtering by target prefix made this a list of absences with
# two entries: mounting /var/run/docker.sock (root on the host) or $HOME at /host passed cleanly.
EXPECTED="/home/vscode/repos/jkb
/home/vscode/.jkb
/home/vscode/.claude-state
/home/vscode/.cargo/target"
# What every container has regardless of configuration. Anything outside this and EXPECTED is
# something a human added to devcontainer.json and must be looked at.
RUNTIME_OWNED='^/$|^/proc|^/sys|^/dev|^/etc/hosts$|^/etc/hostname$|^/etc/resolv\.conf$|^/run/\.containerenv$|^/var/run/secrets'
actual="$(awk '{print $5}' /proc/self/mountinfo | sort -u | grep -Ev "$RUNTIME_OWNED" || true)"
unexpected="$(comm -23 <(printf '%s\n' "$actual") <(printf '%s\n' "$EXPECTED" | sort -u))"
if [ -z "$unexpected" ]; then
    ok "every mount point is declared or runtime-owned ($(printf '%s\n' "$actual" | grep -c . ) checked)"
else
    bad "UNDECLARED mounts: $(tr '\n' ' ' <<<"$unexpected")"
fi

#    ...and the one that would quietly undo the whole posture: ~/.claude must NOT be a host mount.
#    The container writes its own settings.json there (that is the posture, installed by setup.sh);
#    what must never appear is the HOST's, which the agent would then be able to read and which is
#    the file that decides whether it is sandboxed at all. Only the credential file is bind-mounted.
claude_mounts="$(awk '$5 == "/home/vscode/.claude" {print $5}' /proc/self/mountinfo)"
assert "~/.claude is the container's own, not a host mount" \
    "$([ -z "$claude_mounts" ] && echo yes || echo no)"

# 4. ...and these must be present, or the container is merely empty rather than confined.
assert "workspace is mounted"       "$([ -f /home/vscode/repos/jkb/Cargo.toml ] && echo yes || echo no)"
assert "knowledge base is mounted"  "$([ -d /home/vscode/.jkb ] && echo yes || echo no)"

# 5. Egress default-deny. Asserted in BOTH directions: a firewall that blocks everything passes a
#    one-sided test while having broken the container.
if curl -sS -m 6 -o /dev/null https://example.com 2>/dev/null; then
    bad "egress to a NON-allowlisted host was permitted (example.com)"
else
    ok "egress to a non-allowlisted host is refused"
fi
if curl -sS -m 15 -o /dev/null https://api.anthropic.com/ 2>/dev/null \
   || [ "$(curl -sS -m 15 -o /dev/null -w '%{http_code}' https://api.anthropic.com/ 2>/dev/null)" != "000" ]; then
    ok "egress to an allowlisted host still works"
else
    bad "egress to an allowlisted host is blocked — the firewall is too tight to work in"
fi

# 6. The inner posture. `check` is the drift rule from D48; here it also proves the posture
#    survived being installed into a fresh container HOME.
if /home/vscode/repos/jkb/scripts/auto-mode.sh check >/dev/null 2>&1; then
    ok "Claude Code posture is intact"
else
    bad "Claude Code posture is NOT intact (scripts/auto-mode.sh check)"
fi

echo
echo "  note: that the sandbox actually ENGAGES for a tool call is not asserted here — it needs a"
echo "  live session. Inside one, run:  printenv CLAUDE_CODE_SANDBOXED   (set == sandboxed), or"
echo "  ./scripts/auto-mode.sh probe   for the full write/egress/credential probe."
echo
if [ "$fail" -ne 0 ]; then printf '\033[31m%d failed\033[0m, %d passed\n' "$fail" "$pass"; exit 1; fi
printf '\033[32mall %d container checks passed\033[0m\n' "$pass"
