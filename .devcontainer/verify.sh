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

# `--declare <mount-point>`: a mount point the CALLER declares, for the one case devcontainer.json
# cannot express — a bind NESTED inside a declared target. `mutate-verify.sh` is that case: it
# spells its own docker flags, and it must mount the repo at /home/vscode/repos/jkb because in a
# `jkb task work` session the repo's parent directory is `.jkb/work`, not a repos dir, so mounting
# the parent would put the checkout at /home/vscode/repos/<session>.
#
# Nesting is NOT granted automatically, and that is the whole design decision here. A mount point
# and a mount SOURCE are independent: `-v ~/.ssh:/home/vscode/repos/jkb/secrets` sits inside a
# declared region and is still exfiltration. Nor can the source be checked from in here — on
# Docker Desktop for macOS /proc/self/mountinfo reports the path inside the VM, not the host path,
# which is why lib.sh's `dc_mount_sources` is used only by check-config.sh, on the host, where the
# sources are literal strings in the JSON.
#
# So the exception is NAMED rather than inferred, and it is bounded two ways. It only ADDS to the
# derived set, so it can never switch a check off; and it is refused unless the value is a strict
# descendant of a target devcontainer.json already declares, so `--declare /host`,
# `--declare /var/run/docker.sock` and `--declare /home/vscode/.claude/settings.json` — the exact
# mutations mutate-verify.sh exists to catch — cannot be waved through by it. The count is printed
# in the ok line below, because an override nobody can see is indistinguishable from a rule that
# does not exist (D38).
DECLARED_EXTRA=()
while [ $# -gt 0 ]; do
    case "$1" in
        --declare)   shift; [ $# -gt 0 ] || { echo "verify.sh: --declare needs a mount point" >&2; exit 2; }
                     DECLARED_EXTRA+=("$1"); shift ;;
        --declare=*) DECLARED_EXTRA+=("${1#--declare=}"); shift ;;
        *)           echo "usage: verify.sh [--declare <mount-point>]..." >&2; exit 2 ;;
    esac
done

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
#
# The expected set is DERIVED from devcontainer.json rather than transcribed beside it. A
# hand-kept copy went stale the first time the mounts changed: renaming the cargo target volume
# deleted the registry line with it, so a correctly-built container failed this very assertion
# after a full toolchain build. Two lists that must agree is the defect; there is now one list.
# Deriving does not weaken it — the question this asks is "does the running container match what
# it declares", and editing the declaration changes nothing until a human rebuilds.
# Derived through .devcontainer/lib.sh, the SAME function check-config.sh uses on the host, so
# the gate that reviews the boundary and the check that enforces it cannot read it differently.
here_dc="$(cd "$(dirname "$0")" && pwd)"
DC="$here_dc/devcontainer.json"
# The checkout being verified: the one this script is in. Every assertion below that used to name
# /home/vscode/repos/jkb reads this instead — with all of ~/repos mounted, that literal is a
# statement about whichever repo happens to sit there, which is not necessarily this one.
mem_repo="$(cd "$here_dc/.." && pwd)"
# shellcheck source=/dev/null
. "$here_dc/lib.sh"
EXPECTED="$(dc_mount_targets "$DC")"
# What every container has regardless of configuration. Anything outside this and EXPECTED is
# something a human added to devcontainer.json and must be looked at.
RUNTIME_OWNED='^/$|^/proc|^/sys|^/dev|^/etc/hosts$|^/etc/hostname$|^/etc/resolv\.conf$|^/run/\.containerenv$|^/var/run/secrets'
actual="$(awk '{print $5}' /proc/self/mountinfo | sort -u | grep -Ev "$RUNTIME_OWNED" || true)"
# A failed derivation would make every real mount look undeclared — a true failure, but reported
# as the wrong thing. Say which it is, so the fix is not looked for in devcontainer.json's mounts.
#
# The caller's own declarations are folded in HERE, after the derivation and never instead of it,
# and each is refused unless it is strictly inside something already derived. `$d/*` is a strict
# descendant test on purpose: `--declare` cannot restate a declared target (which would be a
# no-op) and cannot name one of its ancestors (which would widen the set upwards).
extra=0
for t in ${DECLARED_EXTRA[@]+"${DECLARED_EXTRA[@]}"}; do
    nested=no
    while IFS= read -r d; do
        [ -n "$d" ] || continue
        case "$t" in "$d"/?*) ;; *) continue ;; esac
        # ...and only inside a BIND. A named volume is container-managed and reaches no host
        # filesystem, which is exactly why check-config.sh reviews bind sources and waves volumes
        # through — so "inside a declared region" cannot mean inside a volume: a bind nested under
        # ~/.cargo/target would be a host mount at a place nobody reviewed. `dc_type_for_target`
        # is lib.sh's own answer to that question, used here rather than derived a second way.
        [ "$(dc_type_for_target "$DC" "$d")" = bind ] || continue
        nested=yes; break
    done <<<"$EXPECTED"
    if [ "$nested" = yes ]; then
        EXPECTED="$(printf '%s\n%s\n' "$EXPECTED" "$t" | sort -u)"
        extra=$((extra+1))
    else
        bad "--declare $t is not inside any host BIND $DC declares — only a mount nested in a declared bind may be named here (a named volume reaches no host filesystem, so nothing under one is reviewable this way)"
    fi
done
if [ -z "$EXPECTED" ]; then
    bad "could not derive the declared mounts from $DC — the check below cannot mean anything"
else
    unexpected="$(comm -23 <(printf '%s\n' "$actual") <(printf '%s\n' "$EXPECTED"))"
    if [ -z "$unexpected" ]; then
        ok "every mount point is declared or runtime-owned ($(printf '%s\n' "$actual" | grep -c . ) checked against $(printf '%s\n' "$EXPECTED" | grep -c . ) declared$([ "$extra" -gt 0 ] && printf ', %s of them nested and named with --declare' "$extra"))"
    else
        bad "UNDECLARED mounts: $(tr '\n' ' ' <<<"$unexpected")"
    fi
fi

#    ...and the one that would quietly undo the whole posture: ~/.claude must NOT be a host mount.
#    The container writes its own settings.json there (that is the posture, installed by setup.sh);
#    what must never appear is the HOST's, which the agent would then be able to read and which is
#    the file that decides whether it is sandboxed at all. Nothing under ~/.claude is mounted from
#    the host at all — not even the credential file, which is why you log in inside the container.
#    Matched as a PREFIX, not for equality: a bind at ~/.claude/settings.json is the posture file
#    itself and an equality test waves it through, which is the one mount that matters most here.
claude_mounts="$(awk '$5 == "/home/vscode/.claude" || index($5, "/home/vscode/.claude/") == 1 {print $5}' /proc/self/mountinfo)"
assert "nothing under ~/.claude is a host mount${claude_mounts:+ (found: $(tr '\n' ' ' <<<"$claude_mounts"))}" \
    "$([ -z "$claude_mounts" ] && echo yes || echo no)"

# 3b. ROOT IS REACHABLE OR IT IS NOT, and everything above depends on which. The mount boundary,
#     the root-owned firewall, its allowlist snapshot and the pinned sudoers argument are all
#     protections against a process that cannot become root — and the devcontainers base image
#     ships /etc/sudoers.d/vscode granting `NOPASSWD:ALL`, so until that is removed the agent can
#     undo every one of them with a single sudo. Asked of sudo itself rather than of the file,
#     because what matters is the policy in force: every command vscode may run as root must be
#     the firewall. A blanket grant re-added by any route fails here.
#     The grant is only as good as the file it names. Replacing the script, or the snapshot beside
#     it, is a root shell by the sudoers entry's own permission — and unlink/replace is governed by
#     the containing DIRECTORY, so the directories are asserted, not just the files.
unwritable_ok=1
# /usr/local is in the list because it governs REPLACING bin and share: `mv /usr/local/bin aside
# && mkdir /usr/local/bin && cp evil .../init-firewall.sh` needs write on the PARENT, not on the
# two directories, and every path below would still test unwritable while the sudoers entry ran the
# agent's script as root. Debian ships /usr/local as root:staff drwxrwsr-x, so this is one group
# membership away on a floating base tag.
for path in /usr/local /usr/local/bin /usr/local/bin/init-firewall.sh /usr/local/share \
            /usr/local/share/jkb-egress-allowlist.json /etc/sudoers.d; do
    # A path that does not exist yet (the snapshot, before the first raise) cannot be replaced
    # either, so absence is fine; what must never be true is that it exists AND is writable.
    if [ -e "$path" ] && [ -w "$path" ]; then
        bad "$path is writable by $(id -un) — the firewall sudo runs as root can be replaced"
        unwritable_ok=0
    fi
done
[ "$unwritable_ok" -eq 1 ] && ok "the root-owned firewall and its allowlist cannot be replaced from here"

#     `sudo -n -l` failing and `sudo -n -l` listing nothing are different facts, and collapsing
#     them reported the friendliest one: sudo missing, PAM broken or sudoers unparseable all
#     produced an empty list and read as "no passwordless root". Worse, an unparseable sudoers is
#     a state in which the FIREWALL cannot run either, so silence there is the wrong direction.
sudo_raw="$(sudo -n -l 2>&1)"; sudo_rc=$?
sudo_entries="$(grep -E '^[[:space:]]*\(' <<<"$sudo_raw" || true)"
if [ "$sudo_rc" -ne 0 ] && [ -z "$sudo_entries" ]; then
    # `sudo -n -l` exits non-zero when the user may run NOTHING, which is a legitimate hardened
    # state — but only if sudo is actually working. Tell that apart from a broken sudo.
    if grep -qiE 'not allowed to run sudo|may not run sudo' <<<"$sudo_raw"; then
        ok "sudo works and grants this user nothing at all"
    else
        bad "sudo -n -l failed for a reason that is not 'nothing granted' (rc $sudo_rc): $(head -1 <<<"$sudo_raw") — the firewall cannot run either"
    fi
elif [ -z "$sudo_entries" ]; then
    bad "sudo -n -l succeeded but listed no grants — cannot establish what this user may run as root"
elif [ -z "$(grep -v '/usr/local/bin/init-firewall.sh' <<<"$sudo_entries")" ]; then
    ok "the only command permitted as root is the firewall ($(grep -c . <<<"$sudo_entries") grant(s))"
else
    bad "vscode may run more than the firewall as root: $(grep -v '/usr/local/bin/init-firewall.sh' <<<"$sudo_entries" | tr -s ' ' | tr '\n' ';')"
fi

# 3c. The login-state links. devcontainer.json and the README both promise a login survives a
#     rebuild, and that promise is entirely these two symlinks — without them the credentials sit
#     in the writable layer and go with it. A promise made in two documents and checked nowhere is
#     the shape this change keeps finding, so it is checked here.
links_ok=1
#     Every link, not only the two whole-file ones. A real directory at a link site is silently
#     declined by `ln -sfn`, so asserting the files alone left seven ways for state to stay in the
#     container layer and vanish on rebuild with nothing reporting it.
for d in projects sessions history file-history shell-snapshots todos statsig; do
    if [ ! -L "/home/vscode/.claude/$d" ]; then
        bad "~/.claude/$d is not a link into the state volume — that state would not survive a rebuild"
        links_ok=0
    fi
done
for pair in "/home/vscode/.claude/.credentials.json:/home/vscode/.claude-state/.credentials.json" \
            "/home/vscode/.claude.json:/home/vscode/.claude-state/claude.json"; do
    link="${pair%%:*}"; want="${pair##*:}"
    if [ ! -L "$link" ]; then
        # A regular file here means the login is NOT persisted — the exact failure, not cosmetic.
        bad "$link is $( [ -e "$link" ] && echo "a regular file" || echo "missing" ), not a link into the state volume — a login here would not survive a rebuild"
        links_ok=0
    elif [ "$(readlink "$link")" != "$want" ]; then
        bad "$link points at $(readlink "$link"), not $want"
        links_ok=0
    fi
done
[ "$links_ok" -eq 1 ] && ok "login state is linked into the persistent volume"

# 3d. Auto-memory reaches the host, and the state volume does NOT give you this: Claude Code keys
#     memory by the project's ABSOLUTE PATH, so this container's /home/vscode/repos/jkb is a
#     different key from the host's, and widening the workspace mount does not change that. The
#     shared store lives inside the ~/.jkb bind that already exists, so nothing under ~/.claude is
#     mounted and the assertion above still holds. Asserted here for the reason 3c is: it is a
#     promise the README makes, and a promise checked nowhere is how this one would rot.
#     The slug comes from the linking script itself rather than being spelled again here — one
#     guess about another program's private encoding is enough.
#     ASKED OF THE LINKER, not inferred from the link's absence. The linker deliberately leaves
#     the link absent in states it recognises — a name that exists on both sides is a collision it
#     refuses to resolve, and a store holding a symlink is one it refuses to follow — so reading
#     "no link" as "the mechanism is broken" made a state its own design calls normal fail this
#     check, and with it postCreate, and with that the container. One rule, stated in one place.
mem_state="$("$mem_repo/scripts/link-claude-memory.sh" --status "$mem_repo" 2>/dev/null)"
case "$mem_state" in
    linked)
        ok "auto-memory is shared with the host through ~/.jkb" ;;
    collision|unsafe|foreign)
        # Not a FAIL: the container works, one repo's memory just is not shared until a person
        # settles it. Said plainly, because a state nobody is told about is one nobody fixes.
        ok "auto-memory is not shared for $(basename "$mem_repo") ($mem_state) — run scripts/link-claude-memory.sh to see what it wants"
        ;;
    unlinked|"")
        # FATAL, and setup.sh's comment now says the same. `unlinked` is not the linker declining
        # — a collision and a poisoned store have their own words above and pass — it means the
        # linker did not run or could not answer, and the README promises this works. The two
        # files used to state opposite rules about the same state.
        bad "auto-memory is not linked into the shared store (state: ${mem_state:-unknown}) — the linker did not run or could not answer; memory written in here would be invisible on the host" ;;
    *)
        bad "scripts/link-claude-memory.sh --status answered '$mem_state', which this check does not recognise" ;;
esac

# 4. ...and these must be present, or the container is merely empty rather than confined.
# THE MOUNT, asked of the kernel. A hard-coded /home/vscode/repos/jkb asserted something about a
# different checkout once ~/repos is mounted whole; deriving it from where this script lives went
# too far the other way and asserted that the directory containing the running script contains a
# Cargo.toml — true by construction on every path that can execute this, so the property was
# asserted nowhere. What must hold is that the declared workspace target is really a mount point
# and this checkout is inside it.
ws_target="$(dc_mount_targets "$DC" | grep -x '/home/vscode/repos' || true)"
ws_mounted=no
if [ -n "$ws_target" ] && printf '%s\n' "$actual" | grep -qx "$ws_target"; then
    case "$mem_repo" in "$ws_target"/?*) ws_mounted=yes ;; *) ;; esac
fi
assert "the workspace bind is mounted and $mem_repo is inside it" "$ws_mounted"
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
if "$mem_repo/scripts/auto-mode.sh" check >/dev/null 2>&1; then
    ok "Claude Code posture is intact"
else
    bad "Claude Code posture is NOT intact (scripts/auto-mode.sh check)"
fi

echo
echo "  note: that the sandbox actually ENGAGES for a tool call is not asserted here — it needs a"
echo "  live session. Inside one, run:  ./scripts/auto-mode.sh sandboxed   (control + canary, no"
echo "  cost; do NOT use printenv CLAUDE_CODE_SANDBOXED, which was unset on a host whose sandbox"
echo "  was provably enforcing), or"
echo "  ./scripts/auto-mode.sh probe   for the full write/egress/credential probe."
echo
if [ "$fail" -ne 0 ]; then printf '\033[31m%d failed\033[0m, %d passed\n' "$fail" "$pass"; exit 1; fi
printf '\033[32mall %d container checks passed\033[0m\n' "$pass"
