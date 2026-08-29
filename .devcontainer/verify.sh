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
# descendant of a target devcontainer.json declares AS A BIND — not a volume, which reaches no
# host filesystem and is therefore a region nobody reviewed sources for — so `--declare /host`,
# `--declare /var/run/docker.sock` and `--declare /home/vscode/.claude/settings.json` — the exact
# mutations mutate-verify.sh exists to catch — cannot be waved through by it. The count is printed
# in the ok line below, because an override nobody can see is indistinguishable from a rule that
# does not exist (D38).
DECLARED_EXTRA=()
SELF_TEST=no
while [ $# -gt 0 ]; do
    case "$1" in
        --declare)   shift; [ $# -gt 0 ] || { echo "verify.sh: --declare needs a mount point" >&2; exit 2; }
                     DECLARED_EXTRA+=("$1"); shift ;;
        --declare=*) DECLARED_EXTRA+=("${1#--declare=}"); shift ;;
        --self-test) SELF_TEST=yes; shift ;;
        *)           echo "usage: verify.sh [--declare <mount-point>]... | --self-test" >&2; exit 2 ;;
    esac
done

# What every container has regardless of configuration. Anything outside this and EXPECTED is
# something a human added to devcontainer.json and must be looked at.
#
# Anchored at a component boundary — `(/|$)` — not as bare prefixes. `^/dev` also matched
# `/devtools`, `^/proc` matched `/procdata` and `^/sys` matched `/sysroot`, so a host mount at any
# of those was silently dropped from the set this check calls exhaustive. A list of exclusions
# that quietly grows is the shape this assertion exists to avoid having.
#
# `/vscode` is the one entry here the CONTAINER RUNTIME does not own: the VS Code Dev Containers
# extension mounts a named volume there to hold the server, unpacks it, and symlinks
# ~/.vscode-server/bin at it — all before postCreate runs. It is added by the launcher's own
# docker flags, so it is expressible neither in devcontainer.json (which would be a false claim:
# we do not create it, and under `devcontainer up` or a plain docker run it is simply absent) nor
# as `--declare` (which is refused for anything not nested inside a declared bind). Without it
# `Reopen in Container` failed postCreate outright on `UNDECLARED mounts: /vscode`, which is why
# the supported path was the one path nothing had exercised.
#
# Anchored `^/vscode$`, deliberately not `(/|$)`: only the mount point itself is the launcher's.
# A bind at /vscode/anything is somebody putting a host path inside it, and must still fail.
# The cost, stated rather than hidden: from in here a volume and a bind are indistinguishable, so
# this one path is a spot where a host bind would pass. Bounded to one path, and it is not the
# threat this check is for — a careless line in devcontainer.json is, and that is still covered.
RUNTIME_OWNED='^/$|^/proc(/|$)|^/sys(/|$)|^/dev(/|$)|^/etc/hosts$|^/etc/hostname$|^/etc/resolv\.conf$|^/run/\.containerenv$|^/var/run/secrets(/|$)|^/vscode$'

# Which declared extension ids are absent from a `code-server --list-extensions` listing.
#
# Pure, and separated from the call for one reason: in EVERY harness this repo has there is no VS
# Code server — `devcontainer up`, a plain docker run and mutate-verify.sh all build a correct
# container with no VS Code in it — so the only arm of assertion 7 that can run there is the skip.
# Left inline, its FAIL arm would be unreachable code wearing the costume of a guard, in a change
# whose entire subject is a check that could not fire.
#
# Case-insensitive: the marketplace treats extension ids that way, and `--list-extensions` prints
# the publisher's own casing rather than the casing devcontainer.json happens to use.
missing_extensions() { # missing_extensions <declared, one per line> <installed, one per line>
    local declared="$1" installed="$2" ext id out=""
    while read -r ext; do
        [ -n "$ext" ] || continue
        id="${ext%@*}"
        printf '%s\n' "$installed" | grep -qiFx "$id" || out="$out $id"
    done <<<"$declared"
    printf '%s' "$out"
}

# --self-test: the exclusion list, exercised with no container. Run by ./scripts/check.sh.
#
# It is the one part of the mount boundary that widens by a TYPO rather than by an edit anyone
# reviews: every entry is an exclusion, so a pattern matching more than it names silently stops
# the exhaustive check from being exhaustive, and the assertion still prints `ok`. That is not
# hypothetical — the `^/dev` / `/devtools` class above is the same file's own history.
# mutate-verify.sh would catch it, but mutate-verify.sh needs a Docker host and is not in the
# gate. This is, and it costs nothing.
if [ "$SELF_TEST" = yes ]; then
    st_fail=0
    st() { # st <mount point> <owned|checked>
        if printf '%s' "$1" | grep -Eq "$RUNTIME_OWNED"; then got=owned; else got=checked; fi
        if [ "$got" = "$2" ]; then printf '  \033[32mok\033[0m   %-24s %s\n' "$1" "$2"
        else printf '  \033[31mFAIL\033[0m %-24s is %s, wanted %s\n' "$1" "$got" "$2"; st_fail=$((st_fail+1)); fi
    }
    echo "==> verify.sh self-test: RUNTIME_OWNED"
    for p in / /proc /proc/sys /sys /sys/fs/cgroup /dev /dev/shm /dev/pts \
             /etc/hosts /etc/hostname /etc/resolv.conf /run/.containerenv \
             /var/run/secrets /var/run/secrets/kubernetes.io /vscode; do
        st "$p" owned
    done
    # Every one of these is a real mount point somebody could add, and each is a near-miss for an
    # entry above. A regex that swallows one of them is a hole with no symptom.
    for p in /vscodex /vscode-evil /vscode/secrets /devtools /procdata /sysroot /etchosts \
             /etc/hostnamex /etc/resolv.conf.bak /run/containerenv /var/run/secretsx \
             /home/vscode/repos /home/vscode/.jkb /host; do
        st "$p" checked
    done

    # Assertion 7's judgement, whose FAIL arm no container harness can reach (see
    # missing_extensions). `st` compares strings, so these read as the mount cases do.
    echo "==> verify.sh self-test: missing_extensions"
    declared="$(printf 'rust-lang.rust-analyzer@0.3.3025\nanthropic.claude-code@2.1.250\n')"
    st2() { # st2 <label> <got> <want>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else printf '  \033[31mFAIL\033[0m %s\n         got:  [%s]\n         want: [%s]\n' "$1" "$2" "$3"; st_fail=$((st_fail+1)); fi
    }
    st2 "both installed is nothing missing" \
        "$(missing_extensions "$declared" "$(printf 'rust-lang.rust-analyzer\nanthropic.claude-code\n')")" ""
    st2 "the one this container exists for, absent, is named" \
        "$(missing_extensions "$declared" "rust-lang.rust-analyzer")" " anthropic.claude-code"
    st2 "an empty listing names every declared one" \
        "$(missing_extensions "$declared" "")" " rust-lang.rust-analyzer anthropic.claude-code"
    # The version suffix is ours, not the listing's: `--list-extensions` prints bare ids, so
    # comparing the pinned string would report every extension missing, always.
    st2 "the @version pin is stripped before comparing" \
        "$(missing_extensions "anthropic.claude-code@2.1.250" "anthropic.claude-code")" ""
    st2 "publisher casing does not decide the answer" \
        "$(missing_extensions "$declared" "$(printf 'rust-lang.rust-analyzer\nAnthropic.Claude-Code\n')")" ""
    # A DIFFERENT extension whose id contains this one must not satisfy it. The direction matters
    # and the first version of this case had it backwards — it asked whether a listing SHORTER
    # than the id matched, which no grep would have said yes to, so it passed with the anchoring
    # deleted. Caught by mutating the guard rather than by reading it.
    st2 "a longer id containing this one is not a match" \
        "$(missing_extensions "anthropic.claude-code@2.1.250" "anthropic.claude-code-preview")" " anthropic.claude-code"
    # The dot in an extension id is a literal. Unfixed, `anthropic.claude-code` is a pattern whose
    # `.` matches any character, so an unrelated id differing only there would read as installed.
    st2 "the dot in an id is literal, not a wildcard" \
        "$(missing_extensions "anthropic.claude-code@2.1.250" "anthropicXclaude-code")" " anthropic.claude-code"

    echo
    [ "$st_fail" -eq 0 ] || { printf '\033[31m%d failed\033[0m\n' "$st_fail"; exit 1; }
    printf '\033[32mverify.sh self-test passed\033[0m\n'
    exit 0
fi

# INSIDE THE CONTAINER, OR NOT AT ALL. Every assertion below is about a Linux container's kernel
# state, and run anywhere else they do not fail — they answer about a machine that was never the
# subject. On the macOS host this printed fourteen confident FAILs (no `~/.claude` links, sudo
# wants a password, the knowledge base "not mounted") and, worse, two `ok` lines: `/proc/self/
# mountinfo` does not exist there, so the mount-boundary check compared an EMPTY set and passed.
# A report an operator could act on, about nothing.
#
# The mount table is the load-bearing input rather than a proxy for the platform: if it cannot be
# read, the one assertion this file exists for cannot mean anything, whatever else is true.
if [ ! -r /proc/self/mountinfo ]; then
    echo "verify.sh asserts what a running container is, and must run INSIDE one." >&2
    echo "  /proc/self/mountinfo is not readable here, so the mount boundary — the assertion this" >&2
    echo "  file exists for — could not be checked at all." >&2
    echo >&2
    echo "  In VS Code:  Reopen in Container, then ./.devcontainer/verify.sh" >&2
    echo "  With Docker: ./.devcontainer/mutate-verify.sh --control   (one healthy run)" >&2
    echo "               ./.devcontainer/mutate-verify.sh             (every guard, watched failing)" >&2
    echo >&2
    echo "  Do NOT hand-roll the docker run: it needs the seccomp profile, NET_ADMIN, both binds" >&2
    echo "  and a preamble that raises the firewall and installs the posture. A command missing" >&2
    echo "  any of those prints a dozen FAILs that read as a broken container." >&2
    exit 2
fi

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
# RUNTIME_OWNED — the exclusion list — is defined at the top of this file, above the
# inside-the-container refusal, so `--self-test` can exercise it on a host with no Docker.
# The READ is kept separate from the result, because "there were no mounts" and "the table could
# not be read" are different facts and the second must never be spelled like the first. `EXPECTED`
# has been guarded that way since it was derived; `actual` was not, so an unreadable table made
# the boundary check pass having compared nothing.
if mountinfo="$(cat /proc/self/mountinfo 2>/dev/null)" && [ -n "$mountinfo" ]; then
    mounts_readable=yes
    actual="$(awk '{print $5}' <<<"$mountinfo" | sort -u | grep -Ev "$RUNTIME_OWNED" || true)"
else
    mounts_readable=no
    actual=""
fi
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
if [ "$mounts_readable" != yes ]; then
    bad "could not read /proc/self/mountinfo — the mount boundary was not checked, which is not the same as finding it clean"
elif [ -z "$EXPECTED" ]; then
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
# From the table read ONCE and guarded above, not a second open: re-reading it here meant an
# unreadable mountinfo printed `ok  nothing under ~/.claude is a host mount` having compared
# nothing — the same defect fixed one assertion over, still live in its neighbour.
claude_mounts="$(awk '$5 == "/home/vscode/.claude" || index($5, "/home/vscode/.claude/") == 1 {print $5}' <<<"$mountinfo")"
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
# ASKED LIVE, EVERY TIME — and the create-time record is an ADDITIONAL alarm, never a substitute.
#
# It used to be the other way round, and that made the store guard unfirable after `postCreate`:
# the record says `linked`, so a redirect planted in the store the next day — the only time that
# state can arise, and exactly the sequence `link_one`'s comment describes — reported `ok`. The
# mirror case was a false FAIL that could not be cleared: after an `exposed` create, the printed
# remedy is a hand re-run of the linker, which writes no status file, so verify.sh kept reading
# `exposed` until a full rebuild.
#
# The record still earns its place: `link_one` REPAIRS, removing a live link into a poisoned store,
# so a live question asked afterwards sees the harmless `unsafe` and not the `exposed` that was
# true at create. So both are consulted, the worse one decides, and an `exposed` record is
# consumed once reported — which is what makes the documented remedy actually clear it.
mem_key="$(basename "$mem_repo")"
mem_status_file=/home/vscode/.claude-state/memory-status
mem_live="$("$mem_repo/scripts/link-claude-memory.sh" --status "$mem_repo" 2>/dev/null)"
mem_recorded="$(awk -v k="$mem_key" '$1 == k { print $2 }' "$mem_status_file" 2>/dev/null | tail -1)"
# The two states ONLY THE RUN can know, so only the record can carry them. `exposed` because the
# repair clears its own alarm; `error` because it means the run stopped part-way — a migration that
# moved some files and then failed leaves a directory the live observer reads as an ordinary
# `unlinked`, and this check then blamed "the linker did not run", the one cause it is not. The
# `error)` arm below was unreachable until now: `status_of` never emits that word.
case "$mem_recorded" in
    exposed|error)
        mem_state="$mem_recorded"
        # Consumed: the alarm describes one create, and leaving it makes the remedy — a hand
        # re-run, which writes no status file — unable to clear it.
        if [ -w "$mem_status_file" ]; then
            awk -v k="$mem_key" '$1 != k' "$mem_status_file" > "$mem_status_file.new" 2>/dev/null \
                && mv "$mem_status_file.new" "$mem_status_file" 2>/dev/null || true
        fi ;;
    *)  mem_state="$mem_live" ;;
esac
case "$mem_state" in
    linked)
        ok "auto-memory is shared with the host through ~/.jkb" ;;
    exposed)
        # A live redirect out of the store, which the linker has just broken. FATAL: the container
        # is not in a state its own design permits, and saying so on the create that discovers it
        # is the only moment anyone reads this.
        bad "auto-memory was pointing into a store holding something that is not a plain file — the link has been removed; clean ~/.jkb/claude-memory and re-run scripts/link-claude-memory.sh" ;;
    collision|unsafe|foreign)
        # Not a FAIL: the container works, one repo's memory just is not shared until a person
        # settles it. Said plainly, because a state nobody is told about is one nobody fixes.
        ok "auto-memory is not shared for $(basename "$mem_repo") ($mem_state) — run scripts/link-claude-memory.sh to see what it wants"
        ;;
    broken)
        # The link is live and points at a store directory that is GONE, so every memory write
        # into it fails. Not one of the declining states: nothing here wants a decision, the
        # linker recreates the directory. FATAL because until it is re-run, memory is silently
        # not being recorded — the failure mode with no symptom.
        bad "auto-memory points at a store directory that no longer exists — re-run scripts/link-claude-memory.sh" ;;
    error)
        # The linker ran and could not finish for this repo — from the RECORD, since a live
        # observation cannot see a run that stopped part-way. Distinct from `unlinked`, which is
        # "no link and nothing recorded", and from the declining states above, which want a person.
        bad "auto-memory could not be linked (state: error) — see scripts/link-claude-memory.sh's output in the create log" ;;
    unlinked|"")
        # FATAL, and setup.sh's comment now says the same. `unlinked` is not the linker declining
        # — a collision and a poisoned store have their own words above and pass — it means the
        # linker did not run or could not answer, and the README promises this works. The two
        # files used to state opposite rules about the same state.
        # Says what was OBSERVED, not why. The previous wording asserted "the linker did not run",
        # which this cannot establish: a hand re-run that fails part-way writes no status file and
        # lands here too.
        bad "auto-memory is not linked into the shared store (state: ${mem_state:-unknown}) — memory written in here would be invisible on the host; run scripts/link-claude-memory.sh and read what it says" ;;
    *)
        bad "scripts/link-claude-memory.sh --status answered '$mem_state', which this check does not recognise" ;;
esac

# 4. ...and these must be present, or the container is merely empty rather than confined.
# THE MOUNT, asked of the kernel — and asked about THIS checkout rather than about a path.
#
# Three versions of this, and the middle two are why the wording matters. A hard-coded
# /home/vscode/repos/jkb asserted something about whichever checkout sat there once ~/repos is
# mounted whole. Deriving it from where this script lives went too far the other way and asserted
# that the directory containing the running script contains a Cargo.toml — true by construction on
# every path that can execute this. Then requiring the DECLARED target to itself appear in
# mountinfo failed the harness outright: mutate-verify.sh mounts the repo AT
# /home/vscode/repos/jkb (it cannot mount the parent — in a `jkb task work` session $REPO's parent
# is `.jkb/work`), so /home/vscode/repos is never a mount point there. Measured: every mutation
# reported CAUGHT and then the control failed, so the harness judged nothing.
#
# What actually has to hold is that this checkout sits inside a mount point that is BOTH really
# mounted and declared. Both halves carry weight: mounted, so a repo baked into the image layer
# with the bind dropped fails rather than passing as "confined"; declared, so it is a mount the
# boundary knows about. `EXPECTED` is used deliberately — it has `--declare` folded in by now,
# which is the one mechanism that exists to name exactly this nested case.
ws_mounted=no
while IFS= read -r m; do
    [ -n "$m" ] || continue
    printf '%s\n' "$EXPECTED" | grep -qx "$m" || continue
    case "$mem_repo" in "$m"|"$m"/?*) ws_mounted=yes; break ;; *) ;; esac
done <<<"$actual"
assert "$mem_repo is inside a declared mount point" "$ws_mounted"
# ASKED OF THE KERNEL TOO, with the table already in hand. `[ -d /home/vscode/.jkb ]` was a test
# that `jkb` itself satisfies: `create_dir_all` runs on the parent of JKB_DB at three sites in
# main.rs, so any verb materialises the directory and this printed `ok` in a container where the
# bind was simply absent — a container-local database, a container-local memory store the linker
# happily reports `linked` for, and every write lost on the next rebuild. The boundary check above
# cannot cover it either: `comm -23 actual EXPECTED` reports mounts that are EXTRA, never a
# declared one that is missing.
kb_mounted=no
while IFS= read -r m; do
    [ "$m" = /home/vscode/.jkb ] && { kb_mounted=yes; break; }
done <<<"$actual"
assert "knowledge base is mounted" "$kb_mounted"

# 5. Egress default-deny. Asserted in BOTH directions: a firewall that blocks everything passes a
#    one-sided test while having broken the container.
# WHAT THE FIREWALL DID, not what egress happens to do. The two probes below both resolve a name,
# so a dead resolver produces the same answers as a deny-all — which meant the harness case for
# `fail_closed` passed without ever establishing that it installed anything. The marker is written
# by `fail_closed` and cleared only by a successful raise, so it says which of the two happened.
if [ -e /run/jkb-egress-failed ]; then
    bad "the firewall failed closed and left no allowlist: $(head -1 /run/jkb-egress-failed 2>/dev/null)"
else
    ok "the firewall raised an allowlist (it did not fail closed)"
fi
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

# 7. The declared extensions are actually installed. This is the assertion the failure that
#    prompted it did not have: VS Code's own install failed with ECONNREFUSED against the egress
#    firewall, logged it, and carried on — so `postCreate` went green with neither extension
#    present and the Claude Code extension, which is most of what this container is for, simply
#    absent. A non-fatal log line is not a guard.
#
#    Asked of the server, not of a directory listing: `--list-extensions` is what VS Code itself
#    considers installed, where a directory can be left behind by a failed install.
#
#    SKIPPED where there is no VS Code server — `devcontainer up`, a plain docker run and
#    mutate-verify.sh all build a correct container with no VS Code in it. The skip is printed
#    rather than silent, and it is the SAME condition setup.sh installs under, so the two cannot
#    disagree about whether this ran.
code_server="$(ls -d "$HOME"/.vscode-server/bin/*/bin/code-server 2>/dev/null | head -1 || true)"
if [ -z "$code_server" ]; then
    echo "  skip no VS Code server in this container — extensions not checked (not a VS Code launch)"
else
    installed="$("$code_server" --server-data-dir "$HOME/.vscode-server" --list-extensions 2>/dev/null || true)"
    declared="$(dc_extensions "$here_dc/devcontainer.json")"
    # ...plus the extension this repo builds itself, which is not in that list and cannot be: it is
    # not on the marketplace. It was absent from every container ever built precisely because
    # nothing declared it, so nothing checked it. Appended rather than checked separately so the
    # one matcher the self-test exercises covers it too.
    if local_ext="$(dc_local_extension "$(cd "$here_dc/.." && pwd)")"; then
        declared="$declared"$'\n'"$local_ext"
    fi
    missing="$(missing_extensions "$declared" "$installed")"
    if [ -n "$missing" ]; then
        bad "declared extensions are not installed:$missing (marketplace ones come from ~/.vsix, the jkb explorer from scripts/install-extension.sh; rebuild the container)"
    else
        ok "every declared VS Code extension is installed ($(printf '%s\n' "$installed" | grep -c .) present)"
    fi
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
